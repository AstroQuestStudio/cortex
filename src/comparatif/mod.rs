//! `cortex bench-compare` : banc COMPARATIF (architecture v2, §11).
//!
//! Mêmes questions, même juge (`metriques::rank_of`, identique à `cortex
//! bench`), même corpus (les fichiers vivants de l'atlas du projet, relus sur
//! disque) pour chaque approche :
//! - `rg-compte`   : mots de la question dans `rg -i`, fichiers classés par nombre de correspondances ;
//! - `rg-termes`   : idem, classés par nombre de mots distincts trouvés puis de correspondances ;
//! - `bm25-fich`   : BM25 Okapi pur sur fichiers (chemin + contenu, sans champs) ;
//! - `rag-dense`   : morceaux 40 lignes + embeddings model2vec + cosinus (feature `bench`) ;
//! - `hybride`     : BM25 sur morceaux + dense, fusion RRF k=60 (feature `bench`) ;
//! - `cortex`      : l'atlas tel qu'il est (`Handle::search`, comme `cortex bench`) ;
//! - `exp:*`       : expériences d'analyse (Cortex fusionné au dense ou au BM25 contenu) — pas le moteur.
//!
//! Pour chaque approche : top-1, top-5, MRR, latence par requête (médiane,
//! p95, hors construction), coût de construction, et tokens lus par l'agent
//! (voir `metriques`). Sortie : tableaux terminal + JSON dans `bench/resultats/`.

pub mod corpus;
#[cfg(feature = "bench")]
pub mod dense;
pub mod lexical;
pub mod metriques;

use crate::bench::BenchFile;
use corpus::Corpus;
use lexical::path_blocks;
use lexical::{Bm25, RgRule};
use metriques::{distinct_files, summarize, tokens_for_top_k, tokens_of, tokens_to_first_good, Block, QResult, Summary, RANK_CUTOFF};
use std::time::Instant;

/// Nombre de hits Cortex demandés (identique à `cortex bench`).
const CORTEX_HITS: usize = 400;
/// Morceaux examinés par requête dense / BM25-morceaux avant réduction en fichiers.
#[cfg_attr(not(feature = "bench"), allow(dead_code))]
const CHUNK_POOL: usize = 3000;
/// Sortie par défaut d'un RAG : les k = 10 meilleurs morceaux.
#[cfg_attr(not(feature = "bench"), allow(dead_code))]
const RAG_K: usize = 10;

/// Ce qu'une approche renvoie pour une question.
pub struct Output {
    pub files: Vec<String>,
    pub header_tokens: usize,
    pub blocks: Vec<Block>,
    pub full_tokens: usize,
}

pub struct Build {
    pub secs: f64,
    pub bytes: u64,
    pub note: String,
}

pub struct Approach<'a> {
    pub name: &'static str,
    pub build: Build,
    pub run: Box<dyn Fn(&str) -> Output + 'a>,
}

#[derive(serde::Serialize)]
struct JsonApproach {
    name: String,
    build_secs: f64,
    build_bytes: u64,
    build_note: String,
    summary: std::collections::BTreeMap<String, Summary>,
    questions: Vec<JsonQuestion>,
}

#[derive(serde::Serialize)]
struct JsonQuestion {
    q: String,
    tag: String,
    #[serde(flatten)]
    r: QResult,
}

/// Classement Cortex (mêmes paramètres que `cortex bench`) : hits triés.
fn cortex_hits(handles: &[crate::atlas::Handle], q: &str) -> Vec<crate::search::Hit> {
    let mut hits: Vec<crate::search::Hit> = Vec::new();
    for h in handles {
        hits.extend(h.search(q, CORTEX_HITS));
    }
    hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    hits
}

fn cortex_output(handles: &[crate::atlas::Handle], q: &str, file_limit: usize) -> Output {
    let hits = cortex_hits(handles, q);
    let files = distinct_files(hits.iter().map(|h| h.file.as_str()), file_limit);
    // Blocs = une ligne de `cortex find` par symbole (identifiant stable,
    // genre, ligne), jusqu'à couvrir RANK_CUTOFF fichiers.
    let mut blocks = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    for h in &hits {
        let id = handles.iter().find(|x| x.project == h.project).map(|x| x.node_id(h.g)).unwrap_or_default();
        let line = format!("{} {} L{}\n", id, h.kind.as_str(), h.line);
        blocks.push(Block { file: h.file.clone(), tokens: tokens_of(&line) });
        if !seen.contains(&h.file.as_str()) {
            seen.push(&h.file);
            if seen.len() >= RANK_CUTOFF {
                break;
            }
        }
    }
    // Sortie réelle : `cortex find -b 2000` (rôles des premiers résultats compris).
    let full_tokens = tokens_of(&crate::outils::executer(handles, &crate::outils::Appel::Find { question: q.to_string() }, Some(2000)));
    Output { files, header_tokens: 0, blocks, full_tokens }
}

/// Expérience « fusion » : RRF k=60 de deux classements de fichiers.
fn fused_files_output(a: Vec<String>, b: Vec<String>) -> Output {
    let files: Vec<String> = metriques::rrf_fuse(&[a, b], metriques::RRF_K).into_iter().map(|(f, _)| f).take(RANK_CUTOFF).collect();
    files_output(files)
}

/// Expérience « re-classement » : le top-20 de `base` est ré-ordonné par RRF
/// entre son rang et le rang (restreint à ces 20) dans `other`. Ne peut PAS
/// faire entrer un fichier absent du top-20 de `base`.
fn rerank_files_output(base: Vec<String>, other: Vec<String>) -> Output {
    let restricted: Vec<String> = other.into_iter().filter(|f| base.contains(f)).collect();
    files_output(metriques::rrf_fuse(&[base, restricted], metriques::RRF_K).into_iter().map(|(f, _)| f).collect())
}

fn files_output(files: Vec<String>) -> Output {
    let blocks = path_blocks(&files);
    let full = blocks.iter().map(|b| b.tokens).sum();
    Output { files, header_tokens: 0, blocks, full_tokens: full }
}

fn dir_or_file_size(p: &std::path::Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

/// Mesure la construction COMPLÈTE d'un atlas Cortex (parcours + analyse
/// tree-sitter + construction + écriture du segment), dans un fichier
/// temporaire : l'atlas réel n'est pas touché.
fn measure_cortex_build(project: &str, root: &str) -> Build {
    let t = Instant::now();
    let Ok((idx, _)) = crate::index::build_index(project, std::path::Path::new(root)) else {
        return Build { secs: f64::NAN, bytes: 0, note: "échec de l'indexation".into() };
    };
    let seg = crate::atlas::build::build_full(&idx);
    let tmp = std::env::temp_dir().join(format!("cortex-bench-compare-{}.atlas", std::process::id()));
    let ok = crate::atlas::segment::write_segment(&tmp, &seg).is_ok();
    let secs = t.elapsed().as_secs_f64();
    let bytes = dir_or_file_size(&tmp);
    let _ = std::fs::remove_file(&tmp);
    Build { secs, bytes, note: if ok { "atlas complet (rayon, 16 threads), segment rkyv".into() } else { "écriture échouée".into() } }
}

fn fmt_bytes(b: u64) -> String {
    if b >= 1 << 30 {
        format!("{:.2} Gio", b as f64 / (1u64 << 30) as f64)
    } else {
        format!("{:.1} Mio", b as f64 / (1u64 << 20) as f64)
    }
}

/// Date civile (UTC) d'un nombre de jours depuis 1970-01-01 (H. Hinnant).
pub(crate) fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + if m <= 2 { 1 } else { 0 }, m, d)
}

pub(crate) fn git_head() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "inconnu".into())
}

pub struct Options {
    pub measure_cortex_build: bool,
    pub out_dir: std::path::PathBuf,
}

/// Point d'entrée de `cortex bench-compare`.
pub fn run(bench: &BenchFile, handles: &[crate::atlas::Handle], opts: &Options) {
    let h0 = &handles[0];
    let root = h0.root().to_string();
    let paths: Vec<&str> = h0.file_paths();
    println!("== Banc comparatif — {} · {} questions · {} fichiers ==", h0.project, bench.queries.len(), paths.len());

    // ── Corpus commun ─────────────────────────────────────────────────────
    let t = Instant::now();
    let corpus: Corpus = corpus::load(&root, &paths);
    let load_s = t.elapsed().as_secs_f64();
    let lines: usize = corpus.docs.iter().map(|d| d.n_lines()).sum();
    println!("corpus relu : {} fichiers, {} lignes, {} en {:.2}s", corpus.docs.len(), lines, fmt_bytes(corpus.bytes as u64), load_s);

    // ── BM25 fichiers ─────────────────────────────────────────────────────
    let t = Instant::now();
    let bm25_files = Bm25::build(corpus.docs.len(), |i| {
        let d = &corpus.docs[i];
        format!("{}\n{}", d.path, d.text)
    });
    let bm25_files_s = t.elapsed().as_secs_f64();

    // ── Dense, hybride et expériences (feature `bench`) ───────────────────
    #[cfg(feature = "bench")]
    let dense_parts = build_dense_parts(&h0.project, &corpus, load_s);

    let mut approaches: Vec<Approach> = Vec::new();

    // ── rg ────────────────────────────────────────────────────────────────
    let rg_build = || Build {
        secs: load_s,
        bytes: (corpus.bytes * 2) as u64,
        note: "aucun index : texte + minuscules en RAM (≈ cache disque chaud de rg)".into(),
    };
    for (name, rule) in [("rg-compte", RgRule::Count), ("rg-termes", RgRule::Terms)] {
        let c = &corpus;
        approaches.push(Approach {
            name,
            build: rg_build(),
            run: Box::new(move |q: &str| {
                let words = corpus::query_rg_words(q);
                let ranked = lexical::rg_rank(c, lexical::rg_scan(c, &words), rule);
                let (blocks, full) = lexical::rg_blocks(c, &ranked, RANK_CUTOFF);
                let files = blocks.iter().map(|b| b.file.clone()).collect();
                Output { files, header_tokens: 0, blocks, full_tokens: full }
            }),
        });
    }

    // ── BM25 fichiers ─────────────────────────────────────────────────────
    {
        let (c, idx) = (&corpus, &bm25_files);
        approaches.push(Approach {
            name: "bm25-fich",
            build: Build {
                secs: load_s + bm25_files_s,
                bytes: idx.approx_bytes() as u64,
                note: "index inversé en RAM (taille estimée)".into(),
            },
            run: Box::new(move |q: &str| {
                let terms = corpus::query_lex_terms(q);
                let files: Vec<String> =
                    idx.search(&terms, RANK_CUTOFF).into_iter().map(|(d, _)| c.docs[d as usize].path.clone()).collect();
                let blocks = path_blocks(&files);
                let full = blocks.iter().map(|b| b.tokens).sum();
                Output { files, header_tokens: 0, blocks, full_tokens: full }
            }),
        });
    }

    // ── Dense, hybride et expériences (feature `bench`) ───────────────────
    #[cfg(feature = "bench")]
    if let Some(p) = &dense_parts {
        push_dense_approaches(&mut approaches, p, &corpus, handles);
    }
    #[cfg(not(feature = "bench"))]
    println!("(rag-dense et hybride non compilés : relance avec `cargo build --release --features bench`)");

    // ── Cortex ────────────────────────────────────────────────────────────
    let cortex_build = if opts.measure_cortex_build {
        println!("mesure de la construction d'un atlas Cortex complet…");
        measure_cortex_build(&h0.project, &root)
    } else {
        Build { secs: f64::NAN, bytes: 0, note: "non mesurée (--sans-construction)".into() }
    };
    approaches.insert(
        approaches.len().min(3 + if cfg!(feature = "bench") { 2 } else { 0 }),
        Approach { name: "cortex", build: cortex_build, run: Box::new(move |q: &str| cortex_output(handles, q, RANK_CUTOFF)) },
    );
    // Expériences d'analyse (hors moteur) : Cortex + BM25 sur le CONTENU des fichiers.
    {
        let (c, idx) = (&corpus, &bm25_files);
        let bm25_order = move |q: &str, n: usize| -> Vec<String> {
            idx.search(&corpus::query_lex_terms(q), n).into_iter().map(|(d, _)| c.docs[d as usize].path.clone()).collect()
        };
        approaches.push(Approach {
            name: "exp:cortex+bm25",
            build: Build { secs: f64::NAN, bytes: 0, note: "RRF k=60 : 100 fichiers Cortex + 100 fichiers BM25 (contenu)".into() },
            run: Box::new(move |q: &str| fused_files_output(cortex_output(handles, q, 100).files, bm25_order(q, 100))),
        });
        approaches.push(Approach {
            name: "exp:rerank20-bm25",
            build: Build { secs: f64::NAN, bytes: 0, note: "top-20 Cortex re-classé par RRF(rang Cortex, rang BM25 contenu)".into() },
            run: Box::new(move |q: &str| rerank_files_output(cortex_output(handles, q, RANK_CUTOFF).files, bm25_order(q, 2000))),
        });
    }

    // ── Passes : échauffement puis mesure ─────────────────────────────────
    let mut all: Vec<(String, Build, Vec<QResult>)> = Vec::new();
    for a in approaches {
        for q in &bench.queries {
            let _ = (a.run)(&q.q);
        }
        let mut rs = Vec::new();
        for q in &bench.queries {
            let t = Instant::now();
            let out = (a.run)(&q.q);
            let latency_ms = t.elapsed().as_secs_f64() * 1000.0;
            let rank = metriques::rank_of(&out.files, &q.expect);
            rs.push(QResult {
                rank,
                tokens_first_good: rank.and_then(|_| tokens_to_first_good(out.header_tokens, &out.blocks, &q.expect)),
                tokens_top5: tokens_for_top_k(out.header_tokens, &out.blocks, 5),
                tokens_full: out.full_tokens,
                latency_ms,
                top_files: out.files.iter().take(5).cloned().collect(),
            });
        }
        all.push((a.name.to_string(), a.build, rs));
    }
    report(bench, &all, &h0.project, corpus.docs.len(), lines, opts);
}

#[cfg(feature = "bench")]
struct DenseParts {
    chunks: Vec<corpus::Chunk>,
    chunk_tokens: Vec<usize>,
    chunk_s: f64,
    dense: dense::Dense,
    bm25_chunks: Bm25,
    bm25_chunks_s: f64,
    load_s: f64,
}

#[cfg(feature = "bench")]
fn build_dense_parts(project: &str, corpus: &Corpus, load_s: f64) -> Option<DenseParts> {
    use rayon::prelude::*;
    let t = Instant::now();
    let chunks = corpus::chunks(corpus);
    let chunk_tokens: Vec<usize> = chunks.par_iter().map(|c| tokens_of(&corpus::chunk_text(corpus, c))).collect();
    let chunk_s = t.elapsed().as_secs_f64();
    println!("morceaux : {} ({} lignes, recouvrement {})", chunks.len(), corpus::CHUNK_LINES, corpus::CHUNK_OVERLAP);
    let dense = match dense::Dense::build(project, corpus, &chunks) {
        Ok(d) => d,
        Err(e) => {
            println!("rag-dense et hybride IMPOSSIBLES : {e}");
            return None;
        }
    };
    println!(
        "dense : dim {}, {} morceaux du cache, {} calculés ; modèle chargé en {:.2}s",
        dense.dim, dense.from_cache, dense.computed, dense.model_load_s
    );
    let t = Instant::now();
    let bm25_chunks = Bm25::build(chunks.len(), |i| corpus::chunk_text(corpus, &chunks[i]));
    let bm25_chunks_s = t.elapsed().as_secs_f64();
    Some(DenseParts { chunks, chunk_tokens, chunk_s, dense, bm25_chunks, bm25_chunks_s, load_s })
}

/// Sortie « morceaux » (RAG, hybride) : un bloc par morceau, jusqu'à couvrir
/// RANK_CUTOFF fichiers ; sortie par défaut = les RAG_K premiers morceaux.
#[cfg(feature = "bench")]
fn chunk_output(p: &DenseParts, corpus: &Corpus, order: &[u32]) -> Output {
    let mut blocks = Vec::new();
    let mut files: Vec<String> = Vec::new();
    for &ci in order {
        let c = &p.chunks[ci as usize];
        let path = &corpus.docs[c.doc as usize].path;
        blocks.push(Block { file: path.clone(), tokens: p.chunk_tokens[ci as usize] });
        if !files.contains(path) {
            files.push(path.clone());
            if files.len() >= RANK_CUTOFF {
                break;
            }
        }
    }
    let full = order.iter().take(RAG_K).map(|&ci| p.chunk_tokens[ci as usize]).sum();
    Output { files, header_tokens: 0, blocks, full_tokens: full }
}

#[cfg(feature = "bench")]
fn dense_file_order(p: &DenseParts, corpus: &Corpus, q: &str, limit: usize) -> Vec<String> {
    let hits = p.dense.search(q, CHUNK_POOL);
    distinct_files(hits.iter().map(|(ci, _)| corpus.docs[p.chunks[*ci as usize].doc as usize].path.as_str()), limit)
}

#[cfg(feature = "bench")]
fn push_dense_approaches<'a>(
    approaches: &mut Vec<Approach<'a>>,
    p: &'a DenseParts,
    corpus: &'a Corpus,
    handles: &'a [crate::atlas::Handle],
) {
    let dense_build = Build {
        secs: p.load_s + p.chunk_s + p.dense.model_load_s + p.dense.embed_cold_s,
        bytes: p.dense.cache_bytes + p.dense.model_bytes,
        note: format!(
            "{} morceaux ; vecteurs {} + modèle {} ; embedding à froid {:.1}s (rayon)",
            p.chunks.len(),
            fmt_bytes(p.dense.cache_bytes),
            fmt_bytes(p.dense.model_bytes),
            p.dense.embed_cold_s
        ),
    };
    approaches.push(Approach {
        name: "rag-dense",
        build: dense_build,
        run: Box::new(move |q: &str| {
            let order: Vec<u32> = p.dense.search(q, CHUNK_POOL).into_iter().map(|(c, _)| c).collect();
            chunk_output(p, corpus, &order)
        }),
    });
    approaches.push(Approach {
        name: "hybride",
        build: Build {
            secs: p.load_s + p.chunk_s + p.dense.model_load_s + p.dense.embed_cold_s + p.bm25_chunks_s,
            bytes: p.dense.cache_bytes + p.dense.model_bytes + p.bm25_chunks.approx_bytes() as u64,
            note: "dense + BM25 sur les mêmes morceaux, RRF k=60 sur les 3000 premiers de chaque".into(),
        },
        run: Box::new(move |q: &str| {
            let d: Vec<u32> = p.dense.search(q, CHUNK_POOL).into_iter().map(|(c, _)| c).collect();
            let b: Vec<u32> = p.bm25_chunks.search(&corpus::query_lex_terms(q), CHUNK_POOL).into_iter().map(|(c, _)| c).collect();
            let order: Vec<u32> = metriques::rrf_fuse(&[b, d], metriques::RRF_K).into_iter().map(|(c, _)| c).collect();
            chunk_output(p, corpus, &order)
        }),
    });
    // Expériences d'analyse (hors moteur) : ce que gagnerait Cortex en empruntant au dense.
    approaches.push(Approach {
        name: "exp:cortex+dense",
        build: Build { secs: f64::NAN, bytes: 0, note: "RRF k=60 : 100 fichiers Cortex + 100 fichiers dense".into() },
        run: Box::new(move |q: &str| fused_files_output(cortex_output(handles, q, 100).files, dense_file_order(p, corpus, q, 100))),
    });
    approaches.push(Approach {
        name: "exp:rerank20-dense",
        build: Build { secs: f64::NAN, bytes: 0, note: "top-20 Cortex re-classé par RRF(rang Cortex, rang dense)".into() },
        run: Box::new(move |q: &str| {
            rerank_files_output(cortex_output(handles, q, RANK_CUTOFF).files, dense_file_order(p, corpus, q, 400))
        }),
    });
}

fn report(bench: &BenchFile, all: &[(String, Build, Vec<QResult>)], project: &str, n_files: usize, n_lines: usize, opts: &Options) {
    let cats = ["global", "fr", "en", "tech"];
    let sub = |rs: &[QResult], cat: &str| -> Summary {
        let v: Vec<&QResult> = rs.iter().zip(&bench.queries).filter(|(_, q)| cat == "global" || q.tag == cat).map(|(r, _)| r).collect();
        summarize(&v)
    };
    let n = |c: &str| bench.queries.iter().filter(|q| c == "global" || q.tag == c).count();

    println!("\n── Global ({} questions) ─ tokens = caractères/4 ; « 1er bon » = médiane sur les trouvées ; « moy. » = échec compté comme la sortie complète", n("global"));
    println!(
        "{:<20} {:>6} {:>6} {:>6} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}",
        "approche", "top-1", "top-5", "MRR", "lat méd", "lat p95", "1er bon", "moy.", "top-5", "sortie"
    );
    for (name, _, rs) in all {
        let s = sub(rs, "global");
        println!(
            "{:<20} {:>5.1}% {:>5.1}% {:>6.3} {:>6.2}ms {:>6.2}ms {:>8.0} {:>8.0} {:>8.0} {:>8.0}",
            name,
            s.top1 * 100.0,
            s.top5 * 100.0,
            s.mrr,
            s.lat_median_ms,
            s.lat_p95_ms,
            s.tok_first_good_median,
            s.tok_first_good_mean_penalized,
            s.tok_top5_median,
            s.tok_full_median
        );
    }
    println!("\n── Par catégorie : top-1 / top-5 / MRR");
    print!("{:<20}", "approche");
    for c in &cats[1..] {
        print!(" {:>22}", format!("{} (n={})", c, n(c)));
    }
    println!();
    for (name, _, rs) in all {
        print!("{:<20}", name);
        for c in &cats[1..] {
            let s = sub(rs, c);
            print!(" {:>22}", format!("{:.0}% / {:.0}% / {:.3}", s.top1 * 100.0, s.top5 * 100.0, s.mrr));
        }
        println!();
    }
    println!("\n── Construction de l'index");
    for (name, b, _) in all {
        let secs = if b.secs.is_nan() { "—".to_string() } else { format!("{:.2}s", b.secs) };
        let size = if b.bytes == 0 { "—".to_string() } else { fmt_bytes(b.bytes) };
        println!("{:<20} {:>9} {:>11}  {}", name, secs, size, b.note);
    }
    println!("\n── Rang du bon fichier par question (- = absent du top-20)");
    print!("{:>4} {:<5}", "#", "tag");
    for (i, _) in all.iter().enumerate() {
        print!(" {:>3}", format!("a{}", i + 1));
    }
    println!("  question");
    for (qi, q) in bench.queries.iter().enumerate() {
        print!("{:>4} {:<5}", qi + 1, q.tag);
        for (_, _, rs) in all {
            print!(" {:>3}", rs[qi].rank.map(|k| k.to_string()).unwrap_or_else(|| "-".into()));
        }
        println!("  {}", q.q);
    }
    let legend: Vec<String> = all.iter().enumerate().map(|(i, (n, _, _))| format!("a{}={}", i + 1, n)).collect();
    println!("      {}", legend.join("  "));

    // ── JSON ──────────────────────────────────────────────────────────────
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (y, m, d) = civil((now / 86_400) as i64);
    let commit = git_head();
    let approaches: Vec<JsonApproach> = all
        .iter()
        .map(|(name, b, rs)| JsonApproach {
            name: name.clone(),
            build_secs: if b.secs.is_nan() { -1.0 } else { b.secs },
            build_bytes: b.bytes,
            build_note: b.note.clone(),
            summary: cats.iter().map(|c| (c.to_string(), sub(rs, c))).collect(),
            questions: rs
                .iter()
                .zip(&bench.queries)
                .map(|(r, q)| JsonQuestion { q: q.q.clone(), tag: q.tag.clone(), r: r.clone() })
                .collect(),
        })
        .collect();
    let json = serde_json::json!({
        "date": format!("{y:04}-{m:02}-{d:02}"),
        "cortex_commit": commit,
        "project": project,
        "files": n_files,
        "lines": n_lines,
        "method": {
            "judge": "rang du premier fichier attendu parmi les fichiers distincts, coupé au rang 20 (identique à cortex bench)",
            "tokens": "caractères Unicode / 4, arrondi au supérieur, identique pour toutes les approches",
            "rank_cutoff": RANK_CUTOFF,
            "chunk_lines": corpus::CHUNK_LINES,
            "chunk_overlap": corpus::CHUNK_OVERLAP,
            "rrf_k": metriques::RRF_K,
            "bm25": {"k1": lexical::BM25_K1, "b": lexical::BM25_B},
            "rag_k": RAG_K,
        },
        "approaches": approaches,
    });
    let _ = std::fs::create_dir_all(&opts.out_dir);
    let projet: String = project.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    let out = opts.out_dir.join(format!("comparatif-{projet}-{y:04}-{m:02}-{d:02}-{commit}.json"));
    match std::fs::write(&out, serde_json::to_string_pretty(&json).unwrap_or_default()) {
        Ok(()) => println!("\nrésultats : {}", out.display()),
        Err(e) => eprintln!("cortex: écriture de {} impossible : {e}", out.display()),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn date_civile() {
        assert_eq!(super::civil(0), (1970, 1, 1));
        assert_eq!(super::civil(20_722), (2026, 9, 26));
    }
}
