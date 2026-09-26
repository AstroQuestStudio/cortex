//! Converter HTML → "AI-Markdown" — format documentaire dense, optimisé pour une IA.
//!
//! Objectif : la doc sert à l'IA, pas à un humain. Donc on vire TOUT le bruit
//! (nav, footer, aside, scripts, bandeaux cookies, "edit this page", widgets) et
//! on garde uniquement la substance, structurée de façon prévisible :
//!   - titres (tous niveaux), paragraphes, listes, code (avec langage), tables,
//!     blockquotes, definition lists.
//!   - chaque nœud émis UNE SEULE fois (pas de redondance li>pre dupliquée).
//!   - texte normalisé (espaces compactés), liens réduits à leur texte.
//!
//! C'est un vrai walk top-down qui DESCEND dans le contenu et SAUTE réellement les
//! sous-arbres-bruit (contrairement à un traverse() plat qui ne peut pas élaguer).

use scraper::ElementRef;

/// Tags dont on saute INTÉGRALEMENT le sous-arbre (bruit non documentaire).
const SKIP_SUBTREE: &[&str] =
    &["nav", "header", "footer", "aside", "script", "style", "noscript", "form", "button", "svg", "iframe", "template", "dialog"];

/// Classes/ids (sous-chaînes) qui trahissent un bloc-bruit même sur un tag neutre.
const SKIP_CLASS_HINTS: &[&str] = &[
    "sidebar",
    "navbar",
    "navigation",
    "menu",
    "toc",
    "breadcrumb",
    "footer",
    "header",
    "cookie",
    "banner",
    "advert",
    "promo",
    "newsletter",
    "social",
    "edit-this-page",
    "edit-page",
    "pagination",
    "pager",
    "skip-link",
    "announcement",
];

/// Convertit le contenu principal (déjà sélectionné) en markdown dense.
pub fn to_ai_markdown(root: ElementRef) -> String {
    let mut out = String::new();
    walk(root, &mut out);
    normalize_blank_lines(&out)
}

/// Mesure le « vrai contenu » d'un markdown : nombre de mots HORS lignes de titre
/// et hors clôtures de code. Sert à rejeter les pages « titres sans corps »
/// (### Titre / ### Titre / … mais aucun paragraphe, aucun exemple).
pub fn substance_words(md: &str) -> usize {
    let mut words = 0;
    let mut in_code = false;
    for line in md.lines() {
        let t = line.trim();
        if t.starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            words += t.split_whitespace().count(); // le code COMPTE comme substance
            continue;
        }
        if t.starts_with('#') {
            continue; // ligne de titre = pas du contenu
        }
        words += t.split_whitespace().count();
    }
    words
}

/// Walk récursif top-down : on contrôle la descente (pour élaguer les sous-arbres).
fn walk(el: ElementRef, out: &mut String) {
    let tag = el.value().name();

    // Élagage : sous-arbre entièrement sauté.
    if SKIP_SUBTREE.contains(&tag) || is_noise_block(el) {
        return;
    }

    match tag {
        "h1" => heading(el, 1, out),
        "h2" => heading(el, 2, out),
        "h3" => heading(el, 3, out),
        "h4" => heading(el, 4, out),
        "h5" => heading(el, 5, out),
        "h6" => heading(el, 6, out),
        "p" => {
            let t = inline_text(el);
            if !t.is_empty() {
                out.push('\n');
                out.push_str(&t);
                out.push('\n');
            }
        }
        "pre" => {
            // Bloc de code : langage déduit de la classe (language-xxx / lang-xxx).
            let lang = code_lang(el);
            let code = el.text().collect::<String>();
            let code = code.trim_end_matches('\n');
            if !code.trim().is_empty() {
                out.push_str(&format!("\n```{}\n{}\n```\n", lang, code));
            }
        }
        // Bloc de code SANS <pre> (sites modernes : <div class="code-block">,
        // <code> multi-lignes, syntax highlighters custom). On le capte comme code
        // si le contenu est multi-lignes OU la classe trahit du code.
        "code" | "div" | "figure" if is_code_block(el) => {
            let lang = code_lang(el);
            let code = el.text().collect::<String>();
            let code = code.trim_matches('\n');
            if !code.trim().is_empty() {
                out.push_str(&format!("\n```{}\n{}\n```\n", lang, code));
            }
        }
        "ul" | "ol" => {
            out.push('\n');
            list(el, tag == "ol", 0, out);
            out.push('\n');
        }
        "table" => {
            out.push('\n');
            table(el, out);
            out.push('\n');
        }
        "blockquote" => {
            let t = inline_text(el);
            if !t.is_empty() {
                out.push('\n');
                for line in t.lines() {
                    out.push_str("> ");
                    out.push_str(line);
                    out.push('\n');
                }
            }
        }
        "dl" => {
            out.push('\n');
            for child in el.child_elements() {
                match child.value().name() {
                    "dt" => out.push_str(&format!("**{}**\n", inline_text(child))),
                    "dd" => out.push_str(&format!(": {}\n", inline_text(child))),
                    _ => {}
                }
            }
        }
        // Conteneurs neutres : on descend (sans rien émettre nous-mêmes).
        _ => {
            for child in el.child_elements() {
                walk(child, out);
            }
        }
    }
}

fn heading(el: ElementRef, level: usize, out: &mut String) {
    let t = inline_text(el);
    if !t.is_empty() {
        out.push('\n');
        out.push_str(&"#".repeat(level));
        out.push(' ');
        out.push_str(&t);
        out.push('\n');
    }
}

/// Liste imbriquée : gère ul/ol et les sous-listes (indentation).
fn list(el: ElementRef, ordered: bool, depth: usize, out: &mut String) {
    let indent = "  ".repeat(depth);
    let mut n = 1;
    for li in el.child_elements().filter(|c| c.value().name() == "li") {
        let marker = if ordered { format!("{}.", n) } else { "-".to_string() };
        // Texte direct du li (hors sous-listes).
        let own = li_own_text(li);
        if !own.is_empty() {
            out.push_str(&format!("{}{} {}\n", indent, marker, own));
        }
        // Sous-listes éventuelles.
        for sub in li.child_elements().filter(|c| matches!(c.value().name(), "ul" | "ol")) {
            list(sub, sub.value().name() == "ol", depth + 1, out);
        }
        n += 1;
    }
}

/// Table → markdown pipe table (header = 1re ligne ou <th>).
fn table(el: ElementRef, out: &mut String) {
    let mut rows: Vec<Vec<String>> = Vec::new();
    for node in el.descendants() {
        let tr = match ElementRef::wrap(node) {
            Some(e) if e.value().name() == "tr" => e,
            _ => continue,
        };
        let cells: Vec<String> =
            tr.child_elements().filter(|c| matches!(c.value().name(), "td" | "th")).map(|c| inline_text(c).replace('|', "\\|")).collect();
        if !cells.is_empty() {
            rows.push(cells);
        }
    }
    if rows.is_empty() {
        return;
    }
    let ncol = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    for (i, row) in rows.iter().enumerate() {
        let mut padded = row.clone();
        while padded.len() < ncol {
            padded.push(String::new());
        }
        out.push_str(&format!("| {} |\n", padded.join(" | ")));
        if i == 0 {
            out.push_str(&format!("|{}|\n", " --- |".repeat(ncol)));
        }
    }
}

/// Texte inline d'un élément : agrège le texte, compacte les espaces.
fn inline_text(el: ElementRef) -> String {
    let raw: String = el.text().collect::<Vec<_>>().join(" ");
    compact_ws(&raw)
}

/// Texte propre d'un <li> SANS le contenu de ses sous-listes (évite la duplication).
fn li_own_text(li: ElementRef) -> String {
    let mut s = String::new();
    for node in li.children() {
        if let Some(child) = ElementRef::wrap(node) {
            if matches!(child.value().name(), "ul" | "ol") {
                continue; // sous-liste traitée séparément
            }
            s.push_str(&child.text().collect::<String>());
            s.push(' ');
        } else if let Some(t) = node.value().as_text() {
            s.push_str(t);
        }
    }
    compact_ws(&s)
}

/// Détecte un bloc de code SANS balise <pre> (syntax highlighters modernes,
/// composants custom). Évite de re-capturer du code déjà dans un <pre> (le walk
/// ne descend pas dans un <pre>, donc pas de double comptage ici).
fn is_code_block(el: ElementRef) -> bool {
    let v = el.value();
    // Si un <pre> est à l'intérieur, ce n'est pas LUI le bloc (le <pre> sera traité).
    if el.child_elements().any(|c| c.value().name() == "pre") {
        return false;
    }
    let mut hay = String::new();
    if let Some(c) = v.attr("class") {
        hay.push_str(c);
        hay.push(' ');
    }
    if let Some(i) = v.attr("id") {
        hay.push_str(i);
    }
    let hay = hay.to_ascii_lowercase();
    const CODE_HINTS: &[&str] = &[
        "code-block",
        "codeblock",
        "code_block",
        "highlight",
        "hljs",
        "prism",
        "language-",
        "shiki",
        "codehilite",
        "sourcecode",
        "code-snippet",
        "snippet",
    ];
    let class_says_code = CODE_HINTS.iter().any(|h| hay.contains(h));
    // <code> multi-lignes (≥ 2 lignes) = bloc, pas inline.
    let text = el.text().collect::<String>();
    let multiline = text.matches('\n').count() >= 1 && text.trim().len() > 30;
    let tag = v.name();
    (tag == "code" && multiline) || (class_says_code && multiline)
}

/// Langage d'un bloc de code, déduit de la classe CSS (language-rust, lang-ts…).
fn code_lang(pre: ElementRef) -> String {
    // Cherche sur le <pre> ou un <code> enfant.
    let mut classes = String::new();
    if let Some(c) = pre.value().attr("class") {
        classes.push_str(c);
    }
    for code in pre.child_elements().filter(|c| c.value().name() == "code") {
        if let Some(c) = code.value().attr("class") {
            classes.push(' ');
            classes.push_str(c);
        }
    }
    for tok in classes.split_whitespace() {
        for prefix in ["language-", "lang-", "highlight-source-"] {
            if let Some(l) = tok.strip_prefix(prefix) {
                return l.to_string();
            }
        }
    }
    String::new()
}

/// Vrai si un élément neutre est en réalité un bloc-bruit (via class/id/role).
fn is_noise_block(el: ElementRef) -> bool {
    let v = el.value();
    if v.attr("role").map(|r| matches!(r, "navigation" | "banner" | "complementary" | "contentinfo")).unwrap_or(false) {
        return true;
    }
    let mut hay = String::new();
    if let Some(c) = v.attr("class") {
        hay.push_str(c);
        hay.push(' ');
    }
    if let Some(i) = v.attr("id") {
        hay.push_str(i);
    }
    let hay = hay.to_ascii_lowercase();
    if hay.is_empty() {
        return false;
    }
    SKIP_CLASS_HINTS.iter().any(|h| hay.contains(h))
}

/// Compacte les espaces/sauts internes en simples espaces, trim.
fn compact_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = true; // trim left
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Compacte les lignes vides multiples en une seule.
fn normalize_blank_lines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank = 0;
    for line in s.lines() {
        let t = line.trim_end();
        if t.is_empty() {
            blank += 1;
            if blank <= 1 {
                out.push('\n');
            }
        } else {
            blank = 0;
            out.push_str(t);
            out.push('\n');
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use scraper::{Html, Selector};

    fn convert(html: &str) -> String {
        let doc = Html::parse_fragment(html);
        let sel = Selector::parse("body, div").unwrap();
        let root = doc.select(&sel).next().unwrap();
        to_ai_markdown(root)
    }

    #[test]
    fn headings_and_paragraphs() {
        let md = convert("<div><h2>Titre</h2><p>Un paragraphe  avec   espaces.</p></div>");
        assert!(md.contains("## Titre"));
        assert!(md.contains("Un paragraphe avec espaces."));
    }

    #[test]
    fn skips_nav_and_footer() {
        let md = convert("<div><nav>menu accueil contact</nav><p>contenu réel</p><footer>copyright</footer></div>");
        assert!(md.contains("contenu réel"));
        assert!(!md.contains("menu accueil"));
        assert!(!md.contains("copyright"));
    }

    #[test]
    fn skips_noise_by_class() {
        let md = convert("<div><div class='sidebar'>liens bruit</div><p>vrai contenu</p></div>");
        assert!(md.contains("vrai contenu"));
        assert!(!md.contains("liens bruit"));
    }

    #[test]
    fn code_block_with_lang() {
        let md = convert("<div><pre><code class='language-rust'>fn main() {}</code></pre></div>");
        assert!(md.contains("```rust"));
        assert!(md.contains("fn main() {}"));
    }

    #[test]
    fn list_no_duplication() {
        // un li avec sous-liste ne doit pas dupliquer le texte parent.
        let md = convert("<div><ul><li>parent<ul><li>enfant</li></ul></li></ul></div>");
        let parent_count = md.matches("parent").count();
        assert_eq!(parent_count, 1, "le texte parent ne doit apparaître qu'une fois");
        assert!(md.contains("enfant"));
    }
}
