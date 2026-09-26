//! Résolution des relations (imports, appels) — UN SEUL algorithme, partagé par
//! la construction complète de l'atlas (`atlas::build::build_full`, univers =
//! un `ProjectIndex` en mémoire) et par la mise à jour incrémentale
//! (`atlas::incremental`, univers = atlas courant + changements en attente).
//! Deux univers, une seule logique : c'est ce qui garantit qu'un delta résout
//! exactement comme une reconstruction complète (test-juge d'équivalence).
//!
//! - IMPORTS : un spécificateur relatif (`./x`, `../x`), aliasé (`@/` → `src/`)
//!   ou absolu-projet (`/x`) est résolu vers les fichiers RÉELLEMENT présents
//!   (chemin exact, chemin + extension connue, `<chemin>/index.<ext>`). Un
//!   package externe n'est jamais résolu (pas de faux lien vers un homonyme).
//! - APPELS (granularité fonction : seuls les appels dans la plage de lignes du
//!   symbole) : (1) même fichier, (2) fichier réellement importé par
//!   l'appelant, (3) globalement seulement si le nom est UNIQUE dans le projet.
//!   Sinon : ambigu — compté, jamais choisi au hasard.

use crate::fx::{FxHashMap, FxHashSet};
use crate::symbol::SymbolKind;

/// Méthodes JS/TS trop génériques pour être résolues comme des appels du projet.
pub const GENERIC_CALLS: &[&str] = &[
    "push", "map", "filter", "find", "select", "from", "rpc", "then", "catch", "log", "get", "set", "split", "join", "slice", "includes",
    "foreach", "keys", "values", "has", "delete", "add",
];

/// Extensions essayées pour résoudre un spécificateur d'import sans extension.
const RESOLVE_EXTS: &[&str] = &["ts", "tsx", "js", "jsx", "mjs", "cjs", "py", "rs"];

/// Retire l'extension connue d'un chemin (index "stem" : `./Foo` → `Foo.tsx`).
pub fn strip_known_ext(path: &str) -> &str {
    for e in RESOLVE_EXTS {
        if let Some(s) = path.strip_suffix(e).and_then(|s| s.strip_suffix('.')) {
            return s;
        }
    }
    path
}

/// Normalise un chemin avec segments `.`/`..`, sans jamais remonter au-dessus
/// de la racine (une remontée en trop est ignorée).
fn normalize_path(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    out.join("/")
}

/// BASE d'un spécificateur d'import depuis le dossier `from_dir` : chemin
/// normalisé qu'il désigne (avant essai des extensions / `index`), ou `None`
/// pour un package externe. Persistée dans l'atlas (`import_bases`) pour
/// retrouver, quand un fichier apparaît ou disparaît, qui pouvait l'importer.
pub fn spec_base(from_dir: &str, spec: &str) -> Option<String> {
    if let Some(rest) = spec.strip_prefix("@/") {
        Some(normalize_path(&format!("src/{}", rest)))
    } else if spec.starts_with("./") || spec.starts_with("../") {
        let joined = if from_dir.is_empty() { spec.to_string() } else { format!("{}/{}", from_dir, spec) };
        Some(normalize_path(&joined))
    } else if spec.starts_with('/') {
        Some(normalize_path(spec.trim_start_matches('/')))
    } else {
        None
    }
}

/// Bases d'import qu'un fichier de chemin `path` SATISFAIT (inverse de la
/// résolution : `base == path`, `base == stem(path)`, `base/index == stem(path)`).
pub fn bases_satisfied_by(path: &str) -> Vec<String> {
    let stem = strip_known_ext(path);
    let mut out = vec![path.to_string()];
    if stem != path {
        out.push(stem.to_string());
    }
    if let Some(dir) = stem.strip_suffix("/index") {
        out.push(dir.to_string());
    }
    out
}

pub fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

/// Un candidat de résolution : symbole défini quelque part.
#[derive(Clone, Copy, Debug)]
pub struct Cand<F, S> {
    pub file: F,
    pub sym: S,
    /// Rang du symbole dans son fichier (départage "même fichier" : le premier).
    pub ord: u32,
    pub kind: SymbolKind,
}

/// Ce que la résolution a besoin de savoir du projet.
pub trait Universe {
    type F: Copy + Eq + std::hash::Hash;
    type S: Copy + Eq;
    fn file_by_path(&self, path: &str) -> Option<Self::F>;
    fn files_by_stem(&self, stem: &str) -> Vec<Self::F>;
    /// Toutes les définitions d'un nom (minuscules ASCII), tous fichiers vivants.
    fn defs(&self, name_lower: &str) -> &[Cand<Self::F, Self::S>];
}

/// Fichiers RÉELLEMENT importés par `from` (chemin `from_path`).
pub fn resolve_imports<'s, U: Universe>(u: &U, from: U::F, from_path: &str, specs: impl Iterator<Item = &'s str>) -> Vec<U::F> {
    let dir = dir_of(from_path);
    let mut out: Vec<U::F> = Vec::new();
    for spec in specs {
        let Some(base) = spec_base(dir, spec) else { continue };
        let mut push = |t: U::F| {
            if t != from && !out.contains(&t) {
                out.push(t);
            }
        };
        if let Some(t) = u.file_by_path(&base) {
            push(t);
        }
        for t in u.files_by_stem(&base) {
            push(t);
        }
        for t in u.files_by_stem(&format!("{}/index", base)) {
            push(t);
        }
    }
    out
}

enum Resolution<F, S> {
    Found(Cand<F, S>),
    Ambiguous,
}

/// Résolution d'un appel `name_lower` fait DEPUIS `from` (voir l'en-tête).
fn resolve_call<U: Universe>(u: &U, name_lower: &str, from: U::F, imported: &[U::F]) -> Option<Resolution<U::F, U::S>> {
    let cands = u.defs(name_lower);
    if cands.is_empty() {
        return None;
    }
    if let Some(c) = cands.iter().filter(|c| c.file == from).min_by_key(|c| c.ord) {
        return Some(Resolution::Found(*c));
    }
    let mut in_imports = cands.iter().filter(|c| imported.contains(&c.file));
    if let Some(first) = in_imports.next() {
        return Some(if in_imports.next().is_none() { Resolution::Found(*first) } else { Resolution::Ambiguous });
    }
    if cands.len() == 1 {
        return Some(Resolution::Found(cands[0]));
    }
    Some(Resolution::Ambiguous)
}

/// Un fichier vu par la résolution d'appels : ses symboles (nom, plage) et ses
/// références brutes.
pub struct FileCalls<'a> {
    /// Noms des symboles du fichier, en minuscules (test "défini localement").
    pub own_lower: FxHashSet<String>,
    /// Noms importés nommés, en minuscules.
    pub imported_lower: FxHashSet<String>,
    /// Appels (nom, ligne), dans l'ordre d'extraction.
    pub calls: Vec<(&'a str, u32)>,
    /// Noms appelés en minuscules (parallèle à `calls`).
    calls_lower: Vec<String>,
}

impl<'a> FileCalls<'a> {
    pub fn new(
        own_names: impl Iterator<Item = &'a str>,
        imported_names: impl Iterator<Item = &'a str>,
        calls: Vec<(&'a str, u32)>,
    ) -> Self {
        let calls_lower = calls.iter().map(|(n, _)| n.to_ascii_lowercase()).collect();
        FileCalls {
            own_lower: own_names.map(|n| n.to_ascii_lowercase()).collect(),
            imported_lower: imported_names.map(|n| n.to_ascii_lowercase()).collect(),
            calls,
            calls_lower,
        }
    }

    /// Noms appelés (minuscules) — clés dont `Universe::defs` aura besoin.
    pub fn called_names(&self) -> impl Iterator<Item = String> + '_ {
        self.calls_lower.iter().cloned()
    }
}

/// Appelés RÉSOLUS (dans l'ordre des appels, sans doublon) et nombre d'appels
/// AMBIGUS d'un symbole de plage `[line, end_line]` (`end_line == 0` : tout le
/// fichier). Filtres des appelés : méthodes génériques exclues, nom importé ou
/// défini localement requis, cible de genre `Import` exclue. Le compte d'ambigus
/// porte sur TOUS les appels de la plage (sans ces filtres), comme avant.
pub fn resolve_symbol_calls<U: Universe>(
    u: &U,
    from: U::F,
    imported_files: &[U::F],
    fc: &FileCalls,
    line: u32,
    end_line: u32,
    cache: &mut FxHashMap<String, Option<(bool, Option<Cand<U::F, U::S>>)>>,
) -> (Vec<Cand<U::F, U::S>>, usize) {
    let mut out: Vec<Cand<U::F, U::S>> = Vec::new();
    let mut ambiguous = 0usize;
    for (i, &(_, cl_line)) in fc.calls.iter().enumerate() {
        if end_line > 0 && !(cl_line >= line && cl_line <= end_line) {
            continue;
        }
        let cl = fc.calls_lower[i].as_str();
        // (ambigu ?, trouvé ?) — mis en cache par nom : la résolution ne dépend
        // que du nom et du fichier appelant.
        let res = match cache.get(cl) {
            Some(r) => *r,
            None => {
                let r = match resolve_call(u, cl, from, imported_files) {
                    None => None,
                    Some(Resolution::Ambiguous) => Some((true, None)),
                    Some(Resolution::Found(c)) => Some((false, Some(c))),
                };
                cache.insert(cl.to_string(), r);
                r
            }
        };
        let Some((is_amb, found)) = res else { continue };
        if is_amb {
            ambiguous += 1;
        }
        if GENERIC_CALLS.contains(&cl) {
            continue;
        }
        if !(fc.imported_lower.contains(cl) || fc.own_lower.contains(cl)) {
            continue;
        }
        if let Some(c) = found {
            if !matches!(c.kind, SymbolKind::Import) && !out.iter().any(|o| o.sym == c.sym) {
                out.push(c);
            }
        }
    }
    (out, ambiguous)
}

// ─── Univers "projet complet en mémoire" (construction de la base) ──────────

use crate::index::ProjectIndex;

/// Univers de résolution sur un `ProjectIndex` complet : fichiers = index dans
/// `idx.files`, symboles = (fichier, rang).
pub struct IndexUniverse<'a> {
    path_index: FxHashMap<&'a str, usize>,
    stem_index: FxHashMap<&'a str, Vec<usize>>,
    defs: FxHashMap<String, Vec<Cand<usize, (usize, usize)>>>,
}

impl<'a> IndexUniverse<'a> {
    pub fn new(idx: &'a ProjectIndex) -> Self {
        let mut path_index = FxHashMap::default();
        let mut stem_index: FxHashMap<&str, Vec<usize>> = FxHashMap::default();
        let mut defs: FxHashMap<String, Vec<Cand<usize, (usize, usize)>>> = FxHashMap::default();
        for (fi, f) in idx.files.iter().enumerate() {
            path_index.insert(f.path.as_str(), fi);
            stem_index.entry(strip_known_ext(&f.path)).or_default().push(fi);
            for (si, s) in f.symbols.iter().enumerate() {
                defs.entry(s.name.to_ascii_lowercase()).or_default().push(Cand { file: fi, sym: (fi, si), ord: si as u32, kind: s.kind });
            }
        }
        IndexUniverse { path_index, stem_index, defs }
    }
}

impl<'a> Universe for IndexUniverse<'a> {
    type F = usize;
    type S = (usize, usize);
    fn file_by_path(&self, path: &str) -> Option<usize> {
        self.path_index.get(path).copied()
    }
    fn files_by_stem(&self, stem: &str) -> Vec<usize> {
        self.stem_index.get(stem).cloned().unwrap_or_default()
    }
    fn defs(&self, name_lower: &str) -> &[Cand<usize, (usize, usize)>] {
        self.defs.get(name_lower).map(|v| v.as_slice()).unwrap_or(&[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{FileEntry, ProjectIndex};
    use crate::lang::Lang;
    use crate::symbol::{CallRef, FileRefs, Symbol, SymbolKind};

    fn sym(name: &str, kind: SymbolKind, line: u32, end_line: u32) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind,
            line,
            end_line,
            signature: String::new(),
            tokens: Vec::new(),
            doc: Vec::new(),
            summary: String::new(),
        }
    }

    fn file(path: &str, symbols: Vec<Symbol>, refs: FileRefs) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            lang: Lang::TypeScript,
            hash: String::new(),
            size: 0,
            mtime: 0,
            lines: 100,
            symbols,
            refs,
            header: Vec::new(),
            summary: String::new(),
            body: Vec::new(),
        }
    }

    fn idx(files: Vec<FileEntry>) -> ProjectIndex {
        let mut files = files;
        files.sort_by(|a, b| a.path.cmp(&b.path));
        ProjectIndex { name: "test".into(), root: "/proj".into(), generated_at: 0, files }
    }

    /// Appelés résolus (chemins des fichiers cibles) du symbole `name`.
    fn callees(p: &ProjectIndex, name: &str) -> (Vec<String>, usize) {
        let u = IndexUniverse::new(p);
        let (fi, si) =
            p.files.iter().enumerate().find_map(|(fi, f)| f.symbols.iter().position(|s| s.name == name).map(|si| (fi, si))).unwrap();
        let f = &p.files[fi];
        let imported = resolve_imports(&u, fi, &f.path, f.refs.imports.iter().map(|s| s.as_str()));
        let fc = FileCalls::new(
            f.symbols.iter().map(|s| s.name.as_str()),
            f.refs.imported_names.iter().map(|s| s.as_str()),
            f.refs.calls.iter().map(|c| (c.name.as_str(), c.line)).collect(),
        );
        let s = &f.symbols[si];
        let (out, amb) = resolve_symbol_calls(&u, fi, &imported, &fc, s.line, s.end_line, &mut FxHashMap::default());
        (out.iter().map(|c| p.files[c.file].path.clone()).collect(), amb)
    }

    /// Deux fichiers définissent `vider` ; un troisième importe celui de
    /// `CacheLocal` et appelle `vider` : la résolution doit pointer vers LE BON.
    #[test]
    fn resout_homonyme_via_import_reel() {
        let f0 = file("src/lib/CacheLocal.ts", vec![sym("vider", SymbolKind::Function, 5, 8)], FileRefs::default());
        let f1 = file("src/components/LazyMarkdownDocument.tsx", vec![sym("vider", SymbolKind::Function, 3, 6)], FileRefs::default());
        let mut refs2 = FileRefs::default();
        refs2.imports.push("./lib/CacheLocal".to_string());
        refs2.imported_names.push("vider".to_string());
        refs2.calls.push(CallRef { name: "vider".to_string(), line: 10 });
        let f2 = file("src/main.ts", vec![sym("caller", SymbolKind::Function, 1, 20)], refs2);
        let (out, _) = callees(&idx(vec![f0, f1, f2]), "caller");
        assert_eq!(out, vec!["src/lib/CacheLocal.ts".to_string()]);
    }

    /// Sans import résolu et avec DEUX définitions : ambigu, jamais un choix arbitraire.
    #[test]
    fn ambigu_sans_import_resolu() {
        let f0 = file("src/a/vider.ts", vec![sym("vider", SymbolKind::Function, 1, 3)], FileRefs::default());
        let f1 = file("src/b/vider.ts", vec![sym("vider", SymbolKind::Function, 1, 3)], FileRefs::default());
        let mut refs2 = FileRefs::default();
        refs2.imported_names.push("vider".to_string());
        refs2.calls.push(CallRef { name: "vider".to_string(), line: 5 });
        let f2 = file("src/c/main.ts", vec![sym("caller", SymbolKind::Function, 1, 10)], refs2);
        let (out, amb) = callees(&idx(vec![f0, f1, f2]), "caller");
        assert!(out.is_empty());
        assert_eq!(amb, 1);
    }

    /// Alias `@/` → `src/`.
    #[test]
    fn alias_arobase_slash() {
        let target = file("src/lib/utils.ts", vec![sym("formatDate", SymbolKind::Function, 1, 3)], FileRefs::default());
        let mut refs = FileRefs::default();
        refs.imports.push("@/lib/utils".to_string());
        refs.imported_names.push("formatDate".to_string());
        refs.calls.push(CallRef { name: "formatDate".to_string(), line: 4 });
        let caller = file("src/components/deep/nested/Widget.tsx", vec![sym("Widget", SymbolKind::Component, 1, 10)], refs);
        assert_eq!(callees(&idx(vec![target, caller]), "Widget").0, vec!["src/lib/utils.ts".to_string()]);
    }

    /// `import { x } from './dossier'` → `dossier/index.ts`.
    #[test]
    fn resout_index_ts() {
        let target = file("src/hooks/index.ts", vec![sym("useThing", SymbolKind::Hook, 1, 3)], FileRefs::default());
        let mut refs = FileRefs::default();
        refs.imports.push("./hooks".to_string());
        refs.imported_names.push("useThing".to_string());
        refs.calls.push(CallRef { name: "useThing".to_string(), line: 4 });
        let caller = file("src/App.tsx", vec![sym("App", SymbolKind::Component, 1, 10)], refs);
        let p = idx(vec![target, caller]);
        let u = IndexUniverse::new(&p);
        let app = p.files.iter().position(|f| f.path == "src/App.tsx").unwrap();
        let imp = resolve_imports(&u, app, "src/App.tsx", ["./hooks"].into_iter());
        assert_eq!(imp.iter().map(|&f| p.files[f].path.as_str()).collect::<Vec<_>>(), vec!["src/hooks/index.ts"]);
        assert_eq!(callees(&p, "App").0, vec!["src/hooks/index.ts".to_string()]);
    }

    /// Import relatif `../` depuis un sous-dossier profond.
    #[test]
    fn resout_import_relatif_parent() {
        let target = file("src/lib/helpers.ts", vec![sym("helper", SymbolKind::Function, 1, 3)], FileRefs::default());
        let mut refs = FileRefs::default();
        refs.imports.push("../../lib/helpers".to_string());
        refs.imported_names.push("helper".to_string());
        refs.calls.push(CallRef { name: "helper".to_string(), line: 4 });
        let caller = file("src/components/deep/Nested.tsx", vec![sym("Nested", SymbolKind::Component, 1, 10)], refs);
        assert_eq!(callees(&idx(vec![target, caller]), "Nested").0, vec!["src/lib/helpers.ts".to_string()]);
    }

    /// Granularité fonction : chaque fonction n'appelle QUE ce qui est dans SA plage.
    #[test]
    fn granularite_fonction() {
        let target = file(
            "src/lib/svc.ts",
            vec![sym("alpha", SymbolKind::Function, 1, 1), sym("beta", SymbolKind::Function, 2, 2)],
            FileRefs::default(),
        );
        let mut refs = FileRefs::default();
        refs.imported_names.push("alpha".to_string());
        refs.imported_names.push("beta".to_string());
        refs.calls.push(CallRef { name: "alpha".to_string(), line: 3 });
        refs.calls.push(CallRef { name: "beta".to_string(), line: 9 });
        let caller = file("src/main.ts", vec![sym("f", SymbolKind::Function, 1, 5), sym("g", SymbolKind::Function, 7, 11)], refs);
        let p = idx(vec![target, caller]);
        assert_eq!(callees(&p, "f").0, vec!["src/lib/svc.ts".to_string()]);
        assert_eq!(callees(&p, "g").0, vec!["src/lib/svc.ts".to_string()]);
        let u = IndexUniverse::new(&p);
        let f = &p.files[1];
        let fc = FileCalls::new(
            f.symbols.iter().map(|s| s.name.as_str()),
            f.refs.imported_names.iter().map(|s| s.as_str()),
            f.refs.calls.iter().map(|c| (c.name.as_str(), c.line)).collect(),
        );
        let (out_f, _) = resolve_symbol_calls(&u, 1, &[], &fc, 1, 5, &mut FxHashMap::default());
        assert_eq!(out_f.len(), 1);
        assert_eq!(p.files[0].symbols[out_f[0].sym.1].name, "alpha");
    }

    #[test]
    fn bases_satisfaites() {
        assert_eq!(bases_satisfied_by("src/hooks/index.ts"), vec!["src/hooks/index.ts", "src/hooks/index", "src/hooks"]);
        assert_eq!(spec_base("src/a", "../b/c"), Some("src/b/c".to_string()));
        assert_eq!(spec_base("src/a", "react"), None);
    }
}
