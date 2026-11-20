//! Cortex Web : lien entre la documentation d'une bibliothèque et les SYMBOLES DU CODE du
//! projet (prototype, feature `web`). Voir docs/WEB.md §3 D7 et §4.3.
//!
//! L'avantage décisif de Cortex Web : une section de doc n'est pas un texte isolé, elle est
//! reliée au code du projet qui utilise la bibliothèque, avec un NIVEAU DE PREUVE explicite
//! (`Proof`). Règle d'or : on ne rend JAMAIS un lien non prouvé. Un mot courant, un nom de
//! une ou deux lettres, un mot vide, un nom qui ne se distingue pas ne produit aucun lien.
//!
//! Trois entrées, toutes lisant l'atlas de code existant (`atlas::Handle`, mmap, sans réseau) :
//! - [`link_section`] : mentions de code d'une section (texte entre accents graves, signatures
//!   et appels des blocs de code, chemins `a::b::c`) résolues contre les symboles du PROJET ;
//! - [`lib_usage`] : fichiers et symboles du projet qui IMPORTENT la bibliothèque (preuve
//!   `ImportEvidence`), à partir des références d'import de l'atlas (Rust, JS/TS, Python) et
//!   d'une recherche de texte exacte bornée au projet (Go, `require(...)`, `crate::` Rust) ;
//! - [`usage_of_item`] : les appelants d'un item de la doc (ex. `Agent::get`) dans le projet ;
//! - [`link_section_dep`] : le tout-en-un d'une section de doc d'une dépendance : chaque item
//!   cité est relié aux appelants du projet (preuve par import de la dépendance).
//!
//! Le texte rendu par [`render_links`] vient de NOTRE code (identifiants de l'atlas du
//! projet) : il n'est pas du texte web non fiable et ne passe donc pas par `untrusted::wrap`.

use crate::atlas::view::NodeRef;
use crate::atlas::Handle;
use crate::symbol::SymbolKind;
use crate::textsearch::{self, FileSet};
use crate::web::resolve::{Dep, Ecosystem};
use std::collections::{HashMap, HashSet};

/// Plafond de liens rendus par `lib_usage` / `usage_of_item`.
pub const MAX_USAGE: usize = 200;
/// Plafond de liens rendus par `link_section`.
const MAX_LINKS: usize = 100;
/// Plafond de liens par fichier (le reste est de la redite).
const MAX_PER_FILE: usize = 10;
/// Nombre maximal d'items différents interrogés par `link_section_dep`.
const MAX_ITEMS: usize = 30;

// ───────────────────────── Types publics ─────────────────────────

/// Niveau de preuve d'un lien, du plus fort au plus faible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Proof {
    /// Le chemin cité (`module::fonction`, `Type.methode`) désigne UN symbole : le qualificatif
    /// correspond au fichier, au dossier ou au type englobant du symbole.
    ExactQualified,
    /// Le nom, distinctif, n'existe qu'une fois dans le projet.
    ExactUnique,
    /// Le nom existe `n` fois (ou plusieurs symboles répondent au chemin) : à départager.
    Ambiguous(u32),
    /// Le fichier importe la bibliothèque (ou l'appelle) : preuve d'usage, pas d'identité.
    ImportEvidence,
}

impl Proof {
    fn rank(self) -> u32 {
        match self {
            Proof::ExactQualified => 0,
            Proof::ExactUnique => 1,
            Proof::ImportEvidence => 2,
            Proof::Ambiguous(n) => 3 + n.min(1000),
        }
    }

    /// Libellé court, tel qu'affiché par `render_links`.
    pub fn label(self) -> String {
        match self {
            Proof::ExactQualified => "exact qualifié".to_string(),
            Proof::ExactUnique => "exact unique".to_string(),
            Proof::Ambiguous(n) => format!("ambigu ×{}", n),
            Proof::ImportEvidence => "import".to_string(),
        }
    }
}

/// Un lien vers le code du projet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolRef {
    /// Identifiant stable de l'atlas : `S:chemin#symbole`, ou `F:chemin` pour un fichier
    /// qui importe la bibliothèque sans symbole identifiable.
    pub id: String,
    pub proof: Proof,
    pub file: String,
    pub line: u32,
}

/// Une ligne compacte « code lié : S:… (preuve) ; … » (au plus `max` liens, `+N` pour le reste).
/// Vide si rien n'est relié. Ce texte vient de NOTRE code, pas du web.
pub fn render_links(links: &[SymbolRef], max: usize) -> String {
    if links.is_empty() || max == 0 {
        return String::new();
    }
    let shown: Vec<String> = links.iter().take(max).map(|l| format!("{} ({})", clean(&l.id), l.proof.label())).collect();
    let mut s = format!("code lié : {}", shown.join(" ; "));
    if links.len() > max {
        s.push_str(&format!(" ; … +{}", links.len() - max));
    }
    s
}

/// Retire tout caractère de contrôle (un nom de fichier du projet ne doit pas casser la ligne).
fn clean(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

// ───────────────────────── Extraction des mentions ─────────────────────────

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Mention {
    /// Qualificatifs, du plus lointain au plus proche (`a::b::c` : [a, b]).
    quals: Vec<String>,
    name: String,
    /// Suivi d'une parenthèse ouvrante.
    call: bool,
    /// Nom introduit par un mot-clé de déclaration (`fn x`, `class X`).
    sig: bool,
}

/// Contexte d'une mention : détermine ce qui suffit pour la retenir.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ctx {
    /// Texte entre accents graves.
    Inline,
    /// Bloc de code clôturé.
    Block,
    /// Prose : seuls les chemins `a::b` comptent.
    Prose,
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '$'
}
fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

const QUAL_IGNORED: &[&str] = &["crate", "self", "super", "this", "Self"];

/// Parcourt `s` et rend chaque chemin `a::b::c` (et `a.b.c` si `allow_dot`).
fn scan_paths(s: &str, allow_dot: bool) -> Vec<Mention> {
    let ch: Vec<char> = s.chars().collect();
    let n = ch.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        let c = ch[i];
        if !is_ident_start(c) {
            if is_ident(c) {
                // Nombre ou mot commençant par un chiffre : on le saute en entier.
                while i < n && is_ident(ch[i]) {
                    i += 1;
                }
            } else {
                i += 1;
            }
            continue;
        }
        let mut segs: Vec<String> = Vec::new();
        loop {
            let st = i;
            while i < n && is_ident(ch[i]) {
                i += 1;
            }
            segs.push(ch[st..i].iter().collect());
            if i + 2 < n && ch[i] == ':' && ch[i + 1] == ':' && is_ident_start(ch[i + 2]) {
                i += 2;
                continue;
            }
            if allow_dot && i + 1 < n && ch[i] == '.' && is_ident_start(ch[i + 1]) {
                i += 1;
                continue;
            }
            break;
        }
        let call = i < n && ch[i] == '(';
        let name = segs.pop().unwrap_or_default();
        let quals: Vec<String> = segs.into_iter().filter(|q| !QUAL_IGNORED.contains(&q.as_str())).collect();
        out.push(Mention { quals, name, call, sig: false });
    }
    out
}

/// Mots introduisant une déclaration : le mot suivant est le nom déclaré.
const SIG_KEYWORDS: &[&str] =
    &["fn", "def", "function", "func", "class", "struct", "enum", "trait", "interface", "type", "impl", "const", "static", "mod"];

/// Dans une ligne de bloc de code : noms déclarés (`fn fetch_page(...)`, `class Agent`).
fn sig_names(line: &str) -> Vec<String> {
    let words: Vec<&str> = line.split(|c: char| !is_ident(c)).filter(|w| !w.is_empty()).collect();
    let mut out = Vec::new();
    for (i, w) in words.iter().enumerate() {
        if SIG_KEYWORDS.contains(w) {
            if let Some(next) = words.get(i + 1) {
                if next.chars().next().is_some_and(is_ident_start) {
                    out.push((*next).to_string());
                }
            }
        }
    }
    out
}

/// Mentions d'une section : (mention, contexte). Les blocs `~~~`/```` ``` ```` sont reconnus
/// ligne par ligne ; hors bloc, le texte entre accents graves est un « span ».
fn extract_mentions(text: &str) -> Vec<(Mention, Ctx)> {
    let mut out: Vec<(Mention, Ctx)> = Vec::new();
    let mut fence: Option<String> = None;
    for line in text.lines() {
        let t = line.trim_start();
        let marker = if t.starts_with("```") {
            Some("```")
        } else if t.starts_with("~~~") {
            Some("~~~")
        } else {
            None
        };
        if let Some(m) = marker {
            match &fence {
                None => fence = Some(m.to_string()),
                Some(open) if open == m => fence = None,
                _ => {}
            }
            continue;
        }
        if fence.is_some() {
            for name in sig_names(line) {
                out.push((Mention { quals: Vec::new(), name, call: false, sig: true }, Ctx::Block));
            }
            for m in scan_paths(line, true) {
                out.push((m, Ctx::Block));
            }
            continue;
        }
        // Hors bloc : spans entre accents graves (le contenu), puis chemins `::` de la prose.
        let chars: Vec<char> = line.chars().collect();
        let mut prose = String::new();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] != '`' {
                prose.push(chars[i]);
                i += 1;
                continue;
            }
            let mut run = 0;
            while i + run < chars.len() && chars[i + run] == '`' {
                run += 1;
            }
            // Fermeture : la prochaine suite de `run` accents graves exactement.
            let mut j = i + run;
            let mut close = None;
            while j < chars.len() {
                if chars[j] == '`' {
                    let mut r2 = 0;
                    while j + r2 < chars.len() && chars[j + r2] == '`' {
                        r2 += 1;
                    }
                    if r2 == run {
                        close = Some(j);
                        break;
                    }
                    j += r2;
                } else {
                    j += 1;
                }
            }
            match close {
                Some(c) => {
                    let inner: String = chars[i + run..c].iter().collect();
                    for m in scan_paths(&inner, true) {
                        out.push((m, Ctx::Inline));
                    }
                    prose.push(' ');
                    i = c + run;
                }
                None => {
                    i += run;
                }
            }
        }
        for m in scan_paths(&prose, false) {
            if !m.quals.is_empty() {
                out.push((m, Ctx::Prose));
            }
        }
    }
    out
}

// ───────────────────────── Filtres : ce qui n'est jamais un lien ─────────────────────────

/// Mots-clés des langages usuels : jamais un nom de symbole utile.
const KEYWORDS: &[&str] = &[
    "fn",
    "let",
    "mut",
    "pub",
    "use",
    "mod",
    "impl",
    "trait",
    "struct",
    "enum",
    "type",
    "const",
    "static",
    "async",
    "await",
    "move",
    "match",
    "loop",
    "while",
    "for",
    "if",
    "else",
    "return",
    "break",
    "continue",
    "where",
    "dyn",
    "ref",
    "crate",
    "self",
    "super",
    "true",
    "false",
    "none",
    "some",
    "ok",
    "err",
    "null",
    "undefined",
    "nil",
    "var",
    "val",
    "def",
    "class",
    "import",
    "from",
    "export",
    "default",
    "function",
    "func",
    "package",
    "interface",
    "extends",
    "implements",
    "new",
    "this",
    "yield",
    "lambda",
    "pass",
    "with",
    "elif",
    "try",
    "catch",
    "finally",
    "throw",
    "throws",
    "raise",
    "except",
    "and",
    "not",
    "the",
    "int",
    "str",
    "bool",
    "char",
    "float",
    "void",
    "string",
    "number",
    "boolean",
    "object",
    "any",
    "unit",
    "usize",
    "isize",
    "u8",
    "u16",
    "u32",
    "u64",
    "i8",
    "i16",
    "i32",
    "i64",
    "f32",
    "f64",
    "u128",
    "i128",
];

/// Noms trop génériques pour prouver quoi que ce soit sans qualificatif : types de la
/// bibliothèque standard, méthodes standard, noms de types passe-partout.
const COMMON: &[&str] = &[
    "string",
    "vec",
    "option",
    "result",
    "error",
    "box",
    "arc",
    "rc",
    "hashmap",
    "hashset",
    "btreemap",
    "btreeset",
    "cow",
    "path",
    "pathbuf",
    "duration",
    "instant",
    "mutex",
    "rwlock",
    "cell",
    "refcell",
    "default",
    "display",
    "debug",
    "clone",
    "send",
    "sync",
    "sized",
    "iterator",
    "intoiterator",
    "from",
    "into",
    "asref",
    "tryfrom",
    "tryinto",
    "fromstr",
    "serialize",
    "deserialize",
    "promise",
    "array",
    "map",
    "set",
    "date",
    "json",
    "regexp",
    "buffer",
    "config",
    "client",
    "server",
    "request",
    "response",
    "builder",
    "options",
    "context",
    "state",
    "handler",
    "service",
    "value",
    "data",
    "item",
    "event",
    "message",
    "node",
    "tree",
    "cache",
    "store",
    "reader",
    "writer",
    "parser",
    "manager",
    "controller",
    "provider",
    "factory",
    "helper",
    "utils",
    "util",
    "test",
    "tests",
    "main",
    "index",
    "init",
    "setup",
    "args",
    "kwargs",
    "props",
    "children",
    "params",
    "query",
    "body",
    "header",
    "headers",
    "name",
    "list",
    "dict",
    "tuple",
    "object",
    "class",
    "module",
    "package",
    "file",
    "files",
    "text",
    "line",
    "lines",
    "page",
    "pages",
    "type_id",
    "to_string",
    "to_owned",
    "to_vec",
    "to_lowercase",
    "to_uppercase",
    "as_str",
    "as_ref",
    "as_mut",
    "as_bytes",
    "unwrap_or",
    "unwrap_or_else",
    "unwrap_or_default",
    "ok_or",
    "ok_or_else",
    "map_err",
    "and_then",
    "or_else",
    "into_iter",
    "iter_mut",
    "is_empty",
    "is_some",
    "is_none",
    "is_ok",
    "is_err",
    "get_or_insert",
    "get_mut",
    "push_str",
    "to_json",
    "from_str",
    "from_utf8",
    "console_log",
    "set_timeout",
    "set_interval",
    "__init__",
    "__main__",
    "__name__",
];

/// Nom utilisable au minimum : 3 caractères ou plus, pas un mot-clé, pas un nombre.
fn name_ok(name: &str) -> bool {
    if name.chars().count() < 3 {
        return false;
    }
    if !name.chars().any(|c| c.is_alphabetic()) {
        return false;
    }
    let l = name.to_lowercase();
    !KEYWORDS.contains(&l.as_str())
}

/// Un nom seul (sans qualificatif) se distingue-t-il assez pour être cherché dans le projet ?
/// Oui : `snake_case` avec tiret bas, `camelCase`/`PascalCase` à plusieurs bosses, `PascalCase`
/// d'au moins 4 lettres, CONSTANTE d'au moins 5 caractères. Non : un mot en minuscules
/// (`get`, `search`, `render`) ; il faut alors un qualificatif.
fn distinctive(name: &str) -> bool {
    let core = name.trim_matches('_');
    if core.chars().count() < 3 {
        return false;
    }
    let has_lower = core.chars().any(|c| c.is_lowercase());
    let has_upper = core.chars().any(|c| c.is_uppercase());
    if core.contains('_') && core.split('_').filter(|p| !p.is_empty()).count() >= 2 {
        return true;
    }
    if has_lower && core.chars().skip(1).any(|c| c.is_uppercase()) {
        return true; // camelCase, PascalCase à bosses multiples
    }
    if has_lower && core.chars().next().is_some_and(|c| c.is_uppercase()) {
        return core.chars().count() >= 4; // PascalCase simple : Agent, Scraper
    }
    if has_upper && !has_lower {
        return core.chars().count() >= 5; // CONSTANTE
    }
    false
}

/// Une mention est-elle assez forte pour être cherchée dans le projet ?
fn accept(m: &Mention, ctx: Ctx) -> bool {
    if !name_ok(&m.name) {
        return false;
    }
    if !m.quals.is_empty() {
        return true; // le qualificatif devra correspondre : c'est lui qui prouve
    }
    if COMMON.contains(&m.name.to_lowercase().as_str()) || !distinctive(&m.name) {
        return false;
    }
    match ctx {
        Ctx::Prose => false,
        Ctx::Inline => true,
        // Dans un bloc de code : un identifiant seul ne compte que s'il est déclaré, appelé,
        // ou s'il a l'allure d'un type (les variables locales ne prouvent rien).
        Ctx::Block => m.sig || m.call || m.name.chars().next().is_some_and(|c| c.is_uppercase()),
    }
}

// ───────────────────────── Résolution contre l'atlas du projet ─────────────────────────

fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

fn indexable(r: &NodeRef) -> bool {
    r.n.kind == 1 && !matches!(r.kind(), SymbolKind::Import | SymbolKind::Export | SymbolKind::Heading)
}

/// Symboles vivants du projet (tous les atlas ouverts) nommés exactement `name`.
fn candidates<'a>(handles: &'a [Handle], name: &str) -> Vec<(&'a Handle, u32)> {
    let key = crate::graph::call_key(name);
    let mut out = Vec::new();
    for h in handles {
        for g in h.defs(&key) {
            if let Some(r) = h.node(g) {
                if indexable(&r) && r.name().rsplit("::").next() == Some(name) {
                    out.push((h, g));
                }
            }
        }
    }
    out
}

/// Types qui englobent le symbole dans son fichier (classe, `impl`, struct, interface…).
fn owner_names(h: &Handle, r: &NodeRef) -> Vec<String> {
    let end = r.n.end_line.max(r.n.line);
    h.symbols_of(r.n.owner_file)
        .into_iter()
        .filter(|x| {
            x.g != r.g
                && matches!(
                    x.kind(),
                    SymbolKind::Class
                        | SymbolKind::Interface
                        | SymbolKind::Struct
                        | SymbolKind::Enum
                        | SymbolKind::Type
                        | SymbolKind::Component
                )
                && x.n.end_line > 0
                && x.n.line <= r.n.line
                && x.n.end_line >= end
        })
        .map(|x| norm(x.name()))
        .collect()
}

/// Le qualificatif immédiat (`parent`, normalisé) désigne-t-il le type englobant (palier 0),
/// le fichier (palier 1) ou un dossier (palier 2) du symbole `g` ? Le palier le plus bas
/// départage : `Pool::reset` dans `pool.rs` vise le type `Pool`, pas tous les symboles du fichier.
fn parent_tier(h: &Handle, g: u32, parent: &str) -> Option<u8> {
    let r = h.node(g)?;
    if owner_names(h, &r).iter().any(|o| o == parent) {
        return Some(0);
    }
    let path = h.path_of(r.n.owner_file);
    let mut comps: Vec<&str> = path.split('/').collect();
    if let Some(last) = comps.pop() {
        let stem = last.rsplit_once('.').map(|(s, _)| s).unwrap_or(last);
        if norm(stem) == parent {
            return Some(1);
        }
    }
    comps.iter().any(|c| norm(c) == parent).then_some(2)
}

fn sym_ref(h: &Handle, g: u32, proof: Proof) -> Option<SymbolRef> {
    let r = h.node(g)?;
    Some(SymbolRef { id: clean(&h.node_id(g)), proof, file: h.path_of(r.n.owner_file).to_string(), line: r.n.line })
}

/// Garde, pour un même identifiant, la meilleure preuve.
fn merge(map: &mut HashMap<String, SymbolRef>, r: SymbolRef) {
    match map.get(&r.id) {
        Some(old) if (old.proof.rank(), old.line) <= (r.proof.rank(), r.line) => {}
        _ => {
            map.insert(r.id.clone(), r);
        }
    }
}

fn resolve(handles: &[Handle], m: &Mention) -> Vec<SymbolRef> {
    let cands = candidates(handles, &m.name);
    if cands.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    if m.quals.is_empty() {
        let n = cands.len() as u32;
        let proof = if n == 1 { Proof::ExactUnique } else { Proof::Ambiguous(n) };
        for (h, g) in cands {
            out.extend(sym_ref(h, g, proof));
        }
        return out;
    }
    let parent = norm(m.quals.last().map(String::as_str).unwrap_or(""));
    if parent.is_empty() {
        return out;
    }
    let tiers: Vec<(&Handle, u32, u8)> = cands.into_iter().filter_map(|(h, g)| parent_tier(h, g, &parent).map(|t| (h, g, t))).collect();
    let best = tiers.iter().map(|x| x.2).min();
    let kept: Vec<(&Handle, u32)> = tiers.into_iter().filter(|x| Some(x.2) == best).map(|x| (x.0, x.1)).collect();
    let proof = if kept.len() == 1 { Proof::ExactQualified } else { Proof::Ambiguous(kept.len() as u32) };
    for (h, g) in kept {
        out.extend(sym_ref(h, g, proof));
    }
    out
}

fn sorted(map: HashMap<String, SymbolRef>, cap: usize) -> Vec<SymbolRef> {
    let mut v: Vec<SymbolRef> = map.into_values().collect();
    v.sort_by(|a, b| {
        a.proof.rank().cmp(&b.proof.rank()).then_with(|| a.file.cmp(&b.file)).then(a.line.cmp(&b.line)).then_with(|| a.id.cmp(&b.id))
    });
    v.truncate(cap);
    v
}

/// (1) Relie les mentions de code d'une section de documentation aux symboles du projet.
///
/// Extrait : le texte entre accents graves, les identifiants déclarés ou appelés dans les blocs
/// de code, les chemins `a::b::c` de la prose et `a.b()`. Chaque mention est résolue contre les
/// symboles de l'atlas : nom distinctif unique → `ExactUnique` ; chemin dont le qualificatif
/// immédiat désigne le fichier, le dossier ou le type englobant → `ExactQualified` ; plusieurs
/// symboles → `Ambiguous(n)`. Un chemin dont le qualificatif ne correspond à rien du projet est
/// ignoré (c'est un item de la bibliothèque, pas du code du projet). Jamais de lien non prouvé :
/// mots courants, mots en minuscules sans qualificatif, noms de 1-2 lettres, mots vides.
pub fn link_section(section_text: &str, handles: &[Handle]) -> Vec<SymbolRef> {
    let mut seen: HashSet<Mention> = HashSet::new();
    let mut map: HashMap<String, SymbolRef> = HashMap::new();
    for (m, ctx) in extract_mentions(section_text) {
        if !accept(&m, ctx) || !seen.insert(m.clone()) {
            continue;
        }
        for r in resolve(handles, &m) {
            merge(&mut map, r);
        }
    }
    sorted(map, MAX_LINKS)
}

// ───────────────────────── Usage d'une bibliothèque ─────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fam {
    Rust,
    Js,
    Py,
    Go,
    Other,
}

fn fam_of_path(path: &str) -> Fam {
    let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "rs" => Fam::Rust,
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "mts" | "cts" => Fam::Js,
        "py" | "pyi" => Fam::Py,
        "go" => Fam::Go,
        _ => Fam::Other,
    }
}

fn fam_of_dep(dep: &Dep) -> Fam {
    match dep.ecosystem {
        Ecosystem::Cargo => Fam::Rust,
        Ecosystem::Npm => Fam::Js,
        Ecosystem::Pypi => Fam::Py,
        Ecosystem::Go => Fam::Go,
    }
}

/// Noms d'import Python connus quand ils diffèrent du nom du paquet.
fn pypi_import_names(name: &str) -> Vec<String> {
    let base = name.to_ascii_lowercase().replace('-', "_");
    let alias: &[&str] = match base.as_str() {
        "pyyaml" => &["yaml"],
        "pillow" => &["pil"],
        "scikit_learn" => &["sklearn"],
        "opencv_python" | "opencv_python_headless" => &["cv2"],
        "beautifulsoup4" => &["bs4"],
        "python_dateutil" => &["dateutil"],
        "attrs" => &["attr", "attrs"],
        "protobuf" => &["google"],
        "pyjwt" => &["jwt"],
        "msgpack_python" => &["msgpack"],
        "python_dotenv" => &["dotenv"],
        _ => &[],
    };
    let mut v: Vec<String> = alias.iter().map(|s| s.to_string()).collect();
    v.push(base);
    v
}

/// Un spécificateur d'import désigne-t-il cette dépendance ?
fn spec_matches(dep: &Dep, spec: &str) -> bool {
    let spec = spec.trim();
    match dep.ecosystem {
        Ecosystem::Cargo => {
            let n = dep.name.replace('-', "_");
            spec == n || spec.strip_prefix(n.as_str()).is_some_and(|r| r.starts_with("::"))
        }
        Ecosystem::Npm => spec == dep.name || spec.strip_prefix(dep.name.as_str()).is_some_and(|r| r.starts_with('/')),
        Ecosystem::Pypi => {
            let first = spec.split('.').next().unwrap_or("").to_ascii_lowercase().replace('-', "_");
            !first.is_empty() && pypi_import_names(&dep.name).contains(&first)
        }
        Ecosystem::Go => spec == dep.name || spec.strip_prefix(dep.name.as_str()).is_some_and(|r| r.starts_with('/')),
    }
}

/// Ce qu'un fichier importe de la dépendance.
struct DepImport {
    /// Noms importés nommément (Rust : dernier segment de chaque `use dep::…::Nom`).
    names: Vec<String>,
}

/// Le fichier (nœud de l'atlas) importe-t-il la dépendance ? (références d'import de l'atlas)
fn dep_import(dep: &Dep, r: &NodeRef) -> Option<DepImport> {
    if fam_of_path(r.name()) != fam_of_dep(dep) {
        return None;
    }
    let refs = r.refs()?;
    let mut names = Vec::new();
    let mut found = false;
    for &i in refs.imports.iter() {
        let spec = r.str(i);
        if !spec_matches(dep, spec) {
            continue;
        }
        found = true;
        if dep.ecosystem == Ecosystem::Cargo {
            if let Some((_, last)) = spec.rsplit_once("::") {
                if last != "*" && last != "self" && !names.iter().any(|n| n == last) {
                    names.push(last.to_string());
                }
            }
        }
    }
    found.then_some(DepImport { names })
}

/// Fichiers vivants de l'atlas qui importent la dépendance : (id global, chemin, imports).
fn files_importing(dep: &Dep, h: &Handle) -> Vec<(u32, String, DepImport)> {
    let mut out = Vec::new();
    for r in h.live_files() {
        if let Some(imp) = dep_import(dep, &r) {
            out.push((r.g, r.name().to_string(), imp));
        }
    }
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out
}

fn fileset<'a>(h: &'a Handle, fam: Fam) -> FileSet<'a> {
    FileSet { project: &h.project, root: h.root(), paths: h.file_paths().into_iter().filter(|p| fam_of_path(p) == fam).collect() }
}

/// Préfixe de ligne d'un commentaire (une mention en commentaire ne prouve pas un usage).
fn is_comment(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("//") || t.starts_with('#') || t.starts_with("/*") || t.starts_with('*')
}

/// `needle` apparaît-il dans `line` en tête de mot (caractère précédent ni identifiant ni `:`) ?
fn has_word_start(line: &str, needle: &str) -> bool {
    let mut from = 0;
    while let Some(p) = line[from..].find(needle) {
        let at = from + p;
        let prev = line[..at].chars().next_back();
        if !prev.is_some_and(|c| is_ident(c) || c == ':') {
            return true;
        }
        from = at + needle.len();
    }
    false
}

/// Ligne (1-based) de la première instruction d'import qui nomme la dépendance, sinon 1.
fn import_line(root: &str, path: &str, dep: &Dep) -> u32 {
    let Ok(text) = std::fs::read_to_string(std::path::Path::new(root).join(path)) else { return 1 };
    let needles: Vec<String> = match dep.ecosystem {
        Ecosystem::Cargo => vec![dep.name.replace('-', "_")],
        Ecosystem::Pypi => pypi_import_names(&dep.name),
        _ => vec![dep.name.clone()],
    };
    for (i, l) in text.lines().enumerate() {
        let t = l.trim_start();
        let is_import = t.starts_with("use ")
            || t.starts_with("pub use ")
            || t.starts_with("import ")
            || t.starts_with("from ")
            || t.starts_with("const ")
            || t.starts_with("var ")
            || t.starts_with("let ")
            || t.starts_with('"')
            || t.contains("require(");
        if is_import && needles.iter().any(|n| t.to_ascii_lowercase().contains(&n.to_ascii_lowercase())) {
            return i as u32 + 1;
        }
    }
    1
}

/// Un lien de preuve `ImportEvidence` vers le symbole englobant la ligne, sinon vers le fichier.
fn evidence_at(h: &Handle, path: &str, line: u32) -> SymbolRef {
    match h.enclosing_symbol(path, line) {
        Some(id) => SymbolRef { id: clean(&id), proof: Proof::ImportEvidence, file: path.to_string(), line },
        None => SymbolRef { id: clean(&crate::ids::file_id(path)), proof: Proof::ImportEvidence, file: path.to_string(), line },
    }
}

/// Recherche de texte exacte (bornée aux fichiers du projet de la bonne famille) pour les cas
/// où l'atlas n'a pas de référence d'import : Go, `require('pkg')`, chemin complet `crate::`.
fn text_usage(dep: &Dep, h: &Handle, out: &mut Vec<SymbolRef>) {
    let fam = fam_of_dep(dep);
    let set = [fileset(h, fam)];
    if set[0].paths.is_empty() {
        return;
    }
    match dep.ecosystem {
        Ecosystem::Go => {
            let q = format!("\"{}", dep.name);
            for hit in textsearch::grep(&set, &q, true, 4 * MAX_USAGE) {
                let t = hit.text.as_str();
                let ok = t.contains(&format!("\"{}\"", dep.name)) || t.contains(&format!("\"{}/", dep.name));
                if ok && !is_comment(t) {
                    out.push(evidence_at(h, &hit.file, hit.line));
                }
            }
        }
        Ecosystem::Cargo => {
            let n = dep.name.replace('-', "_");
            let needle = format!("{}::", n);
            for hit in textsearch::grep(&set, &needle, true, 8 * MAX_USAGE) {
                let t = hit.text.as_str();
                let tt = t.trim_start();
                // Les `use` sont déjà couverts par les références d'import de l'atlas.
                if is_comment(t) || tt.starts_with("use ") || tt.starts_with("pub use ") || !has_word_start(t, &needle) {
                    continue;
                }
                out.push(evidence_at(h, &hit.file, hit.line));
            }
        }
        Ecosystem::Npm => {
            for q in ["('", "(\""] {
                let needle = format!("{}{}", q, dep.name);
                for hit in textsearch::grep(&set, &needle, true, 4 * MAX_USAGE) {
                    let t = hit.text.as_str();
                    let quote = &q[1..];
                    let ok = t.contains(&format!("{}{}{}", q, dep.name, quote)) || t.contains(&format!("{}{}/", q, dep.name));
                    if ok && (t.contains("require(") || t.contains("import(")) && !is_comment(t) {
                        out.push(evidence_at(h, &hit.file, hit.line));
                    }
                }
            }
        }
        Ecosystem::Pypi => {}
    }
}

/// (2) Symboles et fichiers du projet qui importent la bibliothèque (preuve `ImportEvidence`).
///
/// Sources : les références d'import de l'atlas (Rust `use`, JS/TS `import … from`, Python
/// `import`/`from … import`), affinées au SYMBOLE englobant quand le fichier appelle un nom
/// importé de la dépendance (Rust) ; puis une recherche de texte exacte bornée aux fichiers du
/// projet (Go, `require()`, chemin complet `dep::…`). Sans symbole identifiable, le lien porte
/// l'identifiant de fichier `F:chemin`. Groupé par fichier, au plus 10 liens par fichier et
/// `MAX_USAGE` (200) au total.
pub fn lib_usage(dep: &Dep, handles: &[Handle]) -> Vec<SymbolRef> {
    let mut all: Vec<SymbolRef> = Vec::new();
    for h in handles {
        for (_, path, imp) in files_importing(dep, h) {
            let mut got: Vec<SymbolRef> = Vec::new();
            if !imp.names.is_empty() {
                if let Some(refs) = h.file_by_path(&path).and_then(|g| h.node(g)).and_then(|r| r.refs().map(|x| (r, x))) {
                    let (r, refs) = refs;
                    for c in refs.calls.iter() {
                        let n = r.str(c.name);
                        if imp.names.iter().any(|x| x == n) {
                            got.push(evidence_at(h, &path, c.line));
                        }
                    }
                }
            }
            if got.iter().all(|x| x.id.starts_with("F:")) {
                got.clear();
                let line = import_line(h.root(), &path, dep);
                got.push(SymbolRef { id: clean(&crate::ids::file_id(&path)), proof: Proof::ImportEvidence, file: path.clone(), line });
            }
            all.extend(got);
        }
        text_usage(dep, h, &mut all);
    }
    finish_usage(all)
}

/// Dédoublonne (meilleure ligne la plus basse), retire les `F:` d'un fichier qui a des `S:`,
/// groupe par fichier, plafonne par fichier puis au total.
fn finish_usage(all: Vec<SymbolRef>) -> Vec<SymbolRef> {
    let mut map: HashMap<String, SymbolRef> = HashMap::new();
    for r in all {
        merge(&mut map, r);
    }
    let with_symbol: HashSet<String> = map.values().filter(|r| r.id.starts_with("S:")).map(|r| r.file.clone()).collect();
    let mut v: Vec<SymbolRef> = map.into_values().filter(|r| !(r.id.starts_with("F:") && with_symbol.contains(&r.file))).collect();
    v.sort_by(|a, b| {
        a.proof.rank().cmp(&b.proof.rank()).then_with(|| a.file.cmp(&b.file)).then(a.line.cmp(&b.line)).then_with(|| a.id.cmp(&b.id))
    });
    let mut per_file: HashMap<String, usize> = HashMap::new();
    v.retain(|r| {
        let c = per_file.entry(r.file.clone()).or_insert(0);
        *c += 1;
        *c <= MAX_PER_FILE
    });
    v.truncate(MAX_USAGE);
    v
}

// ───────────────────────── Appelants d'un item de la doc ─────────────────────────

/// Ce qu'on sait d'une dépendance dans UN atlas, calculé une fois pour plusieurs items.
struct HandleCtx {
    /// Fichiers qui importent la dépendance.
    files: Vec<(u32, String, DepImport)>,
    /// Rust : chemins complets `dep::…` écrits dans le code (hors `use`, commentaires) :
    /// (fichier, ligne, segments de chaque chemin de la ligne).
    qpaths: Vec<(String, u32, Vec<Vec<String>>)>,
}

fn usage_ctx(dep: &Dep, handles: &[Handle]) -> Vec<HandleCtx> {
    handles
        .iter()
        .map(|h| {
            let mut qpaths = Vec::new();
            if dep.ecosystem == Ecosystem::Cargo {
                let set = [fileset(h, Fam::Rust)];
                if !set[0].paths.is_empty() {
                    let needle = format!("{}::", dep.name.replace('-', "_"));
                    for hit in textsearch::grep(&set, &needle, true, 8 * MAX_USAGE) {
                        let t = hit.text.as_str();
                        let tt = t.trim_start();
                        if is_comment(t) || tt.starts_with("use ") || tt.starts_with("pub use ") {
                            continue;
                        }
                        let paths = paths_after(t, &needle);
                        if !paths.is_empty() {
                            qpaths.push((hit.file.clone(), hit.line, paths));
                        }
                    }
                }
            }
            HandleCtx { files: files_importing(dep, h), qpaths }
        })
        .collect()
}

/// Découpe un item de doc (`Agent::get`, `ureq::Agent::get()`, `requests.get`) en
/// (qualificatif immédiat, nom, écrit avec le nom de la dépendance ?). Le nom de la dépendance
/// elle-même n'est jamais un qualificatif.
fn parse_item(item: &str, dep: &Dep) -> Option<(Option<String>, String, bool)> {
    let m = scan_paths(item.trim(), true).into_iter().next()?;
    let dep_names: Vec<String> = match dep.ecosystem {
        Ecosystem::Pypi => pypi_import_names(&dep.name).iter().map(|n| norm(n)).collect(),
        _ => vec![norm(&dep.name)],
    };
    let dep_qualified = m.quals.iter().any(|q| dep_names.contains(&norm(q)));
    let quals: Vec<&String> = m.quals.iter().filter(|q| !dep_names.contains(&norm(q))).collect();
    if m.name.is_empty() || !name_ok(&m.name) {
        return None;
    }
    Some((quals.last().map(|q| (*q).clone()), m.name, dep_qualified))
}

/// Lignes (1-based) d'un fichier qui appellent `name(` (texte), pour les langages sans graphe
/// d'appels dans l'atlas (Python…). Retourne (ligne, preuve forte ?) : forte si la ligne écrit
/// `<module>.name(` avec un nom d'import de la dépendance, ou contient le qualificatif.
fn text_calls(root: &str, path: &str, name: &str, qual: Option<&str>, mods: &[String]) -> Vec<(u32, bool)> {
    let Ok(text) = std::fs::read_to_string(std::path::Path::new(root).join(path)) else { return Vec::new() };
    let needle = format!("{}(", name);
    let mut out = Vec::new();
    for (i, l) in text.lines().enumerate() {
        if is_comment(l) {
            continue;
        }
        if has_word_start(l, &needle) || l.contains(&format!(".{}", needle)) {
            let by_mod = mods.iter().any(|m| l.contains(&format!("{}.{}", m, needle)));
            let by_qual = qual.is_some_and(|q| l.contains(q));
            out.push((i as u32 + 1, by_mod || by_qual));
        }
    }
    out
}

fn usage_in(ctxs: &[HandleCtx], item: &str, dep: &Dep, handles: &[Handle]) -> Vec<SymbolRef> {
    let Some((qual, name, dep_qualified)) = parse_item(item, dep) else { return Vec::new() };
    let own = candidates(handles, &name).len() as u32;
    let mods: Vec<String> = if dep.ecosystem == Ecosystem::Pypi { pypi_import_names(&dep.name) } else { vec![dep.name.clone()] };
    let weak_ok = distinctive(&name) || dep_qualified; // un nom seul et banal ne prouve rien
    let mut all: Vec<SymbolRef> = Vec::new();
    for (h, ctx) in handles.iter().zip(ctxs) {
        for (_, path, imp) in &ctx.files {
            let Some(r) = h.file_by_path(path).and_then(|g| h.node(g)) else { continue };
            let imported_by_name = imp.names.iter().any(|n| *n == name);
            // None = non prouvé (rien à rendre).
            let grade = |strong: bool| -> Option<Proof> {
                if imported_by_name || strong {
                    Some(Proof::ExactQualified)
                } else if qual.is_none() && weak_ok {
                    Some(if own > 0 { Proof::Ambiguous(own) } else { Proof::ImportEvidence })
                } else {
                    None
                }
            };
            if matches!(fam_of_path(path), Fam::Rust | Fam::Js) {
                let Some(refs) = r.refs() else { continue };
                let q_ids: HashSet<String> = match &qual {
                    Some(q) => refs.calls.iter().filter(|c| r.str(c.name) == q).filter_map(|c| h.enclosing_symbol(path, c.line)).collect(),
                    None => HashSet::new(),
                };
                for c in refs.calls.iter().filter(|c| r.str(c.name) == name) {
                    let mut e = evidence_at(h, path, c.line);
                    if let Some(p) = grade(q_ids.contains(&e.id)) {
                        e.proof = p;
                        all.push(e);
                    }
                }
            } else {
                for (line, strong) in text_calls(h.root(), path, &name, qual.as_deref(), &mods) {
                    let mut e = evidence_at(h, path, line);
                    if let Some(p) = grade(strong) {
                        e.proof = p;
                        all.push(e);
                    }
                }
            }
        }
        // Rust : le chemin complet `dep::…::item` nomme l'item lui-même.
        for (path, line, paths) in &ctx.qpaths {
            if paths.iter().any(|segs| segs.iter().any(|x| *x == name)) {
                let mut e = evidence_at(h, path, *line);
                e.proof = Proof::ExactQualified;
                all.push(e);
            }
        }
    }
    finish_usage(all)
}

/// (3) Appelants, dans le projet, d'un item de la documentation (ex. `Agent::get`).
///
/// Seuls comptent les fichiers qui IMPORTENT la dépendance ; dans ceux-là, les appels au nom de
/// l'item (graphe d'appels de l'atlas pour Rust et JS/TS, recherche de texte du fichier pour les
/// autres langages) sont rattachés au symbole englobant. Preuve : `ExactQualified` si le nom est
/// importé nommément de la dépendance (Rust), si le qualificatif de l'item apparaît au même
/// endroit, ou si le code écrit le chemin complet (`ureq::AgentBuilder`, `requests.get(`) ;
/// `Ambiguous(n)` si le projet définit lui-même `n` symboles de ce nom ; sinon `ImportEvidence`
/// (réservé aux noms distinctifs ou écrits avec le nom de la dépendance : un `get` seul ne prouve
/// rien, un item qualifié dont le qualificatif est absent non plus : jamais de lien non prouvé).
pub fn usage_of_item(item: &str, dep: &Dep, handles: &[Handle]) -> Vec<SymbolRef> {
    usage_in(&usage_ctx(dep, handles), item, dep, handles)
}

/// Segments de chaque chemin `needle…` d'une ligne (`ureq::AgentBuilder::new()` → [AgentBuilder, new]).
fn paths_after(line: &str, needle: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(p) = line[from..].find(needle) {
        let at = from + p;
        let prev = line[..at].chars().next_back();
        let after = at + needle.len();
        if !prev.is_some_and(|c| is_ident(c) || c == ':') {
            let mut segs = Vec::new();
            let mut rest = &line[after..];
            loop {
                let end = rest.find(|c: char| !is_ident(c)).unwrap_or(rest.len());
                if end == 0 {
                    break;
                }
                segs.push(rest[..end].to_string());
                rest = &rest[end..];
                match rest.strip_prefix("::") {
                    Some(r) => rest = r,
                    None => break,
                }
            }
            out.push(segs);
        }
        from = after;
    }
    out
}

/// (4) Une section de documentation d'une DÉPENDANCE : relie chaque item cité (`Agent::get`,
/// `Agent`, `agent.call()`) aux appelants du projet, via `usage_of_item` (contexte calculé une
/// seule fois). Au plus 30 items différents ; les mots courants et les noms d'une ou deux lettres
/// sont ignorés.
pub fn link_section_dep(section_text: &str, dep: &Dep, handles: &[Handle]) -> Vec<SymbolRef> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut all: Vec<SymbolRef> = Vec::new();
    let mut ctxs: Option<Vec<HandleCtx>> = None;
    for (m, ctx) in extract_mentions(section_text) {
        if all.len() >= 4 * MAX_USAGE || seen.len() >= MAX_ITEMS {
            break;
        }
        // Un item de doc : qualifié, ou nom distinctif (un mot en minuscules seul ne dit rien).
        if !name_ok(&m.name) || COMMON.contains(&m.name.to_lowercase().as_str()) && m.quals.is_empty() {
            continue;
        }
        if m.quals.is_empty() && (!distinctive(&m.name) || ctx == Ctx::Prose) {
            continue;
        }
        let key = format!("{}::{}", m.quals.last().cloned().unwrap_or_default(), m.name);
        if !seen.insert(key) {
            continue;
        }
        let item = match m.quals.last() {
            Some(q) => format!("{}::{}", q, m.name),
            None => m.name.clone(),
        };
        let c = ctxs.get_or_insert_with(|| usage_ctx(dep, handles));
        all.extend(usage_in(c, &item, dep, handles));
    }
    finish_usage(all)
}

// ───────────────────────── Tests ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atlas::tests::clean as clean_atlas;
    use std::path::PathBuf;
    use std::time::Instant;

    /// Projet-témoin écrit sur disque, indexé par la vraie extraction, puis nettoyé.
    struct Fx {
        name: String,
        dir: PathBuf,
        handles: Vec<Handle>,
    }

    impl Drop for Fx {
        fn drop(&mut self) {
            self.handles.clear(); // libère les mmap avant de supprimer (Windows)
            clean_atlas(&self.name);
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn projet(tag: &str, files: &[(&str, String)]) -> Fx {
        let name = format!("cortex-test-weblink-{}", tag);
        clean_atlas(&name);
        let dir = std::env::temp_dir().join(format!("cortex-weblink-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (p, c) in files {
            let full = dir.join(p);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, c).unwrap();
        }
        let (idx, tracked) = crate::index::build_index(&name, &dir).unwrap();
        crate::atlas::rebuild_full(&name, &idx, tracked).unwrap();
        let h = Handle::open(&name).unwrap();
        Fx { name, dir, handles: vec![h] }
    }

    fn dep(name: &str, eco: Ecosystem) -> Dep {
        Dep {
            name: name.to_string(),
            version: "1.0.0".to_string(),
            ecosystem: eco,
            direct: true,
            lockfile: PathBuf::new(),
            alternatives: Vec::new(),
        }
    }

    fn base_files() -> Vec<(&'static str, String)> {
        vec![
            (
                "src/net.rs",
                "use ureq::Agent;\nuse std::time::Duration;\n\n/// Récupère une page.\npub fn fetch_page(url: &str) -> String {\n    let agent = Agent::new();\n    agent.post(url).call().unwrap().into_string().unwrap()\n}\n\npub fn plain_download(url: &str) -> String {\n    ureq::get(url).call().unwrap().into_string().unwrap()\n}\n\npub fn no_http(x: u32) -> u32 {\n    x + 1\n}\n"
                    .to_string(),
            ),
            ("src/web/crawl.rs", "pub fn crawl_site() -> u32 {\n    1\n}\n\npub fn helper_run() -> u32 {\n    2\n}\n".to_string()),
            ("src/web/store.rs", "pub fn helper_run() -> u32 {\n    3\n}\n\npub fn store_page() -> u32 {\n    4\n}\n".to_string()),
            (
                "src/pool.rs",
                "pub struct Pool {\n    n: u32,\n}\n\nimpl Pool {\n    pub fn acquire_slot(&self) -> u32 {\n        self.n\n    }\n    pub fn reset(&self) {}\n}\n\npub struct Other;\n\nimpl Other {\n    pub fn reset(&self) {}\n}\n"
                    .to_string(),
            ),
            (
                "src/app/client.ts",
                "import axios from 'axios'\nimport retry from 'axios-retry'\nimport { Foo } from './foo'\n\nexport function loadUsers() {\n  return axios.get('/u')\n}\n\nexport function retryAll() {\n  return retry(3)\n}\n"
                    .to_string(),
            ),
            (
                "src/svc/http_client.py",
                "import requests\nfrom requests.adapters import HTTPAdapter\nimport yaml\n\ndef get_page(url):\n    r = requests.get(url)\n    return r.text\n\ndef other_thing():\n    return 1\n"
                    .to_string(),
            ),
            (
                "cmd/main.go",
                "package main\n\nimport (\n\t\"fmt\"\n\t\"github.com/spf13/cobra\"\n)\n\nfunc RunRoot() {\n\tfmt.Println(cobra.Command{})\n}\n".to_string(),
            ),
            (
                "cmd/extra.go",
                "package main\n\nimport \"github.com/spf13/cobra-extra\"\n\nfunc RunExtra() {\n\t_ = cobraextra.X\n}\n".to_string(),
            ),
        ]
    }

    fn ids(v: &[SymbolRef]) -> Vec<String> {
        v.iter().map(|r| r.id.clone()).collect()
    }

    fn find<'a>(v: &'a [SymbolRef], id_part: &str) -> Option<&'a SymbolRef> {
        v.iter().find(|r| r.id.contains(id_part))
    }

    // ── Extraction et filtres (sans atlas) ──

    #[test]
    fn extraction_des_spans_entre_accents_graves() {
        let ms = extract_mentions("Utilisez `fetch_page(url)` puis ``Agent::new()`` ; sans `fermeture ouverte");
        let fp = ms.iter().find(|(m, _)| m.name == "fetch_page").expect("fetch_page");
        assert!(fp.0.call && fp.1 == Ctx::Inline);
        let an = ms.iter().find(|(m, _)| m.name == "new").expect("Agent::new");
        assert_eq!(an.0.quals, vec!["Agent".to_string()]);
        assert!(ms.iter().all(|(m, _)| m.name != "fermeture"), "un span non fermé n'est pas du code");
    }

    #[test]
    fn extraction_des_blocs_de_code_et_signatures() {
        let t =
            "Texte\n```rust\nfn fetch_page(u: &str) -> String {\n    let a = Agent::new();\n}\nclass Widget:\n```\napres `plain_download`";
        let ms = extract_mentions(t);
        assert!(ms.iter().any(|(m, c)| m.name == "fetch_page" && m.sig && *c == Ctx::Block));
        assert!(ms.iter().any(|(m, c)| m.name == "Widget" && m.sig && *c == Ctx::Block));
        assert!(ms.iter().any(|(m, c)| m.name == "new" && m.quals == ["Agent"] && *c == Ctx::Block));
        assert!(ms.iter().any(|(m, c)| m.name == "plain_download" && *c == Ctx::Inline), "la clôture est bien refermée");
        let tilde = extract_mentions("~~~\nfn only_in_tilde() {}\n~~~\n");
        assert!(tilde.iter().any(|(m, _)| m.name == "only_in_tilde"));
    }

    #[test]
    fn la_prose_ne_compte_que_pour_les_chemins() {
        let ms = extract_mentions(
            "Le module web::crawl::crawl_site est appelé. Le mot fetch_page seul ne compte pas. Voir config.toml et README.md.",
        );
        assert!(ms.iter().any(|(m, c)| m.name == "crawl_site" && m.quals == ["web", "crawl"] && *c == Ctx::Prose));
        assert!(ms.iter().all(|(m, _)| m.name != "fetch_page"), "prose : pas de mot seul");
        assert!(ms.iter().all(|(_, c)| *c == Ctx::Prose));
    }

    #[test]
    fn les_nombres_et_versions_ne_donnent_aucune_mention() {
        let ms = scan_paths("0x1F 1.5 v1.2.3 3d 2024", true);
        assert!(ms.iter().all(|m| m.name != "x1F" && m.name != "d"), "{:?}", ms);
    }

    #[test]
    fn mots_courants_courts_et_vides_sont_refuses() {
        let m = |n: &str| Mention { quals: Vec::new(), name: n.to_string(), call: false, sig: false };
        for bad in [
            "id",
            "x",
            "a",
            "get",
            "set",
            "search",
            "render",
            "String",
            "Config",
            "Error",
            "let",
            "self",
            "fn",
            "to_string",
            "unwrap_or",
            "Vec",
            "main",
            "run",
        ] {
            assert!(!accept(&m(bad), Ctx::Inline), "{} ne doit jamais devenir un lien", bad);
        }
        for good in ["fetch_page", "Agent", "loadUsers", "MAX_SIZE_LIMIT", "HTTPAdapter"] {
            assert!(accept(&m(good), Ctx::Inline), "{} doit être cherché", good);
        }
        // Dans un bloc, une variable locale seule ne prouve rien ; une déclaration, si.
        let var = m("agent_config");
        assert!(!accept(&var, Ctx::Block));
        assert!(accept(&Mention { sig: true, ..var }, Ctx::Block));
        // Un qualificatif remplace la distinction du nom, pas la longueur minimale.
        assert!(accept(&Mention { quals: vec!["Agent".into()], name: "get".into(), call: true, sig: false }, Ctx::Inline));
        assert!(!accept(&Mention { quals: vec!["Agent".into()], name: "go".into(), call: true, sig: false }, Ctx::Inline));
    }

    #[test]
    fn proofs_ordre_et_libelles() {
        assert!(Proof::ExactQualified.rank() < Proof::ExactUnique.rank());
        assert!(Proof::ExactUnique.rank() < Proof::ImportEvidence.rank());
        assert!(Proof::ImportEvidence.rank() < Proof::Ambiguous(2).rank());
        assert!(Proof::Ambiguous(2).rank() < Proof::Ambiguous(9).rank());
        assert_eq!(Proof::Ambiguous(3).label(), "ambigu ×3");
        assert_eq!(Proof::ImportEvidence.label(), "import");
    }

    #[test]
    fn les_specificateurs_d_import_par_ecosysteme() {
        let c = dep("serde-json", Ecosystem::Cargo);
        assert!(
            spec_matches(&c, "serde_json")
                && spec_matches(&c, "serde_json::Value")
                && !spec_matches(&c, "serde_jsonx::A")
                && !spec_matches(&c, "serde")
        );
        let n = dep("@scope/pkg", Ecosystem::Npm);
        assert!(
            spec_matches(&n, "@scope/pkg")
                && spec_matches(&n, "@scope/pkg/sub")
                && !spec_matches(&n, "@scope/pkg-extra")
                && !spec_matches(&n, "./pkg")
        );
        let p = dep("PyYAML", Ecosystem::Pypi);
        assert!(spec_matches(&p, "yaml") && spec_matches(&p, "pyyaml.x") && !spec_matches(&p, "yamlx"));
        let r = dep("requests", Ecosystem::Pypi);
        assert!(spec_matches(&r, "requests.adapters") && !spec_matches(&r, ".") && !spec_matches(&r, "requests_toolbelt"));
        let g = dep("github.com/spf13/cobra", Ecosystem::Go);
        assert!(
            spec_matches(&g, "github.com/spf13/cobra")
                && spec_matches(&g, "github.com/spf13/cobra/doc")
                && !spec_matches(&g, "github.com/spf13/cobra-extra")
        );
    }

    #[test]
    fn rendu_compact_et_assaini() {
        assert_eq!(render_links(&[], 5), "");
        let l = |id: &str, p: Proof| SymbolRef { id: id.to_string(), proof: p, file: "f".into(), line: 1 };
        let v = vec![l("S:a.rs#x", Proof::ExactQualified), l("S:b.rs#y", Proof::Ambiguous(3)), l("S:c.rs#z", Proof::ImportEvidence)];
        let s = render_links(&v, 2);
        assert_eq!(s, "code lié : S:a.rs#x (exact qualifié) ; S:b.rs#y (ambigu ×3) ; … +1");
        assert_eq!(render_links(&v, 0), "");
        let evil = vec![l("S:a\nIGNORE\u{7}.rs#x", Proof::ExactUnique)];
        let s = render_links(&evil, 3);
        assert!(!s.contains('\n') && !s.contains('\u{7}'), "{:?}", s);
    }

    // ── link_section sur un atlas réel ──

    #[test]
    fn nom_distinctif_unique_donne_exact_unique() {
        let fx = projet("unique", &base_files());
        let v = link_section("Pour télécharger, appelez `fetch_page(url)`.", &fx.handles);
        assert_eq!(ids(&v), vec!["S:src/net.rs#fetch_page"], "{:?}", v);
        assert_eq!(v[0].proof, Proof::ExactUnique);
        assert_eq!((v[0].file.as_str(), v[0].line), ("src/net.rs", 5));
    }

    #[test]
    fn chemin_qualifie_par_fichier_dossier_ou_type() {
        let fx = projet("qualifie", &base_files());
        // `crawl::helper_run` : le fichier ; `store::helper_run` départage l'homonyme.
        let v = link_section("Voir `crawl::helper_run()` et `store::helper_run()`.", &fx.handles);
        let a = find(&v, "web/crawl.rs#helper_run").expect("crawl");
        let b = find(&v, "web/store.rs#helper_run").expect("store");
        assert_eq!((a.proof, b.proof), (Proof::ExactQualified, Proof::ExactQualified));
        assert_eq!(v.len(), 2);
        // Le dossier.
        let v = link_section("Le chemin web::crawl_site existe.", &fx.handles);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].proof, Proof::ExactQualified);
        // Le type englobant (bloc `impl`), parmi deux `reset`.
        let v = link_section("`Pool::reset()` remet à zéro ; `Other.reset()` aussi.", &fx.handles);
        assert_eq!(v.len(), 2, "{:?}", v);
        assert!(v.iter().all(|r| r.proof == Proof::ExactQualified));
        assert_ne!(v[0].line, v[1].line);
    }

    #[test]
    fn plusieurs_symboles_donnent_ambigu() {
        let fx = projet("ambigu", &base_files());
        let v = link_section("Appelez `helper_run`.", &fx.handles);
        assert_eq!(v.len(), 2, "{:?}", v);
        assert!(v.iter().all(|r| r.proof == Proof::Ambiguous(2)));
        // Un qualificatif qui ne départage pas (deux fichiers dans `web`) reste ambigu.
        let v = link_section("`web::helper_run()`", &fx.handles);
        assert_eq!(v.len(), 2);
        assert!(v.iter().all(|r| r.proof == Proof::Ambiguous(2)));
    }

    #[test]
    fn un_qualificatif_etranger_au_projet_ne_donne_aucun_lien() {
        let fx = projet("etranger", &base_files());
        // `Agent::get`, `reqwest::fetch_page` : items de bibliothèques, pas du projet.
        let v = link_section("Utilisez `Agent::get(url)` ou `reqwest::fetch_page(u)` ou `agent.helper_run()`.", &fx.handles);
        assert!(v.is_empty(), "{:?}", v);
    }

    #[test]
    fn mots_courants_et_inconnus_ne_donnent_aucun_lien() {
        let fx = projet("courants", &base_files());
        let v = link_section(
            "Le `String` `Vec` `Config` `get` `main` `x` `id` `unwrap_or` `no_such_symbol_anywhere` `reset` sont courants.",
            &fx.handles,
        );
        assert!(v.is_empty(), "{:?}", v);
        // `reset` seul (minuscule) ne prouve rien même s'il existe deux fois dans le projet.
        assert!(link_section("`reset`", &fx.handles).is_empty());
    }

    #[test]
    fn les_blocs_de_code_relient_signatures_et_appels() {
        let fx = projet("blocs", &base_files());
        let t = "Exemple :\n```rust\nlet unused_local = 1;\nfn plain_download(u: &str) {\n    let s = store_page();\n}\n```\n";
        let v = link_section(t, &fx.handles);
        let got = ids(&v);
        assert!(got.contains(&"S:src/net.rs#plain_download".to_string()), "{:?}", got);
        assert!(got.contains(&"S:src/web/store.rs#store_page".to_string()), "{:?}", got);
        assert_eq!(got.len(), 2, "la variable locale ne lie rien : {:?}", got);
    }

    #[test]
    fn link_section_est_trie_borne_et_sans_doublon() {
        let fx = projet("trie", &base_files());
        let t = "`fetch_page` puis encore `fetch_page` puis `helper_run` puis `Pool::reset` puis `plain_download`";
        let v = link_section(t, &fx.handles);
        let all = ids(&v);
        let mut dedup = all.clone();
        dedup.sort();
        dedup.dedup();
        assert_eq!(all.len(), dedup.len(), "doublons : {:?}", all);
        let ranks: Vec<u32> = v.iter().map(|r| r.proof.rank()).collect();
        assert!(ranks.windows(2).all(|w| w[0] <= w[1]), "les preuves fortes d'abord : {:?}", v);
        assert_eq!(v[0].proof, Proof::ExactQualified);
    }

    #[test]
    fn la_resolution_est_rapide_une_fois_les_handles_ouverts() {
        let fx = projet("perf", &base_files());
        let sec = "Voir `fetch_page`, `plain_download`, `crawl::crawl_site`, `Pool::reset`, `helper_run`, `Agent::get`, `String`, `no_http`\n```rust\nfn store_page() {}\n```";
        let _ = link_section(sec, &fx.handles); // construit la vue fusionnée (une fois)
        let t0 = Instant::now();
        let mut n = 0;
        for _ in 0..20 {
            n += link_section(sec, &fx.handles).len();
        }
        let per = t0.elapsed().as_secs_f64() * 1000.0 / 20.0;
        eprintln!("link_section : {} liens, {:.3} ms par section", n / 20, per);
        assert!(n > 0);
        assert!(per < 5.0, "{:.3} ms par section (cible < 5 ms)", per);
    }

    // ── lib_usage ──

    #[test]
    fn lib_usage_rust_remonte_aux_symboles_appelants() {
        let fx = projet("usage-rs", &base_files());
        let v = lib_usage(&dep("ureq", Ecosystem::Cargo), &fx.handles);
        assert!(v.iter().all(|r| r.proof == Proof::ImportEvidence));
        // `Agent` importé nommément : fetch_page l'appelle. `ureq::get` en chemin complet : plain_download.
        assert!(find(&v, "src/net.rs#fetch_page").is_some(), "{:?}", v);
        assert!(find(&v, "src/net.rs#plain_download").is_some(), "{:?}", v);
        assert!(find(&v, "no_http").is_none(), "{:?}", v);
        assert!(v.iter().all(|r| r.file == "src/net.rs"), "seul net.rs importe ureq : {:?}", v);
        assert!(v.iter().all(|r| !r.id.starts_with("F:")), "le fichier a des symboles identifiés : {:?}", v);
    }

    #[test]
    fn lib_usage_js_ne_confond_pas_les_paquets_voisins() {
        let fx = projet("usage-ts", &base_files());
        let v = lib_usage(&dep("axios", Ecosystem::Npm), &fx.handles);
        assert_eq!(v.len(), 1, "{:?}", v);
        assert_eq!(v[0].file, "src/app/client.ts");
        assert_eq!(v[0].id, "F:src/app/client.ts");
        assert_eq!(v[0].line, 1, "ligne de l'import");
        let none = lib_usage(&dep("lodash", Ecosystem::Npm), &fx.handles);
        assert!(none.is_empty());
    }

    #[test]
    fn lib_usage_python_avec_alias_de_paquet() {
        let fx = projet("usage-py", &base_files());
        let v = lib_usage(&dep("requests", Ecosystem::Pypi), &fx.handles);
        assert_eq!(ids(&v), vec!["F:src/svc/http_client.py"], "{:?}", v);
        let y = lib_usage(&dep("PyYAML", Ecosystem::Pypi), &fx.handles);
        assert_eq!(y.len(), 1, "PyYAML s'importe `yaml` : {:?}", y);
        assert_eq!(y[0].line, 3);
    }

    #[test]
    fn lib_usage_go_par_recherche_de_texte_exacte() {
        let fx = projet("usage-go", &base_files());
        let v = lib_usage(&dep("github.com/spf13/cobra", Ecosystem::Go), &fx.handles);
        assert_eq!(v.len(), 1, "cobra-extra ne compte pas : {:?}", v);
        assert_eq!(v[0].file, "cmd/main.go");
        assert_eq!(v[0].line, 5);
        assert_eq!(v[0].proof, Proof::ImportEvidence);
    }

    #[test]
    fn lib_usage_est_plafonne_a_200() {
        let names: Vec<String> = (0..260).map(|i| format!("src/gen/f{}.ts", i)).collect();
        let files: Vec<(&str, String)> = names
            .iter()
            .enumerate()
            .map(|(i, p)| (p.as_str(), format!("import x from 'bigdep'\n\nexport function gen{}Thing() {{\n  return x\n}}\n", i)))
            .collect();
        let fx = projet("cap", &files);
        let v = lib_usage(&dep("bigdep", Ecosystem::Npm), &fx.handles);
        assert_eq!(v.len(), MAX_USAGE);
        // Groupé par fichier : trié par chemin à preuve égale.
        let files_seen: Vec<&str> = v.iter().map(|r| r.file.as_str()).collect();
        let mut sorted = files_seen.clone();
        sorted.sort();
        assert_eq!(files_seen, sorted);
    }

    // ── usage_of_item et link_section_dep ──

    #[test]
    fn usage_d_un_item_rust_importe_nommement() {
        let fx = projet("item-rs", &base_files());
        let d = dep("ureq", Ecosystem::Cargo);
        let v = usage_of_item("Agent", &d, &fx.handles);
        let r = find(&v, "src/net.rs#fetch_page").expect("fetch_page appelle Agent");
        assert_eq!(r.proof, Proof::ExactQualified, "importé nommément de la dépendance : {:?}", v);
        // `ureq::get(url)` : chemin complet écrit dans plain_download, pas d'import nommé.
        let v = usage_of_item("get", &d, &fx.handles);
        let r = find(&v, "src/net.rs#plain_download").expect("ureq::get dans plain_download");
        assert_eq!(r.proof, Proof::ExactQualified, "{:?}", v);
        assert!(v.iter().all(|r| r.proof != Proof::ExactUnique), "{:?}", v);
        // Un item que personne n'appelle.
        assert!(usage_of_item("Unused", &d, &fx.handles).is_empty());
    }

    #[test]
    fn usage_d_un_item_rust_en_chemin_complet_sans_use() {
        let mut files = base_files();
        files.push((
            "src/q.rs",
            "pub fn full_path_user() -> u32 {
    let _a = ureq::AgentBuilder::new().build();
    // ureq::AgentBuilder dans un commentaire
    1
}

pub fn bystander() -> u32 {
    7
}
"
            .to_string(),
        ));
        let fx = projet("item-full", &files);
        let d = dep("ureq", Ecosystem::Cargo);
        let v = usage_of_item("ureq::AgentBuilder", &d, &fx.handles);
        assert_eq!(ids(&v), vec!["S:src/q.rs#full_path_user"], "{:?}", v);
        assert_eq!((v[0].proof, v[0].line), (Proof::ExactQualified, 2));
        assert!(usage_of_item("AgentBuilderX", &d, &fx.handles).is_empty());
    }

    #[test]
    fn chemins_complets_d_une_ligne() {
        let p = paths_after("let a = ureq::AgentBuilder::new().build(); my_ureq::Fake::x(); ureq::get(u)", "ureq::");
        assert_eq!(p, vec![vec!["AgentBuilder".to_string(), "new".to_string()], vec!["get".to_string()]]);
        assert!(paths_after("xureq::A", "ureq::").is_empty());
    }

    #[test]
    fn usage_d_un_item_python_par_texte_borne_aux_importeurs() {
        let fx = projet("item-py", &base_files());
        let d = dep("requests", Ecosystem::Pypi);
        let v = usage_of_item("requests.get", &d, &fx.handles);
        assert_eq!(v.len(), 1, "{:?}", v);
        assert_eq!(v[0].id, "S:src/svc/http_client.py#get_page");
        assert_eq!(v[0].proof, Proof::ExactQualified, "la ligne écrit requests.get( en toutes lettres");
        assert_eq!(v[0].line, 6);
        // `get` seul : le code écrit `requests.get(`, c'est la preuve (même résultat) ;
        // un `get(` nu, sans le module, ne prouverait rien (voir `usage_d_un_item_signale_l_homonyme_du_projet`).
        assert_eq!(usage_of_item("get", &d, &fx.handles), v);
        // Un autre paquet : même nom d'item, mais le fichier n'importe pas CE paquet.
        assert!(usage_of_item("get", &dep("flask", Ecosystem::Pypi), &fx.handles).is_empty());
    }

    #[test]
    fn usage_d_un_item_signale_l_homonyme_du_projet() {
        let mut files = base_files();
        files.push(("src/own.rs", "pub fn post() {}\n".to_string()));
        let fx = projet("item-homonyme", &files);
        let d = dep("ureq", Ecosystem::Cargo);
        // `agent.post(url)` dans net.rs, item écrit `ureq::post` : le projet définit aussi `post`
        // et le fichier n'importe pas `post` nommément → ambigu, pas prouvé.
        let v = usage_of_item("ureq::post", &d, &fx.handles);
        let r = find(&v, "src/net.rs#fetch_page").expect("appel de post dans fetch_page");
        assert_eq!(r.proof, Proof::Ambiguous(1), "{:?}", v);
        // Avec le qualificatif `Agent` présent dans la même fonction : prouvé.
        let v = usage_of_item("Agent::post", &d, &fx.handles);
        assert_eq!(find(&v, "src/net.rs#fetch_page").expect("Agent::post").proof, Proof::ExactQualified, "{:?}", v);
        // Qualificatif absent du code : non prouvé, aucun lien.
        assert!(usage_of_item("Missing::post", &d, &fx.handles).is_empty());
        // `post` seul : mot banal, aucun lien.
        assert!(usage_of_item("post", &d, &fx.handles).is_empty());
    }

    #[test]
    fn link_section_dep_relie_les_items_de_la_doc_aux_appelants() {
        let fx = projet("dep", &base_files());
        let d = dep("ureq", Ecosystem::Cargo);
        let doc = "Créez un `Agent` puis appelez `Agent::new()`.\n```rust\nlet agent = ureq::Agent::new();\n```\nLe mot `get` seul et `String` ne relient rien.";
        let v = link_section_dep(doc, &d, &fx.handles);
        assert!(find(&v, "src/net.rs#fetch_page").is_some(), "{:?}", v);
        assert!(v.iter().all(|r| r.file == "src/net.rs"), "{:?}", v);
        let t = render_links(&v, 5);
        assert!(t.starts_with("code lié : S:src/net.rs#fetch_page"), "{}", t);
        // Une doc qui ne cite rien d'utilisé : aucun lien.
        assert!(link_section_dep("Rien de `neuf_ici` dans `Autre::chose`.", &d, &fx.handles).is_empty());
    }

    // ── Essai réel (test ignoré : lancé à la main avec --ignored --nocapture) ──

    fn cargo_registry_dirs(prefix: &str) -> Vec<PathBuf> {
        let home = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(|h| PathBuf::from(h).join(".cargo")));
        let mut out = Vec::new();
        let Some(home) = home else { return out };
        let Ok(rd) = std::fs::read_dir(home.join("registry").join("src")) else { return out };
        for idx in rd.flatten() {
            if let Ok(rd2) = std::fs::read_dir(idx.path()) {
                for c in rd2.flatten() {
                    let n = c.file_name().to_string_lossy().to_string();
                    if n.starts_with(prefix) && n[prefix.len()..].starts_with(|ch: char| ch.is_ascii_digit()) {
                        out.push(c.path());
                    }
                }
            }
        }
        out.sort();
        out
    }

    /// Texte de doc d'une crate installée : commentaires `//!` de lib.rs + README.
    fn doc_text(dir: &std::path::Path) -> String {
        let mut s = String::new();
        if let Ok(src) = std::fs::read_to_string(dir.join("src").join("lib.rs")) {
            for l in src.lines() {
                let t = l.trim_start();
                if let Some(r) = t.strip_prefix("//!") {
                    s.push_str(r.strip_prefix(' ').unwrap_or(r));
                    s.push('\n');
                }
            }
        }
        if let Ok(r) = std::fs::read_to_string(dir.join("README.md")) {
            s.push('\n');
            s.push_str(&r);
        }
        s
    }

    #[test]
    #[ignore]
    fn essai_reel_sur_cortex() {
        let h = Handle::open("Cortex").expect("projet Cortex indexé (cortex index)");
        let handles = vec![h];
        for krate in ["ureq", "scraper", "rayon"] {
            let dirs = cargo_registry_dirs(&format!("{}-", krate));
            let Some(dir) = dirs.last() else {
                eprintln!("== {} : sources installées introuvables", krate);
                continue;
            };
            let d = dep(krate, Ecosystem::Cargo);
            let t0 = Instant::now();
            let usage = lib_usage(&d, &handles);
            let t_usage = t0.elapsed();
            let doc = doc_text(dir);
            let t1 = Instant::now();
            let linked = link_section_dep(&doc, &d, &handles);
            let t_doc = t1.elapsed();
            let t2 = Instant::now();
            let direct = link_section(&doc, &handles);
            let t_direct = t2.elapsed();
            let syms = usage.iter().filter(|r| r.id.starts_with("S:")).count();
            eprintln!(
                "== {} ({}) : lib_usage {} liens dont {} symboles en {:.1} ms ; link_section_dep {} liens en {:.1} ms ; link_section (code du projet cité par la doc) {} liens en {:.2} ms",
                krate,
                dir.file_name().unwrap().to_string_lossy(),
                usage.len(),
                syms,
                t_usage.as_secs_f64() * 1000.0,
                linked.len(),
                t_doc.as_secs_f64() * 1000.0,
                direct.len(),
                t_direct.as_secs_f64() * 1000.0
            );
            for r in usage.iter().take(5) {
                eprintln!("   usage  {} [{}] {}:{}", r.id, r.proof.label(), r.file, r.line);
            }
            for r in linked.iter().take(5) {
                eprintln!("   doc->  {} [{}] {}:{}", r.id, r.proof.label(), r.file, r.line);
            }
            eprintln!("   {}", render_links(&usage, 3));
        }
    }
}
