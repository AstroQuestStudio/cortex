//! Parcours gitignore-aware d'un projet — LA règle d'exclusion de Cortex.
//!
//! Une seule définition (`builder`) sert à l'indexation complète, à `cortex
//! update`, au contrôle de fraîcheur et à `cortex update --changed` : un
//! fichier est dans l'atlas si et seulement si ce parcours le rend (`.gitignore`
//! de tous les niveaux, `.ignore`, `.git/info/exclude`, exclusions globales de
//! git, dossiers toujours ignorés de `index::EXTRA_IGNORE_DIRS`).
//!
//! Le parcours complet est parallèle (`ignore::WalkParallel`, threads de
//! `par::threads()`), puis trié : son résultat ne dépend pas du nombre de
//! threads. `list_dirs` lit un ensemble de dossiers en parallèle (un `readdir`
//! par dossier ; sous Windows, les métadonnées viennent avec la liste, sans
//! `stat`) : c'est la base du contrôle de fraîcheur sans `git status`.

use crate::atlas::manifest::{SkippedPath, WalkState};
use crate::fx::FxHashSet;
use crate::index::{mtime_micros, rel_path, EXTRA_IGNORE_DIRS};
use crate::lang::Lang;
use ignore::WalkBuilder;
use rayon::prelude::*;
use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

/// Noms des fichiers de règles lus DANS les dossiers parcourus.
pub const RULE_FILES: &[&str] = &[".gitignore", ".ignore"];

/// Le parcours de référence, enraciné en `dir`.
fn builder(dir: &Path) -> WalkBuilder {
    let mut b = WalkBuilder::new(dir);
    b.hidden(false).git_ignore(true).git_global(true).ignore(true).filter_entry(|e| {
        let name = e.file_name().to_string_lossy();
        !EXTRA_IGNORE_DIRS.contains(&name.as_ref())
    });
    b
}

/// Vrai si ce chemin a une extension indexable (le parcours ne garde que ceux-là).
pub fn indexable(rel: &str) -> bool {
    Lang::from_path(rel).is_indexable()
}

/// Un fichier indexable rendu par le parcours.
#[derive(Debug, Clone)]
pub struct WalkFile {
    pub rel: String,
    pub mtime: u64,
    pub size: u64,
}

/// Résultat d'un parcours : fichiers indexables et dossiers visités (chemins
/// relatifs à la racine du projet, triés ; `""` = la racine).
#[derive(Debug, Default)]
pub struct Walk {
    pub files: Vec<WalkFile>,
    pub dirs: Vec<String>,
}

/// Parcours COMPLET du sous-arbre `sub` (`""` : tout le projet), en parallèle.
pub fn walk(root: &Path, sub: &str) -> Walk {
    let start = if sub.is_empty() { root.to_path_buf() } else { root.join(sub) };
    let (tx, rx) = std::sync::mpsc::channel::<(bool, WalkFile)>();
    let mut b = builder(&start);
    b.threads(crate::par::threads());
    b.build_parallel().run(|| {
        let tx = tx.clone();
        Box::new(move |res| {
            if let Ok(e) = res {
                let Some(ft) = e.file_type() else { return ignore::WalkState::Continue };
                if ft.is_dir() {
                    let rel = if e.depth() == 0 { sub.to_string() } else { rel_path(e.path(), root) };
                    let _ = tx.send((true, WalkFile { rel, mtime: 0, size: 0 }));
                } else if ft.is_file() {
                    let rel = rel_path(e.path(), root);
                    if indexable(&rel) {
                        let (mtime, size) = e.metadata().map(|m| (mtime_micros(&m), m.len())).unwrap_or((0, 0));
                        let _ = tx.send((false, WalkFile { rel, mtime, size }));
                    }
                }
            }
            ignore::WalkState::Continue
        })
    });
    drop(tx);
    let mut w = Walk::default();
    for (is_dir, f) in rx {
        if is_dir {
            w.dirs.push(f.rel);
        } else {
            w.files.push(f);
        }
    }
    w.files.sort_by(|a, b| a.rel.cmp(&b.rel));
    w.dirs.sort();
    w
}

/// Noms des entrées que le parcours garde DIRECTEMENT dans le dossier `dir`
/// (règles des dossiers parents comprises) : décide d'une entrée nouvelle sans
/// reparcourir le projet.
pub fn children(root: &Path, dir: &str) -> HashSet<String> {
    let start = if dir.is_empty() { root.to_path_buf() } else { root.join(dir) };
    let mut b = builder(&start);
    b.max_depth(Some(1));
    b.build().flatten().filter(|e| e.depth() == 1).map(|e| e.file_name().to_string_lossy().into_owned()).collect()
}

/// Genre d'une entrée de dossier (les liens symboliques ne sont pas suivis).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Other,
}

/// Une entrée lue par `list_dirs`.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub kind: Kind,
    pub mtime: u64,
    pub size: u64,
}

/// Liste d'un dossier (`None` : illisible ou disparu).
pub fn list_dir(root: &Path, dir: &str) -> Option<Vec<Entry>> {
    let path = if dir.is_empty() { root.to_path_buf() } else { root.join(dir) };
    let rd = std::fs::read_dir(path).ok()?;
    let mut out = Vec::new();
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        let kind = if ft.is_file() {
            Kind::File
        } else if ft.is_dir() {
            Kind::Dir
        } else {
            Kind::Other
        };
        let (mtime, size) = if kind == Kind::File {
            match e.metadata() {
                Ok(m) => (mtime_micros(&m), m.len()),
                Err(_) => continue,
            }
        } else {
            (0, 0)
        };
        out.push(Entry { name: e.file_name().to_string_lossy().into_owned(), kind, mtime, size });
    }
    Some(out)
}

/// `a/b` + `c` → `a/b/c` ; racine + `c` → `c`.
pub fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", dir, name)
    }
}

/// Dossier parent d'un chemin relatif (`""` : la racine).
pub fn parent(rel: &str) -> &str {
    rel.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

/// Tous les dossiers ancêtres des chemins donnés, racine `""` comprise.
pub fn ancestors<'a>(paths: impl Iterator<Item = &'a str>) -> BTreeSet<String> {
    let mut seen: FxHashSet<&str> = FxHashSet::default();
    seen.insert("");
    for p in paths {
        let mut d = parent(p);
        while !seen.contains(d) {
            seen.insert(d);
            d = parent(d);
        }
    }
    seen.into_iter().map(String::from).collect()
}

/// Règles et entrées exclues vues dans les dossiers `dirs` d'un parcours dont
/// les dossiers visités sont `visited` et les fichiers gardés `kept`.
pub fn rules_and_ignored(root: &Path, dirs: &[String], visited: &HashSet<&str>, kept: &HashSet<&str>) -> (Vec<SkippedPath>, Vec<String>) {
    let per_dir: Vec<(Vec<SkippedPath>, Vec<String>)> = dirs
        .par_iter()
        .map(|d| {
            let mut rules = Vec::new();
            let mut ignored = Vec::new();
            for e in list_dir(root, d).unwrap_or_default() {
                if EXTRA_IGNORE_DIRS.contains(&e.name.as_str()) {
                    continue;
                }
                let rel = join(d, &e.name);
                match e.kind {
                    Kind::Dir if !visited.contains(rel.as_str()) => ignored.push(rel),
                    Kind::File => {
                        if RULE_FILES.contains(&e.name.as_str()) {
                            rules.push(SkippedPath { path: rel.clone(), mtime: e.mtime, size: e.size });
                        }
                        if indexable(&rel) && !kept.contains(rel.as_str()) {
                            ignored.push(rel);
                        }
                    }
                    _ => {}
                }
            }
            (rules, ignored)
        })
        .collect();
    let mut rules = Vec::new();
    let mut ignored = Vec::new();
    for (r, i) in per_dir {
        rules.extend(r);
        ignored.extend(i);
    }
    rules.sort_by(|a, b| a.path.cmp(&b.path));
    ignored.sort();
    (rules, ignored)
}

/// État complet après un parcours complet. `tracked` : chemins effectivement
/// suivis ensuite par l'atlas (indexés ou `skipped`).
pub fn full_state<'a>(root: &Path, w: &Walk, tracked: impl Iterator<Item = &'a str>) -> WalkState {
    let visited: HashSet<&str> = w.dirs.iter().map(|s| s.as_str()).collect();
    let kept: HashSet<&str> = w.files.iter().map(|f| f.rel.as_str()).collect();
    let (rules, ignored) = rules_and_ignored(root, &w.dirs, &visited, &kept);
    let derived = ancestors(tracked);
    let extra_dirs = w.dirs.iter().filter(|d| !derived.contains(d.as_str())).cloned().collect();
    WalkState { extra_dirs, ignored, rules, outside: outside_rules(root) }
}

/// Règles qui agissent sur le parcours SANS être dans un dossier parcouru :
/// `.gitignore`/`.ignore` des dossiers parents de la racine, présence de `.git`
/// (les règles git ne s'appliquent que dans un dépôt), `info/exclude` du dépôt
/// et exclusions globales de git (fichiers de configuration + fichier
/// désigné). Quelques `stat`, triés par chemin.
pub fn outside_rules(root: &Path) -> Vec<SkippedPath> {
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut git_markers: Vec<PathBuf> = Vec::new();
    let mut repo_git: Option<PathBuf> = None;
    let mut cur = Some(root);
    let mut first = true;
    while let Some(d) = cur {
        if !first {
            for r in RULE_FILES {
                paths.push(d.join(r));
            }
        }
        let git = d.join(".git");
        git_markers.push(git.clone());
        if repo_git.is_none() && git.exists() {
            repo_git = Some(git);
        }
        first = false;
        cur = d.parent();
    }
    if let Some(g) = repo_git {
        let gitdir = if g.is_file() {
            std::fs::read_to_string(&g)
                .ok()
                .and_then(|s| s.trim().strip_prefix("gitdir:").map(|p| p.trim().to_string()))
                .map(|p| g.parent().map(|par| par.join(&p)).unwrap_or_else(|| PathBuf::from(&p)))
        } else {
            Some(g)
        };
        if let Some(gd) = gitdir {
            paths.push(gd.join("info").join("exclude"));
            if let Ok(common) = std::fs::read_to_string(gd.join("commondir")) {
                paths.push(gd.join(common.trim()).join("info").join("exclude"));
            }
        }
    }
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from) {
        let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config"));
        let configs = [home.join(".gitconfig"), xdg.join("git").join("config")];
        paths.push(xdg.join("git").join("ignore"));
        for c in &configs {
            if let Some(p) = excludes_file(c, &home) {
                paths.push(p);
            }
        }
        paths.extend(configs);
    }
    let mut out: Vec<SkippedPath> = paths
        .iter()
        .map(|p| {
            let (mtime, size) = std::fs::metadata(p).map(|m| (mtime_micros(&m), m.len())).unwrap_or((0, u64::MAX));
            SkippedPath { path: crate::index::normalize_root(p), mtime, size }
        })
        .collect();
    // `.git` : seule sa PRÉSENCE compte (son mtime bouge à chaque commande git).
    out.extend(git_markers.iter().map(|p| SkippedPath {
        path: crate::index::normalize_root(p),
        mtime: 0,
        size: if p.exists() { 0 } else { u64::MAX },
    }));
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out.dedup_by(|a, b| a.path == b.path);
    out
}

/// `core.excludesFile` d'un fichier de configuration git (lecture simple).
fn excludes_file(config: &Path, home: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(config).ok()?;
    let mut in_core = false;
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            in_core = l.to_ascii_lowercase().starts_with("[core");
            continue;
        }
        if !in_core {
            continue;
        }
        if let Some((k, v)) = l.split_once('=') {
            if k.trim().eq_ignore_ascii_case("excludesfile") {
                let v = v.trim().trim_matches('"');
                return Some(match v.strip_prefix("~/") {
                    Some(rest) => home.join(rest),
                    None => PathBuf::from(v),
                });
            }
        }
    }
    None
}
