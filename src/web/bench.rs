//! Cortex Web : le banc de recherche documentaire (lot W-F2, feature `web`).
//!
//! `cortex web bench <corpus.json> <questions.json> [--sources a,b] [--skip-crawl] [--home D]` :
//!
//! 1. **Collecte** (sauf `--skip-crawl`) des sources du corpus dans un `CORTEX_HOME` isolé (`--home`,
//!    obligatoire pour collecter) : deux passes (la 2e doit tout recevoir en 304), durée, pages/s
//!    réseau réel, RAM de crête du processus (échantillonnée par `GetProcessMemoryInfo`), sections
//!    vides, pages hors portée. Les hôtes sont servis un à la fois ; au plus `--parallel` hôtes à la fois.
//! 2. **Base A** (sauf `--skip-base`) : l'existant `cortex docs add` + `cortex docs query`
//!    (`src/scrape.rs`, inchangé), lancé comme processus fils avec le même `CORTEX_HOME` ; les
//!    fichiers `.md` renvoyés sont rapportés à leur URL par `_manifest.tsv` (niveau PAGE seulement).
//! 3. **Questions** : rang de la 1re réponse correcte au niveau PAGE (URL normalisée) et SECTION
//!    (URL#ancre), version correcte, jetons des cartes, temps (médiane de `--repeat` essais),
//!    agrégats (top-1, top-5, MRR ; par langue, étiquette, source), mode « toutes sources ».
//! 4. **Rien de fiable** : les questions sans réponse (`expect: []`) mesurent l'abstention. Le seuil
//!    de confiance se calibre UNIQUEMENT sur un fichier de dev (`--calibrate`, refusé sur un fichier
//!    dont le nom contient « test ») ; la valeur trouvée se rejoue ensuite avec `--threshold`.
//!    `Index::query` rend toujours des résultats classés : l'abstention est décidée ici, sur deux
//!    traits du premier résultat (score BM25F brut ; couverture des termes de la question pondérée
//!    par leur rareté dans la source), sans modifier `index.rs`.
//!
//! Portée du corpus : `crawl` ne connaît qu'une graine + un préfixe de chemin. Le banc traduit
//! `include_paths` en graines supplémentaires (+ profondeur 1), applique `include_paths` et
//! `exclude_paths` APRÈS coup (une page hors portée est conservée dans le magasin mais écartée des
//! résultats, côté Cortex Web comme côté base A) et arrête la collecte (`cancel`) quand le nombre de
//! pages EN portée atteint `max_pages` du corpus (plafond dur de 150 pages au total). Pour les sources
//! à portée restreinte (include/exclude) la collecte se fait SANS sitemap (types XML refusés par la
//! politique) : constaté sur vitepress, le sitemap aspire sinon les 7 langues exclues. La 2e passe est
//! une revalidation pure (état de reprise effacé, sans sitemap, budget = pages connues + 10).
//!
//! Aucun réseau hors collecte ; aucun appel d'un modèle.

use super::cli::Ctx;
use super::crawl::{self, CrawlOpts, CrawlProgress, CrawlReport};
use super::guard::Fetcher;
use super::index::{self, Filter, Hit, Index};
use super::store::{sanitize_source, Store};
use super::{Completeness, Policy, SourceConfig};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use url::Url;

/// Plafond dur de pages (toutes pages comprises) par source.
const HARD_CAP_PAGES: u32 = 150;
/// Résultats gardés par question.
const KEEP: usize = 20;
/// Résultats demandés à chaque index avant filtrage de portée.
const RAW_LIMIT: usize = 60;
/// Budget des cartes (jetons) : celui de `cortex web find`.
const CARDS_BUDGET: usize = 600;
/// Cartes comptées pour « jetons de la réponse ».
const CARDS_SHOWN: usize = 5;

// ───────────────────────── Arguments ─────────────────────────

#[derive(clap::Args, Debug, Clone)]
pub struct BenchArgs {
    /// Corpus (bench/web/corpus.json).
    pub corpus: PathBuf,
    /// Questions (bench/web/questions-dev.json ou questions-test.json).
    pub questions: PathBuf,
    /// Sources du corpus à mesurer (noms séparés par des virgules ; défaut : toutes).
    #[arg(long)]
    pub sources: Option<String>,
    /// Ne collecte rien : rejoue les questions sur les magasins déjà présents dans --home.
    #[arg(long)]
    pub skip_crawl: bool,
    /// CORTEX_HOME isolé du banc (obligatoire pour collecter ; défaut : CORTEX_HOME de l'environnement).
    #[arg(long)]
    pub home: Option<PathBuf>,
    /// Hôtes collectés en parallèle (jamais deux collectes sur le même hôte).
    #[arg(long, default_value_t = 3)]
    pub parallel: usize,
    /// Saute la comparaison avec `cortex docs add` + `cortex docs query` (base A).
    #[arg(long)]
    pub skip_base: bool,
    /// Seuil de confiance : sous ce seuil, « rien de fiable » (valeur calibrée sur le dev).
    #[arg(long)]
    pub threshold: Option<f32>,
    /// Calibre le seuil sur CE fichier de questions (dev seulement : refusé si le nom contient « test »).
    #[arg(long)]
    pub calibrate: bool,
    /// Trait de confiance : `conf` (`Hit::confidence`, calculée par l'index : défaut), `wcov` (couverture
    /// pondérée par la rareté, 0..1) ou `score` (BM25F brut).
    #[arg(long, default_value = "conf")]
    pub feature: String,
    /// Désactive le choix automatique de la version (comme `cortex web find --any-version`).
    #[arg(long)]
    pub any_version: bool,
    /// Rejoue l'EXTRACTION (sans réseau) des pages copiées par `CORTEX_WEB_DUMP_RAW=<dossier>` pendant une
    /// collecte, dans les magasins de --home, avant les questions (mise au point de l'extracteur).
    #[arg(long)]
    pub reextract: Option<PathBuf>,
    /// Essais par question pour le temps (médiane).
    #[arg(long, default_value_t = 3)]
    pub repeat: usize,
    /// Dossier de sortie (rapports .txt et .json).
    #[arg(long)]
    pub out: Option<PathBuf>,
}

// ───────────────────────── Mémoire du processus ─────────────────────────

#[cfg(windows)]
mod mem {
    #[repr(C)]
    struct Counters {
        cb: u32,
        page_faults: u32,
        peak_ws: usize,
        ws: usize,
        quota_peak_paged: usize,
        quota_paged: usize,
        quota_peak_nonpaged: usize,
        quota_nonpaged: usize,
        pagefile: usize,
        peak_pagefile: usize,
    }
    #[link(name = "psapi")]
    unsafe extern "system" {
        fn GetProcessMemoryInfo(process: isize, counters: *mut Counters, cb: u32) -> i32;
    }
    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
    }
    /// (jeu de travail courant, crête du jeu de travail) du processus, en octets.
    pub fn usage() -> (u64, u64) {
        let mut c = Counters {
            cb: std::mem::size_of::<Counters>() as u32,
            page_faults: 0,
            peak_ws: 0,
            ws: 0,
            quota_peak_paged: 0,
            quota_paged: 0,
            quota_peak_nonpaged: 0,
            quota_nonpaged: 0,
            pagefile: 0,
            peak_pagefile: 0,
        };
        let cb = c.cb;
        // SAFETY : `c` est une structure PROCESS_MEMORY_COUNTERS valide de la taille annoncée ;
        // le pseudo-handle du processus courant n'a pas à être fermé.
        let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut c, cb) };
        if ok == 0 {
            (0, 0)
        } else {
            (c.ws as u64, c.peak_ws as u64)
        }
    }
}

#[cfg(not(windows))]
mod mem {
    /// Hors Windows : mesure non disponible (0).
    pub fn usage() -> (u64, u64) {
        (0, 0)
    }
}

/// Échantillonneur du jeu de travail (toutes les 200 ms) : la crête d'une fenêtre de temps est le
/// maximum des échantillons qu'elle contient (le processus est partagé entre les sources en cours).
struct Ram {
    epoch: Instant,
    samples: Mutex<Vec<(f64, u64)>>,
    stop: AtomicBool,
}

impl Ram {
    fn start() -> Arc<Ram> {
        let r = Arc::new(Ram { epoch: Instant::now(), samples: Mutex::new(Vec::new()), stop: AtomicBool::new(false) });
        let r2 = r.clone();
        std::thread::spawn(move || {
            while !r2.stop.load(Ordering::Relaxed) {
                let (ws, _) = mem::usage();
                if let Ok(mut g) = r2.samples.lock() {
                    g.push((r2.epoch.elapsed().as_secs_f64(), ws));
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        });
        r
    }
    fn now(&self) -> f64 {
        self.epoch.elapsed().as_secs_f64()
    }
    /// Crête (octets) des échantillons dans [a, b].
    fn window_peak(&self, a: f64, b: f64) -> u64 {
        let cur = mem::usage().0;
        let g = self.samples.lock().map(|g| g.iter().filter(|(t, _)| *t >= a && *t <= b).map(|(_, v)| *v).max().unwrap_or(0)).unwrap_or(0);
        g.max(cur)
    }
}

fn mb(b: u64) -> f64 {
    (b as f64 / (1024.0 * 1024.0) * 10.0).round() / 10.0
}

// ───────────────────────── Corpus ─────────────────────────

/// Une collecte : une version d'une source (la version principale ou une `other_versions`).
#[derive(Clone, Debug)]
struct Unit {
    /// Nom du magasin : `source` ou `source@version`.
    name: String,
    source: String,
    family: String,
    /// Graines du corpus.
    seeds: Vec<String>,
    hosts: Vec<String>,
    prefix: String,
    include: Vec<String>,
    exclude: Vec<String>,
    max_pages: u32,
    version: Option<String>,
    needs_js: bool,
}

fn str_list(v: &Value, k: &str) -> Vec<String> {
    v.get(k).and_then(|x| x.as_array()).map(|a| a.iter().filter_map(|s| s.as_str().map(|s| s.to_string())).collect()).unwrap_or_default()
}

fn load_units(corpus: &Value, only: &Option<HashSet<String>>) -> Result<Vec<Unit>, String> {
    let sources = corpus.get("sources").and_then(|s| s.as_array()).ok_or("corpus : champ `sources` absent")?;
    let mut out = Vec::new();
    for s in sources {
        let name = s.get("name").and_then(|x| x.as_str()).ok_or("corpus : source sans nom")?.to_string();
        if let Some(set) = only {
            if !set.contains(&name) {
                continue;
            }
        }
        let family = s.get("family").and_then(|x| x.as_str()).unwrap_or("generic").to_string();
        let hosts = str_list(s, "allowed_hosts");
        let exclude = str_list(s, "exclude_paths");
        let needs_js = s.get("needs_js").and_then(|x| x.as_bool()).unwrap_or(false);
        let max_pages = s.get("max_pages").and_then(|x| x.as_u64()).unwrap_or(100) as u32;
        out.push(Unit {
            name: name.clone(),
            source: name.clone(),
            family: family.clone(),
            seeds: str_list(s, "seeds"),
            hosts: hosts.clone(),
            prefix: s.get("path_prefix").and_then(|x| x.as_str()).unwrap_or("/").to_string(),
            include: str_list(s, "include_paths"),
            exclude: exclude.clone(),
            max_pages: max_pages.min(HARD_CAP_PAGES),
            version: s.get("version").and_then(|x| x.as_str()).map(|v| v.to_string()),
            needs_js,
        });
        for o in s.get("other_versions").and_then(|x| x.as_array()).into_iter().flatten() {
            let ver = o.get("version").and_then(|x| x.as_str()).unwrap_or("").to_string();
            if ver.is_empty() {
                continue;
            }
            out.push(Unit {
                name: format!("{name}@{ver}"),
                source: name.clone(),
                family: family.clone(),
                seeds: str_list(o, "seeds"),
                hosts: hosts.clone(),
                prefix: o.get("path_prefix").and_then(|x| x.as_str()).unwrap_or("/").to_string(),
                include: str_list(o, "include_paths"),
                exclude: exclude.clone(),
                max_pages: (o.get("max_pages").and_then(|x| x.as_u64()).unwrap_or(max_pages as u64) as u32).min(HARD_CAP_PAGES),
                version: Some(ver),
                needs_js,
            });
        }
    }
    if out.is_empty() {
        return Err("aucune source du corpus ne correspond à --sources".to_string());
    }
    Ok(out)
}

impl Unit {
    fn host(&self) -> String {
        self.hosts.first().cloned().unwrap_or_default()
    }

    fn prefix_ok(&self, path: &str) -> bool {
        let p = self.prefix.trim();
        if p.is_empty() || p == "/" {
            return true;
        }
        if p.ends_with('/') {
            path.starts_with(p) || path == p.trim_end_matches('/')
        } else {
            path == p || path.starts_with(&format!("{p}/"))
        }
    }

    /// Portée du banc : hôte, préfixe, `include_paths`, `exclude_paths`.
    fn in_scope(&self, url: &str) -> bool {
        let Ok(u) = Url::parse(url) else { return false };
        let Some(h) = u.host_str() else { return false };
        let h = h.to_ascii_lowercase();
        if !self.hosts.iter().any(|a| a.eq_ignore_ascii_case(&h)) {
            return false;
        }
        let path = u.path();
        if !self.prefix_ok(path) {
            return false;
        }
        if self.exclude.iter().any(|e| path.starts_with(e.as_str())) {
            return false;
        }
        self.include.is_empty() || self.include.iter().any(|i| path.starts_with(i.as_str()))
    }

    /// Configuration de collecte : tout ce que dit le corpus (graines, préfixe, `include_paths`,
    /// `exclude_paths`) est passé tel quel à `crawl`, qui l'honore à la découverte (sitemap, liens,
    /// redirections). Profondeur adaptative : jamais plus que `max_pages`.
    fn config(&self) -> SourceConfig {
        let max_pages = self.max_pages.min(HARD_CAP_PAGES);
        SourceConfig {
            name: self.name.clone(),
            seeds: self.seeds.clone(),
            allowed_hosts: self.hosts.clone(),
            path_prefix: self.prefix.clone(),
            version: self.version.clone().map(|v| v.trim().trim_start_matches('v').to_string()).filter(|v| !v.is_empty()),
            max_pages,
            max_depth: crawl::adaptive_depth(max_pages),
            max_total_bytes: 200 * 1024 * 1024,
            max_seconds: 1_800,
            include_paths: self.include.clone(),
            exclude_paths: self.exclude.clone(),
        }
    }
}

// ───────────────────────── Questions ─────────────────────────

#[derive(Clone, Debug)]
struct Question {
    id: String,
    q: String,
    lang: String,
    source: String,
    tag: String,
    /// (URL attendue, ancre attendue).
    expect: Vec<(String, Option<String>)>,
    needs_js: bool,
    version: Option<String>,
    trap: Option<String>,
}

fn load_questions(v: &Value, only: &Option<HashSet<String>>) -> Result<Vec<Question>, String> {
    let arr = v.as_array().ok_or("questions : un tableau JSON est attendu")?;
    let mut out = Vec::new();
    for q in arr {
        let g = |k: &str| q.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let source = g("source");
        if let Some(set) = only {
            if !set.contains(&source) {
                continue;
            }
        }
        let expect = q
            .get("expect")
            .and_then(|e| e.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|e| {
                        Some((e.get("url")?.as_str()?.to_string(), e.get("anchor").and_then(|x| x.as_str()).map(|s| s.to_string())))
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.push(Question {
            id: g("id"),
            q: g("q"),
            lang: g("lang"),
            source,
            tag: g("tag"),
            expect,
            needs_js: q.get("needs_js").and_then(|x| x.as_bool()).unwrap_or(false),
            version: q.get("version_attendue").and_then(|x| x.as_str()).map(|s| s.to_string()),
            trap: q.get("version_piege").and_then(|p| p.get("url")).and_then(|x| x.as_str()).map(|s| s.to_string()),
        });
    }
    Ok(out)
}

// ───────────────────────── Normalisation ─────────────────────────

/// Clé de comparaison d'une URL de page : hôte + chemin sans `/` final ni `index.html`, fragment ôté.
fn url_key(s: &str) -> String {
    let Ok(u) = Url::parse(s.trim()) else { return s.trim().to_ascii_lowercase() };
    let host = u.host_str().unwrap_or("").to_ascii_lowercase();
    let mut p = u.path().to_string();
    while p.len() > 1 && p.ends_with('/') {
        p.pop();
    }
    if let Some(x) = p.strip_suffix("/index.html") {
        p = x.to_string();
    }
    let mut k = format!("{host}{p}");
    if let Some(q) = u.query() {
        k.push('?');
        k.push_str(q);
    }
    k
}

/// Ancre comparable : minuscules, suffixe de doublon `~N` retiré.
fn anchor_key(a: &str) -> String {
    let a = a.trim().to_lowercase();
    match a.rsplit_once('~') {
        Some((h, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => h.to_string(),
        _ => a,
    }
}

fn id_anchor(id: &str) -> String {
    id.rsplit_once('#').map(|(_, a)| anchor_key(a)).unwrap_or_default()
}

fn median(v: &mut Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v[v.len() / 2]
}

fn tokens_of(s: &str) -> usize {
    (s.chars().count() + 3) / 4
}

fn pct(a: usize, b: usize) -> f64 {
    if b == 0 {
        0.0
    } else {
        ((a as f64 / b as f64) * 1000.0).round() / 10.0
    }
}

fn r3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

// ───────────────────────── Collecte ─────────────────────────

/// Résultat d'une passe de collecte.
struct Pass {
    report: CrawlReport,
    wall_s: f64,
    in_scope_seen: usize,
    seen: usize,
    ram_peak: u64,
}

/// Politique sans les types XML : plus de sitemap (robots.txt et llms.txt restent lisibles).
fn policy_sans_xml(p: &Policy) -> Policy {
    let mut p = p.clone();
    p.allowed_content_types.retain(|t| !(t.contains("xml") || t.contains("rss") || t.contains("atom")));
    p
}

/// `target` : nombre de pages EN portée au-delà duquel la collecte est arrêtée (`usize::MAX` : jamais).
fn run_pass(ctx: &Ctx, unit: &Unit, cfg: &SourceConfig, ram: &Ram, policy: Policy, target: usize) -> Result<Pass, String> {
    let mut st = Store::open(&ctx.root, &unit.name).map_err(|e| format!("magasin illisible : {e}"))?;
    st.save_config(cfg).map_err(|e| format!("configuration non écrite : {e}"))?;
    let seen_in: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let seen_all: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let cancel = Arc::new(AtomicBool::new(false));
    let (u2, si, sa, c2) = (unit.clone(), seen_in.clone(), seen_all.clone(), cancel.clone());
    let opts = CrawlOpts {
        force: false,
        workers: 2,
        min_delay_ms: ctx.min_delay_ms,
        progress: Some(Box::new(move |p: &CrawlProgress| {
            if p.url.is_empty() {
                return;
            }
            let k = url_key(&p.url);
            if let Ok(mut g) = sa.lock() {
                g.insert(k.clone());
            }
            if u2.in_scope(&p.url) {
                if let Ok(mut g) = si.lock() {
                    g.insert(k);
                    if g.len() >= target {
                        c2.store(true, Ordering::SeqCst);
                    }
                }
            }
        })),
        cancel: Some(cancel.clone()),
    };
    let t0 = ram.now();
    let t = Instant::now();
    let report = crawl::crawl(cfg, &Fetcher::new(policy), &mut st, &opts);
    let wall_s = t.elapsed().as_secs_f64();
    let ram_peak = ram.window_peak(t0, ram.now());
    let in_scope_seen = seen_in.lock().map(|g| g.len()).unwrap_or(0);
    let seen = seen_all.lock().map(|g| g.len()).unwrap_or(0);
    Ok(Pass { report, wall_s, in_scope_seen, seen, ram_peak })
}

/// Somme des passes d'une même unité (phases successives).
fn merge_passes(parts: Vec<Pass>) -> Pass {
    let mut r = CrawlReport::default();
    let (mut wall, mut ins, mut seen, mut ram) = (0.0f64, 0usize, 0usize, 0u64);
    for p in parts {
        let q = p.report;
        r.fetched += q.fetched;
        r.not_modified += q.not_modified;
        r.unchanged_content += q.unchanged_content;
        r.extracted += q.extracted;
        r.sections += q.sections;
        r.skipped_robots += q.skipped_robots;
        r.skipped_scope += q.skipped_scope;
        r.skipped_lastmod += q.skipped_lastmod;
        r.blocked += q.blocked;
        r.failed += q.failed;
        r.removed += q.removed;
        r.needs_js.extend(q.needs_js);
        r.injection_pages.extend(q.injection_pages);
        r.bytes += q.bytes;
        r.errors.extend(q.errors);
        if r.stopped == crawl::StopReason::Done {
            r.stopped = q.stopped;
        }
        wall += p.wall_s;
        ins += p.in_scope_seen;
        seen += p.seen;
        ram = ram.max(p.ram_peak);
    }
    r.seconds = wall as f32;
    Pass { report: r, wall_s: wall, in_scope_seen: ins, seen, ram_peak: ram }
}

fn pass_json(p: &Pass) -> Value {
    let r = &p.report;
    let net = (r.fetched + r.not_modified) as f64;
    json!({
        "recues_200": r.fetched, "inchangees_304": r.not_modified, "contenu_identique": r.unchanged_content,
        "extraites": r.extracted, "sections_ecrites": r.sections,
        "robots": r.skipped_robots, "hors_portee_crawl": r.skipped_scope, "lastmod": r.skipped_lastmod,
        "bloquees": r.blocked, "echecs": r.failed, "retirees": r.removed,
        "needs_js": r.needs_js.len(), "octets": r.bytes,
        "secondes": r3(p.wall_s), "pages_par_s": r3(if p.wall_s > 0.0 { net / p.wall_s } else { 0.0 }),
        "arret": format!("{:?}", r.stopped), "pages_en_portee_vues": p.in_scope_seen, "pages_vues": p.seen,
        "ram_crete_mo": mb(p.ram_peak),
        "erreurs": r.errors.iter().take(6).cloned().collect::<Vec<_>>(),
    })
}

/// Statistiques d'un magasin après collecte (portée du banc appliquée).
fn store_stats(ctx: &Ctx, unit: &Unit) -> Value {
    let Ok(st) = Store::open_read(&ctx.root, &unit.name) else { return json!({"erreur": "magasin absent"}) };
    let recs = st.records();
    let (mut in_p, mut out_p, mut bytes_in, mut zero) = (0usize, 0usize, 0u64, 0usize);
    let (mut c_ok, mut c_part, mut c_js, mut c_blk) = (0usize, 0usize, 0usize, 0usize);
    let mut out_examples: Vec<String> = Vec::new();
    let mut in_scope_pages: HashSet<String> = HashSet::new();
    for r in &recs {
        if unit.in_scope(&r.url) {
            in_p += 1;
            bytes_in += r.bytes;
            in_scope_pages.insert(url_key(&r.url));
            if r.sections == 0 {
                zero += 1;
            }
            match r.completeness {
                Completeness::Complete => c_ok += 1,
                Completeness::Partial => c_part += 1,
                Completeness::NeedsJs => c_js += 1,
                Completeness::Blocked => c_blk += 1,
            }
        } else {
            out_p += 1;
            if out_examples.len() < 5 {
                out_examples.push(r.url.clone());
            }
        }
    }
    let (mut secs_in, mut empty, mut chars) = (0usize, 0usize, 0usize);
    for s in st.sections() {
        if in_scope_pages.contains(&url_key(&s.page_url)) {
            secs_in += 1;
            let n = s.text.trim().chars().count();
            chars += n;
            if n < 40 {
                empty += 1;
            }
        }
    }
    json!({
        "pages_magasin": recs.len(), "pages_en_portee": in_p, "pages_hors_portee": out_p, "exemples_hors_portee": out_examples,
        "octets_en_portee": bytes_in, "sections_en_portee": secs_in, "sections_vides_lt40": empty,
        "pages_sans_section": zero, "caracteres_sections": chars,
        "completes": c_ok, "partielles": c_part, "needs_js": c_js, "bloquees": c_blk,
    })
}

/// Base A : `cortex docs add` pour chaque graine du corpus, fusionné dans `docs/<unit>`.
fn run_base_crawl(exe: &Path, home: &Path, unit: &Unit) -> Value {
    let docs = home.join("docs");
    let _ = std::fs::remove_dir_all(docs.join(&unit.name));
    let n = unit.seeds.len().max(1);
    let per = ((unit.max_pages as usize + n - 1) / n).max(10);
    let t = Instant::now();
    let mut errors: Vec<String> = Vec::new();
    for (i, seed) in unit.seeds.iter().enumerate() {
        let name = format!("{}__s{}", unit.name, i);
        let _ = std::fs::remove_dir_all(docs.join(&name));
        let out = std::process::Command::new(exe)
            .args(["docs", "add", seed, "--name", &name, "--max", &per.to_string()])
            .env("CORTEX_HOME", home)
            .stdin(std::process::Stdio::null())
            .output();
        match out {
            Ok(o) if o.status.success() => {}
            Ok(o) => errors.push(format!(
                "{seed} : code {:?} {}",
                o.status.code(),
                String::from_utf8_lossy(&o.stderr).chars().take(160).collect::<String>()
            )),
            Err(e) => errors.push(format!("{seed} : {e}")),
        }
    }
    let secs = t.elapsed().as_secs_f64();
    // Fusion des dossiers de graine.
    let dst = docs.join(&unit.name);
    let _ = std::fs::create_dir_all(&dst);
    let mut manifest = String::new();
    for i in 0..unit.seeds.len() {
        let d = docs.join(format!("{}__s{}", unit.name, i));
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                let p = e.path();
                match p.file_name().and_then(|n| n.to_str()) {
                    Some("_manifest.tsv") => manifest.push_str(&std::fs::read_to_string(&p).unwrap_or_default()),
                    Some("_meta.json") => {}
                    Some(n) if n.ends_with(".md") => {
                        let _ = std::fs::copy(&p, dst.join(n));
                    }
                    _ => {}
                }
            }
        }
        let _ = std::fs::remove_dir_all(&d);
    }
    let _ = std::fs::write(dst.join("_manifest.tsv"), &manifest);
    let urls: Vec<&str> = manifest.lines().filter_map(|l| l.split('\t').nth(2)).collect();
    let in_scope = urls.iter().filter(|u| unit.in_scope(u)).count();
    json!({ "pages": urls.len(), "pages_en_portee": in_scope, "secondes": r3(secs), "max_par_graine": per, "erreurs": errors })
}

fn crawl_unit(ctx: &Ctx, exe: &Path, home: &Path, unit: &Unit, ram: &Ram, with_base: bool) -> Value {
    let cfg = unit.config();
    eprintln!(
        "[bench] {} : collecte ({} graines, plafond {} pages, profondeur {})…",
        unit.name,
        cfg.seeds.len(),
        cfg.max_pages,
        cfg.max_depth
    );
    let all_seeds: Vec<String> = cfg.seeds.clone();
    let p1: Result<Pass, String> = run_pass(ctx, unit, &cfg, ram, ctx.policy.clone(), unit.max_pages as usize);
    let n1 = Store::open_read(&ctx.root, &unit.name).map(|s| s.records().len()).unwrap_or(0);
    // 2e passe = revalidation : état de reprise effacé (sinon la file de la passe 1 reprend), pas de
    // sitemap, budget = pages connues + 10 : tout doit revenir en 304.
    let p2 = if p1.is_ok() {
        let _ = std::fs::remove_file(ctx.root.join(sanitize_source(&unit.name)).join("crawl.state"));
        let mut cfg2 = cfg.clone();
        cfg2.max_pages = (n1 + 10) as u32;
        Some(run_pass(ctx, unit, &cfg2, ram, policy_sans_xml(&ctx.policy), usize::MAX))
    } else {
        None
    };
    let mut row = json!({
        "unite": unit.name, "source": unit.source, "famille": unit.family, "version": unit.version,
        "graines": all_seeds, "phases": 1, "needs_js_corpus": unit.needs_js, "max_pages_corpus": unit.max_pages,
    });
    match &p1 {
        Ok(p) => row["passe1"] = pass_json(p),
        Err(e) => row["erreur"] = json!(e),
    }
    match &p2 {
        Some(Ok(p)) => {
            let mut j = pass_json(p);
            let tout_304 = p.report.fetched == 0 && p.report.not_modified > 0;
            j["tout_en_304"] = json!(tout_304);
            j["pages_connues_avant"] = json!(n1);
            row["passe2"] = j;
        }
        Some(Err(e)) => row["erreur2"] = json!(e),
        None => {}
    }
    row["magasin"] = store_stats(ctx, unit);
    if with_base {
        eprintln!("[bench] {} : base A (cortex docs add)…", unit.name);
        row["base_a"] = run_base_crawl(exe, home, unit);
    }
    eprintln!("[bench] {} : fini", unit.name);
    row
}

fn crawl_phase(ctx: &Ctx, exe: &Path, home: &Path, units: &[Unit], par: usize, ram: &Arc<Ram>, with_base: bool) -> Vec<Value> {
    for u in units {
        let _ = std::fs::remove_dir_all(ctx.root.join(sanitize_source(&u.name)));
    }
    let mut groups: BTreeMap<String, VecDeque<Unit>> = BTreeMap::new();
    for u in units {
        groups.entry(u.host()).or_default().push_back(u.clone());
    }
    let queue: Mutex<VecDeque<(String, VecDeque<Unit>)>> = Mutex::new(groups.into_iter().collect());
    let rows: Mutex<Vec<Value>> = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..par.clamp(1, 8) {
            s.spawn(|| loop {
                let g = queue.lock().ok().and_then(|mut q| q.pop_front());
                let Some((_, us)) = g else { break };
                for u in us {
                    let row = crawl_unit(ctx, exe, home, &u, ram, with_base);
                    if let Ok(mut r) = rows.lock() {
                        r.push(row);
                    }
                }
            });
        }
    });
    let mut v = rows.into_inner().unwrap_or_default();
    v.sort_by(|a, b| a["unite"].as_str().cmp(&b["unite"].as_str()));
    v
}

/// Rejoue l'extraction des pages copiées (`index.tsv`) dans leurs magasins : mêmes empreintes, mêmes
/// sections que `crawl`, aucun réseau.
fn reextract(ctx: &Ctx, dir: &Path) -> (usize, usize) {
    let text = std::fs::read_to_string(dir.join("index.tsv")).unwrap_or_default();
    let mut stores: HashMap<String, Store> = HashMap::new();
    let (mut ok, mut ko) = (0usize, 0usize);
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 5 {
            continue;
        }
        let (source, url, file, ct, ver) = (f[0], f[1], f[2], f[3], f[4]);
        let Ok(u) = Url::parse(url) else { continue };
        let Ok(body) = std::fs::read(dir.join(file)) else { continue };
        if !stores.contains_key(source) {
            match Store::open(&ctx.root, source) {
                Ok(s) => {
                    stores.insert(source.to_string(), s);
                }
                Err(_) => continue,
            }
        }
        let Some(st) = stores.get_mut(source) else { continue };
        let v = if ver.is_empty() { None } else { Some(ver) };
        match crawl::ingest(source, st, &u, &body, ct, v) {
            Ok(_) => ok += 1,
            Err(_) => ko += 1,
        }
    }
    (ok, ko)
}

// ───────────────────────── Données de recherche ─────────────────────────

struct UnitIdx {
    unit: Unit,
    index: Index,
    /// Pages en portée du magasin.
    pages: HashSet<String>,
    /// Ancres par page (clé d'URL).
    anchors: HashMap<String, HashSet<String>>,
}

struct SourceData {
    units: Vec<UnitIdx>,
    /// Fréquence documentaire (en sections) des termes racinisés.
    df: HashMap<String, u32>,
    n_sections: usize,
}

fn stems(text: &str) -> HashSet<String> {
    crate::search::query_terms(text).iter().map(|t| crate::stem::stem(t)).collect()
}

fn build_sources(ctx: &Ctx, units: &[Unit]) -> (BTreeMap<String, SourceData>, Vec<Value>) {
    let mut out: BTreeMap<String, SourceData> = BTreeMap::new();
    let mut info = Vec::new();
    for u in units {
        let Ok(st) = Store::open_read(&ctx.root, &u.name) else { continue };
        let t = Instant::now();
        let Ok(ix) = Index::open(&st) else { continue };
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        let s = ix.stats();
        info.push(json!({"unite": u.name, "index_ms": r3(ms), "index_reconstruit": ix.rebuilt(), "sections_indexees": s.docs, "termes": s.terms, "index_ko": s.bytes / 1024}));
        let mut pages = HashSet::new();
        for r in st.records() {
            if u.in_scope(&r.url) {
                pages.insert(url_key(&r.url));
                if let Some(c) = &r.canonical {
                    pages.insert(url_key(c));
                }
            }
        }
        let sd = out.entry(u.source.clone()).or_insert_with(|| SourceData { units: Vec::new(), df: HashMap::new(), n_sections: 0 });
        let mut anchors: HashMap<String, HashSet<String>> = HashMap::new();
        for sec in st.sections() {
            if !u.in_scope(&sec.page_url) {
                continue;
            }
            anchors.entry(url_key(&sec.page_url)).or_default().insert(anchor_key(&sec.anchor));
            sd.n_sections += 1;
            let mut text = sec.heading_path.join(" ");
            text.push(' ');
            text.push_str(&sec.page_title);
            text.push(' ');
            text.push_str(&sec.text);
            for t in stems(&text) {
                *sd.df.entry(t).or_insert(0) += 1;
            }
        }
        sd.units.push(UnitIdx { unit: u.clone(), index: ix, pages, anchors });
    }
    (out, info)
}

#[derive(Clone, Debug)]
struct RHit {
    id: String,
    page_url: String,
    page_key: String,
    anchor: String,
    score: f32,
    hit: Hit,
    /// Indice de l'unité dans `SourceData::units` (ou de la source en mode global).
    unit: usize,
}

fn search_source(sd: &SourceData, q: &str, version: Option<&str>) -> Vec<RHit> {
    let f = Filter { version: version.map(|v| v.trim().trim_start_matches('v').to_string()), ..Default::default() };
    let mut out: Vec<RHit> = Vec::new();
    for (i, u) in sd.units.iter().enumerate() {
        for h in u.index.query(q, &f, RAW_LIMIT) {
            if !u.unit.in_scope(&h.page_url) {
                continue;
            }
            out.push(RHit {
                id: h.id.clone(),
                page_key: url_key(&h.page_url),
                page_url: h.page_url.clone(),
                anchor: id_anchor(&h.id),
                score: h.score,
                hit: h,
                unit: i,
            });
        }
    }
    out.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.id.cmp(&b.id)));
    out.truncate(KEEP);
    out
}

/// Rang (1-based) de la 1re PAGE attendue, les pages comptées une fois à leur 1re apparition.
fn rank_page(hits: &[RHit], expect_keys: &HashSet<String>) -> Option<usize> {
    let mut seen: Vec<&str> = Vec::new();
    for h in hits {
        if seen.contains(&h.page_key.as_str()) {
            continue;
        }
        seen.push(&h.page_key);
        if expect_keys.contains(&h.page_key) {
            return Some(seen.len());
        }
    }
    None
}

/// Rang (1-based, parmi les sections) de la 1re section attendue (page + ancre ; ancre nulle = la page).
fn rank_section(hits: &[RHit], expect: &[(String, Option<String>)]) -> Option<usize> {
    for (i, h) in hits.iter().enumerate() {
        for (u, a) in expect {
            if h.page_key == url_key(u) && a.as_ref().map(|a| anchor_key(a) == h.anchor).unwrap_or(true) {
                return Some(i + 1);
            }
        }
    }
    None
}

fn verdict_version(top: Option<&str>, expect_keys: &HashSet<String>, trap: &Option<String>) -> &'static str {
    match top {
        None => "aucune",
        Some(k) if expect_keys.contains(k) => "correcte",
        Some(k) if trap.as_ref().map(|t| url_key(t) == k).unwrap_or(false) => "piege",
        Some(_) => "autre",
    }
}

/// Couverture des termes de la question dans la section de tête, pondérée par leur rareté (idf de la source).
fn wcov(sd: &SourceData, question: &str, top: &RHit) -> f32 {
    let q = stems(question);
    if q.is_empty() {
        return 0.0;
    }
    let text = match sd.units.get(top.unit).and_then(|u| u.index.get_section(&top.id)) {
        Some(s) => format!("{} {} {}", s.heading_path.join(" "), s.page_title, s.text),
        None => format!("{} {} {}", top.hit.heading_path.join(" "), top.hit.title, top.hit.snippet),
    };
    let t = stems(&text);
    let n = sd.n_sections.max(1) as f32;
    let idf = |s: &str| {
        let df = *sd.df.get(s).unwrap_or(&0) as f32;
        ((n - df + 0.5) / (df + 0.5) + 1.0).ln()
    };
    let (mut num, mut den) = (0f32, 0f32);
    for s in &q {
        let w = idf(s);
        den += w;
        if t.contains(s) {
            num += w;
        }
    }
    if den <= 0.0 {
        0.0
    } else {
        num / den
    }
}

// ───────────────────────── Évaluation d'une question ─────────────────────────

struct Row {
    q: Question,
    answerable: bool,
    // Cortex Web
    page_rank: Option<usize>,
    sec_rank: Option<usize>,
    global_rank: Option<usize>,
    n_hits: usize,
    tokens: usize,
    ms: f64,
    score1: f32,
    wcov: f32,
    /// `Hit::confidence` du 1er résultat et ses comptes (termes de la question, appariés, présents dans la source, noms absents).
    conf: f32,
    counts: (u32, u32, u32, u32),
    /// Sources à plusieurs versions : rangs (page, section) SANS choix de version, et version retenue automatiquement.
    free: Option<(Option<usize>, Option<usize>)>,
    auto_ver: Option<String>,
    /// Une meilleure réponse existe dans une AUTRE version (la carte l'annoncerait).
    also: bool,
    top: Vec<(String, f32)>,
    page_in_store: bool,
    anchor_in_store: Option<bool>,
    ver: Option<(&'static str, &'static str, Option<usize>)>,
    /// Sources à plusieurs versions : rangs (page, section) avec `--version` = version du magasin qui
    /// porte la page attendue (la version du lockfile est supposée connue).
    vf: Option<(Option<usize>, Option<usize>)>,
    // Base A
    base_rank: Option<usize>,
    base_n: usize,
    base_tokens: usize,
    base_ms: f64,
    base_ver: Option<&'static str>,
    base_top: Vec<String>,
}

/// Version choisie automatiquement pour une source à plusieurs versions : celle que la question nomme
/// (`PostgreSQL 17`, `ureq 3`), sinon la version de l'unité principale (celle du lockfile).
fn auto_version(sd: &SourceData, question: &str) -> Option<String> {
    if sd.units.len() < 2 {
        return None;
    }
    let known: Vec<String> = sd.units.iter().filter_map(|u| u.unit.version.clone()).collect();
    let primary = sd.units.iter().find(|u| u.unit.name == u.unit.source).and_then(|u| u.unit.version.clone());
    index::pick_version(question, &known, primary.as_deref())
}

fn eval_web(srcs: &BTreeMap<String, SourceData>, q: &Question, repeat: usize, any_version: bool) -> Row {
    let answerable = !q.expect.is_empty();
    let keys: HashSet<String> = q.expect.iter().map(|(u, _)| url_key(u)).collect();
    let mut row = Row {
        q: q.clone(),
        answerable,
        page_rank: None,
        sec_rank: None,
        global_rank: None,
        n_hits: 0,
        tokens: 0,
        ms: 0.0,
        score1: 0.0,
        wcov: 0.0,
        conf: 0.0,
        counts: (0, 0, 0, 0),
        free: None,
        auto_ver: None,
        also: false,
        top: Vec::new(),
        page_in_store: false,
        anchor_in_store: None,
        ver: None,
        vf: None,
        base_rank: None,
        base_n: 0,
        base_tokens: 0,
        base_ms: 0.0,
        base_ver: None,
        base_top: Vec::new(),
    };
    let Some(sd) = srcs.get(&q.source) else { return row };
    let mut times = Vec::new();
    let mut hits: Vec<RHit> = Vec::new();
    let mut cards = String::new();
    let free_hits = search_source(sd, &q.q, None);
    let auto = if any_version { None } else { auto_version(sd, &q.q) };
    for _ in 0..repeat.max(1) {
        let t = Instant::now();
        hits = if auto.is_some() { search_source(sd, &q.q, auto.as_deref()) } else { free_hits.clone() };
        let top: Vec<Hit> = hits.iter().take(CARDS_SHOWN).map(|h| h.hit.clone()).collect();
        cards = index::render_cards(&top, CARDS_BUDGET);
        times.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    row.ms = median(&mut times);
    row.tokens = tokens_of(&cards);
    row.n_hits = hits.len();
    row.page_rank = if q.needs_js { None } else { rank_page(&hits, &keys) };
    row.sec_rank = if q.needs_js { None } else { rank_section(&hits, &q.expect) };
    row.top = hits.iter().take(3).map(|h| (h.id.clone(), h.score)).collect();
    if let Some(h) = hits.first() {
        row.score1 = h.score;
        row.wcov = wcov(sd, &q.q, h);
        row.conf = h.hit.confidence();
        row.counts = (h.hit.query_terms, h.hit.matched_terms, h.hit.corpus_terms, h.hit.missing_names);
    }
    row.auto_ver = auto.clone();
    if auto.is_some() {
        row.free = Some((rank_page(&free_hits, &keys), rank_section(&free_hits, &q.expect)));
        let (m, o): (Vec<Hit>, Vec<Hit>) = (
            hits.iter().map(|h| h.hit.clone()).collect(),
            free_hits.iter().filter(|h| !hits.iter().any(|x| x.id == h.id)).map(|h| h.hit.clone()).collect(),
        );
        row.also = index::also_in(&m, &o).is_some();
    }
    if answerable && !q.needs_js {
        row.page_in_store = sd.units.iter().any(|u| keys.iter().any(|k| u.pages.contains(k)));
        let with_anchor: Vec<&(String, Option<String>)> = q.expect.iter().filter(|(_, a)| a.is_some()).collect();
        if !with_anchor.is_empty() {
            row.anchor_in_store = Some(with_anchor.iter().any(|(u, a)| {
                let (k, a) = (url_key(u), anchor_key(a.as_deref().unwrap_or("")));
                sd.units.iter().any(|x| x.anchors.get(&k).map(|s| s.contains(&a)).unwrap_or(false))
            }));
        }
    }
    if sd.units.len() > 1 && answerable && !q.needs_js {
        let owner = sd.units.iter().find(|u| keys.iter().any(|k| u.pages.contains(k))).and_then(|u| u.unit.version.clone());
        if let Some(v) = owner {
            let f = search_source(sd, &q.q, Some(&v));
            row.vf = Some((rank_page(&f, &keys), rank_section(&f, &q.expect)));
        }
    }
    if let Some(v) = &q.version {
        let free = verdict_version(free_hits.first().map(|h| h.page_key.as_str()), &keys, &q.trap);
        // Le filtre n'a de sens que si une version collectée correspond (sinon : même résultat que sans filtre).
        let vv = v.trim().trim_start_matches('v');
        let has =
            sd.units.iter().any(|u| u.unit.version.as_deref().map(|uv| uv == vv || uv.starts_with(&format!("{vv}."))).unwrap_or(false));
        let filt = if has { search_source(sd, &q.q, Some(v)) } else { free_hits.clone() };
        let fv = verdict_version(filt.first().map(|h| h.page_key.as_str()), &keys, &q.trap);
        row.ver = Some((free, fv, rank_page(&filt, &keys)));
    }
    row
}

/// Mode « toutes sources » : une question sans indication de source (rang de page).
fn eval_global(srcs: &BTreeMap<String, SourceData>, q: &Question) -> Option<usize> {
    if q.expect.is_empty() || q.needs_js {
        return None;
    }
    let keys: HashSet<String> = q.expect.iter().map(|(u, _)| url_key(u)).collect();
    let mut all: Vec<RHit> = Vec::new();
    for sd in srcs.values() {
        all.extend(search_source(sd, &q.q, None));
    }
    all.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.id.cmp(&b.id)));
    rank_page(&all, &keys)
}

// ───────────────────────── Base A : requêtes ─────────────────────────

fn load_manifest(home: &Path, unit: &str) -> HashMap<String, String> {
    let p = home.join("docs").join(unit).join("_manifest.tsv");
    let mut m = HashMap::new();
    for l in std::fs::read_to_string(p).unwrap_or_default().lines() {
        let mut it = l.split('\t');
        if let (Some(f), Some(_), Some(u)) = (it.next(), it.next(), it.next()) {
            m.insert(f.to_string(), u.to_string());
        }
    }
    m
}

/// Fichiers cités par la sortie de `cortex docs query` : lignes `- [source] fichier : extrait`.
fn parse_base_output(out: &str) -> Vec<String> {
    out.lines()
        .filter_map(|l| {
            let r = l.strip_prefix("- [")?;
            let (_, rest) = r.split_once("] ")?;
            let stem = rest.split(" : ").next()?;
            Some(stem.trim().to_string())
        })
        .collect()
}

fn eval_base(
    exe: &Path,
    home: &Path,
    units: &[Unit],
    manifests: &mut HashMap<String, HashMap<String, String>>,
    row: &mut Row,
    repeat: usize,
) {
    let q = &row.q;
    // Base A ne connaît pas la version : on lui donne le magasin de la bonne version (oracle).
    let unit = units
        .iter()
        .find(|u| {
            u.source == q.source && q.version.as_ref().map(|v| u.version.as_deref() == Some(v.as_str())).unwrap_or(u.name == u.source)
        })
        .or_else(|| units.iter().find(|u| u.name == q.source));
    let Some(unit) = unit else { return };
    let man = manifests.entry(unit.name.clone()).or_insert_with(|| load_manifest(home, &unit.name));
    let mut times = Vec::new();
    let mut output = String::new();
    for _ in 0..repeat.max(1) {
        let t = Instant::now();
        let o = std::process::Command::new(exe)
            .args(["docs", "query", &q.q, "--source", &unit.name])
            .env("CORTEX_HOME", home)
            .stdin(std::process::Stdio::null())
            .output();
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        output = o.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    }
    row.base_ms = median(&mut times);
    row.base_tokens = tokens_of(&output);
    // Pages dans l'ordre, doublons retirés, hors-portée écartés (même règle que Cortex Web).
    let mut pages: Vec<String> = Vec::new();
    for stem in parse_base_output(&output) {
        if let Some(u) = man.get(&stem) {
            if unit.in_scope(u) {
                let k = url_key(u);
                if !pages.contains(&k) {
                    pages.push(k);
                }
            }
        }
    }
    row.base_n = pages.len();
    row.base_top = pages.iter().take(3).cloned().collect();
    let keys: HashSet<String> = q.expect.iter().map(|(u, _)| url_key(u)).collect();
    if !q.expect.is_empty() && !q.needs_js {
        row.base_rank = pages.iter().position(|p| keys.contains(p)).map(|i| i + 1);
    }
    if q.version.is_some() {
        row.base_ver = Some(verdict_version(pages.first().map(|s| s.as_str()), &keys, &q.trap));
    }
}

// ───────────────────────── Agrégats ─────────────────────────

#[derive(Default, Clone, Debug)]
struct Agg {
    n: usize,
    t1: usize,
    t5: usize,
    rr: f64,
}

impl Agg {
    fn add(&mut self, rank: Option<usize>) {
        self.n += 1;
        if let Some(r) = rank {
            if r == 1 {
                self.t1 += 1;
            }
            if r <= 5 {
                self.t5 += 1;
            }
            self.rr += 1.0 / r as f64;
        }
    }
    fn mrr(&self) -> f64 {
        if self.n == 0 {
            0.0
        } else {
            self.rr / self.n as f64
        }
    }
    fn json(&self) -> Value {
        json!({"n": self.n, "top1_pct": pct(self.t1, self.n), "top5_pct": pct(self.t5, self.n), "mrr": r3(self.mrr())})
    }
    fn cell(&self) -> String {
        format!("{:>3} | {:>5.1} | {:>5.1} | {:.3}", self.n, pct(self.t1, self.n), pct(self.t5, self.n), self.mrr())
    }
}

type RankFn = fn(&Row) -> Option<usize>;

fn group_agg(rows: &[&Row], key: impl Fn(&Row) -> String, rk: RankFn) -> BTreeMap<String, Agg> {
    let mut m: BTreeMap<String, Agg> = BTreeMap::new();
    for r in rows {
        m.entry(key(r)).or_default().add(rk(r));
    }
    m
}

fn rk_page(r: &Row) -> Option<usize> {
    r.page_rank
}
fn rk_sec(r: &Row) -> Option<usize> {
    r.sec_rank
}
fn rk_base(r: &Row) -> Option<usize> {
    r.base_rank
}
fn rk_global(r: &Row) -> Option<usize> {
    r.global_rank
}

fn all_agg(rows: &[&Row], rk: RankFn) -> Agg {
    let mut a = Agg::default();
    for r in rows {
        a.add(rk(r));
    }
    a
}

// ───────────────────────── Seuil de confiance ─────────────────────────

#[derive(Clone, Copy, PartialEq, Debug)]
enum Feature {
    Wcov,
    Score,
    Conf,
}

impl Feature {
    fn parse(s: &str) -> Result<Feature, String> {
        match s {
            "wcov" => Ok(Feature::Wcov),
            "score" => Ok(Feature::Score),
            "conf" => Ok(Feature::Conf),
            _ => Err("--feature : `conf`, `wcov` ou `score`".to_string()),
        }
    }
    fn value(self, r: &Row) -> f32 {
        match self {
            Feature::Wcov => r.wcov,
            Feature::Score => r.score1,
            Feature::Conf => r.conf,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Feature::Wcov => "wcov",
            Feature::Score => "score",
            Feature::Conf => "conf",
        }
    }
}

/// « Répondre » ssi des résultats existent et le trait atteint le seuil.
fn answered(r: &Row, f: Feature, th: Option<f32>) -> bool {
    r.n_hits > 0 && th.map(|t| f.value(r) >= t).unwrap_or(true)
}

/// J(θ) = (négatifs rejetés / négatifs) + (positifs répondus ET bien classés 1er / positifs).
fn j_of(rows: &[Row], f: Feature, th: f32) -> (f64, usize, usize, usize, usize) {
    let neg: Vec<&Row> = rows.iter().filter(|r| !r.answerable).collect();
    let pos: Vec<&Row> = rows.iter().filter(|r| r.answerable).collect();
    let neg_ok = neg.iter().filter(|r| !answered(r, f, Some(th))).count();
    let pos_ok = pos.iter().filter(|r| answered(r, f, Some(th)) && r.page_rank == Some(1)).count();
    let j = neg_ok as f64 / neg.len().max(1) as f64 + pos_ok as f64 / pos.len().max(1) as f64;
    (j, neg_ok, neg.len(), pos_ok, pos.len())
}

/// Meilleur seuil sur `rows` (dev) et la table complète. Égalité : le plus bas seuil.
fn calibrate(rows: &[Row], f: Feature) -> (f32, Vec<(f32, f64, usize, usize)>) {
    let mut cands: Vec<f32> = match f {
        Feature::Wcov | Feature::Conf => (0..=20).map(|i| i as f32 * 0.05).collect(),
        Feature::Score => {
            let mut v: Vec<f32> = rows.iter().filter(|r| r.n_hits > 0).map(|r| r.score1).collect();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            v.dedup();
            let mut mids: Vec<f32> = vec![0.0];
            mids.extend(v.windows(2).map(|w| (w[0] + w[1]) / 2.0));
            mids
        }
    };
    cands.dedup();
    let mut best = (0.0f32, -1.0f64);
    let mut table = Vec::new();
    for &th in &cands {
        let (j, neg_ok, _, pos_ok, _) = j_of(rows, f, th);
        table.push((th, j, neg_ok, pos_ok));
        if j > best.1 + 1e-9 {
            best = (th, j);
        }
    }
    (best.0, table)
}

// ───────────────────────── Rapport ─────────────────────────

fn table(rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut w = vec![0usize; cols];
    for r in rows {
        for (i, c) in r.iter().enumerate() {
            w[i] = w[i].max(c.chars().count());
        }
    }
    let mut o = String::new();
    for r in rows {
        let line: Vec<String> = r.iter().enumerate().map(|(i, c)| format!("{:<width$}", c, width = w[i])).collect();
        o.push_str(line.join("  ").trim_end());
        o.push('\n');
    }
    o
}

fn diag(r: &Row) -> &'static str {
    if r.q.needs_js {
        "needs_js : page à rendu JavaScript, hors du moteur (attendu)"
    } else if !r.page_in_store {
        "page attendue ABSENTE du magasin : collecte (portée, budget, robots, liens) ou page non rendue"
    } else if r.anchor_in_store == Some(false) {
        "page présente mais ANCRE inconnue de l'extracteur : découpage ou slug différent"
    } else if r.page_rank.is_some() {
        "page trouvée mais mal classée (index/BM25F)"
    } else {
        "page et ancre présentes, absentes des résultats (index : termes de la question)"
    }
}

fn agg_table(rows: &[&Row], with_sec: bool, with_base: bool) -> String {
    let mut out = String::new();
    let mut lines: Vec<Vec<String>> = Vec::new();
    let mut head = vec!["groupe".to_string(), "Web page (n|top1|top5|MRR)".to_string()];
    if with_sec {
        head.push("Web section".to_string());
    }
    if with_base {
        head.push("Base A page".to_string());
    }
    lines.push(head);
    type KeyFn = Box<dyn Fn(&Row) -> String>;
    let cuts: Vec<(&str, KeyFn)> = vec![
        ("langue", Box::new(|r: &Row| r.q.lang.clone())),
        ("étiquette", Box::new(|r: &Row| r.q.tag.clone())),
        ("source", Box::new(|r: &Row| r.q.source.clone())),
    ];
    let total = |rk: RankFn| all_agg(rows, rk);
    let mut tot = vec!["TOUT".to_string(), total(rk_page).cell()];
    if with_sec {
        tot.push(total(rk_sec).cell());
    }
    if with_base {
        tot.push(total(rk_base).cell());
    }
    lines.push(tot);
    for (name, key) in &cuts {
        let p = group_agg(rows, |r| key(r), rk_page);
        let s = group_agg(rows, |r| key(r), rk_sec);
        let b = group_agg(rows, |r| key(r), rk_base);
        for (k, a) in &p {
            let mut l = vec![format!("{name} {k}"), a.cell()];
            if with_sec {
                l.push(s.get(k).map(|x| x.cell()).unwrap_or_default());
            }
            if with_base {
                l.push(b.get(k).map(|x| x.cell()).unwrap_or_default());
            }
            lines.push(l);
        }
    }
    out.push_str(&table(&lines));
    out
}

fn row_json(r: &Row, f: Feature, th: Option<f32>) -> Value {
    let mut j = json!({
        "id": r.q.id, "source": r.q.source, "lang": r.q.lang, "tag": r.q.tag, "question": r.q.q,
        "repondable": r.answerable, "needs_js": r.q.needs_js,
        "rang_page": r.page_rank, "rang_section": r.sec_rank, "rang_page_toutes_sources": r.global_rank,
        "resultats": r.n_hits, "jetons_cartes": r.tokens, "ms_mediane": r3(r.ms),
        "score1": r3(r.score1 as f64), "wcov": r3(r.wcov as f64), "conf": r3(r.conf as f64), "repond": answered(r, f, th),
        "termes": {"question": r.counts.0, "apparies": r.counts.1, "dans_la_source": r.counts.2, "noms_absents": r.counts.3},
        "version_auto": r.auto_ver, "aussi_dans_autre_version": r.also,
        "top3": r.top.iter().map(|(i, s)| json!({"id": i, "score": r3(*s as f64)})).collect::<Vec<_>>(),
        "page_dans_magasin": r.page_in_store, "ancre_dans_magasin": r.anchor_in_store,
        "base_a": {"rang_page": r.base_rank, "pages": r.base_n, "jetons": r.base_tokens, "ms_mediane": r3(r.base_ms), "top3": r.base_top},
    });
    if let Some((pp, ss)) = &r.vf {
        j["avec_version_du_magasin"] = json!({"rang_page": pp, "rang_section": ss});
    }
    if let Some((free, filt, rf)) = &r.ver {
        j["version"] = json!({"top1_sans_filtre": free, "top1_avec_filtre": filt, "rang_page_avec_filtre": rf, "base_a": r.base_ver});
    }
    if !r.answerable || r.page_rank != Some(1) {
        j["diagnostic"] = json!(if r.answerable { diag(r) } else { "sans réponse attendue" });
    }
    j
}

// ───────────────────────── Orchestration ─────────────────────────

/// Point d'entrée de `cortex web bench`.
pub fn run(ctx: &Ctx, a: &BenchArgs) -> Result<String, String> {
    let feature = Feature::parse(&a.feature)?;
    if a.calibrate && a.questions.to_string_lossy().to_ascii_lowercase().contains("test") {
        return Err("--calibrate refusé : le seuil se règle sur le dev, jamais sur le fichier de test".to_string());
    }
    let corpus: Value =
        serde_json::from_slice(&std::fs::read(&a.corpus).map_err(|e| format!("corpus illisible ({}) : {e}", a.corpus.display()))?)
            .map_err(|e| format!("corpus invalide : {e}"))?;
    let qraw: Value = serde_json::from_slice(
        &std::fs::read(&a.questions).map_err(|e| format!("questions illisibles ({}) : {e}", a.questions.display()))?,
    )
    .map_err(|e| format!("questions invalides : {e}"))?;
    let only: Option<HashSet<String>> =
        a.sources.as_ref().map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect());
    let units = load_units(&corpus, &only)?;
    let questions = load_questions(&qraw, &only)?;
    let home = a.home.clone().unwrap_or_else(crate::index::cortex_home);
    let bctx = Ctx { root: home.join("web"), policy: ctx.policy.clone(), min_delay_ms: ctx.min_delay_ms };
    let out_dir = a.out.clone().unwrap_or_else(|| PathBuf::from("bench/web/resultats"));
    let _ = std::fs::create_dir_all(&out_dir);
    let exe = std::env::current_exe().map_err(|e| format!("binaire introuvable : {e}"))?;
    let with_base = !a.skip_base;
    let ram = Ram::start();
    let started = Instant::now();

    // 1. Collecte.
    let crawl_file = home.join("bench-crawl.json");
    let crawl_rows: Vec<Value> = if a.skip_crawl {
        std::fs::read(&crawl_file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    } else {
        if a.home.is_none() {
            return Err("collecte refusée sans --home : le banc travaille dans un CORTEX_HOME isolé (ou --skip-crawl)".to_string());
        }
        let _ = std::fs::create_dir_all(&home);
        let rows = crawl_phase(&bctx, &exe, &home, &units, a.parallel, &ram, with_base);
        let _ = std::fs::write(&crawl_file, serde_json::to_vec_pretty(&rows).unwrap_or_default());
        rows
    };

    if let Some(dir) = &a.reextract {
        let (ok, ko) = reextract(&bctx, dir);
        eprintln!("[bench] extraction rejouée : {ok} page(s) réécrites, {ko} refusée(s)");
    }

    // 2. Index et questions.
    let (srcs, index_info) = build_sources(&bctx, &units);
    if srcs.is_empty() {
        return Err(format!("aucun magasin exploitable dans {} (collecter d'abord, sans --skip-crawl)", bctx.root.display()));
    }
    let mut manifests: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut rows: Vec<Row> = Vec::new();
    for q in &questions {
        let mut r = eval_web(&srcs, q, a.repeat, a.any_version);
        r.global_rank = eval_global(&srcs, q);
        if with_base {
            eval_base(&exe, &home, &units, &mut manifests, &mut r, a.repeat);
        }
        rows.push(r);
    }

    // 3. Seuil.
    let mut calib_json = Value::Null;
    let mut threshold = a.threshold;
    if a.calibrate {
        let (th, tab) = calibrate(&rows, feature);
        let (_, th_other_tab) = calibrate(&rows, if feature == Feature::Wcov { Feature::Score } else { Feature::Wcov });
        threshold = Some(th);
        calib_json = json!({
            "fichier": a.questions.display().to_string(), "trait": feature.name(), "seuil": th,
            "critere": "J = négatifs rejetés/négatifs + positifs répondus ET 1ers/positifs ; égalité : seuil le plus bas",
            "table": tab.iter().map(|(t, j, n, p)| json!({"seuil": r3(*t as f64), "J": r3(*j), "negatifs_rejetes": n, "positifs_corrects": p})).collect::<Vec<_>>(),
            "autre_trait_meilleur_J": th_other_tab.iter().map(|x| x.1).fold(0.0f64, f64::max),
        });
    }
    if threshold.is_none() && feature == Feature::Conf {
        threshold = Some(index::MIN_CONFIDENCE);
    }
    let th = threshold;

    // 4. Rapport.
    let pos: Vec<&Row> = rows.iter().filter(|r| r.answerable).collect();
    let neg: Vec<&Row> = rows.iter().filter(|r| !r.answerable).collect();
    let mut rep = String::new();
    let dt = chrono_free_date();
    rep.push_str(&format!(
        "BANC CORTEX WEB — {} — questions : {} ({} avec réponse, {} sans) — sources : {}\nCPU PARTAGÉ (serveur de dev local sur le port 8080) : temps = médiane de {} essais ; durées de collecte bornées par la politesse (1 requête/s par hôte), pas par le CPU.\nSeuil : {} (trait {}). Durée du banc : {:.0}s.\n\n",
        dt,
        a.questions.display(),
        pos.len(),
        neg.len(),
        srcs.keys().cloned().collect::<Vec<_>>().join(", "),
        a.repeat,
        th.map(|t| format!("{t:.3}")).unwrap_or_else(|| "aucun (pas d'abstention)".into()),
        feature.name(),
        started.elapsed().as_secs_f64()
    ));

    // (1) collecte
    if !crawl_rows.is_empty() {
        rep.push_str("== 1. COLLECTE (par unité ; passe 1 / passe 2) ==\n");
        let mut t = vec![vec![
            "unité".into(),
            "pages(portée)".into(),
            "hors".into(),
            "Ko".into(),
            "sections".into(),
            "vides".into(),
            "s".into(),
            "pages/s".into(),
            "2e passe".into(),
            "RAM Mo".into(),
            "arrêt".into(),
            "robots".into(),
            "échecs".into(),
            "needs_js".into(),
        ]];
        for r in &crawl_rows {
            let m = &r["magasin"];
            let p1 = &r["passe1"];
            let p2 = &r["passe2"];
            t.push(vec![
                r["unite"].as_str().unwrap_or("?").to_string(),
                m["pages_en_portee"].to_string(),
                m["pages_hors_portee"].to_string(),
                (m["octets_en_portee"].as_u64().unwrap_or(0) / 1024).to_string(),
                m["sections_en_portee"].to_string(),
                m["sections_vides_lt40"].to_string(),
                format!("{:.0}", p1["secondes"].as_f64().unwrap_or(0.0)),
                format!("{:.2}", p1["pages_par_s"].as_f64().unwrap_or(0.0)),
                format!(
                    "{}x304/{}x200{}",
                    p2["inchangees_304"],
                    p2["recues_200"],
                    if p2["tout_en_304"].as_bool().unwrap_or(false) { " OK" } else { " NON" }
                ),
                format!("{}", p1["ram_crete_mo"].as_f64().unwrap_or(0.0)),
                p1["arret"].as_str().unwrap_or("?").to_string(),
                p1["robots"].to_string(),
                p1["echecs"].to_string(),
                p1["needs_js"].to_string(),
            ]);
        }
        rep.push_str(&table(&t));
        rep.push_str("RAM = crête du jeu de travail du PROCESSUS pendant la passe 1 (autres sources en parallèle comprises ; mesure par GetProcessMemoryInfo, échantillons de 200 ms).\n");
        if with_base {
            rep.push_str("Base A (cortex docs add) : ");
            let b: Vec<String> = crawl_rows
                .iter()
                .filter(|r| !r["base_a"].is_null())
                .map(|r| {
                    format!(
                        "{} {}p({} en portée) {}s",
                        r["unite"].as_str().unwrap_or("?"),
                        r["base_a"]["pages"],
                        r["base_a"]["pages_en_portee"],
                        r["base_a"]["secondes"]
                    )
                })
                .collect();
            rep.push_str(&b.join(" ; "));
            rep.push('\n');
        }
        rep.push('\n');
    }
    let (ram_cur, ram_peak) = mem::usage();
    rep.push_str(&format!(
        "RAM du processus de banc : jeu de travail {} Mo, crête {} Mo (toute la session).\n\n",
        mb(ram_cur),
        mb(ram_peak)
    ));

    // (2)(3) questions
    rep.push_str("== 2-3. QUESTIONS AVEC RÉPONSE : Cortex Web (page, section) / base A (page) ==\n");
    rep.push_str(&agg_table(&pos, true, with_base));
    let glob = all_agg(&pos, rk_global);
    rep.push_str(&format!(
        "Mode toutes sources (sans indiquer la source, page) : n {} | top-1 {:.1}% | top-5 {:.1}% | MRR {:.3}\n",
        glob.n,
        pct(glob.t1, glob.n),
        pct(glob.t5, glob.n),
        glob.mrr()
    ));
    let pos_nojs: Vec<&Row> = pos.iter().copied().filter(|r| !r.q.needs_js).collect();
    let nj = all_agg(&pos_nojs, rk_page);
    rep.push_str(&format!(
        "Hors questions needs_js (docsify) : Web page n {} | top-1 {:.1}% | top-5 {:.1}% | MRR {:.3}\n",
        nj.n,
        pct(nj.t1, nj.n),
        pct(nj.t5, nj.n),
        nj.mrr()
    ));
    let mut ms: Vec<f64> = pos.iter().map(|r| r.ms).collect();
    let mut bms: Vec<f64> = pos.iter().map(|r| r.base_ms).collect();
    let mut tk: Vec<f64> = pos.iter().filter(|r| r.n_hits > 0).map(|r| r.tokens as f64).collect();
    let mut btk: Vec<f64> = pos.iter().filter(|r| r.base_tokens > 0).map(|r| r.base_tokens as f64).collect();
    rep.push_str(&format!(
        "Jetons de la réponse (médiane) : Web {:.0} (cartes de {} résultats au plus, budget {}) ; base A {:.0}. Temps par question (médiane) : Web {:.1} ms ; base A {:.0} ms (processus lancé à chaque fois).\n",
        median(&mut tk), CARDS_SHOWN, CARDS_BUDGET, median(&mut btk), median(&mut ms), median(&mut bms)
    ));
    // pages d'abord
    let mut dist = [0usize; 4];
    for r in &pos_nojs {
        match r.page_rank {
            Some(1) => dist[0] += 1,
            Some(2..=5) => dist[1] += 1,
            Some(_) => dist[2] += 1,
            None => dist[3] += 1,
        }
    }
    rep.push_str(&format!("Rang page (hors needs_js) : 1er {} ; 2-5 {} ; >5 {} ; absent {}.\n\n", dist[0], dist[1], dist[2], dist[3]));

    // multi-versions
    let mv: Vec<&Row> = pos.iter().copied().filter(|r| r.vf.is_some()).collect();
    if !mv.is_empty() {
        let (mut a0, mut a1, mut s0, mut s1) = (Agg::default(), Agg::default(), Agg::default(), Agg::default());
        for r in &mv {
            a0.add(r.page_rank);
            s0.add(r.sec_rank);
            a1.add(r.vf.and_then(|x| x.0));
            s1.add(r.vf.and_then(|x| x.1));
        }
        let also_n = mv.iter().filter(|r| r.also).count();
        let (mut a2, mut s2) = (Agg::default(), Agg::default());
        for r in &mv {
            a2.add(r.free.map(|x| x.0).unwrap_or(r.page_rank));
            s2.add(r.free.map(|x| x.1).unwrap_or(r.sec_rank));
        }
        rep.push_str(&format!(
            "== SOURCES À PLUSIEURS VERSIONS ({} questions : ureq 2.10.1/3.5.2, python 3.13/3.11, postgresql 16/17) ==\n",
            mv.len()
        ));
        rep.push_str(&table(&[
            vec!["mode".into(), "page (n|top1|top5|MRR)".into(), "section".into()],
            vec!["sans filtre de version (--any-version)".into(), a2.cell(), s2.cell()],
            vec![
                if a.any_version {
                    "(résultats ci-dessus = sans filtre)".into()
                } else {
                    "automatique : version nommée par la question, sinon version de la source".to_string()
                },
                a0.cell(),
                s0.cell(),
            ],
            vec!["--version = version du magasin attendu (oracle)".into(), a1.cell(), s1.cell()],
        ]));
        rep.push_str(&format!(
            "Carte « aussi dans <version> » affichée pour {also_n}/{} questions (une meilleure réponse existe dans une autre version).\n",
            mv.len()
        ));
        rep.push('\n');
    }

    // version
    let vr: Vec<&Row> = rows.iter().filter(|r| r.ver.is_some()).collect();
    if !vr.is_empty() {
        let c = |sel: &dyn Fn(&Row) -> &'static str, v: &str| vr.iter().filter(|r| sel(**r) == v).count();
        let free = |r: &Row| r.ver.map(|x| x.0).unwrap_or("");
        let filt = |r: &Row| r.ver.map(|x| x.1).unwrap_or("");
        let base = |r: &Row| r.base_ver.unwrap_or("");
        rep.push_str(&format!("== VERSION ({} questions) : version de la 1re page ==\n", vr.len()));
        let mut t = vec![vec!["mode".into(), "correcte".into(), "piège".into(), "autre".into(), "aucune".into()]];
        for (n, f) in [
            ("Web sans filtre", &free as &dyn Fn(&Row) -> &'static str),
            ("Web --version", &filt),
            ("base A (magasin de la bonne version : oracle)", &base),
        ] {
            t.push(vec![
                n.into(),
                c(f, "correcte").to_string(),
                c(f, "piege").to_string(),
                c(f, "autre").to_string(),
                c(f, "aucune").to_string(),
            ]);
        }
        rep.push_str(&table(&t));
        rep.push('\n');
    }

    // (4) négatifs
    rep.push_str("== 4. SANS RÉPONSE (expect:[]) : « rien de fiable » ==\n");
    let neg_ok = neg.iter().filter(|r| !answered(r, feature, th)).count();
    let pos_rej = pos.iter().filter(|r| !answered(r, feature, th)).count();
    let pos_ans1 = pos.iter().filter(|r| answered(r, feature, th) && r.page_rank == Some(1)).count();
    rep.push_str(&format!(
        "Négatifs correctement rejetés : {}/{} ({:.0}%) ; positifs refusés à tort : {}/{} ; positifs répondus et 1ers : {}/{} ({:.1}%). Sans seuil, un négatif est « rejeté » seulement si l'index ne rend rien.\n",
        neg_ok, neg.len(), pct(neg_ok, neg.len()), pos_rej, pos.len(), pos_ans1, pos.len(), pct(pos_ans1, pos.len())
    ));
    let base_neg_ok = neg.iter().filter(|r| r.base_n == 0).count();
    rep.push_str(&format!("Base A (aucune notion de confiance) : {}/{} négatifs sans aucun résultat.\n", base_neg_ok, neg.len()));
    for r in &neg {
        rep.push_str(&format!(
            "  {} [{}] « {} » : {} résultats, score1 {:.2}, wcov {:.2}, conf {:.2} (termes {} / appariés {} / dans la source {} / noms absents {}) -> {}\n",
            r.q.id, r.q.source, r.q.q, r.n_hits, r.score1, r.wcov, r.conf, r.counts.0, r.counts.1, r.counts.2, r.counts.3,
            if answered(r, feature, th) { "RÉPOND (faux)" } else { "rien de fiable (juste)" }
        ));
    }
    if a.calibrate {
        rep.push_str(&format!(
            "\nCALIBRATION (dev) : trait {}, seuil retenu {:.3} ; à rejouer sur le test : --threshold {:.3} --feature {}\n",
            feature.name(),
            th.unwrap_or(0.0),
            th.unwrap_or(0.0),
            feature.name()
        ));
    }
    rep.push('\n');

    // (5) échecs
    rep.push_str("== 5. ÉCHECS (rang de page > 5 ou absent), diagnostic ==\n");
    let mut fails: Vec<&Row> = pos.iter().copied().filter(|r| !r.q.needs_js && r.page_rank.map(|x| x > 5).unwrap_or(true)).collect();
    fails.sort_by(|a, b| a.q.id.cmp(&b.q.id));
    let mut by_diag: BTreeMap<&'static str, usize> = BTreeMap::new();
    for r in &fails {
        *by_diag.entry(diag(r)).or_insert(0) += 1;
    }
    for (d, n) in &by_diag {
        rep.push_str(&format!("  {n} × {d}\n"));
    }
    for r in fails.iter().take(14) {
        rep.push_str(&format!(
            "  {} [{}] « {} »\n    attendu {} #{} | rang page {:?} section {:?} | tête : {}\n",
            r.q.id,
            r.q.source,
            r.q.q,
            r.q.expect.first().map(|e| e.0.as_str()).unwrap_or("?"),
            r.q.expect.first().and_then(|e| e.1.clone()).unwrap_or_default(),
            r.page_rank,
            r.sec_rank,
            r.top.first().map(|t| t.0.as_str()).unwrap_or("(aucun résultat)")
        ));
    }
    let sec_gap: Vec<&Row> = pos_nojs
        .iter()
        .copied()
        .filter(|r| r.page_rank == Some(1) && r.sec_rank.map(|x| x > 1).unwrap_or(true) && r.q.expect.iter().any(|e| e.1.is_some()))
        .collect();
    rep.push_str(&format!("Page 1re mais section non 1re (ancre attendue) : {} question(s).\n", sec_gap.len()));
    for r in sec_gap.iter().take(6) {
        rep.push_str(&format!("  {} section {:?}, ancre dans le magasin : {:?}\n", r.q.id, r.sec_rank, r.anchor_in_store));
    }

    // JSON
    let stem = a.questions.file_stem().and_then(|s| s.to_str()).unwrap_or("questions").to_string();
    let doc = json!({
        "schema": "cortex-web-bench-report/1", "date": dt, "questions_fichier": a.questions.display().to_string(),
        "seuil": th, "trait": feature.name(), "calibration": calib_json, "repeat": a.repeat,
        "cpu_partage": "serveur de dev sur le port 8080 : temps = médiane de plusieurs essais",
        "ram_processus_mo": {"courant": mb(ram_cur), "crete": mb(ram_peak)},
        "collecte": crawl_rows, "index": index_info,
        "agregats": {
            "web_page": all_agg(&pos, rk_page).json(), "web_section": all_agg(&pos, rk_sec).json(),
            "web_toutes_sources": glob.json(), "base_a_page": all_agg(&pos, rk_base).json(),
            "web_page_hors_needs_js": nj.json(),
            "par_langue_web": group_agg(&pos, |r| r.q.lang.clone(), rk_page).iter().map(|(k, v)| (k.clone(), v.json())).collect::<BTreeMap<_, _>>(),
            "par_etiquette_web": group_agg(&pos, |r| r.q.tag.clone(), rk_page).iter().map(|(k, v)| (k.clone(), v.json())).collect::<BTreeMap<_, _>>(),
            "par_source_web": group_agg(&pos, |r| r.q.source.clone(), rk_page).iter().map(|(k, v)| (k.clone(), v.json())).collect::<BTreeMap<_, _>>(),
            "par_langue_base_a": group_agg(&pos, |r| r.q.lang.clone(), rk_base).iter().map(|(k, v)| (k.clone(), v.json())).collect::<BTreeMap<_, _>>(),
            "par_etiquette_base_a": group_agg(&pos, |r| r.q.tag.clone(), rk_base).iter().map(|(k, v)| (k.clone(), v.json())).collect::<BTreeMap<_, _>>(),
            "par_source_base_a": group_agg(&pos, |r| r.q.source.clone(), rk_base).iter().map(|(k, v)| (k.clone(), v.json())).collect::<BTreeMap<_, _>>(),
            "negatifs": {"n": neg.len(), "rejetes_web": neg_ok, "sans_resultat_base_a": base_neg_ok, "positifs_refuses": pos_rej},
        },
        "questions": rows.iter().map(|r| row_json(r, feature, th)).collect::<Vec<_>>(),
    });
    let _ = std::fs::write(out_dir.join(format!("bench-{stem}.json")), serde_json::to_vec_pretty(&doc).unwrap_or_default());
    let _ = std::fs::write(out_dir.join(format!("bench-{stem}.txt")), &rep);
    rep.push_str(&format!("\nRapports écrits : {}/bench-{stem}.json et .txt\n", out_dir.display()));
    ram.stop.store(true, Ordering::Relaxed);
    Ok(rep)
}

/// Date du jour (UTC, AAAA-MM-JJ) sans dépendance.
fn chrono_free_date() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let days = (secs / 86_400) as i64;
    // Algorithme civil de Howard Hinnant.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{:02}-{:02}", if m <= 2 { y + 1 } else { y }, m, d)
}

// ───────────────────────── Tests ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn unit() -> Unit {
        Unit {
            name: "py".into(),
            source: "py".into(),
            family: "sphinx".into(),
            seeds: vec![
                "https://docs.python.org/3.13/library/json.html".into(),
                "https://docs.python.org/3.13/library/asyncio.html".into(),
            ],
            hosts: vec!["docs.python.org".into()],
            prefix: "/3.13/library/".into(),
            include: vec!["/3.13/library/json.html".into(), "/3.13/library/asyncio".into(), "/3.13/library/pathlib".into()],
            exclude: vec!["/3.13/library/asyncio-dev".into()],
            max_pages: 40,
            version: Some("3.13".into()),
            needs_js: false,
        }
    }

    #[test]
    fn url_et_ancre_normalisees() {
        assert_eq!(url_key("https://Docs.Example/a/b/#x"), url_key("https://docs.example/a/b"));
        assert_eq!(url_key("https://e.org/a/index.html"), "e.org/a");
        assert_eq!(anchor_key("Foo~2"), "foo");
        assert_eq!(anchor_key("a~b"), "a~b");
        assert_eq!(id_anchor("W:s/p#Methode~3"), "methode");
    }

    #[test]
    fn portee_include_exclude() {
        let u = unit();
        assert!(u.in_scope("https://docs.python.org/3.13/library/json.html"));
        assert!(u.in_scope("https://docs.python.org/3.13/library/asyncio-task.html"));
        assert!(!u.in_scope("https://docs.python.org/3.13/library/asyncio-dev.html"));
        assert!(!u.in_scope("https://docs.python.org/3.13/library/os.html"));
        assert!(!u.in_scope("https://docs.python.org/3.11/library/json.html"));
        assert!(!u.in_scope("https://autre.org/3.13/library/json.html"));
    }

    #[test]
    fn la_portee_du_corpus_part_telle_quelle_dans_la_configuration() {
        let c = unit().config();
        assert_eq!(c.include_paths.len(), 3);
        assert_eq!(c.exclude_paths, vec!["/3.13/library/asyncio-dev".to_string()]);
        assert_eq!(c.seeds.len(), 2);
        assert_eq!(c.max_pages, 40);
        assert_eq!(c.max_depth, 40);
        assert_eq!(c.path_prefix, "/3.13/library/");
    }

    fn row(answerable: bool, n_hits: usize, rank: Option<usize>, w: f32) -> Row {
        Row {
            q: Question {
                id: "x".into(),
                q: "q".into(),
                lang: "fr".into(),
                source: "s".into(),
                tag: "howto".into(),
                expect: vec![],
                needs_js: false,
                version: None,
                trap: None,
            },
            answerable,
            page_rank: rank,
            sec_rank: rank,
            global_rank: None,
            n_hits,
            tokens: 0,
            ms: 0.0,
            score1: w * 10.0,
            wcov: w,
            conf: w,
            counts: (0, 0, 0, 0),
            free: None,
            auto_ver: None,
            also: false,
            top: vec![],
            page_in_store: true,
            anchor_in_store: None,
            ver: None,
            vf: None,
            base_rank: None,
            base_n: 0,
            base_tokens: 0,
            base_ms: 0.0,
            base_ver: None,
            base_top: vec![],
        }
    }

    #[test]
    fn calibration_separe_positifs_et_negatifs() {
        let rows = vec![
            row(true, 5, Some(1), 0.9),
            row(true, 5, Some(1), 0.8),
            row(true, 5, Some(3), 0.7),
            row(false, 5, None, 0.2),
            row(false, 5, None, 0.3),
        ];
        let (th, _) = calibrate(&rows, Feature::Wcov);
        assert!(th > 0.3 && th <= 0.8, "{th}");
        let (j, neg_ok, _, pos_ok, _) = j_of(&rows, Feature::Wcov, th);
        assert_eq!((neg_ok, pos_ok), (2, 2));
        assert!(j > 1.6);
        let (ths, _) = calibrate(&rows, Feature::Score);
        assert!(ths > 3.0 && ths <= 8.0, "{ths}");
    }

    #[test]
    fn agregats_top1_top5_mrr() {
        let mut a = Agg::default();
        for r in [Some(1), Some(2), None, Some(6)] {
            a.add(r);
        }
        assert_eq!((a.n, a.t1, a.t5), (4, 1, 2));
        assert!((a.mrr() - (1.0 + 0.5 + 0.0 + 1.0 / 6.0) / 4.0).abs() < 1e-9);
    }

    #[test]
    fn sortie_de_la_base_a_est_lue() {
        let out = "# Cortex docs — passages pertinents\n- [py] 3-13-library-json-html : a / b\n- [py] x : y\n(texte)\n";
        assert_eq!(parse_base_output(out), vec!["3-13-library-json-html".to_string(), "x".to_string()]);
    }

    #[test]
    fn abstention_sans_seuil_exige_des_resultats() {
        assert!(!answered(&row(false, 0, None, 0.9), Feature::Wcov, None));
        assert!(answered(&row(false, 2, None, 0.1), Feature::Wcov, None));
        assert!(!answered(&row(false, 2, None, 0.1), Feature::Wcov, Some(0.5)));
    }

    #[test]
    fn date_du_jour_a_la_bonne_forme() {
        let d = chrono_free_date();
        assert_eq!(d.len(), 10);
        assert!(d.starts_with("20"));
    }
}
