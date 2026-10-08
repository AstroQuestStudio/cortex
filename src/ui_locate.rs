//! `cortex ui` — OÙ EST CETTE INTERFACE ?
//!
//! On part de ce qu'on voit à l'écran (le texte d'un bouton, un `data-testid`, un
//! `aria-label`, un `id`, le nom d'un composant React lu dans le navigateur) et on
//! remonte au code qui l'affiche. C'est ce que `grep` ne fait pas : il rend aussi
//! les commentaires, la doc, les tests, et s'arrête à la clé de traduction.
//!
//! Signaux, du plus fort au plus faible :
//! - `data-testid` / `data-test` / `data-cy` identique : 100 ;
//! - nom de composant (la fibre React dit « SubmitButton › FormFooter ») : 95, 85… ;
//! - `aria-label`, `title`, `placeholder` identiques : 85 ;
//! - clé i18n : le texte est trouvé dans un fichier de traduction, on remonte la clé,
//!   puis on cherche qui l'utilise (`t.cle`, `t('cle')`) : 80 ;
//! - texte littéral dans un fichier de code (hors commentaires, tests, docs) : 75 ;
//! - `id` identique : 70.
//!
//! Plusieurs signaux qui désignent le même symbole s'additionnent : trouver le
//! composant ET son texte vaut mieux que l'un des deux.

use crate::atlas;
use crate::search::is_test_path;
use crate::textsearch::{self, FileSet, TextHit};
use std::collections::HashMap;

#[derive(Debug, Default, Clone)]
pub struct UiQuery {
    pub text: String,
    pub testid: Vec<String>,
    pub aria: Vec<String>,
    pub id: Vec<String>,
    pub component: Vec<String>,
}

impl UiQuery {
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.testid.is_empty() && self.aria.is_empty() && self.id.is_empty() && self.component.is_empty()
    }
}

const CODE_EXT: &[&str] =
    &["ts", "tsx", "js", "jsx", "mjs", "cjs", "vue", "svelte", "html", "rs", "py", "php", "kt", "swift", "java", "cs", "dart"];

fn ext(p: &str) -> &str {
    p.rsplit('.').next().unwrap_or("")
}

pub fn is_i18n_path(p: &str) -> bool {
    let l = p.to_ascii_lowercase().replace('\\', "/");
    l.contains("i18n") || l.contains("locale") || l.contains("translation") || l.contains("/lang/") || l.contains("/langs/")
}

fn is_code_path(p: &str) -> bool {
    let l = p.replace('\\', "/");
    CODE_EXT.contains(&ext(&l))
        && !is_test_path(&l)
        && !l.starts_with("docs/")
        && !l.contains("node_modules/")
        && !l.contains("/dist/")
        && !l.starts_with("dist/")
}

/// Bibliothèque de composants de base (shadcn : `components/ui/…`).
pub fn is_ui_primitive(p: &str) -> bool {
    let l = p.replace('\\', "/").to_ascii_lowercase();
    l.contains("/components/ui/") || l.starts_with("components/ui/")
}

fn is_archived(p: &str) -> bool {
    let l = p.replace('\\', "/");
    l.starts_with("_disabled/") || l.contains("/_disabled/") || l.contains("/archive")
}

fn is_comment_line(t: &str) -> bool {
    let t = t.trim_start();
    t.starts_with("//")
        || t.starts_with("/*")
        || t.starts_with('*')
        || t.starts_with("<!--")
        || (t.starts_with('#') && !t.starts_with("#["))
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// La chaîne `needle` apparaît dans `line` en mot entier (pas collée à un identifiant).
pub fn whole_word(line: &str, needle: &str) -> bool {
    let mut from = 0;
    while let Some(i) = line[from..].find(needle) {
        let s = from + i;
        let e = s + needle.len();
        let before_ok = line[..s].chars().next_back().map_or(true, |c| !is_ident(c));
        let after_ok = line[e..].chars().next().map_or(true, |c| !is_ident(c));
        if before_ok && after_ok {
            return true;
        }
        from = e;
    }
    false
}

/// Clé d'une ligne de traduction : ce qui précède `texte` (`key: 'texte'`, `"a.b": "texte"`).
pub fn i18n_key(line: &str, text: &str) -> Option<String> {
    let idx = line.to_lowercase().find(&text.to_lowercase())?;
    let before = line[..idx].trim_end_matches(|c: char| c.is_whitespace() || "'\"`".contains(c));
    let before = before.trim_end_matches(|c: char| c == ':' || c == '=' || c.is_whitespace());
    let before = before.trim_end_matches(|c: char| "'\"`".contains(c));
    let key: String =
        before.chars().rev().take_while(|c| is_ident(*c) || *c == '.' || *c == '-').collect::<Vec<_>>().into_iter().rev().collect();
    (key.len() >= 2 && key.chars().any(|c| c.is_alphabetic())).then_some(key)
}

/// Texte de recherche : début utile du libellé affiché (sans « … », coupé à un mot).
pub fn clean_text(s: &str) -> String {
    let t: String = s.replace('…', " ").replace('\n', " ");
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() <= 48 {
        return t;
    }
    let cut: String = t.chars().take(48).collect();
    match cut.rfind(' ') {
        Some(i) if i > 12 => cut[..i].to_string(),
        _ => cut,
    }
}

/// La ligne contient `attr="v"` (ou `attr={'v'}`, `attr={\`v\`}`…), avec `attr` en mot entier.
pub fn line_has_attr(line: &str, attrs: &[&str], v: &str) -> bool {
    attrs.iter().any(|a| {
        let mut from = 0;
        while let Some(i) = line[from..].find(a) {
            let s0 = from + i;
            let after = &line[s0 + a.len()..];
            let before_ok = line[..s0].chars().next_back().map_or(true, |c| !is_ident(c) && c != '-');
            let rest = after.trim_start_matches(|c: char| c == '=' || c == '{' || c == '"' || c == '\'' || c == '`' || c == ' ');
            if before_ok && after.starts_with(['=', ' ']) && rest.starts_with(v) {
                return true;
            }
            from = s0 + a.len();
        }
        false
    })
}

/// La ligne DÉFINIT le composant `c` (fonction, constante, classe, export par défaut).
pub fn defines_component(line: &str, c: &str) -> bool {
    [format!("function {c}"), format!("const {c} "), format!("const {c}:"), format!("class {c}"), format!("export default {c}")]
        .iter()
        .any(|p| line.contains(p.as_str()))
        && whole_word(line, c)
}

#[derive(Default)]
struct Cand {
    shown: String,
    path: String,
    line: u32,
    score: i32,
    why: Vec<String>,
}

pub fn run_ui_on(handles: &[atlas::Handle], q: &UiQuery, max: usize) -> String {
    if q.is_empty() {
        return "(nothing to look for: give a text, --testid, --aria, --id or --component)\n".to_string();
    }
    let sets: Vec<FileSet> = crate::file_sets(handles);
    let multi = handles.len() > 1;
    let mut cands: HashMap<(usize, String), Cand> = HashMap::new();

    let mut add = |h: &TextHit, score: i32, why: String| {
        let sym = handles[h.set].enclosing_symbol(&h.file, h.line);
        let key = (h.set, sym.clone().unwrap_or_else(|| format!("F:{}", h.file)));
        let shown = sym.unwrap_or_else(|| format!("F:{}", h.file));
        let penalty = if is_archived(&h.file) { -50 } else { 0 };
        let c = cands.entry(key).or_insert_with(|| Cand { shown, path: h.file.clone(), line: h.line, ..Default::default() });
        if !c.why.contains(&why) {
            c.score += score + penalty;
            c.why.push(why);
        }
        if h.line < c.line || c.line == 0 {
            c.line = h.line;
        }
        let _ = multi;
    };

    let grep = |needle: &str, ci: bool, max: usize| -> Vec<TextHit> { textsearch::grep(&sets, needle, !ci, max) };

    // Une seule recherche par valeur (la plus sélective), puis filtre en mémoire :
    // dix recherches par attribut et par guillemet coûtaient des secondes.
    for v in &q.testid {
        for h in grep(v, true, 300) {
            if is_code_path(&h.file)
                && !is_comment_line(&h.text)
                && line_has_attr(&h.text, &["data-testid", "data-test", "data-cy", "data-qa", "testId"], v)
            {
                add(&h, 100, format!("data-testid « {v} »"));
            }
        }
    }
    for v in &q.aria {
        for h in grep(v, true, 300) {
            if !is_code_path(&h.file) || is_comment_line(&h.text) {
                continue;
            }
            for attr in ["aria-label", "title", "placeholder"] {
                if line_has_attr(&h.text, &[attr], v) {
                    add(&h, 85, format!("{attr} « {v} »"));
                }
            }
        }
    }
    for v in &q.id {
        for h in grep(v, true, 300) {
            if is_code_path(&h.file) && !is_comment_line(&h.text) && line_has_attr(&h.text, &["id"], v) {
                add(&h, 70, format!("id « {v} »"));
            }
        }
    }
    for (rank, c) in q.component.iter().enumerate() {
        let c = c.trim();
        if c.len() < 3 {
            continue;
        }
        let score = (95 - 10 * rank as i32).max(55);
        // le mot nu (« Button ») a des milliers d'occurrences : on cherche la DÉFINITION
        for pat in [format!("function {c}"), format!("const {c} "), format!("const {c}:"), format!("class {c}")] {
            for h in grep(&pat, true, 40) {
                if is_code_path(&h.file) && !is_comment_line(&h.text) && defines_component(&h.text, c) {
                    // une primitive de la bibliothèque d'UI (ui/button.tsx) n'est pas « l'endroit où
                    // ce bouton est affiché » : on la garde, mais loin derrière les vrais écrans
                    let prim = if is_ui_primitive(&h.file) { -60 } else { 0 };
                    add(&h, score + prim, format!("composant {c}"));
                }
            }
        }
    }
    // 5. texte affiché : littéral dans le code, ou clé i18n
    let text = clean_text(&q.text);
    if text.chars().count() >= 3 {
        let mut hits = grep(&text, false, 160);
        if hits.is_empty() && text.chars().count() > 26 {
            let short: String = text.chars().take(24).collect();
            hits = grep(short.trim_end(), false, 160);
        }
        let mut keys: Vec<(String, String)> = Vec::new();
        for h in &hits {
            if is_comment_line(&h.text) {
                continue;
            }
            if is_i18n_path(&h.file) && !is_test_path(&h.file) && CODE_EXT.contains(&ext(&h.file)) {
                if let Some(k) = i18n_key(&h.text, &text) {
                    keys.push((k, h.file.clone()));
                }
                continue;
            }
            if is_code_path(&h.file) && !h.text.trim_start().starts_with("import ") {
                let bonus = if matches!(ext(&h.file), "tsx" | "jsx" | "vue" | "svelte" | "html") { 5 } else { 0 };
                add(h, 75 + bonus, format!("texte « {} »", text.chars().take(30).collect::<String>()));
            }
        }
        keys.sort();
        keys.dedup();
        for (key, from) in keys.into_iter().take(4) {
            let last = key.rsplit('.').next().unwrap_or(&key).to_string();
            let mut used = 0;
            for h in grep(&last, true, 400) {
                if used >= 24 {
                    break;
                }
                let dotted = h.text.contains(&format!(".{last}"));
                let quoted = h.text.contains(&format!("'{key}'")) || h.text.contains(&format!("\"{key}\""));
                if (dotted || quoted)
                    && is_code_path(&h.file)
                    && h.file != from
                    && !is_i18n_path(&h.file)
                    && !is_comment_line(&h.text)
                    && whole_word(&h.text, &last)
                {
                    used += 1;
                    add(&h, 80, format!("clé i18n « {key} »"));
                }
            }
        }
    }

    let mut v: Vec<Cand> = cands.into_values().collect();
    v.sort_by(|a, b| b.score.cmp(&a.score).then(a.path.cmp(&b.path)).then(a.line.cmp(&b.line)));
    v.truncate(max.max(1));
    if v.is_empty() {
        return "(no UI match: not found as a code literal, i18n key, test id, aria-label or component)\n".to_string();
    }
    let mut out = String::new();
    for c in &v {
        out.push_str(&format!("{}  {}:{}  ← {}\n", c.shown, c.path, c.line, c.why.join(" ; ")));
    }
    out.push_str(&format!("next : cortex card {}\n", v[0].shown));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cle_i18n_extraite() {
        assert_eq!(i18n_key("    login: 'Se connecter',", "Se connecter").as_deref(), Some("login"));
        assert_eq!(i18n_key(r#"  "nav.login": "Se connecter","#, "se connecter").as_deref(), Some("nav.login"));
        assert_eq!(i18n_key("  submit = `Valider`", "Valider").as_deref(), Some("submit"));
        assert!(i18n_key("'Valider'", "Valider").is_none());
    }

    #[test]
    fn mot_entier() {
        assert!(whole_word("<Button className=x>{t.login}</Button>", "login"));
        assert!(!whole_word("const loginForm = 1", "login"));
        assert!(whole_word("t('login')", "login"));
    }

    #[test]
    fn texte_nettoye() {
        assert_eq!(clean_text("  Se   connecter…"), "Se connecter");
        let long = "Un très long texte de paragraphe qui dépasse largement la limite de recherche utile";
        assert!(clean_text(long).chars().count() <= 48);
        assert!(!clean_text(long).ends_with(' '));
    }

    #[test]
    fn attributs_et_composants() {
        assert!(line_has_attr(r#"<button data-testid="submit-btn" onClick={x}>"#, &["data-testid"], "submit-btn"));
        assert!(line_has_attr("<input aria-label={'Email'} />", &["aria-label"], "Email"));
        assert!(!line_has_attr(r#"<div data-testid="submit-btn-2">"#, &["data-testid"], "other"));
        assert!(!line_has_attr(r#"<div my-id="x">"#, &["id"], "x"));
        assert!(line_has_attr(r#"<div id="x">"#, &["id"], "x"));
        assert!(defines_component("export default function SubmitButton() {", "SubmitButton"));
        assert!(defines_component("const Footer = () => <p/>", "Footer"));
        assert!(!defines_component("const FooterLinks = () => null", "Footer"));
    }

    #[test]
    fn chemins() {
        assert!(is_code_path("src/pages/Login.tsx"));
        assert!(!is_code_path("src/pages/__tests__/Login.test.tsx"));
        assert!(!is_code_path("docs/guide.md"));
        assert!(!is_code_path("CHANGELOG.md"));
        assert!(is_i18n_path("src/i18n/translations.ts"));
        assert!(is_archived("_disabled/code-mort/src/x.ts"));
        assert!(is_ui_primitive("src/components/ui/button.tsx"));
        assert!(!is_ui_primitive("src/components/cloud-os/Button.tsx"));
        assert!(is_comment_line("   // Se connecter"));
        assert!(!is_comment_line("  <span>Se connecter</span>"));
    }
}
