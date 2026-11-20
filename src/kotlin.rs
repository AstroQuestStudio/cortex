//! Kotlin (`.kt`, `.kts`) : symboles, appels, imports.
//!
//! Grammaire tree-sitter-kotlin 0.3.8 (MIT, ABI 14, sans shim). Cette grammaire n'a pas de
//! champs nommés : on lit les enfants par leur genre. Noms de symboles pensés pour la recherche :
//!
//! - `class` / `data class` / `sealed class` / `annotation class` → classe ; `interface` →
//!   interface ; `enum class` → énumération (ses entrées sont des constantes) ; `object` → classe ;
//!   `companion object Nom` → classe `Nom` (un companion sans nom n'est pas un symbole : ses
//!   membres appartiennent à la classe englobante) ;
//! - `fun` dans une classe, un objet ou une interface → méthode ; `fun` de premier niveau et
//!   fonction d'extension (`fun String.shout()` → `shout`, le récepteur reste dans la signature)
//!   → fonction ; constructeur secondaire → méthode au nom de la classe (comme Java) ;
//! - `val` / `var` de premier niveau et `const val` d'un objet → constante ; `typealias` → type ;
//! - les annotations utiles (`@Composable`, `@Test`, `@GET("/x")`…) sont préfixées à la signature ;
//!   `@Override`-like (`override` est un modificateur, déjà dans la signature) et le bruit
//!   (`@Suppress`, `@JvmStatic`…) sont écartés.
//!
//! Rien n'est indexé dans un corps de fonction (variables locales, fonctions locales, classes
//! locales, objets anonymes). Références : appels `f(x)`, `a.b.f(x)`, `Type(x)` (clé = dernier
//! segment, comme `graph::call_key`), types parents d'une classe (`: Base()`, `: Interface`),
//! imports (`import a.b.C [as D]` → spécificateur pointé + nom importé ; les imports Kotlin ne se
//! résolvent pas en fichier : le graphe retombe sur les noms).

use crate::symbol::{tokenize_identifier, CallRef, FileRefs, Symbol, SymbolKind};
use tree_sitter::{Node, Tree};

/// Annotations sans valeur de recherche : jamais préfixées à la signature.
const NOISE_ANNOTATIONS: &[&str] = &[
    "Override",
    "Suppress",
    "SuppressLint",
    "JvmStatic",
    "JvmOverloads",
    "JvmField",
    "JvmName",
    "JvmSuppressWildcards",
    "Throws",
    "OptIn",
    "ExperimentalApi",
    "Deprecated",
    "RequiresApi",
    "Keep",
    "Synchronized",
    "Volatile",
    "Transient",
    "Strictfp",
];

/// Plafonds : annotations préfixées par symbole, longueur d'une annotation.
const MAX_ANNOTATIONS: usize = 3;
const MAX_ANNOTATION_CHARS: usize = 48;

#[derive(Clone, Copy, PartialEq)]
enum Scope {
    /// Corps d'une classe, interface, énumération, objet ou companion (les membres y sont indexés).
    Container,
    /// Fonction, constructeur, accesseur ou propriété : on n'y indexe rien, mais on y relève les appels.
    Opaque,
}

struct Frame {
    depth: usize,
    scope: Scope,
    /// Nom de la classe englobante (celui du constructeur secondaire).
    owner: String,
}

fn txt<'a>(n: Node, src: &'a [u8]) -> &'a str {
    n.utf8_text(src).unwrap_or("")
}

fn child_of_kind<'a>(n: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut c = n.walk();
    let found = n.children(&mut c).find(|k| k.kind() == kind);
    found
}

fn has_child_kind(n: Node, kind: &str) -> bool {
    child_of_kind(n, kind).is_some()
}

/// Début de la signature (octet) : après les annotations de tête, au premier vrai modificateur
/// ou au mot-clé (`fun`, `class`…). `signature_at` ne sait sauter que des lignes entières
/// d'annotations ; en Kotlin `@Test fun f()` tient souvent sur une seule ligne.
fn signature_start(decl: Node) -> usize {
    let mut c = decl.walk();
    // `modifiers` est toujours le premier enfant s'il existe.
    if let Some(k) = decl.children(&mut c).next() {
        if k.kind() == "modifiers" {
            let mut m = k.walk();
            let first_real = k.children(&mut m).find(|x| x.kind() != "annotation");
            return match first_real {
                Some(x) => x.start_byte(),
                None => k.next_sibling().map(|s| s.start_byte()).unwrap_or(k.end_byte()),
            };
        }
    }
    decl.start_byte()
}

/// Annotations utiles de la déclaration, `@Nom` ou `@Nom(args)` compactés.
fn annotations_prefix(decl: Node, src: &[u8]) -> String {
    let Some(mods) = child_of_kind(decl, "modifiers") else { return String::new() };
    let mut out: Vec<String> = Vec::new();
    let mut c = mods.walk();
    for a in mods.children(&mut c).filter(|x| x.kind() == "annotation") {
        let raw = txt(a, src);
        let name = raw.trim_start_matches('@').split(['(', ' ', '\n', '\t']).next().unwrap_or("");
        let name = name.rsplit(':').next().unwrap_or(name); // @get:JvmName → JvmName
        if name.is_empty() || NOISE_ANNOTATIONS.contains(&name) {
            continue;
        }
        let compact = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        let compact: String = if compact.chars().count() > MAX_ANNOTATION_CHARS {
            let cut: String = compact.chars().take(MAX_ANNOTATION_CHARS).collect();
            format!("{}…", cut.trim_end())
        } else {
            compact
        };
        out.push(compact);
        if out.len() >= MAX_ANNOTATIONS {
            break;
        }
    }
    out.join(" ")
}

fn signature(decl: Node, src: &[u8]) -> String {
    let body = crate::extract::signature_at(src, signature_start(decl));
    let ann = annotations_prefix(decl, src);
    if ann.is_empty() {
        body
    } else {
        format!("{} {}", ann, body)
    }
}

fn push_symbol(syms: &mut Vec<Symbol>, src: &[u8], decl: Node, name_node: Node, kind: SymbolKind, name: &str) {
    if name.is_empty() || name.len() > 120 {
        return;
    }
    let line = name_node.start_position().row as u32 + 1;
    syms.push(Symbol {
        name: name.to_string(),
        kind,
        line,
        end_line: (decl.end_position().row as u32 + 1).max(line),
        signature: signature(decl, src),
        tokens: tokenize_identifier(name),
        doc: Vec::new(),
        summary: String::new(),
    });
}

/// Dernier `type_identifier` direct d'un `user_type` (`a.b.Type<X>` → `Type`).
fn user_type_name<'a>(ut: Node<'a>, src: &'a [u8]) -> Option<&'a str> {
    let mut c = ut.walk();
    let last = ut.children(&mut c).filter(|k| k.kind() == "type_identifier").last();
    last.map(|k| txt(k, src))
}

/// Premier `user_type` d'un `delegation_specifier` (`: Base()`, `: Iface<T>`, `: Iface by impl`).
fn delegation_type<'a>(spec: Node<'a>) -> Option<Node<'a>> {
    let mut c = spec.walk();
    let kids: Vec<Node> = spec.children(&mut c).collect();
    for k in kids {
        match k.kind() {
            "user_type" => return Some(k),
            "constructor_invocation" | "explicit_delegation" => {
                if let Some(u) = child_of_kind(k, "user_type") {
                    return Some(u);
                }
            }
            _ => {}
        }
    }
    None
}

/// Nom appelé par un `call_expression` : `f(x)`, `a.b.f(x)`, `a?.f(x)`, `Type(x)`.
fn call_name<'a>(call: Node<'a>, src: &'a [u8]) -> Option<&'a str> {
    let f = call.child(0)?;
    match f.kind() {
        "simple_identifier" => Some(txt(f, src)),
        "navigation_expression" => {
            let mut c = f.walk();
            let suffix = f.children(&mut c).filter(|k| k.kind() == "navigation_suffix").last()?;
            let id = child_of_kind(suffix, "simple_identifier")?;
            Some(txt(id, src))
        }
        _ => None,
    }
}

fn push_call(refs: &mut FileRefs, name: &str, node: Node) {
    if !name.is_empty() && name.len() <= 80 {
        refs.calls.push(CallRef { name: name.to_string(), line: node.start_position().row as u32 + 1 });
    }
}

fn import_of(h: Node, src: &[u8], refs: &mut FileRefs) {
    let Some(id) = child_of_kind(h, "identifier") else { return };
    let path: String = txt(id, src).split_whitespace().collect();
    if path.is_empty() {
        return;
    }
    let wildcard = has_child_kind(h, "wildcard_import");
    if wildcard {
        refs.imports.push(path);
        return;
    }
    if let Some(last) = path.rsplit('.').next() {
        refs.imported_names.push(last.to_string());
    }
    refs.imports.push(path);
}

/// Symboles et références d'un fichier Kotlin.
pub fn extract(source: &str, tree: Option<&Tree>) -> (Vec<Symbol>, FileRefs) {
    let Some(tree) = tree else { return (Vec::new(), FileRefs::default()) };
    let src = source.as_bytes();
    let mut syms: Vec<Symbol> = Vec::new();
    let mut refs = FileRefs::default();
    let mut frames: Vec<Frame> = Vec::new();

    // Parcours itératif en préordre (les expressions en chaîne donnent des arbres profonds).
    let mut cursor = tree.walk();
    let mut depth = 0usize;
    'walk: loop {
        let node = cursor.node();
        while frames.last().is_some_and(|f| f.depth >= depth) {
            frames.pop();
        }
        // Contexte d'indexation : premier niveau ou membre d'un conteneur ; jamais dans un corps.
        let (top_level, in_container, owner) = match frames.last() {
            None => (true, false, String::new()),
            Some(f) => (false, f.scope == Scope::Container, f.owner.clone()),
        };
        let indexable = top_level || in_container;
        let kind = node.kind();
        match kind {
            "import_header" => import_of(node, src, &mut refs),
            "delegation_specifier" => {
                if let Some(ut) = delegation_type(node) {
                    if let Some(n) = user_type_name(ut, src) {
                        push_call(&mut refs, n, ut);
                    }
                }
            }
            "call_expression" => {
                if let Some(n) = call_name(node, src) {
                    push_call(&mut refs, n, node);
                }
            }
            "class_declaration" | "object_declaration" if indexable => {
                if let Some(id) = child_of_kind(node, "type_identifier") {
                    let name = txt(id, src);
                    let k = if kind == "object_declaration" {
                        SymbolKind::Class
                    } else if has_child_kind(node, "interface") {
                        SymbolKind::Interface
                    } else if has_child_kind(node, "enum") {
                        SymbolKind::Enum
                    } else {
                        SymbolKind::Class
                    };
                    push_symbol(&mut syms, src, node, id, k, name);
                    frames.push(Frame { depth, scope: Scope::Container, owner: name.to_string() });
                }
            }
            "companion_object" if indexable => {
                match child_of_kind(node, "type_identifier") {
                    Some(id) => {
                        let name = txt(id, src);
                        push_symbol(&mut syms, src, node, id, SymbolKind::Class, name);
                        frames.push(Frame { depth, scope: Scope::Container, owner: name.to_string() });
                    }
                    // Sans nom : les membres restent ceux de la classe englobante.
                    None => frames.push(Frame { depth, scope: Scope::Container, owner }),
                }
            }
            "function_declaration" => {
                if indexable {
                    if let Some(id) = child_of_kind(node, "simple_identifier") {
                        let k = if in_container { SymbolKind::Method } else { SymbolKind::Function };
                        push_symbol(&mut syms, src, node, id, k, txt(id, src));
                    }
                }
                frames.push(Frame { depth, scope: Scope::Opaque, owner });
            }
            "secondary_constructor" => {
                if in_container && !owner.is_empty() {
                    if let Some(kw) = child_of_kind(node, "constructor") {
                        push_symbol(&mut syms, src, node, kw, SymbolKind::Method, &owner);
                    }
                }
                frames.push(Frame { depth, scope: Scope::Opaque, owner });
            }
            "property_declaration" => {
                if indexable {
                    let is_const = child_of_kind(node, "modifiers").is_some_and(|m| txt(m, src).split_whitespace().any(|w| w == "const"));
                    if top_level || is_const {
                        if let Some(vd) = child_of_kind(node, "variable_declaration") {
                            if let Some(id) = child_of_kind(vd, "simple_identifier") {
                                push_symbol(&mut syms, src, node, id, SymbolKind::Const, txt(id, src));
                            }
                        }
                    }
                }
                frames.push(Frame { depth, scope: Scope::Opaque, owner });
            }
            "type_alias" if indexable => {
                if let Some(id) = child_of_kind(node, "type_identifier") {
                    push_symbol(&mut syms, src, node, id, SymbolKind::Type, txt(id, src));
                }
            }
            "enum_entry" => {
                if in_container {
                    if let Some(id) = child_of_kind(node, "simple_identifier") {
                        push_symbol(&mut syms, src, node, id, SymbolKind::Const, txt(id, src));
                    }
                }
                frames.push(Frame { depth, scope: Scope::Opaque, owner });
            }
            // Accesseurs, blocs init et classes anonymes : le contenu est du code, pas des membres.
            "getter" | "setter" | "anonymous_initializer" | "object_literal" => {
                frames.push(Frame { depth, scope: Scope::Opaque, owner });
            }
            _ => {}
        }

        // Descente / navigation.
        if cursor.goto_first_child() {
            depth += 1;
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                continue 'walk;
            }
            if !cursor.goto_parent() {
                break 'walk;
            }
            depth -= 1;
        }
    }
    // Un symbole du même package (ou du même module) s'utilise SANS import en Kotlin : le graphe
    // n'attache un appel que si son nom est « importé » ou défini dans le fichier. Comme pour C/C++,
    // tout nom appelé compte donc comme visible ; la résolution tranche ensuite (unique = lien,
    // plusieurs = ambigu).
    let called: Vec<String> = refs.calls.iter().map(|c| c.name.clone()).collect();
    refs.imported_names.extend(called);
    let mut seen = std::collections::HashSet::new();
    refs.imported_names.retain(|n| seen.insert(n.clone()));
    let mut seen_calls = std::collections::HashSet::new();
    refs.calls.retain(|c| seen_calls.insert((c.name.clone(), c.line)));
    let mut seen_imports = std::collections::HashSet::new();
    refs.imports.retain(|n| seen_imports.insert(n.clone()));
    (syms, refs)
}

#[cfg(test)]
mod tests {
    use crate::extract::{extract_comments, extract_symbols, extract_symbols_and_refs};
    use crate::lang::Lang;
    use crate::symbol::{Symbol, SymbolKind};

    const SRC: &str = r#"package com.example.app

import kotlinx.coroutines.flow.Flow
import com.example.util.*
import com.example.net.Client as HttpClient

/** Utilisateur de l'application. */
@Serializable
data class User(val id: Long, val name: String, var email: String? = null) {
    constructor(name: String) : this(0, name)
    fun greet(): String = "Hello $name"
    val upper: String get() = name.uppercase()
    companion object Factory {
        const val MAX = 10
        fun create(n: String): User = User(n)
    }
}

sealed class Result<out T> {
    data class Ok<T>(val v: T) : Result<T>()
    object Fail : Result<Nothing>()
}

interface Repo<T> {
    fun find(id: Long): T?
    suspend fun save(item: T) { println("default") }
    val size: Int
}

enum class Color { RED, GREEN }

object Registry : Repo<User> {
    override fun find(id: Long): User? = null
    private val items = mutableListOf<User>()
}

typealias Handler = (String) -> Unit

val topLevel = 5
var counter: Int = 0

fun String.shout(): String = this.uppercase() + "!"
fun <T> List<T>.second(): T = this[1]

suspend fun fetch(url: String): String {
    val local = 3
    fun inner() = local
    listOf(1, 2).map { it + 1 }.forEach { println(it) }
    return client.get(url).body()
}

fun main() {
    val u = User("a")
    u.greet()
    greetAll(u)
}
"#;

    fn find<'a>(syms: &'a [Symbol], name: &str) -> &'a Symbol {
        syms.iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("symbole {} absent : {:?}", name, syms.iter().map(|s| &s.name).collect::<Vec<_>>()))
    }

    fn kind_of(syms: &[Symbol], name: &str) -> SymbolKind {
        find(syms, name).kind
    }

    #[test]
    fn classes_interfaces_enums_objets() {
        let syms = extract_symbols(Lang::Kotlin, SRC);
        assert_eq!(kind_of(&syms, "User"), SymbolKind::Class);
        assert_eq!(kind_of(&syms, "Result"), SymbolKind::Class);
        assert_eq!(kind_of(&syms, "Ok"), SymbolKind::Class);
        assert_eq!(kind_of(&syms, "Fail"), SymbolKind::Class);
        assert_eq!(kind_of(&syms, "Repo"), SymbolKind::Interface);
        assert_eq!(kind_of(&syms, "Color"), SymbolKind::Enum);
        assert_eq!(kind_of(&syms, "Registry"), SymbolKind::Class);
        assert_eq!(kind_of(&syms, "Handler"), SymbolKind::Type);
    }

    #[test]
    fn data_class_signature_et_portee() {
        let syms = extract_symbols(Lang::Kotlin, SRC);
        let u = find(&syms, "User");
        assert_eq!(u.signature, "@Serializable data class User(val id: Long, val name: String, var email: String? = null)");
        assert!(u.end_line > u.line + 5, "la portée couvre la classe entière");
        assert_eq!(u.line, 9);
    }

    #[test]
    fn companion_nomme_et_const() {
        let syms = extract_symbols(Lang::Kotlin, SRC);
        assert_eq!(kind_of(&syms, "Factory"), SymbolKind::Class);
        assert_eq!(kind_of(&syms, "MAX"), SymbolKind::Const);
        assert_eq!(kind_of(&syms, "create"), SymbolKind::Method);
    }

    #[test]
    fn companion_sans_nom_n_est_pas_un_symbole() {
        let src = "class Box {\n    companion object {\n        fun make(): Box = Box()\n        const val SIZE = 3\n    }\n}\n";
        let syms = extract_symbols(Lang::Kotlin, src);
        assert!(syms.iter().all(|s| s.name != "Companion"));
        assert_eq!(kind_of(&syms, "make"), SymbolKind::Method);
        assert_eq!(kind_of(&syms, "SIZE"), SymbolKind::Const);
    }

    #[test]
    fn methodes_fonctions_et_extensions() {
        let syms = extract_symbols(Lang::Kotlin, SRC);
        assert_eq!(kind_of(&syms, "greet"), SymbolKind::Method);
        assert_eq!(kind_of(&syms, "find"), SymbolKind::Method);
        assert_eq!(kind_of(&syms, "save"), SymbolKind::Method);
        assert_eq!(kind_of(&syms, "fetch"), SymbolKind::Function);
        assert_eq!(kind_of(&syms, "main"), SymbolKind::Function);
        // Extensions : nom court, récepteur dans la signature.
        assert_eq!(kind_of(&syms, "shout"), SymbolKind::Function);
        assert_eq!(find(&syms, "shout").signature, "fun String.shout(): String = this.uppercase() + \"!\"");
        assert_eq!(find(&syms, "second").signature, "fun <T> List<T>.second(): T = this[1]");
    }

    #[test]
    fn suspend_et_interface_avec_defaut() {
        let syms = extract_symbols(Lang::Kotlin, SRC);
        assert_eq!(find(&syms, "save").signature, "suspend fun save(item: T)");
        assert_eq!(find(&syms, "fetch").signature, "suspend fun fetch(url: String): String");
        // L'interface a deux méthodes (une abstraite, une avec corps) ; la propriété membre n'est pas indexée.
        assert!(syms.iter().all(|s| s.name != "size"));
    }

    #[test]
    fn proprietes_premier_niveau_seulement() {
        let syms = extract_symbols(Lang::Kotlin, SRC);
        assert_eq!(kind_of(&syms, "topLevel"), SymbolKind::Const);
        assert_eq!(kind_of(&syms, "counter"), SymbolKind::Const);
        assert_eq!(find(&syms, "counter").signature, "var counter: Int = 0");
        // Membres non const et variables locales : absents.
        for absent in ["upper", "items", "local", "inner", "it"] {
            assert!(syms.iter().all(|s| s.name != absent), "{} ne doit pas être indexé", absent);
        }
    }

    #[test]
    fn constructeur_secondaire_et_entrees_d_enum() {
        let syms = extract_symbols(Lang::Kotlin, SRC);
        let ctors: Vec<_> = syms.iter().filter(|s| s.name == "User" && s.kind == SymbolKind::Method).collect();
        assert_eq!(ctors.len(), 1);
        assert_eq!(ctors[0].line, 10);
        assert_eq!(kind_of(&syms, "RED"), SymbolKind::Const);
        assert_eq!(kind_of(&syms, "GREEN"), SymbolKind::Const);
    }

    #[test]
    fn appels_imports_et_types_parents() {
        let (_, refs) = extract_symbols_and_refs(Lang::Kotlin, SRC);
        let calls: Vec<&str> = refs.calls.iter().map(|c| c.name.as_str()).collect();
        for want in ["uppercase", "User", "greet", "greetAll", "map", "forEach", "println", "get", "body", "mutableListOf"] {
            assert!(calls.contains(&want), "appel {} manquant : {:?}", want, calls);
        }
        // Types parents : `Result<T>()` et `Repo<User>` (utilisables pour « qui étend X »).
        assert!(calls.contains(&"Result") && calls.contains(&"Repo"));
        assert!(refs.imports.contains(&"kotlinx.coroutines.flow.Flow".to_string()));
        assert!(refs.imports.contains(&"com.example.util".to_string()), "import avec *");
        assert!(refs.imports.contains(&"com.example.net.Client".to_string()), "import avec alias");
        assert!(refs.imported_names.contains(&"Flow".to_string()));
        assert!(refs.imported_names.contains(&"Client".to_string()));
        // L'appel rattaché à sa ligne : `greetAll(u)` est dans `main`.
        let (syms, refs2) = extract_symbols_and_refs(Lang::Kotlin, SRC);
        let main = find(&syms, "main");
        let c = refs2.calls.iter().find(|c| c.name == "greetAll").unwrap();
        assert!(c.line >= main.line && c.line <= main.end_line);
    }

    #[test]
    fn noms_appeles_visibles_sans_import() {
        // Même package : `helperDuPackage` n'est jamais importé, mais doit rester résoluble.
        let src = "package p\nprivate val F: S = { a -> helperDuPackage(a) }\n";
        let (_, refs) = extract_symbols_and_refs(Lang::Kotlin, src);
        assert!(refs.imported_names.contains(&"helperDuPackage".to_string()));
        assert!(refs.calls.iter().any(|c| c.name == "helperDuPackage" && c.line == 2));
    }

    #[test]
    fn lambdas_et_chaines_ne_creent_pas_de_symboles() {
        let src = "fun run() {\n    items.filter { it.ok }.map { x -> x.name }.forEach { println(it) }\n    val f = { a: Int -> a * 2 }\n    f(3)\n}\n";
        let (syms, refs) = extract_symbols_and_refs(Lang::Kotlin, src);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "run");
        let calls: Vec<&str> = refs.calls.iter().map(|c| c.name.as_str()).collect();
        assert!(calls.contains(&"filter") && calls.contains(&"map") && calls.contains(&"forEach") && calls.contains(&"f"));
    }

    #[test]
    fn annotations_utiles_en_tete_de_signature() {
        let src = "class UiTest {\n    @Test fun shows_title() {}\n    @Composable\n    @Suppress(\"unused\")\n    fun Screen(vm: Vm) {}\n    @GET(\"/users/{id}\") suspend fun user(@Path(\"id\") id: Long): User\n}\n";
        let syms = extract_symbols(Lang::Kotlin, src);
        assert_eq!(find(&syms, "shows_title").signature, "@Test fun shows_title()");
        assert_eq!(find(&syms, "Screen").signature, "@Composable fun Screen(vm: Vm)");
        assert_eq!(find(&syms, "user").signature, "@GET(\"/users/{id}\") suspend fun user(@Path(\"id\") id: Long): User");
    }

    #[test]
    fn kdoc_et_en_tete_de_fichier() {
        let src = "/*\n * Licence Apache 2.0.\n */\npackage x.y\n\nimport a.B\n\n/**\n * Calcule le total de la commande.\n */\n@Deprecated(\"x\")\nfun total(): Int = 1\n";
        let mut syms = extract_symbols(Lang::Kotlin, src);
        let (header, _role) = extract_comments(Lang::Kotlin, src, &mut syms);
        assert!(header.contains(&"licence".to_string()));
        let t = find(&syms, "total");
        assert!(t.doc.contains(&"calcule".to_string()), "doc : {:?}", t.doc);
        assert_eq!(t.summary, "Calcule le total de la commande.");
    }

    #[test]
    fn script_kts_et_objets_anonymes() {
        let src = "plugins {\n    id(\"java\")\n}\nval x = object : Runnable {\n    override fun run() {}\n}\nfun helper() = 1\n";
        assert_eq!(Lang::from_path("build.gradle.kts"), Lang::Kotlin);
        let syms = extract_symbols(Lang::Kotlin, src);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["x", "helper"], "l'objet anonyme n'expose pas `run`");
    }

    #[test]
    fn fichier_vide_ou_invalide_ne_panique_pas() {
        assert!(extract_symbols(Lang::Kotlin, "").is_empty());
        let _ = extract_symbols_and_refs(Lang::Kotlin, "fun (((( class { }}} val");
    }
}
