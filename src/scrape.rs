//! Scraper documentaire offline — l'extension du savoir.
//!
//! Crawl respectueux (rate-limit, profondeur limitée, même domaine) d'un site de
//! doc. Extrait le contenu principal (vire nav/footer/aside), convertit en texte
//! markdown propre, stocke en local `~/.cortex/docs/<name>/`. Robuste : timeout,
//! retries, jamais bloqué sur une page (on continue).

use scraper::{Html, Selector};
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use url::Url;

const USER_AGENT: &str = "Mozilla/5.0 (compatible; CortexDocsBot/1.0; +offline-knowledge)";
const RATE_LIMIT_MS: u64 = 350; // poli : ~3 req/s
const TIMEOUT_SECS: u64 = 20;

/// Domaines mutualisés où "tout le domaine" serait gigantesque : on restreint au
/// préfixe de SECTION de l'URL de départ (ex: github.com/owner/repo, dev.epicgames.com/documentation).
const SHARED_HOST_SUFFIXES: &[&str] = &[
    "github.com",
    "gitlab.com",
    "bitbucket.org",
    "dev.epicgames.com",
    "developer.mozilla.org",
    "learn.microsoft.com",
    "medium.com",
    "dev.to",
    "readthedocs.io",
    "gitbook.io",
    "w3.org",
    "whatwg.org",
    "ietf.org",
    "khronos.org",
    "docs.rs",
    "crates.io",
    "owasp.org",
    "iso.org",
];

/// Calcule le préfixe de chemin autorisé pour le crawl, SANS jamais sortir du domaine.
///
/// - Domaine dédié à une doc (threejs.org, react.dev, docs.univer.ai…) → préfixe "/"
///   = tout le domaine. Exhaustif : on ne rate aucune section sœur.
/// - Domaine mutualisé (github.com, dev.epicgames.com…) → préfixe = les N premiers
///   segments identifiant la section (ex: "/owner/repo/", "/documentation/unreal-engine/"),
///   pour ne pas aspirer tout le site partagé.
pub fn compute_scope(start: &Url, domain: &str) -> String {
    let segs: Vec<&str> = start.path().split('/').filter(|s| !s.is_empty()).collect();
    let is_shared = SHARED_HOST_SUFFIXES.iter().any(|s| domain == *s || domain.ends_with(&format!(".{}", s)));

    // Domaine MUTUALISÉ (github.com, docs.rs, dev.epicgames.com…) → cerne la section
    // par les 2 premiers segments (/owner/repo, /tokio/version, /documentation/ue…).
    if is_shared {
        if segs.is_empty() {
            return "/".to_string();
        }
        let n = 2.min(segs.len());
        return format!("/{}/", segs[..n].join("/"));
    }

    // Domaine DÉDIÉ : on cerne la doc à un préfixe, MAIS pas trop étroit (sinon on
    // rate des sections sœurs : /primitives/docs/overview/intro doit garder
    // /primitives/docs/, pas /primitives/docs/overview/). Compromis : dossier parent,
    // plafonné à 2 segments de profondeur. Départ racine ou page-racine → tout le domaine.
    if segs.is_empty() {
        return "/".to_string();
    }
    let ends_slash = start.path().ends_with('/');
    // segments « dossier » = tout si slash final, sinon sans la dernière (page).
    let dir_segs = if ends_slash { segs.len() } else { segs.len().saturating_sub(1) };
    if dir_segs == 0 {
        return "/".to_string(); // page à la racine (/intro) → tout le domaine
    }
    // Plafond 2 : on ne descend pas plus profond que /a/b/ (assez large pour les
    // docs structurées, assez strict pour ne pas aspirer tout le domaine).
    let keep = dir_segs.min(2);
    format!("/{}/", segs[..keep].join("/"))
}

pub fn docs_home() -> PathBuf {
    crate::index::cortex_home().join("docs")
}

/// Crawl un site de doc et stocke les pages en markdown local (sans progression).
/// Langue par défaut "en" (filtre les traductions).
pub fn scrape_site(start_url: &str, name: &str, max_pages: usize) -> Result<usize, String> {
    scrape_site_progress(start_url, name, max_pages, "en", &mut |_, _| {})
}

/// Crawl un site de doc avec callback de progression `progress(pages_faites, taille_queue)`,
/// appelé à chaque page récupérée — permet un affichage live dans le terminal.
/// max_pages borne le crawl (sécurité). `lang` = langue à garder ("en"/"fr"/"*").
pub fn scrape_site_progress(
    start_url: &str,
    name: &str,
    max_pages: usize,
    lang: &str,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<usize, String> {
    let start = Url::parse(start_url).map_err(|e| format!("URL invalide: {}", e))?;
    let domain = start.host_str().unwrap_or("").to_string();
    // SCOPE du crawl : on ne sort JAMAIS du domaine. Le préfixe de chemin autorisé
    // est calculé intelligemment pour ne RIEN oublier sans aspirer un site entier
    // mutualisé (github.com, dev.epicgames.com…). Voir `compute_scope`.
    let base_path = compute_scope(&start, &domain);

    let agent = build_agent();

    let out_dir = docs_home().join(name);
    std::fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;

    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<Url> = VecDeque::new();
    queue.push_back(start.clone());
    seen.insert(normalize(&start));

    let mut count = 0;
    let mut manifest = String::new();
    let t0 = Instant::now();

    while let Some(url) = queue.pop_front() {
        if count >= max_pages {
            break;
        }
        // Rate-limit poli.
        std::thread::sleep(Duration::from_millis(RATE_LIMIT_MS));

        let html = match fetch(&agent, url.as_str()) {
            Some(h) => h,
            None => continue, // page KO → on continue (jamais bloqué)
        };

        let (title, main_text, links) = parse_page(&html, &url, &domain, &base_path, lang);
        // Validation contenu : ≥ 25 mots de substance (rejette « titres sans corps »).
        if crate::aimd::substance_words(&main_text) >= 25 {
            let fname = slugify(url.path());
            let path = out_dir.join(format!("{}.md", fname));
            let md = format!("<!-- cortex-doc · {} -->\n# {}\n\nURL: {}\n\n{}\n", name, title, url, main_text);
            if std::fs::write(&path, &md).is_ok() {
                manifest.push_str(&format!("{}\t{}\t{}\n", fname, title, url));
                count += 1;
            }
        }

        // Découvre TOUS les liens internes (filtrés, scope exhaustif) → alimente la file.
        for next in links {
            let norm = normalize(&next);
            if seen.insert(norm) {
                queue.push_back(next);
            }
        }

        progress(count, queue.len());
    }

    // Anti-oubli : si la limite a coupé alors qu'il restait des pages à visiter,
    // on le signale clairement (sinon on "oublie" sans le savoir).
    if count >= max_pages && !queue.is_empty() {
        eprintln!(
            "  ⚠ {}: limit of {} pages reached, {} URL(s) still queued; rerun with a higher max to fetch everything.",
            name,
            max_pages,
            queue.len()
        );
    }
    let _ = t0;

    // Manifest + meta.
    let _ = std::fs::write(out_dir.join("_manifest.tsv"), manifest);
    let _ = std::fs::write(
        out_dir.join("_meta.json"),
        format!("{{\"name\":\"{}\",\"start\":\"{}\",\"pages\":{},\"domain\":\"{}\"}}", name, start_url, count, domain),
    );

    Ok(count)
}

const MAX_PAGE_BYTES: usize = 50 * 1024 * 1024; // 50 Mo (grosses specs W3C monolithiques)

fn fetch(agent: &ureq::Agent, url: &str) -> Option<String> {
    use std::io::Read;
    // Jusqu'à 4 tentatives avec backoff exponentiel : robuste face au rate-limiting
    // transitoire (un gros batch 13 sites × 20 conn. peut faire throttler un serveur,
    // ex Epic Games → UnrealEngine ressortait à 0 page sur un échec de la racine).
    const MAX_ATTEMPTS: u32 = 4;
    for attempt in 0..MAX_ATTEMPTS {
        match agent.get(url).call() {
            Ok(resp) => {
                let ct = resp.content_type().to_string();
                if !ct.contains("html") && !ct.contains("xml") {
                    return None;
                }
                // Lecture bornée à 50 Mo (into_string plafonne à 10 Mo → spec WebGPU 4.5 Mo
                // décompressée peut dépasser et échouer silencieusement). On lit nous-mêmes.
                let mut buf = Vec::new();
                if resp.into_reader().take(MAX_PAGE_BYTES as u64).read_to_end(&mut buf).is_ok() {
                    return Some(String::from_utf8_lossy(&buf).into_owned());
                }
                return None;
            }
            // 4xx (sauf 429) = définitif, inutile de réessayer.
            Err(ureq::Error::Status(code, _)) if (400..500).contains(&code) && code != 429 => {
                return None;
            }
            Err(_) if attempt + 1 < MAX_ATTEMPTS => {
                // backoff : 500ms, 1s, 2s
                std::thread::sleep(Duration::from_millis(500 * (1 << attempt)));
                continue;
            }
            Err(_) => return None,
        }
    }
    None
}

/// Sélecteur du conteneur de contenu principal (heuristiques larges des frameworks
/// de doc modernes : Nextra, Docusaurus, VitePress, Mintlify, Fumadocs, GitBook…).
fn build_content_selector() -> Selector {
    Selector::parse(
        "main, article, [role=main], \
         .markdown, .content, .doc-content, .prose, #content, .documentation, \
         .markdown-body, .theme-doc-markdown, .vp-doc, .docMainContainer, \
         .nextra-content, .mdx-content, .docs-content, .doc-markdown, \
         .main-content, #main-content, [data-content], .md-content, .rm-Markdown",
    )
    .unwrap()
}

/// Construit un agent HTTP configuré (timeout + user-agent poli). Réutilisable.
pub fn build_agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(Duration::from_secs(TIMEOUT_SECS)).user_agent(USER_AGENT).build()
}

/// Parse une page HTML : retourne (titre, markdown-IA, liens internes filtrés).
/// Centralise titre + extraction de contenu + découverte de liens (scope exhaustif,
/// jamais hors domaine, sans bruit). Utilisé par le moteur séquentiel ET concurrent.
pub fn parse_page(html: &str, url: &Url, domain: &str, base_path: &str, keep_lang: &str) -> (String, String, Vec<Url>) {
    let doc = Html::parse_document(html);
    let content_sel = build_content_selector();
    let strip_sel = Selector::parse("nav").unwrap(); // placeholder (aimd gère l'élagage)
    let link_sel = Selector::parse("a[href]").unwrap();

    let title =
        doc.select(&Selector::parse("title").unwrap()).next().map(|t| t.text().collect::<String>().trim().to_string()).unwrap_or_default();

    let main_text = extract_main_text(&doc, &content_sel, &strip_sel);

    let mut links = Vec::new();
    for el in doc.select(&link_sel) {
        if let Some(href) = el.value().attr("href") {
            if href.starts_with('#') || href.starts_with("mailto:") || href.starts_with("tel:") || href.starts_with("javascript:") {
                continue;
            }
            if let Ok(mut next) = url.join(href) {
                next.set_fragment(None);
                // Langue en QUERY (Stripe: ?locale=fr-FR, autres: ?lang=, ?hl=) : on
                // déduplique en retirant ces params (sinon ×N langues = ×N pages). Si
                // la query demande une langue NON gardée → on saute carrément.
                if let Some(skip) = normalize_lang_query(&mut next, keep_lang) {
                    if skip {
                        continue;
                    }
                }
                if next.host_str() == Some(domain)
                    && next.path().starts_with(base_path)
                    && (next.scheme() == "http" || next.scheme() == "https")
                    && !is_non_html_path(next.path())
                    && !is_noise_path(domain, next.path(), keep_lang)
                {
                    links.push(next);
                }
            }
        }
    }
    (title, main_text, links)
}

/// Versions publiques des helpers (réutilisées par le moteur concurrent).
pub fn fetch_pub(agent: &ureq::Agent, url: &str) -> Option<String> {
    fetch(agent, url)
}
pub fn normalize_pub(u: &Url) -> String {
    normalize(u)
}
pub fn slugify_pub(path: &str) -> String {
    slugify(path)
}

fn extract_main_text(doc: &Html, content_sel: &Selector, _strip_sel: &Selector) -> String {
    // Choisit le MEILLEUR conteneur de contenu (le plus riche en texte), pas le 1er :
    // certains sites (specs W3C) ont un <span class="content"> vide AVANT le <main>.
    let body_sel = Selector::parse("body").unwrap();
    let best = doc
        .select(content_sel)
        .map(|n| (n.text().map(|t| t.len()).sum::<usize>(), n))
        .filter(|(len, _)| *len > 200) // ignore les conteneurs quasi-vides
        .max_by_key(|(len, _)| *len)
        .map(|(_, n)| n)
        .or_else(|| doc.select(&body_sel).next());
    let node = match best {
        Some(n) => n,
        None => return String::new(),
    };
    // Converter "AI-Markdown" : dense, sans bruit nav/footer, structuré (cf aimd.rs).
    crate::aimd::to_ai_markdown(node)
}

fn normalize(u: &Url) -> String {
    let mut s = u.clone();
    s.set_fragment(None);
    s.as_str().trim_end_matches('/').to_string()
}

/// Vrai si le chemin est du "bruit" non-documentaire pour un host donné.
/// Sur GitHub notamment, on ne veut QUE le contenu (README, blob/tree, wiki),
/// pas les milliers de pages issues/commits/pulls/blame qui ne sont pas de la doc.
fn is_noise_path(domain: &str, path: &str, keep_lang: &str) -> bool {
    let p = path.to_ascii_lowercase();

    // 0. Filtre LANGUE selon keep_lang :
    //    - "en" (défaut) : on ne garde que l'anglais (vire /fr//de/, _ru.html…).
    //    - "fr"          : on garde FR + EN (conformité française), vire les autres.
    //    - "*"           : aucun filtre langue (tout).
    match keep_lang {
        "*" | "all" => {}
        "fr" => {
            // Vire les langues qui ne sont NI fr NI en.
            if is_non_english_path(&p) && !is_french_path(&p) {
                return true;
            }
        }
        _ => {
            // "en" : EN only.
            if is_non_english_path(&p) {
                return true;
            }
        }
    }

    // 1. Filtre FONCTIONNEL universel : on veut tutos / API / guides / reference,
    //    PAS les blogs, news, marketing, pages projet/communauté. Ces segments
    //    explosent le crawl (Debian: news/listes/wiki → 15000+ URLs) sans valeur doc.
    const NON_FUNCTIONAL: &[&str] = &[
        "/blog",
        "/news",
        "/about",
        "/community",
        "/showcase",
        "/gallery",
        "/pricing",
        "/contact",
        "/careers",
        "/jobs",
        "/team",
        "/press",
        "/events",
        "/event/",
        "/sponsors",
        "/donate",
        "/store",
        "/shop",
        "/legal",
        "/privacy",
        "/terms",
        "/cookie",
        "/license-",
        "/contributors",
        "/changelog",
        "/releases/",
        "/release-notes",
        "/roadmap",
        "/forum",
        "/discussions",
        "/support/",
        "/account",
        "/login",
        "/signup",
        "/register",
        "/mailinglist",
        "/lists/",
        "/mirror",
        "/download/",
        "/social",
        "/users/",
        "/profile",
        "/search",
        "/tag/",
        "/tags/",
        "/category/",
        "/author/",
        "/feed",
        "/rss",
        "/archive/",
        "/devel/website",
        // Spécifique Debian-like (gouvernance/i18n/portage = pas de la doc technique).
        "/vote/",
        "/devel/wnpp",
        "/devel/join",
        "/international/",
        "/intl/",
        "/ports/",
        "/security/",
        "/bugs/",
        "/mirror/",
        "/distrib/",
        "/partners/",
        "/consultants/",
        "/banners/",
        "/logos/",
        "/misc/",
        "/devel/people",
    ];
    if NON_FUNCTIONAL.iter().any(|seg| p.contains(seg)) {
        return true;
    }

    // 2. GitHub : ne garder que la DOC, pas l'UI repo NI le code source.
    if domain.ends_with("github.com") {
        const GH_NOISE: &[&str] = &[
            "/issues",
            "/pull",
            "/pulls",
            "/commit",
            "/commits",
            "/compare",
            "/blame",
            "/actions",
            "/projects",
            "/security",
            "/pulse",
            "/graphs",
            "/network",
            "/stargazers",
            "/watchers",
            "/forks",
            "/branches",
            "/settings",
            "/raw/",
            "/find/",
            "/milestones",
            "/labels",
            "/wiki/_",
            "/graphs",
            "/deployments",
            "/packages",
            "/contributors",
            "/community",
        ];
        if GH_NOISE.iter().any(|seg| p.contains(seg)) {
            return true;
        }
        // /blob/ et /tree/ explorent le CODE SOURCE → on ne garde que la doc :
        // fichiers markdown (.md/.mdx/.rst/.txt) ou répertoires de doc (/docs/, /doc/).
        if p.contains("/blob/") {
            let is_doc =
                p.ends_with(".md") || p.ends_with(".mdx") || p.ends_with(".rst") || p.ends_with(".txt") || p.ends_with(".markdown");
            return !is_doc; // tout fichier blob non-doc = bruit
        }
        if p.contains("/tree/") {
            // Garde seulement les répertoires de doc (sinon on descend dans tout le code).
            let is_doc_dir =
                p.contains("/docs") || p.contains("/doc/") || p.contains("/documentation") || p.contains("/guide") || p.contains("/wiki");
            return !is_doc_dir;
        }
        return false; // racine repo + /wiki = OK
    }
    false
}

/// Normalise les paramètres de langue en QUERY STRING (Stripe ?locale=, ?lang=, ?hl=).
/// Retire ces params de l'URL (déduplication). Retourne Some(true) s'il faut SAUTER
/// le lien (langue demandée non gardée), Some(false) si normalisé OK, None si rien à faire.
fn normalize_lang_query(url: &mut Url, keep_lang: &str) -> Option<bool> {
    const LANG_PARAMS: &[&str] = &["locale", "lang", "hl", "language", "lng"];
    let has_lang = url.query_pairs().any(|(k, _)| LANG_PARAMS.contains(&k.as_ref()));
    if !has_lang {
        return None;
    }
    // Récupère la valeur de langue demandée.
    let requested: Option<String> = url.query_pairs().find(|(k, _)| LANG_PARAMS.contains(&k.as_ref())).map(|(_, v)| v.to_ascii_lowercase());

    // Reconstruit la query SANS les params de langue.
    let kept: Vec<(String, String)> =
        url.query_pairs().filter(|(k, _)| !LANG_PARAMS.contains(&k.as_ref())).map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    if kept.is_empty() {
        url.set_query(None);
    } else {
        let q: String = kept.iter().map(|(k, v)| format!("{}={}", k, v)).collect::<Vec<_>>().join("&");
        url.set_query(Some(&q));
    }

    // Décide si on saute : la langue demandée doit correspondre à keep_lang.
    if let Some(req) = requested {
        let is_en = req.starts_with("en");
        let is_fr = req.starts_with("fr");
        let skip = match keep_lang {
            "*" | "all" => false,
            "fr" => !(is_en || is_fr),
            _ => !is_en, // "en"
        };
        return Some(skip);
    }
    Some(false)
}

/// Vrai si le chemin est une version FRANÇAISE (à conserver quand keep_lang="fr").
fn is_french_path(p: &str) -> bool {
    p.contains("/fr/")
        || p.contains("/fr-fr/")
        || p.contains("/francais/")
        || p.contains("/french/")
        || p.contains("_fr.")
        || p.contains(".fr.")
        || p.contains("/fr_fr/")
}

/// Vrai si le chemin est une version NON-anglaise (traduction). On garde l'anglais
/// comme doc de référence ; les traductions sont du volume redondant pour une IA.
fn is_non_english_path(p: &str) -> bool {
    // Codes langue courants en segment de path : /fr/, /de/, /es/, /zh-cn/, /pt-br/…
    // (on N'exclut PAS /en/). Détecte aussi suffixes type _ru.html, _ja, .fr.html.
    const LANG_SEGS: &[&str] = &[
        "/fr/", "/de/", "/es/", "/it/", "/pt/", "/pt-br/", "/nl/", "/ru/", "/ja/", "/zh/", "/zh-cn/", "/zh-tw/", "/ko/", "/pl/", "/cs/",
        "/sv/", "/da/", "/fi/", "/no/", "/tr/", "/uk/", "/ar/", "/he/", "/hi/", "/th/", "/vi/", "/id/", "/ro/", "/hu/", "/el/", "/bg/",
        "/hr/", "/sk/", "/sl/", "/lt/", "/lv/", "/et/", "/ca/", "/fa/", "/ml/", "/ta/", "/cn/",
    ];
    if LANG_SEGS.iter().any(|s| p.contains(s)) {
        return true;
    }
    // Noms de langues (Debian /international/Chinese, /intl/spanish…).
    const LANG_NAMES: &[&str] = &[
        "chinese",
        "spanish",
        "french",
        "german",
        "italian",
        "russian",
        "japanese",
        "korean",
        "portuguese",
        "polish",
        "swedish",
        "danish",
        "finnish",
        "turkish",
        "ukrainian",
        "arabic",
        "hebrew",
        "hindi",
        "thai",
        "vietnamese",
        "romanian",
        "hungarian",
        "greek",
        "bulgarian",
        "croatian",
        "dutch",
        "norwegian",
        "czech",
        "catalan",
        "persian",
        "tamil",
        "malayalam",
        "brazilian",
    ];
    if LANG_NAMES.iter().any(|n| p.contains(n)) {
        return true;
    }
    // Suffixes de fichier traduit : foo_ru.html, foo.fr.html, foo_ja.txt…
    const LANG_SUFFIXES: &[&str] = &[
        "_ru.", "_fr.", "_de.", "_es.", "_it.", "_pt.", "_ja.", "_zh.", "_ko.", "_nl.", "_pl.", "_cn.", "_cs.", ".fr.", ".de.", ".es.",
        ".ru.", ".ja.", ".zh.",
    ];
    LANG_SUFFIXES.iter().any(|s| p.contains(s))
}

/// Vrai si le chemin pointe vers une ressource non-HTML (à ne pas crawler).
fn is_non_html_path(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    const SKIP: &[&str] = &[
        ".png", ".jpg", ".jpeg", ".gif", ".svg", ".webp", ".ico", ".bmp", ".pdf", ".zip", ".tar", ".gz", ".tgz", ".rar", ".7z", ".exe",
        ".dmg", ".msi", ".mp4", ".webm", ".mov", ".mp3", ".wav", ".woff", ".woff2", ".ttf", ".eot", ".css", ".js", ".json", ".xml", ".rss",
        ".atom", ".map", ".wasm", ".csv", ".xlsx", ".docx", ".pptx",
    ];
    SKIP.iter().any(|ext| p.ends_with(ext))
}

fn slugify(path: &str) -> String {
    let s: String = path.trim_matches('/').chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
    if s.is_empty() {
        "index".to_string()
    } else {
        s
    }
}

/// Liste les docs scrapées.
pub fn list_docs() -> Vec<(String, usize)> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(docs_home()) {
        for e in entries.flatten() {
            if e.path().is_dir() {
                let name = e.file_name().to_string_lossy().to_string();
                let pages = std::fs::read_dir(e.path())
                    .map(|d| d.flatten().filter(|f| f.path().extension().map(|x| x == "md").unwrap_or(false)).count())
                    .unwrap_or(0);
                out.push((name, pages));
            }
        }
    }
    out
}

/// Recherche simple dans les docs scrapées (grep token-budgété, sans tantivy pour l'instant).
pub fn search_docs(question: &str, source: Option<&str>, budget: usize) -> String {
    let terms: Vec<String> = question.to_ascii_lowercase().split_whitespace().map(String::from).collect();
    let mut results: Vec<(usize, String, String)> = Vec::new(); // (score, fichier, extrait)

    let home = docs_home();
    let dirs: Vec<PathBuf> = match source {
        Some(s) => vec![home.join(s)],
        None => std::fs::read_dir(&home).map(|d| d.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect()).unwrap_or_default(),
    };

    for dir in dirs {
        let src_name = dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if let Ok(files) = std::fs::read_dir(&dir) {
            for f in files.flatten() {
                let p = f.path();
                if p.extension().map(|x| x == "md").unwrap_or(false) {
                    if let Ok(content) = std::fs::read_to_string(&p) {
                        let lower = content.to_ascii_lowercase();
                        let score: usize = terms.iter().map(|t| lower.matches(t.as_str()).count()).sum();
                        if score > 0 {
                            // Extrait : la 1ère ligne contenant un terme + contexte.
                            let snippet = content
                                .lines()
                                .filter(|l| terms.iter().any(|t| l.to_ascii_lowercase().contains(t.as_str())))
                                .take(3)
                                .collect::<Vec<_>>()
                                .join(" / ");
                            results.push((score, format!("[{}] {}", src_name, p.file_stem().unwrap().to_string_lossy()), snippet));
                        }
                    }
                }
            }
        }
    }

    results.sort_by(|a, b| b.0.cmp(&a.0));
    let mut out = String::from("# Cortex docs — passages pertinents\n");
    let char_budget = budget * 4;
    for (_, file, snippet) in results.iter().take(15) {
        let line = format!("- {} : {}\n", file, snippet.chars().take(180).collect::<String>());
        if out.len() + line.len() > char_budget {
            break;
        }
        out.push_str(&line);
    }
    if results.is_empty() {
        out.push_str("(no passage: is the doc scraped? cortex docs add <url> --name <name>)\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(url: &str) -> String {
        let u = Url::parse(url).unwrap();
        compute_scope(&u, u.host_str().unwrap())
    }

    #[test]
    fn scope_dedicated_stays_under_subpath() {
        // Domaine dédié AVEC sous-chemin → reste sous le dossier parent, plafonné à 2
        // segments (assez large pour les docs profondes, assez strict vs explosion).
        assert_eq!(scope("https://www.postgresql.org/docs/16/"), "/docs/16/");
        assert_eq!(scope("https://react.dev/reference/react"), "/reference/");
        assert_eq!(scope("https://threejs.org/docs/"), "/docs/");
        // Doc PROFONDE → plafonné à 2 segments (sinon trop étroit, rate les sœurs).
        assert_eq!(scope("https://www.radix-ui.com/primitives/docs/overview/introduction"), "/primitives/docs/");
        // Départ = page à la racine → tout le domaine autorisé.
        assert_eq!(scope("https://docs.univer.ai/en-US"), "/");
        assert_eq!(scope("https://zod.dev/"), "/");
    }

    #[test]
    fn lang_query_normalized() {
        use url::Url;
        // Stripe ?locale=fr-FR : en mode "en" → skip ; param retiré de l'URL.
        let mut u = Url::parse("https://docs.stripe.com/payments?locale=fr-FR").unwrap();
        assert_eq!(normalize_lang_query(&mut u, "en"), Some(true)); // français → skip
        let mut u2 = Url::parse("https://docs.stripe.com/payments?locale=en-US").unwrap();
        assert_eq!(normalize_lang_query(&mut u2, "en"), Some(false)); // anglais → garde
        assert_eq!(u2.query(), None); // param locale retiré (déduplication)
                                      // Pas de param langue → None.
        let mut u3 = Url::parse("https://docs.stripe.com/payments").unwrap();
        assert_eq!(normalize_lang_query(&mut u3, "en"), None);
    }

    #[test]
    fn scope_shared_host_is_section() {
        // Mutualisé → on cerne la section, sans aspirer tout le site.
        assert_eq!(scope("https://github.com/CoplayDev/unity-mcp"), "/CoplayDev/unity-mcp/");
        assert_eq!(
            scope("https://dev.epicgames.com/documentation/unreal-engine/unreal-engine-5-8-documentation"),
            "/documentation/unreal-engine/"
        );
        assert_eq!(scope("https://docs.rs/tokio/latest/tokio/"), "/tokio/latest/");
    }

    #[test]
    fn non_html_paths_skipped() {
        assert!(is_non_html_path("/assets/logo.png"));
        assert!(is_non_html_path("/files/guide.pdf"));
        assert!(!is_non_html_path("/reference/react"));
        assert!(!is_non_html_path("/docs/getting-started"));
    }

    #[test]
    fn language_filter_keeps_english_only() {
        assert!(is_non_english_path("/fr/guide/intro"));
        assert!(is_non_english_path("/doc/manual_ru.html"));
        assert!(is_non_english_path("/international/chinese/index"));
        assert!(!is_non_english_path("/en/guide/intro"));
        assert!(!is_non_english_path("/doc/manuals/reference"));
        // /tr/ est ambigu (turc OU /TR/ specs W3C en majuscule) : ici on teste le
        // chemin déjà lowercased, donc /tr/ = turc → filtré. compute_scope gère W3C.
        assert!(is_non_english_path("/tr/manual"));
    }

    #[test]
    fn debian_noise_segments_filtered() {
        assert!(is_noise_path("www.debian.org", "/vote/2022/results", "en"));
        assert!(is_noise_path("www.debian.org", "/international/french", "en"));
        assert!(is_noise_path("www.debian.org", "/devel/wnpp/orphaned", "en"));
        assert!(!is_noise_path("www.debian.org", "/doc/manuals/debian-reference", "en"));
    }

    #[test]
    fn github_noise_filtered() {
        // Bruit GitHub écarté ; contenu (repo, blob, tree, wiki) gardé.
        assert!(is_noise_path("github.com", "/CoplayDev/unity-mcp/issues/42", "en"));
        assert!(is_noise_path("github.com", "/CoplayDev/unity-mcp/commits/main", "en"));
        assert!(!is_noise_path("github.com", "/CoplayDev/unity-mcp", "en"));
        assert!(!is_noise_path("github.com", "/CoplayDev/unity-mcp/blob/main/README.md", "en"));
        assert!(!is_noise_path("github.com", "/CoplayDev/unity-mcp/wiki", "en"));
        // Pas de filtrage bruit sur un domaine de doc dédié.
        assert!(!is_noise_path("react.dev", "/reference/react/useEffect", "en"));
    }

    #[test]
    fn lang_filter_respects_keep_lang() {
        // keep_lang="fr" : garde le français (conformité), vire les autres langues.
        assert!(!is_noise_path("www.cnil.fr", "/fr/reglement-rgpd", "fr"));
        assert!(is_noise_path("www.cnil.fr", "/de/dsgvo", "fr")); // allemand viré
                                                                  // keep_lang="en" : le même /fr/ serait viré.
        assert!(is_noise_path("www.cnil.fr", "/fr/reglement-rgpd", "en"));
        // keep_lang="*" : tout passe.
        assert!(!is_noise_path("example.com", "/de/page", "*"));
        assert!(!is_noise_path("example.com", "/fr/page", "*"));
    }
}
