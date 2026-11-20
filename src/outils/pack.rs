//! `pack "<tâche>"` : le contexte MINIMAL utile à une tâche, sous un budget de jetons.
//!
//! Une description de tâche en entrée ; en sortie, en markdown compact et avec des
//! identifiants réutilisables : les fichiers probables (texte de la tâche contre
//! l'atlas, PLUS les fichiers qui ont déjà changé avec eux dans l'historique git), leurs
//! symboles clés avec la carte d'appelants, les tests liés, les docs, les changements
//! récents liés et, si le projet en a, les parcours d'écran (recettes Spectra).
//!
//! Classement des fichiers : score texte (BM25F de `search`, trois meilleurs symboles du
//! fichier pondérés 1 / 0,5 / 0,25) + co-changement (fichiers qui ont été modifiés dans les
//! mêmes commits que les fichiers de tête, commits de plus de 30 fichiers ignorés).

use super::overlay;
use super::{court, entete, liste, role, tests_par_nom, Out};
use crate::atlas::Handle;
use crate::lang::Lang;
use crate::search::is_test_path;
use crate::symbol::SymbolKind;
use std::collections::{HashMap, HashSet};
use std::path::Path;

pub const BUDGET_DEFAUT: usize = 3000;
/// Commits d'historique lus pour le co-changement.
const COMMITS_LUS: usize = 3000;
/// Commits plus gros que cela : renommages, formatage, versions (pas un signal de lien).
const COMMIT_MAX_FICHIERS: usize = 30;

#[derive(Debug, Clone)]
pub struct Params {
    pub tache: String,
    pub budget: usize,
    /// Historique limité aux commits qui précèdent celui-ci (banc : pas de fuite de la solution).
    pub avant: Option<String>,
}

/// Poids du classement (réglables par variables d'environnement pour le banc).
#[derive(Debug, Clone, Copy)]
pub struct Poids {
    pub cochange: f32,
    pub voisin: f32,
    pub graines: usize,
    /// Fichier souvent modifié récemment (a priori de changement).
    pub chaud: f32,
    /// Même dossier qu'un fichier de tête.
    pub dossier: f32,
}

impl Poids {
    pub fn defaut() -> Poids {
        let f = |k: &str, d: f32| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
        Poids {
            cochange: f("CORTEX_PACK_CO", 0.6),
            voisin: f("CORTEX_PACK_VOISIN", 0.12),
            graines: f("CORTEX_PACK_GRAINES", 5.0) as usize,
            chaud: f("CORTEX_PACK_CHAUD", 0.1),
            dossier: f("CORTEX_PACK_DOSSIER", 0.1),
        }
    }
}

struct Commit {
    sujet: String,
    age_j: i64,
    fichiers: Vec<String>,
}

/// Historique git (le plus récent d'abord).
fn historique(root: &Path, avant: Option<&str>) -> Vec<Commit> {
    let n = format!("-n{COMMITS_LUS}");
    let rev = avant.map(|a| format!("{a}^"));
    let mut args: Vec<&str> = vec!["log", &n, "--name-only", "--no-renames", "--format=%x01%ct%x02%s", "--relative"];
    if let Some(r) = &rev {
        args.push(r);
    }
    args.push("--");
    let Some(b) = overlay::git(root, &args) else { return Vec::new() };
    let txt = String::from_utf8_lossy(&b);
    let mut out: Vec<Commit> = Vec::new();
    let mut ref_t: Option<i64> = None;
    for bloc in txt.split('\u{1}').skip(1) {
        let mut it = bloc.lines();
        let Some(tete) = it.next() else { continue };
        let (ct, sujet) = tete.split_once('\u{2}').unwrap_or((tete, ""));
        let ct: i64 = ct.parse().unwrap_or(0);
        let r = *ref_t.get_or_insert(ct);
        let fichiers: Vec<String> = it.map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
        out.push(Commit { sujet: sujet.to_string(), age_j: (r - ct) / 86_400, fichiers });
    }
    out
}

fn est_doc(p: &str) -> bool {
    let l = p.to_ascii_lowercase();
    l.ends_with(".md") || l.ends_with(".mdx")
}

/// Fichier de code candidat (indexable, ni test, ni doc, ni fichier généré courant).
fn est_code(p: &str) -> bool {
    Lang::from_path(p).is_indexable() && !is_test_path(p) && !est_doc(p)
}

struct Cand {
    texte: f32,
    co: f32,
    voisin: f32,
    chaud: f32,
    dossier: f32,
    /// Meilleurs symboles du fichier d'après le texte : (score, nœud).
    symboles: Vec<(f32, u32)>,
    co_avec: Option<String>,
}

fn classer(h: &Handle, p: &Params, w: Poids, hist: &[Commit]) -> (Vec<(u32, Cand)>, Vec<u32>, Vec<u32>) {
    let hits = h.search(&p.tache, 150);
    let top = hits.iter().map(|x| x.score).fold(1e-6f32, f32::max);
    let mut cands: HashMap<u32, Cand> = HashMap::new();
    let mut tests: Vec<u32> = Vec::new();
    let mut docs: Vec<u32> = Vec::new();
    for hit in &hits {
        let Some(f) = h.file_by_path(&hit.file) else { continue };
        if est_doc(&hit.file) {
            if hit.kind == SymbolKind::Heading && !docs.contains(&hit.g) {
                docs.push(hit.g);
            }
            continue;
        }
        if is_test_path(&hit.file) {
            if !tests.contains(&f) {
                tests.push(f);
            }
            continue;
        }
        let c = cands.entry(f).or_insert(Cand {
            texte: 0.0,
            co: 0.0,
            voisin: 0.0,
            chaud: 0.0,
            dossier: 0.0,
            symboles: Vec::new(),
            co_avec: None,
        });
        let rang = c.symboles.len().min(3);
        c.texte += hit.score / top * [1.0, 0.5, 0.25, 0.12][rang];
        c.symboles.push((hit.score, hit.g));
    }
    // Graines : les meilleurs fichiers d'après le texte.
    let mut graines: Vec<(u32, f32)> = cands.iter().map(|(f, c)| (*f, c.texte)).collect();
    graines.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
    graines.truncate(w.graines);
    let max_t = graines.first().map(|x| x.1).unwrap_or(1.0).max(1e-6);
    let chemins: HashMap<&str, f32> = graines.iter().map(|(f, s)| (h.path_of(*f), s / max_t)).collect();
    // Co-changement : fichiers modifiés dans les mêmes commits que les graines.
    let mut co: HashMap<&str, (f32, &str)> = HashMap::new();
    for c in hist {
        if c.fichiers.len() > COMMIT_MAX_FICHIERS {
            continue;
        }
        let poids_graines: f32 = c.fichiers.iter().filter_map(|f| chemins.get(f.as_str())).sum();
        if poids_graines <= 0.0 {
            continue;
        }
        let norme = (c.fichiers.len() as f32).sqrt();
        let germe = c.fichiers.iter().find(|f| chemins.contains_key(f.as_str())).map(|s| s.as_str()).unwrap_or("");
        for f in &c.fichiers {
            if chemins.contains_key(f.as_str()) {
                continue;
            }
            let e = co.entry(f.as_str()).or_insert((0.0, germe));
            e.0 += poids_graines / norme;
        }
    }
    let max_co = co.values().map(|x| x.0).fold(1e-6f32, f32::max);
    for (path, (s, germe)) in &co {
        if !est_code(path) {
            // Test ou doc qui change avec les graines : piste pour les listes dédiées.
            if is_test_path(path) {
                if let Some(f) = h.file_by_path(path) {
                    if !tests.contains(&f) && *s / max_co > 0.3 {
                        tests.push(f);
                    }
                }
            }
            continue;
        }
        let Some(f) = h.file_by_path(path) else { continue };
        let c = cands.entry(f).or_insert(Cand {
            texte: 0.0,
            co: 0.0,
            voisin: 0.0,
            chaud: 0.0,
            dossier: 0.0,
            symboles: Vec::new(),
            co_avec: None,
        });
        c.co = s / max_co;
        c.co_avec = Some(germe.to_string());
    }
    // Voisins immédiats (importeurs / importés) des deux premières graines.
    for (f, _) in graines.iter().take(2) {
        let mut v: Vec<u32> = h.importers(*f);
        v.extend(h.imports_raw(*f).iter().copied().filter(|&x| h.alive(x)));
        for x in v {
            let path = h.path_of(x);
            if est_code(path) && h.node(x).is_some_and(|r| r.n.kind == 0) {
                if let Some(c) = cands.get_mut(&x) {
                    c.voisin = 1.0;
                } else if cands.len() < 400 {
                    cands.insert(
                        x,
                        Cand { texte: 0.0, co: 0.0, voisin: 1.0, chaud: 0.0, dossier: 0.0, symboles: Vec::new(), co_avec: None },
                    );
                }
            }
        }
    }
    // Même dossier que les trois premières graines (au plus 12 fichiers par dossier).
    if w.dossier > 0.0 {
        let dirs: Vec<&str> = graines.iter().take(3).map(|(f, _)| h.path_of(*f).rsplit_once('/').map(|x| x.0).unwrap_or("")).collect();
        let mut pris: HashMap<&str, usize> = HashMap::new();
        for r in h.live_files() {
            let p = r.name();
            let d = p.rsplit_once('/').map(|x| x.0).unwrap_or("");
            if !dirs.contains(&d) || !est_code(p) {
                continue;
            }
            let n = pris.entry(d).or_insert(0);
            if let Some(c) = cands.get_mut(&r.g) {
                c.dossier = 1.0;
            } else if *n < 12 {
                *n += 1;
                cands.insert(r.g, Cand { texte: 0.0, co: 0.0, voisin: 0.0, chaud: 0.0, dossier: 1.0, symboles: Vec::new(), co_avec: None });
            }
        }
    }
    // A priori « fichier chaud » : modifié souvent dans les 600 derniers commits.
    if w.chaud > 0.0 {
        let mut freq: HashMap<&str, u32> = HashMap::new();
        for c in hist.iter().take(600).filter(|c| c.fichiers.len() <= COMMIT_MAX_FICHIERS) {
            for f in &c.fichiers {
                *freq.entry(f.as_str()).or_insert(0) += 1;
            }
        }
        let max = freq.values().copied().max().unwrap_or(1).max(2) as f32;
        for (f, c) in cands.iter_mut() {
            if let Some(n) = freq.get(h.path_of(*f)) {
                c.chaud = (1.0 + *n as f32).ln() / (1.0 + max).ln();
            }
        }
    }
    let max_t = cands.values().map(|c| c.texte).fold(1e-6f32, f32::max);
    let mut v: Vec<(u32, Cand)> = cands.into_iter().collect();
    for (_, c) in v.iter_mut() {
        c.texte /= max_t;
    }
    let score = |c: &Cand| c.texte + w.cochange * c.co + w.voisin * c.voisin + w.chaud * c.chaud + w.dossier * c.dossier;
    v.sort_by(|a, b| score(&b.1).partial_cmp(&score(&a.1)).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
    (v, tests, docs)
}

/// Fichiers classés (sans rendu) — pour le banc.
pub fn fichiers_classes(h: &Handle, tache: &str, avant: Option<&str>, w: Poids) -> Vec<String> {
    let p = Params { tache: tache.to_string(), budget: BUDGET_DEFAUT, avant: avant.map(String::from) };
    let hist = historique(Path::new(h.root()), avant);
    let (v, _, _) = classer(h, &p, w, &hist);
    v.iter().map(|(f, _)| h.path_of(*f).to_string()).collect()
}

pub fn pack(handles: &[Handle], p: &Params) -> String {
    if handles.is_empty() {
        return "(cortex) no indexed project\n".into();
    }
    let w = Poids::defaut();
    // Un seul projet à la fois (le mieux placé) : un paquet mélange rarement deux dépôts.
    let h = handles
        .iter()
        .max_by(|a, b| {
            let s = |h: &Handle| h.search(&p.tache, 5).first().map(|x| x.score).unwrap_or(0.0);
            s(a).partial_cmp(&s(b)).unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(&handles[0]);
    let hist = historique(Path::new(h.root()), p.avant.as_deref());
    let (cls, tests_texte, docs) = classer(h, p, w, &hist);
    if cls.is_empty() {
        return format!(
            "(cortex) nothing found for this task: rephrase with domain words (a screen, a function, a table)\nnext : find {}\n",
            court(&p.tache, 60)
        );
    }
    let mut out = Out::new(p.budget);
    out.ligne(&format!("# pack — {} (≤ {} tokens)", court(&p.tache, 90), p.budget));
    // 1. Fichiers probables : la part du lion du budget, c'est ce qui fait gagner des lectures.
    // 55 % du budget au plus (le reste : symboles, tests, docs, récents, parcours).
    let limite_fichiers = p.budget * 4 * 55 / 100;
    let mut utilises = 0usize;
    let mut n_fichiers = 0usize;
    out.ligne("## files (most likely first; t=text, c=changed together before, n=neighbour)");
    let ov = overlay::charger(h);
    for (f, c) in cls.iter().take(80) {
        let mut tags = String::new();
        if c.texte > 0.0 {
            tags.push('t');
        }
        if c.co > 0.0 {
            tags.push('c');
        }
        if c.voisin > 0.0 {
            tags.push('n');
        }
        let path = h.path_of(*f);
        let co = match (&c.co_avec, c.texte > 0.0) {
            (Some(g), false) => format!(" ← with {}", g.rsplit('/').next().unwrap_or(g)),
            _ => String::new(),
        };
        let ligne = format!("- {} {}{}{}", crate::ids::file_id(path), tags, co, super::marque(&ov, path));
        utilises += ligne.chars().count() + 1;
        if utilises > limite_fichiers && n_fichiers >= 8 {
            break;
        }
        n_fichiers += 1;
        out.ligne(&ligne);
    }
    // 2. Symboles clés : les meilleurs du texte, avec leur carte courte (appelants).
    out.ligne("## key symbols");
    let mut vus: HashSet<u32> = HashSet::new();
    let mut n_sym = 0;
    let mut premier: Option<String> = None;
    let mut candidats: Vec<(f32, u32)> = cls.iter().flat_map(|(_, c)| c.symboles.iter().copied()).collect();
    candidats.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    for (_, g) in candidats {
        if n_sym >= (p.budget / 300).clamp(4, 12) || !vus.insert(g) {
            continue;
        }
        let Some(r) = h.node(g) else { continue };
        if r.n.kind != 1 || matches!(r.kind(), SymbolKind::Import) {
            continue;
        }
        n_sym += 1;
        let id = h.node_id(g);
        if premier.is_none() {
            premier = Some(format!("read {id}"));
        }
        let app = super::appelants(h, g);
        let ids: Vec<String> = app.iter().map(|&(c, _)| h.node_id(c)).collect();
        let rl = role(h, g);
        out.ligne(&format!(
            "- {}{}{}",
            entete(h, g),
            if rl.is_empty() { String::new() } else { format!(" — {}", court(rl, 80)) },
            if ids.is_empty() { String::new() } else { format!(" · callers {}: {}", ids.len(), liste(&ids, 2)) }
        ));
    }
    // 3. Tests liés : par nom, par import, et ceux que le texte a fait remonter.
    let mut tests: Vec<u32> = Vec::new();
    for (f, _) in cls.iter().take(6) {
        tests.extend(tests_par_nom(h, h.path_of(*f), None));
        tests.extend(h.importers(*f).into_iter().filter(|&i| is_test_path(h.path_of(i))).take(3));
    }
    tests.extend(tests_texte);
    let mut vus_t = HashSet::new();
    tests.retain(|t| vus_t.insert(*t));
    if !tests.is_empty() {
        let t: Vec<String> = tests.iter().take(8).map(|&t| crate::ids::file_id(h.path_of(t))).collect();
        out.ligne(&format!("## tests ({}): {}", tests.len(), t.join(", ")));
    }
    // 4. Docs qui parlent du sujet.
    if !docs.is_empty() {
        let d: Vec<String> = docs.iter().take(4).map(|&g| h.node_id(g)).collect();
        out.ligne(&format!("## docs: {}", d.join(", ")));
    }
    // 5. Changements récents liés (30 jours avant la tâche) sur les fichiers de tête.
    let tete: HashSet<&str> = cls.iter().take(n_fichiers).map(|(f, _)| h.path_of(*f)).collect();
    let mut recents: Vec<String> = Vec::new();
    for c in hist.iter().filter(|c| c.age_j <= 30).take(400) {
        if c.fichiers.len() <= COMMIT_MAX_FICHIERS {
            if let Some(f) = c.fichiers.iter().find(|f| tete.contains(f.as_str())) {
                recents.push(format!("{} ({}d: {})", crate::ids::file_id(f), c.age_j, court(&c.sujet, 50)));
                if recents.len() >= 3 {
                    break;
                }
            }
        }
    }
    if !recents.is_empty() {
        out.ligne(&format!("## recent changes: {}", recents.join(" ; ")));
    }
    // 6. Parcours d'écran (recettes Spectra) qui dépendent des fichiers de tête.
    if Path::new(h.root()).join(".spectra").join("recipes").is_dir() {
        let seeds: Vec<u32> = cls.iter().take(4).map(|(f, _)| *f).collect();
        let rec = super::selection::recettes_touchees(h, &seeds);
        let r: Vec<String> = rec.iter().filter(|x| x.1 <= 2).take(3).map(|x| x.0.clone()).collect();
        if !r.is_empty() {
            out.ligne(&format!("## journeys (Spectra recipes): {}", r.join(", ")));
        }
    }
    out.fin(premier.or_else(|| cls.first().map(|(f, _)| format!("outline {}", crate::ids::file_id(h.path_of(*f))))))
}
