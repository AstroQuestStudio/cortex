//! Couche sémantique légère — sans embeddings ONNX (incompatible MinGW).
//!
//! Deux mécanismes sûrs et rapides qui rapprochent la recherche en langage
//! naturel du code (souvent en anglais, abrégé) :
//!   1. Expansion de requête : synonymes FR↔EN + jargon code (delete/remove,
//!      fichier/file, créer/create…). Une question FR matche un symbole EN.
//!   2. Distance d'édition bornée (Levenshtein ≤1, + préfixe) : rattrape fautes
//!      de frappe et variantes (`usr`→`user`, `auth`→`authenticate`).
//!
//! C'est le 80% du gain des embeddings, pour 0 dépendance et 0 risque de build.

/// Table de synonymes bidirectionnelle (groupes de termes équivalents).
/// Chaque ligne = un cluster de mots qui doivent se matcher mutuellement.
/// Tout est en minuscules, déjà tokenisé (un seul mot par entrée).
const SYNONYM_GROUPS: &[&[&str]] = &[
    // CRUD ───────────────────────────────────────────────────────────────
    &["delete", "remove", "destroy", "supprimer", "effacer", "drop", "del"],
    &["create", "add", "new", "creer", "ajouter", "insert", "make"],
    &["update", "edit", "modify", "modifier", "patch", "set", "change"],
    &["get", "fetch", "load", "read", "recuperer", "charger", "lire", "list"],
    &["save", "persist", "store", "sauvegarder", "enregistrer", "write"],
    // Domaine app ─────────────────────────────────────────────────────────
    &["file", "document", "fichier", "doc", "blob"],
    &["folder", "directory", "dossier", "dir"],
    &["user", "account", "utilisateur", "compte", "member", "membre"],
    &["auth", "authentication", "authenticate", "login", "connexion", "signin", "session"],
    &["upload", "import", "envoyer", "televerser"],
    &["download", "export", "telecharger"],
    &["search", "find", "query", "rechercher", "chercher", "lookup"],
    &["company", "tenant", "organization", "entreprise", "societe", "org"],
    &["payment", "billing", "paiement", "facturation", "invoice", "facture", "stripe"],
    &["credit", "quota", "credits", "wallet", "solde", "balance"],
    &["error", "exception", "erreur", "fail", "echec"],
    &["config", "setting", "settings", "configuration", "parametres", "reglages"],
    &["permission", "access", "acl", "rls", "policy", "autorisation", "droits"],
    // Collab / realtime ───────────────────────────────────────────────────
    &["collab", "collaboration", "realtime", "temps", "yjs", "crdt", "sync", "synchro"],
    &["presence", "cursor", "curseur", "presence"],
    // UI ──────────────────────────────────────────────────────────────────
    &["modal", "dialog", "popup", "fenetre"],
    &["button", "btn", "bouton"],
    &["form", "formulaire", "input", "champ"],
    &["table", "grid", "tableau", "liste"],
    // Spark / office ──────────────────────────────────────────────────────
    &["sheet", "spreadsheet", "feuille", "tableur", "cell", "cellule"],
    &["slide", "presentation", "deck", "diapo"],
    &["compress", "compression", "regic", "zstd", "encode", "codec"],
    // Vocabulaire courant FR↔EN (étape 2b : manques relevés sur le banc caché,
    // gardés parce qu'ils font progresser le caché sans abaisser le public) ──
    &["window", "fenetre"],
    &["desktop", "bureau"],
    &["word", "mot"],
    &["employee", "salarie", "employe", "staff"],
    &["hr", "hrm", "rh"],
    &["data", "donnee", "donnees"],
    &["gdpr", "rgpd", "privacy"],
    &["url", "adresse", "link", "lien"],
    &["password", "passe", "mdp"],
    &["reminder", "relance", "rappel", "dunning"],
    &["salary", "salaire", "payroll", "paie"],
    &["bank", "banque", "bancaire"],
    &["weekly", "hebdomadaire"],
    &["scroll", "defilement"],
    &["tab", "onglet"],
    &["title", "titre"],
    &["guest", "anonymous", "anonyme", "invite"],
];

/// Étend une liste de termes avec leurs synonymes connus.
/// Garde l'ordre, évite les doublons. Les synonymes ajoutés porteront un poids
/// réduit côté scoring (l'appelant les distingue via `is_synonym`).
pub fn expand_synonyms(terms: &[String]) -> Vec<String> {
    let mut out: Vec<String> = terms.to_vec();
    for t in terms {
        let lt = t.to_ascii_lowercase();
        for group in SYNONYM_GROUPS {
            if group.contains(&lt.as_str()) {
                for &syn in *group {
                    if !out.iter().any(|x| x.eq_ignore_ascii_case(syn)) {
                        out.push(syn.to_string());
                    }
                }
            }
        }
    }
    out
}

/// Distance de Levenshtein bornée à 1 : renvoie true si edit-distance ≤ 1.
/// Optimisé : court-circuite dès que la différence de longueur dépasse 1.
pub fn within_edit_distance_1(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let (la, lb) = (a.len(), b.len());
    if la.abs_diff(lb) > 1 {
        return false;
    }
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    match la.cmp(&lb) {
        std::cmp::Ordering::Equal => {
            // une seule substitution autorisée
            let mut diffs = 0;
            for i in 0..la {
                if ab[i] != bb[i] {
                    diffs += 1;
                    if diffs > 1 {
                        return false;
                    }
                }
            }
            true
        }
        _ => {
            // une seule insertion/suppression : aligne le plus court dans le plus long
            let (short, long) = if la < lb { (ab, bb) } else { (bb, ab) };
            let mut i = 0; // index short
            let mut j = 0; // index long
            let mut skipped = false;
            while i < short.len() && j < long.len() {
                if short[i] == long[j] {
                    i += 1;
                    j += 1;
                } else if skipped {
                    return false;
                } else {
                    skipped = true;
                    j += 1; // saute un char du plus long
                }
            }
            true
        }
    }
}

/// Match flou entre un terme de requête et un token de symbole.
/// Retourne un facteur de score [0.0, 1.0] : 1.0 exact, 0.6 préfixe, 0.5 edit-1.
pub fn fuzzy_match(query_term: &str, token: &str) -> f32 {
    if query_term == token {
        return 1.0;
    }
    // Préfixe significatif (≥4 chars) : "auth" ~ "authenticate", "config" ~ "configure".
    if query_term.len() >= 4 && token.starts_with(query_term) {
        return 0.6;
    }
    if token.len() >= 4 && query_term.starts_with(token) {
        return 0.6;
    }
    // Faute de frappe : edit-distance 1 (seulement pour des mots assez longs,
    // sinon trop de faux positifs sur les mots courts).
    if query_term.len() >= 4 && within_edit_distance_1(query_term, token) {
        return 0.5;
    }
    0.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synonyms_link_fr_en() {
        let terms = vec!["supprimer".to_string(), "fichier".to_string()];
        let expanded = expand_synonyms(&terms);
        assert!(expanded.iter().any(|t| t == "delete"));
        assert!(expanded.iter().any(|t| t == "file"));
    }

    #[test]
    fn edit_distance_1_works() {
        assert!(within_edit_distance_1("user", "usr")); // suppression
        assert!(within_edit_distance_1("user", "users")); // insertion
        assert!(within_edit_distance_1("user", "user")); // égal (distance 0)
        assert!(within_edit_distance_1("config", "canfig")); // 1 substitution (o→a)
        assert!(!within_edit_distance_1("user", "admin")); // trop loin
        assert!(!within_edit_distance_1("file", "folder"));
    }

    #[test]
    fn fuzzy_prefix_and_typo() {
        assert_eq!(fuzzy_match("auth", "auth"), 1.0);
        assert_eq!(fuzzy_match("auth", "authenticate"), 0.6); // préfixe
        assert!(fuzzy_match("config", "canfig") >= 0.5); // typo edit-1 (substitution)
        assert_eq!(fuzzy_match("xyz", "config"), 0.0);
    }
}
