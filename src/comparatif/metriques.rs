//! Métriques du banc comparatif : rang, top-k, MRR, tokens lus, fusion RRF,
//! percentiles. Fonctions pures, testées : c'est le juge commun à toutes les
//! approches (mêmes questions, même juge).

/// Au-delà de ce rang (en fichiers distincts), la réponse compte comme non
/// trouvée (RR = 0). Identique à `cortex bench`.
pub const RANK_CUTOFF: usize = 20;

/// Constante k de la fusion RRF (Cormack et al., 2009) : score = Σ 1/(k + rang).
pub const RRF_K: f64 = 60.0;

/// Normalisation de chemin du juge (identique à `bench::rank_of`).
pub fn norm_path(s: &str) -> String {
    s.replace('\\', "/").to_ascii_lowercase()
}

/// Rang (1-based) du premier fichier attendu dans une liste ordonnée de
/// fichiers distincts, borné à `RANK_CUTOFF`.
pub fn rank_of(files: &[String], expect: &[String]) -> Option<usize> {
    let expected: Vec<String> = expect.iter().map(|e| norm_path(e)).collect();
    files.iter().take(RANK_CUTOFF).position(|f| expected.contains(&norm_path(f))).map(|p| p + 1)
}

/// Tokens d'un texte : **caractères Unicode / 4, arrondi au supérieur** —
/// la même règle pour toutes les approches (≈ tokenizer BPE sur du code).
pub fn tokens_of(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

/// Tokens d'un texte dont on ne connaît que le nombre de caractères.
pub fn tokens_of_chars(chars: usize) -> usize {
    chars.div_ceil(4)
}

/// Un bloc de sortie tel que l'agent le lit (une ligne de hit, un morceau de
/// RAG, les lignes rg d'un fichier…), rattaché au fichier qu'il désigne.
pub struct Block {
    pub file: String,
    pub tokens: usize,
}

/// Tokens que l'agent lit dans la sortie **jusqu'au premier bloc désignant un
/// fichier attendu, ce bloc compris** (plus l'en-tête fixe de la sortie).
/// `None` si aucun bloc ne désigne un fichier attendu.
pub fn tokens_to_first_good(header: usize, blocks: &[Block], expect: &[String]) -> Option<usize> {
    let expected: Vec<String> = expect.iter().map(|e| norm_path(e)).collect();
    let mut acc = header;
    for b in blocks {
        acc += b.tokens;
        if expected.contains(&norm_path(&b.file)) {
            return Some(acc);
        }
    }
    None
}

/// Tokens lus jusqu'à couvrir `k` fichiers distincts (le bloc qui introduit le
/// k-ième compris) ; toute la sortie si elle en couvre moins.
pub fn tokens_for_top_k(header: usize, blocks: &[Block], k: usize) -> usize {
    let mut seen: Vec<&str> = Vec::new();
    let mut acc = header;
    for b in blocks {
        acc += b.tokens;
        if !seen.contains(&b.file.as_str()) {
            seen.push(&b.file);
            if seen.len() >= k {
                break;
            }
        }
    }
    acc
}

/// Réduit une liste ordonnée d'éléments (hits, morceaux) à la liste ordonnée
/// des fichiers distincts, bornée à `limit`.
pub fn distinct_files<'a>(items: impl IntoIterator<Item = &'a str>, limit: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for f in items {
        if !out.iter().any(|o| o == f) {
            out.push(f.to_string());
            if out.len() >= limit {
                break;
            }
        }
    }
    out
}

/// Fusion RRF de plusieurs classements (identifiants dans l'ordre, meilleur en
/// tête) : score(d) = Σ_listes 1/(k + rang_1based). Un élément absent d'une
/// liste n'en reçoit rien. Égalités départagées par le meilleur rang obtenu
/// dans une liste, puis par l'identifiant (déterministe).
pub fn rrf_fuse<T: Clone + Ord + std::hash::Hash>(lists: &[Vec<T>], k: f64) -> Vec<(T, f64)> {
    use std::collections::HashMap;
    let mut acc: HashMap<T, (f64, usize)> = HashMap::new();
    for list in lists {
        for (i, id) in list.iter().enumerate() {
            let e = acc.entry(id.clone()).or_insert((0.0, usize::MAX));
            e.0 += 1.0 / (k + (i + 1) as f64);
            e.1 = e.1.min(i);
        }
    }
    let mut v: Vec<(T, f64, usize)> = acc.into_iter().map(|(id, (s, best))| (id, s, best)).collect();
    v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.2.cmp(&b.2)).then(a.0.cmp(&b.0)));
    v.into_iter().map(|(id, s, _)| (id, s)).collect()
}

/// Résultat d'une question pour une approche.
#[derive(Clone, Debug, serde::Serialize)]
pub struct QResult {
    pub rank: Option<usize>,
    /// Tokens lus jusqu'au premier bon résultat (None si non trouvé).
    pub tokens_first_good: Option<usize>,
    /// Tokens lus pour couvrir 5 fichiers distincts.
    pub tokens_top5: usize,
    /// Tokens de la sortie par défaut complète de l'approche.
    pub tokens_full: usize,
    pub latency_ms: f64,
    pub top_files: Vec<String>,
}

/// Agrégats d'un ensemble de questions.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Summary {
    pub n: usize,
    pub top1: f64,
    pub top5: f64,
    pub mrr: f64,
    pub lat_median_ms: f64,
    pub lat_p95_ms: f64,
    /// Médiane des tokens jusqu'au premier bon, sur les questions trouvées.
    pub tok_first_good_median: f64,
    /// Moyenne des tokens jusqu'au premier bon, un échec comptant la sortie
    /// complète (l'agent a tout lu pour rien).
    pub tok_first_good_mean_penalized: f64,
    pub tok_top5_median: f64,
    pub tok_full_median: f64,
}

/// Percentile par rang le plus proche (nearest-rank) : p ∈ [0, 100].
pub fn percentile(values: &[f64], p: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let rank = ((p / 100.0) * v.len() as f64).ceil().max(1.0) as usize;
    v[rank.min(v.len()) - 1]
}

/// Médiane (moyenne des deux valeurs centrales pour un effectif pair).
pub fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// Réciproque du rang (0 si non trouvé).
pub fn reciprocal_rank(rank: Option<usize>) -> f64 {
    rank.map(|k| 1.0 / k as f64).unwrap_or(0.0)
}

pub fn summarize(rs: &[&QResult]) -> Summary {
    let n = rs.len();
    if n == 0 {
        return Summary::default();
    }
    let nf = n as f64;
    let lat: Vec<f64> = rs.iter().map(|r| r.latency_ms).collect();
    let found: Vec<f64> = rs.iter().filter_map(|r| r.tokens_first_good.map(|t| t as f64)).collect();
    let penalized: f64 = rs.iter().map(|r| r.tokens_first_good.unwrap_or(r.tokens_full.max(r.tokens_top5)) as f64).sum::<f64>() / nf;
    Summary {
        n,
        top1: rs.iter().filter(|r| r.rank == Some(1)).count() as f64 / nf,
        top5: rs.iter().filter(|r| matches!(r.rank, Some(k) if k <= 5)).count() as f64 / nf,
        mrr: rs.iter().map(|r| reciprocal_rank(r.rank)).sum::<f64>() / nf,
        lat_median_ms: median(&lat),
        lat_p95_ms: percentile(&lat, 95.0),
        tok_first_good_median: median(&found),
        tok_first_good_mean_penalized: penalized,
        tok_top5_median: median(&rs.iter().map(|r| r.tokens_top5 as f64).collect::<Vec<_>>()),
        tok_full_median: median(&rs.iter().map(|r| r.tokens_full as f64).collect::<Vec<_>>()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }
    fn qr(rank: Option<usize>, first: Option<usize>, full: usize, lat: f64) -> QResult {
        QResult { rank, tokens_first_good: first, tokens_top5: 10, tokens_full: full, latency_ms: lat, top_files: vec![] }
    }

    #[test]
    fn rang_normalise_et_borne() {
        let files = s(&["src/a.ts", "SRC\\B.ts", "c.ts"]);
        assert_eq!(rank_of(&files, &s(&["src/b.ts"])), Some(2));
        assert_eq!(rank_of(&files, &s(&["x.ts", "c.ts"])), Some(3));
        assert_eq!(rank_of(&files, &s(&["x.ts"])), None);
        let long: Vec<String> = (0..30).map(|i| format!("f{i}")).collect();
        assert_eq!(rank_of(&long, &s(&["f19"])), Some(20));
        assert_eq!(rank_of(&long, &s(&["f20"])), None, "au-delà du rang 20 : non trouvé");
    }

    #[test]
    fn mrr_top1_top5() {
        let a = qr(Some(1), Some(5), 50, 1.0);
        let b = qr(Some(4), Some(20), 50, 2.0);
        let c = qr(None, None, 80, 3.0);
        let d = qr(Some(10), Some(40), 50, 4.0);
        let sum = summarize(&[&a, &b, &c, &d]);
        assert_eq!(sum.n, 4);
        assert!((sum.top1 - 0.25).abs() < 1e-9);
        assert!((sum.top5 - 0.5).abs() < 1e-9);
        assert!((sum.mrr - (1.0 + 0.25 + 0.0 + 0.1) / 4.0).abs() < 1e-9);
        // Médiane des tokens sur les trouvées : (5, 20, 40) -> 20.
        assert!((sum.tok_first_good_median - 20.0).abs() < 1e-9);
        // Moyenne pénalisée : l'échec compte sa sortie complète (80).
        assert!((sum.tok_first_good_mean_penalized - (5.0 + 20.0 + 80.0 + 40.0) / 4.0).abs() < 1e-9);
        assert!((sum.lat_median_ms - 2.5).abs() < 1e-9);
    }

    #[test]
    fn tokens_quatre_caracteres() {
        assert_eq!(tokens_of(""), 0);
        assert_eq!(tokens_of("abcd"), 1);
        assert_eq!(tokens_of("abcde"), 2);
        assert_eq!(tokens_of("éèàç"), 1, "compte en caractères, pas en octets");
    }

    #[test]
    fn tokens_jusqu_au_premier_bon_et_top5() {
        let b = |f: &str, t: usize| Block { file: f.into(), tokens: t };
        let blocks = vec![b("a", 10), b("a", 5), b("b", 7), b("c", 3), b("d", 1), b("e", 2), b("f", 100)];
        assert_eq!(tokens_to_first_good(4, &blocks, &s(&["c"])), Some(4 + 10 + 5 + 7 + 3));
        assert_eq!(tokens_to_first_good(4, &blocks, &s(&["A"])), Some(14), "bloc du bon fichier compris");
        assert_eq!(tokens_to_first_good(4, &blocks, &s(&["z"])), None);
        // 5 fichiers distincts atteints au bloc « e ».
        assert_eq!(tokens_for_top_k(4, &blocks, 5), 4 + 10 + 5 + 7 + 3 + 1 + 2);
        assert_eq!(tokens_for_top_k(0, &blocks[..2], 5), 15, "moins de 5 fichiers : toute la sortie");
    }

    #[test]
    fn fusion_rrf() {
        let bm25 = vec!["a", "b", "c"];
        let dense = vec!["c", "a", "d"];
        let fused = rrf_fuse(&[bm25, dense], RRF_K);
        let ids: Vec<&str> = fused.iter().map(|x| x.0).collect();
        // a : 1/61 + 1/62 ; c : 1/63 + 1/61 ; b : 1/62 ; d : 1/63.
        assert_eq!(ids, vec!["a", "c", "b", "d"]);
        assert!((fused[0].1 - (1.0 / 61.0 + 1.0 / 62.0)).abs() < 1e-12);
        // Égalité parfaite : départagée par le meilleur rang puis l'identifiant.
        let tie = rrf_fuse(&[vec!["x", "y"], vec!["y", "x"]], RRF_K);
        assert_eq!(tie[0].0, "x");
    }

    #[test]
    fn fichiers_distincts_et_percentiles() {
        let f = distinct_files(["a", "a", "b", "a", "c", "d"], 3);
        assert_eq!(f, s(&["a", "b", "c"]));
        let v: Vec<f64> = (1..=20).map(|x| x as f64).collect();
        assert_eq!(percentile(&v, 95.0), 19.0);
        assert_eq!(percentile(&v, 100.0), 20.0);
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
    }
}
