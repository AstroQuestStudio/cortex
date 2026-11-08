//! Lua (5.1 de Recoil / Beyond All Reason compris) : symboles, appels, includes.
//!
//! Grammaire tree-sitter-lua. Noms de symboles pensés pour la recherche :
//!
//! - `function widget:DrawWorld()` → méthode `widget:DrawWorld` (idem `gadget:GameFrame`) ;
//! - `function M.foo()`, `local function foo()` → `M.foo`, `foo` ;
//! - `M.foo = function() end`, `local foo = function() end` → fonction `M.foo` / `foo` ;
//! - `local M = { run = function() end }` → `M.run` ;
//! - `return { armck = { ... } }` (tables de définitions `units/*.lua`) → un symbole
//!   `armck` par entrée de premier niveau ; `return function`/`return { f = function }` idem ;
//! - gros `local cache = { ... }` de premier niveau → constante.
//!
//! La clé de résolution des appels est le dernier segment (`graph::call_key`) :
//! `M.foo()`, `obj:foo()` et `foo()` visent tous `foo`. Les tables de l'API moteur et
//! de la bibliothèque standard (`Spring.`, `gl.`, `math.`…) ne sont pas des appels du
//! projet : ignorées. Includes : `VFS.Include("luaui/x.lua")`, `include("x.lua")`,
//! `require("a.b")` → spécificateurs de projet résolubles par le graphe.

use crate::symbol::{tokenize_identifier, CallRef, FileRefs, Symbol, SymbolKind};
use tree_sitter::{Node, Tree};

/// Tables dont les fonctions ne sont jamais des symboles du projet (API Recoil/Spring et
/// bibliothèque standard Lua) : leurs appels ne sont pas enregistrés.
const ENGINE_TABLES: &[&str] = &[
    "Spring",
    "gl",
    "GL",
    "VFS",
    "Script",
    "Game",
    "Engine",
    "math",
    "string",
    "table",
    "os",
    "io",
    "coroutine",
    "debug",
    "package",
    "CMD",
    "LOG",
    "Platform",
    "UnitDefs",
    "WeaponDefs",
    "FeatureDefs",
    "UnitDefNames",
    "WeaponDefNames",
    "FeatureDefNames",
];

/// Fonctions globales du langage, jamais des symboles du projet.
const LUA_BUILTINS: &[&str] = &[
    "pairs",
    "ipairs",
    "type",
    "tostring",
    "tonumber",
    "select",
    "next",
    "print",
    "setmetatable",
    "getmetatable",
    "rawget",
    "rawset",
    "rawequal",
    "unpack",
    "assert",
    "error",
    "pcall",
    "xpcall",
    "loadstring",
    "load",
    "collectgarbage",
    "setfenv",
    "getfenv",
    "newproxy",
    "tointeger",
];

fn txt<'a>(n: Node, src: &'a [u8]) -> &'a str {
    n.utf8_text(src).unwrap_or("")
}

/// Texte d'un nœud sans espaces (`M . foo` → `M.foo`).
fn compact(n: Node, src: &[u8]) -> String {
    txt(n, src).chars().filter(|c| !c.is_whitespace()).collect()
}

fn first_named<'a>(n: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut c = n.walk();
    let r = n.named_children(&mut c).find(|x| x.kind() == kind);
    r
}

/// Racine (`Spring` dans `Spring.Utilities.Foo`) d'un nom pointé.
fn root_of(name: &str) -> &str {
    name.split(['.', ':']).next().unwrap_or(name)
}

/// Contenu d'un littéral chaîne Lua (`"x"`, `'x'`, `[[x]]`).
fn string_value(n: Node, src: &[u8]) -> Option<String> {
    if n.kind() != "string" {
        return None;
    }
    let t = txt(n, src).trim();
    let t = t.strip_prefix("[[").and_then(|s| s.strip_suffix("]]")).unwrap_or_else(|| t.trim_matches(|c| c == '"' || c == '\''));
    Some(t.to_string())
}

/// Premier argument littéral chaîne d'un appel (`f("x")`, `f "x"`).
fn first_string_arg(call: Node, src: &[u8]) -> Option<String> {
    let args = call.child_by_field_name("arguments")?;
    if args.kind() == "arguments" {
        let mut c = args.walk();
        let first = args.named_children(&mut c).next()?;
        string_value(first, src)
    } else {
        string_value(args, src)
    }
}

/// Spécificateur de projet d'un include Lua (`luaui/Include/x.lua` → `/luaui/Include/x.lua`).
fn include_spec(raw: &str, via_require: bool) -> Option<String> {
    let s = raw.trim().replace('\\', "/");
    if s.is_empty() || s.len() > 200 || s.contains(char::is_whitespace) {
        return None;
    }
    let s = s.trim_start_matches("./");
    if via_require && !s.contains('/') {
        // require("a.b") : points = séparateurs de dossiers (sauf extension .lua finale).
        let m = s.strip_suffix(".lua").unwrap_or(s).replace('.', "/");
        return Some(format!("/{}", m));
    }
    Some(format!("/{}", s.trim_start_matches('/')))
}

struct Ctx<'a> {
    src: &'a [u8],
    symbols: Vec<Symbol>,
    refs: FileRefs,
    /// Noms locaux alias d'une fonction du moteur (`local GetUnitPosition = Spring.GetUnitPosition`).
    engine_aliases: Vec<String>,
}

impl<'a> Ctx<'a> {
    fn push(&mut self, name: String, kind: SymbolKind, node: Node, sig_end: Option<usize>) {
        if name.is_empty() || name.len() > 120 {
            return;
        }
        let line = node.start_position().row as u32 + 1;
        let end_line = node.end_position().row as u32 + 1;
        let start = node.start_byte();
        // Signature : de la déclaration à la fin des paramètres (ou à la fin de la première ligne).
        let end = sig_end.unwrap_or_else(|| {
            let rest = &self.src[start..];
            start + rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len())
        });
        let raw = std::str::from_utf8(&self.src[start..end.max(start)]).unwrap_or("");
        let mut signature = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        if let Some(i) = signature.find(" --") {
            signature.truncate(i);
        }
        if signature.chars().count() > 200 {
            signature = signature.chars().take(200).collect::<String>() + "…";
        }
        let tokens = tokenize_identifier(&name.replace([':', '.'], "_"));
        self.symbols.push(Symbol { name, kind, line, end_line, signature, tokens, doc: Vec::new(), summary: String::new() });
    }

    /// `function a.b:c()` / `local function f()`.
    fn function_declaration(&mut self, n: Node) {
        let Some(name_node) = n.child_by_field_name("name") else { return };
        let name = compact(name_node, self.src);
        let kind = if name_node.kind() == "method_index_expression" { SymbolKind::Method } else { SymbolKind::Function };
        let sig_end = n.child_by_field_name("parameters").map(|p| p.end_byte());
        self.push(name, kind, n, sig_end);
    }

    /// Un nom de variable assignable (`foo`, `M.foo`) ; `None` pour `t[i]`.
    fn target_name(&self, v: Node) -> Option<String> {
        match v.kind() {
            "identifier" => Some(txt(v, self.src).to_string()),
            "dot_index_expression" => Some(compact(v, self.src)),
            _ => None,
        }
    }

    /// `x = function() end` / `local x = function() end` / `local M = { ... }` / alias moteur.
    fn assignment(&mut self, n: Node, top_level: bool) {
        let mut c = n.walk();
        let mut kids = n.named_children(&mut c);
        let (Some(vars), Some(vals)) = (kids.next(), kids.next()) else { return };
        if vars.kind() != "variable_list" || vals.kind() != "expression_list" {
            return;
        }
        let mut vc = vars.walk();
        let targets: Vec<Node> = vars.named_children(&mut vc).filter(|x| x.kind() != "attribute").collect();
        let mut ec = vals.walk();
        let values: Vec<Node> = vals.named_children(&mut ec).collect();
        for (i, v) in values.iter().enumerate() {
            let Some(t) = targets.get(i) else { break };
            let Some(name) = self.target_name(*t) else { continue };
            match v.kind() {
                "function_definition" => {
                    let sig_end = v.child_by_field_name("parameters").map(|p| p.end_byte());
                    // Début : la déclaration entière (avec `local`) si elle est le parent.
                    let decl = match n.parent() {
                        Some(p) if p.kind() == "variable_declaration" => p,
                        _ => n,
                    };
                    let kind = SymbolKind::Function;
                    self.push(name, kind, decl, sig_end);
                }
                "table_constructor" if top_level => {
                    let decl = match n.parent() {
                        Some(p) if p.kind() == "variable_declaration" => p,
                        _ => n,
                    };
                    let lines = v.end_position().row.saturating_sub(v.start_position().row) + 1;
                    let has_fields = v.named_child_count() > 0;
                    if lines >= 4 && has_fields {
                        self.push(name.clone(), SymbolKind::Const, decl, None);
                    }
                    self.table_functions(*v, &name);
                }
                "dot_index_expression" => {
                    // `local GetUnitPosition = Spring.GetUnitPosition` : alias moteur.
                    if let (Some(simple), true) = (self.target_name(*t), ENGINE_TABLES.contains(&root_of(&compact(*v, self.src)))) {
                        if !simple.contains('.') {
                            self.engine_aliases.push(simple);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Fonctions déclarées comme champs d'une table (`local M = { run = function() end }`).
    fn table_functions(&mut self, table: Node, prefix: &str) {
        let mut c = table.walk();
        for f in table.named_children(&mut c) {
            if f.kind() != "field" {
                continue;
            }
            let (Some(name), Some(value)) = (f.child_by_field_name("name"), f.child_by_field_name("value")) else { continue };
            if value.kind() != "function_definition" || name.kind() != "identifier" {
                continue;
            }
            let sig_end = value.child_by_field_name("parameters").map(|p| p.end_byte());
            let full = if prefix.is_empty() { txt(name, self.src).to_string() } else { format!("{}.{}", prefix, txt(name, self.src)) };
            self.push(full, SymbolKind::Function, f, sig_end);
        }
    }

    /// `return { armck = { ... } }` au premier niveau : une entrée de définition par clé.
    fn return_table(&mut self, n: Node) {
        let Some(list) = first_named(n, "expression_list") else { return };
        let mut c = list.walk();
        let Some(first) = list.named_children(&mut c).next() else { return };
        if first.kind() != "table_constructor" {
            return;
        }
        let mut fc = first.walk();
        for f in first.named_children(&mut fc) {
            if f.kind() != "field" {
                continue;
            }
            let (Some(name), Some(value)) = (f.child_by_field_name("name"), f.child_by_field_name("value")) else { continue };
            let key = match name.kind() {
                "identifier" => txt(name, self.src).to_string(),
                "string" => string_value(name, self.src).unwrap_or_default(),
                _ => continue,
            };
            if key.is_empty() || !key.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | ' ')) {
                continue;
            }
            match value.kind() {
                "table_constructor" => self.push(key, SymbolKind::Struct, f, None),
                "function_definition" => {
                    let sig_end = value.child_by_field_name("parameters").map(|p| p.end_byte());
                    self.push(key, SymbolKind::Function, f, sig_end)
                }
                _ => {}
            }
        }
    }

    /// Shader GLSL écrit dans une chaîne longue `[[ ... ]]` (motif des widgets GL4 de BAR) :
    /// ses fonctions et uniforms deviennent des symboles du fichier Lua, à leurs vraies lignes.
    fn inline_shader(&mut self, n: Node) {
        let t = txt(n, self.src);
        if t.len() < 120 || !t.starts_with('[') || !t.contains("void main") {
            return;
        }
        // `[[`, `[=[`… : le contenu commence après le premier `[` suivant.
        let Some(open) = t[1..].find('[').map(|p| p + 2) else { return };
        let inner = t[open..].trim_end_matches([']', '=']);
        let tree = crate::extract::parse_glsl(inner);
        let (syms, calls) = crate::glsl::extract_inline(inner, tree.as_ref(), n.start_position().row as u32);
        self.symbols.extend(syms);
        self.refs.calls.extend(calls);
    }

    /// Appel de fonction : référence (nom simple) ou include.
    fn call(&mut self, n: Node) {
        let Some(name_node) = n.child_by_field_name("name") else { return };
        let line = n.start_position().row as u32 + 1;
        let (simple, full) = match name_node.kind() {
            "identifier" => (txt(name_node, self.src).to_string(), txt(name_node, self.src).to_string()),
            "dot_index_expression" => {
                let full = compact(name_node, self.src);
                let simple = name_node.child_by_field_name("field").map(|f| txt(f, self.src).to_string()).unwrap_or_default();
                (simple, full)
            }
            "method_index_expression" => {
                let full = compact(name_node, self.src);
                let simple = name_node.child_by_field_name("method").map(|f| txt(f, self.src).to_string()).unwrap_or_default();
                (simple, full)
            }
            _ => return,
        };
        if simple.is_empty() {
            return;
        }
        // Includes (VFS.Include, include, require, dofile…).
        let lower = full.to_ascii_lowercase();
        let is_include = matches!(lower.as_str(), "include" | "vfs.include" | "dofile" | "loadfile" | "vfs.loadfile");
        let is_require = lower == "require";
        if is_include || is_require {
            if let Some(spec) = first_string_arg(n, self.src).and_then(|s| include_spec(&s, is_require)) {
                self.refs.imports.push(spec);
            }
            return;
        }
        if LUA_BUILTINS.contains(&simple.as_str()) || ENGINE_TABLES.contains(&root_of(&full)) {
            return;
        }
        self.refs.calls.push(CallRef { name: simple, line });
    }
}

/// Symboles et références d'un fichier Lua.
pub fn extract(source: &str, tree: Option<&Tree>) -> (Vec<Symbol>, FileRefs) {
    let Some(tree) = tree else { return (Vec::new(), FileRefs::default()) };
    let mut cx = Ctx { src: source.as_bytes(), symbols: Vec::new(), refs: FileRefs::default(), engine_aliases: Vec::new() };

    // Parcours itératif (pas de récursion : les chaînes `..` géantes donnent des arbres profonds).
    let mut cursor = tree.walk();
    let mut depth_fn = 0usize; // nombre de fonctions englobantes
    let mut fn_stack: Vec<bool> = Vec::new();
    'walk: loop {
        let n = cursor.node();
        let k = n.kind();
        let top_stmt = |m: Node| -> bool {
            // Au premier niveau : parent = chunk, ou variable_declaration dont le parent est un chunk.
            match m.parent() {
                Some(p) if p.kind() == "chunk" => true,
                Some(p) if p.kind() == "variable_declaration" => p.parent().is_some_and(|g| g.kind() == "chunk"),
                _ => false,
            }
        };
        match k {
            "function_declaration" => cx.function_declaration(n),
            "assignment_statement" => {
                let top = top_stmt(n) && depth_fn == 0;
                cx.assignment(n, top);
            }
            "return_statement" if n.parent().is_some_and(|p| p.kind() == "chunk") => cx.return_table(n),
            "function_call" => cx.call(n),
            "string" => cx.inline_shader(n),
            _ => {}
        }
        let is_fn = matches!(k, "function_declaration" | "function_definition");
        if cursor.goto_first_child() {
            fn_stack.push(is_fn);
            if is_fn {
                depth_fn += 1;
            }
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                continue 'walk;
            }
            if !cursor.goto_parent() {
                break 'walk;
            }
            if fn_stack.pop() == Some(true) {
                depth_fn -= 1;
            }
        }
    }

    let aliases = std::mem::take(&mut cx.engine_aliases);
    let mut refs = cx.refs;
    if !aliases.is_empty() {
        refs.calls.retain(|c| !aliases.contains(&c.name));
    }
    // Comme C/C++ : un fichier Lua « importe » les noms qu'il appelle (les `VFS.Include`
    // exposent des fonctions sans les nommer) ; la résolution départage ensuite.
    refs.imported_names = refs.calls.iter().map(|c| c.name.clone()).collect();
    let mut symbols = cx.symbols;
    symbols.sort_by_key(|s| s.line);
    (symbols, refs)
}

/// Texte d'en-tête d'un widget/gadget Lua : `name` et `desc` de `GetInfo()`, qui tiennent lieu
/// de commentaire de tête (les widgets BAR n'en ont pas, leur rôle est dans ces deux champs).
pub fn getinfo_text(source: &str) -> String {
    let mut out = String::new();
    let Some(pos) = source.find(":GetInfo") else { return out };
    for line in source[pos..].lines().skip(1).take(25) {
        let t = line.trim();
        if t == "end" {
            break;
        }
        for key in ["name", "desc", "description"] {
            if let Some(rest) = t.strip_prefix(key) {
                let rest = rest.trim_start();
                if let Some(v) = rest.strip_prefix('=') {
                    let v = v.trim().trim_end_matches(',');
                    let v = v.trim_matches(|c| c == '"' || c == '\'');
                    if !v.is_empty() {
                        out.push_str(v);
                        out.push(' ');
                    }
                }
            }
        }
    }
    out.trim().to_string()
}
