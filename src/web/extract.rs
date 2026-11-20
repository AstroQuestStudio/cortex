//! Cortex Web : extraction HTML / Markdown / JSON de documentation vers `Page` (voir docs/WEB.md §2, D2).
//!
//! Principe : (1) lecture des métadonnées et des liens sur la page brute ; (2) suppression du bruit
//! (scripts, menus, éléments masqués, boutons « Copy », commentaires…) ; (3) choix du conteneur de
//! contenu par FAMILLE de site (Docusaurus, VitePress, MkDocs, Sphinx, rustdoc…) avec repli sur le plus
//! riche en texte ; (4) conversion en Markdown dense, découpé en sections par titres h1-h4 (plus les
//! signatures Sphinx et les entrées rustdoc, traitées comme des sous-titres). Les blocs de code et les
//! signatures sont recopiés tels quels (seuls les espaces de fin de ligne et les espaces insécables
//! changent). Aucun accès réseau, aucun état global autre que des tables de sélecteurs.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::OnceLock;

use ego_tree::{NodeId, NodeRef};
use scraper::{ElementRef, Html, Node, Selector};
use url::Url;

use super::{Completeness, Page, Section};

// ───────────────────────── API ─────────────────────────

/// Famille de site de documentation, détectée par signature de la page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    Docusaurus,
    VitePress,
    MkDocs,
    Sphinx,
    RustDoc,
    Mintlify,
    GitBook,
    Nextra,
    GithubMarkdown,
    Generic,
}

/// Point d'entrée : octets reçus + type de contenu → page découpée en sections.
pub fn extract_page(url: &Url, body: &[u8], content_type: &str) -> Page {
    let text = decode(body, content_type);
    let ct = content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    match kind_of(&ct, url, &text) {
        Kind::Html => extract_html(url, &text),
        Kind::Markdown => extract_markdown(url, &text),
        Kind::Json => extract_json(url, &text),
    }
}

/// Famille détectée pour une page HTML (exposée pour le banc et les autres modules).
pub fn detect_family(url: &Url, html: &str) -> Family {
    let doc = Html::parse_document(html);
    let meta = read_meta(&doc, url);
    family_of(url, &meta)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Html,
    Markdown,
    Json,
}

fn kind_of(ct: &str, url: &Url, text: &str) -> Kind {
    let path = url.path().to_ascii_lowercase();
    let md_path = path.ends_with(".md") || path.ends_with(".markdown") || path.ends_with(".mdx") || path.ends_with(".txt");
    if ct.starts_with("text/html") || ct.starts_with("application/xhtml") {
        return Kind::Html;
    }
    if ct.starts_with("text/markdown") || ct.starts_with("text/x-markdown") {
        return Kind::Markdown;
    }
    if ct == "application/json" || ct.ends_with("+json") || ct == "text/json" {
        return Kind::Json;
    }
    let head: String = text.trim_start().chars().take(400).collect::<String>().to_ascii_lowercase();
    let looks_html =
        head.starts_with("<!doctype html") || head.starts_with("<html") || head.starts_with("<head") || head.starts_with("<body");
    if ct.starts_with("text/plain") {
        return if looks_html && !md_path { Kind::Html } else { Kind::Markdown };
    }
    // Type absent ou inconnu : reniflage.
    if looks_html {
        return Kind::Html;
    }
    let t = text.trim_start();
    if (t.starts_with('{') || t.starts_with('[')) && serde_json::from_str::<serde_json::Value>(t).is_ok() {
        return Kind::Json;
    }
    if md_path {
        return Kind::Markdown;
    }
    if t.starts_with('<') {
        Kind::Html
    } else {
        Kind::Markdown
    }
}

// ───────────────────────── Encodage ─────────────────────────

const CP1252_HIGH: [u32; 32] = [
    0x20AC, 0x81, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160, 0x2039, 0x0152, 0x8D, 0x017D, 0x8F, 0x90, 0x2018,
    0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, 0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0x9D, 0x017E, 0x0178,
];

/// Décode les octets : BOM, en-tête `charset`, `<meta charset>` ; UTF-8 avec remplacement par défaut.
fn decode(body: &[u8], content_type: &str) -> String {
    if body.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(&body[3..]).into_owned();
    }
    if body.starts_with(&[0xFF, 0xFE]) {
        let u: Vec<u16> = body[2..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        return String::from_utf16_lossy(&u);
    }
    if body.starts_with(&[0xFE, 0xFF]) {
        let u: Vec<u16> = body[2..].chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
        return String::from_utf16_lossy(&u);
    }
    let label = charset_from_header(content_type).or_else(|| charset_from_meta(body)).unwrap_or_default();
    match label.as_str() {
        "iso-8859-1" | "latin1" | "latin-1" | "iso8859-1" | "windows-1252" | "cp1252" | "iso-8859-15" | "us-ascii" | "ascii" | "l1" => body
            .iter()
            .map(|&b| match b {
                0x80..=0x9F => char::from_u32(CP1252_HIGH[(b - 0x80) as usize]).unwrap_or('\u{FFFD}'),
                _ => b as char,
            })
            .collect(),
        _ => String::from_utf8_lossy(body).into_owned(),
    }
}

fn charset_from_header(ct: &str) -> Option<String> {
    let l = ct.to_ascii_lowercase();
    let i = l.find("charset=")?;
    Some(l[i + 8..].trim().trim_matches(|c| c == '"' || c == '\'').split(|c: char| c == ';' || c.is_whitespace()).next()?.to_string())
}

fn charset_from_meta(body: &[u8]) -> Option<String> {
    let head: String =
        body[..body.len().min(4096)].iter().map(|&b| if b.is_ascii() { (b as char).to_ascii_lowercase() } else { ' ' }).collect();
    let i = head.find("charset=")?;
    let rest = head[i + 8..].trim_start_matches(|c| c == '"' || c == '\'');
    let end = rest.find(|c: char| c == '"' || c == '\'' || c == ';' || c == '>' || c == '/' || c.is_whitespace()).unwrap_or(rest.len());
    let l = &rest[..end];
    if l.is_empty() {
        None
    } else {
        Some(l.to_string())
    }
}

// ───────────────────────── Utilitaires de texte ─────────────────────────

fn is_zero_width(c: char) -> bool {
    matches!(c, '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{2060}' | '\u{FEFF}' | '\u{00AD}')
}

/// Ajoute du texte HTML en repliant les espaces (un seul espace, jamais en tête).
fn push_text(out: &mut String, t: &str) {
    for c in t.chars() {
        if is_zero_width(c) {
            continue;
        }
        if c.is_whitespace() {
            if !(out.is_empty() || out.ends_with(' ') || out.ends_with('\n')) {
                out.push(' ');
            }
        } else {
            out.push(c);
        }
    }
}

/// Slug façon github-slugger (utilisé par Docusaurus, GitHub, VitePress…).
fn slug(text: &str) -> String {
    let mut out = String::new();
    for c in text.trim().to_lowercase().chars() {
        if c.is_alphanumeric() || c == '_' || c == '-' {
            out.push(c);
        } else if c.is_whitespace() {
            out.push('-');
        }
    }
    if out.is_empty() {
        "section".to_string()
    } else {
        out
    }
}

#[derive(Default)]
struct Anchors {
    seen: HashMap<String, u32>,
}

impl Anchors {
    /// Premier exemplaire tel quel, suivants `~2`, `~3`…
    fn unique(&mut self, base: String) -> String {
        let n = self.seen.entry(base.clone()).or_insert(0);
        *n += 1;
        if *n == 1 {
            base
        } else {
            format!("{}~{}", base, n)
        }
    }
}

fn clean_id(id: &str) -> String {
    let t = id.trim();
    t.strip_prefix("user-content-").unwrap_or(t).to_string()
}

fn norm_token(t: &str) -> String {
    let l = t.to_ascii_lowercase();
    // Modules CSS (`navbar_aB3d`) : on retire le suffixe de hachage.
    if let Some(i) = l.find('_') {
        if i > 0 && !l.contains("__") && l.len() - i >= 4 {
            return l[..i].to_string();
        }
    }
    l
}

fn tokens(v: &scraper::node::Element) -> Vec<String> {
    v.attr("class").map(|c| c.split_whitespace().map(norm_token).collect()).unwrap_or_default()
}

fn has_token(toks: &[String], want: &str) -> bool {
    toks.iter().any(|t| t == want)
}

/// Texte court d'un élément (None si plus long que `limit` caractères).
fn short_text(el: ElementRef, limit: usize) -> Option<String> {
    let mut s = String::new();
    for t in el.text() {
        push_text(&mut s, t);
        if s.chars().count() > limit {
            return None;
        }
    }
    Some(s.trim().to_string())
}

/// Vrai si l'élément est toujours rattaché au document (`Html::select` énumère aussi les nœuds détachés).
fn attached(el: ElementRef) -> bool {
    el.ancestors().last().map(|r| matches!(r.value(), Node::Document)).unwrap_or(false)
}

fn text_len(el: ElementRef) -> usize {
    el.text().map(|t| t.chars().filter(|c| !c.is_whitespace()).count()).sum()
}

// ───────────────────────── Métadonnées ─────────────────────────

#[derive(Default)]
struct Meta {
    title_tag: Option<String>,
    og_title: Option<String>,
    generator: String,
    canonical: Option<String>,
    base: Option<String>,
    lang: Option<String>,
    /// (priorité, valeur) : plus petit = plus fiable.
    versions: Vec<(u8, String)>,
    classes: HashSet<String>,
    ids: HashSet<String>,
    noscript_js: bool,
    script_srcs: usize,
    has_tabs: bool,
    hostname: String,
    app_roots: Vec<NodeId>,
}

fn read_meta(doc: &Html, _url: &Url) -> Meta {
    let mut m = Meta::default();
    for node in doc.tree.nodes() {
        let Some(el) = node.value().as_element() else { continue };
        let name = el.name();
        if let Some(c) = el.attr("class") {
            for t in c.split_whitespace() {
                m.classes.insert(t.to_ascii_lowercase());
                m.classes.insert(norm_token(t));
            }
        }
        if let Some(i) = el.id() {
            m.ids.insert(i.to_ascii_lowercase());
            if matches!(i, "root" | "app" | "__next" | "__nuxt" | "svelte" | "___gatsby" | "app-root") {
                m.app_roots.push(node.id());
            }
        }
        if el.attr("role") == Some("tab") {
            m.has_tabs = true;
        }
        // `data-version` est aussi utilisé par des composants d'interface : on ne le lit que sur html/body.
        if let Some(v) = el.attr("data-version").filter(|_| matches!(name, "html" | "body")) {
            if !v.trim().is_empty() {
                m.versions.push((1, v.trim().to_string()));
            }
        }
        if let Some(v) = el.attr("data-crate-version") {
            if !v.trim().is_empty() {
                m.versions.push((0, v.trim().to_string()));
            }
        }
        let parent_is_svg = node.parent().and_then(|p| p.value().as_element()).map(|p| p.name() == "svg").unwrap_or(false);
        match name {
            "html" => {
                m.lang = el.attr("lang").or_else(|| el.attr("xml:lang")).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
            }
            "title" if m.title_tag.is_none() && !parent_is_svg => {
                let t: String = ElementRef::wrap(node).map(|e| e.text().collect::<String>()).unwrap_or_default();
                let mut s = String::new();
                push_text(&mut s, &t);
                if !s.trim().is_empty() {
                    m.title_tag = Some(s.trim().to_string());
                }
            }
            "meta" => {
                let key = el.attr("name").or_else(|| el.attr("property")).unwrap_or("").to_ascii_lowercase();
                let content = el.attr("content").unwrap_or("").trim();
                match key.as_str() {
                    "generator" => m.generator.push_str(&format!(" {}", content.to_ascii_lowercase())),
                    "og:title" if m.og_title.is_none() && !content.is_empty() => m.og_title = Some(content.to_string()),
                    "docsearch:version" if !content.is_empty() => m.versions.push((0, content.to_string())),
                    "docusaurus_version" if !content.is_empty() => {
                        m.versions.push((if content == "current" { 6 } else { 1 }, content.to_string()))
                    }
                    "version" | "doc-version" | "mike-version" | "project-version" if !content.is_empty() => {
                        m.versions.push((2, content.to_string()))
                    }
                    "hostname" => m.hostname = content.to_ascii_lowercase(),
                    _ => {}
                }
                if key.is_empty()
                    && el.attr("http-equiv").map(|h| h.eq_ignore_ascii_case("content-language")).unwrap_or(false)
                    && m.lang.is_none()
                {
                    m.lang = Some(content.to_string()).filter(|s| !s.is_empty());
                }
            }
            "link" => {
                if el.attr("rel").map(|r| r.split_whitespace().any(|x| x.eq_ignore_ascii_case("canonical"))).unwrap_or(false)
                    && m.canonical.is_none()
                {
                    m.canonical = el.attr("href").map(|s| s.trim().to_string());
                }
            }
            "base" => {
                if m.base.is_none() {
                    m.base = el.attr("href").map(|s| s.trim().to_string());
                }
            }
            "noscript" => {
                if let Some(e) = ElementRef::wrap(node) {
                    let t = e.text().collect::<String>().to_ascii_lowercase();
                    if t.contains("javascript")
                        && ["enable", "required", "need", "turn on", "disabled", "must", "activez", "activer"].iter().any(|k| t.contains(k))
                    {
                        m.noscript_js = true;
                    }
                }
            }
            "script" => {
                if el.attr("src").is_some() {
                    m.script_srcs += 1;
                } else if let Some(e) = ElementRef::wrap(node) {
                    let t = e.text().collect::<String>();
                    if t.contains("DOCUMENTATION_OPTIONS") {
                        if let Some(v) = sphinx_version(&t) {
                            m.versions.push((1, v));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    m
}

/// `VERSION: '2.31.0'` ou `VERSION = "2.31.0"` dans le script d'options de Sphinx.
fn sphinx_version(script: &str) -> Option<String> {
    let i = script.find("VERSION")?;
    let rest = &script[i + 7..];
    let q = rest.find(['\'', '"'])?;
    if q > 8 {
        return None;
    }
    let quote = rest.as_bytes()[q] as char;
    let rest = &rest[q + 1..];
    let end = rest.find(quote)?;
    let v = rest[..end].trim();
    if v.is_empty() || v.len() > 40 {
        None
    } else {
        Some(v.to_string())
    }
}

// ───────────────────────── Version et URL ─────────────────────────

/// Segment d'URL qui ressemble à un numéro de version (v2, 0.21.3, 1.x, 3.11-beta).
fn is_version_segment(seg: &str, first: bool) -> bool {
    let s = seg.strip_prefix(['v', 'V']).unwrap_or(seg);
    let b = s.as_bytes();
    if b.is_empty() || !b[0].is_ascii_digit() {
        return false;
    }
    let digits_only = s.bytes().all(|c| c.is_ascii_digit());
    if digits_only {
        // « 2024 » est une année ; « v2 » et « /3/ » en tête sont des versions.
        let had_v = s.len() != seg.len();
        return s.len() <= 2 && (had_v || first);
    }
    let mut parts = s.splitn(2, ['-', '+']);
    let core = parts.next().unwrap_or("");
    let tail = parts.next();
    let core_ok = core.split('.').enumerate().all(|(i, p)| {
        if i == 0 {
            !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit())
        } else {
            p == "x" || p == "*" || (!p.is_empty() && p.bytes().all(|c| c.is_ascii_digit()))
        }
    });
    core_ok && tail.map(|t| t.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-')).unwrap_or(true)
}

fn version_from_url(url: &Url) -> (Option<String>, Option<String>) {
    let mut strong = None;
    let mut weak = None;
    if let Some(segs) = url.path_segments() {
        for (i, seg) in segs.enumerate() {
            if seg.is_empty() {
                continue;
            }
            if strong.is_none() && is_version_segment(seg, i == 0) {
                strong = Some(seg.to_string());
            }
            let l = seg.to_ascii_lowercase();
            if weak.is_none()
                && matches!(
                    l.as_str(),
                    "stable" | "latest" | "next" | "main" | "master" | "dev" | "nightly" | "canary" | "current" | "trunk" | "unstable"
                )
            {
                weak = Some(seg.to_string());
            }
        }
    }
    (strong, weak)
}

fn pick_version(meta: &Meta, url: &Url) -> Option<String> {
    let (strong, weak) = version_from_url(url);
    let mut cands: Vec<(u8, String)> = meta.versions.iter().filter(|(_, v)| v.len() <= 40).cloned().collect();
    if let Some(s) = strong {
        cands.push((3, s));
    }
    if let Some(w) = weak {
        cands.push((5, w));
    }
    cands.sort_by_key(|(p, _)| *p);
    cands.into_iter().next().map(|(_, v)| v)
}

// ───────────────────────── Famille ─────────────────────────

fn family_of(url: &Url, m: &Meta) -> Family {
    let g = &m.generator;
    let c = &m.classes;
    let has = |k: &str| c.contains(k);
    let has_prefix = |p: &str| c.iter().any(|t| t.starts_with(p));
    if g.contains("docusaurus") || m.ids.contains("__docusaurus") || has("theme-doc-markdown") {
        return Family::Docusaurus;
    }
    if g.contains("vitepress") || has("vp-doc") || has_prefix("vpnav") || has_prefix("vpdoc") {
        return Family::VitePress;
    }
    if g.contains("rustdoc") || has("rustdoc") || m.ids.contains("rustdoc-vars") || has("item-decl") || has("rustdoc-toolbar") {
        return Family::RustDoc;
    }
    if g.contains("mkdocs") || has("md-content") || has("md-typeset") || has_prefix("md-main") {
        return Family::MkDocs;
    }
    if g.contains("sphinx")
        || g.contains("docutils")
        || has("sphinxsidebar")
        || has("wy-nav-content")
        || has("bd-article")
        || has("sig-object")
        || has("rst-content")
    {
        return Family::Sphinx;
    }
    if g.contains("mintlify") || has_prefix("mintlify") || m.ids.contains("content-area") || has("mdx-content") {
        return Family::Mintlify;
    }
    if g.contains("nextra") || has_prefix("nextra") {
        return Family::Nextra;
    }
    if g.contains("gitbook") || has_prefix("gitbook") || url.host_str().map(|h| h.ends_with("gitbook.io")).unwrap_or(false) {
        return Family::GitBook;
    }
    if has("markdown-body")
        && (m.hostname.contains("github") || url.host_str().map(|h| h.ends_with("github.com")).unwrap_or(false) || has("entry-content"))
    {
        return Family::GithubMarkdown;
    }
    Family::Generic
}

// ───────────────────────── Nettoyage ─────────────────────────

const NOISE_TAGS: &[&str] = &[
    "script",
    "style",
    "noscript",
    "template",
    "svg",
    "iframe",
    "form",
    "object",
    "embed",
    "canvas",
    "dialog",
    "select",
    "input",
    "textarea",
    "button",
    "head",
    "clipboard-copy",
    "link",
    "meta",
    "base",
    "title",
    "audio",
    "video",
    "map",
    "picture",
    "source",
    "track",
    "math",
];

const NOISE_EXACT: &[&str] = &[
    "nav",
    "navbar",
    "navigation",
    "menu",
    "menubar",
    "sidebar",
    "side-bar",
    "sidenav",
    "toc",
    "toc-wrapper",
    "table-of-contents",
    "tableofcontents",
    "breadcrumb",
    "breadcrumbs",
    "footer",
    "site-footer",
    "page-footer",
    "cookie",
    "cookies",
    "cookie-banner",
    "cookie-consent",
    "cookiebanner",
    "consent",
    "gdpr",
    "newsletter",
    "social",
    "social-links",
    "share",
    "share-buttons",
    "sharing",
    "advert",
    "ads",
    "advertisement",
    "adsbygoogle",
    "promo",
    "skip-link",
    "skiplink",
    "skip-nav",
    "skip-to-content",
    "skiptocontent",
    "edit-this-page",
    "edit-page",
    "editthispage",
    "pagination",
    "pager",
    "pagination-nav",
    "prev-next",
    "page-nav",
    "announcement",
    "announcement-bar",
    "announcementbar",
    "copy",
    "copy-button",
    "copybutton",
    "copybtn",
    "clipboard",
    "clean-btn",
    "btn-copy",
    "code-copy",
    "toolbar",
    "search",
    "searchbox",
    "search-box",
    "searchform",
    "related",
    "related-posts",
    "related-articles",
    "feedback",
    "was-this-helpful",
    "last-updated",
    "lastupdated",
    "modal",
    "popup",
    "backdrop",
    "linenos",
    "linenodiv",
    "line-numbers-rows",
    "hljs-ln-numbers",
    "hljs-ln-n",
    "lineno",
    "line-number",
    "gutter",
    "boring",
    "hideme",
    "out-of-band",
    "rightside",
    "viewcode-link",
    "viewcode-back",
    "src",
    "sidebar-elems",
    "mobile-topbar",
    "sidebar-resizer",
    "help-button",
    "settings-menu",
    "search-form",
    "copied",
    "sr-copy",
    "github-corner",
    "back-to-top",
    "backtotop",
    "scroll-top",
    "mw-editsection",
    "next-prev",
    "language-name",
    "example-header",
];

const NOISE_PREFIX: &[&str] = &[
    "theme-doc-sidebar",
    "theme-doc-toc",
    "theme-doc-footer",
    "theme-doc-breadcrumbs",
    "theme-edit-this",
    "theme-last-updated",
    "theme-doc-version-banner",
    "theme-announcement",
    "docsidebar",
    "navbar",
    "footer__",
    "footer-",
    "md-nav",
    "md-sidebar",
    "md-header",
    "md-footer",
    "md-search",
    "md-top",
    "md-tabs",
    "md-source",
    "md-feedback",
    "md-skip",
    "md-dialog",
    "md-consent",
    "md-banner",
    "md-clipboard",
    "md-code__nav",
    "md-announce",
    "wy-nav-side",
    "wy-nav-top",
    "wy-side",
    "wy-menu",
    "rst-footer",
    "rst-versions",
    "sphinxsidebar",
    "vpnav",
    "vpsidebar",
    "vpfooter",
    "vpdocfooter",
    "vpdocaside",
    "vplocalnav",
    "vpskiplink",
    "vpbackdrop",
    "vpmenu",
    "vpflyout",
    "vpswitch",
    "vpsocial",
    "vpcontentfooter",
    "vpnavscreen",
    "bd-sidebar",
    "bd-toc",
    "bd-header",
    "bd-footer",
    "toc-",
    "sidebar-",
    "breadcrumb-",
    "cookie-",
    "nextra-nav",
    "nextra-sidebar",
    "nextra-toc",
    "nextra-banner",
    "mintlify-nav",
    "gitbook-nav",
    "readthedocs-",
    "ethical-",
    "rustdoc-toolbar",
    "theme-doc-version",
    "baseline-",
    "document-toc",
    "feedback",
];

const NOISE_IDS: &[&str] = &[
    "toc",
    "sidebar",
    "nav",
    "navigation",
    "footer",
    "breadcrumbs",
    "cookie-banner",
    "cookie-consent",
    "skip-link",
    "table-of-contents",
    "toc-wrapper",
    "site-footer",
    "header-nav",
    "page-nav",
    "menu",
    "search",
    "searchbox",
    "rtd-search-form",
    "sidebar-wrapper",
    "cookiebanner",
];

struct CleanCx {
    has_tabs: bool,
}

fn exact_set() -> &'static HashSet<&'static str> {
    static S: OnceLock<HashSet<&'static str>> = OnceLock::new();
    S.get_or_init(|| NOISE_EXACT.iter().copied().collect())
}

fn class_noise(v: &scraper::node::Element) -> bool {
    let set = exact_set();
    for t in tokens(v) {
        if set.contains(t.as_str()) || NOISE_PREFIX.iter().any(|p| t.starts_with(p)) {
            return true;
        }
    }
    false
}

fn id_noise(v: &scraper::node::Element) -> bool {
    match v.id() {
        Some(i) => {
            let l = i.to_ascii_lowercase();
            NOISE_IDS.contains(&l.as_str()) || l.starts_with("readthedocs-") || l.starts_with("ethical")
        }
        None => false,
    }
}

/// Style en ligne qui masque l'élément (display:none, visibility:hidden, font-size:0, opacity:0…).
fn style_hides(style: &str) -> bool {
    let s: String = style.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
    if s.contains("display:none") || s.contains("visibility:hidden") || s.contains("color:transparent") {
        return true;
    }
    if s.contains("left:-9999") || s.contains("top:-9999") || s.contains("text-indent:-9999") || s.contains("margin-left:-9999") {
        return true;
    }
    let zero_then = |key: &str| -> bool {
        let mut from = 0;
        while let Some(i) = s[from..].find(key) {
            let end = from + i + key.len();
            let next = s[end..].chars().next();
            let ok = match next {
                None | Some(';') | Some('!') | Some('p') | Some('e') | Some('r') | Some('%') | Some('v') => true,
                _ => false,
            };
            if ok {
                return true;
            }
            from = end;
        }
        false
    };
    if zero_then("font-size:0") {
        return true;
    }
    // opacity:0 mais pas 0.5 ; un 0 suivi de rien, de `;` ou de `!`.
    let mut from = 0;
    while let Some(i) = s[from..].find("opacity:0") {
        let end = from + i + 9;
        match s[end..].chars().next() {
            None | Some(';') | Some('!') => return true,
            _ => {}
        }
        from = end;
    }
    if s.contains("height:0") && s.contains("overflow:hidden") && !s.contains("height:0.") {
        return true;
    }
    s.contains("clip:rect(0") || s.contains("width:0;height:0") || s.contains("height:0;width:0")
}

fn is_hidden(el: ElementRef, cx: &CleanCx) -> bool {
    let v = el.value();
    if matches!(v.name(), "html" | "body") {
        return false;
    }
    if v.attr("hidden").is_some() && !(v.attr("role") == Some("tabpanel") && cx.has_tabs) {
        return true;
    }
    if v.attr("aria-hidden").map(|a| a.trim().eq_ignore_ascii_case("true")).unwrap_or(false) {
        return true;
    }
    if v.attr("type") == Some("hidden") {
        return true;
    }
    v.attr("style").map(style_hides).unwrap_or(false)
}

/// Vrai si le sous-arbre contient le contenu principal (main, h1) : on le parcourt au lieu de le supprimer.
fn holds_content(el: ElementRef) -> bool {
    el.descendants()
        .filter_map(ElementRef::wrap)
        .any(|e| matches!(e.value().name(), "main" | "h1") || e.value().attr("role") == Some("main"))
}

fn has_heading(el: ElementRef) -> bool {
    el.descendants().filter_map(ElementRef::wrap).any(|e| matches!(e.value().name(), "h1" | "h2" | "h3"))
}

fn form_has_content(el: ElementRef) -> bool {
    let mut p = 0;
    for e in el.descendants().filter_map(ElementRef::wrap) {
        match e.value().name() {
            "h1" | "h2" => return true,
            "p" => {
                p += 1;
                if p >= 3 {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

fn is_edit_text(t: &str) -> bool {
    let l = t.trim().to_lowercase();
    if l.is_empty() || l.chars().count() > 60 {
        return false;
    }
    const EXACT: &[&str] =
        &["copy", "copied", "copied!", "copy code", "copy to clipboard", "copy item path", "copy page", "copy link", "copy markdown"];
    if EXACT.contains(&l.as_str()) {
        return true;
    }
    const PHRASES: &[&str] = &[
        "edit this page",
        "edit on github",
        "edit page",
        "edit this file",
        "edit source",
        "suggest edits",
        "suggest an edit",
        "improve this page",
        "report an issue",
        "view page source",
        "view source",
        "show source",
        "edit on gitlab",
        "edit on bitbucket",
        "open an issue",
        "give feedback",
        "was this page helpful",
        "was this helpful",
        "skip to main content",
        "skip to content",
        "back to top",
        "scroll to top",
        "modifier cette page",
        "last updated",
        "last modified",
        "dernière mise à jour",
    ];
    PHRASES.iter().any(|p| l.starts_with(p))
}

fn collect_noise(n: NodeRef<Node>, in_content: bool, cx: &CleanCx, out: &mut Vec<NodeId>, depth: usize) {
    if depth > 400 {
        return;
    }
    for child in n.children() {
        let Some(el) = ElementRef::wrap(child) else { continue };
        if is_noise(el, in_content, cx) {
            out.push(child.id());
            continue;
        }
        let v = el.value();
        let ic = in_content || matches!(v.name(), "main" | "article") || v.attr("role") == Some("main");
        collect_noise(child, ic, cx, out, depth + 1);
    }
}

fn is_noise(el: ElementRef, in_content: bool, cx: &CleanCx) -> bool {
    let v = el.value();
    let tag = v.name();
    if matches!(tag, "html" | "body") {
        return false;
    }
    if tag == "form" {
        return !form_has_content(el);
    }
    if tag == "button" && v.attr("role") == Some("tab") {
        return false;
    }
    if NOISE_TAGS.contains(&tag) {
        return true;
    }
    match tag {
        "nav" | "footer" => return true,
        "aside" => return admonition_info(el).is_none(),
        "header" => return !(in_content && has_heading(el)),
        _ => {}
    }
    if is_hidden(el, cx) {
        return true;
    }
    if let Some(r) = v.attr("role") {
        if matches!(r, "navigation" | "banner" | "contentinfo" | "search" | "menu" | "menubar" | "toolbar" | "dialog" | "alertdialog") {
            return true;
        }
        if r == "complementary" {
            return admonition_info(el).is_none();
        }
    }
    let protected = matches!(tag, "main" | "article") || v.attr("role") == Some("main");
    if !protected && (class_noise(v) || id_noise(v)) && !holds_content(el) {
        return true;
    }
    // `span.lang` d'un groupe de code VitePress.
    if tag == "span" && tokens(v).iter().any(|t| t == "lang") {
        if let Some(p) = el.parent().and_then(ElementRef::wrap) {
            if tokens(p.value()).iter().any(|t| t.starts_with("language-")) {
                return true;
            }
        }
    }
    if matches!(tag, "a" | "span" | "div" | "p" | "li") {
        if let Some(t) = short_text(el, 70) {
            if is_edit_text(&t) {
                return true;
            }
        }
    }
    false
}

fn clean(doc: &mut Html, cx: &CleanCx) {
    let mut ids = Vec::new();
    collect_noise(doc.tree.root(), false, cx, &mut ids, 0);
    for id in ids {
        if let Some(mut n) = doc.tree.get_mut(id) {
            n.detach();
        }
    }
}

// ───────────────────────── Liens ─────────────────────────

fn collect_links(doc: &Html, page_url: &Url, base: Option<&str>) -> Vec<Url> {
    let base_url = base.and_then(|b| page_url.join(b).ok()).unwrap_or_else(|| page_url.clone());
    let cx = CleanCx { has_tabs: true };
    let mut out: Vec<Url> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut self_url = page_url.clone();
    self_url.set_fragment(None);
    let mut stack: Vec<NodeRef<Node>> = vec![doc.tree.root()];
    while let Some(n) = stack.pop() {
        if let Some(el) = ElementRef::wrap(n) {
            let tag = el.value().name();
            if matches!(tag, "script" | "style" | "template" | "noscript" | "svg") || is_hidden(el, &cx) {
                continue;
            }
            if matches!(tag, "a" | "area") {
                if let Some(href) = el.value().attr("href") {
                    let h = href.trim();
                    if !h.is_empty() {
                        if let Ok(mut u) = base_url.join(h) {
                            u.set_fragment(None);
                            if matches!(u.scheme(), "http" | "https") && u != self_url && seen.insert(u.as_str().to_string()) {
                                out.push(u);
                                if out.len() >= 5000 {
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
        let kids: Vec<_> = n.children().collect();
        for k in kids.into_iter().rev() {
            stack.push(k);
        }
    }
    out
}

// ───────────────────────── Texte brut et inline ─────────────────────────

const BLOCKISH: &[&str] = &[
    "p",
    "div",
    "li",
    "ul",
    "ol",
    "tr",
    "td",
    "th",
    "dd",
    "dt",
    "section",
    "article",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "pre",
    "table",
    "blockquote",
    "figure",
    "figcaption",
    "details",
    "summary",
    "dl",
    "header",
    "main",
];

const INLINE_TAGS: &[&str] = &[
    "a", "abbr", "acronym", "b", "bdi", "bdo", "big", "br", "cite", "code", "data", "del", "dfn", "em", "font", "i", "img", "ins", "kbd",
    "label", "mark", "q", "s", "samp", "small", "span", "strike", "strong", "sub", "sup", "time", "tt", "u", "var", "wbr",
];

const PERMALINK_CLASSES: &[&str] = &[
    "headerlink",
    "hash-link",
    "header-anchor",
    "anchor",
    "anchor-link",
    "anchorlink",
    "permalink",
    "heading-link",
    "direct-link",
    "md-anchor",
    "doc-anchor",
    "heading-anchor",
    "link-anchor",
    "docs-anchor",
    "viewcode-link",
];

fn is_permalink(el: ElementRef) -> bool {
    let v = el.value();
    if v.name() != "a" {
        return false;
    }
    let toks = tokens(v);
    let by_class = PERMALINK_CLASSES.iter().any(|c| has_token(&toks, c));
    let by_href = v.attr("href").map(|h| h.starts_with('#')).unwrap_or(false);
    if !(by_class || by_href) {
        return false;
    }
    // Un lien-ancre dont le texte est un vrai mot (MDN : <a class="heading-anchor">Syntax</a>) porte le titre : on le garde.
    let Some(t) = short_text(el, 40) else { return false };
    let l = t.trim().to_lowercase();
    l.is_empty()
        || l.chars().all(|c| !c.is_alphanumeric())
        || l.starts_with("permalink")
        || l.starts_with("permanent link")
        || l.starts_with("link to this")
        || l.starts_with("direct link to")
        || l.starts_with("section titled")
}

fn plain_node(n: NodeRef<Node>, out: &mut String, d: usize) {
    if d > 200 {
        return;
    }
    match n.value() {
        Node::Text(t) => push_text(out, t),
        Node::Element(e) => {
            let Some(el) = ElementRef::wrap(n) else { return };
            if is_permalink(el) {
                return;
            }
            let name = e.name();
            if matches!(name, "img" | "svg" | "script" | "style") {
                return;
            }
            let blockish = name == "br" || BLOCKISH.contains(&name);
            if blockish && !(out.is_empty() || out.ends_with(' ')) {
                out.push(' ');
            }
            for c in n.children() {
                plain_node(c, out, d + 1);
            }
            if blockish && !(out.is_empty() || out.ends_with(' ')) {
                out.push(' ');
            }
        }
        _ => {}
    }
}

/// Texte brut d'un élément, espaces repliés, permaliens ignorés.
fn plain(el: ElementRef) -> String {
    let mut s = String::new();
    for c in el.children() {
        plain_node(c, &mut s, 0);
    }
    s.trim().to_string()
}

fn code_span(raw: &str) -> String {
    let mut t = String::new();
    push_text(&mut t, raw);
    let t = t.trim();
    if t.is_empty() {
        return String::new();
    }
    if t.contains('`') {
        format!("`` {} ``", t)
    } else {
        format!("`{}`", t)
    }
}

fn is_block_child(e: ElementRef) -> bool {
    let name = e.value().name();
    !INLINE_TAGS.contains(&name)
}

fn inline_node(n: NodeRef<Node>, out: &mut String, d: usize) {
    if d > 120 {
        return;
    }
    match n.value() {
        Node::Text(t) => push_text(out, t),
        Node::Element(e) => {
            let Some(el) = ElementRef::wrap(n) else { return };
            let name = e.name();
            match name {
                "br" => {
                    while out.ends_with(' ') {
                        out.pop();
                    }
                    if !out.is_empty() && !out.ends_with('\n') {
                        out.push('\n');
                    }
                }
                "wbr" | "img" | "svg" | "script" | "style" => {}
                "code" | "kbd" | "samp" | "tt" | "pre" => {
                    let mut raw = String::new();
                    collect_code(n, &mut raw, 0);
                    let s = code_span(&raw);
                    if !s.is_empty() {
                        out.push_str(&s);
                    }
                }
                "strong" | "b" | "em" | "i" | "cite" | "var" | "del" | "s" | "strike" => {
                    let marker = match name {
                        "strong" | "b" => "**",
                        "em" | "i" | "cite" | "var" => "*",
                        _ => "~~",
                    };
                    let mut inner = String::new();
                    for c in n.children() {
                        inline_node(c, &mut inner, d + 1);
                    }
                    let t = inner.trim();
                    if t.is_empty() {
                        if inner.contains(' ') && !(out.is_empty() || out.ends_with(' ')) {
                            out.push(' ');
                        }
                    } else {
                        if inner.starts_with(' ') && !(out.is_empty() || out.ends_with(' ') || out.ends_with('\n')) {
                            out.push(' ');
                        }
                        out.push_str(marker);
                        out.push_str(t);
                        out.push_str(marker);
                        if inner.ends_with(' ') {
                            out.push(' ');
                        }
                    }
                }
                _ => {
                    if is_permalink(el) {
                        return;
                    }
                    // Deux liens collés (fil d'Ariane, pagination) : on les sépare d'une espace.
                    if name == "a"
                        && n.prev_sibling().and_then(|p| p.value().as_element().map(|e| e.name() == "a")).unwrap_or(false)
                        && !(out.is_empty() || out.ends_with(' ') || out.ends_with('\n'))
                    {
                        out.push(' ');
                    }
                    let blockish = BLOCKISH.contains(&name);
                    if blockish && !(out.is_empty() || out.ends_with(' ') || out.ends_with('\n')) {
                        out.push(' ');
                    }
                    for c in n.children() {
                        inline_node(c, out, d + 1);
                    }
                    if blockish && !out.ends_with(' ') && !out.is_empty() {
                        out.push(' ');
                    }
                }
            }
        }
        _ => {}
    }
}

fn tidy_inline(s: String) -> String {
    let lines: Vec<&str> = s.split('\n').map(|l| l.trim()).collect();
    lines.join("\n").trim().to_string()
}

/// Texte brut d'un bloc de code : `<br>` devient un saut de ligne, rien d'autre n'est modifié.
fn collect_code(n: NodeRef<Node>, out: &mut String, d: usize) {
    if d > 200 {
        return;
    }
    match n.value() {
        Node::Text(t) => out.push_str(t),
        Node::Element(e) => {
            if e.name() == "br" {
                out.push('\n');
                return;
            }
            if matches!(e.name(), "button" | "svg" | "script" | "style") {
                return;
            }
            if let Some(el) = ElementRef::wrap(n) {
                if is_permalink(el) {
                    return;
                }
            }
            // Un bloc (div, p, li) pose sa propre ligne : rustdoc place la clause `where` dans un <div>.
            let block = matches!(e.name(), "div" | "p" | "li" | "tr");
            if block && !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            for c in n.children() {
                collect_code(c, out, d + 1);
            }
            if block && !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
        }
        _ => {}
    }
}

fn normalize_code(raw: &str) -> String {
    let s: String = raw
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .chars()
        .filter(|c| !is_zero_width(*c))
        .map(|c| if c == '\u{a0}' { ' ' } else { c })
        .collect();
    let lines: Vec<&str> = s.split('\n').map(|l| l.trim_end()).collect();
    let start = lines.iter().position(|l| !l.is_empty()).unwrap_or(lines.len());
    let end = lines.iter().rposition(|l| !l.is_empty()).map(|i| i + 1).unwrap_or(start);
    lines[start..end].join("\n")
}

fn fence(code: &str, lang: &str) -> String {
    let mut ticks = 3;
    for l in code.lines() {
        let n = l.trim_start().chars().take_while(|c| *c == '`').count();
        if n >= ticks {
            ticks = n + 1;
        }
    }
    let f = "`".repeat(ticks);
    format!("{}{}\n{}\n{}", f, lang, code, f)
}

// ───────────────────────── Langage d'un bloc de code ─────────────────────────

const LANGS: &[&str] = &[
    "rust",
    "python",
    "py",
    "javascript",
    "js",
    "typescript",
    "ts",
    "tsx",
    "jsx",
    "json",
    "yaml",
    "yml",
    "toml",
    "bash",
    "sh",
    "shell",
    "zsh",
    "console",
    "html",
    "css",
    "scss",
    "sql",
    "go",
    "java",
    "c",
    "cpp",
    "csharp",
    "php",
    "ruby",
    "swift",
    "kotlin",
    "diff",
    "text",
    "xml",
    "markdown",
    "md",
    "ini",
    "dockerfile",
    "powershell",
    "ps1",
    "lua",
    "vue",
    "svelte",
    "graphql",
];

fn lang_from_classes(toks: &[String]) -> Option<String> {
    for t in toks {
        for p in ["language-", "lang-", "highlight-source-", "highlight-", "brush:"] {
            if let Some(l) = t.strip_prefix(p) {
                if !l.is_empty() && l != "default" && l != "none" && l != "text-plain" {
                    return Some(l.to_string());
                }
            }
        }
    }
    toks.iter().find(|t| LANGS.contains(&t.as_str())).cloned()
}

fn code_lang(pre: ElementRef) -> String {
    let mut chain: Vec<ElementRef> = vec![pre];
    for c in pre.children().filter_map(ElementRef::wrap) {
        if c.value().name() == "code" {
            chain.push(c);
        }
    }
    let mut p = pre.parent().and_then(ElementRef::wrap);
    for _ in 0..3 {
        match p {
            Some(e) => {
                chain.push(e);
                p = e.parent().and_then(ElementRef::wrap);
            }
            None => break,
        }
    }
    for e in &chain {
        if let Some(l) = e.value().attr("data-language").or_else(|| e.value().attr("data-lang")) {
            let l = l.trim().to_ascii_lowercase();
            if !l.is_empty() {
                return l;
            }
        }
    }
    for e in &chain {
        if let Some(l) = lang_from_classes(&tokens(e.value())) {
            return l.replace("python3", "python");
        }
    }
    String::new()
}

// ───────────────────────── Titres et ancres ─────────────────────────

fn heading_level(el: ElementRef) -> Option<u8> {
    let v = el.value();
    match v.name() {
        "h1" => Some(1),
        "h2" => Some(2),
        "h3" => Some(3),
        "h4" => Some(4),
        "h5" => Some(5),
        "h6" => Some(6),
        _ => {
            if v.attr("role") == Some("heading") {
                v.attr("aria-level").and_then(|s| s.trim().parse::<u8>().ok()).filter(|l| (1..=6).contains(l))
            } else {
                None
            }
        }
    }
}

const GENERIC_IDS: &[&str] = &[
    "content",
    "main",
    "main-content",
    "__next",
    "root",
    "app",
    "wrapper",
    "page",
    "__docusaurus",
    "readme",
    "article",
    "body",
    "container",
    "docs",
    "doc-content",
    "main-wrapper",
    "layout",
];

fn heading_anchor(h: ElementRef) -> Option<String> {
    if let Some(id) = h.value().id().map(str::trim).filter(|s| !s.is_empty()) {
        return Some(clean_id(id));
    }
    // Ancre enfant : <a id=...> ou <a href="#x">.
    for c in h.descendants().filter_map(ElementRef::wrap) {
        if c.value().name() == "a" {
            if let Some(id) = c.value().id().map(str::trim).filter(|s| !s.is_empty()) {
                return Some(clean_id(id));
            }
            if let Some(f) = c.value().attr("href").and_then(|x| x.strip_prefix('#')).filter(|f| !f.is_empty() && !f.contains('/')) {
                return Some(clean_id(f));
            }
        }
    }
    // Ancre voisine (GitHub : <div class="markdown-heading"><h2/><a class="anchor" id=…/>).
    for sib in [h.prev_siblings().find_map(ElementRef::wrap), h.next_siblings().find_map(ElementRef::wrap)].into_iter().flatten() {
        if sib.value().name() == "a" && PERMALINK_CLASSES.iter().any(|c| has_token(&tokens(sib.value()), c)) {
            if let Some(id) = sib.value().id().map(str::trim).filter(|s| !s.is_empty()) {
                return Some(clean_id(id));
            }
            if let Some(f) = sib.value().attr("href").and_then(|x| x.strip_prefix('#')).filter(|f| !f.is_empty()) {
                return Some(clean_id(f));
            }
        }
    }
    // Section parente portant l'id (Sphinx, rustdoc).
    let p = h.parent().and_then(ElementRef::wrap)?;
    let ptoks = tokens(p.value());
    if p.value().name() == "section" || (p.value().name() == "div" && has_token(&ptoks, "section")) {
        let id = p.value().id().map(str::trim).filter(|s| !s.is_empty())?;
        if GENERIC_IDS.contains(&id.to_ascii_lowercase().as_str()) {
            return None;
        }
        let first = p.children().filter_map(ElementRef::wrap).find(|c| heading_level(*c).is_some());
        if first.map(|f| f.id() == h.id()).unwrap_or(true) {
            return Some(clean_id(id));
        }
    }
    None
}

// ───────────────────────── Admonitions ─────────────────────────

struct Adm {
    label: String,
    known: bool,
    title: Option<NodeId>,
    custom_title: Option<String>,
}

const ADM_CONTAINERS: &[&str] = &[
    "admonition",
    "theme-admonition",
    "callout",
    "custom-block",
    "notecard",
    "markdown-alert",
    "starlight-aside",
    "hint",
    "alert",
    "nextra-callout",
    "stab",
    "mdx-callout",
];

const ADM_PREFIXES: &[&str] = &[
    "theme-admonition-",
    "admonition-",
    "markdown-alert-",
    "starlight-aside--",
    "alert--",
    "alert-",
    "callout--",
    "callout-",
    "notecard-",
    "custom-block-",
    "hint-",
    "stab-",
];

fn adm_label(t: &str) -> Option<&'static str> {
    Some(match t {
        "note" => "Note",
        "tip" => "Tip",
        "hint" => "Hint",
        "info" | "information" => "Info",
        "warning" | "warn" => "Warning",
        "danger" => "Danger",
        "error" => "Error",
        "failure" => "Failure",
        "bug" => "Bug",
        "caution" => "Caution",
        "important" => "Important",
        "attention" => "Attention",
        "seealso" | "see-also" => "See also",
        "deprecated" => "Deprecated",
        "versionadded" => "New",
        "versionchanged" => "Changed",
        "versionremoved" => "Removed",
        "success" => "Success",
        "check" => "Check",
        "example" => "Example",
        "experimental" => "Experimental",
        "unstable" => "Unstable",
        "question" => "Question",
        "abstract" | "summary" => "Summary",
        "todo" => "Todo",
        _ => return None,
    })
}

fn emoji_label(s: &str) -> Option<&'static str> {
    let c = s.trim_start().chars().next()?;
    match c {
        '⚠' => Some("Warning"),
        '💡' => Some("Tip"),
        'ℹ' => Some("Info"),
        '🚫' | '⛔' => Some("Danger"),
        '📝' => Some("Note"),
        _ => None,
    }
}

fn admonition_info(el: ElementRef) -> Option<Adm> {
    let v = el.value();
    let tag = v.name();
    if !matches!(tag, "div" | "aside" | "section" | "details" | "blockquote") {
        return None;
    }
    let toks = tokens(v);
    let container = toks.iter().any(|t| ADM_CONTAINERS.contains(&t.as_str()));
    let attr_type = ["data-callout-type", "data-callout", "data-style", "data-type", "data-variant"]
        .iter()
        .find_map(|a| v.attr(a))
        .map(|s| s.trim().to_ascii_lowercase());
    let mut label: Option<&'static str> = attr_type.as_deref().and_then(adm_label);
    for t in &toks {
        if label.is_some() {
            break;
        }
        let mut cand = t.as_str();
        for p in ADM_PREFIXES {
            if let Some(r) = t.strip_prefix(p) {
                cand = r;
                break;
            }
        }
        if let Some(l) = adm_label(cand) {
            // Les jetons nus (note, warning…) ne comptent qu'avec un conteneur reconnu, ou pour Sphinx.
            let bare_ok = container
                || matches!(cand, "versionadded" | "versionchanged" | "versionremoved" | "seealso" | "deprecated")
                || (matches!(tag, "div" | "aside")
                    && cand == t.as_str()
                    && matches!(cand, "note" | "warning" | "danger" | "caution" | "important" | "tip"));
            if bare_ok && !(cand == "alert" || cand == "hint" && !container) {
                label = Some(l);
                break;
            }
        }
    }
    if label.is_none() && tag == "aside" {
        label = v.attr("aria-label").and_then(|a| adm_label(&a.trim().to_ascii_lowercase()));
    }
    let bare_only = label.is_some()
        && !container
        && !matches!(label, Some("New") | Some("Changed") | Some("Removed") | Some("See also") | Some("Deprecated"));
    if bare_only && text_len(el) > 2500 {
        return None;
    }
    if !container && label.is_none() {
        return None;
    }
    if !container && attr_type.is_none() && label.is_none() {
        return None;
    }
    // Titre : un des deux premiers enfants dont la classe contient « title » ou « heading », ou <summary>.
    let mut title = None;
    for c in el.children().filter_map(ElementRef::wrap).take(2) {
        let ct = tokens(c.value());
        if c.value().name() == "summary" || ct.iter().any(|t| t.contains("title") || t.contains("heading")) {
            title = Some(c);
            break;
        }
    }
    let title_text =
        title.map(|t| plain(t)).map(|s| s.trim_start_matches(|c: char| !c.is_alphanumeric()).trim().to_string()).filter(|s| !s.is_empty());
    let mut known = label.is_some();
    let mut label = label.map(|s| s.to_string());
    if label.is_none() {
        label = emoji_label(&plain(el)).map(|s| s.to_string());
        known = label.is_some();
    }
    let norm = |s: &str| s.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase();
    let mut custom = None;
    let final_label = match (&label, &title_text) {
        (Some(l), Some(t)) => {
            let nl = norm(l);
            let nt = norm(t);
            let alias_same = nt == nl
                || (nl == "warning" && nt == "warn")
                || (nl == "info" && nt == "information")
                || nt.starts_with(&nl) && nt.len() <= nl.len() + 2;
            if !alias_same {
                custom = Some(t.clone());
            }
            l.clone()
        }
        (Some(l), None) => l.clone(),
        (None, Some(t)) => t.clone(),
        (None, None) => "Note".to_string(),
    };
    Some(Adm { label: final_label, known, title: title.map(|t| t.id()), custom_title: custom })
}

// ───────────────────────── Parcours du DOM → éléments ─────────────────────────

enum Item {
    Heading { level: u8, text: String, anchor: Option<String> },
    Block { md: String, code: u32 },
}

struct Ctx {
    tab_by_id: HashMap<String, String>,
}

struct W<'a> {
    cx: &'a Ctx,
    items: Vec<Item>,
    nested: bool,
    tab_queue: VecDeque<String>,
    sig_depth: u8,
}

fn count_fences(md: &str) -> u32 {
    let mut n = 0;
    let mut open: Option<(char, usize)> = None;
    for l in md.lines() {
        let t = l.trim_start_matches([' ', '>']).trim_start();
        let c = t.chars().next();
        if let Some(c) = c.filter(|c| *c == '`' || *c == '~') {
            let k = t.chars().take_while(|x| *x == c).count();
            if k >= 3 {
                match open {
                    None => {
                        open = Some((c, k));
                        n += 1;
                    }
                    Some((oc, ok)) if oc == c && k >= ok && t.chars().skip(k).all(|x| x.is_whitespace()) => open = None,
                    _ => {}
                }
            }
        }
    }
    n
}

/// Indente un bloc Markdown ; les lignes de code (et leurs clôtures) restent à gauche : le code est recopié à l'identique.
fn indent_md(md: &str, first: &str, rest: &str) -> String {
    let mut out = Vec::new();
    let mut in_fence = false;
    for (i, l) in md.lines().enumerate() {
        let t = l.trim_start();
        let is_fence = t.starts_with("```") || t.starts_with("~~~");
        let pre = if i == 0 { first } else { rest };
        if i == 0 {
            out.push(format!("{}{}", pre, l));
        } else if in_fence || is_fence {
            out.push(l.to_string());
        } else if l.trim().is_empty() {
            out.push(String::new());
        } else {
            out.push(format!("{}{}", pre, l));
        }
        if is_fence {
            in_fence = !in_fence;
        }
    }
    out.join("\n")
}

fn quote_md(md: &str) -> String {
    md.lines().map(|l| if l.trim().is_empty() { ">".to_string() } else { format!("> {}", l) }).collect::<Vec<_>>().join("\n")
}

impl<'a> W<'a> {
    fn new(cx: &'a Ctx) -> Self {
        W { cx, items: Vec::new(), nested: false, tab_queue: VecDeque::new(), sig_depth: 0 }
    }

    fn sub(&self) -> W<'a> {
        W { cx: self.cx, items: Vec::new(), nested: true, tab_queue: VecDeque::new(), sig_depth: self.sig_depth }
    }

    fn into_md(self) -> (String, u32) {
        let mut parts = Vec::new();
        let mut code = 0;
        for it in self.items {
            match it {
                Item::Block { md, code: c } => {
                    code += c;
                    parts.push(md);
                }
                Item::Heading { level, text, .. } => parts.push(format!("**{}**", text).replace("**#**", &"#".repeat(level as usize))),
            }
        }
        (parts.join("\n"), code)
    }

    fn block(&mut self, md: String, code: u32) {
        if !md.trim().is_empty() {
            self.items.push(Item::Block { md, code });
        }
    }

    fn para(&mut self, s: String) {
        let t = s.trim();
        // « ← Précédent  Suivant → » : pagination, jamais du contenu.
        if t.chars().count() < 100 && (t.starts_with(['←', '«', '‹']) || t.ends_with(['→', '»', '›'])) {
            return;
        }
        // Icône seule (⚠️, ℹ️…) d'un encadré : le type est déjà dans l'étiquette.
        if t.chars().count() <= 2 && emoji_label(t).is_some() {
            return;
        }
        if t.chars().count() < 100
            && (t.starts_with("Previous") || t.starts_with("Précédent"))
            && (t.contains("Next") || t.contains("Suivant"))
        {
            return;
        }
        if t.is_empty() || t.chars().all(|c| !c.is_alphanumeric() && !matches!(c, '`' | '*')) && t.chars().count() < 3 {
            return;
        }
        self.items.push(Item::Block { md: t.to_string(), code: 0 });
    }

    fn inline_el(&self, el: ElementRef<'a>) -> String {
        let mut out = String::new();
        for c in el.children() {
            inline_node(c, &mut out, 0);
        }
        tidy_inline(out)
    }

    fn flush(&mut self, run: &mut Vec<NodeRef<'a, Node>>) {
        if run.is_empty() {
            return;
        }
        let mut out = String::new();
        for n in run.drain(..) {
            inline_node(n, &mut out, 0);
        }
        self.para(tidy_inline(out));
    }

    fn children(&mut self, el: ElementRef<'a>, depth: usize, skip: Option<NodeId>) {
        let mut run: Vec<NodeRef<'a, Node>> = Vec::new();
        for child in el.children() {
            if Some(child.id()) == skip {
                continue;
            }
            match child.value() {
                Node::Text(_) => run.push(child),
                Node::Element(_) => {
                    let ce = ElementRef::wrap(child).unwrap();
                    if self.is_inline(ce) {
                        run.push(child);
                    } else {
                        self.flush(&mut run);
                        self.walk(ce, depth + 1);
                    }
                }
                _ => {}
            }
        }
        self.flush(&mut run);
    }

    fn is_inline(&self, el: ElementRef<'a>) -> bool {
        let v = el.value();
        let name = v.name();
        if !INLINE_TAGS.contains(&name) {
            return false;
        }
        if heading_level(el).is_some() {
            return false;
        }
        if name == "code" {
            // <code> multi-lignes hors <pre> : bloc de code.
            let t = el.text().collect::<String>();
            if t.contains('\n')
                && t.trim().len() > 30
                && el.parent().and_then(ElementRef::wrap).map(|p| p.value().name() != "pre").unwrap_or(true)
            {
                return false;
            }
        }
        !el.children().filter_map(ElementRef::wrap).any(is_block_child)
    }

    fn walk(&mut self, el: ElementRef<'a>, depth: usize) {
        if depth > 250 {
            let t = plain(el);
            self.para(t);
            return;
        }
        let v = el.value();
        let tag = v.name();
        if let Some(l) = heading_level(el) {
            self.heading(el, l);
            return;
        }
        // Onglets.
        let toks = tokens(v);
        let role = v.attr("role").unwrap_or("");
        let direct_tabs = el.children().filter_map(ElementRef::wrap).any(|c| c.value().attr("role") == Some("tab"));
        if role == "tablist"
            || direct_tabs
            || has_token(&toks, "tabbed-labels")
            || (has_token(&toks, "tabs")
                && el
                    .parent()
                    .map(|p| ElementRef::wrap(p).map(|e| has_token(&tokens(e.value()), "vp-code-group")).unwrap_or(false))
                    .unwrap_or(false))
        {
            let labels: Vec<String> = el
                .descendants()
                .filter_map(ElementRef::wrap)
                .filter(|e| e.value().attr("role") == Some("tab") || e.value().name() == "label")
                .map(plain)
                .filter(|s| !s.is_empty())
                .collect();
            if !labels.is_empty() {
                self.tab_queue = labels.into();
            }
            return;
        }
        let is_panel = role == "tabpanel"
            || has_token(&toks, "tabbed-block")
            || (has_token(&toks, "tab-pane"))
            || (toks.iter().any(|t| t.starts_with("language-"))
                && el
                    .parent()
                    .map(|p| ElementRef::wrap(p).map(|e| has_token(&tokens(e.value()), "blocks")).unwrap_or(false))
                    .unwrap_or(false));
        if is_panel {
            let label =
                v.attr("aria-labelledby").and_then(|id| self.cx.tab_by_id.get(id.trim()).cloned()).or_else(|| self.tab_queue.pop_front());
            if let Some(l) = label {
                self.para(format!("**{}**", l));
            }
            self.children(el, depth, None);
            return;
        }
        if matches!(tag, "div" | "aside" | "section" | "details" | "blockquote") {
            if let Some(adm) = admonition_info(el) {
                self.admonition(el, adm, depth);
                return;
            }
        }
        match tag {
            "pre" => self.code_block(el),
            "p" => {
                let t = self.inline_el(el);
                self.para(t);
            }
            "ul" | "ol" => self.list(el, depth),
            "table" => {
                if has_token(&toks, "highlighttable") {
                    self.children(el, depth, None);
                } else {
                    self.table(el);
                }
            }
            "tbody" | "thead" | "tr" | "td" | "th" if self.in_highlight_table(el) => self.children(el, depth, None),
            "blockquote" => self.quote(el, depth),
            "dl" => self.dl(el, depth),
            "details" => self.details(el, depth),
            "hr" | "br" | "img" | "wbr" => {}
            _ => {
                if self.is_code_div(el) {
                    let mut raw = String::new();
                    collect_code(*el, &mut raw, 0);
                    let code = normalize_code(&raw);
                    if !code.is_empty() {
                        let lang = code_lang(el);
                        self.block(fence(&code, &lang), 1);
                    }
                } else {
                    self.children(el, depth, None);
                }
            }
        }
    }

    fn in_highlight_table(&self, el: ElementRef<'a>) -> bool {
        el.ancestors()
            .filter_map(ElementRef::wrap)
            .take(4)
            .any(|a| a.value().name() == "table" && has_token(&tokens(a.value()), "highlighttable"))
    }

    /// Bloc de code sans <pre> (composants maison) : classe qui trahit du code et contenu multi-lignes.
    fn is_code_div(&self, el: ElementRef<'a>) -> bool {
        if el.children().filter_map(ElementRef::wrap).any(|c| c.value().name() == "pre") {
            return false;
        }
        let toks = tokens(el.value());
        let hint = toks.iter().any(|t| {
            ["code-block", "codeblock", "code_block", "highlight", "hljs", "prism", "shiki", "codehilite", "sourcecode", "code-snippet"]
                .iter()
                .any(|h| t.contains(h))
        });
        if !hint || !matches!(el.value().name(), "div" | "figure") {
            return false;
        }
        if el.descendants().filter_map(ElementRef::wrap).any(|c| is_block_child(c) && !matches!(c.value().name(), "tbody" | "tr" | "td")) {
            return false;
        }
        let t = el.text().collect::<String>();
        t.contains('\n') && t.trim().len() > 20
    }

    fn heading(&mut self, el: ElementRef<'a>, level: u8) {
        let text = plain(el);
        if text.is_empty() {
            return;
        }
        let toks = tokens(el.value());
        if self.nested {
            self.para(format!("**{}**", text));
            return;
        }
        if level >= 5 {
            self.para(format!("{} {}", "#".repeat(level as usize), text));
            return;
        }
        let anchor = heading_anchor(el);
        self.items.push(Item::Heading { level, text: text.clone(), anchor });
        // rustdoc : l'entrée est une signature, on la garde telle quelle si elle tient sur plusieurs lignes.
        if has_token(&toks, "code-header") {
            let mut raw = String::new();
            collect_code(*el, &mut raw, 0);
            let code = normalize_code(&raw);
            let mut compact = String::new();
            push_text(&mut compact, &code);
            if code.contains('\n') && code != compact {
                self.block(fence(&code, "rust"), 1);
            }
        }
    }

    fn code_block(&mut self, pre: ElementRef<'a>) {
        let mut raw = String::new();
        collect_code(*pre, &mut raw, 0);
        let code = normalize_code(&raw);
        if code.is_empty() {
            return;
        }
        let lang = code_lang(pre);
        // Étiquette d'onglet éventuelle (groupes de code VitePress sans role).
        self.block(fence(&code, &lang), 1);
    }

    fn list(&mut self, el: ElementRef<'a>, depth: usize) {
        let ordered = el.value().name() == "ol";
        let mut n: u32 = el.value().attr("start").and_then(|s| s.trim().parse().ok()).unwrap_or(1);
        let mut lines: Vec<String> = Vec::new();
        let mut code = 0;
        for li in el.children().filter_map(ElementRef::wrap) {
            if li.value().name() != "li" {
                // Un conteneur intermédiaire dans la liste : on le parcourt comme un item.
                if li.value().name() == "ul" || li.value().name() == "ol" {
                    continue;
                }
            }
            let mut sub = self.sub();
            sub.children(li, depth + 1, None);
            let (md, c) = sub.into_md();
            if md.trim().is_empty() {
                continue;
            }
            code += c;
            let marker = if ordered { format!("{}. ", n) } else { "- ".to_string() };
            lines.push(indent_md(md.trim(), &marker, &" ".repeat(marker.len())));
            n += 1;
        }
        if !lines.is_empty() {
            self.block(lines.join("\n"), code);
        }
    }

    fn table(&mut self, el: ElementRef<'a>) {
        let mut rows: Vec<Vec<String>> = Vec::new();
        let mut header_rows = 0;
        let mut caption = String::new();
        for node in el.descendants() {
            let Some(e) = ElementRef::wrap(node) else { continue };
            let name = e.value().name();
            if name == "caption" {
                caption = plain(e);
                continue;
            }
            if name != "tr" {
                continue;
            }
            // Ignore les tableaux imbriqués : le plus proche ancêtre <table> doit être `el`.
            if e.ancestors().filter_map(ElementRef::wrap).find(|a| a.value().name() == "table").map(|a| a.id()) != Some(el.id()) {
                continue;
            }
            let mut cells = Vec::new();
            let mut all_th = true;
            for c in e.children().filter_map(ElementRef::wrap).filter(|c| matches!(c.value().name(), "td" | "th")) {
                if c.value().name() == "td" {
                    all_th = false;
                }
                let mut s = String::new();
                for k in c.children() {
                    inline_node(k, &mut s, 0);
                }
                let s = tidy_inline(s).replace('\n', " ").replace('|', "\\|");
                cells.push(s);
                let span: usize = c.value().attr("colspan").and_then(|x| x.trim().parse().ok()).unwrap_or(1);
                for _ in 1..span.min(20) {
                    cells.push(String::new());
                }
            }
            if cells.is_empty() {
                continue;
            }
            if all_th && rows.len() == header_rows {
                header_rows += 1;
            }
            rows.push(cells);
        }
        if rows.is_empty() || rows.iter().all(|r| r.iter().all(|c| c.is_empty())) {
            return;
        }
        let ncol = rows.iter().map(|r| r.len()).max().unwrap_or(0);
        let mut out = String::new();
        if !caption.is_empty() {
            out.push_str(&format!("**{}**\n\n", caption));
        }
        for (i, r) in rows.iter().enumerate() {
            let mut r = r.clone();
            r.resize(ncol, String::new());
            out.push_str(&format!("| {} |\n", r.join(" | ")));
            if i == 0 {
                out.push_str(&format!("|{}\n", " --- |".repeat(ncol)));
            }
        }
        self.block(out.trim_end().to_string(), 0);
    }

    fn quote(&mut self, el: ElementRef<'a>, depth: usize) {
        let mut sub = self.sub();
        sub.children(el, depth + 1, None);
        let (md, c) = sub.into_md();
        let md = md.trim().to_string();
        if md.is_empty() {
            return;
        }
        // Alerte GitHub rendue en blockquote : « [!NOTE] ».
        if let Some(rest) = md.strip_prefix("[!") {
            if let Some(end) = rest.find(']') {
                if let Some(l) = adm_label(&rest[..end].to_ascii_lowercase()) {
                    let body = rest[end + 1..].trim_start().to_string();
                    let s = quote_md(&format!("**{}:** {}", l, body));
                    self.block(s, c);
                    return;
                }
            }
        }
        self.block(quote_md(&md), c);
    }

    fn admonition(&mut self, el: ElementRef<'a>, adm: Adm, depth: usize) {
        let mut sub = self.sub();
        sub.children(el, depth + 1, adm.title);
        let (md, code) = sub.into_md();
        let mut body = md.trim().to_string();
        // Doublon d'étiquette (« **Note:** » déjà dans le texte).
        for pat in [format!("**{}:**", adm.label), format!("**{}**:", adm.label), format!("**{}**", adm.label)] {
            if body.to_lowercase().starts_with(&pat.to_lowercase()) && body.is_char_boundary(pat.len()) {
                body = body[pat.len()..].trim_start().to_string();
                break;
            }
        }
        let mut head = format!("**{}:**", adm.label);
        if let Some(t) = &adm.custom_title {
            head.push_str(&format!(" *{}*", t.trim_end_matches('.')));
            if !body.is_empty() {
                head.push('.');
            }
        }
        if body.is_empty() && adm.custom_title.is_none() {
            return;
        }
        let first_is_fence = body.trim_start().starts_with("```") || body.trim_start().starts_with("~~~");
        let text = if body.is_empty() {
            head
        } else if first_is_fence {
            format!("{}\n\n{}", head, body)
        } else {
            format!("{} {}", head, body)
        };
        let _ = adm.known;
        self.block(quote_md(&text), code);
    }

    fn details(&mut self, el: ElementRef<'a>, depth: usize) {
        let summary = el.children().filter_map(ElementRef::wrap).find(|c| c.value().name() == "summary");
        if let Some(s) = summary {
            if s.descendants().filter_map(ElementRef::wrap).any(|e| heading_level(e).is_some()) {
                // rustdoc : le titre d'entrée (h3/h4) est dans le <summary>.
                self.children(s, depth, None);
            } else {
                let t = plain(s);
                if !t.is_empty() && !t.eq_ignore_ascii_case("expand description") {
                    self.para(format!("**{}**", t));
                }
            }
        }
        self.children(el, depth, summary.map(|s| s.id()));
    }

    fn dl(&mut self, el: ElementRef<'a>, depth: usize) {
        // Aplatit les <div> de regroupement dt/dd.
        let mut flat: Vec<ElementRef<'a>> = Vec::new();
        for c in el.children().filter_map(ElementRef::wrap) {
            if c.value().name() == "div" {
                flat.extend(c.children().filter_map(ElementRef::wrap));
            } else {
                flat.push(c);
            }
        }
        let dl_toks = tokens(el.value());
        let mut pending: Vec<String> = Vec::new();
        let mut last_sig = false;
        let mut lines: Vec<String> = Vec::new();
        let mut code = 0;
        let flush_lines = |this: &mut Self, lines: &mut Vec<String>, code: &mut u32| {
            if !lines.is_empty() {
                let md = lines.join("\n");
                this.block(md, *code);
                lines.clear();
                *code = 0;
            }
        };
        for c in flat {
            match c.value().name() {
                "dt" => {
                    if !self.nested && is_sig(c) {
                        flush_lines(self, &mut lines, &mut code);
                        self.sig_heading(c, &dl_toks);
                        last_sig = true;
                        pending.clear();
                    } else {
                        last_sig = false;
                        let t = plain(c);
                        if !t.is_empty() {
                            pending.push(t);
                        }
                    }
                }
                "dd" => {
                    if last_sig {
                        flush_lines(self, &mut lines, &mut code);
                        self.sig_depth = self.sig_depth.saturating_add(1);
                        self.children(c, depth + 1, None);
                        self.sig_depth = self.sig_depth.saturating_sub(1);
                        last_sig = false;
                    } else {
                        let mut sub = self.sub();
                        sub.children(c, depth + 1, None);
                        let (md, cc) = sub.into_md();
                        let md = md.trim().to_string();
                        let label = pending.join(", ");
                        pending.clear();
                        if md.is_empty() && label.is_empty() {
                            continue;
                        }
                        code += cc;
                        if label.is_empty() {
                            lines.push(md);
                        } else if !md.contains('\n') {
                            lines.push(format!("- **{}**: {}", label, md));
                        } else {
                            lines.push(format!("- **{}**:\n{}", label, indent_md(&md, "  ", "  ")));
                        }
                    }
                }
                _ => {}
            }
        }
        if !pending.is_empty() {
            for p in pending {
                lines.push(format!("- **{}**", p));
            }
        }
        flush_lines(self, &mut lines, &mut code);
    }

    fn sig_heading(&mut self, dt: ElementRef<'a>, dl_toks: &[String]) {
        let sig = plain(dt);
        if sig.is_empty() {
            return;
        }
        let id = dt.value().id().map(str::trim).filter(|s| !s.is_empty()).map(clean_id);
        let level = (5 + self.sig_depth).min(6);
        let title = id.clone().unwrap_or_else(|| sig.chars().take(80).collect());
        self.items.push(Item::Heading { level, text: title, anchor: id });
        let lang = if has_token(dl_toks, "py") {
            "python"
        } else if has_token(dl_toks, "js") {
            "javascript"
        } else if has_token(dl_toks, "cpp") {
            "cpp"
        } else if has_token(dl_toks, "c") {
            "c"
        } else {
            ""
        };
        self.block(fence(&sig, lang), 1);
    }
}

fn is_sig(dt: ElementRef) -> bool {
    let toks = tokens(dt.value());
    if toks.iter().any(|t| t == "sig" || t == "sig-object") {
        return true;
    }
    dt.value().id().is_some()
        && dt.descendants().filter_map(ElementRef::wrap).any(|e| tokens(e.value()).iter().any(|t| t == "descname" || t == "sig-name"))
}

// ───────────────────────── Sections ─────────────────────────

struct Cur {
    anchor: String,
    path: Vec<String>,
    level: u8,
    blocks: Vec<String>,
    code: u32,
}

fn finish(cur: Option<Cur>, out: &mut Vec<Section>) {
    if let Some(c) = cur {
        out.push(Section { anchor: c.anchor, heading_path: c.path, level: c.level, text: c.blocks.join("\n\n"), code_blocks: c.code });
    }
}

fn build_sections(items: Vec<Item>, page_title: &str) -> Vec<Section> {
    let mut anchors = Anchors::default();
    let mut stack: Vec<(u8, String)> = Vec::new();
    let mut cur: Option<Cur> = None;
    let mut out: Vec<Section> = Vec::new();
    let mut pre: Vec<(String, u32)> = Vec::new();
    let preamble_title = if page_title.is_empty() { "Introduction".to_string() } else { page_title.to_string() };
    let mut started = false;
    for it in items {
        match it {
            Item::Heading { level, text, anchor } => {
                if !started {
                    started = true;
                    if !pre.is_empty() {
                        let a = anchors.unique("top".to_string());
                        let code = pre.iter().map(|p| p.1).sum();
                        out.push(Section {
                            anchor: a,
                            heading_path: vec![preamble_title.clone()],
                            level: 1,
                            text: pre.iter().map(|p| p.0.clone()).collect::<Vec<_>>().join("\n\n"),
                            code_blocks: code,
                        });
                        pre.clear();
                    }
                }
                finish(cur.take(), &mut out);
                stack.retain(|(l, _)| *l < level);
                stack.push((level, text.clone()));
                let a = anchors.unique(anchor.filter(|a| !a.is_empty()).unwrap_or_else(|| slug(&text)));
                cur = Some(Cur { anchor: a, path: stack.iter().map(|(_, t)| t.clone()).collect(), level, blocks: Vec::new(), code: 0 });
            }
            Item::Block { md, code } => match cur.as_mut() {
                Some(c) => {
                    c.blocks.push(md);
                    c.code += code;
                }
                None => pre.push((md, code)),
            },
        }
    }
    finish(cur.take(), &mut out);
    if !started && !pre.is_empty() {
        let a = anchors.unique("top".to_string());
        let code = pre.iter().map(|p| p.1).sum();
        out.push(Section {
            anchor: a,
            heading_path: vec![preamble_title],
            level: 1,
            text: pre.iter().map(|p| p.0.clone()).collect::<Vec<_>>().join("\n\n"),
            code_blocks: code,
        });
    }
    // Une section vide suivie d'une section plus profonde n'est qu'un chapeau : le chemin de titres la porte déjà.
    let mut keep = Vec::with_capacity(out.len());
    for i in 0..out.len() {
        let empty = out[i].text.trim().is_empty();
        let parent_of_next = out.get(i + 1).map(|n| n.level > out[i].level).unwrap_or(false);
        keep.push(!(empty && parent_of_next));
    }
    let out: Vec<Section> = out.into_iter().zip(keep).filter(|(_, k)| *k).map(|(s, _)| s).collect();
    drop_boilerplate(out)
}

/// Rustdoc : « Blanket Implementations » et « Auto Trait Implementations » (`impl<T> From<T> for T`,
/// `impl Send for X`…) sont le même bruit sur toutes les pages (« Returns the argument unchanged. ») et
/// noient les vraies réponses (`equivalent`, `try_from`…) : leurs sections n'entrent pas dans l'index.
/// Les « Trait Implementations » propres à la bibliothèque, elles, restent.
///
/// Fusionner les sections très courtes avec leur voisine (essayé, voir le rapport du lot W-G) n'a rien
/// apporté au classement (section top-1 identique, page top-1 légèrement en baisse) et perd des
/// ancres et des titres que les fixtures exigent : les sections courtes restent des sections.
fn drop_boilerplate(v: Vec<Section>) -> Vec<Section> {
    v.into_iter()
        .filter(|s| {
            let blanket = s.heading_path.iter().any(|h| h == "Blanket Implementations" || h == "Auto Trait Implementations");
            !(blanket && s.heading_path.len() > 2 && s.code_blocks == 0)
        })
        .collect()
}

/// Page d'ITEM rustdoc (`fn.get.html`, `struct.Agent.html`, `trait.Read.html`…) : signature + description
/// + exemples tiennent en peu de mots mais forment une réponse complète.
fn is_rustdoc_item(url: &Url) -> bool {
    let Some(last) = url.path_segments().and_then(|mut s| s.next_back()) else { return false };
    const KINDS: [&str; 12] =
        ["fn.", "struct.", "enum.", "trait.", "type.", "macro.", "constant.", "static.", "union.", "primitive.", "attr.", "derive."];
    last.ends_with(".html") && KINDS.iter().any(|k| last.starts_with(k))
}

// ───────────────────────── Conteneur ─────────────────────────

struct Sels {
    by_family: Vec<(Family, Vec<Selector>)>,
    generic: Vec<Selector>,
    body: Selector,
}

fn parse_all(list: &[&str]) -> Vec<Selector> {
    list.iter().map(|s| Selector::parse(s).expect("sélecteur valide")).collect()
}

fn sels() -> &'static Sels {
    static S: OnceLock<Sels> = OnceLock::new();
    S.get_or_init(|| Sels {
        by_family: vec![
            (Family::Docusaurus, parse_all(&[".theme-doc-markdown", "article", "main"])),
            (Family::VitePress, parse_all(&[".vp-doc", "main .content", "main"])),
            (Family::MkDocs, parse_all(&["article.md-content__inner", ".md-content__inner", ".md-content", "main"])),
            (
                Family::Sphinx,
                parse_all(&["div[role=main]", "article[role=main]", "article.bd-article", "div.body", "div.document", "main", "article"]),
            ),
            (Family::RustDoc, parse_all(&["#main-content", "section.content", "main"])),
            (Family::Mintlify, parse_all(&["#content-area", ".mdx-content", "#content", "main"])),
            (Family::GitBook, parse_all(&["[data-testid=\"page.contentEditor\"]", "main"])),
            (Family::Nextra, parse_all(&[".nextra-content", "main article", "article", "main"])),
            (Family::GithubMarkdown, parse_all(&["article.markdown-body", ".markdown-body", "#readme", "article", "main"])),
        ],
        generic: parse_all(&[
            "article",
            "main",
            "[role=main]",
            "#content",
            ".content",
            ".markdown-body",
            ".post-content",
            ".entry-content",
            ".article-body",
            ".doc-content",
            ".documentation",
            "#main-content",
            ".main-content",
            ".prose",
            ".markdown",
            ".mdx-content",
            ".md-content",
            ".rm-Markdown",
        ]),
        body: Selector::parse("body").unwrap(),
    })
}

fn pick_container<'a>(doc: &'a Html, family: Family) -> (ElementRef<'a>, bool) {
    let s = sels();
    let mut list: Vec<&Selector> = Vec::new();
    if let Some((_, v)) = s.by_family.iter().find(|(f, _)| *f == family) {
        list.extend(v.iter());
    }
    list.extend(s.generic.iter());
    let cands: Vec<Option<(usize, ElementRef<'a>)>> =
        list.iter().map(|sel| doc.select(sel).filter(|e| attached(*e)).map(|e| (text_len(e), e)).max_by_key(|(l, _)| *l)).collect();
    let best = cands.iter().flatten().map(|(l, _)| *l).max().unwrap_or(0);
    if best >= 60 {
        for (len, el) in cands.into_iter().flatten() {
            if len >= 60 && len * 10 >= best * 6 {
                return (el, true);
            }
        }
    }
    // Repli : on part de <body> et on descend tant qu'un seul enfant porte l'essentiel du texte.
    let mut cur = doc.select(&s.body).next().unwrap_or_else(|| doc.root_element());
    loop {
        let total = text_len(cur);
        let next = cur
            .children()
            .filter_map(ElementRef::wrap)
            .filter(|c| !matches!(c.value().name(), "p" | "pre" | "ul" | "ol" | "table" | "li"))
            .map(|c| (text_len(c), c))
            .max_by_key(|(l, _)| *l);
        match next {
            Some((l, c)) if total > 0 && l * 100 >= total * 85 && !matches!(c.value().name(), "h1" | "h2" | "h3") => cur = c,
            _ => break,
        }
    }
    (cur, false)
}

// ───────────────────────── Extraction HTML ─────────────────────────

fn visible_text_len(root: NodeRef<Node>) -> usize {
    let mut n = 0;
    let mut stack = vec![root];
    while let Some(x) = stack.pop() {
        match x.value() {
            Node::Text(t) => n += t.chars().filter(|c| !c.is_whitespace()).count(),
            Node::Element(e) => {
                if matches!(e.name(), "script" | "style" | "noscript" | "template" | "head") {
                    continue;
                }
                stack.extend(x.children());
            }
            Node::Document | Node::Fragment => stack.extend(x.children()),
            _ => {}
        }
    }
    n
}

fn words(s: &str) -> usize {
    s.split_whitespace().count()
}

fn host_eq(a: &Url, b: &Url) -> bool {
    match (a.host_str(), b.host_str()) {
        (Some(x), Some(y)) => x.eq_ignore_ascii_case(y),
        _ => false,
    }
}

fn items_from_container(container: ElementRef, cx: &Ctx) -> Vec<Item> {
    let mut w = W::new(cx);
    w.children(container, 0, None);
    w.items
}

fn extract_html(url: &Url, html: &str) -> Page {
    let mut doc = Html::parse_document(html);
    let meta = read_meta(&doc, url);
    let family = family_of(url, &meta);
    let links = collect_links(&doc, url, meta.base.as_deref());

    // Racine d'application vide (SPA) : mesurée avant le nettoyage.
    let app_root_empty =
        !meta.app_roots.is_empty() && meta.app_roots.iter().all(|id| doc.tree.get(*id).map(|n| visible_text_len(n) < 15).unwrap_or(true));
    let body_visible = visible_text_len(doc.tree.root());

    clean(&mut doc, &CleanCx { has_tabs: meta.has_tabs });

    let (items, container_ok) = {
        let (container, ok) = pick_container(&doc, family);
        let mut tab_by_id = HashMap::new();
        for e in container.descendants().filter_map(ElementRef::wrap) {
            if e.value().attr("role") == Some("tab") {
                if let Some(ctl) = e.value().attr("aria-controls") {
                    tab_by_id.insert(ctl.trim().to_string(), plain(e));
                }
            }
        }
        let cx = Ctx { tab_by_id };
        let mut items = items_from_container(container, &cx);
        // Titre h1 resté hors du conteneur (en-tête de page) : on le remet en tête.
        let has_h1 = items.iter().any(|i| matches!(i, Item::Heading { level: 1, .. }));
        if !has_h1 {
            let sel = Selector::parse("h1").unwrap();
            if let Some(h) = doc.select(&sel).find(|h| attached(*h) && !h.ancestors().any(|a| a.id() == container.id())) {
                let text = plain(h);
                if !text.is_empty() {
                    items.insert(0, Item::Heading { level: 1, text, anchor: heading_anchor(h) });
                }
            }
        }
        (items, ok)
    };

    let h1 = items.iter().find_map(|i| match i {
        Item::Heading { level: 1, text, .. } => Some(text.clone()),
        _ => None,
    });
    let title = h1
        .or_else(|| meta.og_title.clone())
        .or_else(|| meta.title_tag.as_ref().map(|t| trim_site_suffix(t)))
        .unwrap_or_else(|| url.path_segments().and_then(|s| s.filter(|x| !x.is_empty()).last().map(|x| x.to_string())).unwrap_or_default());

    let sections = build_sections(items, &title);
    finalize(url, &meta, title, sections, links, container_ok, Some((app_root_empty, body_visible)))
}

fn trim_site_suffix(t: &str) -> String {
    for sep in [" | ", " – ", " — ", " · ", " - "] {
        if let Some(i) = t.find(sep) {
            if i >= 3 {
                return t[..i].trim().to_string();
            }
        }
    }
    t.trim().to_string()
}

fn finalize(
    url: &Url,
    meta: &Meta,
    title: String,
    sections: Vec<Section>,
    links: Vec<Url>,
    container_ok: bool,
    js: Option<(bool, usize)>,
) -> Page {
    let canonical =
        meta.canonical.as_deref().and_then(|c| url.join(c).ok()).filter(|c| matches!(c.scheme(), "http" | "https") && host_eq(c, url)).map(
            |mut c| {
                c.set_fragment(None);
                c
            },
        );
    let useful_chars: usize =
        sections.iter().map(|s| s.text.chars().count() + s.heading_path.last().map(|t| t.chars().count()).unwrap_or(0)).sum();
    let useful_words: usize = sections.iter().map(|s| words(&s.text) + s.heading_path.last().map(|t| words(t)).unwrap_or(0)).sum();
    let (app_root_empty, body_visible) = js.unwrap_or((false, useful_chars));
    let ratio = if body_visible == 0 { 0.0 } else { (useful_chars as f32 / body_visible.max(1) as f32).min(1.0) };
    let mut quality = 0.35 * (useful_words as f32 / 200.0).min(1.0)
        + 0.25 * (sections.len() as f32 / 4.0).min(1.0)
        + if container_ok { 0.2 } else { 0.0 }
        + 0.2 * (ratio / 0.25).min(1.0);
    quality = quality.clamp(0.0, 1.0);

    let needs_js =
        js.is_some() && useful_words < 30 && (app_root_empty || meta.noscript_js || (meta.script_srcs >= 2 && body_visible < 150));
    let item_page = is_rustdoc_item(url) && container_ok && !sections.is_empty() && useful_words >= 5;
    if item_page {
        quality = quality.max(0.6);
    }
    let completeness = if needs_js {
        Completeness::NeedsJs
    } else if item_page {
        Completeness::Complete
    } else if useful_words < 30 || sections.is_empty() || quality < 0.3 {
        Completeness::Partial
    } else {
        Completeness::Complete
    };
    Page {
        url: url.clone(),
        canonical,
        title,
        lang: meta.lang.clone(),
        version_hint: pick_version(meta, url),
        sections,
        links,
        quality,
        completeness,
        injection_signals: 0,
    }
}

// ───────────────────────── Markdown ─────────────────────────

fn split_front_matter(text: &str) -> (HashMap<String, String>, &str) {
    let mut fm = HashMap::new();
    let Some(rest) = text.strip_prefix("---\n") else { return (fm, text) };
    let mut end = None;
    let mut pos = 0;
    for l in rest.split_inclusive('\n') {
        let t = l.trim_end();
        if t == "---" || t == "..." {
            end = Some(pos + l.len());
            break;
        }
        pos += l.len();
    }
    let Some(e) = end else { return (fm, text) };
    for l in rest[..pos].lines() {
        if let Some((k, v)) = l.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
            if !k.is_empty() && !v.is_empty() && !k.contains(' ') {
                fm.entry(k).or_insert(v);
            }
        }
    }
    (fm, &rest[e..])
}

fn fence_open(line: &str) -> Option<(char, usize)> {
    let t = line.trim_start();
    if line.len() - t.len() > 3 {
        return None;
    }
    let c = t.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let n = t.chars().take_while(|x| *x == c).count();
    if n >= 3 && !(c == '`' && t[n..].contains('`')) {
        Some((c, n))
    } else {
        None
    }
}

fn fence_close(line: &str, open: (char, usize)) -> bool {
    let t = line.trim();
    let n = t.chars().take_while(|x| *x == open.0).count();
    n >= open.1 && t.chars().skip(n).all(|c| c.is_whitespace())
}

/// Retire les commentaires HTML (hors blocs de code) : ils peuvent porter des instructions cachées.
fn strip_html_comments(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    let mut in_comment = false;
    for line in text.lines() {
        if let Some(o) = fence {
            if fence_close(line, o) {
                fence = None;
            }
            out.push(line.to_string());
            continue;
        }
        if !in_comment {
            if let Some(o) = fence_open(line) {
                fence = Some(o);
                out.push(line.to_string());
                continue;
            }
        }
        let mut res = String::new();
        let mut rest = line;
        loop {
            if in_comment {
                match rest.find("-->") {
                    Some(i) => {
                        rest = &rest[i + 3..];
                        in_comment = false;
                    }
                    None => break,
                }
            } else {
                match rest.find("<!--") {
                    Some(i) => {
                        res.push_str(&rest[..i]);
                        rest = &rest[i + 4..];
                        in_comment = true;
                    }
                    None => {
                        res.push_str(rest);
                        break;
                    }
                }
            }
        }
        if !(res.trim().is_empty() && line.trim() != res.trim()) {
            out.push(res);
        }
    }
    out
}

fn md_plain_heading(t: &str) -> String {
    let mut s = String::new();
    let mut rest = t;
    // [texte](url) -> texte
    while let Some(i) = rest.find('[') {
        s.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        match (after.find("]("), after.find(')')) {
            (Some(a), Some(b)) if a < b => {
                s.push_str(&after[..a]);
                rest = &after[b + 1..];
            }
            _ => {
                s.push('[');
                rest = after;
            }
        }
    }
    s.push_str(rest);
    let s = s.replace('`', "");
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    let mut r = String::new();
    push_text(&mut r, &out);
    r.trim().to_string()
}

fn atx_heading(line: &str) -> Option<(u8, String)> {
    let t = line.trim_start();
    if line.len() - t.len() > 3 {
        return None;
    }
    let n = t.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&n) {
        return None;
    }
    let rest = &t[n..];
    if !(rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t')) {
        return None;
    }
    let rest = rest.trim();
    let rest = rest.trim_end_matches('#').trim_end();
    Some((n as u8, rest.to_string()))
}

fn directive_label(kind: &str) -> String {
    adm_label(&kind.to_ascii_lowercase()).map(|s| s.to_string()).unwrap_or_else(|| {
        let mut c = kind.chars();
        match c.next() {
            Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            None => "Note".to_string(),
        }
    })
}

fn quote_directive(label: &str, title: &str, body: &[String]) -> String {
    let body_txt = body.iter().map(|l| l.trim_end()).collect::<Vec<_>>().join("\n");
    let body_txt = body_txt.trim().to_string();
    let mut head = format!("**{}:**", label);
    if !title.is_empty() && !title.eq_ignore_ascii_case(label) {
        head.push_str(&format!(" *{}*", title.trim_end_matches('.')));
        if !body_txt.is_empty() {
            head.push('.');
        }
    }
    let t = if body_txt.is_empty() {
        head
    } else if body_txt.starts_with("```") {
        format!("{}\n\n{}", head, body_txt)
    } else {
        format!("{} {}", head, body_txt)
    };
    quote_md(&t)
}

const MDX_TAGS: &[&str] = &["Note", "Tip", "Warning", "Info", "Check", "Danger", "Caution"];

fn md_items(lines: &[String]) -> Vec<Item> {
    let mut items: Vec<Item> = Vec::new();
    let mut buf: Vec<String> = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    let flush = |buf: &mut Vec<String>, items: &mut Vec<Item>| {
        let text = buf.join("\n");
        let t = text.trim_matches('\n').trim_end().to_string();
        buf.clear();
        if !t.trim().is_empty() {
            let code = count_fences(&t);
            items.push(Item::Block { md: t, code });
        }
    };
    let mut i = 0;
    while i < lines.len() {
        let line = &lines[i];
        if let Some(o) = fence {
            if fence_close(line, o) {
                fence = None;
            }
            buf.push(line.clone());
            i += 1;
            continue;
        }
        if let Some(o) = fence_open(line) {
            fence = Some(o);
            buf.push(line.clone());
            i += 1;
            continue;
        }
        let t = line.trim();
        // :::warning Titre … :::
        if let Some(rest) = t.strip_prefix(":::") {
            let rest = rest.trim();
            if !rest.is_empty() && rest.chars().next().map(|c| c.is_alphabetic()).unwrap_or(false) {
                let mut parts = rest.splitn(2, char::is_whitespace);
                let kind = parts.next().unwrap_or("note");
                let title = parts.next().unwrap_or("").trim().trim_matches(|c| c == '[' || c == ']').to_string();
                let mut body = Vec::new();
                let mut depth = 1;
                let mut j = i + 1;
                while j < lines.len() {
                    let tj = lines[j].trim();
                    if tj.starts_with(":::") && tj.len() > 3 && tj[3..].trim().chars().next().map(|c| c.is_alphabetic()).unwrap_or(false) {
                        depth += 1;
                    } else if tj == ":::" {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    body.push(lines[j].clone());
                    j += 1;
                }
                buf.push(String::new());
                buf.push(quote_directive(&directive_label(kind), &title, &body));
                buf.push(String::new());
                i = j + 1;
                continue;
            }
        }
        // !!! warning "Titre" (MkDocs) et ??? (repliable) : corps indenté de 4 espaces.
        if t.starts_with("!!!") || t.starts_with("???") {
            let rest = t[3..].trim_start_matches('+').trim();
            let mut parts = rest.splitn(2, char::is_whitespace);
            let kind = parts.next().unwrap_or("note");
            if !kind.is_empty() {
                let title = parts.next().unwrap_or("").trim().trim_matches('"').to_string();
                let mut body = Vec::new();
                let mut j = i + 1;
                while j < lines.len() {
                    let lj = &lines[j];
                    if lj.trim().is_empty() {
                        body.push(String::new());
                    } else if let Some(r) = lj.strip_prefix("    ").or_else(|| lj.strip_prefix('\t')) {
                        body.push(r.to_string());
                    } else {
                        break;
                    }
                    j += 1;
                }
                buf.push(String::new());
                buf.push(quote_directive(&directive_label(kind), &title, &body));
                buf.push(String::new());
                i = j;
                continue;
            }
        }
        // <Note> … </Note> (Mintlify, MDX).
        if let Some(tag) = MDX_TAGS.iter().find(|tg| t.starts_with(&format!("<{}>", tg)) || t == format!("<{}", tg)) {
            let close = format!("</{}>", tag);
            let open = format!("<{}>", tag);
            let mut body = Vec::new();
            let first = t.strip_prefix(&open).unwrap_or("").to_string();
            let mut j = i;
            let mut done = false;
            if let Some(idx) = first.find(&close) {
                body.push(first[..idx].to_string());
                done = true;
            } else {
                body.push(first);
                j += 1;
                while j < lines.len() {
                    if let Some(idx) = lines[j].find(&close) {
                        body.push(lines[j][..idx].to_string());
                        done = true;
                        break;
                    }
                    body.push(lines[j].clone());
                    j += 1;
                }
            }
            if done {
                buf.push(String::new());
                buf.push(quote_directive(&directive_label(tag), "", &body));
                buf.push(String::new());
                i = j + 1;
                continue;
            }
        }
        // > [!NOTE] (GitHub).
        if let Some(rest) = t.strip_prefix('>') {
            let r = rest.trim_start();
            if let Some(x) = r.strip_prefix("[!") {
                if let Some(end) = x.find(']') {
                    if let Some(l) = adm_label(&x[..end].to_ascii_lowercase()) {
                        let mut body = vec![x[end + 1..].trim().to_string()];
                        let mut j = i + 1;
                        while j < lines.len() && lines[j].trim_start().starts_with('>') {
                            body.push(lines[j].trim_start().trim_start_matches('>').trim_start().to_string());
                            j += 1;
                        }
                        buf.push(String::new());
                        buf.push(quote_directive(l, "", &body));
                        buf.push(String::new());
                        i = j;
                        continue;
                    }
                }
            }
        }
        // Titres ATX.
        if let Some((lvl, raw)) = atx_heading(line) {
            if lvl <= 4 {
                flush(&mut buf, &mut items);
                items.push(md_heading(lvl, &raw));
                i += 1;
                continue;
            }
        }
        // Titres setext (ligne puis === ou ---).
        if i + 1 < lines.len()
            && !t.is_empty()
            && !t.starts_with(['-', '*', '+', '>', '|', '<'])
            && (i == 0 || lines[i - 1].trim().is_empty())
        {
            let nx = lines[i + 1].trim();
            let is_eq = nx.len() >= 2 && nx.chars().all(|c| c == '=');
            let is_dash = nx.len() >= 2 && nx.chars().all(|c| c == '-');
            if is_eq || is_dash {
                flush(&mut buf, &mut items);
                items.push(md_heading(if is_eq { 1 } else { 2 }, t));
                i += 2;
                continue;
            }
        }
        buf.push(line.clone());
        i += 1;
    }
    flush(&mut buf, &mut items);
    items
}

fn md_heading(level: u8, raw: &str) -> Item {
    let mut raw = raw.trim().to_string();
    let mut anchor = None;
    if raw.ends_with('}') {
        if let Some(i) = raw.rfind("{#") {
            let id = raw[i + 2..raw.len() - 1].trim().to_string();
            if !id.is_empty() && !id.contains(' ') {
                anchor = Some(id);
                raw = raw[..i].trim_end().to_string();
            }
        }
    }
    let text = md_plain_heading(&raw);
    Item::Heading { level, text, anchor }
}

fn md_links(lines: &[String], base: &Url) -> Vec<Url> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut self_url = base.clone();
    self_url.set_fragment(None);
    let mut fence: Option<(char, usize)> = None;
    for line in lines {
        if let Some(o) = fence {
            if fence_close(line, o) {
                fence = None;
            }
            continue;
        }
        if let Some(o) = fence_open(line) {
            fence = Some(o);
            continue;
        }
        let mut cands: Vec<String> = Vec::new();
        let mut rest = line.as_str();
        while let Some(i) = rest.find("](") {
            let before_is_image = rest[..i].rfind('[').map(|b| b > 0 && rest.as_bytes()[b - 1] == b'!').unwrap_or(false);
            let after = &rest[i + 2..];
            let end = after.find(')').unwrap_or(after.len());
            let target = after[..end].trim().trim_start_matches('<');
            let target = target.split_whitespace().next().unwrap_or("").trim_end_matches('>');
            if !before_is_image && !target.is_empty() {
                cands.push(target.to_string());
            }
            rest = &after[end.min(after.len())..];
        }
        let mut rest = line.as_str();
        while let Some(i) = rest.find("<http") {
            let after = &rest[i + 1..];
            if let Some(e) = after.find('>') {
                cands.push(after[..e].to_string());
                rest = &after[e..];
            } else {
                break;
            }
        }
        for c in cands {
            if let Ok(mut u) = base.join(&c) {
                u.set_fragment(None);
                if matches!(u.scheme(), "http" | "https") && u != self_url && seen.insert(u.as_str().to_string()) {
                    out.push(u);
                }
            }
        }
    }
    out
}

fn extract_markdown(url: &Url, text: &str) -> Page {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let (fm, body) = split_front_matter(&text);
    let lines = strip_html_comments(body);
    let items = md_items(&lines);
    let h1 = items.iter().find_map(|i| match i {
        Item::Heading { level: 1, text, .. } => Some(text.clone()),
        _ => None,
    });
    let title = h1
        .or_else(|| fm.get("title").cloned())
        .unwrap_or_else(|| url.path_segments().and_then(|s| s.filter(|x| !x.is_empty()).last().map(|x| x.to_string())).unwrap_or_default());
    let sections = build_sections(items, &title);
    let links = md_links(&lines, url);
    let mut meta = Meta::default();
    if let Some(v) = fm.get("version") {
        meta.versions.push((1, v.clone()));
    }
    meta.lang = fm.get("lang").or_else(|| fm.get("language")).cloned();
    meta.canonical = fm.get("canonical").or_else(|| fm.get("canonical_url")).cloned();
    finalize(url, &meta, title, sections, links, true, None)
}

// ───────────────────────── JSON ─────────────────────────

fn looks_html(s: &str) -> bool {
    s.contains("</") || s.contains("<p>") || s.contains("<br")
}

/// Fragment HTML → Markdown (même chaîne de nettoyage et de rendu que les pages).
fn html_fragment_to_md(html: &str) -> String {
    let mut doc = Html::parse_document(&format!("<html><body>{}</body></html>", html));
    clean(&mut doc, &CleanCx { has_tabs: true });
    let (container, _) = pick_container(&doc, Family::Generic);
    let cx = Ctx { tab_by_id: HashMap::new() };
    let items = items_from_container(container, &cx);
    let mut parts = Vec::new();
    for it in items {
        match it {
            Item::Heading { level, text, .. } => parts.push(format!("{} {}", "#".repeat(level as usize), text)),
            Item::Block { md, .. } => parts.push(md),
        }
    }
    parts.join("\n\n")
}

fn json_to_md(v: &serde_json::Value, depth: usize, out: &mut Vec<String>) {
    use serde_json::Value;
    if depth > 6 || out.len() > 20000 {
        return;
    }
    match v {
        Value::Object(map) => {
            let title_key: Option<&str> =
                ["title", "name", "heading", "label"].iter().copied().find(|k| map.get(*k).map(|x| x.is_string()).unwrap_or(false));
            if let Some(k) = title_key {
                let t = map[k].as_str().unwrap_or("").trim();
                if !t.is_empty() {
                    out.push(format!("{} {}", "#".repeat((depth + 1).min(4)), t));
                }
            }
            for (k, val) in map {
                if Some(k.as_str()) == title_key {
                    continue;
                }
                match val {
                    Value::String(s) => {
                        let s = s.trim();
                        if s.is_empty() {
                            continue;
                        }
                        let contentish =
                            matches!(k.as_str(), "content" | "body" | "markdown" | "md" | "text" | "html" | "description" | "summary");
                        if looks_html(s) {
                            out.push(html_fragment_to_md(s));
                        } else if contentish || s.len() >= 60 || s.contains('\n') {
                            out.push(s.to_string());
                        } else {
                            out.push(format!("- **{}**: {}", k, s));
                        }
                    }
                    Value::Number(_) | Value::Bool(_) => out.push(format!("- **{}**: {}", k, val)),
                    Value::Object(_) | Value::Array(_) => {
                        if matches!(val, Value::Object(m) if !["title", "name", "heading", "label"].iter().any(|t| m.contains_key(*t)))
                            && depth < 3
                        {
                            out.push(format!("{} {}", "#".repeat((depth + 2).min(4)), k));
                        }
                        json_to_md(val, depth + 1, out)
                    }
                    Value::Null => {}
                }
            }
        }
        Value::Array(a) => {
            for x in a {
                match x {
                    Value::String(s) if !s.trim().is_empty() => out.push(format!("- {}", s.trim())),
                    _ => json_to_md(x, depth, out),
                }
            }
        }
        Value::String(s) if !s.trim().is_empty() => out.push(s.trim().to_string()),
        _ => {}
    }
}

fn extract_json(url: &Url, text: &str) -> Page {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text.trim_start_matches('\u{feff}')) else {
        return extract_markdown(url, text);
    };
    let mut parts = Vec::new();
    json_to_md(&v, 0, &mut parts);
    extract_markdown(url, &parts.join("\n\n"))
}

// ───────────────────────── Tests ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::path::PathBuf;
    use std::time::Instant;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn html(url: &str, h: &str) -> Page {
        extract_page(&u(url), h.as_bytes(), "text/html; charset=utf-8")
    }

    #[test]
    fn slug_github() {
        assert_eq!(slug("Path params"), "path-params");
        assert_eq!(slug("Foo - Bar"), "foo---bar");
        assert_eq!(slug("foo.bar()"), "foobar");
        assert_eq!(slug("Éléments"), "éléments");
        assert_eq!(slug("???"), "section");
    }

    #[test]
    fn duplicates_get_suffix() {
        let mut a = Anchors::default();
        assert_eq!(a.unique("x".into()), "x");
        assert_eq!(a.unique("x".into()), "x~2");
        assert_eq!(a.unique("x".into()), "x~3");
    }

    #[test]
    fn version_segments() {
        for ok in ["v2", "0.21.3", "1.x", "3.11", "v1.2.3-beta.1", "2.0.0-rc1"] {
            assert!(is_version_segment(ok, false), "{}", ok);
        }
        for ko in ["2024", "docs", "en", "guide", "v", "1a"] {
            assert!(!is_version_segment(ko, false), "{}", ko);
        }
        assert!(is_version_segment("3", true));
        assert!(!is_version_segment("3", false));
        let (s, w) = version_from_url(&u("https://docs.python.org/3/library/os.html"));
        assert_eq!(s.as_deref(), Some("3"));
        assert_eq!(w, None);
        let (s, w) = version_from_url(&u("https://x.dev/en/latest/api/"));
        assert_eq!((s, w.as_deref()), (None, Some("latest")));
    }

    #[test]
    fn hidden_styles() {
        for s in [
            "display:none",
            "DISPLAY: none !important",
            "visibility:hidden",
            "font-size:0",
            "font-size: 0px",
            "opacity:0",
            "opacity: 0;",
            "left:-9999px",
            "color: transparent",
            "height:0;overflow:hidden",
        ] {
            assert!(style_hides(s), "{}", s);
        }
        for s in ["opacity:0.5", "font-size:0.9em", "color:red", "display:block", "font-size:10px", "opacity:0.05"] {
            assert!(!style_hides(s), "{}", s);
        }
    }

    #[test]
    fn decode_variants() {
        assert_eq!(decode(&[0xEF, 0xBB, 0xBF, b'a'], "text/html"), "a");
        assert_eq!(decode(&[0xFF, 0xFE, b'h', 0, b'i', 0], ""), "hi");
        assert_eq!(decode(&[0xFE, 0xFF, 0, b'h', 0, b'i'], ""), "hi");
        assert_eq!(decode(&[b'c', 0xE9], "text/html; charset=ISO-8859-1"), "cé");
        assert_eq!(decode(b"<meta charset=\"windows-1252\">\x80", ""), "<meta charset=\"windows-1252\">\u{20AC}");
        // octets invalides : remplacement, pas de panique.
        assert!(decode(&[0xFF, 0xFF, 0x41], "text/html").contains('A'));
    }

    #[test]
    fn canonical_only_same_host() {
        let ok = html(
            "https://a.dev/x",
            "<html><head><link rel=canonical href=\"/canon\"></head><body><main><h1>T</h1><p>texte</p></main></body></html>",
        );
        assert_eq!(ok.canonical.as_ref().map(|c| c.as_str()), Some("https://a.dev/canon"));
        let ko = html(
            "https://a.dev/x",
            "<html><head><link rel=canonical href=\"https://evil.example/x\"></head><body><main><h1>T</h1></main></body></html>",
        );
        assert!(ko.canonical.is_none());
    }

    #[test]
    fn links_resolved_without_fragments() {
        let p = html(
            "https://a.dev/docs/page",
            "<html><body><a href=\"../x#frag\">x</a><a href=\"mailto:a@b.c\">m</a><a href=\"javascript:void(0)\">j</a><a href=\"https://b.dev/y\">y</a><a href=\"#top\">t</a><a href=\"/x\">dup</a><a href=\"ftp://z/\">f</a></body></html>",
        );
        let l: Vec<&str> = p.links.iter().map(|x| x.as_str()).collect();
        assert_eq!(l, vec!["https://a.dev/x", "https://b.dev/y"]);
    }

    #[test]
    fn code_blocks_untouched() {
        let p = html(
            "https://a.dev/c",
            "<html><body><main><h1>T</h1><pre class=\"language-rust\"><code>fn f&lt;T: Clone&gt;(x: &amp;T)   -&gt; Vec&lt;T&gt; {\n    vec![x.clone()]\n}</code></pre></main></body></html>",
        );
        let all: String = p.sections.iter().map(|s| s.text.clone()).collect();
        assert!(all.contains("```rust\nfn f<T: Clone>(x: &T)   -> Vec<T> {\n    vec![x.clone()]\n}\n```"), "{}", all);
        assert_eq!(p.sections[0].code_blocks, 1);
    }

    #[test]
    fn fence_longer_than_content() {
        let f = fence("a\n```\nb", "md");
        assert!(f.starts_with("````md\n") && f.ends_with("\n````"));
    }

    #[test]
    fn heading_anchor_text_is_kept() {
        // MDN : le titre est le texte du lien-ancre ; Starlight : texte réservé aux lecteurs d'écran.
        let p = html(
            "https://a.dev/m",
            "<html><body><main><h1 id=\"top\">Page</h1><p>Un texte assez long pour que la sélection du conteneur le retienne sans hésiter.</p><h2 id=\"syntax\" class=\"heading\"><a class=\"heading-anchor\" href=\"#syntax\">Syntax</a></h2><p>Un paragraphe assez long pour ne pas être fondu dans sa voisine : la syntaxe.</p><div class=\"sl-heading-wrapper\"><h2 id=\"quick\">Quick</h2><a class=\"sl-anchor-link\" href=\"#quick\"><span aria-hidden=\"true\">#</span><span class=\"sr-only\">Section titled “Quick”</span></a></div><p>Un second paragraphe assez long pour rester une section à part entière ici.</p></main></body></html>",
        );
        let paths: Vec<String> = p.sections.iter().map(|s| s.heading_path.last().cloned().unwrap_or_default()).collect();
        assert!(paths.contains(&"Syntax".to_string()) && paths.contains(&"Quick".to_string()), "{:?}", paths);
        assert!(!p.sections.iter().any(|s| s.text.contains("Section titled")));
    }

    #[test]
    fn rustdoc_where_clause_on_its_own_line() {
        let p = html(
            "https://docs.rs/c/1.0.0/c/struct.S.html",
            "<html><head><meta name=\"generator\" content=\"rustdoc\"></head><body class=\"rustdoc\"><main><section id=\"main-content\" class=\"content\"><h1>Struct c::S</h1><p>Une structure qui contient un texte suffisamment long pour être retenu comme contenu.</p><section id=\"method.max\" class=\"method\"><h4 class=\"code-header\">fn max(self, other: Self) -&gt; Self<div class=\"where\">where Self: Sized,</div></h4><div class=\"docblock\"><p>Compares.</p></div></section></section></main></body></html>",
        );
        let all: String = p.sections.iter().map(|s| s.text.clone()).collect::<Vec<_>>().join("\n");
        assert!(all.contains("fn max(self, other: Self) -> Self\nwhere Self: Sized,"), "{}", all);
        let m = p.sections.iter().find(|s| s.anchor == "method.max").expect("section de la méthode");
        assert_eq!(m.heading_path.last().unwrap(), "fn max(self, other: Self) -> Self where Self: Sized,");
    }

    #[test]
    fn noisy_wrapper_class_does_not_swallow_content() {
        // `wy-nav-content-wrap` (Read the Docs) contient le contenu : un nom de classe de menu ne doit pas le supprimer.
        let p = html(
            "https://a.dev/r",
            "<html><body><div class=\"sidebar-wrapper\"><main><h1>Titre</h1><p>Un paragraphe suffisamment long pour dépasser le seuil de soixante caractères.</p></main></div></body></html>",
        );
        assert!(p.sections.iter().any(|s| s.text.contains("suffisamment long")), "{:?}", p.sections);
    }

    #[test]
    fn needs_js_and_partial() {
        let spa = html(
            "https://a.dev/",
            "<html><body><noscript>Please enable JavaScript</noscript><div id=\"root\"></div><script src=\"/a.js\"></script></body></html>",
        );
        assert_eq!(spa.completeness, Completeness::NeedsJs);
        // Page courte mais avec un corps : pas NeedsJs.
        let short = html("https://a.dev/", "<html><body><main><h1>Titre</h1><p>Un peu de texte.</p></main></body></html>");
        assert_eq!(short.completeness, Completeness::Partial);
        let empty = extract_page(&u("https://a.dev/"), b"", "text/html");
        assert!(empty.sections.is_empty());
        assert_ne!(empty.completeness, Completeness::Complete);
    }

    #[test]
    fn markdown_sections_and_anchors() {
        let md = "# Titre\n\nUne introduction assez longue pour rester une section à part entière.\n\n## Sous {#custom}\n\ntexte\n\n```sh\n# pas un titre\n```\n\n## Sous\n\nUn autre paragraphe assez long pour rester une section à part entière.\n";
        let p = extract_page(&u("https://a.dev/readme.md"), md.as_bytes(), "text/plain");
        let anchors: Vec<&str> = p.sections.iter().map(|s| s.anchor.as_str()).collect();
        assert_eq!(anchors, vec!["titre", "custom", "sous"]);
        assert_eq!(p.sections[1].code_blocks, 1);
        assert!(p.sections[1].text.contains("# pas un titre"));
        assert_eq!(p.sections[2].heading_path, vec!["Titre", "Sous"]);
    }

    #[test]
    fn json_and_sniffing() {
        let p = extract_page(&u("https://a.dev/d"), br#"{"title":"T","content":"<h2>Sous</h2><p>texte</p>"}"#, "application/json");
        assert!(p.sections.iter().any(|s| s.heading_path.last().map(|t| t == "Sous").unwrap_or(false)), "{:?}", p.sections);
        let q = extract_page(&u("https://a.dev/d"), b"# Bonjour\n\ntexte", "");
        assert_eq!(q.title, "Bonjour");
    }

    #[test]
    fn garbage_and_truncated_inputs_never_panic() {
        // Octets pseudo-aléatoires (générateur congruentiel) dans les trois chemins.
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        for n in 0..60 {
            let len = (n * 37) % 900;
            let b: Vec<u8> = (0..len)
                .map(|_| {
                    x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    (x >> 33) as u8
                })
                .collect();
            for ct in ["text/html", "text/markdown", "application/json", "text/plain", ""] {
                let _ = extract_page(&u("https://a.dev/x.md"), &b, ct);
            }
        }
        // Chaque fixture coupée en plusieurs points (dont au milieu d'un caractère multi-octets).
        if let Ok(rd) = std::fs::read_dir(root_dir().join("bench").join("web").join("fixtures")) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.ends_with(".json") && name == "verite.json" || name.starts_with("long-") {
                    continue;
                }
                let body = std::fs::read(e.path()).unwrap();
                for k in 1..12 {
                    let cut = body.len() * k / 12;
                    let ct = if name.ends_with(".md") {
                        "text/markdown"
                    } else if name.ends_with(".json") {
                        "application/json"
                    } else {
                        "text/html"
                    };
                    let _ = extract_page(&u("https://a.dev/x"), &body[..cut], ct);
                }
            }
        }
    }

    #[test]
    fn deep_nesting_does_not_overflow() {
        let mut s = String::from("<html><body><main><h1>T</h1>");
        for _ in 0..3000 {
            s.push_str("<div>");
        }
        s.push_str("<p>profond</p>");
        let p = std::thread::Builder::new().stack_size(2 * 1024 * 1024).spawn(move || html("https://a.dev/", &s)).unwrap().join().unwrap();
        assert!(!p.sections.is_empty());
    }

    // ─────────────── banc des fixtures : métriques ───────────────

    /// Racine du dépôt (CORTEX_ROOT permet de lancer le banc depuis une copie hors dépôt).
    fn root_dir() -> PathBuf {
        std::env::var("CORTEX_ROOT").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")))
    }

    fn fixtures_dir() -> PathBuf {
        root_dir().join("bench").join("web").join("fixtures")
    }

    fn work_dir() -> PathBuf {
        root_dir().join("target").join("web-tests").join("extract")
    }

    fn norm_ws(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// Texte des sections sans les préfixes de citation « > » (le code dans une admonition reste comparable).
    fn corpus(p: &Page) -> String {
        let mut out = String::new();
        for s in &p.sections {
            out.push_str(&s.heading_path.join(" > "));
            out.push('\n');
            for l in s.text.lines() {
                let mut t = l;
                while let Some(r) = t.strip_prefix("> ") {
                    t = r;
                }
                out.push_str(t.trim_end());
                out.push('\n');
            }
            out.push('\n');
        }
        out
    }

    fn note_found(p: &Page, kind: &str, snippet: &str) -> bool {
        let head = format!("**{}:**", kind);
        for s in &p.sections {
            let mut block: Vec<&str> = Vec::new();
            let mut blocks: Vec<String> = Vec::new();
            for l in s.text.lines() {
                if l.starts_with('>') {
                    block.push(l);
                } else if !block.is_empty() {
                    blocks.push(block.join("\n"));
                    block.clear();
                }
            }
            if !block.is_empty() {
                blocks.push(block.join("\n"));
            }
            for b in blocks {
                // Un bloc peut regrouper plusieurs notes collées : on découpe sur chaque étiquette.
                for piece in b.split(&head).skip(1) {
                    let flat = norm_ws(&piece.replace("\n>", " ").replace('>', " "));
                    if flat.contains(&norm_ws(snippet)) {
                        return true;
                    }
                }
            }
        }
        false
    }

    fn jstr(v: &Value, k: &str) -> Option<String> {
        v.get(k).and_then(|x| x.as_str()).map(|s| s.to_string())
    }

    fn jlist(v: &Value, k: &str) -> Vec<String> {
        v.get(k)
            .and_then(|x| x.as_array())
            .map(|a| a.iter().filter_map(|s| s.as_str().map(|t| t.to_string())).collect())
            .unwrap_or_default()
    }

    #[derive(Default)]
    struct Agg {
        t_ok: usize,
        t_all: usize,
        c_ok: usize,
        c_all: usize,
        l_bad: usize,
        l_all: usize,
        n_ok: usize,
        n_all: usize,
        a_ok: usize,
        a_all: usize,
        ms: f64,
        pages: usize,
    }

    fn ratio(a: usize, b: usize) -> f64 {
        if b == 0 {
            1.0
        } else {
            a as f64 / b as f64
        }
    }

    fn leak_rate(bad: usize, all: usize) -> f64 {
        if all == 0 {
            0.0
        } else {
            bad as f64 / all as f64
        }
    }

    /// Affiche les sections extraites d'un fichier : DUMP_FILE=chemin DUMP_URL=url (tests ignorés, avec --ignored --nocapture).
    #[test]
    #[ignore]
    fn dump_page() {
        let (Ok(f), Ok(url)) = (std::env::var("DUMP_FILE"), std::env::var("DUMP_URL")) else { return };
        let ct = std::env::var("DUMP_CT").unwrap_or_else(|_| "text/html; charset=utf-8".into());
        if let Ok(pat) = std::env::var("DUMP_CLEAN") {
            let raw = String::from_utf8_lossy(&std::fs::read(&f).unwrap()).into_owned();
            let mut doc = Html::parse_document(&raw);
            let meta = read_meta(&doc, &u(&url));
            clean(&mut doc, &CleanCx { has_tabs: meta.has_tabs });
            let h = doc.root_element().html();
            let _ = std::fs::write(work_dir().join("clean.html"), &h);
            if let Some(i) = h.find(&pat) {
                println!("CLEAN: {}", &h[i.saturating_sub(300)..(i + 300).min(h.len())]);
            } else {
                println!("CLEAN: motif absent, len={} debut={}", h.len(), &h[..h.len().min(400)]);
            }
        }
        let p = extract_page(&u(&url), &std::fs::read(&f).unwrap(), &ct);
        let mut out = format!(
            "TITLE: {}\nCANON: {:?} LANG: {:?} VERSION: {:?} Q: {:.2} {:?}\nLINKS: {}\n",
            p.title,
            p.canonical.as_ref().map(|c| c.as_str()),
            p.lang,
            p.version_hint,
            p.quality,
            p.completeness,
            p.links.len()
        );
        for s in &p.sections {
            out.push_str(&format!(
                "\n=== [{}] #{} L{} code={} :: {}\n{}\n",
                s.heading_path.len(),
                s.anchor,
                s.level,
                s.code_blocks,
                s.heading_path.join(" > "),
                s.text
            ));
        }
        let base = std::path::Path::new(&f).file_stem().map(|x| x.to_string_lossy().into_owned()).unwrap_or_default();
        let _ = std::fs::create_dir_all(work_dir().join("dumps"));
        let _ = std::fs::write(work_dir().join("dumps").join(format!("{}.txt", base)), &out);
        println!("{}", out);
    }

    #[test]
    fn metriques_fixtures() {
        let dir = fixtures_dir();
        let truth: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("verite.json")).expect("verite.json")).unwrap();
        let obj = truth.as_object().unwrap();
        assert!(obj.len() >= 12, "au moins 12 fixtures");
        let mut report = String::new();
        let mut failures: Vec<String> = Vec::new();
        let mut by_family: std::collections::BTreeMap<String, Agg> = Default::default();
        let mut total = Agg::default();
        report.push_str(&format!(
            "{:<30} {:<10} {:>6} {:>6} {:>6} {:>6} {:>6} {:>5} {:>5} {:>8}\n",
            "fixture", "famille", "titres", "code", "fuite", "notes", "ancres", "sect", "qual", "ms"
        ));
        for (name, t) in obj {
            let body = std::fs::read(dir.join(name)).unwrap_or_else(|_| panic!("fixture {}", name));
            let url = u(&jstr(t, "url").unwrap());
            let ct = jstr(t, "ct").unwrap_or_else(|| if name.ends_with(".md") { "text/markdown".into() } else { "text/html".into() });
            let mut best = f64::MAX;
            let mut page = None;
            for _ in 0..5 {
                let t0 = Instant::now();
                let p = extract_page(&url, &body, &ct);
                best = best.min(t0.elapsed().as_secs_f64() * 1000.0);
                page = Some(p);
            }
            let p = page.unwrap();
            let corp = corpus(&p);
            let corp_norm = norm_ws(&corp);
            let raw_all: String = p.sections.iter().map(|s| format!("{}\n\n", s.text)).collect();
            let paths: HashSet<String> = p.sections.iter().flat_map(|s| s.heading_path.iter().map(|h| norm_ws(h))).collect();
            let anchors: HashSet<&str> = p.sections.iter().map(|s| s.anchor.as_str()).collect();

            let titles = jlist(t, "titles");
            let t_ok = titles.iter().filter(|x| paths.contains(&norm_ws(x))).count();
            for x in titles.iter().filter(|x| !paths.contains(&norm_ws(x))) {
                failures.push(format!("{}: titre manquant {:?}", name, x));
            }
            let code = jlist(t, "code");
            let c_ok = code.iter().filter(|c| corp.contains(c.as_str())).count();
            for c in code.iter().filter(|c| !corp.contains(c.as_str())) {
                failures.push(format!("{}: code altéré ou absent {:?}", name, c));
            }
            let leak = jlist(t, "leak");
            let l_bad = leak.iter().filter(|x| corp.contains(x.as_str())).count();
            for x in leak.iter().filter(|x| corp.contains(x.as_str())) {
                failures.push(format!("{}: fuite {:?}", name, x));
            }
            let notes: Vec<Vec<String>> = t
                .get("notes")
                .and_then(|x| x.as_array())
                .map(|a| a.iter().map(|n| n.as_array().unwrap().iter().map(|s| s.as_str().unwrap().to_string()).collect()).collect())
                .unwrap_or_default();
            let n_ok = notes.iter().filter(|n| note_found(&p, &n[0], &n[1])).count();
            for n in notes.iter().filter(|n| !note_found(&p, &n[0], &n[1])) {
                failures.push(format!("{}: avertissement absent {:?}", name, n));
            }
            let anc = jlist(t, "anchors");
            let a_ok = anc.iter().filter(|a| anchors.contains(a.as_str())).count();
            for a in anc.iter().filter(|a| !anchors.contains(a.as_str())) {
                failures.push(format!(
                    "{}: ancre manquante {:?} (présentes : {:?})",
                    name,
                    a,
                    p.sections.iter().map(|s| s.anchor.clone()).collect::<Vec<_>>().into_iter().take(12).collect::<Vec<_>>()
                ));
            }
            for f in jlist(t, "forbidden") {
                if corp.to_lowercase().contains(&f.to_lowercase()) {
                    failures.push(format!("{}: contenu interdit présent {:?}", name, f));
                }
            }
            for m in jlist(t, "must_have") {
                if !corp_norm.contains(&norm_ws(&m)) && !corp.contains(&m) && !raw_all.contains(&m) {
                    failures.push(format!("{}: texte attendu absent {:?}", name, m));
                }
            }
            // Méta-données.
            let fam = format!(
                "{:?}",
                if name.ends_with(".html") { detect_family(&url, &String::from_utf8_lossy(&body)) } else { Family::Generic }
            );
            if let Some(f) = jstr(t, "family") {
                if f != fam {
                    failures.push(format!("{}: famille {} attendue, {} détectée", name, f, fam));
                }
            }
            let comp = match p.completeness {
                Completeness::Complete => "complete",
                Completeness::Partial => "partial",
                Completeness::NeedsJs => "needs_js",
                Completeness::Blocked => "blocked",
            };
            if let Some(c) = jstr(t, "completeness") {
                if c != comp {
                    failures.push(format!("{}: completeness {} attendue, {} obtenue", name, c, comp));
                }
            }
            if t.get("version").is_some() && p.version_hint != jstr(t, "version") {
                failures.push(format!("{}: version {:?} attendue, {:?} obtenue", name, jstr(t, "version"), p.version_hint));
            }
            if t.get("canonical").is_some() && p.canonical.as_ref().map(|c| c.to_string()) != jstr(t, "canonical") {
                failures.push(format!(
                    "{}: canonical {:?} attendu, {:?} obtenu",
                    name,
                    jstr(t, "canonical"),
                    p.canonical.as_ref().map(|c| c.to_string())
                ));
            }
            if t.get("lang").is_some() && p.lang != jstr(t, "lang") {
                failures.push(format!("{}: lang {:?} attendue, {:?} obtenue", name, jstr(t, "lang"), p.lang));
            }
            for l in jlist(t, "links_contain") {
                if !p.links.iter().any(|x| x.as_str() == l) {
                    failures.push(format!("{}: lien absent {:?}", name, l));
                }
            }
            for l in jlist(t, "links_forbid") {
                if p.links.iter().any(|x| x.as_str().contains(&l)) {
                    failures.push(format!("{}: lien interdit présent {:?}", name, l));
                }
            }
            assert_eq!(p.injection_signals, 0);
            report.push_str(&format!(
                "{:<30} {:<10} {:>6.2} {:>6.2} {:>6.2} {:>6.2} {:>6.2} {:>5} {:>5.2} {:>8.2}\n",
                name,
                fam,
                ratio(t_ok, titles.len()),
                ratio(c_ok, code.len()),
                leak_rate(l_bad, leak.len()),
                ratio(n_ok, notes.len()),
                ratio(a_ok, anc.len()),
                p.sections.len(),
                p.quality,
                best
            ));
            for agg in [by_family.entry(fam.clone()).or_default(), &mut total] {
                agg.t_ok += t_ok;
                agg.t_all += titles.len();
                agg.c_ok += c_ok;
                agg.c_all += code.len();
                agg.l_bad += l_bad;
                agg.l_all += leak.len();
                agg.n_ok += n_ok;
                agg.n_all += notes.len();
                agg.a_ok += a_ok;
                agg.a_all += anc.len();
                agg.ms += best;
                agg.pages += 1;
            }
        }
        report.push_str("\nPar famille (rappel titres / rappel code / fuite / rappel notes / rappel ancres / ms par page)\n");
        for (f, a) in by_family.iter().chain(std::iter::once((&"TOTAL".to_string(), &total))) {
            report.push_str(&format!(
                "{:<12} pages={:<3} titres={:.3} code={:.3} fuite={:.3} notes={:.3} ancres={:.3} ms={:.2}\n",
                f,
                a.pages,
                ratio(a.t_ok, a.t_all),
                ratio(a.c_ok, a.c_all),
                leak_rate(a.l_bad, a.l_all),
                ratio(a.n_ok, a.n_all),
                ratio(a.a_ok, a.a_all),
                a.ms / a.pages.max(1) as f64
            ));
        }
        if !failures.is_empty() {
            report.push_str("\nÉcarts :\n");
            for f in &failures {
                report.push_str(&format!(" - {}\n", f));
            }
        }
        println!("{}", report);
        let _ = std::fs::create_dir_all(work_dir());
        let _ = std::fs::write(work_dir().join("metriques.txt"), &report);
        assert!(ratio(total.t_ok, total.t_all) >= 0.95, "rappel des titres {:.3}", ratio(total.t_ok, total.t_all));
        assert!(ratio(total.c_ok, total.c_all) >= 0.95, "rappel du code {:.3}", ratio(total.c_ok, total.c_all));
        assert!(leak_rate(total.l_bad, total.l_all) <= 0.05, "fuite {:.3}", leak_rate(total.l_bad, total.l_all));
        assert!(failures.is_empty(), "écarts : {:#?}", failures);
    }

    // ─────────────── pages réelles (hors dépôt de test) : rapport sans seuil ───────────────

    fn boilerplate_line(l: &str) -> bool {
        let t = l.trim().trim_start_matches(['>', '-', '*', ' ']).to_lowercase();
        [
            "skip to",
            "edit this page",
            "previous",
            "next",
            "copyright",
            "cookie",
            "privacy",
            "table of contents",
            "on this page",
            "was this page helpful",
            "sign in",
            "log in",
            "© ",
        ]
        .iter()
        .any(|p| t.starts_with(p) && t.chars().count() < 60)
    }

    /// Titre lu sans passer par l'extracteur (vérité indépendante) : texte brut, symboles de permalien retirés.
    fn raw_heading(e: ElementRef) -> String {
        let t = norm_ws(&e.text().collect::<String>());
        let t: String = t.chars().filter(|c| !is_zero_width(*c)).collect();
        let t = t.trim_matches(|c: char| matches!(c, '¶' | '§' | '#' | ' ')).trim_end_matches("Copy item path").trim();
        norm_ws(&t.replace(" ,", ",").replace("( ", "(").replace(" )", ")").replace(" :", ":").replace(" .", "."))
    }

    #[test]
    fn metriques_reel() {
        let dir = work_dir().join("reel");
        let Ok(list) = std::fs::read_to_string(dir.join("urls.txt")) else {
            println!("pas de pages réelles (reel/urls.txt absent)");
            return;
        };
        let mut report = format!(
            "{:<28} {:<10} {:>5} {:>6} {:>7} {:>7} {:>6} {:>6} {:>6} {:>8} {}\n",
            "page", "famille", "sect", "mots", "titres", "code", "fuite", "qual", "ms", "complet", "version"
        );
        let hsel = Selector::parse("h1,h2,h3,h4").unwrap();
        let psel = Selector::parse("pre").unwrap();
        for line in list.lines().filter(|l| !l.trim().is_empty()) {
            let mut it = line.split_whitespace();
            let (file, url) = (it.next().unwrap(), it.next().unwrap());
            let Ok(body) = std::fs::read(dir.join(file)) else { continue };
            let url = u(url);
            let t0 = Instant::now();
            let is_md = file.ends_with(".md");
            let p = extract_page(&url, &body, if is_md { "text/plain; charset=utf-8" } else { "text/html; charset=utf-8" });
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            let raw = decode(&body, "text/html");
            let (hs, cs, fam): (Vec<String>, Vec<String>, String) = if is_md {
                // Vérité Markdown : titres ATX 1-4 hors blocs de code, blocs de code clôturés.
                let mut hs = Vec::new();
                let mut cs = Vec::new();
                let mut fence_open_line: Option<(char, usize)> = None;
                let mut cur = String::new();
                for l in raw.lines() {
                    if let Some(o) = fence_open_line {
                        if fence_close(l, o) {
                            fence_open_line = None;
                            let c = normalize_code(&cur);
                            if c.len() >= 8 {
                                cs.push(c);
                            }
                            cur.clear();
                        } else {
                            cur.push_str(l);
                            cur.push('\n');
                        }
                    } else if let Some(o) = fence_open(l) {
                        fence_open_line = Some(o);
                    } else if let Some((lv, t)) = atx_heading(l) {
                        if lv <= 4 {
                            hs.push(md_plain_heading(&t));
                        }
                    }
                }
                (hs, cs, "Markdown".to_string())
            } else {
                let doc = Html::parse_document(&raw);
                let scope_sel = Selector::parse("main, article, [role=main]").unwrap();
                let scope = doc.select(&scope_sel).max_by_key(|e| text_len(*e));
                let in_noise = |e: ElementRef| {
                    e.ancestors().filter_map(ElementRef::wrap).any(|a| {
                        matches!(
                            a.value().name(),
                            "nav" | "aside" | "footer" | "header" | "button" | "script" | "style" | "template" | "noscript"
                        )
                    })
                };
                let hs: Vec<String> = doc
                    .select(&hsel)
                    .filter(|h| {
                        scope.map(|s| h.ancestors().any(|a| a.id() == s.id())).unwrap_or(true)
                            && !in_noise(*h)
                            && !is_hidden(*h, &CleanCx { has_tabs: true })
                    })
                    .map(raw_heading)
                    .filter(|s| !s.is_empty() && s.chars().count() < 120)
                    .collect();
                let cs: Vec<String> = doc
                    .select(&psel)
                    .filter(|h| scope.map(|s| h.ancestors().any(|a| a.id() == s.id())).unwrap_or(true) && !in_noise(*h))
                    .map(|h| {
                        let mut r = String::new();
                        collect_code(*h, &mut r, 0);
                        normalize_code(&r)
                    })
                    .filter(|c| c.lines().count() >= 1 && c.len() >= 8)
                    .collect();
                (hs, cs, format!("{:?}", family_of(&url, &read_meta(&doc, &url))))
            };
            let corp = corpus(&p);
            let paths: HashSet<String> = p.sections.iter().flat_map(|s| s.heading_path.iter().map(|h| norm_ws(h))).collect();
            let t_ok = hs.iter().filter(|h| paths.contains(&norm_ws(h))).count();
            let c_ok = cs.iter().filter(|c| corp.contains(c.as_str())).count();
            let lines: Vec<&str> = corp.lines().filter(|l| !l.trim().is_empty()).collect();
            let bad = lines.iter().filter(|l| boilerplate_line(l)).count();
            let words: usize = p.sections.iter().map(|s| words(&s.text)).sum();
            report.push_str(&format!(
                "{:<28} {:<10} {:>5} {:>6} {:>3}/{:<3} {:>3}/{:<3} {:>6.3} {:>6.2} {:>6.1} {:>8} {:?}\n",
                file,
                fam,
                p.sections.len(),
                words,
                t_ok,
                hs.len(),
                c_ok,
                cs.len(),
                ratio(bad, lines.len()),
                p.quality,
                ms,
                format!("{:?}", p.completeness),
                p.version_hint
            ));
            for h in hs.iter().filter(|h| !paths.contains(&norm_ws(h))).take(5) {
                report.push_str(&format!("     titre manquant : {:?}\n", h));
            }
            for c in cs.iter().filter(|c| !corp.contains(c.as_str())).take(3) {
                report.push_str(&format!("     code différent : {:?}\n", c.chars().take(80).collect::<String>()));
            }
            for l in lines.iter().filter(|l| boilerplate_line(l)).take(4) {
                report.push_str(&format!("     ligne suspecte : {:?}\n", l));
            }
        }
        println!("{}", report);
        let _ = std::fs::write(work_dir().join("metriques_reel.txt"), &report);
    }
    // ───── rustdoc : bruit des implémentations génériques, pages d'item ─────

    fn sec(path: &[&str], level: u8, text: &str, code: u32) -> Section {
        Section {
            anchor: path.last().unwrap().to_lowercase().replace(' ', "-"),
            heading_path: path.iter().map(|s| s.to_string()).collect(),
            level,
            text: text.to_string(),
            code_blocks: code,
        }
    }

    #[test]
    fn rustdoc_sans_blanket_ni_auto_trait_mais_avec_les_traits_propres() {
        let long = "Make a GET request with the given path and return the response of the server in full.";
        let v = vec![
            sec(&["Struct Agent"], 1, long, 0),
            sec(
                &["Struct Agent", "Blanket Implementations", "impl<T> From<T> for T", "fn from(t: T) -> T"],
                4,
                "Returns the argument unchanged.",
                0,
            ),
            sec(&["Struct Agent", "Auto Trait Implementations", "impl Send for Agent"], 3, "", 0),
            sec(
                &["Struct Agent", "Trait Implementations", "impl Default for Agent", "fn default() -> Agent"],
                4,
                "Returns the default Agent.",
                0,
            ),
        ];
        let out = drop_boilerplate(v);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].anchor, "struct-agent");
        assert_eq!(out[1].heading_path.last().unwrap(), "fn default() -> Agent");
    }

    #[test]
    fn page_d_item_rustdoc_courte_est_complete() {
        let h = "<html><head><title>get in ureq - Rust</title><meta name=\"generator\" content=\"rustdoc\"></head><body class=\"rustdoc fn\"><main><div id=\"main-content\" class=\"content\"><h1>Function <a class=\"fn\" href=\"#\">get</a></h1><pre class=\"rust item-decl\"><code>pub fn get(path: &amp;str) -&gt; Request</code></pre><div class=\"docblock\"><p>Make a GET request with the given path to the server.</p></div></div></main></body></html>";
        let p = html("https://docs.rs/ureq/2.10.1/ureq/fn.get.html", h);
        assert_eq!(p.completeness, Completeness::Complete, "{:?}", p.sections);
        assert!(p.quality >= 0.6);
        // la même page hors d'une URL d'item reste partielle
        let q = html("https://docs.rs/ureq/2.10.1/ureq/about.html", h);
        assert_eq!(q.completeness, Completeness::Partial);
    }
}
