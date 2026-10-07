//! Recherche plein-texte & fichiers — remplace `grep` / `find` lents.
//!
//! La liste des fichiers est DÉJÀ connue par l'atlas (gitignore-aware, filtrée) :
//! `files` ne touche jamais le disque, `grep` ne lit que les fichiers indexés
//! (jamais `node_modules`), en parallèle (rayon), avec une recherche de
//! sous-chaîne SIMD (`memchr::memmem`) sur le fichier ENTIER — une seule passe
//! par fichier, les lignes ne sont découpées que là où il y a un résultat.
//!
//! Les deux fonctions prennent des listes de chemins (`FileSet`) plutôt qu'un
//! index matérialisé : l'appelant les lit directement dans l'atlas mmap-é
//! (`atlas::Handle::file_paths`), sans matérialiser l'index complet.
//!
//! `grep`  → chaîne littérale dans le CONTENU (messages, clés i18n, URLs, TODO…),
//!           groupée par fichier avec la FONCTION ENGLOBANTE (plages de symboles
//!           de l'atlas, renseignée par l'appelant).
//! `files` → fichier par nom/fragment de chemin ou glob (remplace `find -name`).

use rayon::prelude::*;
use std::path::Path;

/// Les fichiers d'UN projet : nom, racine, chemins relatifs (séparateurs '/').
pub struct FileSet<'a> {
    pub project: &'a str,
    pub root: &'a str,
    pub paths: Vec<&'a str>,
}

/// Un résultat de grep : fichier:ligne + le texte de la ligne + le symbole
/// englobant (rempli après coup par l'appelant, voir `atlas::Handle::enclosing_symbol`).
pub struct TextHit {
    /// Index du `FileSet` d'origine (pour retrouver le projet/l'atlas).
    pub set: usize,
    pub project: String,
    pub file: String,
    pub line: u32,
    pub text: String,
    pub symbol: Option<String>,
}

/// Recherche plein-texte (sous-chaîne littérale, insensible à la casse ASCII par
/// défaut) dans le CONTENU des fichiers. Multithread. Résultats triés (projet,
/// fichier, ligne), bornés à `max`. Une ligne qui contient plusieurs fois la
/// chaîne ne compte qu'une fois.
pub fn grep(sets: &[FileSet], needle: &str, case_sensitive: bool, max: usize) -> Vec<TextHit> {
    if needle.is_empty() {
        return Vec::new();
    }
    let needle_cmp: Vec<u8> = if case_sensitive { needle.as_bytes().to_vec() } else { needle.to_ascii_lowercase().into_bytes() };
    let finder = memchr::memmem::Finder::new(&needle_cmp);

    let jobs: Vec<(usize, &str, &str)> =
        sets.iter().enumerate().flat_map(|(si, s)| s.paths.iter().map(move |p| (si, s.root, *p))).collect();

    // Un tampon de lecture et un tampon « minuscules » par thread, réutilisés
    // d'un fichier à l'autre (pas d'allocation par fichier).
    let mut hits: Vec<TextHit> = jobs
        .par_iter()
        .map_init(
            || (Vec::new(), Vec::new()),
            |(buf, lower): &mut (Vec<u8>, Vec<u8>), &(si, root, path)| {
                use std::io::Read as _;
                let mut local: Vec<TextHit> = Vec::new();
                buf.clear();
                let Ok(mut f) = std::fs::File::open(Path::new(root).join(path)) else { return local };
                if f.read_to_end(buf).is_err() {
                    return local;
                }
                let bytes: &[u8] = buf;
                // Binaire grossier : octet nul → ignoré (comme avant).
                if memchr::memchr(0, bytes).is_some() {
                    return local;
                }
                let hay: &[u8] = if case_sensitive {
                    bytes
                } else {
                    lower.clear();
                    lower.extend(bytes.iter().map(|b| b.to_ascii_lowercase()));
                    lower
                };
                // Numérotation des lignes INCRÉMENTALE : on ne compte les '\n' que
                // entre deux résultats successifs (jamais tout le fichier par résultat).
                let (mut line_no, mut counted_to) = (1u32, 0usize);
                let mut last_line_start = usize::MAX;
                for pos in finder.find_iter(hay) {
                    line_no += memchr::memchr_iter(b'\n', &bytes[counted_to..pos]).count() as u32;
                    counted_to = pos;
                    let start = memchr::memrchr(b'\n', &bytes[..pos]).map(|i| i + 1).unwrap_or(0);
                    if start == last_line_start {
                        continue; // même ligne déjà rapportée
                    }
                    last_line_start = start;
                    let end = memchr::memchr(b'\n', &bytes[pos..]).map(|i| pos + i).unwrap_or(bytes.len());
                    let text = String::from_utf8_lossy(&bytes[start..end]);
                    local.push(TextHit {
                        set: si,
                        project: sets[si].project.to_string(),
                        file: path.to_string(),
                        line: line_no,
                        text: text.trim().chars().take(200).collect(),
                        symbol: None,
                    });
                }
                local
            },
        )
        .flatten()
        .collect();

    // Tri stable : projet, fichier puis ligne (lecture humaine, groupage par fichier).
    hits.sort_by(|a, b| a.set.cmp(&b.set).then_with(|| a.file.cmp(&b.file)).then(a.line.cmp(&b.line)));
    hits.truncate(max);
    hits
}

/// Formate les hits de grep en sortie compacte token-budgétée, GROUPÉE par
/// fichier, avec la fonction englobante de chaque ligne quand elle est connue.
pub fn format_grep(hits: &[TextHit], budget: usize, multi_project: bool, truncated: bool) -> String {
    // Groupé par fichier (`F:chemin`) puis par symbole englobant (identifiant
    // stable, recopiable dans card/read/impact), une ligne par occurrence.
    let mut out = String::new();
    let char_budget = budget * 4;
    let mut current: Option<(usize, &str)> = None;
    let mut current_sym: Option<&str> = None;
    let mut coupe = false;
    for h in hits {
        let mut block = String::new();
        if current != Some((h.set, h.file.as_str())) {
            let proj = if multi_project { format!("[{}] ", h.project) } else { String::new() };
            block.push_str(&format!(
                "{}F:{}
",
                proj, h.file
            ));
            current_sym = None;
        }
        let indent = if h.symbol.is_some() { "  " } else { " " };
        if h.symbol.as_deref() != current_sym {
            if let Some(s) = &h.symbol {
                block.push_str(&format!(
                    " {}
",
                    s
                ));
            }
        }
        block.push_str(&format!(
            "{}L{}: {}
",
            indent,
            h.line,
            h.text.trim()
        ));
        if out.len() + block.len() > char_budget {
            coupe = true;
            break;
        }
        current = Some((h.set, h.file.as_str()));
        current_sym = h.symbol.as_deref();
        out.push_str(&block);
    }
    if hits.is_empty() {
        out.push_str(
            "(no occurrence)
",
        );
    } else if coupe {
        out.push_str(
            "… budget -b reached; refine the search
",
        );
    } else if truncated {
        out.push_str(
            "… result limit (--max) reached; refine the search
",
        );
    }
    if let Some(s) = hits.iter().find_map(|h| h.symbol.as_deref()) {
        out.push_str(&format!(
            "next : read {}
",
            s
        ));
    }
    out
}

/// Trouve les fichiers par nom/fragment de chemin (remplace `find -name`).
/// Insensible à la casse. Instantané (chemins lus dans l'atlas, pas de scan disque).
///
/// Deux modes selon `pattern` :
/// - Pas de caractère glob (`*`/`?`) : sous-chaîne simple (ex. "usecompany"
///   trouve n'importe quel chemin qui contient ce fragment).
/// - Présence de `*`/`?`/`**` : vrai matcher glob (ex. "**/*.test.ts",
///   "src/hooks/*.ts").
///
/// Résultats triés par pertinence (nom de fichier exact > préfixe du nom >
/// ailleurs dans le chemin), puis par chemin — tri après coup plutôt qu'un
/// retour anticipé dès `max`, pour que les meilleurs matches remontent.
pub fn find_files(sets: &[FileSet], pattern: &str, max: usize) -> Vec<(String, String)> {
    let is_glob = pattern.contains('*') || pattern.contains('?');
    let p_lower = pattern.to_ascii_lowercase();

    let mut scored: Vec<(i32, &str, &str)> = Vec::new();
    for set in sets {
        for &path in &set.paths {
            let path_lower = path.to_ascii_lowercase();
            let matched = if is_glob { glob_match(&p_lower, &path_lower) } else { path_lower.contains(&p_lower) };
            if !matched {
                continue;
            }
            let filename = path_lower.rsplit('/').next().unwrap_or(&path_lower);
            let score = if filename == p_lower {
                3
            } else if !is_glob && filename.starts_with(&p_lower) {
                2
            } else if !is_glob && filename.contains(&p_lower) {
                1
            } else {
                0
            };
            scored.push((score, set.project, path));
        }
    }

    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.2.cmp(b.2)));
    scored.truncate(max);
    scored.into_iter().map(|(_, proj, path)| (proj.to_string(), path.to_string())).collect()
}

/// Matcher glob minimal : `*` (n'importe quoi sauf rien), `**` (n'importe quoi
/// y compris across `/`), `?` (un seul caractère). Pas de crate externe pour un
/// besoin aussi simple.
///
/// Implémentation récursive sur les octets : `**` consomme 0..N segments en
/// tentant chaque position possible pour le reste du pattern, ce qui reste
/// largement assez rapide vu la taille typique d'un chemin de fichier.
fn glob_match(pattern: &str, text: &str) -> bool {
    fn helper(p: &[u8], t: &[u8]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some(b'*') => {
                // "**" et "*" sont traités identiquement ici : les deux peuvent
                // traverser des "/".
                let mut rest = &p[1..];
                while rest.first() == Some(&b'*') {
                    rest = &rest[1..];
                }
                // "**/" en tête de pattern doit aussi matcher "aucun répertoire du
                // tout" (ex: "**/*.test.ts" doit matcher "useAuth.test.ts").
                if rest.first() == Some(&b'/') {
                    let after_slash = &rest[1..];
                    if helper(after_slash, t) {
                        return true;
                    }
                }
                if rest.is_empty() {
                    return true; // un `*`/`**` final matche tout ce qui reste.
                }
                for i in 0..=t.len() {
                    if helper(rest, &t[i..]) {
                        return true;
                    }
                }
                false
            }
            Some(b'?') => !t.is_empty() && helper(&p[1..], &t[1..]),
            Some(&c) => t.first() == Some(&c) && helper(&p[1..], &t[1..]),
        }
    }
    helper(pattern.as_bytes(), text.as_bytes())
}

#[cfg(test)]
mod glob_tests {
    use super::*;

    #[test]
    fn simple_star() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(!glob_match("*.rs", "main.ts"));
    }

    #[test]
    fn double_star_crosses_slash() {
        assert!(glob_match("**/*.test.ts", "src/hooks/useAuth.test.ts"));
        assert!(glob_match("**/*.test.ts", "useAuth.test.ts"));
    }

    #[test]
    fn question_mark_single_char() {
        assert!(glob_match("file?.rs", "file1.rs"));
        assert!(!glob_match("file?.rs", "file12.rs"));
    }

    #[test]
    fn exact_match_no_wildcard() {
        assert!(glob_match("main.rs", "main.rs"));
        assert!(!glob_match("main.rs", "mainx.rs"));
    }

    /// grep : insensible à la casse, une ligne = un résultat même si la chaîne
    /// y apparaît deux fois, numéros de ligne exacts.
    #[test]
    fn grep_lines_and_case() {
        let dir = std::env::temp_dir().join(format!("cortex-test-grep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.ts"), "un\nFoo foo\ntrois\nfoo\n").unwrap();
        let root = dir.to_string_lossy().to_string();
        let sets = vec![FileSet { project: "p", root: &root, paths: vec!["a.ts"] }];
        let hits = grep(&sets, "foo", false, 10);
        assert_eq!(hits.iter().map(|h| h.line).collect::<Vec<_>>(), vec![2, 4]);
        assert_eq!(hits[0].text, "Foo foo");
        let hits = grep(&sets, "Foo", true, 10);
        assert_eq!(hits.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }
}
