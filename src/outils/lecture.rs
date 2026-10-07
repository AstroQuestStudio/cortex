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
        let path = h.path_of(r.n.owner_file);
        let mut l = format!(
            "{}{} {} {}{}",
            if multi { format!("[{}] ", h.project) } else { String::new() },
            id,
            r.kind().as_str(),
            lines(r.n.line, r.n.end_line),
            marque(&ovs[*hi], path)
        );
        if rang < FIND_ROLES {
            let mut role_s = r.str(r.n.summary);
            if role_s.is_empty() {
                role_s = role_decl;
            }
            if role_s.is_empty() {
                role_s = role(h, r.n.owner_file);
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
fn tests_du_symbole(h: &Handle, g: u32, appelants: &[u32]) -> Vec<u32> {
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

fn exporte(sig: &str) -> bool {
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
fn lignes_fichier(h: &Handle, path: &str) -> Option<Vec<String>> {
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
