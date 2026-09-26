//! Moteur de crawl concurrent + état partagé pilotable en live.
//!
//! Gain de vitesse : au lieu de fetcher 1 page puis dormir 350ms, chaque site
//! maintient K requêtes EN VOL simultanément (pool de fetchers). Le « délai »
//! devient un espacement minimal entre LANCEMENTS, pas une attente bête. La
//! concurrence intra-site (K) et le délai sont lus à CHAQUE tour depuis un état
//! partagé → réglables en live au clavier, par site (pause incluse).

use crate::scrape;
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use url::Url;

/// Max d'URLs enfilées sous un même préfixe de chemin (2 segments). Garde-fou
/// anti-explosion d'un répertoire géant (ex: /doc/manuals de Debian = milliers
/// de pages × langues × versions). 400 = large pour une vraie section de doc.
const MAX_PER_SUBTREE: usize = 400;

/// Préfixe de regroupement d'un chemin = ses 2 premiers segments (ex: /doc/manuals).
fn subtree_prefix(path: &str) -> String {
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match segs.len() {
        0 => "/".to_string(),
        1 => format!("/{}", segs[0]),
        _ => format!("/{}/{}", segs[0], segs[1]),
    }
}

/// Réglages GLOBAUX modifiables en live (délai commun + auto-refill + quit).
pub struct Controls {
    /// Délai min entre lancements de requêtes (ms), commun à tous. 0..=2000.
    pub delay_ms: AtomicU64,
    /// AUTO-RELANCE : si ON, un site qui atteint son budget avec de la file restante
    /// reçoit automatiquement du budget supplémentaire → il finit TOUT seul, sans
    /// rester « coupé ». Toggle live (touche a). Débloque le cas Debian/gros sites.
    pub auto_refill: AtomicBool,
    /// Stop global demandé (touche q).
    pub quit: AtomicBool,
}

impl Controls {
    pub fn new(delay_ms: u64, auto_refill: bool) -> Arc<Self> {
        Arc::new(Controls { delay_ms: AtomicU64::new(delay_ms), auto_refill: AtomicBool::new(auto_refill), quit: AtomicBool::new(false) })
    }
    pub fn bump_delay(&self, d: i64) {
        let cur = self.delay_ms.load(Ordering::Relaxed) as i64;
        self.delay_ms.store((cur + d).clamp(0, 2000) as u64, Ordering::Relaxed);
    }
    pub fn toggle_auto_refill(&self) {
        let v = self.auto_refill.load(Ordering::Relaxed);
        self.auto_refill.store(!v, Ordering::Relaxed);
    }
}

/// État vivant d'UN site (lu par l'UI, écrit par le worker du site).
/// Concurrence ET budget sont PAR-SITE et modifiables en live (contrôles ciblés).
pub struct SiteState {
    pub name: String,
    pub url: String,
    pub lang: String,             // langue à garder ("en"/"fr"/"*")
    pub pages: AtomicUsize,       // pages écrites
    pub queued: AtomicUsize,      // URLs en file
    pub concurrency: AtomicUsize, // fetchs en vol pour CE site (←/→). 1..=32
    pub max_pages: AtomicUsize,   // budget pages de CE site (+/- live)
    pub done: AtomicBool,         // terminé
    pub paused: AtomicBool,       // pause par site (p/r)
    pub truncated: AtomicBool,    // budget atteint avec file non vide
    pub error: Mutex<Option<String>>,
    pub started: Instant,
}

impl SiteState {
    pub fn new(name: &str, url: &str, max_pages: usize, concurrency: usize, lang: &str, started: Instant) -> Arc<Self> {
        Arc::new(SiteState {
            name: name.to_string(),
            url: url.to_string(),
            lang: lang.to_string(),
            pages: AtomicUsize::new(0),
            queued: AtomicUsize::new(0),
            concurrency: AtomicUsize::new(concurrency.clamp(1, 32)),
            max_pages: AtomicUsize::new(max_pages),
            done: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            truncated: AtomicBool::new(false),
            error: Mutex::new(None),
            started,
        })
    }
    pub fn pages_per_sec(&self) -> f64 {
        let secs = self.started.elapsed().as_secs_f64().max(0.001);
        self.pages.load(Ordering::Relaxed) as f64 / secs
    }
    pub fn bump_concurrency(&self, d: i32) {
        let cur = self.concurrency.load(Ordering::Relaxed) as i32;
        self.concurrency.store((cur + d).clamp(1, 32) as usize, Ordering::Relaxed);
    }
    pub fn add_budget(&self, d: i64) {
        let cur = self.max_pages.load(Ordering::Relaxed) as i64;
        self.max_pages.store((cur + d).max(1) as usize, Ordering::Relaxed);
    }
}

/// Crawl concurrent d'un site, piloté par `controls`, reportant dans `state`.
/// Bloquant (à lancer dans un thread). Réutilise la logique de scope/extraction
/// de `scrape` pour rester cohérent (mêmes filtres exhaustifs).
pub fn crawl_site(state: Arc<SiteState>, controls: Arc<Controls>) {
    let start = match Url::parse(&state.url) {
        Ok(u) => u,
        Err(e) => {
            *state.error.lock().unwrap() = Some(format!("URL invalide: {}", e));
            state.done.store(true, Ordering::Relaxed);
            return;
        }
    };
    let domain = start.host_str().unwrap_or("").to_string();
    let base_path = scrape::compute_scope(&start, &domain);
    // Plafond DUR de l'auto-relance : même en auto, un site ne dépasse jamais 3× son
    // budget initial (évite qu'un site géant comme Stripe gonfle à l'infini).
    let hard_cap = state.max_pages.load(Ordering::Relaxed).saturating_mul(3).max(500);

    let out_dir = scrape::docs_home().join(&state.name);
    if std::fs::create_dir_all(&out_dir).is_err() {
        *state.error.lock().unwrap() = Some("création dossier impossible".into());
        state.done.store(true, Ordering::Relaxed);
        return;
    }

    let agent = scrape::build_agent();
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<Url> = VecDeque::new();
    queue.push_back(start.clone());
    seen.insert(scrape::normalize_pub(&start));

    let manifest = Arc::new(Mutex::new(String::new()));
    let mut last_launch = Instant::now() - Duration::from_secs(1);
    // Plafond par sous-arbre : nb d'URLs déjà enfilées sous chaque préfixe de 2 segments.
    let mut subtree_count: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    // Boucle : tant qu'il reste des URLs et qu'on n'a pas atteint le max / quit.
    'outer: loop {
        if controls.quit.load(Ordering::Relaxed) {
            break;
        }
        // Plus rien à visiter → terminé (définitivement).
        if queue.is_empty() {
            break;
        }
        // Pause par site : on dort court et on relit l'état.
        if state.paused.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(120));
            continue;
        }
        let pages_done = state.pages.load(Ordering::Relaxed);
        let max_pages = state.max_pages.load(Ordering::Relaxed);
        if pages_done >= max_pages {
            if !queue.is_empty() {
                // AUTO-RELANCE : étend le budget et continue jusqu'à épuisement → le
                // site finit seul (Debian & co). MAIS jamais au-delà du plafond DUR
                // (3× le budget initial) pour qu'un site géant (Stripe) ne gonfle pas
                // à l'infini. Au plafond → on s'arrête (tronqué, R pour forcer plus).
                if controls.auto_refill.load(Ordering::Relaxed) && max_pages < hard_cap {
                    state.add_budget(500);
                    state.truncated.store(false, Ordering::Relaxed);
                    continue;
                }
                state.truncated.store(true, Ordering::Relaxed);
            }
            break;
        }

        // Combien lancer ce tour : min(concurrence par-site, file, budget restant).
        let conc = state.concurrency.load(Ordering::Relaxed);
        let budget = max_pages.saturating_sub(pages_done);
        let batch_n = conc.min(queue.len()).min(budget).max(1);

        // Prélève un lot d'URLs.
        let mut batch_urls = Vec::with_capacity(batch_n);
        for _ in 0..batch_n {
            if let Some(u) = queue.pop_front() {
                batch_urls.push(u);
            }
        }

        // Espacement minimal entre lancements de lots (politesse réglable).
        let delay = controls.delay_ms.load(Ordering::Relaxed);
        let since = last_launch.elapsed().as_millis() as u64;
        if delay > since {
            std::thread::sleep(Duration::from_millis(delay - since));
        }
        last_launch = Instant::now();

        // Fetch le lot EN PARALLÈLE (threads scoped — fetch HTTP = I/O bound).
        let fetched: Vec<(Url, Option<String>)> = std::thread::scope(|s| {
            let handles: Vec<_> = batch_urls
                .iter()
                .map(|u| {
                    let agent = &agent;
                    let u = u.clone();
                    s.spawn(move || {
                        let html = scrape::fetch_pub(agent, u.as_str());
                        (u, html)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        // Traite chaque page récupérée : écrit le .md + découvre les liens.
        for (url, html) in fetched {
            if controls.quit.load(Ordering::Relaxed) {
                break 'outer;
            }
            let html = match html {
                Some(h) => h,
                None => continue,
            };
            let (title, main_text, links) = scrape::parse_page(&html, &url, &domain, &base_path, &state.lang);

            // VALIDATION CONTENU : on n'écrit que si la page a de la VRAIE substance
            // (≥ 25 mots hors titres). Rejette les pages « ### Titre » sans corps.
            let substance = crate::aimd::substance_words(&main_text);
            if substance >= 25 {
                let fname = scrape::slugify_pub(url.path());
                let path = out_dir.join(format!("{}.md", fname));
                let md = format!("<!-- cortex-doc · {} -->\n# {}\n\nURL: {}\n\n{}\n", state.name, title, url, main_text);
                if std::fs::write(&path, &md).is_ok() {
                    manifest.lock().unwrap().push_str(&format!("{}\t{}\t{}\n", fname, title, url));
                    state.pages.fetch_add(1, Ordering::Relaxed);
                }
            }
            // Alimente la file, avec PLAFOND PAR SOUS-ARBRE : un même préfixe de path
            // (ex /doc/manuals) ne peut pas dépasser MAX_PER_SUBTREE pages — évite
            // qu'un seul répertoire géant (manuels Debian ×langues×versions) noie tout.
            for next in links {
                let prefix = subtree_prefix(next.path());
                let c = subtree_count.entry(prefix).or_insert(0);
                if *c >= MAX_PER_SUBTREE {
                    continue;
                }
                let norm = scrape::normalize_pub(&next);
                if seen.insert(norm) {
                    *c += 1;
                    queue.push_back(next);
                }
            }
            state.queued.store(queue.len(), Ordering::Relaxed);
        }
    }

    // Manifest final.
    let _ = std::fs::write(out_dir.join("_manifest.tsv"), manifest.lock().unwrap().clone());
    let _ = std::fs::write(
        out_dir.join("_meta.json"),
        format!(
            "{{\"name\":\"{}\",\"start\":\"{}\",\"pages\":{},\"domain\":\"{}\"}}",
            state.name,
            state.url,
            state.pages.load(Ordering::Relaxed),
            domain
        ),
    );
    state.queued.store(queue.len(), Ordering::Relaxed);
    // 0 page écrite = échec visible (page de départ injoignable / rate-limited / vide),
    // pas un faux « ✓ terminé ». L'utilisateur peut alors relancer (R) ce site.
    if state.pages.load(Ordering::Relaxed) == 0 && !controls.quit.load(Ordering::Relaxed) {
        let mut e = state.error.lock().unwrap();
        if e.is_none() {
            *e = Some("0 page (départ injoignable/rate-limited ?) — R pour relancer".into());
        }
    }
    state.done.store(true, Ordering::Relaxed);
}
