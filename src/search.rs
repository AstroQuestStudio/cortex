//! Briques communes de la recherche : termes de requête, poids BM25F, pénalité
//! des tests (la sortie pour agents est `outils::lecture::find`). Le moteur lui-même est `atlas::query`
//! (BM25F piloté par l'index inversé de l'atlas) ; l'ancien moteur linéaire v1
//! a été retiré (chiffres de référence figés dans `docs/ARCHITECTURE.md`).

use crate::symbol::{fold_accents, is_stopword, tokenize_identifier, SymbolKind};

/// Poids appliqué aux termes ajoutés par expansion de synonymes (vs termes
/// saisis par l'utilisateur, poids 1.0). Un synonyme compte, mais moins.
pub(crate) const SYNONYM_WEIGHT: f32 = 0.55;

/// Un résultat de recherche (un symbole pertinent).
pub struct Hit {
    pub score: f32,
    /// Nom du symbole (lu par les tests d'équivalence ; les sorties passent
    /// par l'identifiant stable `g`).
    #[cfg_attr(not(test), allow(dead_code))]
    pub name: String,
    pub kind: SymbolKind,
    pub file: String,
    pub line: u32,
    pub project: String,
    /// Id global du symbole dans l'atlas de `project` (identifiant stable :
    /// `Handle::node_id`).
    pub g: u32,
}

/// Paramètres BM25 standard. `pub(crate)` : partagés avec `atlas::query`.
pub(crate) const K1: f32 = 1.4;
pub(crate) const B: f32 = 0.75;

/// Vrai si le chemin désigne un fichier de test (`*.test.*`, `*.spec.*`, `*_test.*`,
/// `tests.rs`, `test_*.py`, `__tests__/`, dossier `tests/` ou `test/`).
pub fn is_test_path(path: &str) -> bool {
    // Chemin rapide sans allocation : tous les motifs contiennent « test » ou « spec ».
    let has = |needle: &[u8]| path.as_bytes().windows(needle.len()).any(|w| w.eq_ignore_ascii_case(needle));
    if !has(b"test") && !has(b"spec") {
        return false;
    }
    let p = path.to_ascii_lowercase().replace('\\', "/");
    let name = p.rsplit('/').next().unwrap_or(&p);
    name.contains(".test.")
        || name.contains(".spec.")
        || name.contains("_test.")
        || name == "tests.rs"
        || (name.starts_with("test_") && name.ends_with(".py"))
        || p.contains("__tests__/")
        || p.starts_with("tests/")
        || p.starts_with("test/")
        || p.contains("/tests/")
        || p.contains("/test/")
}

/// Mots qui signalent qu'on CHERCHE des tests : dans ce cas, pas de pénalité.
pub(crate) const TEST_WORDS: &[&str] =
    &["test", "tests", "spec", "specs", "testing", "vitest", "jest", "mock", "mocks", "banc", "fixture", "fixtures", "e2e", "playwright"];

/// Poids de scoring. Valeurs par défaut réglées sur le banc privé `$CORTEX_BENCH_DIR/queries.json`
/// (voir `cortex bench`) et vérifiées sur le banc caché `holdout.json` ;
/// surchargeables par variables d'environnement pour re-régler sans recompiler :
/// CORTEX_W_HEADER, CORTEX_W_DOC, CORTEX_TEST_PENALTY, CORTEX_W_BODY.
pub(crate) struct Weights {
    /// Poids du score BM25 de l'en-tête de fichier (ajouté au meilleur symbole du fichier).
    pub(crate) header: f32,
    /// Poids du score BM25 du doc-comment d'un symbole (ajouté à ce symbole).
    pub(crate) doc: f32,
    /// Multiplicateur appliqué aux fichiers de test quand la requête ne parle pas de test.
    pub(crate) test_penalty: f32,
    /// Poids du score BM25 du CORPS d'un fichier (ajouté au meilleur symbole du fichier).
    pub(crate) body: f32,
    /// Poids de la couverture des termes de la question par l'identité d'un fichier.
    pub(crate) cov_id: f32,
}

impl Weights {
    pub(crate) fn load() -> Self {
        let env = |k: &str, d: f32| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
        Weights {
            header: env("CORTEX_W_HEADER", 1.0),
            doc: env("CORTEX_W_DOC", 0.4),
            test_penalty: env("CORTEX_TEST_PENALTY", 0.8),
            body: env("CORTEX_W_BODY", 0.5),
            cov_id: env("CORTEX_COV_ID", 2.0),
        }
    }
}

/// Mots vides propres aux QUESTIONS (possessifs, prépositions de temps) : sans effet sur
/// l'indexation, donc sans besoin de réindexer.
const QUERY_STOPWORDS: &[&str] = &[
    "my",
    "mine",
    "your",
    "his",
    "her",
    "their",
    "own",
    "after",
    "before",
    "each",
    "every",
    "any",
    "some",
    "than",
    "then",
    "there",
    "mon",
    "ma",
    "mes",
    "ton",
    "ta",
    "tes",
    "notre",
    "nos",
    "votre",
    "vos",
    "apres",
    "avant",
    "depuis",
    "vers",
    "chez",
    "chaque",
    "entre",
    "seulement",
    "uniquement",
];

/// Décompose la question en tokens de recherche (même transform que les symboles),
/// accents repliés et mots vides retirés (sauf si la question n'est faite que de ça).
pub fn query_terms(question: &str) -> Vec<String> {
    let folded = fold_accents(question);
    let mut terms: Vec<String> = Vec::new();
    for raw in folded.split(|c: char| !c.is_alphanumeric()) {
        if raw.is_empty() {
            continue;
        }
        for t in tokenize_identifier(raw) {
            if !terms.contains(&t) {
                terms.push(t);
            }
        }
    }
    let content: Vec<String> = terms.iter().filter(|t| !is_stopword(t) && !QUERY_STOPWORDS.contains(&t.as_str())).cloned().collect();
    if content.is_empty() {
        terms
    } else {
        content
    }
}
