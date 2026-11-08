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
//!   Rust (`use`, `mod`) : tiges candidates ordonnées (`rust_refs::candidates`),
//!   la première qui existe gagne ; si l'élément importé est ré-exporté par le
//!   fichier atteint (`pub use`), la ré-exportation est suivie (profondeur
//!   bornée) et chaque fichier traversé compte comme importé.
//! - APPELS (granularité fonction : seuls les appels dans la plage de lignes du
//!   symbole) : (1) même fichier, (2) fichier réellement importé par
//!   l'appelant, (3) globalement seulement si le nom est UNIQUE dans le projet.
//!   Sinon : ambigu — compté, jamais choisi au hasard. Un bloc `impl Foo` cède
//!   la place à la définition du type `Foo`, comme un prototype C++ à sa
//!   définition. Fichiers Rust : règle de casse (`Foo` ne vise jamais `fn foo`)
//!   et, dans le fichier même, un homonyme UNIQUE (sinon ambigu).
//!
//! La résolution d'un fichier ne dépend donc que de ses références, des CHEMINS
//! du projet, des définitions des noms qu'il appelle et — pour Rust — des
//! ré-exportations des fichiers qu'il importe (voir `atlas::incremental`).

use crate::fx::{FxHashMap, FxHashSet};
use crate::rust_refs;
use crate::symbol::SymbolKind;

/// Méthodes JS/TS trop génériques pour être résolues comme des appels du projet.
pub const GENERIC_CALLS: &[&str] = &[
    "push", "map", "filter", "find", "select", "from", "rpc", "then", "catch", "log", "get", "set", "split", "join", "slice", "includes",
    "foreach", "keys", "values", "has", "delete", "add",
];

/// Extensions essayées pour résoudre un spécificateur d'import sans extension.
const RESOLVE_EXTS: &[&str] = &[
    "ts", "tsx", "js", "jsx", "mjs", "cjs", "py", "rs", "lua", "glsl", "h", "hpp", "hh", "hxx", "inl", "ipp", "tpp", "c", "cc", "cpp",
    "cxx",
];

/// Clé de résolution d'un nom de symbole : minuscules ASCII du DERNIER segment
/// d'un nom qualifié (`AFoxMissile::Launch` → `launch`). Un appel (`Launch()`,
/// `obj->Launch()`) ne porte que le nom simple ; les noms sans `::` (tous les
/// langages hors C/C++) ne changent pas.
pub fn call_key(name: &str) -> String {
    let simple = name.rsplit("::").next().unwrap_or(name);
    // Lua : `widget:DrawWorld`, `M.foo` → dernier segment (`drawworld`, `foo`), comme un appel
    // `obj:DrawWorld()` / `M.foo()`. Les noms à espaces (titres markdown) restent entiers.
    let simple = if simple.contains(['.', ':']) && !simple.contains(char::is_whitespace) {
        simple.rsplit(['.', ':']).next().filter(|s| !s.is_empty()).unwrap_or(simple)
    } else {
        simple
    };
    simple.to_ascii_lowercase()
}

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

/// Toutes les BASES candidates d'un spécificateur écrit dans le fichier
/// `from_path` (persistées dans `import_bases`) : une seule pour un chemin
/// JS/TS relatif ou aliasé, la liste ordonnée des tiges candidates pour un
/// chemin Rust (`rust_refs::candidates`), aucune pour un package externe.
pub fn spec_bases(from_path: &str, spec: &str) -> Vec<String> {
    if is_rust(from_path, spec) {
        let mut v: Vec<String> = Vec::new();
        for (s, _) in rust_refs::candidates(from_path, spec) {
            if !v.contains(&s) {
                v.push(s);
            }
        }
        v
    } else {
        spec_base(dir_of(from_path), spec).into_iter().collect()
    }
}

/// Tiges sous lesquelles un fichier est indexé (`files_by_stem`) : son chemin
/// sans extension connue et, pour un fichier Rust d'une crate, sa clé
/// `rs:<crate>/<module>` (`rust_refs::crate_key`).
pub fn stem_keys(path: &str) -> Vec<String> {
    let mut v = vec![strip_known_ext(path).to_string()];
    if let Some(k) = rust_refs::crate_key(path) {
        v.push(k);
    }
    v
}

/// Bases d'import qu'un fichier de chemin `path` SATISFAIT (inverse de la
/// résolution : `base == path`, `base == stem(path)`, `base/index == stem(path)`,
/// clé de crate Rust).
pub fn bases_satisfied_by(path: &str) -> Vec<String> {
    let stem = strip_known_ext(path);
    let mut out = vec![path.to_string()];
    if stem != path {
        out.push(stem.to_string());
    }
    if let Some(dir) = stem.strip_suffix("/index") {
        out.push(dir.to_string());
    }
    if let Some(k) = rust_refs::crate_key(path) {
        out.push(k);
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
    /// Nom (dernier segment) à initiale majuscule : règle de casse des
    /// fichiers Rust (`FileCalls::case_class`).
    pub upper: bool,
}

/// Initiale majuscule du nom simple (`a::Foo` → vrai, `M.foo` → faux).
pub fn starts_upper(name: &str) -> bool {
    let simple = name.rsplit("::").next().unwrap_or(name);
    let simple = simple.rsplit(['.', ':']).next().filter(|s| !s.is_empty()).unwrap_or(simple);
    simple.chars().next().is_some_and(|c| c.is_uppercase())
}

/// Ce que la résolution a besoin de savoir du projet.
pub trait Universe {
    type F: Copy + Eq + std::hash::Hash;
    type S: Copy + Eq;
    fn file_by_path(&self, path: &str) -> Option<Self::F>;
    fn files_by_stem(&self, stem: &str) -> Vec<Self::F>;
    /// Toutes les définitions d'un nom (minuscules ASCII), tous fichiers vivants.
    fn defs(&self, name_lower: &str) -> &[Cand<Self::F, Self::S>];
    /// Chemin d'un fichier vivant.
    fn path_of(&self, f: Self::F) -> String;
    /// Ré-exportations d'un fichier vivant (`FileRefs::reexports`).
    fn reexports(&self, f: Self::F) -> Vec<String>;
}

/// Spécificateur Rust (`use`/`mod` d'un fichier `.rs`) ?
fn is_rust(from_path: &str, spec: &str) -> bool {
    from_path.ends_with(".rs") && rust_refs::is_rust_spec(spec)
}

/// Résout un spécificateur Rust : première tige candidate qui existe, puis
/// suivi des ré-exportations de l'élément importé (`pub use`) dans chaque
/// fichier atteint, jusqu'à `MAX_REEXPORT_DEPTH` sauts.
fn resolve_rust<U: Universe>(u: &U, from: U::F, from_path: &str, spec: &str, depth: u32, out: &mut Vec<U::F>) {
    for (stem, item) in rust_refs::candidates(from_path, spec) {
        let hits = u.files_by_stem(&stem);
        if hits.is_empty() {
            continue;
        }
        for t in hits {
            if t == from {
                continue;
            }
            if !out.contains(&t) {
                out.push(t);
            }
            // Suivi même si `t` est déjà importé (par un autre `use` du même
            // fichier) : l'élément, lui, peut être ré-exporté d'ailleurs.
            if let (true, Some(item)) = (depth < rust_refs::MAX_REEXPORT_DEPTH, item.as_deref()) {
                let tp = u.path_of(t);
                for r in u.reexports(t) {
                    let (rspec, exported, _) = rust_refs::parse_reexport(&r);
                    if item == "*" || exported == "*" || exported == item {
                        resolve_rust(u, from, &tp, rspec, depth + 1, out);
                    }
                }
            }
        }
        return;
    }
}

/// Fichiers RÉELLEMENT importés par `from` (chemin `from_path`).
pub fn resolve_imports<'s, U: Universe>(u: &U, from: U::F, from_path: &str, specs: impl Iterator<Item = &'s str>) -> Vec<U::F> {
    let dir = dir_of(from_path);
    let mut out: Vec<U::F> = Vec::new();
    for spec in specs {
        if is_rust(from_path, spec) {
            resolve_rust(u, from, from_path, spec, 0, &mut out);
            continue;
        }
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
fn resolve_call<U: Universe>(
    u: &U,
    name_lower: &str,
    upper: Option<bool>,
    from: U::F,
    imported: &[U::F],
) -> Option<Resolution<U::F, U::S>> {
    let cased: Vec<Cand<U::F, U::S>>;
    let all = match upper {
        // Règle de casse (Rust) : `Foo` (type, variante, constante) ne vise
        // jamais `fn foo`, ni l'inverse — les clés de résolution sont en minuscules.
        Some(up) => {
            cased = u.defs(name_lower).iter().filter(|c| c.upper == up).copied().collect();
            &cased[..]
        }
        None => u.defs(name_lower),
    };
    if all.is_empty() {
        return None;
    }
    // Un prototype d'en-tête (`Decl`) cède la place à la définition (.cpp) quand
    // elle existe ; seul, il reste une cible valable.
    let defined: Vec<Cand<U::F, U::S>>;
    // De même, un bloc `impl Foo` (genre `Class` en Rust, qui porte le nom du
    // type) cède la place à la définition du type (`struct`/`enum`/`type`) :
    // une référence à `Foo` vise la structure, pas l'un de ses `impl`.
    let is_type_def = |k: SymbolKind| matches!(k, SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Type);
    let has_decl = all.iter().any(|c| c.kind == SymbolKind::Decl) && all.iter().any(|c| c.kind != SymbolKind::Decl);
    let has_impl = all.iter().any(|c| c.kind == SymbolKind::Class) && all.iter().any(|c| is_type_def(c.kind));
    let cands: &[Cand<U::F, U::S>] = if has_decl || has_impl {
        defined = all
            .iter()
            .filter(|c| !(has_decl && c.kind == SymbolKind::Decl) && !(has_impl && c.kind == SymbolKind::Class))
            .copied()
            .collect();
        &defined
    } else {
        all
    };
    if upper.is_some() {
        // Rust : plusieurs homonymes dans le fichier même (`build` de deux
        // `impl` voisins) — ambigu, jamais le premier venu.
        let mut local = cands.iter().filter(|c| c.file == from);
        if let Some(first) = local.next() {
            return Some(if local.next().is_none() { Resolution::Found(*first) } else { Resolution::Ambiguous });
        }
    } else if let Some(c) = cands.iter().filter(|c| c.file == from).min_by_key(|c| c.ord) {
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
    /// Règle de casse (fichiers Rust) : un appel à initiale majuscule ne vise
    /// qu'une définition à initiale majuscule, et inversement.
    pub case_class: bool,
}

impl<'a> FileCalls<'a> {
    pub fn new(
        own_names: impl Iterator<Item = &'a str>,
        imported_names: impl Iterator<Item = &'a str>,
        calls: Vec<(&'a str, u32)>,
    ) -> Self {
        let calls_lower = calls.iter().map(|(n, _)| n.to_ascii_lowercase()).collect();
        FileCalls {
            own_lower: own_names.map(call_key).collect(),
            imported_lower: imported_names.map(|n| n.to_ascii_lowercase()).collect(),
            calls,
            calls_lower,
            case_class: false,
        }
    }

    /// Active la règle de casse (fichiers Rust, voir `rust_refs`).
    pub fn with_case_class(mut self, on: bool) -> Self {
        self.case_class = on;
        self
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
    for (i, &(name, cl_line)) in fc.calls.iter().enumerate() {
        if end_line > 0 && !(cl_line >= line && cl_line <= end_line) {
            continue;
        }
        let cl = fc.calls_lower[i].as_str();
        let upper = fc.case_class.then(|| starts_upper(name));
        // (ambigu ?, trouvé ?) — mis en cache par nom (et classe de casse) : la
        // résolution ne dépend que du nom et du fichier appelant.
        let key_upper;
        let key: &str = if upper == Some(true) {
            key_upper = format!("^{}", cl);
            &key_upper
        } else {
            cl
        };
        let res = match cache.get(key) {
            Some(r) => *r,
            None => {
                let r = match resolve_call(u, cl, upper, from, imported_files) {
                    None => None,
                    Some(Resolution::Ambiguous) => Some((true, None)),
                    Some(Resolution::Found(c)) => Some((false, Some(c))),
                };
                cache.insert(key.to_string(), r);
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
    idx: &'a ProjectIndex,
    path_index: FxHashMap<&'a str, usize>,
    stem_index: FxHashMap<String, Vec<usize>>,
    defs: FxHashMap<String, Vec<Cand<usize, (usize, usize)>>>,
}

impl<'a> IndexUniverse<'a> {
    pub fn new(idx: &'a ProjectIndex) -> Self {
        let mut path_index = FxHashMap::default();
        let mut stem_index: FxHashMap<String, Vec<usize>> = FxHashMap::default();
        let mut defs: FxHashMap<String, Vec<Cand<usize, (usize, usize)>>> = FxHashMap::default();
        for (fi, f) in idx.files.iter().enumerate() {
            path_index.insert(f.path.as_str(), fi);
            for k in stem_keys(&f.path) {
                stem_index.entry(k).or_default().push(fi);
            }
            for (si, s) in f.symbols.iter().enumerate() {
                defs.entry(call_key(&s.name)).or_default().push(Cand {
                    file: fi,
                    sym: (fi, si),
                    ord: si as u32,
                    kind: s.kind,
                    upper: starts_upper(&s.name),
                });
            }
        }
        IndexUniverse { idx, path_index, stem_index, defs }
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
    fn path_of(&self, f: usize) -> String {
        self.idx.files[f].path.clone()
    }
    fn reexports(&self, f: usize) -> Vec<String> {
        self.idx.files[f].refs.reexports.clone()
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

    /// C++ : le prototype d'en-tête (`Decl`) cède la place à la définition du .cpp
    /// (même nom qualifié, l'appel ne porte que le dernier segment).
    #[test]
    fn prefere_la_definition_au_prototype() {
        let h = file("src/a/Foo.h", vec![sym("AFoo::Bar", SymbolKind::Decl, 5, 5)], FileRefs::default());
        let c = file("src/a/Foo.cpp", vec![sym("AFoo::Bar", SymbolKind::Method, 10, 20)], FileRefs::default());
        let mut refs = FileRefs::default();
        refs.imports.push("/src/a/Foo".to_string());
        refs.imported_names.push("Bar".to_string());
        refs.calls.push(CallRef { name: "Bar".to_string(), line: 4 });
        let caller = file("src/b/Use.cpp", vec![sym("Use::Run", SymbolKind::Method, 1, 9)], refs);
        assert_eq!(callees(&idx(vec![h, c, caller]), "Use::Run").0, vec!["src/a/Foo.cpp".to_string()]);
        assert_eq!(call_key("AFoo::Bar"), "bar");
        assert_eq!(call_key("useThing"), "usething");
    }

    /// Rust : `impl World` cède la place à `struct World` ; `World` ne vise pas
    /// `fn world` (règle de casse) ; deux `build` dans le fichier même : ambigu.
    #[test]
    fn regles_rust_impl_casse_homonymes_locaux() {
        let mut refs = FileRefs::default();
        for (n, l) in [("World", 21), ("world", 22), ("build", 23)] {
            refs.calls.push(CallRef { name: n.to_string(), line: l });
            refs.imported_names.push(n.to_string());
        }
        let f = file(
            "src/a.rs",
            vec![
                sym("World", SymbolKind::Struct, 1, 3),
                sym("World", SymbolKind::Class, 5, 10),
                sym("build", SymbolKind::Function, 6, 7),
                sym("build", SymbolKind::Function, 8, 9),
                sym("world", SymbolKind::Function, 11, 12),
                sym("f", SymbolKind::Function, 20, 30),
            ],
            refs,
        );
        let p = idx(vec![f]);
        let u = IndexUniverse::new(&p);
        let e = &p.files[0];
        let fc = FileCalls::new(
            e.symbols.iter().map(|s| s.name.as_str()),
            e.refs.imported_names.iter().map(|s| s.as_str()),
            e.refs.calls.iter().map(|c| (c.name.as_str(), c.line)).collect(),
        )
        .with_case_class(true);
        let (out, amb) = resolve_symbol_calls(&u, 0, &[], &fc, 20, 30, &mut FxHashMap::default());
        let got: Vec<(usize, SymbolKind)> = out.iter().map(|c| (c.sym.1, c.kind)).collect();
        assert_eq!(got, vec![(0, SymbolKind::Struct), (4, SymbolKind::Function)]);
        assert_eq!(amb, 1);
        // Sans la règle (autres langages) : `World` et `world` partagent la clé, le
        // premier homonyme du fichier gagne — comportement inchangé.
        let fc2 = FileCalls::new(
            e.symbols.iter().map(|s| s.name.as_str()),
            e.refs.imported_names.iter().map(|s| s.as_str()),
            e.refs.calls.iter().map(|c| (c.name.as_str(), c.line)).collect(),
        );
        let (out2, _) = resolve_symbol_calls(&u, 0, &[], &fc2, 20, 30, &mut FxHashMap::default());
        assert_eq!(out2.iter().map(|c| c.sym.1).collect::<Vec<_>>(), vec![0, 2]);
    }

    #[test]
    fn bases_satisfaites() {
        assert_eq!(bases_satisfied_by("src/hooks/index.ts"), vec!["src/hooks/index.ts", "src/hooks/index", "src/hooks"]);
        assert_eq!(spec_base("src/a", "../b/c"), Some("src/b/c".to_string()));
        assert_eq!(spec_base("src/a", "react"), None);
    }
}
