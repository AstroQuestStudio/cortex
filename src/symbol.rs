//! Symboles extraits d'un fichier (fonctions, classes, méthodes, imports, headings…).
//! Étape 1 : structure définie, extraction réelle ajoutée Étape 2 (tree-sitter).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SymbolKind {
    Function,
    Method,
    Class,
    Interface,
    Struct,
    Enum,
    Type,
    Const,
    Import,
    Export,
    Heading,   // markdown
    Component, // React
    Hook,      // React use*
}

/// Un symbole = un "nœud" du graphe de contexte.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    /// Ligne de début (1-based).
    pub line: u32,
    /// Ligne de fin (1-based, inclusive) du nœud tree-sitter englobant. Sert à
    /// rattacher les appels faits DANS cette fonction/méthode/composant (granularité
    /// fonction plutôt que fichier entier) — voir `graph::RelGraph`. 0 = inconnue
    /// (ancien index, ou symbole sans portée comme un heading markdown) : dans ce
    /// cas on retombe sur la portée "tout le fichier".
    #[serde(default)]
    pub end_line: u32,
    /// Signature compacte (ex: "function useOfficeFile(): UseOfficeFileResult").
    #[serde(default)]
    pub signature: String,
    /// Tokens décomposés (camelCase/snake/PascalCase) — pour la recherche.
    #[serde(default)]
    pub tokens: Vec<String>,
    /// Tokens du doc-comment (JSDoc, ///, #, docstring) placé au-dessus du symbole.
    /// Sert de texte d'appoint au score (poids modéré, voir search.rs).
    #[serde(default)]
    pub doc: Vec<String>,
    /// Rôle en une phrase : première phrase BRUTE du doc-comment (titre de
    /// section pour un heading markdown : première ligne de son texte). Sert
    /// aux cartes I7 et aux sorties des outils, jamais au score.
    #[serde(default)]
    pub summary: String,
}

/// Plafond de tokens gardés par bloc de commentaire (en-tête ou doc-comment) :
/// l'essentiel du "pourquoi" est dans les premières lignes, et un plafond borne
/// le coût du scoring comme la taille de l'index.
pub const MAX_COMMENT_TOKENS: usize = 250;

/// Mots vides FR/EN (déjà sans accents) : ignorés dans les commentaires ET dans
/// les requêtes, sinon "du", "the", "pour"… pèsent sur le score sans rien dire.
const STOPWORDS: &[&str] = &[
    "le", "la", "les", "de", "des", "du", "un", "une", "et", "ou", "en", "au", "aux", "ce", "ces", "cet", "cette", "qui", "que", "quoi",
    "dont", "est", "sont", "pas", "ne", "se", "sa", "son", "ses", "sur", "par", "pour", "dans", "avec", "sans", "plus", "moins", "tout",
    "tous", "toute", "on", "il", "elle", "ils", "nous", "vous", "je", "tu", "leur", "leurs", "mais", "donc", "car", "si", "ni", "comme",
    "quand", "ici", "the", "of", "to", "in", "is", "it", "for", "on", "and", "or", "an", "be", "by", "at", "as", "with", "this", "that",
    "from", "are", "was", "not", "into", "its", "we", "our", "you", "when", "how", "what", "which", "while", "où", "ou", "ete", "etre",
    "avoir", "fait", "faire", "deja", "tres", "aussi", "encore", "meme",
];

pub fn is_stopword(t: &str) -> bool {
    STOPWORDS.contains(&t)
}

/// Replie les accents latins courants vers l'ASCII ("écrire" → "ecrire").
/// Appliqué des deux côtés (commentaires indexés et requête) pour que la frappe
/// sans accents trouve le texte accentué et inversement.
pub fn fold_accents(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            'à' | 'â' | 'ä' | 'á' | 'ã' => out.push('a'),
            'À' | 'Â' | 'Ä' | 'Á' | 'Ã' => out.push('A'),
            'é' | 'è' | 'ê' | 'ë' => out.push('e'),
            'É' | 'È' | 'Ê' | 'Ë' => out.push('E'),
            'î' | 'ï' | 'í' => out.push('i'),
            'Î' | 'Ï' | 'Í' => out.push('I'),
            'ô' | 'ö' | 'ó' | 'õ' => out.push('o'),
            'Ô' | 'Ö' | 'Ó' | 'Õ' => out.push('O'),
            'ù' | 'û' | 'ü' | 'ú' => out.push('u'),
            'Ù' | 'Û' | 'Ü' | 'Ú' => out.push('U'),
            'ç' => out.push('c'),
            'Ç' => out.push('C'),
            'œ' => out.push_str("oe"),
            'Œ' => out.push_str("OE"),
            'æ' => out.push_str("ae"),
            'Æ' => out.push_str("AE"),
            'ñ' => out.push('n'),
            _ => out.push(c),
        }
    }
    out
}

/// Tokens de recherche d'un texte libre (commentaire) : accents repliés,
/// identifiants décomposés (camelCase dans la prose), mots vides et nombres retirés.
pub fn text_tokens(text: &str) -> Vec<String> {
    let folded = fold_accents(text);
    let mut out = Vec::new();
    for word in folded.split(|c: char| !c.is_alphanumeric()) {
        if word.len() < 2 || word.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        for t in tokenize_identifier(word) {
            if !is_stopword(&t) && !t.chars().all(|c| c.is_ascii_digit()) {
                out.push(t);
            }
        }
        if out.len() >= MAX_COMMENT_TOKENS {
            break;
        }
    }
    out.truncate(MAX_COMMENT_TOKENS);
    out
}

/// Longueur maximale d'un rôle (`summary_of`), en caractères.
pub const MAX_SUMMARY_CHARS: usize = 160;

/// Rôle en une phrase d'un commentaire brut : espaces normalisés, balises
/// JSDoc (`@param`…) et directives (`eslint-…`, `@ts-…`) retirées, première
/// phrase seulement, coupée à `MAX_SUMMARY_CHARS` sur une frontière de mot.
pub fn summary_of(text: &str) -> String {
    let mut words: Vec<&str> = Vec::new();
    for w in text.split_whitespace() {
        // Une balise JSDoc termine la prose du commentaire.
        if w.starts_with('@') && w.len() > 1 && w[1..].chars().next().is_some_and(|c| c.is_ascii_lowercase()) {
            if words.is_empty() && (w.starts_with("@ts-") || w == "@vitest-environment" || w == "@jsx") {
                continue;
            }
            if words.is_empty() {
                continue;
            }
            break;
        }
        if words.is_empty()
            && (w.starts_with("eslint")
                || w.starts_with("prettier-")
                || w.starts_with("#region")
                || w.chars().all(|c| !c.is_alphanumeric()))
        {
            continue;
        }
        words.push(w);
    }
    let joined = words.join(" ");
    // Première phrase : jusqu'au premier « . » suivi d'un espace (ou « : » en fin).
    let mut end = joined.len();
    let b = joined.as_bytes();
    for i in 0..b.len() {
        if (b[i] == b'.' || b[i] == b'!' || b[i] == b'?') && (i + 1 == b.len() || b[i + 1] == b' ') {
            // « p. ex. », « etc. » au milieu d'une phrase : on accepte, rare.
            end = i + 1;
            break;
        }
    }
    let first = joined[..end].trim();
    if first.chars().count() <= MAX_SUMMARY_CHARS {
        return first.to_string();
    }
    let mut out = String::new();
    for w in first.split(' ') {
        if out.chars().count() + w.chars().count() + 1 > MAX_SUMMARY_CHARS - 1 {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
    }
    out.push('…');
    out
}

/// Longueur maximale d'un terme de corps (au-delà : base64, empreintes… du bruit
/// qui gonflerait le vocabulaire sans jamais être cherché).
const MAX_BODY_TERM: usize = 40;

/// Champ « corps » d'un fichier : tout son texte (code, commentaires, chaînes)
/// en sac de termes — découpe sur non-alphanumérique puis frontières camelCase
/// et lettre↔chiffre (comme le BM25 de référence du banc comparatif), accents
/// repliés, minuscules, 2 à 40 caractères, mots vides et nombres retirés, puis
/// racinés (`stem`). Rend (terme, occurrences) trié par terme : ordre
/// déterministe, identique à l'extraction et à la matérialisation.
pub fn body_terms(text: &str) -> Vec<(String, u32)> {
    // Chemin rapide : un texte ASCII n'a pas d'accent à replier (pas de copie).
    let folded: std::borrow::Cow<str> =
        if text.is_ascii() { std::borrow::Cow::Borrowed(text) } else { std::borrow::Cow::Owned(fold_accents(text)) };
    // 1. Occurrences des tokens BRUTS (minuscules) — la racinisation et les
    //    mots vides ne sont ensuite appliqués qu'une fois par token distinct.
    let mut raw: crate::fx::FxHashMap<String, u32> = crate::fx::FxHashMap::default();
    let mut cur = String::new();
    let mut flush = |cur: &mut String| {
        let n = cur.len();
        if (2..=MAX_BODY_TERM).contains(&n) {
            if cur.is_ascii() {
                cur.make_ascii_lowercase();
            } else {
                *cur = cur.to_lowercase();
            }
            match raw.get_mut(cur.as_str()) {
                Some(c) => *c = c.saturating_add(1),
                None => {
                    raw.insert(cur.clone(), 1);
                }
            }
        }
        cur.clear();
    };
    for word in folded.split(|c: char| !c.is_alphanumeric()) {
        if word.is_empty() {
            continue;
        }
        let mut prev: Option<char> = None;
        let mut it = word.chars().peekable();
        while let Some(c) = it.next() {
            if let Some(p) = prev {
                let next_lower = it.peek().map(|n| n.is_lowercase()).unwrap_or(false);
                let camel = c.is_uppercase() && (p.is_lowercase() || (p.is_uppercase() && next_lower));
                let digit_edge = c.is_ascii_digit() != p.is_ascii_digit();
                if camel || digit_edge {
                    flush(&mut cur);
                }
            }
            cur.push(c);
            prev = Some(c);
        }
        flush(&mut cur);
    }
    // 2. Mots vides et nombres retirés, racinisation, fusion des radicaux égaux.
    let mut out: Vec<(String, u32)> = raw
        .into_iter()
        .filter(|(t, _)| !t.bytes().all(|c| c.is_ascii_digit()) && !is_stopword(t))
        .map(|(t, n)| (crate::stem::stem(&t), n))
        .collect();
    out.sort_unstable_by(|x, y| x.0.cmp(&y.0));
    let mut merged: Vec<(String, u32)> = Vec::with_capacity(out.len());
    for (t, n) in out {
        match merged.last_mut() {
            Some(last) if last.0 == t => last.1 = last.1.saturating_add(n),
            _ => merged.push((t, n)),
        }
    }
    for e in merged.iter_mut() {
        e.1 = e.1.min(u16::MAX as u32);
    }
    merged
}

/// Références SORTANTES d'un fichier — la matière du graphe de relations.
/// Permet de lier les fichiers/symboles entre eux (qui importe/appelle/utilise quoi).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileRefs {
    /// Spécificateurs d'import (ex: "@/hooks/office/useOfficeFile", "./foo", "react").
    #[serde(default)]
    pub imports: Vec<String>,
    /// Identifiants importés nommés (ex: "useOfficeFile", "OfficeEditor") — pour le lien symbole↔symbole.
    #[serde(default)]
    pub imported_names: Vec<String>,
    /// Identifiants appelés / référencés dans le fichier (call expressions, JSX),
    /// avec leur ligne — pour rattacher chaque appel à SA fonction englobante
    /// (granularité fonction) via `Symbol.line..end_line`. Dédupliqués (nom, ligne).
    #[serde(default)]
    pub calls: Vec<CallRef>,
    /// Tables/RPC Supabase référencées (.from('x') / .rpc('y')) — lien code↔DB.
    #[serde(default)]
    pub db_refs: Vec<String>,
}

/// Un appel/référence à un identifiant, avec sa ligne dans le fichier — la brique
/// qui permet de savoir DANS QUELLE fonction il a été fait (voir `graph.rs`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CallRef {
    pub name: String,
    pub line: u32,
}

impl SymbolKind {
    /// Encodage compact pour l'atlas (rkyv) — stable tant que l'ordre du match
    /// `from_u8` ci-dessous n'est pas changé (un segment déjà écrit y renvoie).
    pub fn as_u8(&self) -> u8 {
        match self {
            SymbolKind::Function => 0,
            SymbolKind::Method => 1,
            SymbolKind::Class => 2,
            SymbolKind::Interface => 3,
            SymbolKind::Struct => 4,
            SymbolKind::Enum => 5,
            SymbolKind::Type => 6,
            SymbolKind::Const => 7,
            SymbolKind::Import => 8,
            SymbolKind::Export => 9,
            SymbolKind::Heading => 10,
            SymbolKind::Component => 11,
            SymbolKind::Hook => 12,
        }
    }

    pub fn from_u8(b: u8) -> SymbolKind {
        match b {
            1 => SymbolKind::Method,
            2 => SymbolKind::Class,
            3 => SymbolKind::Interface,
            4 => SymbolKind::Struct,
            5 => SymbolKind::Enum,
            6 => SymbolKind::Type,
            7 => SymbolKind::Const,
            8 => SymbolKind::Import,
            9 => SymbolKind::Export,
            10 => SymbolKind::Heading,
            11 => SymbolKind::Component,
            12 => SymbolKind::Hook,
            _ => SymbolKind::Function,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            SymbolKind::Function => "fn",
            SymbolKind::Method => "method",
            SymbolKind::Class => "class",
            SymbolKind::Interface => "interface",
            SymbolKind::Struct => "struct",
            SymbolKind::Enum => "enum",
            SymbolKind::Type => "type",
            SymbolKind::Const => "const",
            SymbolKind::Import => "import",
            SymbolKind::Export => "export",
            SymbolKind::Heading => "heading",
            SymbolKind::Component => "component",
            SymbolKind::Hook => "hook",
        }
    }
}

/// Décompose un identifiant en tokens : camelCase, PascalCase, snake_case, kebab-case.
/// "useOfficeFile" -> ["use","office","file","useofficefile"]
/// C'est LA clé du boost recherche (corrige le bug graphify "identifiants non trouvés").
pub fn tokenize_identifier(ident: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let full = ident.to_ascii_lowercase();

    // Découpe sur séparateurs explicites (_ - . / espace) puis sur les frontières de casse.
    let mut current = String::new();
    let mut prev_lower = false;
    let chars: Vec<char> = ident.chars().collect();
    for (i, &ch) in chars.iter().enumerate() {
        if ch == '_' || ch == '-' || ch == '.' || ch == '/' || ch == ' ' {
            if !current.is_empty() {
                tokens.push(current.to_ascii_lowercase());
                current.clear();
            }
            prev_lower = false;
            continue;
        }
        // Frontière camelCase : minuscule→Majuscule, OU fin d'acronyme (XMLParser -> xml, parser)
        let is_upper = ch.is_uppercase();
        let next_lower = chars.get(i + 1).map(|c| c.is_lowercase()).unwrap_or(false);
        if is_upper && (prev_lower || (next_lower && !current.is_empty())) && !current.is_empty() {
            tokens.push(current.to_ascii_lowercase());
            current.clear();
        }
        current.push(ch);
        prev_lower = ch.is_lowercase() || ch.is_ascii_digit();
    }
    if !current.is_empty() {
        tokens.push(current.to_ascii_lowercase());
    }

    // Ajoute l'identifiant complet en minuscule (pour le match exact).
    if !full.is_empty() && !tokens.contains(&full) {
        tokens.push(full);
    }
    // Dédup en gardant l'ordre.
    let mut seen = std::collections::HashSet::new();
    tokens.retain(|t| t.len() >= 2 && seen.insert(t.clone()));
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camel_case() {
        let t = tokenize_identifier("useOfficeFile");
        assert!(t.contains(&"use".to_string()));
        assert!(t.contains(&"office".to_string()));
        assert!(t.contains(&"file".to_string()));
        assert!(t.contains(&"useofficefile".to_string()));
    }

    #[test]
    fn snake_and_pascal() {
        let t = tokenize_identifier("reserve_event_seats");
        assert!(t.contains(&"reserve".to_string()));
        assert!(t.contains(&"event".to_string()));
        assert!(t.contains(&"seats".to_string()));

        let p = tokenize_identifier("HyperOSDesktop");
        assert!(p.contains(&"hyper".to_string()));
        assert!(p.contains(&"desktop".to_string()));
    }

    #[test]
    fn body_terms_sac_racine_trie() {
        let b = body_terms("// Réessayer le chargement\nconst uploadQueue = retryUploads(files, 42); // the files");
        let terms: Vec<&str> = b.iter().map(|(t, _)| t.as_str()).collect();
        let mut sorted = terms.clone();
        sorted.sort();
        assert_eq!(terms, sorted, "trié par terme");
        let tf = |t: &str| b.iter().find(|(x, _)| x == t).map(|x| x.1);
        assert_eq!(tf("upload"), Some(2), "camelCase découpé, pluriel raciné et fusionné");
        assert_eq!(tf("file"), Some(2));
        assert_eq!(tf("charg"), Some(1), "accents repliés puis radical");
        assert_eq!(tf("reessay"), Some(1));
        assert!(tf("the").is_none() && tf("le").is_none(), "mots vides retirés");
        assert!(tf("42").is_none(), "nombres retirés");
        assert!(tf("uploadqueue").is_none(), "pas d'identifiant complet dans le corps");
    }

    #[test]
    fn acronym() {
        let t = tokenize_identifier("XMLHttpRequest");
        assert!(t.contains(&"http".to_string()));
        assert!(t.contains(&"request".to_string()));
    }

    #[test]
    fn summary_premiere_phrase() {
        assert_eq!(summary_of("  Compacte le board en une baseline. Puis écrit  le delta."), "Compacte le board en une baseline.");
        assert_eq!(summary_of("eslint-disable-next-line Rend la clé. @param x la valeur"), "Rend la clé.");
        assert_eq!(summary_of("Charge le tableau @param docId identifiant"), "Charge le tableau");
        assert_eq!(summary_of("v1.2 du format"), "v1.2 du format");
        let long = "mot ".repeat(80);
        let s = summary_of(&long);
        assert!(s.ends_with('…') && s.chars().count() <= MAX_SUMMARY_CHARS);
    }
}
