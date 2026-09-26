//! Approches lexicales de référence : « ripgrep par mots-clés » (ce que fait un
//! agent sans Cortex) et un BM25 Okapi pur (sans champs, sans graphe, sans
//! synonymes), sur fichiers ou sur morceaux.

use super::corpus::{clip, lex_tokens, Corpus};
use super::metriques::{tokens_of, tokens_of_chars, Block};
use memchr::memmem;
use rayon::prelude::*;
use std::collections::HashMap;

// ─── ripgrep par mots-clés ──────────────────────────────────────────────────

/// Règle de classement de la simulation rg.
#[derive(Clone, Copy, PartialEq)]
pub enum RgRule {
    /// Nombre total de correspondances dans le fichier (`rg -i -c --count-matches`).
    Count,
    /// Nombre de mots DISTINCTS de la question présents, puis nombre de
    /// correspondances (variante plus forte : un agent qui lit la couverture).
    Terms,
}

pub struct RgFile {
    pub doc: usize,
    pub matches: usize,
    pub terms: usize,
    /// Lignes (0-based) contenant au moins un mot.
    pub lines: Vec<usize>,
}

/// Équivalent de `rg -i -n -F -e w1 -e w2 …` sur le corpus (en mémoire, texte
/// déjà en minuscules) : sous-chaîne insensible à la casse, pas de limite de
/// mot, multithread (rayon, comme rg). Retourne les fichiers qui matchent.
pub fn rg_scan(corpus: &Corpus, words: &[String]) -> Vec<RgFile> {
    let finders: Vec<memmem::Finder> = words.iter().map(|w| memmem::Finder::new(w.as_bytes())).collect();
    corpus
        .docs
        .par_iter()
        .enumerate()
        .filter_map(|(di, d)| {
            let hay = d.lower.as_bytes();
            let present: Vec<bool> = finders.iter().map(|f| f.find(hay).is_some()).collect();
            let terms = present.iter().filter(|&&p| p).count();
            if terms == 0 {
                return None;
            }
            let mut matches = 0usize;
            let mut lines = Vec::new();
            for (li, line) in d.lower.split('\n').enumerate() {
                let lb = line.as_bytes();
                let mut on_line = 0usize;
                for (fi, f) in finders.iter().enumerate() {
                    if present[fi] {
                        on_line += f.find_iter(lb).count();
                    }
                }
                if on_line > 0 {
                    matches += on_line;
                    lines.push(li);
                }
            }
            Some(RgFile { doc: di, matches, terms, lines })
        })
        .collect()
}

/// Classe les fichiers rg selon la règle ; égalités par chemin (déterministe).
pub fn rg_rank(corpus: &Corpus, mut files: Vec<RgFile>, rule: RgRule) -> Vec<RgFile> {
    files.sort_by(|a, b| {
        let key = |f: &RgFile| match rule {
            RgRule::Count => (f.matches, 0),
            RgRule::Terms => (f.terms, f.matches),
        };
        key(b).cmp(&key(a)).then_with(|| corpus.docs[a.doc].path.cmp(&corpus.docs[b.doc].path))
    });
    files
}

/// Tokens de la ligne de sortie rg `chemin:ligne:texte\n` d'une ligne.
fn rg_line_chars(path: &str, lineno: usize, text: &str) -> usize {
    path.chars().count() + 1 + digits(lineno) + 1 + clip(text).chars().count() + 1
}

fn digits(mut n: usize) -> usize {
    let mut d = 1;
    while n >= 10 {
        n /= 10;
        d += 1;
    }
    d
}

/// Un bloc par fichier (toutes ses lignes rg), dans l'ordre du classement,
/// pour les `limit` premiers fichiers ; plus les tokens de la sortie rg
/// COMPLÈTE (tous les fichiers qui matchent).
pub fn rg_blocks(corpus: &Corpus, ranked: &[RgFile], limit: usize) -> (Vec<Block>, usize) {
    let mut blocks = Vec::new();
    let mut total_chars = 0usize;
    for (i, f) in ranked.iter().enumerate() {
        let d = &corpus.docs[f.doc];
        let chars: usize = f.lines.iter().map(|&l| rg_line_chars(&d.path, l + 1, d.line(l.min(d.n_lines() - 1)))).sum();
        total_chars += chars;
        if i < limit {
            blocks.push(Block { file: d.path.clone(), tokens: tokens_of_chars(chars) });
        }
    }
    (blocks, tokens_of_chars(total_chars))
}

// ─── BM25 Okapi pur ─────────────────────────────────────────────────────────

pub const BM25_K1: f32 = 1.2;
pub const BM25_B: f32 = 0.75;

/// Index inversé BM25 minimal : un document = un sac de tokens `lex_tokens`
/// (chemin + contenu, sans champs ni pondération).
pub struct Bm25 {
    vocab: HashMap<String, u32>,
    postings: Vec<Vec<(u32, u32)>>,
    doc_len: Vec<u32>,
    avgdl: f32,
}

impl Bm25 {
    /// Construit l'index (tokenisation parallèle, fusion séquentielle).
    pub fn build(n: usize, text_of: impl Fn(usize) -> String + Sync) -> Bm25 {
        let per_doc: Vec<(u32, Vec<(String, u32)>)> = (0..n)
            .into_par_iter()
            .map(|i| {
                let mut toks = Vec::new();
                lex_tokens(&text_of(i), &mut toks);
                let len = toks.len() as u32;
                let mut tf: HashMap<String, u32> = HashMap::new();
                for t in toks {
                    *tf.entry(t).or_insert(0) += 1;
                }
                let mut v: Vec<(String, u32)> = tf.into_iter().collect();
                v.sort_unstable();
                (len, v)
            })
            .collect();
        let mut vocab: HashMap<String, u32> = HashMap::new();
        let mut postings: Vec<Vec<(u32, u32)>> = Vec::new();
        let mut doc_len = Vec::with_capacity(n);
        for (di, (len, tfs)) in per_doc.into_iter().enumerate() {
            doc_len.push(len);
            for (t, c) in tfs {
                let id = match vocab.get(&t) {
                    Some(&id) => id,
                    None => {
                        let id = postings.len() as u32;
                        vocab.insert(t, id);
                        postings.push(Vec::new());
                        id
                    }
                };
                postings[id as usize].push((di as u32, c));
            }
        }
        let avgdl = doc_len.iter().map(|&l| l as f64).sum::<f64>() as f32 / n.max(1) as f32;
        Bm25 { vocab, postings, doc_len, avgdl }
    }

    /// Taille mémoire estimée : 8 octets par posting, 4 par longueur de
    /// document, les chaînes du vocabulaire + 16 octets par entrée.
    pub fn approx_bytes(&self) -> usize {
        let p: usize = self.postings.iter().map(|v| v.len() * 8).sum();
        let v: usize = self.vocab.keys().map(|k| k.len() + 16).sum();
        p + v + self.doc_len.len() * 4
    }

    /// Les `limit` meilleurs documents (score décroissant, égalités par id).
    /// idf = ln(1 + (N − df + 0,5)/(df + 0,5)) (variante Lucene, toujours ≥ 0).
    pub fn search(&self, terms: &[String], limit: usize) -> Vec<(u32, f32)> {
        let n = self.doc_len.len() as f32;
        let mut acc: HashMap<u32, f32> = HashMap::new();
        for t in terms {
            let Some(&id) = self.vocab.get(t) else { continue };
            let post = &self.postings[id as usize];
            let df = post.len() as f32;
            let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
            for &(d, tf) in post {
                let tf = tf as f32;
                let dl = self.doc_len[d as usize] as f32;
                let s = idf * tf * (BM25_K1 + 1.0) / (tf + BM25_K1 * (1.0 - BM25_B + BM25_B * dl / self.avgdl));
                *acc.entry(d).or_insert(0.0) += s;
            }
        }
        let mut v: Vec<(u32, f32)> = acc.into_iter().collect();
        v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
        v.truncate(limit);
        v
    }
}

/// Sortie d'un moteur « liste de fichiers » : une ligne `- chemin\n` par fichier.
pub fn path_blocks(files: &[String]) -> Vec<Block> {
    files.iter().map(|f| Block { file: f.clone(), tokens: tokens_of(&format!("- {}\n", f)) }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bm25_classe_le_document_le_plus_specifique() {
        let docs = ["board codec zstd board", "board autre chose longue longue longue longue", "rien du tout"];
        let idx = Bm25::build(docs.len(), |i| docs[i].to_string());
        let r = idx.search(&["board".into(), "zstd".into()], 10);
        assert_eq!(r[0].0, 0);
        assert_eq!(r.len(), 2, "le document sans terme n'est pas renvoyé");
        assert!(idx.search(&["absent".into()], 10).is_empty());
    }
}
