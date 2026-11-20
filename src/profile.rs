//! Profil de projet : un fichier OPTIONNEL `.cortex/profile.toml` à la racine du projet indexé,
//! relu à chaque requête (jamais de réindexation) et sans aucun effet s'il est absent.
//!
//! Il apprend à `find` / `ask` le vocabulaire et la géographie du projet :
//!
//! ```toml
//! [synonyms]                       # mots que CE projet emploie l'un pour l'autre
//! facture = ["invoice", "billing"] # une ligne = un groupe (clé + valeurs)
//!
//! [ignore]
//! terms = ["application", "app"]  # mots de question trop génériques ici : ignorés
//!
//! [boost]                          # dossiers/fichiers à favoriser (multiplicateur > 1)
//! "src/core/" = 1.25
//!
//! [penalty]                        # dossiers/fichiers à reléguer (multiplicateur < 1)
//! "legacy/" = 0.5
//! "vendor/" = 0.3
//!
//! [entry]                          # fichiers d'entrée, légèrement favorisés
//! files = ["src/main.rs"]
//! boost = 1.15
//! ```
//!
//! Un motif de chemin s'applique au chemin relatif normalisé (`/`) : préfixe, ou segment
//! contenu (`legacy/` vaut aussi pour `src/legacy/x.ts`). Pas de globs. Le sous-ensemble de TOML
//! lu ici est volontairement minuscule (sections, `clé = "texte" | nombre | ["liste"]`) : pas de
//! dépendance de plus.

use crate::symbol::fold_accents;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

/// Poids d'un synonyme DÉCLARÉ par le projet (un mot saisi pèse 1, le glossaire général 0,55).
/// Surchargeable par `CORTEX_PROFILE_SYN` (mesures).
pub fn synonym_weight() -> f32 {
    static W: OnceLock<f32> = OnceLock::new();
    *W.get_or_init(|| std::env::var("CORTEX_PROFILE_SYN").ok().and_then(|v| v.parse().ok()).unwrap_or(0.85))
}
/// Multiplicateur par défaut d'un fichier d'entrée.
pub const DEFAULT_ENTRY_BOOST: f32 = 1.05;
/// Garde-fous : un profil ne peut pas écraser le classement.
const MULT_MIN: f32 = 0.1;
const MULT_MAX: f32 = 2.0;

#[derive(Debug, Default, Clone)]
pub struct Profile {
    /// Groupes de mots équivalents (repliés, minuscules).
    pub synonyms: Vec<Vec<String>>,
    /// Radicaux des groupes ci-dessus (calculés à la lecture : la requête ne racinise rien).
    syn_stems: Vec<Vec<String>>,
    /// `"/" + motif` de chaque règle de chemin (recherche « segment contenu »).
    mid_patterns: Vec<String>,
    /// Mots de question ignorés.
    pub ignore: Vec<String>,
    /// (motif de chemin, multiplicateur) — boosts puis pénalités, dans l'ordre du fichier.
    pub path_rules: Vec<(String, f32)>,
    /// Fichiers d'entrée (chemins relatifs normalisés, minuscules).
    pub entries: Vec<String>,
    pub entry_boost: f32,
    /// Les synonymes ne s'appliquent qu'aux mots de question RARES dans le projet (défaut) ;
    /// `always = true` dans [synonyms] les applique à tous les mots.
    pub synonyms_gated: bool,
    /// Messages non bloquants (clé inconnue, valeur hors bornes…), affichés par `profile check`.
    pub warnings: Vec<String>,
}

impl Profile {
    pub fn is_empty(&self) -> bool {
        self.synonyms.is_empty() && self.ignore.is_empty() && self.path_rules.is_empty() && self.entries.is_empty()
    }

    /// Mots équivalents déclarés pour `term` (sans `term` lui-même), comparés par radical :
    /// `factures` retrouve le groupe de `facture`.
    pub fn synonyms_of(&self, term: &str) -> Vec<&str> {
        let st = crate::stem::stem(term);
        let mut out: Vec<&str> = Vec::new();
        for (g, stems) in self.synonyms.iter().zip(&self.syn_stems) {
            if stems.contains(&st) {
                for (w, ws) in g.iter().zip(stems) {
                    if *ws != st && !out.contains(&w.as_str()) {
                        out.push(w.as_str());
                    }
                }
            }
        }
        out
    }

    pub fn ignores(&self, term: &str) -> bool {
        self.ignore.iter().any(|w| w == term)
    }

    /// Multiplicateur du chemin `rel` (relatif, séparateurs quelconques).
    pub fn path_multiplier(&self, rel: &str) -> f32 {
        if self.path_rules.is_empty() && self.entries.is_empty() {
            return 1.0;
        }
        let p = rel.replace('\\', "/").to_ascii_lowercase();
        let mut m = 1.0f32;
        for ((pat, mult), mid) in self.path_rules.iter().zip(&self.mid_patterns) {
            if p.starts_with(pat.as_str()) || p.contains(mid.as_str()) {
                m *= mult;
            }
        }
        if self.entries.contains(&p) {
            m *= self.entry_boost;
        }
        m
    }

    pub fn has_path_rules(&self) -> bool {
        !self.path_rules.is_empty() || !self.entries.is_empty()
    }
}

// ───────────────────────────── lecture ─────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Val {
    Str(String),
    Num(f32),
    List(Vec<String>),
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\''))) {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// Retire un commentaire `#` en fin de ligne hors guillemets.
fn strip_comment(line: &str) -> &str {
    let mut in_q: Option<char> = None;
    for (i, c) in line.char_indices() {
        match (in_q, c) {
            (None, '"') | (None, '\'') => in_q = Some(c),
            (Some(q), c2) if c2 == q => in_q = None,
            (None, '#') => return &line[..i],
            _ => {}
        }
    }
    line
}

fn parse_list(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_q: Option<char> = None;
    for c in body.chars() {
        match (in_q, c) {
            (None, '"') | (None, '\'') => in_q = Some(c),
            (Some(q), c2) if c2 == q => {
                in_q = None;
                out.push(std::mem::take(&mut cur));
            }
            (Some(_), c2) => cur.push(c2),
            (None, ',') | (None, ' ') | (None, '\t') => {}
            (None, c2) => cur.push(c2), // valeur sans guillemets
        }
        if in_q.is_none() && c == ',' && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

pub(crate) fn parse_kv(text: &str) -> Vec<(String, String, Val)> {
    // (section, clé, valeur)
    let mut out = Vec::new();
    let mut section = String::new();
    let mut lines = text.lines().peekable();
    while let Some(raw) = lines.next() {
        let line = strip_comment(raw).trim().to_string();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') && !line.contains('=') {
            section = line[1..line.len() - 1].trim().to_ascii_lowercase();
            continue;
        }
        let Some(eq) = find_eq(&line) else { continue };
        let key = unquote(&line[..eq]);
        let mut val = line[eq + 1..].trim().to_string();
        if val.starts_with('[') {
            // Liste éventuellement sur plusieurs lignes.
            while !val.contains(']') {
                match lines.next() {
                    Some(n) => {
                        val.push(' ');
                        val.push_str(strip_comment(n).trim());
                    }
                    None => break,
                }
            }
            let end = val.rfind(']').unwrap_or(val.len());
            out.push((section.clone(), key, Val::List(parse_list(&val[1..end]))));
        } else if (val.starts_with('"') || val.starts_with('\'')) && val.len() >= 2 {
            out.push((section.clone(), key, Val::Str(unquote(&val))));
        } else if let Ok(n) = val.parse::<f32>() {
            out.push((section.clone(), key, Val::Num(n)));
        } else {
            out.push((section.clone(), key, Val::Str(val)));
        }
    }
    out
}

/// Position du `=` séparateur hors guillemets.
fn find_eq(line: &str) -> Option<usize> {
    let mut in_q: Option<char> = None;
    for (i, c) in line.char_indices() {
        match (in_q, c) {
            (None, '"') | (None, '\'') => in_q = Some(c),
            (Some(q), c2) if c2 == q => in_q = None,
            (None, '=') => return Some(i),
            _ => {}
        }
    }
    None
}

fn norm_word(s: &str) -> Vec<String> {
    fold_accents(s).to_ascii_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).map(|t| t.to_string()).collect()
}

fn norm_path(s: &str) -> String {
    let p = s.trim().replace('\\', "/").to_ascii_lowercase();
    p.trim_start_matches("./").to_string()
}

/// Analyse le texte d'un profil. Ne renvoie jamais d'erreur : les lignes incomprises deviennent
/// des avertissements (un profil abîmé ne doit pas casser `find`).
pub fn parse(text: &str) -> Profile {
    let mut p = Profile { entry_boost: DEFAULT_ENTRY_BOOST, synonyms_gated: true, ..Default::default() };
    for (section, key, val) in parse_kv(text) {
        match (section.as_str(), &val) {
            ("synonyms", Val::List(items)) => {
                let mut g: Vec<String> = norm_word(&key);
                for it in items {
                    for w in norm_word(it) {
                        if !g.contains(&w) {
                            g.push(w);
                        }
                    }
                }
                if g.len() >= 2 {
                    p.synonyms.push(g);
                } else {
                    p.warnings.push(format!("[synonyms] {key}: groupe vide"));
                }
            }
            ("synonyms", Val::Str(s)) if key == "always" => p.synonyms_gated = !matches!(s.as_str(), "true" | "yes" | "1"),
            ("synonyms", Val::Str(s)) => {
                let mut g = norm_word(&key);
                g.extend(norm_word(s));
                g.dedup();
                if g.len() >= 2 {
                    p.synonyms.push(g);
                }
            }
            ("ignore", Val::List(items)) if key == "terms" || key == "words" => {
                for it in items {
                    p.ignore.extend(norm_word(it));
                }
            }
            ("boost", Val::Num(n)) | ("penalty", Val::Num(n)) => {
                let (lo, hi) = (MULT_MIN, MULT_MAX);
                let m = n.clamp(lo, hi);
                if m != *n {
                    p.warnings.push(format!("[{section}] \"{key}\": {n} ramené à {m} (bornes {lo}..{hi})"));
                }
                if section == "boost" && m < 1.0 {
                    p.warnings.push(format!("[boost] \"{key}\" = {m} : inférieur à 1, c'est une pénalité"));
                }
                if section == "penalty" && m > 1.0 {
                    p.warnings.push(format!("[penalty] \"{key}\" = {m} : supérieur à 1, c'est un boost"));
                }
                p.path_rules.push((norm_path(&key), m));
            }
            ("entry", Val::List(items)) if key == "files" => {
                p.entries.extend(items.iter().map(|s| norm_path(s)));
            }
            ("entry", Val::Num(n)) if key == "boost" => p.entry_boost = n.clamp(1.0, MULT_MAX),
            _ => p.warnings.push(format!("[{section}] {key}: ligne ignorée (section ou clé inconnue, ou valeur de mauvais type)")),
        }
    }
    p.syn_stems = p.synonyms.iter().map(|g| g.iter().map(|w| crate::stem::stem(w)).collect()).collect();
    p.mid_patterns = p.path_rules.iter().map(|(pat, _)| format!("/{pat}")).collect();
    p
}

// ───────────────────────────── fichier + cache ─────────────────────────────

pub const PROFILE_REL: &str = ".cortex/profile.toml";

pub fn profile_path(root: &str) -> PathBuf {
    Path::new(root).join(PROFILE_REL)
}

type CacheMap = HashMap<String, (std::time::Instant, Option<(SystemTime, u64)>, Option<Arc<Profile>>)>;

/// Un profil modifié est vu au plus tard après ce délai (un `stat` par seconde et par racine).
const RECHECK: std::time::Duration = std::time::Duration::from_millis(500);

fn cache() -> &'static Mutex<CacheMap> {
    static C: OnceLock<Mutex<CacheMap>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Profil de la racine `root` (None si absent ou vide). Relu seulement quand (mtime, taille)
/// change : un `stat` par requête.
pub fn for_root(root: &str) -> Option<Arc<Profile>> {
    if std::env::var_os("CORTEX_NO_PROFILE").is_some() {
        return None;
    }
    let mut c = cache().lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, _, prof)) = c.get(root) {
        if at.elapsed() < RECHECK {
            return prof.clone();
        }
    }
    let path = profile_path(root);
    let stamp = std::fs::metadata(&path).ok().and_then(|m| m.modified().ok().map(|t| (t, m.len())));
    if let Some((_, old, prof)) = c.get(root) {
        if *old == stamp {
            let prof = prof.clone();
            c.insert(root.to_string(), (std::time::Instant::now(), stamp, prof.clone()));
            return prof;
        }
    }
    let prof = stamp.and_then(|_| std::fs::read_to_string(&path).ok()).map(|t| parse(&t)).filter(|p| !p.is_empty()).map(Arc::new);
    c.insert(root.to_string(), (std::time::Instant::now(), stamp, prof.clone()));
    prof
}

// ───────────────────────────── `profile init` ─────────────────────────────

/// Dossiers qu'on propose de reléguer (bruit probable pour une question de fonctionnalité).
const NOISE_DIRS: &[&str] = &[
    "vendor",
    "third_party",
    "thirdparty",
    "third-party",
    "legacy",
    "deprecated",
    "old",
    "generated",
    "gen",
    "dist",
    "build",
    "fixtures",
    "mocks",
    "snapshots",
    "examples",
    "example",
    "samples",
    "benchmarks",
    "bench",
    "benches",
    "docs_src",
];
/// Noms de fichiers d'entrée usuels.
const ENTRY_NAMES: &[&str] = &[
    "main.rs",
    "lib.rs",
    "main.py",
    "app.py",
    "__main__.py",
    "index.ts",
    "index.js",
    "main.ts",
    "main.tsx",
    "app.tsx",
    "app.ts",
    "server.ts",
    "main.go",
    "cli.py",
    "Program.cs",
];
const SKIP_DIRS: &[&str] =
    &["node_modules", "target", ".git", ".cortex", "__pycache__", ".venv", "venv", ".next", "coverage", ".idea", ".vscode"];

/// Squelette de profil construit à partir de la racine : dossiers de premier niveau, README,
/// dossiers « bruit » et fichiers d'entrée détectés. Aucune question de banc n'est lue.
pub fn skeleton(root: &Path, project: &str) -> String {
    let mut top_dirs: Vec<String> = Vec::new();
    let mut sub_dirs: Vec<String> = Vec::new();
    let mut noise: Vec<String> = Vec::new();
    let mut entries: Vec<String> = Vec::new();
    let mut readme = String::new();
    if let Ok(rd) = std::fs::read_dir(root) {
        let mut names: Vec<(String, bool)> =
            rd.flatten().map(|e| (e.file_name().to_string_lossy().to_string(), e.path().is_dir())).collect();
        names.sort();
        for (n, is_dir) in names {
            let low = n.to_ascii_lowercase();
            if is_dir {
                if SKIP_DIRS.contains(&n.as_str()) || n.starts_with('.') {
                    continue;
                }
                top_dirs.push(n.clone());
                if NOISE_DIRS.contains(&low.as_str()) {
                    noise.push(format!("{n}/"));
                }
                if let Ok(rd2) = std::fs::read_dir(root.join(&n)) {
                    let mut subs: Vec<String> = rd2
                        .flatten()
                        .filter(|e| e.path().is_dir())
                        .map(|e| e.file_name().to_string_lossy().to_string())
                        .filter(|s| !s.starts_with('.') && !SKIP_DIRS.contains(&s.as_str()))
                        .collect();
                    subs.sort();
                    for s in subs.into_iter().take(12) {
                        if NOISE_DIRS.contains(&s.to_ascii_lowercase().as_str()) {
                            noise.push(format!("{n}/{s}/"));
                        } else {
                            sub_dirs.push(format!("{n}/{s}/"));
                        }
                    }
                }
            } else {
                if ENTRY_NAMES.iter().any(|e| e.eq_ignore_ascii_case(&n)) {
                    entries.push(n.clone());
                }
                if readme.is_empty() && low.starts_with("readme") {
                    readme = std::fs::read_to_string(root.join(&n)).unwrap_or_default();
                }
            }
        }
        // Entrées usuelles un niveau plus bas (src/main.rs, src/index.ts, <pkg>/__main__.py…).
        for d in top_dirs.iter().take(12) {
            if let Ok(rd) = std::fs::read_dir(root.join(d)) {
                for e in rd.flatten() {
                    let n = e.file_name().to_string_lossy().to_string();
                    if !e.path().is_dir() && ENTRY_NAMES.iter().any(|x| x.eq_ignore_ascii_case(&n)) {
                        entries.push(format!("{d}/{n}"));
                    }
                }
            }
        }
    }
    // Titres du README : candidats de vocabulaire propre au projet.
    let headings: Vec<String> = readme
        .lines()
        .filter(|l| l.starts_with('#'))
        .map(|l| l.trim_start_matches('#').trim().to_string())
        .filter(|l| !l.is_empty() && l.len() < 60)
        .take(12)
        .collect();

    let mut s = String::new();
    s.push_str(&format!("# Cortex profile for {project} — optional, read on every query (no re-index).\n"));
    s.push_str("# Delete a line to remove a rule; delete the file to go back to the default engine.\n");
    s.push_str("# Check it with: cortex profile check\n\n");
    s.push_str("[synonyms]\n# Words THIS project uses for the same thing (one group per line). Example:\n");
    s.push_str("# facture = [\"invoice\", \"billing\"]\n# salarie = [\"employee\", \"staff\"]\n");
    if !headings.is_empty() {
        s.push_str("# README headings, for inspiration: ");
        s.push_str(&headings.join(" | "));
        s.push('\n');
    }
    s.push_str("\n[ignore]\n# Question words too generic here (they match everywhere). Careful: the project name often\n");
    s.push_str("# helps (it appears in the right paths); ignoring it was measured harmful on 2 of 3 public repositories.\n");
    s.push_str("# terms = [\"application\"]\n");
    s.push_str("\n[boost]\n# Folders that hold the product code (multiplier > 1).\n");
    if sub_dirs.is_empty() {
        for d in top_dirs.iter().filter(|d| !NOISE_DIRS.contains(&d.to_ascii_lowercase().as_str())).take(6) {
            s.push_str(&format!("# \"{d}/\" = 1.2\n"));
        }
    } else {
        for d in sub_dirs.iter().take(6) {
            s.push_str(&format!("# \"{d}\" = 1.2\n"));
        }
    }
    s.push_str("\n[penalty]\n# Folders that rarely answer a feature question (multiplier < 1).\n");
    if noise.is_empty() {
        s.push_str("# \"vendor/\" = 0.4\n# \"legacy/\" = 0.5\n");
    } else {
        noise.sort();
        noise.dedup();
        for d in noise.iter().take(8) {
            s.push_str(&format!("\"{d}\" = 0.6\n"));
        }
    }
    s.push_str("\n[entry]\n# Entry points: slightly favoured when they match.\n");
    entries.sort();
    entries.dedup();
    if entries.is_empty() {
        s.push_str("# files = [\"src/main.rs\"]\n");
    } else {
        // Commented: entry files are big hubs that already rank high; the boost is a tie-breaker
        // (x1.05) and was measured neutral to harmful when applied blindly.
        let list: Vec<String> = entries.iter().take(6).map(|e| format!("\"{e}\"")).collect();
        s.push_str(&format!("# files = [{}]\n", list.join(", ")));
    }
    s
}

/// Rapport de `profile check`.
pub fn report(p: &Profile) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "synonym groups: {} · ignored terms: {} · path rules: {} · entry files: {}\n",
        p.synonyms.len(),
        p.ignore.len(),
        p.path_rules.len(),
        p.entries.len()
    ));
    for g in &p.synonyms {
        s.push_str(&format!("  synonyms  {}\n", g.join(" = ")));
    }
    if !p.ignore.is_empty() {
        s.push_str(&format!("  ignore    {}\n", p.ignore.join(", ")));
    }
    for (pat, m) in &p.path_rules {
        s.push_str(&format!("  path      {pat}  x{m}\n"));
    }
    for e in &p.entries {
        s.push_str(&format!("  entry     {e}  x{}\n", p.entry_boost));
    }
    for w in &p.warnings {
        s.push_str(&format!("  warning   {w}\n"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
# comment
[synonyms]
facture = ["invoice", "billing"]   # inline comment
Salarié = ["employee"]
solo = []

[ignore]
terms = ["app",
         "application"]

[boost]
"src/core/" = 1.25

[penalty]
"legacy/" = 0.5
"vendor/" = 9   # hors bornes

[entry]
files = ["./src/main.rs"]
boost = 1.3
"#;

    #[test]
    fn parses_all_sections() {
        let p = parse(SAMPLE);
        assert_eq!(p.synonyms.len(), 2);
        assert_eq!(p.synonyms_of("facture"), vec!["invoice", "billing"]);
        assert_eq!(p.synonyms_of("invoice"), vec!["facture", "billing"]);
        assert!(p.synonyms_of("salarie").contains(&"employee"));
        assert!(p.ignores("app") && p.ignores("application") && !p.ignores("page"));
        assert_eq!(p.entry_boost, 1.3);
        assert!(p.warnings.iter().any(|w| w.contains("vendor")), "{:?}", p.warnings);
        assert!(p.warnings.iter().any(|w| w.contains("solo")));
    }

    #[test]
    fn path_multiplier_prefix_segment_entry() {
        let p = parse(SAMPLE);
        assert_eq!(p.path_multiplier("src/core/engine.rs"), 1.25);
        assert_eq!(p.path_multiplier("legacy/old.rs"), 0.5);
        assert_eq!(p.path_multiplier("src\\legacy\\old.rs"), 0.5);
        assert_eq!(p.path_multiplier("src/other.rs"), 1.0);
        assert_eq!(p.path_multiplier("src/main.rs"), 1.3);
        assert_eq!(p.path_multiplier("vendor/x.rs"), MULT_MAX);
    }

    #[test]
    fn broken_file_never_panics() {
        let p = parse("[[[ = =\n\"unterminated = [\nfoo\n[synonyms]\na = [\"b\"");
        assert!(p.synonyms.len() <= 1);
    }

    #[test]
    fn empty_profile_is_empty() {
        assert!(parse("# nothing\n").is_empty());
    }

    #[test]
    fn skeleton_is_valid_and_has_no_active_synonyms() {
        let dir = std::env::temp_dir().join(format!("cortex-profile-{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("src/legacy"));
        let _ = std::fs::create_dir_all(dir.join("vendor"));
        let _ = std::fs::write(dir.join("src/main.rs"), "fn main() {}");
        let _ = std::fs::write(dir.join("README.md"), "# Demo\n## Billing engine\n");
        let sk = skeleton(&dir, "Demo");
        let p = parse(&sk);
        assert!(p.synonyms.is_empty());
        assert!(p.path_rules.iter().any(|(k, m)| k == "vendor/" && *m < 1.0));
        assert!(p.path_rules.iter().any(|(k, _)| k == "src/legacy/"));
        assert!(sk.contains("\"src/main.rs\""));
        assert!(p.entries.is_empty() && p.ignore.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
