//! Serveur MCP (Model Context Protocol) — rend Cortex appelable nativement par
//! n'importe quel assistant IA via les outils `mcp__cortex__*`.
//!
//! Implémentation directe du protocole (JSON-RPC 2.0 sur stdio) — zéro dépendance
//! lourde, robuste sur le toolchain MinGW. Synchrone (stdio = pas besoin d'async).
//!
//! Tools exposés (mêmes noms que la CLI, préfixe `cortex_`, voir `outils`) :
//!   - cortex_find / card / outline / read / overview / impact / path / changed
//!   - cortex_query, cortex_explain, cortex_context : alias compatibles
//!   - cortex_grep      : plein-texte dans les fichiers indexés
//!   - cortex_ui        : de ce qu'on voit à l'écran (texte, data-testid, composant) au code qui l'affiche
//!   - cortex_files     : fichiers par nom/fragment/glob
//!   - cortex_docs(_list): documentations externes scrapées offline
//!   - cortex_list      : projets indexés

use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::time::Instant;

const PROTOCOL_VERSION: &str = "2024-11-05";

// Le serveur MCP interroge l'ATLAS (mmap, <1ms d'ouverture mesuré par
// `bench-latence`) à chaque appel : pas de cache d'index en mémoire. Seul le CONTRÔLE DE FRAÎCHEUR (stat() de chaque
// fichier connu + `git status`) reste borné dans le temps (`should_refresh`) :
// une session MCP enchaîne des dizaines d'appels en quelques secondes, refaire
// ce contrôle à CHAQUE appel serait un coût répété pour rien.
thread_local! {
    /// Horodatage du dernier contrôle de fraîcheur PAR PROJET — voir `should_refresh`.
    static LAST_REFRESH: RefCell<HashMap<String, Instant>> = RefCell::new(HashMap::new());
}

/// Intervalle minimal entre deux contrôles de fraîcheur pour un même projet côté
/// MCP. Le serveur reste vivant toute une session de travail et enchaîne des
/// dizaines d'appels d'outils en quelques secondes ; refaire un `stat()` de
/// chaque fichier connu à CHAQUE appel serait un coût répété pour rien. Un
/// agent qui vient d'écrire un fichier le voit au prochain appel passé cet
/// intervalle — largement assez réactif pour un usage interactif.
const MCP_REFRESH_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Vrai si ça fait plus de `MCP_REFRESH_MIN_INTERVAL` que ce projet a été
/// vérifié (et marque l'instant courant comme dernier contrôle dans ce cas).
fn should_refresh(name: &str) -> bool {
    LAST_REFRESH.with(|m| {
        let mut m = m.borrow_mut();
        let now = Instant::now();
        match m.get(name) {
            Some(t) if now.duration_since(*t) < MCP_REFRESH_MIN_INTERVAL => false,
            _ => {
                m.insert(name.to_string(), now);
                true
            }
        }
    })
}

/// Ouvre l'atlas d'un projet (migre/reconstruit si besoin, voir
/// `atlas::ensure_and_open`) et applique le contrôle de fraîcheur automatique
/// (même mécanisme que la CLI, `atlas::fresh::refresh` — lecture des dossiers puis segment
/// delta), borné par `MCP_REFRESH_MIN_INTERVAL` pour ne pas payer son coût à
/// chaque appel.
fn open_atlas(name: &str) -> Option<crate::atlas::Handle> {
    let mut h = match crate::atlas::ensure_and_open(name) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("[cortex] atlas '{}' unavailable ({})", name, e);
            return None;
        }
    };
    if should_refresh(name) {
        crate::atlas::fresh::refresh(&mut h);
    }
    Some(h)
}

/// Ouvre l'atlas de chaque projet demandé (un seul si `project` est donné,
/// sinon tous les projets actifs) — pont MCP équivalent à `main::atlas_handles`.
fn open_atlas_handles(project: &Option<String>) -> Vec<crate::atlas::Handle> {
    match project {
        Some(name) => open_atlas(name).into_iter().collect(),
        None => {
            let cfg = crate::config::Config::load();
            let home = crate::index::cortex_home();
            let mut v = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&home) {
                for e in entries.flatten() {
                    let n = e.file_name().to_string_lossy().to_string();
                    if crate::atlas::is_project_dir(&e.path()) && cfg.is_enabled(&n) {
                        if let Some(h) = open_atlas(&n) {
                            v.push(h);
                        }
                    }
                }
            }
            v
        }
    }
}

/// Lance le serveur MCP sur stdin/stdout (bloquant).
pub fn serve() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut line = String::new();

    loop {
        line.clear();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(_) => break,
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(resp) = handle(&req) {
            let _ = writeln!(stdout, "{}", resp);
            let _ = stdout.flush();
        }
        // Les notifications (sans id) ne reçoivent pas de réponse.
    }
}

fn handle(req: &Value) -> Option<String> {
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let id = req.get("id").cloned();
    // Notification (pas d'id) → pas de réponse.
    let is_notification = id.is_none();

    let result: Result<Value, (i64, String)> = match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "cortex", "version": env!("CARGO_PKG_VERSION") }
        })),
        "tools/list" => Ok(tools_list()),
        "tools/call" => tools_call(req.get("params")),
        "ping" => Ok(json!({})),
        _ if method.starts_with("notifications/") => return None,
        _ => Err((-32601, format!("unknown method '{}'", method))),
    };

    if is_notification {
        return None;
    }

    let resp = match result {
        Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
        Err((code, msg)) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": msg } }),
    };
    Some(resp.to_string())
}

/// Schéma d'entrée commun des outils pour agents : une entrée + projet + budget.
fn schema(champ: &str, desc: &str, extra: Value) -> Value {
    let mut props = json!({
        champ: { "type": "string", "description": desc },
        "project": { "type": "string", "description": "Limit to one project (optional; default: all active projects)" },
        "budget": { "type": "integer", "description": "Output budget in tokens (about characters/4; optional)" }
    });
    if let (Some(p), Some(e)) = (props.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            p.insert(k.clone(), v.clone());
        }
    }
    json!({ "type": "object", "properties": props, "required": [champ] })
}

const ID_DESC: &str = "Id copied from a Cortex output (S:path#symbol, D:path#anchor, F:path), or a symbol name, path, unique path suffix (useX.ts), path:line";

const REGLE: &str = " Output: stable ids to copy as-is into the next call, provenance (path + L<start>-<end>), ✎ = uncommitted, last line \"next : \" = the most useful call to make next.";

fn tools_list() -> Value {
    json!({
        "tools": [
            {
                "name": "cortex_find",
                "description": format!("WHERE IS X? Finds symbols (functions, components, hooks, classes, types, doc headings) for a natural-language question (FR/EN) or keywords. BM25F ranking over names (camelCase/snake split, typo tolerant), paths, file headers, doc-comments and file bodies, FR/EN stemming and FR<->EN synonyms. Use BEFORE grep/find/reading files.{}", REGLE),
                "inputSchema": schema("question", "Question or keywords (FR or EN)", json!({}))
            },
            {
                "name": "cortex_card",
                "description": format!("WHAT IS IT? Card of a symbol in one call, without reading its file: signature, role (doc-comment), what it calls, who calls it (with the call line), how many files import its file, related tests, homonyms. A file as input gives its outline.{}", REGLE),
                "inputSchema": schema("cible", ID_DESC, json!({}))
            },
            {
                "name": "cortex_outline",
                "description": format!("WHAT DOES THIS FILE CONTAIN? Role, imported files and importers, nested symbols with line ranges, exports, instead of reading the whole file.{}", REGLE),
                "inputSchema": schema("cible", "File: F:path, path, unique suffix (useX.ts), or a symbol (its file)", json!({}))
            },
            {
                "name": "cortex_read",
                "description": format!("SHOW THE CODE: the exact lines of a symbol (or a doc section, a file, a path:12-40 range), numbered, read from disk, instead of a Read at a guessed offset.{}", REGLE),
                "inputSchema": schema("cible", ID_DESC, json!({ "contexte": { "type": "integer", "description": "Context lines on each side (default 0)" } }))
            },
            {
                "name": "cortex_overview",
                "description": format!("HOW DOES THIS MODULE WORK? For a folder: files and roles, entry points (files imported from outside), outgoing and incoming dependencies per folder, external packages.{}", REGLE),
                "inputSchema": schema("dossier", "Folder relative to the project root (e.g. src/auth; '.' = whole project)", json!({}))
            },
            {
                "name": "cortex_impact",
                "description": format!("WHAT BREAKS IF I CHANGE THIS? Transitive dependents of a symbol (callers resolved through imports, with the call line, and files that import the symbol without calling it) or of a file (importers), by depth, and the tests to re-run. Dynamic calls (import(), strings) are not seen.{}", REGLE),
                "inputSchema": schema("cible", ID_DESC, json!({ "profondeur": { "type": "integer", "description": "Depth 1 to 6 (default 3)" } }))
            },
            {
                "name": "cortex_path",
                "description": format!("HOW DOES A REACH B? Shortest call path between two symbols or files (else import path between their files; else in the reverse direction), with the line of each call.{}", REGLE),
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "de": { "type": "string", "description": ID_DESC },
                        "vers": { "type": "string", "description": ID_DESC },
                        "project": { "type": "string", "description": "Limit to one project (optional)" },
                        "budget": { "type": "integer", "description": "Output budget in tokens (optional)" }
                    },
                    "required": ["de", "vers"]
                }
            },
            {
                "name": "cortex_changed",
                "description": format!("WHAT DID I CHANGE? Uncommitted work (git status + git diff HEAD): files, touched functions, their callers outside the work in progress and the tests to re-run.{}", REGLE),
                "inputSchema": { "type": "object", "properties": { "project": { "type": "string", "description": "Limit to one project (optional)" }, "budget": { "type": "integer", "description": "Output budget in tokens (optional)" } } }
            },
            {
                "name": "cortex_query",
                "description": "Alias of cortex_find (compatibility): same 'question' input, same output.",
                "inputSchema": schema("question", "Question or keywords (FR or EN)", json!({}))
            },
            {
                "name": "cortex_explain",
                "description": "Alias of cortex_card (compatibility): 'symbol' input. 'depth' is accepted and ignored; for transitive dependents, use cortex_impact.",
                "inputSchema": schema("symbol", ID_DESC, json!({ "depth": { "type": "integer", "description": "Ignored (compatibility)" } }))
            },
            {
                "name": "cortex_context",
                "description": "Alias of cortex_card (compatibility) that adds the docs (docs/**/*.md) citing the symbol or its file. 'symbol' input.",
                "inputSchema": schema("symbol", ID_DESC, json!({}))
            },
            {
                "name": "cortex_grep",
                "description": "EXACT text (literal substring, case-insensitive by default) in the content of indexed files: error messages, i18n keys, URLs, TODO, table names. Multithreaded, never node_modules; results grouped by file with the enclosing symbol of each line (S: id). For a symbol or a question, prefer cortex_find.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "needle": { "type": "string", "description": "Literal string to look for" },
                        "project": { "type": "string", "description": "Limit to one project (optional)" },
                        "case_sensitive": { "type": "boolean", "description": "Case sensitive (default false)" },
                        "max": { "type": "integer", "description": "Max lines (default 60)" },
                        "budget": { "type": "integer", "description": "Approximate token budget (default 1500)" }
                    },
                    "required": ["needle"]
                }
            },
            {
                "name": "cortex_ui",
                "description": "WHERE IS THIS UI? From what you SEE on screen (button text, data-testid, aria-label, id, React component names) to the code that renders it. Resolves i18n keys and their usages, ignores comments/docs/tests, ranks real screens above design-system primitives. One line per candidate: stable id, file:line, why.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "text": { "type": "string", "description": "Visible text (button label, heading…)" },
                        "testid": { "type": "string", "description": "data-testid / data-test / data-cy value" },
                        "aria": { "type": "string", "description": "aria-label / title / placeholder value" },
                        "id": { "type": "string", "description": "DOM id" },
                        "component": { "type": "string", "description": "React component names, nearest first, comma-separated" },
                        "project": { "type": "string", "description": "Limit to one project (optional)" },
                        "max": { "type": "integer", "description": "Max candidates (default 6)" }
                    }
                }
            },
            {
                "name": "cortex_files",
                "description": "Files by path fragment (case-insensitive) or glob ('**/*.test.ts'), read from the atlas without touching the disk. The paths returned are accepted as-is by the other tools.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Name/path fragment, or glob if '*'/'?' is present" },
                        "project": { "type": "string", "description": "Limit to one project (optional)" },
                        "max": { "type": "integer", "description": "Max results (default 80)" }
                    },
                    "required": ["pattern"]
                }
            },
            {
                "name": "cortex_docs",
                "description": "Searches the external documentation scraped locally (React, Rust, PostgreSQL…: `cortex docs add`): offline answers. Use before WebFetch/WebSearch for an already scraped library (list: cortex_docs_list).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "question": { "type": "string", "description": "Question / keywords" },
                        "source": { "type": "string", "description": "Limit to one doc (e.g. Tauri, React). Optional." },
                        "budget": { "type": "integer", "description": "Approximate token budget (default 800)" }
                    },
                    "required": ["question"]
                }
            },
            {
                "name": "cortex_docs_list",
                "description": "Lists the external documentation scraped locally and its page count.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "cortex_list",
                "description": "Lists the projects indexed by Cortex (files, symbols).",
                "inputSchema": { "type": "object", "properties": {} }
            }
        ]
    })
}

fn arg_str(args: &Value, k: &str) -> String {
    args.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string()
}

/// Un outil pour agents (`outils`) côté MCP.
fn run_outil(args: &Value, appel: crate::outils::Appel) -> String {
    let project = args.get("project").and_then(|v| v.as_str()).map(String::from);
    let budget = args.get("budget").and_then(|v| v.as_u64()).map(|b| b as usize);
    let handles = open_atlas_handles(&project);
    crate::outils::executer(&handles, &appel, budget)
}

fn tools_call(params: Option<&Value>) -> Result<Value, (i64, String)> {
    use crate::outils::Appel;
    let params = params.ok_or((-32602, "missing params".into()))?;
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let text = match name {
        "cortex_find" | "cortex_query" => run_outil(&args, Appel::Find { question: arg_str(&args, "question") }),
        "cortex_card" => run_outil(&args, Appel::Card { cible: arg_str(&args, "cible") }),
        "cortex_explain" => run_outil(&args, Appel::Card { cible: arg_str(&args, "symbol") }),
        "cortex_context" => run_outil(&args, Appel::Context { cible: arg_str(&args, "symbol") }),
        "cortex_outline" => run_outil(&args, Appel::Outline { cible: arg_str(&args, "cible") }),
        "cortex_read" => {
            let c = args.get("contexte").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            run_outil(&args, Appel::Read { cible: arg_str(&args, "cible"), contexte: c })
        }
        "cortex_overview" => run_outil(&args, Appel::Overview { dossier: arg_str(&args, "dossier") }),
        "cortex_impact" => {
            let d = args.get("profondeur").and_then(|v| v.as_u64()).unwrap_or(3) as usize;
            run_outil(&args, Appel::Impact { cible: arg_str(&args, "cible"), profondeur: d })
        }
        "cortex_path" => run_outil(&args, Appel::Path { de: arg_str(&args, "de"), vers: arg_str(&args, "vers") }),
        "cortex_changed" => run_outil(&args, Appel::Changed),
        "cortex_grep" => {
            let needle = args.get("needle").and_then(|v| v.as_str()).unwrap_or("");
            let project = args.get("project").and_then(|v| v.as_str()).map(String::from);
            let cs = args.get("case_sensitive").and_then(|v| v.as_bool()).unwrap_or(false);
            let max = args.get("max").and_then(|v| v.as_u64()).unwrap_or(60) as usize;
            let budget = args.get("budget").and_then(|v| v.as_u64()).unwrap_or(1500) as usize;
            run_grep(needle, project, cs, max, budget)
        }
        #[cfg(not(feature = "ui"))]
        "cortex_ui" => {
            let get = |k: &str| args.get(k).and_then(|v| v.as_str()).map(String::from);
            let mut cli = vec!["ui".to_string(), get("text").unwrap_or_default()];
            for k in ["testid", "aria", "id"] {
                for v in args.get(k).and_then(|v| v.as_array()).into_iter().flatten().filter_map(|v| v.as_str()) {
                    cli.push(format!("--{k}"));
                    cli.push(v.to_string());
                }
            }
            if let Some(c) = get("component") {
                cli.push("--component".into());
                cli.push(c);
            }
            if let Some(p) = get("project") {
                cli.push("--project".into());
                cli.push(p);
            }
            cli.push("--max".into());
            cli.push(args.get("max").and_then(|v| v.as_u64()).unwrap_or(6).to_string());
            cli.push("--no-refresh".into());
            crate::sortie_cortex_ui(&cli)
        }
        #[cfg(feature = "ui")]
        "cortex_ui" => {
            let get = |k: &str| args.get(k).and_then(|v| v.as_str()).map(String::from);
            let one = |k: &str| get(k).into_iter().collect::<Vec<_>>();
            let q = crate::ui_locate::UiQuery {
                text: get("text").unwrap_or_default(),
                testid: one("testid"),
                aria: one("aria"),
                id: one("id"),
                component: get("component").map(|c| c.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()).unwrap_or_default(),
            };
            let max = args.get("max").and_then(|v| v.as_u64()).unwrap_or(6) as usize;
            let handles = open_atlas_handles(&get("project"));
            if handles.is_empty() {
                "(cortex) no index.".to_string()
            } else {
                crate::ui_locate::run_ui_on(&handles, &q, max)
            }
        }
        "cortex_files" => {
            let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            let project = args.get("project").and_then(|v| v.as_str()).map(String::from);
            let max = args.get("max").and_then(|v| v.as_u64()).unwrap_or(80) as usize;
            run_files(pattern, project, max)
        }
        "cortex_docs" => {
            let q = args.get("question").and_then(|v| v.as_str()).unwrap_or("");
            let source = args.get("source").and_then(|v| v.as_str());
            let budget = args.get("budget").and_then(|v| v.as_u64()).unwrap_or(800) as usize;
            crate::scrape::search_docs(q, source, budget)
        }
        "cortex_docs_list" => {
            let docs = crate::scrape::list_docs();
            if docs.is_empty() {
                "(no scraped doc: cortex docs add <url> --name <name>)".to_string()
            } else {
                let mut s = String::from("Offline docs available:\n");
                for (name, pages) in docs {
                    s.push_str(&format!("- {}: {} pages\n", name, pages));
                }
                s
            }
        }
        "cortex_list" => run_list(),
        _ => return Err((-32602, format!("unknown tool '{}'", name))),
    };

    Ok(json!({ "content": [ { "type": "text", "text": text } ] }))
}

// ─── Implémentations (toutes sur l'atlas) ───────────────────────────────────

fn run_grep(needle: &str, project: Option<String>, case_sensitive: bool, max: usize, budget: usize) -> String {
    let handles = open_atlas_handles(&project);
    if handles.is_empty() {
        return "(cortex) no index.".into();
    }
    crate::run_grep_on(&handles, needle, case_sensitive, max, budget).0
}

fn run_files(pattern: &str, project: Option<String>, max: usize) -> String {
    let handles = open_atlas_handles(&project);
    if handles.is_empty() {
        return "(cortex) no index.".into();
    }
    crate::run_files_on(&handles, pattern, max).0
}

fn run_list() -> String {
    let home = crate::index::cortex_home();
    let mut out = String::from("Indexed projects:\n");
    if let Ok(entries) = std::fs::read_dir(&home) {
        for e in entries.flatten() {
            if crate::atlas::is_project_dir(&e.path()) {
                let n = e.file_name().to_string_lossy().to_string();
                if let Some(h) = open_atlas(&n) {
                    let (nf, ns, _) = h.counts();
                    out.push_str(&format!("- {}: {} files, {} symbols\n", n, nf, ns));
                }
            }
        }
    }
    out
}
