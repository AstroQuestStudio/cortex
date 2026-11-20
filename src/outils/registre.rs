//! Registre de lectures partagé : plusieurs agents sur un même projet, une même mission.
//!
//! Quand une MISSION est donnée (`--mission`, argument MCP `mission`, ou `CORTEX_MISSION`), chaque
//! `card` et `read` réussi est noté (agent, identifiant stable, état du fichier, résumé). Si un
//! appelant demande `--partage` (ou `partage: true`), un identifiant déjà lu par un agent de la
//! mission, dont le fichier n'a pas changé depuis, ne revient pas en entier : « déjà lu par
//! l'agent X : résumé » (quelques lignes au lieu de centaines). `--frais` force la lecture complète.
//!
//! Local et sans fuite : un fichier JSONL par (projet, mission) sous `~/.cortex/registre/<projet>/`,
//! jamais lu hors de sa mission ; aucune mission = aucun effet (comportement inchangé) ; entrées
//! périmées au bout de 12 h, fichiers effacés au bout de 48 h.

use super::{noeud_fichier, resoudre, Appel, Cible, Resolu};
use crate::atlas::Handle;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::io::Write;
use std::path::PathBuf;

/// Contexte de l'appelant (une mission, un agent).
#[derive(Debug, Clone, Default)]
pub struct Ctx {
    pub mission: String,
    pub agent: String,
    /// Rendre « déjà lu » au lieu du contenu complet, si possible.
    pub partage: bool,
    /// Force la lecture complète (ignore le registre en lecture).
    pub frais: bool,
}

thread_local! {
    static CTX: RefCell<Option<Ctx>> = const { RefCell::new(None) };
}

/// Pose (ou efface) le contexte de l'appel en cours.
pub fn definir(c: Option<Ctx>) {
    CTX.with(|x| *x.borrow_mut() = c);
}

fn courant() -> Option<Ctx> {
    CTX.with(|x| x.borrow().clone())
}

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn vrai(s: &str) -> bool {
    matches!(s.to_ascii_lowercase().as_str(), "1" | "true" | "oui" | "yes")
}

impl Ctx {
    /// Contexte depuis des valeurs explicites, repli sur l'environnement. `None` : pas de mission.
    pub fn depuis(mission: Option<String>, agent: Option<String>, partage: bool, frais: bool) -> Option<Ctx> {
        let mission = mission.filter(|m| !m.trim().is_empty()).or_else(|| env("CORTEX_MISSION"))?;
        Some(Ctx {
            mission,
            agent: agent.filter(|a| !a.trim().is_empty()).or_else(|| env("CORTEX_AGENT")).unwrap_or_else(|| "agent".into()),
            partage: partage || env("CORTEX_PARTAGE").is_some_and(|v| vrai(&v)),
            frais,
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Entree {
    /// Secondes depuis l'époque Unix.
    t: u64,
    agent: String,
    outil: String,
    id: String,
    /// État du fichier au moment de la lecture (taille + date).
    cle: String,
    jetons: usize,
    resume: String,
}

const PEREMPTION_S: u64 = 12 * 3600;
const EFFACEMENT_S: u64 = 48 * 3600;
const TAILLE_MAX: u64 = 400 * 1024;

fn nettoye(s: &str) -> String {
    let n: String = s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).take(80).collect();
    if n.is_empty() {
        "_".into()
    } else {
        n
    }
}

fn dossier(project: &str) -> PathBuf {
    crate::index::cortex_home().join("registre").join(nettoye(project))
}

fn fichier(project: &str, mission: &str) -> PathBuf {
    dossier(project).join(format!("{}.jsonl", nettoye(mission)))
}

fn maintenant() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn lire(project: &str, mission: &str) -> Vec<Entree> {
    let Ok(t) = std::fs::read_to_string(fichier(project, mission)) else { return Vec::new() };
    let now = maintenant();
    t.lines().filter_map(|l| serde_json::from_str::<Entree>(l).ok()).filter(|e| now.saturating_sub(e.t) <= PEREMPTION_S).collect()
}

fn ajouter(project: &str, mission: &str, e: &Entree) {
    let d = dossier(project);
    if std::fs::create_dir_all(&d).is_err() {
        return;
    }
    let f = fichier(project, mission);
    // Trop gros : on garde la seconde moitié.
    if std::fs::metadata(&f).is_ok_and(|m| m.len() > TAILLE_MAX) {
        if let Ok(t) = std::fs::read_to_string(&f) {
            let l: Vec<&str> = t.lines().collect();
            let _ = std::fs::write(&f, l[l.len() / 2..].join("\n") + "\n");
        }
    }
    if let Ok(mut fh) = std::fs::OpenOptions::new().create(true).append(true).open(&f) {
        if let Ok(l) = serde_json::to_string(e) {
            // Une seule écriture : les ajouts concurrents ne s'entrelacent pas.
            let _ = fh.write_all(format!("{l}\n").as_bytes());
        }
    }
    // Ménage : fichiers de mission abandonnés depuis plus de 48 h.
    if let Ok(rd) = std::fs::read_dir(&d) {
        for x in rd.flatten() {
            let vieux =
                x.metadata().and_then(|m| m.modified()).ok().and_then(|m| m.elapsed().ok()).is_some_and(|a| a.as_secs() > EFFACEMENT_S);
            if vieux {
                let _ = std::fs::remove_file(x.path());
            }
        }
    }
}

/// Clé de fraîcheur du fichier qui porte `g` : taille et date de modification.
fn cle_fichier(h: &Handle, f: u32) -> String {
    let p = std::path::Path::new(h.root()).join(h.path_of(f));
    match std::fs::metadata(&p) {
        Ok(m) => format!("{}:{}", m.len(), crate::index::mtime_micros(&m)),
        Err(_) => "-".into(),
    }
}

fn resume(sortie: &str) -> String {
    let mut s = String::new();
    let total = sortie.lines().count();
    for l in sortie.lines().take(4) {
        let l: String = l.chars().take(160).collect();
        if !s.is_empty() {
            s.push('\n');
        }
        s.push_str(&l);
    }
    if total > 4 {
        s.push_str(&format!(
            "
… +{} more line(s)",
            total - 4
        ));
    }
    s
}

fn jetons(s: &str) -> usize {
    s.chars().count() / 4
}

fn il_y_a(s: u64) -> String {
    match s {
        0..=89 => format!("{s}s"),
        90..=5399 => format!("{} min", s / 60),
        _ => format!("{} h", s / 3600),
    }
}

/// Identité d'un appel `card` / `read` : (outil, identifiant stable, projet, clé de fraîcheur).
fn identite(handles: &[Handle], appel: &Appel) -> Option<(String, String, String, String)> {
    let (outil, cible, ctx) = match appel {
        Appel::Card { cible } => ("card", cible, 0),
        Appel::Read { cible, contexte } => ("read", cible, *contexte),
        _ => return None,
    };
    match resoudre(handles, cible) {
        Resolu::Trouve { h, cible: c, .. } => {
            let id = match c {
                Cible::Noeud(g) => h.node_id(g),
                Cible::Lignes { fichier, debut, fin } => format!("{}:{}-{}", h.path_of(fichier), debut, fin),
            };
            let cle = cle_fichier(h, noeud_fichier(h, c));
            Some((outil.to_string(), format!("{id}+{ctx}"), h.project.clone(), cle))
        }
        _ => None,
    }
}

/// Enveloppe `card` / `read` : « déjà lu » si demandé et possible, sinon exécute et note.
pub fn avec_registre(handles: &[Handle], appel: &Appel, executer: impl FnOnce() -> String) -> String {
    let Some(ctx) = courant() else { return executer() };
    let Some((outil, id, project, cle)) = identite(handles, appel) else { return executer() };
    if ctx.partage && !ctx.frais {
        let now = maintenant();
        let deja = lire(&project, &ctx.mission).into_iter().rev().find(|e| e.outil == outil && e.id == id && e.cle == cle);
        if let Some(e) = deja {
            let par = if e.agent == ctx.agent { "you".to_string() } else { format!("agent {}", e.agent) };
            return format!(
                "already read by {} {} ago (~{} tokens, file unchanged since): {}\n{}\nnext : same call with --frais (MCP frais:true) for the full output\n",
                par,
                il_y_a(now.saturating_sub(e.t)),
                e.jetons,
                id.rsplit_once('+').map(|x| x.0).unwrap_or(&id),
                e.resume
            );
        }
    }
    let sortie = executer();
    if !sortie.starts_with("(cortex)") {
        ajouter(
            &project,
            &ctx.mission,
            &Entree { t: maintenant(), agent: ctx.agent.clone(), outil, id, cle, jetons: jetons(&sortie), resume: resume(&sortie) },
        );
    }
    sortie
}

/// `registre` : ce que les agents de la mission ont lu (le plus récent en bas).
pub fn lister(project: Option<&str>, mission: &str, max: usize) -> String {
    let base = crate::index::cortex_home().join("registre");
    let mut projets: Vec<String> = match project {
        Some(p) => vec![nettoye(p)],
        None => {
            std::fs::read_dir(&base).map(|r| r.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect()).unwrap_or_default()
        }
    };
    projets.sort();
    let now = maintenant();
    let mut out = String::new();
    for p in projets {
        let es = lire(&p, mission);
        if es.is_empty() {
            continue;
        }
        let total: usize = es.iter().map(|e| e.jetons).sum();
        let mut agents: Vec<&str> = es.iter().map(|e| e.agent.as_str()).collect();
        agents.sort();
        agents.dedup();
        out.push_str(&format!(
            "[{}] mission {} : {} read(s) by {} agent(s) ({}), ~{} tokens\n",
            p,
            mission,
            es.len(),
            agents.len(),
            agents.join(", "),
            total
        ));
        for e in es.iter().rev().take(max).collect::<Vec<_>>().into_iter().rev() {
            out.push_str(&format!(
                "  {:>6} ago  {:<10} {:<5} {}  (~{})\n",
                il_y_a(now.saturating_sub(e.t)),
                e.agent,
                e.outil,
                e.id.rsplit_once('+').map(|x| x.0).unwrap_or(&e.id),
                e.jetons
            ));
        }
    }
    if out.is_empty() {
        out = format!("(cortex) nothing registered for mission '{}'\n", mission);
    }
    out
}

/// Efface la mission (tous projets, ou un seul).
pub fn effacer(project: Option<&str>, mission: &str) -> String {
    let base = crate::index::cortex_home().join("registre");
    let mut n = 0;
    if let Ok(rd) = std::fs::read_dir(&base) {
        for e in rd.flatten() {
            if project.is_some_and(|p| nettoye(p) != e.file_name().to_string_lossy()) {
                continue;
            }
            if std::fs::remove_file(e.path().join(format!("{}.jsonl", nettoye(mission)))).is_ok() {
                n += 1;
            }
        }
    }
    format!("mission '{}' : {} registry file(s) deleted\n", mission, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contexte_sans_mission_est_inactif() {
        std::env::remove_var("CORTEX_MISSION");
        assert!(Ctx::depuis(None, Some("a".into()), true, false).is_none());
        let c = Ctx::depuis(Some("m1".into()), None, true, false).unwrap();
        assert_eq!((c.mission.as_str(), c.agent.as_str(), c.partage), ("m1", "agent", true));
    }

    #[test]
    fn nettoyage_des_noms() {
        assert_eq!(nettoye("../../etc/passwd"), "______etc_passwd");
        assert_eq!(nettoye(""), "_");
    }

    #[test]
    fn resume_borne() {
        let s = "a\nb\nc\nd\ne\nf";
        assert_eq!(resume(s).lines().count(), 5);
        assert!(resume(s).ends_with("+2 more line(s)"));
    }
}
