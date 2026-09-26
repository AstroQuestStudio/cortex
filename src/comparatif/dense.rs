//! RAG dense classique (feature cargo `bench`) : morceaux de 40 lignes
//! (recouvrement 10), embeddings statiques `model2vec-rs` +
//! `minishlab/potion-multilingual-128M` (MIT, pur Rust), k plus proches par
//! cosinus en force brute. Les vecteurs sont mis en cache sur disque, clés par
//! empreinte blake3 du texte du morceau : une passe suivante ne ré-embarque
//! que les morceaux changés.

use super::corpus::{chunk_text, Chunk, Corpus};
use model2vec_rs::model::StaticModel;
use rayon::prelude::*;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Instant;

pub const MODEL_ID: &str = "minishlab/potion-multilingual-128M";
const MAGIC: &[u8; 8] = b"CXDENSE1";

pub struct Dense {
    model: StaticModel,
    pub dim: usize,
    /// Vecteurs normalisés L2, n × dim, dans l'ordre des morceaux.
    vecs: Vec<f32>,
    pub model_bytes: u64,
    pub model_load_s: f64,
    /// Temps d'embedding à froid de tous les morceaux (mesuré, ou extrapolé du
    /// débit mesuré lors du dernier calcul si tout venait du cache).
    pub embed_cold_s: f64,
    pub cache_bytes: u64,
    pub from_cache: usize,
    pub computed: usize,
}

/// Dossier du modèle : `CORTEX_BENCH_MODEL`, sinon `~/.cortex/models/potion-multilingual-128M`
/// (config.json, tokenizer.json, model.safetensors téléchargés depuis Hugging Face).
pub fn model_dir() -> PathBuf {
    std::env::var("CORTEX_BENCH_MODEL")
        .map(PathBuf::from)
        .unwrap_or_else(|_| crate::index::cortex_home().join("models").join("potion-multilingual-128M"))
}

fn cache_path(project: &str) -> PathBuf {
    crate::index::cortex_home().join("bench-cache").join(format!(
        "{}-potion-multilingual-128M-w{}o{}.bin",
        project,
        super::corpus::CHUNK_LINES,
        super::corpus::CHUNK_OVERLAP
    ))
}

fn normalize(v: &mut [f32]) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
}

struct Cache {
    dim: usize,
    secs_per_chunk: f64,
    map: HashMap<[u8; 32], Vec<f32>>,
}

fn read_cache(p: &PathBuf) -> Option<Cache> {
    let mut f = std::fs::File::open(p).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    if buf.len() < 28 || &buf[..8] != MAGIC {
        return None;
    }
    let dim = u32::from_le_bytes(buf[8..12].try_into().ok()?) as usize;
    let n = u64::from_le_bytes(buf[12..20].try_into().ok()?) as usize;
    let secs_per_chunk = f64::from_le_bytes(buf[20..28].try_into().ok()?);
    let rec = 32 + dim * 4;
    if buf.len() != 28 + n * rec {
        return None;
    }
    let mut map = HashMap::with_capacity(n);
    for i in 0..n {
        let o = 28 + i * rec;
        let key: [u8; 32] = buf[o..o + 32].try_into().ok()?;
        let v: Vec<f32> = buf[o + 32..o + rec].chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
        map.insert(key, v);
    }
    Some(Cache { dim, secs_per_chunk, map })
}

fn write_cache(p: &PathBuf, dim: usize, secs_per_chunk: f64, keys: &[[u8; 32]], vecs: &[f32]) -> std::io::Result<u64> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let mut out = Vec::with_capacity(28 + keys.len() * (32 + dim * 4));
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(dim as u32).to_le_bytes());
    out.extend_from_slice(&(keys.len() as u64).to_le_bytes());
    out.extend_from_slice(&secs_per_chunk.to_le_bytes());
    for (i, k) in keys.iter().enumerate() {
        out.extend_from_slice(k);
        for x in &vecs[i * dim..(i + 1) * dim] {
            out.extend_from_slice(&x.to_le_bytes());
        }
    }
    let tmp = p.with_extension("tmp");
    std::fs::File::create(&tmp)?.write_all(&out)?;
    std::fs::rename(&tmp, p)?;
    Ok(out.len() as u64)
}

impl Dense {
    /// Charge le modèle et embarque tous les morceaux (cache disque d'abord).
    pub fn build(project: &str, corpus: &Corpus, chunks: &[Chunk]) -> Result<Dense, String> {
        let dir = model_dir();
        let model_bytes = std::fs::metadata(dir.join("model.safetensors")).map(|m| m.len()).unwrap_or(0)
            + std::fs::metadata(dir.join("tokenizer.json")).map(|m| m.len()).unwrap_or(0);
        let t = Instant::now();
        let model = StaticModel::from_pretrained(&dir, None, Some(true), None).map_err(|e| {
            format!(
                "modèle {} introuvable dans {} ({e}) — voir docs/BENCHMARKS.md ; CORTEX_BENCH_MODEL = dossier du modèle",
                MODEL_ID,
                dir.display()
            )
        })?;
        let model_load_s = t.elapsed().as_secs_f64();
        let dim = model.encode_single("dimension").len();

        let texts: Vec<String> = chunks.par_iter().map(|c| chunk_text(corpus, c)).collect();
        let keys: Vec<[u8; 32]> = texts.par_iter().map(|s| *blake3::hash(s.as_bytes()).as_bytes()).collect();
        let cp = cache_path(project);
        let cache = read_cache(&cp).filter(|c| c.dim == dim);
        let mut vecs = vec![0f32; chunks.len() * dim];
        let mut missing: Vec<usize> = Vec::new();
        for (i, k) in keys.iter().enumerate() {
            match cache.as_ref().and_then(|c| c.map.get(k)) {
                Some(v) => vecs[i * dim..(i + 1) * dim].copy_from_slice(v),
                None => missing.push(i),
            }
        }
        let computed = missing.len();
        let mut secs_per_chunk = cache.as_ref().map(|c| c.secs_per_chunk).unwrap_or(0.0);
        if !missing.is_empty() {
            let t = Instant::now();
            // Embedding multithread : lots de 256 morceaux répartis sur rayon.
            let batches: Vec<Vec<usize>> = missing.chunks(256).map(|b| b.to_vec()).collect();
            let out: Vec<(usize, Vec<f32>)> = batches
                .par_iter()
                .flat_map_iter(|b| {
                    let sents: Vec<String> = b.iter().map(|&i| texts[i].clone()).collect();
                    let embs = model.encode_with_args(&sents, Some(512), 256);
                    b.iter().copied().zip(embs).collect::<Vec<_>>()
                })
                .collect();
            for (i, mut v) in out {
                normalize(&mut v);
                vecs[i * dim..(i + 1) * dim].copy_from_slice(&v);
            }
            let el = t.elapsed().as_secs_f64();
            if computed >= 1000 {
                secs_per_chunk = el / computed as f64;
            }
        }
        let cache_bytes = if computed > 0 || cache.is_none() {
            write_cache(&cp, dim, secs_per_chunk, &keys, &vecs).map_err(|e| format!("cache {}: {e}", cp.display()))?
        } else {
            std::fs::metadata(&cp).map(|m| m.len()).unwrap_or(0)
        };
        Ok(Dense {
            model,
            dim,
            vecs,
            model_bytes,
            model_load_s,
            embed_cold_s: secs_per_chunk * chunks.len() as f64,
            cache_bytes,
            from_cache: chunks.len() - computed,
            computed,
        })
    }

    /// Les `limit` morceaux les plus proches de la question (cosinus).
    pub fn search(&self, question: &str, limit: usize) -> Vec<(u32, f32)> {
        let mut q = self.model.encode_single(question);
        normalize(&mut q);
        let n = self.vecs.len() / self.dim;
        let mut scores: Vec<(u32, f32)> = (0..n)
            .map(|i| {
                let v = &self.vecs[i * self.dim..(i + 1) * self.dim];
                (i as u32, v.iter().zip(&q).map(|(a, b)| a * b).sum::<f32>())
            })
            .collect();
        let cmp = |a: &(u32, f32), b: &(u32, f32)| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0));
        if scores.len() > limit {
            scores.select_nth_unstable_by(limit, cmp);
            scores.truncate(limit);
        }
        scores.sort_by(cmp);
        scores
    }
}
