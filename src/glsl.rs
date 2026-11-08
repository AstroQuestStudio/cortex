//! GLSL (`.glsl .vert .frag .geom .comp .tesc .tese`) : fonctions, structures, macros,
//! uniforms, appels. Sert aussi aux shaders écrits en chaîne dans du Lua (`[[ ... ]]`,
//! motif de Beyond All Reason) : `extract_inline` rend des symboles dont les lignes sont
//! celles du fichier Lua englobant.
//!
//! Grammaire tree-sitter-glsl (dérivée de C) pour les fonctions, structures et macros ;
//! les `uniform` sont lus par une passe TEXTE (les shaders de BAR mêlent `#if`, `#line`
//! et blocs d'interface que la grammaire n'avale pas toujours).

use crate::symbol::{tokenize_identifier, CallRef, FileRefs, Symbol, SymbolKind};
use tree_sitter::{Node, Tree};

/// Fonctions et mots-clés GLSL qui ne sont jamais des fonctions du projet.
const BUILTINS: &[&str] = &[
    "if",
    "for",
    "while",
    "switch",
    "return",
    "vec2",
    "vec3",
    "vec4",
    "ivec2",
    "ivec3",
    "ivec4",
    "uvec2",
    "uvec3",
    "uvec4",
    "bvec2",
    "bvec3",
    "bvec4",
    "mat2",
    "mat3",
    "mat4",
    "float",
    "int",
    "uint",
    "bool",
    "radians",
    "degrees",
    "sin",
    "cos",
    "tan",
    "asin",
    "acos",
    "atan",
    "pow",
    "exp",
    "log",
    "exp2",
    "log2",
    "sqrt",
    "inversesqrt",
    "abs",
    "sign",
    "floor",
    "ceil",
    "fract",
    "mod",
    "min",
    "max",
    "clamp",
    "mix",
    "step",
    "smoothstep",
    "length",
    "distance",
    "dot",
    "cross",
    "normalize",
    "reflect",
    "refract",
    "transpose",
    "inverse",
    "texture",
    "textureLod",
    "textureProj",
    "texelFetch",
    "textureSize",
    "textureGrad",
    "texture2D",
    "texture2DLod",
    "textureCube",
    "dFdx",
    "dFdy",
    "fwidth",
    "round",
    "trunc",
    "isnan",
    "isinf",
    "any",
    "all",
    "not",
    "lessThan",
    "greaterThan",
    "equal",
    "notEqual",
    "floatBitsToInt",
    "intBitsToFloat",
    "packUnorm4x8",
    "unpackUnorm4x8",
    "EmitVertex",
    "EndPrimitive",
    "barrier",
    "memoryBarrier",
    "imageLoad",
    "imageStore",
    "atomicAdd",
    "determinant",
    "outerProduct",
    "matrixCompMult",
];

fn txt<'a>(n: Node, src: &'a [u8]) -> &'a str {
    n.utf8_text(src).unwrap_or("")
}

fn sig(raw: &str) -> String {
    let mut s = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some(i) = s.find(" //") {
        s.truncate(i);
    }
    if s.chars().count() > 200 {
        s = s.chars().take(200).collect::<String>() + "…";
    }
    s
}

/// Nom d'une fonction depuis son déclarateur (`float f(int a)` → `f`).
fn declarator_name<'a>(d: Node<'a>, src: &'a [u8]) -> Option<&'a str> {
    let mut cur = d;
    for _ in 0..8 {
        match cur.kind() {
            "identifier" | "field_identifier" | "type_identifier" => return Some(txt(cur, src)),
            _ => cur = cur.child_by_field_name("declarator")?,
        }
    }
    None
}

fn push(out: &mut Vec<Symbol>, name: &str, kind: SymbolKind, line: u32, end_line: u32, signature: String) {
    if name.is_empty() || name.len() > 120 {
        return;
    }
    out.push(Symbol {
        name: name.to_string(),
        kind,
        line,
        end_line: end_line.max(line),
        signature,
        tokens: tokenize_identifier(name),
        doc: Vec::new(),
        summary: String::new(),
    });
}

/// Passe texte : `uniform ...;`, `uniform Bloc {`, `layout(...) uniform ...`.
fn uniforms(source: &str, line_off: u32, out: &mut Vec<Symbol>) {
    for (i, line) in source.lines().enumerate() {
        let t = line.trim_start();
        let rest = if let Some(r) = t.strip_prefix("uniform ") {
            r
        } else if t.starts_with("layout") {
            match t.find(") uniform ").or_else(|| t.find(")uniform ")) {
                Some(p) => &t[p + 1..].trim_start()["uniform".len()..],
                None => continue,
            }
        } else {
            continue;
        };
        let decl = rest.split(';').next().unwrap_or(rest);
        let decl = decl.split("//").next().unwrap_or(decl);
        let ln = line_off + i as u32 + 1;
        // Bloc d'interface : `uniform Name {`
        if let Some(b) = decl.find('{') {
            let name = decl[..b].split_whitespace().last().unwrap_or("");
            if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                push(out, name, SymbolKind::Struct, ln, ln, sig(t));
            }
            continue;
        }
        // `uniform float a, b[3] = ...;` : chaque nom déclaré.
        let mut parts = decl.splitn(2, |c: char| c.is_whitespace());
        let mut first = parts.next().unwrap_or("");
        let mut tail = parts.next().unwrap_or("");
        // Qualificateurs de précision/type multiples : on avance jusqu'au dernier mot avant le nom.
        while matches!(first, "highp" | "mediump" | "lowp" | "flat" | "smooth" | "coherent" | "readonly" | "writeonly" | "const") {
            let mut p = tail.trim_start().splitn(2, |c: char| c.is_whitespace());
            first = p.next().unwrap_or("");
            tail = p.next().unwrap_or("");
        }
        for piece in tail.split(',') {
            let name: String = piece.trim().chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
            if !name.is_empty() && !name.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                push(out, &name, SymbolKind::Const, ln, ln, sig(t));
            }
        }
    }
}

fn walk_tree(tree: &Tree, src: &[u8], line_off: u32, out: &mut Vec<Symbol>, calls: &mut Vec<CallRef>) {
    let mut cursor = tree.walk();
    'walk: loop {
        let n = cursor.node();
        let row = n.start_position().row as u32 + 1 + line_off;
        let end_row = n.end_position().row as u32 + 1 + line_off;
        match n.kind() {
            "function_definition" => {
                if let Some(name) = n.child_by_field_name("declarator").and_then(|d| declarator_name(d, src)) {
                    let end = n.child_by_field_name("body").map(|b| b.start_byte()).unwrap_or(n.end_byte());
                    let raw = std::str::from_utf8(&src[n.start_byte()..end.max(n.start_byte())]).unwrap_or("");
                    // `layout(...)` ou `#define` collés en tête ne font pas partie de la signature.
                    push(out, name, SymbolKind::Function, row, end_row, sig(raw));
                }
            }
            "struct_specifier" => {
                if let (Some(name), Some(_)) = (n.child_by_field_name("name"), n.child_by_field_name("body")) {
                    push(out, txt(name, src), SymbolKind::Struct, row, end_row, sig(&format!("struct {}", txt(name, src))));
                }
            }
            "preproc_def" | "preproc_function_def" => {
                if let Some(name) = n.child_by_field_name("name") {
                    let nm = txt(name, src);
                    // Les macros utilitaires seulement (pas les drapeaux `#define FOO` sans valeur).
                    if n.child_by_field_name("value").is_some() || n.kind() == "preproc_function_def" {
                        let first_line = txt(n, src).lines().next().unwrap_or("");
                        push(out, nm, SymbolKind::Const, row, end_row, sig(first_line));
                    }
                }
            }
            "call_expression" => {
                if let Some(f) = n.child_by_field_name("function") {
                    if f.kind() == "identifier" {
                        let name = txt(f, src);
                        if !BUILTINS.contains(&name) {
                            calls.push(CallRef { name: name.to_string(), line: row });
                        }
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
                continue 'walk;
            }
            if !cursor.goto_parent() {
                break 'walk;
            }
        }
    }
}

/// Symboles et références d'un fichier shader.
pub fn extract(source: &str, tree: Option<&Tree>) -> (Vec<Symbol>, FileRefs) {
    let mut symbols = Vec::new();
    let mut refs = FileRefs::default();
    if let Some(t) = tree {
        walk_tree(t, source.as_bytes(), 0, &mut symbols, &mut refs.calls);
    }
    uniforms(source, 0, &mut symbols);
    symbols.sort_by_key(|s| s.line);
    refs.imported_names = refs.calls.iter().map(|c| c.name.clone()).collect();
    (symbols, refs)
}

/// Shader inline (chaîne Lua) : `tree` est l'arbre de `text`, `line_off` le nombre de lignes du
/// fichier hôte qui précèdent la première ligne de `text`.
pub fn extract_inline(text: &str, tree: Option<&Tree>, line_off: u32) -> (Vec<Symbol>, Vec<CallRef>) {
    let mut symbols = Vec::new();
    let mut calls = Vec::new();
    if let Some(t) = tree {
        walk_tree(t, text.as_bytes(), line_off, &mut symbols, &mut calls);
    }
    uniforms(text, line_off, &mut symbols);
    (symbols, calls)
}
