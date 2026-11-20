//! Cortex Web — recherche web et documentation maison (EXPÉRIMENTAL, feature `web`).
//!
//! Conception figée dans `docs/WEB.md` (décisions, sécurité, frontière avec Spectra, banc).
//! Ce fichier ne porte que les TYPES PARTAGÉS et la liste des modules ; chaque module a un
//! seul auteur. Invariants : (1) toute requête réseau passe par `guard::Fetcher` ; (2) tout
//! texte venu du web qui sort vers un agent passe par `untrusted::wrap` ; (3) rien ici ne
//! change le comportement de `find`/`card`/`read`/`docs` : le classement du code est intact.

#![allow(dead_code)]

pub mod bench;
pub mod cli;
pub mod crawl;
pub mod extract;
pub mod guard;
pub mod index;
pub mod link;
pub mod resolve;
pub mod robots;
pub mod search;
pub mod sitemap;
pub mod store;
pub mod untrusted;

use std::net::IpAddr;
use std::time::Duration;
use url::Url;

/// Version de l'extracteur : l'incrémenter force la ré-extraction (sans retélécharger si 304).
pub const EXTRACTOR_VERSION: u32 = 2;

/// Jeton d'agent utilisateur (aucune donnée locale, aucun nom de projet).
pub const USER_AGENT: &str = concat!("CortexBot/", env!("CARGO_PKG_VERSION"), " (+https://github.com/AstroQuestStudio/cortex)");

// ───────────────────────── Sécurité / collecte ─────────────────────────

/// Politique d'une requête sortante. `Default` (à écrire dans `guard`) = la plus stricte raisonnable.
#[derive(Clone, Debug)]
pub struct Policy {
    /// Ports autorisés ; vide = {80, 443}.
    pub allowed_ports: Vec<u16>,
    /// Plafond du corps APRÈS décompression (octets). Défaut 5 Mio.
    pub max_body_bytes: usize,
    /// Redirections suivies à la main (chaque saut est revalidé). Défaut 5.
    pub max_redirects: u8,
    pub connect_timeout: Duration, // 10 s
    pub total_timeout: Duration,   // 30 s, de la connexion au dernier octet
    pub respect_robots: bool,      // vrai ; RFC 9309
    /// Types MIME acceptés (préfixe). Défaut : text/html, application/xhtml+xml, text/markdown,
    /// text/plain, text/xml, application/xml, application/json, application/rss+xml, application/atom+xml.
    pub allowed_content_types: Vec<String>,
    /// TESTS UNIQUEMENT : adresses privées explicitement tolérées (ex. serveur de test en
    /// 127.0.0.1:port). Vide en production ; JAMAIS alimentée par un outil MCP ni par une page.
    pub allow_addrs: Vec<std::net::SocketAddr>,
}

/// Résultat d'une récupération.
#[derive(Debug)]
pub enum FetchOutcome {
    Ok(Response),
    /// 304 : le contenu n'a pas changé (validateurs ETag / Last-Modified).
    NotModified,
    Blocked(BlockReason),
    Failed(String),
}

#[derive(Debug)]
pub struct Response {
    pub final_url: Url,
    /// Chaîne de redirections suivies (hors URL finale).
    pub redirects: Vec<Url>,
    pub status: u16,
    pub content_type: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub body: Vec<u8>,
    /// Vrai si le plafond de taille a coupé le corps.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockReason {
    Scheme(String),
    Port(u16),
    Credentials,
    PrivateAddress(IpAddr),
    ResolveFailed,
    Robots,
    ContentType(String),
    RedirectLimit,
    RedirectToBlocked(Box<BlockReason>),
    Timeout,
}

// ───────────────────────── Documents ─────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Completeness {
    Complete,
    /// Contenu probablement incomplet (page quasi vide, extraction faible).
    Partial,
    /// Page à rendu JavaScript : voir `docs/WEB.md` §Frontière Spectra.
    NeedsJs,
    Blocked,
}

/// Une section = un titre et son contenu en Markdown dense. Unité de recherche et de citation.
#[derive(Clone, Debug)]
pub struct Section {
    /// Ancre stable dans la page (slug du titre, `~2` si doublon). Jamais un hash du contenu.
    pub anchor: String,
    /// Chemin de titres : ["Guide", "Routing", "Params"].
    pub heading_path: Vec<String>,
    pub level: u8,
    /// Markdown (blocs de code, tableaux et signatures conservés tels quels).
    pub text: String,
    pub code_blocks: u32,
}

#[derive(Clone, Debug)]
pub struct Page {
    pub url: Url,
    pub canonical: Option<Url>,
    pub title: String,
    pub lang: Option<String>,
    /// Version de la bibliothèque annoncée par la page ou son URL (/v2/, /0.21.3/, /stable/).
    pub version_hint: Option<String>,
    pub sections: Vec<Section>,
    /// Liens sortants bruts (le filtrage de portée est fait par `crawl`).
    pub links: Vec<Url>,
    /// 0.0..=1.0, heuristique (voir `extract`).
    pub quality: f32,
    pub completeness: Completeness,
    /// Nombre de lignes de la page ressemblant à des instructions adressées à une IA.
    pub injection_signals: u32,
}

/// Ligne du manifeste de collecte (`pages.jsonl`) : de quoi décider « rien à refaire ».
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PageRecord {
    pub url: String,
    pub canonical: Option<String>,
    pub status: u16,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// blake3 hex des octets reçus / du texte normalisé extrait.
    pub raw_hash: String,
    pub content_hash: String,
    pub extractor_version: u32,
    pub fetched_at: u64,
    pub completeness: Completeness,
    pub quality: f32,
    pub sections: u32,
    pub bytes: u64,
}

/// Identifiant stable cité aux agents : `W:<source>/<chemin-de-page>#<ancre>`.
pub fn section_id(source: &str, page_path: &str, anchor: &str) -> String {
    format!("W:{}/{}#{}", source, page_path.trim_matches('/'), anchor)
}

// ───────────────────────── Stockage (sérialisé) ─────────────────────────

/// Configuration d'une source (une documentation collectée), écrite dans `source.json`.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SourceConfig {
    pub name: String,
    /// URL de départ (au moins une).
    pub seeds: Vec<String>,
    /// Hôtes autorisés (jamais d'autre hôte que ceux-ci, redirections comprises).
    pub allowed_hosts: Vec<String>,
    /// Préfixe de chemin autorisé (ex. "/docs/") ; "/" = tout l'hôte.
    pub path_prefix: String,
    /// Version de bibliothèque visée (celle du lockfile), si connue.
    pub version: Option<String>,
    pub max_pages: u32,
    pub max_depth: u8,
    /// Octets reçus maximum pour toute l'exécution.
    pub max_total_bytes: u64,
    /// Secondes maximum pour toute l'exécution.
    pub max_seconds: u64,
    /// Sous-ensemble : un chemin n'est suivi que s'il commence par l'un de ces préfixes (comparaison de
    /// texte, sans frontière : `/3.13/library/asyncio` couvre `asyncio-task.html`). Vide = tout le
    /// `path_prefix`. Les graines déclarées restent toujours permises.
    #[serde(default)]
    pub include_paths: Vec<String>,
    /// Préfixes de chemin jamais suivis (versions anciennes, langues, doublons) ; l'emporte sur `include_paths`.
    #[serde(default)]
    pub exclude_paths: Vec<String>,
}

/// Une section stockée (ligne de `sections.jsonl`) : unité de recherche et de citation.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct StoredSection {
    /// `W:<source>/<page>#<ancre>`
    pub id: String,
    pub page_url: String,
    pub page_title: String,
    pub anchor: String,
    pub heading_path: Vec<String>,
    pub level: u8,
    pub text: String,
    pub code_blocks: u32,
    pub version: Option<String>,
    pub fetched_at: u64,
    pub completeness: Completeness,
}
