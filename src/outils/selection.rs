//! `impact --tests [--parcours]` : pour un changement (fichiers modifiés d'après git, ou
//! identifiants donnés), les fichiers de TEST à lancer, classés, et les recettes Spectra
//! (`.spectra/recipes/*.json`) dont le parcours d'écran touche ces zones.
//!
//! Classement des tests : proximité dans le graphe d'imports (le test importe le fichier
//! changé : 4 ; via un intermédiaire : 1,5 ; deux : 0,5), même nom ou même dossier
//! (bonus), spécificité (un test qui n'importe que 3 fichiers pèse plus qu'un test
//! qui en importe 60), test lui-même modifié (en tête).

use super::overlay;
use super::{noeud_fichier, resoudre, Out, Resolu};
use crate::atlas::Handle;
use crate::search::is_test_path;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Paramètres de `impact --tests`.
#[derive(Debug, Clone, Default)]
pub struct Params {
    /// Identifiants, chemins ou noms (vide : fichiers modifiés d'après git).
    pub cibles: Vec<String>,
    /// Fichiers modifiés depuis cette référence git (`HEAD~3`, une branche…), travail en cours compris.
    pub depuis: Option<String>,
    /// Fichiers modifiés par ce commit.
    pub commit: Option<String>,
    pub parcours: bool,
    /// Nombre maximal de tests rendus.
    pub max: usize,
    /// Sortie brute : un chemin de test par ligne, rien d'autre.
    pub liste: bool,
}

/// Un test candidat.
struct Candidat {
    score: f32,
    raisons: Vec<&'static str>,
}

const PROFONDEUR_TESTS: usize = 3;
/// Au-delà, un fichier importé par tant de fichiers est un « hub » : on ne remonte plus à travers lui.
const HUB: usize = 120;
const POIDS_PROFONDEUR: [f32; 3] = [4.0, 1.5, 0.5];

fn dossier(p: &str) -> &str {
    p.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

/// Dossier « logique » d'un test (sans `__tests__`, `tests`, `test`).
fn dossier_source_du_test(p: &str) -> String {
    let d = dossier(p);
    for suf in ["/__tests__", "/tests", "/test"] {
        if let Some(x) = d.strip_suffix(suf) {
            return x.to_string();
        }
    }
    d.to_string()
}

fn tige(p: &str) -> String {
    let n = p.rsplit('/').next().unwrap_or(p);
    n.split('.').next().unwrap_or(n).to_ascii_lowercase()
}

/// Fichiers (relatifs à la racine) modifiés d'après git, pour ce projet.
fn fichiers_git(h: &Handle, p: &Params) -> Vec<String> {
    let root = Path::new(h.root());
    let lire = |args: &[&str]| -> Vec<String> {
        overlay::git(root, args)
            .map(|b| String::from_utf8_lossy(&b).lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect())
            .unwrap_or_default()
    };
    if let Some(c) = &p.commit {
        return lire(&["show", "--name-only", "--format=", "--relative", c, "--"]);
    }
    if let Some(r) = &p.depuis {
        let mut v = lire(&["diff", "--name-only", "--relative", r, "--"]);
        v.extend(lire(&["ls-files", "--others", "--exclude-standard"]));
        v.sort();
        v.dedup();
        return v;
    }
    overlay::charger(h).fichiers.into_iter().filter(|(_, e)| *e != overlay::Etat::Supprime).map(|(p, _)| p).collect()
}

/// Graine : (projet, fichier de l'atlas).
fn graines(handles: &[Handle], p: &Params) -> (Vec<(usize, u32)>, Vec<String>) {
    let mut out: Vec<(usize, u32)> = Vec::new();
    let mut inconnus: Vec<String> = Vec::new();
    if !p.cibles.is_empty() {
        for c in &p.cibles {
            match resoudre(handles, c) {
                Resolu::Trouve { h, cible, .. } => {
                    let g = noeud_fichier(h, cible);
                    let i = handles.iter().position(|x| std::ptr::eq(x, h)).unwrap_or(0);
                    if !out.contains(&(i, g)) {
                        out.push((i, g));
                    }
                }
                _ => inconnus.push(c.clone()),
            }
        }
        return (out, inconnus);
    }
    for (i, h) in handles.iter().enumerate() {
        for f in fichiers_git(h, p) {
            match h.file_by_path(&f) {
                Some(g) => out.push((i, g)),
                None => inconnus.push(f),
            }
        }
    }
    (out, inconnus)
}

/// Tests dont le TEXTE cite un fichier changé (chemin, ou nom de fichier distinctif) : les tests
/// « statiques » qui lisent une source (`readFileSync` d'un chemin) n'importent rien.
fn tests_qui_citent(h: &Handle, seeds: &[u32]) -> Vec<(u32, usize)> {
    use rayon::prelude::*;
    let mut motifs: Vec<String> = Vec::new();
    let mut de_seed: Vec<usize> = Vec::new();
    for (i, &s) in seeds.iter().enumerate() {
        let p = h.path_of(s);
        if is_test_path(p) {
            continue;
        }
        let sans_ext = p.rsplit_once('.').map(|(a, _)| a).unwrap_or(p);
        let segs: Vec<&str> = p.split('/').collect();
        let base = segs.last().copied().unwrap_or("");
        let mut ajoute = |m: String| {
            if m.len() >= 8 && !motifs.contains(&m) {
                motifs.push(m);
                de_seed.push(i);
            }
        };
        ajoute(sans_ext.to_string());
        let stem = base.rsplit_once('.').map(|(a, _)| a).unwrap_or(base);
        if matches!(stem, "index" | "mod" | "main" | "lib") && segs.len() >= 2 {
            ajoute(format!("{}/{}", segs[segs.len() - 2], base));
        } else {
            ajoute(base.to_string());
        }
    }
    if motifs.is_empty() {
        return Vec::new();
    }
    let Ok(ac) = aho_corasick::AhoCorasick::new(&motifs) else { return Vec::new() };
    let root = Path::new(h.root());
    let tests: Vec<(u32, &str)> = h
        .live_files()
        .map(|r| (r.g, r.name()))
        .filter(|(_, p)| is_test_path(p) && crate::lang::Lang::from_path(p).is_indexable())
        .collect();
    tests
        .par_iter()
        .filter_map(|(g, p)| {
            let t = std::fs::read_to_string(root.join(p)).ok()?;
            let mut vus: HashSet<usize> = HashSet::new();
            for m in ac.find_overlapping_iter(&t) {
                vus.insert(de_seed[m.pattern().as_usize()]);
            }
            (!vus.is_empty()).then_some((*g, vus.len()))
        })
        .collect()
}

/// Tests classés pour les graines d'UN projet.
fn classer(h: &Handle, seeds: &[u32]) -> Vec<(u32, Candidat)> {
    let mentions = tests_qui_citent(h, seeds);
    let mut cand: HashMap<u32, Candidat> = HashMap::new();
    let mut ajouter = |t: u32, s: f32, why: &'static str| {
        let e = cand.entry(t).or_insert(Candidat { score: 0.0, raisons: Vec::new() });
        e.score += s;
        if !e.raisons.contains(&why) {
            e.raisons.push(why);
        }
    };
    for &s in seeds {
        let sp = h.path_of(s).to_string();
        if is_test_path(&sp) {
            ajouter(s, 100.0, "changed");
            continue;
        }
        for t in super::tests_par_nom(h, &sp, None) {
            ajouter(t, 6.0, "same name");
        }
        // Remontée dans le graphe d'imports.
        let mut vus: HashSet<u32> = HashSet::from([s]);
        let mut front = vec![s];
        for (d, poids) in POIDS_PROFONDEUR.iter().enumerate().take(PROFONDEUR_TESTS) {
            let mut suivant = Vec::new();
            for &f in &front {
                let imps = h.importers(f);
                let hub = imps.len() > HUB;
                for i in imps {
                    let ip = h.path_of(i);
                    if is_test_path(ip) {
                        let spec = 1.0 / (1.0 + 0.15 * h.imports_raw(i).len() as f32);
                        let mut sc = poids * spec;
                        let (ds, di) = (dossier(&sp), dossier_source_du_test(ip));
                        if ds == di || di.starts_with(ds) && !ds.is_empty() {
                            sc += 1.5;
                        }
                        if tige(ip).contains(&tige(&sp)) {
                            sc += 2.0;
                        }
                        ajouter(i, sc, ["direct import", "via 1 file", "via 2 files"][d]);
                    } else if !hub && vus.insert(i) {
                        suivant.push(i);
                    }
                }
            }
            if suivant.is_empty() {
                break;
            }
            front = suivant;
        }
    }
    for (t, n) in mentions {
        ajouter(t, 3.0 * n.min(2) as f32, "mentions file");
    }
    let mut v: Vec<(u32, Candidat)> = cand.into_iter().collect();
    for (_, c) in v.iter_mut() {
        const ORDRE: [&str; 6] = ["changed", "same name", "mentions file", "direct import", "via 1 file", "via 2 files"];
        c.raisons.sort_by_key(|r| ORDRE.iter().position(|x| x == r).unwrap_or(9));
    }
    v.sort_by(|a, b| {
        b.1.score.partial_cmp(&a.1.score).unwrap_or(std::cmp::Ordering::Equal).then_with(|| h.path_of(a.0).cmp(h.path_of(b.0)))
    });
    v
}

/// Crate Cargo (dossier du `Cargo.toml` le plus proche) d'un fichier `.rs`.
fn crate_de(root: &Path, rel: &str) -> Option<String> {
    let mut d = Path::new(rel).parent();
    while let Some(x) = d {
        if root.join(x).join("Cargo.toml").is_file() {
            return Some(x.to_string_lossy().replace('\\', "/"));
        }
        if x.as_os_str().is_empty() {
            break;
        }
        d = x.parent();
    }
    None
}

// ─── Parcours Spectra ───────────────────────────────────────────────────────

/// Chaînes visibles d'une recette (textes attendus, noms cliqués) et routes visitées.
fn signaux_recette(v: &serde_json::Value) -> (Vec<String>, Vec<String>) {
    let (mut textes, mut routes) = (Vec::new(), Vec::new());
    let mut ajoute_route = |u: &str| {
        let chemin = match url::Url::parse(u) {
            Ok(x) => x.path().to_string(),
            Err(_) => u.split(['?', '#']).next().unwrap_or("").to_string(),
        };
        if chemin.len() > 1 && !routes.contains(&chemin) {
            routes.push(chemin);
        }
    };
    if let Some(u) = v.get("start_url").and_then(|x| x.as_str()) {
        ajoute_route(u);
    }
    for s in v.get("steps").and_then(|x| x.as_array()).into_iter().flatten() {
        let op = s.get("op").and_then(|x| x.as_str()).unwrap_or("");
        match op {
            "nav" => {
                if let Some(u) = s.get("url").and_then(|x| x.as_str()) {
                    ajoute_route(u);
                }
            }
            "expect" => {
                if let Some(t) = s.get("v").and_then(|x| x.as_str()) {
                    textes.push(t.to_string());
                }
            }
            "click" | "fill" | "hover" | "select" => {
                if let Some(t) = s.get("t").and_then(|t| t.get("name")).and_then(|x| x.as_str()) {
                    textes.push(t.to_string());
                }
            }
            _ => {}
        }
    }
    textes.retain(|t| t.chars().count() >= 4 && t.chars().count() <= 90);
    textes.sort();
    textes.dedup();
    (textes, routes)
}

/// Recettes qui touchent le changement : (nom, distance, fichiers d'écran reconnus).
pub(super) fn recettes_touchees(h: &Handle, seeds: &[u32]) -> Vec<(String, usize, usize)> {
    use rayon::prelude::*;
    let dir = Path::new(h.root()).join(".spectra").join("recipes");
    let Ok(rd) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut recettes: Vec<(String, Vec<String>, Vec<String>)> = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "json") {
            if let Some(v) = std::fs::read_to_string(&p).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok()) {
                let nom = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                let (t, r) = signaux_recette(&v);
                recettes.push((nom, t, r));
            }
        }
    }
    if recettes.is_empty() {
        return Vec::new();
    }
    // Un seul passage sur les fichiers de code d'interface avec tous les motifs.
    let mut motifs: Vec<String> = Vec::new();
    for (_, t, r) in &recettes {
        motifs.extend(t.iter().cloned());
        for x in r {
            for q in ['"', '\'', '`'] {
                motifs.push(format!("{q}{x}{q}"));
            }
        }
    }
    motifs.sort();
    motifs.dedup();
    let Ok(ac) = aho_corasick::AhoCorasick::new(&motifs) else { return Vec::new() };
    let root = Path::new(h.root());
    let fichiers: Vec<(u32, &str)> = h
        .live_files()
        .map(|r| (r.g, r.name()))
        .filter(|(_, p)| {
            let l = p.to_ascii_lowercase();
            (l.starts_with("src/") && (l.ends_with(".tsx") || l.ends_with(".ts") || l.ends_with(".jsx") || l.ends_with(".js")))
                && !is_test_path(p)
        })
        .collect();
    let touches: Vec<(u32, Vec<usize>)> = fichiers
        .par_iter()
        .filter_map(|(g, p)| {
            let t = std::fs::read_to_string(root.join(p)).ok()?;
            let mut ids: Vec<usize> = ac.find_overlapping_iter(&t).map(|m| m.pattern().as_usize()).collect();
            ids.sort_unstable();
            ids.dedup();
            (!ids.is_empty()).then_some((*g, ids))
        })
        .collect();
    let mut par_motif: Vec<Vec<u32>> = vec![Vec::new(); motifs.len()];
    for (g, ids) in &touches {
        for &i in ids {
            par_motif[i].push(*g);
        }
    }
    // Remontée depuis les fichiers changés : qui dépend d'eux (distance en sauts d'imports).
    let mut dist: HashMap<u32, usize> = HashMap::new();
    let mut front: Vec<u32> = Vec::new();
    for &s in seeds {
        dist.insert(s, 0);
        front.push(s);
    }
    for d in 1..=4usize {
        let mut suivant = Vec::new();
        for &f in &front {
            let imps = h.importers(f);
            if imps.len() > 400 {
                continue;
            }
            for i in imps {
                if dist.len() < 20_000 && !dist.contains_key(&i) {
                    dist.insert(i, d);
                    suivant.push(i);
                }
            }
        }
        front = suivant;
    }
    let idx = |m: &str| motifs.binary_search(&m.to_string()).ok();
    let mut out = Vec::new();
    for (nom, textes, routes) in &recettes {
        let mut ecran: HashSet<u32> = HashSet::new();
        for t in textes {
            if let Some(i) = idx(t) {
                // Texte trop générique (« Annuler ») : sans valeur d'identification.
                if par_motif[i].len() <= 12 {
                    ecran.extend(par_motif[i].iter().copied());
                }
            }
        }
        for r in routes {
            for q in ['"', '\'', '`'] {
                if let Some(i) = idx(&format!("{q}{r}{q}")) {
                    if par_motif[i].len() <= 8 {
                        ecran.extend(par_motif[i].iter().copied());
                    }
                }
            }
        }
        let meilleur = ecran.iter().filter_map(|g| dist.get(g)).min();
        if let Some(&d) = meilleur {
            out.push((nom.clone(), d, ecran.len()));
        }
    }
    out.sort_by(|a, b| a.1.cmp(&b.1).then(b.2.cmp(&a.2)).then(a.0.cmp(&b.0)));
    out
}

// ─── Sortie ─────────────────────────────────────────────────────────────────

fn commande_tests(h: &Handle, tests: &[&str]) -> String {
    if tests.is_empty() {
        return String::new();
    }
    let pkg = std::fs::read_to_string(Path::new(h.root()).join("package.json")).unwrap_or_default();
    let base = if pkg.contains("\"test:resume\"") { "npm run test:resume --" } else { "npx vitest run" };
    format!("{} {}", base, tests.join(" "))
}

/// `impact --tests` : tests à lancer (et parcours si demandé).
pub fn tests_a_lancer(handles: &[Handle], p: &Params, budget: usize) -> String {
    let (graines, inconnus) = graines(handles, p);
    if graines.is_empty() {
        if p.liste {
            return String::new();
        }
        let raison = if p.cibles.is_empty() && p.commit.is_none() && p.depuis.is_none() {
            "no uncommitted file known to the index (give identifiers, --depuis <ref> or --commit <sha>)"
        } else {
            "nothing resolved"
        };
        return format!(
            "(cortex) {}{}\n",
            raison,
            if inconnus.is_empty() { String::new() } else { format!(": {}", super::liste(&inconnus, 5)) }
        );
    }
    let max = if p.max == 0 { 30 } else { p.max };
    let mut tous: Vec<String> = Vec::new();
    let mut out = Out::new(budget);
    for (hi, h) in handles.iter().enumerate() {
        let seeds: Vec<u32> = graines.iter().filter(|(i, _)| *i == hi).map(|x| x.1).collect();
        if seeds.is_empty() {
            continue;
        }
        let classes = classer(h, &seeds);
        let total = classes.len();
        let gardes: Vec<&(u32, Candidat)> = classes.iter().take(max).collect();
        let chemins: Vec<&str> = gardes.iter().map(|(g, _)| h.path_of(*g)).collect();
        if p.liste {
            tous.extend(chemins.iter().map(|s| s.to_string()));
            continue;
        }
        let noms: Vec<String> = seeds.iter().map(|&s| h.path_of(s).to_string()).collect();
        let mut multi = String::new();
        if handles.len() > 1 {
            multi = format!("[{}] ", h.project);
        }
        out.ligne(&format!(
            "{}tests to run: {}{} for {} changed file(s): {}",
            multi,
            gardes.len(),
            if total > gardes.len() { format!(" of {} (cap --max {})", total, max) } else { String::new() },
            seeds.len(),
            super::liste(&noms, 4)
        ));
        for (g, c) in &gardes {
            out.ligne(&format!("  {} — {}", h.path_of(*g), c.raisons.iter().take(2).copied().collect::<Vec<_>>().join(", ")));
        }
        let cmd = commande_tests(h, &chemins.iter().copied().filter(|c| !c.ends_with(".rs")).collect::<Vec<_>>());
        if !cmd.is_empty() {
            out.ligne(&format!("run: {cmd}"));
        }
        // Rust : tests en ligne (`#[cfg(test)]`) et crate à lancer.
        let root = Path::new(h.root());
        let mut crates: Vec<(String, Vec<String>)> = Vec::new();
        for &s in &seeds {
            let sp = h.path_of(s);
            if sp.ends_with(".rs") {
                if let Some(c) = crate_de(root, sp) {
                    let stem = sp.rsplit('/').next().unwrap_or(sp).trim_end_matches(".rs");
                    let filtre = if matches!(stem, "mod" | "lib" | "main") { String::new() } else { stem.to_string() };
                    match crates.iter_mut().find(|(x, _)| *x == c) {
                        Some((_, f)) => {
                            if !f.contains(&filtre) {
                                f.push(filtre)
                            }
                        }
                        None => crates.push((c, vec![filtre])),
                    }
                }
            }
        }
        for (c, f) in &crates {
            let filtres: Vec<&String> = f.iter().filter(|x| !x.is_empty()).collect();
            let cmd = format!("cargo test --manifest-path {}/Cargo.toml", c);
            // `cargo test` ne prend qu'un filtre : plusieurs fichiers = tout le crate.
            out.ligne(&format!(
                "run: {}{}",
                cmd,
                if filtres.len() == 1 && f.len() == 1 { format!(" {}", filtres[0]) } else { String::new() }
            ));
        }
        if p.parcours {
            let rec = recettes_touchees(h, &seeds);
            if rec.is_empty() {
                out.ligne(&format!(
                    "parcours: no Spectra recipe ({}/.spectra/recipes) touches these files",
                    h.root().replace('\\', "/").rsplit('/').next().unwrap_or("")
                ));
            } else {
                let liste: Vec<String> = rec.iter().take(8).map(|(n, d, _)| format!("{n} (d{d})")).collect();
                out.ligne(&format!("parcours: {} Spectra recipe(s), d = import hops from the change: {}", rec.len(), liste.join(", ")));
                out.ligne(&format!(
                    "replay: {}",
                    rec.iter().take(8).map(|(n, _, _)| format!("browser_run_recipe {n}")).collect::<Vec<_>>().join(" ; ")
                ));
            }
        }
    }
    if p.liste {
        let mut vu = HashSet::new();
        tous.retain(|t| vu.insert(t.clone()));
        return tous.join("\n") + if tous.is_empty() { "" } else { "\n" };
    }
    out.fin(Some("impact <id> (callers) · peek <test log>".into()))
}

/// Tests classés pour des fichiers donnés (chemins relatifs) — pour le banc.
pub fn classement_tests(handles: &[Handle], fichiers: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for h in handles {
        let seeds: Vec<u32> = fichiers.iter().filter_map(|f| h.file_by_path(f)).collect();
        if seeds.is_empty() {
            continue;
        }
        out.extend(classer(h, &seeds).iter().map(|(g, _)| h.path_of(*g).to_string()));
    }
    out
}
