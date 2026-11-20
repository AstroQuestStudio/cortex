//! Cortex Web : le `Fetcher` sécurisé, SEUL point de sortie réseau de Cortex Web.
//!
//! Modèle de sécurité : `docs/WEB.md` §5. En résumé :
//! - `check_url` refuse par schéma, port, identifiants, NOM d'hôte et littéral IP ;
//! - un **résolveur filtrant** (branché dans `ureq`) refuse l'hôte ENTIER si UNE adresse résolue
//!   n'est pas publique : `ureq` se connecte exactement aux adresses validées (pas de seconde
//!   résolution, donc pas de TOCTOU ni de rebinding DNS) ;
//! - les redirections sont suivies À LA MAIN (`redirects(0)`) : chaque saut repasse `check_url`,
//!   robots.txt et le résolveur ;
//! - plafonds : corps après décompression, redirections, date limite totale, type MIME contrôlé
//!   avant de lire le corps ;
//! - en-têtes : User-Agent fixe, aucun cookie, aucun Referer, aucun Authorization PAR DÉFAUT, proxys
//!   d'environnement ignorés. Seul `Fetcher::fetch_with_headers` en ajoute, depuis une liste blanche
//!   de noms, au premier saut uniquement (jamais retransmis après une redirection).
//!
//! Correspondance noms refusés -> `BlockReason` (mod.rs n'a pas de variante « nom d'hôte ») :
//! `localhost` / `*.localhost` -> `PrivateAddress(127.0.0.1)` ; `metadata.google.internal` ->
//! `PrivateAddress(169.254.169.254)` ; `*.local`, `*.internal`, `*.localdomain` ->
//! `PrivateAddress(0.0.0.0)` ; hôte vide -> `ResolveFailed`.
//!
//! robots.txt : l'application passe par `RobotsGate` (fournie par l'appelant, voir
//! `Fetcher::with_robots`). Le téléchargement de `robots.txt` lui-même doit se faire avec un
//! `Fetcher` SANS porte (sinon récursion).

use std::error::Error as StdError;
use std::io::{self, Read};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

use url::{Host, Url};

use super::{BlockReason, FetchOutcome, Policy, Response, USER_AGENT};

/// Longueur maximale d'une URL acceptée (et d'un en-tête `Location`).
const MAX_URL_LEN: usize = 8192;
/// Taille d'un bloc de lecture du corps.
const BLOCK: usize = 16 * 1024;
/// Longueur maximale des validateurs / en-têtes recopiés.
const MAX_HDR_VALUE: usize = 512;

impl Default for Policy {
    /// La politique la plus stricte raisonnable (valeurs figées par `docs/WEB.md`).
    fn default() -> Self {
        Policy {
            allowed_ports: vec![80, 443],
            max_body_bytes: 5 * 1024 * 1024,
            max_redirects: 5,
            connect_timeout: Duration::from_secs(10),
            total_timeout: Duration::from_secs(30),
            respect_robots: true,
            allowed_content_types: [
                "text/html",
                "application/xhtml+xml",
                "text/markdown",
                "text/plain",
                "text/xml",
                "application/xml",
                "application/json",
                "application/rss+xml",
                "application/atom+xml",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            allow_addrs: Vec::new(),
        }
    }
}

// ───────────────────────── Adresses IP ─────────────────────────

/// Vrai si l'adresse est joignable publiquement (donc autorisée comme destination).
/// Refuse tout ce qui est local, privé, réservé, de documentation, multicast ou ambigu.
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => public_v4(a),
        IpAddr::V6(a) => public_v6(a),
    }
}

fn public_v4(a: Ipv4Addr) -> bool {
    let [o0, o1, o2, _] = a.octets();
    !(o0 == 0                                   // 0.0.0.0/8
        || o0 == 10                              // RFC 1918
        || o0 == 127                             // loopback
        || (o0 == 100 && (64..=127).contains(&o1)) // CGNAT 100.64/10
        || (o0 == 169 && o1 == 254)              // lien-local (métadonnées cloud)
        || (o0 == 172 && (16..=31).contains(&o1)) // RFC 1918
        || (o0 == 192 && o1 == 0 && o2 == 0)     // 192.0.0.0/24
        || (o0 == 192 && o1 == 0 && o2 == 2)     // TEST-NET-1
        || (o0 == 192 && o1 == 88 && o2 == 99)   // relais 6to4 (déprécié)
        || (o0 == 192 && o1 == 168)              // RFC 1918
        || (o0 == 198 && (o1 == 18 || o1 == 19)) // 198.18/15 (bancs d'essai)
        || (o0 == 198 && o1 == 51 && o2 == 100)  // TEST-NET-2
        || (o0 == 203 && o1 == 0 && o2 == 113)   // TEST-NET-3
        || o0 >= 224) // multicast 224/4, réservé 240/4, broadcast
}

fn v4_from_segments(hi: u16, lo: u16) -> Ipv4Addr {
    Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8)
}

fn public_v6(a: Ipv6Addr) -> bool {
    let s = a.segments();
    // ::ffff:a.b.c.d (IPv4-mappé) : on valide l'IPv4 incorporée.
    if s[..5] == [0; 5] && s[5] == 0xffff {
        return public_v4(v4_from_segments(s[6], s[7]));
    }
    // ::/96 : « ::», « ::1 » et les adresses IPv4-compatibles (dépréciées) : tout refusé.
    if s[..6] == [0; 6] {
        return false;
    }
    // NAT64 64:ff9b::/96 : IPv4 incorporée dans les 32 derniers bits.
    if s[0] == 0x0064 && s[1] == 0xff9b && s[2..6] == [0; 4] {
        return public_v4(v4_from_segments(s[6], s[7]));
    }
    // 6to4 2002::/16 : IPv4 incorporée juste après le préfixe.
    if s[0] == 0x2002 {
        return public_v4(v4_from_segments(s[1], s[2]));
    }
    // Hors 2000::/3 (unicast global) : fc00::/7, fe80::/10, fec0::/10, ff00::/8, 100::/64,
    // 64:ff9b:1::/48, 5f00::/16, non alloué... : refusé.
    if s[0] & 0xe000 != 0x2000 {
        return false;
    }
    // 2001::/23 (Teredo 2001::/32, protocoles IETF, ORCHID, bancs d'essai) ; 2001:db8::/32 est
    // inclus dans 2001:0::/16 -> traité ci-dessous.
    if s[0] == 0x2001 && (s[1] < 0x0200 || s[1] == 0x0db8) {
        return false;
    }
    // 3fff::/20 (documentation, RFC 9637).
    if s[0] == 0x3fff && s[1] & 0xf000 == 0 {
        return false;
    }
    true
}

// ───────────────────────── Contrôle d'URL ─────────────────────────

/// Nom d'hôte refusé : renvoie la raison, `None` si le nom est acceptable.
fn denied_name(host: &str) -> Option<BlockReason> {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    if h.is_empty() {
        return Some(BlockReason::ResolveFailed);
    }
    let private = |ip: Ipv4Addr| Some(BlockReason::PrivateAddress(IpAddr::V4(ip)));
    if h == "localhost" || h.ends_with(".localhost") {
        return private(Ipv4Addr::LOCALHOST);
    }
    if h == "metadata.google.internal" {
        return private(Ipv4Addr::new(169, 254, 169, 254));
    }
    for suffix in ["local", "internal", "localdomain"] {
        if h == suffix || h.ends_with(&format!(".{suffix}")) {
            return private(Ipv4Addr::UNSPECIFIED);
        }
    }
    None
}

/// Contrôle statique d'une URL (avant toute résolution DNS). Les littéraux IP sont contrôlés
/// ici ; les noms le sont à la connexion par le résolveur filtrant.
pub fn check_url(url: &Url, p: &Policy) -> Result<(), BlockReason> {
    match url.scheme() {
        "http" | "https" => {}
        other => return Err(BlockReason::Scheme(other.to_string())),
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(BlockReason::Credentials);
    }
    let port = url.port_or_known_default().unwrap_or(0);
    // Adresse tolérée explicitement (tests uniquement) : littéral IP + port exact.
    let mut tolerated = false;
    match url.host() {
        None => return Err(BlockReason::ResolveFailed),
        Some(Host::Domain(d)) => {
            if let Some(r) = denied_name(d) {
                return Err(r);
            }
        }
        Some(Host::Ipv4(a)) => {
            let ip = IpAddr::V4(a);
            tolerated = p.allow_addrs.iter().any(|s| s.ip() == ip && s.port() == port);
            if !tolerated && !is_public_ip(ip) {
                return Err(BlockReason::PrivateAddress(ip));
            }
        }
        Some(Host::Ipv6(a)) => {
            let ip = IpAddr::V6(a);
            tolerated = p.allow_addrs.iter().any(|s| s.ip() == ip && s.port() == port);
            if !tolerated && !is_public_ip(ip) {
                return Err(BlockReason::PrivateAddress(ip));
            }
        }
    }
    let port_ok = if p.allowed_ports.is_empty() { port == 80 || port == 443 } else { p.allowed_ports.contains(&port) };
    if !port_ok && !tolerated {
        return Err(BlockReason::Port(port));
    }
    Ok(())
}

// ───────────────────────── Résolveur filtrant ─────────────────────────

/// Erreur d'E/S portée par le résolveur quand une adresse n'est pas publique.
#[derive(Debug)]
struct PrivateHost(IpAddr);

impl std::fmt::Display for PrivateHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "adresse-non-publique:{}", self.0)
    }
}

impl StdError for PrivateHost {}

/// Résout `netloc` (« hôte:port ») et refuse l'hôte entier si UNE adresse n'est pas publique
/// (hors `allow`). Renvoie exactement les adresses validées : `ureq` ne résout rien d'autre.
fn resolve_public(netloc: &str, allow: &[SocketAddr]) -> io::Result<Vec<SocketAddr>> {
    let addrs: Vec<SocketAddr> = netloc.to_socket_addrs()?.collect();
    check_addrs(&addrs, allow)?;
    Ok(addrs)
}

/// Refuse l'ensemble si UNE adresse n'est ni publique ni explicitement tolérée.
fn check_addrs(addrs: &[SocketAddr], allow: &[SocketAddr]) -> io::Result<()> {
    for a in addrs {
        if !is_public_ip(a.ip()) && !allow.contains(a) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, PrivateHost(a.ip())));
        }
    }
    Ok(())
}

struct FilteringResolver {
    allow: Vec<SocketAddr>,
}

impl ureq::Resolver for FilteringResolver {
    fn resolve(&self, netloc: &str) -> io::Result<Vec<SocketAddr>> {
        resolve_public(netloc, &self.allow)
    }
}

/// Cherche, dans la chaîne des causes d'une erreur `ureq`, l'adresse refusée par le résolveur.
fn private_ip_in(err: &ureq::Error) -> Option<IpAddr> {
    let mut cur: Option<&(dyn StdError + 'static)> = Some(err);
    while let Some(e) = cur {
        if let Some(io_err) = e.downcast_ref::<io::Error>() {
            if let Some(inner) = io_err.get_ref() {
                if let Some(p) = inner.downcast_ref::<PrivateHost>() {
                    return Some(p.0);
                }
            }
        }
        if let Some(p) = e.downcast_ref::<PrivateHost>() {
            return Some(p.0);
        }
        cur = e.source();
    }
    // Repli : le message affiché porte la marque (au cas où la chaîne serait aplatie).
    let text = format!("{err}");
    let marker = "adresse-non-publique:";
    let at = text.find(marker)?;
    let tail = &text[at + marker.len()..];
    let end = tail.find(|c: char| c.is_whitespace() || c == ')').unwrap_or(tail.len());
    tail[..end].parse().ok()
}

fn is_timeout(err: &ureq::Error) -> bool {
    let mut cur: Option<&(dyn StdError + 'static)> = Some(err);
    while let Some(e) = cur {
        if let Some(io_err) = e.downcast_ref::<io::Error>() {
            if matches!(io_err.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock) {
                return true;
            }
        }
        cur = e.source();
    }
    false
}

// ───────────────────────── Fetcher ─────────────────────────

/// Validateurs HTTP d'une version déjà connue (requêtes conditionnelles).
#[derive(Clone, Debug, Default)]
pub struct Validators {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

/// Porte robots.txt fournie par l'appelant : vrai = l'URL est autorisée.
/// Toute closure `Fn(&Url) -> bool + Send + Sync` convient.
pub trait RobotsGate: Send + Sync {
    fn allows(&self, url: &Url) -> bool;
}

impl<F: Fn(&Url) -> bool + Send + Sync> RobotsGate for F {
    fn allows(&self, url: &Url) -> bool {
        self(url)
    }
}

#[derive(Debug)]
enum HopError {
    Blocked(BlockReason),
    Failed(String),
}

/// Client HTTP sécurisé : l'unique chemin réseau de Cortex Web.
#[derive(Clone)]
pub struct Fetcher {
    policy: Policy,
    agent: ureq::Agent,
    robots: Option<Arc<dyn RobotsGate>>,
}

fn clean_header_value(v: &str) -> Option<String> {
    let v = v.trim();
    if v.is_empty() || v.len() > MAX_HDR_VALUE || !v.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        return None;
    }
    Some(v.to_string())
}

/// Noms d'en-têtes supplémentaires acceptés par [`Fetcher::fetch_with_headers`] (insensible à la
/// casse) : uniquement ce dont les fournisseurs de recherche ont besoin. Tout le reste est refusé.
const EXTRA_HEADERS_ALLOWED: &[&str] = &["x-subscription-token", "authorization", "accept", "x-api-key"];
/// Longueur maximale d'une valeur d'en-tête supplémentaire.
const MAX_EXTRA_VALUE: usize = 1024;

/// Contrôle des en-têtes supplémentaires. Le message d'erreur ne contient JAMAIS une valeur
/// (le nom refusé est tronqué et réduit aux caractères d'un nom d'en-tête).
fn validate_extra_headers(extra: &[(&str, &str)]) -> Result<(), String> {
    let mut vus: Vec<String> = Vec::new();
    for (name, value) in extra {
        let lower = name.to_ascii_lowercase();
        if !EXTRA_HEADERS_ALLOWED.contains(&lower.as_str()) {
            let shown: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(40).collect();
            return Err(format!("en-tête refusé par la garde réseau : {shown}"));
        }
        if vus.contains(&lower) {
            return Err(format!("en-tête en double refusé : {lower}"));
        }
        if value.is_empty() || value.len() > MAX_EXTRA_VALUE || !value.bytes().all(|b| (0x20..0x7f).contains(&b)) {
            return Err(format!("valeur d'en-tête refusée pour {lower} (vide, trop longue ou caractère de contrôle)"));
        }
        vus.push(lower);
    }
    Ok(())
}

/// Retire des valeurs d'en-têtes d'un message (ceinture et bretelles : les erreurs `ureq` n'en
/// contiennent pas, mais aucun message ne doit pouvoir en porter).
fn redact_values(msg: &str, extra: &[(&str, &str)]) -> String {
    let mut out = msg.to_string();
    for (_, v) in extra {
        if v.len() >= 4 {
            out = out.replace(v, "***");
        }
    }
    out
}

/// Type accepté ? `essence` = type MIME sans paramètres, en minuscules. Entrée de liste :
/// type exact (« text/html »), ou famille (« text/ » ou « text/* »).
fn content_type_allowed(essence: &str, allowed: &[String]) -> bool {
    if essence.is_empty() {
        return false;
    }
    allowed.iter().any(|a| {
        let a = a.trim().to_ascii_lowercase();
        if let Some(family) = a.strip_suffix('*') {
            essence.starts_with(family)
        } else if a.ends_with('/') {
            essence.starts_with(&a)
        } else {
            essence == a
        }
    })
}

impl Fetcher {
    /// Fetcher sans porte robots.txt (à utiliser pour télécharger robots.txt lui-même, ou quand
    /// l'appelant applique robots.txt autrement).
    pub fn new(policy: Policy) -> Self {
        let agent = ureq::AgentBuilder::new()
            .redirects(0)
            .try_proxy_from_env(false)
            .timeout_connect(policy.connect_timeout)
            .timeout_read(policy.total_timeout)
            .timeout_write(policy.total_timeout)
            .user_agent(USER_AGENT)
            .resolver(FilteringResolver { allow: policy.allow_addrs.clone() })
            .build();
        Fetcher { policy, agent, robots: None }
    }

    /// Fetcher qui consulte `gate` avant CHAQUE saut (URL initiale et redirections) quand
    /// `policy.respect_robots` est vrai. Refus : `Blocked(Robots)`.
    pub fn with_robots(policy: Policy, gate: impl RobotsGate + 'static) -> Self {
        let mut f = Self::new(policy);
        f.robots = Some(Arc::new(gate));
        f
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// Récupère `url` en respectant la politique. Ne panique jamais sur entrée hostile.
    pub fn fetch(&self, url: &Url, v: &Validators) -> FetchOutcome {
        self.fetch_with_headers(url, v, &[])
    }

    /// Comme [`Fetcher::fetch`], avec des en-têtes supplémentaires (jeton d'API d'un fournisseur).
    /// - **Liste blanche** de noms ([`EXTRA_HEADERS_ALLOWED`]) ; tout autre nom (Cookie, Host,
    ///   Proxy-*, Referer, inconnu…), un nom en double ou une valeur vide / contenant un caractère
    ///   de contrôle (dont `\r` et `\n`) est refusé AVANT tout accès réseau (`Failed`).
    /// - Envoyés **uniquement au premier saut** : jamais retransmis après une redirection,
    ///   quelle que soit la destination.
    /// - Les valeurs ne figurent dans aucun message d'erreur.
    pub fn fetch_with_headers(&self, url: &Url, v: &Validators, extra: &[(&str, &str)]) -> FetchOutcome {
        if let Err(msg) = validate_extra_headers(extra) {
            return FetchOutcome::Failed(msg);
        }
        match self.fetch_inner(url, v, extra) {
            FetchOutcome::Failed(m) => FetchOutcome::Failed(redact_values(&m, extra)),
            autre => autre,
        }
    }

    fn fetch_inner(&self, url: &Url, v: &Validators, extra: &[(&str, &str)]) -> FetchOutcome {
        let start = Instant::now();
        let deadline = start.checked_add(self.policy.total_timeout).unwrap_or_else(|| start + Duration::from_secs(3600));
        if url.as_str().len() > MAX_URL_LEN {
            return FetchOutcome::Failed("URL trop longue".to_string());
        }
        let origin_host = url.host_str().map(|h| h.to_string());
        let mut current = url.clone();
        let mut chain: Vec<Url> = Vec::new();

        loop {
            let hop = chain.len();
            // Saut > 0 : toute interdiction est enveloppée dans RedirectToBlocked.
            let wrap = |r: BlockReason| {
                if hop == 0 {
                    FetchOutcome::Blocked(r)
                } else {
                    FetchOutcome::Blocked(BlockReason::RedirectToBlocked(Box::new(r)))
                }
            };
            if let Err(r) = check_url(&current, &self.policy) {
                return wrap(r);
            }
            if self.policy.respect_robots {
                if let Some(gate) = &self.robots {
                    if !gate.allows(&current) {
                        return wrap(BlockReason::Robots);
                    }
                }
            }
            let remaining = match deadline.checked_duration_since(Instant::now()) {
                Some(d) if !d.is_zero() => d,
                _ => return FetchOutcome::Blocked(BlockReason::Timeout),
            };
            // Validateurs : seulement vers l'hôte d'origine (rien ne fuit vers un autre hôte).
            let same_host = current.host_str().map(|h| h.to_string()) == origin_host;
            let (etag, lm) = if same_host { (v.etag.as_deref(), v.last_modified.as_deref()) } else { (None, None) };

            // En-têtes supplémentaires : premier saut seulement (jamais après une redirection).
            let extra_now: &[(&str, &str)] = if hop == 0 { extra } else { &[] };
            let resp = match self.send(&current, remaining, etag, lm, extra_now, deadline) {
                Ok(r) => r,
                Err(HopError::Blocked(r)) => {
                    // Un délai dépassé reste un délai, même après une redirection.
                    return if r == BlockReason::Timeout { FetchOutcome::Blocked(r) } else { wrap(r) };
                }
                Err(HopError::Failed(m)) => return FetchOutcome::Failed(m),
            };

            let status = resp.status();
            match status {
                304 => return FetchOutcome::NotModified,
                301 | 302 | 303 | 307 | 308 => {
                    if chain.len() >= self.policy.max_redirects as usize {
                        return FetchOutcome::Blocked(BlockReason::RedirectLimit);
                    }
                    let loc = match resp.header("location").map(str::trim) {
                        Some(l) if !l.is_empty() => l.to_string(),
                        _ => return FetchOutcome::Failed(format!("HTTP {status} sans en-tête Location")),
                    };
                    if loc.len() > MAX_URL_LEN {
                        return FetchOutcome::Failed("en-tête Location trop long".to_string());
                    }
                    let next = match current.join(&loc) {
                        Ok(u) => u,
                        Err(e) => return FetchOutcome::Failed(format!("Location invalide : {e}")),
                    };
                    chain.push(current);
                    current = next;
                    continue;
                }
                200..=299 => return self.read_response(resp, current, chain, deadline),
                429 | 503 => {
                    let ra =
                        resp.header("retry-after").and_then(clean_header_value).map(|r| format!(" ; Retry-After: {r}")).unwrap_or_default();
                    return FetchOutcome::Failed(format!("HTTP {status}{ra}"));
                }
                _ => return FetchOutcome::Failed(format!("HTTP {status}")),
            }
        }
    }

    /// Un saut réseau : construit la requête (en-têtes minimaux) et classe les erreurs.
    fn send(
        &self,
        url: &Url,
        remaining: Duration,
        etag: Option<&str>,
        last_modified: Option<&str>,
        extra: &[(&str, &str)],
        deadline: Instant,
    ) -> Result<ureq::Response, HopError> {
        let accept = if self.policy.allowed_content_types.is_empty() {
            "text/html".to_string()
        } else {
            self.policy.allowed_content_types.join(",")
        };
        // Un `Accept` fourni remplace celui par défaut (jamais de doublon).
        let accept = extra.iter().find(|(k, _)| k.eq_ignore_ascii_case("accept")).map(|(_, v)| v.to_string()).unwrap_or(accept);
        let mut req = self.agent.get(url.as_str()).timeout(remaining).set("User-Agent", USER_AGENT).set("Accept", &accept);
        if let Some(e) = etag.and_then(clean_header_value) {
            req = req.set("If-None-Match", &e);
        }
        if let Some(m) = last_modified.and_then(clean_header_value) {
            req = req.set("If-Modified-Since", &m);
        }
        for (k, v) in extra.iter().filter(|(k, _)| !k.eq_ignore_ascii_case("accept")) {
            req = req.set(k, v);
        }
        match req.call() {
            Ok(r) => Ok(r),
            // 4xx/5xx : c'est une réponse, classée par le code d'état.
            Err(ureq::Error::Status(_, r)) => Ok(r),
            Err(e) => Err(self.classify(&e, deadline)),
        }
    }

    fn classify(&self, e: &ureq::Error, deadline: Instant) -> HopError {
        if let Some(ip) = private_ip_in(e) {
            return HopError::Blocked(BlockReason::PrivateAddress(ip));
        }
        if is_timeout(e) || Instant::now() >= deadline {
            return HopError::Blocked(BlockReason::Timeout);
        }
        if matches!(e.kind(), ureq::ErrorKind::Dns) {
            return HopError::Blocked(BlockReason::ResolveFailed);
        }
        HopError::Failed(format!("{e}"))
    }

    /// Réponse 2xx : type contrôlé AVANT lecture, puis lecture bornée par blocs.
    fn read_response(&self, resp: ureq::Response, final_url: Url, redirects: Vec<Url>, deadline: Instant) -> FetchOutcome {
        let status = resp.status();
        let content_type = resp.header("content-type").unwrap_or("").trim().to_string();
        let essence = content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
        if !content_type_allowed(&essence, &self.policy.allowed_content_types) {
            let shown: String = essence.chars().take(100).collect();
            return FetchOutcome::Blocked(BlockReason::ContentType(shown));
        }
        let etag = resp.header("etag").and_then(clean_header_value);
        let last_modified = resp.header("last-modified").and_then(clean_header_value);

        let max = self.policy.max_body_bytes;
        // Le plafond s'applique APRÈS décompression (ureq décode gzip dans `into_reader`).
        let mut reader = resp.into_reader().take((max as u64).saturating_add(1));
        let mut body: Vec<u8> = Vec::new();
        let mut buf = vec![0u8; BLOCK];
        loop {
            if Instant::now() >= deadline {
                return FetchOutcome::Blocked(BlockReason::Timeout);
            }
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => body.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if matches!(e.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock) => {
                    return FetchOutcome::Blocked(BlockReason::Timeout);
                }
                Err(e) => return FetchOutcome::Failed(format!("lecture du corps : {e}")),
            }
        }
        let truncated = body.len() > max;
        if truncated {
            body.truncate(max);
        }
        FetchOutcome::Ok(Response { final_url, redirects, status, content_type, etag, last_modified, body, truncated })
    }
}

// ───────────────────────── Tests ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::net::TcpStream;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::thread;

    // ── Tableau d'URL : un test par cas ──

    macro_rules! url_blocked {
        ($name:ident, $url:expr, $pat:pat) => {
            #[test]
            fn $name() {
                let u = Url::parse($url).expect("URL de test valide");
                let r = check_url(&u, &Policy::default());
                assert!(matches!(r, Err($pat)), "{} -> {:?}", $url, r);
            }
        };
    }
    macro_rules! url_allowed {
        ($name:ident, $url:expr) => {
            #[test]
            fn $name() {
                let u = Url::parse($url).expect("URL de test valide");
                let r = check_url(&u, &Policy::default());
                assert!(r.is_ok(), "{} -> {:?}", $url, r);
            }
        };
    }
    macro_rules! ip_case {
        ($name:ident, $ip:expr, $public:expr) => {
            #[test]
            fn $name() {
                let ip: IpAddr = $ip.parse().unwrap();
                assert_eq!(is_public_ip(ip), $public, "{}", $ip);
            }
        };
    }

    // Loopback et ses déguisements.
    url_blocked!(u_loopback_v4, "http://127.0.0.1/", BlockReason::PrivateAddress(_));
    url_blocked!(u_loopback_v4_443, "https://127.0.0.1:443/x", BlockReason::PrivateAddress(_));
    url_blocked!(u_loopback_v4_autre, "http://127.255.255.254/", BlockReason::PrivateAddress(_));
    url_blocked!(u_loopback_v6, "http://[::1]/", BlockReason::PrivateAddress(_));
    url_blocked!(u_non_specifiee_v6, "http://[::]/", BlockReason::PrivateAddress(_));
    url_blocked!(u_non_specifiee_v4, "http://0.0.0.0/", BlockReason::PrivateAddress(_));
    url_blocked!(u_mappee_decimale, "http://[::ffff:127.0.0.1]/", BlockReason::PrivateAddress(_));
    url_blocked!(u_mappee_hexa, "http://[::ffff:7f00:1]/", BlockReason::PrivateAddress(_));
    url_blocked!(u_mappee_metadata, "http://[::ffff:169.254.169.254]/", BlockReason::PrivateAddress(_));
    url_blocked!(u_nat64_loopback, "http://[64:ff9b::7f00:1]/", BlockReason::PrivateAddress(_));
    url_blocked!(u_nat64_prive, "http://[64:ff9b::a00:1]/", BlockReason::PrivateAddress(_));
    url_blocked!(u_6to4_loopback, "http://[2002:7f00:1::]/", BlockReason::PrivateAddress(_));
    url_blocked!(u_6to4_metadata, "http://[2002:a9fe:a9fe::1]/", BlockReason::PrivateAddress(_));
    url_blocked!(u_decimale, "http://2130706433/", BlockReason::PrivateAddress(_));
    url_blocked!(u_hexadecimale, "http://0x7f000001/", BlockReason::PrivateAddress(_));
    url_blocked!(u_octale, "http://0177.0.0.1/", BlockReason::PrivateAddress(_));
    url_blocked!(u_courte, "http://127.1/", BlockReason::PrivateAddress(_));
    url_blocked!(u_hexa_pointee, "http://0x7f.0.0.1/", BlockReason::PrivateAddress(_));
    url_blocked!(u_metadata_aws, "http://169.254.169.254/latest/meta-data/", BlockReason::PrivateAddress(_));
    url_blocked!(u_cgnat, "http://100.64.0.1/", BlockReason::PrivateAddress(_));
    url_blocked!(u_rfc1918_10, "http://10.0.0.1/", BlockReason::PrivateAddress(_));
    url_blocked!(u_rfc1918_172, "http://172.16.5.4/", BlockReason::PrivateAddress(_));
    url_blocked!(u_rfc1918_192, "http://192.168.1.1/", BlockReason::PrivateAddress(_));
    url_blocked!(u_ula, "http://[fc00::1]/", BlockReason::PrivateAddress(_));
    url_blocked!(u_ula_fd, "http://[fd12:3456::1]/", BlockReason::PrivateAddress(_));
    url_blocked!(u_lien_local_v6, "http://[fe80::1]/", BlockReason::PrivateAddress(_));
    url_blocked!(u_multicast_v6, "http://[ff02::1]/", BlockReason::PrivateAddress(_));
    url_blocked!(u_documentation_v6, "http://[2001:db8::1]/", BlockReason::PrivateAddress(_));
    url_blocked!(u_teredo, "http://[2001:0:4136:e378:8000:63bf:3fff:fdd2]/", BlockReason::PrivateAddress(_));
    url_blocked!(u_discard_v6, "http://[100::1]/", BlockReason::PrivateAddress(_));
    url_blocked!(u_broadcast, "http://255.255.255.255/", BlockReason::PrivateAddress(_));
    url_blocked!(u_multicast_v4, "http://224.0.0.1/", BlockReason::PrivateAddress(_));
    url_blocked!(u_reserve_240, "http://240.0.0.1/", BlockReason::PrivateAddress(_));
    // Identifiants, schémas, ports.
    url_blocked!(u_userinfo_mdp, "http://user:pass@example.com/", BlockReason::Credentials);
    url_blocked!(u_userinfo_seul, "https://user@example.com/", BlockReason::Credentials);
    url_blocked!(u_mdp_seul, "https://:pass@example.com/", BlockReason::Credentials);
    url_blocked!(u_file, "file:///etc/passwd", BlockReason::Scheme(_));
    url_blocked!(u_ftp, "ftp://example.com/x", BlockReason::Scheme(_));
    url_blocked!(u_gopher, "gopher://example.com/", BlockReason::Scheme(_));
    url_blocked!(u_data, "data:text/html,hello", BlockReason::Scheme(_));
    url_blocked!(u_javascript, "javascript:alert(1)", BlockReason::Scheme(_));
    url_blocked!(u_port_22, "http://example.com:22/", BlockReason::Port(22));
    url_blocked!(u_port_8443, "https://example.com:8443/", BlockReason::Port(8443));
    url_blocked!(u_port_8080, "http://example.com:8080/", BlockReason::Port(8080));
    // Noms refusés.
    url_blocked!(u_localhost, "http://localhost/", BlockReason::PrivateAddress(_));
    url_blocked!(u_localhost_point, "http://localhost./", BlockReason::PrivateAddress(_));
    url_blocked!(u_localhost_majuscules, "http://LOCALHOST/", BlockReason::PrivateAddress(_));
    url_blocked!(u_localhost_pleine_chasse, "http://\u{ff4c}ocalhost/", BlockReason::PrivateAddress(_));
    url_blocked!(u_sous_domaine_localhost, "http://foo.localhost/", BlockReason::PrivateAddress(_));
    url_blocked!(u_metadata_gcp, "http://metadata.google.internal/computeMetadata/v1/", BlockReason::PrivateAddress(_));
    url_blocked!(u_dot_local, "http://imprimante.local/", BlockReason::PrivateAddress(_));
    url_blocked!(u_dot_internal, "http://db.internal/", BlockReason::PrivateAddress(_));
    url_blocked!(u_dot_localdomain, "http://hote.localdomain/", BlockReason::PrivateAddress(_));
    // Autorisés.
    url_allowed!(u_ok_http, "http://example.com/");
    url_allowed!(u_ok_https_requete, "https://example.com/a/b?c=d#e");
    url_allowed!(u_ok_port_443_explicite, "https://example.com:443/");
    url_allowed!(u_ok_port_80_explicite, "http://example.com:80/");
    url_allowed!(u_ok_ipv4_publique, "http://93.184.216.34/");
    url_allowed!(u_ok_ipv6_publique, "https://[2606:4700:4700::1111]/");
    url_allowed!(u_ok_mappee_publique, "http://[::ffff:8.8.8.8]/");
    url_allowed!(u_ok_nat64_publique, "http://[64:ff9b::808:808]/");
    url_allowed!(u_ok_6to4_publique, "http://[2002:808:808::1]/");
    url_allowed!(u_ok_nom_proche_de_local, "http://localhost.example.com/");
    url_allowed!(u_ok_nom_contient_internal, "http://internal.example.com/");

    // is_public_ip directement.
    ip_case!(i_v4_publique, "8.8.8.8", true);
    ip_case!(i_v4_172_32_publique, "172.32.0.1", true);
    ip_case!(i_v4_100_63_publique, "100.63.255.255", true);
    ip_case!(i_v4_100_128_publique, "100.128.0.1", true);
    ip_case!(i_v4_198_20_publique, "198.20.0.1", true);
    ip_case!(i_v4_cgnat_haut, "100.127.255.255", false);
    ip_case!(i_v4_172_15_publique, "172.15.0.1", true);
    ip_case!(i_v4_172_31, "172.31.255.255", false);
    ip_case!(i_v4_test_net_1, "192.0.2.10", false);
    ip_case!(i_v4_test_net_2, "198.51.100.10", false);
    ip_case!(i_v4_test_net_3, "203.0.113.10", false);
    ip_case!(i_v4_192_0_0, "192.0.0.8", false);
    ip_case!(i_v4_bench, "198.19.255.255", false);
    ip_case!(i_v6_publique, "2001:4860:4860::8888", true);
    ip_case!(i_v6_compat_v4, "::8.8.8.8", false);
    ip_case!(i_v6_site_local, "fec0::1", false);
    ip_case!(i_v6_doc_3fff, "3fff::1", false);

    // ── Politique et utilitaires ──

    #[test]
    fn politique_par_defaut() {
        let p = Policy::default();
        assert_eq!(p.allowed_ports, vec![80, 443]);
        assert_eq!(p.max_body_bytes, 5 * 1024 * 1024);
        assert_eq!(p.max_redirects, 5);
        assert_eq!(p.connect_timeout, Duration::from_secs(10));
        assert_eq!(p.total_timeout, Duration::from_secs(30));
        assert!(p.respect_robots);
        assert!(p.allow_addrs.is_empty());
        assert_eq!(p.allowed_content_types.len(), 9);
    }

    #[test]
    fn ports_vides_equivalent_a_80_443() {
        let mut p = Policy::default();
        p.allowed_ports.clear();
        assert!(check_url(&Url::parse("http://example.com/").unwrap(), &p).is_ok());
        assert!(check_url(&Url::parse("https://example.com/").unwrap(), &p).is_ok());
        assert_eq!(check_url(&Url::parse("http://example.com:81/").unwrap(), &p), Err(BlockReason::Port(81)));
    }

    #[test]
    fn allow_addrs_tolere_seulement_l_adresse_exacte() {
        let mut p = Policy::default();
        p.allow_addrs.push("127.0.0.1:9000".parse().unwrap());
        assert!(check_url(&Url::parse("http://127.0.0.1:9000/").unwrap(), &p).is_ok());
        assert!(check_url(&Url::parse("http://127.0.0.1:9001/").unwrap(), &p).is_err());
        assert!(check_url(&Url::parse("http://127.0.0.2:9000/").unwrap(), &p).is_err());
    }

    #[test]
    fn types_mime_par_prefixe_et_parametres() {
        let a = Policy::default().allowed_content_types;
        assert!(content_type_allowed("text/html", &a));
        assert!(content_type_allowed("application/json", &a));
        assert!(!content_type_allowed("text/htmlx", &a));
        assert!(!content_type_allowed("image/png", &a));
        assert!(!content_type_allowed("application/octet-stream", &a));
        assert!(!content_type_allowed("", &a));
        assert!(content_type_allowed("text/css", &["text/*".to_string()]));
        assert!(content_type_allowed("text/css", &["text/".to_string()]));
    }

    #[test]
    fn url_geante_sans_panique() {
        let f = Fetcher::new(Policy::default());
        let big = format!("http://example.com/{}", "a".repeat(200_000));
        let u = Url::parse(&big).unwrap();
        assert!(matches!(f.fetch(&u, &Validators::default()), FetchOutcome::Failed(_)));
        // check_url seul ne panique pas non plus.
        assert!(check_url(&u, &Policy::default()).is_ok());
    }

    #[test]
    fn fetch_refuse_schema_et_nom_avant_tout_reseau() {
        let f = Fetcher::new(Policy::default());
        let v = Validators::default();
        let r = f.fetch(&Url::parse("file:///etc/passwd").unwrap(), &v);
        assert!(matches!(r, FetchOutcome::Blocked(BlockReason::Scheme(_))), "{r:?}");
        let r = f.fetch(&Url::parse("http://169.254.169.254/").unwrap(), &v);
        assert!(matches!(r, FetchOutcome::Blocked(BlockReason::PrivateAddress(_))), "{r:?}");
    }

    // ── Résolveur filtrant ──

    #[test]
    fn resolveur_refuse_loopback() {
        let e = resolve_public("127.0.0.1:80", &[]).unwrap_err();
        let inner = e.get_ref().and_then(|i| i.downcast_ref::<PrivateHost>()).expect("marque PrivateHost");
        assert_eq!(inner.0, "127.0.0.1".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn resolveur_refuse_nom_resolu_en_loopback() {
        // « localhost » est résolu par le fichier hosts local (pas de DNS public).
        if let Ok(addrs) = resolve_public("localhost:80", &[]) {
            panic!("localhost aurait dû être refusé : {addrs:?}");
        }
    }

    #[test]
    fn resolveur_tolere_allow_addrs_et_adresse_publique() {
        let allow: SocketAddr = "127.0.0.1:9000".parse().unwrap();
        assert_eq!(resolve_public("127.0.0.1:9000", &[allow]).unwrap(), vec![allow]);
        assert!(resolve_public("127.0.0.1:9001", &[allow]).is_err());
        assert_eq!(resolve_public("8.8.8.8:443", &[]).unwrap().len(), 1);
    }

    #[test]
    fn resolveur_refuse_l_hote_entier_si_une_adresse_est_privee() {
        let addrs: Vec<SocketAddr> = vec!["8.8.8.8:80".parse().unwrap(), "10.0.0.1:80".parse().unwrap()];
        let e = check_addrs(&addrs, &[]).unwrap_err();
        assert_eq!(e.get_ref().and_then(|i| i.downcast_ref::<PrivateHost>()).unwrap().0, "10.0.0.1".parse::<IpAddr>().unwrap());
        // Même ordre inversé : une seule adresse privée suffit, où qu'elle soit.
        let inv: Vec<SocketAddr> = vec!["10.0.0.1:80".parse().unwrap(), "8.8.8.8:80".parse().unwrap()];
        assert!(check_addrs(&inv, &[]).is_err());
        let pubs: Vec<SocketAddr> = vec!["8.8.8.8:80".parse().unwrap(), "[2606:4700:4700::1111]:80".parse().unwrap()];
        assert!(check_addrs(&pubs, &[]).is_ok());
    }

    // ── Serveur local de test ──

    struct Srv {
        addr: SocketAddr,
        reqs: Arc<Mutex<Vec<String>>>,
    }

    impl Srv {
        fn url(&self, path: &str) -> Url {
            Url::parse(&format!("http://{}{}", self.addr, path)).unwrap()
        }
        fn count(&self) -> usize {
            self.reqs.lock().unwrap().len()
        }
        fn heads(&self) -> Vec<String> {
            self.reqs.lock().unwrap().clone()
        }
    }

    fn read_head(s: &mut TcpStream) -> String {
        s.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let mut buf = Vec::new();
        let mut b = [0u8; 1];
        while !buf.ends_with(b"\r\n\r\n") && buf.len() < 65536 {
            match s.read(&mut b) {
                Ok(1) => buf.push(b[0]),
                _ => break,
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    fn spawn<F>(f: F) -> Srv
    where
        F: Fn(&str, &mut TcpStream) + Send + Sync + 'static,
    {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let reqs = Arc::new(Mutex::new(Vec::new()));
        let f = Arc::new(f);
        let r2 = reqs.clone();
        thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { continue };
                let f = f.clone();
                let r = r2.clone();
                thread::spawn(move || {
                    let head = read_head(&mut s);
                    r.lock().unwrap().push(head.clone());
                    f(&head, &mut s);
                    let _ = s.flush();
                });
            }
        });
        Srv { addr, reqs }
    }

    fn path_of(head: &str) -> String {
        head.split_whitespace().nth(1).unwrap_or("/").to_string()
    }

    fn has_header(head: &str, name: &str) -> bool {
        let prefix = format!("{}:", name.to_ascii_lowercase());
        head.lines().any(|l| l.to_ascii_lowercase().starts_with(&prefix))
    }

    fn header_value(head: &str, name: &str) -> Option<String> {
        let prefix = format!("{}:", name.to_ascii_lowercase());
        head.lines().find(|l| l.to_ascii_lowercase().starts_with(&prefix)).map(|l| l[prefix.len()..].trim().to_string())
    }

    fn http(status: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
        let mut head = format!("HTTP/1.1 {status}\r\n");
        for (k, v) in headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str(&format!("Content-Length: {}\r\nConnection: close\r\n\r\n", body.len()));
        let mut out = head.into_bytes();
        out.extend_from_slice(body);
        out
    }

    fn html(body: &str) -> Vec<u8> {
        http("200 OK", &[("Content-Type", "text/html; charset=utf-8")], body.as_bytes())
    }

    fn pol(srv: &Srv) -> Policy {
        let mut p = Policy::default();
        p.allow_addrs.push(srv.addr);
        p.total_timeout = Duration::from_secs(10);
        p
    }

    fn fetch(srv: &Srv, path: &str) -> FetchOutcome {
        Fetcher::new(pol(srv)).fetch(&srv.url(path), &Validators::default())
    }

    fn ok(o: FetchOutcome) -> Response {
        match o {
            FetchOutcome::Ok(r) => r,
            other => panic!("attendu Ok, obtenu {other:?}"),
        }
    }

    // ── Serveur : cas nominaux et en-têtes ──

    #[test]
    fn srv_ok_simple_et_champs() {
        let s = spawn(|_, st| {
            let _ = st.write_all(&http(
                "200 OK",
                &[("Content-Type", "text/html; charset=utf-8"), ("ETag", "\"v1\""), ("Last-Modified", "Wed, 01 Jan 2025 00:00:00 GMT")],
                b"<p>bonjour</p>",
            ));
        });
        let r = ok(fetch(&s, "/page"));
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"<p>bonjour</p>");
        assert!(!r.truncated);
        assert!(r.redirects.is_empty());
        assert_eq!(r.final_url.path(), "/page");
        assert_eq!(r.etag.as_deref(), Some("\"v1\""));
        assert_eq!(r.last_modified.as_deref(), Some("Wed, 01 Jan 2025 00:00:00 GMT"));
        assert_eq!(r.content_type, "text/html; charset=utf-8");
    }

    #[test]
    fn srv_en_tetes_minimaux_sans_cookie_referer_authorization() {
        let s = spawn(|_, st| {
            let _ = st.write_all(&http("200 OK", &[("Content-Type", "text/plain"), ("Set-Cookie", "sid=secret; Path=/")], b"x"));
        });
        let _ = ok(fetch(&s, "/"));
        let h = &s.heads()[0];
        assert_eq!(header_value(h, "user-agent").as_deref(), Some(USER_AGENT));
        assert!(has_header(h, "accept"));
        for banni in ["cookie", "referer", "authorization", "proxy-authorization"] {
            assert!(!has_header(h, banni), "en-tête interdit présent : {banni}\n{h}");
        }
    }

    #[test]
    fn srv_pas_de_cookie_renvoye_apres_redirection() {
        let s = spawn(|head, st| {
            let out = if path_of(head) == "/a" {
                http("302 Found", &[("Location", "/b"), ("Set-Cookie", "sid=1")], b"")
            } else {
                http("200 OK", &[("Content-Type", "text/plain")], b"fin")
            };
            let _ = st.write_all(&out);
        });
        let r = ok(fetch(&s, "/a"));
        assert_eq!(r.body, b"fin");
        for h in s.heads() {
            assert!(!has_header(&h, "cookie"), "{h}");
            assert!(!has_header(&h, "referer"), "{h}");
        }
    }

    // ── En-têtes supplémentaires (fetch_with_headers) ──

    fn ok_server() -> Srv {
        spawn(|_, st| {
            let _ = st.write_all(&http("200 OK", &[("Content-Type", "application/json")], b"{}"));
        })
    }

    fn fetch_h(srv: &Srv, path: &str, extra: &[(&str, &str)]) -> FetchOutcome {
        Fetcher::new(pol(srv)).fetch_with_headers(&srv.url(path), &Validators::default(), extra)
    }

    #[test]
    fn hdr_en_tetes_autorises_envoyes_au_premier_saut() {
        let s = ok_server();
        let r = ok(fetch_h(&s, "/", &[("X-Subscription-Token", "tok-0123456789"), ("Accept", "application/json")]));
        assert_eq!(r.body, b"{}");
        let h = &s.heads()[0];
        assert_eq!(header_value(h, "x-subscription-token").as_deref(), Some("tok-0123456789"));
        assert_eq!(header_value(h, "accept").as_deref(), Some("application/json"), "Accept remplacé, pas doublé");
        assert_eq!(h.to_ascii_lowercase().matches("\r\naccept:").count(), 1, "{h}");
        assert_eq!(header_value(h, "user-agent").as_deref(), Some(USER_AGENT));
        for banni in ["cookie", "referer", "proxy-authorization"] {
            assert!(!has_header(h, banni), "{banni}\n{h}");
        }
        let s2 = ok_server();
        let _ = ok(fetch_h(&s2, "/", &[("authorization", "Bearer ghp_abcdef012345"), ("X-Api-Key", "k-0123456789")]));
        let h2 = &s2.heads()[0];
        assert_eq!(header_value(h2, "authorization").as_deref(), Some("Bearer ghp_abcdef012345"));
        assert_eq!(header_value(h2, "x-api-key").as_deref(), Some("k-0123456789"));
    }

    #[test]
    fn hdr_sans_extra_aucun_en_tete_d_auth_par_defaut() {
        let s = ok_server();
        let _ = ok(fetch_h(&s, "/", &[]));
        let h = &s.heads()[0];
        for banni in ["cookie", "referer", "authorization", "proxy-authorization", "x-subscription-token", "x-api-key"] {
            assert!(!has_header(h, banni), "{banni}\n{h}");
        }
    }

    #[test]
    fn hdr_noms_hors_liste_blanche_refuses_sans_reseau() {
        let s = ok_server();
        for nom in [
            "Cookie",
            "Host",
            "Proxy-Authorization",
            "Proxy-Connection",
            "Referer",
            "X-Forwarded-For",
            "X-Inconnu",
            "Set-Cookie",
            "",
            "Au\r\nthorization",
            "X-Mojeek-Key",
        ] {
            let r = fetch_h(&s, "/", &[(nom, "valeur-secrete-0123")]);
            match r {
                FetchOutcome::Failed(m) => assert!(!m.contains("valeur-secrete-0123"), "valeur dans le message : {m}"),
                autre => panic!("{nom:?} aurait dû être refusé : {autre:?}"),
            }
        }
        assert_eq!(s.count(), 0, "aucune requête ne doit partir");
    }

    #[test]
    fn hdr_valeurs_hostiles_refusees_sans_reseau() {
        let s = ok_server();
        let longue = "x".repeat(2000);
        for val in ["a\r\nCookie: sid=1", "a\nb", "a\rb", "a\0b", "", "é-non-ascii-0123", longue.as_str()] {
            let r = fetch_h(&s, "/", &[("Authorization", val)]);
            assert!(matches!(r, FetchOutcome::Failed(_)), "{val:?} -> {r:?}");
        }
        // Doublon de nom : refusé.
        let r = fetch_h(&s, "/", &[("Authorization", "a-0123456"), ("authorization", "b-0123456")]);
        assert!(matches!(r, FetchOutcome::Failed(_)), "{r:?}");
        assert_eq!(s.count(), 0);
    }

    #[test]
    fn hdr_jamais_retransmis_apres_redirection_vers_un_autre_serveur() {
        let b = ok_server();
        let b_url = b.url("/final").to_string();
        let a = spawn(move |_, st| {
            let _ = st.write_all(&http("302 Found", &[("Location", &b_url)], b""));
        });
        let mut p = pol(&a);
        p.allow_addrs.push(b.addr);
        let f = Fetcher::new(p);
        let extra =
            [("Authorization", "Bearer secret-0123456789"), ("X-Subscription-Token", "tok-0123456789"), ("X-Api-Key", "key-0123456789")];
        let r = ok(f.fetch_with_headers(&a.url("/start"), &Validators::default(), &extra));
        assert_eq!(r.body, b"{}");
        assert_eq!(r.redirects.len(), 1);
        assert!(has_header(&a.heads()[0], "authorization"), "le premier saut doit porter l'en-tête");
        let hb = &b.heads()[0];
        for nom in ["authorization", "x-subscription-token", "x-api-key", "cookie", "referer"] {
            assert!(!has_header(hb, nom), "{nom} retransmis au second serveur\n{hb}");
        }
        assert!(!hb.contains("secret-0123456789") && !hb.contains("tok-0123456789"), "{hb}");
    }

    #[test]
    fn hdr_jamais_retransmis_apres_redirection_meme_hote() {
        let s = spawn(|head, st| {
            let out = if path_of(head) == "/a" {
                http("302 Found", &[("Location", "/b")], b"")
            } else {
                http("200 OK", &[("Content-Type", "application/json")], b"{}")
            };
            let _ = st.write_all(&out);
        });
        let _ = ok(fetch_h(&s, "/a", &[("Authorization", "Bearer secret-0123456789")]));
        let heads = s.heads();
        assert_eq!(heads.len(), 2);
        assert!(has_header(&heads[0], "authorization"));
        assert!(!has_header(&heads[1], "authorization"), "{}", heads[1]);
    }

    #[test]
    fn hdr_valeur_absente_des_messages_d_erreur() {
        // HTTP 401 : le message ne contient pas la valeur envoyée.
        let s = spawn(|_, st| {
            let _ = st.write_all(&http("401 Unauthorized", &[("Content-Type", "text/plain")], b"non"));
        });
        match fetch_h(&s, "/", &[("Authorization", "Bearer secret-0123456789")]) {
            FetchOutcome::Failed(m) => assert!(!m.contains("secret-0123456789"), "{m}"),
            autre => panic!("{autre:?}"),
        }
        // L'expurgation elle-même (filet pour tout message qui en porterait une).
        let m = redact_values("échec avec Bearer secret-0123456789 dedans", &[("Authorization", "Bearer secret-0123456789")]);
        assert!(!m.contains("secret-0123456789"), "{m}");
        // Cible interdite : Blocked, sans valeur non plus.
        let r = Fetcher::new(Policy::default()).fetch_with_headers(
            &Url::parse("http://127.0.0.1/").unwrap(),
            &Validators::default(),
            &[("Authorization", "Bearer secret-0123456789")],
        );
        assert!(matches!(r, FetchOutcome::Blocked(_)), "{r:?}");
        assert!(!format!("{r:?}").contains("secret-0123456789"));
    }

    #[test]
    fn srv_304_et_validateurs_envoyes() {
        let s = spawn(|head, st| {
            let cond = has_header(head, "if-none-match") && has_header(head, "if-modified-since");
            let out = if cond { http("304 Not Modified", &[], b"") } else { html("plein") };
            let _ = st.write_all(&out);
        });
        let v = Validators { etag: Some("\"abc\"".into()), last_modified: Some("Wed, 01 Jan 2025 00:00:00 GMT".into()) };
        let o = Fetcher::new(pol(&s)).fetch(&s.url("/"), &v);
        assert!(matches!(o, FetchOutcome::NotModified), "{o:?}");
        let h = &s.heads()[0];
        assert_eq!(header_value(h, "if-none-match").as_deref(), Some("\"abc\""));
        assert_eq!(header_value(h, "if-modified-since").as_deref(), Some("Wed, 01 Jan 2025 00:00:00 GMT"));
    }

    #[test]
    fn srv_validateurs_hostiles_non_envoyes() {
        let s = spawn(|_, st| {
            let _ = st.write_all(&html("x"));
        });
        let v = Validators { etag: Some("a\r\nX-Injecte: 1".into()), last_modified: Some("x".repeat(5000)) };
        let _ = ok(Fetcher::new(pol(&s)).fetch(&s.url("/"), &v));
        let h = &s.heads()[0];
        assert!(!has_header(h, "if-none-match"));
        assert!(!has_header(h, "if-modified-since"));
        assert!(!has_header(h, "x-injecte"));
    }

    #[test]
    fn srv_chunked() {
        let s = spawn(|_, st| {
            let _ = st.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
            let _ = st.write_all(b"5\r\nhello\r\n");
            let _ = st.write_all(b"7\r\n, monde\r\n");
            let _ = st.write_all(b"0\r\n\r\n");
        });
        let r = ok(fetch(&s, "/"));
        assert_eq!(r.body, b"hello, monde");
        assert!(!r.truncated);
    }

    // ── Statuts ──

    #[test]
    fn srv_404_failed() {
        let s = spawn(|_, st| {
            let _ = st.write_all(&http("404 Not Found", &[("Content-Type", "text/html")], b"non"));
        });
        match fetch(&s, "/") {
            FetchOutcome::Failed(m) => assert!(m.contains("404"), "{m}"),
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn srv_500_failed() {
        let s = spawn(|_, st| {
            let _ = st.write_all(&http("500 Internal Server Error", &[], b""));
        });
        match fetch(&s, "/") {
            FetchOutcome::Failed(m) => assert!(m.contains("500"), "{m}"),
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn srv_429_retry_after() {
        let s = spawn(|_, st| {
            let _ = st.write_all(&http("429 Too Many Requests", &[("Retry-After", "120")], b""));
        });
        match fetch(&s, "/") {
            FetchOutcome::Failed(m) => assert!(m.contains("429") && m.contains("120"), "{m}"),
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn srv_503_retry_after_date() {
        let s = spawn(|_, st| {
            let _ = st.write_all(&http("503 Service Unavailable", &[("Retry-After", "Wed, 21 Oct 2026 07:28:00 GMT")], b""));
        });
        match fetch(&s, "/") {
            FetchOutcome::Failed(m) => assert!(m.contains("503") && m.contains("Wed, 21 Oct 2026"), "{m}"),
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn srv_401_jamais_d_authorization() {
        let s = spawn(|_, st| {
            let _ = st.write_all(&http("401 Unauthorized", &[("WWW-Authenticate", "Basic realm=x")], b""));
        });
        assert!(matches!(fetch(&s, "/"), FetchOutcome::Failed(_)));
        assert_eq!(s.count(), 1, "aucune seconde requête avec identifiants");
        assert!(!has_header(&s.heads()[0], "authorization"));
    }

    // ── Types MIME ──

    #[test]
    fn srv_image_png_bloque() {
        let s = spawn(|_, st| {
            let _ = st.write_all(&http("200 OK", &[("Content-Type", "image/png")], &[0x89, b'P', b'N', b'G']));
        });
        match fetch(&s, "/") {
            FetchOutcome::Blocked(BlockReason::ContentType(t)) => assert_eq!(t, "image/png"),
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn srv_type_absent_bloque() {
        let s = spawn(|_, st| {
            let _ = st.write_all(&http("200 OK", &[], b"sans type"));
        });
        assert!(matches!(fetch(&s, "/"), FetchOutcome::Blocked(BlockReason::ContentType(_))));
    }

    #[test]
    fn srv_octet_stream_bloque() {
        let s = spawn(|_, st| {
            let _ = st.write_all(&http("200 OK", &[("Content-Type", "application/octet-stream")], b"x"));
        });
        assert!(matches!(fetch(&s, "/"), FetchOutcome::Blocked(BlockReason::ContentType(_))));
    }

    #[test]
    fn srv_type_majuscules_et_parametres_acceptes() {
        let s = spawn(|_, st| {
            let _ = st.write_all(&http("200 OK", &[("Content-Type", "TEXT/HTML ; Charset=ISO-8859-1")], b"ok"));
        });
        let r = ok(fetch(&s, "/"));
        assert_eq!(r.body, b"ok");
    }

    #[test]
    fn srv_type_verifie_avant_de_lire_le_corps() {
        // Le corps annoncé est énorme mais jamais envoyé : si on le lisait, on attendrait le délai.
        let s = spawn(|_, st| {
            let _ = st.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nContent-Length: 999999999\r\nConnection: close\r\n\r\n");
            let _ = st.flush();
            thread::sleep(Duration::from_secs(8));
        });
        let t = Instant::now();
        assert!(matches!(fetch(&s, "/"), FetchOutcome::Blocked(BlockReason::ContentType(_))));
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    }

    // ── Plafond de taille ──

    #[test]
    fn srv_corps_plus_gros_que_le_plafond() {
        let s = spawn(|_, st| {
            let _ = st.write_all(&http("200 OK", &[("Content-Type", "text/plain")], &vec![b'a'; 100_000]));
        });
        let mut p = pol(&s);
        p.max_body_bytes = 10_000;
        let r = ok(Fetcher::new(p).fetch(&s.url("/"), &Validators::default()));
        assert!(r.truncated);
        assert_eq!(r.body.len(), 10_000);
    }

    #[test]
    fn srv_corps_exactement_au_plafond_non_tronque() {
        let s = spawn(|_, st| {
            let _ = st.write_all(&http("200 OK", &[("Content-Type", "text/plain")], &vec![b'a'; 10_000]));
        });
        let mut p = pol(&s);
        p.max_body_bytes = 10_000;
        let r = ok(Fetcher::new(p).fetch(&s.url("/"), &Validators::default()));
        assert!(!r.truncated);
        assert_eq!(r.body.len(), 10_000);
    }

    #[test]
    fn srv_corps_vide() {
        let s = spawn(|_, st| {
            let _ = st.write_all(&http("200 OK", &[("Content-Type", "text/plain")], b""));
        });
        let r = ok(fetch(&s, "/"));
        assert!(r.body.is_empty());
        assert!(!r.truncated);
    }

    #[test]
    fn srv_bombe_gzip_plafond_tenu() {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        // 8 Mio de zéros -> quelques Kio compressés ; plafond à 100 Kio.
        let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
        let zeros = vec![0u8; 1024 * 1024];
        for _ in 0..8 {
            enc.write_all(&zeros).unwrap();
        }
        let gz = Arc::new(enc.finish().unwrap());
        assert!(gz.len() < 100_000, "la bombe doit être petite sur le fil : {}", gz.len());
        let gz2 = gz.clone();
        let s = spawn(move |_, st| {
            let _ = st.write_all(&http("200 OK", &[("Content-Type", "text/plain"), ("Content-Encoding", "gzip")], &gz2));
        });
        let mut p = pol(&s);
        p.max_body_bytes = 100_000;
        let r = ok(Fetcher::new(p).fetch(&s.url("/"), &Validators::default()));
        assert!(r.truncated);
        assert_eq!(r.body.len(), 100_000);
        assert!(r.body.iter().all(|b| *b == 0));
    }

    #[test]
    fn srv_gzip_normal_decompresse() {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(b"<h1>compresse</h1>").unwrap();
        let gz = Arc::new(enc.finish().unwrap());
        let s = spawn(move |_, st| {
            let _ = st.write_all(&http("200 OK", &[("Content-Type", "text/html"), ("Content-Encoding", "gzip")], &gz));
        });
        let r = ok(fetch(&s, "/"));
        assert_eq!(r.body, b"<h1>compresse</h1>");
        assert!(!r.truncated);
    }

    // ── Délais ──

    #[test]
    fn srv_serveur_qui_dort_echoue_vite() {
        let s = spawn(|_, _| {
            thread::sleep(Duration::from_secs(8));
        });
        let mut p = pol(&s);
        p.total_timeout = Duration::from_secs(1);
        let t = Instant::now();
        let o = Fetcher::new(p).fetch(&s.url("/"), &Validators::default());
        let dt = t.elapsed();
        assert!(matches!(o, FetchOutcome::Blocked(BlockReason::Timeout) | FetchOutcome::Failed(_)), "{o:?}");
        assert!(dt < Duration::from_secs(3), "trop long : {dt:?}");
    }

    #[test]
    fn srv_goutte_a_goutte_echoue_vite() {
        let s = spawn(|_, st| {
            let _ = st.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 100000\r\nConnection: close\r\n\r\n");
            for _ in 0..200 {
                if st.write_all(b"x").is_err() || st.flush().is_err() {
                    return;
                }
                thread::sleep(Duration::from_millis(100));
            }
        });
        let mut p = pol(&s);
        p.total_timeout = Duration::from_secs(1);
        let t = Instant::now();
        let o = Fetcher::new(p).fetch(&s.url("/"), &Validators::default());
        let dt = t.elapsed();
        assert!(matches!(o, FetchOutcome::Blocked(BlockReason::Timeout) | FetchOutcome::Failed(_)), "{o:?}");
        assert!(dt < Duration::from_secs(3), "trop long : {dt:?}");
    }

    #[test]
    fn srv_connexion_fermee_sans_reponse_failed() {
        let s = spawn(|_, _| {});
        let o = fetch(&s, "/");
        assert!(matches!(o, FetchOutcome::Failed(_)), "{o:?}");
    }

    #[test]
    fn srv_reponse_garbage_sans_panique() {
        let s = spawn(|_, st| {
            let _ = st.write_all(b"\x00\xff\xfe pas du HTTP \r\n\r\n");
        });
        assert!(matches!(fetch(&s, "/"), FetchOutcome::Failed(_) | FetchOutcome::Blocked(_)));
    }

    // ── Redirections ──

    #[test]
    fn red_relative_suivie() {
        let s = spawn(|head, st| {
            let out = match path_of(head).as_str() {
                "/a" => http("302 Found", &[("Location", "/b/c")], b""),
                "/b/c" => html("arrivé"),
                _ => http("404 Not Found", &[], b""),
            };
            let _ = st.write_all(&out);
        });
        let r = ok(fetch(&s, "/a"));
        assert_eq!(r.final_url.path(), "/b/c");
        assert_eq!(r.redirects.len(), 1);
        assert_eq!(r.redirects[0].path(), "/a");
        assert_eq!(r.body, "arrivé".as_bytes());
    }

    #[test]
    fn red_tous_les_codes() {
        for code in ["301 Moved Permanently", "302 Found", "303 See Other", "307 Temporary Redirect", "308 Permanent Redirect"] {
            let s = spawn(move |head, st| {
                let out = if path_of(head) == "/a" { http(code, &[("Location", "/b")], b"") } else { html("ok") };
                let _ = st.write_all(&out);
            });
            let r = ok(fetch(&s, "/a"));
            assert_eq!(r.final_url.path(), "/b", "{code}");
        }
    }

    #[test]
    fn red_vers_second_serveur_local_non_liste() {
        let hits = Arc::new(AtomicUsize::new(0));
        let h2 = hits.clone();
        let b = spawn(move |_, st| {
            h2.fetch_add(1, Ordering::SeqCst);
            let _ = st.write_all(&html("secret interne"));
        });
        let target = b.url("/secret").to_string();
        let a = spawn(move |_, st| {
            let _ = st.write_all(&http("302 Found", &[("Location", &target)], b""));
        });
        let o = fetch(&a, "/");
        match o {
            FetchOutcome::Blocked(BlockReason::RedirectToBlocked(inner)) => {
                assert_eq!(*inner, BlockReason::PrivateAddress("127.0.0.1".parse().unwrap()));
            }
            o => panic!("{o:?}"),
        }
        assert_eq!(hits.load(Ordering::SeqCst), 0, "le second serveur ne doit jamais être contacté");
    }

    #[test]
    fn red_vers_metadata_cloud() {
        let a = spawn(|_, st| {
            let _ = st.write_all(&http("302 Found", &[("Location", "http://169.254.169.254/latest/meta-data/")], b""));
        });
        match fetch(&a, "/") {
            FetchOutcome::Blocked(BlockReason::RedirectToBlocked(inner)) => {
                assert_eq!(*inner, BlockReason::PrivateAddress("169.254.169.254".parse().unwrap()));
            }
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn red_vers_file_et_ftp() {
        for loc in ["file:///etc/passwd", "ftp://example.com/x", "gopher://example.com/"] {
            let a = spawn(move |_, st| {
                let _ = st.write_all(&http("301 Moved Permanently", &[("Location", loc)], b""));
            });
            match fetch(&a, "/") {
                FetchOutcome::Blocked(BlockReason::RedirectToBlocked(inner)) => {
                    assert!(matches!(*inner, BlockReason::Scheme(_)), "{loc}: {inner:?}");
                }
                o => panic!("{loc}: {o:?}"),
            }
        }
    }

    #[test]
    fn red_vers_port_interdit_et_identifiants() {
        let a = spawn(|_, st| {
            let _ = st.write_all(&http("302 Found", &[("Location", "http://example.com:22/")], b""));
        });
        match fetch(&a, "/") {
            FetchOutcome::Blocked(BlockReason::RedirectToBlocked(inner)) => assert_eq!(*inner, BlockReason::Port(22)),
            o => panic!("{o:?}"),
        }
        let b = spawn(|_, st| {
            let _ = st.write_all(&http("302 Found", &[("Location", "http://u:p@example.com/")], b""));
        });
        match fetch(&b, "/") {
            FetchOutcome::Blocked(BlockReason::RedirectToBlocked(inner)) => assert_eq!(*inner, BlockReason::Credentials),
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn red_boucle_depasse_le_plafond() {
        let s = spawn(|_, st| {
            let _ = st.write_all(&http("302 Found", &[("Location", "/boucle")], b""));
        });
        let mut p = pol(&s);
        p.max_redirects = 3;
        let o = Fetcher::new(p).fetch(&s.url("/boucle"), &Validators::default());
        assert!(matches!(o, FetchOutcome::Blocked(BlockReason::RedirectLimit)), "{o:?}");
        assert_eq!(s.count(), 4, "1 requête initiale + 3 redirections suivies");
    }

    #[test]
    fn red_exactement_au_plafond_passe() {
        let s = spawn(|head, st| {
            let out = match path_of(head).as_str() {
                "/1" => http("302 Found", &[("Location", "/2")], b""),
                "/2" => http("302 Found", &[("Location", "/3")], b""),
                _ => html("fin"),
            };
            let _ = st.write_all(&out);
        });
        let mut p = pol(&s);
        p.max_redirects = 2;
        let r = ok(Fetcher::new(p).fetch(&s.url("/1"), &Validators::default()));
        assert_eq!(r.redirects.len(), 2);
        assert_eq!(r.final_url.path(), "/3");
    }

    #[test]
    fn red_zero_redirection_autorisee() {
        let s = spawn(|_, st| {
            let _ = st.write_all(&http("302 Found", &[("Location", "/b")], b""));
        });
        let mut p = pol(&s);
        p.max_redirects = 0;
        let o = Fetcher::new(p).fetch(&s.url("/a"), &Validators::default());
        assert!(matches!(o, FetchOutcome::Blocked(BlockReason::RedirectLimit)), "{o:?}");
    }

    #[test]
    fn red_location_vide_ou_absente() {
        let a = spawn(|_, st| {
            let _ = st.write_all(&http("302 Found", &[("Location", "")], b""));
        });
        assert!(matches!(fetch(&a, "/"), FetchOutcome::Failed(_)));
        let b = spawn(|_, st| {
            let _ = st.write_all(&http("302 Found", &[], b""));
        });
        assert!(matches!(fetch(&b, "/"), FetchOutcome::Failed(_)));
    }

    #[test]
    fn red_location_geante_sans_panique() {
        let big = format!("/{}", "z".repeat(60_000));
        let a = spawn(move |_, st| {
            let _ = st.write_all(&http("302 Found", &[("Location", &big)], b""));
        });
        let o = fetch(&a, "/");
        assert!(matches!(o, FetchOutcome::Failed(_)), "{o:?}");
    }

    #[test]
    fn red_location_incoherente_sans_panique() {
        for loc in ["http://", "//", "http://[::1", "http://exa mple.com/", ":::", "\u{0}"] {
            let a = spawn(move |_, st| {
                let _ = st.write_all(&http("302 Found", &[("Location", loc)], b""));
            });
            let o = fetch(&a, "/");
            assert!(matches!(o, FetchOutcome::Failed(_) | FetchOutcome::Blocked(_)), "{loc:?}: {o:?}");
        }
    }

    #[test]
    fn red_validateurs_conserves_vers_le_meme_hote() {
        // Même hôte textuel (127.0.0.1, ports différents) : les validateurs suivent la redirection.
        let b = spawn(|_, st| {
            let _ = st.write_all(&html("b"));
        });
        let target = b.url("/").to_string();
        let a = spawn(move |_, st| {
            let _ = st.write_all(&http("302 Found", &[("Location", &target)], b""));
        });
        let mut p = pol(&b);
        p.allow_addrs.push(a.addr);
        let v = Validators { etag: Some("\"e\"".into()), last_modified: None };
        let r = ok(Fetcher::new(p).fetch(&a.url("/"), &v));
        assert_eq!(r.body, b"b");
        assert!(has_header(&b.heads()[0], "if-none-match"));
    }

    // ── Robots ──

    #[test]
    fn robots_refus_sur_url_initiale() {
        let s = spawn(|_, st| {
            let _ = st.write_all(&html("x"));
        });
        let f = Fetcher::with_robots(pol(&s), |_: &Url| false);
        let o = f.fetch(&s.url("/"), &Validators::default());
        assert!(matches!(o, FetchOutcome::Blocked(BlockReason::Robots)), "{o:?}");
        assert_eq!(s.count(), 0, "aucune requête ne doit partir");
    }

    #[test]
    fn robots_autorise_laisse_passer() {
        let s = spawn(|_, st| {
            let _ = st.write_all(&html("x"));
        });
        let f = Fetcher::with_robots(pol(&s), |_: &Url| true);
        assert!(matches!(f.fetch(&s.url("/"), &Validators::default()), FetchOutcome::Ok(_)));
    }

    #[test]
    fn robots_refus_sur_une_redirection() {
        let s = spawn(|head, st| {
            let out = if path_of(head) == "/a" { http("302 Found", &[("Location", "/prive")], b"") } else { html("x") };
            let _ = st.write_all(&out);
        });
        let f = Fetcher::with_robots(pol(&s), |u: &Url| u.path() != "/prive");
        match f.fetch(&s.url("/a"), &Validators::default()) {
            FetchOutcome::Blocked(BlockReason::RedirectToBlocked(inner)) => assert_eq!(*inner, BlockReason::Robots),
            o => panic!("{o:?}"),
        }
        assert_eq!(s.count(), 1, "la cible interdite n'est jamais demandée");
    }

    #[test]
    fn robots_ignore_si_politique_le_desactive() {
        let s = spawn(|_, st| {
            let _ = st.write_all(&html("x"));
        });
        let mut p = pol(&s);
        p.respect_robots = false;
        let f = Fetcher::with_robots(p, |_: &Url| false);
        assert!(matches!(f.fetch(&s.url("/"), &Validators::default()), FetchOutcome::Ok(_)));
    }

    // ── Anti-rebinding : le résolveur est branché dans ureq ──

    #[test]
    fn hop_vers_loopback_non_tolere_est_bloque_par_le_resolveur() {
        // On contourne check_url (qui l'arrêterait) pour prouver que la connexion elle-même est
        // refusée par le résolveur filtrant et que l'erreur est reconnue (pas un simple Failed).
        let hits = Arc::new(AtomicUsize::new(0));
        let h2 = hits.clone();
        let s = spawn(move |_, st| {
            h2.fetch_add(1, Ordering::SeqCst);
            let _ = st.write_all(&html("ne doit pas être lu"));
        });
        let f = Fetcher::new(Policy::default()); // allow_addrs vide
        let r = f.send(&s.url("/"), Duration::from_secs(5), None, None, &[], Instant::now() + Duration::from_secs(5));
        match r {
            Err(HopError::Blocked(BlockReason::PrivateAddress(ip))) => assert_eq!(ip, "127.0.0.1".parse::<IpAddr>().unwrap()),
            other => panic!("{:?}", other.map(|r| r.status())),
        }
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn hop_vers_adresse_tolere_passe_le_resolveur() {
        let s = spawn(|_, st| {
            let _ = st.write_all(&html("ok"));
        });
        let f = Fetcher::new(pol(&s));
        let r = f.send(&s.url("/"), Duration::from_secs(5), None, None, &[], Instant::now() + Duration::from_secs(5));
        assert_eq!(r.map(|r| r.status()).unwrap_or(0), 200);
    }

    #[test]
    fn fetch_deadline_deja_depassee() {
        let s = spawn(|_, st| {
            let _ = st.write_all(&html("x"));
        });
        let mut p = pol(&s);
        p.total_timeout = Duration::ZERO;
        let o = Fetcher::new(p).fetch(&s.url("/"), &Validators::default());
        assert!(matches!(o, FetchOutcome::Blocked(BlockReason::Timeout)), "{o:?}");
        assert_eq!(s.count(), 0);
    }

    #[test]
    fn fetcher_est_clonable_et_partageable() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Fetcher>();
        let s = spawn(|_, st| {
            let _ = st.write_all(&html("x"));
        });
        let f = Fetcher::new(pol(&s));
        let g = f.clone();
        assert!(matches!(g.fetch(&s.url("/"), &Validators::default()), FetchOutcome::Ok(_)));
    }
}
