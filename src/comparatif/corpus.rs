//! Corpus commun du banc comparatif : EXACTEMENT les fichiers vivants de
//! l'atlas Cortex du projet (même liste, relue sur disque), plus les briques
//! partagées par les approches de référence : tokenizer lexical, mots de
//! requête, découpage en morceaux pour le RAG.

use crate::symbol::{fold_accents, is_stopword};
use rayon::prelude::*;

/// Longueur maximale d'une ligne telle qu'affichée à l'agent (rg, morceaux) :
/// au-delà, tronquée (équivalent `rg --max-columns 500`), pour qu'une ligne
/// minifiée ne fausse pas le compte de tokens.
pub const MAX_LINE_CHARS: usize = 500;

/// Morceaux du RAG : fenêtres de `CHUNK_LINES` lignes, avec `CHUNK_OVERLAP`
/// lignes de recouvrement (pas = 30 lignes). Choix classique (fenêtre fixe,
/// indépendante du langage) : ne réutilise PAS les plages de symboles de
/// Cortex, pour que la référence reste une référence.
pub const CHUNK_LINES: usize = 40;
pub const CHUNK_OVERLAP: usize = 10;

pub struct Doc {
    pub path: String,
    pub text: String,
    /// Contenu en minuscules (Unicode) : recherche rg insensible à la casse.
    pub lower: String,
    /// Début (octet) de chaque ligne de `text`.
    pub line_starts: Vec<usize>,
}

impl Doc {
    pub fn n_lines(&self) -> usize {
        self.line_starts.len()
    }
    /// Ligne `i` (0-based) sans fin de ligne.
    pub fn line(&self, i: usize) -> &str {
        let s = self.line_starts[i];
        let e = self.line_starts.get(i + 1).map(|&x| x - 1).unwrap_or(self.text.len());
        self.text[s..e.max(s)].trim_end_matches('\r')
    }
}

pub struct Corpus {
    pub docs: Vec<Doc>,
    pub bytes: usize,
}

/// Relit sur disque les fichiers listés (chemins relatifs à `root`), en
/// parallèle. Un fichier illisible est gardé vide (même liste que Cortex).
pub fn load(root: &str, paths: &[&str]) -> Corpus {
    let docs: Vec<Doc> = paths
        .par_iter()
        .map(|p| {
            let full = std::path::Path::new(root).join(p);
            let text = std::fs::read(&full).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
            let lower = text.to_lowercase();
            let mut line_starts = vec![0usize];
            line_starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
            if line_starts.len() > 1 && *line_starts.last().unwrap() == text.len() {
                line_starts.pop();
            }
            Doc { path: p.to_string(), text, lower, line_starts }
        })
        .collect();
    let bytes = docs.iter().map(|d| d.text.len()).sum();
    Corpus { docs, bytes }
}

/// Tronque une ligne pour l'affichage (en caractères).
pub fn clip(line: &str) -> &str {
    match line.char_indices().nth(MAX_LINE_CHARS) {
        Some((i, _)) => &line[..i],
        None => line,
    }
}

/// Tokenizer lexical des références BM25 : accents repliés, découpe sur tout
/// non-alphanumérique PUIS sur les frontières camelCase / lettre↔chiffre,
/// minuscules, tokens d'au moins 2 caractères. Pas de racinisation, pas de
/// synonymes (ceux-là sont propres à Cortex).
pub fn lex_tokens(text: &str, out: &mut Vec<String>) {
    let folded = fold_accents(text);
    for word in folded.split(|c: char| !c.is_alphanumeric()) {
        if word.is_empty() {
            continue;
        }
        let chars: Vec<char> = word.chars().collect();
        let mut cur = String::new();
        for (i, &c) in chars.iter().enumerate() {
            if i > 0 {
                let p = chars[i - 1];
                let next_lower = chars.get(i + 1).map(|n| n.is_lowercase()).unwrap_or(false);
                let camel = c.is_uppercase() && (p.is_lowercase() || (p.is_uppercase() && next_lower));
                let digit_edge = c.is_ascii_digit() != p.is_ascii_digit();
                if camel || digit_edge {
                    push_tok(&mut cur, out);
                }
            }
            cur.push(c);
        }
        push_tok(&mut cur, out);
    }
}

fn push_tok(cur: &mut String, out: &mut Vec<String>) {
    if cur.chars().count() >= 2 {
        out.push(cur.to_lowercase());
    }
    cur.clear();
}

/// Termes de requête BM25 : `lex_tokens` de la question, mots vides retirés
/// (liste de Cortex, commune à toutes les approches), sans doublon.
pub fn query_lex_terms(question: &str) -> Vec<String> {
    let mut toks = Vec::new();
    lex_tokens(question, &mut toks);
    let mut out: Vec<String> = Vec::new();
    for t in toks {
        if !is_stopword(&t) && !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

/// Mots que taperait un agent dans `rg -i` : mots de la question (découpe sur
/// non-alphanumérique, accents GARDÉS, minuscules), au moins 2 caractères,
/// mots vides retirés, sans doublon. Si tout est mot vide, on garde tout.
pub fn query_rg_words(question: &str) -> Vec<String> {
    let all: Vec<String> =
        question.split(|c: char| !c.is_alphanumeric()).filter(|w| w.chars().count() >= 2).map(|w| w.to_lowercase()).collect();
    let mut out: Vec<String> = Vec::new();
    for w in all.iter().filter(|w| !is_stopword(&fold_accents(w))) {
        if !out.contains(w) {
            out.push(w.clone());
        }
    }
    if out.is_empty() {
        for w in all {
            if !out.contains(&w) {
                out.push(w);
            }
        }
    }
    out
}

/// Un morceau : lignes [start, end) du document `doc`.
#[cfg_attr(not(feature = "bench"), allow(dead_code))]
#[derive(Clone, Copy)]
pub struct Chunk {
    pub doc: u32,
    pub start: u32,
    pub end: u32,
}

/// Fenêtres [s, s+CHUNK_LINES) avec un pas de CHUNK_LINES - CHUNK_OVERLAP ; la
/// dernière fenêtre s'arrête à la fin du fichier ; un fichier vide donne un
/// morceau vide (son chemin reste cherchable).
pub fn chunk_ranges(n_lines: usize) -> Vec<(usize, usize)> {
    let step = CHUNK_LINES - CHUNK_OVERLAP;
    let mut out = Vec::new();
    let mut s = 0usize;
    loop {
        let e = (s + CHUNK_LINES).min(n_lines);
        out.push((s, e));
        if e >= n_lines {
            break;
        }
        s += step;
    }
    out
}

#[cfg_attr(not(feature = "bench"), allow(dead_code))]
pub fn chunks(corpus: &Corpus) -> Vec<Chunk> {
    let mut out = Vec::new();
    for (di, d) in corpus.docs.iter().enumerate() {
        for (s, e) in chunk_ranges(d.n_lines()) {
            out.push(Chunk { doc: di as u32, start: s as u32, end: e as u32 });
        }
    }
    out
}

/// Texte d'un morceau tel qu'embarqué ET tel qu'affiché à l'agent : chemin et
/// plage de lignes en première ligne (métadonnée usuelle des RAG de code),
/// puis les lignes (tronquées à `MAX_LINE_CHARS`).
#[cfg_attr(not(feature = "bench"), allow(dead_code))]
pub fn chunk_text(corpus: &Corpus, c: &Chunk) -> String {
    let d = &corpus.docs[c.doc as usize];
    let mut s = format!("{}:{}-{}\n", d.path, c.start + 1, c.end);
    for i in c.start as usize..c.end as usize {
        s.push_str(clip(d.line(i)));
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizer_camel_accents() {
        let mut v = Vec::new();
        lex_tokens("useYMapVersion rejetPaume écrire XMLParser v2 a", &mut v);
        assert_eq!(v, vec!["use", "map", "version", "rejet", "paume", "ecrire", "xml", "parser"]);
    }

    #[test]
    fn mots_rg_et_termes() {
        assert_eq!(query_rg_words("ignorer la paume quand le stylet écrit"), vec!["ignorer", "paume", "stylet", "écrit"]);
        assert_eq!(query_lex_terms("compacterBoard du board"), vec!["compacter", "board"]);
    }

    #[test]
    fn fenetres_de_morceaux() {
        assert_eq!(chunk_ranges(0), vec![(0, 0)]);
        assert_eq!(chunk_ranges(40), vec![(0, 40)]);
        assert_eq!(chunk_ranges(41), vec![(0, 40), (30, 41)]);
        assert_eq!(chunk_ranges(100), vec![(0, 40), (30, 70), (60, 100)]);
    }
}
