//! Cortex Web : recherche web générale gratuite et robuste pour agents (docs/WEB.md §7, décision D10).
//!
//! Principes :
//! - **Liens + extraits** : rien n'est téléchargé en plus ; le texte d'une page n'est lu que si
//!   l'agent le demande (`cortex_web_fetch`, sous la garde réseau).
//! - **Intention d'abord** ([`detect_intent`]) : bibliothèque, erreur, dépôt, encyclopédique -> source
//!   structurée correspondante (crates.io + npm, Stack Exchange, GitHub, Wikipedia) ; seule une
//!   question ouverte tombe sur un moteur : fournisseur à clé de l'utilisateur (Brave, Mojeek,
//!   SearXNG auto-hébergé) s'il existe, sinon DuckDuckGo (repli, jamais mis en cache).
//! - **Toute requête passe par la garde réseau** (`guard::Fetcher` : SSRF, plafonds, User-Agent
//!   honnête), sans exception : les fournisseurs qui exigent un EN-TÊTE d'authentification (Brave,
//!   GitHub avec jeton) utilisent `Fetcher::fetch_with_headers` (liste blanche de noms d'en-têtes,
//!   envoyés au premier saut seulement, jamais retransmis après une redirection).
//! - **Secrets** : les clés viennent de l'environnement de l'utilisateur, ne sont écrites nulle part
//!   (ni fichier, ni journal, ni sortie : `Debug` les masque, les messages d'erreur sont expurgés).
//! - **Requête telle que formulée** : seuls les caractères de contrôle sont retirés et la longueur
//!   plafonnée ; aucun nom de projet ni chemin n'est ajouté.
//! - **Robustesse** : limiteur par hôte, coupe-circuit par fournisseur (3 échecs => suspendu 15 min ;
//!   un BLOCAGE — captcha, HTTP 202/403/429 — ouvre le circuit tout de suite), quotas locaux
//!   (Stack Exchange 300/jour, GitHub 10/min sans jeton), déduplication par URL normalisée.
//! - **Conditions des fournisseurs** : User-Agent honnête, jamais de contournement de captcha, jamais
//!   de rotation d'IP/UA ; Brave/Mojeek/SearXNG/DuckDuckGo non cacheables ; Stack Exchange et
//!   Wikipedia portent leur licence CC BY-SA et leur attribution dans chaque résultat.
//!
//! La sortie de [`render`] est du texte brut sans balisage : l'appelant la cadre avec
//! `untrusted::wrap`. Les titres et extraits venus du web sont déjà assainis (`untrusted::sanitize`).

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;
use url::Url;

use super::guard::{check_url, Fetcher, Validators};
use super::untrusted;
use super::{FetchOutcome, Policy, Response};

// ───────────────────────── Constantes ─────────────────────────

pub const P_CRATES: &str = "crates.io";
pub const P_NPM: &str = "npm";
pub const P_GITHUB: &str = "github";
pub const P_STACKEXCHANGE: &str = "stackexchange";
pub const P_HACKERNEWS: &str = "hackernews";
pub const P_WIKIPEDIA: &str = "wikipedia";
pub const P_DUCKDUCKGO: &str = "duckduckgo";
pub const P_BRAVE: &str = "brave";
pub const P_MOJEEK: &str = "mojeek";
pub const P_SEARXNG: &str = "searxng";

const MAX_QUERY_CHARS: usize = 300;
const MAX_URL_CHARS: usize = 2048;
const TITLE_MAX: usize = 200;
const SNIPPET_STORE: usize = 400;
/// Longueur maximale d'un extrait dans la sortie de [`render`].
pub const SNIPPET_RENDER: usize = 200;
const MAX_LIMIT: usize = 20;

/// Coupe-circuit : nombre d'échecs consécutifs avant suspension, et durée de suspension.
pub const BREAKER_THRESHOLD: u32 = 3;
pub const BREAKER_SUSPEND: Duration = Duration::from_secs(15 * 60);

// ───────────────────────── Types publics ─────────────────────────

/// Un résultat de recherche : un LIEN et un extrait (jamais le texte de la page).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub provider: &'static str,
    /// Licence du contenu cité (ex. « CC BY-SA 4.0 », SPDX d'un dépôt).
    pub license: Option<String>,
    /// Attribution à afficher avec le résultat (obligatoire pour Stack Exchange / Wikipedia).
    pub attribution: Option<String>,
}

/// Intention détectée d'une requête (FR/EN).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Intent {
    /// « quelle crate/paquet npm/pip… », version, installation.
    Library,
    /// Message d'erreur, code d'erreur, trace.
    Error,
    /// « dépôt GitHub de… ».
    Repo,
    /// « qu'est-ce que… », « who is… ».
    Encyclopedic,
    /// Tout le reste.
    Open,
}

impl Intent {
    pub fn label(self) -> &'static str {
        match self {
            Intent::Library => "bibliothèque",
            Intent::Error => "erreur",
            Intent::Repo => "dépôt",
            Intent::Encyclopedic => "encyclopédique",
            Intent::Open => "ouverte",
        }
    }
}

/// Échec d'un fournisseur. Les messages ne contiennent jamais de clé (expurgés).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchError {
    /// Blocage par le service (captcha, HTTP 202/403/429) : ouvre le coupe-circuit.
    Blocked(String),
    /// Quota local ou distant épuisé (aucune requête envoyée pour un quota local).
    Quota(String),
    /// Réseau, HTTP, délai, garde réseau.
    Failed(String),
    /// Réponse reçue mais illisible (format changé).
    Parse(String),
    /// Fournisseur indisponible : coupe-circuit ouvert, ou configuration absente.
    Unavailable(String),
}

impl fmt::Display for SearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SearchError::Blocked(m) => write!(f, "bloqué : {m}"),
            SearchError::Quota(m) => write!(f, "quota : {m}"),
            SearchError::Failed(m) => write!(f, "échec : {m}"),
            SearchError::Parse(m) => write!(f, "réponse illisible : {m}"),
            SearchError::Unavailable(m) => write!(f, "indisponible : {m}"),
        }
    }
}

impl std::error::Error for SearchError {}

/// Un fournisseur de recherche enfichable.
pub trait SearchProvider: Send + Sync {
    fn name(&self) -> &'static str;
    fn search(&self, fetcher: &Fetcher, q: &str, limit: usize) -> Result<Vec<SearchHit>, SearchError>;
    /// Les résultats peuvent-ils être conservés sur disque ? Faux pour Brave (conditions du
    /// 01/09/2026), DuckDuckGo, Mojeek, SearXNG.
    fn cacheable(&self) -> bool;
    /// Hôte servant de clé au limiteur (défaut : le nom du fournisseur).
    fn host(&self) -> &'static str {
        self.name()
    }
    /// Intervalle minimal entre deux requêtes vers cet hôte.
    fn min_gap(&self) -> Duration {
        Duration::ZERO
    }
}

// ───────────────────────── Texte : assainissement ─────────────────────────

/// Retire les balises HTML (`<b>`, `</span>`, `<!-- -->`…). Un `<` qui n'ouvre pas une balise reste.
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if in_tag {
            if c == '>' {
                in_tag = false;
            }
            continue;
        }
        if c == '<' {
            match it.peek() {
                Some(n) if n.is_ascii_alphabetic() || *n == '/' || *n == '!' || *n == '?' => {
                    in_tag = true;
                    continue;
                }
                _ => {}
            }
        }
        out.push(c);
    }
    out
}

fn named_entity(name: &str) -> Option<char> {
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        "hellip" => '…',
        "ndash" => '–',
        "mdash" => '—',
        "rsquo" | "lsquo" => '\'',
        "ldquo" | "rdquo" => '"',
        "laquo" => '«',
        "raquo" => '»',
        "copy" => '©',
        _ => return None,
    })
}

/// Décode les entités HTML courantes (nommées, `&#39;`, `&#x27;`). Une entité inconnue reste telle quelle.
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let end = rest.char_indices().take(12).find(|&(_, c)| c == ';').map(|(k, _)| k);
        let decoded = end.and_then(|k| {
            let ent = &rest[1..k];
            let ch = if let Some(num) = ent.strip_prefix('#') {
                let code = if let Some(h) = num.strip_prefix('x').or_else(|| num.strip_prefix('X')) {
                    u32::from_str_radix(h, 16).ok()
                } else {
                    num.parse::<u32>().ok()
                };
                code.and_then(char::from_u32)
            } else {
                named_entity(ent)
            };
            ch.map(|c| (c, k))
        });
        match decoded {
            Some((c, k)) => {
                out.push(c);
                rest = &rest[k + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
    t.push('…');
    t
}

/// Texte venu du web -> une ligne propre : balises retirées, entités décodées, caractères invisibles
/// et balises de cadrage neutralisés (`untrusted::sanitize`), blancs réduits, longueur plafonnée.
pub fn clean_text(s: &str, max_chars: usize) -> String {
    let t = decode_entities(&strip_tags(s));
    let t = untrusted::sanitize(&t);
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&t, max_chars)
}

/// URL http(s) valide, sans identifiants ; sinon `None` (les liens `javascript:`, `ftp:`… sont écartés).
fn http_url(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() || s.len() > MAX_URL_CHARS {
        return None;
    }
    let u = Url::parse(s).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
    u.host_str()?;
    if !u.username().is_empty() || u.password().is_some() {
        return None;
    }
    Some(u.to_string())
}

fn make_hit(
    provider: &'static str,
    title: &str,
    url: &str,
    snippet: &str,
    license: Option<&str>,
    attribution: Option<&str>,
) -> Option<SearchHit> {
    let url = http_url(url)?;
    let mut title = clean_text(title, TITLE_MAX);
    if title.is_empty() {
        title = truncate_chars(&url, TITLE_MAX);
    }
    let opt = |o: Option<&str>| o.map(|x| clean_text(x, 120)).filter(|x| !x.is_empty());
    Some(SearchHit {
        title,
        url,
        snippet: clean_text(snippet, SNIPPET_STORE),
        provider,
        license: opt(license),
        attribution: opt(attribution),
    })
}

/// Requête envoyée telle que formulée : caractères de contrôle -> espace, bords rognés, longueur plafonnée.
pub fn prepare_query(q: &str) -> Result<String, SearchError> {
    let cleaned: String = q.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let t = cleaned.trim();
    if t.is_empty() {
        return Err(SearchError::Failed("requête vide".to_string()));
    }
    Ok(t.chars().take(MAX_QUERY_CHARS).collect())
}

fn clamp_limit(limit: usize) -> usize {
    limit.clamp(1, MAX_LIMIT)
}

/// Remplace toute occurrence d'un secret par `***` (messages d'erreur, journaux).
pub fn redact(msg: &str, secrets: &[&str]) -> String {
    let mut out = msg.to_string();
    for s in secrets {
        if s.len() >= 4 {
            out = out.replace(s, "***");
        }
    }
    out
}

/// Valeur d'environnement utilisable comme secret : ASCII imprimable sans espace (pas d'injection
/// d'en-tête par CR/LF), 8 à 256 caractères. Sinon ignorée.
fn clean_secret(v: &str) -> Option<String> {
    let v = v.trim();
    if v.len() < 8 || v.len() > 256 || !v.bytes().all(|b| (0x21..0x7f).contains(&b)) {
        return None;
    }
    Some(v.to_string())
}

fn env_secret(name: &str) -> Option<String> {
    std::env::var(name).ok().and_then(|v| clean_secret(&v))
}

// ───────────────────────── Réseau ─────────────────────────

/// Classe un message d'échec : HTTP 202/403/429 = blocage (ouvre le coupe-circuit).
fn classify_failure(msg: &str) -> SearchError {
    let blocked = ["HTTP 202", "HTTP 403", "HTTP 429"].iter().any(|p| msg.starts_with(p));
    if blocked {
        SearchError::Blocked(msg.to_string())
    } else {
        SearchError::Failed(msg.to_string())
    }
}

/// Une requête GET d'un fournisseur, toujours par le `Fetcher` (en-têtes d'authentification
/// éventuels passés à `fetch_with_headers`). Messages expurgés.
fn http_get(fetcher: &Fetcher, url: &Url, headers: &[(&str, &str)], secrets: &[&str]) -> Result<Response, SearchError> {
    let outcome = fetcher.fetch_with_headers(url, &Validators::default(), headers);
    match outcome {
        FetchOutcome::Ok(r) => Ok(r),
        FetchOutcome::NotModified => Err(SearchError::Failed("réponse 304 inattendue".to_string())),
        FetchOutcome::Blocked(b) => Err(SearchError::Failed(redact(&format!("requête refusée par la garde réseau : {b:?}"), secrets))),
        FetchOutcome::Failed(m) => Err(classify_failure(&redact(&m, secrets))),
    }
}

fn parse_json(body: &[u8]) -> Result<Value, SearchError> {
    serde_json::from_slice(body).map_err(|e| SearchError::Parse(format!("JSON invalide ({e})")))
}

fn jstr<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn build_url(base: &str, params: &[(&str, &str)]) -> Result<Url, SearchError> {
    Url::parse_with_params(base, params).map_err(|e| SearchError::Failed(format!("URL de fournisseur invalide : {e}")))
}

// ───────────────────────── Quotas locaux ─────────────────────────

/// Fenêtre glissante : au plus `max` requêtes sur `span`.
#[derive(Debug)]
pub struct QuotaWindow {
    max: u32,
    span: Duration,
    stamps: VecDeque<Instant>,
}

impl QuotaWindow {
    pub fn new(max: u32, span: Duration) -> Self {
        QuotaWindow { max, span, stamps: VecDeque::new() }
    }

    /// Prend un jeton ; sinon renvoie l'attente avant le prochain.
    pub fn try_take(&mut self, now: Instant) -> Result<(), Duration> {
        while let Some(&t) = self.stamps.front() {
            if now.saturating_duration_since(t) >= self.span {
                self.stamps.pop_front();
            } else {
                break;
            }
        }
        if self.stamps.len() as u32 >= self.max {
            // Fenêtre de capacité nulle : jamais de jeton, attente = la fenêtre entière.
            let Some(&oldest) = self.stamps.front() else { return Err(self.span) };
            return Err((oldest + self.span).saturating_duration_since(now));
        }
        self.stamps.push_back(now);
        Ok(())
    }

    pub fn used(&self) -> usize {
        self.stamps.len()
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ───────────────────────── crates.io ─────────────────────────

/// crates.io : 1 requête/s, User-Agent identifiant obligatoire (fourni par le `Fetcher`).
pub struct CratesIo {
    base: String,
}

impl CratesIo {
    pub fn new() -> Self {
        CratesIo { base: "https://crates.io/api/v1/crates".to_string() }
    }
}

impl Default for CratesIo {
    fn default() -> Self {
        Self::new()
    }
}

pub fn parse_crates(body: &[u8], limit: usize) -> Result<Vec<SearchHit>, SearchError> {
    let v = parse_json(body)?;
    let arr =
        v.get("crates").and_then(Value::as_array).ok_or_else(|| SearchError::Parse("crates.io : champ « crates » absent".to_string()))?;
    let mut out = Vec::new();
    for c in arr {
        let name = jstr(c, "name");
        if name.is_empty() || !name.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-') {
            continue;
        }
        let ver = ["max_stable_version", "max_version", "newest_version", "default_version"]
            .iter()
            .map(|k| jstr(c, k))
            .find(|s| !s.is_empty())
            .unwrap_or("");
        let title = if ver.is_empty() { name.to_string() } else { format!("{name} {ver}") };
        let mut snippet = jstr(c, "description").to_string();
        if !ver.is_empty() {
            snippet.push_str(&format!(" — docs.rs/{name}/{ver}"));
        }
        if let Some(h) = make_hit(P_CRATES, &title, &format!("https://crates.io/crates/{name}"), &snippet, None, None) {
            out.push(h);
        }
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

impl SearchProvider for CratesIo {
    fn name(&self) -> &'static str {
        P_CRATES
    }
    fn cacheable(&self) -> bool {
        true
    }
    fn host(&self) -> &'static str {
        "crates.io"
    }
    fn min_gap(&self) -> Duration {
        Duration::from_secs(1)
    }
    fn search(&self, fetcher: &Fetcher, q: &str, limit: usize) -> Result<Vec<SearchHit>, SearchError> {
        let q = prepare_query(q)?;
        let limit = clamp_limit(limit);
        let lim = limit.to_string();
        let url = build_url(&self.base, &[("q", q.as_str()), ("per_page", lim.as_str())])?;
        let resp = http_get(fetcher, &url, &[], &[])?;
        parse_crates(&resp.body, limit)
    }
}

// ───────────────────────── npm ─────────────────────────

pub struct Npm {
    base: String,
}

impl Npm {
    pub fn new() -> Self {
        Npm { base: "https://registry.npmjs.org/-/v1/search".to_string() }
    }
}

impl Default for Npm {
    fn default() -> Self {
        Self::new()
    }
}

pub fn parse_npm(body: &[u8], limit: usize) -> Result<Vec<SearchHit>, SearchError> {
    let v = parse_json(body)?;
    let arr = v.get("objects").and_then(Value::as_array).ok_or_else(|| SearchError::Parse("npm : champ « objects » absent".to_string()))?;
    let mut out = Vec::new();
    for o in arr {
        let Some(p) = o.get("package") else { continue };
        let name = jstr(p, "name");
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || "@/._~-".contains(c)) {
            continue;
        }
        let ver = jstr(p, "version");
        let title = if ver.is_empty() { name.to_string() } else { format!("{name} {ver}") };
        let links = p.get("links");
        let from_links = links.map(|l| jstr(l, "npm")).unwrap_or("");
        let url = if http_url(from_links).is_some() { from_links.to_string() } else { format!("https://www.npmjs.com/package/{name}") };
        if let Some(h) = make_hit(P_NPM, &title, &url, jstr(p, "description"), None, None) {
            out.push(h);
        }
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

impl SearchProvider for Npm {
    fn name(&self) -> &'static str {
        P_NPM
    }
    fn cacheable(&self) -> bool {
        true
    }
    fn host(&self) -> &'static str {
        "registry.npmjs.org"
    }
    fn min_gap(&self) -> Duration {
        Duration::from_millis(250)
    }
    fn search(&self, fetcher: &Fetcher, q: &str, limit: usize) -> Result<Vec<SearchHit>, SearchError> {
        let q = prepare_query(q)?;
        let limit = clamp_limit(limit);
        let lim = limit.to_string();
        let url = build_url(&self.base, &[("text", q.as_str()), ("size", lim.as_str())])?;
        let resp = http_get(fetcher, &url, &[], &[])?;
        parse_npm(&resp.body, limit)
    }
}

// ───────────────────────── GitHub (dépôts) ─────────────────────────

/// GitHub search/repositories : 10 req/min sans jeton (30 avec). `GITHUB_TOKEN` est lu seulement s'il
/// existe, n'est jamais journalisé et n'est envoyé qu'à `api.github.com`.
pub struct GitHubRepos {
    base: String,
    token: Option<String>,
    quota: Mutex<QuotaWindow>,
}

impl GitHubRepos {
    pub fn new(token: Option<String>) -> Self {
        let max = if token.is_some() { 30 } else { 10 };
        GitHubRepos {
            base: "https://api.github.com/search/repositories".to_string(),
            token,
            quota: Mutex::new(QuotaWindow::new(max, Duration::from_secs(60))),
        }
    }

    pub fn from_env() -> Self {
        Self::new(env_secret("GITHUB_TOKEN"))
    }

    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }
}

impl fmt::Debug for GitHubRepos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GitHubRepos {{ token: {} }}", if self.token.is_some() { "***" } else { "aucun" })
    }
}

pub fn parse_github(body: &[u8], limit: usize) -> Result<Vec<SearchHit>, SearchError> {
    let v = parse_json(body)?;
    let arr = v.get("items").and_then(Value::as_array).ok_or_else(|| SearchError::Parse("GitHub : champ « items » absent".to_string()))?;
    let mut out = Vec::new();
    for it in arr {
        let full = jstr(it, "full_name");
        if full.is_empty() {
            continue;
        }
        let spdx = it.get("license").map(|l| jstr(l, "spdx_id")).unwrap_or("");
        let license = if spdx.is_empty() || spdx == "NOASSERTION" { None } else { Some(spdx) };
        let mut snippet = jstr(it, "description").to_string();
        let stars = it.get("stargazers_count").and_then(Value::as_u64).unwrap_or(0);
        let lang = jstr(it, "language");
        let mut meta = format!("★{stars}");
        if !lang.is_empty() {
            meta.push_str(&format!(" · {lang}"));
        }
        if snippet.is_empty() {
            snippet = meta;
        } else {
            snippet.push_str(&format!(" · {meta}"));
        }
        if let Some(h) = make_hit(P_GITHUB, full, jstr(it, "html_url"), &snippet, license, None) {
            out.push(h);
        }
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

impl SearchProvider for GitHubRepos {
    fn name(&self) -> &'static str {
        P_GITHUB
    }
    fn cacheable(&self) -> bool {
        true
    }
    fn host(&self) -> &'static str {
        "api.github.com"
    }
    fn min_gap(&self) -> Duration {
        Duration::from_millis(1500)
    }
    fn search(&self, fetcher: &Fetcher, q: &str, limit: usize) -> Result<Vec<SearchHit>, SearchError> {
        let q = prepare_query(q)?;
        let limit = clamp_limit(limit);
        let lim = limit.to_string();
        if let Err(wait) = lock(&self.quota).try_take(Instant::now()) {
            return Err(SearchError::Quota(format!("GitHub : limite locale atteinte, encore {} s", wait.as_secs() + 1)));
        }
        let url = build_url(&self.base, &[("q", q.as_str()), ("per_page", lim.as_str())])?;
        let auth;
        let mut headers: Vec<(&str, &str)> = Vec::new();
        if let Some(t) = &self.token {
            auth = format!("Bearer {t}");
            headers.push(("Authorization", &auth));
            headers.push(("Accept", "application/json"));
        }
        let secrets: Vec<&str> = self.token.iter().map(|s| s.as_str()).collect();
        let resp = http_get(fetcher, &url, &headers, &secrets)?;
        parse_github(&resp.body, limit)
    }
}

// ───────────────────────── Stack Exchange ─────────────────────────

/// Résultat d'analyse Stack Exchange (avec les champs de quota de la réponse).
#[derive(Debug)]
pub struct SeParsed {
    pub hits: Vec<SearchHit>,
    /// Secondes à attendre avant la prochaine requête (champ `backoff`).
    pub backoff: Option<u64>,
    pub quota_remaining: Option<i64>,
}

struct SeState {
    quota: QuotaWindow,
    backoff_until: Option<Instant>,
}

/// Stack Exchange `search/advanced` : 300 requêtes/jour/IP sans clé (compteur local), contenu
/// CC BY-SA 4.0 : la licence et l'attribution figurent dans CHAQUE résultat.
pub struct StackExchange {
    base: String,
    site: String,
    state: Mutex<SeState>,
}

impl StackExchange {
    pub fn new() -> Self {
        StackExchange {
            base: "https://api.stackexchange.com/2.3/search/advanced".to_string(),
            site: "stackoverflow".to_string(),
            state: Mutex::new(SeState { quota: QuotaWindow::new(300, Duration::from_secs(24 * 3600)), backoff_until: None }),
        }
    }
}

impl Default for StackExchange {
    fn default() -> Self {
        Self::new()
    }
}

pub fn parse_stackexchange(body: &[u8], limit: usize) -> Result<SeParsed, SearchError> {
    let v = parse_json(body)?;
    if let Some(id) = v.get("error_id").and_then(Value::as_i64) {
        let msg = clean_text(jstr(&v, "error_message"), 160);
        return Err(if id == 502 {
            SearchError::Quota(format!("Stack Exchange : {msg}"))
        } else {
            SearchError::Failed(format!("Stack Exchange {id} : {msg}"))
        });
    }
    let arr = v
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| SearchError::Parse("Stack Exchange : champ « items » absent".to_string()))?;
    let mut hits = Vec::new();
    for it in arr {
        let license = {
            let l = jstr(it, "content_license");
            if l.is_empty() {
                "CC BY-SA 4.0"
            } else {
                l
            }
        };
        let author = it.get("owner").map(|o| jstr(o, "display_name")).unwrap_or("");
        let site = "Stack Overflow";
        let attribution =
            if author.is_empty() { format!("{site} (Stack Exchange)") } else { format!("© {} / {site}", clean_text(author, 60)) };
        let score = it.get("score").and_then(Value::as_i64).unwrap_or(0);
        let answers = it.get("answer_count").and_then(Value::as_u64).unwrap_or(0);
        let accepted = it.get("accepted_answer_id").is_some() || it.get("is_answered").and_then(Value::as_bool).unwrap_or(false);
        let tags: Vec<&str> =
            it.get("tags").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).take(5).collect()).unwrap_or_default();
        let mut snippet = format!("{score} votes, {answers} réponse(s){}", if accepted { ", répondue" } else { "" });
        if !tags.is_empty() {
            snippet.push_str(&format!(" — tags : {}", tags.join(", ")));
        }
        if let Some(h) = make_hit(P_STACKEXCHANGE, jstr(it, "title"), jstr(it, "link"), &snippet, Some(license), Some(attribution.as_str()))
        {
            hits.push(h);
        }
        if hits.len() >= limit {
            break;
        }
    }
    Ok(SeParsed {
        hits,
        backoff: v.get("backoff").and_then(Value::as_u64),
        quota_remaining: v.get("quota_remaining").and_then(Value::as_i64),
    })
}

impl SearchProvider for StackExchange {
    fn name(&self) -> &'static str {
        P_STACKEXCHANGE
    }
    fn cacheable(&self) -> bool {
        true
    }
    fn host(&self) -> &'static str {
        "api.stackexchange.com"
    }
    fn min_gap(&self) -> Duration {
        Duration::from_millis(500)
    }
    fn search(&self, fetcher: &Fetcher, q: &str, limit: usize) -> Result<Vec<SearchHit>, SearchError> {
        let q = prepare_query(q)?;
        let limit = clamp_limit(limit);
        let lim = limit.to_string();
        {
            let now = Instant::now();
            let mut st = lock(&self.state);
            if let Some(until) = st.backoff_until {
                if now < until {
                    return Err(SearchError::Quota(format!(
                        "Stack Exchange : backoff demandé par l'API, encore {} s",
                        (until - now).as_secs() + 1
                    )));
                }
                st.backoff_until = None;
            }
            if let Err(wait) = st.quota.try_take(now) {
                return Err(SearchError::Quota(format!(
                    "Stack Exchange : 300 requêtes/jour atteintes (compteur local), encore {} min",
                    wait.as_secs() / 60 + 1
                )));
            }
        }
        let url = build_url(
            &self.base,
            &[("q", q.as_str()), ("site", self.site.as_str()), ("order", "desc"), ("sort", "relevance"), ("pagesize", lim.as_str())],
        )?;
        let resp = http_get(fetcher, &url, &[], &[])?;
        let parsed = parse_stackexchange(&resp.body, limit)?;
        let mut st = lock(&self.state);
        if let Some(b) = parsed.backoff {
            st.backoff_until = Some(Instant::now() + Duration::from_secs(b.min(3600)));
        }
        if parsed.quota_remaining == Some(0) {
            st.backoff_until = Some(Instant::now() + Duration::from_secs(3600));
        }
        Ok(parsed.hits)
    }
}

// ───────────────────────── Hacker News ─────────────────────────

pub struct HackerNews {
    base: String,
}

impl HackerNews {
    pub fn new() -> Self {
        HackerNews { base: "https://hn.algolia.com/api/v1/search".to_string() }
    }
}

impl Default for HackerNews {
    fn default() -> Self {
        Self::new()
    }
}

pub fn parse_hackernews(body: &[u8], limit: usize) -> Result<Vec<SearchHit>, SearchError> {
    let v = parse_json(body)?;
    let arr =
        v.get("hits").and_then(Value::as_array).ok_or_else(|| SearchError::Parse("Hacker News : champ « hits » absent".to_string()))?;
    let mut out = Vec::new();
    for h in arr {
        let id = jstr(h, "objectID");
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let discussion = format!("https://news.ycombinator.com/item?id={id}");
        let title = {
            let t = jstr(h, "title");
            if t.is_empty() {
                jstr(h, "story_title")
            } else {
                t
            }
        };
        let ext = jstr(h, "url");
        let url = if http_url(ext).is_some() { ext.to_string() } else { discussion.clone() };
        let points = h.get("points").and_then(Value::as_u64).unwrap_or(0);
        let comments = h.get("num_comments").and_then(Value::as_u64).unwrap_or(0);
        let mut snippet = format!("{points} points, {comments} commentaires — discussion : {discussion}");
        let text = clean_text(jstr(h, "story_text"), 160);
        if !text.is_empty() {
            snippet.push_str(" — ");
            snippet.push_str(&text);
        }
        if let Some(hit) = make_hit(P_HACKERNEWS, title, &url, &snippet, None, None) {
            out.push(hit);
        }
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

impl SearchProvider for HackerNews {
    fn name(&self) -> &'static str {
        P_HACKERNEWS
    }
    fn cacheable(&self) -> bool {
        true
    }
    fn host(&self) -> &'static str {
        "hn.algolia.com"
    }
    fn min_gap(&self) -> Duration {
        Duration::from_millis(300)
    }
    fn search(&self, fetcher: &Fetcher, q: &str, limit: usize) -> Result<Vec<SearchHit>, SearchError> {
        let q = prepare_query(q)?;
        let limit = clamp_limit(limit);
        let lim = limit.to_string();
        let url = build_url(&self.base, &[("query", q.as_str()), ("tags", "story"), ("hitsPerPage", lim.as_str())])?;
        let resp = http_get(fetcher, &url, &[], &[])?;
        parse_hackernews(&resp.body, limit)
    }
}

// ───────────────────────── Wikipedia ─────────────────────────

/// Wikipedia (fr ou en selon la langue de la requête, repli sur l'autre langue si rien) ; texte CC BY-SA.
pub struct Wikipedia {
    /// Gabarit d'origine ; `{lang}` est remplacé par `fr` ou `en`.
    base: String,
}

impl Wikipedia {
    pub fn new() -> Self {
        Wikipedia { base: "https://{lang}.wikipedia.org".to_string() }
    }
}

impl Default for Wikipedia {
    fn default() -> Self {
        Self::new()
    }
}

/// Langue probable d'une requête : `fr` ou `en`.
pub fn detect_lang(q: &str) -> &'static str {
    let lower = q.to_lowercase();
    if lower.chars().any(|c| "éèêëàâùûôîïçœ".contains(c)) {
        return "fr";
    }
    let norm = fold(&lower);
    let ws = words(&norm);
    const FR: &[&str] = &[
        "est",
        "les",
        "des",
        "une",
        "du",
        "que",
        "qui",
        "quoi",
        "pour",
        "dans",
        "sur",
        "avec",
        "quand",
        "comment",
        "pourquoi",
        "histoire",
        "definition",
        "langage",
        "quel",
        "quelle",
        "quels",
        "quelles",
        "ou",
        "etait",
        "sont",
    ];
    if ws.iter().any(|w| FR.contains(w)) {
        "fr"
    } else {
        "en"
    }
}

fn wiki_page_url(lang: &str, title: &str) -> Option<String> {
    let mut u = Url::parse(&format!("https://{lang}.wikipedia.org/wiki/")).ok()?;
    u.path_segments_mut().ok()?.pop_if_empty().push(&title.trim().replace(' ', "_"));
    Some(u.to_string())
}

pub fn parse_wikipedia(body: &[u8], limit: usize, lang: &str) -> Result<Vec<SearchHit>, SearchError> {
    let v = parse_json(body)?;
    if let Some(e) = v.get("error") {
        return Err(SearchError::Failed(format!("Wikipedia : {}", clean_text(jstr(e, "info"), 160))));
    }
    let arr = v
        .get("query")
        .and_then(|q| q.get("search"))
        .and_then(Value::as_array)
        .ok_or_else(|| SearchError::Parse("Wikipedia : champ « query.search » absent".to_string()))?;
    let attribution = if lang == "fr" { "Contributeurs de Wikipédia" } else { "Wikipedia contributors" };
    let mut out = Vec::new();
    for s in arr {
        let title = jstr(s, "title");
        if title.is_empty() {
            continue;
        }
        let Some(url) = wiki_page_url(lang, title) else { continue };
        if let Some(h) = make_hit(P_WIKIPEDIA, title, &url, jstr(s, "snippet"), Some("CC BY-SA 4.0"), Some(attribution)) {
            out.push(h);
        }
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

impl Wikipedia {
    fn query_lang(&self, fetcher: &Fetcher, q: &str, limit: usize, lang: &'static str) -> Result<Vec<SearchHit>, SearchError> {
        let lim = limit.to_string();
        let base = self.base.replace("{lang}", lang);
        let base = format!("{}/w/api.php", base.trim_end_matches('/'));
        let url = build_url(
            &base,
            &[
                ("action", "query"),
                ("list", "search"),
                ("srsearch", q),
                ("srlimit", lim.as_str()),
                ("srnamespace", "0"),
                ("format", "json"),
                ("utf8", "1"),
            ],
        )?;
        let resp = http_get(fetcher, &url, &[], &[])?;
        parse_wikipedia(&resp.body, limit, lang)
    }
}

impl SearchProvider for Wikipedia {
    fn name(&self) -> &'static str {
        P_WIKIPEDIA
    }
    fn cacheable(&self) -> bool {
        true
    }
    fn host(&self) -> &'static str {
        "wikipedia.org"
    }
    fn min_gap(&self) -> Duration {
        Duration::from_millis(300)
    }
    fn search(&self, fetcher: &Fetcher, q: &str, limit: usize) -> Result<Vec<SearchHit>, SearchError> {
        let q = prepare_query(q)?;
        let limit = clamp_limit(limit);
        let first = detect_lang(&q);
        let hits = self.query_lang(fetcher, &q, limit, first)?;
        if !hits.is_empty() {
            return Ok(hits);
        }
        let other = if first == "fr" { "en" } else { "fr" };
        // Le repli est un échec silencieux : la première langue a répondu (vide), ce n'est pas une erreur.
        Ok(self.query_lang(fetcher, &q, limit, other).unwrap_or_default())
    }
}

// ───────────────────────── DuckDuckGo (repli) ─────────────────────────

/// DuckDuckGo `html.duckduckgo.com/html/` : REPLI sans clé. Analyse du HTML (sans contrat, fragile),
/// décodage des liens de redirection `uddg=`, détection du blocage (HTTP 202/403/429, page de défi) qui
/// ouvre le coupe-circuit. Jamais de cache, jamais de contournement du défi.
pub struct DuckDuckGo {
    base: String,
}

impl DuckDuckGo {
    pub fn new() -> Self {
        DuckDuckGo { base: "https://html.duckduckgo.com/html/".to_string() }
    }
}

impl Default for DuckDuckGo {
    fn default() -> Self {
        Self::new()
    }
}

/// Décode un lien de résultat DuckDuckGo : `//duckduckgo.com/l/?uddg=<url>` -> `<url>` ; lien direct
/// conservé ; publicités (`/y.js`) et liens non http(s) -> `None`.
pub fn decode_ddg_href(href: &str) -> Option<String> {
    let href = href.trim();
    let full = if let Some(rest) = href.strip_prefix("//") { format!("https://{rest}") } else { href.to_string() };
    let u = Url::parse(&full).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
    let host = u.host_str().unwrap_or("").to_ascii_lowercase();
    if host == "duckduckgo.com" || host.ends_with(".duckduckgo.com") {
        if u.path() == "/l/" || u.path() == "/l" {
            let target = u.query_pairs().find(|(k, _)| k == "uddg").map(|(_, v)| v.into_owned())?;
            return http_url(&target);
        }
        return None; // publicité (/y.js), page interne
    }
    http_url(&full)
}

fn ddg_challenge(html_lower: &str) -> bool {
    ["anomaly-modal", "bots use duckduckgo too", "g-recaptcha", "captcha", "challenge-form", "confirm this search was made by a human"]
        .iter()
        .any(|m| html_lower.contains(m))
}

pub fn parse_ddg(html: &str, limit: usize) -> Result<Vec<SearchHit>, SearchError> {
    use scraper::{Html, Selector};
    let doc = Html::parse_document(html);
    let sel_result = Selector::parse("div.result").map_err(|_| SearchError::Parse("sélecteur interne".to_string()))?;
    let sel_a = Selector::parse("a.result__a").map_err(|_| SearchError::Parse("sélecteur interne".to_string()))?;
    let sel_snip = Selector::parse(".result__snippet").map_err(|_| SearchError::Parse("sélecteur interne".to_string()))?;
    let mut out = Vec::new();
    for r in doc.select(&sel_result) {
        if r.value().classes().any(|c| c == "result--ad") {
            continue;
        }
        let Some(a) = r.select(&sel_a).next() else { continue };
        let Some(url) = a.value().attr("href").and_then(decode_ddg_href) else { continue };
        let title: String = a.text().collect();
        let snippet: String = r.select(&sel_snip).next().map(|s| s.text().collect()).unwrap_or_default();
        if let Some(h) = make_hit(P_DUCKDUCKGO, &title, &url, &snippet, None, None) {
            out.push(h);
        }
        if out.len() >= limit {
            break;
        }
    }
    if !out.is_empty() {
        return Ok(out);
    }
    let lower = html.to_lowercase();
    if ddg_challenge(&lower) {
        return Err(SearchError::Blocked("DuckDuckGo demande une vérification (page de défi) : repli suspendu".to_string()));
    }
    let collapsed = lower.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.contains("no-results") || collapsed.contains("no results") || collapsed.contains("aucun résultat") {
        return Ok(Vec::new());
    }
    Err(SearchError::Parse("DuckDuckGo : format de page inattendu (aucun résultat reconnu)".to_string()))
}

impl SearchProvider for DuckDuckGo {
    fn name(&self) -> &'static str {
        P_DUCKDUCKGO
    }
    fn cacheable(&self) -> bool {
        false
    }
    fn host(&self) -> &'static str {
        "html.duckduckgo.com"
    }
    fn min_gap(&self) -> Duration {
        Duration::from_secs(2)
    }
    fn search(&self, fetcher: &Fetcher, q: &str, limit: usize) -> Result<Vec<SearchHit>, SearchError> {
        let q = prepare_query(q)?;
        let limit = clamp_limit(limit);
        let url = build_url(&self.base, &[("q", q.as_str())])?;
        let resp = http_get(fetcher, &url, &[], &[])?;
        if resp.status == 202 {
            return Err(SearchError::Blocked("HTTP 202 (page de vérification de DuckDuckGo)".to_string()));
        }
        parse_ddg(&String::from_utf8_lossy(&resp.body), limit)
    }
}

// ───────────────────────── Brave (clé de l'utilisateur) ─────────────────────────

/// Brave Search API. Clé = `BRAVE_API_KEY` (en-tête `X-Subscription-Token`). Conditions du 01/09/2026 :
/// stockage et cache INTERDITS => `cacheable() == false`.
pub struct BraveSearch {
    base: String,
    key: String,
}

impl BraveSearch {
    pub fn new(key: &str) -> Option<Self> {
        Some(BraveSearch { base: "https://api.search.brave.com/res/v1/web/search".to_string(), key: clean_secret(key)? })
    }

    pub fn from_env() -> Option<Self> {
        Self::new(&std::env::var("BRAVE_API_KEY").ok()?)
    }
}

impl fmt::Debug for BraveSearch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BraveSearch {{ key: *** }}")
    }
}

pub fn parse_brave(body: &[u8], limit: usize) -> Result<Vec<SearchHit>, SearchError> {
    let v = parse_json(body)?;
    // Réponse sans résultat web : l'objet `web` peut manquer.
    let Some(arr) = v.get("web").and_then(|w| w.get("results")).and_then(Value::as_array) else {
        return if v.get("type").is_some() { Ok(Vec::new()) } else { Err(SearchError::Parse("Brave : réponse inattendue".to_string())) };
    };
    let mut out = Vec::new();
    for r in arr {
        if let Some(h) = make_hit(P_BRAVE, jstr(r, "title"), jstr(r, "url"), jstr(r, "description"), None, None) {
            out.push(h);
        }
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

impl SearchProvider for BraveSearch {
    fn name(&self) -> &'static str {
        P_BRAVE
    }
    fn cacheable(&self) -> bool {
        false
    }
    fn host(&self) -> &'static str {
        "api.search.brave.com"
    }
    fn min_gap(&self) -> Duration {
        Duration::from_millis(1100)
    }
    fn search(&self, fetcher: &Fetcher, q: &str, limit: usize) -> Result<Vec<SearchHit>, SearchError> {
        let q = prepare_query(q)?;
        let limit = clamp_limit(limit);
        let lim = limit.to_string();
        let url = build_url(&self.base, &[("q", q.as_str()), ("count", lim.as_str())])?;
        let headers = [("X-Subscription-Token", self.key.as_str()), ("Accept", "application/json")];
        let resp = http_get(fetcher, &url, &headers, &[&self.key])?;
        parse_brave(&resp.body, limit)
    }
}

// ───────────────────────── Mojeek (clé de l'utilisateur) ─────────────────────────

/// Mojeek Search API. Clé = `MOJEEK_API_KEY` (paramètre `api_key` : jamais journalisé, expurgé des
/// erreurs). Non cacheable par prudence : l'offre d'entrée limite la conservation à 1 h.
pub struct Mojeek {
    base: String,
    key: String,
}

impl Mojeek {
    pub fn new(key: &str) -> Option<Self> {
        Some(Mojeek { base: "https://api.mojeek.com/search".to_string(), key: clean_secret(key)? })
    }

    pub fn from_env() -> Option<Self> {
        Self::new(&std::env::var("MOJEEK_API_KEY").ok()?)
    }
}

impl fmt::Debug for Mojeek {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Mojeek {{ key: *** }}")
    }
}

pub fn parse_mojeek(body: &[u8], limit: usize) -> Result<Vec<SearchHit>, SearchError> {
    let v = parse_json(body)?;
    let resp = v.get("response").ok_or_else(|| SearchError::Parse("Mojeek : champ « response » absent".to_string()))?;
    let status = jstr(resp, "status");
    if !status.is_empty() && !status.eq_ignore_ascii_case("ok") {
        return Err(SearchError::Failed(format!("Mojeek : statut {}", clean_text(status, 60))));
    }
    let arr = resp.get("results").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut out = Vec::new();
    for r in &arr {
        if let Some(h) = make_hit(P_MOJEEK, jstr(r, "title"), jstr(r, "url"), jstr(r, "desc"), None, None) {
            out.push(h);
        }
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

impl SearchProvider for Mojeek {
    fn name(&self) -> &'static str {
        P_MOJEEK
    }
    fn cacheable(&self) -> bool {
        false
    }
    fn host(&self) -> &'static str {
        "api.mojeek.com"
    }
    fn min_gap(&self) -> Duration {
        Duration::from_millis(500)
    }
    fn search(&self, fetcher: &Fetcher, q: &str, limit: usize) -> Result<Vec<SearchHit>, SearchError> {
        let q = prepare_query(q)?;
        let limit = clamp_limit(limit);
        let lim = limit.to_string();
        let url = build_url(&self.base, &[("q", q.as_str()), ("api_key", self.key.as_str()), ("fmt", "json"), ("t", lim.as_str())])?;
        let resp = http_get(fetcher, &url, &[], &[&self.key])?;
        parse_mojeek(&resp.body, limit)
    }
}

// ───────────────────────── SearXNG auto-hébergé ─────────────────────────

/// SearXNG auto-hébergé (AGPL : appelé en HTTP, jamais embarqué). URL = `CORTEX_SEARXNG_URL`, validée
/// par `check_url` comme tout le reste. Une adresse privée n'est acceptée que si l'utilisateur l'a
/// fixée lui-même par `CORTEX_SEARXNG_ALLOW_PRIVATE=1` ; ce réglage n'existe que dans l'environnement
/// (aucun outil agent, aucun paramètre) et ne vaut que pour CE fournisseur (Fetcher dédié).
pub struct SearxngProvider {
    base: Url,
    own: Option<Fetcher>,
}

impl SearxngProvider {
    /// Construit depuis l'environnement. `None` si `CORTEX_SEARXNG_URL` est absente ; `Some(Err(..))`
    /// si elle est refusée (le message dit pourquoi, sans secret).
    pub fn from_env() -> Option<Result<Self, String>> {
        let raw = std::env::var("CORTEX_SEARXNG_URL").ok()?;
        if raw.trim().is_empty() {
            return None;
        }
        let allow = std::env::var("CORTEX_SEARXNG_ALLOW_PRIVATE").map(|v| v.trim() == "1").unwrap_or(false);
        Some(Self::configure(raw.trim(), allow))
    }

    fn configure(raw: &str, allow_private: bool) -> Result<Self, String> {
        let mut url = Url::parse(raw).map_err(|e| format!("CORTEX_SEARXNG_URL invalide : {e}"))?;
        let policy = Policy::default();
        let public_check = check_url(&url, &policy);
        let (base, own) = match public_check {
            Ok(()) => (url, None),
            Err(reason) => {
                use super::BlockReason::{Port, PrivateAddress};
                let private = matches!(reason, PrivateAddress(_) | Port(_));
                if !allow_private || !private {
                    return Err(format!(
                        "CORTEX_SEARXNG_URL refusée ({reason:?}) : adresse privée acceptée seulement avec CORTEX_SEARXNG_ALLOW_PRIVATE=1"
                    ));
                }
                if url.scheme() != "http" && url.scheme() != "https" {
                    return Err("CORTEX_SEARXNG_URL : schéma http ou https exigé".to_string());
                }
                if !url.username().is_empty() || url.password().is_some() {
                    return Err("CORTEX_SEARXNG_URL : identifiants dans l'URL refusés".to_string());
                }
                let port = url.port_or_known_default().ok_or_else(|| "CORTEX_SEARXNG_URL : port inconnu".to_string())?;
                let host = url.host_str().ok_or_else(|| "CORTEX_SEARXNG_URL : hôte absent".to_string())?.to_string();
                let addrs: Vec<SocketAddr> = (host.as_str(), port)
                    .to_socket_addrs()
                    .map_err(|e| format!("CORTEX_SEARXNG_URL : résolution impossible ({e})"))?
                    .collect();
                if addrs.is_empty() {
                    return Err("CORTEX_SEARXNG_URL : aucune adresse".to_string());
                }
                // Les noms refusés par la garde (`localhost`, `*.local`…) sont remplacés par l'adresse résolue.
                if let Err(r) = check_url(&url, &Policy { allow_addrs: addrs.clone(), allowed_ports: vec![port], ..Policy::default() }) {
                    if matches!(r, PrivateAddress(_)) {
                        let ip = addrs.iter().find(|a| a.is_ipv4()).unwrap_or(&addrs[0]).ip();
                        url.set_ip_host(ip).map_err(|_| "CORTEX_SEARXNG_URL : hôte non remplaçable".to_string())?;
                    }
                }
                let policy = Policy { allow_addrs: addrs, allowed_ports: vec![port], ..Policy::default() };
                check_url(&url, &policy).map_err(|r| format!("CORTEX_SEARXNG_URL refusée ({r:?})"))?;
                (url, Some(Fetcher::new(policy)))
            }
        };
        Ok(SearxngProvider { base, own })
    }

    fn endpoint(&self) -> Url {
        let mut u = self.base.clone();
        let p = u.path().trim_end_matches('/').to_string();
        if !p.ends_with("/search") {
            u.set_path(&format!("{p}/search"));
        }
        u.set_query(None);
        u.set_fragment(None);
        u
    }
}

impl fmt::Debug for SearxngProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SearxngProvider {{ hôte: {}, privé: {} }}", self.base.host_str().unwrap_or("?"), self.own.is_some())
    }
}

pub fn parse_searxng(body: &[u8], limit: usize) -> Result<Vec<SearchHit>, SearchError> {
    let v = parse_json(body)?;
    let arr = v
        .get("results")
        .and_then(Value::as_array)
        .ok_or_else(|| SearchError::Parse("SearXNG : champ « results » absent (format JSON activé ?)".to_string()))?;
    let mut out = Vec::new();
    for r in arr {
        if let Some(h) = make_hit(P_SEARXNG, jstr(r, "title"), jstr(r, "url"), jstr(r, "content"), None, None) {
            out.push(h);
        }
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

impl SearchProvider for SearxngProvider {
    fn name(&self) -> &'static str {
        P_SEARXNG
    }
    fn cacheable(&self) -> bool {
        false
    }
    fn host(&self) -> &'static str {
        "searxng"
    }
    fn min_gap(&self) -> Duration {
        Duration::from_millis(500)
    }
    fn search(&self, fetcher: &Fetcher, q: &str, limit: usize) -> Result<Vec<SearchHit>, SearchError> {
        let q = prepare_query(q)?;
        let limit = clamp_limit(limit);
        let mut url = self.endpoint();
        url.query_pairs_mut().append_pair("q", q.as_str()).append_pair("format", "json");
        let f = self.own.as_ref().unwrap_or(fetcher);
        let resp = http_get(f, &url, &[], &[])?;
        parse_searxng(&resp.body, limit)
    }
}

// ───────────────────────── Détection d'intention ─────────────────────────

/// Minuscules sans accents, apostrophes typographiques normalisées.
fn fold(s: &str) -> String {
    s.chars()
        .flat_map(|c| c.to_lowercase())
        .map(|c| match c {
            'à' | 'â' | 'ä' | 'á' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'î' | 'ï' | 'í' => 'i',
            'ô' | 'ö' | 'ó' => 'o',
            'ù' | 'û' | 'ü' | 'ú' => 'u',
            'ç' => 'c',
            '’' | '‘' => '\'',
            other => other,
        })
        .collect()
}

fn words(norm: &str) -> Vec<&str> {
    norm.split(|c: char| !c.is_alphanumeric() && c != '_').filter(|w| !w.is_empty()).collect()
}

fn is_error_code(w: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    if let Some(d) = w.strip_prefix('e') {
        if d.len() == 4 && digits(d) {
            return true; // E0502 (Rust)
        }
    }
    if let Some(d) = w.strip_prefix("ts") {
        if d.len() == 4 && digits(d) {
            return true; // TS2345
        }
    }
    if w.starts_with("err_") {
        return true;
    }
    if let Some(h) = w.strip_prefix("0x") {
        if h.len() >= 6 && h.chars().all(|c| c.is_ascii_hexdigit()) {
            return true;
        }
    }
    matches!(
        w,
        "errno"
            | "enoent"
            | "eacces"
            | "econnrefused"
            | "econnreset"
            | "eaddrinuse"
            | "etimedout"
            | "eperm"
            | "eexist"
            | "enomem"
            | "epipe"
    )
}

/// Détecte l'intention d'une requête (FR/EN). Ordre : erreur > dépôt > bibliothèque > encyclopédique > ouverte.
pub fn detect_intent(q: &str) -> Intent {
    let norm = fold(q);
    let ws = words(&norm);
    if ws.is_empty() {
        return Intent::Open;
    }
    let flat = ws.join(" ");
    let has = |set: &[&str]| ws.iter().any(|w| set.contains(w));

    // 1. Erreur : message, trace ou code d'erreur.
    let err_word = ws.iter().any(|w| w.contains("error") || w.contains("erreur") || w.contains("exception") || w.contains("traceback"));
    const ERR_PHRASES: &[&str] = &[
        "stack trace",
        "stacktrace",
        "backtrace",
        "panicked",
        "segmentation fault",
        "segfault",
        "core dumped",
        "cannot find",
        "could not",
        "failed to",
        "unable to",
        "is not defined",
        "is not a function",
        "cannot read",
        "unresolved import",
        "mismatched types",
        "impossible de",
        "n a pas pu",
        "ne compile pas",
        "exit code",
    ];
    if err_word || ERR_PHRASES.iter().any(|p| flat.contains(p)) || ws.iter().any(|w| is_error_code(w)) {
        return Intent::Error;
    }

    // 2. Dépôt.
    if has(&["repo", "repos", "repository", "repositories", "depot", "depots"]) {
        return Intent::Repo;
    }
    if has(&["github", "gitlab"])
        && has(&["project", "projects", "projet", "projets", "awesome", "alternative", "alternatives", "stars", "opensource"])
    {
        return Intent::Repo;
    }

    // 3. Bibliothèque.
    const REGISTRY: &[&str] = &[
        "crate",
        "crates",
        "cratesio",
        "npm",
        "npmjs",
        "pip",
        "pip3",
        "pypi",
        "yarn",
        "pnpm",
        "cargo",
        "gem",
        "rubygems",
        "nuget",
        "maven",
        "composer",
        "packagist",
    ];
    const PKG: &[&str] =
        &["package", "packages", "paquet", "paquets", "library", "librairie", "librairies", "bibliotheque", "bibliotheques", "lib", "libs"];
    const PKG_CTX: &[&str] = &[
        "version",
        "versions",
        "latest",
        "derniere",
        "dernier",
        "install",
        "installer",
        "changelog",
        "release",
        "alternative",
        "alternatives",
    ];
    if has(REGISTRY) || (has(PKG) && has(PKG_CTX)) {
        return Intent::Library;
    }

    // 4. Encyclopédique : formulations de définition, courtes, sans code.
    const PREFIXES: &[&str] = &[
        "qu est ce que",
        "qu est ce qu",
        "qu est ce",
        "c est quoi",
        "qui est",
        "qui etait",
        "qui sont",
        "qui a invente",
        "quand a ete",
        "quand est",
        "what is",
        "what s",
        "what are",
        "what was",
        "who is",
        "who was",
        "who are",
        "who invented",
        "when was",
        "when did",
        "define",
        "definition de",
        "definition of",
        "meaning of",
        "history of",
        "histoire de",
        "biographie",
        "biography",
    ];
    let code_like = q.chars().any(|c| "(){}[]<>;=`".contains(c)) || q.contains("::");
    let short = ws.len() <= 9;
    if !code_like && short && (PREFIXES.iter().any(|p| flat.starts_with(p)) || has(&["wikipedia"])) {
        return Intent::Encyclopedic;
    }
    Intent::Open
}

// ───────────────────────── Limiteur par hôte et coupe-circuit ─────────────────────────

/// Limiteur par hôte : espace les requêtes d'au moins `gap`. Pur (horloge passée en paramètre).
#[derive(Default)]
pub struct Limiter {
    next: Mutex<HashMap<String, Instant>>,
}

impl Limiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Réserve le prochain créneau de `host` et renvoie l'attente à observer avant d'envoyer.
    pub fn reserve(&self, host: &str, gap: Duration, now: Instant) -> Duration {
        let mut m = lock(&self.next);
        let start = m.get(host).copied().map_or(now, |t| t.max(now));
        m.insert(host.to_string(), start + gap);
        start.saturating_duration_since(now)
    }
}

#[derive(Debug, Default)]
struct BState {
    fails: u32,
    open_until: Option<Instant>,
}

/// Coupe-circuit par fournisseur, en mémoire : `threshold` échecs consécutifs => suspendu `suspend`.
/// Un BLOCAGE (captcha, HTTP 202/403/429) ouvre le circuit tout de suite. Après la suspension, un
/// seul nouvel échec le rouvre.
pub struct Breaker {
    threshold: u32,
    suspend: Duration,
    st: Mutex<HashMap<&'static str, BState>>,
}

impl Breaker {
    pub fn new(threshold: u32, suspend: Duration) -> Self {
        Breaker { threshold: threshold.max(1), suspend, st: Mutex::new(HashMap::new()) }
    }

    /// `Ok` si le fournisseur peut être appelé ; sinon `Err(durée restante de suspension)`.
    pub fn check(&self, name: &'static str, now: Instant) -> Result<(), Duration> {
        let mut m = lock(&self.st);
        let s = m.entry(name).or_default();
        if let Some(until) = s.open_until {
            if now < until {
                return Err(until - now);
            }
            s.open_until = None;
            s.fails = self.threshold.saturating_sub(1); // demi-ouvert : un échec suffit
        }
        Ok(())
    }

    pub fn on_success(&self, name: &'static str) {
        let mut m = lock(&self.st);
        let s = m.entry(name).or_default();
        s.fails = 0;
        s.open_until = None;
    }

    pub fn on_failure(&self, name: &'static str, now: Instant, blocked: bool) {
        let mut m = lock(&self.st);
        let s = m.entry(name).or_default();
        s.fails += 1;
        if blocked || s.fails >= self.threshold {
            s.open_until = Some(now + self.suspend);
        }
    }
}

// ───────────────────────── Normalisation, fusion ─────────────────────────

/// Forme canonique pour dédoublonner : sans schéma, `www.`, port par défaut, fragment, slash final ni
/// paramètres de pistage (`utm_*`, `fbclid`, `gclid`).
pub fn normalize_url(u: &str) -> String {
    let Ok(url) = Url::parse(u.trim()) else { return u.trim().to_lowercase() };
    let host = url.host_str().unwrap_or("").trim_start_matches("www.").to_string();
    let port = match url.port() {
        Some(p) => format!(":{p}"),
        None => String::new(),
    };
    let path = url.path().trim_end_matches('/').to_string();
    let mut q = String::new();
    for (k, v) in url.query_pairs() {
        let lk = k.to_ascii_lowercase();
        if lk.starts_with("utm_") || lk == "fbclid" || lk == "gclid" {
            continue;
        }
        q.push(if q.is_empty() { '?' } else { '&' });
        q.push_str(&format!("{k}={v}"));
    }
    format!("{host}{port}{path}{q}")
}

/// Alterne les résultats des fournisseurs d'un groupe (le premier de chacun d'abord).
fn interleave(lists: Vec<Vec<SearchHit>>) -> Vec<SearchHit> {
    let mut iters: Vec<_> = lists.into_iter().map(|l| l.into_iter()).collect();
    let mut out = Vec::new();
    loop {
        let mut any = false;
        for it in iters.iter_mut() {
            if let Some(h) = it.next() {
                out.push(h);
                any = true;
            }
        }
        if !any {
            break;
        }
    }
    out
}

fn dedup(hits: Vec<SearchHit>) -> Vec<SearchHit> {
    let mut seen = std::collections::HashSet::new();
    hits.into_iter().filter(|h| seen.insert(normalize_url(&h.url))).collect()
}

// ───────────────────────── Routeur ─────────────────────────

/// Résultat d'un routage.
#[derive(Debug, Clone)]
pub struct SearchOutcome {
    pub hits: Vec<SearchHit>,
    /// Chaque fournisseur essayé : nombre de résultats, ou l'erreur.
    pub tried: Vec<(&'static str, Result<usize, SearchError>)>,
    pub intent: Intent,
    /// Vrai si TOUS les résultats viennent de fournisseurs dont les conditions autorisent un cache.
    pub cacheable: bool,
}

pub struct Router {
    providers: Vec<Box<dyn SearchProvider>>,
    fetcher: Fetcher,
    limiter: Limiter,
    breaker: Breaker,
    preferred: Option<String>,
    notes: Vec<String>,
    /// Faux uniquement dans les tests : ne pas dormir entre deux requêtes.
    pace: bool,
}

impl Router {
    pub fn new(providers: Vec<Box<dyn SearchProvider>>, fetcher: Fetcher) -> Self {
        Router {
            providers,
            fetcher,
            limiter: Limiter::new(),
            breaker: Breaker::new(BREAKER_THRESHOLD, BREAKER_SUSPEND),
            preferred: None,
            notes: Vec::new(),
            pace: true,
        }
    }

    /// Configuration depuis l'environnement : fournisseurs sans clé toujours présents ; Brave, Mojeek
    /// et SearXNG seulement si l'utilisateur a fourni leur réglage. `CORTEX_SEARCH_PROVIDER`
    /// (brave|mojeek|searxng|duckduckgo) met un fournisseur en tête des questions ouvertes.
    pub fn from_env() -> Self {
        let mut providers: Vec<Box<dyn SearchProvider>> = vec![
            Box::new(CratesIo::new()),
            Box::new(Npm::new()),
            Box::new(GitHubRepos::from_env()),
            Box::new(StackExchange::new()),
            Box::new(HackerNews::new()),
            Box::new(Wikipedia::new()),
            Box::new(DuckDuckGo::new()),
        ];
        let mut notes = Vec::new();
        if let Some(b) = BraveSearch::from_env() {
            providers.push(Box::new(b));
        }
        if let Some(m) = Mojeek::from_env() {
            providers.push(Box::new(m));
        }
        match SearxngProvider::from_env() {
            Some(Ok(s)) => providers.push(Box::new(s)),
            Some(Err(e)) => notes.push(e),
            None => {}
        }
        let mut r = Router::new(providers, Fetcher::new(Policy::default()));
        r.notes = notes;
        r.preferred = std::env::var("CORTEX_SEARCH_PROVIDER").ok().map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty());
        r
    }

    pub fn with_preferred(mut self, name: &str) -> Self {
        self.preferred = Some(name.trim().to_ascii_lowercase());
        self
    }

    /// Désactive les attentes du limiteur (tests).
    pub fn without_pacing(mut self) -> Self {
        self.pace = false;
        self
    }

    pub fn with_breaker(mut self, b: Breaker) -> Self {
        self.breaker = b;
        self
    }

    /// Avertissements de configuration (ex. URL SearXNG refusée). Jamais de secret.
    pub fn notes(&self) -> &[String] {
        &self.notes
    }

    pub fn provider_names(&self) -> Vec<&'static str> {
        self.providers.iter().map(|p| p.name()).collect()
    }

    pub fn is_cacheable(&self, name: &str) -> bool {
        self.providers.iter().find(|p| p.name() == name).is_some_and(|p| p.cacheable())
    }

    fn find(&self, name: &str) -> Option<&dyn SearchProvider> {
        self.providers.iter().find(|p| p.name() == name).map(|b| b.as_ref())
    }

    fn available(&self, name: &str) -> bool {
        self.find(name).is_some()
    }

    /// Groupes ordonnés pour une intention. Un groupe interroge tous ses fournisseurs et fusionne ;
    /// le groupe suivant n'est consulté que si rien n'a été trouvé jusque-là (repli).
    pub fn plan(&self, intent: Intent) -> Vec<Vec<&'static str>> {
        let mut groups: Vec<Vec<&'static str>> = Vec::new();
        match intent {
            Intent::Library => groups.push(vec![P_CRATES, P_NPM]),
            Intent::Error => groups.push(vec![P_STACKEXCHANGE]),
            Intent::Repo => groups.push(vec![P_GITHUB]),
            Intent::Encyclopedic => groups.push(vec![P_WIKIPEDIA]),
            Intent::Open => {}
        }
        // Moteurs : clés de l'utilisateur d'abord (préférence explicite en tête), puis DuckDuckGo.
        let mut engines: Vec<&'static str> = vec![P_BRAVE, P_MOJEEK, P_SEARXNG, P_DUCKDUCKGO];
        if let Some(pref) = &self.preferred {
            if let Some(pos) = engines.iter().position(|n| *n == pref.as_str() || (pref == "ddg" && *n == P_DUCKDUCKGO)) {
                let n = engines.remove(pos);
                engines.insert(0, n);
            }
        }
        for e in engines {
            groups.push(vec![e]);
        }
        if intent == Intent::Open {
            groups.push(vec![P_HACKERNEWS]);
        }
        groups.into_iter().map(|g| g.into_iter().filter(|n| self.available(n)).collect::<Vec<_>>()).filter(|g| !g.is_empty()).collect()
    }

    pub fn route(&self, q: &str, limit: usize) -> SearchOutcome {
        self.route_as(q, limit, detect_intent(q))
    }

    pub fn route_as(&self, q: &str, limit: usize, intent: Intent) -> SearchOutcome {
        let limit = clamp_limit(limit);
        let mut tried: Vec<(&'static str, Result<usize, SearchError>)> = Vec::new();
        let q = match prepare_query(q) {
            Ok(q) => q,
            Err(e) => return SearchOutcome { hits: Vec::new(), tried: vec![("routeur", Err(e))], intent, cacheable: false },
        };
        let mut hits: Vec<SearchHit> = Vec::new();
        for group in self.plan(intent) {
            let mut lists: Vec<Vec<SearchHit>> = Vec::new();
            for name in group {
                let Some(p) = self.find(name) else { continue };
                if let Err(rest) = self.breaker.check(name, Instant::now()) {
                    tried.push((name, Err(SearchError::Unavailable(format!("coupe-circuit ouvert, encore {} s", rest.as_secs() + 1)))));
                    continue;
                }
                let wait = self.limiter.reserve(p.host(), p.min_gap(), Instant::now());
                if self.pace && !wait.is_zero() {
                    std::thread::sleep(wait);
                }
                match p.search(&self.fetcher, &q, limit) {
                    Ok(h) => {
                        self.breaker.on_success(name);
                        tried.push((name, Ok(h.len())));
                        lists.push(h);
                    }
                    Err(e) => {
                        // Un quota local n'a rien envoyé : il ne compte pas comme un échec du service.
                        if !matches!(e, SearchError::Quota(_)) {
                            self.breaker.on_failure(name, Instant::now(), matches!(e, SearchError::Blocked(_)));
                        }
                        tried.push((name, Err(e)));
                    }
                }
            }
            let merged = dedup(interleave(lists));
            if !merged.is_empty() {
                hits = merged;
                break;
            }
        }
        hits.truncate(limit);
        let cacheable = !hits.is_empty() && hits.iter().all(|h| self.is_cacheable(h.provider));
        SearchOutcome { hits, tried, intent, cacheable }
    }
}

// ───────────────────────── Sortie ─────────────────────────

/// Texte brut pour l'agent : une ligne par résultat `n. titre — url — fournisseur (licence ; attribution)`
/// puis l'extrait (≤ 200 caractères). SANS balisage : l'appelant cadre avec `untrusted::wrap`.
pub fn render(o: &SearchOutcome) -> String {
    let mut out = String::new();
    out.push_str(&format!("Intention : {} ; {} résultat(s).\n", o.intent.label(), o.hits.len()));
    for (i, h) in o.hits.iter().enumerate() {
        let title = clean_text(&h.title, TITLE_MAX);
        let mut line = format!("{}. {} — {} — {}", i + 1, title, h.url, h.provider);
        let extra: Vec<String> = [h.license.as_deref(), h.attribution.as_deref()]
            .iter()
            .flatten()
            .map(|s| clean_text(s, 120))
            .filter(|s| !s.is_empty())
            .collect();
        if !extra.is_empty() {
            line.push_str(&format!(" ({})", extra.join(" ; ")));
        }
        out.push_str(&line);
        out.push('\n');
        let snip = clean_text(&h.snippet, SNIPPET_RENDER);
        if !snip.is_empty() {
            out.push_str("   ");
            out.push_str(&snip);
            out.push('\n');
        }
    }
    let failed: Vec<String> =
        o.tried.iter().filter_map(|(n, r)| r.as_ref().err().map(|e| format!("{n} ({})", clean_text(&e.to_string(), 100)))).collect();
    if !failed.is_empty() {
        out.push_str(&format!("Sources en échec : {}.\n", failed.join(" ; ")));
    }
    out
}

// ───────────────────────── Tests ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{channel, Receiver};
    use std::sync::Arc;
    use std::thread;

    const CRATES: &str = include_str!("../../bench/web/search-fixtures/crates.json");
    const NPM: &str = include_str!("../../bench/web/search-fixtures/npm.json");
    const GITHUB: &str = include_str!("../../bench/web/search-fixtures/github.json");
    const SE: &str = include_str!("../../bench/web/search-fixtures/stackexchange.json");
    const SE_ERR: &str = include_str!("../../bench/web/search-fixtures/stackexchange-erreur.json");
    const HN: &str = include_str!("../../bench/web/search-fixtures/hackernews.json");
    const WIKI_FR: &str = include_str!("../../bench/web/search-fixtures/wikipedia-fr.json");
    const WIKI_VIDE: &str = include_str!("../../bench/web/search-fixtures/wikipedia-vide.json");
    const BRAVE: &str = include_str!("../../bench/web/search-fixtures/brave.json");
    const MOJEEK: &str = include_str!("../../bench/web/search-fixtures/mojeek.json");
    const SEARXNG: &str = include_str!("../../bench/web/search-fixtures/searxng.json");
    const DDG: &str = include_str!("../../bench/web/search-fixtures/ddg-results.html");
    const DDG_CHALLENGE: &str = include_str!("../../bench/web/search-fixtures/ddg-challenge.html");
    const DDG_EMPTY: &str = include_str!("../../bench/web/search-fixtures/ddg-empty.html");

    fn h(provider: &'static str, title: &str, url: &str) -> SearchHit {
        SearchHit { title: title.into(), url: url.into(), snippet: "extrait".into(), provider, license: None, attribution: None }
    }

    // ── Analyseurs sur fixtures ──

    #[test]
    fn crates_analyse_et_ecarte_les_noms_invalides() {
        let hits = parse_crates(CRATES.as_bytes(), 10).unwrap();
        assert_eq!(hits.len(), 2, "le nom « bad/name » doit être écarté");
        assert_eq!(hits[0].title, "serde_json 1.0.140");
        assert_eq!(hits[0].url, "https://crates.io/crates/serde_json");
        assert!(hits[0].snippet.contains("docs.rs/serde_json/1.0.140"));
        // max_stable_version nul : repli sur max_version.
        assert_eq!(hits[1].title, "serde 1.0.228");
        assert_eq!(hits[1].provider, P_CRATES);
    }

    #[test]
    fn crates_respecte_la_limite_et_le_json_invalide() {
        assert_eq!(parse_crates(CRATES.as_bytes(), 1).unwrap().len(), 1);
        assert!(matches!(parse_crates(b"<html>", 5), Err(SearchError::Parse(_))));
        assert!(matches!(parse_crates(b"{\"autre\":1}", 5), Err(SearchError::Parse(_))));
    }

    #[test]
    fn npm_analyse_les_paquets_et_les_portees() {
        let hits = parse_npm(NPM.as_bytes(), 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "express 5.1.0");
        assert_eq!(hits[0].url, "https://www.npmjs.com/package/express");
        // Sans lien npm : URL construite depuis le nom.
        assert_eq!(hits[1].url, "https://www.npmjs.com/package/@fastify/express");
    }

    #[test]
    fn github_analyse_licence_et_etoiles() {
        let hits = parse_github(GITHUB.as_bytes(), 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "tokio-rs/tokio");
        assert_eq!(hits[0].license.as_deref(), Some("MIT"));
        assert!(hits[0].snippet.contains("★29000") && hits[0].snippet.contains("Rust"));
        // NOASSERTION n'est pas une licence ; description nulle : l'extrait = étoiles.
        assert_eq!(hits[1].license, None);
        assert_eq!(hits[1].snippet, "★3");
    }

    #[test]
    fn stackexchange_porte_licence_et_attribution_dans_chaque_resultat() {
        let p = parse_stackexchange(SE.as_bytes(), 10).unwrap();
        assert_eq!(p.hits.len(), 2, "le lien javascript: doit être écarté");
        for hit in &p.hits {
            assert_eq!(hit.license.as_deref(), Some("CC BY-SA 4.0"));
            let a = hit.attribution.as_deref().expect("attribution obligatoire");
            assert!(a.contains("Stack Overflow"), "{a}");
        }
        assert_eq!(p.hits[0].attribution.as_deref(), Some("© Jean & Co / Stack Overflow"));
        assert_eq!(p.hits[0].title, "Borrow of moved value: why does \"let b = a;\" fail?");
        assert_eq!(p.backoff, Some(5));
        assert_eq!(p.quota_remaining, Some(287));
    }

    #[test]
    fn stackexchange_erreur_de_throttle_est_un_quota() {
        assert!(matches!(parse_stackexchange(SE_ERR.as_bytes(), 5), Err(SearchError::Quota(_))));
    }

    #[test]
    fn hackernews_url_nulle_renvoie_a_la_discussion() {
        let hits = parse_hackernews(HN.as_bytes(), 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].url, "https://example.org/rust-async");
        assert!(hits[0].snippet.contains("https://news.ycombinator.com/item?id=4242"));
        assert_eq!(hits[1].url, "https://news.ycombinator.com/item?id=4243");
        assert_eq!(hits[1].title, "Ask HN: What's your Rust stack?");
    }

    #[test]
    fn wikipedia_decode_extrait_url_et_attribution() {
        let hits = parse_wikipedia(WIKI_FR.as_bytes(), 10, "fr").unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].url, "https://fr.wikipedia.org/wiki/Rust_(langage)");
        assert_eq!(hits[0].snippet, "Rust est un langage de programmation compilé & multi-paradigme");
        assert_eq!(hits[0].license.as_deref(), Some("CC BY-SA 4.0"));
        assert_eq!(hits[0].attribution.as_deref(), Some("Contributeurs de Wikipédia"));
        // Un « / » dans un titre est encodé, pas interprété comme un séparateur.
        assert_eq!(hits[1].url, "https://fr.wikipedia.org/wiki/Rust%2FTest");
        assert!(parse_wikipedia(WIKI_VIDE.as_bytes(), 10, "en").unwrap().is_empty());
    }

    #[test]
    fn ddg_decode_uddg_ecarte_publicites_et_liens_pieges() {
        let hits = parse_ddg(DDG, 10).unwrap();
        let urls: Vec<&str> = hits.iter().map(|h| h.url.as_str()).collect();
        assert_eq!(urls, vec!["https://doc.rust-lang.org/book/", "https://www.rust-lang.org/learn?utm_source=ddg"]);
        assert_eq!(hits[0].title, "The Rust Programming Language");
        assert_eq!(
            hits[0].snippet,
            "Affectionately nicknamed \"the book,\" The Rust Programming Language will give you an overview of the language."
        );
        assert!(hits.iter().all(|h| h.provider == P_DUCKDUCKGO));
    }

    #[test]
    fn ddg_page_de_defi_est_un_blocage() {
        assert!(matches!(parse_ddg(DDG_CHALLENGE, 10), Err(SearchError::Blocked(_))));
    }

    #[test]
    fn ddg_page_sans_resultat_est_vide_mais_une_page_inconnue_est_une_erreur() {
        assert_eq!(parse_ddg(DDG_EMPTY, 10).unwrap(), Vec::new());
        assert!(matches!(parse_ddg("<html><body>autre chose</body></html>", 10), Err(SearchError::Parse(_))));
    }

    #[test]
    fn ddg_decode_href_cas_limites() {
        assert_eq!(
            decode_ddg_href("//duckduckgo.com/l/?uddg=https%3A%2F%2Fa.example%2Fx%3Fq%3D1&rut=z").as_deref(),
            Some("https://a.example/x?q=1")
        );
        assert_eq!(decode_ddg_href("https://duckduckgo.com/y.js?ad_domain=x"), None);
        assert_eq!(decode_ddg_href("//duckduckgo.com/l/?rut=sans-cible"), None);
        assert_eq!(decode_ddg_href("//duckduckgo.com/l/?uddg=javascript%3Aalert(1)"), None);
        assert_eq!(decode_ddg_href("ftp://example.org/x"), None);
        assert_eq!(decode_ddg_href("https://example.org/page").as_deref(), Some("https://example.org/page"));
    }

    #[test]
    fn brave_nettoie_le_balisage_et_ecarte_les_schemas_refuses() {
        let hits = parse_brave(BRAVE.as_bytes(), 10).unwrap();
        assert_eq!(hits.len(), 1, "ftp:// doit être écarté");
        assert_eq!(hits[0].title, "The Rust Programming Language");
        assert!(!hits[0].snippet.contains('<') && hits[0].snippet.contains("\"the book\""));
        assert_eq!(parse_brave(b"{\"type\":\"search\",\"query\":{}}", 5).unwrap(), Vec::new());
        assert!(matches!(parse_brave(b"{\"x\":1}", 5), Err(SearchError::Parse(_))));
    }

    #[test]
    fn mojeek_et_searxng_analysent_leurs_formats() {
        let m = parse_mojeek(MOJEEK.as_bytes(), 10).unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].snippet, "A language empowering everyone to build reliable and efficient software.");
        assert_eq!(m[1].url, "https://en.wikipedia.org/wiki/Rust_(programming_language)");
        let s = parse_searxng(SEARXNG.as_bytes(), 1).unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].provider, P_SEARXNG);
        assert!(matches!(parse_searxng(b"{\"x\":1}", 5), Err(SearchError::Parse(_))));
        assert!(matches!(parse_mojeek(b"{\"response\":{\"status\":\"ERR\"}}", 5), Err(SearchError::Failed(_))));
    }

    // ── Assainissement ──

    #[test]
    fn clean_text_retire_balises_entites_et_invisibles() {
        assert_eq!(clean_text("<b>Rust</b> &amp; C&#43;&#x2B; &lt;3 &nbsp;ok", 100), "Rust & C++ <3 ok");
        assert_eq!(clean_text("a\u{200b}b\u{202e}c", 100), "abc");
        assert_eq!(clean_text("x < y et y > z", 100), "x < y et y > z");
        assert_eq!(clean_text("ligne1\n\n  ligne2\t", 100), "ligne1 ligne2");
        assert!(clean_text(&"é".repeat(50), 10).chars().count() <= 10);
        assert_eq!(clean_text("&inconnue; reste", 100), "&inconnue; reste");
    }

    #[test]
    fn les_urls_non_http_sont_ecartees() {
        assert!(http_url("javascript:alert(1)").is_none());
        assert!(http_url("file:///etc/passwd").is_none());
        assert!(http_url("https://user:pw@example.org/").is_none());
        assert!(http_url("https://example.org/a").is_some());
    }

    #[test]
    fn requete_telle_que_formulee() {
        assert_eq!(prepare_query("  rust\nasync \u{7}book ").unwrap(), "rust async  book");
        assert!(prepare_query("  \n ").is_err());
        assert_eq!(prepare_query(&"a".repeat(1000)).unwrap().chars().count(), MAX_QUERY_CHARS);
    }

    #[test]
    fn les_cles_sont_expurgees_et_masquees() {
        assert_eq!(redact("GET https://x/?api_key=SECRETKEY123&q=a : refus", &["SECRETKEY123"]), "GET https://x/?api_key=***&q=a : refus");
        let b = BraveSearch::new("brave-key-0123456789").unwrap();
        assert!(!format!("{b:?}").contains("0123456789"));
        let m = Mojeek::new("mojeek-key-0123456789").unwrap();
        assert!(!format!("{m:?}").contains("0123456789"));
        let g = GitHubRepos::new(Some("ghp_0123456789abcdef".to_string()));
        assert!(!format!("{g:?}").contains("0123456789"));
        // Une valeur avec CR/LF (injection d'en-tête) ou trop courte n'est jamais acceptée.
        assert!(clean_secret("abc\r\nX-Evil: 1xxxxxxx").is_none());
        assert!(clean_secret("court").is_none());
        assert!(BraveSearch::new("a b c d e f g h").is_none());
    }

    // ── Intention ──

    #[test]
    fn intention_bibliotheque() {
        for q in [
            "crate pour parser du JSON",
            "latest version of tokio crate",
            "npm install express",
            "pip requests version",
            "quelle bibliothèque python pour les PDF, dernière version",
            "cargo add serde features",
        ] {
            assert_eq!(detect_intent(q), Intent::Library, "{q}");
        }
    }

    #[test]
    fn intention_erreur() {
        for q in [
            "error[E0502]: cannot borrow as mutable",
            "TypeError: undefined is not a function",
            "thread 'main' panicked at src/main.rs:4",
            "erreur de segmentation en C++",
            "ERR_CONNECTION_REFUSED chrome",
            "TS2345 argument of type",
            "ECONNREFUSED 127.0.0.1:5432",
            "npm install error ENOENT",
            "impossible de lire le fichier Cargo.lock",
        ] {
            assert_eq!(detect_intent(q), Intent::Error, "{q}");
        }
    }

    #[test]
    fn intention_depot() {
        for q in [
            "github repo for rust tui library",
            "dépôt github d'un éditeur markdown",
            "awesome rust github projects",
            "repository gitignore templates",
        ] {
            assert_eq!(detect_intent(q), Intent::Repo, "{q}");
        }
    }

    #[test]
    fn intention_encyclopedique() {
        for q in [
            "qu'est-ce que la photosynthèse",
            "Qui est Ada Lovelace ?",
            "what is a monad",
            "who was Alan Turing",
            "c'est quoi un trou noir",
        ] {
            assert_eq!(detect_intent(q), Intent::Encyclopedic, "{q}");
        }
    }

    #[test]
    fn intention_ouverte_par_defaut_et_priorites() {
        for q in ["meilleur restaurant lyon", "comparatif claviers mécaniques", "", "how to center a div"] {
            assert_eq!(detect_intent(q), Intent::Open, "{q}");
        }
        // Un « what is » qui contient du code reste ouvert ; une erreur l'emporte sur tout.
        assert_eq!(detect_intent("what is Vec<T>::push()"), Intent::Open);
        assert_eq!(detect_intent("what is the npm error ENOENT"), Intent::Error);
        assert_eq!(detect_intent("github repo serde crate"), Intent::Repo);
    }

    #[test]
    fn langue_de_la_requete() {
        assert_eq!(detect_lang("qu'est-ce que la photosynthèse"), "fr");
        assert_eq!(detect_lang("histoire de la Rome antique"), "fr");
        assert_eq!(detect_lang("Rust programming language"), "en");
    }

    // ── Limiteur, coupe-circuit, quotas ──

    #[test]
    fn limiteur_espace_les_requetes_par_hote() {
        let l = Limiter::new();
        let t0 = Instant::now();
        let gap = Duration::from_secs(1);
        assert_eq!(l.reserve("crates.io", gap, t0), Duration::ZERO);
        assert_eq!(l.reserve("crates.io", gap, t0), Duration::from_secs(1));
        assert_eq!(l.reserve("crates.io", gap, t0), Duration::from_secs(2));
        // Un autre hôte n'est pas gêné.
        assert_eq!(l.reserve("npm", gap, t0), Duration::ZERO);
        // Plus tard, le créneau est libre.
        assert_eq!(l.reserve("crates.io", gap, t0 + Duration::from_secs(10)), Duration::ZERO);
    }

    #[test]
    fn coupe_circuit_trois_echecs_puis_quinze_minutes() {
        let b = Breaker::new(3, Duration::from_secs(900));
        let t0 = Instant::now();
        b.on_failure("x", t0, false);
        b.on_failure("x", t0, false);
        assert!(b.check("x", t0).is_ok());
        b.on_failure("x", t0, false);
        let rest = b.check("x", t0 + Duration::from_secs(10)).unwrap_err();
        assert!(rest <= Duration::from_secs(890) && rest > Duration::from_secs(880));
        assert!(b.check("x", t0 + Duration::from_secs(901)).is_ok());
        // Demi-ouvert : un seul nouvel échec rouvre.
        b.on_failure("x", t0 + Duration::from_secs(902), false);
        assert!(b.check("x", t0 + Duration::from_secs(903)).is_err());
        // Un succès remet à zéro.
        b.on_success("x");
        assert!(b.check("x", t0 + Duration::from_secs(904)).is_ok());
    }

    #[test]
    fn un_blocage_ouvre_le_circuit_immediatement() {
        let b = Breaker::new(3, Duration::from_secs(900));
        let t0 = Instant::now();
        b.on_failure("ddg", t0, true);
        assert!(b.check("ddg", t0).is_err());
        assert!(b.check("autre", t0).is_ok());
    }

    #[test]
    fn fenetre_de_quota_glissante() {
        let mut w = QuotaWindow::new(3, Duration::from_secs(60));
        let t0 = Instant::now();
        for i in 0..3 {
            assert!(w.try_take(t0 + Duration::from_secs(i)).is_ok());
        }
        let wait = w.try_take(t0 + Duration::from_secs(5)).unwrap_err();
        assert_eq!(wait, Duration::from_secs(55));
        assert!(w.try_take(t0 + Duration::from_secs(61)).is_ok());
        assert_eq!(w.used(), 2);
    }

    // ── Fusion et normalisation ──

    #[test]
    fn normalisation_d_url_pour_la_deduplication() {
        let a = normalize_url("https://www.Example.org/Doc/?utm_source=x&id=3#frag");
        let b = normalize_url("http://example.org/Doc?id=3");
        assert_eq!(a, b);
        assert_ne!(normalize_url("https://example.org/a"), normalize_url("https://example.org/b"));
        assert_eq!(normalize_url("https://example.org/"), normalize_url("https://example.org"));
    }

    #[test]
    fn deduplication_et_alternance() {
        let merged = dedup(interleave(vec![
            vec![h("crates.io", "a", "https://a.example/x"), h("crates.io", "b", "https://b.example/")],
            vec![h("npm", "a'", "http://www.a.example/x/"), h("npm", "c", "https://c.example/")],
        ]));
        let urls: Vec<&str> = merged.iter().map(|x| x.url.as_str()).collect();
        assert_eq!(urls, vec!["https://a.example/x", "https://b.example/", "https://c.example/"]);
    }

    // ── Routeur avec fournisseurs simulés ──

    struct Fake {
        name: &'static str,
        cacheable: bool,
        replies: Mutex<VecDeque<Result<Vec<SearchHit>, SearchError>>>,
        calls: Arc<AtomicUsize>,
    }

    fn fake(name: &'static str, replies: Vec<Result<Vec<SearchHit>, SearchError>>) -> (Box<dyn SearchProvider>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (Box::new(Fake { name, cacheable: true, replies: Mutex::new(replies.into()), calls: calls.clone() }), calls)
    }

    impl SearchProvider for Fake {
        fn name(&self) -> &'static str {
            self.name
        }
        fn cacheable(&self) -> bool {
            self.cacheable
        }
        fn search(&self, _f: &Fetcher, _q: &str, _l: usize) -> Result<Vec<SearchHit>, SearchError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.replies.lock().unwrap().pop_front().unwrap_or(Ok(Vec::new()))
        }
    }

    fn router(providers: Vec<Box<dyn SearchProvider>>) -> Router {
        Router::new(providers, Fetcher::new(Policy::default())).without_pacing()
    }

    #[test]
    fn plan_par_intention_sans_cle_utilisateur() {
        let r = Router::new(
            vec![
                Box::new(CratesIo::new()),
                Box::new(Npm::new()),
                Box::new(GitHubRepos::new(None)),
                Box::new(StackExchange::new()),
                Box::new(HackerNews::new()),
                Box::new(Wikipedia::new()),
                Box::new(DuckDuckGo::new()),
            ],
            Fetcher::new(Policy::default()),
        );
        assert_eq!(r.plan(Intent::Library), vec![vec![P_CRATES, P_NPM], vec![P_DUCKDUCKGO]]);
        assert_eq!(r.plan(Intent::Error), vec![vec![P_STACKEXCHANGE], vec![P_DUCKDUCKGO]]);
        assert_eq!(r.plan(Intent::Repo), vec![vec![P_GITHUB], vec![P_DUCKDUCKGO]]);
        assert_eq!(r.plan(Intent::Encyclopedic), vec![vec![P_WIKIPEDIA], vec![P_DUCKDUCKGO]]);
        assert_eq!(r.plan(Intent::Open), vec![vec![P_DUCKDUCKGO], vec![P_HACKERNEWS]]);
    }

    #[test]
    fn plan_ouvert_met_la_cle_de_l_utilisateur_en_tete() {
        let brave = Box::new(BraveSearch::new("brave-key-0123456789").unwrap()) as Box<dyn SearchProvider>;
        let r = Router::new(vec![brave, Box::new(DuckDuckGo::new()), Box::new(HackerNews::new())], Fetcher::new(Policy::default()));
        assert_eq!(r.plan(Intent::Open), vec![vec![P_BRAVE], vec![P_DUCKDUCKGO], vec![P_HACKERNEWS]]);
        let r = r.with_preferred("ddg");
        assert_eq!(r.plan(Intent::Open)[0], vec![P_DUCKDUCKGO]);
    }

    #[test]
    fn le_groupe_structure_fusionne_et_ne_declenche_pas_le_repli() {
        let (crates, c1) = fake(P_CRATES, vec![Ok(vec![h(P_CRATES, "serde", "https://crates.io/crates/serde")])]);
        let (npm, c2) = fake(P_NPM, vec![Ok(vec![h(P_NPM, "serde", "https://www.npmjs.com/package/serde")])]);
        let (ddg, c3) = fake(P_DUCKDUCKGO, vec![]);
        let o = router(vec![crates, npm, ddg]).route("crate serde version", 5);
        assert_eq!(o.intent, Intent::Library);
        assert_eq!(o.hits.len(), 2);
        assert_eq!((c1.load(Ordering::SeqCst), c2.load(Ordering::SeqCst), c3.load(Ordering::SeqCst)), (1, 1, 0));
        assert!(o.cacheable);
    }

    #[test]
    fn repli_sur_ddg_quand_la_source_structuree_echoue() {
        let (se, _) = fake(P_STACKEXCHANGE, vec![Err(SearchError::Failed("HTTP 500".into()))]);
        let (ddg, c) = fake(P_DUCKDUCKGO, vec![Ok(vec![h(P_DUCKDUCKGO, "r", "https://r.example/")])]);
        let o = router(vec![se, ddg]).route("error[E0502] cannot borrow", 5);
        assert_eq!(o.intent, Intent::Error);
        assert_eq!(o.hits.len(), 1);
        assert_eq!(c.load(Ordering::SeqCst), 1);
        assert_eq!(o.tried.len(), 2);
        assert!(o.tried[0].1.is_err() && o.tried[1].1 == Ok(1));
        // DuckDuckGo (simulé ici comme cacheable) : le drapeau suit le fournisseur réel du résultat.
        assert!(o.cacheable);
    }

    #[test]
    fn ddg_reel_n_est_jamais_cacheable_et_brave_non_plus() {
        let r = router(vec![Box::new(DuckDuckGo::new()), Box::new(BraveSearch::new("brave-key-0123456789").unwrap())]);
        assert!(!r.is_cacheable(P_DUCKDUCKGO));
        assert!(!r.is_cacheable(P_BRAVE));
        assert!(!Mojeek::new("mojeek-key-0123456789").unwrap().cacheable());
        assert!(CratesIo::new().cacheable() && StackExchange::new().cacheable());
    }

    #[test]
    fn un_blocage_suspend_le_fournisseur_pour_les_requetes_suivantes() {
        let (ddg, calls) =
            fake(P_DUCKDUCKGO, vec![Err(SearchError::Blocked("HTTP 202".into())), Ok(vec![h(P_DUCKDUCKGO, "x", "https://x.example/")])]);
        let r = router(vec![ddg]);
        let o1 = r.route("meilleur restaurant lyon", 5);
        assert!(o1.hits.is_empty());
        let o2 = r.route("autre question ouverte", 5);
        assert!(o2.hits.is_empty(), "le circuit ouvert ne doit pas rappeler le fournisseur");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(matches!(o2.tried[0].1, Err(SearchError::Unavailable(_))));
    }

    #[test]
    fn trois_echecs_ordinaires_ouvrent_le_circuit_mais_pas_un_quota() {
        let fails = || Err(SearchError::Failed("HTTP 500".into()));
        let (p, calls) = fake(P_DUCKDUCKGO, vec![fails(), fails(), fails(), Ok(vec![h(P_DUCKDUCKGO, "x", "https://x.example/")])]);
        let r = router(vec![p]);
        for _ in 0..5 {
            r.route("une question ouverte", 3);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        let q = || Err(SearchError::Quota("épuisé".into()));
        let (p, calls) = fake(P_DUCKDUCKGO, vec![q(), q(), q(), q()]);
        let r = router(vec![p]);
        for _ in 0..4 {
            r.route("une question ouverte", 3);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 4, "un quota n'ouvre pas le circuit");
    }

    #[test]
    fn la_limite_tronque_et_la_requete_vide_est_refusee() {
        let many: Vec<SearchHit> = (0..8).map(|i| h(P_DUCKDUCKGO, "t", &format!("https://s{i}.example/"))).collect();
        let (p, _) = fake(P_DUCKDUCKGO, vec![Ok(many)]);
        assert_eq!(router(vec![p]).route("question ouverte", 3).hits.len(), 3);
        let o = router(vec![]).route("   ", 3);
        assert!(o.hits.is_empty() && o.tried[0].1.is_err());
    }

    // ── Sortie ──

    #[test]
    fn rendu_court_sans_balisage_avec_licence_et_attribution() {
        let mut hit = h(P_STACKEXCHANGE, "Titre <b>gras</b>\nsuite", "https://stackoverflow.com/questions/1/x");
        hit.license = Some("CC BY-SA 4.0".into());
        hit.attribution = Some("© Jean / Stack Overflow".into());
        hit.snippet = "é".repeat(500);
        let o = SearchOutcome {
            hits: vec![hit],
            tried: vec![(P_STACKEXCHANGE, Ok(1)), (P_DUCKDUCKGO, Err(SearchError::Blocked("HTTP 202".into())))],
            intent: Intent::Error,
            cacheable: true,
        };
        let s = render(&o);
        assert!(
            s.contains(
                "1. Titre gras suite — https://stackoverflow.com/questions/1/x — stackexchange (CC BY-SA 4.0 ; © Jean / Stack Overflow)"
            ),
            "{s}"
        );
        let snippet_line = s.lines().find(|l| l.starts_with("   é")).unwrap();
        assert!(snippet_line.trim().chars().count() <= SNIPPET_RENDER);
        assert!(s.contains("Sources en échec : duckduckgo"));
        assert!(!s.contains("<b>") && !s.contains("cortex-web-data"));
    }

    #[test]
    fn rendu_sans_resultat() {
        let o = SearchOutcome {
            hits: vec![],
            tried: vec![(P_DUCKDUCKGO, Err(SearchError::Parse("x".into())))],
            intent: Intent::Open,
            cacheable: false,
        };
        let s = render(&o);
        assert!(s.starts_with("Intention : ouverte ; 0 résultat(s)."));
        assert!(s.contains("Sources en échec"));
    }

    // ── Réseau local (127.0.0.1, port 0) : en-têtes, classification, SearXNG privé ──

    fn serve(responses: Vec<(u16, &'static str, String)>) -> (SocketAddr, Receiver<String>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let (tx, rx) = channel();
        thread::spawn(move || {
            for (status, ct, body) in responses {
                let Ok((mut s, _)) = l.accept() else { break };
                let mut buf = [0u8; 4096];
                let mut req = Vec::new();
                loop {
                    let n = s.read(&mut buf).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    req.extend_from_slice(&buf[..n]);
                    if req.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let _ = tx.send(String::from_utf8_lossy(&req).to_string());
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes());
                let _ = s.flush();
            }
        });
        (addr, rx)
    }

    fn local_fetcher(addr: SocketAddr) -> Fetcher {
        Fetcher::new(Policy { allow_addrs: vec![addr], ..Policy::default() })
    }

    #[test]
    fn brave_envoie_la_cle_en_en_tete_et_pas_dans_l_url() {
        let (addr, rx) = serve(vec![(200, "application/json", BRAVE.to_string())]);
        let mut p = BraveSearch::new("brave-key-0123456789").unwrap();
        p.base = format!("http://{addr}/res/v1/web/search");
        let hits = p.search(&local_fetcher(addr), "rust book", 5).unwrap();
        assert_eq!(hits.len(), 1);
        let req = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let lower = req.to_ascii_lowercase();
        assert!(lower.starts_with("get /res/v1/web/search?q=rust+book&count=5"), "{req}");
        assert!(lower.contains("x-subscription-token: brave-key-0123456789"), "{req}");
        assert!(lower.contains("user-agent: cortexbot/"), "User-Agent honnête attendu : {req}");
        assert!(!req.lines().next().unwrap().contains("brave-key"), "la clé ne doit jamais être dans l'URL");
        assert!(!lower.contains("cookie") && !lower.contains("referer"));
    }

    #[test]
    fn brave_erreur_http_ne_fuit_pas_la_cle() {
        let (addr, _rx) = serve(vec![(401, "application/json", "{}".to_string())]);
        let mut p = BraveSearch::new("brave-key-0123456789").unwrap();
        p.base = format!("http://{addr}/x");
        let e = p.search(&local_fetcher(addr), "rust", 5).unwrap_err();
        assert_eq!(e, SearchError::Failed("HTTP 401".to_string()));
        assert!(!format!("{e}").contains("brave-key"));
    }

    #[test]
    fn mojeek_met_la_cle_dans_la_requete_mais_l_erreur_l_expurge() {
        let (addr, rx) = serve(vec![(200, "application/json", MOJEEK.to_string())]);
        let mut p = Mojeek::new("mojeek-key-0123456789").unwrap();
        p.base = format!("http://{addr}/search");
        assert_eq!(p.search(&local_fetcher(addr), "rust", 5).unwrap().len(), 2);
        let req = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(req.contains("api_key=mojeek-key-0123456789") && req.contains("fmt=json"));
        // Hôte injoignable : le message d'erreur de ureq cite l'URL, la clé doit en être retirée.
        let mut q = Mojeek::new("mojeek-key-0123456789").unwrap();
        q.base = "http://127.0.0.1:9/search".to_string(); // refusé par la garde (adresse non tolérée)
        let e = q.search(&Fetcher::new(Policy::default()), "rust", 5).unwrap_err();
        assert!(!format!("{e}").contains("mojeek-key"), "{e}");
    }

    #[test]
    fn ddg_http_202_et_429_sont_des_blocages() {
        let (addr, _rx) = serve(vec![(202, "text/html", "<html></html>".to_string())]);
        let mut p = DuckDuckGo::new();
        p.base = format!("http://{addr}/html/");
        assert!(matches!(p.search(&local_fetcher(addr), "rust", 5), Err(SearchError::Blocked(_))));
        let (addr, _rx) = serve(vec![(429, "text/html", "slow down".to_string())]);
        p.base = format!("http://{addr}/html/");
        assert!(matches!(p.search(&local_fetcher(addr), "rust", 5), Err(SearchError::Blocked(_))));
        let (addr, _rx) = serve(vec![(500, "text/html", "oops".to_string())]);
        p.base = format!("http://{addr}/html/");
        assert!(matches!(p.search(&local_fetcher(addr), "rust", 5), Err(SearchError::Failed(_))));
    }

    #[test]
    fn ddg_page_de_defi_en_200_est_un_blocage_de_bout_en_bout() {
        let (addr, _rx) = serve(vec![(200, "text/html; charset=UTF-8", DDG_CHALLENGE.to_string())]);
        let mut p = DuckDuckGo::new();
        p.base = format!("http://{addr}/html/");
        assert!(matches!(p.search(&local_fetcher(addr), "rust", 5), Err(SearchError::Blocked(_))));
    }

    #[test]
    fn stackexchange_de_bout_en_bout_backoff_et_compteur() {
        let (addr, rx) = serve(vec![(200, "application/json; charset=utf-8", SE.to_string())]);
        let mut p = StackExchange::new();
        p.base = format!("http://{addr}/2.3/search/advanced");
        let f = local_fetcher(addr);
        let hits = p.search(&f, "borrow of moved value", 5).unwrap();
        assert_eq!(hits.len(), 2);
        let req = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(req.contains("site=stackoverflow") && req.contains("pagesize=5"), "{req}");
        // La réponse demandait un backoff de 5 s : la requête suivante est refusée localement.
        assert!(matches!(p.search(&f, "autre", 5), Err(SearchError::Quota(_))));
    }

    #[test]
    fn quota_local_stackexchange_epuise_sans_requete() {
        let p = StackExchange::new();
        {
            let mut st = lock(&p.state);
            st.quota = QuotaWindow::new(0, Duration::from_secs(86400));
        }
        // Aucune connexion possible (hôte public jamais contacté : le quota coupe avant).
        let e = p.search(&Fetcher::new(Policy::default()), "x", 5).unwrap_err();
        assert!(matches!(e, SearchError::Quota(_)), "{e:?}");
    }

    #[test]
    fn github_sans_jeton_limite_a_dix_par_minute() {
        let p = GitHubRepos::new(None);
        {
            let mut q = lock(&p.quota);
            for _ in 0..10 {
                q.try_take(Instant::now()).unwrap();
            }
        }
        assert!(matches!(p.search(&Fetcher::new(Policy::default()), "tokio", 3), Err(SearchError::Quota(_))));
        assert!(!p.has_token());
        assert!(GitHubRepos::new(Some("ghp_0123456789".into())).has_token());
    }

    #[test]
    fn github_avec_jeton_l_envoie_en_en_tete() {
        let (addr, rx) = serve(vec![(200, "application/json; charset=utf-8", GITHUB.to_string())]);
        let mut p = GitHubRepos::new(Some("ghp_0123456789abcdef".to_string()));
        p.base = format!("http://{addr}/search/repositories");
        let hits = p.search(&local_fetcher(addr), "tokio", 5).unwrap();
        assert_eq!(hits.len(), 2);
        let req = rx.recv_timeout(Duration::from_secs(5)).unwrap().to_ascii_lowercase();
        assert!(req.contains("authorization: bearer ghp_0123456789abcdef"), "{req}");
        assert!(!req.lines().next().unwrap().contains("ghp_"));
    }

    #[test]
    fn wikipedia_de_bout_en_bout_avec_repli_de_langue() {
        let (addr, rx) =
            serve(vec![(200, "application/json; charset=utf-8", WIKI_VIDE.to_string()), (200, "application/json", WIKI_FR.to_string())]);
        let mut p = Wikipedia::new();
        p.base = format!("http://{addr}");
        let hits = p.search(&local_fetcher(addr), "qu'est-ce que Rust", 5).unwrap();
        assert_eq!(hits.len(), 2, "la 2e langue doit être essayée quand la 1re est vide");
        let r1 = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(r1.contains("srsearch=qu%27est-ce+que+Rust") || r1.contains("srsearch=qu%27est%2Dce+que+Rust"), "{r1}");
    }

    #[test]
    fn searxng_public_valide_par_check_url_et_prive_seulement_si_autorise() {
        // URL publique : acceptée sans résolution DNS.
        let ok = SearxngProvider::configure("https://searx.example.org", false).unwrap();
        assert!(ok.own.is_none());
        assert_eq!(ok.endpoint().as_str(), "https://searx.example.org/search");
        // Adresse privée / localhost : refusée par défaut, même si l'on insiste sur le port.
        for u in
            ["http://127.0.0.1:8080", "http://localhost:8080", "http://192.168.1.5:8080", "http://[::1]:8080", "http://169.254.169.254/"]
        {
            assert!(SearxngProvider::configure(u, false).is_err(), "{u}");
        }
        // Schéma et identifiants restent refusés même avec l'autorisation.
        assert!(SearxngProvider::configure("file:///etc/passwd", true).is_err());
        assert!(SearxngProvider::configure("http://u:p@127.0.0.1:8080", true).is_err());
        // Avec l'autorisation explicite : acceptée, via un Fetcher dédié.
        let p = SearxngProvider::configure("http://localhost:8080/", true).unwrap();
        assert!(p.own.is_some());
        assert_eq!(p.base.host_str(), Some("127.0.0.1"));
    }

    #[test]
    fn searxng_prive_de_bout_en_bout() {
        let (addr, rx) = serve(vec![(200, "application/json", SEARXNG.to_string())]);
        let p = SearxngProvider::configure(&format!("http://127.0.0.1:{}", addr.port()), true).unwrap();
        // Le Fetcher passé par l'appelant (politique par défaut) ne tolère PAS l'adresse : seul le Fetcher dédié passe.
        let hits = p.search(&Fetcher::new(Policy::default()), "rust", 5).unwrap();
        assert_eq!(hits.len(), 2);
        let req = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(req.starts_with("GET /search?q=rust&format=json"), "{req}");
        // Et la même URL sans autorisation ne s'ouvre pas avec le Fetcher par défaut.
        assert!(SearxngProvider::configure(&format!("http://127.0.0.1:{}", addr.port()), false).is_err());
    }

    #[test]
    fn en_tetes_d_auth_via_le_fetcher_refusent_adresses_privees_et_redirections_vers_le_prive() {
        let f = Fetcher::new(Policy::default());
        let h = [("X-Subscription-Token", "tok-0123456789")];
        let u = Url::parse("http://127.0.0.1:1/x").unwrap();
        assert!(matches!(f.fetch_with_headers(&u, &Validators::default(), &h), FetchOutcome::Blocked(_)));
        let u = Url::parse("file:///etc/passwd").unwrap();
        assert!(matches!(f.fetch_with_headers(&u, &Validators::default(), &h), FetchOutcome::Blocked(_)));
        // Une redirection vers une adresse interne est refusée par la garde (et l'en-tête ne la suit pas).
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        thread::spawn(move || {
            if let Ok((mut s, _)) = l.accept() {
                let mut b = [0u8; 2048];
                let _ = s.read(&mut b);
                let _ = s.write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            }
        });
        let pol = Policy { allow_addrs: vec![addr], ..Policy::default() };
        let u = Url::parse(&format!("http://{addr}/")).unwrap();
        let r = Fetcher::new(pol).fetch_with_headers(&u, &Validators::default(), &h);
        assert!(matches!(r, FetchOutcome::Blocked(super::super::BlockReason::RedirectToBlocked(_))), "{r:?}");
        // Un nom hors liste blanche ne part jamais : refus avant tout accès réseau.
        let r = f.fetch_with_headers(&Url::parse("https://example.com/").unwrap(), &Validators::default(), &[("Cookie", "sid=1")]);
        assert!(matches!(r, FetchOutcome::Failed(_)), "{r:?}");
    }

    #[test]
    fn route_de_bout_en_bout_sur_serveur_local_avec_fournisseur_reel() {
        let (addr, _rx) = serve(vec![(200, "application/json", CRATES.to_string())]);
        let mut c = CratesIo::new();
        c.base = format!("http://{addr}/api/v1/crates");
        let r = Router::new(vec![Box::new(c)], local_fetcher(addr)).without_pacing();
        let o = r.route("crate serde json", 5);
        assert_eq!(o.intent, Intent::Library);
        assert_eq!(o.hits.len(), 2);
        let s = render(&o);
        assert!(s.contains("1. serde_json 1.0.140 — https://crates.io/crates/serde_json — crates.io"), "{s}");
    }

    // ── Test « live » : UNE requête par fournisseur, espacées, User-Agent honnête ──

    #[test]
    #[ignore = "réseau réel : lancé à la main, une requête par fournisseur"]
    fn live_une_requete_par_fournisseur() {
        let fetcher = Fetcher::new(Policy::default());
        let cases: Vec<(Box<dyn SearchProvider>, &str)> = vec![
            (Box::new(CratesIo::new()), "serde json"),
            (Box::new(Npm::new()), "express"),
            (Box::new(GitHubRepos::from_env()), "tokio rust"),
            (Box::new(StackExchange::new()), "borrow of moved value rust"),
            (Box::new(HackerNews::new()), "rust async"),
            (Box::new(Wikipedia::new()), "Rust langage de programmation"),
            (Box::new(DuckDuckGo::new()), "rust programming language book"),
        ];
        let mut lines = Vec::new();
        for (p, q) in cases {
            let t = Instant::now();
            let res = p.search(&fetcher, q, 3);
            let ms = t.elapsed().as_millis();
            let line = match &res {
                Ok(h) => format!(
                    "{:<14} OK        {} résultat(s) en {} ms ; 1er : {}",
                    p.name(),
                    h.len(),
                    ms,
                    h.first().map(|x| x.url.as_str()).unwrap_or("-")
                ),
                Err(SearchError::Blocked(m)) => format!("{:<14} BLOCAGE   {} (en {} ms)", p.name(), m, ms),
                Err(SearchError::Quota(m)) => format!("{:<14} QUOTA     {} (en {} ms)", p.name(), m, ms),
                Err(e) => format!("{:<14} ECHEC     {} (en {} ms)", p.name(), e, ms),
            };
            println!("{line}");
            lines.push(line);
            thread::sleep(Duration::from_millis(2500));
        }
        for (name, var) in [("brave", "BRAVE_API_KEY"), ("mojeek", "MOJEEK_API_KEY"), ("searxng", "CORTEX_SEARXNG_URL")] {
            let l = format!("{name:<14} NON TESTÉ  (variable {var} absente : fournisseur à clé de l'utilisateur)");
            if std::env::var(var).map(|v| v.trim().is_empty()).unwrap_or(true) {
                println!("{l}");
                lines.push(l);
            }
        }
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join("web-tests").join("search");
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join("live.txt"), lines.join("\n") + "\n");
    }
}
