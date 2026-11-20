//! Cortex Web : stockage d'une source (manifeste des pages + sections), prototype feature `web`.
//!
//! Dossier `<root>/<source>/` : `source.json` (config), `pages.jsonl` (un `PageRecord` par URL),
//! `sections.jsonl` (une `StoredSection` par ligne), `crawl.state` (file du crawl), `.lock`
//! (un seul écrivain), et plus tard `index.bin` (module `index`). Voir `docs/WEB.md` §4.2.
//!
//! # Écriture et reprise après crash
//! * Les petits fichiers (`source.json`, `crawl.state`) et les compactations s'écrivent en
//!   fichier temporaire + `rename` : on voit l'ancien ou le nouveau contenu, jamais un mélange.
//! * `pages.jsonl` et `sections.jsonl` sont en AJOUT (un rewrite complet par page serait
//!   quadratique). L'atomicité d'un `put_page` vient d'une GÉNÉRATION : chaque `put_page` prend
//!   un numéro `g` (clé `"g"` en tête de chaque ligne), ajoute les sections, PUIS la ligne de
//!   page qui porte le même `g`. La ligne de page est le point de validation : au chargement,
//!   une section n'est vivante que si son `g` est celui de la dernière ligne de page de son
//!   URL. Un crash entre les deux laisse des sections orphelines (ignorées), l'ancienne version
//!   de la page reste intacte. Dernier enregistrement d'une URL gagne.
//! * Une ligne tronquée ou corrompue est ignorée et comptée (`StoreStats::corrupt_lines`) ;
//!   jamais de panique. Un écrivain répare une fin de fichier sans saut de ligne à l'ouverture.
//! * Une page dont le nombre de sections vivantes diffère de `record.sections` (écriture
//!   partielle après coupure de courant) est écartée : le crawl la refera.
//! * Pas de `fsync` par page (coût) : un crash du processus ne perd rien de validé ; une coupure
//!   de courant peut perdre la fin du fichier, jamais corrompre l'avant.

use super::{PageRecord, SourceConfig, StoredSection};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// Compactation au-delà de cette part de lignes obsolètes (et un minimum de lignes).
const COMPACT_MIN_LINES: usize = 64;
const STATE_MAGIC: &[u8; 4] = b"CXST";

// ───────────────────────── Nom de source ─────────────────────────

/// Nom de dossier sûr : `[a-z0-9._-]` seulement, jamais `..`, ni point en tête ou en queue,
/// ni nom réservé de Windows. Une source vide devient `source`.
pub fn sanitize_source(name: &str) -> String {
    let mut s = String::new();
    for c in name.trim().chars().flat_map(|c| c.to_lowercase()) {
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_' || c == '-' {
            s.push(c);
        } else {
            s.push('-');
        }
    }
    while s.contains("..") {
        s = s.replace("..", ".");
    }
    let edge = |c: char| c == '.' || c == '-' || c == '_';
    let mut s: String = s.trim_matches(edge).chars().take(64).collect();
    s = s.trim_matches(edge).to_string();
    if s.is_empty() {
        return "source".to_string();
    }
    let stem = s.split('.').next().unwrap_or("");
    let reserve = matches!(stem, "con" | "prn" | "aux" | "nul")
        || ((stem.starts_with("com") || stem.starts_with("lpt")) && stem.len() == 4 && stem.as_bytes()[3].is_ascii_digit());
    if reserve {
        s = format!("s-{s}");
    }
    s
}

// ───────────────────────── Verrou ─────────────────────────

static HELD: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Verrou d'écrivain : fichier `.lock` (pid + date) créé en mode « create_new » (atomique),
/// supprimé au `Drop`. Un verrou dont le processus est mort est périmé et repris.
struct LockGuard {
    path: PathBuf,
    key: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        if let Ok(mut h) = HELD.lock() {
            h.retain(|p| p != &self.key);
        }
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    use std::ffi::c_void;
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
        fn GetExitCodeProcess(h: *mut c_void, code: *mut u32) -> i32;
        fn CloseHandle(h: *mut c_void) -> i32;
    }
    // PROCESS_QUERY_LIMITED_INFORMATION ; ERROR_ACCESS_DENIED (5) = le processus existe.
    unsafe {
        let h = OpenProcess(0x1000, 0, pid);
        if h.is_null() {
            return io::Error::last_os_error().raw_os_error() == Some(5);
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(h, &mut code);
        CloseHandle(h);
        ok == 0 || code == 259 // STILL_ACTIVE
    }
}

#[cfg(not(windows))]
fn pid_alive(pid: u32) -> bool {
    if Path::new("/proc").is_dir() {
        return Path::new(&format!("/proc/{pid}")).exists();
    }
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(true)
}

fn lock_is_stale(path: &Path, key: &Path) -> bool {
    let content = fs::read_to_string(path).unwrap_or_default();
    let pid = content.split_whitespace().next().and_then(|t| t.parse::<u32>().ok());
    match pid {
        // Fichier vide ou illisible : un autre écrivain est peut-être en train de l'écrire.
        None => fs::metadata(path).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).map(|d| d.as_secs() > 10).unwrap_or(true),
        Some(p) if p == std::process::id() => !HELD.lock().map(|h| h.iter().any(|k| k == key)).unwrap_or(false),
        Some(p) => !pid_alive(p),
    }
}

fn acquire_lock(dir: &Path) -> io::Result<LockGuard> {
    let path = dir.join(".lock");
    let key = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    for _ in 0..3 {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                let _ = writeln!(f, "{}\n{}", std::process::id(), now_secs());
                if let Ok(mut h) = HELD.lock() {
                    h.push(key.clone());
                }
                return Ok(LockGuard { path, key });
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                if lock_is_stale(&path, &key) {
                    let _ = fs::remove_file(&path);
                    continue;
                }
                let who = fs::read_to_string(&path).unwrap_or_default();
                let pid = who.split_whitespace().next().unwrap_or("?");
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("source verrouillée par un autre écrivain (pid {pid}) : {}", path.display()),
                ));
            }
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(io::ErrorKind::WouldBlock, format!("verrou instable : {}", path.display())))
}

// ───────────────────────── Fichiers atomiques ─────────────────────────

/// Écrit `bytes` dans `path` de façon atomique : fichier temporaire voisin, `sync`, `rename`.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = tmp_path(path);
    let res = (|| {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(format!(".tmp{}", std::process::id()));
    path.with_file_name(name)
}

/// Si le fichier ne finit pas par un saut de ligne (crash en cours d'écriture) : complète la
/// dernière ligne si c'est un JSON valide, sinon la coupe. Renvoie vrai si le fichier a changé.
fn repair_tail(path: &Path) -> io::Result<bool> {
    let mut f = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let len = f.metadata()?.len();
    if len == 0 {
        return Ok(false);
    }
    f.seek(SeekFrom::Start(len - 1))?;
    let mut last = [0u8; 1];
    f.read_exact(&mut last)?;
    if last[0] == b'\n' {
        return Ok(false);
    }
    // Cherche le dernier saut de ligne en remontant par blocs.
    let mut start = len;
    let mut cut = 0u64;
    let mut buf = vec![0u8; 64 * 1024];
    while start > 0 {
        let from = start.saturating_sub(buf.len() as u64);
        let n = (start - from) as usize;
        f.seek(SeekFrom::Start(from))?;
        f.read_exact(&mut buf[..n])?;
        if let Some(i) = buf[..n].iter().rposition(|&b| b == b'\n') {
            cut = from + i as u64 + 1;
            break;
        }
        start = from;
    }
    // La dernière ligne est-elle un JSON complet (seul le saut de ligne a manqué) ?
    let mut tail = Vec::new();
    f.seek(SeekFrom::Start(cut))?;
    f.read_to_end(&mut tail)?;
    if serde_json::from_slice::<serde::de::IgnoredAny>(&tail).is_ok() {
        f.seek(SeekFrom::End(0))?;
        f.write_all(b"\n")?;
    } else {
        f.set_len(cut)?;
    }
    Ok(true)
}

// ───────────────────────── Lignes ─────────────────────────

#[derive(serde::Deserialize)]
struct Head {
    #[serde(default)]
    g: u64,
    #[serde(default)]
    page_url: String,
}

/// Sépare le préfixe `{"g":N,` d'une ligne et renvoie `(g, octets du JSON sans le préfixe, avec
/// une accolade ouvrante rétablie)`. Une ligne sans `g` (outil externe) vaut `g = 0`.
fn split_g(line: &[u8]) -> (u64, Vec<u8>) {
    const P: &[u8] = b"{\"g\":";
    if line.starts_with(P) {
        let mut i = P.len();
        let mut g: u64 = 0;
        let mut any = false;
        while i < line.len() && line[i].is_ascii_digit() {
            g = g.saturating_mul(10).saturating_add((line[i] - b'0') as u64);
            any = true;
            i += 1;
        }
        if any && i < line.len() && line[i] == b',' {
            let mut v = line[i..].to_vec();
            v[0] = b'{';
            return (g, v);
        }
    }
    (0, line.to_vec())
}

fn trim_eol(mut l: &[u8]) -> &[u8] {
    while let Some((&last, rest)) = l.split_last() {
        if last == b'\n' || last == b'\r' {
            l = rest;
        } else {
            break;
        }
    }
    l
}

fn section_line(g: u64, s: &StoredSection) -> io::Result<String> {
    let j = serde_json::to_string(s).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(format!("{{\"g\":{g},{}\n", &j[1..]))
}

fn page_line(g: u64, r: &PageRecord) -> io::Result<String> {
    let j = serde_json::to_string(r).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(format!("{{\"g\":{g},{}\n", &j[1..]))
}

fn tomb_line(g: u64, url: &str) -> String {
    format!("{{\"g\":{g},\"removed\":{}}}\n", serde_json::to_string(url).unwrap_or_else(|_| "\"\"".into()))
}

/// Lecture d'une ligne de `sections.jsonl` : `(g, section)`.
pub(crate) fn parse_section_line(line: &[u8]) -> Option<(u64, StoredSection)> {
    let line = trim_eol(line);
    if line.is_empty() {
        return None;
    }
    let (g, body) = split_g(line);
    serde_json::from_slice::<StoredSection>(&body).ok().map(|s| (g, s))
}

enum PageLine {
    Record(u64, PageRecord),
    Removed(u64, String),
}

fn parse_page_line(line: &[u8]) -> Option<PageLine> {
    let line = trim_eol(line);
    if line.is_empty() {
        return None;
    }
    let (g, body) = split_g(line);
    if body.starts_with(b"{\"removed\":") {
        #[derive(serde::Deserialize)]
        struct T {
            removed: String,
        }
        return serde_json::from_slice::<T>(&body).ok().map(|t| PageLine::Removed(g, t.removed));
    }
    serde_json::from_slice::<PageRecord>(&body).ok().map(|r| PageLine::Record(g, r))
}

fn each_line(path: &Path, mut f: impl FnMut(&[u8])) -> io::Result<()> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let mut r = BufReader::with_capacity(256 * 1024, file);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if r.read_until(b'\n', &mut buf)? == 0 {
            return Ok(());
        }
        f(&buf);
    }
}

// ───────────────────────── Store ─────────────────────────

struct Entry {
    record: PageRecord,
    g: u64,
}

/// Compteurs et tailles (`Store::stats`).
#[derive(Clone, Debug, Default)]
pub struct StoreStats {
    pub pages: u32,
    /// Sections vivantes.
    pub sections: u32,
    /// Lignes de `sections.jsonl` (vivantes + obsolètes).
    pub section_lines: usize,
    pub obsolete_section_lines: usize,
    pub page_lines: usize,
    pub obsolete_page_lines: usize,
    /// Lignes illisibles ignorées (chargement + lectures en flux).
    pub corrupt_lines: usize,
    /// Pages écartées parce que leurs sections étaient incomplètes (écriture interrompue).
    pub incomplete_pages: usize,
    /// Fins de fichier réparées à l'ouverture (écrivain).
    pub repaired_tails: usize,
    pub compactions: usize,
    pub sections_bytes: u64,
    pub pages_bytes: u64,
    pub next_generation: u64,
}

pub struct Store {
    dir: PathBuf,
    name: String,
    entries: HashMap<String, Entry>,
    next_gen: u64,
    section_lines: usize,
    live_section_lines: usize,
    page_lines: usize,
    corrupt: AtomicUsize,
    incomplete: usize,
    repaired: usize,
    compactions: usize,
    _lock: Option<LockGuard>,
}

impl Store {
    /// Ouvre (et crée) la source en ÉCRITURE : prend le verrou, répare la fin des fichiers,
    /// charge le manifeste. `root` = `cortex_home().join("web")` en production.
    pub fn open(root: &Path, source: &str) -> io::Result<Store> {
        Self::open_inner(root, source, true)
    }

    /// Ouvre en LECTURE seule (sans verrou, sans réparation, rien n'est créé si la source
    /// n'existe pas : `NotFound`). Sert à `find`, `read`, `list` pendant qu'un crawl écrit.
    pub fn open_read(root: &Path, source: &str) -> io::Result<Store> {
        Self::open_inner(root, source, false)
    }

    fn open_inner(root: &Path, source: &str, write: bool) -> io::Result<Store> {
        let name = sanitize_source(source);
        let dir = root.join(&name);
        let lock = if write {
            fs::create_dir_all(&dir)?;
            Some(acquire_lock(&dir)?)
        } else {
            if !dir.is_dir() {
                return Err(io::Error::new(io::ErrorKind::NotFound, format!("source inconnue : {name}")));
            }
            None
        };
        let mut s = Store {
            dir,
            name,
            entries: HashMap::new(),
            next_gen: 1,
            section_lines: 0,
            live_section_lines: 0,
            page_lines: 0,
            corrupt: AtomicUsize::new(0),
            incomplete: 0,
            repaired: 0,
            compactions: 0,
            _lock: lock,
        };
        if write {
            for p in [s.pages_path(), s.sections_path()] {
                if repair_tail(&p)? {
                    s.repaired += 1;
                }
            }
        }
        s.load()?;
        Ok(s)
    }

    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn dir(&self) -> &Path {
        &self.dir
    }
    pub fn pages_path(&self) -> PathBuf {
        self.dir.join("pages.jsonl")
    }
    pub fn sections_path(&self) -> PathBuf {
        self.dir.join("sections.jsonl")
    }
    pub fn config_path(&self) -> PathBuf {
        self.dir.join("source.json")
    }
    pub fn state_path(&self) -> PathBuf {
        self.dir.join("crawl.state")
    }
    pub fn index_path(&self) -> PathBuf {
        self.dir.join("index.bin")
    }

    fn load(&mut self) -> io::Result<()> {
        // 1. Manifeste des pages : le dernier enregistrement d'une URL gagne.
        let mut entries: HashMap<String, Entry> = HashMap::new();
        let mut page_lines = 0usize;
        let mut max_g = 0u64;
        let mut corrupt = 0usize;
        each_line(&self.pages_path(), |line| {
            if trim_eol(line).is_empty() {
                return;
            }
            match parse_page_line(line) {
                Some(PageLine::Record(g, r)) => {
                    page_lines += 1;
                    max_g = max_g.max(g);
                    entries.insert(r.url.clone(), Entry { record: r, g });
                }
                Some(PageLine::Removed(g, url)) => {
                    page_lines += 1;
                    max_g = max_g.max(g);
                    entries.remove(&url);
                }
                None => corrupt += 1,
            }
        })?;
        // 2. Sections : on compte les lignes vivantes par URL (lecture légère : `g` et `page_url`).
        let mut live: HashMap<String, u32> = HashMap::new();
        let mut total = 0usize;
        each_line(&self.sections_path(), |line| {
            let l = trim_eol(line);
            if l.is_empty() {
                return;
            }
            let (g, body) = split_g(l);
            match serde_json::from_slice::<Head>(&body) {
                Ok(h) if !h.page_url.is_empty() => {
                    total += 1;
                    max_g = max_g.max(g);
                    if entries.get(&h.page_url).map(|e| e.g == g).unwrap_or(false) {
                        *live.entry(h.page_url).or_insert(0) += 1;
                    }
                }
                _ => corrupt += 1,
            }
        })?;
        // 3. Une page dont les sections vivantes ne correspondent pas au compte annoncé est écartée.
        let mut incomplete = 0usize;
        entries.retain(|url, e| {
            let ok = live.get(url).copied().unwrap_or(0) == e.record.sections;
            if !ok {
                incomplete += 1;
            }
            ok
        });
        self.live_section_lines = entries.values().map(|e| e.record.sections as usize).sum();
        self.entries = entries;
        self.section_lines = total;
        self.page_lines = page_lines;
        self.next_gen = max_g + 1;
        self.incomplete = incomplete;
        self.corrupt.store(corrupt, Ordering::Relaxed);
        Ok(())
    }

    // ───── configuration et état du crawl ─────

    pub fn save_config(&self, cfg: &SourceConfig) -> io::Result<()> {
        let j = serde_json::to_vec_pretty(cfg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        write_atomic(&self.config_path(), &j)
    }

    /// `Ok(None)` si la source n'a pas encore de configuration ; `Err(InvalidData)` si elle est illisible.
    pub fn load_config(&self) -> io::Result<Option<SourceConfig>> {
        match fs::read(self.config_path()) {
            Ok(b) => serde_json::from_slice(&b).map(Some).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// File du crawl (octets opaques pour `store`) : écriture atomique, somme de contrôle blake3.
    pub fn save_state(&self, bytes: &[u8]) -> io::Result<()> {
        let mut out = Vec::with_capacity(bytes.len() + 36);
        out.extend_from_slice(STATE_MAGIC);
        out.extend_from_slice(blake3::hash(bytes).as_bytes());
        out.extend_from_slice(bytes);
        write_atomic(&self.state_path(), &out)
    }

    /// `Ok(None)` si absent OU abîmé (un état illisible n'empêche jamais de repartir).
    pub fn load_state(&self) -> io::Result<Option<Vec<u8>>> {
        let b = match fs::read(self.state_path()) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        if b.len() >= 36 && &b[..4] == STATE_MAGIC && blake3::hash(&b[36..]).as_bytes()[..] == b[4..36] {
            Ok(Some(b[36..].to_vec()))
        } else {
            self.corrupt.fetch_add(1, Ordering::Relaxed);
            Ok(None)
        }
    }

    // ───── pages ─────

    pub fn get_record(&self, url: &str) -> Option<PageRecord> {
        self.entries.get(url).map(|e| e.record.clone())
    }

    /// Tous les enregistrements vivants, triés par URL.
    pub fn records(&self) -> Vec<PageRecord> {
        let mut v: Vec<PageRecord> = self.entries.values().map(|e| e.record.clone()).collect();
        v.sort_by(|a, b| a.url.cmp(&b.url));
        v
    }

    /// Remplace la page `record.url` et TOUTES ses sections. `record.sections` est recalculé.
    pub fn put_page(&mut self, mut record: PageRecord, sections: Vec<StoredSection>) -> io::Result<()> {
        record.sections = sections.len() as u32;
        if !record.quality.is_finite() {
            record.quality = 0.0;
        }
        let g = self.next_gen;
        self.next_gen += 1;
        let mut buf = String::new();
        for s in &sections {
            // La section porte l'URL du manifeste : jamais d'orpheline par faute de l'appelant.
            if s.page_url != record.url {
                let mut s2 = s.clone();
                s2.page_url = record.url.clone();
                buf.push_str(&section_line(g, &s2)?);
            } else {
                buf.push_str(&section_line(g, s)?);
            }
        }
        self.append(&self.sections_path(), buf.as_bytes())?;
        self.append(&self.pages_path(), page_line(g, &record)?.as_bytes())?;
        let old = self.entries.insert(record.url.clone(), Entry { record, g });
        self.section_lines += sections.len();
        self.live_section_lines += sections.len();
        if let Some(o) = old {
            self.live_section_lines -= o.record.sections as usize;
        }
        self.page_lines += 1;
        self.maybe_compact()
    }

    /// 304 : met à jour la date de collecte sans réécrire les sections. Faux si la page est inconnue.
    pub fn mark_unchanged(&mut self, url: &str, fetched_at: u64) -> io::Result<bool> {
        let Some(e) = self.entries.get_mut(url) else { return Ok(false) };
        e.record.fetched_at = fetched_at;
        let line = page_line(e.g, &e.record)?;
        let path = self.pages_path();
        self.append(&path, line.as_bytes())?;
        self.page_lines += 1;
        self.maybe_compact()?;
        Ok(true)
    }

    /// Retire la page et ses sections (pierre tombale ; l'espace est repris à la compactation).
    pub fn remove_page(&mut self, url: &str) -> io::Result<bool> {
        if !self.entries.contains_key(url) {
            return Ok(false);
        }
        let g = self.next_gen;
        self.next_gen += 1;
        self.append(&self.pages_path(), tomb_line(g, url).as_bytes())?;
        if let Some(o) = self.entries.remove(url) {
            self.live_section_lines -= o.record.sections as usize;
        }
        self.page_lines += 1;
        self.maybe_compact()?;
        Ok(true)
    }

    /// Ajout en fin de fichier ; en cas d'échec le fichier est ramené à sa taille d'origine.
    fn append(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let mut f = OpenOptions::new().create(true).append(true).open(path)?;
        let before = f.metadata()?.len();
        if let Err(e) = f.write_all(bytes).and_then(|_| f.flush()) {
            let _ = f.set_len(before);
            return Err(e);
        }
        Ok(())
    }

    // ───── compactation ─────

    fn maybe_compact(&mut self) -> io::Result<()> {
        let obsolete_s = self.section_lines.saturating_sub(self.live_section_lines);
        if self.section_lines >= COMPACT_MIN_LINES && obsolete_s * 2 > self.section_lines {
            self.compact_sections()?;
        }
        let obsolete_p = self.page_lines.saturating_sub(self.entries.len());
        if self.page_lines >= COMPACT_MIN_LINES && obsolete_p * 2 > self.page_lines {
            self.compact_pages()?;
        }
        Ok(())
    }

    /// Réécrit `sections.jsonl` sans les lignes obsolètes (fichier temporaire + rename).
    pub fn compact_sections(&mut self) -> io::Result<()> {
        let path = self.sections_path();
        let tmp = tmp_path(&path);
        let mut kept = 0usize;
        let res = (|| -> io::Result<()> {
            let mut w = BufWriter::with_capacity(256 * 1024, File::create(&tmp)?);
            let mut werr: Option<io::Error> = None;
            each_line(&path, |line| {
                let l = trim_eol(line);
                if l.is_empty() || werr.is_some() {
                    return;
                }
                let (g, body) = split_g(l);
                if let Ok(h) = serde_json::from_slice::<Head>(&body) {
                    if self.entries.get(&h.page_url).map(|e| e.g == g).unwrap_or(false) {
                        if let Err(e) = w.write_all(l).and_then(|_| w.write_all(b"\n")) {
                            werr = Some(e);
                        }
                        kept += 1;
                    }
                }
            })?;
            if let Some(e) = werr {
                return Err(e);
            }
            let f = w.into_inner().map_err(|e| e.into_error())?;
            f.sync_all()?;
            drop(f);
            fs::rename(&tmp, &path)
        })();
        if let Err(e) = res {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        self.section_lines = kept;
        self.live_section_lines = kept;
        self.compactions += 1;
        Ok(())
    }

    /// Réécrit `pages.jsonl` : une ligne par page vivante, sans pierres tombales.
    pub fn compact_pages(&mut self) -> io::Result<()> {
        let mut urls: Vec<&String> = self.entries.keys().collect();
        urls.sort();
        let mut out = String::new();
        for u in urls {
            let e = &self.entries[u];
            out.push_str(&page_line(e.g, &e.record)?);
        }
        write_atomic(&self.pages_path(), out.as_bytes())?;
        self.page_lines = self.entries.len();
        self.compactions += 1;
        Ok(())
    }

    // ───── lecture des sections ─────

    /// Sections vivantes, en flux (jamais tout en mémoire), dans l'ordre du fichier.
    pub fn sections(&self) -> impl Iterator<Item = StoredSection> + '_ {
        self.sections_at().map(|(_, _, s)| s)
    }

    /// Comme `sections()` avec `(décalage, longueur de la ligne sans saut de ligne)` dans
    /// `sections.jsonl` : sert à l'index pour relire une section sans tout charger.
    pub fn sections_at(&self) -> SectionIter<'_> {
        SectionIter {
            store: self,
            reader: File::open(self.sections_path()).ok().map(|f| BufReader::with_capacity(256 * 1024, f)),
            pos: 0,
            buf: Vec::new(),
        }
    }

    /// Relit une section à `(offset, len)` ; `None` si le fichier a changé depuis.
    pub fn read_section_at(&self, offset: u64, len: u32) -> Option<StoredSection> {
        let mut f = File::open(self.sections_path()).ok()?;
        read_section_from(&mut f, offset, len)
    }

    /// Recherche linéaire d'une section par identifiant (repli : l'index fait mieux).
    pub fn find_section(&self, id: &str) -> Option<StoredSection> {
        self.sections().find(|s| s.id == id)
    }

    pub fn stats(&self) -> StoreStats {
        let size = |p: PathBuf| fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        StoreStats {
            pages: self.entries.len() as u32,
            sections: self.live_section_lines as u32,
            section_lines: self.section_lines,
            obsolete_section_lines: self.section_lines.saturating_sub(self.live_section_lines),
            page_lines: self.page_lines,
            obsolete_page_lines: self.page_lines.saturating_sub(self.entries.len()),
            corrupt_lines: self.corrupt.load(Ordering::Relaxed),
            incomplete_pages: self.incomplete,
            repaired_tails: self.repaired,
            compactions: self.compactions,
            sections_bytes: size(self.sections_path()),
            pages_bytes: size(self.pages_path()),
            next_generation: self.next_gen,
        }
    }

    /// Empreinte `[len sections, mtime sections, len pages, mtime pages]` (mtime en ns) :
    /// l'index sait ainsi s'il est périmé.
    pub fn fingerprint(&self) -> [u64; 4] {
        let f = |p: PathBuf| -> (u64, u64) {
            match fs::metadata(p) {
                Ok(m) => {
                    let t = m
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos() as u64)
                        .unwrap_or(0);
                    (m.len(), t)
                }
                Err(_) => (0, 0),
            }
        };
        let (sl, st) = f(self.sections_path());
        let (pl, pt) = f(self.pages_path());
        [sl, st, pl, pt]
    }
}

pub(crate) fn read_section_from(f: &mut File, offset: u64, len: u32) -> Option<StoredSection> {
    f.seek(SeekFrom::Start(offset)).ok()?;
    let mut buf = vec![0u8; len as usize];
    f.read_exact(&mut buf).ok()?;
    parse_section_line(&buf).map(|(_, s)| s)
}

/// Itérateur en flux sur les sections vivantes (`Store::sections_at`).
pub struct SectionIter<'a> {
    store: &'a Store,
    reader: Option<BufReader<File>>,
    pos: u64,
    buf: Vec<u8>,
}

impl Iterator for SectionIter<'_> {
    type Item = (u64, u32, StoredSection);

    fn next(&mut self) -> Option<Self::Item> {
        let r = self.reader.as_mut()?;
        loop {
            self.buf.clear();
            let n = r.read_until(b'\n', &mut self.buf).ok()?;
            if n == 0 {
                return None;
            }
            let offset = self.pos;
            self.pos += n as u64;
            let l = trim_eol(&self.buf);
            if l.is_empty() {
                continue;
            }
            let len = l.len() as u32;
            match parse_section_line(l) {
                Some((g, s)) => {
                    if self.store.entries.get(&s.page_url).map(|e| e.g == g).unwrap_or(false) {
                        return Some((offset, len, s));
                    }
                }
                None => {
                    self.store.corrupt.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}

// ───────────────────────── Liste des sources ─────────────────────────

/// `(nom, pages, sections)` de chaque source sous `root`, triées par nom. Ne lit que le
/// manifeste des pages (rapide) ; une source sans manifeste donne `(nom, 0, 0)`.
pub fn list_sources(root: &Path) -> Vec<(String, u32, u32)> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(root) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        let name = e.file_name().to_string_lossy().to_string();
        if !(p.join("source.json").exists() || p.join("pages.jsonl").exists() || p.join("sections.jsonl").exists()) {
            continue;
        }
        let mut recs: HashMap<String, u32> = HashMap::new();
        let _ = each_line(&p.join("pages.jsonl"), |line| match parse_page_line(line) {
            Some(PageLine::Record(_, r)) => {
                recs.insert(r.url, r.sections);
            }
            Some(PageLine::Removed(_, u)) => {
                recs.remove(&u);
            }
            None => {}
        });
        out.push((name, recs.len() as u32, recs.values().sum()));
    }
    out.sort();
    out
}

// ───────────────────────── Tests ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::Completeness;
    use std::sync::atomic::AtomicU32;

    static N: AtomicU32 = AtomicU32::new(0);

    /// Dossier temporaire unique, supprimé à la fin du test.
    struct Tmp(PathBuf);
    impl Tmp {
        fn new() -> Tmp {
            let p = std::env::temp_dir().join(format!("cortex-web-store-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            Tmp(p)
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn rec(url: &str) -> PageRecord {
        PageRecord {
            url: url.into(),
            canonical: None,
            status: 200,
            etag: Some("\"abc\"".into()),
            last_modified: None,
            raw_hash: "r".into(),
            content_hash: "c".into(),
            extractor_version: 1,
            fetched_at: 1000,
            completeness: Completeness::Complete,
            quality: 0.8,
            sections: 0,
            bytes: 100,
        }
    }

    fn sec(url: &str, anchor: &str, text: &str) -> StoredSection {
        StoredSection {
            id: format!("W:t/{}#{}", url.trim_start_matches("https://x/"), anchor),
            page_url: url.into(),
            page_title: "Titre".into(),
            anchor: anchor.into(),
            heading_path: vec!["Guide".into(), anchor.into()],
            level: 2,
            text: text.into(),
            code_blocks: 0,
            version: Some("1.0".into()),
            fetched_at: 1000,
            completeness: Completeness::Complete,
        }
    }

    #[test]
    fn nom_de_source_assaini() {
        assert_eq!(sanitize_source("Axum"), "axum");
        assert_eq!(sanitize_source("../../etc/passwd"), "etc-passwd");
        assert_eq!(sanitize_source("a/b\\c:d"), "a-b-c-d");
        assert_eq!(sanitize_source(".."), "source");
        assert_eq!(sanitize_source(""), "source");
        assert_eq!(sanitize_source("a..b...c"), "a.b.c");
        assert_eq!(sanitize_source("serde_json-1.0"), "serde_json-1.0");
        assert_eq!(sanitize_source("CON"), "s-con");
        assert_eq!(sanitize_source("nul.txt"), "s-nul.txt");
        assert!(!sanitize_source("é€ü").contains(".."));
        assert!(sanitize_source(&"x".repeat(500)).len() <= 64);
    }

    #[test]
    fn source_hors_racine_impossible() {
        let t = Tmp::new();
        let s = Store::open(&t.0, "../evil").unwrap();
        assert!(s.dir().starts_with(&t.0));
        assert_eq!(s.name(), "evil");
    }

    #[test]
    fn ouverture_cree_le_dossier_vide() {
        let t = Tmp::new();
        let s = Store::open(&t.0, "demo").unwrap();
        assert!(t.0.join("demo").is_dir());
        assert!(s.records().is_empty());
        assert_eq!(s.sections().count(), 0);
        assert_eq!(s.stats().pages, 0);
    }

    #[test]
    fn put_puis_get_record() {
        let t = Tmp::new();
        let mut s = Store::open(&t.0, "demo").unwrap();
        s.put_page(rec("https://x/a"), vec![sec("https://x/a", "intro", "bonjour"), sec("https://x/a", "usage", "salut")]).unwrap();
        let r = s.get_record("https://x/a").unwrap();
        assert_eq!(r.sections, 2);
        assert_eq!(r.etag.as_deref(), Some("\"abc\""));
        assert!(s.get_record("https://x/zzz").is_none());
        assert_eq!(s.sections().count(), 2);
    }

    #[test]
    fn rechargement_identique() {
        let t = Tmp::new();
        {
            let mut s = Store::open(&t.0, "demo").unwrap();
            s.put_page(rec("https://x/a"), vec![sec("https://x/a", "intro", "un")]).unwrap();
            s.put_page(rec("https://x/b"), vec![sec("https://x/b", "intro", "deux"), sec("https://x/b", "fin", "trois")]).unwrap();
        }
        let s = Store::open(&t.0, "demo").unwrap();
        assert_eq!(s.records().len(), 2);
        let texts: Vec<String> = s.sections().map(|x| x.text).collect();
        assert_eq!(texts, vec!["un", "deux", "trois"]);
        assert_eq!(s.stats().corrupt_lines, 0);
    }

    #[test]
    fn remplacement_de_page_remplace_les_sections() {
        let t = Tmp::new();
        let mut s = Store::open(&t.0, "demo").unwrap();
        s.put_page(rec("https://x/a"), vec![sec("https://x/a", "vieux1", "v1"), sec("https://x/a", "vieux2", "v1")]).unwrap();
        s.put_page(rec("https://x/a"), vec![sec("https://x/a", "neuf", "v2")]).unwrap();
        let v: Vec<StoredSection> = s.sections().collect();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].anchor, "neuf");
        assert_eq!(s.get_record("https://x/a").unwrap().sections, 1);
        // Après rechargement : même résultat (le dernier gagne).
        drop(s);
        let s = Store::open(&t.0, "demo").unwrap();
        let v: Vec<StoredSection> = s.sections().collect();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].text, "v2");
    }

    #[test]
    fn mark_unchanged_ne_reecrit_pas_les_sections() {
        let t = Tmp::new();
        let mut s = Store::open(&t.0, "demo").unwrap();
        s.put_page(rec("https://x/a"), vec![sec("https://x/a", "i", "texte")]).unwrap();
        let avant = fs::metadata(s.sections_path()).unwrap().len();
        assert!(s.mark_unchanged("https://x/a", 5555).unwrap());
        assert!(!s.mark_unchanged("https://x/inconnue", 1).unwrap());
        assert_eq!(fs::metadata(s.sections_path()).unwrap().len(), avant);
        assert_eq!(s.get_record("https://x/a").unwrap().fetched_at, 5555);
        drop(s);
        let s = Store::open(&t.0, "demo").unwrap();
        assert_eq!(s.get_record("https://x/a").unwrap().fetched_at, 5555);
        assert_eq!(s.sections().count(), 1);
    }

    #[test]
    fn remove_page_retire_page_et_sections() {
        let t = Tmp::new();
        let mut s = Store::open(&t.0, "demo").unwrap();
        s.put_page(rec("https://x/a"), vec![sec("https://x/a", "i", "a")]).unwrap();
        s.put_page(rec("https://x/b"), vec![sec("https://x/b", "i", "b")]).unwrap();
        assert!(s.remove_page("https://x/a").unwrap());
        assert!(!s.remove_page("https://x/a").unwrap());
        assert!(s.get_record("https://x/a").is_none());
        assert_eq!(s.sections().count(), 1);
        drop(s);
        let s = Store::open(&t.0, "demo").unwrap();
        assert_eq!(s.records().len(), 1);
        assert_eq!(s.sections().next().unwrap().text, "b");
    }

    #[test]
    fn page_rajoutee_apres_suppression() {
        let t = Tmp::new();
        let mut s = Store::open(&t.0, "demo").unwrap();
        s.put_page(rec("https://x/a"), vec![sec("https://x/a", "i", "ancien")]).unwrap();
        s.remove_page("https://x/a").unwrap();
        s.put_page(rec("https://x/a"), vec![sec("https://x/a", "i", "nouveau")]).unwrap();
        drop(s);
        let s = Store::open(&t.0, "demo").unwrap();
        let v: Vec<String> = s.sections().map(|x| x.text).collect();
        assert_eq!(v, vec!["nouveau"]);
    }

    #[test]
    fn config_aller_retour_et_absente() {
        let t = Tmp::new();
        let s = Store::open(&t.0, "demo").unwrap();
        assert!(s.load_config().unwrap().is_none());
        let cfg = SourceConfig {
            name: "demo".into(),
            seeds: vec!["https://x/".into()],
            allowed_hosts: vec!["x".into()],
            path_prefix: "/docs/".into(),
            version: Some("2.1".into()),
            max_pages: 10,
            max_depth: 3,
            max_total_bytes: 1 << 20,
            max_seconds: 60,
            include_paths: vec![],
            exclude_paths: vec![],
        };
        s.save_config(&cfg).unwrap();
        let back = s.load_config().unwrap().unwrap();
        assert_eq!(back.seeds, cfg.seeds);
        assert_eq!(back.version.as_deref(), Some("2.1"));
        assert_eq!(back.max_pages, 10);
        // Fichier corrompu : erreur explicite, pas de panique.
        fs::write(s.config_path(), b"{ pas du json").unwrap();
        assert!(s.load_config().is_err());
    }

    #[test]
    fn etat_du_crawl_atomique_et_verifie() {
        let t = Tmp::new();
        let s = Store::open(&t.0, "demo").unwrap();
        assert!(s.load_state().unwrap().is_none());
        s.save_state(b"file: a b c").unwrap();
        assert_eq!(s.load_state().unwrap().unwrap(), b"file: a b c");
        s.save_state(b"v2").unwrap();
        assert_eq!(s.load_state().unwrap().unwrap(), b"v2");
        // Aucun fichier temporaire ne traîne.
        let restes: Vec<_> =
            fs::read_dir(s.dir()).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().contains(".tmp")).collect();
        assert!(restes.is_empty());
        // État abîmé : ignoré, compté, jamais de panique.
        let mut b = fs::read(s.state_path()).unwrap();
        let n = b.len();
        b[n - 1] ^= 0xff;
        fs::write(s.state_path(), b).unwrap();
        assert!(s.load_state().unwrap().is_none());
        assert!(s.stats().corrupt_lines >= 1);
        fs::write(s.state_path(), b"").unwrap();
        assert!(s.load_state().unwrap().is_none());
    }

    #[test]
    fn crash_ligne_de_sections_tronquee() {
        let t = Tmp::new();
        {
            let mut s = Store::open(&t.0, "demo").unwrap();
            s.put_page(rec("https://x/a"), vec![sec("https://x/a", "i", "a1")]).unwrap();
            s.put_page(rec("https://x/b"), vec![sec("https://x/b", "i", "b1"), sec("https://x/b", "j", "b2 texte assez long pour couper")])
                .unwrap();
        }
        let p = t.0.join("demo").join("sections.jsonl");
        let len = fs::metadata(&p).unwrap().len();
        let f = OpenOptions::new().write(true).open(&p).unwrap();
        f.set_len(len - 30).unwrap(); // au milieu de la dernière ligne
        drop(f);
        let mut s = Store::open(&t.0, "demo").unwrap();
        // La page b est incomplète : écartée ; la page a est intacte ; pas de panique.
        assert!(s.get_record("https://x/b").is_none());
        assert!(s.get_record("https://x/a").is_some());
        let st = s.stats();
        assert!(st.incomplete_pages >= 1);
        assert!(st.repaired_tails >= 1);
        assert_eq!(s.sections().count(), 1);
        // Le fichier reste exploitable : on peut réécrire la page et recharger.
        s.put_page(rec("https://x/b"), vec![sec("https://x/b", "i", "b1")]).unwrap();
        drop(s);
        let s = Store::open(&t.0, "demo").unwrap();
        assert_eq!(s.records().len(), 2);
        assert_eq!(s.sections().count(), 2);
        assert_eq!(s.stats().corrupt_lines, 0);
    }

    #[test]
    fn crash_entre_sections_et_page_garde_l_ancienne_version() {
        let t = Tmp::new();
        {
            let mut s = Store::open(&t.0, "demo").unwrap();
            s.put_page(rec("https://x/a"), vec![sec("https://x/a", "i", "ancienne")]).unwrap();
        }
        // Simule un crash : de nouvelles sections ajoutées, mais pas la ligne de page.
        let orphelines = section_line(99, &sec("https://x/a", "neuf", "orpheline")).unwrap();
        let mut f = OpenOptions::new().append(true).open(t.0.join("demo").join("sections.jsonl")).unwrap();
        f.write_all(orphelines.as_bytes()).unwrap();
        drop(f);
        let s = Store::open(&t.0, "demo").unwrap();
        let v: Vec<String> = s.sections().map(|x| x.text).collect();
        assert_eq!(v, vec!["ancienne"]);
        assert_eq!(s.stats().obsolete_section_lines, 1);
    }

    #[test]
    fn lignes_corrompues_ignorees_et_comptees() {
        let t = Tmp::new();
        {
            let mut s = Store::open(&t.0, "demo").unwrap();
            s.put_page(rec("https://x/a"), vec![sec("https://x/a", "i", "a")]).unwrap();
        }
        let d = t.0.join("demo");
        let mut f = OpenOptions::new().append(true).open(d.join("sections.jsonl")).unwrap();
        f.write_all(b"ceci n'est pas du json\n{\"g\":1,\"id\":\n\xff\xfe\n").unwrap();
        drop(f);
        let mut f = OpenOptions::new().append(true).open(d.join("pages.jsonl")).unwrap();
        f.write_all(b"{\"url\":\n[]\n").unwrap();
        drop(f);
        let s = Store::open(&t.0, "demo").unwrap();
        assert_eq!(s.records().len(), 1);
        assert_eq!(s.sections().count(), 1);
        assert!(s.stats().corrupt_lines >= 4, "{:?}", s.stats());
    }

    #[test]
    fn fin_de_fichier_sans_saut_de_ligne_complete() {
        let t = Tmp::new();
        {
            let mut s = Store::open(&t.0, "demo").unwrap();
            s.put_page(rec("https://x/a"), vec![sec("https://x/a", "i", "a")]).unwrap();
        }
        // Retire seulement le dernier saut de ligne de pages.jsonl : la ligne est complète.
        let p = t.0.join("demo").join("pages.jsonl");
        let b = fs::read(&p).unwrap();
        fs::write(&p, &b[..b.len() - 1]).unwrap();
        let mut s = Store::open(&t.0, "demo").unwrap();
        assert!(s.get_record("https://x/a").is_some());
        s.put_page(rec("https://x/b"), vec![sec("https://x/b", "i", "b")]).unwrap();
        drop(s);
        let s = Store::open(&t.0, "demo").unwrap();
        assert_eq!(s.records().len(), 2);
        assert_eq!(s.stats().corrupt_lines, 0);
    }

    #[test]
    fn deux_ecrivains_interdits_puis_verrou_libere() {
        let t = Tmp::new();
        let s1 = Store::open(&t.0, "demo").unwrap();
        let e = Store::open(&t.0, "demo").err().expect("le second écrivain doit échouer");
        assert_eq!(e.kind(), io::ErrorKind::WouldBlock);
        // La lecture seule reste possible pendant l'écriture.
        assert!(Store::open_read(&t.0, "demo").is_ok());
        assert!(t.0.join("demo").join(".lock").exists());
        drop(s1);
        assert!(!t.0.join("demo").join(".lock").exists());
        assert!(Store::open(&t.0, "demo").is_ok());
    }

    #[test]
    fn verrou_perime_repris() {
        let t = Tmp::new();
        let d = t.0.join("demo");
        fs::create_dir_all(&d).unwrap();
        // Un pid qui n'existe pas.
        fs::write(d.join(".lock"), "3999999999\n1\n").unwrap();
        let s = Store::open(&t.0, "demo").unwrap();
        let c = fs::read_to_string(d.join(".lock")).unwrap();
        assert!(c.starts_with(&std::process::id().to_string()));
        drop(s);
        // Verrou laissé par NOTRE pid mais sans Store vivant (crash puis relance) : périmé aussi.
        fs::write(d.join(".lock"), format!("{}\n1\n", std::process::id())).unwrap();
        assert!(Store::open(&t.0, "demo").is_ok());
    }

    #[test]
    fn open_read_source_inconnue_ne_cree_rien() {
        let t = Tmp::new();
        let e = Store::open_read(&t.0, "fantome").err().unwrap();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        assert!(!t.0.join("fantome").exists());
    }

    #[test]
    fn compactation_des_sections_obsoletes() {
        let t = Tmp::new();
        let mut s = Store::open(&t.0, "demo").unwrap();
        // Une page réécrite 100 fois : > 50 % de lignes obsolètes => compactation automatique.
        for i in 0..100 {
            s.put_page(rec("https://x/a"), vec![sec("https://x/a", "i", &format!("version {i}")), sec("https://x/a", "j", "stable")])
                .unwrap();
        }
        let st = s.stats();
        assert!(st.compactions >= 1, "{st:?}");
        assert!(st.section_lines < 100, "{st:?}");
        let v: Vec<String> = s.sections().map(|x| x.text).collect();
        assert_eq!(v, vec!["version 99", "stable"]);
        drop(s);
        let s = Store::open(&t.0, "demo").unwrap();
        assert_eq!(s.sections().count(), 2);
        assert_eq!(s.stats().corrupt_lines, 0);
        // Les pages aussi sont compactées (pierres tombales et anciennes lignes).
        assert!(s.stats().page_lines < 100);
    }

    #[test]
    fn compactation_conserve_les_autres_pages() {
        let t = Tmp::new();
        let mut s = Store::open(&t.0, "demo").unwrap();
        for i in 0..10 {
            s.put_page(rec(&format!("https://x/p{i}")), vec![sec(&format!("https://x/p{i}"), "i", &format!("page {i}"))]).unwrap();
        }
        for i in 0..200 {
            s.put_page(rec("https://x/chaude"), vec![sec("https://x/chaude", "i", &format!("v{i}"))]).unwrap();
        }
        assert!(s.stats().compactions >= 1);
        assert_eq!(s.records().len(), 11);
        assert_eq!(s.sections().count(), 11);
        let mut textes: Vec<String> = s.sections().map(|x| x.text).collect();
        textes.sort();
        assert!(textes.contains(&"page 7".to_string()));
        assert!(textes.contains(&"v199".to_string()));
    }

    #[test]
    fn list_sources_compte_pages_et_sections() {
        let t = Tmp::new();
        {
            let mut a = Store::open(&t.0, "alpha").unwrap();
            a.put_page(rec("https://x/a"), vec![sec("https://x/a", "i", "1"), sec("https://x/a", "j", "2")]).unwrap();
            a.put_page(rec("https://x/b"), vec![sec("https://x/b", "i", "3")]).unwrap();
            let mut b = Store::open(&t.0, "beta").unwrap();
            b.put_page(rec("https://y/a"), vec![sec("https://y/a", "i", "4")]).unwrap();
            b.remove_page("https://y/a").unwrap();
        }
        fs::create_dir_all(t.0.join("dossier-etranger")).unwrap(); // pas une source
        let l = list_sources(&t.0);
        assert_eq!(l, vec![("alpha".to_string(), 2, 3), ("beta".to_string(), 0, 0)]);
        assert!(list_sources(&t.0.join("inexistant")).is_empty());
    }

    #[test]
    fn sections_en_flux_avec_decalages() {
        let t = Tmp::new();
        let mut s = Store::open(&t.0, "demo").unwrap();
        s.put_page(rec("https://x/a"), vec![sec("https://x/a", "i", "premier"), sec("https://x/a", "j", "second é accentué")]).unwrap();
        let v: Vec<(u64, u32, StoredSection)> = s.sections_at().collect();
        assert_eq!(v.len(), 2);
        for (off, len, sec) in &v {
            let back = s.read_section_at(*off, *len).unwrap();
            assert_eq!(back.id, sec.id);
            assert_eq!(back.text, sec.text);
        }
        assert!(s.read_section_at(7, 5).is_none());
        assert_eq!(s.find_section(&v[1].2.id).unwrap().text, "second é accentué");
        assert!(s.find_section("W:rien").is_none());
    }

    #[test]
    fn la_section_prend_l_url_de_la_page() {
        let t = Tmp::new();
        let mut s = Store::open(&t.0, "demo").unwrap();
        let mut x = sec("https://x/AUTRE", "i", "texte");
        x.page_url = "https://x/AUTRE".into();
        s.put_page(rec("https://x/a"), vec![x]).unwrap();
        let v: Vec<StoredSection> = s.sections().collect();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].page_url, "https://x/a");
    }

    #[test]
    fn qualite_non_finie_et_unicode() {
        let t = Tmp::new();
        let mut s = Store::open(&t.0, "demo").unwrap();
        let mut r = rec("https://x/é?q=1&b=\"2\"");
        r.quality = f32::NAN;
        s.put_page(r, vec![sec("https://x/é?q=1&b=\"2\"", "i", "ligne1\nligne2\t\"guillemets\" \\ 日本語 😀")]).unwrap();
        drop(s);
        let s = Store::open(&t.0, "demo").unwrap();
        assert_eq!(s.get_record("https://x/é?q=1&b=\"2\"").unwrap().quality, 0.0);
        assert_eq!(s.sections().next().unwrap().text, "ligne1\nligne2\t\"guillemets\" \\ 日本語 😀");
    }

    #[test]
    fn statistiques_coherentes() {
        let t = Tmp::new();
        let mut s = Store::open(&t.0, "demo").unwrap();
        s.put_page(rec("https://x/a"), vec![sec("https://x/a", "i", "1"), sec("https://x/a", "j", "2")]).unwrap();
        s.put_page(rec("https://x/a"), vec![sec("https://x/a", "k", "3")]).unwrap();
        let st = s.stats();
        assert_eq!(st.pages, 1);
        assert_eq!(st.sections, 1);
        assert_eq!(st.section_lines, 3);
        assert_eq!(st.obsolete_section_lines, 2);
        assert_eq!(st.page_lines, 2);
        assert_eq!(st.obsolete_page_lines, 1);
        assert!(st.sections_bytes > 0 && st.pages_bytes > 0);
        assert!(st.next_generation >= 3);
    }

    #[test]
    fn lecture_seule_voit_les_pages_ecrites() {
        let t = Tmp::new();
        let mut w = Store::open(&t.0, "demo").unwrap();
        w.put_page(rec("https://x/a"), vec![sec("https://x/a", "i", "visible")]).unwrap();
        let r = Store::open_read(&t.0, "demo").unwrap();
        assert_eq!(r.sections().next().unwrap().text, "visible");
        assert_eq!(r.records().len(), 1);
    }
}
