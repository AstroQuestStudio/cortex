//! `trace` : d'une pile d'erreur (console Chrome/Firefox, Node/vitest, panique Rust,
//! ou JSON de Spectra) aux symboles source EXACTS, en résolvant les cartes de sources
//! (inline des modules Vite, `.map` voisins ou servis, `--maps` d'un build). Le code
//! autour de la ligne et les appelants viennent de l'atlas. Format JSON : docs/TRACE.md.

use super::lecture::{englobant, tests_du_symbole};
use super::{appelants, court, fichier, liste, Out};
use crate::atlas::cards::lines;
use crate::atlas::Handle;
use crate::sourcemap::{self, SourceMap};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// Réglages de `trace` (CLI et MCP).
#[derive(Debug, Clone, Default)]
pub struct Opts {
    /// Dossiers (ou fichiers `.map`) où chercher les cartes d'un build.
    pub maps: Vec<PathBuf>,
    /// Hôtes HTTP autorisés en plus de localhost.
    pub hotes: Vec<String>,
    /// Lignes de code autour de la ligne fautive (défaut 2).
    pub contexte: Option<u32>,
    /// Aucun accès réseau : cartes sur disque seulement.
    pub hors_ligne: bool,
}

/// Un cadre de pile. Lignes et colonnes en base 1 (`col` 0 : inconnue).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Cadre {
    pub fonction: String,
    pub url: String,
    pub ligne: u32,
    pub col: u32,
}

#[derive(Debug, Clone, Default)]
pub struct Pile {
    pub message: String,
    pub cadres: Vec<Cadre>,
}

// ─── Lecture de la pile ─────────────────────────────────────────────────────

const EXT_CODE: &[&str] = &[
    "ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts", "vue", "svelte", "rs", "py", "go", "java", "kt", "cs", "cpp", "cc", "c", "h",
    "hpp", "lua", "rb", "php", "swift", "astro",
];

fn est_chiffres(s: &str) -> bool {
    !s.is_empty() && s.len() <= 9 && s.bytes().all(|b| b.is_ascii_digit())
}

/// `chemin:ligne[:col]`, `chemin(ligne,col)` ou `chemin:ligne:col:` d'un mot.
fn position(mot: &str) -> Option<(String, u32, u32)> {
    let m = mot.trim_matches(|c| "[]'\",;<>".contains(c));
    // Diagnostic TypeScript : `src/a.ts(12,5)` (la parenthèse fermante finale est conservée ici).
    if let Some(p) = m.rfind('(') {
        let inner = m[p + 1..].trim_end_matches([')', ':']);
        if let Some((l, c)) = inner.split_once(',') {
            if est_chiffres(l) && est_chiffres(c) && m[..p].contains('.') {
                return Some((m[..p].to_string(), l.parse().ok()?, c.parse().ok()?));
            }
        }
    }
    let m = m.trim_matches(|c| "()".contains(c)).trim_end_matches([':', '.']);
    let (reste, dernier) = m.rsplit_once(':')?;
    if !est_chiffres(dernier) {
        return None;
    }
    let (chemin, ligne, col) = match reste.rsplit_once(':') {
        Some((r2, avant)) if est_chiffres(avant) => (r2, avant.parse().ok()?, dernier.parse().ok()?),
        _ => (reste, dernier.parse().ok()?, 0),
    };
    let chemin = chemin.trim_matches(|c| "()".contains(c));
    if chemin.is_empty() || chemin.contains('<') || chemin == "native" {
        return None;
    }
    let ext_ok = chemin.rsplit_once('.').is_some_and(|(_, e)| {
        let e = e.split(['?', '#']).next().unwrap_or("");
        EXT_CODE.contains(&e.to_ascii_lowercase().as_str())
    });
    let url_ok = chemin.starts_with("http://") || chemin.starts_with("https://") || chemin.starts_with("file://");
    // Un chemin « a/b » sans extension de code (ex. `12:30:45`, `host:8080`) n'est pas une position.
    if !(ext_ok || url_ok && chemin.rsplit('/').next().is_some_and(|b| b.contains('.') || chemin.contains("/@"))) {
        return None;
    }
    Some((chemin.to_string(), ligne, col))
}

/// Cadre d'une ligne de texte, s'il y en a un.
fn cadre_de_ligne(ligne: &str) -> Option<Cadre> {
    let l = ligne.trim();
    // Firefox/Safari : `fonction@url:l:c`.
    if let Some(i) = l.find('@') {
        if let Some((url, ln, c)) = position(l[i + 1..].split_whitespace().next().unwrap_or("")) {
            if !l[..i].contains(char::is_whitespace) {
                return Some(Cadre { fonction: l[..i].trim().to_string(), url, ligne: ln, col: c });
            }
        }
    }
    let mots: Vec<&str> = l.split_whitespace().collect();
    for (i, m) in mots.iter().enumerate() {
        if *m == "eval" && mots.get(i + 1) == Some(&"at") {
            continue;
        }
        if let Some((url, ln, c)) = position(m) {
            let marqueurs = ["at", "async", "new", "❯", "-->", "|", ">", "•", "×"];
            let avant: Vec<&str> = mots[..i].iter().copied().filter(|x| !marqueurs.contains(x) && !x.ends_with('|')).collect();
            let fonction = avant.join(" ").trim_end_matches('(').trim().to_string();
            // `at fn (loc)` : la position est entre parenthèses ; sinon `❯ fn loc` ou `at loc`.
            let fonction = if fonction.starts_with("Error") && fonction.contains(':') { String::new() } else { fonction };
            return Some(Cadre { fonction, url, ligne: ln, col: c });
        }
    }
    None
}

fn ressemble_a_une_erreur(l: &str) -> bool {
    let t = l.trim();
    let premier = t.split([':', ' ']).next().unwrap_or("");
    premier.ends_with("Error")
        || premier.ends_with("Exception")
        || t.starts_with("Uncaught")
        || t.starts_with("Caused by")
        || t.starts_with("thread '")
        || t.starts_with("FAIL")
        || t.starts_with("panicked at")
        || t.starts_with("error[")
        || t.starts_with("error:")
}

/// Piles d'un texte brut (console, terminal).
pub fn analyser_texte(texte: &str) -> Vec<Pile> {
    let mut piles: Vec<Pile> = Vec::new();
    let mut cur = Pile::default();
    for l in texte.lines() {
        let t = l.trim();
        if t.is_empty() {
            continue;
        }
        if let Some(c) = cadre_de_ligne(t) {
            // Une ligne « panicked at src/a.rs:1:2 » porte aussi son message.
            if cur.message.is_empty() && ressemble_a_une_erreur(t) {
                cur.message = court(t, 160);
            }
            if cur.cadres.last() != Some(&c) {
                cur.cadres.push(c);
            }
        } else if ressemble_a_une_erreur(t) && !cur.cadres.is_empty() {
            piles.push(std::mem::take(&mut cur));
            cur.message = court(t, 160);
        } else if cur.message.is_empty() || (cur.cadres.is_empty() && ressemble_a_une_erreur(t)) {
            cur.message = court(t.trim_start_matches("Uncaught "), 160);
        }
    }
    if !cur.cadres.is_empty() || !cur.message.is_empty() {
        piles.push(cur);
    }
    piles.retain(|p| !p.cadres.is_empty());
    piles
}

fn cle<'a>(o: &'a serde_json::Map<String, Value>, noms: &[&str]) -> Option<&'a Value> {
    noms.iter().find_map(|n| o.get(*n)).filter(|v| !v.is_null())
}

fn cadre_json(v: &Value, base0: bool) -> Option<Cadre> {
    let o = v.as_object()?;
    let url = cle(o, &["url", "file", "fichier", "fileName", "filename", "scriptUrl", "script", "source", "path"])?.as_str()?.to_string();
    let n = |noms: &[&str]| cle(o, noms).and_then(|x| x.as_u64().or_else(|| x.as_str().and_then(|s| s.parse().ok()))).map(|x| x as u32);
    let ligne = n(&["ligne", "line", "lineNumber", "line_number", "row"])?;
    let col = n(&["colonne", "column", "col", "columnNumber", "column_number"]).unwrap_or(0);
    // CDP (`Runtime.CallFrame`, `scriptId`) : lignes et colonnes en base 0.
    let b0 = base0 || o.contains_key("scriptId");
    let fonction = cle(o, &["fonction", "function", "functionName", "fn", "method", "methodName", "name"])
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    Some(Cadre { fonction, url, ligne: ligne + b0 as u32, col: if col > 0 || b0 { col + b0 as u32 } else { 0 } })
}

fn message_json(o: &serde_json::Map<String, Value>) -> String {
    let t = |noms: &[&str]| cle(o, noms).and_then(|x| x.as_str()).map(|s| s.to_string());
    let nom = t(&["name"]).filter(|n| n.ends_with("Error") || n.ends_with("Exception"));
    match (nom, t(&["message", "text", "description", "erreur", "error"])) {
        (Some(n), Some(m)) if !m.starts_with(&n) => format!("{n}: {m}"),
        (_, Some(m)) => m,
        (Some(n), None) => n,
        _ => String::new(),
    }
}

const CLES_CADRES: &[&str] = &["frames", "cadres", "pile", "stackFrames", "callFrames", "stack", "stackTrace", "trace"];

fn piles_json(v: &Value, base0: bool, out: &mut Vec<Pile>, message: &str) {
    match v {
        Value::Array(a) => {
            let cadres: Vec<Cadre> = a.iter().filter_map(|x| cadre_json(x, base0)).collect();
            if !cadres.is_empty() && cadres.len() * 2 >= a.len() {
                out.push(Pile { message: message.to_string(), cadres });
            } else {
                for x in a {
                    piles_json(x, base0, out, message);
                }
            }
        }
        Value::Object(o) => {
            let base0 = base0
                || cle(o, &["indexation", "base"]).and_then(|x| x.as_u64()) == Some(0)
                || cle(o, &["base0"]).and_then(|x| x.as_bool()) == Some(true);
            let msg = Some(message_json(o)).filter(|m| !m.is_empty()).unwrap_or_else(|| message.to_string());
            // Un objet qui est lui-même un cadre (liste plate d'événements).
            if let Some(c) = cadre_json(v, base0) {
                out.push(Pile { message: msg, cadres: vec![c] });
                return;
            }
            let avant = out.len();
            for k in CLES_CADRES {
                match o.get(*k) {
                    Some(Value::String(s)) => {
                        for mut p in analyser_texte(s) {
                            if p.message.is_empty() {
                                p.message = msg.clone();
                            }
                            out.push(p);
                        }
                    }
                    Some(x @ (Value::Array(_) | Value::Object(_))) => {
                        let b = base0 || *k == "callFrames";
                        piles_json(x, b, out, &msg);
                    }
                    _ => {}
                }
                if out.len() > avant {
                    return;
                }
            }
            for (_, x) in o {
                if matches!(x, Value::Array(_) | Value::Object(_)) {
                    piles_json(x, base0, out, &msg);
                }
            }
        }
        _ => {}
    }
}

/// Piles d'une entrée : JSON (objet ou tableau) ou texte brut.
pub fn analyser_entree(entree: &str) -> Vec<Pile> {
    let t = entree.trim_start_matches('\u{feff}').trim();
    if t.starts_with('{') || t.starts_with('[') {
        if let Ok(v) = serde_json::from_str::<Value>(t) {
            let mut out = Vec::new();
            piles_json(&v, false, &mut out, "");
            return out;
        }
    }
    analyser_texte(t)
}

// ─── Cartes : acquisition ───────────────────────────────────────────────────

const MAX_OCTETS: u64 = 25 * 1024 * 1024;

fn http_get(url: &str) -> Result<String, String> {
    use std::io::Read;
    let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(5)).redirects(0).build();
    let rep = agent.get(url).call().map_err(|e| e.to_string())?;
    let mut s = String::new();
    rep.into_reader().take(MAX_OCTETS).read_to_string(&mut s).map_err(|e| e.to_string())?;
    Ok(s)
}

fn hote_autorise(url: &url::Url, extra: &[String]) -> bool {
    match url.host_str() {
        Some(h) => {
            let h = h.trim_matches(|c| c == '[' || c == ']');
            h == "localhost"
                || h == "127.0.0.1"
                || h == "::1"
                || h.ends_with(".localhost")
                || extra.iter().any(|e| e.eq_ignore_ascii_case(h))
        }
        None => false,
    }
}

fn lire_texte(p: &Path) -> Option<String> {
    let nom = p.file_name()?.to_string_lossy().to_ascii_lowercase();
    // Jamais de fichier de secrets.
    if nom.starts_with(".env") || nom.ends_with(".pem") || nom.ends_with(".key") {
        return None;
    }
    let m = std::fs::metadata(p).ok()?;
    if !m.is_file() || m.len() > MAX_OCTETS {
        return None;
    }
    std::fs::read_to_string(p).ok()
}

/// D'où vient une position d'origine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    CarteEnLigne,
    CarteHttp,
    CarteFichier,
    /// Le cadre pointe déjà un fichier source (Node/vitest/Rust, ou chemin deviné).
    Direct,
    /// Pas de carte : chemin du serveur, ligne telle quelle (approximatif).
    Devine,
}

impl Via {
    fn libelle(self) -> &'static str {
        match self {
            Via::CarteEnLigne => "map inline",
            Via::CarteHttp => "map http",
            Via::CarteFichier => "map file",
            Via::Direct => "direct",
            Via::Devine => "no map: approximate",
        }
    }
}

#[derive(Debug, Clone)]
struct Origine {
    source: String,
    ligne: u32,
    col: u32,
    nom: Option<String>,
    contenu: Option<String>,
    via: Via,
}

struct Resolveur<'a> {
    handles: &'a [Handle],
    opts: &'a Opts,
    cartes: HashMap<String, Option<(Rc<SourceMap>, Via)>>,
    index_maps: Option<HashMap<String, PathBuf>>,
    reseau: usize,
}

fn url_en_chemin_local(url: &str) -> Option<PathBuf> {
    if let Some(r) = url.strip_prefix("file://") {
        let r = String::from_utf8_lossy(&sourcemap::pourcent_decoder(r)).to_string();
        // `file:///C:/x` -> `C:/x` ; `file:///home/x` -> `/home/x`.
        let r = if r.len() > 3 && r.as_bytes()[0] == b'/' && r.as_bytes()[2] == b':' { r[1..].to_string() } else { r };
        return Some(PathBuf::from(r));
    }
    if url.contains("://") || url.starts_with("node:") {
        return None;
    }
    Some(PathBuf::from(url))
}

impl<'a> Resolveur<'a> {
    fn new(handles: &'a [Handle], opts: &'a Opts) -> Self {
        Resolveur { handles, opts, cartes: HashMap::new(), index_maps: None, reseau: 0 }
    }

    /// Index `nom.map` → chemin des dossiers `--maps` et des dossiers de build usuels.
    fn carte_sur_disque(&mut self, base: &str) -> Option<PathBuf> {
        if self.index_maps.is_none() {
            let mut racines: Vec<PathBuf> = self.opts.maps.clone();
            for h in self.handles {
                for d in ["dist", "build", "out", ".next", "target/web"] {
                    let p = Path::new(h.root()).join(d);
                    if p.is_dir() {
                        racines.push(p);
                    }
                }
            }
            let mut idx: HashMap<String, PathBuf> = HashMap::new();
            for r in racines {
                if r.is_file() {
                    if let Some(n) = r.file_name().map(|n| n.to_string_lossy().to_string()) {
                        idx.entry(n).or_insert(r.clone());
                    }
                    continue;
                }
                let mut vus = 0usize;
                for e in ignore::WalkBuilder::new(&r).standard_filters(false).max_depth(Some(8)).build().flatten() {
                    vus += 1;
                    if vus > 30_000 {
                        break;
                    }
                    let p = e.path();
                    if p.extension().is_some_and(|x| x == "map") {
                        if let Some(n) = p.file_name().map(|n| n.to_string_lossy().to_string()) {
                            idx.entry(n).or_insert_with(|| p.to_path_buf());
                        }
                    }
                }
            }
            self.index_maps = Some(idx);
        }
        self.index_maps.as_ref()?.get(&format!("{base}.map")).cloned()
    }

    fn analyser_carte(json: &str, via: Via) -> Option<(Rc<SourceMap>, Via)> {
        SourceMap::analyser(json).ok().map(|m| (Rc::new(m), via))
    }

    /// La carte du script `url` (cache par URL).
    fn carte(&mut self, url: &str) -> Option<(Rc<SourceMap>, Via)> {
        if let Some(c) = self.cartes.get(url) {
            return c.clone();
        }
        let c = self.carte_sans_cache(url);
        self.cartes.insert(url.to_string(), c.clone());
        c
    }

    fn carte_sans_cache(&mut self, url: &str) -> Option<(Rc<SourceMap>, Via)> {
        let sans_requete = url.split(['?', '#']).next().unwrap_or(url);
        let base = sans_requete.rsplit('/').next().unwrap_or(sans_requete).to_string();
        if let Some(p) = url_en_chemin_local(sans_requete) {
            if let Some(src) = lire_texte(&p) {
                if let Some(r) = sourcemap::url_de_carte(&src) {
                    if let Some(j) = sourcemap::carte_en_ligne(r) {
                        if let Some(c) = Self::analyser_carte(&j, Via::CarteEnLigne) {
                            return Some(c);
                        }
                    } else if let Some(j) = p.parent().and_then(|d| lire_texte(&d.join(r.split(['?', '#']).next().unwrap_or(r)))) {
                        if let Some(c) = Self::analyser_carte(&j, Via::CarteFichier) {
                            return Some(c);
                        }
                    }
                }
                let mut voisin = p.as_os_str().to_os_string();
                voisin.push(".map");
                if let Some(c) = lire_texte(Path::new(&voisin)).and_then(|j| Self::analyser_carte(&j, Via::CarteFichier)) {
                    return Some(c);
                }
            }
        } else if let Ok(u) = url::Url::parse(sans_requete) {
            if !self.opts.hors_ligne && hote_autorise(&u, &self.opts.hotes) {
                if let Some(c) = self.carte_http(url, &u) {
                    return Some(c);
                }
            }
        }
        // Cartes d'un build, par nom de fichier.
        let p = self.carte_sur_disque(&base)?;
        lire_texte(&p).and_then(|j| Self::analyser_carte(&j, Via::CarteFichier))
    }

    fn carte_http(&mut self, url_complete: &str, u: &url::Url) -> Option<(Rc<SourceMap>, Via)> {
        if self.reseau >= 60 {
            return None;
        }
        self.reseau += 1;
        let corps = http_get(url_complete).or_else(|_| http_get(u.as_str())).ok()?;
        let cible = match sourcemap::url_de_carte(&corps) {
            Some(r) => {
                if let Some(j) = sourcemap::carte_en_ligne(r) {
                    return Self::analyser_carte(&j, Via::CarteEnLigne);
                }
                u.join(r).ok()?
            }
            None => url::Url::parse(&format!("{}.map", u.as_str())).ok()?,
        };
        if !hote_autorise(&cible, &self.opts.hotes) {
            return None;
        }
        self.reseau += 1;
        http_get(cible.as_str()).ok().and_then(|j| Self::analyser_carte(&j, Via::CarteHttp))
    }

    /// Position d'origine d'un cadre.
    fn origine(&mut self, c: &Cadre) -> Origine {
        let devine = |c: &Cadre| Origine {
            source: chemin_de_url(&c.url),
            ligne: c.ligne,
            col: c.col,
            nom: None,
            contenu: None,
            via: if c.url.contains("://") && !c.url.starts_with("file://") { Via::Devine } else { Via::Direct },
        };
        // Un fichier source déjà (extension de code non empaquetée, sans carte à côté) : direct.
        if let Some((m, via)) = self.carte(&c.url) {
            let col0 = c.col.saturating_sub(1);
            let trouve = m.origine(c.ligne.saturating_sub(1), col0).or_else(|| m.origine(c.ligne.saturating_sub(1), u32::MAX));
            if let Some(o) = trouve {
                let source = relatif_a(&chemin_de_url(&c.url), &o.source);
                return Origine { source, ligne: o.ligne + 1, col: o.col + 1, nom: o.nom, contenu: o.contenu, via };
            }
        }
        devine(c)
    }
}

/// Source relative d'une carte (`utils.ts`, `../src/a.ts`) résolue contre le dossier du script
/// (les modules Vite en mode dev ne portent que le nom de fichier).
fn relatif_a(script: &str, source: &str) -> String {
    let src = source.replace('\\', "/");
    let absolu = src.contains("://") || src.starts_with('/') || src.as_bytes().get(1) == Some(&b':');
    if absolu {
        return src;
    }
    let mut segs: Vec<&str> = script.rsplit_once('/').map(|(d, _)| d).unwrap_or("").split('/').filter(|s| !s.is_empty()).collect();
    for s in src.split('/') {
        match s {
            "" | "." => {}
            ".." => {
                segs.pop();
            }
            x => segs.push(x),
        }
    }
    segs.join("/")
}

/// Chemin d'une URL (sans hôte, requête ni fragment) ; chemin local inchangé.
fn chemin_de_url(url: &str) -> String {
    let sans = url.split(['?', '#']).next().unwrap_or(url);
    if let Some(p) = url_en_chemin_local(sans) {
        return p.to_string_lossy().replace('\\', "/");
    }
    match url::Url::parse(sans) {
        Ok(u) => String::from_utf8_lossy(&sourcemap::pourcent_decoder(u.path())).to_string(),
        Err(_) => sans.to_string(),
    }
}

// ─── Projection sur le projet ───────────────────────────────────────────────

enum Classe {
    Projet { h: usize, f: u32 },
    Lib(String),
    Hors(String),
}

fn normaliser_source(s: &str) -> String {
    let mut t = s.replace('\\', "/");
    for p in ["webpack-internal:///", "webpack://", "vite://", "file:///", "file://", "ng://", "turbopack://"] {
        if let Some(r) = t.strip_prefix(p) {
            // `file:///C:/x` -> `C:/x` ; `file:///home/x` -> `/home/x`.
            t = if p == "file:///" && !(r.len() > 1 && r.as_bytes()[1] == b':') { format!("/{r}") } else { r.to_string() };
            break;
        }
    }
    if let Some(r) = t.strip_prefix("/@fs/") {
        t = r.to_string();
    }
    let t = t.split(['?', '#']).next().unwrap_or("").to_string();
    String::from_utf8_lossy(&sourcemap::pourcent_decoder(&t)).to_string()
}

fn nom_de_lib(norm: &str) -> Option<String> {
    if let Some(i) = norm.rfind("node_modules/") {
        let r = &norm[i + "node_modules/".len()..];
        let mut it = r.split('/');
        let a = it.next().unwrap_or("");
        if a == ".vite" || a == ".pnpm" || a == ".cache" {
            // Dépendance pré-empaquetée : `.vite/deps/react-dom_client.js?v=…`.
            let f = r.rsplit('/').next().unwrap_or(r);
            return Some(f.split('.').next().unwrap_or(f).trim_start_matches("chunk-").to_string());
        }
        return Some(if a.starts_with('@') { format!("{}/{}", a, it.next().unwrap_or("")) } else { a.to_string() });
    }
    if norm.starts_with("node:") || norm.starts_with("internal/") {
        return Some("node".into());
    }
    if norm.contains("/@vite/") || norm.contains("/@react-refresh") || norm.starts_with("@vite/") {
        return Some("vite".into());
    }
    None
}

fn projeter(handles: &[Handle], source: &str) -> Classe {
    let norm = normaliser_source(source);
    if let Some(l) = nom_de_lib(&norm) {
        return Classe::Lib(l);
    }
    let segs: Vec<&str> = norm.split('/').filter(|s| !s.is_empty() && *s != "." && *s != "..").collect();
    for (i, h) in handles.iter().enumerate() {
        let racine = h.root().replace('\\', "/");
        let racine = racine.trim_end_matches('/');
        if norm.len() > racine.len() + 1 && norm[..racine.len()].eq_ignore_ascii_case(racine) && norm.as_bytes()[racine.len()] == b'/' {
            if let Some(f) = h.file_by_path(&norm[racine.len() + 1..]) {
                return Classe::Projet { h: i, f };
            }
        }
        // Du plus long suffixe au plus court : le premier chemin que l'atlas connaît exactement.
        for k in 0..segs.len() {
            if let Some(f) = h.file_by_path(&segs[k..].join("/")) {
                return Classe::Projet { h: i, f };
            }
        }
        if let Some(b) = segs.last() {
            if let Ok(Some(f)) = fichier(h, b) {
                return Classe::Projet { h: i, f };
            }
        }
    }
    Classe::Hors(norm)
}

// ─── Rendu ──────────────────────────────────────────────────────────────────

fn extrait(contenu: Option<&str>, disque: Option<String>, ligne: u32, col: u32, ctx: u32) -> (Vec<String>, bool) {
    let source = contenu.map(|c| c.to_string()).or(disque);
    let Some(src) = source else { return (Vec::new(), false) };
    let tout: Vec<&str> = src.lines().collect();
    if ligne == 0 || ligne as usize > tout.len() {
        return (Vec::new(), false);
    }
    let (a, b) = (ligne.saturating_sub(ctx).max(1), (ligne + ctx).min(tout.len() as u32));
    let mut out = Vec::new();
    for n in a..=b {
        let t = tout[n as usize - 1].trim_end();
        let mut t = t.replace('\t', "  ");
        // Ligne très longue (code empaqueté) : fenêtre autour de la colonne.
        if t.chars().count() > 160 {
            let c0 = if n == ligne { col.saturating_sub(1) as usize } else { 0 };
            let debut = c0.saturating_sub(60);
            t = t.chars().skip(debut).take(140).collect::<String>();
            if debut > 0 {
                t = format!("…{t}");
            }
        }
        out.push(format!("{} {:>4} | {}", if n == ligne { ">" } else { " " }, n, t));
    }
    (out, true)
}

/// `trace <pile>` : résout et rend.
pub fn trace(handles: &[Handle], entree: &str, opts: &Opts, budget: usize) -> String {
    let piles = analyser_entree(entree);
    if piles.is_empty() {
        return "(cortex) no stack frame recognised (text: `at fn (url:line:col)`, `fn@url:line:col`, `file:line:col`; JSON: docs/TRACE.md)\nnext : grep <error message>\n".into();
    }
    let ctx = opts.contexte.unwrap_or(2);
    let mut r = Resolveur::new(handles, opts);
    let mut out = Out::new(budget);
    let mut suite: Option<String> = None;
    let mut libs: HashMap<String, usize> = HashMap::new();
    let mut sans_carte = 0usize;
    let mut par_via: HashMap<&'static str, usize> = HashMap::new();
    for (np, pile) in piles.iter().enumerate() {
        let mut n_proj = 0usize;
        let mut lignes: Vec<String> = Vec::new();
        let mut resumes: Vec<String> = Vec::new();
        let mut vues: Vec<(u32, u32, u32)> = Vec::new();
        for (i, c) in pile.cadres.iter().enumerate() {
            let o = r.origine(c);
            match projeter(handles, &o.source) {
                Classe::Lib(l) => *libs.entry(l).or_insert(0) += 1,
                Classe::Hors(p) => {
                    if resumes.len() < 4 {
                        resumes.push(format!("#{} {}:{} (outside the indexed projects)", i + 1, court(&p, 70), o.ligne));
                    }
                }
                Classe::Projet { h: hi, f } => {
                    let h = &handles[hi];
                    if vues.contains(&(hi as u32, f, o.ligne)) {
                        continue;
                    }
                    vues.push((hi as u32, f, o.ligne));
                    n_proj += 1;
                    *par_via.entry(o.via.libelle()).or_insert(0) += 1;
                    if o.via == Via::Devine {
                        sans_carte += 1;
                    }
                    let mut g = englobant(h, f, o.ligne);
                    // Sans carte, la ligne est celle du fichier servi : le nom de la fonction de la pile vaut mieux.
                    if o.via == Via::Devine {
                        let nom = c
                            .fonction
                            .rsplit(['.', ' '])
                            .next()
                            .unwrap_or("")
                            .trim_matches(|x: char| !x.is_alphanumeric() && x != '_' && x != '$');
                        if !nom.is_empty() {
                            if let Some(r) = h.symbols_of(f).iter().find(|r| r.n.kind == 1 && r.name() == nom) {
                                g = Some(r.g);
                            }
                        }
                    }
                    let chemin = h.path_of(f);
                    let id = g.map(|g| h.node_id(g)).unwrap_or_else(|| crate::ids::file_id(chemin));
                    let plage = g.and_then(|g| h.node(g)).map(|r| format!(" {}", lines(r.n.line, r.n.end_line))).unwrap_or_default();
                    if suite.is_none() {
                        suite = Some(format!(
                            "read {}",
                            g.map(|g| h.node_id(g)).unwrap_or_else(|| format!(
                                "{}:{}-{}",
                                chemin,
                                o.ligne.saturating_sub(5).max(1),
                                o.ligne + 5
                            ))
                        ));
                    }
                    let gen_ = if o.via == Via::Direct && o.source == chemin_de_url(&c.url) {
                        String::new()
                    } else {
                        let b = chemin_de_url(&c.url);
                        format!(" ← {}:{}:{}", court(b.rsplit('/').next().unwrap_or(&b), 40), c.ligne, c.col)
                    };
                    let fn_ = if c.fonction.is_empty() { o.nom.clone().unwrap_or_default() } else { c.fonction.clone() };
                    let fn_ = if fn_.is_empty() { String::new() } else { format!(" · {}", court(&fn_, 40)) };
                    let approx = if o.via == Via::Devine { " ~ line approximate (no map)" } else { "" };
                    lignes.push(format!("#{} {}{} · {}:{}{}{}{}", i + 1, id, plage, o.ligne, o.col, fn_, gen_, approx));
                    // Code autour (les 5 premiers cadres du projet).
                    if n_proj <= 5 {
                        let disque = lire_texte(&Path::new(h.root()).join(chemin));
                        let (ex, ok) = extrait(o.contenu.as_deref(), disque.clone(), o.ligne, o.col, ctx);
                        if ok {
                            if let (Some(cont), Some(d)) = (&o.contenu, &disque) {
                                let l1 = cont.lines().nth(o.ligne as usize - 1).map(str::trim);
                                let l2 = d.lines().nth(o.ligne as usize - 1).map(str::trim);
                                if l1.is_some() && l1 != l2 {
                                    lignes.push("    ⚠ file changed since the error (excerpt = the code that ran)".into());
                                }
                            }
                            for e in ex {
                                lignes.push(format!("    {e}"));
                            }
                        }
                        if let Some(g) = g {
                            let app = appelants(h, g);
                            if !app.is_empty() {
                                let ids: Vec<String> = app
                                    .iter()
                                    .map(|&(c, site)| format!("{}{}", h.node_id(c), site.map(|l| format!(" L{l}")).unwrap_or_default()))
                                    .collect();
                                let nodes: Vec<u32> = app.iter().map(|x| x.0).collect();
                                let t: Vec<String> = tests_du_symbole(h, g, &nodes).iter().map(|&t| h.node_id(t)).collect();
                                let tests = if t.is_empty() { String::new() } else { format!(" · tests: {}", liste(&t, 2)) };
                                lignes.push(format!("    callers {}: {}{}", app.len(), liste(&ids, 3), tests));
                            } else {
                                let t: Vec<String> = tests_du_symbole(h, g, &[]).iter().map(|&t| h.node_id(t)).collect();
                                if !t.is_empty() {
                                    lignes.push(format!("    callers 0 · tests: {}", liste(&t, 2)));
                                }
                            }
                        }
                    }
                }
            }
        }
        let msg = if pile.message.is_empty() { "(no message)".to_string() } else { pile.message.clone() };
        let n_libs: usize = libs.values().sum();
        out.ligne(&format!(
            "trace{} {} — {} frame(s), {} in project",
            if piles.len() > 1 { format!(" {}/{}", np + 1, piles.len()) } else { String::new() },
            court(&msg, 120),
            pile.cadres.len(),
            n_proj
        ));
        for l in &lignes {
            out.ligne(l);
        }
        for s in &resumes {
            out.ligne(s);
        }
        if n_libs > 0 && np + 1 == piles.len() {
            let mut v: Vec<(&String, &usize)> = libs.iter().collect();
            v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
            let l: Vec<String> = v.iter().take(5).map(|(n, c)| format!("{n} ×{c}")).collect();
            out.ligne(&format!("libs skipped: {}", l.join(", ")));
        }
    }
    if !par_via.is_empty() {
        let mut v: Vec<String> = par_via.iter().map(|(k, n)| format!("{k} ×{n}")).collect();
        v.sort();
        out.ligne(&format!("sources: {}", v.join(", ")));
    }
    if sans_carte > 0 {
        out.ligne(&format!(
            "~ {sans_carte} frame(s) without a source map (dev server down? try --maps <dist>): lines are those of the served file"
        ));
    }
    out.fin(suite.or_else(|| Some("grep <error message>".to_string())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrome() {
        let t = "TypeError: Cannot read properties of undefined (reading 'map')\n    at renderList (http://localhost:8080/src/pages/List.tsx?t=17:61:5)\n    at async Promise.all (index 0)\n    at http://localhost:8080/node_modules/.vite/deps/react-dom_client.js?v=ab:12:3\n";
        let p = analyser_texte(t);
        assert_eq!(p.len(), 1);
        assert!(p[0].message.starts_with("TypeError"));
        assert_eq!(p[0].cadres.len(), 2);
        assert_eq!(p[0].cadres[0].fonction, "renderList");
        assert_eq!((p[0].cadres[0].ligne, p[0].cadres[0].col), (61, 5));
        assert!(p[0].cadres[0].url.ends_with("List.tsx?t=17"));
    }

    #[test]
    fn firefox_node_vitest_rust() {
        let ff = analyser_texte("render@http://localhost:8080/src/a.tsx:10:7\n@http://localhost:8080/@vite/client:5:1\n");
        assert_eq!(ff[0].cadres[0].fonction, "render");
        assert_eq!(ff[0].cadres.len(), 2);
        let node =
            analyser_texte("Error: boom\n    at f (C:\\Users\\x\\proj\\src\\a.ts:3:9)\n    at file:///C:/Users/x/proj/src/b.ts:4:1\n");
        assert_eq!(node[0].cadres.len(), 2);
        assert_eq!(node[0].cadres[0].url, "C:\\Users\\x\\proj\\src\\a.ts");
        let vt = analyser_texte(" FAIL  src/lib/x.test.ts > a > b\nAssertionError: expected 1 to be 2\n ❯ src/lib/x.test.ts:12:20\n ❯ fnName src/lib/x.ts:7:3\n");
        assert_eq!(vt[0].cadres.len(), 2);
        assert_eq!(vt[0].cadres[1].fonction, "fnName");
        let rs = analyser_texte("thread 'main' panicked at src/foo.rs:12:5:\nboom\n");
        assert_eq!((rs[0].cadres[0].ligne, rs[0].cadres[0].col), (12, 5));
        let ts = analyser_texte("src/a.ts(12,5): error TS2322: nope\n");
        assert_eq!(ts[0].cadres[0].ligne, 12);
    }

    #[test]
    fn arobase_dans_l_url_n_est_pas_firefox() {
        let t = "Error: x\n    at fn (http://localhost:8080/node_modules/@scope/pkg/index.js:1:2)\n";
        let p = analyser_texte(t);
        assert_eq!(p[0].cadres.len(), 1);
        assert_eq!(p[0].cadres[0].fonction, "fn");
        assert!(p[0].cadres[0].url.ends_with("/@scope/pkg/index.js"), "{}", p[0].cadres[0].url);
    }

    #[test]
    fn heures_et_ports_ne_sont_pas_des_positions() {
        assert!(analyser_texte("12:30:45 INFO started on localhost:8080\n").is_empty());
    }

    #[test]
    fn json_formats() {
        let j = r#"{"message":"Boom","stack":[{"url":"http://localhost:8080/src/a.tsx","line":10,"column":4,"function":"f"}]}"#;
        let p = analyser_entree(j);
        assert_eq!(p[0].message, "Boom");
        assert_eq!((p[0].cadres[0].ligne, p[0].cadres[0].col), (10, 4));
        // CDP : base 0.
        let cdp = r#"{"exceptionDetails":{"text":"Uncaught","exception":{"description":"Error: x"},"stackTrace":{"callFrames":[{"functionName":"g","scriptId":"1","url":"http://localhost:8080/src/b.ts","lineNumber":9,"columnNumber":3}]}}}"#;
        let p = analyser_entree(cdp);
        assert_eq!((p[0].cadres[0].ligne, p[0].cadres[0].col), (10, 4));
        // Chaîne `stack` dans un objet, tableau d'événements.
        let ev = r#"[{"type":"error","text":"Oops","stack":"Error: Oops\n    at f (http://localhost:8080/src/c.ts:1:2)"}]"#;
        assert_eq!(analyser_entree(ev)[0].cadres[0].ligne, 1);
        // Liste plate de cadres.
        let plat = r#"[{"fichier":"src/a.ts","ligne":3,"colonne":1},{"fichier":"src/b.ts","ligne":4,"colonne":2}]"#;
        assert_eq!(analyser_entree(plat)[0].cadres.len(), 2);
    }

    #[test]
    fn format_spectra_console() {
        let j = r#"{"schema":"spectra.console/1","page":{"url":"http://localhost:8080/app"},"messages":[
          {"level":"error","kind":"exception","text":"TypeError: Cannot read properties of undefined (reading 'id')","count":2,
           "location":"http://localhost:8080/src/App.tsx?t=17636:42:17",
           "stack":[{"function":"render","url":"http://localhost:8080/src/App.tsx?t=17636","line":42,"column":17},
                    {"function":"commit","url":"http://localhost:8080/node_modules/.vite/deps/react-dom_client.js?v=1","line":9,"column":3}]},
          {"level":"warning","kind":"console","text":"Deprecated","count":1}]}"#;
        let p = analyser_entree(j);
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(p[0].message.starts_with("TypeError"));
        assert_eq!(p[0].cadres.len(), 2);
        assert_eq!((p[0].cadres[0].ligne, p[0].cadres[0].col, p[0].cadres[0].fonction.as_str()), (42, 17, "render"));
        // `console_errors` d'un proof.json : même forme, sous une autre clé.
        let proof = format!(
            r#"{{"schema":"spectra.proof/1","result":"fail","console_errors":[{}]}}"#,
            r#"{"text":"Boom","stack":[{"function":"f","url":"http://localhost:8080/src/a.ts","line":1,"column":1}]}"#
        );
        assert_eq!(analyser_entree(&proof)[0].message, "Boom");
    }

    #[test]
    fn source_relative_au_script() {
        assert_eq!(relatif_a("/src/lib/utils.ts", "utils.ts"), "src/lib/utils.ts");
        assert_eq!(relatif_a("/assets/index.js", "../src/a.tsx"), "src/a.tsx");
        assert_eq!(relatif_a("/x/a.js", "webpack://app/./src/a.ts"), "webpack://app/./src/a.ts");
        assert_eq!(relatif_a("/x/a.js", "C:/p/src/a.ts"), "C:/p/src/a.ts");
    }

    #[test]
    fn normalisation() {
        assert_eq!(normaliser_source("webpack://app/./src/a.ts?x"), "app/./src/a.ts");
        assert_eq!(normaliser_source("/@fs/C:/p/src/a.ts"), "C:/p/src/a.ts");
        assert_eq!(normaliser_source("file:///C:/p/a.ts"), "C:/p/a.ts");
        assert_eq!(normaliser_source("file:///home/p/a.ts"), "/home/p/a.ts");
        assert_eq!(nom_de_lib("C:/p/node_modules/@tanstack/react-query/build/x.js").as_deref(), Some("@tanstack/react-query"));
        assert_eq!(nom_de_lib("/node_modules/.vite/deps/react-dom_client.js").as_deref(), Some("react-dom_client"));
        assert!(nom_de_lib("src/a.ts").is_none());
    }
}
