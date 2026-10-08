//! Atlas — cœur du contexte code de Cortex 2 (architecture v2, §4 et §8).
//!
//! Un atlas = un DOSSIER `~/.cortex/<projet>/atlas/` avec un `manifest.json`
//! (petit, JSON) et une pile de SEGMENTS `.atlas` (rkyv, ouverts par mmap —
//! voir `segment.rs`) : un TRONC (tout le projet) puis des DELTAS (une mise à
//! jour chacun, quelques Ko). La lecture fusionne tronc + deltas (`view.rs`) ;
//! une mise à jour n'écrit qu'un delta (`incremental.rs`) ; au-delà d'un seuil,
//! la compaction refond la pile en un tronc neuf (`compact`). L'atlas est la
//! SEULE source de vérité : pas d'autre cache (l'ancien `index.bin` n'est lu
//! qu'une fois, pour retrouver la racine d'un projet à migrer).

pub mod build;
pub mod cards;
pub mod fresh;
pub mod incremental;
pub mod manifest;
pub mod query;
pub mod schema;
pub mod segment;
pub mod view;

use crate::index::ProjectIndex;
use manifest::Manifest;
use segment::OpenSegment;
use std::path::{Path, PathBuf};

/// Un atlas ouvert : le manifeste + tous ses segments actifs, mmap-és.
pub struct Handle {
    pub project: String,
    pub(crate) manifest: Manifest,
    pub(crate) opened: Vec<OpenSegment>,
    /// Vue fusionnée (version courante de chaque id) — construite à la première
    /// lecture qui en a besoin, jamais à `open()` (cible d'ouverture < 5 ms).
    pub(crate) view: std::sync::OnceLock<view::View>,
}

impl Handle {
    /// Ouvre l'atlas existant d'un projet (ne construit rien).
    ///
    /// La validation bytecheck d'un gros segment coûte l'essentiel du temps
    /// d'ouverture : un segment n'étant JAMAIS réécrit (nom neuf à chaque
    /// écriture, voir `manifest.rs`), on ne la saute que si le manifeste
    /// contient une empreinte (nom, taille, mtime) IDENTIQUE au fichier — sinon
    /// bytecheck complet avant toute lecture. Un manifeste d'un autre format
    /// est refusé (`InvalidData`) : `ensure_and_open` réindexe alors.
    pub fn open(project: &str) -> std::io::Result<Handle> {
        let m = Manifest::load(project)?;
        if m.format_version != schema::FORMAT_VERSION {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("format d'atlas v{} (attendu v{})", m.format_version, schema::FORMAT_VERSION),
            ));
        }
        let mut opened = Vec::with_capacity(m.segments.len());
        for name in &m.segments {
            let path = m.segment_path(project, name);
            let seg = OpenSegment::open(&path)?;
            let already_validated =
                Manifest::stat_segment(project, name).map(|(size, mtime)| m.is_validated(name, size, mtime)).unwrap_or(false);
            if !already_validated {
                seg.view().map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            }
            opened.push(seg);
        }
        if opened.is_empty() {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "atlas sans segment"));
        }
        Ok(Handle { project: project.to_string(), manifest: m, opened, view: std::sync::OnceLock::new() })
    }

    /// Ouvre le TRONC seul (sans les deltas) — base de la fusion des deltas.
    pub(crate) fn open_trunk(project: &str) -> std::io::Result<Handle> {
        let mut h = Handle::open(project)?;
        h.opened.truncate(1);
        h.manifest.segments.truncate(1);
        Ok(h)
    }

    pub fn root(&self) -> &str {
        &self.manifest.root
    }

    /// Nombre de segments actifs (1 = tronc seul).
    pub fn segment_count(&self) -> usize {
        self.opened.len()
    }

    /// Empreinte de la pile de segments : change à chaque mise à jour écrite.
    pub fn segments_cle(&self) -> String {
        self.manifest.segments.join(",")
    }
}

/// Racine d'un ancien `index.bin` (format v1, bincode) : les deux premiers
/// champs sérialisés sont `name` puis `root`, chacun préfixé de sa longueur
/// (u64 little-endian). Lu UNE fois, pour migrer un projet qui n'a pas encore
/// d'atlas — sans dépendre de bincode.
fn legacy_root(project: &str) -> Option<String> {
    let bytes = std::fs::read(legacy_index_path(project)).ok()?;
    let read_str = |at: usize| -> Option<(String, usize)> {
        let len = u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?) as usize;
        let s = std::str::from_utf8(bytes.get(at + 8..at + 8 + len)?).ok()?.to_string();
        Some((s, at + 8 + len))
    };
    let (_name, next) = read_str(0)?;
    read_str(next).map(|(root, _)| root)
}

fn legacy_index_path(project: &str) -> PathBuf {
    crate::index::cortex_home().join(project).join("index.bin")
}

/// Vrai si ce dossier de `~/.cortex` est un projet (atlas, ou ancien index à migrer).
pub fn is_project_dir(dir: &Path) -> bool {
    dir.join("atlas").join("manifest.json").exists() || dir.join("index.bin").exists()
}

/// Racine enregistrée d'un projet (manifeste, sinon ancien `index.bin`).
pub fn project_root(project: &str) -> Option<String> {
    Manifest::load(project).ok().map(|m| m.root).or_else(|| legacy_root(project))
}

/// Ouvre l'atlas d'un projet ; s'il manque, est d'un autre format ou est
/// illisible (segment absent/corrompu), le RECONSTRUIT depuis les sources
/// (racine du manifeste, ou d'un ancien `index.bin`), une fois.
pub fn ensure_and_open(project: &str) -> std::io::Result<Handle> {
    match Handle::open(project) {
        Ok(h) => Ok(h),
        Err(e) => {
            let Some(root) = project_root(project) else { return Err(e) };
            let root_path = PathBuf::from(&root);
            if !root_path.exists() {
                return Err(std::io::Error::new(std::io::ErrorKind::NotFound, format!("source missing: {}", root)));
            }
            eprintln!("[cortex] atlas '{}' needs rebuilding ({}); full reindex of {}…", project, e, root);
            let (idx, tracked) = crate::index::build_index(project, &root_path)?;
            rebuild_full(project, &idx, tracked).map_err(|we| {
                if matches!(we.kind(), std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem) {
                    std::io::Error::new(
                        we.kind(),
                        format!(
                            "index of '{}' must be rebuilt ({}) but the cortex home is read-only: run cortex outside the sandbox once",
                            project, e
                        ),
                    )
                } else {
                    we
                }
            })?;
            Handle::open(project)
        }
    }
}

/// Reconstruit ENTIÈREMENT l'atlas (un seul tronc) à partir d'un `ProjectIndex`
/// à jour, dans un fichier NEUF ; le manifeste bascule de façon atomique, les
/// anciens segments deviennent orphelins (nettoyés en best-effort). Supprime
/// l'ancien `index.bin` s'il traîne encore (migration terminée).
pub fn rebuild_full(project: &str, idx: &ProjectIndex, tracked: manifest::Tracked) -> std::io::Result<()> {
    let _lock = manifest::WriteLock::acquire(project)?;
    rebuild_full_locked(project, idx, tracked)
}

/// `rebuild_full` sous verrou d'écriture déjà pris.
pub(crate) fn rebuild_full_locked(project: &str, idx: &ProjectIndex, tracked: manifest::Tracked) -> std::io::Result<()> {
    let dbg = std::env::var("CORTEX_DEBUG_TIMING").is_ok();
    let seg = build::build_full(idx);
    let name = manifest::new_segment_name();
    let path = manifest::atlas_dir(project).join(&name);
    let bytes = segment::write_segment(&path, &seg)?;
    drop(seg);
    let mut m = Manifest::new(&idx.root, vec![name.clone()]);
    m.skipped = tracked.skipped;
    m.walk = tracked.walk;
    let t = std::time::Instant::now();
    // Validation sur les octets écrits, sans rouvrir le fichier (voir
    // `validate_bytes_and_record`).
    validate_bytes_and_record(project, &mut m, &name, &bytes)?;
    if dbg {
        eprintln!("[timing] validation: {:.2}ms", t.elapsed().as_secs_f64() * 1000.0);
    }
    m.save(project)?;
    Manifest::clean_orphans(project, &[name]);
    let _ = std::fs::remove_file(legacy_index_path(project));
    Ok(())
}

/// Valide (bytecheck) un segment qui vient d'être écrit et enregistre son
/// empreinte dans le manifeste — un segment entre dans le manifeste déjà connu
/// bon, ou pas du tout.
pub(crate) fn validate_and_record(project: &str, m: &mut Manifest, name: &str) -> std::io::Result<()> {
    let path = m.segment_path(project, name);
    let seg = OpenSegment::open(&path)?;
    seg.view().map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("segment fraîchement écrit invalide: {e}")))?;
    let (size, mtime) = Manifest::stat_segment(project, name)?;
    m.mark_validated(name, size, mtime);
    Ok(())
}

/// Comme `validate_and_record`, mais sur les octets EXACTS que `write_segment`
/// vient d'écrire, sans rouvrir le fichier : sous Windows, la première
/// ouverture d'un fichier tout juste écrit attend l'analyse de l'antivirus
/// (≈ 10 ms mesurées pour un delta de 12 Ko), coût qui dominait la mise à jour
/// d'un fichier. La sécurité est la même : ce sont ces octets-là qui sont sur
/// disque, et une ouverture ultérieure revalide si (taille, mtime) diffèrent.
pub(crate) fn validate_bytes_and_record(project: &str, m: &mut Manifest, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    rkyv::check_archived_root::<schema::AtlasSegment>(bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("segment fraîchement écrit invalide: {e}")))?;
    let (size, mtime) = Manifest::stat_segment(project, name)?;
    m.mark_validated(name, size, mtime);
    Ok(())
}

/// Revalide (bytecheck) tous les segments et enregistre leurs empreintes —
/// utile après une COPIE de l'atlas (mtimes neufs : sans cela la première
/// ouverture revaliderait tout, voir `Handle::open`).
pub fn revalidate(project: &str) -> std::io::Result<()> {
    let mut m = Manifest::load(project)?;
    for name in m.segments.clone() {
        validate_and_record(project, &mut m, &name)?;
    }
    m.save(project)
}

/// Compaction (§4) : refond tronc + deltas en UN tronc neuf, numéroté dans
/// l'ordre canonique. Reconstruit depuis l'état fusionné COMPLET (références
/// brutes comprises, `View::materialize`) par la construction de référence
/// (`build_full`) : le résultat est par construction celui d'une reconstruction
/// complète — sans relire ni reparser une seule source.
pub fn compact(project: &str) -> std::io::Result<()> {
    let _lock = manifest::WriteLock::acquire(project)?;
    compact_locked(project)
}

/// `compact` sous verrou d'écriture déjà pris.
pub(crate) fn compact_locked(project: &str) -> std::io::Result<()> {
    let dbg = std::env::var("CORTEX_DEBUG_TIMING").is_ok();
    let t0 = std::time::Instant::now();
    let h = Handle::open(project)?;
    let idx = h.materialize();
    if dbg {
        eprintln!("[timing] open + materialization: {:.2}ms", t0.elapsed().as_secs_f64() * 1000.0);
    }
    let tracked = manifest::Tracked { skipped: h.manifest.skipped.clone(), walk: h.manifest.walk.clone() };
    drop(h);
    let r = rebuild_full_locked(project, &idx, tracked);
    let t1 = std::time::Instant::now();
    // ~1,5 M chaînes à libérer : en parallèle.
    {
        use rayon::prelude::*;
        idx.files.into_par_iter().for_each(drop);
    }
    if dbg {
        eprintln!(
            "[timing] release: {:.2}ms (compaction {:.2}ms)",
            t1.elapsed().as_secs_f64() * 1000.0,
            t0.elapsed().as_secs_f64() * 1000.0
        );
    }
    r
}

/// Compaction à faire après une mise à jour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compaction {
    /// Réécriture du tronc (`compact`) : deltas trop gros par rapport au tronc.
    Full,
    /// Fusion des deltas en un seul (`incremental::merge_deltas_locked`) :
    /// trop de segments, mais peu de contenu changé depuis le tronc.
    MergeDeltas,
}

/// Compaction nécessaire après une mise à jour : deltas trop gros par rapport
/// au tronc → réécriture du tronc ; sinon, trop de segments → fusion des
/// deltas (le coût d'une lecture croît avec le nombre de segments, pas avec
/// leur taille).
pub fn compaction_needed(project: &str, m: &Manifest) -> Option<Compaction> {
    // Tailles déjà connues par les empreintes de validation (pas de `stat`).
    let size = |n: &String| {
        m.validated
            .iter()
            .find(|v| &v.name == n)
            .map(|v| v.size)
            .unwrap_or_else(|| Manifest::stat_segment(project, n).map(|(s, _)| s).unwrap_or(0))
    };
    let base = m.segments.first().map(size).unwrap_or(0);
    let deltas: u64 = m.segments.iter().skip(1).map(size).sum();
    if deltas > manifest::COMPACT_DELTA_MIN_BYTES && deltas * 100 > base * manifest::COMPACT_DELTA_PERCENT {
        return Some(Compaction::Full);
    }
    if m.segments.len() > manifest::COMPACT_THRESHOLD {
        return Some(Compaction::MergeDeltas);
    }
    None
}

pub fn atlas_root_for(project: &str) -> PathBuf {
    manifest::atlas_dir(project)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::index::FileEntry;
    use crate::lang::Lang;
    use crate::symbol::{Symbol, SymbolKind};

    /// Petit projet-jouet (2 fichiers, 2 symboles, un appel résolu par import).
    fn fixture(name: &str) -> ProjectIndex {
        let f0 = FileEntry {
            path: "src/lib.ts".into(),
            lang: Lang::TypeScript,
            hash: "h0".into(),
            size: 10,
            mtime: 111,
            lines: 3,
            symbols: vec![Symbol {
                name: "vider".into(),
                kind: SymbolKind::Function,
                line: 1,
                end_line: 2,
                signature: "function vider()".into(),
                tokens: vec!["vider".into()],
                doc: vec!["nettoie".into(), "letat".into()],
                summary: "Nettoie l'état.".into(),
            }],
            refs: Default::default(),
            header: vec!["module".into(), "utilitaire".into()],
            summary: "Module utilitaire.".into(),
            body: vec![("etat".into(), 2), ("nettoi".into(), 1)],
        };
        let mut refs1 = crate::symbol::FileRefs::default();
        refs1.imports.push("./lib".to_string());
        refs1.imported_names.push("vider".to_string());
        refs1.calls.push(crate::symbol::CallRef { name: "vider".to_string(), line: 2 });
        let f1 = FileEntry {
            path: "src/main.ts".into(),
            lang: Lang::TypeScript,
            hash: "h1".into(),
            size: 20,
            mtime: 222,
            lines: 5,
            symbols: vec![Symbol {
                name: "main".into(),
                kind: SymbolKind::Function,
                line: 1,
                end_line: 5,
                signature: "function main()".into(),
                tokens: vec!["main".into()],
                doc: Vec::new(),
                summary: String::new(),
            }],
            refs: refs1,
            header: Vec::new(),
            summary: String::new(),
            body: Vec::new(),
        };
        ProjectIndex { name: name.to_string(), root: "/fixture".into(), generated_at: 0, files: vec![f0, f1] }
    }

    pub(crate) fn clean(project: &str) {
        let _ = std::fs::remove_dir_all(manifest::atlas_dir(project).parent().unwrap());
    }

    /// Aller-retour : ce qu'on écrit se relit à l'identique (références brutes comprises).
    #[test]
    fn roundtrip_full_segment() {
        let name = "cortex-test-atlas-roundtrip";
        clean(name);
        let idx = fixture(name);
        rebuild_full(name, &idx, Default::default()).expect("rebuild_full");
        let h = Handle::open(name).expect("open");
        let back = h.materialize();
        assert_eq!(back.files.len(), 2);
        let main = back.files.iter().find(|f| f.path == "src/main.ts").unwrap();
        assert_eq!(main.symbols[0].name, "main");
        assert_eq!(main.refs.calls.len(), 1);
        assert_eq!(main.refs.imports, vec!["./lib".to_string()]);
        let lib = back.files.iter().find(|f| f.path == "src/lib.ts").unwrap();
        assert_eq!(lib.header, vec!["module".to_string(), "utilitaire".to_string()]);
        let ctx = crate::outils::executer(std::slice::from_ref(&h), &crate::outils::Appel::Card { cible: "main".into() }, None);
        assert!(ctx.contains("S:src/lib.ts#vider"), "carte: {}", ctx);
        clean(name);
    }

    /// Recherche BM25F (index inversé).
    #[test]
    fn search_via_inverted_index() {
        let name = "cortex-test-atlas-search";
        clean(name);
        rebuild_full(name, &fixture(name), Default::default()).expect("rebuild_full");
        let h = Handle::open(name).expect("open");
        let hits = h.search("vider", 10);
        assert!(!hits.is_empty(), "aucun résultat pour 'vider'");
        assert_eq!(hits[0].name, "vider");
        clean(name);
    }

    /// Piège Windows (§4) : un segment déjà mmap-é reste lisible APRÈS qu'une
    /// mise à jour a écrit un segment neuf et basculé le manifeste.
    #[test]
    fn mmap_survives_concurrent_rebuild() {
        let name = "cortex-test-atlas-mmap-open";
        clean(name);
        let idx = fixture(name);
        rebuild_full(name, &idx, Default::default()).expect("rebuild_full 1");
        let h_old = Handle::open(name).expect("open ancien tronc");
        let mut idx2 = idx.clone();
        idx2.files[0].hash = "h0-modifie".into();
        rebuild_full(name, &idx2, Default::default()).expect("rebuild_full 2 (pendant que l'ancien est mmap-é)");
        let ctx = crate::outils::executer(std::slice::from_ref(&h_old), &crate::outils::Appel::Card { cible: "main".into() }, None);
        assert!(ctx.contains("vider"), "l'ancien mmap doit rester lisible: {}", ctx);
        let h_new = Handle::open(name).expect("réouverture après mise à jour");
        assert!(!h_new.search("vider", 5).is_empty());
        drop(h_old);
        clean(name);
    }

    /// Après un `rebuild_full`, le tronc est marqué validé avec son empreinte
    /// EXACTE ; une empreinte différente ne passe jamais pour validée.
    #[test]
    fn rebuild_full_marks_segment_validated() {
        let name = "cortex-test-atlas-validated";
        clean(name);
        rebuild_full(name, &fixture(name), Default::default()).expect("rebuild_full");
        let m = Manifest::load(name).unwrap();
        assert_eq!(m.validated.len(), 1);
        let seg_name = &m.segments[0];
        let (size, mtime) = Manifest::stat_segment(name, seg_name).unwrap();
        assert!(m.is_validated(seg_name, size, mtime));
        assert!(!m.is_validated(seg_name, size + 1, mtime));
        assert!(!m.is_validated(seg_name, size, mtime + 1));
        let h = Handle::open(name).expect("réouverture via le cache de validation");
        assert!(!h.search("vider", 5).is_empty());
        clean(name);
    }

    /// Une empreinte périmée retombe sur le bytecheck complet.
    #[test]
    fn stale_validation_falls_back_to_bytecheck() {
        let name = "cortex-test-atlas-stale-validated";
        clean(name);
        rebuild_full(name, &fixture(name), Default::default()).expect("rebuild_full");
        let mut m = Manifest::load(name).unwrap();
        let seg_name = m.segments[0].clone();
        m.mark_validated(&seg_name, 999_999_999, 0);
        m.save(name).unwrap();
        let h = Handle::open(name).expect("revalidation malgré l'empreinte périmée");
        assert!(!h.search("vider", 5).is_empty());
        clean(name);
    }

    /// Un manifeste d'un ancien format est refusé à l'ouverture (jamais des
    /// octets mal interprétés) — `ensure_and_open` réindexe alors.
    #[test]
    fn old_format_is_refused() {
        let name = "cortex-test-atlas-old-format";
        clean(name);
        rebuild_full(name, &fixture(name), Default::default()).expect("rebuild_full");
        let mut m = Manifest::load(name).unwrap();
        m.format_version = 1;
        m.save(name).unwrap();
        assert!(Handle::open(name).is_err());
        clean(name);
    }

    /// Lecture de la racine d'un ancien `index.bin` (bincode : longueurs u64 LE).
    #[test]
    fn legacy_root_is_read_without_bincode() {
        let name = "cortex-test-atlas-legacy-root";
        clean(name);
        let dir = crate::index::cortex_home().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let mut bytes = Vec::new();
        for s in [name, "C:/proj/racine"] {
            bytes.extend((s.len() as u64).to_le_bytes());
            bytes.extend(s.as_bytes());
        }
        bytes.extend([0u8; 16]);
        std::fs::write(dir.join("index.bin"), bytes).unwrap();
        assert_eq!(legacy_root(name).as_deref(), Some("C:/proj/racine"));
        clean(name);
    }
}
