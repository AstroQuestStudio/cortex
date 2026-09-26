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
    about = "Cortex — moteur de contexte code pour agents IA (find, card, read, impact…), by AstroQuest"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
    /// Nombre de threads de tout le code parallèle (défaut : un par cœur logique ;
    /// aussi `CORTEX_THREADS`).
    #[arg(long, global = true)]
    threads: Option<usize>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Indexe un projet (chemin) sous un nom. Ex: cortex index . --name MonProjet
    Index {
        /// Chemin du projet à indexer (défaut: répertoire courant).
        path: Option<PathBuf>,
        /// Nom du projet (défaut: nom du dossier).
        #[arg(short, long)]
        name: Option<String>,
    },
    /// Met à jour l'atlas : parcours complet, compare taille + hash blake3 de chaque
    /// fichier sur disque (indépendant de git : le travail non commité est vu), puis
    /// écrit un segment delta (ou reconstruit si le changement est massif).
    Update {
        /// Nom du projet déjà indexé.
        name: String,
        /// Chemin (défaut: la racine enregistrée dans l'index).
        path: Option<PathBuf>,
        /// Rapide : le contrôle de fraîcheur (mtime/taille lus dans les dossiers,
        /// nouveaux fichiers selon les mêmes règles d'exclusion), sans relire ni
        /// hacher les fichiers inchangés. Sans git.
        #[arg(long)]
        changed: bool,
    },
    /// Affiche les stats d'un index existant (fichiers, lignes, symboles, langages).
    Stats {
        /// Nom du projet indexé.
        name: String,
    },
    /// OÙ EST X ? Symboles classés pour une question (BM25F : noms camelCase, chemins,
    /// en-têtes de fichier, doc-comments, corps des fichiers ; racinisation FR/EN,
    /// synonymes FR↔EN, fautes tolérées). Une ligne par symbole : identifiant stable
    /// `S:chemin#nom`, genre, plage ; rôle des premiers.
    Find {
        /// Question en langage naturel ou mots-clés (FR ou EN).
        question: String,
        #[command(flatten)]
        c: Commun,
    },
    /// Alias de `find` (compatibilité).
    Query {
        question: String,
        #[command(flatten)]
        c: Commun,
    },
    /// C'EST QUOI ? Carte d'un symbole : signature, rôle, appelés, appelants (avec
    /// site d'appel), importeurs, tests, homonymes. Entrée : identifiant, nom,
    /// chemin:ligne ; un fichier donne son outline.
    Card {
        cible: String,
        #[command(flatten)]
        c: Commun,
    },
    /// Alias de `card` (compatibilité ; `-d`/`-l` acceptés et ignorés).
    Explain {
        symbol: String,
        #[arg(short, long, default_value_t = 1)]
        depth: usize,
        #[arg(short, long, default_value = "signatures")]
        level: String,
        #[command(flatten)]
        c: Commun,
    },
    /// Alias de `card` + les docs (docs/**/*.md) qui citent le symbole.
    Context {
        symbol: String,
        #[command(flatten)]
        c: Commun,
    },
    /// QUE CONTIENT CE FICHIER ? Rôle, imports, importeurs, symboles imbriqués avec
    /// plages et exports. Entrée : F:chemin, chemin, suffixe unique (`useX.ts`).
    Outline {
        cible: String,
        #[command(flatten)]
        c: Commun,
    },
    /// MONTRE LE CODE : les lignes exactes d'un symbole (section de doc, fichier,
    /// ou plage `chemin:12-40`), numérotées.
    Read {
        cible: String,
        /// Lignes de contexte de part et d'autre.
        #[arg(short = 'C', long = "contexte", default_value_t = 0)]
        contexte: u32,
        #[command(flatten)]
        c: Commun,
    },
    /// COMMENT MARCHE CE MODULE ? Fichiers et rôles, points d'entrée (importés de
    /// l'extérieur), dépendances sortantes/entrantes, paquets externes.
    Overview {
        /// Dossier (relatif à la racine du projet ; `.` = tout le projet).
        dossier: String,
        #[command(flatten)]
        c: Commun,
    },
    /// QU'EST-CE QUI CASSE SI JE CHANGE ÇA ? Appelants (et importeurs) transitifs par
    /// profondeur, avec sites d'appel, et tests à relancer.
    Impact {
        cible: String,
        /// Profondeur (1 à 6).
        #[arg(short, long, default_value_t = 3)]
        depth: usize,
        #[command(flatten)]
        c: Commun,
    },
    /// COMMENT A ARRIVE À B ? Plus court chemin d'appels (sinon d'imports), dans un
    /// sens ou dans l'autre.
    Path {
        de: String,
        vers: String,
        #[command(flatten)]
        c: Commun,
    },
    /// QU'AI-JE MODIFIÉ ? Fichiers non commités (git), fonctions touchées, leurs
    /// appelants hors du travail en cours et les tests à relancer.
    Changed {
        #[command(flatten)]
        c: Commun,
    },
    /// Banc d'AGENT (architecture v2 §7) : rejoue des tâches de compréhension avec
    /// Cortex et sans (rg + lecture de fichiers) ; appels, tokens lus, faits couverts.
    BenchAgent {
        /// Fichier de tâches (défaut : `$CORTEX_BENCH_DIR/agent_tasks.json`, sinon
        /// `bench/agent_tasks.json`).
        file: Option<PathBuf>,
        #[arg(short, long)]
        project: Option<String>,
        /// Affiche les sorties de chaque appel.
        #[arg(short, long)]
        verbose: bool,
        /// Ne joue que les tâches dont l'identifiant contient ce texte.
        #[arg(long)]
        tache: Option<String>,
    },
    /// Banc de pertinence : lance chaque question d'un fichier JSON et calcule
    /// top-1, top-5 et MRR sur le rang du fichier attendu.
    Bench {
        /// Fichier de banc JSON ({"project": .., "queries": [{"q","expect":[..]}]}).
        file: PathBuf,
        /// Projet à interroger (défaut: champ "project" du fichier).
        #[arg(short, long)]
        project: Option<String>,
        /// Affiche le détail de chaque question (sinon seulement les échecs).
        #[arg(short, long)]
        verbose: bool,
        /// Moteur mesuré : seul `atlas` existe (l'ancien moteur v1 a été retiré ;
        /// sa référence est figée : 72,0 % / 88,0 % / MRR 0,776).
        #[arg(short = 'm', long, default_value = "atlas")]
        moteur: String,
    },
    /// Banc COMPARATIF (architecture v2 §11) : mêmes questions, même juge, même
    /// corpus pour ripgrep par mots-clés, BM25 pur, RAG dense, hybride RRF et
    /// Cortex. Top-1/top-5/MRR, latence, construction, tokens lus par l'agent.
    /// Le dense et l'hybride exigent un build `--features bench`.
    BenchCompare {
        /// Fichier de banc JSON (même format que `cortex bench`).
        file: PathBuf,
        /// Projet à interroger (défaut: champ "project" du fichier).
        #[arg(short, long)]
        project: Option<String>,
        /// Ne mesure pas la construction complète d'un atlas Cortex (≈ 1 min).
        #[arg(long)]
        sans_construction: bool,
        /// Dossier du JSON de résultats (défaut : `$CORTEX_BENCH_RESULTS`, sinon
        /// `$CORTEX_BENCH_DIR/resultats`, sinon `bench/resultats`).
        #[arg(long)]
        sortie: Option<PathBuf>,
    },
    /// Recherche plein-texte ULTRA-RAPIDE dans le contenu (remplace grep). Multithread,
    /// scanne uniquement les fichiers indexés (pas de re-walk de node_modules).
    Grep {
        /// Chaîne/mot à chercher dans le contenu des fichiers.
        needle: String,
        /// Limiter à un projet (défaut: tous les projets actifs).
        #[arg(short, long)]
        project: Option<String>,
        /// Sensible à la casse (défaut: insensible).
        #[arg(short = 's', long)]
        case_sensitive: bool,
        /// Nombre max de résultats.
        #[arg(short, long, default_value_t = 60)]
        max: usize,
        /// Budget de sortie en tokens approximatifs.
        #[arg(short, long, default_value_t = 2000)]
        budget: usize,
        /// Saute le contrôle de fraîcheur automatique (lecture des dossiers) avant de répondre.
        #[arg(long)]
        no_refresh: bool,
    },
    /// Trouve un fichier par nom/fragment de chemin (remplace find -name). Instantané.
    Files {
        /// Fragment de nom/chemin (insensible à la casse), ou pattern glob si le
        /// pattern contient '*'/'?' (ex: "**/*.test.ts", "src/hooks/*.ts").
        pattern: String,
        /// Limiter à un projet (défaut: tous les projets actifs).
        #[arg(short, long)]
        project: Option<String>,
        /// Nombre max de fichiers affichés.
        #[arg(short, long, default_value_t = 80)]
        max: usize,
        /// Saute le contrôle de fraîcheur automatique (lecture des dossiers) avant de répondre.
        #[arg(long)]
        no_refresh: bool,
    },
    /// Liste les projets indexés.
    List,
    /// Exporte tous les projets + docs en galaxie 3D JSON (~/.cortex/galaxy.json).
    Galaxy {
        /// Chemin de sortie (défaut: ~/.cortex/galaxy.json).
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// Génère la galaxie et ouvre le VIEWER 3D dans le navigateur (serveur local).
    Viewer {
        /// Port du serveur local (défaut 7777).
        #[arg(short, long, default_value_t = 7777)]
        port: u16,
        /// Ne pas régénérer la galaxie (réutilise ~/.cortex/galaxy.json existant).
        #[arg(long)]
        no_build: bool,
    },
    /// Active/désactive un projet pour la recherche (focus). Sans arg: affiche l'état.
    Config {
        #[command(subcommand)]
        action: Option<ConfigAction>,
    },
    /// Lance le serveur MCP (stdio) — outils cortex_query/explain/context/grep/files/docs/docs_list/list.
    Mcp,
    /// Documentation offline : scrape un site de doc et la rend cherchable en local.
    Docs {
        #[command(subcommand)]
        action: DocsAction,
    },
    /// Snapshot de l'architecture des serveurs d'un projet (SSH lecture seule, sinon
    /// topologie statique). Serveurs lus dans un .env : chaque préfixe `<P>` qui a
    /// une clé `<P>_IPV4` (ou `<P>_HOST`) ; `<P>_SSH_PORT`, `<P>_LOGIN`,
    /// `<P>_SSH_KEY_PATH`, `<P>_LABEL` optionnels. Aucun secret n'est recopié.
    Infra {
        /// Nom du projet auquel rattacher l'infra.
        #[arg(short, long)]
        project: String,
        /// Chemin du .env contenant les coordonnées des serveurs (défaut : ./.env).
        #[arg(short, long)]
        env: Option<PathBuf>,
    },
    /// MET À JOUR TOUTE la base de connaissance : tous les index + la galaxie.
    UpdateAll,
    /// Banc de LATENCE (architecture v2 §4) : ouverture, requête (médiane/p95 sur
    /// le banc de pertinence), `context`, `files`, `grep`, contrôle de fraîcheur,
    /// mise à jour d'un fichier par delta, compaction.
    BenchLatence {
        #[arg(short, long)]
        project: String,
        /// Banc de pertinence dont les questions servent à mesurer `find` (défaut :
        /// `$CORTEX_BENCH_DIR/queries.json`, sinon `bench/queries.json`).
        #[arg(long)]
        questions: Option<PathBuf>,
        /// Courbe de passage à l'échelle : chaque poste à 1, 2, 4, 8 et 16 threads
        /// (médiane, min, max des passes) + construction complète ; JSON dans
        /// `bench/resultats/`.
        #[arg(long)]
        echelle: bool,
        /// Passes par nombre de threads (avec --echelle).
        #[arg(long, default_value_t = 3)]
        passes: usize,
        /// Ne mesure pas la construction complète (avec --echelle).
        #[arg(long)]
        sans_construction: bool,
        /// Nombres de threads mesurés (avec --echelle ; défaut : 1,2,4,8,16).
        #[arg(long, value_delimiter = ',')]
        threads_liste: Vec<usize>,
    },
}

/// Options communes des outils pour agents.
#[derive(clap::Args, Clone)]
struct Commun {
    /// Limiter à un projet (défaut : tous les projets actifs).
    #[arg(short, long)]
    project: Option<String>,
    /// Budget de sortie en tokens (≈ caractères / 4 ; défaut propre à l'outil).
    #[arg(short, long)]
    budget: Option<usize>,
    /// Saute le contrôle de fraîcheur automatique (lecture des dossiers) avant de répondre.
    #[arg(long)]
    no_refresh: bool,
}

#[derive(Subcommand)]
enum DocsAction {
    /// Scrape un site de doc en local. Ex: cortex docs add https://react.dev/reference --name React
    Add {
        /// URL de départ du crawl (même domaine, sous-chemin de doc déduit de l'URL).
        url: String,
        /// Nom de la doc (sert de --source pour `docs query`).
        #[arg(short, long)]
        name: String,
        /// Nombre max de pages à crawler (défaut 200).
        #[arg(short, long, default_value_t = 200)]
        max: usize,
    },
    /// Recherche dans les docs scrapées (offline).
    Query {
        /// Question / mots-clés.
        question: String,
        /// Limiter à une doc (ex: React, Tauri). Défaut: toutes.
        #[arg(short, long)]
        source: Option<String>,
        /// Budget de sortie en tokens approximatifs.
        #[arg(short, long, default_value_t = 1500)]
        budget: usize,
    },
    /// Scrape EN BATCH une liste de sites (config file) avec DASHBOARD live.
    /// Format config (1 ligne/site) : nom | url | max_pages?
    /// Contrôles live : +/- concurrence · [ ] délai · ↑↓ select · p pause · q quitter.
    Batch {
        /// Fichier de config (liste de sites).
        config: PathBuf,
        /// Nombre de SITES en parallèle (défaut: TOUS). À ne pas confondre avec
        /// --concurrency (pages en vol DANS un site). Ex: 14 sites × 20 pages.
        #[arg(short, long)]
        workers: Option<usize>,
        /// Limite le nb de sites en // : light(2) | normal(tous) | turbo(8).
        #[arg(short, long, default_value = "normal")]
        preset: String,
        /// max_pages par défaut si non précisé dans la config (garde-fou crawl complet).
        #[arg(short = 'm', long, default_value_t = 800)]
        default_max: usize,
        /// Concurrence intra-site initiale (pages en vol par site). Réglable en live (←/→).
        #[arg(short, long, default_value_t = 20)]
        concurrency: usize,
        /// Délai min entre lancements de requêtes (ms). Réglable en live.
        #[arg(short = 'd', long, default_value_t = 120)]
        delay: u64,
        /// AUTO-relance : les sites qui atteignent leur budget continuent
        /// automatiquement jusqu'à épuisement (finissent seuls). Toggle live: touche a.
        #[arg(long, default_value_t = true)]
        auto: bool,
        /// Affichage simple (pas de dashboard TUI) — pour logs/CI/pipe.
        #[arg(long)]
        plain: bool,
    },
    /// Liste les docs scrapées.
    List,
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Active un projet (inclus dans la recherche cross-projets).
    Enable { project: String },
    /// Désactive un projet (exclu de la recherche — focus + vitesse).
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
            Err(e) => eprintln!("[cortex] atlas '{}' indisponible ({}) — ignoré", n, e),
        }
    }
    handles
}

/// Log discret (stderr) du coût/résultat du contrôle de fraîcheur.
fn report_refresh(project: &str, s: atlas::fresh::RefreshStats) {
    if s.walked {
        eprintln!("[cortex] fraîcheur {} : règles d'exclusion changées (ou premier contrôle) — parcours complet", project);
    }
    if s.refreshed {
        eprintln!(
            "[cortex] fraîcheur {} : contrôle {:.1}ms ({} vérifiés) puis {} chemin(s) réindexé(s) en {:.1}ms",
            project, s.check_ms, s.checked, s.examined, s.update_ms
        );
    } else if s.examined > 0 {
        eprintln!(
            "[cortex] fraîcheur {} : {} chemin(s) relu(s), contenu inchangé ({:.1}ms)",
            project,
            s.examined,
            s.check_ms + s.update_ms
        );
    } else {
        eprintln!("[cortex] fraîcheur {} : rien de changé ({:.1}ms, {} vérifiés)", project, s.check_ms, s.checked);
    }
}

fn require_handles(project: &Option<String>, no_refresh: bool) -> Vec<atlas::Handle> {
    let handles = atlas_handles(project, no_refresh);
    if handles.is_empty() {
        match project {
            Some(n) => eprintln!("cortex: projet '{}' introuvable. Lance: cortex index <path> --name {}", n, n),
            None => eprintln!("cortex: aucun projet actif. Lance: cortex index <path> (ou cortex config enable <projet>)"),
        }
        std::process::exit(1);
    }
    handles
}

/// Liste de fichiers de chaque atlas pour `textsearch` (chaînes mmap-ées).
fn file_sets(handles: &[atlas::Handle]) -> Vec<textsearch::FileSet<'_>> {
    handles.iter().map(|h| textsearch::FileSet { project: &h.project, root: h.root(), paths: h.file_paths() }).collect()
}

/// Un outil pour agents (`outils`) en CLI : ouverture, fraîcheur, sortie.
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
        return (format!("(aucun fichier ne contient '{}')\n", pattern), 0);
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
        eprintln!("cortex: aucun projet indexé. Lance d'abord: cortex index <path> --name <Projet>");
        std::process::exit(1);
    }
    println!("Mise à jour de {} projet(s)…", names.len());
    let (mut ok, mut skipped) = (0, 0);
    for n in &names {
        let Some(root) = atlas::project_root(n) else {
            eprintln!("  ⚠ {} : racine inconnue — ignoré", n);
            skipped += 1;
            continue;
        };
        let root = PathBuf::from(root);
        if !root.exists() {
            eprintln!("  ⚠ {} : source absente ({}) — ignoré", n, root.display());
            skipped += 1;
            continue;
        }
        match atlas::fresh::update_full(n, &root) {
            Ok((out, changed, total)) => {
                println!("  ✓ {} : {} chemin(s) changé(s) sur {} → {:?}", n, changed, total, out);
                ok += 1;
            }
            Err(e) => {
                eprintln!("  ⚠ {} : {} — ignoré", n, e);
                skipped += 1;
            }
        }
    }
    println!("\nRégénération de la galaxie…");
    cmd_galaxy(None);
    println!("\n✓ {} projet(s) à jour ({} ignoré(s)) en {:.1}s", ok, skipped, t0.elapsed().as_secs_f64());
}

fn cmd_config(action: Option<ConfigAction>) {
    let mut cfg = config::Config::load();
    match action {
        Some(ConfigAction::Enable { project }) => {
            cfg.enable(&project);
            cfg.save().ok();
            println!("OK {} active", project);
        }
        Some(ConfigAction::Disable { project }) => {
            cfg.disable(&project);
            cfg.save().ok();
            println!("OK {} desactive (exclu de la recherche)", project);
        }
        None => {
            println!("Projets (actif/desactive) :");
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
        eprintln!("cortex: aucun index. Lance: cortex index <path>");
        std::process::exit(1);
    }
    let out = out.unwrap_or_else(|| index::cortex_home().join("galaxy.json"));
    match galaxy::build_galaxy(&indexes, &out) {
        Ok((n, p)) => {
            println!("OK galaxie : {} symboles, {} projets - {} - {:.2}s", n, p, out.display(), t0.elapsed().as_secs_f64());
        }
        Err(e) => {
            eprintln!("cortex: échec galaxy: {}", e);
            std::process::exit(1);
        }
    }
}

fn cmd_index(path: Option<PathBuf>, name: Option<String>) {
    let root = path.unwrap_or_else(|| PathBuf::from("."));
    let root = match root.canonicalize() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("cortex: chemin invalide: {}", e);
            std::process::exit(1);
        }
    };
    let name = name.unwrap_or_else(|| root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "projet".to_string()));
    let t0 = Instant::now();
    match index::build_index(&name, &root) {
        Ok((idx, tracked)) => {
            let (nf, ns, nl) = idx.stats();
            if let Err(e) = atlas::rebuild_full(&name, &idx, tracked) {
                eprintln!("cortex: échec écriture de l'atlas: {}", e);
                std::process::exit(1);
            }
            println!("OK {} indexe : {} fichiers - {} lignes - {} symboles - {:.2}s", name, nf, nl, ns, t0.elapsed().as_secs_f64());
            println!("  -> {}", atlas::atlas_root_for(&name).display());
        }
        Err(e) => {
            eprintln!("cortex: échec indexation: {}", e);
            std::process::exit(1);
        }
    }
}

fn cmd_update(name: &str, path: Option<PathBuf>) {
    let root = match path.map(|p| p.to_string_lossy().to_string()).or_else(|| atlas::project_root(name)) {
        Some(r) => PathBuf::from(r),
        None => {
            eprintln!("cortex: projet '{}' introuvable. Lance d'abord: cortex index <path> --name {}", name, name);
            std::process::exit(1);
        }
    };
    let root = root.canonicalize().unwrap_or(root);
    let t0 = Instant::now();
    match atlas::fresh::update_full(name, &root) {
        Ok((out, changed, total)) => {
            println!(
                "OK {} a jour : {} chemin(s) changé(s) sur {} fichiers ({:?}) - {:.2}s",
                name,
                changed,
                total,
                out,
                t0.elapsed().as_secs_f64()
            );
        }
        Err(e) => {
            eprintln!("cortex: échec update: {}", e);
            std::process::exit(1);
        }
    }
}

/// `cortex update --changed` : contrôle rapide (lecture des dossiers, sans git).
fn cmd_update_changed(name: &str) {
    let t0 = Instant::now();
    match atlas::fresh::update_changed(name) {
        Ok((out, n)) => println!(
            "OK {} a jour (--changed, {} chemin(s) relu(s) ou retiré(s)) : {:?} - {:.2}s",
            name,
            n,
            out,
            t0.elapsed().as_secs_f64()
        ),
        Err(e) => {
            eprintln!("cortex: {} — utilise `cortex update {}` (complet).", e, name);
            std::process::exit(1);
        }
    }
}

/// `cortex bench` : top-1 / top-5 / MRR du classement de query sur un banc JSON.
fn cmd_bench(file: &std::path::Path, project: Option<String>, verbose: bool, moteur: &str) {
    if moteur != "atlas" {
        eprintln!("cortex: seul le moteur 'atlas' existe (v1 retiré : 72,0 % / 88,0 % / MRR 0,776, voir docs/ARCHITECTURE.md)");
        std::process::exit(1);
    }
    let content = match std::fs::read_to_string(file) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cortex: impossible de lire {}: {}", file.display(), e);
            std::process::exit(1);
        }
    };
    let bench = match bench::parse(&content) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("cortex: banc invalide ({}): {}", file.display(), e);
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
            println!("[{:>2}] rang {:>2}  {:<48} attendu {}", i + 1, ok, q.q, q.expect.join(" | "));
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
    println!("\n== moteur atlas · {} questions · {:.0} ms ==", s.n, t0.elapsed().as_secs_f64() * 1000.0);
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
            eprintln!("cortex: banc illisible ({}): {}", file.display(), e);
            std::process::exit(1);
        }
    };
    let Some(project) = project.or(bench.project.clone()) else {
        eprintln!("cortex: précise le projet (-p) ou le champ \"project\" du banc");
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
            println!("Projet  : {}", h.project);
            println!("Racine  : {}", h.root());
            println!("Fichiers: {}", nf);
            println!("Lignes  : {}", nl);
            println!("Symboles: {}", ns);
            println!("Segments: {}", h.segment_count());
            let mut by_lang: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
            for r in h.file_langs() {
                *by_lang.entry(r.as_str()).or_insert(0) += 1;
            }
            let mut langs: Vec<_> = by_lang.into_iter().collect();
            langs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
            print!("Langages: ");
            for (l, c) in langs.iter().take(8) {
                print!("{}({}) ", l, c);
            }
            println!();
        }
        Err(e) => {
            eprintln!("cortex: projet '{}' introuvable ({}). Lance: cortex index <path> --name {}", name, e, name);
            std::process::exit(1);
        }
    }
}

fn cmd_grep(needle: &str, project: Option<String>, case_sensitive: bool, max: usize, budget: usize, no_refresh: bool) {
    let t0 = Instant::now();
    let handles = require_handles(&project, no_refresh);
    let (out, n, scanned) = run_grep_on(&handles, needle, case_sensitive, max, budget);
    print!("{}", out);
    eprintln!("[cortex grep] {} occurrence(s) en {:.1}ms ({} fichiers scannés)", n, t0.elapsed().as_secs_f64() * 1000.0, scanned);
}

fn cmd_files(pattern: &str, project: Option<String>, max: usize, no_refresh: bool) {
    let t0 = Instant::now();
    let handles = require_handles(&project, no_refresh);
    let (out, n) = run_files_on(&handles, pattern, max);
    print!("{}", out);
    eprintln!("[cortex files] {} fichier(s) en {:.1}ms", n, t0.elapsed().as_secs_f64() * 1000.0);
}

fn cmd_list() {
    let names = project_names(false);
    if names.is_empty() {
        println!("Aucun projet indexe. Lance: cortex index <path>");
        return;
    }
    println!("Projets indexes (~/.cortex/) :");
    for name in names {
        match atlas::Handle::open(&name) {
            Ok(h) => {
                let (nf, ns, _) = h.counts();
                println!("  - {:<20} {} fichiers, {} symboles", name, nf, ns);
            }
            Err(_) => println!("  - {:<20} (à migrer : réindexé au prochain usage)", name),
        }
    }
}

fn cmd_infra(project: &str, env: Option<PathBuf>) {
    let env_path = env.unwrap_or_else(|| PathBuf::from(".env"));
    if !env_path.exists() {
        eprintln!("cortex infra: .env introuvable: {} (précise --env)", env_path.display());
        std::process::exit(1);
    }
    println!("Snapshot infra de {} (env: {})…", project, env_path.display());
    match infra::snapshot(project, &env_path) {
        Ok(n) => println!("OK : {} serveur(s) → ~/.cortex/infra/ (cherchables + dans la galaxie)", n),
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
                Ok(n) => println!("OK {} : {} pages scrapees en {:.0}s → ~/.cortex/docs/{}", name, n, t0.elapsed().as_secs_f64(), name),
                Err(e) => {
                    eprintln!("cortex: scrape echoue: {}", e);
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
                    eprintln!("cortex: impossible de lire {}: {}", config.display(), e);
                    std::process::exit(1);
                }
            };
            let (jobs, errors) = batch::parse_config(&content, default_max);
            for err in &errors {
                eprintln!("  ⚠ {}", err);
            }
            if jobs.is_empty() {
                eprintln!("cortex: aucun site valide dans {}. Format: nom | url | max_pages?", config.display());
                std::process::exit(1);
            }
            // Workers = sites en parallèle. Par défaut : TOUS les sites d'un coup
            // (scraping I/O-bound → on joue sur le temps, pas le CPU). --workers N
            // ou --preset light/normal/turbo pour limiter si besoin (réseau/RAM).
            let n_workers = workers.unwrap_or_else(|| if preset == "normal" { jobs.len() } else { batch::preset_workers(&preset) });
            if plain {
                // Mode simple (pipe/CI) : ancien affichage ligne-à-ligne.
                println!("Sites à scraper ({}) :", jobs.len());
                for j in &jobs {
                    println!("  • {:<16} {} (max {})", j.name, j.url, j.max_pages);
                }
                println!();
                let results = batch::run_batch(&jobs, n_workers);
                println!("\n── Récap ──");
                for (name, res) in &results {
                    match res {
                        Ok(p) => println!("  ✓ {:<16} {} pages", name, p),
                        Err(e) => println!("  ✗ {:<16} ÉCHEC: {}", name, e),
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
                println!("Aucune doc scrapee. Lance: cortex docs add <url> --name <nom>");
            } else {
                println!("Docs scrapees (~/.cortex/docs/) :");
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
        eprintln!("cortex: pas de galaxy.json — retire --no-build pour la générer.");
        std::process::exit(1);
    }
    let url = format!("http://127.0.0.1:{}/", port);
    println!("Viewer Cortex → {}\n(Ctrl+C pour arrêter)", url);
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
        eprintln!("cortex: ouvre {} dans ton navigateur", url);
    }
}

/// Serveur HTTP minimal (std::net, zéro dépendance) : sert le HTML + galaxy.json.
fn serve_viewer(port: u16, galaxy_path: &std::path::Path) {
    use std::io::{Read, Write};
    let listener = match std::net::TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cortex: port {} occupé ({}). Essaie --port autre.", port, e);
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
