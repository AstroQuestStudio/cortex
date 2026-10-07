//! I4 (amorce) — superposition du travail non commité.
//!
//! L'atlas suit le disque (contrôle de fraîcheur) : il décrit déjà le code EN
//! COURS d'écriture. L'overlay dit ce qui, dans ce code, n'est pas commité :
//! les fichiers que `git status` rapporte (modifiés, indexés mais non commités,
//! nouveaux non suivis, supprimés). Chaque sortie les marque `✎`, et `changed`
//! résume les fonctions touchées (plages des hunks de `git diff HEAD` croisées
//! avec les plages des symboles de l'atlas) et leur impact.
//!
//! Coût : `git status` (~100 ms sur AstroQuest) n'est relancé que si sa
//! réponse peut avoir changé. La réponse est mémorisée dans
//! `~/.cortex/<projet>/overlay.json` sous une clé faite de l'état de l'index git
//! (taille, date), de `HEAD`, de la branche courante et de la pile de segments
//! de l'atlas — toute modification d'un fichier suivi par l'atlas écrit un
//! segment, tout `git add`/`commit`/`checkout` touche l'index ou `HEAD`. Un
//! appel sans changement ne coûte que quelques `stat`.

use super::{entete, fichier_de, liste, tests_par_nom, Out};
use crate::atlas::Handle;
use crate::ids;
use crate::search::is_test_path;
use crate::symbol::SymbolKind;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// État d'un fichier non commité.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Etat {
    Modifie,
    Nouveau,
    Supprime,
}

impl Etat {
    fn libelle(self) -> &'static str {
        match self {
            Etat::Modifie => "modified",
            Etat::Nouveau => "new",
            Etat::Supprime => "deleted",
        }
    }
}

/// Fichiers non commités d'un projet (chemins relatifs à sa racine).
#[derive(Debug, Default, Clone)]
pub struct Overlay {
    /// Vrai si la racine est dans un dépôt git lisible.
    pub git: bool,
    pub fichiers: Vec<(String, Etat)>,
    set: HashSet<String>,
}

impl Overlay {
    pub fn contient(&self, path: &str) -> bool {
        self.set.contains(path)
    }

    fn from(git: bool, fichiers: Vec<(String, Etat)>) -> Overlay {
        let set = fichiers.iter().filter(|(_, e)| *e != Etat::Supprime).map(|(p, _)| p.clone()).collect();
        Overlay { git, fichiers, set }
    }
}

#[derive(Serialize, Deserialize)]
struct Cache {
    cle: String,
    fichiers: Vec<(String, Etat)>,
}

/// Dépôt git contenant `root` : (dossier git, racine du dépôt).
fn depot(root: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut dir = Some(root);
    while let Some(d) = dir {
        let g = d.join(".git");
        if g.is_dir() {
            return Some((g, d.to_path_buf()));
        }
        if g.is_file() {
            // Worktree : « gitdir: <chemin> ».
            let txt = std::fs::read_to_string(&g).ok()?;
            let p = txt.trim().strip_prefix("gitdir:")?.trim();
            let p = if Path::new(p).is_absolute() { PathBuf::from(p) } else { d.join(p) };
            return Some((p, d.to_path_buf()));
        }
        dir = d.parent();
    }
    None
}

/// Préfixe de la racine du projet dans le dépôt (`""` ou `sous/dossier/`).
fn prefixe(root: &Path, top: &Path) -> String {
    let r = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let t = top.canonicalize().unwrap_or_else(|_| top.to_path_buf());
    match r.strip_prefix(&t) {
        Ok(p) if !p.as_os_str().is_empty() => format!("{}/", p.to_string_lossy().replace('\\', "/")),
        _ => String::new(),
    }
}

fn stat_cle(p: &Path) -> String {
    match std::fs::metadata(p) {
        Ok(m) => format!("{}:{}", m.len(), crate::index::mtime_micros(&m)),
        Err(_) => "-".into(),
    }
}

fn git(root: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let mut c = std::process::Command::new("git");
    c.arg("-C").arg(root).arg("--no-optional-locks").args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = c.output().ok()?;
    out.status.success().then_some(out.stdout)
}

/// `git status` (porcelaine v1, -z) → fichiers non commités sous la racine.
fn statut(root: &Path, pref: &str) -> Option<Vec<(String, Etat)>> {
    let out = git(root, &["status", "--porcelain=v1", "-z", "--untracked-files=all", "--no-renames", "--", "."])?;
    let mut v = Vec::new();
    for e in out.split(|&b| b == 0) {
        if e.len() < 4 {
            continue;
        }
        let (x, y) = (e[0], e[1]);
        let path = String::from_utf8_lossy(&e[3..]).to_string();
        let Some(rel) = path.strip_prefix(pref) else { continue };
        let etat = if x == b'?' || x == b'A' {
            Etat::Nouveau
        } else if x == b'D' || y == b'D' {
            Etat::Supprime
        } else {
            Etat::Modifie
        };
        v.push((rel.to_string(), etat));
    }
    v.sort();
    Some(v)
}

fn chemin_cache(project: &str) -> PathBuf {
    crate::index::cortex_home().join(project).join("overlay.json")
}

/// Overlay d'un projet (mémorisé, voir l'en-tête).
pub fn charger(h: &Handle) -> Overlay {
    let root = PathBuf::from(h.root());
    let Some((gitdir, top)) = depot(&root) else { return Overlay::default() };
    let pref = prefixe(&root, &top);
    let head = std::fs::read_to_string(gitdir.join("HEAD")).unwrap_or_default();
    let branche = head.trim().strip_prefix("ref:").map(|r| stat_cle(&gitdir.join(r.trim()))).unwrap_or_default();
    let cle = format!("{}|{}|{}|{}|{}", stat_cle(&gitdir.join("index")), head.trim(), branche, h.segments_cle(), pref);
    let chemin = chemin_cache(&h.project);
    if let Ok(txt) = std::fs::read_to_string(&chemin) {
        if let Ok(c) = serde_json::from_str::<Cache>(&txt) {
            if c.cle == cle {
                return Overlay::from(true, c.fichiers);
            }
        }
    }
    let Some(fichiers) = statut(&root, &pref) else { return Overlay::default() };
    let c = Cache { cle, fichiers };
    if let Ok(txt) = serde_json::to_string(&c) {
        let tmp = chemin.with_extension(format!("json.{}", std::process::id()));
        if std::fs::write(&tmp, txt).is_ok() && std::fs::rename(&tmp, &chemin).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
    Overlay::from(true, c.fichiers)
}

/// Plages (côté nouveau) modifiées par fichier, d'après `git diff -U0 HEAD`.
fn hunks(root: &Path, pref: &str, chemins: &[&str]) -> std::collections::HashMap<String, Vec<(u32, u32)>> {
    let mut out: std::collections::HashMap<String, Vec<(u32, u32)>> = std::collections::HashMap::new();
    if chemins.is_empty() {
        return out;
    }
    let mut args: Vec<&str> = vec!["diff", "-U0", "--no-color", "--no-ext-diff", "HEAD", "--"];
    args.extend(chemins.iter().copied());
    let Some(txt) = git(root, &args).or_else(|| {
        args.retain(|a| *a != "HEAD");
        git(root, &args)
    }) else {
        return out;
    };
    let txt = String::from_utf8_lossy(&txt);
    let mut courant: Option<String> = None;
    for l in txt.lines() {
        if let Some(p) = l.strip_prefix("+++ ") {
            courant = p.strip_prefix("b/").and_then(|p| p.strip_prefix(pref)).map(|s| s.to_string());
        } else if let (Some(c), Some(h)) = (&courant, l.strip_prefix("@@ ")) {
            // « -a,b +c,d @@ »
            let Some(plus) = h.split_whitespace().find(|t| t.starts_with('+')) else { continue };
            let plus = &plus[1..];
            let (c0, d) = match plus.split_once(',') {
                Some((c0, d)) => (c0.parse::<u32>().unwrap_or(0), d.parse::<u32>().unwrap_or(0)),
                None => (plus.parse::<u32>().unwrap_or(0), 1),
            };
            // Suppression pure (d = 0) : lignes retirées après la ligne c0.
            let (a, b) = if d == 0 { (c0.max(1), c0.max(1)) } else { (c0, c0 + d - 1) };
            out.entry(c.clone()).or_default().push((a, b));
        }
    }
    out
}

/// Symboles touchés par des plages : ceux qui chevauchent une plage, sans les
/// englobants dont un symbole interne est déjà touché.
fn symboles_touches(h: &Handle, f: u32, plages: &[(u32, u32)]) -> Vec<u32> {
    let syms: Vec<(u32, u32, u32)> = h
        .symbols_of(f)
        .iter()
        .filter(|r| r.kind() != SymbolKind::Import && r.kind() != SymbolKind::Heading)
        .map(|r| (r.g, r.n.line, if r.n.end_line == 0 { r.n.line } else { r.n.end_line }))
        .collect();
    let touche = |&(_, a, b): &(u32, u32, u32)| plages.iter().any(|&(x, y)| x <= b && y >= a);
    let t: Vec<&(u32, u32, u32)> = syms.iter().filter(|s| touche(s)).collect();
    t.iter().filter(|s| !t.iter().any(|o| o.0 != s.0 && o.1 >= s.1 && o.2 <= s.2 && (o.1, o.2) != (s.1, s.2))).map(|s| s.0).collect()
}

/// `changed` : fichiers non commités, fonctions touchées et leur impact.
pub fn changed(handles: &[Handle], budget: usize) -> String {
    let mut out = Out::new(budget);
    let mut suite: Option<String> = None;
    let mut rien = true;
    for h in handles {
        let ov = charger(h);
        if !ov.git || ov.fichiers.is_empty() {
            continue;
        }
        let root = PathBuf::from(h.root());
        let pref = depot(&root).map(|(_, top)| prefixe(&root, &top)).unwrap_or_default();
        // Seuls les fichiers que l'atlas connaît (ou connaissait : supprimés
        // d'un langage indexable) ont des symboles à rapporter.
        let suivis: Vec<(&str, Etat, Option<u32>)> = ov
            .fichiers
            .iter()
            .map(|(p, e)| (p.as_str(), *e, h.file_by_path(p)))
            .filter(|(p, e, g)| g.is_some() || (*e == Etat::Supprime && crate::lang::Lang::from_path(p).is_indexable()))
            .collect();
        if suivis.is_empty() {
            continue;
        }
        rien = false;
        let modifies: Vec<&str> = suivis.iter().filter(|(_, e, _)| *e == Etat::Modifie).map(|(p, _, _)| *p).collect();
        let plages = hunks(&root, &pref, &modifies);
        if handles.len() > 1 {
            out.ligne(&format!("[{}]", h.project));
        }
        // Par fichier : symboles de CODE touchés (titres de doc exclus).
        let code = |f: u32| {
            h.symbols_of(f)
                .iter()
                .filter(|r| !matches!(r.kind(), SymbolKind::Import | SymbolKind::Heading))
                .map(|r| r.g)
                .collect::<Vec<u32>>()
        };
        let mut detailles: Vec<(&str, Etat, Vec<u32>)> = Vec::new();
        let mut nouveaux: Vec<(&str, usize)> = Vec::new();
        let mut supprimes: Vec<String> = Vec::new();
        let mut autres: Vec<String> = Vec::new();
        let mut touches_total: Vec<u32> = Vec::new();
        for (p, e, g) in &suivis {
            let Some(f) = *g else {
                supprimes.push(ids::file_id(p));
                continue;
            };
            match e {
                Etat::Nouveau => {
                    let c = code(f);
                    touches_total.extend(c.iter().copied());
                    if c.is_empty() {
                        autres.push(ids::file_id(p));
                    } else {
                        nouveaux.push((p, c.len()));
                    }
                }
                _ => {
                    let t = symboles_touches(h, f, plages.get(*p).map(|v| v.as_slice()).unwrap_or(&[]));
                    touches_total.extend(t.iter().copied());
                    if t.is_empty() {
                        autres.push(ids::file_id(p));
                    } else {
                        detailles.push((p, *e, t));
                    }
                }
            }
        }
        // Impact hors du travail en cours : appelants dont le fichier n'est pas modifié.
        let changes: HashSet<u32> = suivis.iter().filter_map(|(_, _, g)| *g).collect();
        let mut appelants_ext: Vec<u32> = Vec::new();
        let mut tests: Vec<u32> = Vec::new();
        let mut meilleur: Option<(usize, u32)> = None;
        let mut n_app: std::collections::HashMap<u32, Vec<(u32, Option<u32>)>> = std::collections::HashMap::new();
        for &s in &touches_total {
            let callers = super::appelants(h, s);
            for &(c, _) in &callers {
                let cf = fichier_de(h, c);
                if is_test_path(h.path_of(cf)) {
                    tests.push(cf);
                } else if !changes.contains(&cf) {
                    appelants_ext.push(c);
                }
            }
            if meilleur.is_none_or(|(n, _)| callers.len() > n) && !callers.is_empty() {
                meilleur = Some((callers.len(), s));
            }
            n_app.insert(s, callers);
        }
        for (p, _, g) in &suivis {
            if g.is_some() {
                tests.extend(tests_par_nom(h, p, None));
            }
        }
        tests.sort_by(|&a, &b| h.path_of(a).cmp(h.path_of(b)));
        tests.dedup();
        appelants_ext.sort_by(|&a, &b| h.sort_key(a).cmp(&h.sort_key(b)));
        appelants_ext.dedup();
        let n_fichiers_ext: HashSet<u32> = appelants_ext.iter().map(|&c| fichier_de(h, c)).collect();
        out.ligne(&format!(
            "✎ uncommitted: {} file(s), {} code file(s) touched, {} symbol(s) touched; {} caller(s) outside the work in progress ({} files); {} test(s) to re-run",
            suivis.len(),
            detailles.len() + nouveaux.len(),
            touches_total.len(),
            appelants_ext.len(),
            n_fichiers_ext.len(),
            tests.len()
        ));
        if !tests.is_empty() {
            let t: Vec<String> = tests.iter().map(|&t| h.node_id(t)).collect();
            out.ligne(&format!("tests : {}", liste(&t, 8)));
        }
        // Les fichiers dont les symboles touchés ont le plus d'appelants d'abord.
        let poids = |t: &[u32]| t.iter().map(|s| n_app.get(s).map_or(0, |v| v.len())).sum::<usize>();
        detailles.sort_by(|a, b| poids(&b.2).cmp(&poids(&a.2)).then_with(|| a.0.cmp(b.0)));
        for (p, e, touches) in &detailles {
            out.ligne(&format!("{} ✎ {} · {} symbol(s) touched", ids::file_id(p), e.libelle(), touches.len()));
            for &s in touches {
                let n = n_app.get(&s).cloned().unwrap_or_default();
                let ex =
                    n.first().map(|&(c, site)| format!(" — e.g. {}{}", h.node_id(c), site.map(|l| format!(" L{}", l)).unwrap_or_default()));
                out.ligne(&format!("  {} · {} caller(s){}", entete(h, s), n.len(), ex.unwrap_or_default()));
            }
        }
        for (p, n) in &nouveaux {
            out.ligne(&format!("{} ✎ new · {} symbol(s)", ids::file_id(p), n));
        }
        if !supprimes.is_empty() {
            out.ligne(&format!("deleted {}: {}", supprimes.len(), liste(&supprimes, 10)));
        }
        if !autres.is_empty() {
            out.ligne(&format!("others (no code symbol touched) {}: {}", autres.len(), liste(&autres, 10)));
        }
        if suite.is_none() {
            suite = meilleur
                .map(|(_, s)| format!("impact {}", h.node_id(s)))
                .or_else(|| touches_total.first().map(|&s| format!("read {}", h.node_id(s))));
        }
    }
    if rien {
        return "(cortex) no uncommitted work in the indexed projects (or no git repository)\n".into();
    }
    out.fin(suite)
}
