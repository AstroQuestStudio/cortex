//! Rust : références sortantes (tree-sitter) et chemins de modules.
//!
//! EXTRACTION (`extract`) — sur l'arbre déjà analysé pour les symboles :
//! - `use` complets : `pub use` / `pub(crate) use` (ré-exportations, aussi dans
//!   `FileRefs::reexports`), groupes imbriqués `a::{b::{C, D}, E}`, `self`, `*`,
//!   alias `as`, déclarations multi-lignes ; `mod x;` → import de `self::x`.
//!   Dans un module en ligne (`mod tests { use super::*; }`), `self::`/`super::`
//!   sont ramenés au module du FICHIER.
//! - appels : `f()`, `a::b::f()` → `f`, `x.f()` → `f` (méthodes de la
//!   bibliothèque standard écartées sauf sur `self`), `f::<T>()` ; `Type::new()`
//!   donne `new` ET une référence à `Type` (le modèle `CallRef` ne porte que des
//!   noms simples, comme pour C++ : le qualificatif devient sa propre référence).
//! - références de types (des « appels » au sens du graphe, comme `new X()` en
//!   JS ou les classes de base en C++) : types des signatures, champs, motifs
//!   et expressions de structure, tête de chemin `SimEvent::Hit`, constantes en
//!   CAPITALES. Les types de la bibliothèque standard sont écartés.
//! - macros : leurs arguments ne sont pas analysés par la grammaire ; on y
//!   repère `f(`, `x.f(`, `Type::` dans les jetons. Le nom de la macro est ignoré.
//!
//! CHEMINS (`candidates`, `crate_key`) — purement à partir des CHEMINS de
//! fichiers (aucune lecture de Cargo.toml) : c'est ce qui permet à la mise à
//! jour incrémentale de rester exacte (voir `graph.rs`).
//! - `crate::a::b` : racine = dossier `src` le plus profond du chemin du
//!   fichier ; sans `src` (ex. `crates/core/main.rs` de ripgrep), chacun des
//!   dossiers parents, du plus profond au plus haut.
//! - `self::`, `super::` : module du fichier (`a/b.rs` → `a/b`, `a/mod.rs` → `a`).
//! - `nom::…` : d'abord un module enfant (chemins uniformes de l'édition 2018),
//!   puis une crate du dépôt : clé `rs:<crate>/<module>` d'un fichier
//!   `<dossier>/src/<module>.rs`, où `<crate>` est le nom du dossier (`-` → `_`).
//!   Variantes essayées : le nom, et ce qui suit son premier `_`
//!   (`grep_searcher` → dossier `searcher`).
//! - module `Q` : fichiers `Q.rs`, `Q/mod.rs` (et `lib.rs`/`main.rs` à la racine).
//!   Le plus long préfixe de chemin qui existe gagne ; le segment restant est
//!   l'élément importé (pour suivre une ré-exportation).

use crate::symbol::{CallRef, FileRefs};
use tree_sitter::{Node, Tree};

/// Types et traits de la bibliothèque standard (et du prélude) : jamais des
/// références utiles au graphe — un projet qui définit son propre `Result`
/// serait relié à chaque signature.
const STD_TYPES: &[&str] = &[
    "String",
    "Vec",
    "Option",
    "Result",
    "Box",
    "Rc",
    "Arc",
    "Weak",
    "RefCell",
    "Cell",
    "Mutex",
    "RwLock",
    "HashMap",
    "HashSet",
    "BTreeMap",
    "BTreeSet",
    "VecDeque",
    "BinaryHeap",
    "Path",
    "PathBuf",
    "Cow",
    "Self",
    "Some",
    "None",
    "Ok",
    "Err",
    "Clone",
    "Copy",
    "Debug",
    "Default",
    "Display",
    "PartialEq",
    "Eq",
    "Hash",
    "Hasher",
    "Ord",
    "PartialOrd",
    "Ordering",
    "Send",
    "Sync",
    "Sized",
    "Fn",
    "FnMut",
    "FnOnce",
    "Iterator",
    "IntoIterator",
    "From",
    "Into",
    "TryFrom",
    "TryInto",
    "AsRef",
    "AsMut",
    "Deref",
    "DerefMut",
    "Drop",
    "Error",
    "Formatter",
    "Duration",
    "Instant",
    "SystemTime",
    "PhantomData",
    "OsStr",
    "OsString",
    "Any",
    "ToString",
    "FromStr",
    "Borrow",
    "Pin",
    "Future",
];

/// Méthodes de la bibliothèque standard, trop génériques pour un appel `x.f()`
/// (gardées sur `self.f()`, qui vise le type courant).
const STD_METHODS: &[&str] = &[
    "clone",
    "cloned",
    "copied",
    "to_string",
    "to_owned",
    "into",
    "try_into",
    "iter",
    "iter_mut",
    "into_iter",
    "map",
    "map_err",
    "and_then",
    "or_else",
    "unwrap",
    "unwrap_or",
    "unwrap_or_else",
    "unwrap_or_default",
    "expect",
    "ok",
    "err",
    "ok_or",
    "ok_or_else",
    "is_some",
    "is_none",
    "is_ok",
    "is_err",
    "as_ref",
    "as_mut",
    "as_str",
    "as_slice",
    "as_bytes",
    "as_deref",
    "as_path",
    "len",
    "is_empty",
    "push",
    "push_str",
    "pop",
    "insert",
    "remove",
    "get",
    "get_mut",
    "contains",
    "contains_key",
    "extend",
    "collect",
    "filter",
    "filter_map",
    "flat_map",
    "find",
    "find_map",
    "position",
    "any",
    "all",
    "fold",
    "sum",
    "count",
    "max",
    "min",
    "max_by_key",
    "min_by_key",
    "rev",
    "enumerate",
    "zip",
    "chain",
    "take",
    "skip",
    "next",
    "last",
    "first",
    "sort",
    "sort_by",
    "sort_by_key",
    "sort_unstable",
    "sort_unstable_by",
    "sort_unstable_by_key",
    "dedup",
    "retain",
    "truncate",
    "clear",
    "join",
    "split",
    "splitn",
    "rsplit",
    "split_whitespace",
    "lines",
    "chars",
    "bytes",
    "trim",
    "trim_start",
    "trim_end",
    "starts_with",
    "ends_with",
    "strip_prefix",
    "strip_suffix",
    "to_lowercase",
    "to_uppercase",
    "to_ascii_lowercase",
    "replace",
    "parse",
    "lock",
    "borrow",
    "borrow_mut",
    "write",
    "read",
    "flush",
    "send",
    "recv",
    "entry",
    "or_insert",
    "or_insert_with",
    "or_default",
    "keys",
    "values",
    "values_mut",
    "cmp",
    "partial_cmp",
    "eq",
    "ne",
    "hash",
    "fmt",
    "with_capacity",
    "capacity",
    "reserve",
    "drain",
    "windows",
    "chunks",
    "copied",
    "abs",
    "powi",
    "sqrt",
    "floor",
    "ceil",
    "round",
    "clamp",
    "to_vec",
    "into_boxed_slice",
    "unwrap_err",
    "take_while",
    "skip_while",
    "peekable",
    "step_by",
    "display",
    "to_str",
    "to_string_lossy",
    "exists",
    "is_file",
    "is_dir",
    "file_name",
    "extension",
    "parent",
];

/// Appels qui sont des constructeurs du prélude (`Some(x)`, `Ok(x)`…).
const PRELUDE_CALLS: &[&str] = &["Some", "None", "Ok", "Err", "Box", "Vec", "String", "drop"];

/// Profondeur maximale de suivi des ré-exportations (`pub use`) en chaîne.
pub const MAX_REEXPORT_DEPTH: u32 = 3;

fn txt<'a>(n: Node, src: &'a [u8]) -> &'a str {
    n.utf8_text(src).unwrap_or("")
}

fn line_of(n: Node) -> u32 {
    n.start_position().row as u32 + 1
}

fn upper_first(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

/// Constante en CAPITALES (`GRID_CELL`, `MAX2`) : au moins deux caractères, une lettre.
fn screaming(s: &str) -> bool {
    s.len() >= 2
        && s.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        && s.bytes().any(|b| b.is_ascii_uppercase())
}

/// Nom de type référencé (hors bibliothèque standard, hors paramètres d'une lettre).
fn useful_type(s: &str) -> bool {
    s.len() >= 2 && s.len() <= 80 && !STD_TYPES.contains(&s)
}

/// Un élément importé par un `use` : chemin (segments), alias, import global.
struct UseItem {
    path: Vec<String>,
    alias: Option<String>,
    glob: bool,
}

/// Segments d'un chemin (`crate::a::b`, `::x` → `x`).
fn segments(n: Node, src: &[u8]) -> Vec<String> {
    txt(n, src).split("::").map(|s| s.split_whitespace().collect::<String>()).filter(|s| !s.is_empty()).collect()
}

/// Déplie l'arbre d'un `use` en éléments importés.
fn use_tree(n: Node, src: &[u8], prefix: &[String], out: &mut Vec<UseItem>) {
    match n.kind() {
        "use_as_clause" => {
            let mut path = prefix.to_vec();
            if let Some(p) = n.child_by_field_name("path") {
                path.extend(segments(p, src));
            }
            let alias = n.child_by_field_name("alias").map(|a| txt(a, src).to_string());
            out.push(UseItem { path, alias, glob: false });
        }
        "scoped_use_list" => {
            let mut p = prefix.to_vec();
            if let Some(path) = n.child_by_field_name("path") {
                p.extend(segments(path, src));
            }
            if let Some(list) = n.child_by_field_name("list") {
                use_tree(list, src, &p, out);
            }
        }
        "use_list" => {
            let mut c = n.walk();
            for ch in n.named_children(&mut c) {
                use_tree(ch, src, prefix, out);
            }
        }
        "use_wildcard" => {
            let mut path = prefix.to_vec();
            let mut c = n.walk();
            if let Some(ch) = n.named_children(&mut c).next() {
                path.extend(segments(ch, src));
            }
            out.push(UseItem { path, alias: None, glob: true });
        }
        "scoped_identifier" | "identifier" | "crate" | "self" | "super" | "metavariable" => {
            let mut path = prefix.to_vec();
            path.extend(segments(n, src));
            // `use a::{self, X}` : `self` désigne le module `a` lui-même.
            if path.len() > 1 && path.last().is_some_and(|s| s == "self") {
                path.pop();
            }
            out.push(UseItem { path, alias: None, glob: false });
        }
        _ => {}
    }
}

/// Ramène un chemin écrit dans des modules EN LIGNE (`inline`, du plus externe
/// au plus interne) au module du fichier : `super::x` dans `mod tests` → `self::x`.
fn normalize(path: Vec<String>, inline: &[String]) -> Vec<String> {
    if inline.is_empty() {
        return path;
    }
    let mut depth = inline.len();
    let mut i = 0;
    match path.first().map(String::as_str) {
        Some("self") => i = 1,
        Some("super") => {
            while path.get(i).is_some_and(|s| s == "super") && depth > 0 {
                depth -= 1;
                i += 1;
            }
            if path.get(i).is_some_and(|s| s == "super") {
                // Remonte au-delà du fichier : chemin `super::` ordinaire.
                return path[i..].to_vec();
            }
        }
        _ => return path, // `crate::`, crate externe ou chemin uniforme : inchangé.
    }
    let mut out = vec!["self".to_string()];
    out.extend(inline[..depth].iter().cloned());
    out.extend(path[i..].iter().cloned());
    out
}

struct Walker<'s> {
    src: &'s [u8],
    refs: &'s mut FileRefs,
    inline: Vec<String>,
}

impl Walker<'_> {
    fn push(&mut self, name: &str, n: Node) {
        if !name.is_empty() && name.len() <= 80 {
            self.refs.calls.push(CallRef { name: name.to_string(), line: line_of(n) });
        }
    }

    fn type_ref(&mut self, n: Node) {
        let name = txt(n, self.src);
        if useful_type(name) {
            self.push(name, n);
        }
    }

    fn use_decl(&mut self, n: Node) {
        let Some(arg) = n.child_by_field_name("argument") else { return };
        let public = {
            let mut c = n.walk();
            let v = n.children(&mut c).any(|ch| ch.kind() == "visibility_modifier");
            v
        };
        let mut items = Vec::new();
        use_tree(arg, self.src, &[], &mut items);
        for it in items {
            let path = normalize(it.path, &self.inline);
            if path.is_empty() {
                continue;
            }
            let mut spec = path.join("::");
            if it.glob {
                spec.push_str("::*");
            }
            let last = path.last().map(String::as_str).unwrap_or("");
            if !it.glob && !matches!(last, "self" | "super" | "crate") {
                self.refs.imported_names.push(last.to_string());
            }
            if let Some(a) = &it.alias {
                if a != "_" {
                    self.refs.imported_names.push(a.clone());
                }
            }
            if public {
                self.refs.reexports.push(match &it.alias {
                    Some(a) if a != "_" => format!("{} as {}", spec, a),
                    _ => spec.clone(),
                });
            }
            self.refs.imports.push(spec);
        }
    }

    /// Nom appelé en position « fonction » d'un `call_expression`.
    fn callee(&mut self, f: Node) {
        match f.kind() {
            "identifier" => {
                let name = txt(f, self.src);
                if !PRELUDE_CALLS.contains(&name) {
                    self.push(name, f);
                }
            }
            "scoped_identifier" => {
                if let Some(name) = f.child_by_field_name("name") {
                    // `Enum::Variant(x)` : une variante, pas un appel (et la clé de
                    // résolution, en minuscules, la confondrait avec `fn variant`).
                    // Le type, lui, est vu par `scoped`.
                    let n = txt(name, self.src);
                    if name.kind() == "identifier" && !(upper_first(n) && scoped_head_is_type(f, self.src)) {
                        self.push(n, name);
                    }
                }
            }
            "field_expression" => {
                let Some(field) = f.child_by_field_name("field") else { return };
                if field.kind() != "field_identifier" {
                    return;
                }
                let name = txt(field, self.src);
                let on_self = f.child_by_field_name("value").is_some_and(|v| v.kind() == "self");
                if on_self || !STD_METHODS.contains(&name) {
                    self.push(name, field);
                }
            }
            "generic_function" => {
                if let Some(inner) = f.child_by_field_name("function") {
                    self.callee(inner);
                }
            }
            _ => {}
        }
    }

    /// Chemin `a::b::c` (expression, motif, type) : si son avant-dernier segment
    /// est un type (`SimEvent::Hit`, `Foo::new`), c'est LUI la référence (le
    /// dernier est une variante ou un élément associé) ; sinon (`world::World`,
    /// `config::MAX`), le dernier segment s'il a l'allure d'un type ou d'une constante.
    fn scoped(&mut self, n: Node, name_is_call: bool) {
        if let Some(h) = scoped_head(n) {
            if upper_first(txt(h, self.src)) {
                self.type_ref(h);
                return;
            }
        }
        if !name_is_call {
            if let Some(nm) = n.child_by_field_name("name") {
                let t = txt(nm, self.src);
                if upper_first(t) {
                    self.type_ref(nm);
                }
            }
        }
    }

    /// Jetons d'une macro : `f(`, `x.f(`, `Type::`, CONSTANTES (récursif).
    fn token_tree(&mut self, tt: Node) {
        let mut c = tt.walk();
        let kids: Vec<Node> = tt.children(&mut c).collect();
        for (i, k) in kids.iter().enumerate() {
            match k.kind() {
                "token_tree" => self.token_tree(*k),
                "identifier" => {
                    let name = txt(*k, self.src);
                    let next = kids.get(i + 1);
                    let prev = if i > 0 { kids.get(i - 1) } else { None };
                    if next.is_some_and(|n| n.kind() == "token_tree" && txt(*n, self.src).starts_with('(')) {
                        let prev_kind = prev.map(|p| p.kind()).unwrap_or("");
                        let keep = match prev_kind {
                            // `x.f(` : méthode ; `self.f(` toujours gardé.
                            "." => (i >= 2 && kids[i - 2].kind() == "self") || !STD_METHODS.contains(&name),
                            // `a::f(` / `Type::new(` : appel qualifié (le type est vu à part) ;
                            // `Enum::Variant(` : variante, écartée.
                            "::" => !(upper_first(name) && i >= 2 && upper_first(txt(kids[i - 2], self.src))),
                            _ => !PRELUDE_CALLS.contains(&name),
                        };
                        if keep {
                            self.push(name, *k);
                        }
                    } else if next.is_some_and(|n| n.kind() == "::") && upper_first(name) {
                        self.type_ref(*k);
                    } else if screaming(name) {
                        self.push(name, *k);
                    }
                }
                _ => {}
            }
        }
    }

    fn visit(&mut self, root: Node) {
        // Parcours en profondeur explicite (pas de récursion sur l'arbre entier) ;
        // `None` marque la sortie d'un module en ligne.
        let mut stack: Vec<Option<Node>> = vec![Some(root)];
        while let Some(item) = stack.pop() {
            let Some(n) = item else {
                self.inline.pop();
                continue;
            };
            let mut descend = true;
            match n.kind() {
                "use_declaration" => {
                    self.use_decl(n);
                    descend = false;
                }
                "macro_definition"
                | "attribute_item"
                | "inner_attribute_item"
                | "line_comment"
                | "block_comment"
                | "extern_crate_declaration"
                | "string_literal"
                | "raw_string_literal" => descend = false,
                "mod_item" => {
                    let name = n.child_by_field_name("name").map(|x| txt(x, self.src).to_string()).unwrap_or_default();
                    match n.child_by_field_name("body") {
                        None => {
                            if !name.is_empty() {
                                let mut p = vec!["self".to_string()];
                                p.extend(self.inline.iter().cloned());
                                p.push(name);
                                self.refs.imports.push(p.join("::"));
                            }
                        }
                        Some(body) => {
                            self.inline.push(name);
                            stack.push(None);
                            stack.push(Some(body));
                        }
                    }
                    descend = false;
                }
                "macro_invocation" => {
                    let mut c = n.walk();
                    let tts: Vec<Node> = n.children(&mut c).filter(|k| k.kind() == "token_tree").collect();
                    for tt in tts {
                        self.token_tree(tt);
                    }
                    descend = false;
                }
                "call_expression" => {
                    if let Some(f) = n.child_by_field_name("function") {
                        self.callee(f);
                        if f.kind() == "scoped_identifier" {
                            self.scoped(f, true);
                            // Le chemin a été traité : ne redescendre que dans les arguments.
                            if let Some(a) = n.child_by_field_name("arguments") {
                                stack.push(Some(a));
                            }
                            if let Some(p) = f.child_by_field_name("path") {
                                if p.kind() == "generic_type" {
                                    stack.push(Some(p));
                                }
                            }
                            descend = false;
                        }
                    }
                }
                "scoped_identifier" | "scoped_type_identifier" => {
                    self.scoped(n, false);
                    // Seuls les arguments de type d'un chemin générique restent à voir.
                    if let Some(p) = n.child_by_field_name("path") {
                        if p.kind() == "generic_type" {
                            if let Some(ta) = p.child_by_field_name("type_arguments") {
                                stack.push(Some(ta));
                            }
                        }
                    }
                    descend = false;
                }
                "type_identifier" => {
                    if !is_definition_name(n) {
                        self.type_ref(n);
                    }
                }
                "identifier" => {
                    let t = txt(n, self.src);
                    if screaming(t) && !is_definition_name(n) {
                        self.push(t, n);
                    }
                }
                "tuple_struct_pattern" => {
                    if let Some(t) = n.child_by_field_name("type") {
                        if t.kind() == "identifier" && upper_first(txt(t, self.src)) {
                            self.type_ref(t);
                        }
                    }
                }
                "impl_item" => {
                    // Le type implémenté (`impl Foo`, `impl Trait for Foo`) n'est pas une
                    // référence : le bloc `impl` porte déjà son nom. Le trait, si.
                    let ty = n.child_by_field_name("type").map(|t| t.id());
                    let mut c = n.walk();
                    let kids: Vec<Node> = n.children(&mut c).filter(|k| Some(k.id()) != ty).collect();
                    for k in kids.into_iter().rev() {
                        stack.push(Some(k));
                    }
                    descend = false;
                }
                _ => {}
            }
            if descend {
                let mut c = n.walk();
                let kids: Vec<Node> = n.children(&mut c).collect();
                for k in kids.into_iter().rev() {
                    stack.push(Some(k));
                }
            }
        }
    }
}

/// Dernier segment du chemin qualifiant d'un chemin `a::b::c` : `a::B` → `B`,
/// `Foo<T>` → `Foo` (`None` : pas de chemin, ou `crate`/`super`…).
fn scoped_head(n: Node) -> Option<Node> {
    let mut head = n.child_by_field_name("path");
    while let Some(h) = head {
        head = match h.kind() {
            "identifier" | "type_identifier" => break,
            "scoped_identifier" | "scoped_type_identifier" => h.child_by_field_name("name"),
            "generic_type" => h.child_by_field_name("type"),
            _ => None,
        };
    }
    head
}

/// Le chemin qualifiant d'un `scoped_identifier` finit-il par un type
/// (`Enum::…`, `a::Foo::…`, `Self::…`) ?
fn scoped_head_is_type(n: Node, src: &[u8]) -> bool {
    scoped_head(n).is_some_and(|h| upper_first(txt(h, src)))
}

/// Le nœud est-il le NOM d'une définition (structure, énumération, trait,
/// alias, constante, paramètre de type) plutôt qu'une référence ?
fn is_definition_name(n: Node) -> bool {
    let Some(p) = n.parent() else { return false };
    match p.kind() {
        "struct_item"
        | "enum_item"
        | "trait_item"
        | "type_item"
        | "union_item"
        | "const_item"
        | "static_item"
        | "associated_type"
        | "enum_variant"
        | "function_item"
        | "function_signature_item"
        | "macro_definition" => p.child_by_field_name("name").is_some_and(|x| x.id() == n.id()),
        "type_parameters" | "constrained_type_parameter" | "optional_type_parameter" => true,
        _ => false,
    }
}

/// Références d'un fichier Rust (arbre déjà analysé). Les noms appelés sont
/// aussi « importés » : en Rust, une méthode ou un chemin qualifié n'ont pas
/// besoin d'un `use` (même convention que C/C++, Lua, GLSL).
pub fn extract(source: &str, tree: &Tree, refs: &mut FileRefs) {
    let mut w = Walker { src: source.as_bytes(), refs, inline: Vec::new() };
    w.visit(tree.root_node());
    let called: Vec<String> = refs.calls.iter().map(|c| c.name.clone()).collect();
    refs.imported_names.extend(called);
}

// ─── Chemins de modules ─────────────────────────────────────────────────────

/// Un spécificateur d'import Rust (chemin `use` ou `mod`) ?
pub fn is_rust_spec(spec: &str) -> bool {
    spec.contains("::")
}

fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

fn join(a: &str, b: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (true, _) => b.to_string(),
        (_, true) => a.to_string(),
        _ => format!("{}/{}", a, b),
    }
}

/// Dossier « module » d'un fichier : `a/b.rs` → `a/b` ; `a/mod.rs`, `a/lib.rs`,
/// `a/main.rs` → `a`.
fn module_dir(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    if matches!(name, "mod.rs" | "lib.rs" | "main.rs") {
        dir_of(path).to_string()
    } else {
        path.strip_suffix(".rs").unwrap_or(path).to_string()
    }
}

/// Racine `src` de la crate d'un fichier (le dossier `src` le plus profond).
fn src_root(path: &str) -> Option<String> {
    let parts: Vec<&str> = path.split('/').collect();
    let last_dir = parts.len().saturating_sub(1);
    (0..last_dir).rev().find(|&i| parts[i] == "src").map(|i| parts[..=i].join("/"))
}

/// Clé de crate d'un fichier Rust `<dossier>/src/<module>.rs` :
/// `rs:<nom du dossier, - → _>/<module>` (`crates/game_sim/src/world.rs` →
/// `rs:game_sim/world`). Indexée comme une « tige » de chemin : un `use
/// game_sim::world::X` d'une autre crate la retrouve sans lire Cargo.toml.
pub fn crate_key(path: &str) -> Option<String> {
    let rel_stem = path.strip_suffix(".rs")?;
    let root = src_root(path)?;
    let crate_dir = dir_of(&root);
    let name = crate_dir.rsplit('/').next().filter(|s| !s.is_empty())?;
    let rest = rel_stem.strip_prefix(&root)?.trim_start_matches('/');
    Some(format!("rs:{}/{}", name.replace('-', "_"), rest))
}

/// Candidats d'un module `base/segs…` : du plus long préfixe au plus court,
/// chaque niveau `Q` → (`Q`, `Q/mod`), puis le fichier racine du dossier
/// (`roots`). Chaque candidat porte l'élément restant s'il en reste UN.
fn module_levels(base: &str, segs: &[String], roots: &[&str], out: &mut Vec<(String, Option<String>)>) {
    for j in (1..=segs.len()).rev() {
        let q = join(base, &segs[..j].join("/"));
        let item = if segs.len() - j == 1 { Some(segs[j].clone()) } else { None };
        out.push((q.clone(), item.clone()));
        out.push((format!("{}/mod", q), item));
    }
    let item = if segs.len() == 1 { Some(segs[0].clone()) } else { None };
    for r in roots {
        out.push((join(base, r), item.clone()));
    }
}

/// Variantes de dossier d'un nom de crate : le nom, et la partie qui suit son
/// premier `_` (`grep_searcher` → `searcher`).
fn crate_variants(name: &str) -> Vec<String> {
    let mut v = vec![name.to_string()];
    if let Some((_, rest)) = name.split_once('_') {
        if !rest.is_empty() {
            v.push(rest.to_string());
        }
    }
    v
}

/// Candidats (tige de fichier, élément restant) d'un spécificateur Rust écrit
/// dans le fichier `from_path`, par priorité : le premier qui existe gagne.
/// Purement fonction des deux chaînes (aucun accès au projet).
pub fn candidates(from_path: &str, spec: &str) -> Vec<(String, Option<String>)> {
    let mut segs: Vec<String> = spec.split("::").filter(|s| !s.is_empty()).map(str::to_string).collect();
    let glob = segs.last().is_some_and(|s| s == "*");
    if glob {
        segs.pop();
    }
    let mut out = Vec::new();
    let Some(first) = segs.first().cloned() else { return out };
    let finish = |mut out: Vec<(String, Option<String>)>| {
        if glob {
            // `use a::b::*` : le module `a::b` lui-même ; tout ce qu'il ré-exporte.
            for c in out.iter_mut() {
                c.1 = if c.0.ends_with("/lib") || c.0.ends_with("/main") || c.1.is_none() { Some("*".to_string()) } else { None };
            }
        }
        out
    };
    let module_roots: &[&str] = &["lib", "main"];
    match first.as_str() {
        "std" | "core" | "alloc" | "proc_macro" | "test" => {}
        "crate" => {
            let rest = &segs[1..];
            match src_root(from_path) {
                Some(r) => module_levels(&r, rest, module_roots, &mut out),
                None => {
                    // Sans `src` : chaque dossier parent peut être la racine.
                    let mut d = dir_of(from_path).to_string();
                    loop {
                        module_levels(&d, rest, module_roots, &mut out);
                        if d.is_empty() {
                            break;
                        }
                        d = dir_of(&d).to_string();
                    }
                }
            }
        }
        "self" | "super" => {
            let mut d = module_dir(from_path);
            let mut i = 0;
            if first == "self" {
                i = 1;
            }
            while segs.get(i).is_some_and(|s| s == "super") {
                d = dir_of(&d).to_string();
                i += 1;
            }
            let rest = &segs[i..];
            // Le module visé lui-même : `d.rs`, `d/mod.rs`, ou racine `d/lib.rs`.
            module_levels(&d, rest, &[], &mut out);
            let item = if rest.len() == 1 { Some(rest[0].clone()) } else { None };
            if !d.is_empty() {
                out.push((d.clone(), item.clone()));
            }
            for r in ["mod", "lib", "main"] {
                out.push((join(&d, r), item.clone()));
            }
        }
        _ => {
            // (1) module enfant du module courant (chemins uniformes) ;
            let m = module_dir(from_path);
            module_levels(&m, &segs, &[], &mut out);
            // (2) crate du dépôt, par sa clé `rs:<crate>/…`.
            let rest = &segs[1..];
            for v in crate_variants(&first) {
                module_levels(&format!("rs:{}", v), rest, &["lib"], &mut out);
            }
        }
    }
    finish(out)
}

/// Une ré-exportation stockée (`a::b::X` ou `a::b::X as Y`) : (spécificateur,
/// nom exporté — `*` pour un `pub use a::*`, nom de l'élément d'origine).
pub fn parse_reexport(r: &str) -> (&str, &str, &str) {
    let (spec, alias) = match r.split_once(" as ") {
        Some((s, a)) => (s, Some(a)),
        None => (r, None),
    };
    let last = spec.rsplit("::").next().unwrap_or(spec);
    (spec, alias.unwrap_or(last), last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::extract_refs;
    use crate::lang::Lang;

    fn calls(src: &str) -> Vec<String> {
        extract_refs(Lang::Rust, src).calls.into_iter().map(|c| c.name).collect()
    }

    #[test]
    fn use_complets() {
        let src = r#"
use std::collections::HashMap;
pub use world::{SimEvent, World as Monde};
pub(crate) use crate::combat::apply_damage;
use crate::{
    command::{self, Command},
    net::*,
};
use super::super::x::Y;
mod combat;
mod tests {
    use super::*;
    use super::helper as h;
}
"#;
        let r = extract_refs(Lang::Rust, src);
        for s in [
            "std::collections::HashMap",
            "world::SimEvent",
            "world::World",
            "crate::combat::apply_damage",
            "crate::command",
            "crate::command::Command",
            "crate::net::*",
            "super::super::x::Y",
            "self::combat",
            "self::*",
            "self::helper",
        ] {
            assert!(r.imports.contains(&s.to_string()), "import manquant {s} : {:?}", r.imports);
        }
        for n in ["HashMap", "SimEvent", "World", "Monde", "apply_damage", "command", "Command", "Y", "helper", "h"] {
            assert!(r.imported_names.contains(&n.to_string()), "nom manquant {n} : {:?}", r.imported_names);
        }
        assert_eq!(r.reexports, vec!["world::SimEvent", "world::World as Monde", "crate::combat::apply_damage"]);
    }

    #[test]
    fn appels_methodes_types() {
        let src = r#"
fn run(w: &mut World, ev: SimEvent) -> Result<Ship, CommandError> {
    let s = Ship::new(3);
    apply_damage(&mut s, 2.0);
    crate::combat::fire_weapons(w);
    w.take_control(1);
    w.ships.iter().map(|x| x.clone()).collect::<Vec<_>>();
    self.len();
    match ev { SimEvent::Hit { .. } => {}, SimEvent::Boom(x) => {} }
    let p = Projectile { speed: MAX_SPEED };
    assert_eq!(compute(w), helper::value());
    println!("{}", w.describe());
    let i = Intention::Impact(c);
    vec![Mode::Search(1)];
}
"#;
        let c = calls(src);
        for n in [
            "World",
            "SimEvent",
            "Ship",
            "CommandError",
            "new",
            "apply_damage",
            "fire_weapons",
            "take_control",
            "len",
            "Projectile",
            "MAX_SPEED",
            "compute",
            "value",
            "describe",
            "Intention",
            "Mode",
        ] {
            assert!(c.contains(&n.to_string()), "appel manquant {n} : {c:?}");
        }
        for n in [
            "iter",
            "map",
            "clone",
            "collect",
            "Vec",
            "Result",
            "Hit",
            "Boom",
            "println",
            "assert_eq",
            "combat",
            "crate",
            "Impact",
            "Search",
        ] {
            assert!(!c.contains(&n.to_string()), "faux appel {n} : {c:?}");
        }
    }

    #[test]
    fn definitions_et_impl_ne_sont_pas_des_references() {
        let src = "pub struct Foo<T> { x: Bar }\nimpl<T> Foo<T> { fn f(&self) -> Baz { todo!() } }\nimpl Show for Foo<u8> {}\nconst LIMIT: u32 = 3;";
        let c = calls(src);
        assert!(!c.contains(&"Foo".to_string()), "{c:?}");
        assert!(!c.contains(&"LIMIT".to_string()), "{c:?}");
        assert!(!c.contains(&"T".to_string()), "{c:?}");
        for n in ["Bar", "Baz", "Show"] {
            assert!(c.contains(&n.to_string()), "{n} : {c:?}");
        }
    }

    #[test]
    fn candidats_de_chemins() {
        let c = |from: &str, spec: &str| candidates(from, spec).into_iter().map(|(s, _)| s).collect::<Vec<_>>();
        let v = c("crates/game_sim/src/command.rs", "crate::combat::apply_damage");
        assert_eq!(v[0], "crates/game_sim/src/combat/apply_damage");
        assert!(v.contains(&"crates/game_sim/src/combat".to_string()));
        assert!(v.contains(&"crates/game_sim/src/lib".to_string()));
        let v = c("crates/game_net/src/msg.rs", "game_sim::SimEvent");
        assert!(v.contains(&"rs:game_sim/lib".to_string()), "{v:?}");
        assert!(v.contains(&"rs:sim/lib".to_string()), "{v:?}");
        let v = c("src/a/b.rs", "super::c::D");
        assert_eq!(v[0], "src/a/c/D");
        assert!(v.contains(&"src/a/c".to_string()));
        let v = c("src/a/mod.rs", "self::inner");
        assert_eq!(v[0], "src/a/inner");
        assert!(c("src/lib.rs", "std::fmt::Debug").is_empty());
        // Sans dossier `src` : chaque parent est une racine possible (ripgrep `crates/core`).
        let v = c("crates/core/flags/defs.rs", "crate::flags::lowargs::Mode");
        assert!(v.contains(&"crates/core/flags/lowargs".to_string()), "{v:?}");
        assert_eq!(crate_key("crates/game-sim/src/world.rs").as_deref(), Some("rs:game_sim/world"));
        assert_eq!(crate_key("crates/game_sim/src/lib.rs").as_deref(), Some("rs:game_sim/lib"));
        assert_eq!(crate_key("crates/core/main.rs"), None);
        assert_eq!(parse_reexport("world::World as Monde"), ("world::World", "Monde", "World"));
        assert_eq!(parse_reexport("a::*"), ("a::*", "*", "*"));
    }
}
