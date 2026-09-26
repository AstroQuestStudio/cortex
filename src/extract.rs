//! Extraction de symboles via tree-sitter (robuste, vs regex de graphify).
//!
//! Pour chaque langage, on parse le source et on capture les déclarations
//! (fonctions, classes, méthodes, types, imports) via des requêtes tree-sitter.
//! Chaque symbole reçoit ses tokens décomposés (camelCase) pour la recherche.

use crate::lang::Lang;
use crate::symbol::{tokenize_identifier, CallRef, FileRefs, Symbol, SymbolKind};
use std::sync::OnceLock;
use tree_sitter::{Parser, Query, QueryCursor, Tree};

// ─── Grammaires, requêtes compilées UNE fois, analyseur par thread ────────────
//
// Compiler une requête tree-sitter (`Query::new`) coûte plusieurs ms pour la
// grammaire TSX : le faire pour CHAQUE fichier (deux fois : symboles puis
// références) dominait la construction complète (≈ 18 ms par fichier sur un
// thread, 148 s de CPU pour AstroQuest). Les requêtes sont compilées une fois
// par processus (`Query` est `Sync`) ; chaque thread garde son `Parser` ; un
// fichier JS/TS n'est analysé qu'une fois pour ses symboles ET ses références.
// Mêmes requêtes, même arbre : les résultats sont identiques.

#[derive(Clone, Copy, PartialEq, Eq)]
enum Grammar {
    Ts,
    Tsx,
    Js,
    Py,
    Rust,
    CSharp,
}

impl Grammar {
    fn of(lang: Lang) -> Option<Grammar> {
        match lang {
            Lang::TypeScript => Some(Grammar::Ts),
            Lang::Tsx => Some(Grammar::Tsx),
            Lang::JavaScript | Lang::Jsx => Some(Grammar::Js),
            Lang::Python => Some(Grammar::Py),
            Lang::Rust => Some(Grammar::Rust),
            Lang::CSharp => Some(Grammar::CSharp),
            _ => None,
        }
    }

    fn language(self) -> tree_sitter::Language {
        match self {
            Grammar::Ts => tree_sitter_typescript::language_typescript(),
            Grammar::Tsx => tree_sitter_typescript::language_tsx(),
            Grammar::Js => tree_sitter_javascript::language(),
            Grammar::Py => tree_sitter_python::language(),
            Grammar::Rust => tree_sitter_rust::language(),
            Grammar::CSharp => tree_sitter_c_sharp::language(),
        }
    }

    fn symbol_query_src(self) -> &'static str {
        match self {
            Grammar::Ts | Grammar::Tsx | Grammar::Js => TS_QUERY,
            Grammar::Py => PY_QUERY,
            Grammar::Rust => RUST_QUERY,
            Grammar::CSharp => CS_QUERY,
        }
    }
}

const N_GRAMMARS: usize = 6;
const QUERY_SLOT: OnceLock<Option<Query>> = OnceLock::new();
/// Requêtes de symboles, une par grammaire.
static SYMBOL_QUERIES: [OnceLock<Option<Query>>; N_GRAMMARS] = [QUERY_SLOT; N_GRAMMARS];
/// Requêtes de références JS/TS : (grammaire, avec JSX).
static REF_QUERIES: [OnceLock<Option<Query>>; 4] = [QUERY_SLOT; 4];

fn symbol_query(g: Grammar) -> Option<&'static Query> {
    SYMBOL_QUERIES[g as usize].get_or_init(|| Query::new(&g.language(), g.symbol_query_src()).ok()).as_ref()
}

thread_local! {
    /// Un analyseur par thread et par grammaire (réutilisé d'un fichier à l'autre).
    static PARSERS: std::cell::RefCell<Vec<Option<Parser>>> = std::cell::RefCell::new((0..N_GRAMMARS).map(|_| None).collect());
}

/// Arbre syntaxique d'un source (analyseur du thread courant).
fn parse(g: Grammar, source: &[u8]) -> Option<Tree> {
    PARSERS.with(|cell| {
        let mut parsers = cell.borrow_mut();
        let slot = &mut parsers[g as usize];
        if slot.is_none() {
            let mut p = Parser::new();
            p.set_language(&g.language()).ok()?;
            *slot = Some(p);
        }
        slot.as_mut()?.parse(source, None)
    })
}

/// Extrait les symboles d'un fichier. Retourne vide si langage non supporté ou parse KO.
#[cfg(test)]
pub fn extract_symbols(lang: Lang, source: &str) -> Vec<Symbol> {
    extract_symbols_and_refs(lang, source).0
}

/// Symboles ET références d'un fichier, en une seule analyse tree-sitter
/// (même résultat que `extract_symbols` puis `extract_refs`).
pub fn extract_symbols_and_refs(lang: Lang, source: &str) -> (Vec<Symbol>, FileRefs) {
    if lang == Lang::Markdown {
        return (extract_markdown(source), extract_refs_with(lang, source, None));
    }
    let Some(g) = Grammar::of(lang) else { return (Vec::new(), extract_refs_with(lang, source, None)) };
    let tree = parse(g, source.as_bytes());
    let symbols = match (&tree, symbol_query(g)) {
        (Some(t), Some(q)) => run(source.as_bytes(), t, q),
        _ => Vec::new(),
    };
    let refs = extract_refs_with(lang, source, tree.as_ref());
    (symbols, refs)
}

/// Exécute une requête tree-sitter et construit les symboles à partir des captures.
/// Convention de la requête : capture `@name` = identifiant, `@kind.<x>` = type de symbole.
fn run(source: &[u8], tree: &Tree, query: &Query) -> Vec<Symbol> {
    let capture_names = query.capture_names();
    let mut cursor = QueryCursor::new();
    let mut symbols = Vec::new();

    // Itération via StreamingIterator (tree-sitter 0.22 : `matches` n'est pas un
    // Iterator standard ; on consomme chaque match avec `next()` qui emprunte &mut).
    let it = cursor.matches(query, tree.root_node(), source);
    for mat in it {
        let mut name: Option<String> = None;
        let mut line = 0u32;
        let mut end_line: Option<u32> = None;
        let mut kind = SymbolKind::Function;
        // Début de la déclaration (octet) : nœud @kind (remonté à un `export`
        // englobant), sinon début de la ligne du nom.
        let mut decl_start: Option<usize> = None;
        let mut name_start = 0usize;
        for cap in mat.captures {
            let cname = capture_names[cap.index as usize];
            if cname == "name" {
                let text = cap.node.utf8_text(source).unwrap_or("").trim();
                name = Some(text.to_string());
                line = cap.node.start_position().row as u32 + 1;
                name_start = cap.node.start_byte();
            } else if let Some(k) = cname.strip_prefix("kind.") {
                let mut decl = cap.node;
                if let Some(p) = decl.parent() {
                    if p.kind() == "export_statement" {
                        decl = p;
                    }
                }
                decl_start = Some(decl.start_byte());
                kind = kind_from_str(k);
                // Le nœud taggé @kind.xxx est la déclaration ENTIÈRE (fonction, classe…) :
                // sa ligne de fin délimite la portée pour rattacher les appels (granularité
                // fonction, voir graph.rs). Les patterns sans tag @kind.xxx (ex: public_field_definition
                // seul) gardent end_line = 0 → portée "fichier entier" par défaut ailleurs.
                end_line = Some(cap.node.end_position().row as u32 + 1);
            }
        }
        if let Some(n) = name {
            if !n.is_empty() && n.len() <= 120 {
                let kind = refine_kind(&n, kind);
                let tokens = tokenize_identifier(&n);
                let start =
                    decl_start.unwrap_or_else(|| source[..name_start].iter().rposition(|&b| b == b'\n').map(|i| i + 1).unwrap_or(0));
                symbols.push(Symbol {
                    name: n,
                    kind,
                    line,
                    end_line: end_line.map(|e| e.max(line)).unwrap_or(0),
                    signature: signature_at(source, start),
                    tokens,
                    doc: Vec::new(),
                    summary: String::new(),
                });
            }
        }
    }
    symbols
}

fn kind_from_str(s: &str) -> SymbolKind {
    match s {
        "function" => SymbolKind::Function,
        "method" => SymbolKind::Method,
        "class" => SymbolKind::Class,
        "interface" => SymbolKind::Interface,
        "struct" => SymbolKind::Struct,
        "enum" => SymbolKind::Enum,
        "type" => SymbolKind::Type,
        "const" => SymbolKind::Const,
        "import" => SymbolKind::Import,
        _ => SymbolKind::Function,
    }
}

/// Affine le kind selon les conventions (React : use* = hook, PascalCase composant).
fn refine_kind(name: &str, base: SymbolKind) -> SymbolKind {
    if matches!(base, SymbolKind::Function | SymbolKind::Const) {
        if name.starts_with("use") && name.len() > 3 && name.chars().nth(3).map(|c| c.is_uppercase()).unwrap_or(false) {
            return SymbolKind::Hook;
        }
        // PascalCase = composant ; SCREAMING_CASE (constante) reste tel quel.
        if name.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) && name.chars().any(|c| c.is_lowercase()) {
            return SymbolKind::Component;
        }
    }
    base
}

/// Longueur maximale d'une signature (caractères, avant « … »).
const MAX_SIGNATURE_CHARS: usize = 200;

/// Signature compacte d'une déclaration commençant à l'octet `start` : le texte
/// jusqu'au corps (`{` hors parenthèses/crochets/chevrons), à un `;` ou à une
/// fin de ligne hors parenthèses — paramètres multi-lignes compris —, espaces
/// normalisés, `=`/`=>`/`:` final retiré. Les lignes d'attributs en tête
/// (`[Attr]`, `#[attr]`, `@decorateur`) sont sautées.
pub fn signature_at(source: &[u8], start: usize) -> String {
    let mut i = start.min(source.len());
    loop {
        let rest = &source[i..];
        let lead = rest.iter().position(|&b| b != b' ' && b != b'\t').unwrap_or(rest.len());
        let r = &rest[lead..];
        if r.starts_with(b"[") || r.starts_with(b"#[") || (r.starts_with(b"@") && !r.starts_with(b"@/")) {
            match r.iter().position(|&b| b == b'\n') {
                Some(nl) => i += lead + nl + 1,
                None => return String::new(),
            }
        } else {
            i += lead;
            break;
        }
    }
    let (mut paren, mut angle) = (0i32, 0i32);
    let mut end = i;
    let mut prev = b' ';
    let limit = (i + 600).min(source.len());
    while end < limit {
        let c = source[end];
        match c {
            b'(' | b'[' => paren += 1,
            b')' | b']' => paren = (paren - 1).max(0),
            b'<' if prev.is_ascii_alphanumeric() || prev == b'_' => angle += 1,
            b'>' if angle > 0 && prev != b'=' && prev != b'-' => angle -= 1,
            b'{' if paren == 0 && angle == 0 => break,
            b';' if paren == 0 => break,
            b'\n' if paren == 0 && angle == 0 => break,
            _ => {}
        }
        if !c.is_ascii_whitespace() {
            prev = c;
        }
        end += 1;
    }
    let raw = String::from_utf8_lossy(&source[i..end]);
    let mut sig = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    loop {
        let t = sig.trim_end();
        let t2 = t.strip_suffix("=>").or_else(|| t.strip_suffix('=')).or_else(|| t.strip_suffix(':')).unwrap_or(t).trim_end();
        if t2.len() == sig.len() {
            break;
        }
        sig = t2.to_string();
    }
    if sig.chars().count() > MAX_SIGNATURE_CHARS {
        let cut: String = sig.chars().take(MAX_SIGNATURE_CHARS).collect();
        sig = format!("{}…", cut.trim_end());
    }
    sig
}

/// Markdown : les headings (#, ##, ...) deviennent des symboles ; la première
/// ligne de texte d'une section est son rôle (`summary`).
fn extract_markdown(source: &str) -> Vec<Symbol> {
    let mut symbols: Vec<Symbol> = Vec::new();
    let mut in_fence = false;
    for (i, raw) in source.lines().enumerate() {
        let line = raw.trim_start();
        if line.starts_with("```") || line.starts_with("~~~") {
            in_fence = !in_fence;
        }
        if let Some(rest) = line.strip_prefix('#') {
            let title = rest.trim_start_matches('#').trim();
            if !title.is_empty() && title.len() <= 120 {
                symbols.push(Symbol {
                    name: title.to_string(),
                    kind: SymbolKind::Heading,
                    line: i as u32 + 1,
                    end_line: 0,
                    // Le titre brut : son nombre de `#` donne le niveau (outline, read).
                    signature: line.trim_end().to_string(),
                    tokens: tokenize_identifier(title),
                    doc: Vec::new(),
                    summary: String::new(),
                });
            }
            continue;
        }
        if let Some(last) = symbols.last_mut() {
            let t = line.trim();
            if last.summary.is_empty()
                && !in_fence
                && !t.is_empty()
                && !t.starts_with("```")
                && !t.starts_with('|')
                && !t.starts_with("<!--")
                && !t.starts_with("---")
            {
                last.summary = crate::symbol::summary_of(t.trim_start_matches(['>', '-', '*']).trim());
            }
        }
    }
    symbols
}

/// Rôle d'un fichier markdown : champ `description` du frontmatter, sinon
/// première ligne de texte (hors titres, tableaux, blocs de code).
pub fn markdown_summary(source: &str) -> String {
    let mut in_front = false;
    let mut in_fence = false;
    for (idx, raw) in source.lines().enumerate().take(300) {
        let t = raw.trim();
        if idx == 0 && t == "---" {
            in_front = true;
            continue;
        }
        if in_front {
            if t == "---" {
                in_front = false;
            } else if let Some(d) = t.strip_prefix("description:") {
                let d = d.trim().trim_matches(|c| c == '"' || c == '\'');
                if !d.is_empty() {
                    return crate::symbol::summary_of(d);
                }
            }
            continue;
        }
        if t.starts_with("```") || t.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence
            || t.is_empty()
            || t.starts_with('#')
            || t.starts_with('|')
            || t.starts_with("<!--")
            || t.starts_with("---")
            || t.starts_with("![")
        {
            continue;
        }
        return crate::symbol::summary_of(t.trim_start_matches(['>', '-', '*']).trim());
    }
    String::new()
}

// ─── Requêtes tree-sitter par langage (S-expressions) ──────────────────────────

const TS_QUERY: &str = r#"
(function_declaration name: (identifier) @name) @kind.function
(class_declaration name: (type_identifier) @name) @kind.class
(interface_declaration name: (type_identifier) @name) @kind.interface
(method_definition name: (property_identifier) @name) @kind.method
(type_alias_declaration name: (type_identifier) @name) @kind.type
(enum_declaration name: (identifier) @name) @kind.enum
(lexical_declaration (variable_declarator name: (identifier) @name value: [(arrow_function) (function_expression)])) @kind.function
(public_field_definition name: (property_identifier) @name)
"#;

const PY_QUERY: &str = r#"
(function_definition name: (identifier) @name) @kind.function
(class_definition name: (identifier) @name) @kind.class
"#;

const RUST_QUERY: &str = r#"
(function_item name: (identifier) @name) @kind.function
(struct_item name: (type_identifier) @name) @kind.struct
(enum_item name: (type_identifier) @name) @kind.enum
(trait_item name: (type_identifier) @name) @kind.interface
(impl_item type: (type_identifier) @name) @kind.class
(type_item name: (type_identifier) @name) @kind.type
(const_item name: (identifier) @name) @kind.const
"#;

const CS_QUERY: &str = r#"
(class_declaration name: (identifier) @name) @kind.class
(interface_declaration name: (identifier) @name) @kind.interface
(struct_declaration name: (identifier) @name) @kind.struct
(enum_declaration name: (identifier) @name) @kind.enum
(method_declaration name: (identifier) @name) @kind.method
"#;

// ─── Extraction des RÉFÉRENCES (graphe de relations) ──────────────────────────

/// Extrait les références sortantes d'un fichier : imports, appels, refs DB.
/// Hybride : tree-sitter (imports/calls structurés) + scan léger (patterns Supabase).
#[cfg(test)]
pub fn extract_refs(lang: Lang, source: &str) -> FileRefs {
    let tree = Grammar::of(lang).filter(|_| is_js(lang)).and_then(|g| parse(g, source.as_bytes()));
    extract_refs_with(lang, source, tree.as_ref())
}

#[cfg(test)]
fn is_js(lang: Lang) -> bool {
    matches!(lang, Lang::TypeScript | Lang::Tsx | Lang::JavaScript | Lang::Jsx)
}

/// `extract_refs` avec l'arbre JS/TS déjà analysé (`None` : pas d'arbre, ou
/// langage sans références tree-sitter).
fn extract_refs_with(lang: Lang, source: &str, tree: Option<&Tree>) -> FileRefs {
    let mut refs = FileRefs::default();
    match lang {
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript | Lang::Jsx => {
            if let Some(t) = tree {
                extract_js_refs(lang, source, t, &mut refs);
            }
            extract_db_refs(source, &mut refs); // patterns Supabase
        }
        Lang::Python => extract_py_refs(source, &mut refs),
        Lang::Rust => extract_rust_refs(source, &mut refs),
        _ => {}
    }
    dedup(&mut refs.imports);
    dedup(&mut refs.imported_names);
    dedup_calls(&mut refs.calls);
    dedup(&mut refs.db_refs);
    refs
}

fn dedup(v: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    v.retain(|s| !s.is_empty() && seen.insert(s.clone()));
}

/// Dédup des appels par (nom, ligne) — deux appels du même nom à des lignes
/// différentes sont deux occurrences distinctes (nécessaire à la granularité
/// fonction : chacune doit pouvoir être rattachée à SA fonction englobante).
fn dedup_calls(v: &mut Vec<CallRef>) {
    let mut seen = std::collections::HashSet::new();
    v.retain(|c| !c.name.is_empty() && seen.insert((c.name.clone(), c.line)));
}

/// Requête de références JS/TS (compilée une fois) : imports `import ... from
/// "spec"` + identifiants importés ; appels (`call_expression`) ; pour le JSX
/// (Tsx/Jsx), les éléments de composant en plus.
fn js_ref_query(lang: Lang) -> Option<&'static Query> {
    let jsx = matches!(lang, Lang::Tsx | Lang::Jsx);
    let g = Grammar::of(lang)?;
    let slot = match (g, jsx) {
        (Grammar::Ts, _) => 0,
        (Grammar::Tsx, _) => 1,
        (_, false) => 2,
        (_, true) => 3,
    };
    REF_QUERIES[slot]
        .get_or_init(|| {
            let q = if jsx {
                r#"
(import_statement source: (string) @import.path)
(import_specifier (identifier) @import.name)
(call_expression function: (identifier) @call)
(new_expression constructor: (identifier) @call)
(call_expression function: (member_expression property: (property_identifier) @call))
(jsx_opening_element (identifier) @call)
(jsx_self_closing_element (identifier) @call)
"#
            } else {
                r#"
(import_statement source: (string) @import.path)
(import_specifier (identifier) @import.name)
(call_expression function: (identifier) @call)
(new_expression constructor: (identifier) @call)
(call_expression function: (member_expression property: (property_identifier) @call))
"#
            };
            match Query::new(&g.language(), q) {
                Ok(q) => Some(q),
                Err(e) => {
                    // En test, signale la requête fautive.
                    if cfg!(test) {
                        eprintln!("QUERY ERROR: {:?}", e);
                    }
                    None
                }
            }
        })
        .as_ref()
}

/// Imports + appels JS/TS via tree-sitter (arbre déjà analysé).
fn extract_js_refs(lang: Lang, source: &str, tree: &Tree, refs: &mut FileRefs) {
    let bytes = source.as_bytes();
    let Some(query) = js_ref_query(lang) else { return };
    let names = query.capture_names();
    let mut cursor = QueryCursor::new();
    let it = cursor.matches(query, tree.root_node(), bytes);
    for m in it {
        for cap in m.captures {
            let cname = names[cap.index as usize];
            let text = cap.node.utf8_text(bytes).unwrap_or("").trim();
            if text.is_empty() || text.len() > 80 {
                continue;
            }
            match cname {
                "import.path" => {
                    // Retire les quotes entourant le spécificateur (string node complet).
                    let clean = text.trim_matches(|c| c == '\'' || c == '"' || c == '`');
                    refs.imports.push(clean.to_string());
                }
                "import.name" => refs.imported_names.push(text.to_string()),
                "call" => refs.calls.push(CallRef { name: text.to_string(), line: cap.node.start_position().row as u32 + 1 }),
                _ => {}
            }
        }
    }
}

/// Patterns Supabase : .from('table') / .rpc('fn') — lien code↔DB (clé pour AstroQuest).
fn extract_db_refs(source: &str, refs: &mut FileRefs) {
    for (marker, _) in [(".from(", "table"), (".rpc(", "rpc")] {
        let mut start = 0;
        while let Some(pos) = source[start..].find(marker) {
            let abs = start + pos + marker.len();
            let rest = &source[abs..];
            // Capture le contenu entre quotes simples ou doubles.
            if let Some(first) = rest.chars().next() {
                if first == '\'' || first == '"' || first == '`' {
                    if let Some(end) = rest[1..].find(first) {
                        let name = &rest[1..1 + end];
                        if !name.is_empty() && name.len() < 64 && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                            refs.db_refs.push(name.to_string());
                        }
                    }
                }
            }
            start = abs;
        }
    }
}

/// Imports Python (import x / from x import y) via scan léger.
fn extract_py_refs(source: &str, refs: &mut FileRefs) {
    for line in source.lines() {
        let l = line.trim_start();
        if let Some(rest) = l.strip_prefix("from ") {
            if let Some(idx) = rest.find(" import ") {
                refs.imports.push(rest[..idx].trim().to_string());
                for n in rest[idx + 8..].split(',') {
                    let n = n.trim().split(" as ").next().unwrap_or("").trim();
                    if !n.is_empty() && n != "*" {
                        refs.imported_names.push(n.to_string());
                    }
                }
            }
        } else if let Some(rest) = l.strip_prefix("import ") {
            for n in rest.split(',') {
                let n = n.trim().split(" as ").next().unwrap_or("").trim();
                if !n.is_empty() {
                    refs.imports.push(n.to_string());
                }
            }
        }
    }
}

/// `use` Rust (scan léger).
fn extract_rust_refs(source: &str, refs: &mut FileRefs) {
    for line in source.lines() {
        let l = line.trim_start();
        if let Some(rest) = l.strip_prefix("use ") {
            let path = rest.trim_end_matches(';').trim();
            if let Some(last) = path.rsplit("::").next() {
                let name = last.trim_matches(|c| c == '{' || c == '}' || c == ' ');
                if !name.is_empty() {
                    refs.imported_names.push(name.to_string());
                }
            }
            refs.imports.push(path.to_string());
        }
    }
}

// ─── Commentaires : en-tête de fichier + doc-comments ─────────────────────────
//
// Beaucoup de modules expliquent leur rôle en prose (souvent en français) dans un
// commentaire d'en-tête, ou en JSDoc au-dessus d'une fonction. Les noms de symboles
// seuls ne portent pas ce vocabulaire ("double du document" ≠ `CacheLocal`).
// Extraction TEXTUELLE (pas tree-sitter) : robuste, indépendante du langage, et
// suffisante pour délimiter des blocs de commentaires.

/// Plafond de lignes lues pour un doc-comment au-dessus d'un symbole.
const MAX_DOC_LINES: usize = 40;
/// Plafond de lignes parcourues pour trouver/lire l'en-tête de fichier.
const MAX_HEADER_SCAN: usize = 120;

/// Remplit `symbols[i].doc` et `.summary` ; renvoie les tokens de l'en-tête du
/// fichier et son rôle (première phrase brute).
pub fn extract_comments(lang: Lang, source: &str, symbols: &mut [Symbol]) -> (Vec<String>, String) {
    if matches!(lang, Lang::Markdown) {
        return (Vec::new(), markdown_summary(source));
    }
    let lines: Vec<&str> = source.lines().collect();
    let hash_comments = matches!(lang, Lang::Python);

    for s in symbols.iter_mut() {
        if matches!(s.kind, SymbolKind::Import) || s.line < 1 {
            continue;
        }
        let idx = s.line as usize - 1;
        let mut text = doc_above(&lines, idx, hash_comments);
        if matches!(lang, Lang::Python) {
            text.push_str(&python_docstring(&lines, idx));
        }
        if !text.is_empty() {
            s.doc = crate::symbol::text_tokens(&text);
            s.summary = crate::symbol::summary_of(&text);
        }
    }
    let header = header_text(&lines, lang);
    (crate::symbol::text_tokens(&header), crate::symbol::summary_of(&header))
}

/// Vrai si la ligne (trimmée) est une ligne de commentaire.
fn is_comment_line(t: &str, hash_comments: bool) -> bool {
    t.starts_with("//")
        || t.starts_with("/*")
        || t.starts_with('*')
        || t.starts_with("*/")
        || (hash_comments && t.starts_with('#') && !t.starts_with("#!"))
}

/// Retire les marqueurs de commentaire d'une ligne.
fn strip_comment(t: &str) -> &str {
    let t = t.trim();
    let t = t
        .trim_start_matches("/**")
        .trim_start_matches("/*")
        .trim_start_matches("//!")
        .trim_start_matches("///")
        .trim_start_matches("//")
        .trim_start_matches("*/")
        .trim_start_matches('*')
        .trim_start_matches('#');
    t.trim_end_matches("*/").trim()
}

/// Commentaire contigu juste au-dessus de la ligne `idx` (en sautant les
/// attributs/décorateurs et la ligne `export`/signature multi-ligne éventuelle).
fn doc_above(lines: &[&str], idx: usize, hash_comments: bool) -> String {
    if idx == 0 || idx > lines.len() {
        return String::new();
    }
    let mut i = idx; // on regarde lines[i-1]
                     // Saute attributs Rust (#[..]) et décorateurs (@x) collés au symbole.
    while i > 0 {
        let t = lines[i - 1].trim();
        if t.starts_with("#[") || (t.starts_with('@') && !t.starts_with("@/")) {
            i -= 1;
        } else {
            break;
        }
    }
    let mut collected: Vec<&str> = Vec::new();
    while i > 0 && collected.len() < MAX_DOC_LINES {
        let t = lines[i - 1].trim();
        if t.is_empty() || !is_comment_line(t, hash_comments) {
            break;
        }
        collected.push(strip_comment(t));
        i -= 1;
    }
    collected.reverse();
    collected.join(" ")
}

/// Docstring Python juste sous `def`/`class` (lignes idx+1..).
fn python_docstring(lines: &[&str], idx: usize) -> String {
    let mut out = String::new();
    let Some(first) = lines.get(idx + 1).map(|l| l.trim()) else { return out };
    let quote = if first.starts_with("\"\"\"") {
        "\"\"\""
    } else if first.starts_with("'''") {
        "'''"
    } else {
        return out;
    };
    let body = &first[3..];
    out.push_str(body.trim_end_matches(quote));
    if body.contains(quote) {
        return out;
    }
    for l in lines.iter().skip(idx + 2).take(MAX_DOC_LINES) {
        out.push(' ');
        out.push_str(l.trim().trim_end_matches(quote));
        if l.contains(quote) {
            break;
        }
    }
    out
}

/// En-tête de fichier : les blocs de commentaire rencontrés avant la première ligne
/// de code "réelle" (imports, directives et lignes vides sont sautés, car beaucoup
/// de fichiers placent leur explication juste après les imports).
fn header_text(lines: &[&str], lang: Lang) -> String {
    let hash_comments = matches!(lang, Lang::Python);
    let mut out: Vec<&str> = Vec::new();
    let mut in_block = false; // dans un /* ... */ multi-ligne
    let mut in_import = false; // dans un import { a, b } multi-ligne
    let mut in_docstring = false;
    for raw in lines.iter().take(MAX_HEADER_SCAN) {
        let t = raw.trim();
        if in_docstring {
            out.push(t.trim_end_matches("\"\"\""));
            if t.contains("\"\"\"") {
                in_docstring = false;
            }
            continue;
        }
        if in_block {
            out.push(strip_comment(t));
            if t.contains("*/") {
                in_block = false;
            }
            continue;
        }
        if in_import {
            if t.contains(" from ") || t.starts_with('}') || t.ends_with(';') {
                in_import = false;
            }
            continue;
        }
        if t.is_empty() || t.starts_with("/// <reference") || t.starts_with("#!") || t.starts_with("'use ") || t.starts_with("\"use ") {
            continue;
        }
        if t.starts_with("/*") {
            out.push(strip_comment(t));
            if !t.contains("*/") {
                in_block = true;
            }
            continue;
        }
        if is_comment_line(t, hash_comments) {
            out.push(strip_comment(t));
            continue;
        }
        if matches!(lang, Lang::Python) && out.is_empty() && (t.starts_with("\"\"\"") || t.starts_with("'''")) {
            let body = &t[3..];
            out.push(body.trim_end_matches("\"\"\"").trim_end_matches("'''"));
            if !(body.contains("\"\"\"") || body.contains("'''")) {
                in_docstring = true;
            }
            continue;
        }
        let is_import = t.starts_with("import ")
            || t.starts_with("import{")
            || t.starts_with("export * from")
            || (t.starts_with("export {") && t.contains(" from "))
            || t.starts_with("use ")
            || t.starts_with("from ")
            || t.starts_with("using ")
            || t.starts_with("mod ")
            || t.starts_with("extern crate")
            || t.starts_with("require(")
            || (t.starts_with("const ") && t.contains("require("));
        if is_import {
            if t.starts_with("import") && !t.contains(" from ") && !t.ends_with(';') && !t.ends_with('\'') && !t.ends_with('"') {
                in_import = true;
            }
            continue;
        }
        break; // première ligne de code réelle : fin de l'en-tête
    }
    out.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_after_imports_and_jsdoc() {
        let src = "/// <reference lib=\"webworker\" />\n// Éditeur — double du document dans un worker.\n//\n// Réencoder coûte cher.\nimport * as Y from 'yjs'\nimport {\n  a,\n} from './x'\n\n/** Compacte le board en une baseline. */\nexport async function compacterBoard() {}\n";
        let mut syms = extract_symbols(Lang::TypeScript, src);
        let (header, role) = extract_comments(Lang::TypeScript, src, &mut syms);
        assert!(header.contains(&"double".to_string()));
        assert!(header.contains(&"reencoder".to_string()));
        assert_eq!(role, "Éditeur — double du document dans un worker.");
        let s = syms.iter().find(|s| s.name == "compacterBoard").unwrap();
        assert!(s.doc.contains(&"compacte".to_string()));
        assert!(s.doc.contains(&"baseline".to_string()));
        assert_eq!(s.summary, "Compacte le board en une baseline.");
        assert_eq!(s.signature, "export async function compacterBoard()");
    }

    #[test]
    fn signatures_compactes() {
        let src = "export function f(\n  a: string,\n  b: Map<string, { x: number }>,\n): Promise<void> {\n  return\n}\nexport const g = async (x: number): Promise<number> => {\n  return x\n}\nclass C extends D {\n  async m(y: string): Promise<void> {}\n}\n";
        let syms = extract_symbols(Lang::TypeScript, src);
        let sig = |n: &str| syms.iter().find(|s| s.name == n).unwrap().signature.clone();
        assert_eq!(sig("f"), "export function f( a: string, b: Map<string, { x: number }>, ): Promise<void>");
        assert_eq!(sig("g"), "export const g = async (x: number): Promise<number>");
        assert_eq!(sig("C"), "class C extends D");
        assert_eq!(sig("m"), "async m(y: string): Promise<void>");
        let rs = extract_symbols(Lang::Rust, "#[inline]\npub fn h<'a>(s: &'a str) -> Vec<u8> {\n    vec![]\n}\n");
        assert_eq!(rs[0].signature, "pub fn h<'a>(s: &'a str) -> Vec<u8>");
        let py = extract_symbols(Lang::Python, "def k(a,\n      b) -> int:\n    return 1\n");
        assert_eq!(py[0].signature, "def k(a, b) -> int");
    }

    #[test]
    fn markdown_roles() {
        let src = "---\ndescription: Guide du stockage.\n---\n# Titre\n\nPremière ligne. Suite.\n```\n# pas un titre\n```\n## Section\n| a | b |\ntexte\n";
        let syms = extract_symbols(Lang::Markdown, src);
        // Règle historique gardée : une ligne « # … » d'un bloc de code reste un
        // titre (l'exclure a fait baisser le banc caché, voir docs/ARCHITECTURE.md §6).
        assert_eq!(syms.len(), 3);
        assert_eq!(syms[0].summary, "Première ligne.");
        let sec = syms.iter().find(|s| s.name == "Section").unwrap();
        assert_eq!(sec.signature, "## Section");
        assert_eq!(sec.summary, "texte");
        assert_eq!(markdown_summary(src), "Guide du stockage.");
    }

    #[test]
    fn ts_function_and_hook() {
        let src = r#"
export function useOfficeFile(): Result { return x; }
export class OfficeEditor {}
const handleClick = () => {};
"#;
        let syms = extract_symbols(Lang::TypeScript, src);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"useOfficeFile"));
        assert!(names.contains(&"OfficeEditor"));
        // useOfficeFile doit être détecté comme Hook
        let hook = syms.iter().find(|s| s.name == "useOfficeFile").unwrap();
        assert_eq!(hook.kind, SymbolKind::Hook);
    }

    #[test]
    fn python_class_method() {
        let src = "class Foo:\n    def bar(self):\n        pass\n";
        let syms = extract_symbols(Lang::Python, src);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Foo"));
        assert!(names.contains(&"bar"));
    }

    #[test]
    fn markdown_headings() {
        let src = "# Titre\n## Section\ntexte\n### Sous-section\n";
        let syms = extract_symbols(Lang::Markdown, src);
        assert_eq!(syms.len(), 3);
        assert_eq!(syms[0].name, "Titre");
    }

    #[test]
    fn ts_refs_imports_calls_db() {
        let src = r#"
import { useOfficeFile } from '@/hooks/office/useOfficeFile';
import OfficeEditor from './OfficeEditor';
function go() {
  const x = useOfficeFile();
  const e = new OfficeEditor(x);
  supabase.from('office_documents').select();
  supabase.rpc('wallet_recharge', {});
}
"#;
        let refs = extract_refs(Lang::TypeScript, src);
        assert!(refs.imports.contains(&"@/hooks/office/useOfficeFile".to_string()));
        assert!(refs.imported_names.contains(&"useOfficeFile".to_string()));
        assert!(refs.calls.iter().any(|c| c.name == "useOfficeFile"));
        assert!(refs.calls.iter().any(|c| c.name == "OfficeEditor"), "new X() est un appel de X");
        assert!(refs.db_refs.contains(&"office_documents".to_string()));
        assert!(refs.db_refs.contains(&"wallet_recharge".to_string()));
    }
}
