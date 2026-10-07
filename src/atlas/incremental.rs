//! Mise à jour INCRÉMENTALE par segment delta (architecture v2 §4.1).
//!
//! Une mise à jour (fichiers modifiés, ajoutés, supprimés, touchés) écrit UN
//! petit segment delta — jamais le tronc — et l'ajoute au manifeste. La lecture
//! fusionne tronc + deltas (`view.rs`). Au-delà d'un seuil, la compaction
//! refond la pile en un tronc neuf (`atlas::compact`).
//!
//! ## Identifiants
//!
//! Un fichier modifié garde son id ; ses symboles sont appariés par (nom, genre,
//! n-ième occurrence) : un symbole apparié garde son id (nouvelle VERSION dans
//! le delta), un symbole disparu est tombé (tombstone), un symbole nouveau reçoit
//! un id neuf. Un fichier ajouté reçoit des ids neufs ; un fichier supprimé est
//! tombé avec ses symboles. Un renommage de fichier = suppression + ajout.
//!
//! ## Résolution incrémentale (exacte, pas une heuristique)
//!
//! La résolution d'un fichier (`graph`) ne dépend que de (1) ses propres
//! symboles et références, (2) l'ensemble des CHEMINS du projet (imports), (3)
//! pour chaque nom appelé, l'ensemble de ses définitions (fichier, rang, genre).
//! Après un changement, un fichier INCHANGÉ ne peut donc voir sa résolution
//! bouger que si :
//! - il appelle un nom dont l'ensemble des définitions a changé — noms des
//!   symboles tombés ou créés (un symbole apparié garde nom, genre et id ; son
//!   rang n'intervient que pour son propre fichier) → index `call_names` ;
//! - un de ses imports a pour base un chemin apparu ou disparu (chemin exact,
//!   sans extension, ou dossier d'un `index.*`) → index `import_bases`.
//! Ces fichiers-là (et eux seuls) sont re-résolus avec EXACTEMENT l'algorithme
//! de la construction complète ; seules les versions dont le résultat change
//! (liens, compte d'ambigus, carte) sont écrites. Le reste du projet n'est ni
//! relu ni recopié. Le test-juge ci-dessous vérifie qu'une chaîne de mises à jour
//! (puis une compaction) répond exactement comme une reconstruction complète.

use super::build::{self, contribution, FileData, SegBuilder, SymData};
use super::cards::render_card;
use super::manifest::{self, Manifest, Tracked};
use super::schema::CorpusStats;
use super::view::NodeRef;
use super::Handle;
use crate::graph::{self, Cand, FileCalls, Universe};
use crate::index::{FileEntry, ProjectIndex};
use crate::symbol::SymbolKind;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

/// Un changement à appliquer à l'atlas.
pub enum Change {
    /// Fichier (re)lu sur disque : ajouté, modifié, ou identique (hash égal →
    /// simple mise à jour de mtime/taille).
    Upsert(FileEntry),
    /// Fichier disparu (ou devenu inindexable).
    Remove(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Rien à écrire.
    Unchanged,
    /// Un delta a été écrit.
    Delta { changed_files: usize, reresolved_files: usize, versions: usize },
    /// Delta écrit puis pile compactée en un tronc neuf.
    Compacted,
    /// Delta écrit puis deltas fusionnés en un seul (tronc inchangé).
    Merged,
    /// Changement trop massif : reconstruction complète directe.
    Rebuilt,
}

/// Au-delà de cette part de fichiers changés (%), un delta n'a plus d'intérêt :
/// reconstruction complète depuis l'état fusionné.
const MASSIVE_CHANGE_PERCENT: usize = 10;

struct Plan {
    modified: Vec<(u32, FileEntry)>,
    added: Vec<FileEntry>,
    removed: Vec<u32>,
    touched: Vec<(u32, u64, u64)>,
}

impl Plan {
    fn structural(&self) -> usize {
        self.modified.len() + self.added.len() + self.removed.len()
    }
}

fn classify(h: &Handle, changes: Vec<Change>) -> Plan {
    let mut by_path: HashMap<String, Change> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for c in changes {
        let p = match &c {
            Change::Upsert(e) => e.path.clone(),
            Change::Remove(p) => p.clone(),
        };
        if by_path.insert(p.clone(), c).is_none() {
            order.push(p);
        }
    }
    order.sort();
    let mut plan = Plan { modified: Vec::new(), added: Vec::new(), removed: Vec::new(), touched: Vec::new() };
    for p in order {
        match by_path.remove(&p).unwrap() {
            Change::Upsert(e) => match h.file_by_path(&e.path).and_then(|g| h.node(g)) {
                Some(r) => {
                    if r.str(r.n.hash) == e.hash {
                        if h.meta(&r) != (e.mtime, e.size) {
                            plan.touched.push((r.g, e.mtime, e.size));
                        }
                    } else {
                        plan.modified.push((r.g, e));
                    }
                }
                None => plan.added.push(e),
            },
            Change::Remove(p) => {
                if let Some(g) = h.file_by_path(&p) {
                    plan.removed.push(g);
                }
            }
        }
    }
    plan
}

/// Applique des changements : delta (cas normal), reconstruction directe si le
/// changement est massif, compaction si la pile dépasse le seuil. `tracked` :
/// nouvelle liste des chemins inindexables et nouvel état de parcours à
/// mémoriser (voir `manifest` ; `walk: None` garde l'état courant).
pub fn apply(project: &str, changes: Vec<Change>, tracked: Option<Tracked>) -> std::io::Result<Outcome> {
    let dbg = std::env::var("CORTEX_DEBUG_TIMING").is_ok();
    let t0 = std::time::Instant::now();
    macro_rules! lap {
        ($l:expr) => {
            if dbg {
                eprintln!("[timing:apply] {}: {:.2}ms", $l, t0.elapsed().as_secs_f64() * 1000.0);
            }
        };
    }
    // Un seul écrivain : la pile lue ici est celle sur laquelle le delta est publié.
    let _lock = manifest::WriteLock::acquire(project)?;
    let h = Handle::open(project)?;
    lap!("verrou + ouverture");
    let plan = classify(&h, changes);
    lap!("ouverture + classement");
    let tracked = match tracked {
        Some(t) => Tracked { skipped: t.skipped, walk: t.walk.or_else(|| h.manifest.walk.clone()) },
        None => Tracked { skipped: h.manifest.skipped.clone(), walk: h.manifest.walk.clone() },
    };
    let tracked_changed = tracked.skipped != h.manifest.skipped || tracked.walk != h.manifest.walk;
    if plan.structural() == 0 && plan.touched.is_empty() {
        if tracked_changed {
            let mut m = Manifest::load(project)?;
            m.skipped = tracked.skipped;
            m.walk = tracked.walk;
            m.save(project)?;
        }
        return Ok(Outcome::Unchanged);
    }

    if plan.structural() > 50 && plan.structural() * 100 > h.live_files().count().max(1) * MASSIVE_CHANGE_PERCENT {
        let mut idx = h.materialize();
        apply_plan_to_index(&h, &mut idx, plan);
        drop(h);
        super::rebuild_full_locked(project, &idx, tracked)?;
        return Ok(Outcome::Rebuilt);
    }

    let (seg, outcome) = build_delta(&h, plan);
    drop(h);
    lap!("delta construit");
    let name = manifest::new_segment_name();
    let path = manifest::atlas_dir(project).join(&name);
    let bytes = super::segment::write_segment(&path, &seg)?;
    lap!(format!("delta écrit ({} octets)", bytes.len()));
    let mut m = Manifest::load(project)?;
    super::validate_bytes_and_record(project, &mut m, &name, &bytes)?;
    m.segments.push(name);
    m.skipped = tracked.skipped;
    m.walk = tracked.walk;
    m.save(project)?;
    lap!("manifeste publié");
    match super::compaction_needed(project, &m) {
        Some(super::Compaction::Full) => {
            super::compact_locked(project)?;
            Ok(Outcome::Compacted)
        }
        Some(super::Compaction::MergeDeltas) => {
            merge_deltas_locked(project)?;
            Ok(Outcome::Merged)
        }
        None => Ok(outcome),
    }
}

/// Compaction COURANTE : fusionne tous les deltas en UN seul, sans toucher au
/// tronc. Le changement net depuis le tronc (fichiers dont la version courante
/// vit dans un delta ou dont les métadonnées ont bougé, fichiers du tronc
/// disparus) est ré-appliqué au tronc SEUL par la voie delta ordinaire
/// (`classify` + `build_delta`) : la résolution incrémentale étant exacte, le
/// résultat répond comme la pile qu'il remplace — et comme une reconstruction
/// complète (test `incremental_chain_then_compaction_matches_full_rebuild`). Coût : celui d'une mise
/// à jour des seuls fichiers changés depuis le tronc, au lieu d'une réécriture
/// du tronc entier.
/// `merge_deltas_locked` sous verrou d'écriture.
pub fn merge_deltas(project: &str) -> std::io::Result<()> {
    let _lock = manifest::WriteLock::acquire(project)?;
    merge_deltas_locked(project)
}

pub(crate) fn merge_deltas_locked(project: &str) -> std::io::Result<()> {
    let full = Handle::open(project)?;
    if full.segment_count() <= 2 {
        return Ok(());
    }
    let trunk = Handle::open_trunk(project)?;
    let mut changes: Vec<Change> = Vec::new();
    for r in full.live_files() {
        let (s, _) = full.v().loc(r.g).expect("fichier vivant");
        let cur_meta = full.meta(&r);
        match trunk.node(r.g) {
            // Version courante dans le tronc : seules ses métadonnées peuvent avoir bougé.
            Some(t) if s == 0 => {
                if trunk.meta(&t) != cur_meta {
                    let mut e = full.materialize_file_meta(&r);
                    (e.mtime, e.size) = cur_meta;
                    changes.push(Change::Upsert(e));
                }
            }
            _ => {
                let mut e = full.materialize_file(&r);
                (e.mtime, e.size) = cur_meta;
                changes.push(Change::Upsert(e));
            }
        }
    }
    for t in trunk.live_files() {
        // Chemin disparu (un chemin re-créé dans un delta est déjà un `Upsert`).
        if full.file_by_path(t.name()).is_none() {
            changes.push(Change::Remove(t.name().to_string()));
        }
    }
    drop(full);
    let plan = classify(&trunk, changes);
    let (seg, _) = build_delta(&trunk, plan);
    let name = manifest::new_segment_name();
    let path = manifest::atlas_dir(project).join(&name);
    let bytes = super::segment::write_segment(&path, &seg)?;
    drop(seg);
    let mut m = Manifest::load(project)?;
    m.segments.truncate(1);
    m.validated.retain(|v| v.name == m.segments[0]);
    super::validate_bytes_and_record(project, &mut m, &name, &bytes)?;
    m.segments.push(name);
    m.save(project)?;
    Manifest::clean_orphans(project, &m.segments);
    Ok(())
}

/// Même changement, appliqué à un `ProjectIndex` matérialisé (voie massive).
fn apply_plan_to_index(h: &Handle, idx: &mut ProjectIndex, plan: Plan) {
    let mut gone: HashSet<String> = plan.removed.iter().map(|&g| h.path_of(g).to_string()).collect();
    for (g, _) in &plan.modified {
        gone.insert(h.path_of(*g).to_string());
    }
    let touched: HashMap<String, (u64, u64)> = plan.touched.iter().map(|&(g, mt, sz)| (h.path_of(g).to_string(), (mt, sz))).collect();
    idx.files.retain(|f| !gone.contains(&f.path));
    for f in idx.files.iter_mut() {
        if let Some(&(mt, sz)) = touched.get(&f.path) {
            f.mtime = mt;
            f.size = sz;
        }
    }
    idx.files.extend(plan.modified.into_iter().map(|(_, e)| e));
    idx.files.extend(plan.added);
    idx.files.sort_by(|a, b| a.path.cmp(&b.path));
}

/// Un fichier changé (modifié ou ajouté) en attente d'écriture.
struct PFile {
    gid: u32,
    is_new: bool,
    e: FileEntry,
    sym_gids: Vec<u32>,
    sym_new: Vec<bool>,
}

/// Univers de résolution « atlas courant + changements en attente ».
struct IncUniverse<'h> {
    h: &'h Handle,
    removed: &'h HashSet<u32>,
    added_path: HashMap<String, u32>,
    added_stem: HashMap<String, Vec<u32>>,
    defs: HashMap<String, Vec<Cand<u32, u32>>>,
}

impl<'h> Universe for IncUniverse<'h> {
    type F = u32;
    type S = u32;
    fn file_by_path(&self, path: &str) -> Option<u32> {
        self.added_path.get(path).copied().or_else(|| self.h.file_by_path(path).filter(|g| !self.removed.contains(g)))
    }
    fn files_by_stem(&self, stem: &str) -> Vec<u32> {
        let mut v: Vec<u32> = self.h.files_by_stem(stem).into_iter().filter(|g| !self.removed.contains(g)).collect();
        if let Some(a) = self.added_stem.get(stem) {
            v.extend(a.iter().copied());
        }
        v
    }
    fn defs(&self, name_lower: &str) -> &[Cand<u32, u32>] {
        self.defs.get(name_lower).map(|v| v.as_slice()).unwrap_or(&[])
    }
}

/// Résultat de résolution d'un symbole d'un fichier changé.
struct SymOut {
    calls: Vec<u32>,
    amb: u32,
    card: String,
}

fn sym_data<'a>(pf: &'a PFile, i: usize, o: &SymOut) -> SymData<'a> {
    let s = &pf.e.symbols[i];
    SymData {
        name: &s.name,
        kind: s.kind.as_u8(),
        ord: i as u32,
        owner_file: pf.gid,
        line: s.line,
        end_line: s.end_line,
        signature: &s.signature,
        tokens: s.tokens.iter().map(|x| x.as_str()).collect(),
        doc: s.doc.iter().map(|x| x.as_str()).collect(),
        summary: &s.summary,
        ambiguous: o.amb,
        card: o.card.clone(),
    }
}

/// Symbole en attente : (fichier, rang, genre, nom, chemin du fichier, rang
/// d'homonymie dans le fichier).
struct PSym<'a> {
    file: u32,
    ord: u32,
    kind: SymbolKind,
    name: &'a str,
    path: &'a str,
    homonym: u32,
}

fn stats_of(r: &NodeRef) -> CorpusStats {
    contribution(r.n.kind, r.n.tokens.len(), r.n.doc.len(), r.n.body_len)
}

fn sorted(v: &[u32]) -> Vec<u32> {
    let mut v = v.to_vec();
    v.sort_unstable();
    v
}

/// Construit le segment delta d'un plan (voir l'en-tête du module).
fn build_delta(h: &Handle, plan: Plan) -> (super::schema::AtlasSegment, Outcome) {
    let dbg = std::env::var("CORTEX_DEBUG_TIMING").is_ok();
    let t0 = std::time::Instant::now();
    macro_rules! lap {
        ($l:expr) => {
            if dbg {
                eprintln!("[timing:delta] {}: {:.2}ms", $l, t0.elapsed().as_secs_f64() * 1000.0);
            }
        };
    }

    let id_base = h.v().next_id;
    let mut next = id_base;
    let mut tomb: Vec<u32> = Vec::new();
    let mut pfiles: Vec<PFile> = Vec::new();

    // ── Identifiants : appariement (nom, genre, occurrence) ────────────────
    for (fg, e) in plan.modified {
        let old = h.symbols_of(fg);
        let mut pool: HashMap<(&str, u8), VecDeque<u32>> = HashMap::new();
        for r in &old {
            pool.entry((r.name(), r.n.sym_kind)).or_default().push_back(r.g);
        }
        let mut sym_gids = Vec::with_capacity(e.symbols.len());
        let mut sym_new = Vec::with_capacity(e.symbols.len());
        for s in &e.symbols {
            match pool.get_mut(&(s.name.as_str(), s.kind.as_u8())).and_then(|q| q.pop_front()) {
                Some(g) => {
                    sym_gids.push(g);
                    sym_new.push(false);
                }
                None => {
                    sym_gids.push(u32::MAX);
                    sym_new.push(true);
                }
            }
        }
        let mut left: Vec<u32> = pool.into_values().flatten().collect();
        left.sort_unstable();
        tomb.extend(left);
        pfiles.push(PFile { gid: fg, is_new: false, e, sym_gids, sym_new });
    }
    for e in plan.added {
        let n = e.symbols.len();
        pfiles.push(PFile { gid: u32::MAX, is_new: true, e, sym_gids: vec![u32::MAX; n], sym_new: vec![true; n] });
    }
    pfiles.sort_by(|a, b| a.e.path.cmp(&b.e.path));
    // Ids neufs, dans l'ordre d'écriture (fichier puis ses symboles).
    for pf in pfiles.iter_mut() {
        if pf.is_new {
            pf.gid = next;
            next += 1;
        }
        for (i, g) in pf.sym_gids.iter_mut().enumerate() {
            if pf.sym_new[i] {
                *g = next;
                next += 1;
            }
        }
    }
    let removed: HashSet<u32> = plan.removed.iter().copied().collect();
    for &fg in &plan.removed {
        tomb.push(fg);
        tomb.extend(h.symbols_of(fg).iter().map(|r| r.g));
    }
    let dead: HashSet<u32> = tomb.iter().copied().collect();

    // ── État en attente ────────────────────────────────────────────────────
    let mut pend: HashMap<u32, PSym> = HashMap::new();
    let mut new_defs: HashMap<String, Vec<u32>> = HashMap::new();
    let mut added_path: HashMap<String, u32> = HashMap::new();
    let mut added_stem: HashMap<String, Vec<u32>> = HashMap::new();
    let mut changed_names: BTreeSet<String> = BTreeSet::new();
    for pf in &pfiles {
        if pf.is_new {
            added_path.insert(pf.e.path.clone(), pf.gid);
            added_stem.entry(graph::strip_known_ext(&pf.e.path).to_string()).or_default().push(pf.gid);
        }
        let homonyms = crate::ids::ranks(pf.e.symbols.iter().map(|s| (s.name.as_str(), s.kind)));
        for (i, s) in pf.e.symbols.iter().enumerate() {
            let g = pf.sym_gids[i];
            pend.insert(g, PSym { file: pf.gid, ord: i as u32, kind: s.kind, name: &s.name, path: &pf.e.path, homonym: homonyms[i] });
            if pf.sym_new[i] {
                let l = graph::call_key(&s.name);
                new_defs.entry(l.clone()).or_default().push(g);
                changed_names.insert(l);
            }
        }
    }
    for &g in &tomb {
        if let Some(r) = h.node(g) {
            if r.n.kind == 1 {
                changed_names.insert(graph::call_key(r.name()));
            }
        }
    }

    // ── Fichiers inchangés dont la résolution peut bouger ─────────────────
    let changed_files: HashSet<u32> = pfiles.iter().map(|p| p.gid).chain(removed.iter().copied()).collect();
    let mut affected: BTreeSet<u32> = BTreeSet::new();
    for n in &changed_names {
        affected.extend(h.files_calling(n).into_iter().filter(|f| !changed_files.contains(f)));
    }
    let moved_paths: Vec<String> = pfiles
        .iter()
        .filter(|p| p.is_new)
        .map(|p| p.e.path.clone())
        .chain(plan.removed.iter().map(|&g| h.path_of(g).to_string()))
        .collect();
    for p in &moved_paths {
        for base in graph::bases_satisfied_by(p) {
            affected.extend(h.files_importing_base(&base).into_iter().filter(|f| !changed_files.contains(f)));
        }
    }
    lap!(format!("plan ({} changés, {} à re-résoudre)", pfiles.len() + removed.len(), affected.len()));

    // Vue de résolution des fichiers inchangés concernés (lue dans l'atlas).
    struct AFile<'a> {
        r: NodeRef<'a>,
        syms: Vec<NodeRef<'a>>,
        fc: FileCalls<'a>,
        specs: Vec<&'a str>,
    }
    let afiles: Vec<AFile> = affected
        .iter()
        .filter_map(|&fg| {
            let r = h.node(fg)?;
            let refs = r.refs()?;
            let syms = h.symbols_of(fg);
            let fc = FileCalls::new(
                syms.iter().map(|s| s.name()),
                refs.imported_names.iter().map(|&i| r.str(i)),
                refs.calls.iter().map(|c| (r.str(c.name), c.line)).collect(),
            );
            let specs = refs.imports.iter().map(|&i| r.str(i)).collect();
            Some(AFile { r, syms, fc, specs })
        })
        .collect();
    let pcalls: Vec<FileCalls> = pfiles.iter().map(|pf| build::file_calls(&pf.e)).collect();

    // Définitions de chaque nom appelé par un fichier à résoudre.
    let mut needed: HashSet<String> = HashSet::new();
    for fc in pcalls.iter().chain(afiles.iter().map(|a| &a.fc)) {
        needed.extend(fc.called_names());
    }
    let cand_of = |g: u32| -> Option<Cand<u32, u32>> {
        if let Some(p) = pend.get(&g) {
            return Some(Cand { file: p.file, sym: g, ord: p.ord, kind: p.kind });
        }
        let r = h.node(g)?;
        Some(Cand { file: r.n.owner_file, sym: g, ord: r.n.ord, kind: r.kind() })
    };
    let mut defs: HashMap<String, Vec<Cand<u32, u32>>> = HashMap::with_capacity(needed.len());
    for n in needed {
        let mut v: Vec<Cand<u32, u32>> = h.defs(&n).into_iter().filter(|g| !dead.contains(g)).filter_map(cand_of).collect();
        if let Some(ng) = new_defs.get(&n) {
            v.extend(ng.iter().filter_map(|&g| cand_of(g)));
        }
        if !v.is_empty() {
            defs.insert(n, v);
        }
    }
    let u = IncUniverse { h, removed: &removed, added_path, added_stem, defs };
    // Identifiant stable d'un symbole (en attente ou déjà dans l'atlas).
    let id_of = |g: u32| -> String {
        if let Some(p) = pend.get(&g) {
            return crate::ids::sym_id(p.path, p.name, p.kind, p.homonym);
        }
        h.node_id(g)
    };
    lap!("univers de résolution");

    // ── Écriture : nœuds créés (ordre des ids), puis nouvelles versions ─────
    let mut b = SegBuilder::new(id_base);
    let mut versions = 0usize;
    let mut resolved: Vec<(Vec<u32>, Vec<SymOut>)> = Vec::with_capacity(pfiles.len());
    for (pi, pf) in pfiles.iter().enumerate() {
        let e = &pf.e;
        let imported = graph::resolve_imports(&u, pf.gid, &e.path, e.refs.imports.iter().map(|s| s.as_str()));
        let mut cache = crate::fx::FxHashMap::default();
        let outs = e
            .symbols
            .iter()
            .enumerate()
            .map(|(si, s)| {
                let (callees, amb) = graph::resolve_symbol_calls(&u, pf.gid, &imported, &pcalls[pi], s.line, s.end_line, &mut cache);
                let named: Vec<String> = callees.iter().map(|c| id_of(c.sym)).collect();
                let card = render_card(&id_of(pf.sym_gids[si]), s.kind, s.line, s.end_line, &s.signature, &s.summary, &named, amb);
                SymOut { calls: callees.iter().map(|c| c.sym).collect(), amb: amb as u32, card }
            })
            .collect();
        resolved.push((imported, outs));
    }
    // (1) nœuds CRÉÉS, dans l'ordre d'attribution des ids.
    for (pi, pf) in pfiles.iter().enumerate() {
        let (imported, outs) = &resolved[pi];
        if pf.is_new {
            b.add_file(pf.gid, true, &build::file_data(&pf.e), pf.sym_gids.clone(), imported.clone());
        }
        for (i, o) in outs.iter().enumerate() {
            if pf.sym_new[i] {
                b.add_symbol(pf.sym_gids[i], true, &sym_data(pf, i, o), o.calls.clone());
            }
        }
    }
    // (2) nouvelles versions des fichiers modifiés et de leurs symboles appariés.
    for (pi, pf) in pfiles.iter().enumerate() {
        if pf.is_new {
            continue;
        }
        let (imported, outs) = &resolved[pi];
        if let Some(r) = h.node(pf.gid) {
            b.sub_stats(stats_of(&r));
        }
        b.add_file(pf.gid, false, &build::file_data(&pf.e), pf.sym_gids.clone(), imported.clone());
        versions += 1;
        for (i, o) in outs.iter().enumerate() {
            if !pf.sym_new[i] {
                if let Some(r) = h.node(pf.sym_gids[i]) {
                    b.sub_stats(stats_of(&r));
                }
                b.add_symbol(pf.sym_gids[i], false, &sym_data(pf, i, o), o.calls.clone());
                versions += 1;
            }
        }
    }
    // (3) fichiers inchangés re-résolus : versions seulement si le résultat change.
    let mut reresolved = 0usize;
    for a in &afiles {
        let r = a.r;
        let imported = graph::resolve_imports(&u, r.g, r.name(), a.specs.iter().copied());
        let mut cache = crate::fx::FxHashMap::default();
        let mut touched_any = false;
        if sorted(&imported) != sorted(h.imports_raw(r.g)) {
            let refs = r.refs().expect("références d'un fichier");
            let (mtime, size) = h.meta(&r);
            let fd = FileData {
                path: r.name(),
                lang: r.n.lang,
                hash: r.str(r.n.hash),
                size,
                mtime,
                lines: r.n.lines,
                header: r.strs(&r.n.doc),
                summary: r.str(r.n.summary),
                imports: a.specs.clone(),
                imported_names: r.strs(&refs.imported_names),
                calls: a.fc.calls.clone(),
                db_refs: r.strs(&refs.db_refs),
                body: h.body_of(&r),
            };
            b.sub_stats(stats_of(&r));
            b.add_file(r.g, false, &fd, h.contains_raw(r.g).to_vec(), imported.clone());
            versions += 1;
            touched_any = true;
        }
        for sr in &a.syms {
            let (callees, amb) = graph::resolve_symbol_calls(&u, r.g, &imported, &a.fc, sr.n.line, sr.n.end_line, &mut cache);
            let calls: Vec<u32> = callees.iter().map(|c| c.sym).collect();
            // La carte est re-rendue et comparée elle aussi : un appelé renommé
            // ou déplacé change son identifiant sans changer l'arête.
            let named: Vec<String> = callees.iter().map(|c| id_of(c.sym)).collect();
            let card = render_card(
                &h.node_id(sr.g),
                sr.kind(),
                sr.n.line,
                sr.n.end_line,
                sr.str(sr.n.signature),
                sr.str(sr.n.summary),
                &named,
                amb,
            );
            if sorted(&calls) == sorted(h.calls_raw(sr.g)) && amb as u32 == sr.n.ambiguous_calls && card == sr.str(sr.n.card) {
                continue;
            }
            let doc = sr.strs(&sr.n.doc);
            let sd = SymData {
                name: sr.name(),
                kind: sr.n.sym_kind,
                ord: sr.n.ord,
                owner_file: r.g,
                line: sr.n.line,
                end_line: sr.n.end_line,
                signature: sr.str(sr.n.signature),
                tokens: sr.strs(&sr.n.tokens),
                doc,
                summary: sr.str(sr.n.summary),
                ambiguous: amb as u32,
                card,
            };
            b.sub_stats(stats_of(sr));
            b.add_symbol(sr.g, false, &sd, calls);
            versions += 1;
            touched_any = true;
        }
        if touched_any {
            reresolved += 1;
        }
    }
    // (4) morts et métadonnées.
    for &g in &tomb {
        if let Some(r) = h.node(g) {
            b.sub_stats(stats_of(&r));
        }
    }
    b.tombstones = tomb;
    b.meta_overrides = plan.touched;
    lap!(format!("résolution + segment ({} versions)", versions));
    let changed_files = pfiles.len() + removed.len();
    let seg = b.finish();
    (seg, Outcome::Delta { changed_files, reresolved_files: reresolved, versions })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atlas::manifest::Manifest;
    use crate::atlas::tests::clean;
    use crate::lang::Lang;
    use crate::outils::Appel;
    use crate::symbol::{CallRef, FileRefs, Symbol};

    fn sym(name: &str, kind: SymbolKind, line: u32, end_line: u32, doc: &[&str]) -> Symbol {
        Symbol {
            name: name.into(),
            kind,
            line,
            end_line,
            signature: format!("function {}()", name),
            tokens: crate::symbol::tokenize_identifier(name),
            doc: doc.iter().map(|s| s.to_string()).collect(),
            summary: doc.join(" "),
        }
    }

    /// Fichier : chemin, symboles, imports (spécificateur, noms), appels (nom, ligne), en-tête.
    fn file(path: &str, symbols: Vec<Symbol>, imports: &[(&str, &[&str])], calls: &[(&str, u32)], header: &[&str]) -> FileEntry {
        let mut refs = FileRefs::default();
        for (spec, names) in imports {
            refs.imports.push(spec.to_string());
            refs.imported_names.extend(names.iter().map(|s| s.to_string()));
        }
        refs.calls = calls.iter().map(|(n, l)| CallRef { name: n.to_string(), line: *l }).collect();
        refs.db_refs = vec!["table_x".into()];
        let hash = blake3::hash(format!("{:?}{:?}{:?}", symbols, refs, header).as_bytes()).to_hex().to_string();
        // Corps : texte synthétique (noms, docs, appels, en-tête, plus un mot
        // propre au fichier) passé par la vraie extraction — les postings,
        // statistiques et re-versions du champ corps sont ainsi exercés.
        let mut text = format!("corps {} ", path.replace('/', " "));
        for s in &symbols {
            text.push_str(&format!("{} {} ", s.name, s.doc.join(" ")));
        }
        for c in &refs.calls {
            text.push_str(&format!("{}() ", c.name));
        }
        text.push_str(&header.join(" "));
        let lang = Lang::from_path(path);
        let body = if lang == Lang::Markdown || symbols.is_empty() { Vec::new() } else { crate::symbol::body_terms(&text) };
        FileEntry {
            path: path.into(),
            lang,
            hash,
            size: 100,
            mtime: 1,
            lines: 50,
            symbols,
            refs,
            header: header.iter().map(|s| s.to_string()).collect(),
            summary: header.join(" "),
            body,
        }
    }

    /// Remplace le corps d'un fichier par celui d'un texte donné.
    fn with_body(mut e: FileEntry, text: &str, hash: &str) -> FileEntry {
        e.body = crate::symbol::body_terms(text);
        e.hash = hash.into();
        e
    }

    /// Corpus de test : homonymes, alias `@/`, `index.ts`, appels résolus et
    /// ambigus, en-têtes, docs, un test — de quoi exercer toute la résolution.
    fn corpus() -> Vec<FileEntry> {
        use SymbolKind::*;
        vec![
            file(
                "src/lib/store.ts",
                vec![sym("vider", Function, 1, 5, &["nettoie", "le", "cache"]), sym("remplir", Function, 6, 12, &["charge", "cache"])],
                &[],
                &[("vider", 8)],
                &["gestion", "du", "cache", "local"],
            ),
            file(
                "src/lib/other.ts",
                vec![sym("vider", Function, 1, 4, &["vide", "autre"]), sym("helper", Function, 5, 9, &[])],
                &[],
                &[],
                &["autre", "module"],
            ),
            file(
                "src/hooks/index.ts",
                vec![sym("useCache", Hook, 1, 10, &["hook", "cache"])],
                &[("../lib/store", &["vider", "remplir"])],
                &[("vider", 3), ("remplir", 4)],
                &[],
            ),
            file(
                "src/app/main.tsx",
                vec![sym("App", Component, 1, 30, &["racine"]), sym("demarrer", Function, 31, 40, &["lance", "application"])],
                &[("@/hooks", &["useCache"]), ("@/lib/other", &["helper"]), ("pkg-externe", &["globalFn"])],
                &[("useCache", 5), ("helper", 6), ("vider", 7), ("globalFn", 8), ("demarrer", 20), ("map", 21)],
                &["point", "entree", "application"],
            ),
            file(
                "src/app/cachePanel.tsx",
                vec![sym("CachePanel", Component, 1, 20, &["panneau", "cache"])],
                &[("@/lib/absent", &["nouveau"])],
                &[("vider", 5), ("useCache", 6), ("nouveau", 7)],
                &["panneau"],
            ),
            file("src/lib/store.test.ts", vec![sym("testVider", Function, 1, 9, &[])], &[("./store", &["vider"])], &[("vider", 3)], &[]),
            file("docs/guide.md", vec![sym("Guide cache", Heading, 1, 0, &[])], &[], &[], &["guide"]),
        ]
    }

    fn index_of(name: &str, mut files: Vec<FileEntry>) -> ProjectIndex {
        files.sort_by(|a, b| a.path.cmp(&b.path));
        ProjectIndex { name: name.into(), root: "/fixture-inexistante".into(), generated_at: 0, files }
    }

    const QUERIES: &[&str] = &[
        "vider",
        "cache",
        "vider cache",
        "use cache",
        "application",
        "panneau cache",
        "helper",
        "store",
        "test vider",
        "guide",
        "main",
        "remplir",
        "corps",
        "corps cache",
        "sentinelles",
        "chargement rapide",
        "src app",
    ];

    /// Signature complète des réponses d'un atlas : context/explain/carte de
    /// chaque nom, recherche (nom, fichier, ligne, score exact) de chaque
    /// requête, index matérialisé (références comprises).
    fn fingerprint(h: &Handle, idx: &ProjectIndex) -> Vec<String> {
        let mut out = Vec::new();
        let mut names: Vec<String> = idx.files.iter().flat_map(|f| f.symbols.iter().map(|s| s.name.clone())).collect();
        names.sort();
        names.dedup();
        names.push("zzz-inconnu".into());
        for n in &names {
            // Carte I7 stockée (au bit près) puis chaque outil pour agents.
            let stored = h.find_symbol(n).and_then(|g| h.node(g)).map(|r| r.str(r.n.card).to_string());
            out.push(format!("carte stockée {}: {:?}", n, stored));
            out.push(format!("context {}:\n{}", n, ctx(h, n)));
            out.push(format!("impact {}:\n{}", n, imp(h, n)));
            out.push(format!("outline {}:\n{}", n, outil(h, Appel::Outline { cible: n.clone() })));
            out.push(format!("path {} -> vider:\n{}", n, outil(h, Appel::Path { de: n.clone(), vers: "vider".into() })));
        }
        for f in &idx.files {
            out.push(format!("outline {}:\n{}", f.path, outil(h, Appel::Outline { cible: format!("F:{}", f.path) })));
        }
        out.push(format!("overview src:\n{}", outil(h, Appel::Overview { dossier: "src".into() })));
        for q in QUERIES {
            let hits: Vec<String> =
                h.search(q, 50).iter().map(|x| format!("{}|{}|{}|{:?}|{}", x.name, x.file, x.line, x.kind, x.score.to_bits())).collect();
            out.push(format!("search {}: {:?}", q, hits));
        }
        let m = h.materialize();
        out.push(serde_json::to_string(&m.files).unwrap());
        let mut paths = h.file_paths();
        paths.sort();
        out.push(format!("{:?}", paths));
        out.push(format!("{:?}", h.counts()));
        // Le nom du projet (différent pour la reconstruction témoin) n'entre pas en compte.
        out.into_iter().map(|x| x.replace(&h.project, "<projet>")).collect()
    }

    /// Sortie complète d'un outil pour agents sur un seul atlas.
    fn outil(h: &Handle, a: Appel) -> String {
        crate::outils::executer(std::slice::from_ref(h), &a, Some(100_000))
    }
    fn ctx(h: &Handle, n: &str) -> String {
        outil(h, Appel::Context { cible: n.into() })
    }
    fn imp(h: &Handle, n: &str) -> String {
        outil(h, Appel::Impact { cible: n.into(), profondeur: 3 })
    }

    /// Compare l'atlas `name` (tronc + deltas) à une reconstruction complète de `idx`.
    fn assert_equivalent(name: &str, idx: &ProjectIndex, step: &str) {
        let full = format!("{}-full", name);
        clean(&full);
        let mut fidx = idx.clone();
        fidx.name = full.clone();
        crate::atlas::rebuild_full(&full, &fidx, Default::default()).unwrap();
        let h_incr = Handle::open(name).unwrap();
        let h_full = Handle::open(&full).unwrap();
        let a = fingerprint(&h_incr, idx);
        let b = fingerprint(&h_full, idx);
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(x, y, "divergence tronc+deltas / reconstruction à l'étape « {} »", step);
        }
        clean(&full);
    }

    fn upsert(files: &mut Vec<FileEntry>, e: FileEntry) -> Change {
        files.retain(|f| f.path != e.path);
        files.push(e.clone());
        Change::Upsert(e)
    }

    fn remove(files: &mut Vec<FileEntry>, path: &str) -> Change {
        files.retain(|f| f.path != path);
        Change::Remove(path.into())
    }

    fn segs(name: &str) -> usize {
        Manifest::load(name).unwrap().segments.len()
    }

    fn start(name: &str) -> Vec<FileEntry> {
        clean(name);
        let files = corpus();
        crate::atlas::rebuild_full(name, &index_of(name, files.clone()), Default::default()).unwrap();
        files
    }

    /// Modification de CORPS (doc/signature) : voie delta, appelants intacts,
    /// nouveau contenu cherchable.
    #[test]
    fn body_edit_takes_delta_path() {
        let name = "cortex-test-incr-body";
        let mut files = start(name);
        let mut e = files.iter().find(|f| f.path == "src/lib/store.ts").unwrap().clone();
        e.symbols[0].doc = vec!["nettoie".into(), "force".into()];
        e.symbols[0].signature = "function vider(force: boolean)".into();
        e.hash = "h-body".into();
        let c = upsert(&mut files, e);
        let out = apply(name, vec![c], None).unwrap();
        assert!(matches!(out, Outcome::Delta { .. }), "{:?}", out);
        assert_eq!(segs(name), 2, "un delta s'AJOUTE au tronc");
        let h = Handle::open(name).unwrap();
        assert!(h.search("force", 5).iter().any(|x| x.name == "vider"));
        assert!(ctx(&h, "useCache").contains("src/lib/store.ts"));
        drop(h);
        assert_equivalent(name, &index_of(name, files.clone()), "corps modifié");
        // Un mot présent SEULEMENT dans le contenu du fichier (ni nom, ni doc,
        // ni en-tête) le fait remonter par le champ corps, après un delta.
        let e = files.iter().find(|f| f.path == "src/lib/other.ts").unwrap().clone();
        let c = upsert(&mut files, with_body(e, "const x = chiffrerSentinelle(zanzibar)", "h-contenu"));
        apply(name, vec![c], None).unwrap();
        let h = Handle::open(name).unwrap();
        let hits = h.search("zanzibar", 5);
        assert_eq!(hits.first().map(|x| x.file.as_str()), Some("src/lib/other.ts"), "le champ corps doit trouver le fichier");
        assert!(
            h.search("chiffrer sentinelles", 5).iter().any(|x| x.file == "src/lib/other.ts"),
            "corps raciné : pluriel et découpe camelCase"
        );
        drop(h);
        assert_equivalent(name, &index_of(name, files), "contenu seul");
        clean(name);
    }

    /// Identifiants stables : un homonyme inséré AVANT un symbole existant du
    /// même fichier décale son rang (`#run` → `#run~2`) ; cartes et sorties
    /// des outils de « tronc + deltas » ≡ reconstruction complète.
    #[test]
    fn homonym_ids_follow_deltas() {
        use SymbolKind::*;
        let name = "cortex-test-incr-homonymes";
        let mut files = start(name);
        let c = upsert(
            &mut files,
            file(
                "src/lib/svc.ts",
                vec![sym("run", Method, 1, 3, &["premier"]), sym("autre", Function, 4, 6, &[])],
                &[],
                &[("run", 5)],
                &["service"],
            ),
        );
        apply(name, vec![c], None).unwrap();
        assert_equivalent(name, &index_of(name, files.clone()), "fichier à méthode run");
        let mut e = files.iter().find(|f| f.path == "src/lib/svc.ts").unwrap().clone();
        e.symbols.insert(0, sym("run", Method, 1, 1, &["insere"]));
        e.hash = "hom".into();
        let c = upsert(&mut files, e);
        apply(name, vec![c], None).unwrap();
        let h = Handle::open(name).unwrap();
        let o = outil(&h, Appel::Outline { cible: "src/lib/svc.ts".into() });
        assert!(
            o.contains("S:src/lib/svc.ts#run method L1 — insere") && o.contains("S:src/lib/svc.ts#run~2 method L1-3 — premier"),
            "{}",
            o
        );
        drop(h);
        assert_equivalent(name, &index_of(name, files), "homonyme inséré avant");
        clean(name);
    }

    /// La sélection partielle des `limit` premiers candidats (sans trier tout)
    /// rend exactement le préfixe du classement complet, à toute coupure.
    #[test]
    fn search_limit_is_prefix_of_full_ranking() {
        let name = "cortex-test-incr-limit";
        let _ = start(name);
        let h = Handle::open(name).unwrap();
        let key = |x: &crate::search::Hit| format!("{}|{}|{}|{}", x.name, x.file, x.line, x.score.to_bits());
        for q in QUERIES {
            let full: Vec<String> = h.search(q, 10_000).iter().map(key).collect();
            for k in 0..=full.len() + 1 {
                let part: Vec<String> = h.search(q, k).iter().map(key).collect();
                assert_eq!(part, full[..k.min(full.len())].to_vec(), "« {} » coupé à {}", q, k);
            }
        }
        drop(h);
        clean(name);
    }

    /// Ajout d'un fichier (et d'un export qui rend AMBIGU un appel jusque-là
    /// résolu globalement) : voie delta, résolution identique à une reconstruction.
    #[test]
    fn file_added_takes_delta_path() {
        let name = "cortex-test-incr-add";
        let mut files = start(name);
        let e = file("src/lib/extra.ts", vec![sym("remplir", SymbolKind::Function, 1, 3, &["autre", "remplissage"])], &[], &[], &["extra"]);
        // `nouveau` : importé par cachePanel depuis `@/lib/absent`, qui apparaît.
        let e2 = file("src/lib/absent.ts", vec![sym("nouveau", SymbolKind::Function, 1, 3, &[])], &[], &[], &[]);
        // `globalFn` : importé d'un paquet externe, devient résolu (nom unique).
        let e3 = file("src/lib/g.ts", vec![sym("globalFn", SymbolKind::Function, 1, 3, &[])], &[], &[], &[]);
        let cs = vec![upsert(&mut files, e), upsert(&mut files, e2), upsert(&mut files, e3)];
        let h = Handle::open(name).unwrap();
        assert!(!ctx(&h, "CachePanel").contains("src/lib/absent.ts"));
        drop(h);
        assert!(matches!(apply(name, cs, None).unwrap(), Outcome::Delta { reresolved_files, .. } if reresolved_files >= 2));
        let h = Handle::open(name).unwrap();
        assert!(h.search("remplissage", 5).iter().any(|x| x.file == "src/lib/extra.ts"));
        assert!(ctx(&h, "CachePanel").contains("src/lib/absent.ts"), "{}", ctx(&h, "CachePanel"));
        assert!(ctx(&h, "App").contains("src/lib/g.ts"));
        drop(h);
        assert_equivalent(name, &index_of(name, files), "fichier ajouté");
        clean(name);
    }

    /// Suppression d'un fichier : plus aucun lien vers lui, voie delta.
    #[test]
    fn file_removed_takes_delta_path() {
        let name = "cortex-test-incr-remove";
        let mut files = start(name);
        let c = remove(&mut files, "src/lib/other.ts");
        assert!(matches!(apply(name, vec![c], None).unwrap(), Outcome::Delta { .. }));
        let h = Handle::open(name).unwrap();
        assert!(!ctx(&h, "App").contains("src/lib/other.ts"));
        assert!(!h.file_paths().contains(&"src/lib/other.ts"));
        drop(h);
        assert_equivalent(name, &index_of(name, files), "fichier supprimé");
        clean(name);
    }

    /// Renommage d'un symbole (et de ses appels) : l'ancien nom disparaît, le
    /// nouveau résout — voie delta.
    #[test]
    fn symbol_renamed_takes_delta_path() {
        let name = "cortex-test-incr-rename-sym";
        let mut files = start(name);
        let mut lib = files.iter().find(|f| f.path == "src/lib/store.ts").unwrap().clone();
        lib.symbols[0] = sym("purger", SymbolKind::Function, 1, 5, &["nettoie"]);
        lib.hash = "h-ren".into();
        let mut hooks = files.iter().find(|f| f.path == "src/hooks/index.ts").unwrap().clone();
        hooks.refs.imported_names = vec!["purger".into(), "remplir".into()];
        hooks.refs.calls[0].name = "purger".into();
        hooks.hash = "h-ren2".into();
        let c1 = upsert(&mut files, lib);
        let c2 = upsert(&mut files, hooks);
        assert!(matches!(apply(name, vec![c1, c2], None).unwrap(), Outcome::Delta { .. }));
        let h = Handle::open(name).unwrap();
        assert!(ctx(&h, "useCache").contains("purger"));
        drop(h);
        assert_equivalent(name, &index_of(name, files), "symbole renommé");
        clean(name);
    }

    /// Renommage d'un fichier (suppression + ajout) : les importeurs par l'ancien
    /// chemin perdent le lien, ceux qui visent le nouveau le gagnent.
    #[test]
    fn file_renamed_takes_delta_path() {
        let name = "cortex-test-incr-rename-file";
        let mut files = start(name);
        let mut moved = files.iter().find(|f| f.path == "src/hooks/index.ts").unwrap().clone();
        moved.path = "src/hooks/useCache.ts".into();
        let c1 = remove(&mut files, "src/hooks/index.ts");
        let c2 = upsert(&mut files, moved);
        assert!(matches!(apply(name, vec![c1, c2], None).unwrap(), Outcome::Delta { .. }));
        assert_equivalent(name, &index_of(name, files), "fichier renommé");
        clean(name);
    }

    /// TEST-JUGE : une chaîne de mises à jour variées (chacune par la voie
    /// delta), puis une compaction, répond EXACTEMENT comme une reconstruction
    /// complète de l'état final — à chaque étape.
    #[test]
    fn incremental_chain_then_compaction_matches_full_rebuild() {
        use SymbolKind::*;
        let name = "cortex-test-incr-equivalence";
        let mut files = start(name);
        let step = |files: &mut Vec<FileEntry>, label: &str, changes: Vec<Change>| {
            let out = apply(name, changes, None).unwrap();
            assert!(matches!(out, Outcome::Delta { .. } | Outcome::Unchanged), "{}: {:?}", label, out);
            assert_equivalent(name, &index_of(name, files.clone()), label);
        };
        // 1. corps modifié
        let mut e = files.iter().find(|f| f.path == "src/lib/store.ts").unwrap().clone();
        e.symbols[1].doc.push("rapide".into());
        e.hash = "s1".into();
        let c = upsert(&mut files, e);
        step(&mut files, "corps", vec![c]);
        // 1b. contenu seul modifié (symboles identiques) : champ corps en delta
        let e = files.iter().find(|f| f.path == "src/app/main.tsx").unwrap().clone();
        let c = upsert(&mut files, with_body(e, "Sentinelle du chargement rapide, sentinelles corps corps", "s1b"));
        step(&mut files, "contenu seul", vec![c]);
        // 2. homonyme ajouté : `helper` devient ambigu pour qui ne l'importe pas
        let c = upsert(
            &mut files,
            file(
                "src/util/helper.ts",
                vec![sym("helper", Function, 1, 2, &["aide"]), sym("vider", Const, 3, 3, &[])],
                &[],
                &[],
                &["utilitaires"],
            ),
        );
        step(&mut files, "homonyme ajouté", vec![c]);
        // 3. symbole ajouté + réordonné dans un fichier existant
        let mut e = files.iter().find(|f| f.path == "src/lib/other.ts").unwrap().clone();
        e.symbols.insert(0, sym("preparer", Function, 1, 1, &["prepare"]));
        e.symbols.swap(1, 2);
        e.refs.calls.push(CallRef { name: "helper".into(), line: 1 });
        e.hash = "s3".into();
        let c = upsert(&mut files, e);
        step(&mut files, "symbole ajouté/réordonné", vec![c]);
        // 4. fichier renommé (index.ts → useCache.ts : l'import `@/hooks` ne résout plus)
        let mut moved = files.iter().find(|f| f.path == "src/hooks/index.ts").unwrap().clone();
        moved.path = "src/hooks/useCache.ts".into();
        let c1 = remove(&mut files, "src/hooks/index.ts");
        let c2 = upsert(&mut files, moved);
        step(&mut files, "fichier renommé", vec![c1, c2]);
        // 5. fichier supprimé + fichier touché (même contenu, mtime neuf)
        let mut t = files.iter().find(|f| f.path == "docs/guide.md").unwrap().clone();
        t.mtime = 999;
        t.size = 101;
        let c1 = remove(&mut files, "src/lib/other.ts");
        let c2 = upsert(&mut files, t);
        step(&mut files, "supprimé + touché", vec![c1, c2]);
        // 6. symbole renommé + genre changé (vider fonction → vider classe)
        let mut e = files.iter().find(|f| f.path == "src/lib/store.ts").unwrap().clone();
        e.symbols[0].kind = Class;
        e.hash = "s6".into();
        let c = upsert(&mut files, e);
        step(&mut files, "genre changé", vec![c]);
        // 7. index.ts recréé (l'import `@/hooks` résout de nouveau)
        let c = upsert(
            &mut files,
            file("src/hooks/index.ts", vec![sym("useCache", Hook, 1, 4, &["reexport"])], &[("./useCache", &["useCache"])], &[], &[]),
        );
        step(&mut files, "index recréé", vec![c]);
        assert!(segs(name) > 2, "la chaîne doit avoir empilé des deltas");

        // Fusion des deltas (compaction courante) : même réponse, un seul delta,
        // qui sert ensuite de base à une mise à jour ordinaire.
        merge_deltas(name).unwrap();
        assert_eq!(segs(name), 2, "la fusion ramène à tronc + un delta");
        assert_equivalent(name, &index_of(name, files.clone()), "après fusion des deltas");
        let mut e = files.iter().find(|f| f.path == "src/lib/store.ts").unwrap().clone();
        e.symbols[1].doc.push("apres_fusion".into());
        e.hash = "s8".into();
        let c = upsert(&mut files, e);
        step(&mut files, "mise à jour après fusion", vec![c]);

        crate::atlas::compact(name).unwrap();
        assert_eq!(segs(name), 1, "la compaction ramène à un seul tronc");
        assert_equivalent(name, &index_of(name, files.clone()), "après compaction");
        clean(name);
    }

    /// Pertinence : l'ORDRE des résultats (égalités comprises) est identique
    /// entre « tronc + deltas » et une reconstruction complète, sur un corpus
    /// riche en ex-aequo (mêmes tokens, mêmes longueurs de nom, fichiers
    /// différents).
    #[test]
    fn relevance_order_identical_between_deltas_and_rebuild() {
        use SymbolKind::*;
        let name = "cortex-test-incr-relevance";
        clean(name);
        let mut files: Vec<FileEntry> = (0..12)
            .map(|i| {
                file(
                    &format!("src/mod{:02}/file.ts", i),
                    vec![
                        sym("loadData", Function, 1, 5, &["charge", "donnees"]),
                        sym("saveData", Function, 6, 9, &["sauve"]),
                        sym(&format!("helper{}", i % 3), Function, 10, 12, &[]),
                    ],
                    &[],
                    &[("loadData", 7)],
                    &["module", "donnees"],
                )
            })
            .collect();
        crate::atlas::rebuild_full(name, &index_of(name, files.clone()), Default::default()).unwrap();
        // Des deltas qui ajoutent des ex-aequo AVANT et APRÈS les existants
        // dans l'ordre canonique, et modifient un fichier du milieu.
        let c1 = upsert(
            &mut files,
            file("src/mod00a/file.ts", vec![sym("loadData", Function, 1, 5, &["charge", "donnees"])], &[], &[], &["module", "donnees"]),
        );
        apply(name, vec![c1], None).unwrap();
        let mut mid = files.iter().find(|f| f.path == "src/mod05/file.ts").unwrap().clone();
        mid.symbols.reverse();
        mid.hash = "mid".into();
        let c2 = upsert(&mut files, mid);
        let c3 = upsert(&mut files, file("src/zz/file.ts", vec![sym("saveData", Function, 1, 5, &["sauve"])], &[], &[], &[]));
        apply(name, vec![c2, c3], None).unwrap();
        let h_incr = Handle::open(name).unwrap();
        assert!(h_incr.segment_count() >= 3);
        let full = format!("{}-full", name);
        clean(&full);
        crate::atlas::rebuild_full(&full, &index_of(&full, files), Default::default()).unwrap();
        let h_full = Handle::open(&full).unwrap();
        for q in ["load data", "save data", "donnees", "helper", "charge module", "data"] {
            let a: Vec<(String, String, u32, u32)> =
                h_incr.search(q, 100).into_iter().map(|x| (x.name, x.file, x.line, x.score.to_bits())).collect();
            let b: Vec<(String, String, u32, u32)> =
                h_full.search(q, 100).into_iter().map(|x| (x.name, x.file, x.line, x.score.to_bits())).collect();
            assert!(!a.is_empty(), "requête {}", q);
            assert_eq!(a, b, "ordre différent pour « {} »", q);
        }
        drop(h_incr);
        drop(h_full);
        clean(name);
        clean(&full);
    }

    /// Équivalence à l'échelle d'un VRAI projet (manuel) :
    /// `CORTEX_EQUIV_PROJECT=<projet> cargo test real_project -- --ignored`
    /// (questions lues dans `CORTEX_BENCH_DIR` : `queries.json`, `holdout.json`).
    /// Copie l'atlas, applique une série de changements réalistes (corps,
    /// export ajouté, symbole renommé, fichier supprimé, fichier ajouté, fichier
    /// renommé) par deltas, puis compare recherche (banc de pertinence) et
    /// context/explain des symboles touchés à une reconstruction complète.
    #[test]
    #[ignore]
    fn real_project_equivalence() {
        let Ok(src) = std::env::var("CORTEX_EQUIV_PROJECT") else { return };
        let name = "cortex-test-equiv-real";
        clean(name);
        let h0 = Handle::open(&src).expect("projet source");
        let mut files = h0.materialize().files;
        drop(h0);
        crate::atlas::rebuild_full(name, &index_of(name, files.clone()), Default::default()).unwrap();
        // Fichiers-échantillons : les plus « connectés » (beaucoup d'appels).
        let mut order: Vec<usize> = (0..files.len()).filter(|&i| files[i].symbols.len() >= 3 && !files[i].refs.calls.is_empty()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse(files[i].refs.calls.len()));
        let picks: Vec<String> = order.iter().take(12).map(|&i| files[i].path.clone()).collect();
        let mut touched: Vec<String> = Vec::new();
        for (k, p) in picks.iter().enumerate() {
            let mut e = files.iter().find(|f| &f.path == p).unwrap().clone();
            let change = match k % 6 {
                0 => {
                    e.symbols[0].doc.push("modifie".into());
                    upsert(&mut files, {
                        e.hash.push('0');
                        e
                    })
                }
                1 => {
                    let mut s = e.symbols[1].clone();
                    s.name = format!("{}Bis", s.name);
                    s.tokens = crate::symbol::tokenize_identifier(&s.name);
                    touched.push(s.name.clone());
                    e.symbols.push(s);
                    e.hash.push('1');
                    upsert(&mut files, e)
                }
                2 => {
                    touched.push(e.symbols[0].name.clone());
                    e.symbols[0].name = format!("{}Renomme", e.symbols[0].name);
                    e.symbols[0].tokens = crate::symbol::tokenize_identifier(&e.symbols[0].name);
                    touched.push(e.symbols[0].name.clone());
                    e.hash.push('2');
                    upsert(&mut files, e)
                }
                3 => {
                    touched.extend(e.symbols.iter().map(|s| s.name.clone()));
                    remove(&mut files, p)
                }
                4 => {
                    e.path = format!("{}.copie.ts", e.path);
                    touched.extend(e.symbols.iter().map(|s| s.name.clone()));
                    upsert(&mut files, e)
                }
                _ => {
                    let c1 = remove(&mut files, p);
                    apply(name, vec![c1], None).unwrap();
                    e.path = format!("{}-deplace.ts", p.trim_end_matches(".ts").trim_end_matches(".tsx"));
                    upsert(&mut files, e)
                }
            };
            let out = apply(name, vec![change], None).unwrap();
            assert!(matches!(out, Outcome::Delta { .. } | Outcome::Compacted), "{:?}", out);
        }
        let full = format!("{}-full", name);
        clean(&full);
        crate::atlas::rebuild_full(&full, &index_of(&full, files.clone()), Default::default()).unwrap();
        let (hi, hf) = (Handle::open(name).unwrap(), Handle::open(&full).unwrap());
        eprintln!("segments tronc+deltas : {}", hi.segment_count());
        let qs: Vec<String> = ["queries.json", "holdout.json"]
            .iter()
            .filter_map(|f| std::fs::read_to_string(crate::bench::bench_file(f)).ok())
            .filter_map(|c| crate::bench::parse(&c).ok())
            .flat_map(|b| b.queries.into_iter().map(|q| q.q))
            .collect();
        for q in qs.iter().map(|s| s.as_str()).chain(touched.iter().map(|s| s.as_str())) {
            let a: Vec<(String, String, u32, u32)> =
                hi.search(q, 60).into_iter().map(|x| (x.name, x.file, x.line, x.score.to_bits())).collect();
            let b: Vec<(String, String, u32, u32)> =
                hf.search(q, 60).into_iter().map(|x| (x.name, x.file, x.line, x.score.to_bits())).collect();
            assert_eq!(a, b, "recherche « {} »", q);
        }
        for n in &touched {
            assert_eq!(ctx(&hi, n).replace(name, "P"), ctx(&hf, n).replace(&full, "P"), "context {}", n);
            assert_eq!(imp(&hi, n).replace(name, "P"), imp(&hf, n).replace(&full, "P"), "explain {}", n);
            assert_eq!(outil(&hi, Appel::Card { cible: n.clone() }), outil(&hf, Appel::Card { cible: n.clone() }), "carte {}", n);
        }
        eprintln!("{} requêtes et {} symboles touchés : identiques", qs.len(), touched.len());
        // Fusion des deltas : toujours identique à la reconstruction complète.
        drop(hi);
        merge_deltas(name).unwrap();
        let hi = Handle::open(name).unwrap();
        assert_eq!(hi.segment_count(), 2);
        for q in qs.iter().map(|s| s.as_str()).chain(touched.iter().map(|s| s.as_str())) {
            let a: Vec<(String, String, u32, u32)> =
                hi.search(q, 60).into_iter().map(|x| (x.name, x.file, x.line, x.score.to_bits())).collect();
            let b: Vec<(String, String, u32, u32)> =
                hf.search(q, 60).into_iter().map(|x| (x.name, x.file, x.line, x.score.to_bits())).collect();
            assert_eq!(a, b, "recherche après fusion « {} »", q);
        }
        for n in &touched {
            assert_eq!(ctx(&hi, n).replace(name, "P"), ctx(&hf, n).replace(&full, "P"), "context après fusion {}", n);
        }
        eprintln!("après fusion des deltas : identiques");
        drop(hi);
        drop(hf);
        clean(name);
        clean(&full);
    }

    /// Statistiques de volume d'un vrai projet (manuel).
    #[test]
    #[ignore]
    fn real_project_volumes() {
        let Ok(src) = std::env::var("CORTEX_EQUIV_PROJECT") else { return };
        let h = Handle::open(&src).unwrap();
        let idx = h.materialize();
        let calls: usize = idx.files.iter().map(|f| f.refs.calls.len()).sum();
        let pairs: usize = idx.files.iter().map(|f| f.refs.calls.len() * f.symbols.len()).sum();
        let imports: usize = idx.files.iter().map(|f| f.refs.imports.len()).sum();
        let doc: usize = idx.files.iter().flat_map(|f| f.symbols.iter()).map(|s| s.doc.len()).sum();
        eprintln!("appels {} ; appels×symboles {} ; imports {} ; tokens de doc {}", calls, pairs, imports, doc);
    }

    /// Deux écrivains CONCURRENTS (CLI + serveur MCP) : le verrou d'écriture
    /// sérialise les publications — aucun id attribué deux fois, les deux
    /// changements sont présents.
    #[test]
    fn concurrent_writers_are_serialized() {
        let name = "cortex-test-incr-concurrent";
        let mut files = start(name);
        let mut a = files.iter().find(|f| f.path == "src/lib/other.ts").unwrap().clone();
        a.symbols.push(sym("ajouteA", SymbolKind::Function, 20, 21, &[]));
        a.hash = "ca".into();
        let mut b = files.iter().find(|f| f.path == "src/app/cachePanel.tsx").unwrap().clone();
        b.symbols.push(sym("ajouteB", SymbolKind::Function, 30, 31, &[]));
        b.hash = "cb".into();
        upsert(&mut files, a.clone());
        upsert(&mut files, b.clone());
        let ta = std::thread::spawn(move || apply(name, vec![Change::Upsert(a)], None).unwrap());
        let tb = std::thread::spawn(move || apply(name, vec![Change::Upsert(b)], None).unwrap());
        assert!(matches!(ta.join().unwrap(), Outcome::Delta { .. }));
        assert!(matches!(tb.join().unwrap(), Outcome::Delta { .. }));
        assert_eq!(segs(name), 3);
        assert_equivalent(name, &index_of(name, files), "écrivains concurrents");
        clean(name);
    }

    /// Mise à jour « touché seulement » (mtime) : un delta de métadonnées, et
    /// le fichier n'est plus vu comme changé.
    #[test]
    fn touch_only_updates_meta() {
        let name = "cortex-test-incr-touch";
        let mut files = start(name);
        let mut t = files.iter().find(|f| f.path == "src/lib/store.ts").unwrap().clone();
        t.mtime = 424242;
        let c = upsert(&mut files, t);
        assert!(matches!(apply(name, vec![c], None).unwrap(), Outcome::Delta { .. }));
        let h = Handle::open(name).unwrap();
        let r = h.node(h.file_by_path("src/lib/store.ts").unwrap()).unwrap();
        assert_eq!(h.meta(&r).0, 424242);
        drop(h);
        let same = files.iter().find(|f| f.path == "src/lib/store.ts").unwrap().clone();
        assert_eq!(apply(name, vec![Change::Upsert(same)], None).unwrap(), Outcome::Unchanged);
        assert_equivalent(name, &index_of(name, files), "touché");
        clean(name);
    }

    /// Compaction automatique au-delà du seuil de segments : petits deltas →
    /// fusion des deltas (le tronc n'est pas réécrit).
    #[test]
    fn automatic_compaction_beyond_threshold() {
        let name = "cortex-test-incr-autocompact";
        let mut files = start(name);
        let mut compacted = false;
        for i in 0..(manifest::COMPACT_THRESHOLD + 1) {
            let mut e = files.iter().find(|f| f.path == "src/app/cachePanel.tsx").unwrap().clone();
            e.symbols[0].doc = vec![format!("version{}", i)];
            e.hash = format!("v{}", i);
            let c = upsert(&mut files, e);
            if apply(name, vec![c], None).unwrap() == Outcome::Merged {
                compacted = true;
            }
        }
        assert!(compacted, "le seuil doit déclencher une compaction");
        assert!(segs(name) <= manifest::COMPACT_THRESHOLD);
        assert_equivalent(name, &index_of(name, files), "compaction automatique");
        clean(name);
    }
}
