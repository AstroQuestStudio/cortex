//! Fraîcheur de l'atlas — UNE seule voie, sur l'atlas : détection des chemins
//! changés, lecture/extraction de ceux-là seulement, puis `incremental::apply`
//! (segment delta). Utilisée avant chaque réponse (CLI et MCP), par
//! `cortex update --changed` (même contrôle, explicite) et, avec comparaison
//! par empreinte blake3 de chaque fichier, par `cortex update`.
//!
//! **Sans git.** Le contrôle lit les dossiers que le parcours de référence
//! (`walk`) a visités, un `readdir` par dossier en parallèle (sous Windows, la
//! liste porte déjà mtime et taille) :
//! - un fichier suivi dont (mtime µs, taille) a changé, ou disparu, est relu ;
//! - une entrée INCONNUE (fichier d'extension indexable, dossier) est jugée
//!   par la règle du parcours pour son seul dossier (`walk::children`) : gardée,
//!   elle est lue (un dossier nouveau est parcouru en entier) ; exclue, elle est
//!   mémorisée (`WalkState.ignored`) pour ne plus être réexaminée ;
//! - un fichier de règles (`.gitignore`, `.ignore`, règles hors du projet :
//!   voir `walk::outside_rules`) créé, modifié ou supprimé impose un parcours
//!   complet, comme `cortex update` (hors comparaison par empreinte).
//! Un fichier est donc dans l'atlas si et seulement si le parcours de référence
//! le rend : le travail non commité est vu, un fichier ignoré ne l'est jamais,
//! et `update`, `update --changed` et le contrôle automatique sont d'accord.
//! Un chemin inindexable (binaire) est mémorisé (`Manifest.skipped`) avec son
//! empreinte, pour ne pas être relu tant qu'il ne change pas.

use super::incremental::{self, Change, Outcome};
use super::manifest::{SkippedPath, Tracked, WalkState};
use super::Handle;
use crate::fx::{FxHashMap, FxHashSet};
use crate::index;
use crate::lang::Lang;
use crate::walk::{self, Kind};
use rayon::prelude::*;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Statistiques d'un contrôle de fraîcheur.
#[derive(Debug, Default, Clone, Copy)]
pub struct RefreshStats {
    /// Fichiers connus vérifiés.
    pub checked: usize,
    /// Chemins relus (changés d'après mtime/taille, ou nouveaux) ou retirés.
    pub examined: usize,
    /// Vrai si l'atlas a été mis à jour.
    pub refreshed: bool,
    /// Vrai si le contrôle a dû refaire un parcours complet (règles changées,
    /// ou premier contrôle d'un atlas sans état de parcours).
    pub walked: bool,
    /// Durée du contrôle seul (lecture des dossiers).
    pub check_ms: f64,
    /// Durée de la mise à jour (lecture, extraction, delta).
    pub update_ms: f64,
}

/// Résultat d'une détection : quoi relire, quoi retirer, et le nouvel état
/// du parcours (hors `extra_dirs`, qui dépend des fichiers suivis après lecture).
struct Detection {
    /// Chemins à relire (changés ou nouveaux, gardés par la règle du parcours).
    stale: Vec<String>,
    /// Chemins suivis (indexés ou `skipped`) que le parcours ne rend plus.
    gone: Vec<String>,
    checked: usize,
    /// Dossiers visités par le parcours après ce contrôle.
    visited: BTreeSet<String>,
    ignored: Vec<String>,
    rules: Vec<SkippedPath>,
    outside: Vec<SkippedPath>,
    /// Ancêtres des chemins suivis AVANT le contrôle.
    derived: BTreeSet<String>,
    walked: bool,
}

/// Tout ce qui est suivi : fichiers indexés (chemin, mtime, taille) puis `skipped`.
fn tracked_meta(h: &Handle) -> Vec<(&str, u64, u64)> {
    let mut v: Vec<(&str, u64, u64)> = h
        .live_files()
        .map(|r| {
            let (mt, sz) = h.meta(&r);
            (r.name(), mt, sz)
        })
        .collect();
    v.extend(h.manifest.skipped.iter().map(|s| (s.path.as_str(), s.mtime, s.size)));
    v
}

/// Ce qu'a donné la lecture d'un dossier visité.
#[derive(Default)]
struct DirScan {
    stale: Vec<String>,
    gone: Vec<String>,
    /// Entrées inconnues à juger : (chemin, est un dossier).
    candidates: Vec<(String, bool)>,
    rules: Vec<SkippedPath>,
    ignored: Vec<String>,
    /// Dossier illisible ou disparu.
    lost: bool,
}

/// Contrôle bon marché (voir l'en-tête) ; retombe sur `detect_full` sans état
/// de parcours ou si une règle a changé.
fn detect(h: &Handle, root: &Path) -> Detection {
    let dbg = std::env::var("CORTEX_DEBUG_TIMING").is_ok();
    let t0 = Instant::now();
    let Some(state) = h.manifest.walk.as_ref() else { return detect_full(h, root) };
    let tracked = tracked_meta(h);
    let checked = tracked.len() - h.manifest.skipped.len();
    let derived = walk::ancestors(tracked.iter().map(|t| t.0));
    let mut by_dir: FxHashMap<&str, Vec<(&str, u64, u64)>> = FxHashMap::default();
    for &(p, mt, sz) in &tracked {
        by_dir.entry(walk::parent(p)).or_default().push((p, mt, sz));
    }
    let mut visited: BTreeSet<String> = derived.clone();
    visited.extend(state.extra_dirs.iter().cloned());
    let visited_set: FxHashSet<&str> = visited.iter().map(|s| s.as_str()).collect();
    let ignored_set: FxHashSet<&str> = state.ignored.iter().map(|s| s.as_str()).collect();
    let dirs: Vec<&str> = visited.iter().map(|s| s.as_str()).collect();
    if dbg {
        eprintln!("[timing:fraîcheur] préparation ({} dossiers): {:.2}ms", dirs.len(), t0.elapsed().as_secs_f64() * 1000.0);
    }
    // Règles hors du projet contrôlées PENDANT la lecture des dossiers (un
    // changement, rare, fait jeter cette lecture au profit d'un parcours complet).
    let (outside, scans): (Vec<SkippedPath>, Vec<DirScan>) = rayon::join(
        || walk::outside_rules(root),
        || {
            dirs.par_iter()
                .map(|&d| {
                    let mut s = DirScan::default();
                    let files = by_dir.get(d).map(|v| v.as_slice()).unwrap_or(&[]);
                    let Some(list) = walk::list_dir(root, d) else {
                        s.lost = true;
                        s.gone.extend(files.iter().map(|f| f.0.to_string()));
                        return s;
                    };
                    let mut seen: FxHashMap<&str, &walk::Entry> = FxHashMap::default();
                    for e in &list {
                        seen.insert(e.name.as_str(), e);
                    }
                    let mut mine: FxHashSet<&str> = FxHashSet::default();
                    for &(p, mt, sz) in files {
                        let name = p.rsplit('/').next().unwrap_or(p);
                        mine.insert(name);
                        match seen.get(name) {
                            Some(e) if e.kind == Kind::File => {
                                if (e.mtime, e.size) != (mt, sz) {
                                    s.stale.push(p.to_string());
                                }
                            }
                            _ => s.gone.push(p.to_string()),
                        }
                    }
                    for e in &list {
                        if mine.contains(e.name.as_str()) || index::EXTRA_IGNORE_DIRS.contains(&e.name.as_str()) {
                            continue;
                        }
                        match e.kind {
                            Kind::Dir => {
                                let rel = walk::join(d, &e.name);
                                if visited_set.contains(rel.as_str()) {
                                    continue;
                                }
                                if ignored_set.contains(rel.as_str()) {
                                    s.ignored.push(rel);
                                } else {
                                    s.candidates.push((rel, true));
                                }
                            }
                            Kind::File => {
                                if walk::RULE_FILES.contains(&e.name.as_str()) {
                                    s.rules.push(SkippedPath { path: walk::join(d, &e.name), mtime: e.mtime, size: e.size });
                                }
                                if !walk::indexable(&e.name) {
                                    continue;
                                }
                                let rel = walk::join(d, &e.name);
                                if ignored_set.contains(rel.as_str()) {
                                    s.ignored.push(rel);
                                } else {
                                    s.candidates.push((rel, false));
                                }
                            }
                            Kind::Other => {}
                        }
                    }
                    s
                })
                .collect()
        },
    );
    if outside != state.outside {
        return detect_full(h, root);
    }
    if dbg {
        eprintln!("[timing:fraîcheur] lecture des dossiers: {:.2}ms", t0.elapsed().as_secs_f64() * 1000.0);
    }
    let mut det = Detection {
        stale: Vec::new(),
        gone: Vec::new(),
        checked,
        visited: visited.clone(),
        ignored: Vec::new(),
        rules: Vec::new(),
        outside,
        derived,
        walked: false,
    };
    let mut candidates: Vec<(String, bool)> = Vec::new();
    for (d, s) in dirs.iter().zip(scans) {
        if s.lost {
            det.visited.remove(*d);
        }
        det.stale.extend(s.stale);
        det.gone.extend(s.gone);
        det.rules.extend(s.rules);
        det.ignored.extend(s.ignored);
        candidates.extend(s.candidates);
    }
    det.rules.sort_by(|a, b| a.path.cmp(&b.path));
    if det.rules != state.rules {
        return detect_full(h, root);
    }
    // Entrées nouvelles : jugées par la règle du parcours, dossier par dossier.
    let mut by_parent: HashMap<String, Vec<(String, bool)>> = HashMap::new();
    for c in candidates {
        by_parent.entry(walk::parent(&c.0).to_string()).or_default().push(c);
    }
    for (parent, list) in by_parent {
        let kids = walk::children(root, &parent);
        for (rel, is_dir) in list {
            let name = rel.rsplit('/').next().unwrap_or(&rel);
            if !kids.contains(name) {
                det.ignored.push(rel);
            } else if !is_dir {
                det.stale.push(rel);
            } else {
                let sub = walk::walk(root, &rel);
                let vis: HashSet<&str> = sub.dirs.iter().map(|s| s.as_str()).collect();
                let kept: HashSet<&str> = sub.files.iter().map(|f| f.rel.as_str()).collect();
                let (r, i) = walk::rules_and_ignored(root, &sub.dirs, &vis, &kept);
                det.rules.extend(r);
                det.ignored.extend(i);
                det.stale.extend(sub.files.iter().map(|f| f.rel.clone()));
                det.visited.extend(sub.dirs.iter().cloned());
            }
        }
    }
    finish(&mut det);
    if dbg {
        eprintln!("[timing:fraîcheur] total: {:.2}ms", t0.elapsed().as_secs_f64() * 1000.0);
    }
    det
}

fn finish(det: &mut Detection) {
    det.stale.sort();
    det.stale.dedup();
    det.gone.sort();
    det.gone.dedup();
    det.ignored.sort();
    det.ignored.dedup();
    det.rules.sort_by(|a, b| a.path.cmp(&b.path));
    det.rules.dedup_by(|a, b| a.path == b.path);
}

/// Parcours COMPLET (règles changées, ou pas encore d'état) : exactement ce
/// que verrait `cortex update`, comparé par (mtime, taille).
fn detect_full(h: &Handle, root: &Path) -> Detection {
    let w = walk::walk(root, "");
    let tracked = tracked_meta(h);
    let checked = h.live_files().count();
    let meta: HashMap<&str, (u64, u64)> = tracked.iter().map(|&(p, mt, sz)| (p, (mt, sz))).collect();
    let kept: HashSet<&str> = w.files.iter().map(|f| f.rel.as_str()).collect();
    let mut det = Detection {
        stale: w.files.iter().filter(|f| meta.get(f.rel.as_str()) != Some(&(f.mtime, f.size))).map(|f| f.rel.clone()).collect(),
        gone: tracked.iter().filter(|t| !kept.contains(t.0)).map(|t| t.0.to_string()).collect(),
        checked,
        visited: w.dirs.iter().cloned().collect(),
        ignored: Vec::new(),
        rules: Vec::new(),
        outside: walk::outside_rules(root),
        derived: walk::ancestors(tracked.iter().map(|t| t.0)),
        walked: true,
    };
    let vis: HashSet<&str> = w.dirs.iter().map(|s| s.as_str()).collect();
    let (rules, ignored) = walk::rules_and_ignored(root, &w.dirs, &vis, &kept);
    det.rules = rules;
    det.ignored = ignored;
    finish(&mut det);
    det
}

/// Nouvel état du parcours, une fois connus les fichiers suivis APRÈS la mise
/// à jour (`None` : aucun changement de fichier, les ancêtres sont inchangés).
fn next_state(det: &Detection, h: &Handle, after: Option<(&[Change], &[SkippedPath])>) -> WalkState {
    let derived_after;
    let derived = match after {
        None => &det.derived,
        Some((changes, skipped)) => {
            let removed: HashSet<&str> =
                changes.iter().filter_map(|c| if let Change::Remove(p) = c { Some(p.as_str()) } else { None }).collect();
            let mut paths: Vec<&str> = h.live_files().map(|r| r.name()).filter(|p| !removed.contains(p)).collect();
            paths.extend(changes.iter().filter_map(|c| if let Change::Upsert(e) = c { Some(e.path.as_str()) } else { None }));
            paths.extend(skipped.iter().map(|s| s.path.as_str()));
            derived_after = walk::ancestors(paths.into_iter());
            &derived_after
        }
    };
    WalkState {
        extra_dirs: det.visited.iter().filter(|d| !derived.contains(*d)).cloned().collect(),
        ignored: det.ignored.clone(),
        rules: det.rules.clone(),
        outside: det.outside.clone(),
    }
}

/// Relit les chemins donnés et en fait des changements. Un chemin absent,
/// inindexable ou binaire devient une suppression (si connu) ; un binaire est
/// ajouté à la liste des ignorés. Le contenu d'un fichier au hash inchangé n'est
/// pas ré-extrait (simple mise à jour de mtime/taille). `gone` : chemins que le
/// parcours ne rend plus (retirés de l'atlas et de `skipped`, même s'ils existent).
pub fn read_changes(h: &Handle, root: &Path, rels: &[String], gone: &[String]) -> (Vec<Change>, Vec<SkippedPath>) {
    let known_hash: HashMap<&str, &str> =
        rels.iter().filter_map(|p| h.file_by_path(p).and_then(|g| h.node(g)).map(|r| (r.name(), r.str(r.n.hash)))).collect();
    let results: Vec<(Option<Change>, Option<SkippedPath>)> = rels
        .par_iter()
        .map(|rel| {
            let abs = root.join(rel);
            let indexable = !index::in_ignored_dir(rel) && Lang::from_path(rel).is_indexable() && abs.is_file();
            if !indexable {
                return (Some(Change::Remove(rel.clone())), None);
            }
            let Some((bytes, mtime, size)) = index::read_file(&abs) else { return (Some(Change::Remove(rel.clone())), None) };
            let hash = index::hash_bytes(&bytes);
            if let Some(&kh) = known_hash.get(rel.as_str()) {
                if kh == hash {
                    // Contenu identique : pas d'extraction, `apply` n'en fera
                    // qu'une mise à jour de métadonnées.
                    let r = h.node(h.file_by_path(rel).unwrap()).unwrap();
                    let mut e = h.materialize_file_meta(&r);
                    e.mtime = mtime;
                    e.size = size;
                    return (Some(Change::Upsert(e)), None);
                }
            }
            match index::process_bytes(rel.clone(), &bytes, mtime, Some(hash)) {
                Some(e) => (Some(Change::Upsert(e)), None),
                None => (Some(Change::Remove(rel.clone())), Some(SkippedPath { path: rel.clone(), mtime, size })),
            }
        })
        .collect();
    let touched: HashSet<&str> = rels.iter().chain(gone).map(|s| s.as_str()).collect();
    let mut changes = Vec::new();
    let mut skipped: Vec<SkippedPath> = h.manifest.skipped.iter().filter(|s| !touched.contains(s.path.as_str())).cloned().collect();
    for (c, s) in results {
        changes.extend(c);
        skipped.extend(s);
    }
    changes.extend(gone.iter().filter(|p| h.file_by_path(p).is_some()).map(|p| Change::Remove(p.clone())));
    skipped.sort_by(|a, b| a.path.cmp(&b.path));
    (changes, skipped)
}

/// Contrôle + mise à jour ; rend le résultat de l'écriture (`Unchanged` si rien).
fn refresh_outcome(h: &mut Handle) -> std::io::Result<(Outcome, RefreshStats)> {
    let root = PathBuf::from(h.root());
    let t0 = Instant::now();
    let det = detect(h, &root);
    let check_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let mut stats = RefreshStats { checked: det.checked, walked: det.walked, check_ms, ..Default::default() };
    let project = h.project.clone();
    let t1 = Instant::now();
    let out = if det.stale.is_empty() && det.gone.is_empty() {
        let state = next_state(&det, h, None);
        if h.manifest.walk.as_ref() == Some(&state) {
            return Ok((Outcome::Unchanged, stats));
        }
        // Seul l'état du parcours change (entrée exclue apparue ou disparue).
        incremental::apply(&project, Vec::new(), Some(Tracked { skipped: h.manifest.skipped.clone(), walk: Some(state) }))?
    } else {
        let (changes, skipped) = read_changes(h, &root, &det.stale, &det.gone);
        let state = next_state(&det, h, Some((&changes, &skipped)));
        stats.examined = det.stale.len() + det.gone.len();
        incremental::apply(&project, changes, Some(Tracked { skipped, walk: Some(state) }))?
    };
    stats.refreshed = out != Outcome::Unchanged;
    if let Ok(nh) = Handle::open(&project) {
        *h = nh;
    }
    stats.update_ms = t1.elapsed().as_secs_f64() * 1000.0;
    Ok((out, stats))
}

/// Contrôle de fraîcheur + mise à jour si besoin ; `h` est rouvert après écriture.
pub fn refresh(h: &mut Handle) -> RefreshStats {
    // Source inaccessible (disque débranché, dossier déplacé) : ne surtout pas
    // conclure que tous les fichiers ont disparu.
    if !Path::new(h.root()).is_dir() {
        return RefreshStats::default();
    }
    match refresh_outcome(h) {
        Ok((_, s)) => s,
        Err(e) => {
            eprintln!("[cortex] atlas '{}': mise à jour en échec ({})", h.project, e);
            RefreshStats::default()
        }
    }
}

/// `cortex update` : parcours COMPLET et comparaison par taille + empreinte
/// blake3 de chaque fichier (pas seulement mtime/taille), puis mise à jour de
/// l'atlas (delta, ou reconstruction si le changement est massif).
pub fn update_full(project: &str, root: &Path) -> std::io::Result<(Outcome, usize, usize)> {
    let h = super::ensure_and_open(project)?;
    let w = walk::walk(root, "");
    let known: HashMap<&str, (u64, u64, &str)> = h
        .live_files()
        .map(|r| {
            let (mt, sz) = h.meta(&r);
            (r.name(), (mt, sz, r.str(r.n.hash)))
        })
        .collect();
    let skipped: HashMap<&str, (u64, u64)> = h.manifest.skipped.iter().map(|s| (s.path.as_str(), (s.mtime, s.size))).collect();
    let kept: HashSet<&str> = w.files.iter().map(|f| f.rel.as_str()).collect();
    let mut rels: Vec<String> = w
        .files
        .par_iter()
        .filter_map(|f| match known.get(f.rel.as_str()) {
            Some(&(mt, size, hash)) => {
                let (bytes, mtime, sz) = index::read_file(&root.join(&f.rel))?;
                // Contenu identique : relu seulement si le mtime a bougé (sinon
                // le contrôle de fraîcheur le relirait à chaque appel).
                if sz == size && index::hash_bytes(&bytes) == hash && mtime == mt {
                    None
                } else {
                    Some(f.rel.clone())
                }
            }
            // Déjà vu inindexable (binaire) et inchangé : pas de relecture.
            None if skipped.get(f.rel.as_str()) == Some(&(f.mtime, f.size)) => None,
            None => Some(f.rel.clone()),
        })
        .collect();
    rels.sort();
    let mut gone: Vec<String> = known.keys().chain(skipped.keys()).filter(|p| !kept.contains(**p)).map(|p| p.to_string()).collect();
    gone.sort();
    gone.dedup();
    let n = rels.len() + gone.iter().filter(|p| known.contains_key(p.as_str())).count();
    let (changes, new_skipped) = read_changes(&h, root, &rels, &gone);
    let det = Detection {
        stale: Vec::new(),
        gone: Vec::new(),
        checked: 0,
        visited: w.dirs.iter().cloned().collect(),
        ignored: Vec::new(),
        rules: Vec::new(),
        outside: walk::outside_rules(root),
        derived: BTreeSet::new(),
        walked: true,
    };
    let mut state = next_state(&det, &h, Some((&changes, &new_skipped)));
    let vis: HashSet<&str> = w.dirs.iter().map(|s| s.as_str()).collect();
    (state.rules, state.ignored) = walk::rules_and_ignored(root, &w.dirs, &vis, &kept);
    drop(known);
    let total = h.live_files().count();
    drop(h);
    let out = incremental::apply(project, changes, Some(Tracked { skipped: new_skipped, walk: Some(state) }))?;
    Ok((out, n, total))
}

/// `cortex update --changed` : le contrôle de fraîcheur, explicite (lecture des
/// dossiers, sans git ni relecture des fichiers inchangés). Même règle
/// d'exclusion que `cortex update` : les deux donnent le même atlas.
pub fn update_changed(project: &str) -> std::io::Result<(Outcome, usize)> {
    let mut h = super::ensure_and_open(project)?;
    if !Path::new(h.root()).is_dir() {
        return Err(std::io::Error::new(std::io::ErrorKind::NotFound, format!("source absente: {}", h.root())));
    }
    let (out, s) = refresh_outcome(&mut h)?;
    Ok((out, s.examined))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atlas::tests::clean;

    fn project(tag: &str) -> (String, PathBuf) {
        let name = format!("cortex-test-fresh-{}", tag);
        clean(&name);
        let dir = std::env::temp_dir().join(format!("cortex-src-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (name, dir)
    }

    fn index_it(name: &str, dir: &Path) -> Handle {
        let (idx, tracked) = index::build_index(name, dir).unwrap();
        crate::atlas::rebuild_full(name, &idx, tracked).unwrap();
        Handle::open(name).unwrap()
    }

    /// Rien n'a changé sur disque → rien de relu, rien d'écrit.
    #[test]
    fn refresh_sans_changement() {
        let (name, dir) = project("idle");
        std::fs::write(dir.join("a.ts"), "export function a() {}\n").unwrap();
        let mut h = index_it(&name, &dir);
        let s = refresh(&mut h);
        assert!(!s.refreshed);
        assert_eq!(s.examined, 0);
        std::fs::remove_dir_all(&dir).ok();
        clean(&name);
    }

    /// Fichier modifié, puis nouveau fichier : vus et indexés par la voie delta ;
    /// un SECOND contrôle ne relit plus rien (bug « N chemins réindexés à chaque appel »).
    #[test]
    fn refresh_detecte_puis_se_stabilise() {
        let (name, dir) = project("mod");
        std::fs::write(dir.join("a.ts"), "export function a() {}\n").unwrap();
        let mut h = index_it(&name, &dir);
        std::fs::write(dir.join("a.ts"), "export function a() {}\nexport function nouvelleFonction() {}\n").unwrap();
        std::fs::write(dir.join("bin.ts"), [0u8, 1, 2, 3]).unwrap();
        let s = refresh(&mut h);
        assert!(s.refreshed);
        assert!(h.search("nouvelle fonction", 5).iter().any(|x| x.name == "nouvelleFonction"));
        assert!(h.segment_count() >= 2, "mise à jour par delta");
        let s2 = refresh(&mut h);
        assert_eq!(s2.examined, 0, "second contrôle : rien à relire");
        assert!(!s2.refreshed);
        // Suppression.
        std::fs::remove_file(dir.join("a.ts")).unwrap();
        let s3 = refresh(&mut h);
        assert!(s3.refreshed);
        assert!(h.search("nouvelle fonction", 5).is_empty());
        std::fs::remove_dir_all(&dir).ok();
        clean(&name);
    }

    fn write(dir: &Path, rel: &str, content: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    fn git(dir: &Path, args: &[&str]) -> bool {
        std::process::Command::new("git").arg("-C").arg(dir).args(args).output().map(|o| o.status.success()).unwrap_or(false)
    }

    /// (chemin, empreinte) de chaque fichier vivant, triés.
    fn live(h: &Handle) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = h.live_files().map(|r| (r.name().to_string(), r.str(r.n.hash).to_string())).collect();
        v.sort();
        v
    }

    /// Nouveau dossier (même profond) vu sans git ; dossier exclu par `.ignore`
    /// jamais indexé et mémorisé (pas réexaminé) ; dossier vidé de ses fichiers
    /// toujours surveillé.
    #[test]
    fn nouveaux_dossiers_et_exclusions() {
        let (name, dir) = project("dossiers");
        write(&dir, "a.ts", "export function a() {}\n");
        write(&dir, ".ignore", "tmp/\n");
        let mut h = index_it(&name, &dir);
        write(&dir, "neuf/profond/x.ts", "export function fonctionNeuve() {}\n");
        write(&dir, "tmp/y.ts", "export function fonctionExclue() {}\n");
        let s = refresh(&mut h);
        assert!(s.refreshed && !s.walked);
        let paths: Vec<String> = live(&h).into_iter().map(|x| x.0).collect();
        assert!(paths.contains(&"neuf/profond/x.ts".to_string()), "{:?}", paths);
        assert!(!paths.iter().any(|p| p.starts_with("tmp/")), "{:?}", paths);
        assert!(h.manifest.walk.as_ref().unwrap().ignored.contains(&"tmp".to_string()));
        let s2 = refresh(&mut h);
        assert_eq!(s2.examined, 0);
        assert!(!s2.refreshed);
        // Dossier vidé : reste surveillé (dossier visité sans fichier suivi).
        std::fs::remove_file(dir.join("neuf/profond/x.ts")).unwrap();
        assert!(refresh(&mut h).refreshed);
        assert!(h.manifest.walk.as_ref().unwrap().extra_dirs.contains(&"neuf/profond".to_string()));
        write(&dir, "neuf/profond/w.ts", "export function reapparue() {}\n");
        assert!(refresh(&mut h).refreshed);
        assert!(live(&h).iter().any(|x| x.0 == "neuf/profond/w.ts"));
        std::fs::remove_dir_all(&dir).ok();
        clean(&name);
    }

    /// Une règle d'exclusion modifiée impose un parcours complet : un fichier
    /// désormais exclu sort de l'atlas, puis y revient quand la règle disparaît.
    #[test]
    fn regle_modifiee_parcours_complet() {
        let (name, dir) = project("regle");
        write(&dir, "a.ts", "export function a() {}\n");
        write(&dir, "gen/x.ts", "export function genere() {}\n");
        let mut h = index_it(&name, &dir);
        write(&dir, ".ignore", "gen/\n");
        let s = refresh(&mut h);
        assert!(s.walked && s.refreshed);
        assert!(!live(&h).iter().any(|x| x.0 == "gen/x.ts"));
        assert_eq!(refresh(&mut h).examined, 0);
        std::fs::remove_file(dir.join(".ignore")).unwrap();
        let s = refresh(&mut h);
        assert!(s.walked && s.refreshed);
        assert!(live(&h).iter().any(|x| x.0 == "gen/x.ts"));
        std::fs::remove_dir_all(&dir).ok();
        clean(&name);
    }

    /// `update`, `update --changed` et une indexation neuve donnent le MÊME
    /// atlas, avec la même règle d'exclusion : fichier modifié, supprimé,
    /// ajouté dans un nouveau dossier, exclu par `.ignore`, sous un dossier
    /// exclu par `.gitignore`, et `secret.ts` quand `.gitignore` dit
    /// `Secret.ts` (git, insensible à la casse sous Windows, l'ignore ; le
    /// parcours le garde : c'était l'écart entre `--changed` et `update`).
    #[test]
    fn update_et_changed_d_accord() {
        let (a, dir) = project("accord");
        let b = "cortex-test-fresh-accord-b";
        let c = "cortex-test-fresh-accord-c";
        clean(b);
        clean(c);
        if git(&dir, &["init", "-q"]) {
            git(&dir, &["config", "core.ignorecase", "true"]);
        }
        write(&dir, ".gitignore", "Secret.ts\nsortie/\n");
        write(&dir, ".ignore", "cache.ts\n");
        write(&dir, "a.ts", "export function a() {}\n");
        write(&dir, "b.ts", "export function b() {}\n");
        write(&dir, "sub/c.ts", "export function c() {}\n");
        let _ = index_it(&a, &dir);
        let _ = index_it(b, &dir);
        write(&dir, "a.ts", "export function a() {}\nexport function ajoutee() {}\n");
        std::fs::remove_file(dir.join("b.ts")).unwrap();
        write(&dir, "secret.ts", "export function secrete() {}\n");
        write(&dir, "cache.ts", "export function cache() {}\n");
        write(&dir, "neuf/d.ts", "export function d() {}\n");
        write(&dir, "sortie/e.ts", "export function e() {}\n");
        write(&dir, "sub/f.ts", "export function f() {}\n");
        let (_, n) = update_changed(&a).unwrap();
        assert!(n >= 4, "chemins relus : {}", n);
        update_full(b, &dir).unwrap();
        let ha = Handle::open(&a).unwrap();
        let hb = Handle::open(b).unwrap();
        let hc = index_it(c, &dir);
        assert_eq!(live(&ha), live(&hb), "update --changed ≠ update");
        assert_eq!(live(&hb), live(&hc), "update ≠ indexation neuve");
        let paths: Vec<String> = live(&ha).into_iter().map(|x| x.0).collect();
        assert!(paths.contains(&"neuf/d.ts".to_string()) && paths.contains(&"sub/f.ts".to_string()));
        assert!(!paths.contains(&"b.ts".to_string()) && !paths.contains(&"cache.ts".to_string()));
        assert!(!paths.iter().any(|p| p.starts_with("sortie/")) || !git(&dir, &["status"]));
        assert_eq!(update_changed(&a).unwrap().1, 0, "second passage : rien à relire");
        drop((ha, hb, hc));
        std::fs::remove_dir_all(&dir).ok();
        clean(&a);
        clean(b);
        clean(c);
    }
}
