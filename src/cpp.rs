//! C et C++ (Unreal Engine compris) : symboles, appels, includes.
//!
//! Une seule grammaire (tree-sitter-cpp) sert les extensions C et C++. Le code
//! Unreal est plein de macros que le C++ pur ne connaît pas (`UCLASS(...)`,
//! `UFUNCTION(...)`, `UPROPERTY(...)`, `GENERATED_BODY()`, `FOX_API` devant le
//! nom de classe…) et qui feraient dérailler l'analyseur : `blank_macros` les
//! EFFACE avant l'analyse en les remplaçant par des espaces, octet pour octet
//! (les sauts de ligne restent) — les décalages et numéros de ligne de l'arbre
//! sont donc ceux du fichier d'origine, et les signatures se lisent dans le
//! source d'origine.
//!
//! Noms de symboles QUALIFIÉS : `AFoxMissile::Launch`, `FoxUi::BuildMenu`. La
//! clé de résolution des appels reste le dernier segment (`graph::call_key`).
//! Un prototype d'en-tête (`void Launch();`) est un symbole `Decl` : les
//! appels se résolvent de préférence vers la définition du .cpp.

use crate::symbol::{tokenize_identifier, CallRef, FileRefs, Symbol, SymbolKind};
use tree_sitter::{Node, Tree};

/// Macros Unreal avec arguments entre parenthèses (effacées, parenthèses comprises).
const PAREN_MACROS: &[&str] = &[
    "UCLASS",
    "USTRUCT",
    "UENUM",
    "UINTERFACE",
    "UFUNCTION",
    "UPROPERTY",
    "UDELEGATE",
    "UMETA",
    "UPARAM",
    "UE_DEPRECATED",
    "UE_STATIC_DEPRECATE",
    "GENERATED_BODY",
    "GENERATED_UCLASS_BODY",
    "GENERATED_USTRUCT_BODY",
    "GENERATED_UINTERFACE_BODY",
    "GENERATED_IINTERFACE_BODY",
    "GENERATED_BODY_LEGACY",
    "RIGVM_METHOD",
    "UE_INTERNAL",
    "UE_EXPERIMENTAL",
];

/// Macros de décoration sans argument (effacées seules ; parenthèses éventuelles incluses).
const BARE_MACROS: &[&str] = &[
    "FORCEINLINE",
    "FORCENOINLINE",
    "FORCEINLINE_DEBUGGABLE",
    "FORCEINLINE_STATS",
    "UE_FORCEINLINE_HINT",
    "UE_NODISCARD",
    "UE_REQUIRES",
    "PRAGMA_DISABLE_DEPRECATION_WARNINGS",
    "PRAGMA_ENABLE_DEPRECATION_WARNINGS",
    "PRAGMA_DISABLE_OPTIMIZATION",
    "PRAGMA_ENABLE_OPTIMIZATION",
    "PRAGMA_DISABLE_UNSAFE_TYPECAST_WARNINGS",
    "PRAGMA_ENABLE_UNSAFE_TYPECAST_WARNINGS",
];

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// `FOX_API`, `ENGINE_API`… (export de module Unreal).
fn is_api_macro(id: &[u8]) -> bool {
    id.len() > 4 && id.ends_with(b"_API") && id.iter().all(|&b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

/// Fin (exclue) d'un commentaire, d'une chaîne ou d'un littéral caractère qui
/// commence en `i`, si c'en est un.
fn skip_literal(b: &[u8], i: usize) -> Option<usize> {
    let n = b.len();
    match b[i] {
        b'/' if i + 1 < n && b[i + 1] == b'/' => Some(b[i..].iter().position(|&c| c == b'\n').map_or(n, |p| i + p)),
        b'/' if i + 1 < n && b[i + 1] == b'*' => {
            let mut j = i + 2;
            while j + 1 < n {
                if b[j] == b'*' && b[j + 1] == b'/' {
                    return Some(j + 2);
                }
                j += 1;
            }
            Some(n)
        }
        b'"' => {
            // Chaîne brute R"delim( … )delim"
            if i > 0 && b[i - 1] == b'R' {
                if let Some(open) = b[i + 1..n.min(i + 20)].iter().position(|&c| c == b'(') {
                    let delim = &b[i + 1..i + 1 + open];
                    let mut close = vec![b')'];
                    close.extend_from_slice(delim);
                    close.push(b'"');
                    let from = i + 2 + open;
                    if let Some(p) = b[from..].windows(close.len()).position(|w| w == close.as_slice()) {
                        return Some(from + p + close.len());
                    }
                    return Some(n);
                }
            }
            let mut j = i + 1;
            while j < n {
                match b[j] {
                    b'\\' => j += 2,
                    b'"' => return Some(j + 1),
                    b'\n' => return Some(j), // chaîne non terminée : on s'arrête à la ligne
                    _ => j += 1,
                }
            }
            Some(n)
        }
        // Littéral caractère (mais pas le séparateur de chiffres 1'000).
        b'\'' if i == 0 || !is_ident_byte(b[i - 1]) => {
            let mut j = i + 1;
            while j < n && j < i + 8 {
                match b[j] {
                    b'\\' => j += 2,
                    b'\'' => return Some(j + 1),
                    b'\n' => return Some(j),
                    _ => j += 1,
                }
            }
            Some((i + 1).min(n))
        }
        _ => None,
    }
}

/// Parenthèse fermante qui répond à la `(` de `open`, si elle existe de façon
/// raisonnable (moins de 4 000 octets plus loin).
fn match_paren(b: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = open;
    let limit = b.len().min(open + 4000);
    while i < limit {
        if let Some(e) = skip_literal(b, i) {
            i = e.max(i + 1);
            continue;
        }
        match b[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn blank(out: &mut [u8], from: usize, to: usize) {
    for c in out[from..to].iter_mut() {
        if *c != b'\n' && *c != b'\r' {
            *c = b' ';
        }
    }
}

/// Copie du source où les macros Unreal sont remplacées par des espaces (même
/// longueur, mêmes sauts de ligne).
pub fn blank_macros(src: &str) -> Vec<u8> {
    let b = src.as_bytes();
    let mut out = b.to_vec();
    let n = b.len();
    let mut i = 0;
    while i < n {
        if let Some(e) = skip_literal(b, i) {
            i = e.max(i + 1);
            continue;
        }
        let c = b[i];
        if (c.is_ascii_alphabetic() || c == b'_') && (i == 0 || !is_ident_byte(b[i - 1])) {
            let mut j = i;
            while j < n && is_ident_byte(b[j]) {
                j += 1;
            }
            let id = &b[i..j];
            let paren = PAREN_MACROS.iter().any(|m| m.as_bytes() == id);
            let bare = BARE_MACROS.iter().any(|m| m.as_bytes() == id);
            if paren || bare || is_api_macro(id) {
                // Arguments éventuels.
                let mut k = j;
                while k < n && (b[k] == b' ' || b[k] == b'\t') {
                    k += 1;
                }
                let mut end = j;
                if k < n && b[k] == b'(' && (paren || bare) {
                    if let Some(close) = match_paren(b, k) {
                        end = close + 1;
                    }
                }
                blank(&mut out, i, end);
                i = end.max(j);
                continue;
            }
            i = j;
            continue;
        }
        i += 1;
    }
    out
}

/// Lignes (0-based) qui ne portent QUE des macros effacées : `UFUNCTION(...)`,
/// `UPROPERTY(...)` multi-lignes… — elles se glissent entre un commentaire de
/// documentation et son symbole, `doc_above` doit les enjamber.
pub fn macro_only_lines(src: &str) -> Vec<bool> {
    let blanked = blank_macros(src);
    src.lines()
        .zip(blanked.split(|&c| c == b'\n'))
        .map(|(orig, bl)| {
            let o = orig.trim();
            !o.is_empty()
                && !o.starts_with("//")
                && !o.starts_with("/*")
                && !o.starts_with('*')
                && bl.iter().all(|c| c.is_ascii_whitespace())
        })
        .collect()
}

// ─── Extraction ─────────────────────────────────────────────────────────────

// Les membres de données (champs UPROPERTY) ne sont PAS des symboles : mesuré sur le
// banc FOX_THREE, ils noient les fonctions dans le classement (MRR 0,742 sans, 0,706
// avec les champs commentés) ; `cortex grep` les retrouve avec leur fonction englobante.

struct Ctx<'a> {
    /// Source d'origine (signatures).
    orig: &'a [u8],
    /// Source aux macros effacées (texte des nœuds).
    text: &'a [u8],
    /// Portée courante : (nom, est une classe/struct).
    scope: Vec<(String, bool)>,
    symbols: Vec<Symbol>,
    refs: FileRefs,
}

fn txt<'a>(n: Node, text: &'a [u8]) -> &'a str {
    n.utf8_text(text).unwrap_or("")
}

/// Texte d'un nom sans espaces ni arguments de template (`TFoo<T>::Bar` → `TFoo::Bar`).
fn clean_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut depth = 0i32;
    for ch in raw.chars() {
        match ch {
            '<' => depth += 1,
            '>' if depth > 0 => depth -= 1,
            c if depth == 0 && !c.is_whitespace() => out.push(c),
            _ => {}
        }
    }
    // `operator()` etc. : les parenthèses gêneraient les identifiants (`S:…#nom`).
    out.replace("operator()", "operator_call").replace("operator[]", "operator_index")
}

/// Macro (et non fonction) : `DECLARE_…`, `IMPLEMENT_…`, `UE_LOG`, MAJUSCULES_AVEC_SOULIGNÉS.
fn is_macro_name(simple: &str) -> bool {
    simple.starts_with("DECLARE_")
        || simple.starts_with("DEFINE_")
        || simple.starts_with("IMPLEMENT_")
        || simple.starts_with("BEGIN_")
        || simple.starts_with("END_")
        || simple.starts_with("UE_")
        || (simple.contains('_') && !simple.bytes().any(|b| b.is_ascii_lowercase()))
}

fn simple_of(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

impl<'a> Ctx<'a> {
    fn qualify(&self, declared: &str) -> String {
        let mut q = String::new();
        for (s, _) in &self.scope {
            q.push_str(s);
            q.push_str("::");
        }
        q.push_str(declared);
        q
    }

    fn in_class(&self) -> bool {
        self.scope.last().is_some_and(|s| s.1)
    }

    #[allow(clippy::too_many_arguments)]
    fn push(&mut self, qname: String, kind: SymbolKind, name_node: Node, decl: Node) {
        if qname.is_empty() || qname.len() > 120 {
            return;
        }
        let line = name_node.start_position().row as u32 + 1;
        let end_line = (decl.end_position().row as u32 + 1).max(line);
        let simple = simple_of(&qname).to_string();
        let mut tokens = tokenize_identifier(&simple);
        for part in qname.split("::").filter(|p| !p.is_empty()) {
            for t in tokenize_identifier(part) {
                if !tokens.contains(&t) {
                    tokens.push(t);
                }
            }
        }
        self.symbols.push(Symbol {
            name: qname,
            kind,
            line,
            end_line,
            signature: crate::extract::signature_at(self.orig, decl.start_byte()),
            tokens,
            doc: Vec::new(),
            summary: String::new(),
        });
    }

    /// Nom déclaré (sans portée englobante) et nœud-nom d'un déclarateur, en
    /// traversant pointeurs/références/parenthèses. `None` si ce n'est pas une
    /// fonction ; `Some((nom, noeud, est_fonction))` sinon.
    fn declarator_name<'t>(&self, mut d: Node<'t>) -> Option<(String, Node<'t>, bool)> {
        loop {
            match d.kind() {
                "pointer_declarator"
                | "reference_declarator"
                | "parenthesized_declarator"
                | "attributed_declarator"
                | "init_declarator" => {
                    d = d.child_by_field_name("declarator").or_else(|| d.named_child(d.named_child_count().saturating_sub(1)))?;
                }
                "function_declarator" => {
                    let inner = d.child_by_field_name("declarator")?;
                    return match inner.kind() {
                        "identifier"
                        | "field_identifier"
                        | "qualified_identifier"
                        | "destructor_name"
                        | "operator_name"
                        | "template_function"
                        | "template_method"
                        | "operator_cast" => Some((clean_name(txt(inner, self.text)), inner, true)),
                        _ => None,
                    };
                }
                "identifier" | "field_identifier" => return Some((txt(d, self.text).to_string(), d, false)),
                _ => return None,
            }
        }
    }

    /// Les paramètres d'un `function_declarator` sont-ils de vrais paramètres
    /// (et non des arguments de constructeur `FFoo X(1, 2);`) ?
    fn has_real_params(&self, d: Node) -> bool {
        let mut cur = d;
        while cur.kind() != "function_declarator" {
            match cur.child_by_field_name("declarator") {
                Some(c) => cur = c,
                None => return false,
            }
        }
        let Some(params) = cur.child_by_field_name("parameters") else { return false };
        let mut c = params.walk();
        let ok = params.named_children(&mut c).all(|p| {
            matches!(p.kind(), "parameter_declaration" | "optional_parameter_declaration" | "variadic_parameter_declaration" | "comment")
        });
        ok
    }

    fn visit(&mut self, node: Node) {
        match node.kind() {
            "namespace_definition" => {
                let name = node.child_by_field_name("name").map(|n| clean_name(txt(n, self.text))).unwrap_or_default();
                let pushed = if name.is_empty() {
                    0
                } else {
                    let parts: Vec<String> = name.split("::").filter(|p| !p.is_empty()).map(|s| s.to_string()).collect();
                    if let Some(nn) = node.child_by_field_name("name") {
                        let q = self.qualify(&name);
                        self.push(q, SymbolKind::Type, nn, node);
                    }
                    for p in &parts {
                        self.scope.push((p.clone(), false));
                    }
                    parts.len()
                };
                if let Some(body) = node.child_by_field_name("body") {
                    self.visit_children(body);
                }
                for _ in 0..pushed {
                    self.scope.pop();
                }
            }
            "class_specifier" | "struct_specifier" | "union_specifier" => {
                let (Some(name_node), Some(body)) = (node.child_by_field_name("name"), node.child_by_field_name("body")) else {
                    self.visit_children(node);
                    return;
                };
                let name = clean_name(txt(name_node, self.text));
                let kind = if node.kind() == "class_specifier" { SymbolKind::Class } else { SymbolKind::Struct };
                let q = self.qualify(&name);
                self.push(q, kind, name_node, node);
                // Classes de base : une référence (le graphe en fait les « dérivées »).
                let mut c = node.walk();
                for ch in node.children(&mut c) {
                    if ch.kind() == "base_class_clause" {
                        let mut c2 = ch.walk();
                        for b in ch.named_children(&mut c2) {
                            if matches!(b.kind(), "type_identifier" | "qualified_identifier" | "template_type") {
                                self.add_type_ref(b);
                            }
                        }
                    }
                }
                for p in name.split("::").filter(|p| !p.is_empty()) {
                    self.scope.push((p.to_string(), true));
                }
                let depth = name.split("::").filter(|p| !p.is_empty()).count();
                self.visit_children(body);
                for _ in 0..depth {
                    self.scope.pop();
                }
            }
            "enum_specifier" => {
                if let (Some(name_node), Some(_)) = (node.child_by_field_name("name"), node.child_by_field_name("body")) {
                    let name = clean_name(txt(name_node, self.text));
                    let q = self.qualify(&name);
                    self.push(q, SymbolKind::Enum, name_node, node);
                }
            }
            "function_definition" => {
                if let Some(d) = node.child_by_field_name("declarator") {
                    if let Some((name, name_node, true)) = self.declarator_name(d) {
                        let simple = simple_of(&name).to_string();
                        if !is_macro_name(&simple) {
                            let kind = if self.in_class() || name.contains("::") { SymbolKind::Method } else { SymbolKind::Function };
                            let q = self.qualify(&name);
                            self.push(q, kind, name_node, node);
                        }
                    }
                }
                // Classes locales / lambdas : pas de symboles dans les corps.
            }
            "declaration" | "field_declaration" => {
                // Type défini dans la déclaration : `struct FFoo {...} Foo;`
                if let Some(t) = node.child_by_field_name("type") {
                    if matches!(t.kind(), "class_specifier" | "struct_specifier" | "union_specifier" | "enum_specifier") {
                        self.visit(t);
                    }
                }
                let head = txt(node, self.text).trim_start();
                if head.starts_with("friend") {
                    return;
                }
                let mut c = node.walk();
                let decls: Vec<Node> = node.children_by_field_name("declarator", &mut c).collect();
                for d in decls {
                    let Some((name, name_node, is_fn)) = self.declarator_name(d) else { continue };
                    if is_fn {
                        if !self.has_real_params(d) || is_macro_name(simple_of(&name)) {
                            continue;
                        }
                        let q = self.qualify(&name);
                        self.push(q, SymbolKind::Decl, name_node, node);
                    }
                }
            }
            "type_definition" => {
                if let Some(t) = node.child_by_field_name("type") {
                    if matches!(t.kind(), "class_specifier" | "struct_specifier" | "union_specifier" | "enum_specifier") {
                        self.visit(t);
                    }
                }
                let mut c = node.walk();
                let decls: Vec<Node> = node.children_by_field_name("declarator", &mut c).collect();
                for d in decls {
                    if d.kind() == "type_identifier" {
                        let q = self.qualify(txt(d, self.text));
                        self.push(q, SymbolKind::Type, d, node);
                    }
                }
            }
            "alias_declaration" => {
                if let Some(n) = node.child_by_field_name("name") {
                    let q = self.qualify(txt(n, self.text));
                    self.push(q, SymbolKind::Type, n, node);
                }
            }
            "preproc_def" | "preproc_function_def" => {
                if let Some(n) = node.child_by_field_name("name") {
                    let name = txt(n, self.text);
                    if !name.ends_with("_H") && !name.ends_with("_H_") && !name.ends_with("_h") {
                        self.push(name.to_string(), SymbolKind::Const, n, node);
                    }
                }
            }
            "preproc_include" => {
                if let Some(p) = node.child_by_field_name("path") {
                    if p.kind() == "string_literal" {
                        let raw = txt(p, self.text).trim_matches('"');
                        if !raw.is_empty() && !raw.ends_with(".generated.h") {
                            self.refs.imports.push(raw.to_string());
                        }
                    }
                }
            }
            "template_declaration"
            | "linkage_specification"
            | "declaration_list"
            | "field_declaration_list"
            | "translation_unit"
            | "preproc_if"
            | "preproc_ifdef"
            | "preproc_else"
            | "preproc_elif"
            | "preproc_elifdef"
            | "ERROR"
            | "access_specifier"
            | "labeled_statement" => self.visit_children(node),
            _ => {}
        }
    }

    fn visit_children(&mut self, node: Node) {
        let mut c = node.walk();
        let kids: Vec<Node> = node.children(&mut c).collect();
        for k in kids {
            self.visit(k);
        }
    }

    /// Référence à un type (classe de base, argument de template) : un « appel »
    /// à son nom, pour que le graphe relie utilisateurs et classe.
    fn add_type_ref(&mut self, n: Node) {
        let mut cur = n;
        loop {
            match cur.kind() {
                "qualified_identifier" => match cur.child_by_field_name("name") {
                    Some(x) => cur = x,
                    None => return,
                },
                "template_type" => match cur.child_by_field_name("name") {
                    Some(x) => cur = x,
                    None => return,
                },
                "type_identifier" | "identifier" => {
                    let name = txt(cur, self.text);
                    self.refs.calls.push(CallRef { name: name.to_string(), line: cur.start_position().row as u32 + 1 });
                    return;
                }
                _ => return,
            }
        }
    }

    /// Nom simple appelé par la position « fonction » d'un `call_expression`.
    fn callee(&mut self, f: Node) {
        let mut cur = f;
        let mut first = true;
        loop {
            match cur.kind() {
                "identifier" | "field_identifier" | "type_identifier" => {
                    let name = txt(cur, self.text);
                    if !is_macro_name(name) && name.len() <= 80 {
                        self.refs.calls.push(CallRef { name: name.to_string(), line: cur.start_position().row as u32 + 1 });
                    }
                    return;
                }
                "field_expression" => match cur.child_by_field_name("field") {
                    Some(x) => cur = x,
                    None => return,
                },
                "qualified_identifier" => {
                    // `UFoo::StaticClass()` : la portée est aussi une référence.
                    if first {
                        if let Some(s) = cur.child_by_field_name("scope") {
                            if matches!(s.kind(), "namespace_identifier" | "type_identifier" | "identifier") {
                                let name = txt(s, self.text);
                                self.refs.calls.push(CallRef { name: name.to_string(), line: s.start_position().row as u32 + 1 });
                            }
                        }
                    }
                    match cur.child_by_field_name("name") {
                        Some(x) => cur = x,
                        None => return,
                    }
                }
                "template_function" | "template_method" => {
                    if let Some(args) = cur.child_by_field_name("arguments") {
                        let mut c = args.walk();
                        let kids: Vec<Node> = args.named_children(&mut c).collect();
                        for a in kids {
                            if a.kind() == "type_descriptor" {
                                if let Some(t) = a.child_by_field_name("type") {
                                    self.add_type_ref(t);
                                }
                            }
                        }
                    }
                    match cur.child_by_field_name("name") {
                        Some(x) => cur = x,
                        None => return,
                    }
                }
                "destructor_name" | "operator_name" => return,
                _ => return,
            }
            first = false;
        }
    }

    /// Passe « appels » : parcours itératif de tout l'arbre (les expressions
    /// très imbriquées ne doivent pas faire déborder la pile).
    fn collect_calls(&mut self, tree: &Tree) {
        let mut cursor = tree.walk();
        loop {
            let node = cursor.node();
            match node.kind() {
                "call_expression" => {
                    if let Some(f) = node.child_by_field_name("function") {
                        self.callee(f);
                    }
                }
                "new_expression" => {
                    if let Some(t) = node.child_by_field_name("type") {
                        self.add_type_ref(t);
                    }
                }
                // `&AFoo::OnHit` : pointeur de méthode (liaison de délégué).
                "pointer_expression" => {
                    if let Some(a) = node.child_by_field_name("argument") {
                        if a.kind() == "qualified_identifier" {
                            self.callee(a);
                        }
                    }
                }
                _ => {}
            }
            if cursor.goto_first_child() {
                continue;
            }
            loop {
                if cursor.goto_next_sibling() {
                    break;
                }
                if !cursor.goto_parent() {
                    return;
                }
            }
        }
    }
}

/// Texte du commentaire `//` de fin de ligne (hors chaînes), vide sinon.
pub fn trailing_comment(line: &str) -> String {
    let b = line.as_bytes();
    let mut i = 0;
    while i + 1 < b.len() {
        if let Some(e) = skip_literal(b, i) {
            if b[i] == b'/' && b[i + 1] == b'/' && i > 0 {
                return line[i..].trim_start_matches('/').trim_start_matches(['!', '<']).trim().to_string();
            }
            i = e.max(i + 1);
            continue;
        }
        i += 1;
    }
    String::new()
}

/// Premier argument d'une macro d'après le texte `(a, b, c)`.
fn macro_args(rest: &str) -> Vec<String> {
    let rest = rest.trim_start();
    if !rest.starts_with('(') {
        return Vec::new();
    }
    let mut depth = 0i32;
    let mut cur = String::new();
    let mut args = Vec::new();
    for ch in rest.chars() {
        match ch {
            '(' => {
                depth += 1;
                if depth > 1 {
                    cur.push(ch);
                }
            }
            ')' => {
                depth -= 1;
                if depth == 0 {
                    args.push(cur.trim().to_string());
                    break;
                }
                cur.push(ch);
            }
            ',' if depth == 1 => {
                args.push(cur.trim().to_string());
                cur.clear();
            }
            '\n' if depth == 0 => break,
            _ => cur.push(ch),
        }
        if args.len() > 6 {
            break;
        }
    }
    args
}

/// Délégués et catégories de log Unreal (`DECLARE_DYNAMIC_MULTICAST_DELEGATE_OneParam(FOnHit, …)`,
/// `DECLARE_LOG_CATEGORY_EXTERN(LogFox, …)`), que la grammaire ne voit pas comme
/// des déclarations de type.
fn macro_declared_names(src: &str, out: &mut Vec<Symbol>) {
    for (i, raw) in src.lines().enumerate() {
        let line = raw.trim_start();
        let Some(paren) = line.find('(') else { continue };
        let head = &line[..paren];
        if !head.bytes().all(is_ident_byte) {
            continue;
        }
        let (kind, idx) = if head.starts_with("DECLARE_") && head.contains("DELEGATE") {
            (SymbolKind::Type, if head.contains("RetVal") { 1 } else { 0 })
        } else if head.starts_with("DECLARE_LOG_CATEGORY") || head.starts_with("DEFINE_LOG_CATEGORY") {
            (SymbolKind::Const, 0)
        } else {
            continue;
        };
        let args = macro_args(&line[paren..]);
        let Some(name) = args.get(idx).filter(|n| !n.is_empty() && n.bytes().all(is_ident_byte)) else { continue };
        // Une catégorie déclarée puis définie : un seul symbole.
        if out.iter().any(|s| s.name == *name) {
            continue;
        }
        out.push(Symbol {
            name: name.clone(),
            kind,
            line: i as u32 + 1,
            end_line: i as u32 + 1,
            signature: line.trim_end().chars().take(200).collect::<String>().trim_end_matches(';').to_string(),
            tokens: tokenize_identifier(name),
            doc: Vec::new(),
            summary: String::new(),
        });
    }
}

/// Symboles et références d'un fichier C/C++ (arbre déjà produit sur `blank_macros`).
pub fn extract(source: &str, tree: Option<&Tree>) -> (Vec<Symbol>, FileRefs) {
    let Some(tree) = tree else { return (Vec::new(), FileRefs::default()) };
    // Les nœuds se lisent dans le source aux macros effacées (mêmes octets sinon).
    let text = blank_macros(source);
    let mut cx = Ctx { orig: source.as_bytes(), text: &text, scope: Vec::new(), symbols: Vec::new(), refs: FileRefs::default() };
    cx.visit(tree.root_node());
    cx.collect_calls(tree);
    let mut symbols = cx.symbols;
    let mut refs = cx.refs;
    macro_declared_names(source, &mut symbols);
    symbols.sort_by_key(|s| s.line);
    // Un fichier C++ « importe » les noms qu'il appelle : l'inclusion d'un en-tête
    // les rend visibles sans les nommer, la résolution (même fichier, fichiers
    // inclus, nom unique du projet) départage ensuite.
    refs.imported_names = refs.calls.iter().map(|c| c.name.clone()).collect();
    (symbols, refs)
}

/// Includes entre guillemets → spécificateurs de projet résolubles par le graphe
/// (`/dossier/Nom`, sans extension : `Nom.h` et `Nom.cpp` partagent une souche).
/// Une inclusion se lit depuis un dossier d'inclusion que l'on ne connaît pas :
/// on essaie le dossier du fichier puis chacun de ses ancêtres (et leur
/// sous-dossier `Public`, convention des modules Unreal) ; ceux qui ne désignent
/// aucun fichier sont ignorés à la résolution.
pub fn expand_includes(rel: &str, refs: &mut FileRefs) {
    let raw = std::mem::take(&mut refs.imports);
    let mut dirs: Vec<String> = Vec::new();
    let mut d = rel;
    while let Some((parent, _)) = d.rsplit_once('/') {
        dirs.push(parent.to_string());
        d = parent;
    }
    dirs.push(String::new());
    let mut out: Vec<String> = Vec::new();
    for inc in raw {
        let inc = inc.replace('\\', "/");
        let stem = match inc.rsplit_once('.') {
            Some((s, e)) if e.len() <= 4 && !s.is_empty() => s.to_string(),
            _ => inc.clone(),
        };
        for dir in &dirs {
            let base = if dir.is_empty() { stem.clone() } else { format!("{}/{}", dir, stem) };
            out.push(format!("/{}", base));
            if !dir.is_empty() {
                out.push(format!("/{}/Public/{}", dir, stem));
            }
        }
    }
    refs.imports = out;
}

#[cfg(test)]
mod tests {
    use crate::extract::{extract_comments, extract_symbols};
    use crate::lang::Lang;

    const SRC: &str = r#"// Copyright
#pragma once
#include "CoreMinimal.h"
#include "Weapons/FoxMissile.h"
#include "FoxMissile.generated.h"

/** Missile guidé : suit sa cible. */
UCLASS(Blueprintable, meta=(DisplayName="Missile (guidé)"))
class FOX_API AFoxMissile : public AActor
{
    GENERATED_BODY()
public:
    AFoxMissile();

    /** Tire le missile vers la cible. */
    UFUNCTION(BlueprintCallable,
        Category="Fox")
    void Launch(AActor* Target, float Speed = 1.f);

    UPROPERTY(EditAnywhere, Category="Fox")
    float MaxSpeed = 900.f;

    FORCEINLINE bool IsArmed() const { return bArmed; }
private:
    bool bArmed = false;
};

namespace FoxUi
{
    /** Construit le menu. */
    void BuildMenu(int32 Index);
}

UENUM(BlueprintType)
enum class EFoxState : uint8 { Idle UMETA(DisplayName="Idle"), Fly };

DECLARE_DYNAMIC_MULTICAST_DELEGATE_OneParam(FOnMissileHit, AActor*, Victim);

void AFoxMissile::Launch(AActor* Target, float Speed)
{
    UE_LOG(LogTemp, Log, TEXT("launch"));
    Engage(Target);
    FoxUi::BuildMenu(3);
    Handle.BindUObject(this, &AFoxMissile::OnHit);
    auto* P = NewObject<UFoxPayload>(this);
}

template <typename T>
T TFoo<T>::Get() const { return Value; }
"#;

    #[test]
    fn symboles_unreal() {
        let syms = extract_symbols(Lang::Cpp, SRC);
        let find = |n: &str| syms.iter().find(|s| s.name == n);
        let names: Vec<String> = syms.iter().map(|s| format!("{}:{}", s.kind.as_str(), s.name)).collect();
        assert!(find("AFoxMissile").is_some(), "{names:?}");
        assert_eq!(find("AFoxMissile").unwrap().kind, crate::symbol::SymbolKind::Class);
        assert_eq!(find("AFoxMissile::Launch").map(|s| s.kind), Some(crate::symbol::SymbolKind::Decl), "{names:?}");
        assert!(syms.iter().any(|s| s.name == "AFoxMissile::Launch" && s.kind == crate::symbol::SymbolKind::Method), "{names:?}");
        assert!(find("AFoxMissile::IsArmed").is_some(), "{names:?}");
        assert!(find("AFoxMissile::MaxSpeed").is_none(), "les champs ne sont pas des symboles");
        assert!(find("FoxUi::BuildMenu").is_some(), "{names:?}");
        assert!(find("EFoxState").is_some(), "{names:?}");
        assert!(find("FOnMissileHit").is_some(), "{names:?}");
        assert!(find("TFoo::Get").is_some(), "{names:?}");
        assert_eq!(find("AFoxMissile").unwrap().signature, "class FOX_API AFoxMissile : public AActor");
    }

    #[test]
    fn docs_et_appels() {
        let mut syms = extract_symbols(Lang::Cpp, SRC);
        let (_, refs) = crate::extract::extract_symbols_and_refs(Lang::Cpp, SRC);
        extract_comments(Lang::Cpp, SRC, &mut syms);
        let cls = syms.iter().find(|s| s.name == "AFoxMissile").unwrap();
        assert!(cls.doc.contains(&"missile".to_string()) && cls.doc.contains(&"cible".to_string()), "{:?}", cls.doc);
        let launch = syms.iter().find(|s| s.name == "AFoxMissile::Launch").unwrap();
        assert!(launch.doc.contains(&"tire".to_string()), "{:?}", launch.doc);
        let calls: Vec<&str> = refs.calls.iter().map(|c| c.name.as_str()).collect();
        for want in ["Engage", "BuildMenu", "OnHit", "UFoxPayload", "AActor", "NewObject"] {
            assert!(calls.contains(&want), "{want} absent de {calls:?}");
        }
        assert!(!calls.contains(&"UE_LOG"));
        assert!(refs.imports.iter().any(|i| i == "Weapons/FoxMissile.h"));
        assert!(!refs.imports.iter().any(|i| i.contains("generated")));
    }

    #[test]
    fn efface_les_macros_sans_decaler() {
        let s = "UPROPERTY(meta=(A=\"(\"))\nint X; // UFUNCTION(\nFOX_API int Y;";
        let b = super::blank_macros(s);
        assert_eq!(b.len(), s.len());
        let t = String::from_utf8(b).unwrap();
        assert!(t.starts_with(&format!("{}\nint X; // UFUNCTION(", " ".repeat(23))), "{t:?}");
        assert!(t.ends_with("        int Y;"), "{t:?}");
    }

    #[test]
    fn inclusions_en_specificateurs() {
        let mut refs = crate::symbol::FileRefs { imports: vec!["Weapons/FoxMissile.h".into()], ..Default::default() };
        super::expand_includes("Source/FOX_THREE/AI/Foo.cpp", &mut refs);
        assert!(refs.imports.contains(&"/Source/FOX_THREE/Weapons/FoxMissile".to_string()), "{:?}", refs.imports);
    }
}
