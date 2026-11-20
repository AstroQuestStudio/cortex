//! `find`, `card`, `outline`, `read` — localiser, résumer, lister, montrer.

use super::overlay::{self, Overlay};
use super::{court, echec, entete, liste, marque, resoudre, role, tests_par_nom, Cible, Out, Resolu};
use crate::atlas::cards::lines;
use crate::atlas::Handle;
use crate::ids;
use crate::lang::Lang;
use crate::search::is_test_path;
use crate::symbol::SymbolKind;

/// Nombre de résultats de `find` qui portent leur rôle en une phrase.
const FIND_ROLES: usize = 5;

/// Symbole le plus INTERNE (hors imports et titres) qui contient la ligne.
pub(crate) fn englobant(h: &Handle, f: u32, line: u32) -> Option<u32> {
    let mut best: Option<(u32, u32)> = None;
    for r in h.symbols_of(f) {
        let n = r.n;
        if n.end_line == 0 || matches!(r.kind(), SymbolKind::Import | SymbolKind::Heading) || line < n.line || line > n.end_line {
            continue;
        }
        let span = n.end_line - n.line;
        if best.is_none_or(|(b, _)| span <= b) {
            best = Some((span, r.g));
        }
    }
    best.map(|(_, g)| g)
}

/// Cible → nœud (symbole englobant pour une plage, sinon le fichier).
fn noeud(h: &Handle, c: Cible) -> u32 {
    match c {
        Cible::Noeud(g) => g,
        Cible::Lignes { fichier, debut, .. } => englobant(h, fichier, debut).unwrap_or(fichier),
    }
}

// ─── find ───────────────────────────────────────────────────────────────────

/// `find <question>` : symboles classés (BM25F), un par ligne, identifiant et
/// plage ; les premiers portent leur rôle.
pub fn find(handles: &[Handle], question: &str, budget: usize) -> String {
    // Nombre de résultats proportionné au budget (~40 tokens par ligne avec les rôles).
    let n_hits = (budget / 40).clamp(10, 100);
    let mut hits: Vec<(usize, crate::search::Hit)> = Vec::new();
    for (i, h) in handles.iter().enumerate() {
        hits.extend(h.search(question, n_hits).into_iter().map(|x| (i, x)));
    }
    hits.sort_by(|a, b| b.1.score.partial_cmp(&a.1.score).unwrap_or(std::cmp::Ordering::Equal));
    hits.truncate(n_hits);
    if hits.is_empty() {
        let mot = crate::search::query_terms(question).into_iter().next().unwrap_or_default();
        return format!("(cortex) no symbol for '{}'\nnext : grep {}\n", question, mot);
    }
    let multi = handles.len() > 1;
    let ovs: Vec<Overlay> = handles.iter().map(overlay::charger).collect();
    let mut out = Out::new(budget);
    let mut suite = None;
    let mut vus: std::collections::HashSet<(usize, u32)> = std::collections::HashSet::new();
    for (rang, (hi, hit)) in hits.iter().enumerate() {
        let h = &handles[*hi];
        // C/C++ : un prototype d'en-tête est montré par sa définition (.cpp) — c'est
        // elle qu'on lit — avec le rôle (commentaire) du prototype ; les deux ne
        // comptent qu'une fois.
        let mut g = hit.g;
        let mut role_decl = "";
        if h.node(g).is_some_and(|r| r.kind() == SymbolKind::Decl) {
            if let Some(&d) = contreparties(h, g).first() {
                role_decl = h.node(g).map(|r| r.str(r.n.summary)).unwrap_or("");
                g = d;
            }
        }
        if !vus.insert((*hi, g)) {
            continue;
        }
        let Some(r) = h.node(g) else { continue };
        let id = h.node_id(g);
        if suite.is_none() {
            suite = Some(format!("card {}", id));
        }
        // Un fichier sans symbole (script de haut niveau, fonction serveur inline) est
        // lui-même le résultat : le nœud est le fichier.
        let is_file = r.n.kind == 0;
        let file_g = if is_file { g } else { r.n.owner_file };
        let path = h.path_of(file_g);
        let mut l = format!(
            "{}{} {} {}{}",
            if multi { format!("[{}] ", h.project) } else { String::new() },
            id,
            if is_file { "file" } else { r.kind().as_str() },
            lines(r.n.line, r.n.end_line),
            marque(&ovs[*hi], path)
        );
        if rang < FIND_ROLES {
            let mut role_s = r.str(r.n.summary);
            if role_s.is_empty() {
                role_s = role_decl;
            }
            if role_s.is_empty() {
                role_s = role(h, file_g);
            }
            if !role_s.is_empty() {
                l.push_str(" — ");
                l.push_str(&court(role_s, 90));
            }
        }
        out.ligne(&l);
    }
    out.fin(suite)
}

// ─── card ───────────────────────────────────────────────────────────────────

/// Docs (`docs/**/*.md`) qui citent le symbole ou son fichier : (mentions, chemin).
fn docs_citant(h: &Handle, nom: &str, file_path: &str) -> Vec<(usize, String)> {
    use rayon::prelude::*;
    let root = std::path::Path::new(h.root());
    let file_name = file_path.rsplit('/').next().unwrap_or(file_path).to_string();
    let doc_paths: Vec<&str> = h
        .live_files()
        .map(|f| f.name())
        .filter(|p| {
            let pl = p.to_ascii_lowercase();
            pl.starts_with("docs/") && pl.ends_with(".md")
        })
        .collect();
    let mut docs: Vec<(usize, String)> = doc_paths
        .par_iter()
        .filter_map(|p| {
            let content = std::fs::read_to_string(root.join(p)).ok()?;
            let n = content.matches(nom).count() * 2 + content.matches(file_name.as_str()).count();
            (n > 0).then(|| (n, p.to_string()))
        })
        .collect();
    docs.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    docs
}

/// Tests liés à un symbole : fichiers de test qui l'appellent, qui importent
/// son fichier, ou nommés d'après lui ou son fichier.
pub(super) fn tests_du_symbole(h: &Handle, g: u32, appelants: &[u32]) -> Vec<u32> {
    let Some(r) = h.node(g) else { return Vec::new() };
    let f = r.n.owner_file;
    let mut t: Vec<u32> = appelants.iter().map(|&c| super::fichier_de(h, c)).filter(|&cf| is_test_path(h.path_of(cf))).collect();
    t.extend(h.importers(f).into_iter().filter(|&i| is_test_path(h.path_of(i))));
    t.extend(tests_par_nom(h, h.path_of(f), Some(r.name())));
    t.sort_by(|&a, &b| h.path_of(a).cmp(h.path_of(b)));
    t.dedup();
    t
}

/// Les cartes stockées par un atlas antérieur sont en français (`rôle:`,
/// `appelle N`, `ambigu(s)`) : on les rend en anglais à la lecture, sans
/// changer le format de l'atlas.
fn carte_anglaise(carte: &str) -> String {
    carte
        .lines()
        .map(|l| {
            if let Some(r) = l.strip_prefix("rôle:") {
                format!("role:{}", r)
            } else if let Some(r) = l.strip_prefix("appelle ") {
                format!("calls {}", r.replace(" ambigus)", " ambiguous)").replace(" ambigu)", " ambiguous)"))
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(
            "
",
        )
}

/// C/C++ : la définition (.cpp) d'un prototype d'en-tête, ou les prototypes
/// d'une définition — mêmes noms qualifiés, genre `Decl` d'un côté seulement.
pub(super) fn contreparties(h: &Handle, g: u32) -> Vec<u32> {
    let Some(r) = h.node(g) else { return Vec::new() };
    let est_decl = r.kind() == SymbolKind::Decl;
    let mut v: Vec<u32> = h
        .defs(&crate::graph::call_key(r.name()))
        .into_iter()
        .filter(|&x| x != g)
        .filter(|&x| h.node(x).is_some_and(|y| (y.kind() == SymbolKind::Decl) != est_decl && y.name().eq_ignore_ascii_case(r.name())))
        .collect();
    v.sort_by(|&a, &b| h.sort_key(a).cmp(&h.sort_key(b)));
    v
}

/// `card <id|nom>` : la carte précompilée (I7) + appelants, importeurs, tests.
/// `avec_docs` : alias `context` (ajoute les docs qui citent le symbole).
pub fn card(handles: &[Handle], entree: &str, budget: usize, avec_docs: bool) -> String {
    let (h, cible, homonymes) = match resoudre(handles, entree) {
        Resolu::Trouve { h, cible, homonymes } => (h, cible, homonymes),
        r => return echec(r, entree),
    };
    let g = noeud(h, cible);
    let Some(r) = h.node(g) else { return echec(Resolu::Introuvable(entree.into()), entree) };
    if r.n.kind == 0 {
        return outline_de(h, g, budget);
    }
    let ov = overlay::charger(h);
    let f = r.n.owner_file;
    let path = h.path_of(f);
    let id = h.node_id(g);
    let mut out = Out::new(budget);
    let carte = &carte_anglaise(r.str(r.n.card));
    let a_role = carte.lines().any(|l| l.starts_with("role:"));
    let cp = contreparties(h, g);
    // Sans rôle propre : celui du prototype d'en-tête (C/C++), sinon celui du fichier.
    let role_decl = if a_role { "" } else { cp.iter().map(|&x| role(h, x)).find(|s| !s.is_empty()).unwrap_or("") };
    let (rf, rf_etiquette) = if a_role {
        ("", "")
    } else if !role_decl.is_empty() {
        (role_decl, "role (decl)")
    } else {
        (role(h, f), "role (file)")
    };
    for (i, l) in carte.lines().enumerate() {
        if i == 0 {
            out.ligne(&format!("{}{}", l, marque(&ov, path)));
            continue;
        }
        // Sans rôle propre, celui du fichier, avant les relations.
        if !rf.is_empty() && l.starts_with("calls") {
            out.ligne(&format!("{}: {}", rf_etiquette, rf));
        }
        out.ligne(l);
    }
    if !rf.is_empty() && !carte.lines().any(|l| l.starts_with("calls")) {
        out.ligne(&format!("{}: {}", rf_etiquette, rf));
    }
    if !cp.is_empty() {
        let ids_: Vec<String> = cp.iter().map(|&x| h.node_id(x)).collect();
        let etiquette = if r.kind() == SymbolKind::Decl { "defined in" } else { "declared in" };
        out.ligne(&format!("{}: {}", etiquette, liste(&ids_, 3)));
    }
    let mut appelants = super::appelants(h, g);
    // Code d'abord, tests ensuite (ordre canonique dans chaque groupe).
    appelants.sort_by_key(|&(c, _)| is_test_path(h.path_of(super::fichier_de(h, c))));
    let appelants_g: Vec<u32> = appelants.iter().map(|&(c, _)| c).collect();
    if !appelants.is_empty() {
        let fichiers: std::collections::HashSet<u32> = appelants_g.iter().map(|&c| super::fichier_de(h, c)).collect();
        let ids_: Vec<String> = appelants
            .iter()
            .map(|&(c, site)| match site {
                Some(l) => format!("{} L{}", h.node_id(c), l),
                None => h.node_id(c),
            })
            .collect();
        out.ligne(&format!("called by {} ({} files, L = call line): {}", appelants.len(), fichiers.len(), liste(&ids_, 8)));
    } else if r.kind() != SymbolKind::Heading {
        out.ligne("called by 0 (no resolved call; imports: see impact)");
    }
    let importeurs = h.importers(f);
    if !importeurs.is_empty() {
        out.ligne(&format!("file imported by {} file(s)", importeurs.len()));
    }
    let tests = tests_du_symbole(h, g, &appelants_g);
    if !tests.is_empty() {
        let t: Vec<String> = tests.iter().map(|&t| h.node_id(t)).collect();
        out.ligne(&format!("tests: {}", liste(&t, 5)));
    }
    // Membres (classe, interface…) et autres exports du même fichier : de quoi
    // naviguer sans lire le fichier.
    let freres = h.symbols_of(f);
    let (l0, l1) = (r.n.line, r.n.end_line);
    let contenant = matches!(r.kind(), SymbolKind::Class | SymbolKind::Interface | SymbolKind::Struct | SymbolKind::Enum);
    if contenant && l1 > l0 {
        let membres: Vec<String> = freres
            .iter()
            .filter(|x| x.g != g && x.n.line > l0 && x.n.line <= l1 && x.kind() != SymbolKind::Import)
            .map(|x| h.node_id(x.g))
            .collect();
        if !membres.is_empty() {
            out.ligne(&format!("members {}: {}", membres.len(), liste(&membres, 6)));
        }
    }
    let voisins: Vec<String> = freres
        .iter()
        .filter(|x| x.g != g && exporte(x.str(x.n.signature)) && !(contenant && x.n.line > l0 && x.n.line <= l1))
        .map(|x| h.node_id(x.g))
        .collect();
    if !voisins.is_empty() {
        out.ligne(&format!("other exports of the file {}: {}", voisins.len(), liste(&voisins, 5)));
    }
    if !homonymes.is_empty() {
        let hs: Vec<String> = homonymes.iter().map(|&x| h.node_id(x)).collect();
        out.ligne(&format!("homonyms: {}", liste(&hs, 5)));
    }
    if avec_docs {
        let docs = docs_citant(h, r.name(), path);
        if !docs.is_empty() {
            let d: Vec<String> = docs.iter().map(|(n, p)| format!("{} ({}×)", ids::file_id(p), n)).collect();
            out.ligne(&format!("docs: {}", liste(&d, 5)));
        }
    }
    out.fin(Some(format!("read {}", id)))
}

// ─── outline ────────────────────────────────────────────────────────────────

pub(super) fn exporte(sig: &str) -> bool {
    sig.starts_with("export ") || sig.starts_with("pub ") || sig.starts_with("pub(")
}

/// Niveau d'un titre markdown (nombre de `#` de sa signature).
fn niveau(sig: &str) -> usize {
    sig.chars().take_while(|&c| c == '#').count().max(1)
}

/// `outline <fichier>` : rôle, imports, importeurs, symboles avec plages
/// (imbriqués), exports.
pub fn outline(handles: &[Handle], entree: &str, budget: usize) -> String {
    match resoudre(handles, entree) {
        Resolu::Trouve { h, cible, .. } => {
            let f = match cible {
                Cible::Noeud(g) => super::fichier_de(h, g),
                Cible::Lignes { fichier, .. } => fichier,
            };
            outline_de(h, f, budget)
        }
        r => echec(r, entree),
    }
}

pub(crate) fn outline_de(h: &Handle, f: u32, budget: usize) -> String {
    let Some(r) = h.node(f) else { return String::new() };
    let path = h.path_of(f);
    let ov = overlay::charger(h);
    let mut out = Out::new(budget);
    out.ligne(
        format!("{} {} {} lines{}", ids::file_id(path), Lang::from_u8(r.n.lang).as_str(), r.n.lines, marque(&ov, path))
            .trim_end()
            .to_string()
            .as_str(),
    );
    let rl = role(h, f);
    if !rl.is_empty() {
        out.ligne(&format!("role: {}", rl));
    }
    let importe: Vec<String> = {
        let mut v: Vec<u32> = h.imports_raw(f).iter().copied().filter(|&t| h.alive(t)).collect();
        v.sort_by(|&a, &b| h.path_of(a).cmp(h.path_of(b)));
        v.iter().map(|&t| ids::file_id(h.path_of(t))).collect()
    };
    if !importe.is_empty() {
        out.ligne(&format!("imports {}: {}", importe.len(), liste(&importe, 6)));
    }
    let importeurs: Vec<String> = h.importers(f).iter().map(|&t| ids::file_id(h.path_of(t))).collect();
    if !importeurs.is_empty() {
        out.ligne(&format!("imported by {}: {}", importeurs.len(), liste(&importeurs, 4)));
    }
    let syms: Vec<_> = h.symbols_of(f).into_iter().filter(|s| s.kind() != SymbolKind::Import).collect();
    let rangs = ids::ranks(h.symbols_of(f).iter().map(|s| (s.name(), s.kind())));
    let tous = h.symbols_of(f);
    let rang_de = |g: u32| tous.iter().position(|x| x.g == g).map(|i| rangs[i]).unwrap_or(1);
    let n_exports = syms.iter().filter(|s| exporte(s.str(s.n.signature))).count();
    if syms.is_empty() {
        return out.fin(Some(format!("read {}", ids::file_id(path))));
    }
    if n_exports > 0 {
        out.ligne(&format!("symbols {} ({} exported):", syms.len(), n_exports));
    } else {
        out.ligne(&format!("symbols {}:", syms.len()));
    }
    // Imbrication par inclusion des plages (ordre des lignes).
    let mut ordre: Vec<&crate::atlas::view::NodeRef> = syms.iter().collect();
    ordre.sort_by_key(|s| (s.n.line, std::cmp::Reverse(s.n.end_line)));
    let mut pile: Vec<u32> = Vec::new();
    let mut suite: Option<String> = None;
    let mut premier: Option<String> = None;
    for s in ordre {
        let id = ids::sym_id(path, s.name(), s.kind(), rang_de(s.g));
        let prof = if s.kind() == SymbolKind::Heading {
            niveau(s.str(s.n.signature)) - 1
        } else {
            while pile.last().is_some_and(|&fin| s.n.line > fin) {
                pile.pop();
            }
            let p = pile.len();
            if s.n.end_line > s.n.line {
                pile.push(s.n.end_line);
            }
            p
        };
        let exp = exporte(s.str(s.n.signature));
        if suite.is_none() && exp {
            suite = Some(format!("card {}", id));
        }
        if premier.is_none() {
            premier = Some(format!("card {}", id));
        }
        let mut l = format!("{}{} ", "  ".repeat(prof + 1), id);
        if s.kind() != SymbolKind::Heading {
            l.push_str(s.kind().as_str());
            l.push(' ');
        }
        l.push_str(&lines(s.n.line, s.n.end_line));
        if exp {
            l.push_str(" export");
        }
        let rs = s.str(s.n.summary);
        if !rs.is_empty() {
            l.push_str(" — ");
            l.push_str(&court(rs, 70));
        }
        out.ligne(&l);
    }
    out.fin(suite.or(premier))
}

// ─── read ───────────────────────────────────────────────────────────────────

/// Lignes d'un fichier du projet, lues sur disque (fin de ligne `\r` retirée).
pub(super) fn lignes_fichier(h: &Handle, path: &str) -> Option<Vec<String>> {
    let bytes = std::fs::read(std::path::Path::new(h.root()).join(path)).ok()?;
    let txt = String::from_utf8_lossy(&bytes);
    let mut v: Vec<String> = txt.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l).to_string()).collect();
    if v.last().is_some_and(|l| l.is_empty()) {
        v.pop();
    }
    Some(v)
}

/// Fin d'une section markdown : ligne avant le titre suivant de niveau ≤.
fn fin_section(h: &Handle, f: u32, g: u32, n_lignes: u32) -> u32 {
    let syms = h.symbols_of(f);
    let Some(i) = syms.iter().position(|s| s.g == g) else { return n_lignes };
    let lvl = niveau(syms[i].str(syms[i].n.signature));
    syms[i + 1..]
        .iter()
        .find(|s| s.kind() == SymbolKind::Heading && niveau(s.str(s.n.signature)) <= lvl)
        .map(|s| s.n.line.saturating_sub(1))
        .unwrap_or(n_lignes)
}

/// `read <id>` : les lignes exactes d'un symbole (ou d'une section, d'un
/// fichier, d'une plage), numérotées ; `contexte` lignes de plus de part et
/// d'autre.
pub fn read(handles: &[Handle], entree: &str, contexte: u32, budget: usize) -> String {
    lire(handles, entree, contexte, budget, None)
}

/// `read <id> --autour "<question>"` : comme `read`, mais si la plage dépasse
/// `SEUIL_AUTOUR` lignes, seules les lignes pertinentes pour la question sont rendues
/// (voir `rendre_autour`). `max_lignes` : plafond explicite (sinon `min(60, max(12, n/4))`).
pub fn read_autour(handles: &[Handle], entree: &str, contexte: u32, budget: usize, question: &str, max_lignes: Option<usize>) -> String {
    lire(handles, entree, contexte, budget, Some((question, max_lignes)))
}

fn lire(handles: &[Handle], entree: &str, contexte: u32, budget: usize, autour: Option<(&str, Option<usize>)>) -> String {
    let (h, cible) = match resoudre(handles, entree) {
        Resolu::Trouve { h, cible, .. } => (h, cible),
        r => return echec(r, entree),
    };
    let (f, g, a, b) = match cible {
        Cible::Noeud(g) => match h.node(g) {
            Some(r) if r.n.kind == 1 => (r.n.owner_file, Some(g), r.n.line, r.n.end_line.max(r.n.line)),
            Some(_) => (g, None, 1, u32::MAX),
            None => return echec(Resolu::Introuvable(entree.into()), entree),
        },
        Cible::Lignes { fichier, debut, fin } => (fichier, None, debut, fin),
    };
    let path = h.path_of(f).to_string();
    let Some(src) = lignes_fichier(h, &path) else {
        return format!(
            "(cortex) {} unreadable on disk (moved or deleted?)\nnext : find {}\n",
            ids::file_id(&path),
            path.rsplit('/').next().unwrap_or(&path)
        );
    };
    let n = src.len() as u32;
    let b = match g.and_then(|g| h.node(g)) {
        Some(r) if r.kind() == SymbolKind::Heading => fin_section(h, f, r.g, n),
        _ => b,
    };
    let a = a.saturating_sub(contexte).max(1).min(n.max(1));
    let b = b.saturating_add(contexte).min(n).max(a);
    let ov = overlay::charger(h);
    let mut out = Out::new(budget);
    // Provenance : l'identifiant porte le chemin, chaque ligne son numéro.
    let tete = match g.and_then(|g| h.node(g).map(|r| (g, r.n.line, r.n.end_line.max(r.n.line)))) {
        Some((g, l0, l1)) if (a, b) == (l0, l1) => format!("{}{}", entete(h, g), marque(&ov, &path)),
        Some((g, _, _)) => format!("{} (lines {}-{}){}", entete(h, g), a, b, marque(&ov, &path)),
        None => format!("{} L{}-{}{}", ids::file_id(&path), a, b, marque(&ov, &path)),
    };
    out.ligne(&tete);
    if let Some((question, max)) = autour {
        if !question.trim().is_empty() && b - a + 1 > SEUIL_AUTOUR {
            let tete_n = match g.and_then(|g| h.node(g)) {
                Some(r) if r.kind() == SymbolKind::Heading => 1,
                Some(r) if r.n.kind == 1 => lignes_signature(&src[a as usize - 1..b as usize]),
                _ => 0,
            };
            return rendre_autour(out, &src, &path, (a, b), tete_n, question.trim(), max);
        }
    }
    let mut premiere_coupee: Option<u32> = None;
    for i in a..=b {
        let txt = src.get(i as usize - 1).map(|s| s.as_str()).unwrap_or("");
        if !out.ligne(&format!("{}│{}", i, txt)) && premiere_coupee.is_none() {
            premiere_coupee = Some(i);
        }
    }
    let definition =
        g.filter(|&g| h.node(g).is_some_and(|r| r.kind() == SymbolKind::Decl)).and_then(|g| contreparties(h, g).first().copied());
    let suite = match (premiere_coupee, g) {
        (Some(c), _) => format!("read {}:{}-{}", path, c, b),
        (None, Some(_)) if definition.is_some() => format!("read {}", h.node_id(definition.unwrap())),
        (None, Some(g)) if !h.callers(g).is_empty() => format!("impact {}", h.node_id(g)),
        (None, Some(g)) => format!("card {}", h.node_id(g)),
        (None, None) => format!("outline {}", ids::file_id(&path)),
    };
    out.fin(Some(suite))
}

// ─── read --autour : les lignes pertinentes d'un GROS symbole ───────────────

/// Au-delà de ce nombre de lignes, `--autour` filtre ; en deçà, `read` est inchangé.
pub const SEUIL_AUTOUR: u32 = 80;
/// Plafond par défaut de lignes rendues (hors en-tête de provenance).
pub const AUTOUR_LIGNES_DEFAUT: usize = 60;
/// Contexte autour de chaque ligne retenue : 1 avant, 3 après.
const AUTOUR_AVANT: usize = 1;
const AUTOUR_APRES: usize = 3;
/// Largeur max (caractères) d'une ligne rendue.
const AUTOUR_LARGEUR: usize = 240;

/// Nombre de lignes de l'en-tête d'un symbole (signature qui peut courir sur plusieurs
/// lignes) : jusqu'à la première ligne qui ouvre le corps (`{`, `:`, `=>`, `;`), 4 au plus.
fn lignes_signature(corps: &[String]) -> usize {
    for (i, l) in corps.iter().take(4).enumerate() {
        let t = l.trim_end();
        if t.contains('{') || t.ends_with(':') || t.ends_with("=>") || t.ends_with(';') {
            return i + 1;
        }
    }
    corps.len().min(1)
}

/// Choisit les lignes à rendre : l'en-tête (`tete_n` premières), puis les fenêtres autour des
/// lignes les mieux classées, jusqu'à `cap` lignes. Rend le masque et la meilleure fenêtre.
fn choisir_lignes(scores: &[f32], tete_n: usize, cap: usize) -> (Vec<bool>, Option<(usize, usize)>) {
    let n = scores.len();
    let mut pris = vec![false; n];
    let mut total = 0usize;
    for p in pris.iter_mut().take(tete_n.min(cap)) {
        *p = true;
        total += 1;
    }
    // Même lissage que peek : une ligne vaut son score + 0,25 x ses voisines.
    let s = |i: usize| scores.get(i).copied().unwrap_or(0.0);
    let mut cand: Vec<(f32, usize)> =
        (0..n).filter(|&i| s(i) > 0.0).map(|i| (s(i) + 0.25 * (if i > 0 { s(i - 1) } else { 0.0 } + s(i + 1)), i)).collect();
    cand.sort_by(|x, y| y.0.total_cmp(&x.0).then(x.1.cmp(&y.1)));
    let mut meilleure: Option<(usize, usize)> = None;
    for &(_, i) in &cand {
        if pris[i] {
            continue;
        }
        let (mut lo, mut hi) = (i.saturating_sub(AUTOUR_AVANT), (i + AUTOUR_APRES).min(n - 1));
        let mut cout = (lo..=hi).filter(|&k| !pris[k]).count();
        if total + cout > cap {
            let reste = cap.saturating_sub(total);
            if reste < 2 {
                continue;
            }
            lo = i;
            hi = (i + reste - 1).min(hi);
            cout = (lo..=hi).filter(|&k| !pris[k]).count();
            if total + cout > cap {
                continue;
            }
        }
        for p in pris.iter_mut().take(hi + 1).skip(lo) {
            *p = true;
        }
        total += cout;
        meilleure.get_or_insert((lo, hi));
    }
    // Un trou d'UNE ligne entre deux fenêtres se comble (« 1 ligne omise » coûte plus qu'elle).
    for i in 1..n.saturating_sub(1) {
        if !pris[i] && pris[i - 1] && pris[i + 1] && total < cap {
            pris[i] = true;
            total += 1;
        }
    }
    (pris, meilleure)
}

/// Rend la plage `(a, b)` (numéros absolus, 1-based) de `src` réduite aux lignes pertinentes.
fn rendre_autour(
    mut out: Out,
    src: &[String],
    path: &str,
    (a, b): (u32, u32),
    tete_n: usize,
    question: &str,
    max: Option<usize>,
) -> String {
    let corps: Vec<&str> = (a..=b).map(|i| src.get(i as usize - 1).map(|s| s.as_str()).unwrap_or("")).collect();
    let n = corps.len();
    let cap = match max {
        Some(m) => m.clamp(4, 2000),
        None => AUTOUR_LIGNES_DEFAUT.min((n / 4).max(12)),
    };
    let scores = crate::peek::scores_lignes(&corps, question);
    let (pris, meilleure) = choisir_lignes(&scores, tete_n, cap);
    let montrees = pris.iter().filter(|&&p| p).count();
    let hors_tete = scores.iter().enumerate().any(|(i, &s)| s > 0.0 && i >= tete_n);
    if !hors_tete {
        out.ligne(&format!(
            "(no line of these {} matches \"{}\": signature only; try other words, or read {}:{}-{})",
            n, question, path, a, b
        ));
    } else {
        out.ligne(&format!("(around \"{}\": {} of {} lines, absolute line numbers)", question, montrees, n));
    }
    let mut i = 0usize;
    while i < n {
        if pris[i] {
            let ln = a as usize + i;
            let txt = corps[i];
            let txt = if txt.chars().count() > AUTOUR_LARGEUR {
                format!("{}…", txt.chars().take(AUTOUR_LARGEUR).collect::<String>())
            } else {
                txt.to_string()
            };
            out.ligne(&format!("{}│{}", ln, txt));
            i += 1;
        } else {
            let j = (i..n).find(|&k| pris[k]).unwrap_or(n);
            out.ligne(&format!("… {} lines omitted (L{}-{})", j - i, a as usize + i, a as usize + j - 1));
            i = j;
        }
    }
    let suite = match meilleure {
        Some((lo, hi)) => format!("read {}:{}-{}", path, a as usize + lo, a as usize + hi),
        None => format!("read {}:{}-{}", path, a, b),
    };
    out.fin(Some(suite))
}

#[cfg(test)]
mod tests_autour {
    use super::*;
    use crate::atlas::tests::clean;

    /// Gros symbole (200 lignes de corps) avec trois endroits distinctifs, plus un petit symbole.
    fn projet(tag: &str) -> (String, std::path::PathBuf, Handle) {
        let name = format!("cortex-test-autour-{}", tag);
        clean(&name);
        let dir = std::env::temp_dir().join(format!("cortex-autour-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let mut s = String::from("export function petit(x: number) {\n  return x + 1\n}\n\nexport function gros(x: number, y: string) {\n");
        for k in 0..200 {
            match k {
                60 => s.push_str("  const backoffMs = retryConnection(x) * 2 // exponential backoff before the next attempt\n"),
                120 => s.push_str("  const jeton = rafraichirJetonSession(y) // session token renewal\n"),
                170 => s.push_str("  logger.warn('quota exceeded for tenant', y)\n"),
                _ => s.push_str(&format!("  const v{} = x + {} // filler line number {}\n", k, k, k)),
            }
        }
        s.push_str("  return x\n}\n");
        std::fs::write(dir.join("src/mod.ts"), s).unwrap();
        let (idx, tracked) = crate::index::build_index(&name, &dir).unwrap();
        crate::atlas::rebuild_full(&name, &idx, tracked).unwrap();
        let h = Handle::open(&name).unwrap();
        (name, dir, h)
    }

    fn nombre_lignes(sortie: &str) -> usize {
        sortie.lines().filter(|l| l.split_once('│').is_some_and(|(n, _)| n.parse::<u32>().is_ok())).count()
    }

    #[test]
    fn autour_gros_symbole_lignes_pertinentes_numeros_absolus() {
        let (name, dir, h) = projet("gros");
        let hs = std::slice::from_ref(&h);
        let plein = read(hs, "S:src/mod.ts#gros", 0, 4000);
        let total = nombre_lignes(&plein);
        assert_eq!(total, 203, "{}", plein.lines().take(3).collect::<Vec<_>>().join("|"));
        let r = read_autour(hs, "S:src/mod.ts#gros", 0, 4000, "retry connection backoff", None);
        // Provenance, signature toujours, ligne attendue au numéro ABSOLU du fichier.
        assert!(r.starts_with("S:src/mod.ts#gros fn L5-"), "{}", r);
        assert!(r.contains("5│export function gros(x: number, y: string) {"), "{}", r);
        assert!(r.contains("66│  const backoffMs = retryConnection(x) * 2"), "{}", r);
        assert!(r.contains("lines omitted"), "{}", r);
        assert!(nombre_lignes(&r) * 4 <= total, "{} lignes sur {}\n{}", nombre_lignes(&r), total, r);
        assert!(!r.contains("session token"), "{}", r);
        // Autre question, autre partie du corps.
        let r = read_autour(hs, "S:src/mod.ts#gros", 0, 4000, "session token renewal", None);
        assert!(r.contains("126│  const jeton = rafraichirJetonSession(y)"), "{}", r);
        assert!(!r.contains("backoffMs"), "{}", r);
        // Suite proposée : relire la meilleure fenêtre avec les numéros absolus.
        assert!(r.contains("next : read src/mod.ts:"), "{}", r);
        drop(h);
        clean(&name);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn autour_plafond_explicite_et_aucune_ligne_ne_correspond() {
        let (name, dir, h) = projet("plafond");
        let hs = std::slice::from_ref(&h);
        let r = read_autour(hs, "S:src/mod.ts#gros", 0, 4000, "filler line number", Some(10));
        assert!(nombre_lignes(&r) <= 10, "{}", r);
        let r = read_autour(hs, "S:src/mod.ts#gros", 0, 4000, "zorglub introuvable", None);
        assert!(r.contains("no line of these") && r.contains("5│export function gros"), "{}", r);
        assert_eq!(nombre_lignes(&r), 1, "signature seule : {}", r);
        drop(h);
        clean(&name);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Non-régression : sans `--autour`, ou sous le seuil, la sortie est celle de `read`.
    #[test]
    fn autour_sans_effet_sous_le_seuil_et_read_inchange() {
        let (name, dir, h) = projet("seuil");
        let hs = std::slice::from_ref(&h);
        let normal = read(hs, "S:src/mod.ts#petit", 0, 4000);
        assert_eq!(
            normal,
            "S:src/mod.ts#petit fn L1-3\n1│export function petit(x: number) {\n2│  return x + 1\n3│}\nnext : card S:src/mod.ts#petit\n"
        );
        assert_eq!(read_autour(hs, "S:src/mod.ts#petit", 0, 4000, "return", None), normal);
        // Question vide : pas de filtrage.
        assert_eq!(read_autour(hs, "S:src/mod.ts#gros", 0, 4000, "  ", None), read(hs, "S:src/mod.ts#gros", 0, 4000));
        // Plage de fichier > seuil : filtrée aussi, numéros absolus.
        let r = read_autour(hs, "src/mod.ts:1-203", 0, 4000, "quota exceeded tenant", None);
        assert!(r.contains("176│  logger.warn('quota exceeded for tenant', y)"), "{}", r);
        drop(h);
        clean(&name);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn choisir_lignes_respecte_le_plafond_et_comble_les_trous_d_une_ligne() {
        let mut sc = vec![0.0f32; 100];
        for i in [10, 30, 50, 70, 90] {
            sc[i] = 5.0;
        }
        let (pris, meilleure) = choisir_lignes(&sc, 1, 12);
        assert!(pris.iter().filter(|&&p| p).count() <= 12);
        assert!(pris[0], "l'en-tête est toujours pris");
        assert!(meilleure.is_some());
        // Deux fenêtres séparées d'une seule ligne se rejoignent.
        let mut sc = vec![0.0f32; 40];
        sc[10] = 5.0;
        sc[15] = 4.0;
        let (pris, _) = choisir_lignes(&sc, 1, 30);
        assert!((9..=18).all(|i| pris[i]), "{:?}", pris);
        assert_eq!(lignes_signature(&["fn a(".into(), "  x: u32,".into(), ") -> u32 {".into(), "  1".into()]), 3);
        assert_eq!(lignes_signature(&["export const X = {".into()]), 1);
    }
}
