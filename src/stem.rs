//! Racinisation LÉGÈRE français + anglais, maison (aucune dépendance).
//!
//! Appliquée à l'identique à l'INDEXATION (termes des champs noms,
//! commentaires et corps de l'atlas) et à la REQUÊTE : c'est la seule façon
//! que « virtualization » retrouve `Virtualizer`, « chargement » `charger`, ou
//! « security » `securite`. Un seul jeu de règles pour les deux langues, parce
//! que la langue d'un token de code n'est pas connue (identifiants anglais,
//! commentaires français).
//!
//! Volontairement conservatrice : un seul suffixe retiré, un radical d'au moins
//! 4 lettres (5 pour les suffixes courts ambigus comme `-er`, `-age`), et rien
//! pour un token qui n'est pas fait de lettres ASCII minuscules (les accents
//! sont déjà repliés en amont). Mieux vaut rater une variante que confondre
//! deux mots.

/// Suffixes retirés (une seule fois, le plus long d'abord), avec la longueur
/// minimale du radical restant.
const SUFFIXES: &[(&str, usize)] = &[
    ("ization", 4),
    ("isation", 4),
    ("ement", 4),
    ("ation", 4),
    ("izing", 4),
    ("ique", 4),
    ("able", 4),
    ("ible", 4),
    ("izer", 4),
    ("iser", 4),
    ("ment", 4),
    ("euse", 4),
    ("ized", 4),
    ("ing", 4),
    ("ion", 4),
    ("ize", 4),
    ("eur", 4),
    ("ite", 4),
    ("ity", 4),
    ("age", 5),
    ("er", 5),
    ("ed", 4),
    ("ee", 4),
    ("ic", 4),
];

/// Radical d'un token (minuscules, accents repliés). Pas forcément idempotent,
/// et ce n'est pas requis : on ne racinise jamais un terme déjà raciné (l'atlas
/// garde les tokens bruts et racinise à l'écriture des postings ; le corps est
/// raciné une fois, à l'extraction).
pub fn stem(t: &str) -> String {
    let b = t.as_bytes();
    if b.len() < 5 || !b.iter().all(|c| c.is_ascii_lowercase()) {
        return t.to_string();
    }
    // 1. Pluriels.
    let plural: String = if t.ends_with("ies") {
        format!("{}y", &t[..t.len() - 3])
    } else if t.ends_with("sses") || t.ends_with("xes") || t.ends_with("ches") || t.ends_with("shes") {
        t[..t.len() - 2].to_string()
    } else if t.ends_with("aux") && t.len() >= 6 {
        format!("{}al", &t[..t.len() - 3])
    } else if t.ends_with('s') && !t.ends_with("ss") && !t.ends_with("us") && !t.ends_with("is") {
        t[..t.len() - 1].to_string()
    } else {
        t.to_string()
    };
    let mut w: &str = &plural;
    // 2. Un suffixe dérivationnel ou flexionnel.
    let mut stripped = "";
    for &(suf, min) in SUFFIXES {
        if w.len() >= suf.len() + min && w.ends_with(suf) {
            w = &w[..w.len() - suf.len()];
            stripped = suf;
            break;
        }
    }
    // 3. Consonne doublée après -ing/-ed/-er (embedded → embed, wrapper → wrap).
    if matches!(stripped, "ing" | "ed" | "er") {
        let wb = w.as_bytes();
        let n = wb.len();
        if n >= 4 && wb[n - 1] == wb[n - 2] && !b"aeioulsz".contains(&wb[n - 1]) {
            w = &w[..n - 1];
        }
    }
    // 4. « e » final muet (cache → cach, sauvegarde → sauvegard).
    if w.len() >= 5 && w.ends_with('e') {
        w = &w[..w.len() - 1];
    }
    w.to_string()
}

#[cfg(test)]
mod tests {
    use super::stem;

    fn same(a: &str, b: &str) {
        assert_eq!(stem(a), stem(b), "{} / {} : {} ≠ {}", a, b, stem(a), stem(b));
    }

    #[test]
    fn variantes_anglaises() {
        same("virtualization", "virtualizer");
        same("virtualizer", "virtualize");
        same("loading", "load");
        same("caching", "cache");
        same("cached", "cache");
        same("entries", "entry");
        same("policies", "policy");
        same("renderer", "rendering");
        same("uploader", "upload");
        same("embedded", "embed");
        same("files", "file");
        same("boxes", "box");
        same("readable", "read");
        same("wrapper", "wrap");
    }

    #[test]
    fn variantes_francaises_et_croisees() {
        same("chargement", "charger");
        same("sauvegarde", "sauvegarder");
        same("deplacement", "deplacer");
        same("dessiner", "dessin");
        same("journaux", "journal");
        same("securite", "security");
        same("visibilite", "visibility");
        same("automatique", "automatic");
        same("graphique", "graphic");
        same("compression", "compresser");
        same("enregistree", "enregistrer");
        same("stockage", "stock");
    }

    #[test]
    fn prudence_sur_les_mots_courts_et_les_identifiants() {
        assert_eq!(stem("user"), "user");
        assert_eq!(stem("render"), "render");
        assert_eq!(stem("folder"), "folder");
        assert_eq!(stem("header"), "header");
        assert_eq!(stem("string"), "string");
        assert_eq!(stem("status"), "status");
        assert_eq!(stem("table"), "tabl");
        assert_eq!(stem("message"), "messag");
        assert_eq!(stem("write"), "writ");
        assert_eq!(stem("utf8"), "utf8");
        assert_eq!(stem("x2y"), "x2y");
        assert_ne!(stem("comment"), stem("com"));
    }
}
