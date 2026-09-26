//! Snapshot de l'architecture des serveurs — fait entrer l'infra dans la base de connaissance.
//!
//! `cortex infra --project <P> --env <.env>` lit les coordonnées des serveurs dans un
//! fichier .env, se connecte en SSH **LECTURE SEULE** et génère un markdown par
//! serveur dans `~/.cortex/infra/<nom>.md`. Si un serveur est injoignable, on retombe
//! sur un snapshot STATIQUE (ce que le .env documente).
//!
//! Convention du .env : chaque préfixe `<P>` qui porte une clé `<P>_IPV4` (ou
//! `<P>_HOST`) décrit un serveur ; clés optionnelles `<P>_SSH_PORT` (22),
//! `<P>_LOGIN` (root), `<P>_SSH_KEY_PATH`, `<P>_LABEL` (rôle en une ligne).
//! Exemple : `WEB_HOST=203.0.113.10`, `WEB_LABEL=Web / reverse proxy`.
//!
//! SÉCURITÉ : aucun secret n'est écrit dans le markdown. Les creds restent dans le
//! .env ; le doc référence "voir .env". Les commandes SSH sont toutes en lecture
//! seule (jamais de mutation). Stocké hors de tout repo (~/.cortex/infra/).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Un VPS à inspecter (déduit du .env).
struct VpsTarget {
    name: String,  // ex "web_monprojet" (préfixe en minuscules + projet)
    label: String, // ex "Web / reverse proxy"
    host: String,
    port: String,
    user: String,
    key_path: String,
}

pub fn infra_home() -> PathBuf {
    crate::index::cortex_home().join("infra")
}

/// Parse un fichier .env en map clé→valeur (gère les guillemets).
fn parse_env(path: &Path) -> HashMap<String, String> {
    let mut m = HashMap::new();
    if let Ok(content) = std::fs::read_to_string(path) {
        for line in content.lines() {
            let l = line.trim();
            if l.is_empty() || l.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = l.split_once('=') {
                let v = v.trim().trim_matches('"').trim_matches('\'');
                m.insert(k.trim().to_string(), v.to_string());
            }
        }
    }
    m
}

/// Construit la liste des serveurs à partir du .env : un serveur par préfixe `<P>`
/// qui porte `<P>_IPV4` ou `<P>_HOST` (ordre alphabétique des préfixes).
fn targets_from_env(env: &HashMap<String, String>, project: &str) -> Vec<VpsTarget> {
    let g = |k: &str| env.get(k).cloned().unwrap_or_default();
    let mut prefixes: Vec<String> = env
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .filter_map(|(k, _)| k.strip_suffix("_IPV4").or_else(|| k.strip_suffix("_HOST")))
        .filter(|p| !p.is_empty())
        .map(|p| p.to_string())
        .collect();
    prefixes.sort();
    prefixes.dedup();
    prefixes
        .into_iter()
        .map(|p| {
            let host = if g(&format!("{p}_IPV4")).is_empty() { g(&format!("{p}_HOST")) } else { g(&format!("{p}_IPV4")) };
            let port = g(&format!("{p}_SSH_PORT"));
            let user = g(&format!("{p}_LOGIN"));
            let label = g(&format!("{p}_LABEL"));
            VpsTarget {
                name: format!("{}_{}", p.to_ascii_lowercase(), project.to_ascii_lowercase()),
                label: if label.is_empty() { p.clone() } else { format!("{p} — {label}") },
                host,
                port: if port.is_empty() { "22".into() } else { port },
                user: if user.is_empty() { "root".into() } else { user },
                key_path: g(&format!("{p}_SSH_KEY_PATH")),
            }
        })
        .collect()
}

/// Commandes SSH LECTURE SEULE exécutées sur chaque VPS (ssection → commande).
const READONLY_PROBES: &[(&str, &str)] = &[
    ("OS & uptime", "uname -a; uptime; cat /etc/os-release 2>/dev/null | head -2"),
    ("CPU & RAM", "nproc; free -h | head -2; df -h / 2>/dev/null | tail -1"),
    (
        "Services systemd actifs",
        "systemctl list-units --type=service --state=running --no-pager --no-legend 2>/dev/null | awk '{print $1}' | head -40",
    ),
    ("Ports en écoute", "ss -tlnp 2>/dev/null | awk 'NR>1{print $4}' | sort -u | head -40"),
    ("Conteneurs Docker", "docker ps --format '{{.Names}} ({{.Image}}) {{.Status}}' 2>/dev/null | head -40"),
    ("Reverse proxy", "ls /etc/nginx/sites-enabled/ 2>/dev/null; ls /etc/caddy/ 2>/dev/null; caddy version 2>/dev/null"),
    ("PostgreSQL", "sudo -u postgres psql -tAc 'SELECT version();' 2>/dev/null | head -1"),
    (
        "Bases & tables (compte)",
        "sudo -u postgres psql -tAc \"SELECT datname FROM pg_database WHERE datistemplate=false;\" 2>/dev/null | head -10",
    ),
];

/// Génère le snapshot pour un projet. `env_path` = chemin du .env. SSH live si
/// possible, sinon fallback statique. Retourne le nombre de VPS traités.
pub fn snapshot(project: &str, env_path: &Path) -> std::io::Result<usize> {
    let env = parse_env(env_path);
    let targets = targets_from_env(&env, project);
    if targets.is_empty() {
        eprintln!("cortex infra: aucun serveur (<P>_IPV4 ou <P>_HOST) dans {}", env_path.display());
        return Ok(0);
    }
    std::fs::create_dir_all(infra_home())?;
    let mut done = 0;
    for t in &targets {
        eprintln!("  → snapshot {} ({}:{})…", t.name, t.host, t.port);
        let md = snapshot_one(t, env_path);
        let path = infra_home().join(format!("{}.md", t.name));
        std::fs::write(&path, md)?;
        eprintln!("    écrit → {}", path.display());
        done += 1;
    }
    Ok(done)
}

/// Snapshot d'UN serveur : tente SSH live, sinon fallback statique.
fn snapshot_one(t: &VpsTarget, env_path: &Path) -> String {
    let mut md = String::new();
    md.push_str(&format!("<!-- cortex-infra · {} -->\n# {} ({})\n\n", t.name, t.label, t.host));
    md.push_str(&format!(
        "> Hôte : `{}:{}` · user `{}` · clé SSH : voir `{}` (jamais de secret ici).\n\n",
        t.host,
        t.port,
        t.user,
        env_path.display()
    ));

    // Tente une connexion SSH live (lecture seule).
    let live = ssh_probe(t);
    if let Some(sections) = live {
        md.push_str("_Source : snapshot SSH live (lecture seule)._\n");
        for (title, output) in sections {
            md.push_str(&format!("\n## {}\n```\n{}\n```\n", title, output.trim()));
        }
    } else {
        md.push_str("_Source : statique (serveur injoignable — topologie d'après le .env)._\n");
        md.push_str(&fallback_static(t, env_path));
    }
    md
}

/// Exécute les probes en lecture seule via `ssh`. None si la connexion échoue.
fn ssh_probe(t: &VpsTarget) -> Option<Vec<(String, String)>> {
    // Vérifie d'abord que la clé existe.
    if t.key_path.is_empty() || !Path::new(&t.key_path).exists() {
        return None;
    }
    // Test de connectivité rapide (timeout court).
    let test = run_ssh(t, "echo ok", 8);
    if test.as_deref().map(|s| s.trim()) != Some("ok") {
        return None;
    }
    let mut sections = Vec::new();
    for (title, cmd) in READONLY_PROBES {
        let out = run_ssh(t, cmd, 15).unwrap_or_else(|| "(indisponible)".into());
        sections.push((title.to_string(), out));
    }
    Some(sections)
}

/// Lance une commande SSH en lecture seule (BatchMode, timeout). Retourne stdout.
fn run_ssh(t: &VpsTarget, remote_cmd: &str, timeout_s: u32) -> Option<String> {
    let out = Command::new("ssh")
        .args([
            "-i",
            &t.key_path,
            "-p",
            &t.port,
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=accept-new",
            &format!("-o ConnectTimeout={}", timeout_s),
            &format!("{}@{}", t.user, t.host),
            remote_cmd,
        ])
        .output()
        .ok()?;
    if out.stdout.is_empty() && !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Snapshot statique d'après le .env (topologie connue, sans connexion). Seuls
/// l'hôte, le port, l'utilisateur et le rôle sont repris ; jamais une autre clé.
fn fallback_static(t: &VpsTarget, env_path: &Path) -> String {
    let mut s = String::new();
    s.push_str("\n## Topologie connue (.env)\n```\n");
    s.push_str(&format!("Rôle      : {}\n", t.label));
    s.push_str(&format!("Hôte      : {}:{} (user {})\n", t.host, t.port, t.user));
    s.push_str(&format!("Creds     : voir {} (jamais copiés ici)\n", env_path.display()));
    s.push_str("```\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serveurs_par_prefixe() {
        let env: HashMap<String, String> = [
            ("WEB_HOST", "203.0.113.10"),
            ("WEB_LABEL", "Web"),
            ("DB_IPV4", "203.0.113.20"),
            ("DB_SSH_PORT", "2222"),
            ("DB_PASSWORD", "ne-doit-jamais-sortir"),
            ("EMPTY_HOST", ""),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let t = targets_from_env(&env, "Demo");
        assert_eq!(t.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), vec!["db_demo", "web_demo"]);
        assert_eq!((t[0].port.as_str(), t[0].user.as_str()), ("2222", "root"));
        assert_eq!(t[1].label, "WEB — Web");
        let md = fallback_static(&t[0], Path::new(".env"));
        assert!(!md.contains("ne-doit-jamais-sortir"));
    }
}
