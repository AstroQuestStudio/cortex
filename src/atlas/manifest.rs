//! Manifeste de l'atlas — JSON (petit, lisible), PAS rkyv : c'est la seule
//! partie qu'on réécrit à chaque mise à jour, et une écriture-puis-renommage
//! atomique dessus suffit à ne jamais laisser un lecteur voir un état
//! incohérent (le renommage de fichier est atomique sur NTFS comme ailleurs).
//!
//! Piège Windows (voir architecture v2 §4) : un segment mmap-é par un
//! processus (le serveur MCP, un autre `cortex` encore ouvert) ne peut être NI
//! écrasé NI supprimé. Une mise à jour n'écrit donc JAMAIS dans un fichier de
//! segment existant : elle en crée un NOUVEAU (nom horodaté), pointe le
//! manifeste dessus, puis tente de nettoyer les orphelins — en tolérant l'échec
//! (ils seront retentés à la prochaine mise à jour ou à la compaction).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub format_version: u32,
    /// Racine du projet (pour la fraîcheur / update sans repasser le chemin).
    pub root: String,
    /// Segments ACTIFS, du plus ancien au plus récent. Le DERNIER est le tronc
    /// (`AtlasSegment.is_head == true`) : lui seul porte le graphe/l'index à jour.
    pub segments: Vec<String>,
    pub generated_at: u64,
    /// Segments déjà validés par bytecheck (étape 4) — voir `ValidatedSegment`
    /// et `Handle::open`. `#[serde(default)]` : un manifeste écrit par une
    /// version antérieure (sans ce champ) se relit avec une liste vide, ce qui
    /// force une revalidation (comportement sûr par défaut, jamais l'inverse).
    #[serde(default)]
    pub validated: Vec<ValidatedSegment>,
    /// Chemins examinés mais NON indexés (binaire, illisible) avec l'empreinte
    /// (mtime µs, taille) vue à ce moment : le contrôle de fraîcheur ne les
    /// relit pas tant qu'elle ne change pas — sinon un fichier non suivi
    /// inindexable serait retraité à CHAQUE appel.
    #[serde(default)]
    pub skipped: Vec<SkippedPath>,
    /// État du dernier parcours (dossiers, entrées exclues, fichiers de règles) :
    /// le contrôle de fraîcheur voit les nouveaux fichiers sans `git status`
    /// (voir `fresh.rs`). `None` (manifeste antérieur) : le premier contrôle
    /// refait un parcours complet et l'enregistre.
    #[serde(default)]
    pub walk: Option<WalkState>,
}

/// Ce qu'un parcours sait au-delà des fichiers indexés — passé avec eux à
/// chaque écriture du manifeste.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tracked {
    pub skipped: Vec<SkippedPath>,
    pub walk: Option<WalkState>,
}

/// État d'un parcours gitignore-aware, pour détecter les changements par une
/// simple lecture des dossiers parcourus (voir `fresh.rs`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct WalkState {
    /// Dossiers parcourus qui ne contiennent (même en profondeur) aucun fichier
    /// suivi (indexé ou `skipped`) : les autres se déduisent des chemins.
    pub extra_dirs: Vec<String>,
    /// Entrées visibles dans un dossier parcouru mais EXCLUES par les règles
    /// (fichier d'extension indexable, dossier) : pas réexaminées à chaque contrôle.
    pub ignored: Vec<String>,
    /// Fichiers de règles DANS les dossiers parcourus (`.gitignore`, `.ignore`),
    /// chemin relatif + empreinte : un changement impose un parcours complet.
    pub rules: Vec<SkippedPath>,
    /// Règles HORS du projet (dossiers parents, `.git/info/exclude`, exclusions
    /// globales de git, présence de `.git`) : chemin absolu + empreinte
    /// (`size == u64::MAX` : absent).
    pub outside: Vec<SkippedPath>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SkippedPath {
    pub path: String,
    pub mtime: u64,
    pub size: u64,
}

/// Empreinte d'un segment validé UNE FOIS (juste après écriture, voir
/// `atlas::mod::validate_and_record`) — permet à une ouverture ultérieure de
/// SAUTER la revalidation bytecheck (coût O(taille du segment), la majeure
/// partie du temps d'ouverture mesuré) sans jamais lire un fichier qui n'a
/// jamais été validé. Voir la justification de sûreté dans `Handle::open`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidatedSegment {
    pub name: String,
    pub size: u64,
    pub mtime: u64,
}

/// Seuil de compaction : au-delà de ce nombre de segments actifs (tronc +
/// deltas), la mise à jour qui l'a franchi refond tout en un tronc neuf (voir
/// `atlas::compact`).
pub const COMPACT_THRESHOLD: usize = 16;

/// Seuil de compaction en TAILLE : deltas cumulés au-delà de ce pourcentage du
/// tronc (une grosse mise à jour se refond tout de suite plutôt que de ralentir
/// chaque lecture).
pub const COMPACT_DELTA_PERCENT: u64 = 20;
/// ... et seulement au-delà de cette taille absolue (un petit projet a un tronc
/// minuscule : quelques deltas le dépasseraient vite sans rien coûter).
pub const COMPACT_DELTA_MIN_BYTES: u64 = 1 << 20;

/// Âge minimal (secondes) d'un segment orphelin avant suppression : un autre
/// processus peut avoir écrit un segment neuf sans avoir encore publié le
/// manifeste qui le référence.
const ORPHAN_MIN_AGE_SECS: u64 = 120;

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Verrou d'ÉCRITURE d'un atlas (fichier créé en exclusif) : un seul
/// processus à la fois construit et publie un segment (CLI et serveur MCP
/// peuvent rafraîchir en même temps). Sans lui, deux deltas bâtis sur la même
/// pile attribueraient les mêmes ids neufs. Les lecteurs n'en ont pas besoin
/// (manifeste renommé atomiquement, segments immuables). Un verrou orphelin
/// (processus tué) est repris au-delà de `LOCK_STALE_SECS`.
pub struct WriteLock {
    path: PathBuf,
}

const LOCK_STALE_SECS: u64 = 30;

impl WriteLock {
    pub fn acquire(project: &str) -> std::io::Result<WriteLock> {
        let dir = atlas_dir(project);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("write.lock");
        let t0 = std::time::Instant::now();
        loop {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => return Ok(WriteLock { path }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(&path)
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|d| d.as_secs() > LOCK_STALE_SECS);
                    if stale {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    if t0.elapsed().as_secs() > 10 {
                        return Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, "atlas verrouillé en écriture"));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(e) => return Err(e),
            }
        }
    }
}

impl Drop for WriteLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn atlas_dir(project: &str) -> PathBuf {
    crate::index::cortex_home().join(project).join("atlas")
}

fn manifest_path(project: &str) -> PathBuf {
    atlas_dir(project).join("manifest.json")
}

/// Nom de fichier de segment neuf, horodaté à la microseconde (jamais de
/// collision avec un segment déjà mmap-é) — jamais un nom fixe réutilisé.
pub fn new_segment_name() -> String {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_micros()).unwrap_or(0);
    format!("seg-{}.atlas", t)
}

impl Manifest {
    pub fn load(project: &str) -> std::io::Result<Manifest> {
        let bytes = std::fs::read(manifest_path(project))?;
        serde_json::from_slice(&bytes).map_err(std::io::Error::other)
    }

    pub fn new(root: &str, segments: Vec<String>) -> Manifest {
        Manifest {
            format_version: super::schema::FORMAT_VERSION,
            root: root.to_string(),
            segments,
            generated_at: now_secs(),
            validated: Vec::new(),
            skipped: Vec::new(),
            walk: None,
        }
    }

    /// Empreinte (taille, mtime) actuelle d'un segment sur disque.
    pub fn stat_segment(project: &str, name: &str) -> std::io::Result<(u64, u64)> {
        let meta = std::fs::metadata(atlas_dir(project).join(name))?;
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_micros() as u64) // micro : deux écritures à la même seconde restent distinguables
            .unwrap_or(0);
        Ok((meta.len(), mtime))
    }

    /// Enregistre qu'un segment vient d'être validé (bytecheck) avec CETTE
    /// empreinte précise — voir `ValidatedSegment` et `Handle::open`.
    pub fn mark_validated(&mut self, name: &str, size: u64, mtime: u64) {
        self.validated.retain(|v| v.name != name);
        self.validated.push(ValidatedSegment { name: name.to_string(), size, mtime });
    }

    /// Vrai si `name` a déjà été validé avec EXACTEMENT cette empreinte
    /// (taille + mtime). Les segments ne sont jamais réécrits après coup (voir
    /// l'en-tête du fichier) : un nom est toujours associé aux mêmes octets,
    /// donc cette empreinte ne peut correspondre qu'au contenu déjà validé —
    /// sauf altération hors du contrôle de Cortex (disque corrompu), un risque
    /// qui existe de toute façon avec ou sans cache (mmap fait confiance au
    /// disque une fois la validation passée).
    pub fn is_validated(&self, name: &str, size: u64, mtime: u64) -> bool {
        self.validated.iter().any(|v| v.name == name && v.size == size && v.mtime == mtime)
    }

    /// Écriture ATOMIQUE : fichier temporaire puis renommage (jamais un
    /// manifeste à moitié écrit lu par un autre processus).
    pub fn save(&self, project: &str) -> std::io::Result<()> {
        let dir = atlas_dir(project);
        std::fs::create_dir_all(&dir)?;
        let final_path = manifest_path(project);
        let tmp_path = dir.join(format!("manifest.tmp-{}", std::process::id()));
        let json = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(&tmp_path, json)?;
        std::fs::rename(&tmp_path, &final_path)
    }

    pub fn segment_path(&self, project: &str, name: &str) -> PathBuf {
        atlas_dir(project).join(name)
    }

    /// Nettoie les segments qui ne sont plus listés dans un manifeste actuel,
    /// mais qui traînent encore sur disque (anciens segments remplacés). Best
    /// effort : un fichier encore mmap-é ailleurs échoue à la suppression sur
    /// Windows — c'est TOLÉRÉ (retenté à la prochaine passe), jamais fatal.
    pub fn clean_orphans(project: &str, active: &[String]) {
        let dir = atlas_dir(project);
        let Ok(entries) = std::fs::read_dir(&dir) else { return };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.ends_with(".atlas") || active.contains(&name) {
                continue;
            }
            let age =
                e.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| t.elapsed().ok()).map(|d| d.as_secs()).unwrap_or(u64::MAX);
            if age < ORPHAN_MIN_AGE_SECS {
                continue; // peut-être un segment d'un autre processus pas encore publié
            }
            let _ = std::fs::remove_file(e.path()); // échec toléré (verrouillé)
        }
    }
}
