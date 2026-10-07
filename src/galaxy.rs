//! Export "galaxie" : agrège projets indexés + docs scrapées + infra en un JSON 3D.
//! Remplace claude_os_aggregate.py (Python) — en Rust, 100× plus rapide.
//!
//! Hiérarchie à 3 niveaux pour le viewer :
//!   - noyau IA central (0,0,0)
//!   - branches = CONSTELLATIONS (kind: "code" projet | "doc" scrapée | "infra" VPS),
//!     positionnées sur une sphère (Fibonacci).
//!   - sous-branches = THÈMES/modules (premier dossier significatif du chemin, Docs, Infra…).
//!   - feuilles = nœuds (symboles de code | pages de doc | items d'infra).

use crate::index::ProjectIndex;
use serde::Serialize;
use std::f32::consts::PI;

const PROJECT_COLORS: &[&str] = &["#a855f7", "#06b6d4", "#f59e0b", "#10b981", "#ec4899", "#6366f1", "#14b8a6", "#f43f5e"];
const DOC_COLOR: &str = "#38bdf8"; // bleu clair pour les docs
const INFRA_COLOR: &str = "#22c55e"; // vert pour l'infra
const GALAXY_RADIUS: f32 = 9000.0; // espacement des constellations (assez large pour les sous-boules)
const SUB_ORBIT: f32 = 1000.0; // distance des sous-boules (thèmes) au centre de la constellation
const SUB_SPREAD: f32 = 340.0; // rayon d'une sous-boule (dispersion des nœuds d'un thème)

#[derive(Serialize)]
struct GalaxyNode {
    l: String, // label
    p: usize,  // index de la constellation
    m: usize,  // module/thème
    t: u8,     // type : 0=code, 1=doc, 2=infra
    x: f32,
    y: f32,
    z: f32,
    s: f32, // taille
    c: String,
    d: u32,    // degré (placeholder = 1)
    f: String, // fichier:ligne ou URL
}

#[derive(Serialize)]
struct ProjectStat {
    name: String,
    kind: String, // "code" | "doc" | "infra"
    nodes: usize,
    color: String,
    center: [f32; 3],
}

#[derive(Serialize)]
struct Galaxy {
    v: u32,
    core: [f32; 3],
    projects: Vec<ProjectStat>,
    modules: Vec<String>,
    stats: Stats,
    nodes: Vec<GalaxyNode>,
    edges: Vec<[u32; 2]>,
}

#[derive(Serialize)]
struct Stats {
    nodes: usize,
    edges: usize,
    projects: usize,
    code: usize,
    docs: usize,
    infra: usize,
}

fn project_center(i: usize, n: usize) -> [f32; 3] {
    if n <= 1 {
        return [GALAXY_RADIUS, 0.0, 0.0];
    }
    let phi = (1.0 - 2.0 * (i as f32 + 0.5) / n as f32).acos();
    let theta = PI * (1.0 + 5.0_f32.sqrt()) * i as f32;
    [GALAXY_RADIUS * phi.sin() * theta.cos(), GALAXY_RADIUS * phi.sin() * theta.sin(), GALAXY_RADIUS * phi.cos()]
}

/// Hash FNV déterministe d'une chaîne.
fn fnv(seed: &str) -> u32 {
    let mut h: u32 = 2166136261;
    for b in seed.bytes() {
        h = (h ^ b as u32).wrapping_mul(16777619);
    }
    h
}

/// Position pseudo-aléatoire déterministe dans une sphère de rayon `spread`.
fn jitter_r(seed: &str, spread: f32) -> [f32; 3] {
    let h = fnv(seed);
    let a = (h % 1000) as f32 / 1000.0 * 2.0 * PI;
    let b = ((h >> 10) % 1000) as f32 / 1000.0 * PI;
    let r = spread * (((h >> 20) % 1000) as f32 / 1000.0).cbrt();
    [r * b.sin() * a.cos(), r * b.sin() * a.sin(), r * b.cos()]
}

/// Sous-centre d'un THÈME : positionné sur une sphère AUTOUR du centre de sa
/// constellation (Fibonacci sur l'index du thème). Crée les « sous-boules » :
/// constellation = grappe de boules-thèmes, pas une seule boule géante.
fn theme_subcenter(center: &[f32; 3], theme_idx: usize, n_themes: usize) -> [f32; 3] {
    if n_themes <= 1 {
        return *center;
    }
    let i = theme_idx as f32;
    let phi = (1.0 - 2.0 * (i + 0.5) / n_themes as f32).acos();
    let golden = PI * (1.0 + 5.0_f32.sqrt());
    let theta = golden * i;
    // Rayon de l'orbite des thèmes autour de la constellation.
    let orbit = SUB_ORBIT;
    [center[0] + orbit * phi.sin() * theta.cos(), center[1] + orbit * phi.sin() * theta.sin(), center[2] + orbit * phi.cos()]
}

/// Thème/module déduit du chemin (CODE) : le premier dossier significatif, après
/// les dossiers génériques (`src/components/billing/x.tsx` → « Billing »). Sans
/// dossier significatif : un thème par nature (Docs, UI, Hooks, Core, Infra, Root).
fn module_of(path: &str) -> String {
    const GENERIQUES: &[&str] = &[
        "src",
        "lib",
        "app",
        "apps",
        "packages",
        "pkg",
        "internal",
        "crates",
        "source",
        "sources",
        "components",
        "pages",
        "routes",
        "views",
        "hooks",
        "utils",
        "util",
        "services",
        "features",
        "modules",
        "shared",
        "common",
        "core",
        "include",
        "main",
        "java",
        "kotlin",
        "python",
    ];
    let p = path.replace('\\', "/").to_ascii_lowercase();
    if p.starts_with("docs/") || p.ends_with(".md") {
        return "Docs".into();
    }
    let dirs: Vec<&str> = p.split('/').collect();
    let dirs = &dirs[..dirs.len().saturating_sub(1)];
    if let Some(d) = dirs.iter().find(|d| !d.is_empty() && !d.starts_with('.') && !GENERIQUES.contains(d)) {
        return capitalize(d);
    }
    if p.contains("/components/") {
        "UI"
    } else if p.contains("/hooks/") {
        "Hooks"
    } else if p.contains("scripts") || p.contains("infra") {
        "Infra"
    } else if dirs.is_empty() {
        "Root"
    } else {
        "Core"
    }
    .into()
}

/// Thème d'une page de doc = sa SECTION (1er segment significatif du nom de fichier
/// slugifié, ex "docs_guides_self_hosting_docker" → "guides").
fn doc_section(fname: &str) -> String {
    let parts: Vec<&str> = fname.split('_').filter(|s| !s.is_empty()).collect();
    // saute les préfixes génériques (docs, en, en-us, www…)
    for p in &parts {
        let pl = p.to_ascii_lowercase();
        if !matches!(pl.as_str(), "docs" | "doc" | "en" | "us" | "www" | "guide" | "index" | "reference") {
            return capitalize(p);
        }
    }
    parts.first().map(|s| capitalize(s)).unwrap_or_else(|| "General".into())
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Construit la galaxie (code + docs + infra) et l'écrit en JSON.
pub fn build_galaxy(indexes: &[ProjectIndex], out_path: &std::path::Path) -> std::io::Result<(usize, usize)> {
    // Constellations = projets de code + docs scrapées. L'infra n'est PAS une
    // constellation : elle se rattache à son projet comme un thème (cf plus bas).
    let docs = list_scraped_docs();
    let infra = list_infra_docs();
    let n_const = indexes.len() + docs.len();

    let mut nodes = Vec::new();
    let mut projects = Vec::new();
    let mut modules: Vec<String> = Vec::new();
    let mut mod_index = std::collections::HashMap::new();
    let mut color_i = 0usize;
    let mut ci = 0usize; // index global de constellation
    let (mut n_docs, mut n_infra) = (0usize, 0usize);

    let get_mod = |modules: &mut Vec<String>, mod_index: &mut std::collections::HashMap<String, usize>, m: &str| -> usize {
        *mod_index.entry(m.to_string()).or_insert_with(|| {
            modules.push(m.to_string());
            modules.len() - 1
        })
    };

    // ── 1. CODE : un projet = une constellation, ÉCLATÉE en sous-boules par thème.
    //    PASSE A : recense les thèmes du projet → un sous-centre (orbite) chacun.
    //    PASSE B : chaque nœud est jitteré autour du sous-centre de SON thème.
    //    Son INFRA (VPS) y est rattachée comme un thème de plus.
    for idx in indexes.iter() {
        let center = project_center(ci, n_const);
        let color = PROJECT_COLORS[color_i % PROJECT_COLORS.len()].to_string();
        color_i += 1;
        let mut count = 0;

        // PASSE A : thèmes distincts de ce projet (ordre stable).
        let mut local_themes: Vec<String> = Vec::new();
        for f in &idx.files {
            let m = module_of(&f.path);
            if !local_themes.contains(&m) {
                local_themes.push(m);
            }
        }
        let infra_themes: Vec<&str> = infra.iter().filter(|(n, _)| infra_belongs_to(n, &idx.name)).map(|_| "Infra").collect();
        let n_themes = local_themes.len() + if infra_themes.is_empty() { 0 } else { 1 };
        let sub_of = |m: &str| -> [f32; 3] {
            let ti = local_themes.iter().position(|x| *x == m).unwrap_or(0);
            theme_subcenter(&center, ti, n_themes)
        };

        // PASSE B : positionne les nœuds dans leur sous-boule.
        for f in &idx.files {
            let module = module_of(&f.path);
            let mi = get_mod(&mut modules, &mut mod_index, &module);
            let sc = sub_of(&module);
            for s in &f.symbols {
                let id = format!("{}::{}::{}", idx.name, f.path, s.name);
                let j = jitter_r(&id, SUB_SPREAD);
                nodes.push(GalaxyNode {
                    l: s.name.clone(),
                    p: ci,
                    m: mi,
                    t: 0,
                    x: sc[0] + j[0],
                    y: sc[1] + j[1],
                    z: sc[2] + j[2],
                    s: 1.0 + (s.tokens.len() as f32).ln_1p(),
                    c: color.clone(),
                    d: 1,
                    f: format!("{}:{}", f.path, s.line),
                });
                count += 1;
            }
        }
        // Infra rattachée à CE projet : sa propre sous-boule (dernier thème).
        let mi_infra = get_mod(&mut modules, &mut mod_index, "Infra");
        let infra_sc = theme_subcenter(&center, n_themes.saturating_sub(1), n_themes);
        for (snap_name, items) in infra.iter().filter(|(n, _)| infra_belongs_to(n, &idx.name)) {
            for item in items {
                let id = format!("infra::{}::{}", snap_name, item);
                let j = jitter_r(&id, SUB_SPREAD * 0.7);
                nodes.push(GalaxyNode {
                    l: item.clone(),
                    p: ci,
                    m: mi_infra,
                    t: 2,
                    x: infra_sc[0] + j[0],
                    y: infra_sc[1] + j[1],
                    z: infra_sc[2] + j[2],
                    s: 2.4,
                    c: INFRA_COLOR.into(),
                    d: 1,
                    f: format!("infra/{}.md", snap_name),
                });
                count += 1;
                n_infra += 1;
            }
        }
        projects.push(ProjectStat { name: idx.name.clone(), kind: "code".into(), nodes: count, color, center });
        eprintln!("  + [code] {}: {} nodes (code + attached infra)", idx.name, count);
        ci += 1;
    }
    let n_code = nodes.iter().filter(|n| n.t == 0).count();

    // ── 2. DOCS : une doc scrapée = une constellation, ÉCLATÉE en sous-boules par
    //    SECTION d'URL (mêmes 2 passes que le code).
    for (name, pages) in &docs {
        let center = project_center(ci, n_const);
        let mut count = 0;
        // PASSE A : sections distinctes.
        let mut sections: Vec<String> = Vec::new();
        for page in pages {
            let sec = doc_section(page);
            if !sections.contains(&sec) {
                sections.push(sec);
            }
        }
        let n_sec = sections.len().max(1);
        // PASSE B : positionne par sous-boule de section.
        for page in pages {
            let section = doc_section(page);
            let mi = get_mod(&mut modules, &mut mod_index, &format!("doc:{}", section));
            let ti = sections.iter().position(|s| *s == section).unwrap_or(0);
            let sc = theme_subcenter(&center, ti, n_sec);
            let id = format!("doc::{}::{}", name, page);
            let j = jitter_r(&id, SUB_SPREAD);
            nodes.push(GalaxyNode {
                l: page.clone(),
                p: ci,
                m: mi,
                t: 1,
                x: sc[0] + j[0],
                y: sc[1] + j[1],
                z: sc[2] + j[2],
                s: 1.6,
                c: DOC_COLOR.into(),
                d: 1,
                f: format!("docs/{}/{}.md", name, page),
            });
            count += 1;
        }
        projects.push(ProjectStat { name: name.clone(), kind: "doc".into(), nodes: count, color: DOC_COLOR.into(), center });
        n_docs += count;
        eprintln!("  + [doc] {} : {} pages", name, count);
        ci += 1;
    }

    // (L'infra n'est PAS une constellation séparée : elle est rattachée à son projet
    //  comme un thème "Infra/VPS", cf bloc CODE ci-dessus. Un snapshot infra non
    //  rattachable à un projet indexé est ignoré du graphe — il reste cherchable.)

    let galaxy = Galaxy {
        v: 2,
        core: [0.0, 0.0, 0.0],
        stats: Stats { nodes: nodes.len(), edges: 0, projects: n_const, code: n_code, docs: n_docs, infra: n_infra },
        projects,
        modules,
        nodes,
        edges: Vec::new(),
    };

    let json = serde_json::to_string(&galaxy).map_err(std::io::Error::other)?;
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(out_path, json)?;
    Ok((galaxy.stats.nodes, n_const))
}

/// Liste les docs scrapées : (nom, [noms de pages sans .md]). Lit ~/.cortex/docs/.
fn list_scraped_docs() -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    let home = crate::scrape::docs_home();
    if let Ok(entries) = std::fs::read_dir(&home) {
        for e in entries.flatten() {
            if e.path().is_dir() {
                let name = e.file_name().to_string_lossy().to_string();
                let mut pages = Vec::new();
                if let Ok(files) = std::fs::read_dir(e.path()) {
                    for f in files.flatten() {
                        let p = f.path();
                        if p.extension().map(|x| x == "md").unwrap_or(false) {
                            if let Some(stem) = p.file_stem() {
                                let s = stem.to_string_lossy().to_string();
                                if !s.starts_with('_') {
                                    pages.push(s);
                                }
                            }
                        }
                    }
                }
                if !pages.is_empty() {
                    out.push((name, pages));
                }
            }
        }
    }
    out
}

/// Vrai si un snapshot d'infra (ex "web_monprojet") appartient à un projet (ex
/// "MonProjet"). Convention (`cortex infra`) : `<préfixe>_<projet>` en minuscules.
fn infra_belongs_to(snap_name: &str, project: &str) -> bool {
    let s = snap_name.to_ascii_lowercase();
    let p = project.to_ascii_lowercase();
    s == p || s.ends_with(&format!("_{p}"))
}

/// Liste les snapshots d'infra : (nom_vps, [items extraits du markdown]). Lit ~/.cortex/infra/.
fn list_infra_docs() -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    let home = crate::index::cortex_home().join("infra");
    if let Ok(entries) = std::fs::read_dir(&home) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().map(|x| x == "md").unwrap_or(false) {
                let name = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                // Items = titres markdown (## ...) du snapshot.
                let mut items = Vec::new();
                if let Ok(content) = std::fs::read_to_string(&p) {
                    for line in content.lines() {
                        let t = line.trim();
                        if let Some(h) = t.strip_prefix("## ") {
                            items.push(h.trim().to_string());
                        }
                    }
                }
                if items.is_empty() {
                    items.push(name.clone());
                }
                out.push((name, items));
            }
        }
    }
    out
}
