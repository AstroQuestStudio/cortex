//! Cortex Web : module `resolve` (feature `web`, voir docs/WEB.md §3 D7).
//!
//! Dit quelle VERSION EXACTE d'une bibliothèque le projet utilise (lockfiles) et OÙ sont ses
//! sources installées (niveau T0 : hors ligne, zéro octet réseau). `docs_candidates` ne fait
//! que LISTER les URL que le Fetcher récupérera plus tard (T1/T2) : aucune requête ici.
//!
//! Analyse tolérante écrite à la main : pas de parseur TOML/YAML complet. JSON passe par
//! serde_json. Un fichier tronqué ou illisible ne fait jamais paniquer : il ne donne rien.

use serde_json::Value;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Plafond de lecture par fichier (verrouillage ou manifeste) : au-delà, seul le début est lu.
pub const MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;
/// Profondeur du scan : racine = 0, sous-dossiers = 1, petits-enfants = 2 (monorepo).
const MAX_DEPTH: usize = 2;
/// Dossiers jamais scannés (artefacts, dépendances installées, environnements Python).
const IGNORED_DIRS: &[&str] = &["node_modules", "target", "_agents", "venv", "vendor"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Ecosystem {
    Cargo,
    Npm,
    Pypi,
    Go,
}

/// Une dépendance telle que le lockfile la fixe. Une seule entrée par (lockfile, nom) :
/// la version retenue est celle du lockfile (hoistée npm, sinon la plus haute) ; les autres
/// versions présentes sont dans `alternatives`, triées de la plus haute à la plus basse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dep {
    pub name: String,
    pub version: String,
    pub ecosystem: Ecosystem,
    pub direct: bool,
    pub lockfile: PathBuf,
    pub alternatives: Vec<String>,
}

/// Résultat de la vérification des sources installées (lecture de la version dans le dossier).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstalledCheck {
    Found(PathBuf),
    VersionMismatch { lock: String, found: String, path: PathBuf },
    Missing,
}

/// URL candidate pour un niveau de documentation (T1 = méta/dépôt, T2 = site versionné).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocSource {
    pub tier: u8,
    pub kind: &'static str,
    pub url: String,
    pub versioned: bool,
}

/// Une ligne de lockfile, avant agrégation.
#[derive(Clone, Debug)]
struct Raw {
    name: String,
    version: String,
    /// Marqué directe par le format lui-même (requirements.txt, go.mod sans `// indirect`).
    direct: bool,
    /// Entrée à la racine de l'arbre installé (npm `node_modules/<nom>`).
    hoisted: bool,
}

/// Racines d'installation de l'environnement (injectables, pour les tests).
#[derive(Clone, Debug, Default)]
struct Env {
    cargo_home: Option<PathBuf>,
    gomodcache: Option<PathBuf>,
}

impl Env {
    fn from_process() -> Env {
        let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from);
        let cargo_home = std::env::var_os("CARGO_HOME").map(PathBuf::from).or_else(|| home.as_ref().map(|h| h.join(".cargo")));
        let gopath = std::env::var_os("GOPATH").and_then(|v| std::env::split_paths(&v).next());
        let gomodcache = std::env::var_os("GOMODCACHE")
            .map(PathBuf::from)
            .or_else(|| gopath.or_else(|| home.as_ref().map(|h| h.join("go"))).map(|g| g.join("pkg").join("mod")));
        Env { cargo_home, gomodcache }
    }
}

// ───────────────────────── API publique ─────────────────────────

/// Dépendances figées par les lockfiles de `root` et de ses sous-dossiers (profondeur ≤ 2).
pub fn detect_deps(root: &Path) -> Vec<Dep> {
    let dirs = scan_dirs(root);
    // Noms déclarés par chaque Cargo.toml scanné (un workspace déclare ses membres en sous-dossiers).
    let cargo_decl: Vec<(PathBuf, Vec<String>)> = dirs
        .iter()
        .filter_map(|d| {
            let t = read_capped(&d.join("Cargo.toml"), MAX_FILE_BYTES)?;
            Some((d.clone(), cargo_toml_deps(&t)))
        })
        .collect();
    let mut out = Vec::new();
    for dir in &dirs {
        let p = dir.join("Cargo.lock");
        if let Some(t) = read_capped(&p, MAX_FILE_BYTES) {
            let declared = cargo_decl
                .iter()
                .filter(|(d, _)| d.starts_with(dir))
                .flat_map(|(_, ns)| ns.iter().map(|n| norm_name(Ecosystem::Cargo, n)))
                .collect();
            out.extend(aggregate(Ecosystem::Cargo, &p, parse_cargo_lock(&t), &declared));
        }
        let npm = json_declared(dir);
        let p = dir.join("package-lock.json");
        if let Some(t) = read_capped(&p, MAX_FILE_BYTES) {
            out.extend(aggregate(Ecosystem::Npm, &p, parse_npm_lock(&t), &npm));
        }
        let p = dir.join("pnpm-lock.yaml");
        if let Some(t) = read_capped(&p, MAX_FILE_BYTES) {
            out.extend(aggregate(Ecosystem::Npm, &p, parse_pnpm(&t), &npm));
        }
        let p = dir.join("yarn.lock");
        if let Some(t) = read_capped(&p, MAX_FILE_BYTES) {
            out.extend(aggregate(Ecosystem::Npm, &p, parse_yarn(&t), &npm));
        }
        let py = pyproject_declared(dir);
        let p = dir.join("poetry.lock");
        if let Some(t) = read_capped(&p, MAX_FILE_BYTES) {
            let raws = toml_package_blocks(&t).iter().filter_map(raw_from_block).collect();
            out.extend(aggregate(Ecosystem::Pypi, &p, raws, &py));
        }
        let p = dir.join("uv.lock");
        if let Some(t) = read_capped(&p, MAX_FILE_BYTES) {
            let raws = toml_package_blocks(&t).iter().filter(|b| !is_local_source(b)).filter_map(raw_from_block).collect();
            out.extend(aggregate(Ecosystem::Pypi, &p, raws, &py));
        }
        let p = dir.join("Pipfile.lock");
        if let Some(t) = read_capped(&p, MAX_FILE_BYTES) {
            let declared = read_capped(&dir.join("Pipfile"), MAX_FILE_BYTES)
                .map(|t| names_set(Ecosystem::Pypi, toml_keys_in(&t, &["packages", "dev-packages"])))
                .unwrap_or_default();
            out.extend(aggregate(Ecosystem::Pypi, &p, parse_pipfile_lock(&t), &declared));
        }
        if let Ok(rd) = fs::read_dir(dir) {
            for e in rd.flatten() {
                let fname = e.file_name().to_string_lossy().into_owned();
                if fname.starts_with("requirements") && fname.ends_with(".txt") {
                    let p = e.path();
                    if let Some(t) = read_capped(&p, MAX_FILE_BYTES) {
                        out.extend(aggregate(Ecosystem::Pypi, &p, parse_requirements(&t), &BTreeSet::new()));
                    }
                }
            }
        }
        let p = dir.join("go.mod");
        if let Some(t) = read_capped(&p, MAX_FILE_BYTES) {
            out.extend(aggregate(Ecosystem::Go, &p, parse_go_mod(&t), &BTreeSet::new()));
        }
    }
    out.sort_by(|a, b| a.lockfile.cmp(&b.lockfile).then_with(|| a.name.cmp(&b.name)));
    out
}

/// Version lockée de `name` dans `root` : dépendance directe d'abord, sinon la plus haute ;
/// les autres versions (même écosystème) vont dans `alternatives`.
pub fn installed_version(root: &Path, name: &str) -> Option<Dep> {
    pick_dep(detect_deps(root), name)
}

/// Vérifie que la version lockée est bien celle présente sur disque (T0, hors ligne).
pub fn check_installed(root: &Path, dep: &Dep) -> InstalledCheck {
    check_installed_with(root, dep, &Env::from_process())
}

/// Dossier des sources installées, seulement si la version concorde (ou n'est pas vérifiable).
pub fn locate_installed(root: &Path, dep: &Dep) -> Option<PathBuf> {
    match check_installed(root, dep) {
        InstalledCheck::Found(p) => Some(p),
        _ => None,
    }
}

/// URL candidates à récupérer plus tard par le Fetcher. Aucune requête ici.
pub fn docs_candidates(dep: &Dep) -> Vec<DocSource> {
    let (n, v) = (dep.name.as_str(), dep.version.as_str());
    match dep.ecosystem {
        Ecosystem::Cargo => vec![
            DocSource { tier: 2, kind: "docs.rs", url: format!("https://docs.rs/{n}/{v}/{}/", n.replace('-', "_")), versioned: true },
            DocSource { tier: 1, kind: "crates.io-meta", url: format!("https://crates.io/api/v1/crates/{n}/{v}"), versioned: true },
        ],
        Ecosystem::Npm => vec![DocSource {
            tier: 1,
            kind: "npm-meta",
            url: format!("https://registry.npmjs.org/{}/{v}", n.replace('/', "%2F")),
            versioned: true,
        }],
        Ecosystem::Pypi => {
            vec![DocSource { tier: 1, kind: "pypi-meta", url: format!("https://pypi.org/pypi/{n}/{v}/json"), versioned: true }]
        }
        Ecosystem::Go => vec![DocSource { tier: 2, kind: "pkg.go.dev", url: format!("https://pkg.go.dev/{n}@{v}"), versioned: true }],
    }
}

// ───────────────────────── Scan et agrégation ─────────────────────────

fn scan_dirs(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut frontier = vec![root.to_path_buf()];
    for depth in 0..=MAX_DEPTH {
        let mut next = Vec::new();
        for dir in &frontier {
            out.push(dir.clone());
            if depth == MAX_DEPTH {
                continue;
            }
            let Ok(rd) = fs::read_dir(dir) else { continue };
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') || IGNORED_DIRS.contains(&name.as_str()) {
                    continue;
                }
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    next.push(e.path());
                }
            }
        }
        frontier = next;
    }
    out
}

/// Regroupe les lignes par nom normalisé ; version = hoistée, sinon la plus haute.
fn aggregate(eco: Ecosystem, lockfile: &Path, raws: Vec<Raw>, declared: &BTreeSet<String>) -> Vec<Dep> {
    let mut groups: BTreeMap<String, Vec<Raw>> = BTreeMap::new();
    for r in raws.into_iter().filter(|r| !r.name.is_empty() && !r.version.is_empty()) {
        groups.entry(norm_name(eco, &r.name)).or_default().push(r);
    }
    groups
        .into_iter()
        .map(|(key, group)| {
            let mut versions: Vec<String> = Vec::new();
            for r in &group {
                if !versions.contains(&r.version) {
                    versions.push(r.version.clone());
                }
            }
            versions.sort_by(|a, b| cmp_version(b, a));
            let chosen = group.iter().find(|r| r.hoisted);
            let name = chosen.unwrap_or(&group[0]).name.clone();
            let version = chosen.map(|r| r.version.clone()).unwrap_or_else(|| versions[0].clone());
            let direct = group.iter().any(|r| r.direct) || declared.contains(&key);
            let alternatives = versions.into_iter().filter(|v| *v != version).collect();
            Dep { name, version, ecosystem: eco, direct, lockfile: lockfile.to_path_buf(), alternatives }
        })
        .collect()
}

fn pick_dep(deps: Vec<Dep>, name: &str) -> Option<Dep> {
    let matching: Vec<Dep> = deps.into_iter().filter(|d| norm_name(d.ecosystem, &d.name) == norm_name(d.ecosystem, name)).collect();
    let mut best = matching.iter().max_by(|a, b| a.direct.cmp(&b.direct).then_with(|| cmp_version(&a.version, &b.version)))?.clone();
    let mut alts: Vec<String> = Vec::new();
    for d in matching.iter().filter(|d| d.ecosystem == best.ecosystem) {
        for v in std::iter::once(&d.version).chain(d.alternatives.iter()) {
            if *v != best.version && !alts.contains(v) {
                alts.push(v.clone());
            }
        }
    }
    alts.sort_by(|a, b| cmp_version(b, a));
    best.alternatives = alts;
    Some(best)
}

// ───────────────────────── Vérification sur disque ─────────────────────────

fn check_installed_with(root: &Path, dep: &Dep, env: &Env) -> InstalledCheck {
    let bases = search_bases(root, dep);
    match dep.ecosystem {
        Ecosystem::Npm => check_npm(&bases, dep),
        Ecosystem::Cargo => check_cargo(&bases, dep, env),
        Ecosystem::Pypi => check_pypi(&bases, dep),
        Ecosystem::Go => check_go(dep, env),
    }
}

/// Dossiers où chercher (remontée depuis le lockfile, puis la racine), comme Node et pip.
fn search_bases(root: &Path, dep: &Dep) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut cur = dep.lockfile.parent().map(Path::to_path_buf);
    while let Some(d) = cur {
        cur = d.parent().map(Path::to_path_buf);
        out.push(d);
    }
    out.push(root.to_path_buf());
    out
}

fn check_npm(bases: &[PathBuf], dep: &Dep) -> InstalledCheck {
    for base in bases {
        let pkg = base.join("node_modules").join(&dep.name);
        let manifest = pkg.join("package.json");
        if !manifest.is_file() {
            continue;
        }
        // Le plus proche node_modules gagne, comme Node : pas de remontée plus haut.
        let found = read_capped(&manifest, MAX_FILE_BYTES)
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| v.get("version").and_then(Value::as_str).map(str::to_string));
        return match found {
            Some(f) if !same_version(&f, &dep.version) => {
                InstalledCheck::VersionMismatch { lock: dep.version.clone(), found: f, path: pkg }
            }
            _ => InstalledCheck::Found(pkg),
        };
    }
    InstalledCheck::Missing
}

fn check_cargo(bases: &[PathBuf], dep: &Dep, env: &Env) -> InstalledCheck {
    let exact = format!("{}-{}", dep.name, dep.version);
    for base in bases {
        let vendored = base.join("vendor").join(&exact);
        if vendored.is_dir() {
            return verify_crate_dir(vendored, dep);
        }
    }
    let Some(home) = &env.cargo_home else { return InstalledCheck::Missing };
    let mut others: Vec<(String, PathBuf)> = Vec::new();
    for index in sub_dirs(&home.join("registry").join("src")) {
        let dir = index.join(&exact);
        if dir.is_dir() {
            return verify_crate_dir(dir, dep);
        }
        others.extend(crate_versions_in(&index, &dep.name));
    }
    match others.into_iter().max_by(|a, b| cmp_version(&a.0, &b.0)) {
        Some((found, path)) => InstalledCheck::VersionMismatch { lock: dep.version.clone(), found, path },
        None => InstalledCheck::Missing,
    }
}

fn verify_crate_dir(dir: PathBuf, dep: &Dep) -> InstalledCheck {
    let found = read_capped(&dir.join("Cargo.toml"), MAX_FILE_BYTES).and_then(|t| toml_value(&t, "package", "version"));
    match found {
        Some(f) if !same_version(&f, &dep.version) => InstalledCheck::VersionMismatch { lock: dep.version.clone(), found: f, path: dir },
        _ => InstalledCheck::Found(dir),
    }
}

/// Dossiers `<nom>-<version>` (version commençant par un chiffre) du registre.
fn crate_versions_in(index: &Path, name: &str) -> Vec<(String, PathBuf)> {
    let prefix = format!("{name}-");
    sub_dirs(index)
        .into_iter()
        .filter_map(|p| {
            let fname = p.file_name()?.to_string_lossy().into_owned();
            let rest = fname.strip_prefix(&prefix)?.to_string();
            let ok = rest.starts_with(|c: char| c.is_ascii_digit())
                && rest.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-'));
            ok.then_some((rest, p))
        })
        .collect()
}

fn check_pypi(bases: &[PathBuf], dep: &Dep) -> InstalledCheck {
    let want = norm_name(Ecosystem::Pypi, &dep.name);
    let underscored = want.replace('-', "_");
    let mut fallback: Option<PathBuf> = None;
    for site in pypi_site_dirs(bases) {
        let pkg = [want.clone(), underscored.clone(), dep.name.clone()].iter().map(|n| site.join(n)).find(|p| p.is_dir());
        if let Ok(rd) = fs::read_dir(&site) {
            for e in rd.flatten() {
                let fname = e.file_name().to_string_lossy().into_owned();
                let Some(stem) = fname.strip_suffix(".dist-info") else { continue };
                let Some((dname, dver)) = stem.rsplit_once('-') else { continue };
                if norm_name(Ecosystem::Pypi, dname) != want {
                    continue;
                }
                let found = read_capped(&e.path().join("METADATA"), MAX_FILE_BYTES)
                    .and_then(|t| t.lines().find_map(|l| l.strip_prefix("Version:").map(|v| v.trim().to_string())))
                    .unwrap_or_else(|| dver.to_string());
                let path = pkg.clone().unwrap_or_else(|| e.path());
                return if same_version(&found, &dep.version) {
                    InstalledCheck::Found(path)
                } else {
                    InstalledCheck::VersionMismatch { lock: dep.version.clone(), found, path }
                };
            }
        }
        if fallback.is_none() {
            fallback = pkg;
        }
    }
    // Paquet présent sans métadonnées de dist-info : le dossier existe, la version n'est pas vérifiable.
    fallback.map_or(InstalledCheck::Missing, InstalledCheck::Found)
}

fn pypi_site_dirs(bases: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for base in bases {
        for v in [".venv", "venv"] {
            let env = base.join(v);
            if !env.is_dir() {
                continue;
            }
            out.push(env.join("Lib").join("site-packages"));
            for e in sub_dirs(&env.join("lib")) {
                if e.file_name().map_or(false, |n| n.to_string_lossy().starts_with("python")) {
                    out.push(e.join("site-packages"));
                }
            }
        }
    }
    out.retain(|p| p.is_dir());
    out
}

fn check_go(dep: &Dep, env: &Env) -> InstalledCheck {
    let Some(cache) = &env.gomodcache else { return InstalledCheck::Missing };
    let esc = go_escape(&dep.name);
    let dir = cache.join(format!("{esc}@{}", dep.version));
    if dir.is_dir() {
        return InstalledCheck::Found(dir);
    }
    let base = cache.join(&esc);
    let (Some(parent), Some(last)) = (base.parent(), base.file_name()) else {
        return InstalledCheck::Missing;
    };
    let prefix = format!("{}@", last.to_string_lossy());
    let mut others: Vec<(String, PathBuf)> = sub_dirs(parent)
        .into_iter()
        .filter_map(|p| {
            let fname = p.file_name()?.to_string_lossy().into_owned();
            let v = fname.strip_prefix(&prefix)?.to_string();
            Some((v, p))
        })
        .collect();
    others.sort_by(|a, b| cmp_version(&b.0, &a.0));
    match others.into_iter().next() {
        Some((found, path)) => InstalledCheck::VersionMismatch { lock: dep.version.clone(), found, path },
        None => InstalledCheck::Missing,
    }
}

/// Échappement des modules Go : chaque majuscule devient `!` + minuscule.
fn go_escape(s: &str) -> String {
    s.chars().flat_map(|c| if c.is_ascii_uppercase() { vec!['!', c.to_ascii_lowercase()] } else { vec![c] }).collect()
}

fn sub_dirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = fs::read_dir(dir) else { return Vec::new() };
    rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect()
}

// ───────────────────────── Noms et versions ─────────────────────────

fn norm_name(eco: Ecosystem, name: &str) -> String {
    match eco {
        Ecosystem::Go => name.to_string(),
        Ecosystem::Npm => name.to_lowercase(),
        Ecosystem::Cargo => name.to_lowercase().replace('_', "-"),
        Ecosystem::Pypi => pypi_norm(name),
    }
}

/// PEP 503 : minuscules, suites de `-`, `_`, `.` remplacées par un seul `-`.
fn pypi_norm(name: &str) -> String {
    let mut out = String::new();
    let mut sep = false;
    for c in name.chars() {
        if matches!(c, '-' | '_' | '.') {
            sep = true;
        } else {
            if sep && !out.is_empty() {
                out.push('-');
            }
            sep = false;
            out.extend(c.to_lowercase());
        }
    }
    out
}

fn names_set(eco: Ecosystem, names: Vec<String>) -> BTreeSet<String> {
    names.iter().map(|n| norm_name(eco, n)).collect()
}

/// Parties d'une version ; `.0` finaux retirés (1.0 == 1.0.0), préfixe `v` de Go ignoré.
fn version_parts(v: &str) -> Vec<String> {
    let v = v.strip_prefix('v').filter(|r| r.starts_with(|c: char| c.is_ascii_digit())).unwrap_or(v);
    v.split(|c: char| !c.is_ascii_alphanumeric()).filter(|s| !s.is_empty()).map(str::to_string).collect()
}

/// Comparaison tolérante : partie par partie, numérique ; une partie alphabétique (pré-version)
/// est plus ancienne qu'une partie numérique ; une partie absente vaut `0` (1.0 == 1.0.0).
pub(crate) fn cmp_version(a: &str, b: &str) -> Ordering {
    let (pa, pb) = (version_parts(a), version_parts(b));
    for i in 0..pa.len().max(pb.len()) {
        let x = pa.get(i).map_or("0", String::as_str);
        let y = pb.get(i).map_or("0", String::as_str);
        let o = match (x.parse::<u64>(), y.parse::<u64>()) {
            (Ok(p), Ok(q)) => p.cmp(&q),
            (Ok(_), Err(_)) => Ordering::Greater,
            (Err(_), Ok(_)) => Ordering::Less,
            (Err(_), Err(_)) => x.cmp(y),
        };
        if o != Ordering::Equal {
            return o;
        }
    }
    Ordering::Equal
}

fn same_version(a: &str, b: &str) -> bool {
    cmp_version(a, b) == Ordering::Equal
}

// ───────────────────────── Lecture de fichiers ─────────────────────────

/// Lit au plus `cap` octets, en UTF-8 tolérant. `None` si le fichier est absent ou illisible.
fn read_capped(path: &Path, cap: u64) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    file.take(cap).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Dépendances déclarées dans le package.json du dossier (npm, pnpm, yarn).
fn json_declared(dir: &Path) -> BTreeSet<String> {
    let Some(t) = read_capped(&dir.join("package.json"), MAX_FILE_BYTES) else {
        return BTreeSet::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&t) else { return BTreeSet::new() };
    let mut names = Vec::new();
    for section in ["dependencies", "devDependencies", "optionalDependencies"] {
        if let Some(obj) = v.get(section).and_then(Value::as_object) {
            names.extend(obj.keys().cloned());
        }
    }
    names_set(Ecosystem::Npm, names)
}

fn pyproject_declared(dir: &Path) -> BTreeSet<String> {
    read_capped(&dir.join("pyproject.toml"), MAX_FILE_BYTES).map(|t| names_set(Ecosystem::Pypi, pyproject_direct(&t))).unwrap_or_default()
}

// ───────────────────────── Parseurs de lockfiles ─────────────────────────

fn parse_cargo_lock(text: &str) -> Vec<Raw> {
    // Les paquets sans `source` sont les crates locaux du workspace : pas de documentation tierce.
    toml_package_blocks(text).iter().filter(|b| b.contains_key("source")).filter_map(raw_from_block).collect()
}

/// `package-lock.json` : v2/v3 (`packages`), repli sur v1 (`dependencies` imbriqués).
fn parse_npm_lock(text: &str) -> Vec<Raw> {
    let Ok(v) = serde_json::from_str::<Value>(text) else { return Vec::new() };
    let mut out = Vec::new();
    if let Some(pk) = v.get("packages").and_then(Value::as_object) {
        for (key, entry) in pk {
            let Some(pos) = key.rfind("node_modules/") else { continue }; // racine, paquets locaux
            if entry.get("link").and_then(Value::as_bool).unwrap_or(false) {
                continue;
            }
            let Some(ver) = entry.get("version").and_then(Value::as_str) else { continue };
            out.push(Raw {
                name: key[pos + "node_modules/".len()..].to_string(),
                version: ver.to_string(),
                direct: false,
                hoisted: key.matches("node_modules/").count() == 1,
            });
        }
    } else if let Some(deps) = v.get("dependencies").and_then(Value::as_object) {
        collect_npm_v1(deps, true, &mut out);
    }
    out
}

fn collect_npm_v1(deps: &serde_json::Map<String, Value>, top: bool, out: &mut Vec<Raw>) {
    for (name, entry) in deps {
        if let Some(ver) = entry.get("version").and_then(Value::as_str) {
            out.push(Raw { name: name.clone(), version: ver.to_string(), direct: false, hoisted: top });
        }
        if let Some(nested) = entry.get("dependencies").and_then(Value::as_object) {
            collect_npm_v1(nested, false, out);
        }
    }
}

/// `pnpm-lock.yaml` : clés de la section `packages:` (`/nom@ver`, `nom@ver(peer)`, `/nom/ver`).
fn parse_pnpm(text: &str) -> Vec<Raw> {
    let mut out = Vec::new();
    let mut in_packages = false;
    for line in text.lines() {
        if !line.is_empty() && !line.starts_with(' ') && !line.starts_with('#') {
            in_packages = line.trim_end() == "packages:";
            continue;
        }
        // Clés d'entrée : indentation exacte de deux espaces, terminées par `:`.
        if !in_packages || !line.starts_with("  ") || line.starts_with("   ") {
            continue;
        }
        let t = line.trim_end();
        let Some(key) = t.strip_suffix(':') else { continue };
        let key = key.trim().trim_matches(|c: char| c == '\'' || c == '"');
        if let Some((name, version)) = pnpm_key(key) {
            out.push(Raw { name, version, direct: false, hoisted: false });
        }
    }
    out
}

fn pnpm_key(key: &str) -> Option<(String, String)> {
    let key = key.strip_prefix('/').unwrap_or(key);
    let key = key.split('(').next()?;
    let from = usize::from(key.starts_with('@'));
    if let Some(i) = key.get(from..).and_then(|s| s.rfind('@')).map(|i| i + from) {
        let (n, v) = (&key[..i], &key[i + 1..]);
        return (!n.is_empty() && !v.is_empty()).then(|| (n.to_string(), v.to_string()));
    }
    // Format v5 : `/nom/version` ou `/@scope/nom/version`.
    let i = key.rfind('/')?;
    let (n, v) = (&key[..i], &key[i + 1..]);
    (i > 0 && !n.is_empty() && !v.is_empty()).then(|| (n.to_string(), v.to_string()))
}

/// `yarn.lock` v1 et berry : en-têtes `"nom@spec", "nom@spec":` puis `version`.
fn parse_yarn(text: &str) -> Vec<Raw> {
    let mut out = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for line in text.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !line.starts_with(' ') {
            names = yarn_header_names(line);
            continue;
        }
        if line.starts_with("  ") && !line.starts_with("   ") {
            let t = line.trim();
            if let Some(v) = t.strip_prefix("version:").or_else(|| t.strip_prefix("version ")) {
                let v = v.trim().trim_matches('"');
                for n in names.drain(..) {
                    out.push(Raw { name: n, version: v.to_string(), direct: false, hoisted: false });
                }
            }
        }
    }
    out
}

fn yarn_header_names(line: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for spec in line.trim_end().trim_end_matches(':').split(',') {
        let spec = spec.trim().trim_matches('"');
        // Paquets locaux (workspace, fichiers, liens) : pas de documentation tierce.
        if ["@workspace:", "@file:", "@link:", "@portal:"].iter().any(|p| spec.contains(p)) {
            return Vec::new();
        }
        let from = usize::from(spec.starts_with('@'));
        let Some(i) = spec.get(from..).and_then(|s| s.find('@')).map(|i| i + from) else { continue };
        let name = &spec[..i];
        if !name.is_empty() && name != "__metadata" && !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    }
    names
}

/// `poetry.lock`, `uv.lock` : blocs `[[package]]` (noms et versions) lus par `raw_from_block`.
fn raw_from_block(b: &BTreeMap<String, String>) -> Option<Raw> {
    Some(Raw { name: b.get("name")?.clone(), version: b.get("version")?.clone(), direct: false, hoisted: false })
}

/// uv : paquets du projet lui-même (`virtual` / `editable` / `directory`) exclus.
fn is_local_source(b: &BTreeMap<String, String>) -> bool {
    b.get("source").map_or(false, |s| s.contains("virtual") || s.contains("editable") || s.contains("directory"))
}

/// `Pipfile.lock` (JSON) : sections `default` et `develop`, version `==x.y`.
fn parse_pipfile_lock(text: &str) -> Vec<Raw> {
    let Ok(v) = serde_json::from_str::<Value>(text) else { return Vec::new() };
    let mut out = Vec::new();
    for section in ["default", "develop"] {
        let Some(obj) = v.get(section).and_then(Value::as_object) else { continue };
        for (name, e) in obj {
            if let Some(ver) = e.get("version").and_then(Value::as_str) {
                out.push(Raw { name: name.clone(), version: ver.trim_start_matches('=').to_string(), direct: false, hoisted: false });
            }
        }
    }
    out
}

/// `requirements*.txt` : seulement les pins `==` (extras et marqueurs tolérés, plages ignorées).
fn parse_requirements(text: &str) -> Vec<Raw> {
    let mut out = Vec::new();
    for line in text.lines() {
        let l = line.split('#').next().unwrap_or("");
        let l = l.split(';').next().unwrap_or("").trim().trim_end_matches('\\').trim();
        if l.is_empty() || l.starts_with('-') {
            continue;
        }
        let Some((lhs, rhs)) = l.split_once("==") else { continue };
        let name = lhs.split('[').next().unwrap_or("").trim();
        let ver = rhs.trim_start_matches('=').split_whitespace().next().unwrap_or("");
        if ver.is_empty() || ver.chars().any(|c| c == ',' || c == '*') {
            continue;
        }
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
            continue;
        }
        out.push(Raw { name: name.to_string(), version: ver.to_string(), direct: true, hoisted: false });
    }
    out
}

/// `go.mod` : blocs `require (...)` et lignes `require x vY`. `// indirect` = non direct.
fn parse_go_mod(text: &str) -> Vec<Raw> {
    let mut out = Vec::new();
    let mut in_block = false;
    for line in text.lines() {
        let (code, comment) = line.split_once("//").unwrap_or((line, ""));
        let code = code.trim();
        if in_block {
            if code == ")" {
                in_block = false;
            } else {
                go_require(code, comment, &mut out);
            }
        } else if code.starts_with("require") && code.ends_with('(') {
            in_block = true;
        } else if let Some(rest) = code.strip_prefix("require ") {
            go_require(rest.trim(), comment, &mut out);
        }
    }
    out
}

fn go_require(code: &str, comment: &str, out: &mut Vec<Raw>) {
    let mut it = code.split_whitespace();
    let (Some(path), Some(ver)) = (it.next(), it.next()) else { return };
    if !ver.starts_with('v') {
        return;
    }
    out.push(Raw {
        name: path.trim_matches('"').to_string(),
        version: ver.to_string(),
        direct: !comment.trim().starts_with("indirect"),
        hoisted: false,
    });
}

// ───────────────────────── Manifestes (noms déclarés) ─────────────────────────

/// Noms des `Cargo.toml` : `[dependencies]`, `[dev-dependencies]`, `[target.*.dependencies]`,
/// `[workspace.dependencies]`, `[dependencies.nom]`, clés `nom.workspace = true`, `package = "vrai"`.
fn cargo_toml_deps(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_deps = false;
    for raw in text.lines() {
        let line = raw.trim();
        if let Some(h) = header(line) {
            if let Some(i) = h.rfind("dependencies.") {
                out.push(h[i + "dependencies.".len()..].trim_matches('"').to_string());
                in_deps = false;
            } else {
                in_deps = h.ends_with("dependencies");
            }
            continue;
        }
        if !in_deps {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let key = k.trim().trim_matches('"');
        let base = key.split('.').next().unwrap_or(key);
        out.push(package_rename(v).unwrap_or_else(|| base.to_string()));
    }
    out
}

fn package_rename(value: &str) -> Option<String> {
    let i = value.find("package")?;
    let rest = value[i + "package".len()..].trim_start().strip_prefix('=')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    Some(rest.split('"').next()?.to_string())
}

/// Noms déclarés dans `pyproject.toml` : `[project] dependencies`, `[dependency-groups]`,
/// `[tool.poetry.*dependencies]`.
fn pyproject_direct(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut sec = String::new();
    let mut open = false;
    for raw in text.lines() {
        let line = raw.trim();
        if open {
            push_quoted_names(line, &mut out);
            open = !line.contains(']');
            continue;
        }
        if let Some(h) = header(line) {
            sec = h.to_string();
            continue;
        }
        let Some((k, _)) = kv(line) else { continue };
        let rhs = line.split_once('=').map(|x| x.1).unwrap_or("").trim_start();
        let is_array = rhs.starts_with('[');
        if ((sec == "project" && k == "dependencies") || sec == "dependency-groups") && is_array {
            push_quoted_names(rhs, &mut out);
            open = !rhs.contains(']');
        } else if is_poetry_section(&sec) && k != "python" {
            out.push(k.to_string());
        }
    }
    out
}

fn is_poetry_section(sec: &str) -> bool {
    sec == "tool.poetry.dependencies"
        || sec == "tool.poetry.dev-dependencies"
        || (sec.starts_with("tool.poetry.group.") && sec.ends_with(".dependencies"))
}

/// Extrait le nom PEP 508 de chaque chaîne entre guillemets (`"requests>=2"` -> `requests`).
fn push_quoted_names(s: &str, out: &mut Vec<String>) {
    for (i, part) in s.split('"').enumerate() {
        if i % 2 == 0 || part == "include-group" {
            continue;
        }
        let name: String = part.chars().take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')).collect();
        if !name.is_empty() {
            out.push(name);
        }
    }
}

// ───────────────────────── TOML minimal (lignes) ─────────────────────────

type Block = BTreeMap<String, String>;

/// En-tête de section `[x]` -> `x` (un seul crochet retiré : `[[x]]` donne `[x`).
fn header(line: &str) -> Option<&str> {
    line.strip_prefix('[').map(|s| s.trim_end_matches(']').trim())
}

/// `clé = valeur` ; clé entre guillemets tolérée, valeur entre guillemets retirée.
fn kv(line: &str) -> Option<(&str, String)> {
    let (k, v) = line.split_once('=')?;
    let k = k.trim().trim_matches('"');
    if k.is_empty() || !k.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')) {
        return None;
    }
    let v = v.trim();
    let v = if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') { &v[1..v.len() - 1] } else { v };
    Some((k, v.to_string()))
}

fn toml_value(text: &str, section: &str, key: &str) -> Option<String> {
    let mut cur = String::new();
    for raw in text.lines() {
        let line = raw.trim();
        if let Some(h) = header(line) {
            cur = h.to_string();
            continue;
        }
        if cur == section {
            if let Some((k, v)) = kv(line) {
                if k == key {
                    return Some(v);
                }
            }
        }
    }
    None
}

fn toml_keys_in(text: &str, sections: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for raw in text.lines() {
        let line = raw.trim();
        if let Some(h) = header(line) {
            cur = h.to_string();
            continue;
        }
        if sections.contains(&cur.as_str()) {
            if let Some((k, _)) = kv(line) {
                out.push(k.to_string());
            }
        }
    }
    out
}

/// Blocs `[[package]]` : seules les clés de premier niveau du bloc (pas `[package.x]`).
fn toml_package_blocks(text: &str) -> Vec<Block> {
    let mut out = Vec::new();
    let mut cur: Option<Block> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('[') {
            out.extend(cur.take());
            if line == "[[package]]" {
                cur = Some(Block::new());
            }
            continue;
        }
        if let (Some(b), Some((k, v))) = (cur.as_mut(), kv(line)) {
            b.entry(k.to_string()).or_insert(v);
        }
    }
    out.extend(cur);
    out
}

// ───────────────────────── Tests ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// Dossier temporaire sous `std::env::temp_dir()`, supprimé à la sortie (même en cas d'échec).
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> TempDir {
            static N: AtomicUsize = AtomicUsize::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let p = std::env::temp_dir().join(format!("cortex-web-test-{}-{tag}-{n}", std::process::id()));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).expect("création du dossier de test");
            TempDir(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
        fn write(&self, rel: &str, content: &str) -> PathBuf {
            let p = self.0.join(rel);
            fs::create_dir_all(p.parent().expect("parent")).expect("dossier parent");
            fs::write(&p, content).expect("écriture de test");
            p
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn dep(eco: Ecosystem, name: &str, version: &str, lock: &Path) -> Dep {
        Dep {
            name: name.into(),
            version: version.into(),
            ecosystem: eco,
            direct: false,
            lockfile: lock.to_path_buf(),
            alternatives: vec![],
        }
    }

    fn env_in(t: &TempDir) -> Env {
        Env { cargo_home: Some(t.path().join("cargo")), gomodcache: Some(t.path().join("modcache")) }
    }

    #[test]
    fn cargo_lock_ignore_les_crates_locaux() {
        let t = r#"
[[package]]
name = "cortex"
version = "0.5.3"

[[package]]
name = "serde"
version = "1.0.200"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "abc"

[[package]]
name = "tokio"
version = "1.40.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
"#;
        let raws = parse_cargo_lock(t);
        let names: Vec<_> = raws.iter().map(|r| (r.name.as_str(), r.version.as_str())).collect();
        assert_eq!(names, vec![("serde", "1.0.200"), ("tokio", "1.40.0")]);
    }

    #[test]
    fn cargo_lock_plusieurs_versions_prend_la_plus_haute() {
        let lock = Path::new("Cargo.lock");
        let t = "[[package]]\nname = \"syn\"\nversion = \"1.0.9\"\nsource = \"registry+x\"\n\n\
                 [[package]]\nname = \"syn\"\nversion = \"1.0.10\"\nsource = \"registry+x\"\n\n\
                 [[package]]\nname = \"syn\"\nversion = \"2.0.1\"\nsource = \"registry+x\"\n";
        let deps = aggregate(Ecosystem::Cargo, lock, parse_cargo_lock(t), &BTreeSet::new());
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].version, "2.0.1");
        assert_eq!(deps[0].alternatives, vec!["1.0.10".to_string(), "1.0.9".to_string()]);
    }

    #[test]
    fn cargo_toml_directes_y_compris_formes_particulieres() {
        let t = r#"
[package]
name = "app"
version = "0.1.0"

[dependencies]
serde = { version = "1", features = ["derive"] }
clap.workspace = true
mon-alias = { package = "vrai-nom", version = "2" }

[dev-dependencies]
tempfile = "3"

[target.'cfg(unix)'.dependencies]
libc = "0.2"

[dependencies.foo]
version = "1"

[profile.release]
lto = true
"#;
        let names = cargo_toml_deps(t);
        for attendu in ["serde", "clap", "vrai-nom", "tempfile", "libc", "foo"] {
            assert!(names.contains(&attendu.to_string()), "manque {attendu} dans {names:?}");
        }
        assert!(!names.contains(&"mon-alias".to_string()));
        assert!(!names.contains(&"lto".to_string()));
    }

    #[test]
    fn cargo_lock_tronque_ne_panique_pas() {
        let full = "[[package]]\nname = \"serde\"\nversion = \"1.0.200\"\nsource = \"registry+x\"\n\n[[package]]\nname = \"to";
        let raws = parse_cargo_lock(full);
        assert_eq!(raws.len(), 1);
        assert_eq!(raws[0].name, "serde");
        // Coupure au milieu d'un caractère multi-octets : lecture tolérante, pas de panique.
        let bytes = "name = \"café\"\n".as_bytes();
        let coupe = String::from_utf8_lossy(&bytes[..bytes.len() - 3]).into_owned();
        let _ = parse_cargo_lock(&coupe);
    }

    #[test]
    fn npm_v3_hoisse_et_imbrique_scope() {
        let t = r#"{"name":"x","lockfileVersion":3,"packages":{
            "":{"name":"x","dependencies":{"@scope/pkg":"^1.0.0"}},
            "node_modules/@scope/pkg":{"version":"1.2.0"},
            "node_modules/a/node_modules/@scope/pkg":{"version":"0.9.0"}
        }}"#;
        let deps = aggregate(Ecosystem::Npm, Path::new("package-lock.json"), parse_npm_lock(t), &BTreeSet::new());
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "@scope/pkg");
        assert_eq!(deps[0].version, "1.2.0");
        assert_eq!(deps[0].alternatives, vec!["0.9.0".to_string()]);
    }

    #[test]
    fn npm_v1_dependencies_imbriques() {
        let t = r#"{"lockfileVersion":1,"dependencies":{
            "left-pad":{"version":"1.3.0"},
            "express":{"version":"4.18.0","dependencies":{"left-pad":{"version":"1.1.0"}}}
        }}"#;
        let raws = parse_npm_lock(t);
        let pad = raws.iter().find(|r| r.name == "left-pad" && r.hoisted).expect("left-pad racine");
        assert_eq!(pad.version, "1.3.0");
        assert!(raws.iter().any(|r| r.name == "left-pad" && r.version == "1.1.0" && !r.hoisted));
    }

    #[test]
    fn npm_v2_prefere_packages_et_ignore_liens_et_workspaces() {
        let t = r#"{"lockfileVersion":2,
          "packages":{
            "":{},
            "packages/ui":{"name":"ui","version":"0.0.1"},
            "node_modules/ui":{"resolved":"packages/ui","link":true},
            "node_modules/react":{"version":"18.3.1"}
          },
          "dependencies":{"react":{"version":"0.0.0-legacy"}}}"#;
        let raws = parse_npm_lock(t);
        assert_eq!(raws.len(), 1);
        assert_eq!((raws[0].name.as_str(), raws[0].version.as_str()), ("react", "18.3.1"));
    }

    #[test]
    fn npm_json_tronque_donne_rien_sans_panique() {
        assert!(parse_npm_lock(r#"{"packages":{"node_modules/a":{"version":"1"#).is_empty());
    }

    #[test]
    fn pnpm_cles_v6_et_v9_avec_suffixe_peer() {
        let t = r#"lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      lodash:
        specifier: ^4

packages:

  lodash@4.17.21:
    resolution: {integrity: sha512-x}

  '@babel/core@7.24.0':
    resolution: {integrity: y}

  /react-dom@18.2.0(react@18.2.0):
    resolution: {integrity: z}

snapshots:

  lodash@4.17.21: {}
"#;
        let raws = parse_pnpm(t);
        let got: Vec<_> = raws.iter().map(|r| (r.name.as_str(), r.version.as_str())).collect();
        assert_eq!(got, vec![("lodash", "4.17.21"), ("@babel/core", "7.24.0"), ("react-dom", "18.2.0")]);
    }

    #[test]
    fn pnpm_cles_v5_slash() {
        assert_eq!(pnpm_key("/lodash/4.17.21"), Some(("lodash".into(), "4.17.21".into())));
        assert_eq!(pnpm_key("/@babel/core/7.0.0"), Some(("@babel/core".into(), "7.0.0".into())));
    }

    #[test]
    fn yarn_v1_en_tetes_multiples_et_scopes() {
        let t = "# yarn lockfile v1\n\n\"@scope/pkg@^1.0.0\", \"@scope/pkg@^1.2.0\":\n  version \"1.2.3\"\n  resolved \"https://x\"\n\n\
                 lodash@^4.17.0, lodash@^4.17.21:\n  version \"4.17.21\"\n";
        let raws = parse_yarn(t);
        let got: Vec<_> = raws.iter().map(|r| (r.name.as_str(), r.version.as_str())).collect();
        assert_eq!(got, vec![("@scope/pkg", "1.2.3"), ("lodash", "4.17.21")]);
    }

    #[test]
    fn yarn_berry_version_et_workspace_ignore() {
        let t = "__metadata:\n  version: 6\n\n\"my-app@workspace:.\":\n  version: 0.0.0-use.local\n\n\
                 \"react@npm:^18.2.0\":\n  version: 18.3.1\n  resolution: \"react@npm:18.3.1\"\n";
        let raws = parse_yarn(t);
        assert_eq!(raws.len(), 1);
        assert_eq!((raws[0].name.as_str(), raws[0].version.as_str()), ("react", "18.3.1"));
    }

    #[test]
    fn poetry_lock_ignore_les_sous_tables_et_pyproject_donne_les_directes() {
        let lock = "[[package]]\nname = \"Flask\"\nversion = \"3.0.0\"\n\n[package.dependencies]\nwerkzeug = \">=3\"\nname = \"piege\"\n\n\
                    [[package]]\nname = \"werkzeug\"\nversion = \"3.0.1\"\n";
        let raws: Vec<_> = toml_package_blocks(lock).iter().filter_map(raw_from_block).collect();
        assert_eq!(raws.len(), 2);
        assert_eq!(raws[0].name, "Flask");
        let py = "[project]\nname = \"app\"\ndependencies = [\n  \"flask>=3\",\n  \"requests[socks]==2.31.0\",\n]\n\n\
                  [tool.poetry.group.dev.dependencies]\npytest = \"^8\"\n";
        let direct = pyproject_direct(py);
        assert!(direct.contains(&"flask".to_string()));
        assert!(direct.contains(&"requests".to_string()));
        assert!(direct.contains(&"pytest".to_string()));
    }

    #[test]
    fn uv_lock_ignore_le_projet_local() {
        let t = "[[package]]\nname = \"app\"\nversion = \"0.1.0\"\nsource = { virtual = \".\" }\n\n\
                 [[package]]\nname = \"idna\"\nversion = \"3.7\"\nsource = { registry = \"https://pypi.org/simple\" }\n";
        let raws: Vec<_> = toml_package_blocks(t).iter().filter(|b| !is_local_source(b)).filter_map(raw_from_block).collect();
        assert_eq!(raws.len(), 1);
        assert_eq!(raws[0].name, "idna");
    }

    #[test]
    fn pipfile_lock_default_et_develop_sans_le_prefixe() {
        let t = r#"{"default":{"requests":{"version":"==2.31.0"}},"develop":{"pytest":{"version":"==8.1.1"},"git-dep":{"git":"x"}}}"#;
        let raws = parse_pipfile_lock(t);
        let got: Vec<_> = raws.iter().map(|r| (r.name.as_str(), r.version.as_str())).collect();
        assert_eq!(got, vec![("requests", "2.31.0"), ("pytest", "8.1.1")]);
    }

    #[test]
    fn requirements_pins_seulement() {
        let t = "# commentaire\n-r autre.txt\nFlask[async]==3.0.0 ; python_version >= '3.9'\nrequests>=2.0\nnumpy==1.26.*\n\
                 Django_Rest==3.15.2  # pin\nlxml==5.2.1 \\\n    --hash=sha256:abc\n";
        let raws = parse_requirements(t);
        let got: Vec<_> = raws.iter().map(|r| (r.name.as_str(), r.version.as_str())).collect();
        assert_eq!(got, vec![("Flask", "3.0.0"), ("Django_Rest", "3.15.2"), ("lxml", "5.2.1")]);
        assert!(raws.iter().all(|r| r.direct));
    }

    #[test]
    fn go_mod_blocs_et_indirect() {
        let t = "module example.com/app\n\ngo 1.22\n\nrequire github.com/gin-gonic/gin v1.9.1\n\n\
                 require (\n\tgithub.com/Azure/go-ansi v0.2.0\n\tgolang.org/x/net v0.20.0 // indirect\n)\n";
        let raws = parse_go_mod(t);
        assert_eq!(raws.len(), 3);
        assert!(raws[0].direct && raws[1].direct);
        assert!(!raws[2].direct);
        assert_eq!(raws[2].version, "v0.20.0");
    }

    #[test]
    fn normalisation_pypi_et_comparaison_de_versions() {
        assert_eq!(pypi_norm("Flask_SQLAlchemy"), "flask-sqlalchemy");
        assert_eq!(pypi_norm("zope.interface"), "zope-interface");
        assert_eq!(cmp_version("1.10.0", "1.9.0"), Ordering::Greater);
        assert!(same_version("1.0", "1.0.0"));
        assert_eq!(cmp_version("1.0.0-beta", "1.0.0"), Ordering::Less);
        assert_eq!(cmp_version("v10.0.0", "v9.0.0"), Ordering::Greater);
    }

    #[test]
    fn detect_monorepo_profondeur_deux_et_ignore_les_dossiers_generes() {
        let t = TempDir::new("profondeur");
        t.write("Cargo.lock", "");
        t.write("sub1/package-lock.json", r#"{"packages":{"node_modules/a":{"version":"1.0.0"}}}"#);
        t.write("sub1/sub2/go.mod", "require x.io/a v1.0.0\n");
        t.write("sub1/sub2/sub3/go.mod", "require x.io/profond v1.0.0\n");
        t.write("node_modules/paquet/package-lock.json", r#"{"packages":{"node_modules/z":{"version":"9.0.0"}}}"#);
        t.write("target/go.mod", "require x.io/cible v1.0.0\n");
        t.write("_agents/go.mod", "require x.io/agents v1.0.0\n");
        t.write(".venv/go.mod", "require x.io/venv v1.0.0\n");
        let deps = detect_deps(t.path());
        let names: BTreeSet<_> = deps.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains("a"));
        assert!(names.contains("x.io/a"));
        assert!(!names.contains("x.io/profond"));
        assert!(!names.contains("z"));
        assert!(!names.contains("x.io/cible"));
        assert!(!names.contains("x.io/agents"));
        assert!(!names.contains("x.io/venv"));
    }

    #[test]
    fn detect_ne_panique_pas_sur_un_lockfile_binaire() {
        let t = TempDir::new("binaire");
        fs::write(t.path().join("Cargo.lock"), [0u8, 159, 146, 150, 255, 0, 10, 61]).expect("écriture");
        assert!(detect_deps(t.path()).iter().all(|d| !d.name.is_empty()));
    }

    #[test]
    fn detect_npm_direct_depuis_package_json() {
        let t = TempDir::new("npmdirect");
        t.write("package.json", r#"{"dependencies":{"@Scope/Pkg":"^1.0.0"}}"#);
        t.write(
            "package-lock.json",
            r#"{"packages":{"node_modules/@scope/pkg":{"version":"1.2.0"},"node_modules/transit":{"version":"2.0.0"}}}"#,
        );
        let deps = detect_deps(t.path());
        let pkg = deps.iter().find(|d| d.name == "@scope/pkg").expect("paquet présent");
        assert!(pkg.direct);
        let transit = deps.iter().find(|d| d.name == "transit").expect("transitive présente");
        assert!(!transit.direct);
    }

    #[test]
    fn detect_poetry_et_pyproject() {
        let t = TempDir::new("poetry");
        t.write("pyproject.toml", "[project]\ndependencies = [\"Flask>=3\"]\n");
        t.write(
            "poetry.lock",
            "[[package]]\nname = \"flask\"\nversion = \"3.0.0\"\n\n[[package]]\nname = \"click\"\nversion = \"8.1.7\"\n",
        );
        let deps = detect_deps(t.path());
        assert_eq!(deps.len(), 2);
        assert!(deps.iter().find(|d| d.name == "flask").expect("flask").direct);
        assert!(!deps.iter().find(|d| d.name == "click").expect("click").direct);
    }

    #[test]
    fn lecture_plafonnee_ne_lit_que_le_debut() {
        let t = TempDir::new("plafond");
        let p = t.write("gros.txt", "0123456789abcdef");
        assert_eq!(read_capped(&p, 10).as_deref(), Some("0123456789"));
        assert_eq!(read_capped(&t.path().join("absent"), 10), None);
    }

    #[test]
    fn installed_version_npm_scope_insensible_a_la_casse() {
        let t = TempDir::new("iv-npm");
        t.write("package-lock.json", r#"{"packages":{"node_modules/@scope/pkg":{"version":"1.2.0"}}}"#);
        let d = installed_version(t.path(), "@SCOPE/Pkg").expect("trouvé");
        assert_eq!(d.name, "@scope/pkg");
        assert_eq!(d.version, "1.2.0");
        assert!(installed_version(t.path(), "autre").is_none());
    }

    #[test]
    fn installed_version_prefere_la_directe_puis_la_plus_haute() {
        let t = TempDir::new("iv-direct");
        t.write("package.json", r#"{"dependencies":{"x":"^1.0.0"}}"#);
        t.write("package-lock.json", r#"{"packages":{"node_modules/x":{"version":"1.0.0"}}}"#);
        t.write("sub/package-lock.json", r#"{"packages":{"node_modules/x":{"version":"2.0.0"}}}"#);
        let d = installed_version(t.path(), "x").expect("trouvé");
        assert_eq!(d.version, "1.0.0");
        assert!(d.direct);
        assert_eq!(d.alternatives, vec!["2.0.0".to_string()]);
    }

    #[test]
    fn installed_version_python_normalise_les_noms() {
        let t = TempDir::new("iv-py");
        t.write("requirements.txt", "Flask_Login==0.6.3\n");
        let d = installed_version(t.path(), "flask-login").expect("trouvé");
        assert_eq!(d.version, "0.6.3");
        assert_eq!(d.ecosystem, Ecosystem::Pypi);
    }

    #[test]
    fn check_npm_trouve_manque_et_mismatch() {
        let t = TempDir::new("chk-npm");
        let lock = t.write("package-lock.json", "{}");
        t.write("node_modules/ok/package.json", r#"{"name":"ok","version":"1.0.0"}"#);
        t.write("node_modules/@s/bad/package.json", r#"{"name":"bad","version":"3.0.0"}"#);
        let env = env_in(&t);
        let ok = check_installed_with(t.path(), &dep(Ecosystem::Npm, "ok", "1.0.0", &lock), &env);
        assert_eq!(ok, InstalledCheck::Found(t.path().join("node_modules").join("ok")));
        let bad = check_installed_with(t.path(), &dep(Ecosystem::Npm, "@s/bad", "2.0.0", &lock), &env);
        assert!(matches!(bad, InstalledCheck::VersionMismatch { ref found, .. } if found == "3.0.0"));
        let miss = check_installed_with(t.path(), &dep(Ecosystem::Npm, "absent", "1.0.0", &lock), &env);
        assert_eq!(miss, InstalledCheck::Missing);
    }

    #[test]
    fn locate_installed_renvoie_none_si_la_version_differe() {
        let t = TempDir::new("locate");
        let lock = t.write("package-lock.json", "{}");
        t.write("node_modules/ok/package.json", r#"{"version":"1.0.0"}"#);
        assert!(locate_installed(t.path(), &dep(Ecosystem::Npm, "ok", "1.0.0", &lock)).is_some());
        // Sans cache hors projet : une version différente ne doit pas être présentée comme trouvée.
        assert!(locate_installed(t.path(), &dep(Ecosystem::Npm, "ok", "9.9.9", &lock)).is_none());
    }

    #[test]
    fn check_cargo_registre_trouve_et_mismatch() {
        let t = TempDir::new("chk-cargo");
        let lock = t.write("Cargo.lock", "");
        let env = env_in(&t);
        t.write("cargo/registry/src/index-abc/serde-1.0.200/Cargo.toml", "[package]\nname = \"serde\"\nversion = \"1.0.200\"\n");
        let found = check_installed_with(t.path(), &dep(Ecosystem::Cargo, "serde", "1.0.200", &lock), &env);
        assert!(matches!(found, InstalledCheck::Found(ref p) if p.ends_with("serde-1.0.200")));
        let mism = check_installed_with(t.path(), &dep(Ecosystem::Cargo, "serde", "1.0.100", &lock), &env);
        assert!(matches!(mism, InstalledCheck::VersionMismatch { ref found, .. } if found == "1.0.200"));
        let miss = check_installed_with(t.path(), &dep(Ecosystem::Cargo, "tokio", "1.0.0", &lock), &env);
        assert_eq!(miss, InstalledCheck::Missing);
    }

    #[test]
    fn check_cargo_vendor_a_la_priorite() {
        let t = TempDir::new("chk-vendor");
        let lock = t.write("Cargo.lock", "");
        t.write("vendor/mini-1.2.3/Cargo.toml", "[package]\nversion = \"1.2.3\"\n");
        let env = Env::default();
        let got = check_installed_with(t.path(), &dep(Ecosystem::Cargo, "mini", "1.2.3", &lock), &env);
        assert!(matches!(got, InstalledCheck::Found(ref p) if p.ends_with("mini-1.2.3")));
    }

    #[test]
    fn check_pypi_dist_info_trouve_et_mismatch() {
        let t = TempDir::new("chk-py");
        let lock = t.write("poetry.lock", "");
        t.write(".venv/Lib/site-packages/flask/__init__.py", "");
        t.write(".venv/Lib/site-packages/Flask-3.0.0.dist-info/METADATA", "Metadata-Version: 2.1\nName: Flask\nVersion: 3.0.0\n");
        let env = Env::default();
        let ok = check_installed_with(t.path(), &dep(Ecosystem::Pypi, "Flask", "3.0.0", &lock), &env);
        assert!(matches!(ok, InstalledCheck::Found(ref p) if p.ends_with("flask")));
        let bad = check_installed_with(t.path(), &dep(Ecosystem::Pypi, "flask", "2.0.0", &lock), &env);
        assert!(matches!(bad, InstalledCheck::VersionMismatch { ref found, .. } if found == "3.0.0"));
    }

    #[test]
    fn check_go_echappement_et_mismatch() {
        let t = TempDir::new("chk-go");
        let lock = t.write("go.mod", "");
        t.write("modcache/github.com/!azure/lib@v1.2.0/go.mod", "module x\n");
        let env = env_in(&t);
        let ok = check_installed_with(t.path(), &dep(Ecosystem::Go, "github.com/Azure/lib", "v1.2.0", &lock), &env);
        assert!(matches!(ok, InstalledCheck::Found(ref p) if p.ends_with("lib@v1.2.0")));
        let bad = check_installed_with(t.path(), &dep(Ecosystem::Go, "github.com/Azure/lib", "v1.3.0", &lock), &env);
        assert!(matches!(bad, InstalledCheck::VersionMismatch { ref found, .. } if found == "v1.2.0"));
        assert_eq!(go_escape("github.com/Azure/X"), "github.com/!azure/!x");
    }

    #[test]
    fn docs_candidates_par_ecosysteme() {
        let lock = Path::new("x.lock");
        let cargo = docs_candidates(&dep(Ecosystem::Cargo, "serde-json", "1.0.1", lock));
        assert_eq!(cargo[0].url, "https://docs.rs/serde-json/1.0.1/serde_json/");
        assert_eq!((cargo[0].tier, cargo[0].versioned), (2, true));
        assert_eq!(cargo[1].url, "https://crates.io/api/v1/crates/serde-json/1.0.1");
        let npm = docs_candidates(&dep(Ecosystem::Npm, "@scope/pkg", "1.2.0", lock));
        assert_eq!(npm[0].url, "https://registry.npmjs.org/@scope%2Fpkg/1.2.0");
        assert_eq!(npm[0].tier, 1);
        let py = docs_candidates(&dep(Ecosystem::Pypi, "flask", "3.0.0", lock));
        assert_eq!(py[0].url, "https://pypi.org/pypi/flask/3.0.0/json");
        let go = docs_candidates(&dep(Ecosystem::Go, "github.com/x/y", "v1.0.0", lock));
        assert_eq!(go[0].url, "https://pkg.go.dev/github.com/x/y@v1.0.0");
        assert_eq!(go[0].kind, "pkg.go.dev");
    }

    /// Vérification réelle (lecture seule) : `RESOLVE_REEL=<dossier;dossier>` puis
    /// `cargo test --features web web::resolve -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn verif_reelle_lockfiles() {
        let Ok(liste) = std::env::var("RESOLVE_REEL") else { return };
        for racine in liste.split(';').filter(|s| !s.is_empty()) {
            let debut = std::time::Instant::now();
            let deps = detect_deps(Path::new(racine));
            let ms = debut.elapsed().as_millis();
            println!("== {racine} : {} dépendances en {ms} ms", deps.len());
            for d in deps.iter().take(5) {
                println!(
                    "   {:?} {} {} direct={} alt={:?} lock={}",
                    d.ecosystem,
                    d.name,
                    d.version,
                    d.direct,
                    d.alternatives,
                    d.lockfile.display()
                );
            }
        }
    }

    #[test]
    fn detect_sur_un_dossier_absent_ne_panique_pas() {
        assert!(detect_deps(Path::new("C:/cortex-inexistant-zzz")).is_empty());
    }
}
