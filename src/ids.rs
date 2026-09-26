//! Identifiants stables et lisibles (architecture v2 §3) — ce qu'un agent
//! recopie tel quel d'une sortie de Cortex dans l'appel suivant :
//!
//! - `S:<chemin>#<symbole>` : un symbole de code. Si le fichier contient
//!   plusieurs symboles de MÊME nom exact, le premier (ordre du fichier) garde
//!   `#nom`, les suivants reçoivent `#nom~2`, `#nom~3`… : l'identifiant d'un
//!   symbole ne dépend que des homonymes qui le PRÉCÈDENT dans son fichier.
//! - `D:<chemin>#<ancre>` : une section de doc (titre markdown), ancre = titre
//!   en minuscules, accents repliés, non-alphanumériques → `-` ; `~k` idem.
//! - `F:<chemin>` : un fichier.
//!
//! En entrée, les outils acceptent aussi : un chemin nu (`src/x.ts`), un
//! suffixe de chemin unique (`useUploadQueue.ts`), `chemin:ligne` ou
//! `chemin:début-fin` (symbole englobant, ou plage pour `read`), un nom de
//! symbole, et un identifiant suivi de sa provenance (`S:…#f L12-40`,
//! `S:…#f:12`) ou de ponctuation recopiée par erreur (`,`, `` ` ``).

use crate::symbol::{fold_accents, SymbolKind};

/// Ancre d'un titre markdown.
pub fn slug(title: &str) -> String {
    let folded = fold_accents(title).to_lowercase();
    let mut out = String::with_capacity(folded.len());
    for c in folded.chars() {
        if c.is_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Clé d'homonymie d'un symbole dans son fichier : le nom exact, ou l'ancre
/// pour un titre.
fn key(name: &str, kind: SymbolKind) -> String {
    if kind == SymbolKind::Heading {
        slug(name)
    } else {
        name.to_string()
    }
}

/// Rang (1 = premier) de chaque symbole parmi ses homonymes du même fichier,
/// symboles donnés dans l'ordre du fichier.
pub fn ranks<'a>(syms: impl Iterator<Item = (&'a str, SymbolKind)>) -> Vec<u32> {
    let mut seen: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    syms.map(|(n, k)| {
        let c = seen.entry(key(n, k)).or_insert(0);
        *c += 1;
        *c
    })
    .collect()
}

/// Identifiant d'un symbole (`S:` ou `D:` pour un titre).
pub fn sym_id(path: &str, name: &str, kind: SymbolKind, rank: u32) -> String {
    let (p, frag) = if kind == SymbolKind::Heading { ("D", slug(name)) } else { ("S", name.to_string()) };
    if rank > 1 {
        format!("{}:{}#{}~{}", p, path, frag, rank)
    } else {
        format!("{}:{}#{}", p, path, frag)
    }
}

pub fn file_id(path: &str) -> String {
    format!("F:{}", path)
}

/// Ce que désigne une entrée d'outil.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// `S:chemin#nom[~k]` ou `D:chemin#ancre[~k]` (fragment sans `~`, rang).
    Sym { path: String, frag: String, rank: u32, doc: bool },
    /// Un fichier (`F:chemin`, chemin nu ou suffixe de chemin).
    File(String),
    /// `chemin:ligne` ou `chemin:début-fin`.
    Lines { path: String, start: u32, end: u32 },
    /// Un nom de symbole.
    Name(String),
}

/// Retire la provenance et la ponctuation recopiées avec un identifiant.
fn clean(s: &str) -> &str {
    let mut s = s.trim().trim_matches(|c| c == '`' || c == '"' || c == '\'' || c == ',' || c == ';' || c == '(' || c == ')');
    // « S:…#f L12-40 » ou « … (fn) » : on garde le premier mot.
    if let Some(i) = s.find(char::is_whitespace) {
        if s.starts_with("S:") || s.starts_with("D:") || s.starts_with("F:") || s.contains('/') {
            s = &s[..i];
        }
    }
    s.trim_end_matches([',', ';', '.', ')'])
}

/// `…:12` ou `…:12-40` en fin de chaîne → (reste, début, fin).
fn split_lines(s: &str) -> Option<(&str, u32, u32)> {
    let (head, tail) = s.rsplit_once(':')?;
    if head.is_empty() || head.ends_with(':') {
        return None;
    }
    let (a, b) = match tail.split_once('-') {
        Some((a, b)) => (a.parse::<u32>().ok()?, b.parse::<u32>().ok()?),
        None => {
            let a = tail.parse::<u32>().ok()?;
            (a, a)
        }
    };
    Some((head, a.min(b).max(1), a.max(b).max(1)))
}

fn split_rank(frag: &str) -> (String, u32) {
    if let Some((f, k)) = frag.rsplit_once('~') {
        if let Ok(k) = k.parse::<u32>() {
            return (f.to_string(), k.max(1));
        }
    }
    (frag.to_string(), 1)
}

fn looks_like_path(s: &str) -> bool {
    s.contains('/') || s.contains('\\') || crate::lang::Lang::from_path(s) != crate::lang::Lang::Other
}

pub fn parse(input: &str) -> Target {
    let s = clean(input);
    for (prefix, doc) in [("S:", false), ("D:", true)] {
        if let Some(rest) = s.strip_prefix(prefix) {
            if let Some((path, frag)) = rest.rsplit_once('#') {
                // `#f:12` : provenance collée au fragment.
                let frag = match split_lines(frag) {
                    Some((f, _, _)) => f,
                    None => frag,
                };
                let (frag, rank) = split_rank(frag);
                return Target::Sym { path: path.replace('\\', "/"), frag, rank, doc };
            }
            return Target::File(rest.replace('\\', "/"));
        }
    }
    let s = s.strip_prefix("F:").unwrap_or(s);
    if let Some((path, a, b)) = split_lines(s) {
        if looks_like_path(path) {
            return Target::Lines { path: path.replace('\\', "/"), start: a, end: b };
        }
    }
    if looks_like_path(s) {
        return Target::File(s.trim_start_matches("./").replace('\\', "/"));
    }
    Target::Name(s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_et_rangs() {
        let r = ranks(
            [
                ("run", SymbolKind::Method),
                ("Run", SymbolKind::Class),
                ("run", SymbolKind::Method),
                ("Titre", SymbolKind::Heading),
                ("titre !", SymbolKind::Heading),
            ]
            .into_iter(),
        );
        assert_eq!(r, vec![1, 1, 2, 1, 2]);
        assert_eq!(sym_id("src/a.ts", "run", SymbolKind::Method, 2), "S:src/a.ts#run~2");
        assert_eq!(sym_id("src/a.ts", "run", SymbolKind::Method, 1), "S:src/a.ts#run");
        assert_eq!(sym_id("docs/x.md", "Mise à jour (v2)", SymbolKind::Heading, 1), "D:docs/x.md#mise-a-jour-v2");
        assert_eq!(file_id("src/a.ts"), "F:src/a.ts");
    }

    #[test]
    fn analyse_des_entrees() {
        let sym = |p: &str, f: &str, r: u32| Target::Sym { path: p.into(), frag: f.into(), rank: r, doc: false };
        assert_eq!(parse("S:src/a.ts#run~2"), sym("src/a.ts", "run", 2));
        assert_eq!(parse("`S:src/a.ts#run`,"), sym("src/a.ts", "run", 1));
        assert_eq!(parse("S:src/a.ts#run L12-40"), sym("src/a.ts", "run", 1));
        assert_eq!(parse("S:src/a.ts#run:12"), sym("src/a.ts", "run", 1));
        assert_eq!(
            parse("D:docs/x.md#mise-a-jour"),
            Target::Sym { path: "docs/x.md".into(), frag: "mise-a-jour".into(), rank: 1, doc: true }
        );
        assert_eq!(parse("F:src/a.ts"), Target::File("src/a.ts".into()));
        assert_eq!(parse("src\\a.ts"), Target::File("src/a.ts".into()));
        assert_eq!(parse("useUploadQueue.ts"), Target::File("useUploadQueue.ts".into()));
        assert_eq!(parse("src/a.ts:12"), Target::Lines { path: "src/a.ts".into(), start: 12, end: 12 });
        assert_eq!(parse("F:src/a.ts:40-12"), Target::Lines { path: "src/a.ts".into(), start: 12, end: 40 });
        assert_eq!(parse("safeInternalPath"), Target::Name("safeInternalPath".into()));
    }
}
