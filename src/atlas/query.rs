//! Requêtes SUR l'atlas ouvert (mmap), en lecture fusionnée tronc + deltas
//! (`view.rs`) : `search` (BM25F piloté par les index inversés des segments),
//! la résolution des symboles par nom (`find_symbol`). Les outils pour agents
//! (`outils` : card, impact, path…) lisent les liens dans les CSR (`view.rs`).
//!
//! Champs du BM25F : noms de symboles (flou : préfixe, distance 1), doc-comments
//! et en-têtes de fichier, bonus de chemin, et CORPS des fichiers (terme exact,
//! poids faible, ajouté au meilleur symbole du fichier). Requête et index sont
//! racinés par la même fonction (`stem`), après l'expansion par le glossaire.
//!
//! `search` ne parcourt que le VOCABULAIRE (automate fst persisté de chaque
//! segment, lu dans le mmap) pour trouver quels termes matchent flou la
//! requête, puis ne visite QUE leurs postings — en ignorant ceux d'une version
//! de nœud qui n'est plus la courante. Statistiques de corpus = somme exacte
//! des contributions des segments. Tout départage d'égalité et tout affichage
//! suivent l'ordre canonique (chemin, rang) : « tronc + deltas » répond
//! exactement comme une reconstruction complète (test d'équivalence dans
//! `incremental.rs`).

use super::view::NodeRef;
use super::Handle;
use crate::fx::{FxHashMap, FxHashSet};
use crate::search::{is_test_path, query_terms, Hit, Weights, B, K1, SYNONYM_WEIGHT, TEST_WORDS};
use crate::semantic::{expand_synonyms, fuzzy_match};
use crate::symbol::SymbolKind;
use fst::automaton::{Levenshtein, Str};
use fst::{Automaton, IntoStreamer, Map, Streamer};

/// Candidats d'un terme de requête contre un ensemble de mots trié (automate
/// fst d'un segment), sans le balayer — reproduit les 3 branches de
/// `semantic::fuzzy_match` (égalité, préfixe dans un sens ou dans l'autre,
/// distance d'édition 1) ; le score exact est ensuite recalculé par
/// `fuzzy_match`. L'ordre relatif de deux mots candidats ne dépend que des mots
/// et du terme (phase puis ordre lexicographique/longueur), jamais de
/// l'ensemble de clés : un nœud reçoit donc ses contributions dans le même
/// ordre qu'il vive dans le tronc ou dans un delta (sommes flottantes identiques).
fn fst_candidates<D: AsRef<[u8]>>(map: &Map<D>, q: &TermQuery) -> Vec<u32> {
    let term = q.term.as_str();
    let mut out: Vec<u32> = Vec::new();
    let mut push = |v: u64| {
        let v = v as u32;
        if !out.contains(&v) {
            out.push(v);
        }
    };
    if let Some(v) = map.get(term.as_bytes()) {
        push(v);
    }
    if term.len() >= 4 {
        let aut = Str::new(term).starts_with();
        let mut stream = map.search(aut).into_stream();
        while let Some((_, v)) = stream.next() {
            push(v);
        }
        for end in 4..term.len() {
            if let Some(v) = map.get(term[..end].as_bytes()) {
                push(v);
            }
        }
        if let Some(lev) = &q.lev {
            let mut stream = map.search(lev).into_stream();
            while let Some((_, v)) = stream.next() {
                push(v);
            }
        }
    }
    out
}

/// Un terme de requête et son automate de Levenshtein (distance 1), construit
/// UNE fois par requête puis appliqué à l'automate de chaque segment.
struct TermQuery {
    term: String,
    lev: Option<Levenshtein>,
}

impl TermQuery {
    fn new(term: &str) -> Self {
        let lev = if term.len() >= 4 { Levenshtein::new(term, 1).ok() } else { None };
        TermQuery { term: term.to_string(), lev }
    }
}

/// Automate fst persisté d'un segment, ouvert sans copie (mmap).
fn seg_map(bytes: &[u8]) -> Option<Map<&[u8]>> {
    Map::new(bytes).ok()
}

/// Postings d'UN terme de requête, tous segments confondus : (tf brut,
/// longueur du champ) par nœud global pour les noms et les commentaires,
/// nœuds distincts (df), et (nœud, tf, longueur) pour le corps.
struct TermPostings {
    raw_name: FxHashMap<u32, (f32, usize)>,
    raw_com: FxHashMap<u32, (f32, usize)>,
    df_name: FxHashSet<u32>,
    df_com: FxHashSet<u32>,
    raw_body: Vec<(u32, u32, u32)>,
}

impl Handle {
    /// Lecture des postings d'un terme (`term` raciné, automate `tq`). Une seule
    /// version valide par nœud, donc un seul segment contribue pour un nœud
    /// donné ; les segments sont lus dans l'ordre (sommes flottantes stables).
    fn term_postings(&self, term: &str, tq: &TermQuery, with_body: bool) -> TermPostings {
        let mut tp = TermPostings {
            raw_name: FxHashMap::default(),
            raw_com: FxHashMap::default(),
            df_name: FxHashSet::default(),
            df_com: FxHashSet::default(),
            raw_body: Vec::new(),
        };
        for s in 0..self.nsegs() {
            let seg = self.seg(s);
            let inv = &seg.inverted;
            let Some(map) = seg_map(&seg.vocab_fst) else { continue };
            for vid in fst_candidates(&map, tq) {
                let vid = vid as usize;
                let fw = fuzzy_match(&tq.term, seg.strings[inv.vocab[vid] as usize].as_str());
                if fw <= 0.0 {
                    continue;
                }
                let (a, b) = (inv.name_postings_off[vid] as usize, inv.name_postings_off[vid + 1] as usize);
                for &local in &inv.name_postings[a..b] {
                    let Some(g) = self.current_local(s, local as usize) else { continue };
                    let e = tp.raw_name.entry(g).or_insert((0.0, seg.nodes[local as usize].tokens.len()));
                    e.0 += fw;
                    tp.df_name.insert(g);
                }
                let (a, b) = (inv.com_postings_off[vid] as usize, inv.com_postings_off[vid + 1] as usize);
                for p in &inv.com_postings[a..b] {
                    let Some(g) = self.current_local(s, p.node as usize) else { continue };
                    let e = tp.raw_com.entry(g).or_insert((0.0, seg.nodes[p.node as usize].doc.len()));
                    e.0 += fw * p.tf as f32;
                    tp.df_com.insert(g);
                }
            }
        }
        // Champ CORPS : terme exact (déjà raciné), vocabulaire propre par
        // segment. Une seule version valide par fichier → un seul posting.
        if with_body {
            for s in 0..self.nsegs() {
                let seg = self.seg(s);
                let Some(map) = seg_map(&seg.body_fst) else { continue };
                let Some(bid) = map.get(term.as_bytes()) else { continue };
                let inv = &seg.inverted;
                let (a, b) = (inv.body_postings_off[bid as usize] as usize, inv.body_postings_off[bid as usize + 1] as usize);
                for (&node, &tf) in inv.body_post_nodes[a..b].iter().zip(&inv.body_post_tf[a..b]) {
                    let Some(g) = self.current_local(s, node as usize) else { continue };
                    tp.raw_body.push((g, tf as u32, seg.nodes[node as usize].body_len));
                }
            }
        }
        tp
    }

    /// Fichiers vivants dont un token de chemin matche flou `term` (automates
    /// de chemins persistés, un par segment).
    fn files_matching_path_term(&self, q: &TermQuery) -> Vec<u32> {
        let term = q.term.as_str();
        let mut files: Vec<u32> = Vec::new();
        for s in 0..self.nsegs() {
            let seg = self.seg(s);
            let Some(map) = seg_map(&seg.path_fst) else { continue };
            for tid in fst_candidates(&map, q) {
                let tid = tid as usize;
                if fuzzy_match(term, seg.strings[seg.path_tok_sids[tid] as usize].as_str()) <= 0.0 {
                    continue;
                }
                let (a, b) = (seg.path_tok_off[tid] as usize, seg.path_tok_off[tid + 1] as usize);
                for &local in &seg.path_tok_files[a..b] {
                    if let Some(g) = self.current_local(s, local as usize) {
                        if !files.contains(&g) {
                            files.push(g);
                        }
                    }
                }
            }
        }
        files
    }

    /// Trouve un symbole par nom (exact insensible à la casse, sinon premier
    /// qui le contient) — le PREMIER dans l'ordre canonique (chemin, rang).
    pub(crate) fn find_symbol(&self, name: &str) -> Option<u32> {
        let n = name.to_ascii_lowercase();
        let exact = self.defs(&n);
        if !exact.is_empty() {
            return exact.into_iter().min_by(|&a, &b| self.sort_key(a).cmp(&self.sort_key(b)));
        }
        self.live_nodes()
            .filter(|r| r.n.kind == 1 && r.name().to_ascii_lowercase().contains(&n))
            .map(|r| r.g)
            .min_by(|&a, &b| self.sort_key(a).cmp(&self.sort_key(b)))
    }

    /// Recherche BM25F — voir l'en-tête du fichier.
    pub fn search(&self, question: &str, limit: usize) -> Vec<Hit> {
        let base_terms = query_terms(question);
        if base_terms.is_empty() {
            return Vec::new();
        }
        let w = Weights::load();
        let test_query = base_terms.iter().any(|t| TEST_WORDS.contains(&t.as_str()));
        let expanded = expand_synonyms(&base_terms);
        // Termes racinés (comme à l'indexation) ; deux mots de même radical
        // n'en font qu'un, au poids le plus fort. `raw` garde le mot saisi pour
        // les chemins (tokens bruts).
        let mut weighted: Vec<(String, f32)> = Vec::new();
        let mut raw: Vec<String> = Vec::new();
        for t in &expanded {
            let wt = if base_terms.contains(t) { 1.0 } else { SYNONYM_WEIGHT };
            let st = crate::stem::stem(t);
            match weighted.iter().position(|(x, _)| *x == st) {
                Some(i) => weighted[i].1 = weighted[i].1.max(wt),
                None => {
                    weighted.push((st, wt));
                    raw.push(t.clone());
                }
            }
        }

        let st = self.v().stats;
        let n_names = (st.name_docs.max(1)) as f32;
        let avgdl_names = (st.name_len as f32 / n_names).max(1.0);
        let n_com = (st.com_docs.max(1)) as f32;
        let avgdl_com = (st.com_len as f32 / n_com).max(1.0);
        let n_body = (st.body_docs.max(1)) as f32;
        let avgdl_body = (st.body_len as f32 / n_body).max(1.0);

        let mut name_score: FxHashMap<u32, f32> = FxHashMap::default();
        let mut com_score: FxHashMap<u32, f32> = FxHashMap::default();
        let mut body_score: FxHashMap<u32, f32> = FxHashMap::default();
        let mut term_idf_names: Vec<f32> = vec![0.0; weighted.len()];

        // Termes indépendants les uns des autres jusqu'au cumul des scores :
        // automates de Levenshtein et lecture des postings EN PARALLÈLE (un
        // terme par tâche), puis cumul SÉQUENTIEL dans l'ordre des termes — les
        // sommes flottantes, donc le classement, sont identiques au bit près.
        use rayon::prelude::*;
        let tqs: Vec<TermQuery> = weighted.par_iter().map(|(t, _)| TermQuery::new(t)).collect();
        let per_term: Vec<TermPostings> =
            (0..weighted.len()).into_par_iter().map(|ti| self.term_postings(&weighted[ti].0, &tqs[ti], w.body > 0.0)).collect();
        for (ti, ((_, term_w), tp)) in weighted.iter().zip(per_term).enumerate() {
            let TermPostings { raw_name, raw_com, df_name, df_com, raw_body } = tp;
            let df = df_name.len() as f32;
            let idf = (((n_names - df + 0.5) / (df + 0.5)) + 1.0).ln().max(0.0);
            term_idf_names[ti] = idf;
            for (node, (raw_tf, dl)) in raw_name {
                let tf = raw_tf * term_w;
                let dl = dl as f32;
                let contrib = idf * (tf * (K1 + 1.0)) / (tf + K1 * (1.0 - B + B * dl / avgdl_names));
                *name_score.entry(node).or_insert(0.0) += contrib;
            }
            let df_c = df_com.len() as f32;
            let idf_c = (((n_com - df_c + 0.5) / (df_c + 0.5)) + 1.0).ln().max(0.0);
            for (node, (raw_tf, dl)) in raw_com {
                let tf = raw_tf * term_w;
                let dl = dl as f32;
                let contrib = idf_c * (tf * (K1 + 1.0)) / (tf + K1 * (1.0 - B + B * dl / avgdl_com));
                *com_score.entry(node).or_insert(0.0) += contrib;
            }
            // Champ CORPS : terme exact (déjà raciné), vocabulaire propre par
            // segment. Une seule version valide par fichier → un seul posting.
            if w.body > 0.0 {
                let df_b = raw_body.len() as f32;
                let idf_b = (((n_body - df_b + 0.5) / (df_b + 0.5)) + 1.0).ln().max(0.0);
                for (node, tf, dl) in raw_body {
                    let tf = tf as f32 * term_w;
                    let dl = dl as f32;
                    let contrib = idf_b * (tf * (K1 + 1.0)) / (tf + K1 * (1.0 - B + B * dl / avgdl_body));
                    *body_score.entry(node).or_insert(0.0) += contrib;
                }
            }
        }

        // Bonus "terme dans le chemin" : par FICHIER, via `PathIndex` (fst).
        // Fichiers de chaque terme en parallèle, cumul dans l'ordre des termes.
        // Les chemins gardent leurs tokens bruts : on les cherche avec le mot
        // saisi (automate réutilisé s'il n'a pas changé à la racinisation).
        let path_files: Vec<Vec<u32>> = (0..weighted.len())
            .into_par_iter()
            .map(|ti| {
                if weighted[ti].1 * term_idf_names[ti] * 0.3 <= 0.0 {
                    Vec::new()
                } else if raw[ti] == weighted[ti].0 {
                    self.files_matching_path_term(&tqs[ti])
                } else {
                    self.files_matching_path_term(&TermQuery::new(&raw[ti]))
                }
            })
            .collect();
        let mut file_path_bonus: FxHashMap<u32, f32> = FxHashMap::default();
        for (ti, ((_, term_w), files)) in weighted.iter().zip(path_files).enumerate() {
            let add = term_w * term_idf_names[ti] * 0.3;
            if add <= 0.0 {
                continue;
            }
            for f in files {
                *file_path_bonus.entry(f).or_insert(0.0) += add;
            }
        }

        let joined = base_terms.join(" ");
        let joined_ns = joined.replace(' ', "");
        let contains_ci = |hay: &str, needle: &str| -> bool {
            if needle.is_empty() || needle.len() > hay.len() {
                return false;
            }
            hay.as_bytes().windows(needle.len()).any(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
        };
        // Candidats SANS allocation (chaînes empruntées au mmap) : les `Hit`
        // ne sont construits que pour les `limit` premiers. Phase 0 = symbole,
        // 1 = repli sur le premier symbole d'un fichier trouvé par son en-tête
        // ou son corps.
        struct Cand<'h> {
            score: f32,
            name: &'h str,
            phase: u8,
            file: &'h str,
            ord: u32,
            kind: SymbolKind,
            line: u32,
            g: u32,
        }
        // Symboles candidats : tranches de nœuds indépendantes, notées en
        // parallèle puis fusionnées DANS L'ORDRE des nœuds (mêmes indices ; même
        // meilleur symbole par fichier, la préférence (score, rang) étant un
        // ordre strict entre symboles distincts).
        const TRANCHE: usize = 4096;
        let tranches: Vec<(usize, usize, usize)> = (0..self.nsegs())
            .flat_map(|s| {
                let n = self.seg(s).nodes.len();
                (0..n).step_by(TRANCHE).map(move |a| (s, a, (a + TRANCHE).min(n)))
            })
            .collect();
        type Best = (f32, u32, usize);
        let parts: Vec<(Vec<Cand>, Vec<(u32, Best)>)> = tranches
            .par_iter()
            .map(|&(s, a, b)| {
                let seg = self.seg(s);
                let mut cands: Vec<Cand> = Vec::new();
                let mut best: FxHashMap<u32, Best> = FxHashMap::default();
                for local in a..b {
                    let n = &seg.nodes[local];
                    if n.kind != 1 {
                        continue;
                    }
                    let Some(g) = self.current_local(s, local) else { continue };
                    let r = NodeRef { seg, g, n };
                    let mut score = *name_score.get(&g).unwrap_or(&0.0);
                    let file_g = n.owner_file;
                    score += *file_path_bonus.get(&file_g).unwrap_or(&0.0);
                    let name_str = r.name();
                    if name_str.eq_ignore_ascii_case(&joined) || name_str.eq_ignore_ascii_case(&joined_ns) {
                        score += 50.0;
                    } else if contains_ci(name_str, &joined_ns) {
                        score += 5.0;
                    }
                    let kind = SymbolKind::from_u8(n.sym_kind);
                    if matches!(kind, SymbolKind::Class | SymbolKind::Hook | SymbolKind::Component | SymbolKind::Interface) {
                        score *= 1.15;
                    }
                    if w.doc > 0.0 && !n.doc.is_empty() {
                        score += w.doc * com_score.get(&g).unwrap_or(&0.0);
                    }
                    if score <= 0.0 {
                        continue;
                    }
                    let ci = cands.len();
                    cands.push(Cand { score, name: name_str, phase: 0, file: self.path_of(file_g), ord: n.ord, kind, line: n.line, g });
                    let e = best.entry(file_g).or_insert((f32::MIN, u32::MAX, ci));
                    if score > e.0 || (score == e.0 && n.ord < e.1) {
                        *e = (score, n.ord, ci);
                    }
                }
                (cands, best.into_iter().collect())
            })
            .collect();
        let mut cands: Vec<Cand> = Vec::new();
        // Meilleur symbole par fichier : (score, rang, index du candidat) — à
        // score égal, le plus petit rang (ordre canonique).
        let mut best_in_file: FxHashMap<u32, Best> = FxHashMap::default();
        for (pc, pb) in parts {
            let off = cands.len();
            for (f, (score, ord, ci)) in pb {
                let e = best_in_file.entry(f).or_insert((f32::MIN, u32::MAX, ci + off));
                if score > e.0 || (score == e.0 && ord < e.1) {
                    *e = (score, ord, ci + off);
                }
            }
            cands.extend(pc);
        }

        if w.header > 0.0 || w.body > 0.0 {
            for fr in self.live_files() {
                let mut hs = 0.0f32;
                if w.header > 0.0 && !fr.n.doc.is_empty() {
                    hs += w.header * com_score.get(&fr.g).unwrap_or(&0.0);
                }
                if w.body > 0.0 && fr.n.body_len > 0 {
                    hs += w.body * body_score.get(&fr.g).unwrap_or(&0.0);
                }
                if hs <= 0.0 {
                    continue;
                }
                if let Some(&(_, _, ci)) = best_in_file.get(&fr.g) {
                    cands[ci].score += hs;
                } else {
                    // Premier symbole non-import du fichier (ordre des rangs).
                    let first = self
                        .contains_raw(fr.g)
                        .iter()
                        .filter_map(|&sg| self.node(sg))
                        .filter(|x| x.kind() != SymbolKind::Import)
                        .min_by_key(|x| x.n.ord);
                    if let Some(sn) = first {
                        cands.push(Cand {
                            score: hs,
                            name: sn.name(),
                            phase: 1,
                            file: fr.name(),
                            ord: sn.n.ord,
                            kind: sn.kind(),
                            line: sn.n.line,
                            g: sn.g,
                        });
                    }
                }
            }
        }
        if !test_query && w.test_penalty != 1.0 {
            let mut is_test: FxHashMap<&str, bool> = FxHashMap::default();
            for c in cands.iter_mut() {
                if *is_test.entry(c.file).or_insert_with(|| is_test_path(c.file)) {
                    c.score *= w.test_penalty;
                }
            }
        }
        // Score décroissant, nom court, puis ordre canonique (symboles avant les
        // replis, chemin, rang) — l'ordre d'une reconstruction complète. Ordre
        // TOTAL (un symbole n'est candidat qu'une fois) : sélectionner les
        // `limit` premiers puis les trier donne exactement le tri complet tronqué.
        let cmp = |a: &Cand, b: &Cand| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.name.len().cmp(&b.name.len()))
                .then_with(|| a.phase.cmp(&b.phase))
                .then_with(|| a.file.cmp(b.file))
                .then_with(|| a.ord.cmp(&b.ord))
        };
        if limit == 0 {
            return Vec::new();
        }
        if cands.len() > limit {
            cands.select_nth_unstable_by(limit - 1, cmp);
            cands.truncate(limit);
        }
        cands.sort_unstable_by(cmp);
        cands
            .into_iter()
            .map(|c| Hit {
                score: c.score,
                name: c.name.to_string(),
                kind: c.kind,
                file: c.file.to_string(),
                line: c.line,
                project: self.project.clone(),
                g: c.g,
            })
            .collect()
    }
}
