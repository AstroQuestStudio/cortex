//! Cortex Web : module sitemap (feature `web`, EXPÉRIMENTAL). Voir docs/WEB.md §3 D6 et §4.1.
//!
//! Module PUR : aucun réseau, aucune dépendance nouvelle. Il lit le texte d'un sitemap XML
//! (`urlset` ou `sitemapindex`) et d'un `llms.txt`, liste les candidats de découverte d'un site,
//! et dit quelles entrées ont changé depuis le dernier passage (`lastmod`).
//!
//! Le texte lu depuis le web est une DONNÉE : ce qui sort de ce module est une URL validée
//! (http/https uniquement, sans identifiants, longueur plafonnée) ou un texte court.
//!
//! Analyseur XML écrit à la main (quick-xml n'est pas une dépendance du projet). Il ignore
//! entièrement les déclarations `<!DOCTYPE ...>` et ne résout aucune entité externe : pas de XXE.
//! Les entités XML usuelles (`&amp;`, `&#x41;`) sont décodées ; une entité inconnue reste littérale,
//! et une `<loc>` qui en contient une est rejetée.

use std::collections::HashMap;
use url::Url;

/// Plafond d'entrées retenues par fichier (`urlset` comme `sitemapindex`).
pub const MAX_ENTRIES: usize = 50_000;
/// Nombre maximal de sitemaps annoncés par robots.txt pris en compte.
pub const MAX_ROBOTS_SITEMAPS: usize = 50;
/// Longueur maximale d'une URL acceptée (octets).
pub const MAX_URL_LEN: usize = 4096;

/// Une entrée d'un `<urlset>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SitemapEntry {
    pub url: Url,
    /// Valeur brute de `<lastmod>` (ISO 8601), sans normalisation, espaces retirés.
    pub lastmod: Option<String>,
}

/// Résultat d'une analyse de sitemap : entrées d'un `urlset`, enfants d'un `sitemapindex`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sitemap {
    pub urls: Vec<SitemapEntry>,
    pub children: Vec<Url>,
}

/// Une entrée de `llms.txt` : `- [titre](url): note`, rangée sous une section H2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmsEntry {
    /// Nom de la section H2 ; vide avant la première section. « Optional » est gardée telle quelle.
    pub section: String,
    pub title: String,
    pub url: Url,
    pub note: Option<String>,
}

// ───────────────────────── Sitemap XML ─────────────────────────

/// Analyse un sitemap (`<urlset>`) ou un index (`<sitemapindex>`).
///
/// Tolérant : une balise en majuscules ou avec préfixe (`<sm:loc>`) est reconnue ; un XML tronqué
/// ou mal formé donne les entrées complètes lues avant l'incident. Une entrée est retenue dès que
/// son `<loc>` est fermé, même si `</url>` manque. Ne panique jamais.
pub fn parse_sitemap(xml: &str, base: &Url) -> Sitemap {
    let mut out = Sitemap::default();
    let mut stack: Vec<String> = Vec::new();
    let mut pending: Option<Pending> = None;
    let mut field: Option<(Field, usize)> = None;
    let mut buf = String::new();

    for tok in Lexer::new(xml) {
        match tok {
            Tok::Start { name, empty } => {
                let depth = stack.len();
                let parent = stack.last().map(String::as_str).unwrap_or("");
                let opens_entry = (name == "url" && parent == "urlset") || (name == "sitemap" && parent == "sitemapindex");
                if opens_entry {
                    flush(&mut pending, &mut out, base);
                    field = None;
                    pending = Some(Pending { index: name == "sitemap", loc: None, lastmod: None });
                } else if !empty {
                    if let Some(p) = pending.as_ref() {
                        let owner = if p.index { "sitemap" } else { "url" };
                        if parent == owner {
                            if name == "loc" {
                                field = Some((Field::Loc, depth));
                                buf.clear();
                            } else if name == "lastmod" {
                                field = Some((Field::Lastmod, depth));
                                buf.clear();
                            }
                        }
                    }
                }
                if !empty {
                    stack.push(name);
                }
            }
            Tok::Text(text) => {
                if field.is_some() {
                    buf.push_str(&text);
                }
            }
            Tok::End(name) => {
                if let Some(idx) = stack.iter().rposition(|n| *n == name) {
                    if let Some((f, fd)) = field {
                        if fd >= idx {
                            if fd == idx {
                                let value = buf.trim().to_string();
                                if let Some(p) = pending.as_mut() {
                                    match f {
                                        Field::Loc => {
                                            p.loc = if has_entity_ref(&value) { None } else { Some(value) };
                                        }
                                        Field::Lastmod => p.lastmod = Some(value),
                                    }
                                }
                            }
                            field = None;
                        }
                    }
                    stack.truncate(idx);
                    if name == "url" || name == "sitemap" {
                        flush(&mut pending, &mut out, base);
                    }
                }
            }
        }
    }
    flush(&mut pending, &mut out, base);
    out
}

#[derive(Clone, Copy)]
enum Field {
    Loc,
    Lastmod,
}

/// Entrée en cours de lecture ; `index` vrai pour un `<sitemap>` d'un `sitemapindex`.
struct Pending {
    index: bool,
    loc: Option<String>,
    lastmod: Option<String>,
}

/// Range l'entrée en cours si son `<loc>` est complet et valide, puis la vide.
fn flush(pending: &mut Option<Pending>, out: &mut Sitemap, base: &Url) {
    let Some(p) = pending.take() else { return };
    let Some(loc) = p.loc else { return };
    let Some(url) = resolve_web(&loc, base) else { return };
    if p.index {
        if out.children.len() < MAX_ENTRIES {
            out.children.push(url);
        }
    } else if out.urls.len() < MAX_ENTRIES {
        let lastmod = p.lastmod.filter(|s| !s.is_empty());
        out.urls.push(SitemapEntry { url, lastmod });
    }
}

// ───────────────────────── Lexeur XML minimal ─────────────────────────

enum Tok {
    /// Balise ouvrante ; `name` = nom local en minuscules ; `empty` = `<x/>`.
    Start { name: String, empty: bool },
    /// Balise fermante ; nom local en minuscules.
    End(String),
    /// Texte (entités décodées) ou contenu de CDATA (brut).
    Text(String),
}

struct Lexer<'a> {
    s: &'a str,
    pos: usize,
}

impl<'a> Lexer<'a> {
    fn new(s: &'a str) -> Self {
        Lexer { s, pos: 0 }
    }
}

impl Iterator for Lexer<'_> {
    type Item = Tok;

    fn next(&mut self) -> Option<Tok> {
        let s = self.s;
        loop {
            let rest = &s[self.pos..];
            if rest.is_empty() {
                return None;
            }
            if !rest.starts_with('<') {
                let end = rest.find('<').unwrap_or(rest.len());
                self.pos += end;
                return Some(Tok::Text(decode_entities(&rest[..end])));
            }
            if let Some(body) = rest.strip_prefix("<!--") {
                self.pos += match body.find("-->") {
                    Some(k) => 4 + k + 3,
                    None => rest.len(),
                };
                continue;
            }
            if let Some(body) = rest.strip_prefix("<![CDATA[") {
                return Some(match body.find("]]>") {
                    Some(k) => {
                        self.pos += 9 + k + 3;
                        Tok::Text(body[..k].to_string())
                    }
                    None => {
                        self.pos += rest.len();
                        Tok::Text(body.to_string())
                    }
                });
            }
            if rest.starts_with("<!") {
                self.pos += skip_declaration(rest);
                continue;
            }
            if rest.starts_with("<?") {
                self.pos += match rest.find("?>") {
                    Some(k) => k + 2,
                    None => rest.len(),
                };
                continue;
            }
            match tag_end(rest) {
                TagEnd::Gt(end) => {
                    let inner = &rest[1..end];
                    self.pos += end + 1;
                    if let Some(name) = inner.strip_prefix('/') {
                        let n = local_name(name);
                        if !n.is_empty() {
                            return Some(Tok::End(n));
                        }
                        continue;
                    }
                    let empty = inner.trim_end().ends_with('/');
                    let raw = inner.split(|c: char| c.is_whitespace() || c == '/').next().unwrap_or("");
                    let name = local_name(raw);
                    if name.is_empty() {
                        continue;
                    }
                    return Some(Tok::Start { name, empty });
                }
                // Balise cassée (un '<' la coupe) : on jette le fragment et on reprend au '<'.
                TagEnd::Lt(lt) => self.pos += lt,
                // Balise tronquée en fin de texte : on s'arrête.
                TagEnd::Eof => {
                    self.pos = s.len();
                    return None;
                }
            }
        }
    }
}

enum TagEnd {
    Gt(usize),
    Lt(usize),
    Eof,
}

/// Cherche le `>` qui ferme la balise commençant à `rest[0]`, en ignorant les valeurs entre guillemets.
fn tag_end(rest: &str) -> TagEnd {
    let mut quote: Option<u8> = None;
    for (i, &c) in rest.as_bytes().iter().enumerate().skip(1) {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                b'"' | b'\'' => quote = Some(c),
                b'>' => return TagEnd::Gt(i),
                b'<' => return TagEnd::Lt(i),
                _ => {}
            },
        }
    }
    TagEnd::Eof
}

/// Longueur (octets) d'une déclaration `<!...>` (DOCTYPE, éventuel sous-ensemble `[...]`).
/// Son contenu n'est jamais interprété.
fn skip_declaration(rest: &str) -> usize {
    let mut depth: i32 = 0;
    let mut quote: Option<u8> = None;
    for (i, &c) in rest.as_bytes().iter().enumerate().skip(2) {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                b'"' | b'\'' => quote = Some(c),
                b'[' => depth += 1,
                b']' => depth -= 1,
                b'>' if depth <= 0 => return i + 1,
                _ => {}
            },
        }
    }
    rest.len()
}

/// Nom local (sans préfixe), en minuscules : `sm:URL ` -> `url`.
fn local_name(raw: &str) -> String {
    raw.trim().rsplit(':').next().unwrap_or("").to_ascii_lowercase()
}

/// Décode `&amp; &lt; &gt; &quot; &apos;` et `&#N;` / `&#xN;`. Une entité inconnue reste littérale.
fn decode_entities(raw: &str) -> String {
    if !raw.contains('&') {
        return raw.to_string();
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        let after = &rest[p + 1..];
        match entity_end(after) {
            Some(semi) => match decode_name(&after[..semi]) {
                Some(c) => {
                    out.push(c);
                    rest = &after[semi + 1..];
                }
                None => {
                    out.push('&');
                    rest = after;
                }
            },
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Position du `;` fermant une entité, cherché sur 33 octets au plus (borne le coût).
fn entity_end(after: &str) -> Option<usize> {
    after.as_bytes().iter().take(33).position(|&c| c == b';')
}

fn decode_name(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => {
            let num = name.strip_prefix('#')?;
            let v = match num.strip_prefix('x').or_else(|| num.strip_prefix('X')) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => num.parse::<u32>().ok()?,
            };
            char::from_u32(v).filter(|&c| c != '\0')
        }
    }
}

/// Vrai si le texte contient encore une référence d'entité non décodée (`&xxe;`).
fn has_entity_ref(s: &str) -> bool {
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        let after = &rest[p + 1..];
        if let Some(semi) = entity_end(after) {
            if semi >= 1 && after.as_bytes()[..semi].iter().all(|c| c.is_ascii_alphanumeric() || *c == b'#') {
                return true;
            }
        }
        rest = after;
    }
    false
}

/// Résout une URL relative ou absolue ; refuse tout schéma autre que http/https,
/// les identifiants, et les URL trop longues ou vides.
fn resolve_web(raw: &str, base: &Url) -> Option<Url> {
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > MAX_URL_LEN {
        return None;
    }
    let url = base.join(raw).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    if !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    Some(url)
}

// ───────────────────────── llms.txt ─────────────────────────

/// Analyse un `llms.txt` (https://llmstxt.org) : titre (premier H1), résumé (premier blockquote),
/// entrées `- [titre](url): note` rangées par section H2. Les blocs de code sont ignorés.
pub fn parse_llms_txt(text: &str, base: &Url) -> (Option<String>, Option<String>, Vec<LlmsEntry>) {
    let mut title: Option<String> = None;
    let mut summary: Vec<String> = Vec::new();
    let mut in_quote = false;
    let mut summary_closed = false;
    let mut fence = false;
    let mut section = String::new();
    let mut entries: Vec<LlmsEntry> = Vec::new();

    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with("```") || line.starts_with("~~~") {
            fence = !fence;
            continue;
        }
        if fence {
            continue;
        }
        if let Some(q) = line.strip_prefix('>') {
            if !summary_closed {
                in_quote = true;
                let t = q.trim();
                if !t.is_empty() {
                    summary.push(t.to_string());
                }
            }
            continue;
        }
        if in_quote {
            in_quote = false;
            summary_closed = true;
        }
        if line.is_empty() {
            continue;
        }
        if let Some(h) = line.strip_prefix("# ") {
            let h = h.trim();
            if title.is_none() && !h.is_empty() {
                title = Some(h.to_string());
            }
            continue;
        }
        if let Some(h) = line.strip_prefix("## ") {
            section = h.trim().to_string();
            continue;
        }
        if let Some(item) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
            if let Some(entry) = parse_llms_item(item.trim(), &section, base) {
                entries.push(entry);
            }
        }
    }
    let summary = if summary.is_empty() { None } else { Some(summary.join(" ")) };
    (title, summary, entries)
}

/// `[titre](url)` suivi éventuellement de `: note`.
fn parse_llms_item(item: &str, section: &str, base: &Url) -> Option<LlmsEntry> {
    let rest = item.strip_prefix('[')?;
    let close = rest.find("](")?;
    let title = rest[..close].trim().to_string();
    let after = &rest[close + 2..];
    let end = after.find(')')?;
    let url = resolve_web(&after[..end], base)?;
    let tail = after[end + 1..].trim();
    let note = tail.strip_prefix(':').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    Some(LlmsEntry { section: section.to_string(), title, url, note })
}

// ───────────────────────── Découverte et changements ─────────────────────────

/// Candidats de sitemap pour un site, dans l'ordre : sitemaps annoncés par robots.txt, puis
/// `/sitemap.xml`, `/sitemap_index.xml`, `/llms.txt`. Sans doublon.
///
/// Un sitemap annoncé par robots.txt doit être de la même origine ou sur un sous-domaine de
/// l'origine (même schéma et même port). Aucun autre hôte n'est accepté.
pub fn sitemap_candidates(origin: &Url, robots_sitemaps: &[String]) -> Vec<Url> {
    if !matches!(origin.scheme(), "http" | "https") {
        return Vec::new();
    }
    let mut out: Vec<Url> = Vec::new();
    for raw in robots_sitemaps.iter().take(MAX_ROBOTS_SITEMAPS) {
        if let Some(u) = resolve_web(raw, origin) {
            if same_origin_or_subdomain(&u, origin) {
                push_unique(&mut out, u);
            }
        }
    }
    for path in ["/sitemap.xml", "/sitemap_index.xml", "/llms.txt"] {
        if let Ok(u) = origin.join(path) {
            push_unique(&mut out, u);
        }
    }
    out
}

fn push_unique(v: &mut Vec<Url>, u: Url) {
    if !v.contains(&u) {
        v.push(u);
    }
}

fn same_origin_or_subdomain(u: &Url, origin: &Url) -> bool {
    if u.scheme() != origin.scheme() || u.port_or_known_default() != origin.port_or_known_default() {
        return false;
    }
    match (u.host_str(), origin.host_str()) {
        (Some(h), Some(oh)) => h == oh || h.strip_suffix(oh).map_or(false, |p| p.ends_with('.')),
        _ => false,
    }
}

/// Entrées absentes du dernier passage, ou dont `lastmod` diffère de la valeur connue.
/// `lastmod` absent = toujours à revoir. Clé de `last_seen` : `Url::as_str()` de l'entrée.
/// Les dates sont comparées après normalisation (voir `normalize_lastmod`).
pub fn changed_since<'a>(entries: &'a [SitemapEntry], last_seen: &HashMap<String, String>) -> Vec<&'a SitemapEntry> {
    entries
        .iter()
        .filter(|e| match (&e.lastmod, last_seen.get(e.url.as_str())) {
            (None, _) => true,
            (Some(_), None) => true,
            (Some(now), Some(prev)) => normalize_lastmod(now) != normalize_lastmod(prev),
        })
        .collect()
}

/// Forme comparable d'une date ISO 8601 : `AAAA-MM-JJ` si pas d'heure ; sinon `AAAA-MM-JJTHH:MM:SSZ`
/// en UTC (fraction de seconde ignorée, décalage appliqué). Une valeur non reconnue est renvoyée
/// telle quelle, espaces retirés.
fn normalize_lastmod(raw: &str) -> String {
    let t = raw.trim();
    iso_to_canonical(t).unwrap_or_else(|| t.to_string())
}

fn iso_to_canonical(t: &str) -> Option<String> {
    let b = t.as_bytes();
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y = digits(&b[0..4])?;
    let m = digits(&b[5..7])?;
    let d = digits(&b[8..10])?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    if b.len() == 10 {
        return Some(format!("{y:04}-{m:02}-{d:02}"));
    }
    if !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    let r = &b[11..];
    if r.len() < 5 || r[2] != b':' {
        return None;
    }
    let hh = digits(&r[0..2])?;
    let mi = digits(&r[3..5])?;
    let mut i = 5;
    let mut ss = 0;
    if r.len() >= 8 && r[5] == b':' {
        ss = digits(&r[6..8])?;
        i = 8;
    }
    if r.get(i) == Some(&b'.') {
        i += 1;
        while i < r.len() && r[i].is_ascii_digit() {
            i += 1;
        }
    }
    if hh > 23 || mi > 59 || ss > 60 {
        return None;
    }
    let offset_min = match &r[i..] {
        [] | [b'Z' | b'z'] => 0,
        [sign @ (b'+' | b'-'), rest @ ..] => {
            if rest.len() != 5 || rest[2] != b':' {
                return None;
            }
            let oh = digits(&rest[0..2])?;
            let om = digits(&rest[3..5])?;
            if oh > 23 || om > 59 {
                return None;
            }
            let mins = oh * 60 + om;
            if *sign == b'-' {
                -mins
            } else {
                mins
            }
        }
        _ => return None,
    };
    let days = days_from_civil(y, m, d);
    let secs = days * 86_400 + hh * 3_600 + mi * 60 + ss - offset_min * 60;
    Some(format_utc(secs))
}

fn digits(s: &[u8]) -> Option<i64> {
    if s.is_empty() || !s.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(s.iter().fold(0i64, |acc, c| acc * 10 + i64::from(c - b'0')))
}

/// Jours depuis le 1970-01-01 (algorithme de H. Hinnant, calendrier grégorien proleptique).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn format_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3_600, rem % 3_600 / 60, rem % 60)
}

// ───────────────────────── Tests ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://exemple.test/docs/").unwrap()
    }

    fn origin() -> Url {
        Url::parse("https://exemple.test/").unwrap()
    }

    fn urls(sm: &Sitemap) -> Vec<String> {
        sm.urls.iter().map(|e| e.url.to_string()).collect()
    }

    fn entry(url: &str, lastmod: Option<&str>) -> SitemapEntry {
        SitemapEntry { url: Url::parse(url).unwrap(), lastmod: lastmod.map(str::to_string) }
    }

    #[test]
    fn sitemap_urlset_simple() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <url><loc>https://exemple.test/a</loc><lastmod>2024-05-01</lastmod></url>
  <url><loc>https://exemple.test/b</loc></url>
</urlset>"#;
        let sm = parse_sitemap(xml, &base());
        assert_eq!(urls(&sm), vec!["https://exemple.test/a", "https://exemple.test/b"]);
        assert_eq!(sm.urls[0].lastmod.as_deref(), Some("2024-05-01"));
        assert_eq!(sm.urls[1].lastmod, None);
        assert!(sm.children.is_empty());
    }

    #[test]
    fn sitemap_index_donne_des_enfants() {
        let xml = r#"<sitemapindex><sitemap><loc>https://exemple.test/s1.xml</loc><lastmod>2024-01-01</lastmod></sitemap><sitemap><loc>/s2.xml</loc></sitemap></sitemapindex>"#;
        let sm = parse_sitemap(xml, &origin());
        assert!(sm.urls.is_empty());
        let kids: Vec<String> = sm.children.iter().map(|u| u.to_string()).collect();
        assert_eq!(kids, vec!["https://exemple.test/s1.xml", "https://exemple.test/s2.xml"]);
    }

    #[test]
    fn cdata_loc_sans_decodage_des_entites() {
        let xml = "<urlset><url><loc><![CDATA[https://exemple.test/c?x=1&y=2]]></loc></url></urlset>";
        let sm = parse_sitemap(xml, &base());
        assert_eq!(urls(&sm), vec!["https://exemple.test/c?x=1&y=2"]);
    }

    #[test]
    fn entites_xml_decodees() {
        let xml = "<urlset><url><loc>https://exemple.test/p?a=1&amp;b=2&#x41;&#66;</loc></url></urlset>";
        let sm = parse_sitemap(xml, &base());
        assert_eq!(urls(&sm), vec!["https://exemple.test/p?a=1&b=2AB"]);
    }

    #[test]
    fn loc_relative_resolue_sur_la_base() {
        let b = Url::parse("https://exemple.test/docs/guide/").unwrap();
        let xml = "<urlset><url><loc>../img/x.html</loc></url></urlset>";
        let sm = parse_sitemap(xml, &b);
        assert_eq!(urls(&sm), vec!["https://exemple.test/docs/img/x.html"]);
    }

    #[test]
    fn schemas_refuses_ftp_file_javascript_et_identifiants() {
        let xml = "<urlset>\
            <url><loc>ftp://exemple.test/x</loc></url>\
            <url><loc>file:///etc/passwd</loc></url>\
            <url><loc>javascript:alert(1)</loc></url>\
            <url><loc>https://u:p@exemple.test/creds</loc></url>\
            <url><loc>https://exemple.test/ok</loc></url>\
            </urlset>";
        let sm = parse_sitemap(xml, &base());
        assert_eq!(urls(&sm), vec!["https://exemple.test/ok"]);
    }

    #[test]
    fn xml_tronque_garde_les_entrees_closes() {
        let xml = "<urlset><url><loc>https://exemple.test/1</loc></url><url><loc>https://exemple.test/2</lo";
        let sm = parse_sitemap(xml, &base());
        assert_eq!(urls(&sm), vec!["https://exemple.test/1"]);
    }

    #[test]
    fn entree_sans_fermeture_url_gardee_si_loc_ferme() {
        let xml = "<urlset><url><loc>https://exemple.test/1</loc></url><url><loc>https://exemple.test/2</loc>";
        let sm = parse_sitemap(xml, &base());
        assert_eq!(urls(&sm), vec!["https://exemple.test/1", "https://exemple.test/2"]);
    }

    #[test]
    fn troncature_a_chaque_octet_ne_panique_pas() {
        let sample = r#"<?xml version="1.0"?><urlset><url><loc>https://exemple.test/a?x=1&amp;y=2</loc><lastmod>2024-05-01T10:00:00+02:00</lastmod></url><url><loc><![CDATA[https://exemple.test/b]]></loc></url><!-- c --></urlset>"#;
        let full = parse_sitemap(sample, &base());
        assert_eq!(full.urls.len(), 2);
        for n in 0..=sample.len() {
            if sample.is_char_boundary(n) {
                let partial = parse_sitemap(&sample[..n], &base());
                assert!(partial.urls.len() <= 2);
            }
        }
    }

    #[test]
    fn entrees_garbage_ne_paniquent_pas() {
        let inputs = [
            "",
            "<",
            "<<<",
            "<!--",
            "<!--x",
            "<![CDATA[",
            "<![CDATA[x",
            "<!DOCTYPE",
            "<?xml",
            "</",
            "<urlset><url><loc>",
            "<url><loc>&#xFFFFFFFFFFFF;</loc></url>",
            "<url><loc>é€𝄞</loc></url>",
            "<a b='>",
            "<url><loc>https://exemple.test/</loc></url",
            "&&&;;;",
            "<urlset>é<url>",
            "<urlset><url/><url/><url><loc>&#0;</loc></url></urlset>",
            "# [x](",
            "- [",
            "> > >",
            "```",
        ];
        for s in inputs {
            let _ = parse_sitemap(s, &base());
            let _ = parse_llms_txt(s, &base());
            let _ = sitemap_candidates(&base(), &[s.to_string()]);
        }
    }

    #[test]
    fn plafond_50000_entrees() {
        let mut s = String::from("<?xml version=\"1.0\"?><urlset>");
        for i in 0..60_000 {
            s.push_str(&format!("<url><loc>https://exemple.test/p/{i}</loc></url>"));
        }
        s.push_str("</urlset>");
        let sm = parse_sitemap(&s, &base());
        assert_eq!(sm.urls.len(), MAX_ENTRIES);
        assert_eq!(sm.urls[0].url.as_str(), "https://exemple.test/p/0");
        assert_eq!(sm.urls[MAX_ENTRIES - 1].url.as_str(), "https://exemple.test/p/49999");
    }

    #[test]
    fn xxe_doctype_ignore() {
        let xml = r#"<?xml version="1.0"?>
<!DOCTYPE urlset [ <!ENTITY xxe SYSTEM "file:///etc/passwd"> ]>
<urlset><url><loc>&xxe;</loc></url><url><loc>https://exemple.test/ok</loc></url></urlset>"#;
        let sm = parse_sitemap(xml, &base());
        assert_eq!(urls(&sm), vec!["https://exemple.test/ok"]);

        let xml2 = r#"<!DOCTYPE urlset SYSTEM "file:///etc/passwd"><urlset><url><loc>https://exemple.test/ok2</loc></url></urlset>"#;
        let sm2 = parse_sitemap(xml2, &base());
        assert_eq!(urls(&sm2), vec!["https://exemple.test/ok2"]);
    }

    #[test]
    fn balises_majuscules_et_prefixe() {
        let xml = r#"<SM:URLSET xmlns:SM="x"><SM:URL><SM:LOC>https://exemple.test/x</SM:LOC><SM:LASTMOD>2024-01-01</SM:LASTMOD></SM:URL></SM:URLSET>"#;
        let sm = parse_sitemap(xml, &base());
        assert_eq!(urls(&sm), vec!["https://exemple.test/x"]);
        assert_eq!(sm.urls[0].lastmod.as_deref(), Some("2024-01-01"));
    }

    #[test]
    fn image_loc_imbriquee_ignoree() {
        let xml = r#"<urlset xmlns:image="u"><url><loc>https://exemple.test/page</loc><image:image><image:loc>https://cdn.exemple.test/i.png</image:loc></image:image></url></urlset>"#;
        let sm = parse_sitemap(xml, &base());
        assert_eq!(urls(&sm), vec!["https://exemple.test/page"]);
    }

    #[test]
    fn url_auto_fermante_ignoree() {
        let xml = "<urlset><url/><url><loc>https://exemple.test/x</loc></url></urlset>";
        let sm = parse_sitemap(xml, &base());
        assert_eq!(urls(&sm), vec!["https://exemple.test/x"]);
    }

    #[test]
    fn lastmod_brut_trimme() {
        let xml = "<urlset><url><loc>https://exemple.test/a</loc><lastmod>\n 2024-05-01T10:00:00+02:00 \n</lastmod></url></urlset>";
        let sm = parse_sitemap(xml, &base());
        assert_eq!(sm.urls[0].lastmod.as_deref(), Some("2024-05-01T10:00:00+02:00"));
    }

    #[test]
    fn llms_txt_forme_standard() {
        let txt = "# FastHTML\n\n> FastHTML est une bibliothèque Python.\n> Deuxième ligne du résumé.\n\nDétails libres.\n\n## Docs\n\n- [Démarrage rapide](https://exemple.test/quickstart.md): Vue d'ensemble\n- [Routage](/docs/routing.md)\n- pas un lien\n\n## Optional\n\n- [Changelog](https://exemple.test/changelog.md): historique\n";
        let (title, summary, entries) = parse_llms_txt(txt, &base());
        assert_eq!(title.as_deref(), Some("FastHTML"));
        assert_eq!(summary.as_deref(), Some("FastHTML est une bibliothèque Python. Deuxième ligne du résumé."));
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].section, "Docs");
        assert_eq!(entries[0].title, "Démarrage rapide");
        assert_eq!(entries[0].url.as_str(), "https://exemple.test/quickstart.md");
        assert_eq!(entries[0].note.as_deref(), Some("Vue d'ensemble"));
        assert_eq!(entries[1].url.as_str(), "https://exemple.test/docs/routing.md");
        assert_eq!(entries[1].note, None);
        assert_eq!(entries[2].section, "Optional");
        assert_eq!(entries[2].note.as_deref(), Some("historique"));
    }

    #[test]
    fn llms_txt_blocs_de_code_et_schemas_refuses() {
        let txt = "# T\n\n## S\n\n```\n## faux\n- [x](https://exemple.test/x)\n```\n- [ok](https://exemple.test/ok)\n- [js](javascript:alert(1))\n- [ftp](ftp://exemple.test/f)\n- [creds](https://u:p@exemple.test/c)\n";
        let (_, _, entries) = parse_llms_txt(txt, &base());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].section, "S");
        assert_eq!(entries[0].url.as_str(), "https://exemple.test/ok");
    }

    #[test]
    fn llms_txt_sans_titre_ni_resume() {
        let (t, s, e) = parse_llms_txt("- [a](https://exemple.test/a)", &base());
        assert_eq!(t, None);
        assert_eq!(s, None);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].section, "");
    }

    #[test]
    fn candidats_ordre_et_doublons() {
        let robots: Vec<String> = vec![
            "https://exemple.test/s/a.xml".into(),
            "https://docs.exemple.test/sitemap.xml".into(),
            "https://evil.test/sitemap.xml".into(),
            "https://exemple.test/s/a.xml".into(),
            "/relatif.xml".into(),
            "ftp://exemple.test/x.xml".into(),
        ];
        let got: Vec<String> = sitemap_candidates(&origin(), &robots).iter().map(|u| u.to_string()).collect();
        assert_eq!(
            got,
            vec![
                "https://exemple.test/s/a.xml",
                "https://docs.exemple.test/sitemap.xml",
                "https://exemple.test/relatif.xml",
                "https://exemple.test/sitemap.xml",
                "https://exemple.test/sitemap_index.xml",
                "https://exemple.test/llms.txt",
            ]
        );
    }

    #[test]
    fn candidats_sans_robots() {
        let got: Vec<String> = sitemap_candidates(&origin(), &[]).iter().map(|u| u.to_string()).collect();
        assert_eq!(
            got,
            vec!["https://exemple.test/sitemap.xml", "https://exemple.test/sitemap_index.xml", "https://exemple.test/llms.txt",]
        );
    }

    #[test]
    fn candidats_hotes_etrangers_refuses() {
        let robots: Vec<String> = vec![
            "https://exemple.test.evil.test/s.xml".into(),
            "http://exemple.test/s.xml".into(),
            "https://exemple.test:8443/s.xml".into(),
            "https://u:p@exemple.test/s.xml".into(),
            "https://xexemple.test/s.xml".into(),
        ];
        let got = sitemap_candidates(&origin(), &robots);
        assert_eq!(got.len(), 3);
        assert!(got.iter().all(|u| u.host_str() == Some("exemple.test") && u.scheme() == "https"));
    }

    #[test]
    fn candidats_origine_non_http_vide() {
        let ftp = Url::parse("ftp://exemple.test/").unwrap();
        assert!(sitemap_candidates(&ftp, &[]).is_empty());
    }

    #[test]
    fn changed_since_lastmod_absent_ou_different() {
        let entries = vec![
            entry("https://exemple.test/a", Some("2024-05-01T10:00:00+02:00")),
            entry("https://exemple.test/b", Some("2024-05-02")),
            entry("https://exemple.test/c", None),
            entry("https://exemple.test/d", Some("2024-05-03")),
        ];
        let mut last = HashMap::new();
        last.insert("https://exemple.test/a".to_string(), "2024-05-01T08:00:00Z".to_string());
        last.insert("https://exemple.test/b".to_string(), "2024-05-01".to_string());
        last.insert("https://exemple.test/c".to_string(), "2024-05-09".to_string());
        let changed: Vec<String> = changed_since(&entries, &last).iter().map(|e| e.url.to_string()).collect();
        assert_eq!(changed, vec!["https://exemple.test/b", "https://exemple.test/c", "https://exemple.test/d"]);
    }

    #[test]
    fn changed_since_precision_seconde_et_fuseau() {
        let entries = vec![entry("https://exemple.test/a", Some("2024-05-01T10:00:00.250Z"))];
        let mut last = HashMap::new();
        last.insert("https://exemple.test/a".to_string(), "2024-05-01 10:00:00+00:00".to_string());
        assert!(changed_since(&entries, &last).is_empty());

        last.insert("https://exemple.test/a".to_string(), "2024-05-01T10:00:01Z".to_string());
        assert_eq!(changed_since(&entries, &last).len(), 1);
    }

    #[test]
    fn normalisation_lastmod() {
        assert_eq!(normalize_lastmod("2024-05-01T10:00:00+02:00"), normalize_lastmod("2024-05-01T08:00:00Z"));
        assert_eq!(normalize_lastmod("2024-05-01 10:00"), "2024-05-01T10:00:00Z");
        assert_eq!(normalize_lastmod("1970-01-01T00:00:00+01:00"), "1969-12-31T23:00:00Z");
        assert_eq!(normalize_lastmod("  pas une date  "), "pas une date");
        assert_eq!(normalize_lastmod("2024-13-40"), "2024-13-40");
    }

    #[test]
    fn calendrier_aller_retour() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        for days in [-100_000_i64, -1, 0, 1, 10_957, 19_000, 60_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
    }
}
