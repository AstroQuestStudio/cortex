//! Cortex Web : robots.txt (RFC 9309, https://www.rfc-editor.org/rfc/rfc9309). Voir docs/WEB.md §5.
//!
//! Module PUR : aucune requête réseau, aucune dépendance. L'appelant (le `Fetcher`) récupère
//! le fichier et fournit `(statut, corps)` ; ce module décide quoi en faire.
//!
//! Règles appliquées (relues dans la RFC) :
//! - §2.1 : un groupe = une ou plusieurs lignes `User-agent` suivies de règles ; deux groupes
//!   visant le même agent sont FUSIONNÉS ; le jeton de produit se compare sans tenir compte de la
//!   casse ; à défaut de groupe pour notre jeton, on prend le(s) groupe(s) `*`. Un groupe
//!   spécifique, même vide, l'emporte sur `*` (pas de cumul).
//! - §2.2.2 : `Allow`/`Disallow` = motif préfixe sur « chemin + requête » ; `*` = n'importe quelle
//!   suite de caractères, `$` final = fin de l'URL. Le motif le plus LONG (en octets) gagne ; à
//!   égalité, `Allow` gagne ; aucune règle qui correspond = permis. `Disallow:` vide = tout permis.
//! - §2.2.2 (encodage) : les octets non ASCII et réservés sont comparés encodés en `%XX` (majuscules) ;
//!   un `%XX` ASCII non réservé (lettre, chiffre, `-._~`) est décodé avant comparaison.
//! - §2.3.1 : 2xx = analyser ; 4xx = fichier absent, tout permis ; 5xx ou erreur réseau = tout
//!   REFUSÉ. Choix ici (comme Google) : 429 compte comme une erreur serveur (refus).
//! - §2.5 : analyse limitée aux 500 Kio premiers ; une ligne coupée par la limite est abandonnée.
//! - §2.2.4 : `/robots.txt` reste toujours permis (sinon on ne pourrait pas le relire).
//!
//! Hors RFC mais courant : `Crawl-delay` (par groupe, en secondes) et `Sitemap` (global).

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Limite d'analyse (RFC 9309 §2.5 : au moins 500 kibioctets).
pub const MAX_ROBOTS_BYTES: usize = 500 * 1024;
/// Durée de validité d'une entrée du cache (RFC 9309 §2.4 : pas au-delà de 24 h sans raison).
pub const CACHE_TTL: Duration = Duration::from_secs(24 * 3600);
/// Une réponse « refus total » due à une erreur serveur ou réseau est retentée plus tôt.
pub const ERROR_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Rules,
    AllowAll,
    DisallowAll,
}

#[derive(Clone, Debug)]
struct Rule {
    allow: bool,
    /// Motif normalisé (encodage), SANS le `$` final.
    pattern: Vec<u8>,
    /// Vrai si le motif se terminait par `$`.
    anchored: bool,
    /// Longueur du motif normalisé, `$` compris : critère de priorité.
    len: usize,
}

#[derive(Default)]
struct Group {
    agents: Vec<String>,
    rules: Vec<Rule>,
    delays: Vec<f32>,
}

/// Règles robots.txt applicables à NOTRE jeton d'agent.
#[derive(Clone, Debug)]
pub struct Robots {
    mode: Mode,
    rules: Vec<Rule>,
    delay: Option<f32>,
    sitemaps: Vec<String>,
}

impl Robots {
    /// Fichier absent (statut 4xx) : tout est permis.
    pub fn allow_all() -> Robots {
        Robots { mode: Mode::AllowAll, rules: Vec::new(), delay: None, sitemaps: Vec::new() }
    }

    /// Erreur serveur (5xx) ou réseau (RFC 9309 §2.3.1.4) : tout est refusé (sauf `/robots.txt`).
    pub fn disallow_all() -> Robots {
        Robots { mode: Mode::DisallowAll, rules: Vec::new(), delay: None, sitemaps: Vec::new() }
    }

    /// Applique les règles de statut de la RFC puis, pour un 2xx, analyse le corps.
    /// `status` 0 = échec réseau (DNS, connexion, délai, corps illisible).
    pub fn from_status(status: u16, body: &str, user_agent_token: &str) -> Robots {
        match status {
            200..=299 => Robots::parse(body, user_agent_token),
            // Redirections non résolues (plus de 5) : « indisponible » (§2.3.1.2) = permis.
            300..=399 => Robots::allow_all(),
            // 429 : limitation de débit, traitée comme une erreur serveur (prudence, comme Google).
            429 => Robots::disallow_all(),
            400..=499 => Robots::allow_all(),
            _ => Robots::disallow_all(),
        }
    }

    /// Analyse un robots.txt pour `user_agent_token` (jeton de produit, ex. `CortexBot`).
    pub fn parse(text: &str, user_agent_token: &str) -> Robots {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let (text, truncated) = if text.len() > MAX_ROBOTS_BYTES {
            let mut cut = MAX_ROBOTS_BYTES;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            (&text[..cut], true)
        } else {
            (text, false)
        };
        let mut lines: Vec<&str> = text.split(|c| c == '\n' || c == '\r').collect();
        if truncated && !text.ends_with(['\n', '\r']) {
            lines.pop(); // ligne coupée par la limite : abandonnée
        }

        let mut groups: Vec<Group> = Vec::new();
        let mut current: Option<Group> = None;
        let mut last_was_agent = false;
        let mut sitemaps: Vec<String> = Vec::new();

        for raw in lines {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once(':') else { continue };
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim();
            match key.as_str() {
                "user-agent" | "useragent" | "user agent" => {
                    let agent = product_token(value);
                    if agent.is_empty() {
                        continue;
                    }
                    match current.as_mut() {
                        Some(g) if last_was_agent => g.agents.push(agent),
                        _ => {
                            if let Some(g) = current.take() {
                                groups.push(g);
                            }
                            current = Some(Group { agents: vec![agent], ..Group::default() });
                        }
                    }
                    last_was_agent = true;
                }
                "allow" | "disallow" => {
                    let Some(g) = current.as_mut() else { continue };
                    last_was_agent = false;
                    if let Some(rule) = make_rule(key == "allow", value) {
                        g.rules.push(rule);
                    }
                }
                "crawl-delay" | "crawl delay" => {
                    let Some(g) = current.as_mut() else { continue };
                    last_was_agent = false;
                    if let Ok(d) = value.parse::<f32>() {
                        if d.is_finite() && d >= 0.0 {
                            g.delays.push(d);
                        }
                    }
                }
                "sitemap" => {
                    let lower = value.to_ascii_lowercase();
                    if (lower.starts_with("http://") || lower.starts_with("https://")) && !sitemaps.iter().any(|s| s == value) {
                        sitemaps.push(value.to_string());
                    }
                }
                _ => {}
            }
        }
        if let Some(g) = current.take() {
            groups.push(g);
        }

        // Sélection : groupes de notre jeton, sinon groupes `*` ; fusion des groupes retenus.
        let token = product_token(user_agent_token);
        let specific = !token.is_empty() && token != "*" && groups.iter().any(|g| g.agents.iter().any(|a| *a == token));
        let wanted: &str = if specific { &token } else { "*" };
        let mut rules = Vec::new();
        let mut delay: Option<f32> = None;
        for g in groups.iter().filter(|g| g.agents.iter().any(|a| a == wanted)) {
            rules.extend(g.rules.iter().cloned());
            for d in &g.delays {
                delay = Some(delay.map_or(*d, |x: f32| x.max(*d))); // le plus prudent
            }
        }
        Robots { mode: Mode::Rules, rules, delay, sitemaps }
    }

    /// Le chemin (avec requête éventuelle, ex. `/a/b?x=1`) peut-il être récupéré ?
    pub fn allowed(&self, path_and_query: &str) -> bool {
        let target = normalize_target(path_and_query);
        let is_robots = target.split(|b| *b == b'?').next() == Some(b"/robots.txt".as_slice());
        if is_robots {
            return true;
        }
        match self.mode {
            Mode::AllowAll => return true,
            Mode::DisallowAll => return false,
            Mode::Rules => {}
        }
        let mut best: Option<(usize, bool)> = None;
        for r in &self.rules {
            if !rule_matches(r, &target) {
                continue;
            }
            best = match best {
                None => Some((r.len, r.allow)),
                Some((l, a)) => {
                    if r.len > l || (r.len == l && r.allow && !a) {
                        Some((r.len, r.allow))
                    } else {
                        Some((l, a))
                    }
                }
            };
        }
        best.map_or(true, |(_, allow)| allow)
    }

    /// `Crawl-delay` en secondes (extension non normalisée), le plus grand des groupes fusionnés.
    pub fn crawl_delay(&self) -> Option<f32> {
        self.delay
    }

    /// URL des `Sitemap:` (globaux, sans doublon, dans l'ordre du fichier).
    pub fn sitemaps(&self) -> Vec<String> {
        self.sitemaps.clone()
    }

    /// Vrai pour le résultat d'une erreur serveur ou réseau (`disallow_all`).
    pub fn is_disallow_all(&self) -> bool {
        self.mode == Mode::DisallowAll
    }
}

// ───────────────────────── Cache ─────────────────────────

/// Cache des robots.txt par origine (`https://exemple.org`), TTL 24 h, horloge injectable.
pub struct RobotsCache {
    token: String,
    ttl: Duration,
    map: HashMap<String, (Robots, Instant)>,
}

impl RobotsCache {
    pub fn new(user_agent_token: &str) -> RobotsCache {
        RobotsCache { token: user_agent_token.to_string(), ttl: CACHE_TTL, map: HashMap::new() }
    }

    pub fn with_ttl(mut self, ttl: Duration) -> RobotsCache {
        self.ttl = ttl;
        self
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn invalidate(&mut self, origin: &str) {
        self.map.remove(&origin_key(origin));
    }

    /// Entrée valide pour `origin`, ou appelle `fetch` (qui rend `(statut, corps)`, statut 0 = échec réseau).
    pub fn get_or_insert_with(&mut self, origin: &str, fetch: impl FnOnce() -> (u16, String)) -> &Robots {
        self.get_or_insert_with_at(origin, Instant::now(), fetch)
    }

    /// Variante à horloge injectée (tests, ou instant déjà lu par l'appelant).
    pub fn get_or_insert_with_at(&mut self, origin: &str, now: Instant, fetch: impl FnOnce() -> (u16, String)) -> &Robots {
        let key = origin_key(origin);
        let fresh = self.map.get(&key).map_or(false, |(r, t)| {
            let ttl = if r.is_disallow_all() { self.ttl.min(ERROR_TTL) } else { self.ttl };
            now.saturating_duration_since(*t) < ttl
        });
        if !fresh {
            let (status, body) = fetch();
            let robots = Robots::from_status(status, &body, &self.token);
            self.map.insert(key.clone(), (robots, now));
        }
        &self.map[&key].0
    }
}

fn origin_key(origin: &str) -> String {
    origin.trim().trim_end_matches('/').to_ascii_lowercase()
}

// ───────────────────────── Motifs et comparaison ─────────────────────────

/// Jeton de produit : minuscules, sans version (`CortexBot/1.2` -> `cortexbot`).
fn product_token(s: &str) -> String {
    let s = s.trim().to_ascii_lowercase();
    s.split(|c: char| c == '/' || c.is_whitespace()).next().unwrap_or("").to_string()
}

fn make_rule(allow: bool, value: &str) -> Option<Rule> {
    if value.is_empty() {
        return None; // `Disallow:` vide = tout permis ; `Allow:` vide = sans effet
    }
    let mut pattern = normalize(value.as_bytes());
    if pattern.first().map_or(true, |b| *b != b'/' && *b != b'*') {
        pattern.insert(0, b'/'); // motif sans « / » initial : lecture prudente
    }
    let len = pattern.len();
    let anchored = pattern.last() == Some(&b'$');
    if anchored {
        pattern.pop();
    }
    Some(Rule { allow, pattern, anchored, len })
}

/// Cible : retire schéma/hôte et fragment, garantit le « / » initial, normalise l'encodage.
fn normalize_target(path_and_query: &str) -> Vec<u8> {
    let mut s = path_and_query.trim();
    if let Some(i) = s.find('#') {
        s = &s[..i];
    }
    let lower = s.get(..8).unwrap_or(s).to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        let rest = &s[s.find("://").map_or(0, |i| i + 3)..];
        s = match rest.find(['/', '?']) {
            Some(i) => &rest[i..],
            None => "/",
        };
    }
    let mut out = normalize(s.as_bytes());
    if out.first() != Some(&b'/') {
        out.insert(0, b'/');
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn push_pct(out: &mut Vec<u8>, b: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    out.push(b'%');
    out.push(HEX[(b >> 4) as usize]);
    out.push(HEX[(b & 15) as usize]);
}

/// Normalisation d'encodage commune au motif et à la cible (RFC 9309 §2.2.2).
fn normalize(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        let b = input[i];
        if b == b'%' {
            let hi = input.get(i + 1).copied().and_then(hex_val);
            let lo = input.get(i + 2).copied().and_then(hex_val);
            if let (Some(h), Some(l)) = (hi, lo) {
                let v = h * 16 + l;
                if v.is_ascii_alphanumeric() || matches!(v, b'-' | b'.' | b'_' | b'~') {
                    out.push(v);
                } else {
                    push_pct(&mut out, v);
                }
                i += 3;
                continue;
            }
            push_pct(&mut out, b'%'); // « % » isolé : littéral
        } else if b >= 0x80 || b <= 0x20 || b == 0x7f {
            push_pct(&mut out, b);
        } else {
            out.push(b);
        }
        i += 1;
    }
    out
}

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Correspondance de préfixe avec jokers `*` ; `$` final = ancrage en fin de cible.
fn rule_matches(rule: &Rule, target: &[u8]) -> bool {
    let segs: Vec<&[u8]> = rule.pattern.split(|b| *b == b'*').collect();
    if segs.len() == 1 {
        return if rule.anchored { target == segs[0] } else { target.starts_with(segs[0]) };
    }
    if !target.starts_with(segs[0]) {
        return false;
    }
    let mut cur = segs[0].len();
    for seg in &segs[1..segs.len() - 1] {
        match find_sub(&target[cur..], seg) {
            Some(i) => cur += i + seg.len(),
            None => return false,
        }
    }
    let last = segs[segs.len() - 1];
    if rule.anchored {
        last.is_empty() || (target.len() >= cur + last.len() && target.ends_with(last))
    } else {
        find_sub(&target[cur..], last).is_some()
    }
}

// ───────────────────────── Tests ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const UA: &str = "CortexBot";

    fn r(text: &str) -> Robots {
        Robots::parse(text, UA)
    }

    // --- Exemples de la RFC 9309 ---

    #[test]
    fn rfc_exemple_groupes_multiples_et_fusion() {
        // RFC 9309 §5.1 : `*` interdit /, mais le groupe spécifique de foobot l'emporte.
        let txt = "User-agent: *\nDisallow: *.gif$\nDisallow: /example/\nAllow: /publications/\n\n\
                   User-agent: foobot\nDisallow:/\nAllow:/example/page.html\nAllow:/example/allowed.gif\n\n\
                   User-agent: barbot\nUser-agent: bazbot\nDisallow: /example/page.html\n\n\
                   User-agent: quxbot\n\nDisallow:\n";
        let foo = Robots::parse(txt, "foobot");
        assert!(!foo.allowed("/"));
        assert!(foo.allowed("/example/page.html"));
        assert!(foo.allowed("/example/allowed.gif"));
        assert!(!foo.allowed("/example/other.html"));
        let bar = Robots::parse(txt, "barbot");
        let baz = Robots::parse(txt, "BazBot");
        assert!(!bar.allowed("/example/page.html"));
        assert!(!baz.allowed("/example/page.html"));
        assert!(bar.allowed("/example/other.html")); // seul son groupe compte, pas `*`
        let qux = Robots::parse(txt, "quxbot");
        assert!(qux.allowed("/example/page.html") && qux.allowed("/anything.gif"));
        let autre = Robots::parse(txt, "inconnu");
        assert!(!autre.allowed("/pic.gif"));
        assert!(!autre.allowed("/example/x"));
        assert!(autre.allowed("/publications/x"));
        assert!(autre.allowed("/ailleurs"));
    }

    #[test]
    fn rfc_exemple_longest_match_google() {
        // RFC 9309 §5.2 : le motif le plus long l'emporte.
        let t = "user-agent: *\nallow: /p\ndisallow: /\n";
        let x = r(t);
        assert!(x.allowed("/page"));
        assert!(!x.allowed("/folder/page"));
        let t2 = "user-agent: *\nallow: /folder\ndisallow: /folder\n";
        assert!(r(t2).allowed("/folder/page")); // égalité : Allow
        let t3 = "user-agent: *\nallow: /page\ndisallow: /*.htm\n";
        assert!(!r(t3).allowed("/page.htm")); // /*.htm (6) > /page (5)
        let t4 = "user-agent: *\nallow: /$\ndisallow: /\n";
        assert!(r(t4).allowed("/"));
        assert!(!r(t4).allowed("/page.htm"));
    }

    // --- Jokers, ancrage ---

    #[test]
    fn joker_etoile_au_milieu() {
        let x = r("User-agent: *\nDisallow: /a/*/c\n");
        assert!(!x.allowed("/a/b/c"));
        assert!(!x.allowed("/a/b/b/c/d"));
        assert!(x.allowed("/a/c"));
        assert!(x.allowed("/b/b/c"));
    }

    #[test]
    fn joker_etoile_initiale_et_finale() {
        let x = r("User-agent: *\nDisallow: *.pdf\nDisallow: /tmp*\n");
        assert!(!x.allowed("/docs/a.pdf"));
        assert!(!x.allowed("/docs/a.pdf?x=1")); // préfixe sur chemin + requête
        assert!(!x.allowed("/tmpfile"));
        assert!(x.allowed("/docs/a.html"));
    }

    #[test]
    fn dollar_ancre_la_fin() {
        let x = r("User-agent: *\nDisallow: /*.php$\n");
        assert!(!x.allowed("/index.php"));
        assert!(x.allowed("/index.php?x=1"));
        assert!(x.allowed("/index.phps"));
        assert!(x.allowed("/index.php/x"));
    }

    #[test]
    fn dollar_sans_joker_egalite_stricte() {
        let x = r("User-agent: *\nDisallow: /fish$\n");
        assert!(!x.allowed("/fish"));
        assert!(x.allowed("/fish/"));
        assert!(x.allowed("/fishing"));
    }

    #[test]
    fn etoile_dollar_equivaut_a_prefixe() {
        let x = r("User-agent: *\nDisallow: /a*$\n");
        assert!(!x.allowed("/abc"));
        assert!(x.allowed("/b"));
    }

    #[test]
    fn etoiles_consecutives() {
        let x = r("User-agent: *\nDisallow: /a**b\n");
        assert!(!x.allowed("/axxb"));
        assert!(!x.allowed("/ab"));
        assert!(x.allowed("/a"));
    }

    #[test]
    fn requete_comparee_avec_le_chemin() {
        let x = r("User-agent: *\nDisallow: /search?q=\nAllow: /search?q=rust\n");
        assert!(!x.allowed("/search?q=python"));
        assert!(x.allowed("/search?q=rust"));
        assert!(x.allowed("/search"));
    }

    // --- Égalité Allow / Disallow, longueur ---

    #[test]
    fn egalite_allow_gagne_quel_que_soit_l_ordre() {
        let a = r("User-agent: *\nAllow: /x\nDisallow: /x\n");
        let b = r("User-agent: *\nDisallow: /x\nAllow: /x\n");
        assert!(a.allowed("/x/y") && b.allowed("/x/y"));
    }

    #[test]
    fn disallow_plus_long_bat_allow_plus_court() {
        let x = r("User-agent: *\nAllow: /docs/\nDisallow: /docs/private/\n");
        assert!(x.allowed("/docs/public"));
        assert!(!x.allowed("/docs/private/a"));
    }

    #[test]
    fn la_longueur_compte_les_jokers() {
        // `/*.html` (7) bat `/docs` (5) ; `/docs/` (6) perd contre `/*.html`.
        let x = r("User-agent: *\nDisallow: /*.html\nAllow: /docs/\n");
        assert!(!x.allowed("/docs/a.html"));
        assert!(x.allowed("/docs/a.txt"));
    }

    // --- Groupes et agents ---

    #[test]
    fn jeton_insensible_a_la_casse_et_a_la_version() {
        let txt = "USER-AGENT: CORTEXBOT/2.0\nDISALLOW: /prive\n";
        assert!(!Robots::parse(txt, "cortexbot").allowed("/prive"));
        assert!(!Robots::parse(txt, "CortexBot/9.9").allowed("/prive"));
    }

    #[test]
    fn repli_sur_etoile_pour_agent_inconnu() {
        let txt = "User-agent: googlebot\nDisallow: /g\n\nUser-agent: *\nDisallow: /tous\n";
        let x = r(txt);
        assert!(!x.allowed("/tous"));
        assert!(x.allowed("/g"));
    }

    #[test]
    fn agent_specifique_vide_ne_herite_pas_de_etoile() {
        let txt = "User-agent: *\nDisallow: /\n\nUser-agent: cortexbot\nAllow: /\n";
        let x = r(txt);
        assert!(x.allowed("/n-importe-quoi"));
    }

    #[test]
    fn groupes_du_meme_agent_fusionnes() {
        let txt = "User-agent: cortexbot\nDisallow: /a\n\nUser-agent: autre\nDisallow: /z\n\n\
                   User-agent: CortexBot\nDisallow: /b\n";
        let x = r(txt);
        assert!(!x.allowed("/a") && !x.allowed("/b"));
        assert!(x.allowed("/z"));
    }

    #[test]
    fn plusieurs_user_agent_consecutifs_partagent_les_regles() {
        let txt = "User-agent: a\nUser-agent: cortexbot\nUser-agent: b\nDisallow: /x\n";
        assert!(!r(txt).allowed("/x"));
    }

    #[test]
    fn user_agent_apres_regle_ouvre_un_nouveau_groupe() {
        let txt = "User-agent: a\nDisallow: /x\nUser-agent: cortexbot\nDisallow: /y\n";
        let x = r(txt);
        assert!(x.allowed("/x"));
        assert!(!x.allowed("/y"));
    }

    #[test]
    fn regle_avant_tout_user_agent_ignoree() {
        let x = r("Disallow: /\nUser-agent: *\nDisallow: /b\n");
        assert!(x.allowed("/a"));
        assert!(!x.allowed("/b"));
    }

    #[test]
    fn sans_groupe_utile_tout_est_permis() {
        let x = r("User-agent: autre\nDisallow: /\n");
        assert!(x.allowed("/n-importe-quoi"));
    }

    // --- Disallow vide, lignes invalides ---

    #[test]
    fn disallow_vide_autorise_tout() {
        let x = r("User-agent: *\nDisallow:\n");
        assert!(x.allowed("/") && x.allowed("/a/b?c=d"));
        let y = r("User-agent: *\nDisallow:   # rien\n");
        assert!(y.allowed("/x"));
    }

    #[test]
    fn commentaires_bom_crlf_et_lignes_invalides() {
        let txt = "\u{feff}# en-tête\r\nUser-agent: *  # tous\r\nCeci n'est pas une directive\r\nDisallow: /secret # note\r\n\
                   Disalow: /faute\r\n:\r\n\r\nAllow: /secret/ok\r\n";
        let x = r(txt);
        assert!(!x.allowed("/secret/a"));
        assert!(x.allowed("/secret/ok"));
        assert!(x.allowed("/faute"));
    }

    #[test]
    fn fins_de_ligne_cr_seul() {
        let x = r("User-agent: *\rDisallow: /cr\r");
        assert!(!x.allowed("/cr"));
    }

    #[test]
    fn directives_insensibles_a_la_casse() {
        let x = r("uSeR-aGeNt: *\nDISALLOW: /m\nallow: /m/ok\n");
        assert!(!x.allowed("/m/x") && x.allowed("/m/ok"));
    }

    // --- Encodage ---

    #[test]
    fn pourcentage_encodage_non_reserve_decode() {
        let x = r("User-agent: *\nDisallow: /a%62c\n");
        assert!(!x.allowed("/abc")); // %62 = b
        let y = r("User-agent: *\nDisallow: /abc\n");
        assert!(!y.allowed("/a%62c"));
        assert!(!y.allowed("/%61bc"));
    }

    #[test]
    fn pourcentage_encodage_reserve_conserve_et_majuscules() {
        let x = r("User-agent: *\nDisallow: /a%2fb\n");
        assert!(!x.allowed("/a%2Fb")); // casse de l'hexadécimal indifférente
        assert!(x.allowed("/a/b")); // %2F (réservé) n'est PAS un « / »
    }

    #[test]
    fn non_ascii_compare_encode() {
        let x = r("User-agent: *\nDisallow: /café\n");
        assert!(!x.allowed("/caf%C3%A9"));
        assert!(!x.allowed("/café/menu"));
        let y = r("User-agent: *\nDisallow: /caf%c3%a9\n");
        assert!(!y.allowed("/café"));
    }

    #[test]
    fn pourcent_isole_litteral() {
        let x = r("User-agent: *\nDisallow: /100%\n");
        assert!(!x.allowed("/100%"));
        assert!(x.allowed("/100"));
    }

    // --- robots.txt, chemins ---

    #[test]
    fn robots_txt_toujours_permis() {
        let x = r("User-agent: *\nDisallow: /\n");
        assert!(x.allowed("/robots.txt"));
        assert!(x.allowed("/robots.txt?v=2"));
        assert!(!x.allowed("/robots.txt.bak"));
        assert!(Robots::disallow_all().allowed("/robots.txt"));
    }

    #[test]
    fn cible_sans_slash_ou_avec_url_ou_fragment() {
        let x = r("User-agent: *\nDisallow: /a\n");
        assert!(!x.allowed("a"));
        assert!(!x.allowed("https://exemple.org/a/b"));
        assert!(!x.allowed("/a#frag"));
        assert!(x.allowed("https://exemple.org"));
    }

    #[test]
    fn motif_sans_slash_initial_lu_comme_chemin() {
        let x = r("User-agent: *\nDisallow: prive\n");
        assert!(!x.allowed("/prive/x"));
    }

    // --- Crawl-delay, Sitemap ---

    #[test]
    fn crawl_delay_du_groupe_retenu() {
        let txt = "User-agent: *\nCrawl-delay: 10\n\nUser-agent: cortexbot\nCrawl-delay: 2.5\n";
        assert_eq!(r(txt).crawl_delay(), Some(2.5));
        assert_eq!(Robots::parse(txt, "autre").crawl_delay(), Some(10.0));
        assert_eq!(r("User-agent: *\nDisallow: /x\n").crawl_delay(), None);
        assert_eq!(r("User-agent: *\nCrawl-delay: abc\n").crawl_delay(), None);
        assert_eq!(r("User-agent: *\nCrawl-delay: -3\n").crawl_delay(), None);
    }

    #[test]
    fn crawl_delay_fusion_prend_le_plus_prudent() {
        let txt = "User-agent: cortexbot\nCrawl-delay: 1\n\nUser-agent: cortexbot\nCrawl-delay: 4\n";
        assert_eq!(r(txt).crawl_delay(), Some(4.0));
    }

    #[test]
    fn sitemaps_globaux_sans_doublon() {
        let txt = "Sitemap: https://a.test/s.xml\nUser-agent: autre\nDisallow: /\n\
                   Sitemap: https://a.test/s.xml\nsitemap: http://a.test/s2.xml # c\nSitemap: ftp://x/y\nSitemap:\n";
        let x = r(txt);
        assert_eq!(x.sitemaps(), vec!["https://a.test/s.xml".to_string(), "http://a.test/s2.xml".to_string()]);
    }

    // --- Statuts ---

    #[test]
    fn statut_5xx_et_erreur_reseau_refusent_tout() {
        for s in [500u16, 502, 503, 504, 599, 0] {
            let x = Robots::from_status(s, "User-agent: *\nDisallow:\n", UA);
            assert!(!x.allowed("/"), "statut {s}");
            assert!(x.allowed("/robots.txt"));
        }
        assert!(Robots::disallow_all().is_disallow_all());
    }

    #[test]
    fn statut_4xx_autorise_tout() {
        for s in [400u16, 401, 403, 404, 410, 451] {
            let x = Robots::from_status(s, "User-agent: *\nDisallow: /\n", UA);
            assert!(x.allowed("/page"), "statut {s}");
        }
        assert!(!Robots::allow_all().is_disallow_all());
    }

    #[test]
    fn statut_429_traite_comme_erreur_serveur() {
        assert!(!Robots::from_status(429, "", UA).allowed("/x"));
    }

    #[test]
    fn statut_2xx_analyse_le_corps_et_3xx_est_indisponible() {
        let x = Robots::from_status(200, "User-agent: *\nDisallow: /x\n", UA);
        assert!(!x.allowed("/x/y") && x.allowed("/z"));
        assert!(Robots::from_status(301, "peu importe", UA).allowed("/x"));
        assert!(Robots::from_status(204, "", UA).allowed("/x"));
    }

    // --- Fichier géant ---

    #[test]
    fn fichier_geant_tronque_a_500_kio() {
        let mut txt = String::from("User-agent: *\nDisallow: /avant\n");
        while txt.len() < MAX_ROBOTS_BYTES + 100 {
            txt.push_str("# remplissage remplissage remplissage remplissage\n");
        }
        txt.push_str("Disallow: /apres-la-limite\n");
        let x = r(&txt);
        assert!(!x.allowed("/avant"));
        assert!(x.allowed("/apres-la-limite"));
    }

    #[test]
    fn fichier_geant_ligne_coupee_par_la_limite_abandonnee() {
        let prefixe = "User-agent: *
Disallow: /garde
";
        // La ligne `Disallow: /secret` est coupée après « Disallow: /s » par la limite de 500 Kio.
        let remplissage = "#".repeat(MAX_ROBOTS_BYTES - prefixe.len() - 1 - 12);
        let txt = format!(
            "{prefixe}{remplissage}
Disallow: /secret
"
        );
        let x = r(&txt);
        assert!(!x.allowed("/garde"));
        assert!(x.allowed("/sx")); // sans l'abandon, le motif « /s » aurait tout bloqué
        assert!(x.allowed("/secret"));
    }

    #[test]
    fn fichier_geant_multioctet_a_la_limite_sans_panique() {
        let mut txt = String::from("User-agent: *\nDisallow: /x\n# ");
        while txt.len() < MAX_ROBOTS_BYTES + 10 {
            txt.push('é');
        }
        let x = r(&txt);
        assert!(!x.allowed("/x"));
    }

    #[test]
    fn beaucoup_de_regles_reste_rapide() {
        let mut txt = String::from("User-agent: *\n");
        for i in 0..20_000 {
            txt.push_str(&format!("Disallow: /dir{i}/*/x$\n"));
        }
        let x = r(&txt);
        assert!(x.allowed("/dir1"));
        assert!(!x.allowed("/dir19999/a/x"));
    }

    // --- Cache ---

    #[test]
    fn cache_ne_telecharge_qu_une_fois_puis_expire_apres_24h() {
        let mut c = RobotsCache::new(UA);
        let t0 = Instant::now();
        let mut appels = 0;
        let mut fetch = |corps: &str| {
            appels += 1;
            (200u16, corps.to_string())
        };
        let a = c.get_or_insert_with_at("https://A.test/", t0, || fetch("User-agent: *\nDisallow: /v1\n")).clone();
        assert!(!a.allowed("/v1"));
        let b =
            c.get_or_insert_with_at("https://a.test", t0 + Duration::from_secs(3600), || fetch("User-agent: *\nDisallow: /v2\n")).clone();
        assert!(!b.allowed("/v1")); // entrée en cache (origine normalisée)
        let d = c
            .get_or_insert_with_at("https://a.test", t0 + CACHE_TTL + Duration::from_secs(1), || fetch("User-agent: *\nDisallow: /v2\n"))
            .clone();
        assert!(d.allowed("/v1") && !d.allowed("/v2"));
        assert_eq!(appels, 2);
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn cache_erreur_serveur_retentee_plus_tot() {
        let mut c = RobotsCache::new(UA);
        let t0 = Instant::now();
        let a = c.get_or_insert_with_at("https://e.test", t0, || (503, String::new())).clone();
        assert!(!a.allowed("/x"));
        let b = c.get_or_insert_with_at("https://e.test", t0 + Duration::from_secs(60), || (200, "User-agent: *\n".into())).clone();
        assert!(!b.allowed("/x")); // encore en cache
        let d =
            c.get_or_insert_with_at("https://e.test", t0 + ERROR_TTL + Duration::from_secs(1), || (200, "User-agent: *\n".into())).clone();
        assert!(d.allowed("/x"));
    }

    #[test]
    fn cache_origines_distinctes_et_invalidation() {
        let mut c = RobotsCache::new(UA);
        c.get_or_insert_with("https://a.test", || (404, String::new()));
        c.get_or_insert_with("https://b.test", || (200, "User-agent: *\nDisallow: /\n".into()));
        assert_eq!(c.len(), 2);
        assert!(c.get_or_insert_with("https://a.test", || panic!("pas de nouvel appel")).allowed("/p"));
        assert!(!c.get_or_insert_with("https://b.test", || panic!("pas de nouvel appel")).allowed("/p"));
        c.invalidate("https://B.test/");
        assert_eq!(c.len(), 1);
        assert!(!c.is_empty());
    }

    #[test]
    fn cache_utilise_son_jeton_d_agent() {
        let mut c = RobotsCache::new("CortexBot");
        let txt = "User-agent: cortexbot\nDisallow: /a\nUser-agent: *\nDisallow: /b\n";
        let r = c.get_or_insert_with("https://x.test", || (200, txt.into()));
        assert!(!r.allowed("/a") && r.allowed("/b"));
    }
}
