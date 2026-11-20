//! Cortex Web : index BM25F persisté sur les sections d'un `Store` (prototype, feature `web`).
//!
//! # Modèle
//! * Trois champs par section : chemin de titres (+ titre de page) ×3, identifiants et code
//!   (blocs de code et mots entre accents graves) ×2, texte courant ×1. BM25F de Robertson
//!   (2004) : `tf' = Σ w_f · tf_f / (1 − b + b · len_f / moy_f)`, `score = idf · tf' / (k1 + tf')`
//!   avec `k1 = 1,2` et `b = 0,5` pour chaque champ.
//! * Termes : exactement ceux de `find` côté requête (`crate::search::query_terms` puis
//!   `crate::stem::stem`) ; côté index, même découpage (mots alphanumériques, camelCase en
//!   morceaux + identifiant entier, accents repliés), mots vides retirés, racinisation.
//! * Bonus : phrase exacte du titre ×1,35 (titre égal à la requête ×1,6), tous les termes
//!   présents ×1,3 ; malus `Partial` ×0,8, `NeedsJs` ×0,6, `Blocked` ×0,35.
//!
//! # `index.bin` (little-endian, version 1)
//! ```text
//! "CXWI" | version u32 | empreinte 4×u64 (taille+mtime de sections.jsonl et pages.jsonl)
//! n_docs u32 | n_urls u32 | n_versions u32 | n_terms u32 | jetons par champ 3×u64
//! blob_termes u64 (octets) | blob_postings u64 (octets)
//! urls  : [varint len, octets, qualité f32, date u64, complétude u8] × n_urls
//! versions : [varint len, octets] × n_versions
//! docs  : 40 octets × n_docs (offset u64, longueur u32, url u32, version u32, hash_id u32,
//!         longueurs de champ 3×u32, complétude u8 + 3 octets de réserve)
//! hash_id : [hash u32, doc u32] × n_docs, trié (recherche d'un id)
//! index des termes : offset u32 × n_terms (dans le blob de termes, termes triés)
//! blob de termes : [len u8, octets, varint df, varint offset postings, varint longueur] × n_terms
//! blob de postings : par terme, [varint (delta_doc << 3 | masque de champs), varint tf × champs présents]
//! blake3 (32 octets) de tout ce qui précède
//! ```
//! L'index ne contient aucun texte : l'extrait est relu dans `sections.jsonl` à l'offset
//! mémorisé (vérifié par un hash de l'identifiant). Périmé (empreinte différente), absent ou
//! corrompu (somme de contrôle, structure) : `Index::open` le reconstruit.

use super::store::{read_section_from, write_atomic, Store};
use super::{Completeness, StoredSection};
use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::path::PathBuf;

const MAGIC: &[u8; 4] = b"CXWI";
const VERSION: u32 = 1;
const DOC_BYTES: usize = 40;
/// Poids des champs : titres, identifiants/code, texte.
const W: [f32; 3] = [3.0, 2.0, 1.0];
const K1: f32 = 1.2;
const B: f32 = 0.5;
const NONE: u32 = u32::MAX;
const MAX_TERM_LEN: usize = 40;
const MAX_QUERY_TERMS: usize = 32;
/// Bonus maximal (tous les termes dans le titre propre de la section).
const LEAF_BONUS: f32 = 0.35;

// ───────────────────────── Types publics ─────────────────────────

/// Filtres de requête (minimal). `Default` = aucun filtre.
#[derive(Clone, Debug, Default)]
pub struct Filter {
    /// Version voulue : égalité, ou préfixe sur frontière de point (`2` accepte `2.1.3`). Une
    /// section SANS version connue passe (elle ne contredit rien) ; `Hit::version` le dit.
    pub version: Option<String>,
    /// Qualité minimale de la page (`PageRecord::quality`).
    pub min_quality: Option<f32>,
    /// États de complétude acceptés ; vide = tous.
    pub completeness: Vec<Completeness>,
    /// Préfixe d'URL de page.
    pub page_prefix: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub id: String,
    pub score: f32,
    pub page_url: String,
    pub title: String,
    pub heading_path: Vec<String>,
    pub snippet: String,
    pub version: Option<String>,
    pub fetched_at: u64,
    pub completeness: Completeness,
    pub quality: f32,
    /// Termes (racinisés, sans mots vides) de la question.
    pub query_terms: u32,
    /// Parmi eux, ceux que cette section contient (n'importe quel champ).
    pub matched_terms: u32,
    /// Parmi eux, ceux que la source contient quelque part (le reste n'existe dans aucune page).
    pub corpus_terms: u32,
    /// Noms propres / identifiants de la question (MongoDB, DocSearch, QUIC…) qu'AUCUNE page de la source ne contient.
    pub missing_names: u32,
    /// Part du « poids de rareté » (idf) des termes que la source connaît qui se trouve dans cette
    /// section : 1 = elle porte tous les termes distinctifs ; les termes qu'AUCUNE page ne contient
    /// (mots d'une autre langue que la doc) ne comptent pas, ils ne diraient rien d'une section.
    pub coverage: f32,
}

impl Hit {
    /// Confiance 0..1 dans la réponse : la question est-elle couverte par cette section ?
    /// Une question qui nomme une chose absente de la source (`missing_names > 0`) n'a pas de réponse
    /// fiable ici ; sinon, part du poids de rareté des termes connus de la source que la section porte
    /// (au moins un tiers pour répondre : en dessous, la question parle d'autre chose que de la section).
    pub fn confidence(&self) -> f32 {
        if self.query_terms == 0 {
            return 0.0;
        }
        if self.missing_names > 0 {
            return 0.0;
        }
        self.coverage.clamp(0.0, 1.0)
    }
}

/// Version à privilégier quand une documentation existe en plusieurs versions : celle que la question
/// nomme si elle désigne UNE version connue (`PostgreSQL 17`, `ureq 3`, `Python 3.11`), sinon `primary`
/// (la version du lockfile, ou de la source principale). Un nombre qui désigne plusieurs versions
/// (`3` pour 3.13 et 3.11) ou aucune (`404`, `1.1`) ne choisit rien.
pub fn pick_version(question: &str, known: &[String], primary: Option<&str>) -> Option<String> {
    let norm = |v: &str| v.trim().trim_start_matches(['v', 'V']).to_string();
    let known: Vec<String> = known.iter().map(|v| norm(v)).filter(|v| !v.is_empty()).collect();
    let b = question.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'.')) {
            let mut j = i;
            while j < b.len() && (b[j].is_ascii_digit() || (b[j] == b'.' && j + 1 < b.len() && b[j + 1].is_ascii_digit())) {
                j += 1;
            }
            let tok = &question[i..j];
            let exact: Vec<&String> = known.iter().filter(|k| k.as_str() == tok).collect();
            let prefix: Vec<&String> = known.iter().filter(|k| k.starts_with(tok) && k.as_bytes().get(tok.len()) == Some(&b'.')).collect();
            let hit = if exact.len() == 1 {
                Some(exact[0])
            } else if exact.is_empty() && prefix.len() == 1 {
                Some(prefix[0])
            } else {
                None
            };
            if let Some(v) = hit {
                return Some(v.clone());
            }
            i = j;
        } else {
            i += 1;
        }
    }
    primary.map(norm).filter(|v| !v.is_empty())
}

/// « aussi dans <version> » : la meilleure réponse d'une AUTRE version, si elle bat celle de la version
/// choisie (ou si celle-ci ne répond rien).
pub fn also_in<'a>(main: &[Hit], others: &'a [Hit]) -> Option<&'a Hit> {
    let best = others.first()?;
    match main.first() {
        None => Some(best),
        Some(m) if best.score > m.score => Some(best),
        _ => None,
    }
}

/// Seuil de `Hit::confidence` sous lequel `find` répond « rien de fiable ».
pub const MIN_CONFIDENCE: f32 = 0.33;

#[derive(Clone, Debug, Default)]
pub struct IndexStats {
    pub docs: usize,
    pub terms: usize,
    pub postings: u64,
    pub bytes: usize,
}

struct UrlInfo {
    url: String,
    quality: f32,
    fetched_at: u64,
}

struct Doc {
    offset: u64,
    len: u32,
    url: u32,
    ver: u32, // 0 = inconnue, sinon indice + 1
    idh: u32,
    flen: [u32; 3],
    comp: u8,
}

pub struct Index {
    data: Vec<u8>,
    sections_path: PathBuf,
    n_docs: usize,
    n_terms: usize,
    avg: [f32; 3],
    urls: Vec<UrlInfo>,
    versions: Vec<String>,
    docs: Vec<Doc>,
    idh: Vec<(u32, u32)>,
    off_tidx: usize,
    off_tblob: usize,
    off_post: usize,
    post_total: u64,
    fingerprint: [u64; 4],
    rebuilt: bool,
}

// ───────────────────────── Découpage en termes ─────────────────────────

/// Mots alphanumériques de `text` (`char::is_alphanumeric`, comme `query_terms`) ; pour chacun,
/// les jetons minuscules dans l'ordre de `tokenize_identifier` (camelCase en morceaux puis mot
/// entier), accents repliés. `f(début, fin, jeton)` : les octets sont ceux de `text`.
fn for_each_token(text: &str, mut f: impl FnMut(usize, usize, &str)) {
    let mut start: Option<usize> = None;
    let mut end_idx = 0usize;
    for (i, c) in text.char_indices() {
        if c.is_alphanumeric() {
            if start.is_none() {
                start = Some(i);
            }
            end_idx = i + c.len_utf8();
        } else if let Some(s) = start.take() {
            emit_word(&text[s..end_idx], s, end_idx, &mut f);
        }
    }
    if let Some(s) = start {
        emit_word(&text[s..end_idx], s, end_idx, &mut f);
    }
}

fn emit_word(w: &str, s: usize, e: usize, f: &mut impl FnMut(usize, usize, &str)) {
    if w.is_ascii() {
        emit_folded(w, s, e, f);
    } else {
        let folded = crate::symbol::fold_accents(w);
        emit_folded(&folded, s, e, f);
    }
}

fn emit_folded(w: &str, s: usize, e: usize, f: &mut impl FnMut(usize, usize, &str)) {
    if w.chars().any(|c| c.is_uppercase()) {
        for t in crate::symbol::tokenize_identifier(w) {
            f(s, e, &t);
        }
    } else {
        f(s, e, w);
    }
}

fn keep_token(t: &str) -> bool {
    t.len() >= 2 && t.len() <= MAX_TERM_LEN && !crate::symbol::is_stopword(t)
}

/// Racines des termes de `text`, dans l'ordre (mots vides retirés).
/// Noms propres et identifiants de la question : mot de plus de 2 lettres avec une majuscule ailleurs
/// qu'en tête (`MongoDB`, `DocSearch`, `QUIC`), ou capitalisé hors début de phrase (`Heroku`), ou mêlant
/// lettres et chiffres. Les racines (minuscules, accents repliés) servent à vérifier qu'ils existent
/// quelque part dans la source.
fn entity_stems(question: &str) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    let mut first = true;
    for raw in question.split(|c: char| !c.is_alphanumeric()) {
        if raw.is_empty() {
            continue;
        }
        let at_start = first;
        first = false;
        if raw.chars().count() < 3 || raw.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let mut cs = raw.chars();
        let c0 = cs.next().unwrap_or(' ');
        let rest_upper = cs.any(|c| c.is_uppercase());
        let has_digit = raw.chars().any(|c| c.is_ascii_digit());
        let capitalized = c0.is_uppercase() && !at_start;
        if rest_upper || capitalized || has_digit {
            let mut last: Option<String> = None;
            for_each_token(raw, |_, _, t| last = Some(t.to_string()));
            let whole = last.unwrap_or_else(|| raw.to_lowercase());
            // Formes acceptées : racine, mot entier, mot sans `s` final (pluriel : `URLs` / `url`).
            let mut alts = vec![crate::stem::stem(&whole), whole.clone()];
            if let Some(sing) = whole.strip_suffix('s') {
                alts.push(sing.to_string());
                alts.push(crate::stem::stem(sing));
            }
            alts.retain(|a| !a.is_empty());
            if !alts.is_empty() && !out.contains(&alts) {
                out.push(alts);
            }
        }
    }
    out
}

/// Clé d'identité d'une page : `/index.html`, `.html`, `.md`, `.mdx` et `/` final retirés.
fn twin_key(url: &str) -> String {
    let u = url.split('#').next().unwrap_or(url);
    let mut k = u.to_string();
    for suf in ["/index.html", ".html", ".mdx", ".md"] {
        if let Some(x) = k.strip_suffix(suf) {
            k = x.to_string();
            break;
        }
    }
    while k.ends_with('/') {
        k.pop();
    }
    k
}

/// Pages jumelles (la même page servie sous `/page`, `/page.md`, `/dossier/` et `/dossier/index.html`) :
/// une seule reste dans l'index (HTML plutôt que Markdown brut ; sinon la première collectée).
fn twin_pages_to_skip(records: &[super::PageRecord]) -> std::collections::HashSet<String> {
    let mut groups: HashMap<String, Vec<&super::PageRecord>> = HashMap::new();
    for r in records {
        groups.entry(twin_key(&r.url)).or_default().push(r);
    }
    let mut skip = std::collections::HashSet::new();
    for (_, g) in groups {
        if g.len() < 2 {
            continue;
        }
        let md = |u: &str| u.ends_with(".md") || u.ends_with(".mdx");
        let keep =
            g.iter().find(|r| !md(&r.url) && !r.url.ends_with("index.html")).or_else(|| g.iter().find(|r| !md(&r.url))).unwrap_or(&g[0]);
        for r in &g {
            if r.url != keep.url {
                skip.insert(r.url.clone());
            }
        }
    }
    skip
}

fn stems_of(text: &str) -> Vec<String> {
    let mut v = Vec::new();
    for_each_token(text, |_, _, t| {
        if keep_token(t) {
            v.push(crate::stem::stem(t));
        }
    });
    v
}

/// Sépare un texte Markdown en (prose, code) : blocs clôturés et mots entre accents graves
/// vont dans le code.
fn split_code(text: &str) -> (String, String) {
    let mut prose = String::with_capacity(text.len());
    let mut code = String::new();
    let mut fence = false;
    for line in text.lines() {
        if is_fence(line) {
            fence = !fence;
            continue;
        }
        if fence {
            code.push_str(line);
            code.push('\n');
        } else if line.matches('`').count() >= 2 && line.matches('`').count() % 2 == 0 {
            for (i, seg) in line.split('`').enumerate() {
                let dst = if i % 2 == 1 { &mut code } else { &mut prose };
                dst.push_str(seg);
                dst.push(' ');
            }
            prose.push('\n');
        } else {
            prose.push_str(line);
            prose.push('\n');
        }
    }
    (prose, code)
}

fn is_fence(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("```") || t.starts_with("~~~")
}

fn fnv32(s: &str) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    for b in s.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    h
}

fn comp_code(c: Completeness) -> u8 {
    match c {
        Completeness::Complete => 0,
        Completeness::Partial => 1,
        Completeness::NeedsJs => 2,
        Completeness::Blocked => 3,
    }
}

fn comp_from(b: u8) -> Completeness {
    match b {
        1 => Completeness::Partial,
        2 => Completeness::NeedsJs,
        3 => Completeness::Blocked,
        _ => Completeness::Complete,
    }
}

// ───────────────────────── Octets ─────────────────────────

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

struct Rd<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.p.checked_add(n)?;
        let s = self.b.get(self.p..end)?;
        self.p = end;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|s| s[0])
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4).map(|s| u32::from_le_bytes(s.try_into().unwrap()))
    }
    fn u64(&mut self) -> Option<u64> {
        self.take(8).map(|s| u64::from_le_bytes(s.try_into().unwrap()))
    }
    fn f32(&mut self) -> Option<f32> {
        self.take(4).map(|s| f32::from_le_bytes(s.try_into().unwrap()))
    }
    fn varint(&mut self) -> Option<u64> {
        varint_at(self.b, &mut self.p)
    }
    fn string(&mut self) -> Option<String> {
        let n = self.varint()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).ok()
    }
}

fn varint_at(b: &[u8], p: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *b.get(*p)?;
        *p += 1;
        v |= ((byte & 0x7f) as u64) << shift;
        if byte < 0x80 {
            return Some(v);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

// ───────────────────────── Construction ─────────────────────────

struct TermAcc {
    buf: Vec<u8>,
    df: u32,
    last: u32,
}

struct Builder {
    memo: HashMap<String, u32>,
    by_stem: HashMap<String, u32>,
    names: Vec<String>,
    terms: Vec<TermAcc>,
    dense: Vec<[u32; 3]>,
    touched: Vec<u32>,
}

impl Builder {
    fn new() -> Builder {
        Builder {
            memo: HashMap::new(),
            by_stem: HashMap::new(),
            names: Vec::new(),
            terms: Vec::new(),
            dense: Vec::new(),
            touched: Vec::new(),
        }
    }

    fn term_id(&mut self, tok: &str) -> u32 {
        if let Some(&i) = self.memo.get(tok) {
            return i;
        }
        let id = if keep_token(tok) {
            let st = crate::stem::stem(tok);
            match self.by_stem.get(&st) {
                Some(&i) => i,
                None => {
                    let i = self.names.len() as u32;
                    self.names.push(st.clone());
                    self.terms.push(TermAcc { buf: Vec::new(), df: 0, last: 0 });
                    self.dense.push([0; 3]);
                    self.by_stem.insert(st, i);
                    i
                }
            }
        } else {
            NONE
        };
        self.memo.insert(tok.to_string(), id);
        id
    }

    fn add_text(&mut self, text: &str, field: usize) -> u32 {
        let mut n = 0u32;
        for_each_token(text, |_, _, tok| {
            let id = self.term_id(tok);
            if id != NONE {
                let d = &mut self.dense[id as usize];
                if *d == [0u32; 3] {
                    self.touched.push(id);
                }
                d[field] += 1;
                n += 1;
            }
        });
        n
    }

    /// Écrit les postings du document `doc` et remet les compteurs à zéro.
    fn commit_doc(&mut self, doc: u32) {
        for &t in &self.touched {
            let tf = std::mem::replace(&mut self.dense[t as usize], [0; 3]);
            let acc = &mut self.terms[t as usize];
            let delta = doc - acc.last;
            acc.last = doc;
            acc.df += 1;
            let mask = (tf[0] > 0) as u64 | ((tf[1] > 0) as u64) << 1 | ((tf[2] > 0) as u64) << 2;
            put_varint(&mut acc.buf, (delta as u64) << 3 | mask);
            for f in 0..3 {
                if tf[f] > 0 {
                    put_varint(&mut acc.buf, tf[f] as u64);
                }
            }
        }
        self.touched.clear();
    }
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    put_varint(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

impl Index {
    /// Construit l'index de toutes les sections vivantes du `Store`, l'écrit dans `index.bin`
    /// (atomique) et le renvoie.
    pub fn build(store: &Store) -> io::Result<Index> {
        let fingerprint = store.fingerprint();
        let records = store.records();
        let mut url_idx: HashMap<String, u32> = HashMap::new();
        let mut urls: Vec<UrlInfo> = Vec::new();
        let mut url_comp: Vec<u8> = Vec::new();
        for r in &records {
            url_idx.insert(r.url.clone(), urls.len() as u32);
            urls.push(UrlInfo { url: r.url.clone(), quality: r.quality, fetched_at: r.fetched_at });
            url_comp.push(comp_code(r.completeness));
        }
        let mut ver_idx: HashMap<String, u32> = HashMap::new();
        let mut versions: Vec<String> = Vec::new();
        let twins = twin_pages_to_skip(&records);

        let mut b = Builder::new();
        let mut docs: Vec<Doc> = Vec::new();
        let mut totals = [0u64; 3];
        for (offset, len, s) in store.sections_at() {
            let Some(&u) = url_idx.get(&s.page_url) else { continue };
            if twins.contains(&s.page_url) {
                continue;
            }
            let doc = docs.len() as u32;
            let mut head = s.heading_path.join(" ");
            if s.heading_path.first().map(|h| h != &s.page_title).unwrap_or(true) {
                head.push(' ');
                head.push_str(&s.page_title);
            }
            let (prose, code) = split_code(&s.text);
            let mut flen = [0u32; 3];
            flen[0] = b.add_text(&head, 0);
            flen[1] = b.add_text(&code, 1);
            flen[2] = b.add_text(&prose, 2);
            b.commit_doc(doc);
            for f in 0..3 {
                totals[f] += flen[f] as u64;
            }
            let ver = match &s.version {
                None => 0,
                Some(v) => *ver_idx.entry(v.clone()).or_insert_with(|| {
                    versions.push(v.clone());
                    versions.len() as u32
                }),
            };
            docs.push(Doc { offset, len, url: u, ver, idh: fnv32(&s.id), flen, comp: comp_code(s.completeness) });
        }

        // Termes triés (recherche dichotomique à la lecture).
        let mut order: Vec<u32> = (0..b.names.len() as u32).collect();
        order.sort_by(|&x, &y| b.names[x as usize].as_bytes().cmp(b.names[y as usize].as_bytes()));
        let mut tblob: Vec<u8> = Vec::new();
        let mut tidx: Vec<u8> = Vec::with_capacity(order.len() * 4);
        let mut post: Vec<u8> = Vec::new();
        let mut post_total = 0u64;
        for &t in &order {
            let name = b.names[t as usize].as_bytes();
            let acc = &b.terms[t as usize];
            tidx.extend_from_slice(&(tblob.len() as u32).to_le_bytes());
            tblob.push(name.len().min(255) as u8);
            tblob.extend_from_slice(&name[..name.len().min(255)]);
            put_varint(&mut tblob, acc.df as u64);
            put_varint(&mut tblob, post.len() as u64);
            put_varint(&mut tblob, acc.buf.len() as u64);
            post.extend_from_slice(&acc.buf);
            post_total += acc.df as u64;
        }

        let mut out: Vec<u8> = Vec::with_capacity(post.len() + tblob.len() + tidx.len() + docs.len() * 56 + 4096);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        for v in fingerprint {
            out.extend_from_slice(&v.to_le_bytes());
        }
        for v in [docs.len() as u32, urls.len() as u32, versions.len() as u32, order.len() as u32] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        for t in totals {
            out.extend_from_slice(&t.to_le_bytes());
        }
        out.extend_from_slice(&(tblob.len() as u64).to_le_bytes());
        out.extend_from_slice(&(post.len() as u64).to_le_bytes());
        out.extend_from_slice(&post_total.to_le_bytes());
        for (i, u) in urls.iter().enumerate() {
            put_str(&mut out, &u.url);
            out.extend_from_slice(&u.quality.to_le_bytes());
            out.extend_from_slice(&u.fetched_at.to_le_bytes());
            out.push(url_comp[i]);
        }
        for v in &versions {
            put_str(&mut out, v);
        }
        for d in &docs {
            out.extend_from_slice(&d.offset.to_le_bytes());
            for v in [d.len, d.url, d.ver, d.idh, d.flen[0], d.flen[1], d.flen[2]] {
                out.extend_from_slice(&v.to_le_bytes());
            }
            out.extend_from_slice(&[d.comp, 0, 0, 0]);
        }
        let mut idh: Vec<(u32, u32)> = docs.iter().enumerate().map(|(i, d)| (d.idh, i as u32)).collect();
        idh.sort_unstable();
        for (h, d) in &idh {
            out.extend_from_slice(&h.to_le_bytes());
            out.extend_from_slice(&d.to_le_bytes());
        }
        out.extend_from_slice(&tidx);
        out.extend_from_slice(&tblob);
        out.extend_from_slice(&post);
        let sum = blake3::hash(&out);
        out.extend_from_slice(sum.as_bytes());
        write_atomic(&store.index_path(), &out)?;
        let mut ix = Index::from_bytes(out, store.sections_path(), true)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "index fraîchement construit illisible"))?;
        ix.rebuilt = true;
        Ok(ix)
    }

    /// Recharge `index.bin` s'il est valide ET à jour par rapport aux fichiers du `Store`
    /// (taille et date de `sections.jsonl` / `pages.jsonl`) ; sinon le reconstruit.
    pub fn open(store: &Store) -> io::Result<Index> {
        if let Ok(data) = std::fs::read(store.index_path()) {
            if let Some(ix) = Index::from_bytes(data, store.sections_path(), false) {
                if ix.fingerprint == store.fingerprint() {
                    return Ok(ix);
                }
            }
        }
        Index::build(store)
    }

    /// Vrai si cet objet vient d'être reconstruit (et non rechargé).
    pub fn rebuilt(&self) -> bool {
        self.rebuilt
    }

    fn from_bytes(data: Vec<u8>, sections_path: PathBuf, trusted: bool) -> Option<Index> {
        if data.len() < 8 + 32 {
            return None;
        }
        let body = data.len() - 32;
        if !trusted && blake3::hash(&data[..body]).as_bytes()[..] != data[body..] {
            return None;
        }
        let mut r = Rd { b: &data[..body], p: 0 };
        if r.take(4)? != MAGIC || r.u32()? != VERSION {
            return None;
        }
        let fingerprint = [r.u64()?, r.u64()?, r.u64()?, r.u64()?];
        let n_docs = r.u32()? as usize;
        let n_urls = r.u32()? as usize;
        let n_versions = r.u32()? as usize;
        let n_terms = r.u32()? as usize;
        let totals = [r.u64()?, r.u64()?, r.u64()?];
        let tblob_len = r.u64()? as usize;
        let post_len = r.u64()? as usize;
        let post_total = r.u64()?;
        let mut urls = Vec::with_capacity(n_urls.min(1 << 20));
        for _ in 0..n_urls {
            let url = r.string()?;
            let quality = r.f32()?;
            let fetched_at = r.u64()?;
            let _comp = r.u8()?;
            urls.push(UrlInfo { url, quality, fetched_at });
        }
        let mut versions = Vec::with_capacity(n_versions.min(1 << 16));
        for _ in 0..n_versions {
            versions.push(r.string()?);
        }
        let mut docs = Vec::with_capacity(n_docs.min(1 << 24));
        for _ in 0..n_docs {
            let offset = r.u64()?;
            let len = r.u32()?;
            let url = r.u32()?;
            let ver = r.u32()?;
            let idh = r.u32()?;
            let flen = [r.u32()?, r.u32()?, r.u32()?];
            let comp = r.u8()?;
            r.take(3)?;
            if url as usize >= urls.len() || ver as usize > versions.len() {
                return None;
            }
            docs.push(Doc { offset, len, url, ver, idh, flen, comp });
        }
        let mut idh = Vec::with_capacity(n_docs);
        for _ in 0..n_docs {
            idh.push((r.u32()?, r.u32()?));
        }
        let off_tidx = r.p;
        r.take(n_terms.checked_mul(4)?)?;
        let off_tblob = r.p;
        r.take(tblob_len)?;
        let off_post = r.p;
        r.take(post_len)?;
        if r.p != body {
            return None;
        }
        let avg = |f: usize| if n_docs == 0 { 0.0 } else { totals[f] as f32 / n_docs as f32 };
        Some(Index {
            data,
            sections_path,
            n_docs,
            n_terms,
            avg: [avg(0), avg(1), avg(2)],
            urls,
            versions,
            docs,
            idh,
            off_tidx,
            off_tblob,
            off_post,
            post_total,
            fingerprint,
            rebuilt: false,
        })
    }

    pub fn stats(&self) -> IndexStats {
        IndexStats { docs: self.n_docs, terms: self.n_terms, postings: self.post_total, bytes: self.data.len() }
    }

    // ───────────────────────── Requête ─────────────────────────

    /// `(octets du nom, df, offset des postings, longueur)` du terme n° `i` (ordre trié).
    fn term_at(&self, i: usize) -> Option<(&[u8], u32, usize, usize)> {
        let t = self.off_tidx + i * 4;
        let off = u32::from_le_bytes(self.data.get(t..t + 4)?.try_into().ok()?) as usize;
        let mut p = self.off_tblob + off;
        let n = *self.data.get(p)? as usize;
        p += 1;
        let name = self.data.get(p..p + n)?;
        p += n;
        let df = varint_at(&self.data, &mut p)? as u32;
        let poff = varint_at(&self.data, &mut p)? as usize;
        let plen = varint_at(&self.data, &mut p)? as usize;
        Some((name, df, poff, plen))
    }

    fn lookup(&self, term: &str) -> Option<(u32, usize, usize)> {
        let (mut lo, mut hi) = (0usize, self.n_terms);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let (name, df, poff, plen) = self.term_at(mid)?;
            match name.cmp(term.as_bytes()) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some((df, poff, plen)),
            }
        }
        None
    }

    fn version_ok(&self, d: &Doc, want: &str) -> bool {
        if d.ver == 0 {
            return true;
        }
        let have = self.versions[d.ver as usize - 1].trim_start_matches('v');
        let want = want.trim_start_matches('v');
        have == want || (have.starts_with(want) && have.as_bytes().get(want.len()) == Some(&b'.'))
    }

    fn filter_ok(&self, d: &Doc, f: &Filter) -> bool {
        let u = &self.urls[d.url as usize];
        if let Some(q) = f.min_quality {
            if u.quality < q {
                return false;
            }
        }
        if !f.completeness.is_empty() && !f.completeness.contains(&comp_from(d.comp)) {
            return false;
        }
        if let Some(p) = &f.page_prefix {
            if !u.url.starts_with(p.as_str()) {
                return false;
            }
        }
        match &f.version {
            Some(v) => self.version_ok(d, v),
            None => true,
        }
    }

    /// Meilleures sections pour `question` (BM25F + bonus), au plus `limit`, avec extrait.
    pub fn query(&self, question: &str, filter: &Filter, limit: usize) -> Vec<Hit> {
        if limit == 0 || self.n_docs == 0 {
            return Vec::new();
        }
        let mut qs: Vec<String> = Vec::new();
        for t in crate::search::query_terms(question) {
            let s = crate::stem::stem(&t);
            if !qs.contains(&s) {
                qs.push(s);
            }
        }
        qs.truncate(MAX_QUERY_TERMS);
        if qs.is_empty() {
            return Vec::new();
        }
        let query_terms = qs.len() as u32;
        let corpus_terms = qs.iter().filter(|t| self.lookup(t).is_some()).count() as u32;
        let missing_names = entity_stems(question)
            .iter()
            .filter(|alts| alts.iter().any(|a| qs.contains(a)) && alts.iter().all(|a| self.lookup(a).is_none()))
            .count() as u32;
        let mut known_w = 0f32;
        let mut matched_w = vec![0f32; self.n_docs];
        let n = self.n_docs as f32;
        let mut score = vec![0f32; self.n_docs];
        let mut matched = vec![0u8; self.n_docs];
        let mut touched: Vec<u32> = Vec::new();
        let norm_den = [self.avg[0].max(1e-6), self.avg[1].max(1e-6), self.avg[2].max(1e-6)];
        for term in &qs {
            let Some((df, poff, plen)) = self.lookup(term) else { continue };
            let idf = ((n - df as f32 + 0.5) / (df as f32 + 0.5) + 1.0).ln();
            known_w += idf;
            let Some(post) = self.data.get(self.off_post + poff..self.off_post + poff + plen) else { continue };
            let mut p = 0usize;
            let mut doc = 0u32;
            while p < post.len() {
                let Some(v) = varint_at(post, &mut p) else { break };
                doc += (v >> 3) as u32;
                let mask = (v & 7) as u8;
                let mut tf = [0u32; 3];
                for f in 0..3 {
                    if mask & (1 << f) != 0 {
                        tf[f] = varint_at(post, &mut p).unwrap_or(0) as u32;
                    }
                }
                let Some(d) = self.docs.get(doc as usize) else { break };
                let mut tfp = 0f32;
                for f in 0..3 {
                    if tf[f] > 0 {
                        tfp += W[f] * tf[f] as f32 / (1.0 - B + B * d.flen[f] as f32 / norm_den[f]);
                    }
                }
                let di = doc as usize;
                if matched[di] == 0 {
                    touched.push(doc);
                }
                matched[di] = matched[di].saturating_add(1);
                matched_w[di] += idf;
                score[di] += idf * tfp / (K1 + tfp);
            }
        }
        if touched.is_empty() {
            return Vec::new();
        }
        // Premier tri : sans lecture disque (filtres, tous les termes, complétude).
        let mut cands: Vec<(f32, u32)> = Vec::with_capacity(touched.len());
        for &doc in &touched {
            let d = &self.docs[doc as usize];
            if !self.filter_ok(d, filter) {
                continue;
            }
            let mut s = score[doc as usize];
            if qs.len() >= 2 && matched[doc as usize] as usize == qs.len() {
                s *= 1.3;
            }
            s *= match comp_from(d.comp) {
                Completeness::Complete => 1.0,
                Completeness::Partial => 0.8,
                Completeness::NeedsJs => 0.6,
                Completeness::Blocked => 0.35,
            };
            cands.push((s, doc));
        }
        let by_score = |a: &(f32, u32), b: &(f32, u32)| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then(a.1.cmp(&b.1));
        let k = (limit * 3).max(30);
        if cands.len() > k {
            cands.select_nth_unstable_by(k, by_score);
            cands.truncate(k);
        }
        cands.sort_by(by_score);

        // Second temps : relecture des candidats (bonus de titre, extrait).
        let Ok(mut file) = File::open(&self.sections_path) else { return Vec::new() };
        let mut ranked: Vec<(f32, u32, StoredSection)> = Vec::with_capacity(cands.len());
        for (s0, doc) in cands {
            let d = &self.docs[doc as usize];
            let Some(sec) = read_section_from(&mut file, d.offset, d.len) else { continue };
            if fnv32(&sec.id) != d.idh {
                continue; // fichier modifié depuis la construction
            }
            let mut bonus = 1.0f32;
            let titles = [sec.heading_path.last().map(|s| s.as_str()).unwrap_or(""), sec.page_title.as_str()];
            for t in titles {
                if !t.is_empty() {
                    bonus = bonus.max(phrase_bonus(&stems_of(t), &qs));
                }
            }
            // À page égale, la section dont son PROPRE titre (dernier niveau du chemin) porte les termes passe devant.
            if let Some(leaf) = sec.heading_path.last() {
                let ls = stems_of(leaf);
                let hit = qs.iter().filter(|q| ls.contains(q)).count();
                if hit > 0 {
                    bonus *= 1.0 + LEAF_BONUS * hit as f32 / qs.len() as f32;
                }
            }
            ranked.push((s0 * bonus, doc, sec));
        }
        ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then(a.1.cmp(&b.1)));
        ranked.truncate(limit);
        ranked
            .into_iter()
            .map(|(score, doc, sec)| {
                let d = &self.docs[doc as usize];
                let u = &self.urls[d.url as usize];
                Hit {
                    snippet: make_snippet(&sec.text, &qs, 200),
                    id: sec.id,
                    score,
                    page_url: sec.page_url,
                    title: sec.page_title,
                    heading_path: sec.heading_path,
                    version: sec.version,
                    fetched_at: u.fetched_at.max(sec.fetched_at),
                    completeness: comp_from(d.comp),
                    quality: u.quality,
                    query_terms,
                    matched_terms: matched[doc as usize] as u32,
                    corpus_terms,
                    missing_names,
                    coverage: if known_w > 0.0 { (matched_w[doc as usize] / known_w).min(1.0) } else { 0.0 },
                }
            })
            .collect()
    }

    /// Relit une section par son identifiant (`cortex web read <id>`).
    pub fn get_section(&self, id: &str) -> Option<StoredSection> {
        let h = fnv32(id);
        let start = self.idh.partition_point(|&(x, _)| x < h);
        let mut file = File::open(&self.sections_path).ok()?;
        for &(x, doc) in &self.idh[start..] {
            if x != h {
                break;
            }
            let d = &self.docs[doc as usize];
            if let Some(s) = read_section_from(&mut file, d.offset, d.len) {
                if s.id == id {
                    return Some(s);
                }
            }
        }
        None
    }
}

fn contains_run(hay: &[String], needle: &[String]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

/// Titre égal à la requête ×1,6 ; requête (≥ 2 termes) contenue dans le titre ×1,35.
fn phrase_bonus(title: &[String], qs: &[String]) -> f32 {
    if title.is_empty() || qs.is_empty() {
        return 1.0;
    }
    if title == qs {
        1.6
    } else if qs.len() >= 2 && contains_run(title, qs) {
        1.35
    } else {
        1.0
    }
}

// ───────────────────────── Extrait ─────────────────────────

fn count_fences(s: &str) -> usize {
    s.lines().filter(|l| is_fence(l)).count()
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn line_start(s: &str, i: usize) -> usize {
    s[..i].rfind('\n').map(|p| p + 1).unwrap_or(0)
}

fn line_end(s: &str, i: usize) -> usize {
    s[i..].find('\n').map(|p| i + p).unwrap_or(s.len())
}

/// Meilleure fenêtre d'environ `width` caractères autour des termes de la requête : jamais au
/// milieu d'un mot, et toujours avec des clôtures de code équilibrées (une fenêtre qui commence
/// ou finit dans un bloc est alignée sur des lignes entières et refermée).
fn make_snippet(text: &str, qs: &[String], width: usize) -> String {
    let text = text.trim();
    if text.len() <= width + width / 5 {
        return text.to_string();
    }
    // Positions des termes de la requête.
    let mut ms: Vec<(usize, usize)> = Vec::new(); // (début, indice de terme)
    for_each_token(text, |s, _, tok| {
        if !keep_token(tok) {
            return;
        }
        let st = crate::stem::stem(tok);
        if let Some(i) = qs.iter().position(|q| *q == st) {
            ms.push((s, i));
        }
    });
    ms.truncate(400);
    let lead = width / 4;
    let mut best_start = 0usize;
    let mut best = (0usize, 0usize);
    for (k, &(pos, _)) in ms.iter().enumerate() {
        let ws = floor_boundary(text, pos.saturating_sub(lead));
        let we = ws + width;
        let mut seen = 0u64;
        let mut cnt = 0usize;
        for &(p, t) in &ms[..] {
            if p >= ws && p < we {
                seen |= 1u64 << (t % 63);
                cnt += 1;
            }
        }
        let score = (seen.count_ones() as usize, cnt);
        if score > best || k == 0 {
            best = score;
            best_start = ws;
        }
    }
    let mut ws = best_start;
    let mut we = floor_boundary(text, ws + width);
    // Début : au début d'un mot.
    let mut steps = 0;
    while ws > 0 && steps < 40 {
        let prev = text[..ws].chars().next_back().unwrap();
        if prev.is_whitespace() {
            break;
        }
        ws -= prev.len_utf8();
        steps += 1;
    }
    // Fin : à la fin d'un mot.
    steps = 0;
    while we < text.len() && steps < 40 {
        let c = text[we..].chars().next().unwrap();
        if c.is_whitespace() {
            break;
        }
        we += c.len_utf8();
        steps += 1;
    }
    // Clôtures de code.
    let open_before = count_fences(&text[..line_start(text, ws)]) % 2 == 1;
    if open_before {
        ws = line_start(text, ws);
    }
    let open_at_end = (count_fences(&text[..line_start(text, we.min(text.len()))])
        + if is_fence(&text[line_start(text, we.min(text.len()))..line_end(text, we.min(text.len()))]) { 1 } else { 0 })
        % 2
        == 1;
    if open_at_end {
        we = line_end(text, we.min(text.len()));
    }
    let mut out = String::new();
    let body = text[ws..we].trim();
    if open_before {
        // La clôture ouvrante dit déjà que le bloc est coupé : pas de « … » devant.
        out.push_str("```\n");
    } else if ws > 0 {
        out.push('…');
    }
    out.push_str(body);
    // Équilibre final : état ouvert à la fin de la fenêtre => on referme.
    let toggles = count_fences(body) + if open_before { 1 } else { 0 };
    if toggles % 2 == 1 {
        out.push_str("\n```");
    }
    if we < text.len() {
        out.push('…');
    }
    out
}

// ───────────────────────── Cartes ─────────────────────────

fn est_tokens(s: &str) -> usize {
    (s.chars().count() + 3) / 4
}

/// `YYYY-MM-DD` (UTC) d'un horodatage Unix ; vide pour 0.
fn date_iso(secs: u64) -> String {
    if secs == 0 {
        return "date inconnue".into();
    }
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{:02}-{:02}", if m <= 2 { y + 1 } else { y }, m, d)
}

fn confiance(h: &Hit) -> &'static str {
    match h.completeness {
        Completeness::Complete if h.quality >= 0.6 => "confiance haute",
        Completeness::Complete => "confiance moyenne",
        Completeness::Partial => "confiance faible (page partielle)",
        Completeness::NeedsJs => "confiance faible (rendu JS requis)",
        Completeness::Blocked => "confiance nulle (page bloquée)",
    }
}

/// Coupe `s` à `max` caractères au plus, sans couper un mot, en refermant un bloc de code ouvert.
fn fit_snippet(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut end = s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len());
    while end > 0 && !s[..end].ends_with(char::is_whitespace) && !s[end..].starts_with(char::is_whitespace) {
        end -= s[..end].chars().next_back().map(|c| c.len_utf8()).unwrap_or(1);
    }
    let mut out = s[..end].trim_end().to_string();
    if count_fences(&out) % 2 == 1 {
        out.push_str("\n```");
    }
    out.push('…');
    out
}

fn card(h: &Hit, snippet_max: usize) -> String {
    let crumb = if h.heading_path.is_empty() {
        h.title.clone()
    } else if h.heading_path.first() == Some(&h.title) {
        h.heading_path.join(" › ")
    } else {
        format!("{} › {}", h.title, h.heading_path.join(" › "))
    };
    let ver = h.version.as_deref().map(|v| format!("v{}", v.trim_start_matches('v'))).unwrap_or_else(|| "version inconnue".into());
    let head = format!("{} · {} · {} · {} · {}", h.id, crumb, ver, date_iso(h.fetched_at), confiance(h));
    let next = format!("next: cortex web read {}", h.id);
    // Une carte reste sous 200 jetons (800 caractères).
    let room = 800usize.saturating_sub(head.chars().count() + next.chars().count() + 2).min(snippet_max);
    let snip = fit_snippet(h.snippet.trim(), room);
    if snip.is_empty() {
        format!("{head}\n{next}")
    } else {
        format!("{head}\n{snip}\n{next}")
    }
}

/// Rend les résultats en CARTES (une par hit, 70 à 200 jetons) jusqu'à `budget_tokens`
/// (4 caractères = 1 jeton). Le texte n'est PAS balisé : l'appelant passe par `untrusted::wrap`.
pub fn render_cards(hits: &[Hit], budget_tokens: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    let mut shown = 0usize;
    for h in hits {
        let mut c = card(h, 800);
        let mut cost = est_tokens(&c) + if shown > 0 { 1 } else { 0 };
        if used + cost > budget_tokens {
            if shown == 0 && budget_tokens >= 40 {
                // Une seule carte, extrait réduit pour tenir.
                let fixe = card(h, 0);
                let reste = budget_tokens.saturating_sub(est_tokens(&fixe)) * 4;
                c = card(h, reste.saturating_sub(4));
                cost = est_tokens(&c);
                if cost > budget_tokens {
                    c = fixe;
                    cost = est_tokens(&c);
                    if cost > budget_tokens {
                        break;
                    }
                }
            } else {
                break;
            }
        }
        if shown > 0 {
            out.push_str("\n\n");
        }
        out.push_str(&c);
        used += cost;
        shown += 1;
    }
    let reste = hits.len() - shown;
    if reste > 0 && shown > 0 {
        let note = format!("\n\n(+{reste} autre(s) résultat(s) : affiner la question ou augmenter le budget)");
        if used + est_tokens(&note) <= budget_tokens {
            out.push_str(&note);
        }
    }
    out
}

// ───────────────────────── Tests ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::PageRecord;
    use std::sync::atomic::{AtomicU32, Ordering};

    static N: AtomicU32 = AtomicU32::new(0);

    struct Tmp(PathBuf);
    impl Tmp {
        fn new() -> Tmp {
            let p = std::env::temp_dir().join(format!("cortex-web-index-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Tmp(p)
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn rec(url: &str, quality: f32, comp: Completeness) -> PageRecord {
        PageRecord {
            url: url.into(),
            canonical: None,
            status: 200,
            etag: None,
            last_modified: None,
            raw_hash: "r".into(),
            content_hash: "c".into(),
            extractor_version: 1,
            fetched_at: 1_700_000_000,
            completeness: comp,
            quality,
            sections: 0,
            bytes: 10,
        }
    }

    fn sec(url: &str, title: &str, path: &[&str], text: &str) -> StoredSection {
        let anchor = path.last().copied().unwrap_or("top").to_lowercase().replace(' ', "-");
        StoredSection {
            id: format!("W:t/{}#{}", url.trim_start_matches("https://x/"), anchor),
            page_url: url.into(),
            page_title: title.into(),
            anchor,
            heading_path: path.iter().map(|s| s.to_string()).collect(),
            level: 2,
            text: text.into(),
            code_blocks: 0,
            version: Some("1.2.3".into()),
            fetched_at: 1_700_000_000,
            completeness: Completeness::Complete,
        }
    }

    fn put(s: &mut Store, url: &str, secs: Vec<StoredSection>) {
        s.put_page(rec(url, 0.9, Completeness::Complete), secs).unwrap();
    }

    fn store(t: &Tmp) -> Store {
        Store::open(&t.0, "demo").unwrap()
    }

    fn qstems(w: &[&str]) -> Vec<String> {
        w.iter().map(|x| crate::stem::stem(x)).collect()
    }

    fn ids(h: &[Hit]) -> Vec<String> {
        h.iter().map(|x| x.id.clone()).collect()
    }

    #[test]
    fn index_vide_ne_renvoie_rien() {
        let t = Tmp::new();
        let s = store(&t);
        let ix = Index::build(&s).unwrap();
        assert_eq!(ix.stats().docs, 0);
        assert!(ix.query("routeur", &Filter::default(), 5).is_empty());
        assert!(Index::open(&s).unwrap().query("x", &Filter::default(), 5).is_empty());
    }

    #[test]
    fn trouve_la_bonne_section() {
        let t = Tmp::new();
        let mut s = store(&t);
        put(
            &mut s,
            "https://x/a",
            vec![
                sec("https://x/a", "Guide", &["Guide", "Routing"], "Les routes sont déclarées avec le routeur."),
                sec("https://x/a", "Guide", &["Guide", "Database"], "Connexion à la base de données Postgres avec un pool."),
            ],
        );
        let ix = Index::build(&s).unwrap();
        let h = ix.query("connexion postgres pool", &Filter::default(), 5);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].heading_path, vec!["Guide", "Database"]);
        assert!(h[0].score > 0.0);
        assert_eq!(h[0].version.as_deref(), Some("1.2.3"));
        assert_eq!(h[0].fetched_at, 1_700_000_000);
    }

    #[test]
    fn le_titre_pese_plus_que_le_texte() {
        let t = Tmp::new();
        let mut s = store(&t);
        put(
            &mut s,
            "https://x/a",
            vec![
                sec(
                    "https://x/a",
                    "Doc",
                    &["Doc", "Alpha"],
                    "Ici on parle surtout de middleware et de middleware encore, puis de middleware.",
                ),
                sec("https://x/a", "Doc", &["Doc", "Middleware"], "Texte sans rapport avec la question posée."),
            ],
        );
        let ix = Index::build(&s).unwrap();
        let h = ix.query("middleware", &Filter::default(), 5);
        assert_eq!(h[0].heading_path.last().unwrap(), "Middleware");
    }

    #[test]
    fn identifiants_du_code_et_backticks() {
        let t = Tmp::new();
        let mut s = store(&t);
        put(
            &mut s,
            "https://x/a",
            vec![
                sec(
                    "https://x/a",
                    "API",
                    &["API", "Une"],
                    "Un texte banal.\n\n```rust\nlet cfg = ServerConfig::builder().max_connections(10);\n```\n",
                ),
                sec("https://x/a", "API", &["API", "Deux"], "Voir aussi `useOfficeFile` pour ouvrir les documents."),
                sec("https://x/a", "API", &["API", "Trois"], "Rien d'utile ici, juste des généralités."),
            ],
        );
        let ix = Index::build(&s).unwrap();
        assert_eq!(ix.query("ServerConfig builder", &Filter::default(), 3)[0].heading_path[1], "Une");
        assert_eq!(ix.query("useOfficeFile", &Filter::default(), 3)[0].heading_path[1], "Deux");
        // Le morceau camelCase seul retrouve aussi l'identifiant.
        assert_eq!(ix.query("office file", &Filter::default(), 3)[0].heading_path[1], "Deux");
        assert_eq!(ix.query("max_connections", &Filter::default(), 3)[0].heading_path[1], "Une");
    }

    #[test]
    fn racinisation_francais_et_anglais() {
        let t = Tmp::new();
        let mut s = store(&t);
        put(
            &mut s,
            "https://x/a",
            vec![
                sec("https://x/a", "Doc", &["Doc", "Virtualisation"], "The Virtualizer renders long lists efficiently."),
                sec("https://x/a", "Doc", &["Doc", "Cache"], "Le chargement des fichiers est mis en cache."),
            ],
        );
        let ix = Index::build(&s).unwrap();
        assert_eq!(ix.query("virtualization", &Filter::default(), 3)[0].heading_path[1], "Virtualisation");
        assert_eq!(ix.query("comment charger un fichier", &Filter::default(), 3)[0].heading_path[1], "Cache");
        assert_eq!(ix.query("Où est géré le chargement ?", &Filter::default(), 3)[0].heading_path[1], "Cache");
    }

    #[test]
    fn accents_repliees_des_deux_cotes() {
        let t = Tmp::new();
        let mut s = store(&t);
        put(
            &mut s,
            "https://x/a",
            vec![sec("https://x/a", "Doc", &["Doc", "Sécurité"], "La vérification de la signature est obligatoire.")],
        );
        let ix = Index::build(&s).unwrap();
        assert_eq!(ix.query("securite verification", &Filter::default(), 3).len(), 1);
        assert_eq!(ix.query("sécurité vérification", &Filter::default(), 3).len(), 1);
    }

    #[test]
    fn filtre_de_version() {
        let t = Tmp::new();
        let mut s = store(&t);
        let mut a = sec("https://x/a", "Doc", &["Doc", "Alpha"], "timeout de connexion");
        a.version = Some("2.1.0".into());
        let mut b = sec("https://x/b", "Doc", &["Doc", "Beta"], "timeout de connexion");
        b.version = Some("3.0.0".into());
        let mut c = sec("https://x/c", "Doc", &["Doc", "Gamma"], "timeout de connexion");
        c.version = None;
        put(&mut s, "https://x/a", vec![a]);
        put(&mut s, "https://x/b", vec![b]);
        put(&mut s, "https://x/c", vec![c]);
        let ix = Index::build(&s).unwrap();
        let f = |v: &str| Filter { version: Some(v.into()), ..Default::default() };
        let r = ix.query("timeout", &f("2"), 10);
        let p: Vec<String> = r.iter().map(|h| h.heading_path[1].clone()).collect();
        assert!(p.contains(&"Alpha".to_string()) && p.contains(&"Gamma".to_string()) && !p.contains(&"Beta".to_string()), "{p:?}");
        let r = ix.query("timeout", &f("3.0.0"), 10);
        assert!(r.iter().any(|h| h.heading_path[1] == "Beta") && !r.iter().any(|h| h.heading_path[1] == "Alpha"));
        assert_eq!(ix.query("timeout", &f("21"), 10).len(), 1); // seule la section sans version passe
        assert_eq!(ix.query("timeout", &Filter::default(), 10).len(), 3);
    }

    #[test]
    fn filtres_qualite_completude_prefixe() {
        let t = Tmp::new();
        let mut s = store(&t);
        s.put_page(rec("https://x/bon", 0.9, Completeness::Complete), vec![sec("https://x/bon", "D", &["D", "Bon"], "pagination curseur")])
            .unwrap();
        s.put_page(
            rec("https://x/faible", 0.2, Completeness::Complete),
            vec![sec("https://x/faible", "D", &["D", "Faible"], "pagination curseur")],
        )
        .unwrap();
        let mut part = sec("https://x/partiel", "D", &["D", "Partiel"], "pagination curseur");
        part.completeness = Completeness::Partial;
        s.put_page(rec("https://x/partiel", 0.9, Completeness::Partial), vec![part]).unwrap();
        let ix = Index::build(&s).unwrap();
        let mq = Filter { min_quality: Some(0.5), ..Default::default() };
        assert_eq!(ix.query("pagination", &mq, 10).len(), 2);
        let c = Filter { completeness: vec![Completeness::Complete], ..Default::default() };
        assert_eq!(ix.query("pagination", &c, 10).len(), 2);
        let p = Filter { page_prefix: Some("https://x/fa".into()), ..Default::default() };
        let r = ix.query("pagination", &p, 10);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].heading_path[1], "Faible");
    }

    #[test]
    fn section_partielle_classee_apres_la_complete() {
        let t = Tmp::new();
        let mut s = store(&t);
        let mut part = sec("https://x/p", "D", &["D", "Partielle"], "gestion des erreurs réseau");
        part.completeness = Completeness::Partial;
        s.put_page(rec("https://x/p", 0.9, Completeness::Partial), vec![part]).unwrap();
        put(&mut s, "https://x/c", vec![sec("https://x/c", "D", &["D", "Complete"], "gestion des erreurs réseau")]);
        let ix = Index::build(&s).unwrap();
        let h = ix.query("gestion erreurs réseau", &Filter::default(), 5);
        assert_eq!(h[0].heading_path[1], "Complete");
        assert_eq!(h[1].completeness, Completeness::Partial);
        assert!(h[0].score > h[1].score);
    }

    #[test]
    fn bonus_phrase_exacte_du_titre() {
        let t = Tmp::new();
        let mut s = store(&t);
        put(
            &mut s,
            "https://x/a",
            vec![
                sec("https://x/a", "Doc", &["Doc", "Retry policy"], "Détails divers sur autre chose."),
                sec(
                    "https://x/a",
                    "Doc",
                    &["Doc", "Policy"],
                    "Voir la retry configuration : la retry policy est appliquée à chaque retry.",
                ),
            ],
        );
        let ix = Index::build(&s).unwrap();
        let h = ix.query("retry policy", &Filter::default(), 5);
        assert_eq!(h[0].heading_path[1], "Retry policy");
        assert!(phrase_bonus(&["retry".into(), "polic".into()], &["retry".into(), "polic".into()]) > 1.5);
        assert!(phrase_bonus(&["x".into(), "retry".into(), "polic".into()], &["retry".into(), "polic".into()]) > 1.0);
        assert_eq!(phrase_bonus(&["policy".into(), "retry".into()], &["retry".into(), "policy".into()]), 1.0);
    }

    #[test]
    fn bonus_tous_les_termes_presents() {
        let t = Tmp::new();
        let mut s = store(&t);
        put(
            &mut s,
            "https://x/a",
            vec![
                sec("https://x/a", "Doc", &["Doc", "Seul"], "stream stream stream stream stream stream stream."),
                sec("https://x/a", "Doc", &["Doc", "Les deux"], "Le stream gère le backpressure."),
            ],
        );
        let ix = Index::build(&s).unwrap();
        assert_eq!(ix.query("stream backpressure", &Filter::default(), 5)[0].heading_path[1], "Les deux");
    }

    #[test]
    fn limite_respectee_et_ordre_stable() {
        let t = Tmp::new();
        let mut s = store(&t);
        for i in 0..30 {
            put(
                &mut s,
                &format!("https://x/p{i}"),
                vec![sec(&format!("https://x/p{i}"), "D", &["D", &format!("S{i}")], "le même texte sur le cache")],
            );
        }
        let ix = Index::build(&s).unwrap();
        let a = ix.query("cache", &Filter::default(), 7);
        let b = ix.query("cache", &Filter::default(), 7);
        assert_eq!(a.len(), 7);
        assert_eq!(ids(&a), ids(&b));
        assert_eq!(ix.query("cache", &Filter::default(), 0).len(), 0);
        assert_eq!(ix.query("cache", &Filter::default(), 100).len(), 30);
    }

    #[test]
    fn requetes_degenerees_sans_panique() {
        let t = Tmp::new();
        let mut s = store(&t);
        put(&mut s, "https://x/a", vec![sec("https://x/a", "D", &["D", "A"], "du texte")]);
        let ix = Index::build(&s).unwrap();
        for q in ["", "   ", "le la les", "???", "😀", "zzzzzzzzzzzzzz", &"mot ".repeat(500), "a", "\u{0}\u{1}"] {
            let _ = ix.query(q, &Filter::default(), 5);
        }
    }

    #[test]
    fn pages_remplacees_ne_sont_plus_indexees() {
        let t = Tmp::new();
        let mut s = store(&t);
        put(&mut s, "https://x/a", vec![sec("https://x/a", "D", &["D", "Vieux"], "ancienne documentation du webhook")]);
        put(&mut s, "https://x/a", vec![sec("https://x/a", "D", &["D", "Neuf"], "nouvelle documentation du handler")]);
        let ix = Index::build(&s).unwrap();
        assert!(ix.query("webhook", &Filter::default(), 5).is_empty());
        assert_eq!(ix.query("handler", &Filter::default(), 5).len(), 1);
        assert_eq!(ix.stats().docs, 1);
    }

    #[test]
    fn persistance_et_rechargement() {
        let t = Tmp::new();
        let mut s = store(&t);
        put(&mut s, "https://x/a", vec![sec("https://x/a", "D", &["D", "Auth"], "jeton d'accès OAuth et rafraîchissement")]);
        let a = Index::open(&s).unwrap();
        assert!(a.rebuilt()); // absent : construit
        assert!(s.index_path().exists());
        let b = Index::open(&s).unwrap();
        assert!(!b.rebuilt()); // rechargé
        assert_eq!(ids(&a.query("oauth", &Filter::default(), 3)), ids(&b.query("oauth", &Filter::default(), 3)));
        assert_eq!(a.stats().terms, b.stats().terms);
        assert_eq!(a.stats().bytes, b.stats().bytes);
    }

    #[test]
    fn perime_apres_ecriture_reconstruit() {
        let t = Tmp::new();
        let mut s = store(&t);
        put(&mut s, "https://x/a", vec![sec("https://x/a", "D", &["D", "A"], "alpha")]);
        assert!(Index::open(&s).unwrap().rebuilt());
        assert!(!Index::open(&s).unwrap().rebuilt());
        put(&mut s, "https://x/b", vec![sec("https://x/b", "D", &["D", "B"], "bravo")]);
        let ix = Index::open(&s).unwrap();
        assert!(ix.rebuilt());
        assert_eq!(ix.query("bravo", &Filter::default(), 3).len(), 1);
        // Une simple mise à jour de date (304) rend aussi l'index périmé (dates des cartes).
        s.mark_unchanged("https://x/b", 1_800_000_000).unwrap();
        let ix = Index::open(&s).unwrap();
        assert!(ix.rebuilt());
        assert_eq!(ix.query("bravo", &Filter::default(), 3)[0].fetched_at, 1_800_000_000);
    }

    #[test]
    fn index_corrompu_ou_tronque_reconstruit() {
        let t = Tmp::new();
        let mut s = store(&t);
        put(&mut s, "https://x/a", vec![sec("https://x/a", "D", &["D", "A"], "charlie delta echo")]);
        Index::build(&s).unwrap();
        let p = s.index_path();
        let orig = std::fs::read(&p).unwrap();
        // 1. Un octet retourné au milieu.
        let mut b = orig.clone();
        let m = b.len() / 2;
        b[m] ^= 0x55;
        std::fs::write(&p, &b).unwrap();
        let ix = Index::open(&s).unwrap();
        assert!(ix.rebuilt());
        assert_eq!(ix.query("delta", &Filter::default(), 3).len(), 1);
        // 2. Tronqué.
        std::fs::write(&p, &orig[..orig.len() / 3]).unwrap();
        assert!(Index::open(&s).unwrap().rebuilt());
        // 3. Vide, puis déchet.
        std::fs::write(&p, b"").unwrap();
        assert!(Index::open(&s).unwrap().rebuilt());
        std::fs::write(&p, b"n'importe quoi, pas un index").unwrap();
        assert_eq!(Index::open(&s).unwrap().query("echo", &Filter::default(), 3).len(), 1);
        // 4. Somme de contrôle seule faussée.
        let mut b = orig.clone();
        let n = b.len();
        b[n - 1] ^= 1;
        std::fs::write(&p, &b).unwrap();
        assert!(Index::open(&s).unwrap().rebuilt());
        // 5. Index valide d'une autre version de format : refusé.
        let mut b = orig;
        b[4] = 99;
        let n = b.len();
        let sum = blake3::hash(&b[..n - 32]);
        b[n - 32..].copy_from_slice(sum.as_bytes());
        std::fs::write(&p, &b).unwrap();
        assert!(Index::open(&s).unwrap().rebuilt());
    }

    #[test]
    fn index_apres_compactation_du_store() {
        let t = Tmp::new();
        let mut s = store(&t);
        for i in 0..80 {
            put(&mut s, "https://x/chaude", vec![sec("https://x/chaude", "D", &["D", "Chaude"], &format!("contenu numéro {i} sur kafka"))]);
        }
        assert!(s.stats().compactions >= 1);
        let ix = Index::open(&s).unwrap();
        let h = ix.query("kafka", &Filter::default(), 5);
        assert_eq!(h.len(), 1);
        assert!(h[0].snippet.contains("79"));
    }

    #[test]
    fn get_section_par_identifiant() {
        let t = Tmp::new();
        let mut s = store(&t);
        put(
            &mut s,
            "https://x/a",
            vec![sec("https://x/a", "D", &["D", "Un"], "premier"), sec("https://x/a", "D", &["D", "Deux"], "second")],
        );
        let ix = Index::build(&s).unwrap();
        let x = ix.get_section("W:t/a#deux").unwrap();
        assert_eq!(x.text, "second");
        assert!(ix.get_section("W:t/a#absent").is_none());
    }

    #[test]
    fn extrait_ne_coupe_pas_les_mots_et_contient_le_terme() {
        let mut texte = String::new();
        for i in 0..40 {
            texte.push_str(&format!("Phrase numéro {i} avec des mots ordinaires et anticonstitutionnellement longs. "));
        }
        texte.push_str("Ici apparaît le mot rarissime zébulon au milieu des phrases. ");
        for i in 0..40 {
            texte.push_str(&format!("Suite {i} de mots banals sans importance. "));
        }
        let qs = qstems(&["zebulon"]);
        let s = make_snippet(&texte, &qs, 200);
        assert!(s.contains("zébulon"), "{s}");
        assert!(s.chars().count() < 300, "{}", s.chars().count());
        let inner = s.trim_matches('…');
        // Chaque mot de l'extrait existe tel quel dans le texte : aucun mot coupé.
        for w in inner.split_whitespace() {
            assert!(texte.split_whitespace().any(|x| x == w), "mot coupé : {w:?}");
        }
        assert!(s.starts_with('…') && s.ends_with('…'));
    }

    #[test]
    fn extrait_court_renvoye_en_entier() {
        let s = make_snippet("Un petit texte.", &qstems(&["petit"]), 200);
        assert_eq!(s, "Un petit texte.");
    }

    #[test]
    fn extrait_garde_les_blocs_de_code_equilibres() {
        let mut texte = String::from("Introduction assez longue pour décaler la fenêtre. ");
        texte.push_str(&"Du blabla sans intérêt pour la recherche en cours. ".repeat(8));
        texte.push_str("\n\n```rust\n");
        for i in 0..30 {
            texte.push_str(&format!("let valeur_{i} = compute_{i}(argument);\n"));
        }
        texte.push_str("let cible = rarissime_fonction(argument);\n");
        for i in 0..30 {
            texte.push_str(&format!("let suite_{i} = encore_{i}();\n"));
        }
        texte.push_str("```\n\nFin du texte avec encore quelques mots.");
        let qs = qstems(&["rarissime", "fonction"]);
        let s = make_snippet(&texte, &qs, 200);
        assert!(s.contains("rarissime"), "{s}");
        assert_eq!(count_fences(&s) % 2, 0, "clôtures déséquilibrées :\n{s}");
        for l in s.lines() {
            if !l.starts_with('…') && !l.ends_with('…') && !l.starts_with("```") {
                assert!(texte.contains(l.trim_matches('…')), "ligne coupée : {l:?}");
            }
        }
    }

    #[test]
    fn extrait_prefere_la_fenetre_avec_le_plus_de_termes() {
        let mut texte = "alpha ".repeat(10);
        texte.push_str(&"bourrage de texte neutre ".repeat(30));
        texte.push_str("alpha et bravo ensemble ici ");
        texte.push_str(&"bourrage de texte neutre ".repeat(30));
        let s = make_snippet(&texte, &qstems(&["alpha", "bravo"]), 200);
        assert!(s.contains("bravo"), "{s}");
    }

    #[test]
    fn dates_iso() {
        assert_eq!(date_iso(0), "date inconnue");
        assert_eq!(date_iso(1_700_000_000), "2023-11-14");
        assert_eq!(date_iso(951_782_400), "2000-02-29");
        assert_eq!(date_iso(1_798_675_200), "2026-12-31");
    }

    fn hit(i: usize, snippet: &str) -> Hit {
        Hit {
            id: format!("W:demo/page{i}#section-{i}"),
            score: 1.0,
            page_url: format!("https://x/page{i}"),
            title: "Guide du routeur".into(),
            heading_path: vec!["Guide du routeur".into(), "Paramètres".into()],
            snippet: snippet.into(),
            version: Some("1.2.3".into()),
            fetched_at: 1_700_000_000,
            completeness: Completeness::Complete,
            quality: 0.9,
            query_terms: 2,
            matched_terms: 2,
            corpus_terms: 2,
            missing_names: 0,
            coverage: 1.0,
        }
    }

    #[test]
    fn carte_contient_les_preuves() {
        let c = render_cards(&[hit(1, "Le routeur associe une URL à un gestionnaire.")], 1000);
        assert!(c.contains("W:demo/page1#section-1"), "{c}");
        assert!(c.contains("Guide du routeur › Paramètres"));
        assert!(c.contains("v1.2.3"));
        assert!(c.contains("2023-11-14"));
        assert!(c.contains("confiance haute"));
        assert!(c.contains("next: cortex web read W:demo/page1#section-1"));
        assert!(c.contains("Le routeur associe"));
        assert!(est_tokens(&c) <= 200);
    }

    #[test]
    fn carte_confiance_selon_la_completude() {
        let mut h = hit(1, "x");
        h.completeness = Completeness::Partial;
        assert!(render_cards(&[h.clone()], 500).contains("faible"));
        h.completeness = Completeness::NeedsJs;
        assert!(render_cards(&[h.clone()], 500).contains("JS"));
        h.completeness = Completeness::Complete;
        h.quality = 0.3;
        assert!(render_cards(&[h.clone()], 500).contains("moyenne"));
        h.version = None;
        assert!(render_cards(&[h], 500).contains("version inconnue"));
    }

    #[test]
    fn cartes_limitees_par_le_budget() {
        let long = "mot ".repeat(400);
        let hits: Vec<Hit> = (0..10).map(|i| hit(i, &long)).collect();
        for budget in [60usize, 100, 300, 800, 2000] {
            let out = render_cards(&hits, budget);
            assert!(est_tokens(&out) <= budget, "budget {budget} dépassé : {}", est_tokens(&out));
        }
        let out = render_cards(&hits, 800);
        let n = out.matches("next: cortex web read").count();
        assert!((3..10).contains(&n), "{n} cartes");
        assert!(out.contains("autre(s) résultat(s)"));
        // Chaque carte pèse entre 70 et 200 jetons quand l'extrait est long.
        for c in out.split("\n\n").filter(|c| c.starts_with("W:")) {
            let t = est_tokens(c);
            assert!((70..=200).contains(&t), "carte de {t} jetons");
        }
    }

    #[test]
    fn budget_minuscule_ou_nul_et_liste_vide() {
        assert_eq!(render_cards(&[], 1000), "");
        assert_eq!(render_cards(&[hit(1, "x")], 0), "");
        assert_eq!(render_cards(&[hit(1, "x")], 5), "");
        // Un budget serré réduit l'extrait de la première carte plutôt que de ne rien rendre.
        let out = render_cards(&[hit(1, &"mot ".repeat(300))], 70);
        assert!(!out.is_empty());
        assert!(est_tokens(&out) <= 70);
    }

    #[test]
    fn carte_ferme_un_bloc_de_code_coupe() {
        let snip = format!("```rust\n{}", "let a = 1;\n".repeat(200));
        let out = render_cards(&[hit(1, &snip)], 300);
        assert_eq!(count_fences(&out) % 2, 0, "{out}");
    }

    #[test]
    fn de_bout_en_bout_requete_puis_cartes() {
        let t = Tmp::new();
        let mut s = store(&t);
        put(
            &mut s,
            "https://x/guide",
            vec![sec(
                "https://x/guide",
                "Guide",
                &["Guide", "Authentification"],
                "Pour authentifier une requête, ajoutez l'en-tête `Authorization: Bearer <jeton>`. Le jeton expire après une heure.",
            )],
        );
        let ix = Index::open(&s).unwrap();
        let h = ix.query("comment authentifier une requête avec un jeton", &Filter::default(), 3);
        assert_eq!(h.len(), 1);
        let cartes = render_cards(&h, 400);
        assert!(cartes.contains("W:t/guide#authentification"));
        assert!(cartes.contains("jeton"));
    }

    // ───── performance (ignoré par défaut : cargo test --release --features web web::index -- --ignored --nocapture) ─────

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    fn mot(i: usize) -> String {
        const SYL: [&str; 24] = [
            "ba", "ke", "ri", "so", "tu", "la", "me", "ni", "po", "ru", "da", "fe", "gi", "ho", "ju", "ka", "lo", "mu", "ne", "pa", "qui",
            "ra", "se", "to",
        ];
        let mut n = i + 1;
        let mut s = String::new();
        while n > 0 {
            s.push_str(SYL[n % 24]);
            n /= 24;
        }
        s
    }

    #[test]
    #[ignore]
    fn perf_20000_sections() {
        let t = Tmp::new();
        let mut s = store(&t);
        let mut rng = Rng(0x9E3779B97F4A7C15);
        let vocab = 30_000usize;
        // Zipf approché : un indice tiré au carré favorise les petits rangs.
        let tire = |rng: &mut Rng| -> String {
            let u = rng.below(1_000_000) as f64 / 1_000_000.0;
            mot(((u * u * u) * vocab as f64) as usize)
        };
        let t0 = std::time::Instant::now();
        let pages = 2000usize;
        for p in 0..pages {
            let url = format!("https://x/p{p}");
            let mut secs = Vec::new();
            for k in 0..10 {
                let mut text = String::new();
                while text.len() < 1300 {
                    text.push_str(&tire(&mut rng));
                    text.push(' ');
                    if rng.below(12) == 0 {
                        text.push_str(". ");
                    }
                }
                text.push_str(&format!("\n\n```rust\nlet {} = {}::{}();\n```\n", tire(&mut rng), tire(&mut rng), tire(&mut rng)));
                let heading = format!("{} {}", tire(&mut rng), tire(&mut rng));
                let mut x = sec(&url, &format!("Page {p}"), &["Guide", &heading], &text);
                x.id = format!("W:perf/p{p}#s{k}");
                secs.push(x);
            }
            s.put_page(rec(&url, 0.9, Completeness::Complete), secs).unwrap();
        }
        let ecriture = t0.elapsed();
        let st = s.stats();
        let taille = st.sections_bytes;

        let t1 = std::time::Instant::now();
        let ix = Index::build(&s).unwrap();
        let build = t1.elapsed();
        let ix_bytes = std::fs::metadata(s.index_path()).unwrap().len();

        let t2 = std::time::Instant::now();
        let ix2 = Index::open(&s).unwrap();
        let ouverture = t2.elapsed();
        assert!(!ix2.rebuilt());

        let mut lat: Vec<f64> = Vec::new();
        let mut trouves = 0usize;
        for _ in 0..100 {
            let q = format!("{} {} {}", tire(&mut rng), tire(&mut rng), tire(&mut rng));
            let t = std::time::Instant::now();
            let h = ix2.query(&q, &Filter::default(), 8);
            lat.push(t.elapsed().as_secs_f64() * 1000.0);
            if !h.is_empty() {
                trouves += 1;
            }
        }
        lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let med = lat[lat.len() / 2];
        let p95 = lat[lat.len() * 95 / 100];
        let ratio = ix_bytes as f64 / taille as f64;
        let rapport = format!(
            "sections={} sections.jsonl={} Mo écriture={:.2}s | build={:.2}s ouverture={:.1}ms | index.bin={:.1} Mo ({:.1} % de sections.jsonl) termes={} postings={} | requêtes: médiane={:.2}ms p95={:.2}ms max={:.2}ms ({} avec résultat) | mode={}",
            ix.stats().docs,
            taille / 1_000_000,
            ecriture.as_secs_f64(),
            build.as_secs_f64(),
            ouverture.as_secs_f64() * 1000.0,
            ix_bytes as f64 / 1e6,
            ratio * 100.0,
            ix.stats().terms,
            ix.stats().postings,
            med,
            p95,
            lat[lat.len() - 1],
            trouves,
            if cfg!(debug_assertions) { "debug" } else { "release" },
        );
        println!("PERF {rapport}");
        if let Ok(dir) = std::env::var("CORTEX_WEB_PERF_OUT") {
            let _ = std::fs::write(dir, &rapport);
        }
        assert_eq!(ix.stats().docs, 20_000);
        if !cfg!(debug_assertions) {
            assert!(build.as_secs_f64() < 5.0, "construction trop lente : {rapport}");
            assert!(med < 30.0, "requête médiane trop lente : {rapport}");
        }
        assert!(ratio < 0.5, "index trop gros : {rapport}");
    }
    // ───── confiance, version, pages jumelles ─────

    #[test]
    fn hit_porte_les_comptes_de_termes_et_la_confiance() {
        let t = Tmp::new();
        let mut s = store(&t);
        put(
            &mut s,
            "https://x/a",
            vec![
                sec("https://x/a", "Guide", &["Guide", "Deploy"], "Deploy the site to GitHub Pages with the build command."),
                sec("https://x/a", "Guide", &["Guide", "Other"], "Autre chose sans rapport avec le sujet principal du guide."),
            ],
        );
        let ix = Index::build(&s).unwrap();
        let h = ix.query("deploy the site to GitHub Pages", &Filter::default(), 3);
        assert!(h[0].query_terms >= 4 && h[0].matched_terms == h[0].query_terms, "{:?}", h[0]);
        assert_eq!(h[0].missing_names, 0);
        assert!(h[0].confidence() >= MIN_CONFIDENCE);
        // un nom propre que la source ne contient nulle part : « rien de fiable »
        let h = ix.query("deploy the site to Heroku", &Filter::default(), 3);
        assert_eq!(h[0].missing_names, 1, "{:?}", h[0]);
        assert_eq!(h[0].confidence(), 0.0);
        // pluriel d'un nom présent au singulier : pas absent
        put(&mut s, "https://x/b", vec![sec("https://x/b", "Net", &["Net", "Url"], "The URL of the request is set with the builder.")]);
        let ix = Index::build(&s).unwrap();
        let h = ix.query("Which URLs does the request use", &Filter::default(), 3);
        assert_eq!(h[0].missing_names, 0, "{:?}", h[0]);
    }

    #[test]
    fn version_choisie_par_la_question_sinon_la_principale() {
        let known = vec!["2.10.1".to_string(), "3.5.2".to_string()];
        assert_eq!(pick_version("ureq 3 read timeout", &known, Some("2.10.1")).as_deref(), Some("3.5.2"));
        assert_eq!(pick_version("avec ureq 2.10", &known, Some("3.5.2")).as_deref(), Some("2.10.1"));
        assert_eq!(pick_version("read timeout", &known, Some("2.10.1")).as_deref(), Some("2.10.1"));
        assert_eq!(pick_version("returns 404", &known, None), None);
        let py = vec!["3.13".to_string(), "3.11".to_string()];
        assert_eq!(pick_version("python 3 json", &py, Some("3.13")).as_deref(), Some("3.13"), "3 désigne deux versions : pas de choix");
        assert_eq!(pick_version("Python 3.11 json", &py, Some("3.13")).as_deref(), Some("3.11"));
        let pg = vec!["16".to_string(), "17".to_string()];
        assert_eq!(pick_version("PostgreSQL 17 : COPY", &pg, Some("16")).as_deref(), Some("17"));
    }

    #[test]
    fn page_jumelle_markdown_ou_index_html_n_est_indexee_qu_une_fois() {
        let t = Tmp::new();
        let mut s = store(&t);
        let body = "Configure the base path of the site with the base option.";
        put(&mut s, "https://x/guide/deploy", vec![sec("https://x/guide/deploy", "Deploy", &["Deploy", "Base"], body)]);
        put(&mut s, "https://x/guide/deploy.md", vec![sec("https://x/guide/deploy.md", "Deploy", &["Deploy", "Base"], body)]);
        put(&mut s, "https://x/ref/", vec![sec("https://x/ref/", "Ref", &["Ref", "Base"], body)]);
        put(&mut s, "https://x/ref/index.html", vec![sec("https://x/ref/index.html", "Ref", &["Ref", "Base"], body)]);
        let ix = Index::build(&s).unwrap();
        let urls: Vec<String> = ix.query("base path option", &Filter::default(), 10).into_iter().map(|h| h.page_url).collect();
        assert_eq!(urls.len(), 2, "{urls:?}");
        assert!(urls.contains(&"https://x/guide/deploy".to_string()) && urls.contains(&"https://x/ref/".to_string()), "{urls:?}");
    }

    #[test]
    fn a_page_egale_le_titre_propre_de_la_section_passe_devant() {
        let t = Tmp::new();
        let mut s = store(&t);
        put(
            &mut s,
            "https://x/a",
            vec![
                sec(
                    "https://x/a",
                    "Guide",
                    &["Guide", "Intro"],
                    "Cette section parle du délai de lecture et du délai global des requêtes réseau.",
                ),
                sec(
                    "https://x/a",
                    "Guide",
                    &["Guide", "Délai de lecture"],
                    "Cette section parle du délai de lecture et du délai global des requêtes réseau.",
                ),
            ],
        );
        let ix = Index::build(&s).unwrap();
        let h = ix.query("délai de lecture", &Filter::default(), 2);
        assert_eq!(h[0].heading_path.last().unwrap(), "Délai de lecture");
    }

    #[test]
    fn autre_version_signalee_si_elle_repond_mieux() {
        let a = Hit { score: 5.0, ..hit(1, "x") };
        let b = Hit { score: 7.0, ..hit(2, "y") };
        assert!(also_in(&[a.clone()], &[b.clone()]).is_some());
        assert!(also_in(&[b], &[a.clone()]).is_none());
        assert!(also_in(&[], &[a]).is_some());
    }
}
