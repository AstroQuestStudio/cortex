//! Cortex — moteur de contexte code ultra-rapide pour assistants IA.
//!
//! Indexe un projet (gitignore-aware, parallèle) et permet une recherche
//! token-optimale. Conçu pour remplacer Grep/Read aveugles : on trouve le
//! contexte EXACT au lieu de tout lire.

mod aimd;
mod atlas;
mod banc_agent;
mod batch;
mod bench;
mod comparatif;
mod config;
mod cpp;
mod dash;
mod engine;
mod extract;
mod fx;
mod galaxy;
mod graph;
mod ids;
mod index;
mod infra;
mod lang;
mod latence;
mod mcp;
mod outils;
mod par;
mod scrape;
mod search;
mod semantic;
mod stem;
mod symbol;
mod textsearch;
#[cfg(feature = "ui")]
mod ui_locate;
mod walk;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::Instant;

#[derive(Parser)]
#[command(
    name = "cortex",
    version = concat!(env!("CARGO_PKG_VERSION"), " — by AstroQuest"),
    about = "Cortex — code context engine for AI agents (find, card, read, impact…), by AstroQuest"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
    /// Thread count for all parallel code (default: one per logical core;
    /// also `CORTEX_THREADS`).
    #[arg(long, global = true)]
    threads: Option<usize>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Indexes a project (path) under a name. E.g. cortex index . --name MyProject
    Index {
        /// Path of the project to index (default: current directory).
        path: Option<PathBuf>,
        /// Project name (default: folder name).
        #[arg(short, long)]
        name: Option<String>,
    },
    /// Updates the atlas: full walk, compares size + blake3 hash of each file on
    /// disk (independent of git: uncommitted work is seen), then writes a delta
    /// segment (or rebuilds if the change is massive).
    Update {
        /// Name of an already indexed project.
        name: String,
        /// Path (default: the root recorded in the index).
        path: Option<PathBuf>,
        /// Fast: the freshness check (mtime/size read from directories, new files
        /// under the same exclusion rules), without re-reading or hashing unchanged
        /// files. No git.
        #[arg(long)]
        changed: bool,
    },
    /// Shows the stats of an existing index (files, lines, symbols, languages).
    Stats {
        /// Name of the indexed project.
        name: String,
    },
    /// WHERE IS X? Symbols ranked for a question (BM25F: camelCase names, paths,
    /// file headers, doc-comments, file bodies; FR/EN stemming, FR<->EN synonyms,
    /// typo tolerant). One line per symbol: stable id `S:path#name`, kind, range;
    /// role of the first ones.
    Find {
        /// Natural-language question or keywords (FR or EN).
        question: String,
        #[command(flatten)]
        c: Commun,
    },
    /// Alias of `find` (compatibility).
    Query {
        question: String,
        #[command(flatten)]
        c: Commun,
    },
    /// WHAT IS IT? Card of a symbol: signature, role, callees, callers (with call
    /// site), importers, tests, homonyms. Input: id, name, path:line; a file gives
    /// its outline.
    Card {
        cible: String,
        #[command(flatten)]
        c: Commun,
    },
    /// Alias of `card` (compatibility; `-d`/`-l` accepted and ignored).
    Explain {
        symbol: String,
        #[arg(short, long, default_value_t = 1)]
        depth: usize,
        #[arg(short, long, default_value = "signatures")]
        level: String,
        #[command(flatten)]
        c: Commun,
    },
    /// Alias of `card` + the docs (docs/**/*.md) that cite the symbol.
    Context {
        symbol: String,
        #[command(flatten)]
        c: Commun,
    },
    /// WHAT DOES THIS FILE CONTAIN? Role, imports, importers, nested symbols with
    /// ranges and exports. Input: F:path, path, unique suffix (`useX.ts`).
    Outline {
        cible: String,
        #[command(flatten)]
        c: Commun,
    },
    /// SHOW THE CODE: the exact lines of a symbol (doc section, file, or range
    /// `path:12-40`), numbered.
    Read {
        cible: String,
        /// Context lines on each side.
        #[arg(short = 'C', long = "contexte", default_value_t = 0)]
        contexte: u32,
        #[command(flatten)]
        c: Commun,
    },
    /// HOW DOES THIS MODULE WORK? Files and roles, entry points (imported from
    /// outside), outgoing/incoming dependencies, external packages.
    Overview {
        /// Folder (relative to the project root; `.` = whole project).
        dossier: String,
        #[command(flatten)]
        c: Commun,
    },
    /// WHAT BREAKS IF I CHANGE THIS? Transitive callers (and importers) by depth,
    /// with call sites, and tests to re-run.
    Impact {
        cible: String,
        /// Depth (1 to 6).
        #[arg(short, long, default_value_t = 3)]
        depth: usize,
        #[command(flatten)]
        c: Commun,
    },
    /// HOW DOES A REACH B? Shortest call path (else import path), in either
    /// direction.
    Path {
        de: String,
        vers: String,
        #[command(flatten)]
        c: Commun,
    },
    /// WHAT DID I CHANGE? Uncommitted files (git), touched functions, their callers
    /// outside the work in progress and the tests to re-run.
    Changed {
        #[command(flatten)]
        c: Commun,
    },
    /// AGENT bench (architecture v2 §7): replays comprehension tasks with and
    /// without Cortex (rg + file reads); calls, tokens read, facts covered.
    BenchAgent {
        /// Tasks file (default: `$CORTEX_BENCH_DIR/agent_tasks.json`, else
        /// `bench/agent_tasks.json`).
        file: Option<PathBuf>,
        #[arg(short, long)]
        project: Option<String>,
        /// Prints the output of each call.
        #[arg(short, long)]
        verbose: bool,
        /// Only plays tasks whose id contains this text.
        #[arg(long)]
        tache: Option<String>,
    },
    /// Relevance bench: runs each question of a JSON file and computes top-1,
    /// top-5 and MRR on the rank of the expected file.
    Bench {
        /// Bench JSON file ({"project": .., "queries": [{"q","expect":[..]}]}).
        file: PathBuf,
        /// Project to query (default: the file's "project" field).
        #[arg(short, long)]
        project: Option<String>,
        /// Prints the detail of each question (otherwise only failures).
        #[arg(short, long)]
        verbose: bool,
        /// Engine measured: only `atlas` exists (the old v1 engine was removed;
        /// its reference is frozen: 72.0% / 88.0% / MRR 0.776).
        #[arg(short = 'm', long, default_value = "atlas")]
        moteur: String,
    },
    /// COMPARATIVE bench (architecture v2 §11): same questions, same judge, same
    /// corpus for keyword ripgrep, pure BM25, dense RAG, RRF hybrid and Cortex.
    /// Top-1/top-5/MRR, latency, build, tokens read by the agent. Dense and hybrid
    /// need a `--features bench` build.
    BenchCompare {
        /// Bench JSON file (same format as `cortex bench`).
        file: PathBuf,
        /// Project to query (default: the file's "project" field).
        #[arg(short, long)]
        project: Option<String>,
        /// Does not measure the full build of a Cortex atlas (about 1 min).
        #[arg(long)]
        sans_construction: bool,
        /// Results JSON folder (default: `$CORTEX_BENCH_RESULTS`, else
        /// `$CORTEX_BENCH_DIR/resultats`, else `bench/resultats`).
        #[arg(long)]
        sortie: Option<PathBuf>,
    },
    /// ULTRA-FAST full-text search in file contents (replaces grep). Multithreaded,
    /// scans only indexed files (no re-walk of node_modules).
    Grep {
        /// String/word to look for in file contents.
        needle: String,
        /// Limit to one project (default: all active projects).
        #[arg(short, long)]
        project: Option<String>,
        /// Case sensitive (default: insensitive).
        #[arg(short = 's', long)]
        case_sensitive: bool,
        /// Max number of results.
        #[arg(short, long, default_value_t = 60)]
        max: usize,
        /// Output budget in approximate tokens.
        #[arg(short, long, default_value_t = 2000)]
        budget: usize,
        /// Skips the automatic freshness check (directory read) before answering.
        #[arg(long)]
        no_refresh: bool,
    },
    /// WHERE IS THIS UI? From what you SEE (button text, test id, aria-label, id, React
    /// component name) to the code that renders it: resolves i18n keys and their usages,
    /// ignores comments, docs and tests. One line per candidate: stable id, file:line, why.
    Ui {
        /// Visible text (button label, heading…). May be empty if another signal is given.
        #[arg(default_value = "")]
        text: String,
        /// data-testid / data-test / data-cy value (repeatable).
        #[arg(long)]
        testid: Vec<String>,
        /// aria-label / title / placeholder value (repeatable).
        #[arg(long)]
        aria: Vec<String>,
        /// DOM id (repeatable).
        #[arg(long)]
        id: Vec<String>,
        /// React component names, nearest first, comma-separated (e.g. SubmitButton,FormFooter).
        #[arg(long, value_delimiter = ',')]
        component: Vec<String>,
        /// Limit to one project (default: all active projects).
        #[arg(short, long)]
        project: Option<String>,
        /// Max number of candidates.
        #[arg(short, long, default_value_t = 6)]
        max: usize,
        /// Skips the automatic freshness check (directory read) before answering.
        #[arg(long)]
        no_refresh: bool,
    },
    /// Finds a file by name/path fragment (replaces find -name). Instant.
    Files {
        /// Name/path fragment (case-insensitive), or glob pattern if it contains
        /// '*'/'?' (e.g. "**/*.test.ts", "src/hooks/*.ts").
        pattern: String,
        /// Limit to one project (default: all active projects).
        #[arg(short, long)]
        project: Option<String>,
        /// Max number of files shown.
        #[arg(short, long, default_value_t = 80)]
        max: usize,
        /// Skips the automatic freshness check (directory read) before answering.
        #[arg(long)]
        no_refresh: bool,
    },
    /// Lists the indexed projects.
    List,
    /// Exports all projects + docs as a 3D galaxy JSON (~/.cortex/galaxy.json).
    Galaxy {
        /// Output path (default: ~/.cortex/galaxy.json).
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// Generates the galaxy and opens the 3D VIEWER in the browser (local server).
    Viewer {
        /// Local server port (default 7777).
        #[arg(short, long, default_value_t = 7777)]
        port: u16,
        /// Do not regenerate the galaxy (reuses the existing ~/.cortex/galaxy.json).
        #[arg(long)]
        no_build: bool,
    },
    /// Enables/disables a project for search (focus). No arg: shows the state.
    Config {
        #[command(subcommand)]
        action: Option<ConfigAction>,
    },
    /// Runs the MCP server (stdio): tools cortex_find/card/outline/read/overview/impact/path/changed/grep/files/docs/docs_list/list.
    Mcp,
    /// Offline documentation: scrapes a doc site and makes it searchable locally.
    Docs {
        #[command(subcommand)]
        action: DocsAction,
    },
    /// Snapshot of a project's server architecture (read-only SSH, else static
    /// topology). Servers read from a .env: each prefix `<P>` that has a key
    /// `<P>_IPV4` (or `<P>_HOST`); `<P>_SSH_PORT`, `<P>_LOGIN`, `<P>_SSH_KEY_PATH`,
    /// `<P>_LABEL` optional. No secret is copied.
    Infra {
        /// Name of the project the infra is attached to.
        #[arg(short, long)]
        project: String,
        /// Path of the .env holding the server coordinates (default: ./.env).
        #[arg(short, long)]
        env: Option<PathBuf>,
    },
    /// UPDATES THE WHOLE knowledge base: all indexes + the galaxy.
    UpdateAll,
    /// LATENCY bench (architecture v2 §4): open, query (median/p95 over the
    /// relevance bench), `context`, `files`, `grep`, freshness check, single-file
    /// delta update, compaction.
    BenchLatence {
        #[arg(short, long)]
        project: String,
        /// Relevance bench whose questions are used to measure `find` (default:
        /// `$CORTEX_BENCH_DIR/queries.json`, else `bench/queries.json`).
        #[arg(long)]
        questions: Option<PathBuf>,
        /// Scaling curve: each item at 1, 2, 4, 8 and 16 threads (median, min, max
        /// of the passes) + full build; JSON in `bench/resultats/`.
        #[arg(long)]
        echelle: bool,
        /// Passes per thread count (with --echelle).
        #[arg(long, default_value_t = 3)]
        passes: usize,
        /// Does not measure the full build (with --echelle).
        #[arg(long)]
        sans_construction: bool,
        /// Thread counts measured (with --echelle; default: 1,2,4,8,16).
        #[arg(long, value_delimiter = ',')]
        threads_liste: Vec<usize>,
    },
}

/// Options shared by the agent tools.
#[derive(clap::Args, Clone)]
struct Commun {
    /// Limit to one project (default: all active projects).
    #[arg(short, long)]
    project: Option<String>,
    /// Output budget in tokens (about characters / 4; default depends on the tool).
    #[arg(short, long)]
    budget: Option<usize>,
    /// Skips the automatic freshness check (directory read) before answering.
    #[arg(long)]
    no_refresh: bool,
}

#[derive(Subcommand)]
enum DocsAction {
    /// Scrapes a doc site locally. E.g. cortex docs add https://react.dev/reference --name React
    Add {
        /// Crawl start URL (same domain, doc sub-path inferred from the URL).
        url: String,
        /// Doc name (used as --source for `docs query`).
        #[arg(short, long)]
        name: String,
        /// Max number of pages to crawl (default 200).
        #[arg(short, long, default_value_t = 200)]
        max: usize,
    },
    /// Searches the scraped docs (offline).
    Query {
        /// Question / keywords.
        question: String,
        /// Limit to one doc (e.g. React, Tauri). Default: all.
        #[arg(short, long)]
        source: Option<String>,
        /// Output budget in approximate tokens.
        #[arg(short, long, default_value_t = 1500)]
        budget: usize,
    },
    /// BATCH-scrapes a list of sites (config file) with a live DASHBOARD.
    /// Config format (1 line/site): name | url | max_pages?
    /// Live controls: +/- concurrency · [ ] delay · up/down select · p pause · q quit.
    Batch {
        /// Config file (list of sites).
        config: PathBuf,
        /// Number of SITES in parallel (default: ALL). Not to be confused with
        /// --concurrency (pages in flight WITHIN a site). E.g. 14 sites x 20 pages.
        #[arg(short, long)]
        workers: Option<usize>,
        /// Limits the number of sites in parallel: light(2) | normal(all) | turbo(8).
        #[arg(short, long, default_value = "normal")]
        preset: String,
        /// Default max_pages if not set in the config (full-crawl safeguard).
        #[arg(short = 'm', long, default_value_t = 800)]
        default_max: usize,
        /// Initial intra-site concurrency (pages in flight per site). Adjustable live (left/right).
        #[arg(short, long, default_value_t = 20)]
        concurrency: usize,
        /// Min delay between request launches (ms). Adjustable live.
        #[arg(short = 'd', long, default_value_t = 120)]
        delay: u64,
        /// AUTO-resume: sites that hit their budget keep going automatically until
        /// exhausted (finish on their own). Live toggle: key a.
        #[arg(long, default_value_t = true)]
        auto: bool,
        /// Plain output (no TUI dashboard), for logs/CI/pipes.
        #[arg(long)]
        plain: bool,
    },
    /// Lists the scraped docs.
    List,
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Enables a project (included in cross-project search).
    Enable { project: String },
    /// Disables a project (excluded from search: focus + speed).
    Disable { project: String },
}

fn main() {
    let cli = Cli::parse();
    par::init_global(cli.threads);
    match cli.cmd {
        Cmd::Index { path, name } => cmd_index(path, name),
        Cmd::Update { name, path, changed } => {
            if changed {
                cmd_update_changed(&name)
            } else {
                cmd_update(&name, path)
            }
        }
        Cmd::Stats { name } => cmd_stats(&name),
        Cmd::Find { question, c } | Cmd::Query { question, c } => cmd_outil("find", outils::Appel::Find { question }, c),
        Cmd::Card { cible, c } => cmd_outil("card", outils::Appel::Card { cible }, c),
        Cmd::Explain { symbol, depth: _, level: _, c } => cmd_outil("card", outils::Appel::Card { cible: symbol }, c),
        Cmd::Context { symbol, c } => cmd_outil("context", outils::Appel::Context { cible: symbol }, c),
        Cmd::Outline { cible, c } => cmd_outil("outline", outils::Appel::Outline { cible }, c),
        Cmd::Read { cible, contexte, c } => cmd_outil("read", outils::Appel::Read { cible, contexte }, c),
        Cmd::Overview { dossier, c } => cmd_outil("overview", outils::Appel::Overview { dossier }, c),
        Cmd::Impact { cible, depth, c } => cmd_outil("impact", outils::Appel::Impact { cible, profondeur: depth }, c),
        Cmd::Path { de, vers, c } => cmd_outil("path", outils::Appel::Path { de, vers }, c),
        Cmd::Changed { c } => cmd_outil("changed", outils::Appel::Changed, c),
        Cmd::BenchAgent { file, project, verbose, tache } => {
            let file = file.unwrap_or_else(|| bench::bench_file("agent_tasks.json"));
            banc_agent::run(&file, project, verbose, tache)
        }
        Cmd::Bench { file, project, verbose, moteur } => cmd_bench(&file, project, verbose, &moteur),
        Cmd::BenchCompare { file, project, sans_construction, sortie } => {
            cmd_bench_compare(&file, project, sans_construction, sortie.unwrap_or_else(bench::results_dir))
        }
        Cmd::Grep { needle, project, case_sensitive, max, budget, no_refresh } => {
            cmd_grep(&needle, project, case_sensitive, max, budget, no_refresh)
        }
        #[cfg(not(feature = "ui"))]
        Cmd::Ui { text, testid, aria, id, component, project, max, no_refresh } => {
            let mut args = vec!["ui".to_string(), text];
            for v in testid { args.push("--testid".into()); args.push(v); }
            for v in aria { args.push("--aria".into()); args.push(v); }
            for v in id { args.push("--id".into()); args.push(v); }
            if !component.is_empty() { args.push("--component".into()); args.push(component.join(",")); }
            if let Some(p) = project { args.push("--project".into()); args.push(p); }
            args.push("--max".into());
            args.push(max.to_string());
            if no_refresh { args.push("--no-refresh".into()); }
            std::process::exit(lancer_cortex_ui(&args));
        }
        #[cfg(feature = "ui")]
        Cmd::Ui { text, testid, aria, id, component, project, max, no_refresh } => {
            let q = ui_locate::UiQuery { text, testid, aria, id, component };
            let t0 = Instant::now();
            let handles = require_handles(&project, no_refresh);
            print!("{}", ui_locate::run_ui_on(&handles, &q, max));
            eprintln!("[cortex ui] {:.1}ms", t0.elapsed().as_secs_f64() * 1000.0);
        }
        Cmd::Files { pattern, project, max, no_refresh } => cmd_files(&pattern, project, max, no_refresh),
        Cmd::List => cmd_list(),
        Cmd::Galaxy { out } => cmd_galaxy(out),
        Cmd::Viewer { port, no_build } => cmd_viewer(port, no_build),
        Cmd::Config { action } => cmd_config(action),
        Cmd::Mcp => mcp::serve(),
        Cmd::Docs { action } => cmd_docs(action),
        Cmd::Infra { project, env } => cmd_infra(&project, env),
        Cmd::UpdateAll => cmd_update_all(),
        Cmd::BenchLatence { project, questions, echelle, passes, sans_construction, threads_liste } => {
            let questions = questions.unwrap_or_else(|| bench::bench_file("queries.json"));
            if echelle {
                latence::run_echelle(&project, &questions, passes, sans_construction, &threads_liste)
            } else {
                latence::run(&project, &questions)
            }
        }
    }
}

// ─── Atlas : toutes les commandes lisent l'atlas (architecture v2 §8) ───────
//
// `query`/`explain`/`context` : lecture directe (index inversé, CSR, cartes).
// `files`/`grep` : chemins et plages de symboles lus dans l'atlas mmap-é (pas
// de matérialisation). `stats`/`list` : compteurs lus dans l'atlas. `galaxy` :
// matérialisation (`View::materialize`). Fraîcheur : `atlas::fresh` (delta).

/// Projets connus de `~/.cortex` (atlas, ou ancien index à migrer).
fn project_names(only_enabled: bool) -> Vec<String> {
    let cfg = config::Config::load();
    let mut v = Vec::new();
    if let Ok(entries) = std::fs::read_dir(index::cortex_home()) {
        for e in entries.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if atlas::is_project_dir(&e.path()) && (!only_enabled || cfg.is_enabled(&n)) {
                v.push(n);
            }
        }
    }
    v.sort();
    v
}

/// Ouvre (migre/reconstruit si besoin) l'atlas de chaque projet demandé, avec
/// le contrôle de fraîcheur automatique (sauf `no_refresh`).
fn atlas_handles(project: &Option<String>, no_refresh: bool) -> Vec<atlas::Handle> {
    let names: Vec<String> = match project {
        Some(n) => vec![n.clone()],
        None => project_names(true),
    };
    let mut handles = Vec::new();
    for n in names {
        match atlas::ensure_and_open(&n) {
            Ok(mut h) => {
                if !no_refresh {
                    report_refresh(&n, atlas::fresh::refresh(&mut h));
                }
                handles.push(h);
            }
            Err(e) => eprintln!("[cortex] atlas '{}' unavailable ({}) - skipped", n, e),
        }
    }
    handles
}

/// Log discret (stderr) du coût/résultat du contrôle de fraîcheur.
fn report_refresh(project: &str, s: atlas::fresh::RefreshStats) {
    if s.read_only {
        return; // l'avertissement « lecture seule » a déjà été donné (une fois)
    }
    if s.walked {
        eprintln!("[cortex] freshness {}: exclusion rules changed (or first check), full walk", project);
    }
    if s.refreshed {
        eprintln!(
            "[cortex] freshness {}: check {:.1}ms ({} verified) then {} path(s) reindexed in {:.1}ms",
            project, s.check_ms, s.checked, s.examined, s.update_ms
        );
    } else if s.examined > 0 {
        eprintln!(
            "[cortex] freshness {}: {} path(s) re-read, content unchanged ({:.1}ms)",
            project,
            s.examined,
            s.check_ms + s.update_ms
        );
    } else {
        eprintln!("[cortex] freshness {}: nothing changed ({:.1}ms, {} verified)", project, s.check_ms, s.checked);
    }
}

fn require_handles(project: &Option<String>, no_refresh: bool) -> Vec<atlas::Handle> {
    let handles = atlas_handles(project, no_refresh);
    if handles.is_empty() {
        match project {
            Some(n) => eprintln!("cortex: project '{}' not found. Run: cortex index <path> --name {}", n, n),
            None => eprintln!("cortex: no active project. Run: cortex index <path> (or cortex config enable <project>)"),
        }
        std::process::exit(1);
    }
    handles
}

/// Liste de fichiers de chaque atlas pour `textsearch` (chaînes mmap-ées).
fn file_sets(handles: &[atlas::Handle]) -> Vec<textsearch::FileSet<'_>> {
    handles.iter().map(|h| textsearch::FileSet { project: &h.project, root: h.root(), paths: h.file_paths() }).collect()
}

/// An agent tool (`outils`) on the CLI: open, freshness, output.
fn cmd_outil(nom: &str, appel: outils::Appel, c: Commun) {
    let t0 = Instant::now();
    let handles = require_handles(&c.project, c.no_refresh);
    print!("{}", outils::executer(&handles, &appel, c.budget));
    eprintln!("[cortex {}] {:.1}ms (atlas)", nom, t0.elapsed().as_secs_f64() * 1000.0);
}

/// `grep` partagé CLI/MCP : occurrences + fonction englobante (plages de symboles).
pub(crate) fn run_grep_on(
    handles: &[atlas::Handle],
    needle: &str,
    case_sensitive: bool,
    max: usize,
    budget: usize,
) -> (String, usize, usize) {
    let sets = file_sets(handles);
    let scanned: usize = sets.iter().map(|s| s.paths.len()).sum();
    let mut hits = textsearch::grep(&sets, needle, case_sensitive, max + 1);
    let truncated = hits.len() > max;
    hits.truncate(max);
    for h in hits.iter_mut() {
        h.symbol = handles[h.set].enclosing_symbol(&h.file, h.line);
    }
    (textsearch::format_grep(&hits, budget, handles.len() > 1, truncated), hits.len(), scanned)
}

/// `files` partagé CLI/MCP.
pub(crate) fn run_files_on(handles: &[atlas::Handle], pattern: &str, max: usize) -> (String, usize) {
    let sets = file_sets(handles);
    let found = textsearch::find_files(&sets, pattern, max);
    if found.is_empty() {
        return (format!("(no file matches '{}')\n", pattern), 0);
    }
    let multi = handles.len() > 1;
    let mut out = String::new();
    for (proj, path) in &found {
        if multi {
            out.push_str(&format!("[{}] {}\n", proj, path));
        } else {
            out.push_str(&format!("{}\n", path));
        }
    }
    (out, found.len())
}

/// Met à jour TOUS les projets (parcours complet) puis régénère la galaxie.
fn cmd_update_all() {
    let t0 = Instant::now();
    let names = project_names(false);
    if names.is_empty() {
        eprintln!("cortex: no indexed project. Run first: cortex index <path> --name <Project>");
        std::process::exit(1);
    }
    println!("Updating {} project(s)…", names.len());
    let (mut ok, mut skipped) = (0, 0);
    for n in &names {
        let Some(root) = atlas::project_root(n) else {
            eprintln!("  ⚠ {}: unknown root, skipped", n);
            skipped += 1;
            continue;
        };
        let root = PathBuf::from(root);
        if !root.exists() {
            eprintln!("  ⚠ {}: source missing ({}), skipped", n, root.display());
            skipped += 1;
            continue;
        }
        match atlas::fresh::update_full(n, &root) {
            Ok((out, changed, total)) => {
                println!("  ✓ {}: {} path(s) changed out of {} → {:?}", n, changed, total, out);
                ok += 1;
            }
            Err(e) => {
                eprintln!("  ⚠ {}: {}, skipped", n, e);
                skipped += 1;
            }
        }
    }
    println!("\nRegenerating the galaxy…");
    cmd_galaxy(None);
    println!("\n✓ {} project(s) up to date ({} skipped) in {:.1}s", ok, skipped, t0.elapsed().as_secs_f64());
}

fn cmd_config(action: Option<ConfigAction>) {
    let mut cfg = config::Config::load();
    match action {
        Some(ConfigAction::Enable { project }) => {
            cfg.enable(&project);
            cfg.save().ok();
            println!("OK {} enabled", project);
        }
        Some(ConfigAction::Disable { project }) => {
            cfg.disable(&project);
            cfg.save().ok();
            println!("OK {} disabled (excluded from search)", project);
        }
        None => {
            println!("Projects (enabled/disabled):");
            for n in project_names(false) {
                let state = if cfg.is_enabled(&n) { "[ON] " } else { "[off]" };
                println!("  {} {}", state, n);
            }
        }
    }
}

fn cmd_galaxy(out: Option<PathBuf>) {
    let t0 = Instant::now();
    let mut indexes = Vec::new();
    for n in project_names(false) {
        if let Ok(h) = atlas::ensure_and_open(&n) {
            indexes.push(h.materialize());
        }
    }
    if indexes.is_empty() {
        eprintln!("cortex: no index. Run: cortex index <path>");
        std::process::exit(1);
    }
    let out = out.unwrap_or_else(|| index::cortex_home().join("galaxy.json"));
    match galaxy::build_galaxy(&indexes, &out) {
        Ok((n, p)) => {
            println!("OK galaxy: {} symbols, {} projects - {} - {:.2}s", n, p, out.display(), t0.elapsed().as_secs_f64());
        }
        Err(e) => {
            eprintln!("cortex: galaxy failed: {}", e);
            std::process::exit(1);
        }
    }
}

fn cmd_index(path: Option<PathBuf>, name: Option<String>) {
    let root = path.unwrap_or_else(|| PathBuf::from("."));
    let root = match root.canonicalize() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("cortex: invalid path: {}", e);
            std::process::exit(1);
        }
    };
    let name = name.unwrap_or_else(|| root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "project".to_string()));
    let t0 = Instant::now();
    match index::build_index(&name, &root) {
        Ok((idx, tracked)) => {
            let (nf, ns, nl) = idx.stats();
            if let Err(e) = atlas::rebuild_full(&name, &idx, tracked) {
                eprintln!("cortex: atlas write failed: {}", e);
                std::process::exit(1);
            }
            println!("OK {} indexed: {} files - {} lines - {} symbols - {:.2}s", name, nf, nl, ns, t0.elapsed().as_secs_f64());
            println!("  -> {}", atlas::atlas_root_for(&name).display());
        }
        Err(e) => {
            eprintln!("cortex: indexing failed: {}", e);
            std::process::exit(1);
        }
    }
}

fn cmd_update(name: &str, path: Option<PathBuf>) {
    let root = match path.map(|p| p.to_string_lossy().to_string()).or_else(|| atlas::project_root(name)) {
        Some(r) => PathBuf::from(r),
        None => {
            eprintln!("cortex: project '{}' not found. Run first: cortex index <path> --name {}", name, name);
            std::process::exit(1);
        }
    };
    let root = root.canonicalize().unwrap_or(root);
    let t0 = Instant::now();
    match atlas::fresh::update_full(name, &root) {
        Ok((out, changed, total)) => {
            println!(
                "OK {} up to date: {} path(s) changed out of {} files ({:?}) - {:.2}s",
                name,
                changed,
                total,
                out,
                t0.elapsed().as_secs_f64()
            );
        }
        Err(e) => {
            eprintln!("cortex: update failed: {}", e);
            std::process::exit(1);
        }
    }
}

/// `cortex update --changed`: fast check (directory read, no git).
fn cmd_update_changed(name: &str) {
    let t0 = Instant::now();
    match atlas::fresh::update_changed(name) {
        Ok((out, n)) => println!(
            "OK {} up to date (--changed, {} path(s) re-read or removed): {:?} - {:.2}s",
            name,
            n,
            out,
            t0.elapsed().as_secs_f64()
        ),
        Err(e) => {
            eprintln!("cortex: {}. Use `cortex update {}` (full).", e, name);
            std::process::exit(1);
        }
    }
}

/// `cortex bench` : top-1 / top-5 / MRR du classement de query sur un banc JSON.
fn cmd_bench(file: &std::path::Path, project: Option<String>, verbose: bool, moteur: &str) {
    if moteur != "atlas" {
        eprintln!("cortex: only the 'atlas' engine exists (v1 removed: 72.0% / 88.0% / MRR 0.776, see docs/ARCHITECTURE.md)");
        std::process::exit(1);
    }
    let content = match std::fs::read_to_string(file) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cortex: cannot read {}: {}", file.display(), e);
            std::process::exit(1);
        }
    };
    let bench = match bench::parse(&content) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("cortex: invalid bench ({}): {}", file.display(), e);
            std::process::exit(1);
        }
    };
    let project = project.or(bench.project.clone());
    // Index STABLE : pas de contrôle de fraîcheur (reproductible).
    let handles = require_handles(&project, true);
    let t0 = Instant::now();
    let mut results = Vec::new();
    let mut by_tag: std::collections::BTreeMap<String, Vec<usize>> = Default::default();
    for (i, q) in bench.queries.iter().enumerate() {
        let r = bench::run_one_atlas(&handles, q);
        let ok = r.rank.map(|k| k.to_string()).unwrap_or_else(|| "-".into());
        if verbose || r.rank.map(|k| k > 5).unwrap_or(true) {
            println!("[{:>2}] rank {:>2}  {:<48} expected {}", i + 1, ok, q.q, q.expect.join(" | "));
            if r.rank != Some(1) {
                for (j, f) in r.top_files.iter().take(3).enumerate() {
                    println!("        {}. {}", j + 1, f);
                }
            }
        }
        by_tag.entry(if q.tag.is_empty() { "-".into() } else { q.tag.clone() }).or_default().push(i);
        results.push(r);
    }
    let s = bench::summarize(&results);
    println!("\n== atlas engine · {} questions · {:.0} ms ==", s.n, t0.elapsed().as_secs_f64() * 1000.0);
    println!("top-1 {:.1}%  top-5 {:.1}%  MRR {:.3}", s.top1 * 100.0, s.top5 * 100.0, s.mrr);
    for (tag, ids) in &by_tag {
        let sub: Vec<bench::QueryResult> =
            ids.iter().map(|&i| bench::QueryResult { rank: results[i].rank, top_files: Vec::new() }).collect();
        let t = bench::summarize(&sub);
        println!("  [{}] n={} top-1 {:.1}%  top-5 {:.1}%  MRR {:.3}", tag, t.n, t.top1 * 100.0, t.top5 * 100.0, t.mrr);
    }
}

/// `cortex bench-compare` : banc comparatif (voir `comparatif`).
fn cmd_bench_compare(file: &std::path::Path, project: Option<String>, sans_construction: bool, sortie: PathBuf) {
    let bench = match std::fs::read_to_string(file).map_err(|e| e.to_string()).and_then(|c| bench::parse(&c)) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("cortex: unreadable bench ({}): {}", file.display(), e);
            std::process::exit(1);
        }
    };
    let Some(project) = project.or(bench.project.clone()) else {
        eprintln!("cortex: give the project (-p) or the bench \"project\" field");
        std::process::exit(1);
    };
    // Index STABLE : pas de contrôle de fraîcheur (reproductible, comme `bench`).
    let handles = require_handles(&Some(project), true);
    comparatif::run(&bench, &handles, &comparatif::Options { measure_cortex_build: !sans_construction, out_dir: sortie });
}

fn cmd_stats(name: &str) {
    match atlas::ensure_and_open(name) {
        Ok(h) => {
            let (nf, ns, nl) = h.counts();
            println!("Project : {}", h.project);
            println!("Root    : {}", h.root());
            println!("Files   : {}", nf);
            println!("Lines   : {}", nl);
            println!("Symbols : {}", ns);
            println!("Segments: {}", h.segment_count());
            let mut by_lang: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
            for r in h.file_langs() {
                *by_lang.entry(r.as_str()).or_insert(0) += 1;
            }
            let mut langs: Vec<_> = by_lang.into_iter().collect();
            langs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
            print!("Languages: ");
            for (l, c) in langs.iter().take(8) {
                print!("{}({}) ", l, c);
            }
            println!();
        }
        Err(e) => {
            eprintln!("cortex: project '{}' not found ({}). Run: cortex index <path> --name {}", name, e, name);
            std::process::exit(1);
        }
    }
}

fn cmd_grep(needle: &str, project: Option<String>, case_sensitive: bool, max: usize, budget: usize, no_refresh: bool) {
    let t0 = Instant::now();
    let handles = require_handles(&project, no_refresh);
    let (out, n, scanned) = run_grep_on(&handles, needle, case_sensitive, max, budget);
    print!("{}", out);
    eprintln!("[cortex grep] {} occurrence(s) in {:.1}ms ({} files scanned)", n, t0.elapsed().as_secs_f64() * 1000.0, scanned);
}

fn cmd_files(pattern: &str, project: Option<String>, max: usize, no_refresh: bool) {
    let t0 = Instant::now();
    let handles = require_handles(&project, no_refresh);
    let (out, n) = run_files_on(&handles, pattern, max);
    print!("{}", out);
    eprintln!("[cortex files] {} file(s) in {:.1}ms", n, t0.elapsed().as_secs_f64() * 1000.0);
}

fn cmd_list() {
    let names = project_names(false);
    if names.is_empty() {
        println!("No indexed project. Run: cortex index <path>");
        return;
    }
    println!("Indexed projects (~/.cortex/):");
    for name in names {
        match atlas::Handle::open(&name) {
            Ok(h) => {
                let (nf, ns, _) = h.counts();
                println!("  - {:<20} {} files, {} symbols", name, nf, ns);
            }
            Err(_) => println!("  - {:<20} (to migrate: reindexed on next use)", name),
        }
    }
}

fn cmd_infra(project: &str, env: Option<PathBuf>) {
    let env_path = env.unwrap_or_else(|| PathBuf::from(".env"));
    if !env_path.exists() {
        eprintln!("cortex infra: .env not found: {} (give --env)", env_path.display());
        std::process::exit(1);
    }
    println!("Infra snapshot of {} (env: {})…", project, env_path.display());
    match infra::snapshot(project, &env_path) {
        Ok(n) => println!("OK: {} server(s) → ~/.cortex/infra/ (searchable + in the galaxy)", n),
        Err(e) => {
            eprintln!("cortex infra: {}", e);
            std::process::exit(1);
        }
    }
}

fn cmd_docs(action: DocsAction) {
    match action {
        DocsAction::Add { url, name, max } => {
            let t0 = Instant::now();
            println!("Scraping {} → doc '{}' (max {} pages)…", url, name, max);
            match scrape::scrape_site(&url, &name, max) {
                Ok(n) => println!("OK {}: {} pages scraped in {:.0}s → ~/.cortex/docs/{}", name, n, t0.elapsed().as_secs_f64(), name),
                Err(e) => {
                    eprintln!("cortex: scrape failed: {}", e);
                    std::process::exit(1);
                }
            }
        }
        DocsAction::Query { question, source, budget } => {
            print!("{}", scrape::search_docs(&question, source.as_deref(), budget));
        }
        DocsAction::Batch { config, workers, preset, default_max, concurrency, delay, auto, plain } => {
            let content = match std::fs::read_to_string(&config) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("cortex: cannot read {}: {}", config.display(), e);
                    std::process::exit(1);
                }
            };
            let (jobs, errors) = batch::parse_config(&content, default_max);
            for err in &errors {
                eprintln!("  ⚠ {}", err);
            }
            if jobs.is_empty() {
                eprintln!("cortex: no valid site in {}. Format: name | url | max_pages?", config.display());
                std::process::exit(1);
            }
            // Workers = sites en parallèle. Par défaut : TOUS les sites d'un coup
            // (scraping I/O-bound → on joue sur le temps, pas le CPU). --workers N
            // ou --preset light/normal/turbo pour limiter si besoin (réseau/RAM).
            let n_workers = workers.unwrap_or_else(|| if preset == "normal" { jobs.len() } else { batch::preset_workers(&preset) });
            if plain {
                // Mode simple (pipe/CI) : ancien affichage ligne-à-ligne.
                println!("Sites to scrape ({}):", jobs.len());
                for j in &jobs {
                    println!("  • {:<16} {} (max {})", j.name, j.url, j.max_pages);
                }
                println!();
                let results = batch::run_batch(&jobs, n_workers);
                println!("\n── Recap ──");
                for (name, res) in &results {
                    match res {
                        Ok(p) => println!("  ✓ {:<16} {} pages", name, p),
                        Err(e) => println!("  ✗ {:<16} FAILED: {}", name, e),
                    }
                }
            } else {
                // Dashboard TUI live (UI fixe + contrôles clavier).
                dash::run_dashboard(&jobs, n_workers, concurrency, delay, auto);
            }
        }
        DocsAction::List => {
            let docs = scrape::list_docs();
            if docs.is_empty() {
                println!("No scraped doc. Run: cortex docs add <url> --name <name>");
            } else {
                println!("Scraped docs (~/.cortex/docs/):");
                for (name, pages) in docs {
                    println!("  - {:<20} {} pages", name, pages);
                }
            }
        }
    }
}

/// Génère la galaxie (sauf --no-build) et sert le viewer 3D sur un port local.
fn cmd_viewer(port: u16, no_build: bool) {
    let galaxy_path = index::cortex_home().join("galaxy.json");
    if !no_build {
        cmd_galaxy(Some(galaxy_path.clone()));
    } else if !galaxy_path.exists() {
        eprintln!("cortex: no galaxy.json; drop --no-build to generate it.");
        std::process::exit(1);
    }
    let url = format!("http://127.0.0.1:{}/", port);
    println!("Cortex viewer → {}\n(Ctrl+C to stop)", url);
    open_browser(&url);
    serve_viewer(port, &galaxy_path);
}

/// Le viewer 3D est embarqué dans le binaire : rien à installer à côté.
/// `CORTEX_VIEWER_HTML` permet de servir une copie locale (développement du viewer).
const VIEWER_HTML: &str = include_str!("../viewer/cortex-viewer.html");

/// Ouvre l'URL dans le navigateur par défaut (Windows, macOS, Linux).
fn open_browser(url: &str) {
    let r = if cfg!(target_os = "windows") {
        std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn()
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(url).spawn()
    };
    if r.is_err() {
        eprintln!("cortex: open {} in your browser", url);
    }
}

/// Serveur HTTP minimal (std::net, zéro dépendance) : sert le HTML + galaxy.json.
fn serve_viewer(port: u16, galaxy_path: &std::path::Path) {
    use std::io::{Read, Write};
    let listener = match std::net::TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cortex: port {} is busy ({}). Try another --port.", port, e);
            std::process::exit(1);
        }
    };
    let html: Vec<u8> =
        std::env::var_os("CORTEX_VIEWER_HTML").and_then(|p| std::fs::read(p).ok()).unwrap_or_else(|| VIEWER_HTML.as_bytes().to_vec());
    for stream in listener.incoming() {
        let mut s = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        let mut buf = [0u8; 2048];
        let nread = s.read(&mut buf).unwrap_or(0);
        let req = String::from_utf8_lossy(&buf[..nread]);
        let path = req.split_whitespace().nth(1).unwrap_or("/");
        let (ctype, body): (&str, Vec<u8>) = if path.starts_with("/galaxy.json") {
            ("application/json", std::fs::read(galaxy_path).unwrap_or_default())
        } else {
            ("text/html; charset=utf-8", html.clone())
        };
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
            ctype,
            body.len()
        );
        let _ = s.write_all(header.as_bytes());
        let _ = s.write_all(&body);
        let _ = s.flush();
    }
}

/// `cortex ui` et l'outil MCP `cortex_ui` : délèguent au binaire compagnon `cortex-ui` (même dossier).
#[cfg(not(feature = "ui"))]
pub fn lancer_cortex_ui(args: &[String]) -> i32 {
    let exe = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join(format!("cortex-ui{}", std::env::consts::EXE_SUFFIX))));
    match exe.filter(|p| p.exists()) {
        Some(p) => std::process::Command::new(p).args(args).status().map(|s| s.code().unwrap_or(1)).unwrap_or(1),
        None => {
            eprintln!("cortex ui : binaire cortex-ui introuvable. Construire : cargo build --release --features ui --bin cortex-ui");
            1
        }
    }
}

/// Sortie texte du binaire compagnon (pour l'outil MCP `cortex_ui`).
#[cfg(not(feature = "ui"))]
pub fn sortie_cortex_ui(args: &[String]) -> String {
    let exe = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join(format!("cortex-ui{}", std::env::consts::EXE_SUFFIX))));
    match exe.filter(|p| p.exists()) {
        Some(p) => match std::process::Command::new(p).args(args).output() {
            Ok(o) => format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)),
            Err(e) => format!("(cortex) cortex-ui inaccessible : {e}"),
        },
        None => "(cortex) cortex-ui introuvable. Construire : cargo build --release --features ui --bin cortex-ui".to_string(),
    }
}
