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
    let mut h = crate::atlas::ensure_and_open(name).ok()?;
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
        _ => Err((-32601, format!("method '{}' inconnue", method))),
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
        "project": { "type": "string", "description": "Limiter à un projet (optionnel ; défaut : tous les projets actifs)" },
        "budget": { "type": "integer", "description": "Budget de sortie en tokens (≈ caractères/4 ; optionnel)" }
    });
    if let (Some(p), Some(e)) = (props.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            p.insert(k.clone(), v.clone());
        }
    }
    json!({ "type": "object", "properties": props, "required": [champ] })
}

const ID_DESC: &str = "Identifiant copié d'une sortie Cortex (S:chemin#symbole, D:chemin#ancre, F:chemin), ou nom de symbole, chemin, suffixe de chemin unique (useX.ts), chemin:ligne";

const REGLE: &str = " Sorties : identifiants stables à recopier tels quels dans l'appel suivant, provenance (chemin + L<début>-<fin>), ✎ = non commité, dernière ligne « suite : » = l'appel le plus utile ensuite.";

fn tools_list() -> Value {
    json!({
        "tools": [
            {
                "name": "cortex_find",
                "description": format!("OÙ EST X ? Trouve les symboles (fonctions, composants, hooks, classes, types, titres de doc) pour une question en langage naturel FR/EN ou des mots-clés. Classement BM25F sur les noms (camelCase/snake découpés, fautes tolérées), les chemins, les en-têtes de fichier, les doc-comments et le corps des fichiers, racinisation FR/EN et synonymes FR↔EN. À utiliser AVANT grep/find/lecture de fichiers.{}", REGLE),
                "inputSchema": schema("question", "Question ou mots-clés (FR ou EN)", json!({}))
            },
            {
                "name": "cortex_card",
                "description": format!("C'EST QUOI ? Carte d'un symbole en un appel, sans lire son fichier : signature, rôle (doc-comment), ce qu'il appelle, qui l'appelle (avec la ligne de l'appel), combien de fichiers importent son fichier, tests liés, homonymes. Un fichier en entrée donne son outline.{}", REGLE),
                "inputSchema": schema("cible", ID_DESC, json!({}))
            },
            {
                "name": "cortex_outline",
                "description": format!("QUE CONTIENT CE FICHIER ? Rôle, fichiers importés et importeurs, symboles imbriqués avec plages de lignes, exports — au lieu de lire le fichier entier.{}", REGLE),
                "inputSchema": schema("cible", "Fichier : F:chemin, chemin, suffixe unique (useX.ts), ou un symbole (son fichier)", json!({}))
            },
            {
                "name": "cortex_read",
                "description": format!("MONTRE LE CODE : les lignes exactes d'un symbole (ou d'une section de doc, d'un fichier, d'une plage chemin:12-40), numérotées, lues sur disque — au lieu d'un Read à offset deviné.{}", REGLE),
                "inputSchema": schema("cible", ID_DESC, json!({ "contexte": { "type": "integer", "description": "Lignes de contexte de part et d'autre (défaut 0)" } }))
            },
            {
                "name": "cortex_overview",
                "description": format!("COMMENT MARCHE CE MODULE ? Pour un dossier : fichiers et rôles, points d'entrée (fichiers importés depuis l'extérieur), dépendances sortantes et entrantes par dossier, paquets externes.{}", REGLE),
                "inputSchema": schema("dossier", "Dossier relatif à la racine du projet (ex. src/auth ; « . » = tout le projet)", json!({}))
            },
            {
                "name": "cortex_impact",
                "description": format!("QU'EST-CE QUI CASSE SI JE CHANGE ÇA ? Dépendants transitifs d'un symbole (appelants résolus par les imports, avec la ligne de l'appel, et fichiers qui importent le symbole sans l'appeler) ou d'un fichier (importeurs), par profondeur, et les tests à relancer. Les appels dynamiques (import(), chaînes) ne sont pas vus.{}", REGLE),
                "inputSchema": schema("cible", ID_DESC, json!({ "profondeur": { "type": "integer", "description": "Profondeur 1 à 6 (défaut 3)" } }))
            },
            {
                "name": "cortex_path",
                "description": format!("COMMENT A ARRIVE-T-IL À B ? Plus court chemin d'appels entre deux symboles ou fichiers (sinon d'imports entre leurs fichiers ; sinon dans le sens inverse), avec la ligne de chaque appel.{}", REGLE),
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "de": { "type": "string", "description": ID_DESC },
                        "vers": { "type": "string", "description": ID_DESC },
                        "project": { "type": "string", "description": "Limiter à un projet (optionnel)" },
                        "budget": { "type": "integer", "description": "Budget de sortie en tokens (optionnel)" }
                    },
                    "required": ["de", "vers"]
                }
            },
            {
                "name": "cortex_changed",
                "description": format!("QU'AI-JE MODIFIÉ ? Travail non commité (git status + git diff HEAD) : fichiers, fonctions touchées, leurs appelants hors du travail en cours et les tests à relancer.{}", REGLE),
                "inputSchema": { "type": "object", "properties": { "project": { "type": "string", "description": "Limiter à un projet (optionnel)" }, "budget": { "type": "integer", "description": "Budget de sortie en tokens (optionnel)" } } }
            },
            {
                "name": "cortex_query",
                "description": "Alias de cortex_find (compatibilité) : même entrée « question », même sortie.",
                "inputSchema": schema("question", "Question ou mots-clés (FR ou EN)", json!({}))
            },
            {
                "name": "cortex_explain",
                "description": "Alias de cortex_card (compatibilité) : entrée « symbol ». « depth » est accepté et ignoré ; pour les dépendants transitifs, utiliser cortex_impact.",
                "inputSchema": schema("symbol", ID_DESC, json!({ "depth": { "type": "integer", "description": "Ignoré (compatibilité)" } }))
            },
            {
                "name": "cortex_context",
                "description": "Alias de cortex_card (compatibilité) qui ajoute les docs (docs/**/*.md) citant le symbole ou son fichier. Entrée « symbol ».",
                "inputSchema": schema("symbol", ID_DESC, json!({}))
            },
            {
                "name": "cortex_grep",
                "description": "Texte EXACT (sous-chaîne littérale, insensible à la casse par défaut) dans le contenu des fichiers indexés : messages d'erreur, clés i18n, URLs, TODO, noms de table. Multithread, jamais node_modules ; résultats groupés par fichier avec le symbole englobant de chaque ligne (identifiant S:). Pour un symbole ou une question, préférer cortex_find.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "needle": { "type": "string", "description": "Chaîne littérale à chercher" },
                        "project": { "type": "string", "description": "Limiter à un projet (optionnel)" },
                        "case_sensitive": { "type": "boolean", "description": "Sensible à la casse (défaut false)" },
                        "max": { "type": "integer", "description": "Max de lignes (défaut 60)" },
                        "budget": { "type": "integer", "description": "Budget tokens approx (défaut 1500)" }
                    },
                    "required": ["needle"]
                }
            },
            {
                "name": "cortex_files",
                "description": "Fichiers par fragment de chemin (insensible à la casse) ou glob ('**/*.test.ts'), lus dans l'atlas sans toucher le disque. Les chemins rendus sont acceptés tels quels par les autres outils.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Fragment de nom/chemin, ou glob si '*'/'?' présent" },
                        "project": { "type": "string", "description": "Limiter à un projet (optionnel)" },
                        "max": { "type": "integer", "description": "Max de résultats (défaut 80)" }
                    },
                    "required": ["pattern"]
                }
            },
            {
                "name": "cortex_docs",
                "description": "Recherche dans les documentations externes aspirées en local (React, Rust, PostgreSQL… : `cortex docs add`) : réponses hors ligne. À utiliser avant WebFetch/WebSearch pour une bibliothèque déjà aspirée (liste : cortex_docs_list).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "question": { "type": "string", "description": "Question / mots-clés" },
                        "source": { "type": "string", "description": "Limiter à une doc (ex: Tauri, React). Optionnel." },
                        "budget": { "type": "integer", "description": "Budget tokens approx (défaut 800)" }
                    },
                    "required": ["question"]
                }
            },
            {
                "name": "cortex_docs_list",
                "description": "Liste les documentations externes aspirées en local et leur nombre de pages.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "cortex_list",
                "description": "Liste les projets indexés par Cortex (fichiers, symboles).",
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
    let params = params.ok_or((-32602, "params manquants".into()))?;
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
                "(aucune doc scrapée — cortex docs add <url> --name <nom>)".to_string()
            } else {
                let mut s = String::from("Docs offline disponibles :\n");
                for (name, pages) in docs {
                    s.push_str(&format!("- {} : {} pages\n", name, pages));
                }
                s
            }
        }
        "cortex_list" => run_list(),
        _ => return Err((-32602, format!("tool '{}' inconnu", name))),
    };

    Ok(json!({ "content": [ { "type": "text", "text": text } ] }))
}

// ─── Implémentations (toutes sur l'atlas) ───────────────────────────────────

fn run_grep(needle: &str, project: Option<String>, case_sensitive: bool, max: usize, budget: usize) -> String {
    let handles = open_atlas_handles(&project);
    if handles.is_empty() {
        return "(cortex) aucun index.".into();
    }
    crate::run_grep_on(&handles, needle, case_sensitive, max, budget).0
}

fn run_files(pattern: &str, project: Option<String>, max: usize) -> String {
    let handles = open_atlas_handles(&project);
    if handles.is_empty() {
        return "(cortex) aucun index.".into();
    }
    crate::run_files_on(&handles, pattern, max).0
}

fn run_list() -> String {
    let home = crate::index::cortex_home();
    let mut out = String::from("Projets indexés :\n");
    if let Ok(entries) = std::fs::read_dir(&home) {
        for e in entries.flatten() {
            if crate::atlas::is_project_dir(&e.path()) {
                let n = e.file_name().to_string_lossy().to_string();
                if let Some(h) = open_atlas(&n) {
                    let (nf, ns, _) = h.counts();
                    out.push_str(&format!("- {} : {} fichiers, {} symboles\n", n, nf, ns));
                }
            }
        }
    }
    out
}
