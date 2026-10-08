//! Extraction : parcours gitignore-aware d'un projet, détection du langage,
//! hash, symboles et références (tree-sitter). Produit un `ProjectIndex` EN
//! MÉMOIRE, que l'atlas persiste (`atlas::build`) — l'atlas est la seule forme
//! stockée (plus de `index.bin`).

use crate::atlas::manifest::{SkippedPath, Tracked};
use crate::lang::Lang;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Un fichier indexé.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    /// Chemin relatif au root du projet (séparateurs '/').
    pub path: String,
    pub lang: Lang,
    /// Hash blake3 du contenu (détection de changement).
    pub hash: String,
    pub size: u64,
    /// Dernière modification (MICROsecondes epoch) — contrôle de fraîcheur bon
    /// marché : un `readdir` suffit pour savoir qu'un fichier n'a PAS changé.
    pub mtime: u64,
    pub lines: u32,
    /// Symboles extraits (tree-sitter).
    pub symbols: Vec<crate::symbol::Symbol>,
    /// Références sortantes (imports/appels/db) — la matière du graphe de relations.
    pub refs: crate::symbol::FileRefs,
    /// Tokens du commentaire d'en-tête du fichier (rôle du module).
    pub header: Vec<String>,
    /// Rôle du fichier en une phrase (première phrase brute de l'en-tête ;
    /// markdown : `description` du frontmatter ou première ligne de texte).
    #[serde(default)]
    pub summary: String,
    /// Champ « corps » : termes racinés du contenu entier et leurs occurrences
    /// (`symbol::body_terms`), triés par terme.
    #[serde(default)]
    pub body: Vec<(String, u32)>,
}

/// Index complet d'un projet, en mémoire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectIndex {
    pub name: String,
    pub root: String,
    pub generated_at: u64,
    pub files: Vec<FileEntry>,
}

impl ProjectIndex {
    pub fn stats(&self) -> (usize, usize, u64) {
        let n_files = self.files.len();
        let n_symbols: usize = self.files.iter().map(|f| f.symbols.len()).sum();
        let total_lines: u64 = self.files.iter().map(|f| f.lines as u64).sum();
        (n_files, n_symbols, total_lines)
    }
}

/// Répertoires ignorés en plus du .gitignore.
pub const EXTRA_IGNORE_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    "build",
    ".git",
    ".next",
    ".vite",
    "graphify-out",
    ".cortex",
    "coverage",
    "__pycache__",
    ".venv",
    "venv",
    "Library",
    "Temp",
    "obj",
    "bin",
    "Logs",
    ".turbo",
    // Code désactivé (AstroQuest) : hors recherche, pour ne pas polluer les résultats.
    "_disabled",
];

/// Vrai si un chemin relatif traverse un répertoire toujours ignoré.
pub fn in_ignored_dir(rel: &str) -> bool {
    rel.split('/').any(|seg| EXTRA_IGNORE_DIRS.contains(&seg))
}

/// Chemin relatif (séparateurs '/') d'un chemin absolu sous `root`.
pub fn rel_path(p: &Path, root: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).to_string_lossy().replace('\\', "/")
}

/// Contenu brut d'un fichier + (mtime µs, taille), `None` si illisible. Une
/// seule ouverture : les métadonnées sont lues sur le descripteur ouvert
/// (mêmes valeurs qu'un `metadata` par chemin, sans seconde ouverture).
pub fn read_file(p: &Path) -> Option<(Vec<u8>, u64, u64)> {
    use std::io::Read as _;
    let mut f = std::fs::File::open(p).ok()?;
    let hint = f.metadata().map(|m| m.len() as usize).unwrap_or(0);
    let mut bytes = Vec::with_capacity(hint + 1);
    f.read_to_end(&mut bytes).ok()?;
    let mtime = f.metadata().ok().map(|m| mtime_micros(&m)).unwrap_or(0);
    let size = bytes.len() as u64;
    Some((bytes, mtime, size))
}

pub fn hash_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Extrait un `FileEntry` d'un contenu déjà lu (`None` si binaire).
pub fn process_bytes(rel: String, bytes: &[u8], mtime: u64, hash: Option<String>) -> Option<FileEntry> {
    let sample = &bytes[..bytes.len().min(8192)];
    if sample.contains(&0) {
        return None; // binaire
    }
    let hash = hash.unwrap_or_else(|| hash_bytes(bytes));
    let lines = bytes.iter().filter(|&&b| b == b'\n').count() as u32 + 1;
    let lang = Lang::from_path(&rel);
    let (symbols, refs, (header, summary), body) = if lang.has_parser() {
        match std::str::from_utf8(bytes) {
            Ok(src) => {
                let (mut symbols, mut refs) = crate::extract::extract_symbols_and_refs(lang, src);
                if lang == Lang::Cpp {
                    crate::cpp::expand_includes(&rel, &mut refs);
                }
                let header = crate::extract::extract_comments(lang, src, &mut symbols);
                // Le corps ne sert qu'à classer des symboles : inutile sans symbole.
                // Pas de corps pour le markdown : sa prose double les titres et
                // l'en-tête déjà indexés (mesuré neutre sur les deux bancs).
                let body = if symbols.is_empty() || lang == Lang::Markdown { Vec::new() } else { crate::symbol::body_terms(src) };
                (symbols, refs, header, body)
            }
            Err(_) => (Vec::new(), Default::default(), (Vec::new(), String::new()), Vec::new()),
        }
    } else {
        (Vec::new(), Default::default(), (Vec::new(), String::new()), Vec::new())
    };
    Some(FileEntry { path: rel, lang, hash, size: bytes.len() as u64, mtime, lines, symbols, refs, header, summary, body })
}

/// Mtime d'un `Metadata` en MICROsecondes epoch (0 si indisponible).
pub fn mtime_micros(meta: &std::fs::Metadata) -> u64 {
    meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_micros() as u64).unwrap_or(0)
}

/// Indexation COMPLÈTE : parcours parallèle, puis lecture, empreinte blake3 et
/// analyse tree-sitter de chaque fichier en parallèle (rayon), les plus gros
/// d'abord (un gros fichier traité en dernier allongerait la fin du lot). Rend
/// aussi ce que l'atlas mémorise en plus des fichiers indexés (`Tracked`) :
/// chemins indexables mais NON indexés (binaires) et état du parcours.
/// Le résultat est trié par chemin : identique quel que soit le nombre de threads.
pub fn build_index(name: &str, root: &Path) -> std::io::Result<(ProjectIndex, Tracked)> {
    let t0 = Instant::now();
    let w = crate::walk::walk(root, "");
    let mut jobs: Vec<&crate::walk::WalkFile> = w.files.iter().collect();
    jobs.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.rel.cmp(&b.rel)));
    let results: Vec<Result<FileEntry, SkippedPath>> = jobs
        .par_iter()
        .with_max_len(1)
        .filter_map(|f| {
            let (bytes, mtime, size) = read_file(&root.join(&f.rel))?;
            Some(process_bytes(f.rel.clone(), &bytes, mtime, None).ok_or(SkippedPath { path: f.rel.clone(), mtime, size }))
        })
        .collect();
    let mut files = Vec::with_capacity(results.len());
    let mut skipped = Vec::new();
    for r in results {
        match r {
            Ok(f) => files.push(f),
            Err(s) => skipped.push(s),
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    skipped.sort_by(|a, b| a.path.cmp(&b.path));
    let state = crate::walk::full_state(root, &w, files.iter().map(|f| f.path.as_str()).chain(skipped.iter().map(|s| s.path.as_str())));
    let idx = ProjectIndex { name: name.to_string(), root: normalize_root(root), generated_at: now_secs(), files };
    let (nf, _, nl) = idx.stats();
    eprintln!("[cortex] indexed {}: {} files, {} lines in {:.2}s", name, nf, nl, t0.elapsed().as_secs_f64());
    Ok((idx, Tracked { skipped, walk: Some(state) }))
}

/// Nettoie le préfixe UNC Windows (\\?\) du chemin canonicalisé.
pub fn normalize_root(root: &Path) -> String {
    let s = root.to_string_lossy().replace('\\', "/");
    s.trim_start_matches("//?/").to_string()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Dossier des données de Cortex : `CORTEX_HOME` si défini, sinon `~/.cortex`.
pub fn cortex_home() -> PathBuf {
    if let Some(d) = std::env::var_os("CORTEX_HOME").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    if let Some(h) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        PathBuf::from(h).join(".cortex")
    } else {
        PathBuf::from(".cortex")
    }
}
