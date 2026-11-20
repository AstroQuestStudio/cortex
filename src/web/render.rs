//! Option B de `docs/WEB.md` §6 : rendu JavaScript d'une page par Spectra.
//!
//! Cortex ne lance jamais Chrome lui-même : il appelle l'exécutable de Spectra
//! (`spectra-server capture <url> --json-responses --out <fichier>`, contrat documenté dans
//! `Spectra/spectra-mcp/docs/RENDER.md`, format `spectra.render/1`) et verse le DOM final par
//! `crawl::ingest`, donc par la même extraction, le même balisage « donnée non fiable » et le
//! même index que n'importe quelle page. Défense en profondeur : l'URL demandée ET l'URL finale
//! passent le garde réseau de Cortex (`guard::check_url`) en plus de la politique de Spectra
//! (qui revérifie chaque redirection côté Chrome).
//!
//! Le client est un trait (`Renderer`) : les tests de ce module le remplacent par un faux.

use serde_json::Value;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Une réponse JSON (XHR/fetch) capturée pendant le rendu.
#[derive(Debug, Clone, PartialEq)]
pub struct ReponseJson {
    pub url: String,
    pub status: u16,
    pub mime: String,
    pub body: String,
    pub tronquee: bool,
}

/// Résultat d'un rendu (`spectra.render/1`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rendu {
    pub url: String,
    pub url_finale: String,
    pub status: u16,
    pub titre: String,
    pub html: String,
    pub texte: String,
    pub reponses: Vec<ReponseJson>,
    /// Sous-ressources refusées par la politique réseau de Spectra : (url, raison).
    pub bloquees: Vec<(String, String)>,
    pub html_tronque: bool,
    pub ecoule_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ErreurRendu {
    /// Spectra est introuvable : message d'installation.
    Absent(String),
    /// Refusé par la politique réseau (code de sortie 3 de `capture`).
    Refuse(String),
    /// Délai dépassé, Chrome absent, JSON illisible…
    Echec(String),
}

impl std::fmt::Display for ErreurRendu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ErreurRendu::Absent(m) | ErreurRendu::Refuse(m) | ErreurRendu::Echec(m) => f.write_str(m),
        }
    }
}

pub trait Renderer {
    fn rendre(&self, url: &str, timeout_s: u64, reponses_json: bool) -> Result<Rendu, ErreurRendu>;
}

/// Analyse un JSON `spectra.render/1`.
pub fn analyser_rendu(json: &str) -> Result<Rendu, ErreurRendu> {
    let v: Value = serde_json::from_str(json).map_err(|e| ErreurRendu::Echec(format!("sortie de Spectra illisible : {e}")))?;
    if v.get("refused").and_then(|x| x.as_bool()) == Some(true) {
        return Err(ErreurRendu::Refuse(
            v.get("error").and_then(|x| x.as_str()).unwrap_or("refusé par la politique réseau de Spectra").to_string(),
        ));
    }
    if let Some(e) = v.get("error").and_then(|x| x.as_str()) {
        return Err(ErreurRendu::Echec(e.to_string()));
    }
    let schema = v.get("schema").and_then(|x| x.as_str()).unwrap_or("");
    if !schema.starts_with("spectra.render/") {
        return Err(ErreurRendu::Echec(format!("format inattendu « {schema} » (attendu spectra.render/1)")));
    }
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let reponses = v
        .get("responses")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .map(|r| ReponseJson {
                    url: r.get("url").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    status: r.get("status").and_then(|x| x.as_u64()).unwrap_or(0) as u16,
                    mime: r.get("mime").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    body: r.get("body").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    tronquee: r.get("truncated").and_then(|x| x.as_bool()).unwrap_or(false),
                })
                .collect()
        })
        .unwrap_or_default();
    let bloquees = v
        .get("blocked")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .map(|b| {
                    (
                        b.get("url").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                        b.get("reason").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Rendu {
        url: s("url"),
        url_finale: Some(s("final_url")).filter(|u| !u.is_empty()).unwrap_or_else(|| s("url")),
        status: v.get("status").and_then(|x| x.as_u64()).unwrap_or(0) as u16,
        titre: s("title"),
        html: s("html"),
        texte: s("text"),
        reponses,
        bloquees,
        html_tronque: v.get("truncated").and_then(|t| t.get("html")).and_then(|x| x.as_bool()).unwrap_or(false),
        ecoule_ms: v.get("elapsed_ms").and_then(|x| x.as_u64()).unwrap_or(0),
    })
}

/// L'exécutable de Spectra : `CORTEX_SPECTRA` (chemin), puis `spectra-server` dans le PATH,
/// puis `~/.cortex/bin/`.
pub fn trouver_spectra() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("CORTEX_SPECTRA").map(PathBuf::from).filter(|p| p.is_file()) {
        return Some(p);
    }
    let nom = format!("spectra-server{}", std::env::consts::EXE_SUFFIX);
    if let Some(path) = std::env::var_os("PATH") {
        if let Some(p) = std::env::split_paths(&path).map(|d| d.join(&nom)).find(|p| p.is_file()) {
            return Some(p);
        }
    }
    let p = crate::index::cortex_home().join("bin").join(&nom);
    p.is_file().then_some(p)
}

const AIDE_INSTALLATION: &str = "Spectra est introuvable. Installer spectra-server (dépôt Spectra : cargo build --release -p spectra-server), puis soit le mettre dans le PATH, soit poser CORTEX_SPECTRA=<chemin de spectra-server>. Sans lui : rendre la page avec l'outil browser_render de Spectra et la verser par `cortex web ingest <url> --source <s> --html <fichier>`.";

/// Client en sous-processus de `spectra-server capture`.
pub struct SpectraCli {
    pub exe: PathBuf,
}

impl SpectraCli {
    pub fn detecter() -> Result<SpectraCli, ErreurRendu> {
        trouver_spectra().map(|exe| SpectraCli { exe }).ok_or_else(|| ErreurRendu::Absent(AIDE_INSTALLATION.to_string()))
    }
}

impl Renderer for SpectraCli {
    fn rendre(&self, url: &str, timeout_s: u64, reponses_json: bool) -> Result<Rendu, ErreurRendu> {
        let out = std::env::temp_dir().join(format!("cortex-render-{}-{}.json", std::process::id(), Instant::now().elapsed().as_nanos()));
        let mut cmd = std::process::Command::new(&self.exe);
        cmd.arg("capture").arg(url).arg("--timeout").arg(timeout_s.to_string()).arg("--out").arg(&out);
        if reponses_json {
            cmd.arg("--json-responses");
        }
        cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000);
        }
        let mut child = cmd.spawn().map_err(|e| ErreurRendu::Echec(format!("lancement de Spectra impossible : {e}")))?;
        // Garde-fou : le délai de Spectra + 20 s de marge pour démarrer/fermer Chrome.
        let limite = Instant::now() + Duration::from_secs(timeout_s + 20);
        let statut = loop {
            match child.try_wait() {
                Ok(Some(s)) => break s,
                Ok(None) if Instant::now() > limite => {
                    let _ = child.kill();
                    let _ = std::fs::remove_file(&out);
                    return Err(ErreurRendu::Echec(format!("Spectra n'a pas répondu en {} s (processus arrêté)", timeout_s + 20)));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => return Err(ErreurRendu::Echec(format!("attente de Spectra : {e}"))),
            }
        };
        let json = std::fs::read_to_string(&out).unwrap_or_default();
        let _ = std::fs::remove_file(&out);
        match statut.code() {
            Some(0) => analyser_rendu(&json),
            Some(3) => Err(analyser_rendu(&json).err().unwrap_or(ErreurRendu::Refuse("refusé par la politique réseau de Spectra".into()))),
            c => {
                let mut err = String::new();
                if let Some(mut e) = child.stderr.take() {
                    use std::io::Read;
                    let _ = e.by_ref().take(2000).read_to_string(&mut err);
                }
                let detail = analyser_rendu(&json).err().map(|e| e.to_string()).unwrap_or_else(|| err.trim().chars().take(300).collect());
                Err(ErreurRendu::Echec(format!(
                    "Spectra a échoué (code {}) : {}",
                    c.map(|c| c.to_string()).unwrap_or_else(|| "signal".into()),
                    detail
                )))
            }
        }
    }
}

#[cfg(test)]
pub struct Faux(pub Result<Rendu, ErreurRendu>);

#[cfg(test)]
impl Renderer for Faux {
    fn rendre(&self, _url: &str, _t: u64, _j: bool) -> Result<Rendu, ErreurRendu> {
        self.0.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXEMPLE: &str = r#"{"schema":"spectra.render/1","url":"https://docs.example.com/guide","final_url":"https://docs.example.com/guide/","status":200,"title":"Guide","html":"<html><body><h1>Guide</h1></body></html>","text":"Guide","responses":[{"url":"https://docs.example.com/api/page.json","status":200,"mime":"application/json","body":"{\"a\":1}","truncated":false}],"documents":["https://docs.example.com/guide/"],"blocked":[{"url":"http://169.254.169.254/","reason":"réseau interne","ts":1}],"console_errors":0,"truncated":{"html":false,"text":false,"responses":false},"elapsed_ms":2310}"#;

    #[test]
    fn analyse_le_contrat_spectra() {
        let r = analyser_rendu(EXEMPLE).unwrap();
        assert_eq!(r.url_finale, "https://docs.example.com/guide/");
        assert_eq!(r.status, 200);
        assert!(r.html.contains("<h1>Guide</h1>"));
        assert_eq!(r.reponses.len(), 1);
        assert_eq!(r.reponses[0].mime, "application/json");
        assert_eq!(r.bloquees[0].1, "réseau interne");
        assert_eq!(r.ecoule_ms, 2310);
    }

    #[test]
    fn refus_et_erreurs() {
        let e = analyser_rendu(r#"{"error":"hôte interdit","refused":true}"#).unwrap_err();
        assert_eq!(e, ErreurRendu::Refuse("hôte interdit".into()));
        assert!(matches!(analyser_rendu(r#"{"error":"délai"}"#), Err(ErreurRendu::Echec(_))));
        assert!(matches!(analyser_rendu("pas du json"), Err(ErreurRendu::Echec(_))));
        assert!(matches!(analyser_rendu(r#"{"schema":"autre/1"}"#), Err(ErreurRendu::Echec(_))));
    }
}
