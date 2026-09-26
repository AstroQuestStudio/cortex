//! Banc de pertinence : mesure objective du classement de `query`.
//!
//! Un fichier JSON liste des questions réelles et, pour chacune, le(s) fichier(s)
//! qui constituent la bonne réponse. Pour chaque question on lance la recherche
//! interne (exactement celle de `cortex query`), on réduit les hits (symboles) à
//! la liste ORDONNÉE des fichiers distincts, et on regarde le rang du premier
//! fichier attendu. On en tire top-1, top-5 et MRR.
//!
//! Sert de juge avant/après toute modification du scoring : un changement qui ne
//! fait pas progresser le banc n'est pas gardé.
//!
//! Format :
//! ```json
//! { "project": "MonProjet",
//!   "queries": [ { "q": "où sont signés les cookies", "expect": ["src/http/cookie.ts"], "tag": "fr" } ] }
//! ```
//!
//! Emplacement des bancs : `CORTEX_BENCH_DIR` (défaut `bench/`). Les bancs d'un
//! code fermé vivent hors du dépôt public et sont désignés par cette variable ;
//! le banc public reproductible est dans `bench/public/`.

use serde::Deserialize;

#[derive(Deserialize)]
pub struct BenchFile {
    #[serde(default)]
    pub project: Option<String>,
    pub queries: Vec<BenchQuery>,
}

#[derive(Deserialize)]
pub struct BenchQuery {
    pub q: String,
    pub expect: Vec<String>,
    #[serde(default)]
    pub tag: String,
}

/// Résultat d'une question : rang (1-based) du premier fichier attendu, None si absent.
pub struct QueryResult {
    pub rank: Option<usize>,
    pub top_files: Vec<String>,
}

/// Nombre de hits (symboles) demandés à la recherche : assez pour couvrir
/// largement le top-20 fichiers même quand un fichier monopolise plusieurs hits.
const HIT_LIMIT: usize = 400;
/// Au-delà de ce rang, la réponse est considérée comme non trouvée (RR = 0).
const RANK_CUTOFF: usize = 20;

/// Dossier des bancs : `CORTEX_BENCH_DIR`, sinon `bench/` (relatif au dossier courant).
pub fn bench_dir() -> std::path::PathBuf {
    match std::env::var_os("CORTEX_BENCH_DIR") {
        Some(d) if !d.is_empty() => std::path::PathBuf::from(d),
        _ => std::path::PathBuf::from("bench"),
    }
}

/// Fichier `nom` du dossier des bancs (ex. `queries.json`, `holdout.json`, `agent_tasks.json`).
pub fn bench_file(nom: &str) -> std::path::PathBuf {
    bench_dir().join(nom)
}

/// Dossier des résultats JSON des bancs : `CORTEX_BENCH_RESULTS`, sinon
/// `<dossier des bancs>/resultats`.
pub fn results_dir() -> std::path::PathBuf {
    match std::env::var_os("CORTEX_BENCH_RESULTS") {
        Some(d) if !d.is_empty() => std::path::PathBuf::from(d),
        _ => bench_dir().join("resultats"),
    }
}

pub fn parse(content: &str) -> Result<BenchFile, String> {
    serde_json::from_str(content).map_err(|e| e.to_string())
}

/// Classement des fichiers distincts pour une question, via l'atlas
/// (`atlas::query::search`). Fusionne les hits de chaque handle (un par projet actif).
pub fn ranked_files_atlas(handles: &[crate::atlas::Handle], question: &str) -> Vec<String> {
    let mut hits: Vec<crate::search::Hit> = Vec::new();
    for h in handles {
        hits.extend(h.search(question, HIT_LIMIT));
    }
    hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    let mut files: Vec<String> = Vec::new();
    for h in hits {
        if !files.contains(&h.file) {
            files.push(h.file);
            if files.len() >= RANK_CUTOFF {
                break;
            }
        }
    }
    files
}

fn rank_of(files: &[String], expect: &[String]) -> Option<usize> {
    let norm = |s: &str| s.replace('\\', "/").to_ascii_lowercase();
    let expected: Vec<String> = expect.iter().map(|e| norm(e)).collect();
    files.iter().position(|f| expected.contains(&norm(f))).map(|p| p + 1)
}

/// Rang du fichier attendu pour une question (moteur atlas).
pub fn run_one_atlas(handles: &[crate::atlas::Handle], q: &BenchQuery) -> QueryResult {
    let files = ranked_files_atlas(handles, &q.q);
    let rank = rank_of(&files, &q.expect);
    QueryResult { rank, top_files: files }
}

pub struct Summary {
    pub n: usize,
    pub top1: f64,
    pub top5: f64,
    pub mrr: f64,
}

pub fn summarize(results: &[QueryResult]) -> Summary {
    let n = results.len().max(1);
    let top1 = results.iter().filter(|r| r.rank == Some(1)).count() as f64 / n as f64;
    let top5 = results.iter().filter(|r| matches!(r.rank, Some(k) if k <= 5)).count() as f64 / n as f64;
    let mrr = results.iter().map(|r| r.rank.map(|k| 1.0 / k as f64).unwrap_or(0.0)).sum::<f64>() / n as f64;
    Summary { n: results.len(), top1, top5, mrr }
}
