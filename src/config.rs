//! Settings on/off — focus projet, scoping.
//!
//! ~/.cortex/config.json : liste des projets désactivés. Une recherche sans
//! --project ne scanne que les projets ACTIFS (vitesse + focus). Permet de se
//! concentrer sur un projet sans le bruit des autres.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    /// Projets désactivés (exclus de la recherche cross-projets).
    #[serde(default)]
    pub disabled: HashSet<String>,
    /// Docs scrapées désactivées.
    #[serde(default)]
    pub disabled_docs: HashSet<String>,
}

fn config_path() -> PathBuf {
    crate::index::cortex_home().join("config.json")
}

impl Config {
    pub fn load() -> Config {
        match std::fs::read_to_string(config_path()) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => Config::default(),
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, json)
    }

    pub fn is_enabled(&self, project: &str) -> bool {
        !self.disabled.contains(project)
    }

    pub fn enable(&mut self, project: &str) {
        self.disabled.remove(project);
    }

    pub fn disable(&mut self, project: &str) {
        self.disabled.insert(project.to_string());
    }
}
