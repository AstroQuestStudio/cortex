//! `impact`, `path`, `overview` — le graphe (appels, imports) lu dans les CSR
//! de l'atlas, sans relire une source.

use super::lecture::englobant;
use super::overlay;
use super::{court, echec, entete, fichier_de, liste, marque, resoudre, role, site_appel, tests_par_nom, Cible, Out, Resolu};
use crate::atlas::Handle;
use crate::ids;
use crate::search::is_test_path;
use crate::symbol::SymbolKind;
use std::collections::{HashMap, HashSet, VecDeque};

/// Nœuds visités au plus par `impact` (borne la latence des utilitaires très appelés).
const IMPACT_MAX_NOEUDS: usize = 4000;
/// Sauts au plus pour `path`.
const PATH_MAX_SAUTS: usize = 12;

fn noeud(h: &Handle, c: Cible) -> u32 {
    match c {
        Cible::Noeud(g) => g,
        Cible::Lignes { fichier, debut, .. } => englobant(h, fichier, debut).unwrap_or(fichier),
    }
}

fn est_symbole(h: &Handle, g: u32) -> bool {
    h.node(g).is_some_and(|r| r.n.kind == 1)
}

fn est_test(h: &Handle, g: u32) -> bool {
    is_test_path(h.path_of(fichier_de(h, g)))
}

// ─── impact ─────────────────────────────────────────────────────────────────

/// Ceux qui dépendent directement de `x` : appelants d'un symbole (avec le site
/// d'appel) et fichiers qui importent son fichier EN NOMMANT le symbole sans
/// l'appeler (types, constantes) ; importeurs d'un fichier.
fn dependants(h: &Handle, x: u32) -> Vec<(u32, Option<u32>)> {
    let Some(r) = h.node(x) else { return Vec::new() };
    if r.n.kind == 0 {
        return h.importers(x).into_iter().map(|f| (f, None)).collect();
    }
    let nom = r.name();
    let mut v = super::appelants(h, x);
    let fichiers_appelants: HashSet<u32> = v.iter().map(|&(c, _)| fichier_de(h, c)).collect();
    for f in h.importers(r.n.owner_file) {
        if fichiers_appelants.contains(&f) {
            continue;
        }
        let Some(fr) = h.node(f) else { continue };
        if fr.refs().is_some_and(|refs| refs.imported_names.iter().any(|&n| fr.str(n) == nom)) {
            v.push((f, None));
        }
    }
    v
}

/// `impact <id>` : dépendants transitifs par profondeur, tests à relancer.
pub fn impact(handles: &[Handle], entree: &str, profondeur: usize, budget: usize) -> String {
    let (h, cible) = match resoudre(handles, entree) {
        Resolu::Trouve { h, cible, .. } => (h, cible),
        r => return echec(r, entree),
    };
    let start = noeud(h, cible);
    let ov = overlay::charger(h);
    let mut vus: HashSet<u32> = HashSet::from([start]);
    let mut niveaux: Vec<Vec<(u32, Option<u32>)>> = Vec::new();
    let mut front = vec![start];
    let mut borne = false;
    for _ in 0..profondeur {
        let mut suivant: Vec<(u32, Option<u32>)> = Vec::new();
        'front: for &x in &front {
            for (y, site) in dependants(h, x) {
                if vus.insert(y) {
                    suivant.push((y, site));
                    if vus.len() >= IMPACT_MAX_NOEUDS {
                        borne = true;
                        break 'front;
                    }
                }
            }
        }
        if suivant.is_empty() {
            break;
        }
        suivant.sort_by(|a, b| est_test(h, a.0).cmp(&est_test(h, b.0)).then_with(|| h.sort_key(a.0).cmp(&h.sort_key(b.0))));
        front = suivant.iter().map(|x| x.0).collect();
        niveaux.push(suivant);
        if borne {
            break;
        }
    }
    let f0 = fichier_de(h, start);
    let nom0 = h.node(start).filter(|r| r.n.kind == 1).map(|r| r.name().to_string());
    let mut tests: Vec<u32> = vus.iter().map(|&g| fichier_de(h, g)).filter(|&f| is_test_path(h.path_of(f)) && f != start).collect();
    tests.extend(tests_par_nom(h, h.path_of(f0), nom0.as_deref()));
    tests.sort_by(|&a, &b| h.path_of(a).cmp(h.path_of(b)));
    tests.dedup();

    let mut out = Out::new(budget);
    out.ligne(&format!("impact {}{}", entete(h, start), marque(&ov, h.path_of(f0))));
    let total: usize = niveaux.iter().map(|n| n.len()).sum();
    let fichiers: HashSet<u32> = niveaux.iter().flatten().map(|&(g, _)| fichier_de(h, g)).collect();
    if total == 0 {
        out.ligne("aucun appelant ni importeur résolu (appel dynamique, chaîne, réexport ?)");
    } else {
        out.ligne(&format!(
            "{} dépendant(s) sur {} niveau(x), {} fichier(s){}",
            total,
            niveaux.len(),
            fichiers.len(),
            if borne { format!(" (borné à {} nœuds)", IMPACT_MAX_NOEUDS) } else { String::new() }
        ));
    }
    if !tests.is_empty() {
        let t: Vec<String> = tests.iter().map(|&t| h.node_id(t)).collect();
        out.ligne(&format!("tests à relancer {}: {}", tests.len(), liste(&t, 10)));
    }
    for (d, niv) in niveaux.iter().enumerate() {
        let nf: HashSet<u32> = niv.iter().map(|&(g, _)| fichier_de(h, g)).collect();
        out.ligne(&format!(
            "profondeur {} — {} ({} fichiers){}",
            d + 1,
            niv.len(),
            nf.len(),
            if d == 0 { ", L = ligne de l'appel" } else { "" }
        ));
        for &(g, site) in niv {
            let s = site.map(|l| format!(" L{}", l)).unwrap_or_default();
            out.ligne(&format!("  {}{}{}", h.node_id(g), s, marque(&ov, h.path_of(fichier_de(h, g)))));
        }
    }
    let suite = match (niveaux.first().and_then(|n| n.first()), &nom0) {
        (Some(&(g, _)), _) => format!("read {}", h.node_id(g)),
        (None, Some(n)) => format!("grep {}", n),
        (None, None) => format!("outline {}", h.node_id(start)),
    };
    out.fin(Some(suite))
}

// ─── path ───────────────────────────────────────────────────────────────────

/// Plus court chemin (BFS, voisins en ordre canonique) de `sources` vers un
/// nœud de `cibles`, en `sauts` au plus. Rend la suite de nœuds.
fn bfs(sources: &[u32], cibles: &HashSet<u32>, voisins: impl Fn(u32) -> Vec<u32>, sauts: usize) -> Option<Vec<u32>> {
    let mut parent: HashMap<u32, u32> = HashMap::new();
    let mut prof: HashMap<u32, usize> = HashMap::new();
    let mut file: VecDeque<u32> = VecDeque::new();
    for &s in sources {
        if cibles.contains(&s) {
            return Some(vec![s]);
        }
        if prof.insert(s, 0).is_none() {
            file.push_back(s);
        }
    }
    while let Some(x) = file.pop_front() {
        let d = prof[&x];
        if d >= sauts {
            continue;
        }
        for y in voisins(x) {
            if prof.contains_key(&y) {
                continue;
            }
            prof.insert(y, d + 1);
            parent.insert(y, x);
            if cibles.contains(&y) {
                let mut chemin = vec![y];
                let mut c = y;
                while let Some(&p) = parent.get(&c) {
                    chemin.push(p);
                    c = p;
                }
                chemin.reverse();
                return Some(chemin);
            }
            file.push_back(y);
        }
    }
    None
}

/// Symboles de départ d'une cible (un symbole, ou ceux d'un fichier).
fn symboles_de(h: &Handle, g: u32) -> Vec<u32> {
    if est_symbole(h, g) {
        vec![g]
    } else {
        h.symbols_of(g).iter().filter(|s| s.kind() != SymbolKind::Import).map(|s| s.g).collect()
    }
}

enum Chemin {
    Appels(Vec<u32>),
    Imports(Vec<u32>),
}

fn chercher(h: &Handle, a: u32, b: u32) -> Option<Chemin> {
    let sources = symboles_de(h, a);
    let mut cibles: HashSet<u32> = symboles_de(h, b).into_iter().collect();
    if !est_symbole(h, b) {
        cibles.insert(b);
    }
    if let Some(c) = bfs(&sources, &cibles, |x| h.callees(x), PATH_MAX_SAUTS) {
        return Some(Chemin::Appels(c));
    }
    let (fa, fb) = (fichier_de(h, a), fichier_de(h, b));
    if fa == fb && (est_symbole(h, a) || est_symbole(h, b)) {
        // Même fichier : un « chemin d'imports » de 0 saut ne dirait rien.
        return None;
    }
    let fc: HashSet<u32> = HashSet::from([fb]);
    let imports = |x: u32| {
        let mut v: Vec<u32> = h.imports_raw(x).iter().copied().filter(|&t| h.alive(t)).collect();
        v.sort_by(|&p, &q| h.path_of(p).cmp(h.path_of(q)));
        v
    };
    bfs(&[fa], &fc, imports, PATH_MAX_SAUTS).map(Chemin::Imports)
}

/// `path <a> <b>` : plus court chemin d'appels (sinon d'imports) de A vers B,
/// ou de B vers A s'il n'y en a pas dans ce sens.
pub fn path(handles: &[Handle], de: &str, vers: &str, budget: usize) -> String {
    let (h, ca) = match resoudre(handles, de) {
        Resolu::Trouve { h, cible, .. } => (h, cible),
        r => return echec(r, de),
    };
    let cb = match resoudre(std::slice::from_ref(h), vers) {
        Resolu::Trouve { cible, .. } => cible,
        r => return echec(r, vers),
    };
    let (a, b) = (noeud(h, ca), noeud(h, cb));
    let mut out = Out::new(budget);
    let (chemin, inverse) = match chercher(h, a, b) {
        Some(c) => (c, false),
        None => match chercher(h, b, a) {
            Some(c) => (c, true),
            None => {
                out.ligne(&format!(
                    "aucun chemin d'appels ni d'imports entre {} et {} (≤ {} sauts, dans les deux sens)",
                    h.node_id(a),
                    h.node_id(b),
                    PATH_MAX_SAUTS
                ));
                return out.fin(Some(format!("impact {}", h.node_id(b))));
            }
        },
    };
    if inverse {
        out.ligne(&format!("aucun chemin de {} vers {} ; chemin inverse :", h.node_id(a), h.node_id(b)));
    }
    let noeuds = match &chemin {
        Chemin::Appels(c) | Chemin::Imports(c) => c.clone(),
    };
    let genre = if matches!(chemin, Chemin::Appels(_)) { "appels" } else { "imports" };
    out.ligne(&format!("{} {} saut(s):", genre, noeuds.len().saturating_sub(1)));
    out.ligne(&entete(h, noeuds[0]));
    for w in noeuds.windows(2) {
        let (p, n) = (w[0], w[1]);
        let lien = match &chemin {
            Chemin::Appels(_) => {
                let nom = h.node(n).map(|r| r.name().to_string()).unwrap_or_default();
                match site_appel(h, p, &nom) {
                    Some(l) => format!("→ appelle en L{}", l),
                    None => "→ appelle".to_string(),
                }
            }
            Chemin::Imports(_) => "→ importe".to_string(),
        };
        out.ligne(&format!("  {} {}", lien, entete(h, n)));
    }
    let milieu = noeuds[noeuds.len() / 2];
    out.fin(Some(format!("read {}", h.node_id(milieu))))
}

// ─── overview ───────────────────────────────────────────────────────────────

/// Dossier d'un chemin, sur `n` segments au plus (zone de dépendance).
fn zone(path: &str, n: usize) -> String {
    let segs: Vec<&str> = path.split('/').collect();
    let d = &segs[..segs.len().saturating_sub(1)];
    d[..d.len().min(n)].join("/")
}

/// Paquet externe d'un spécificateur d'import non résolu (`@scope/x/y` → `@scope/x`).
fn paquet(spec: &str) -> Option<String> {
    if spec.starts_with('.') || spec.starts_with('/') || spec.starts_with("@/") || spec.is_empty() {
        return None;
    }
    let mut it = spec.split('/');
    let first = it.next()?;
    Some(if first.starts_with('@') { format!("{}/{}", first, it.next().unwrap_or("")) } else { first.to_string() })
}

/// `overview <dossier>` : fichiers et rôles, points d'entrée (importés de
/// l'extérieur), dépendances sortantes et entrantes, paquets externes.
pub fn overview(handles: &[Handle], dossier: &str, budget: usize) -> String {
    let d = dossier.trim().trim_start_matches("F:").trim_start_matches("./").trim_end_matches('/').replace('\\', "/");
    let d = if d == "." { String::new() } else { d };
    let prefixe = if d.is_empty() { String::new() } else { format!("{}/", d) };
    let Some(h) = handles.iter().find(|h| h.live_files().any(|r| r.name().starts_with(&prefixe))) else {
        // Un fichier plutôt qu'un dossier : son outline.
        if let Resolu::Trouve { .. } = resoudre(handles, &d) {
            return super::lecture::outline(handles, &d, budget);
        }
        return format!("(cortex) aucun fichier indexé sous « {} »\nsuite : files {}\n", d, d.rsplit('/').next().unwrap_or(&d));
    };
    let ov = overlay::charger(h);
    let dedans = |p: &str| p.starts_with(&prefixe);
    let mut fichiers: Vec<(&str, u32)> = h.live_files().filter(|r| dedans(r.name())).map(|r| (r.name(), r.g)).collect();
    fichiers.sort();
    let prof_zone = prefixe.matches('/').count() + 1;
    let mut entrees: Vec<(usize, &str, u32)> = Vec::new();
    let mut sortant: HashMap<String, usize> = HashMap::new();
    let mut entrant: HashMap<String, usize> = HashMap::new();
    let mut paquets: HashMap<String, usize> = HashMap::new();
    let mut utilisateurs: HashSet<u32> = HashSet::new();
    let mut n_sym = 0usize;
    let mut n_modifies = 0usize;
    for &(p, f) in &fichiers {
        n_sym += h.symbols_of(f).len();
        if ov.contient(p) {
            n_modifies += 1;
        }
        let ext: Vec<u32> = h.importers(f).into_iter().filter(|&i| !dedans(h.path_of(i))).collect();
        for &i in &ext {
            *entrant.entry(zone(h.path_of(i), prof_zone)).or_default() += 1;
            utilisateurs.insert(i);
        }
        if !ext.is_empty() {
            entrees.push((ext.len(), p, f));
        }
        for &t in h.imports_raw(f) {
            if h.alive(t) && !dedans(h.path_of(t)) {
                *sortant.entry(zone(h.path_of(t), prof_zone)).or_default() += 1;
            }
        }
        if let Some(r) = h.node(f) {
            if let Some(refs) = r.refs() {
                for &s in refs.imports.iter() {
                    if let Some(pk) = paquet(r.str(s)) {
                        *paquets.entry(pk).or_default() += 1;
                    }
                }
            }
        }
    }
    entrees.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    let tri = |m: HashMap<String, usize>| {
        let mut v: Vec<(String, usize)> = m.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v.into_iter().map(|(z, n)| format!("{} ({})", if z.is_empty() { ".".into() } else { z }, n)).collect::<Vec<_>>()
    };
    let mut out = Out::new(budget);
    let titre = if d.is_empty() { h.project.clone() } else { format!("{}/", d) };
    out.ligne(&format!(
        "overview {} — {} fichiers, {} symboles{}",
        titre,
        fichiers.len(),
        n_sym,
        if n_modifies > 0 { format!(", {} ✎ non commité(s)", n_modifies) } else { String::new() }
    ));
    // Rôle du module : README ou index du dossier lui-même.
    for (p, f) in &fichiers {
        let nom = p.rsplit('/').next().unwrap_or(p).to_ascii_lowercase();
        if p.matches('/').count() == prefixe.matches('/').count()
            && (nom.starts_with("readme") || nom.starts_with("index.") || nom == "mod.rs" || nom == "lib.rs")
        {
            let r = role(h, *f);
            if !r.is_empty() {
                out.ligne(&format!("rôle: {} ({})", court(r, 140), ids::file_id(p)));
                break;
            }
        }
    }
    if !entrees.is_empty() {
        out.ligne(&format!("points d'entrée (importés de l'extérieur) {}:", entrees.len()));
        for (n, p, f) in entrees.iter().take(8) {
            let r = role(h, *f);
            let r = if r.is_empty() { String::new() } else { format!(" — {}", court(r, 80)) };
            out.ligne(&format!("  {} ×{}{}{}", ids::file_id(p), n, marque(&ov, p), r));
        }
    }
    if !sortant.is_empty() {
        out.ligne(&format!("dépend de : {}", liste(&tri(sortant), 8)));
    }
    if !entrant.is_empty() {
        out.ligne(&format!("utilisé par (dossiers) : {}", liste(&tri(entrant), 8)));
        // Les fichiers extérieurs eux-mêmes : code d'abord, tests ensuite.
        let mut u: Vec<u32> = utilisateurs.into_iter().collect();
        u.sort_by(|&a, &b| is_test_path(h.path_of(a)).cmp(&is_test_path(h.path_of(b))).then_with(|| h.path_of(a).cmp(h.path_of(b))));
        let ids_u: Vec<String> = u.iter().map(|&f| ids::file_id(h.path_of(f))).collect();
        out.ligne(&format!("utilisé par (fichiers) {}: {}", ids_u.len(), liste(&ids_u, 8)));
    }
    if !paquets.is_empty() {
        out.ligne(&format!("paquets : {}", liste(&tri(paquets), 8)));
    }
    if fichiers.len() <= 40 {
        out.ligne("fichiers:");
        for (p, f) in &fichiers {
            let r = role(h, *f);
            let r = if r.is_empty() { String::new() } else { format!(" — {}", court(r, 70)) };
            out.ligne(&format!("  {} {} sym{}{}", ids::file_id(p), h.symbols_of(*f).len(), marque(&ov, p), r));
        }
    } else {
        let mut sous: HashMap<String, usize> = HashMap::new();
        for (p, _) in &fichiers {
            let rest = &p[prefixe.len()..];
            let k = match rest.split_once('/') {
                Some((s, _)) => format!("{}{}/", prefixe, s),
                None => format!("{}*", prefixe),
            };
            *sous.entry(k).or_default() += 1;
        }
        let mut v: Vec<(String, usize)> = sous.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out.ligne(&format!("sous-dossiers ({}):", v.len()));
        for (s, n) in v.iter().take(20) {
            out.ligne(&format!("  {} {} fichiers", s, n));
        }
    }
    let suite = entrees
        .first()
        .map(|(_, p, _)| format!("outline {}", ids::file_id(p)))
        .or_else(|| fichiers.first().map(|(p, _)| format!("outline {}", ids::file_id(p))));
    out.fin(suite)
}
