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
/// CORTEX_W_HEADER, CORTEX_W_DOC, CORTEX_TEST_PENALTY, CORTEX_W_BODY, CORTEX_COV_ID, CORTEX_IDX_DIR,
/// CORTEX_BODY_RR, CORTEX_BODY_N.
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
    /// Poids ajouté aux termes qui nomment le DOSSIER d'un `index.*` / `mod.rs` / `__init__.py`.
    pub(crate) idx_dir: f32,
    /// Bonus d'accord corps/identité (0 = désactivé) et nombre de fichiers du classement par corps concernés.
    pub(crate) body_rr: f32,
    pub(crate) body_n: usize,
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
            idx_dir: env("CORTEX_IDX_DIR", 0.7),
            body_rr: env("CORTEX_BODY_RR", 0.1),
            body_n: env("CORTEX_BODY_N", 100.0) as usize,
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

/// Échafaudage d'une question française : verbes auxiliaires et modaux conjugués, temps, lieu.
const QUERY_STOPWORDS_FR: &[&str] = &[
    "ont",
    "etait",
    "etaient",
    "sera",
    "seront",
    "serait",
    "soit",
    "doit",
    "doivent",
    "sait",
    "savoir",
    "va",
    "vont",
    "fois",
    "cote",
    "moment",
    "partir",
    "lors",
    "grace",
    "selon",
    "durant",
    "parmi",
    "autour",
    "dessus",
    "dessous",
    "contre",
    "sous",
    "tant",
    "bien",
    "ensuite",
    "toujours",
    "jamais",
    "rien",
    "chacun",
    "chacune",
    "lequel",
    "laquelle",
    "lesquels",
    "lesquelles",
    "auquel",
    "duquel",
    "celles",
    "autant",
    "peu",
    "beaucoup",
    "trop",
    "assez",
    "parfois",
    "souvent",
    "deux",
    "trois",
];

/// Verbes de LOCALISATION d'une question (« où est défini / géré / situé / implémenté X ») : ils
/// disent où chercher, pas quoi chercher.
const QUERY_VERBES_LIEU: &[&str] = &[
    "defini",
    "definis",
    "definie",
    "definies",
    "gere",
    "geree",
    "geres",
    "gerees",
    "situe",
    "situee",
    "situes",
    "situees",
    "implemente",
    "implementee",
    "implementes",
    "implementees",
    "realise",
    "realisee",
    "realises",
    "realisees",
];

/// Marqueurs d'une question FRANÇAISE (mots grammaticaux, accents repliés). Une question sans
/// aucun d'eux ni lettre accentuée est traitée comme anglaise ou technique : aucune des
/// normalisations françaises (locutions, formes de base) ne lui est appliquée.
const MARQUEURS_FR: &[&str] = &[
    "le", "la", "les", "des", "du", "une", "est", "sont", "dans", "pour", "avec", "comment", "quelle", "quel", "quels", "quelles",
    "pourquoi", "quand", "que", "qui", "sur", "par", "au", "aux", "ses", "ces", "cette", "leur", "leurs", "nous", "peut", "lors", "ou",
    "ete", "sans", "chaque", "apres", "avant", "entre", "vers",
];

/// Locutions françaises courantes du développement, ramenées à UN mot du code (écrites sans
/// accent, tirets compris). Remplacement sans tenir compte de la casse.
const LOCUTIONS: &[(&str, &str)] = &[
    ("en-tetes", "entetes"),
    ("en-tete", "entete"),
    ("mises a jour", "update"),
    ("mise a jour", "update"),
    ("mettre a jour", "update"),
    ("mettent a jour", "update"),
    ("met a jour", "update"),
    ("mis a jour", "update"),
    ("mots de passe", "password"),
    ("mot de passe", "password"),
    ("mots-cles", "keyword"),
    ("mot-cle", "keyword"),
    ("mots cles", "keyword"),
    ("mot cle", "keyword"),
    ("base de donnees", "database"),
    ("file d'attente", "queue"),
    ("files d'attente", "queue"),
    ("ligne de commande", "cli"),
    ("lignes de commande", "cli"),
    ("expressions regulieres", "regex"),
    ("expression reguliere", "regex"),
    ("variables d'environnement", "env"),
    ("variable d'environnement", "env"),
    ("temps reel", "realtime"),
    ("par defaut", "default"),
    ("nom d'utilisateur", "username"),
    ("mise en cache", "cache"),
    ("mettre en cache", "cache"),
    ("mis en cache", "cache"),
    ("hors ligne", "offline"),
    ("arriere-plan", "background"),
    ("arriere plan", "background"),
    ("premier plan", "foreground"),
];

/// Vrai si la question est rédigée en français (voir `MARQUEURS_FR`).
/// Traitements des questions FRANÇAISES, désactivables pour comparer sans recompiler :
/// `CORTEX_FR` absent = `lpstnw` ; une liste de lettres sinon (`CORTEX_FR=` vide = rien).
/// `l` formes de base (conjugaison, pluriel), `p` locutions (« en-tête », « mise à jour »),
/// `s` mots vides français de question, `t` table français → code (`semantic::FR_VERS_CODE`),
/// `n` négation (« sans X » : X n'est pas cherché), `w` verbes de localisation (« où est défini »).
pub(crate) fn fr_on(c: char) -> bool {
    static F: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    F.get_or_init(|| std::env::var("CORTEX_FR").unwrap_or_else(|_| "lpstnw".to_string())).contains(c)
}

pub(crate) fn est_francais(question: &str) -> bool {
    if question.chars().any(|c| c.is_alphabetic() && !c.is_ascii()) {
        return true;
    }
    question.split(|c: char| !c.is_alphanumeric()).any(|w| !w.is_empty() && MARQUEURS_FR.contains(&w.to_ascii_lowercase().as_str()))
}

/// Remplace les locutions de `LOCUTIONS` (texte aux accents déjà repliés), casse ignorée,
/// mots entiers seulement.
fn normaliser_locutions(texte: &str) -> String {
    let mut cur = texte.to_string();
    for &(loc, rempl) in LOCUTIONS {
        let mut depuis = 0;
        loop {
            let bas = cur.to_ascii_lowercase();
            let Some(rel) = bas[depuis..].find(loc) else { break };
            let i = depuis + rel;
            let apres = i + loc.len();
            let avant_ok = i == 0 || !bas.as_bytes()[i - 1].is_ascii_alphanumeric();
            let apres_ok = apres >= bas.len() || !bas.as_bytes()[apres].is_ascii_alphanumeric();
            if avant_ok && apres_ok {
                cur.replace_range(i..apres, rempl);
                depuis = i + rempl.len();
            } else {
                depuis = i + 1;
            }
        }
    }
    cur
}

/// Décompose la question en tokens de recherche (même transform que les symboles),
/// accents repliés et mots vides retirés (sauf si la question n'est faite que de ça).
pub fn query_terms(question: &str) -> Vec<String> {
    let mut folded = fold_accents(question);
    let francais = est_francais(question);
    if francais && fr_on('p') {
        folded = normaliser_locutions(&folded.replace('\u{2019}', "'"));
    }
    let mut terms: Vec<String> = Vec::new();
    // « sans X » : X est ce que le code NE fait PAS ; le terme qui suit « sans » n'est pas cherché.
    let mut apres_sans = false;
    for raw in folded.split(|c: char| !c.is_alphanumeric()) {
        if raw.is_empty() {
            continue;
        }
        if francais && fr_on('n') {
            if raw.eq_ignore_ascii_case("sans") {
                apres_sans = true;
                continue;
            }
            if apres_sans {
                if is_stopword(&raw.to_ascii_lowercase()) || raw.len() == 1 {
                    continue;
                }
                apres_sans = false;
                continue;
            }
        }
        for t in tokenize_identifier(raw) {
            if !terms.contains(&t) {
                terms.push(t);
            }
        }
    }
    // Une lettre seule est presque toujours une élision (« l'utilisateur », « qu'on », « n'est »).
    let content: Vec<String> = terms
        .iter()
        .filter(|t| {
            !is_stopword(t)
                && !QUERY_STOPWORDS.contains(&t.as_str())
                && !(fr_on('s') && QUERY_STOPWORDS_FR.contains(&t.as_str()))
                && !(fr_on('w') && QUERY_VERBES_LIEU.contains(&t.as_str()))
                && !crate::generalisation::is_filler(t)
        })
        .filter(|t| !(t.len() == 1 && t.chars().all(|c| c.is_ascii_alphabetic())))
        .map(|t| if francais && fr_on('l') { crate::semantic::lemme_fr(t).unwrap_or_else(|| t.clone()) } else { t.clone() })
        .fold(Vec::<String>::new(), |mut acc, t| {
            if !acc.contains(&t) {
                acc.push(t);
            }
            acc
        });
    if content.is_empty() {
        terms
    } else {
        content
    }
}

#[cfg(test)]
mod tests_fr {
    use super::*;

    #[test]
    fn question_francaise_detectee() {
        assert!(est_francais("Où est vérifiée la signature ?"));
        assert!(est_francais("comment est gere le cache"));
        assert!(!est_francais("WalkParallel struct run WalkState"));
        assert!(!est_francais("Matcher trait definition find_at"));
    }

    #[test]
    fn locutions_normalisees() {
        assert_eq!(normaliser_locutions("les en-tetes de la reponse"), "les entetes de la reponse");
        assert_eq!(normaliser_locutions("la Mise a jour du mot de passe"), "la update du password");
        assert_eq!(normaliser_locutions("la file d'attente"), "la queue");
        // Pas de remplacement au milieu d'un mot.
        assert_eq!(normaliser_locutions("promis a jour"), "promis a jour");
    }

    #[test]
    fn formes_conjuguees_ramenees_a_la_base() {
        let t = query_terms("Quelles fonctions sont déclarées et comment les requêtes sont-elles transformées ?");
        assert!(t.contains(&"declarer".to_string()), "{t:?}");
        assert!(t.contains(&"requete".to_string()), "{t:?}");
        assert!(t.contains(&"transformer".to_string()), "{t:?}");
        // Mots vides conjugués filtrés.
        let t = query_terms("Où est refusé un jeton dont la date est dépassée ?");
        assert!(!t.contains(&"est".to_string()));
    }

    #[test]
    fn negation_et_verbes_de_lieu() {
        let t = query_terms("Où est défini le hook, sans enregistrer de cache global ?");
        assert!(!t.contains(&"defini".to_string()), "{t:?}");
        assert!(!t.contains(&"enregistrer".to_string()), "{t:?}");
        assert!(t.contains(&"hook".to_string()) && t.contains(&"cache".to_string()), "{t:?}");
    }

    #[test]
    fn question_anglaise_inchangee() {
        let t = query_terms("registered routes are declared");
        assert!(t.contains(&"declared".to_string()), "{t:?}");
        assert!(t.contains(&"routes".to_string()), "{t:?}");
    }
}
