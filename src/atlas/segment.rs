//! Lecture/écriture d'un segment : rkyv sur disque, ouvert par `mmap` (§4 —
//! "zéro décodage au chargement"). `OpenSegment` garde le mapping et expose la
//! vue archivée (zero-copy : `archived()` ne copie rien, ce sont des pointeurs
//! dans les octets mappés).

use super::schema::AtlasSegment;
use memmap2::Mmap;
use rkyv::AlignedVec;
use std::fs::File;
use std::path::Path;

/// Sérialise et écrit un segment NEUF (jamais un fichier existant — voir le
/// piège Windows documenté dans `manifest.rs`). Écriture-puis-renommage :
/// le fichier n'apparaît sous son nom final qu'une fois complet. Rend les
/// octets écrits (validables sans rouvrir le fichier).
pub fn write_segment(path: &Path, seg: &AtlasSegment) -> std::io::Result<AlignedVec> {
    let dbg = std::env::var("CORTEX_DEBUG_TIMING").is_ok();
    let t0 = std::time::Instant::now();
    let bytes: AlignedVec = rkyv::to_bytes::<_, 4096>(seg).map_err(|e| std::io::Error::other(format!("rkyv: {e}")))?;
    if dbg {
        eprintln!("[timing] rkyv serialize: {:.2}ms ({} octets)", t0.elapsed().as_secs_f64() * 1000.0, bytes.len());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let t1 = std::time::Instant::now();
    let tmp = path.with_extension("atlas.tmp");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, path)?;
    if dbg {
        eprintln!("[timing] fs write+rename: {:.2}ms", t1.elapsed().as_secs_f64() * 1000.0);
    }
    Ok(bytes)
}

/// Un segment ouvert par mmap. `mmap` doit rester en vie tant que `view()` est
/// utilisée — c'est garanti car `view()` emprunte `&self`.
pub struct OpenSegment {
    mmap: Mmap,
}

impl OpenSegment {
    pub fn open(path: &Path) -> std::io::Result<OpenSegment> {
        let file = File::open(path)?;
        // SAFETY: on ne mute jamais ce fichier après écriture (voir manifest.rs :
        // un segment n'est jamais réécrit, seulement remplacé par un NOM neuf).
        let mmap = unsafe { Mmap::map(&file)? };
        Ok(OpenSegment { mmap })
    }

    /// Vue archivée zero-copy, VALIDÉE (bytecheck) — coût O(taille) mais SANS
    /// allocation ni copie des données elles-mêmes, juste une passe de
    /// vérification des offsets/tailles pour la sécurité mémoire (mmap = des
    /// octets qui peuvent, en théorie, être corrompus sur disque). Appelée une
    /// fois à l'ouverture (voir `Handle::open`).
    pub fn view(&self) -> Result<&rkyv::Archived<AtlasSegment>, String> {
        rkyv::check_archived_root::<AtlasSegment>(&self.mmap[..]).map_err(|e| format!("segment invalide: {e}"))
    }

    /// Vue archivée SANS revalidation — chemin chaud (requête). Sûr ici : on
    /// n'ouvre jamais un segment sans l'avoir déjà validé une fois avec
    /// `view()` à l'ouverture (voir `Handle::open`), et un segment n'est
    /// JAMAIS modifié après écriture (nouveau fichier à chaque mise à jour).
    pub fn view_unchecked(&self) -> &rkyv::Archived<AtlasSegment> {
        unsafe { rkyv::archived_root::<AtlasSegment>(&self.mmap[..]) }
    }
}
