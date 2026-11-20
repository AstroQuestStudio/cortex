//! Symboles pour les fichiers de configuration et de schéma : SQL, YAML, TOML, JSON.
//!
//! Scanners écrits à la main, SANS grammaire tree-sitter ni dépendance : la version
//! tree-sitter 0.22 de Cortex bloque les grammaires récentes, et ces formats se
//! prêtent bien à une lecture par lignes ou par lexèmes. Un fichier de ces
//! langages donnait jusqu'ici zéro symbole (`has_parser` faux) : `cortex find
//! "politique RLS sur la table files"` ne trouvait rien dans des migrations.
//!
//! Format d'index INCHANGÉ : on emploie les `SymbolKind` existants (table, vue :
//! `Struct` ; fonction, procédure, trigger : `Function` ; enum : `Enum` ; policy,
//! colonne, index, clé de config : `Const`). Le vrai genre (TABLE, POLICY…) est
//! dans la signature et dans le résumé ; les mots d'alias de recherche (policy,
//! politique, rls, table, colonne, workflow…) sont dans le champ `doc`
//! (poids 0,4), pas dans les noms : ils ne concurrencent pas les noms du code.
//!
//! Garde-fous : jamais de panique (tout est lu en octets, `catch_unwind` en
//! dernier recours), plafonds de taille de fichier et de nombre de symboles,
//! fichiers générés ou de données ignorés (lockfiles, JSON volumineux…).

use crate::lang::Lang;
use crate::symbol::{text_tokens, tokenize_identifier, Symbol, SymbolKind, MAX_COMMENT_TOKENS};

/// Taille maximale analysée, par langage (octets). Au-delà : aucun symbole.
const MAX_SQL_BYTES: usize = 16 << 20;
const MAX_JSON_BYTES: usize = 256 << 10;
const MAX_YAML_BYTES: usize = 512 << 10;
const MAX_TOML_BYTES: usize = 256 << 10;
/// Plafonds de symboles par fichier.
const MAX_SYMS_SQL: usize = 3000;
const MAX_SYMS_CONF: usize = 200;
/// Profondeur maximale des clés YAML retenues (`jobs.build.steps` = 3).
const MAX_YAML_DEPTH: usize = 3;
/// Au-delà, un objet JSON / une table YAML de premier niveau est une donnée, pas une config.
const MAX_TOP_KEYS: usize = 80;
/// Longueur maximale d'un nom de symbole (la même limite que `extract::run`).
const MAX_NAME: usize = 120;
/// Plafond de lignes de colonnes dans la signature d'une table.
const MAX_SIG_COLS: usize = 60;
const MAX_SIG_CHARS: usize = 3000;

/// Symboles d'un fichier SQL / YAML / TOML / JSON. Vide pour tout autre langage,
/// pour un fichier trop gros ou jugé généré. Ne panique jamais.
pub fn extract(lang: Lang, source: &str) -> Vec<Symbol> {
    let r = std::panic::catch_unwind(|| match lang {
        Lang::Sql => extract_sql(source),
        Lang::Yaml => extract_yaml(source),
        Lang::Toml => extract_toml(source),
        Lang::Json => extract_json(source),
        _ => Vec::new(),
    });
    r.unwrap_or_default()
}

/// Vrai pour les langages traités par ce module.
pub fn handles(lang: Lang) -> bool {
    matches!(lang, Lang::Sql | Lang::Yaml | Lang::Toml | Lang::Json)
}

/// Coefficient appliqué au score des symboles de fichiers de configuration quand la question ne
/// les nomme pas : ils restent atteignables, mais ne passent pas devant le code (banc public).
pub const CONFIG_PENALTY: f32 = 0.5;

/// Vrai si le chemin est un fichier SQL / YAML / TOML / JSON.
pub fn is_config_path(path: &str) -> bool {
    let ext = path.rsplit('.').next().unwrap_or("");
    ["sql", "yml", "yaml", "toml", "json"].iter().any(|e| ext.eq_ignore_ascii_case(e))
}

/// Mots qui désignent explicitement de la configuration ou un schéma : pas de pénalité.
const MOTS_CONFIG: &[&str] = &[
    "table",
    "tables",
    "policy",
    "policies",
    "politique",
    "politiques",
    "rls",
    "migration",
    "migrations",
    "workflow",
    "workflows",
    "yaml",
    "yml",
    "toml",
    "json",
    "sql",
    "colonne",
    "colonnes",
    "column",
    "columns",
    "schema",
    "schéma",
    "trigger",
    "triggers",
    "déclencheur",
    "declencheur",
    "enum",
    "vue",
    "view",
    "rpc",
    "postgres",
    "supabase",
    "pipeline",
    "jobs",
    "ci",
];

/// Vrai si la question demande de la configuration / un schéma (au moins un mot de `MOTS_CONFIG`).
pub fn query_mentions_config(question: &str) -> bool {
    question.to_lowercase().split(|c: char| !c.is_alphanumeric()).any(|w| MOTS_CONFIG.contains(&w))
}

// ───────────────────────── utilitaires communs ─────────────────────────

/// Construit un symbole : jetons de nom (segments pointés, schéma `public` omis),
/// mots d'alias et texte libre dans `doc` (plafonné).
fn mk(name: &str, kind: SymbolKind, line: u32, end_line: u32, signature: String, summary: String, words: &[&str], free: &[&str]) -> Symbol {
    let name = clean_name(name);
    let mut tokens = name_tokens(&name);
    tokens.retain(|t| t.len() >= 2);
    let mut doc: Vec<String> = Vec::new();
    for w in words {
        for t in w.split_whitespace() {
            push_uniq(&mut doc, t.to_ascii_lowercase());
        }
    }
    for f in free {
        for t in text_tokens(f) {
            push_uniq(&mut doc, t);
        }
        // Identifiants à tiret bas gardés entiers (`size_bytes`), en plus de leurs morceaux.
        for piece in f.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
            if piece.len() >= 3 && piece.len() <= 60 && piece.contains('_') && !piece.starts_with('_') {
                push_uniq(&mut doc, piece.to_ascii_lowercase());
            }
        }
    }
    doc.truncate(MAX_COMMENT_TOKENS);
    Symbol { name, kind, line, end_line: end_line.max(line), signature, tokens, doc, summary }
}

fn push_uniq(v: &mut Vec<String>, t: String) {
    if t.len() >= 2 && !v.contains(&t) {
        v.push(t);
    }
}

/// Nom copiable dans un identifiant : plus d'espaces ni de `#`, longueur bornée.
fn clean_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut replaced = false;
    for c in name.trim().chars() {
        if c.is_whitespace() || c == '#' || c.is_control() {
            // Une suite de blancs donne un seul `_`.
            if !replaced {
                out.push('_');
            }
            replaced = true;
        } else {
            out.push(c);
            replaced = false;
        }
        if out.len() >= MAX_NAME {
            break;
        }
    }
    out
}

/// Jetons du nom : décomposition de chaque segment pointé + segments entiers
/// (`doc_files`), schéma `public` omis (il n'apporte rien et allonge le champ).
fn name_tokens(name: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let segs: Vec<&str> = name.split('.').filter(|s| !s.is_empty()).collect();
    for (i, seg) in segs.iter().enumerate() {
        if i == 0 && segs.len() > 1 && seg.eq_ignore_ascii_case("public") {
            continue;
        }
        for t in tokenize_identifier(seg) {
            push_uniq(&mut out, t);
        }
        push_uniq(&mut out, seg.to_ascii_lowercase());
    }
    if out.is_empty() {
        for t in tokenize_identifier(name) {
            push_uniq(&mut out, t);
        }
    }
    out
}

/// Tronque à `n` caractères avec « … ».
fn cut(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut o: String = s.chars().take(n.saturating_sub(1)).collect();
        o.push('…');
        o
    }
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Dernier segment d'un nom qualifié (`public.doc_files` → `doc_files`).
fn bare(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

// ═════════════════════════════════ SQL ═════════════════════════════════

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum K {
    Word,
    QIdent,
    Str,
    Dollar,
    Num,
    Punct,
    Term,
}

#[derive(Clone, Copy, Debug)]
struct Tok {
    k: K,
    s: usize,
    e: usize,
    line: u32,
}

struct Lexer<'a> {
    b: &'a [u8],
    i: usize,
    line: u32,
    /// Délimiteur MySQL personnalisé (`DELIMITER //`) ; `None` = `;`.
    delim: Option<Vec<u8>>,
}

fn is_word_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c >= 0x80
}
fn is_word_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
}

impl<'a> Lexer<'a> {
    fn new(b: &'a [u8]) -> Self {
        let mut i = 0;
        if b.starts_with(&[0xEF, 0xBB, 0xBF]) {
            i = 3;
        }
        Lexer { b, i, line: 1, delim: None }
    }

    /// Saute espaces et commentaires (`--`, `/* */` imbriqués).
    fn skip_blank(&mut self) {
        let b = self.b;
        while self.i < b.len() {
            let c = b[self.i];
            if c == b'\n' {
                self.line += 1;
                self.i += 1;
            } else if c == b' ' || c == b'\t' || c == b'\r' || c == 0x0c {
                self.i += 1;
            } else if c == b'-' && b.get(self.i + 1) == Some(&b'-') {
                while self.i < b.len() && b[self.i] != b'\n' {
                    self.i += 1;
                }
            } else if c == b'/' && b.get(self.i + 1) == Some(&b'*') {
                let mut depth = 1;
                self.i += 2;
                while self.i < b.len() && depth > 0 {
                    if b[self.i] == b'/' && b.get(self.i + 1) == Some(&b'*') {
                        depth += 1;
                        self.i += 2;
                    } else if b[self.i] == b'*' && b.get(self.i + 1) == Some(&b'/') {
                        depth -= 1;
                        self.i += 2;
                    } else {
                        if b[self.i] == b'\n' {
                            self.line += 1;
                        }
                        self.i += 1;
                    }
                }
            } else {
                break;
            }
        }
    }

    fn next(&mut self) -> Option<Tok> {
        self.skip_blank();
        let b = self.b;
        if self.i >= b.len() {
            return None;
        }
        let start = self.i;
        let line0 = self.line;
        let c = b[start];
        if let Some(d) = &self.delim {
            if b[start..].starts_with(d) {
                let e = start + d.len();
                self.i = e;
                return Some(Tok { k: K::Term, s: start, e, line: line0 });
            }
        }
        let (k, e) = match c {
            b';' => (if self.delim.is_some() { K::Punct } else { K::Term }, start + 1),
            b'\'' => {
                let esc = start > 0 && (b[start - 1] == b'e' || b[start - 1] == b'E') && (start < 2 || !is_word_byte(b[start - 2]));
                let mut j = start + 1;
                while j < b.len() {
                    if esc && b[j] == b'\\' {
                        j += 2;
                        continue;
                    }
                    if b[j] == b'\'' {
                        if b.get(j + 1) == Some(&b'\'') {
                            j += 2;
                            continue;
                        }
                        j += 1;
                        break;
                    }
                    j += 1;
                }
                (K::Str, j.min(b.len()))
            }
            b'"' | b'`' => {
                let q = c;
                let mut j = start + 1;
                while j < b.len() {
                    if b[j] == q {
                        if b.get(j + 1) == Some(&q) {
                            j += 2;
                            continue;
                        }
                        j += 1;
                        break;
                    }
                    j += 1;
                }
                (K::QIdent, j.min(b.len()))
            }
            b'$' if self.delim.is_none() => {
                let mut j = start + 1;
                while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] >= 0x80) {
                    j += 1;
                }
                let tag_ok = j < b.len() && b[j] == b'$' && !(j > start + 1 && b[start + 1].is_ascii_digit());
                if tag_ok {
                    let tag = &b[start..=j];
                    let from = j + 1;
                    let mut end = b.len();
                    let mut p = from;
                    while p < b.len() {
                        match b[p..].iter().position(|&x| x == b'$') {
                            Some(off) => {
                                let q = p + off;
                                if b[q..].starts_with(tag) {
                                    end = q + tag.len();
                                    break;
                                }
                                p = q + 1;
                            }
                            None => break,
                        }
                    }
                    (K::Dollar, end)
                } else {
                    // Paramètre positionnel `$1`.
                    (K::Num, j.max(start + 1))
                }
            }
            c if c.is_ascii_digit() => {
                let mut j = start + 1;
                while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'.' || b[j] == b'_') {
                    j += 1;
                }
                (K::Num, j)
            }
            c if is_word_start(c) => {
                let mut j = start + 1;
                while j < b.len() && is_word_byte(b[j]) {
                    j += 1;
                }
                (K::Word, j)
            }
            _ => (K::Punct, start + 1),
        };
        self.line += b[start..e].iter().filter(|&&x| x == b'\n').count() as u32;
        self.i = e;
        Some(Tok { k, s: start, e, line: line0 })
    }

    /// `DELIMITER //` : lit le reste de la ligne et change le terminateur.
    fn read_delimiter(&mut self) {
        let b = self.b;
        while self.i < b.len() && (b[self.i] == b' ' || b[self.i] == b'\t') {
            self.i += 1;
        }
        let s = self.i;
        while self.i < b.len() && !b[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
        let d = &b[s..self.i];
        self.delim = if d.is_empty() || d == b";" { None } else { Some(d.to_vec()) };
    }

    /// Données d'un `COPY … FROM stdin;` : jusqu'à la ligne `\.`.
    fn skip_copy_data(&mut self) {
        let b = self.b;
        // Fin de la ligne courante.
        while self.i < b.len() && b[self.i] != b'\n' {
            self.i += 1;
        }
        while self.i < b.len() {
            // Début de ligne : après le '\n'.
            if b[self.i] == b'\n' {
                self.line += 1;
                self.i += 1;
            }
            let ls = self.i;
            while self.i < b.len() && b[self.i] != b'\n' {
                self.i += 1;
            }
            let l = &b[ls..self.i];
            if l.starts_with(b"\\.") {
                return;
            }
        }
    }
}

/// Jeton de lecture d'une instruction : accès aux textes et aux mots en minuscules.
struct Stmt<'a> {
    b: &'a [u8],
    t: &'a [Tok],
}

impl<'a> Stmt<'a> {
    fn raw(&self, i: usize) -> String {
        self.t.get(i).map(|t| lossy(&self.b[t.s..t.e])).unwrap_or_default()
    }
    /// Mot (non guillemeté) en minuscules.
    fn w(&self, i: usize) -> Option<String> {
        let t = self.t.get(i)?;
        (t.k == K::Word).then(|| lossy(&self.b[t.s..t.e]).to_ascii_lowercase())
    }
    fn is(&self, i: usize, kw: &str) -> bool {
        match self.t.get(i) {
            Some(t) if t.k == K::Word => self.b[t.s..t.e].eq_ignore_ascii_case(kw.as_bytes()),
            _ => false,
        }
    }
    fn punct(&self, i: usize, c: u8) -> bool {
        matches!(self.t.get(i), Some(t) if t.k == K::Punct && self.b[t.s] == c)
    }
    /// Identifiant : mot tel quel, ou contenu d'un identifiant entre guillemets.
    fn ident(&self, i: usize) -> Option<String> {
        let t = self.t.get(i)?;
        match t.k {
            K::Word => Some(lossy(&self.b[t.s..t.e])),
            K::QIdent => {
                let inner = &self.b[t.s + 1..t.e.saturating_sub(1).max(t.s + 1)];
                let q = self.b[t.s];
                let s = lossy(inner);
                let dbl = format!("{0}{0}", q as char);
                Some(s.replace(&dbl, &(q as char).to_string()))
            }
            _ => None,
        }
    }
    /// Nom qualifié `a.b.c` à partir de `i` ; rend le nom et l'indice suivant.
    fn name(&self, mut i: usize) -> Option<(String, usize)> {
        let mut out = self.ident(i)?;
        i += 1;
        while self.punct(i, b'.') {
            match self.ident(i + 1) {
                Some(n) => {
                    out.push('.');
                    out.push_str(&n);
                    i += 2;
                }
                None => break,
            }
        }
        Some((out, i))
    }
    /// Indice du `)` qui ferme la `(` de `open` (profondeur), ou la fin.
    fn close_of(&self, open: usize) -> usize {
        let mut d = 0i32;
        for j in open..self.t.len() {
            let t = &self.t[j];
            if t.k == K::Punct {
                match self.b[t.s] {
                    b'(' => d += 1,
                    b')' => {
                        d -= 1;
                        if d == 0 {
                            return j;
                        }
                    }
                    _ => {}
                }
            }
        }
        self.t.len()
    }
    /// Texte compact de `[a, b)` : espaces d'origine conservés (pas de commentaires),
    /// chaînes et corps `$$` abrégés.
    fn render(&self, a: usize, b: usize, max: usize) -> String {
        let mut out = String::new();
        let mut prev_end: Option<usize> = None;
        for t in &self.t[a.min(self.t.len())..b.min(self.t.len())] {
            if let Some(pe) = prev_end {
                if t.s > pe {
                    out.push(' ');
                }
            }
            match t.k {
                K::Str if t.e - t.s > 40 => {
                    out.push('\'');
                    out.push_str(&cut(&lossy(&self.b[t.s + 1..t.e - 1]), 30));
                    out.push('\'');
                }
                K::Dollar => out.push_str("$$…$$"),
                _ => out.push_str(&lossy(&self.b[t.s..t.e])),
            }
            prev_end = Some(t.e);
            if out.chars().count() > max + 20 {
                break;
            }
        }
        cut(out.trim(), max)
    }
    /// Découpe `[a, b)` aux virgules de profondeur 0 (parenthèses).
    fn split_commas(&self, a: usize, b: usize) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut d = 0i32;
        let mut s = a;
        for j in a..b.min(self.t.len()) {
            let t = &self.t[j];
            if t.k == K::Punct {
                match self.b[t.s] {
                    b'(' => d += 1,
                    b')' => d -= 1,
                    b',' if d == 0 => {
                        out.push((s, j));
                        s = j + 1;
                    }
                    _ => {}
                }
            }
        }
        if s < b.min(self.t.len()) {
            out.push((s, b.min(self.t.len())));
        }
        out
    }
}

/// Run de GRANT consécutifs agrégé en un symbole.
struct GrantRun {
    line: u32,
    end: u32,
    lines: Vec<String>,
    objects: Vec<String>,
    roles: Vec<String>,
    privs: Vec<String>,
    count: usize,
}

struct SqlOut {
    syms: Vec<Symbol>,
    grants: Option<GrantRun>,
}

impl SqlOut {
    fn full(&self) -> bool {
        self.syms.len() >= MAX_SYMS_SQL
    }
    fn push(&mut self, s: Symbol) {
        self.flush_grants();
        if !self.full() {
            self.syms.push(s);
        }
    }
    fn flush_grants(&mut self) {
        let Some(g) = self.grants.take() else { return };
        if self.full() {
            return;
        }
        let first_obj = g.objects.first().map(|o| bare(o).to_string()).unwrap_or_else(|| "all".into());
        let mut sig = g.lines.join("\n");
        if g.count > g.lines.len() {
            sig.push_str(&format!("\n… +{} GRANT", g.count - g.lines.len()));
        }
        let summary = format!("grants {} ({} instruction(s)) to {}", g.privs.join(", "), g.count, cut(&g.roles.join(", "), 60));
        let mut free: Vec<String> = Vec::new();
        free.extend(g.objects.iter().take(60).cloned());
        free.extend(g.roles.iter().take(20).cloned());
        free.extend(g.privs.iter().take(10).cloned());
        let free_ref: Vec<&str> = free.iter().map(|s| s.as_str()).collect();
        let s = mk(
            &format!("grants.{}", first_obj),
            SymbolKind::Const,
            g.line,
            g.end,
            sig,
            summary,
            &["sql", "migration", "grant", "grants", "droit", "droits", "privilege", "privileges", "permission", "permissions", "acces"],
            &free_ref,
        );
        self.syms.push(s);
    }
}

fn extract_sql(src: &str) -> Vec<Symbol> {
    let b = src.as_bytes();
    if b.len() > MAX_SQL_BYTES || b.len() < 8 {
        return Vec::new();
    }
    let mut lx = Lexer::new(b);
    let mut out = SqlOut { syms: Vec::new(), grants: None };
    let mut toks: Vec<Tok> = Vec::new();
    let mut skipping = false;
    let mut first_word: Option<String> = None;
    let mut saw_stdin = false;
    let mut routine = false;
    let mut depth: i32 = 0;
    let mut pending_end = false;
    let mut last_line = 1u32;

    while let Some(t) = lx.next() {
        last_line = t.line;
        if t.k == K::Term && depth <= 0 {
            if !toks.is_empty() {
                analyze(b, &toks, t.line, &mut out);
            }
            let was_copy = first_word.as_deref() == Some("copy") && saw_stdin;
            toks.clear();
            skipping = false;
            first_word = None;
            saw_stdin = false;
            routine = false;
            depth = 0;
            pending_end = false;
            if was_copy {
                lx.skip_copy_data();
            }
            if out.full() {
                break;
            }
            continue;
        }
        if first_word.is_none() {
            // Premier jeton de l'instruction : intéressant ?
            let w = if t.k == K::Word { lossy(&b[t.s..t.e]).to_ascii_lowercase() } else { String::new() };
            if w == "delimiter" {
                lx.read_delimiter();
                continue;
            }
            skipping = !matches!(w.as_str(), "create" | "alter" | "grant");
            first_word = Some(w);
        } else if skipping && first_word.as_deref() == Some("copy") && t.k == K::Word && b[t.s..t.e].eq_ignore_ascii_case(b"stdin") {
            saw_stdin = true;
        }
        if skipping {
            continue;
        }
        if toks.len() < 60_000 {
            toks.push(t);
        }
        if t.k == K::Word {
            let w = lossy(&b[t.s..t.e]).to_ascii_lowercase();
            if !routine
                && matches!(w.as_str(), "function" | "procedure" | "trigger")
                && first_word.as_deref() == Some("create")
                && toks.len() <= 14
            {
                routine = true;
            } else if routine {
                if pending_end {
                    pending_end = false;
                    if matches!(w.as_str(), "if" | "loop" | "while" | "repeat" | "for") {
                        depth += 1;
                    }
                }
                match w.as_str() {
                    "begin" | "case" => depth += 1,
                    "end" => {
                        depth -= 1;
                        pending_end = true;
                    }
                    _ => {}
                }
            }
        } else {
            pending_end = false;
        }
    }
    if !toks.is_empty() && !out.full() {
        // Fichier tronqué : on analyse ce qu'on a, la fin = dernière ligne vue.
        analyze(b, &toks, last_line, &mut out);
    }
    out.flush_grants();
    out.syms
}

/// Mots mots-clés d'objets pour CREATE.
const OBJ_WORDS: &[&str] =
    &["table", "view", "index", "function", "procedure", "trigger", "type", "schema", "extension", "policy", "domain"];
/// Modificateurs qui peuvent précéder le mot-clé d'objet.
const CREATE_MODS: &[&str] = &[
    "or",
    "replace",
    "temp",
    "temporary",
    "unlogged",
    "global",
    "local",
    "unique",
    "materialized",
    "recursive",
    "constraint",
    "event",
    "virtual",
    "foreign",
    "trusted",
    "force",
    "editionable",
    "noneditionable",
    "concurrently",
];

fn analyze(b: &[u8], toks: &[Tok], end_line: u32, out: &mut SqlOut) {
    if toks.is_empty() || out.full() {
        return;
    }
    let st = Stmt { b, t: toks };
    let line = toks[0].line;
    match st.w(0).as_deref() {
        Some("create") => create(&st, line, end_line, out),
        Some("alter") => alter(&st, line, end_line, out),
        Some("grant") => grant(&st, line, end_line, out),
        _ => {}
    }
}

const SQL: &[&str] = &["sql", "migration"];

fn cat(a: &[&'static str], b: &[&'static str]) -> Vec<&'static str> {
    a.iter().chain(b.iter()).copied().collect()
}

fn create(st: &Stmt, line: u32, end_line: u32, out: &mut SqlOut) {
    let mut i = 1;
    let mut loose = false;
    let mut materialized = false;
    let mut unique = false;
    let mut obj: Option<String> = None;
    while i < st.t.len() && i < 24 {
        if let Some(w) = st.w(i) {
            if OBJ_WORDS.contains(&w.as_str()) {
                obj = Some(w);
                break;
            }
            match w.as_str() {
                "materialized" => materialized = true,
                "unique" => unique = true,
                "definer" | "algorithm" | "sql" => loose = true,
                _ => {
                    if !CREATE_MODS.contains(&w.as_str()) && !loose {
                        return;
                    }
                }
            }
        } else if !loose && !st.punct(i, b'=') {
            return;
        }
        i += 1;
    }
    let Some(obj) = obj else { return };
    i += 1;
    // `CONCURRENTLY`, `IF NOT EXISTS`.
    while i < st.t.len() && (st.is(i, "concurrently") || st.is(i, "if") || st.is(i, "not") || st.is(i, "exists")) {
        i += 1;
    }
    match obj.as_str() {
        "table" => create_table(st, i, line, end_line, out),
        "view" => create_view(st, i, line, end_line, materialized, out),
        "index" => create_index(st, i, line, end_line, unique, out),
        "function" | "procedure" => create_function(st, i, line, end_line, &obj, out),
        "trigger" => create_trigger(st, i, line, end_line, out),
        "policy" => create_policy(st, i, line, end_line, out),
        "type" | "domain" => create_type(st, i, line, end_line, &obj, out),
        "extension" => {
            let Some((name, j)) = st.name(i) else { return };
            let schema = (0..st.t.len()).find(|&k| st.is(k, "schema") && k > j).and_then(|k| st.ident(k + 1));
            let mut sig = format!("CREATE EXTENSION {}", name);
            if let Some(s) = &schema {
                sig.push_str(&format!(" WITH SCHEMA {}", s));
            }
            let summary = format!("extension {}", name);
            let words = cat(SQL, &["extension", "extensions", "create"]);
            let s = mk(&name, SymbolKind::Const, line, end_line, sig, summary, &words, &[]);
            out.push(s);
        }
        "schema" => {
            let (name, _) = match st.name(i) {
                Some(x) => x,
                None => return,
            };
            let summary = format!("schema {}", name);
            let words = cat(SQL, &["schema", "schemas", "create"]);
            let s = mk(&name, SymbolKind::Const, line, end_line, format!("CREATE SCHEMA {}", name), summary, &words, &[]);
            out.push(s);
        }
        _ => {}
    }
}

fn create_table(st: &Stmt, i: usize, line: u32, end_line: u32, out: &mut SqlOut) {
    let Some((name, j)) = st.name(i) else { return };
    let mut cols: Vec<(String, String)> = Vec::new(); // (nom, ligne)
    let mut cons: Vec<String> = Vec::new();
    let mut refs: Vec<String> = Vec::new();
    if st.punct(j, b'(') {
        let close = st.close_of(j).min(st.t.len());
        for (a, e) in st.split_commas(j + 1, close) {
            if a >= e {
                continue;
            }
            // Références : `REFERENCES autre(...)`.
            for k in a..e {
                if st.is(k, "references") {
                    if let Some((r, _)) = st.name(k + 1) {
                        push_uniq(&mut refs, bare(&r).to_string());
                    }
                }
            }
            let head = st.w(a).unwrap_or_default();
            if matches!(head.as_str(), "constraint" | "primary" | "foreign" | "unique" | "check" | "exclude" | "like" | "period") {
                cons.push(st.render(a, e, 90));
            } else if let Some(cn) = st.ident(a) {
                let rest = st.render(a + 1, e, 90);
                cols.push((cn.clone(), if rest.is_empty() { cn } else { format!("{} {}", cn, rest) }));
            }
        }
    }
    let mut sig = format!("TABLE {}", name);
    let mut n = 0;
    for (_, l) in cols.iter().take(MAX_SIG_COLS) {
        sig.push_str("\n  ");
        sig.push_str(l);
        n += 1;
        if sig.len() > MAX_SIG_CHARS {
            break;
        }
    }
    if cols.len() > n {
        sig.push_str(&format!("\n  … +{} colonnes", cols.len() - n));
    }
    for c in cons.iter().take(6) {
        if sig.len() > MAX_SIG_CHARS {
            break;
        }
        sig.push_str("\n  ");
        sig.push_str(c);
    }
    let names: Vec<&str> = cols.iter().map(|(n, _)| n.as_str()).collect();
    let summary = format!(
        "table {} — {} colonnes : {}{}",
        name,
        cols.len(),
        names.iter().take(8).cloned().collect::<Vec<_>>().join(", "),
        if names.len() > 8 { ", …" } else { "" }
    );
    let words = cat(SQL, &["table", "tables", "colonne", "colonnes", "column", "columns", "create", "schema", "creation"]);
    let mut free: Vec<String> = Vec::new();
    for (cn, _) in cols.iter().take(120) {
        free.push(cn.clone());
    }
    free.extend(refs.iter().cloned());
    let free_ref: Vec<&str> = free.iter().map(|s| s.as_str()).collect();
    out.push(mk(&name, SymbolKind::Struct, line, end_line, sig, summary, &words, &free_ref));
}

fn create_view(st: &Stmt, i: usize, line: u32, end_line: u32, materialized: bool, out: &mut SqlOut) {
    let Some((name, j)) = st.name(i) else { return };
    let as_idx = (j..st.t.len()).find(|&k| st.is(k, "as")).unwrap_or(st.t.len());
    let col_part = if st.punct(j, b'(') { st.render(j, st.close_of(j) + 1, 100) } else { String::new() };
    let body = if as_idx < st.t.len() { st.render(as_idx + 1, st.t.len(), 110) } else { String::new() };
    let kw = if materialized { "MATERIALIZED VIEW" } else { "VIEW" };
    let sig = format!("{} {}{} AS {}", kw, name, col_part, body).replace(" AS ", " AS ");
    // Tables lues : mots après FROM / JOIN.
    let mut froms: Vec<String> = Vec::new();
    for k in as_idx..st.t.len() {
        if st.is(k, "from") || st.is(k, "join") {
            if let Some((n, _)) = st.name(k + 1) {
                push_uniq(&mut froms, bare(&n).to_string());
            }
        }
    }
    let summary = format!(
        "{} {}{}",
        kw.to_ascii_lowercase(),
        name,
        if froms.is_empty() { String::new() } else { format!(" sur {}", froms.join(", ")) }
    );
    let words = cat(SQL, &["view", "views", "vue", "vues", "create"]);
    let mut w2 = words.clone();
    if materialized {
        w2.extend_from_slice(&["materialized", "materialisee"]);
    }
    let free: Vec<&str> = froms.iter().map(|s| s.as_str()).collect();
    out.push(mk(&name, SymbolKind::Struct, line, end_line, sig, summary, &w2, &free));
}

fn create_index(st: &Stmt, i: usize, line: u32, end_line: u32, unique: bool, out: &mut SqlOut) {
    // `CREATE INDEX [nom] ON [ONLY] table [USING m] (cols)`.
    let (iname, mut j) = if st.is(i, "on") {
        (None, i)
    } else {
        match st.name(i) {
            Some((n, j)) => (Some(n), j),
            None => return,
        }
    };
    if !st.is(j, "on") {
        return;
    }
    j += 1;
    if st.is(j, "only") {
        j += 1;
    }
    let Some((table, k)) = st.name(j) else { return };
    let open = (k..st.t.len()).find(|&x| st.punct(x, b'(')).unwrap_or(st.t.len());
    let close = st.close_of(open);
    let cols = if open < st.t.len() { st.render(open, close + 1, 100) } else { String::new() };
    let using = if st.is(k, "using") { st.w(k + 1).unwrap_or_default() } else { String::new() };
    let where_part = (close + 1..st.t.len()).find(|&x| st.is(x, "where")).map(|x| st.render(x, st.t.len(), 80)).unwrap_or_default();
    let col_names: Vec<String> = (open + 1..close.min(st.t.len()))
        .filter_map(|x| if st.t[x].k == K::Word || st.t[x].k == K::QIdent { st.ident(x) } else { None })
        .take(8)
        .collect();
    let name = iname.unwrap_or_else(|| format!("{}_{}_idx", bare(&table), col_names.iter().take(3).cloned().collect::<Vec<_>>().join("_")));
    let mut sig = format!("CREATE {}INDEX {} ON {}", if unique { "UNIQUE " } else { "" }, name, table);
    if !using.is_empty() {
        sig.push_str(&format!(" USING {}", using));
    }
    if !cols.is_empty() {
        sig.push(' ');
        sig.push_str(&cols);
    }
    if !where_part.is_empty() {
        sig.push(' ');
        sig.push_str(&where_part);
    }
    let summary = format!("{}index {} sur {} {}", if unique { "unique " } else { "" }, name, table, cols);
    let mut words = cat(SQL, &["index", "indexes", "indice", "create"]);
    if unique {
        words.push("unique");
    }
    let mut free: Vec<String> = vec![table.clone()];
    free.extend(col_names);
    let free_ref: Vec<&str> = free.iter().map(|s| s.as_str()).collect();
    out.push(mk(&name, SymbolKind::Const, line, end_line, sig, summary, &words, &free_ref));
}

fn create_function(st: &Stmt, i: usize, line: u32, end_line: u32, obj: &str, out: &mut SqlOut) {
    let Some((name, j)) = st.name(i) else { return };
    let args;
    let mut arg_names: Vec<String> = Vec::new();
    let mut k = j;
    if st.punct(j, b'(') {
        let close = st.close_of(j);
        args = st.render(j, close + 1, 140);
        for (a, e) in st.split_commas(j + 1, close.min(st.t.len())) {
            // `[IN|OUT|INOUT|VARIADIC] nom type` : le nom est le premier mot qui n'est pas un mode.
            let mut x = a;
            while x < e && matches!(st.w(x).as_deref(), Some("in" | "out" | "inout" | "variadic")) {
                x += 1;
            }
            if x + 1 < e {
                if let Some(n) = st.ident(x) {
                    arg_names.push(n);
                }
            }
        }
        k = close + 1;
    } else {
        args = "()".into();
    }
    let mut ret = String::new();
    let mut lang = String::new();
    let mut secdef = false;
    let mut x = k;
    while x < st.t.len() {
        if st.is(x, "returns") {
            // jusqu'au prochain mot-clé d'option.
            let mut e = x + 1;
            while e < st.t.len() {
                if st.t[e].k == K::Dollar {
                    break;
                }
                if matches!(
                    st.w(e).as_deref(),
                    Some(
                        "language"
                            | "as"
                            | "security"
                            | "volatile"
                            | "stable"
                            | "immutable"
                            | "strict"
                            | "set"
                            | "cost"
                            | "rows"
                            | "parallel"
                            | "window"
                            | "leakproof"
                            | "called"
                            | "begin"
                            | "return"
                            | "external"
                    )
                ) {
                    break;
                }
                if st.punct(e, b'(') {
                    e = st.close_of(e);
                }
                e += 1;
            }
            ret = st.render(x + 1, e.min(st.t.len()), 100);
            x = e;
            continue;
        }
        if st.is(x, "language") {
            lang = st.ident(x + 1).unwrap_or_default().to_ascii_lowercase();
        }
        if st.is(x, "security") && st.is(x + 1, "definer") {
            secdef = true;
        }
        x += 1;
    }
    let mut sig = format!("{} {}{}", obj.to_ascii_uppercase(), name, args);
    if !ret.is_empty() {
        sig.push_str(&format!(" RETURNS {}", ret));
    }
    if !lang.is_empty() {
        sig.push_str(&format!(" LANGUAGE {}", lang));
    }
    if secdef {
        sig.push_str(" SECURITY DEFINER");
    }
    let summary = format!("{} {}{}{}", obj, name, args, if ret.is_empty() { String::new() } else { format!(" → {}", cut(&ret, 50)) });
    let words = cat(SQL, &["function", "functions", "fonction", "fonctions", "rpc", "procedure", "create", "replace"]);
    let mut w2 = words;
    if secdef {
        w2.extend_from_slice(&["security", "definer", "securite"]);
    }
    let mut free: Vec<String> = arg_names;
    if !ret.is_empty() {
        free.push(ret.clone());
    }
    if !lang.is_empty() {
        free.push(lang.clone());
    }
    let free_ref: Vec<&str> = free.iter().map(|s| s.as_str()).collect();
    out.push(mk(&name, SymbolKind::Function, line, end_line, sig, summary, &w2, &free_ref));
}

fn create_trigger(st: &Stmt, i: usize, line: u32, end_line: u32, out: &mut SqlOut) {
    let Some((name, j)) = st.name(i) else { return };
    let on = (j..st.t.len()).find(|&k| st.is(k, "on"));
    let Some(on) = on else { return };
    let timing = (j..on)
        .filter_map(|k| st.w(k))
        .filter(|w| matches!(w.as_str(), "before" | "after" | "instead" | "of" | "insert" | "update" | "delete" | "truncate" | "or"))
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_uppercase();
    let (table, k) = st.name(on + 1).unwrap_or((String::new(), on + 1));
    let exec = (k..st.t.len()).find(|&x| st.is(x, "execute"));
    let func = exec.map(|x| st.render(x, st.t.len(), 80)).unwrap_or_default();
    let mut sig = format!("TRIGGER {} {} ON {}", name, timing, table);
    if !func.is_empty() {
        sig.push(' ');
        sig.push_str(&func);
    }
    let summary = format!("trigger {} {} sur {}", name, timing.to_ascii_lowercase(), table);
    let words = cat(SQL, &["trigger", "triggers", "declencheur", "create"]);
    let mut free: Vec<String> = vec![table.clone(), func.clone()];
    free.push(timing.clone());
    let free_ref: Vec<&str> = free.iter().map(|s| s.as_str()).collect();
    out.push(mk(&name, SymbolKind::Function, line, end_line, sig, summary, &words, &free_ref));
}

fn create_policy(st: &Stmt, i: usize, line: u32, end_line: u32, out: &mut SqlOut) {
    // `CREATE POLICY "nom" ON table [AS ..] [FOR cmd] [TO rôles] [USING (..)] [WITH CHECK (..)]`.
    let Some(pname) = st.ident(i) else { return };
    if !st.is(i + 1, "on") {
        return;
    }
    let Some((table, mut k)) = st.name(i + 2) else { return };
    let mut cmd = String::from("ALL");
    let mut roles: Vec<String> = Vec::new();
    let mut using = String::new();
    let mut check = String::new();
    let mut permissive = String::new();
    while k < st.t.len() {
        if st.is(k, "as") {
            permissive = st.w(k + 1).unwrap_or_default();
            k += 2;
        } else if st.is(k, "for") {
            cmd = st.w(k + 1).unwrap_or_default().to_ascii_uppercase();
            k += 2;
        } else if st.is(k, "to") {
            k += 1;
            while k < st.t.len() && !st.is(k, "using") && !st.is(k, "with") {
                if let Some(r) = st.ident(k) {
                    roles.push(r);
                }
                k += 1;
            }
        } else if st.is(k, "using") && st.punct(k + 1, b'(') {
            let c = st.close_of(k + 1);
            using = st.render(k + 2, c, 90);
            k = c + 1;
        } else if st.is(k, "with") && st.is(k + 1, "check") && st.punct(k + 2, b'(') {
            let c = st.close_of(k + 2);
            check = st.render(k + 3, c, 90);
            k = c + 1;
        } else {
            k += 1;
        }
    }
    let name = format!("{}.{}", bare(&table), pname);
    let mut sig = format!("POLICY \"{}\" ON {}", pname, table);
    if !permissive.is_empty() {
        sig.push_str(&format!(" AS {}", permissive.to_ascii_uppercase()));
    }
    sig.push_str(&format!(" FOR {}", cmd));
    if !roles.is_empty() {
        sig.push_str(&format!(" TO {}", roles.join(", ")));
    }
    if !using.is_empty() {
        sig.push_str(&format!(" USING ({})", using));
    }
    if !check.is_empty() {
        sig.push_str(&format!(" WITH CHECK ({})", check));
    }
    let summary =
        format!("policy RLS {} sur {}{}", cmd, table, if roles.is_empty() { String::new() } else { format!(" pour {}", roles.join(", ")) });
    let words = cat(
        SQL,
        &["policy", "policies", "politique", "politiques", "rls", "row", "level", "security", "securite", "acces", "access", "create"],
    );
    let mut w2 = words;
    let cmd_l = cmd.to_ascii_lowercase();
    w2.push(match cmd_l.as_str() {
        "select" => "select",
        "insert" => "insert",
        "update" => "update",
        "delete" => "delete",
        _ => "all",
    });
    let mut free: Vec<String> = vec![table.clone(), pname.clone(), using.clone(), check.clone()];
    free.extend(roles.iter().cloned());
    let free_ref: Vec<&str> = free.iter().map(|s| s.as_str()).collect();
    out.push(mk(&name, SymbolKind::Const, line, end_line, sig, summary, &w2, &free_ref));
}

fn create_type(st: &Stmt, i: usize, line: u32, end_line: u32, obj: &str, out: &mut SqlOut) {
    let Some((name, j)) = st.name(i) else { return };
    let is_enum = st.is(j, "as") && st.is(j + 1, "enum");
    let mut vals: Vec<String> = Vec::new();
    let mut sig = format!("{} {}", obj.to_ascii_uppercase(), name);
    if is_enum && st.punct(j + 2, b'(') {
        let c = st.close_of(j + 2);
        for x in j + 3..c.min(st.t.len()) {
            if st.t[x].k == K::Str {
                let r = st.raw(x);
                vals.push(r.trim_matches('\'').to_string());
            }
        }
        sig.push_str(&format!(" AS ENUM ({})", cut(&vals.iter().map(|v| format!("'{}'", v)).collect::<Vec<_>>().join(", "), 160)));
    } else {
        sig.push(' ');
        sig.push_str(&st.render(j, st.t.len(), 120));
    }
    let summary = format!(
        "{} {}{}",
        if is_enum { "enum" } else { obj },
        name,
        if vals.is_empty() { String::new() } else { format!(" : {}", cut(&vals.join(", "), 60)) }
    );
    let words = cat(SQL, &["type", "types", "enum", "enumeration", "create"]);
    let free: Vec<&str> = vals.iter().map(|s| s.as_str()).collect();
    let kind = if is_enum { SymbolKind::Enum } else { SymbolKind::Type };
    out.push(mk(&name, kind, line, end_line, sig, summary, &words, &free));
}

fn alter(st: &Stmt, line: u32, end_line: u32, out: &mut SqlOut) {
    if st.is(1, "type") {
        // ALTER TYPE t ADD VALUE 'x' : une valeur d'enum.
        let Some((name, j)) = st.name(2) else { return };
        if st.is(j, "add") && st.is(j + 1, "value") {
            let mut x = j + 2;
            while st.is(x, "if") || st.is(x, "not") || st.is(x, "exists") {
                x += 1;
            }
            if st.t.get(x).map(|t| t.k) == Some(K::Str) {
                let v = st.raw(x).trim_matches('\'').to_string();
                let sig = format!("ALTER TYPE {} ADD VALUE '{}'", name, v);
                let words = cat(SQL, &["type", "enum", "alter", "add", "value", "valeur", "ajout"]);
                out.push(mk(
                    &format!("{}.{}", bare(&name), v),
                    SymbolKind::Const,
                    line,
                    end_line,
                    sig,
                    format!("valeur '{}' ajoutée à l'enum {}", v, name),
                    &words,
                    &[name.as_str()],
                ));
            }
        }
        return;
    }
    if !st.is(1, "table") {
        return;
    }
    let mut i = 2;
    while st.is(i, "if") || st.is(i, "exists") || st.is(i, "only") {
        i += 1;
    }
    let Some((table, j)) = st.name(i) else { return };
    let tb = bare(&table).to_string();
    let mut emitted = 0;
    let mut generic: Option<String> = None;
    for (a, e) in st.split_commas(j, st.t.len()) {
        if emitted >= 12 {
            break;
        }
        let w0 = st.w(a).unwrap_or_default();
        match w0.as_str() {
            "add" => {
                let mut x = a + 1;
                if st.is(x, "column") {
                    x += 1;
                }
                let constraint_kw =
                    matches!(st.w(x).as_deref(), Some("constraint" | "primary" | "foreign" | "unique" | "check" | "exclude"));
                while st.is(x, "if") || st.is(x, "not") || st.is(x, "exists") {
                    x += 1;
                }
                if constraint_kw && !st.is(a + 1, "column") {
                    // ADD CONSTRAINT nom …
                    if st.is(a + 1, "constraint") {
                        if let Some(cn) = st.ident(a + 2) {
                            let sig = format!("ALTER TABLE {} {}", table, st.render(a, e, 140));
                            let words = cat(SQL, &["alter", "table", "constraint", "contrainte", "add", "ajout"]);
                            out.push(mk(
                                &format!("{}.{}", tb, cn),
                                SymbolKind::Const,
                                line,
                                end_line,
                                sig,
                                format!("contrainte {} ajoutée à {}", cn, table),
                                &words,
                                &[table.as_str()],
                            ));
                            emitted += 1;
                        }
                    } else {
                        generic.get_or_insert_with(|| st.render(a, e, 140));
                    }
                    continue;
                }
                if let Some(cn) = st.ident(x) {
                    let rest = st.render(x, e, 120);
                    let sig = format!("ALTER TABLE {} ADD COLUMN {}", table, rest);
                    let words = cat(SQL, &["column", "colonne", "alter", "table", "add", "ajout", "ajoute", "ajouter", "colonnes"]);
                    let ty = st.render(x + 1, e, 60);
                    out.push(mk(
                        &format!("{}.{}", tb, cn),
                        SymbolKind::Const,
                        line,
                        end_line,
                        sig,
                        format!("colonne {} ajoutée à la table {} : {}", cn, table, ty),
                        &words,
                        &[table.as_str(), ty.as_str()],
                    ));
                    emitted += 1;
                }
            }
            "enable" | "force" | "disable" | "no" => {
                let has_rls = (a..e).any(|k| st.is(k, "row")) && (a..e).any(|k| st.is(k, "security"));
                if has_rls {
                    let act = st.render(a, e, 60);
                    let sig = format!("ALTER TABLE {} {}", table, act);
                    let words = cat(
                        SQL,
                        &[
                            "rls",
                            "row",
                            "level",
                            "security",
                            "securite",
                            "policy",
                            "politique",
                            "alter",
                            "table",
                            "enable",
                            "active",
                            "activer",
                        ],
                    );
                    out.push(mk(
                        &format!("{}.rls", tb),
                        SymbolKind::Const,
                        line,
                        end_line,
                        sig,
                        format!("RLS {} sur {}", act.to_ascii_lowercase(), table),
                        &words,
                        &[table.as_str()],
                    ));
                    emitted += 1;
                }
            }
            "drop" | "alter" | "rename" => {
                if w0 == "drop" || w0 == "alter" {
                    let mut x = a + 1;
                    let kw = st.w(x).unwrap_or_default();
                    if kw == "column" {
                        x += 1;
                    }
                    while st.is(x, "if") || st.is(x, "exists") {
                        x += 1;
                    }
                    let is_constraint = kw == "constraint";
                    if is_constraint {
                        x = a + 2;
                        while st.is(x, "if") || st.is(x, "exists") {
                            x += 1;
                        }
                    }
                    if kw != "constraint" && kw != "column" && w0 == "alter" && !st.is(a + 1, "column") && st.ident(a + 1).is_none() {
                        continue;
                    }
                    if let Some(cn) = st.ident(x) {
                        if w0 == "alter" && kw == "constraint" {
                            continue;
                        }
                        let sig = format!("ALTER TABLE {} {}", table, st.render(a, e, 140));
                        let words = cat(SQL, &["alter", "table", "column", "colonne", "drop", "supprime", "modifie", "change"]);
                        out.push(mk(
                            &format!("{}.{}", tb, cn),
                            SymbolKind::Const,
                            line,
                            end_line,
                            sig,
                            format!("{} {} sur {}", w0, cn, table),
                            &words,
                            &[table.as_str()],
                        ));
                        emitted += 1;
                    }
                } else {
                    // RENAME [COLUMN] a TO b | RENAME TO b
                    let mut x = a + 1;
                    if st.is(x, "column") {
                        x += 1;
                    }
                    let target = if st.is(x, "to") {
                        st.ident(x + 1)
                    } else if st.is(x + 1, "to") {
                        st.ident(x + 2)
                    } else {
                        None
                    };
                    if let Some(nn) = target {
                        let sig = format!("ALTER TABLE {} {}", table, st.render(a, e, 140));
                        let words = cat(SQL, &["alter", "table", "rename", "renomme", "renommage", "column", "colonne"]);
                        out.push(mk(
                            &format!("{}.{}", tb, nn),
                            SymbolKind::Const,
                            line,
                            end_line,
                            sig,
                            format!("renommage vers {} sur {}", nn, table),
                            &words,
                            &[table.as_str()],
                        ));
                        emitted += 1;
                    }
                }
            }
            _ => {}
        }
    }
    let _ = generic;
}

fn grant(st: &Stmt, line: u32, end_line: u32, out: &mut SqlOut) {
    // GRANT privs ON [obj-type] objets TO rôles
    let on = (1..st.t.len()).find(|&k| st.is(k, "on"));
    let to = (1..st.t.len()).find(|&k| st.is(k, "to"));
    let (privs, objects, roles): (Vec<String>, Vec<String>, Vec<String>) = match (on, to) {
        (Some(on), Some(to)) if on < to => {
            let privs = (1..on).filter_map(|k| st.w(k)).collect::<Vec<_>>();
            let mut objs = Vec::new();
            let mut x = on + 1;
            while x < to {
                if st.punct(x, b'(') {
                    // Types d'arguments d'une fonction : ne sont pas des objets.
                    x = st.close_of(x) + 1;
                    continue;
                }
                if let Some((n, nx)) = st.name(x) {
                    if !matches!(
                        n.to_ascii_lowercase().as_str(),
                        "table"
                            | "tables"
                            | "function"
                            | "functions"
                            | "schema"
                            | "sequence"
                            | "sequences"
                            | "all"
                            | "in"
                            | "type"
                            | "procedure"
                    ) {
                        objs.push(n);
                    }
                    x = nx;
                } else {
                    x += 1;
                }
            }
            let mut roles = Vec::new();
            let mut y = to + 1;
            while y < st.t.len() {
                if st.is(y, "with") {
                    break;
                }
                if let Some(r) = st.ident(y) {
                    roles.push(r);
                }
                y += 1;
            }
            (privs, objs, roles)
        }
        _ => (Vec::new(), Vec::new(), Vec::new()),
    };
    let text = st.render(0, st.t.len(), 130);
    match &mut out.grants {
        Some(g) => {
            g.end = end_line;
            g.count += 1;
            if g.lines.len() < 12 {
                g.lines.push(text);
            }
            for o in objects {
                push_uniq_cs(&mut g.objects, o);
            }
            for r in roles {
                push_uniq_cs(&mut g.roles, r);
            }
            for p in privs {
                push_uniq_cs(&mut g.privs, p);
            }
        }
        None => {
            let mut g =
                GrantRun { line, end: end_line, lines: vec![text], objects: Vec::new(), roles: Vec::new(), privs: Vec::new(), count: 1 };
            for o in objects {
                push_uniq_cs(&mut g.objects, o);
            }
            for r in roles {
                push_uniq_cs(&mut g.roles, r);
            }
            for p in privs {
                push_uniq_cs(&mut g.privs, p);
            }
            out.grants = Some(g);
        }
    }
}

fn push_uniq_cs(v: &mut Vec<String>, s: String) {
    if !s.is_empty() && !v.contains(&s) && v.len() < 100 {
        v.push(s);
    }
}

// ═════════════════════════════════ YAML ═════════════════════════════════

struct YOpen {
    indent: usize,
    path: String,
    sym: Option<usize>,
}

struct YSym {
    sym: Symbol,
    doc_idx: usize,
    children: Vec<String>,
    depth: usize,
    path: String,
}

/// Valeur scalaire (après `:`) sans commentaire de fin ni ancre/étiquette de tête.
fn yaml_value(rest: &str) -> String {
    let mut r = rest.trim();
    loop {
        if r.starts_with('&') || r.starts_with('!') {
            match r.find(char::is_whitespace) {
                Some(p) => r = r[p..].trim_start(),
                None => return String::new(),
            }
        } else {
            break;
        }
    }
    if r.starts_with('#') {
        return String::new();
    }
    // Commentaire de fin de ligne ` #…` (hors guillemets simples/doubles en tête).
    if !(r.starts_with('"') || r.starts_with('\'')) {
        if let Some(p) = r.find(" #") {
            r = r[..p].trim_end();
        }
    }
    r.to_string()
}

/// Analyse `clé: reste` ; `None` si la ligne n'est pas une clé.
fn yaml_key(content: &str) -> Option<(String, String)> {
    let c = content.trim_end();
    let first = c.chars().next()?;
    if matches!(first, '{' | '[' | '*' | '&' | '!' | '|' | '>' | '%' | '@' | '`' | '#' | '?') {
        return None;
    }
    if first == '"' || first == '\'' {
        let bytes = c.as_bytes();
        let mut j = 1;
        while j < bytes.len() {
            if bytes[j] == first as u8 {
                if first == '\'' && bytes.get(j + 1) == Some(&b'\'') {
                    j += 2;
                    continue;
                }
                break;
            }
            if first == '"' && bytes[j] == b'\\' {
                j += 2;
                continue;
            }
            j += 1;
        }
        if j >= bytes.len() {
            return None;
        }
        let key = c.get(1..j)?.to_string();
        let after = c.get(j + 1..)?.trim_start();
        let rest = after.strip_prefix(':')?;
        if !(rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t')) {
            return None;
        }
        return Some((key, rest.to_string()));
    }
    // Clé simple : jusqu'au premier `:` suivi d'un blanc ou de la fin.
    let bytes = c.as_bytes();
    let mut j = 0;
    while j < bytes.len() {
        if bytes[j] == b':' && (j + 1 == bytes.len() || bytes[j + 1] == b' ' || bytes[j + 1] == b'\t') {
            let key = c.get(..j)?.trim_end();
            if key.is_empty() || key.contains(" #") {
                return None;
            }
            return Some((key.to_string(), c.get(j + 1..)?.to_string()));
        }
        if bytes[j] == b'#' && j > 0 && bytes[j - 1] == b' ' {
            return None;
        }
        j += 1;
    }
    None
}

fn flow_balance(s: &str) -> i32 {
    let mut d = 0i32;
    let mut q: Option<char> = None;
    for c in s.chars() {
        match q {
            Some(qc) => {
                if c == qc {
                    q = None;
                }
            }
            None => match c {
                '"' | '\'' => q = Some(c),
                '[' | '{' => d += 1,
                ']' | '}' => d -= 1,
                '#' => break,
                _ => {}
            },
        }
    }
    d
}

fn extract_yaml(src: &str) -> Vec<Symbol> {
    if src.len() > MAX_YAML_BYTES || src.len() < 4 {
        return Vec::new();
    }
    let lines: Vec<&str> = src.lines().collect();
    let mut syms: Vec<YSym> = Vec::new();
    let mut stack: Vec<YOpen> = Vec::new();
    let mut doc_idx = 0usize;
    // Par document : (kind, nom metadata, apiVersion vu)
    let mut doc_kind: Vec<(Option<String>, Option<String>, bool)> = vec![(None, None, false)];
    let mut top_keys: Vec<String> = Vec::new();
    let mut top_count = 0usize;
    let mut last_content: usize = 0;
    let mut block_indent: Option<usize> = None;
    let mut seq_dash: Option<usize> = None;
    let mut flow: i32 = 0;
    let mut data_like = false;

    fn close_to(stack: &mut Vec<YOpen>, indent: usize, syms: &mut [YSym], last_content: usize) {
        while let Some(top) = stack.last() {
            if top.indent >= indent {
                if let Some(si) = top.sym {
                    syms[si].sym.end_line = (last_content as u32 + 1).max(syms[si].sym.line);
                }
                stack.pop();
            } else {
                break;
            }
        }
    }

    for (li, raw) in lines.iter().enumerate() {
        if raw.len() > 2000 {
            continue;
        }
        let line = raw.trim_end();
        let indent = line.len() - line.trim_start_matches(' ').len();
        let content = &line[indent..];
        if content.is_empty() {
            continue;
        }
        if flow > 0 {
            flow += flow_balance(content);
            last_content = li;
            continue;
        }
        if let Some(bi) = block_indent {
            if indent > bi {
                last_content = li;
                continue;
            }
            block_indent = None;
        }
        if content.starts_with('#') {
            continue;
        }
        if indent == 0 && (line == "---" || line.starts_with("--- ") || line == "...") {
            close_to(&mut stack, 0, &mut syms, last_content);
            seq_dash = None;
            if line.starts_with("---") {
                doc_idx += 1;
                doc_kind.push((None, None, false));
            }
            continue;
        }
        if let Some(d) = seq_dash {
            if content.starts_with("- ") || content == "-" || indent > d {
                last_content = li;
                continue;
            }
            seq_dash = None;
        }
        if content.starts_with("- ") || content == "-" {
            // Élément de liste : appartient à la clé ouverte ; ses clés internes sont ignorées.
            close_to(&mut stack, indent + 1, &mut syms, last_content);
            seq_dash = Some(indent);
            last_content = li;
            continue;
        }
        let Some((key, rest)) = yaml_key(content) else { continue };
        close_to(&mut stack, indent, &mut syms, last_content);
        let depth = stack.len() + 1;
        let value = yaml_value(&rest);
        let parent_path = stack.last().map(|o| o.path.clone());
        let path = match &parent_path {
            Some(p) => format!("{}.{}", p, key),
            None => key.clone(),
        };
        last_content = li;
        if depth == 1 {
            top_count += 1;
            top_keys.push(key.clone());
            if key == "lockfileVersion" || key == "__metadata" {
                return Vec::new();
            }
            if top_count > MAX_TOP_KEYS {
                data_like = true;
            }
            if key == "kind" && !value.is_empty() {
                doc_kind[doc_idx].0 = Some(value.trim_matches(|c| c == '"' || c == '\'').to_string());
            }
            if key == "apiVersion" {
                doc_kind[doc_idx].2 = true;
            }
        }
        if depth == 2 && path == "metadata.name" && !value.is_empty() {
            doc_kind[doc_idx].1 = Some(value.trim_matches(|c| c == '"' || c == '\'').to_string());
        }
        let container = value.is_empty() || value.starts_with('|') || value.starts_with('>');
        let emit = key != "<<" && depth <= MAX_YAML_DEPTH && (depth <= 2 || container) && syms.len() < MAX_SYMS_CONF;
        let mut sym_idx = None;
        if emit {
            let sig = cut(line.trim(), 150);
            let summary = if container { String::new() } else { cut(&value, 80) };
            let s = mk(&path, SymbolKind::Const, li as u32 + 1, li as u32 + 1, sig, summary, &[], &[]);
            syms.push(YSym { sym: s, doc_idx, children: Vec::new(), depth, path: path.clone() });
            sym_idx = Some(syms.len() - 1);
        }
        // Clé enfant : la remonter dans le parent émis le plus proche.
        if let Some(p) = stack.iter().rev().find_map(|o| o.sym) {
            if syms[p].children.len() < 14 {
                syms[p].children.push(key.clone());
            }
        }
        stack.push(YOpen { indent, path, sym: sym_idx });
        if value.starts_with('|') || value.starts_with('>') {
            block_indent = Some(indent);
        } else if value.starts_with('[') || value.starts_with('{') {
            flow = flow_balance(&value).max(0);
        }
    }
    close_to(&mut stack, 0, &mut syms, last_content);
    if data_like || syms.is_empty() {
        return Vec::new();
    }
    // Saveur du fichier, d'après les clés de premier niveau.
    let has = |k: &str| top_keys.iter().any(|t| t == k);
    let mut flavor: Vec<&str> = vec!["yaml", "yml"];
    if has("jobs") && (has("on") || has("true")) {
        flavor.extend_from_slice(&["workflow", "github", "actions", "ci", "pipeline"]);
    } else if has("stages") || has("script") || has("image") && has("variables") {
        flavor.extend_from_slice(&["gitlab", "ci", "pipeline"]);
    }
    if has("services") && !has("apiVersion") {
        flavor.extend_from_slice(&["docker", "compose", "service", "services", "container"]);
    }
    if has("openapi") || has("swagger") {
        flavor.extend_from_slice(&["openapi", "swagger", "api", "endpoint", "route", "schema"]);
    }
    if has("apiVersion") {
        flavor.extend_from_slice(&["kubernetes", "k8s", "manifest"]);
    }
    // Identité des documents Kubernetes : `Kind.name` préfixe les chemins.
    let ident: Vec<Option<String>> = doc_kind
        .iter()
        .map(|(k, n, api)| match (k, n, api) {
            (Some(k), Some(n), true) => Some(format!("{}.{}", k, n)),
            (Some(k), None, true) => Some(k.clone()),
            _ => None,
        })
        .collect();
    let mut out: Vec<Symbol> = Vec::new();
    let mut doc_syms: Vec<(usize, u32, u32)> = Vec::new(); // (doc, première ligne, dernière)
    for ys in syms {
        let mut s = ys.sym;
        if let Some(id) = ident.get(ys.doc_idx).and_then(|x| x.as_ref()) {
            if ys.depth == 1 && matches!(ys.path.as_str(), "kind" | "apiVersion" | "metadata") {
                continue;
            }
            if ys.path == "metadata.name" || ys.path.starts_with("metadata.") && ys.depth == 2 {
                continue;
            }
            let np = format!("{}.{}", id, ys.path);
            let n = clean_name(&np);
            s.tokens = name_tokens(&n);
            s.name = n;
        }
        // Résumé et mots d'alias.
        let mut doc: Vec<String> = Vec::new();
        for f in &flavor {
            push_uniq(&mut doc, f.to_string());
        }
        for c in &ys.children {
            for t in text_tokens(c) {
                push_uniq(&mut doc, t);
            }
        }
        if !ys.children.is_empty() {
            s.summary = format!("{} : {}", s.name, cut(&ys.children.join(", "), 100));
        } else if s.summary.is_empty() {
            s.summary = s.name.clone();
        }
        doc.truncate(MAX_COMMENT_TOKENS);
        s.doc = doc;
        if ident.get(ys.doc_idx).map(|x| x.is_some()).unwrap_or(false) {
            doc_syms.push((ys.doc_idx, s.line, s.end_line));
        }
        out.push(s);
    }
    // Un symbole par ressource Kubernetes (plage du document).
    let mut seen_doc: Vec<usize> = Vec::new();
    for (d, _, _) in doc_syms.clone() {
        if seen_doc.contains(&d) || out.len() >= MAX_SYMS_CONF + 40 {
            continue;
        }
        seen_doc.push(d);
        let first = doc_syms.iter().filter(|x| x.0 == d).map(|x| x.1).min().unwrap_or(1);
        let last = doc_syms.iter().filter(|x| x.0 == d).map(|x| x.2).max().unwrap_or(first);
        if let Some(id) = ident[d].clone() {
            let mut fl = flavor.clone();
            fl.extend_from_slice(&["resource", "ressource"]);
            let kind_word = id.split('.').next().unwrap_or("").to_ascii_lowercase();
            let mut s = mk(
                &id,
                SymbolKind::Const,
                first,
                last,
                format!("{} (Kubernetes)", id),
                format!("ressource Kubernetes {}", id),
                &fl,
                &[&kind_word],
            );
            s.line = first.saturating_sub(1).max(1);
            out.push(s);
        }
    }
    out.sort_by_key(|s| s.line);
    out.truncate(MAX_SYMS_CONF + 40);
    out
}

// ═════════════════════════════════ TOML ═════════════════════════════════

fn extract_toml(src: &str) -> Vec<Symbol> {
    if src.len() > MAX_TOML_BYTES || src.len() < 4 {
        return Vec::new();
    }
    if src.matches("[[package]]").count() >= 4 {
        return Vec::new(); // Cargo.lock, poetry.lock
    }
    struct T {
        name: String,
        line: u32,
        end: u32,
        sig: String,
        keys: Vec<String>,
        array: bool,
        label: Option<String>,
        top_scalar: bool,
        value: String,
    }
    let mut tabs: Vec<T> = Vec::new();
    let mut ml: Option<&'static str> = None;
    let mut arr_depth: i32 = 0;
    let mut last_content = 0u32;
    let mut headers = 0usize;
    for (li, raw) in src.lines().enumerate() {
        let ln = li as u32 + 1;
        if raw.len() > 4000 {
            continue;
        }
        let t = raw.trim();
        if let Some(d) = ml {
            if t.contains(d) {
                ml = None;
            }
            last_content = ln;
            continue;
        }
        if arr_depth > 0 {
            arr_depth += flow_balance(t);
            last_content = ln;
            continue;
        }
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if t.starts_with('[') {
            let arr = t.starts_with("[[");
            let inner_end = if arr { t.find("]]") } else { t.find(']') };
            if let Some(e) = inner_end {
                let inner = t[if arr { 2 } else { 1 }..e].trim();
                if !inner.is_empty() {
                    if let Some(prev) = tabs.last_mut() {
                        prev.end = last_content.max(prev.line);
                    }
                    headers += 1;
                    let name: String = inner.split('.').map(|p| p.trim()).collect::<Vec<_>>().join(".");
                    tabs.push(T {
                        name,
                        line: ln,
                        end: ln,
                        sig: cut(t, 120),
                        keys: Vec::new(),
                        array: arr,
                        label: None,
                        top_scalar: false,
                        value: String::new(),
                    });
                    last_content = ln;
                    continue;
                }
            }
        }
        // `clé = valeur`
        if let Some(eq) = t.find('=') {
            let key = t[..eq].trim().trim_matches(|c| c == '"' || c == '\'');
            let val = t[eq + 1..].trim();
            let key0 = key.split('.').next().unwrap_or(key).trim().to_string();
            if !key0.is_empty() && key0.len() < 80 {
                match tabs.last_mut().filter(|_| headers > 0) {
                    Some(cur) => {
                        if cur.keys.len() < 40 && !cur.keys.contains(&key0) {
                            cur.keys.push(key0.clone());
                        }
                        if cur.array && key == "name" && cur.label.is_none() {
                            cur.label = Some(val.trim_matches(|c| c == '"' || c == '\'').to_string());
                        }
                    }
                    None => {
                        if tabs.len() < MAX_SYMS_CONF {
                            tabs.push(T {
                                name: key0.clone(),
                                line: ln,
                                end: ln,
                                sig: cut(t, 120),
                                keys: Vec::new(),
                                array: false,
                                label: None,
                                top_scalar: true,
                                value: cut(val, 80),
                            });
                        }
                    }
                }
            }
            if val.starts_with("\"\"\"") && !val[3..].contains("\"\"\"") {
                ml = Some("\"\"\"");
            } else if val.starts_with("'''") && !val[3..].contains("'''") {
                ml = Some("'''");
            } else if val.starts_with('[') {
                arr_depth = flow_balance(val).max(0);
            }
            last_content = ln;
        }
    }
    if let Some(prev) = tabs.last_mut() {
        prev.end = last_content.max(prev.line);
    }
    // Les scalaires de tête sont tous d'une ligne.
    if tabs.is_empty() || headers > MAX_SYMS_CONF {
        return Vec::new();
    }
    let names: Vec<String> = tabs.iter().map(|t| t.name.clone()).collect();
    let has = |k: &str| names.iter().any(|n| n == k || n.starts_with(&format!("{}.", k)));
    let mut flavor: Vec<&str> = vec!["toml"];
    if has("package") || has("dependencies") || has("workspace") || has("lib") || has("features") {
        flavor.extend_from_slice(&["cargo", "crate", "manifest"]);
    }
    if has("project") || has("tool") || has("build-system") {
        flavor.extend_from_slice(&["pyproject", "python"]);
    }
    let mut out: Vec<Symbol> = Vec::new();
    for t in tabs.into_iter().take(MAX_SYMS_CONF) {
        let name = match (&t.label, t.array) {
            (Some(l), true) if !l.is_empty() => format!("{}.{}", t.name, l),
            _ => t.name.clone(),
        };
        let mut free: Vec<String> = t.keys.clone();
        free.push(t.name.clone());
        let free_ref: Vec<&str> = free.iter().map(|s| s.as_str()).collect();
        let summary = if t.top_scalar {
            format!("{} = {}", t.name, t.value)
        } else if t.keys.is_empty() {
            format!("table {}", t.name)
        } else {
            format!("table {} : {}", t.name, cut(&t.keys.join(", "), 100))
        };
        out.push(mk(&name, SymbolKind::Const, t.line, t.end, t.sig.clone(), summary, &flavor, &free_ref));
    }
    out
}

// ═════════════════════════════════ JSON ═════════════════════════════════

/// Clés dont la présence en tête d'objet indique un fichier de CONFIGURATION
/// (package.json, tsconfig, turbo, tauri…) plutôt qu'une donnée.
const JSON_CONFIG_KEYS: &[&str] = &[
    "scripts",
    "dependencies",
    "devDependencies",
    "peerDependencies",
    "compilerOptions",
    "extends",
    "include",
    "exclude",
    "references",
    "$schema",
    "workspaces",
    "exports",
    "tasks",
    "pipeline",
    "productName",
    "identifier",
    "configurations",
    "rules",
    "plugins",
    "eslintConfig",
    "engines",
    "private",
    "main",
    "bin",
    "build",
    "tauri",
    "app",
    "version",
    "files",
    "paths",
    "lib",
    "types",
];
/// Parents dont les clés de second niveau sont retenues.
const JSON_L2: &[&str] = &["scripts", "compilerOptions", "tasks", "pipeline", "engines", "bin", "workspaces"];

struct JSym {
    name: String,
    line: u32,
    end: u32,
    value: String,
    container: bool,
    depth: usize,
    is_str: bool,
}

fn extract_json(src: &str) -> Vec<Symbol> {
    let b = src.as_bytes();
    if b.len() > MAX_JSON_BYTES || b.len() < 4 {
        return Vec::new();
    }
    // JSON minifié (une longue ligne) : donnée ou artefact, pas une config éditée à la main.
    let nl = b.iter().filter(|&&c| c == b'\n').count();
    if b.len() > 20_000 && nl * 400 < b.len() {
        return Vec::new();
    }
    let mut i = if b.starts_with(&[0xEF, 0xBB, 0xBF]) { 3 } else { 0 };
    let mut line = 1u32;
    // Racine : doit être un objet.
    let skip = |i: &mut usize, line: &mut u32| {
        while *i < b.len() {
            match b[*i] {
                b'\n' => {
                    *line += 1;
                    *i += 1;
                }
                b' ' | b'\t' | b'\r' => *i += 1,
                b'/' if b.get(*i + 1) == Some(&b'/') => {
                    while *i < b.len() && b[*i] != b'\n' {
                        *i += 1;
                    }
                }
                b'/' if b.get(*i + 1) == Some(&b'*') => {
                    *i += 2;
                    while *i < b.len() && !(b[*i] == b'*' && b.get(*i + 1) == Some(&b'/')) {
                        if b[*i] == b'\n' {
                            *line += 1;
                        }
                        *i += 1;
                    }
                    *i = (*i + 2).min(b.len());
                }
                _ => break,
            }
        }
    };
    skip(&mut i, &mut line);
    if b.get(i) != Some(&b'{') {
        return Vec::new();
    }
    // Pile de conteneurs : (est_objet, nom de la clé de premier niveau parente, symbole propriétaire).
    let mut stack: Vec<(bool, Option<String>, Option<usize>)> = Vec::new();
    let mut syms: Vec<JSym> = Vec::new();
    let mut expect_key = false;
    let mut cur_key: Option<usize> = None;
    let mut top_keys: Vec<String> = Vec::new();
    while i < b.len() {
        skip(&mut i, &mut line);
        if i >= b.len() {
            break;
        }
        let c = b[i];
        match c {
            b'{' | b'[' => {
                if stack.len() >= 40 {
                    break;
                }
                let owner = cur_key.take();
                let parent_top = match stack.len() {
                    1 => owner.map(|o| syms[o].name.clone()),
                    _ => stack.last().and_then(|x| x.1.clone()),
                };
                stack.push((c == b'{', parent_top, owner));
                expect_key = c == b'{';
                i += 1;
            }
            b'}' | b']' => {
                if let Some((_, _, Some(o))) = stack.pop() {
                    syms[o].end = line;
                }
                expect_key = false;
                i += 1;
                if stack.is_empty() {
                    break;
                }
            }
            b',' => {
                expect_key = matches!(stack.last(), Some((true, _, _)));
                cur_key = None;
                i += 1;
            }
            b':' => {
                expect_key = false;
                i += 1;
            }
            b'"' => {
                let sline = line;
                let s0 = i;
                i += 1;
                while i < b.len() {
                    if b[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if b[i] == b'"' {
                        i += 1;
                        break;
                    }
                    if b[i] == b'\n' {
                        line += 1;
                    }
                    i += 1;
                }
                let end = i.min(b.len());
                let text = lossy(&b[(s0 + 1).min(end)..end.saturating_sub(1).max((s0 + 1).min(end))]);
                if expect_key && matches!(stack.last(), Some((true, _, _))) {
                    let depth = stack.len();
                    let parent = stack.last().and_then(|x| x.1.clone());
                    let keep = depth == 1 || (depth == 2 && parent.as_deref().map(|p| JSON_L2.contains(&p)).unwrap_or(false));
                    if depth == 1 {
                        top_keys.push(text.clone());
                    }
                    if keep && syms.len() < MAX_SYMS_CONF {
                        let name = if depth == 1 { text.clone() } else { format!("{}.{}", parent.unwrap_or_default(), text) };
                        syms.push(JSym { name, line: sline, end: sline, value: String::new(), container: false, depth, is_str: false });
                        cur_key = Some(syms.len() - 1);
                    } else {
                        cur_key = None;
                    }
                    expect_key = false;
                } else if let Some(k) = cur_key {
                    syms[k].value = cut(&text, 100);
                    syms[k].is_str = true;
                    syms[k].end = line;
                }
            }
            _ => {
                // nombre, true, false, null
                let s0 = i;
                while i < b.len() && !matches!(b[i], b',' | b'}' | b']' | b'\n' | b' ' | b'\t' | b'\r' | b':') {
                    i += 1;
                }
                if let Some(k) = cur_key {
                    syms[k].value = cut(&lossy(&b[s0..i]), 40);
                    syms[k].end = line;
                }
                if i == s0 {
                    i += 1;
                }
            }
        }
        if top_keys.len() > MAX_TOP_KEYS {
            return Vec::new();
        }
    }
    // Conteneurs : le symbole propriétaire a reçu sa fin à la fermeture ; marquer.
    // (Un symbole sans valeur scalaire est un conteneur.)
    for s in syms.iter_mut() {
        s.container = s.value.is_empty();
    }
    if top_keys.iter().any(|k| k == "lockfileVersion") {
        return Vec::new();
    }
    if !top_keys.iter().any(|k| JSON_CONFIG_KEYS.contains(&k.as_str())) {
        return Vec::new();
    }
    let has = |k: &str| top_keys.iter().any(|t| t == k);
    let mut flavor: Vec<&str> = vec!["json"];
    if has("scripts") || has("dependencies") || has("devDependencies") {
        flavor.extend_from_slice(&["package", "npm", "node"]);
    }
    if has("compilerOptions") {
        flavor.extend_from_slice(&["tsconfig", "typescript", "compiler"]);
    }
    let mut out: Vec<Symbol> = Vec::new();
    for s in syms {
        let sig = if s.container {
            format!("\"{}\": {}", s.name.rsplit('.').next().unwrap_or(&s.name), if s.end > s.line { "{…}" } else { "{}" })
        } else {
            let q = if s.is_str { "\"" } else { "" };
            format!("\"{}\": {}{}{}", s.name.rsplit('.').next().unwrap_or(&s.name), q, s.value, q)
        };
        let summary = if s.container { s.name.clone() } else { format!("{} = {}", s.name, cut(&s.value, 80)) };
        let mut w2 = flavor.clone();
        if s.depth == 2 && s.name.starts_with("scripts.") {
            w2.extend_from_slice(&["script", "scripts", "commande", "command"]);
        }
        let free: Vec<&str> = vec![s.value.as_str()];
        out.push(mk(&s.name, SymbolKind::Const, s.line, s.end, sig, summary, &w2, &free));
    }
    out
}

// ═════════════════════════ commentaires (doc, en-tête) ═════════════════════════

/// Doc-comments au-dessus de chaque symbole (`--` SQL, `#` YAML/TOML) et en-tête
/// de fichier (SQL seulement : le rôle de la migration). Les symboles gardent leur
/// résumé structuré, le commentaire s'y ajoute. Renvoie (jetons d'en-tête, rôle).
pub fn comments(lang: Lang, source: &str, symbols: &mut [Symbol]) -> (Vec<String>, String) {
    let lines: Vec<&str> = source.lines().collect();
    let hash = matches!(lang, Lang::Yaml | Lang::Toml);
    let dash = lang == Lang::Sql;
    let is_c = |t: &str| -> bool {
        if dash {
            t.starts_with("--") || t.starts_with("/*") || t.starts_with('*') || t.starts_with("*/")
        } else if hash {
            t.starts_with('#')
        } else {
            false
        }
    };
    let strip = |t: &str| -> String {
        let t = t.trim();
        let t = if dash {
            t.trim_start_matches('-').trim_start_matches("/*").trim_start_matches('*').trim_end_matches("*/")
        } else {
            t.trim_start_matches('#')
        };
        t.trim().to_string()
    };
    if !(dash || hash) {
        return (Vec::new(), String::new());
    }
    for s in symbols.iter_mut() {
        let idx = s.line as usize;
        if idx == 0 || idx > lines.len() {
            continue;
        }
        let mut i = idx - 1;
        let mut got: Vec<String> = Vec::new();
        while i > 0 && got.len() < 12 {
            let t = lines[i - 1].trim();
            if t.is_empty() || !is_c(t) {
                break;
            }
            let c = strip(t);
            if !c.is_empty() && !c.chars().all(|ch| ch == '-' || ch == '=' || ch == '*' || ch == '#') {
                got.push(c);
            }
            i -= 1;
        }
        if got.is_empty() {
            continue;
        }
        got.reverse();
        let text = got.join(" ");
        for t in text_tokens(&text) {
            if !s.doc.contains(&t) && s.doc.len() < MAX_COMMENT_TOKENS {
                s.doc.push(t);
            }
        }
        let first = crate::symbol::summary_of(&text);
        if !first.is_empty() {
            s.summary = if s.summary.is_empty() { first } else { format!("{} — {}", s.summary, first) };
        }
    }
    if !dash {
        return (Vec::new(), String::new());
    }
    // En-tête SQL : bloc de commentaires avant la première instruction.
    let mut head: Vec<String> = Vec::new();
    let mut in_block = false;
    for raw in lines.iter().take(60) {
        let t = raw.trim();
        if in_block {
            head.push(strip(t));
            if t.contains("*/") {
                in_block = false;
            }
            continue;
        }
        if t.is_empty() {
            continue;
        }
        if t.starts_with("/*") {
            head.push(strip(t));
            if !t.contains("*/") {
                in_block = true;
            }
            continue;
        }
        if t.starts_with("--") {
            let c = strip(t);
            if !c.is_empty() && !c.chars().all(|ch| ch == '-' || ch == '=') {
                head.push(c);
            }
            continue;
        }
        break;
    }
    let text = head.join(" ");
    (text_tokens(&text), crate::symbol::summary_of(&text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn penalite_config_mentions_et_chemins() {
        assert!(is_config_path("supabase/migrations/001.SQL"));
        assert!(is_config_path(".github/workflows/ci.yml"));
        assert!(!is_config_path("src/flask/app.py"));
        assert!(query_mentions_config("Quelle policy protège la table files ?"));
        assert!(query_mentions_config("which workflow runs the tests"));
        assert!(!query_mentions_config("where does Flask sign the session cookie"));
    }

    fn sql(src: &str) -> Vec<Symbol> {
        extract(Lang::Sql, src)
    }
    fn names(v: &[Symbol]) -> Vec<String> {
        v.iter().map(|s| s.name.clone()).collect()
    }
    fn find<'a>(v: &'a [Symbol], name: &str) -> &'a Symbol {
        v.iter().find(|s| s.name == name).unwrap_or_else(|| panic!("symbole {} absent de {:?}", name, names(v)))
    }

    const TABLE: &str = "-- Table des fichiers du cloud\nCREATE TABLE IF NOT EXISTS public.doc_files (\n  id uuid PRIMARY KEY DEFAULT gen_random_uuid(),\n  owner_id uuid NOT NULL REFERENCES auth.users(id) ON DELETE CASCADE,\n  name text NOT NULL,\n  size_bytes bigint DEFAULT 0,\n  created_at timestamptz DEFAULT now(),\n  CONSTRAINT doc_files_name_len CHECK (char_length(name) < 256)\n);\n";

    #[test]
    fn create_table_nom_qualifie_et_colonnes() {
        let s = sql(TABLE);
        assert_eq!(s.len(), 1);
        let t = &s[0];
        assert_eq!(t.name, "public.doc_files");
        assert_eq!(t.kind, SymbolKind::Struct);
        assert_eq!(t.line, 2);
        assert_eq!(t.end_line, 9);
        assert!(t.signature.starts_with("TABLE public.doc_files\n"));
        assert!(t.signature.contains("\n  owner_id uuid NOT NULL REFERENCES auth.users(id) ON DELETE CASCADE"));
        assert!(t.signature.contains("size_bytes bigint DEFAULT 0"));
        assert!(t.signature.contains("CONSTRAINT doc_files_name_len CHECK"));
        assert!(t.summary.starts_with("table public.doc_files — 5 colonnes : id, owner_id"));
    }

    #[test]
    fn table_alias_de_recherche() {
        let s = sql(TABLE);
        let t = &s[0];
        for w in ["table", "colonne", "column", "migration", "size_bytes", "owner_id", "users"] {
            assert!(t.doc.iter().any(|d| d == w), "alias {} absent de {:?}", w, t.doc);
        }
        // Les jetons de nom portent le nom nu (doc_files) mais pas le schéma public.
        assert!(t.tokens.contains(&"doc_files".to_string()));
        assert!(t.tokens.contains(&"files".to_string()));
        assert!(!t.tokens.contains(&"public".to_string()));
    }

    #[test]
    fn table_sans_schema_et_guillemets() {
        let s = sql("CREATE TABLE \"Weird Name\" (a int, \"b c\" text);\nCREATE TEMP TABLE IF NOT EXISTS tmp_x (a int);");
        assert_eq!(names(&s), vec!["Weird_Name", "tmp_x"]);
        assert!(s[0].signature.contains("\"b c\" text") || s[0].signature.contains("b c text"));
    }

    #[test]
    fn alter_add_column_et_rls() {
        let src = "ALTER TABLE public.doc_files ADD COLUMN IF NOT EXISTS deleted_at timestamptz, ADD COLUMN thumb_path text;\nALTER TABLE public.doc_files ENABLE ROW LEVEL SECURITY;\nALTER TABLE ONLY doc_files FORCE ROW LEVEL SECURITY;";
        let s = sql(src);
        assert_eq!(names(&s), vec!["doc_files.deleted_at", "doc_files.thumb_path", "doc_files.rls", "doc_files.rls"]);
        let c = find(&s, "doc_files.deleted_at");
        assert_eq!(c.line, 1);
        assert!(c.signature.contains("ADD COLUMN deleted_at timestamptz"));
        assert!(c.doc.iter().any(|d| d == "colonne"));
        let r = find(&s, "doc_files.rls");
        assert!(r.doc.iter().any(|d| d == "rls") && r.doc.iter().any(|d| d == "politique"));
        assert_eq!(r.line, 2);
    }

    #[test]
    fn alter_add_constraint_drop_rename() {
        let src = "ALTER TABLE t ADD CONSTRAINT t_fk FOREIGN KEY (a) REFERENCES u(id);\nALTER TABLE t DROP COLUMN IF EXISTS old_col;\nALTER TABLE t RENAME COLUMN a TO b;\nALTER TABLE t OWNER TO postgres;";
        let s = sql(src);
        assert_eq!(names(&s), vec!["t.t_fk", "t.old_col", "t.b"]);
    }

    #[test]
    fn policy_multiligne() {
        let src = "CREATE POLICY \"Users can view own files\"\n  ON public.doc_files\n  FOR SELECT\n  TO authenticated\n  USING (\n    owner_id = auth.uid()\n    OR public.user_has_access(id)\n  );\n";
        let s = sql(src);
        assert_eq!(s.len(), 1);
        let p = &s[0];
        assert_eq!(p.name, "doc_files.Users_can_view_own_files");
        assert_eq!(p.line, 1);
        assert_eq!(p.end_line, 8);
        assert!(p.signature.contains("POLICY \"Users can view own files\" ON public.doc_files FOR SELECT TO authenticated USING ("));
        assert!(p.signature.contains("owner_id = auth.uid()"));
        for w in ["policy", "politique", "rls", "select", "authenticated", "files"] {
            assert!(p.doc.iter().any(|d| d == w), "{} absent de {:?}", w, p.doc);
        }
    }

    #[test]
    fn policy_with_check_et_all() {
        let src = "CREATE POLICY p_ins ON t FOR INSERT TO authenticated, service_role WITH CHECK (user_id = auth.uid());\nCREATE POLICY p_all ON t AS RESTRICTIVE USING (true);";
        let s = sql(src);
        assert_eq!(names(&s), vec!["t.p_ins", "t.p_all"]);
        assert!(s[0].signature.contains("TO authenticated, service_role"));
        assert!(s[0].signature.contains("WITH CHECK (user_id = auth.uid())"));
        assert!(s[1].signature.contains("AS RESTRICTIVE FOR ALL"));
    }

    #[test]
    fn fonction_avec_corps_dollar() {
        let src = "CREATE OR REPLACE FUNCTION public.list_children(p_folder uuid, p_limit int DEFAULT 50)\nRETURNS TABLE(id uuid, name text)\nLANGUAGE plpgsql\nSECURITY DEFINER\nAS $$\nBEGIN\n  RETURN QUERY SELECT 1; -- un ; dans le corps\nEND;\n$$;\nSELECT 1;";
        let s = sql(src);
        assert_eq!(s.len(), 1);
        let f = &s[0];
        assert_eq!(f.name, "public.list_children");
        assert_eq!(f.kind, SymbolKind::Function);
        assert_eq!((f.line, f.end_line), (1, 9));
        assert!(f.signature.contains("(p_folder uuid, p_limit int DEFAULT 50)"));
        assert!(f.signature.contains("RETURNS TABLE(id uuid, name text)"));
        assert!(f.signature.contains("LANGUAGE plpgsql"));
        assert!(f.signature.contains("SECURITY DEFINER"));
        assert!(f.doc.iter().any(|d| d == "p_folder") && f.doc.iter().any(|d| d == "fonction"));
    }

    #[test]
    fn dollar_tague_imbrique() {
        let src = "CREATE FUNCTION a() RETURNS void AS $outer$\nBEGIN\n  EXECUTE $inner$ select 1; $inner$;\n  RAISE NOTICE '$other$ pas une fin';\nEND;\n$outer$ LANGUAGE plpgsql;\nCREATE FUNCTION b() RETURNS int AS $$ select 2 $$ LANGUAGE sql;";
        let s = sql(src);
        assert_eq!(names(&s), vec!["a", "b"]);
        assert_eq!(s[0].end_line, 6);
        assert_eq!(s[1].line, 7);
    }

    #[test]
    fn chaines_avec_point_virgule_et_apostrophes() {
        let src = "CREATE TABLE t (a text DEFAULT 'x;y', b text DEFAULT 'it''s; ok');\nCREATE TABLE u (c int);";
        let s = sql(src);
        assert_eq!(names(&s), vec!["t", "u"]);
        assert!(s[0].signature.contains("'x;y'"));
        assert_eq!(s[1].line, 2);
    }

    #[test]
    fn commentaires_ligne_et_bloc_imbriques() {
        let src = "-- CREATE TABLE fantome (a int);\n/* CREATE TABLE autre (b int); /* imbrique ; */ encore ; */\nCREATE TABLE reel (a int); -- fin ; de ligne\n";
        let s = sql(src);
        assert_eq!(names(&s), vec!["reel"]);
        assert_eq!(s[0].line, 3);
    }

    #[test]
    fn identifiant_guillemete_avec_point_virgule() {
        let src = "CREATE TABLE \"a;b\" (id int);\nCREATE TABLE c (id int);";
        let s = sql(src);
        assert_eq!(names(&s), vec!["a;b", "c"]);
    }

    #[test]
    fn trigger_et_index() {
        let src = "CREATE TRIGGER trg_touch\n  BEFORE UPDATE ON public.doc_files\n  FOR EACH ROW EXECUTE FUNCTION public.touch_updated_at();\nCREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS idx_doc_files_owner_name ON public.doc_files (owner_id, name) WHERE deleted_at IS NULL;\nCREATE INDEX ON t USING gin (tags);";
        let s = sql(src);
        assert_eq!(s.len(), 3);
        let tr = find(&s, "trg_touch");
        assert_eq!(tr.kind, SymbolKind::Function);
        assert!(tr.signature.contains("BEFORE UPDATE ON public.doc_files"));
        assert!(tr.signature.contains("EXECUTE FUNCTION public.touch_updated_at()"));
        let ix = find(&s, "idx_doc_files_owner_name");
        assert!(ix
            .signature
            .contains("CREATE UNIQUE INDEX idx_doc_files_owner_name ON public.doc_files (owner_id, name) WHERE deleted_at IS NULL"));
        assert!(ix.doc.iter().any(|d| d == "index") && ix.doc.iter().any(|d| d == "unique"));
        assert_eq!(s[2].name, "t_tags_idx");
    }

    #[test]
    fn vues_et_vue_materialisee() {
        let src = "CREATE OR REPLACE VIEW public.v_files AS\n  SELECT f.id, f.name FROM public.doc_files f JOIN public.profiles p ON p.id = f.owner_id;\nCREATE MATERIALIZED VIEW IF NOT EXISTS mv_stats AS SELECT count(*) FROM doc_files;";
        let s = sql(src);
        assert_eq!(names(&s), vec!["public.v_files", "mv_stats"]);
        assert_eq!(s[0].kind, SymbolKind::Struct);
        assert!(s[0].summary.contains("sur doc_files, profiles"));
        assert!(s[1].signature.starts_with("MATERIALIZED VIEW mv_stats"));
        assert!(s[1].doc.iter().any(|d| d == "materialized"));
    }

    #[test]
    fn type_enum_extension_schema() {
        let src = "CREATE EXTENSION IF NOT EXISTS \"uuid-ossp\" WITH SCHEMA extensions;\nCREATE SCHEMA IF NOT EXISTS billing AUTHORIZATION postgres;\nCREATE TYPE public.plan_kind AS ENUM ('free', 'pro', 'team');\nALTER TYPE public.plan_kind ADD VALUE IF NOT EXISTS 'enterprise';";
        let s = sql(src);
        assert_eq!(names(&s), vec!["uuid-ossp", "billing", "public.plan_kind", "plan_kind.enterprise"]);
        assert_eq!(s[2].kind, SymbolKind::Enum);
        assert!(s[2].signature.contains("AS ENUM ('free', 'pro', 'team')"));
        assert!(s[0].signature.contains("WITH SCHEMA extensions"));
    }

    #[test]
    fn grants_agreges() {
        let src = "GRANT USAGE ON SCHEMA public TO anon, authenticated;\nGRANT SELECT, INSERT ON public.doc_files TO authenticated;\nGRANT EXECUTE ON FUNCTION public.list_children(uuid) TO authenticated;\nCREATE TABLE z (a int);\nGRANT ALL ON z TO service_role;";
        let s = sql(src);
        assert_eq!(s.len(), 3, "{:?}", names(&s));
        assert!(s[0].name.starts_with("grants."));
        assert_eq!((s[0].line, s[0].end_line), (1, 3));
        assert!(s[0].doc.iter().any(|d| d == "grant") && s[0].doc.iter().any(|d| d == "authenticated"));
        assert_eq!(s[1].name, "z");
        assert_eq!(s[2].line, 5);
    }

    #[test]
    fn homonymes_numerotes_par_ids() {
        let src = "CREATE OR REPLACE FUNCTION f() RETURNS int AS $$ select 1 $$ LANGUAGE sql;\nCREATE OR REPLACE FUNCTION f() RETURNS int AS $$ select 2 $$ LANGUAGE sql;";
        let s = sql(src);
        assert_eq!(names(&s), vec!["f", "f"]);
        let ranks = crate::ids::ranks(s.iter().map(|x| (x.name.as_str(), x.kind)));
        assert_eq!(ranks, vec![1, 2]);
        assert_eq!(crate::ids::sym_id("m.sql", "f", SymbolKind::Function, 2), "S:m.sql#f~2");
    }

    #[test]
    fn begin_atomic_et_trigger_sqlite() {
        let src = "CREATE FUNCTION g() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT CASE WHEN true THEN 1 ELSE 2 END; END;\nCREATE TRIGGER tr AFTER INSERT ON t BEGIN UPDATE t SET a = 1; DELETE FROM u; END;\nCREATE TABLE after_ (a int);";
        let s = sql(src);
        assert_eq!(names(&s), vec!["g", "tr", "after_"]);
        assert_eq!(s[2].line, 3);
    }

    #[test]
    fn delimiter_mysql() {
        let src = "DELIMITER //\nCREATE PROCEDURE p()\nBEGIN\n  SELECT 1;\n  IF x THEN SELECT 2; END IF;\nEND//\nDELIMITER ;\nCREATE TABLE after_p (a int);";
        let s = sql(src);
        assert_eq!(names(&s), vec!["p", "after_p"]);
        assert_eq!(s[0].end_line, 6);
    }

    #[test]
    fn mysql_definer_et_backticks() {
        let src = "CREATE DEFINER=`root`@`localhost` PROCEDURE `proc_x`() SELECT 1;\nCREATE TABLE `users` (`id` int NOT NULL AUTO_INCREMENT, `name` varchar(255), PRIMARY KEY (`id`)) ENGINE=InnoDB;";
        let s = sql(src);
        assert_eq!(names(&s), vec!["proc_x", "users"]);
        assert!(s[1].signature.contains("id int NOT NULL AUTO_INCREMENT") || s[1].signature.contains("`id` int NOT NULL AUTO_INCREMENT"));
    }

    #[test]
    fn copy_stdin_donnees_ignorees() {
        let src = "CREATE TABLE a (x text);\nCOPY a (x) FROM stdin;\nfoo ' bar\nCREATE TABLE faux (z int);\n\\.\nCREATE TABLE b (y int);";
        let s = sql(src);
        assert_eq!(names(&s), vec!["a", "b"]);
    }

    #[test]
    fn insert_et_autres_ignores() {
        let src =
            "INSERT INTO t VALUES ('CREATE TABLE faux (a int);');\nSET search_path = public;\nBEGIN;\nCREATE TABLE ok (a int);\nCOMMIT;";
        let s = sql(src);
        assert_eq!(names(&s), vec!["ok"]);
        assert_eq!(s[0].line, 4);
    }

    #[test]
    fn fichier_tronque_jamais_de_panique() {
        let full =
            format!("{}\nCREATE POLICY \"x\" ON t USING (a = 1);\nCREATE OR REPLACE FUNCTION h() RETURNS int AS $$ select 1 $$;", TABLE);
        for n in 0..full.len() {
            if !full.is_char_boundary(n) {
                continue;
            }
            let _ = sql(&full[..n]);
        }
        // Chaîne, commentaire, dollar et guillemets non fermés.
        for src in [
            "CREATE TABLE t (a text DEFAULT 'abc",
            "/* jamais fermé CREATE TABLE a (b int);",
            "CREATE FUNCTION f() AS $x$ jamais",
            "CREATE TABLE \"abc (a int);",
            "CREATE POLICY",
            "ALTER TABLE",
            "CREATE TABLE t (",
            "GRANT",
            "CREATE INDEX ON",
            "DELIMITER",
            "CREATE TRIGGER x BEGIN",
        ] {
            let _ = sql(src);
        }
    }

    #[test]
    fn octets_invalides_et_unicode() {
        let s = sql("CREATE TABLE tâche_é (nom text); -- été\nCREATE TABLE b (é int);");
        assert_eq!(names(&s), vec!["tâche_é", "b"]);
    }

    #[test]
    fn gros_fichier_ignore() {
        let big = "CREATE TABLE t (a int);\n".to_string();
        assert!(!sql(&big).is_empty());
        let huge = format!("{}{}", "-- x\n".repeat(MAX_SQL_BYTES / 5 + 10), "CREATE TABLE t (a int);");
        assert!(sql(&huge).is_empty());
    }

    #[test]
    fn plafond_de_symboles() {
        let src = "CREATE TABLE t (a int);\n".repeat(MAX_SYMS_SQL + 500);
        assert_eq!(sql(&src).len(), MAX_SYMS_SQL);
    }

    #[test]
    fn commentaire_au_dessus_devient_doc_et_resume() {
        let src =
            "-- Migration: partage des dossiers avec les employés\n-- Ajoute la colonne de partage\nCREATE TABLE shared_pool (id int);\n";
        let mut s = sql(src);
        let (head, role) = comments(Lang::Sql, src, &mut s);
        assert!(s[0].doc.iter().any(|d| d == "partage"));
        assert!(s[0].summary.contains("table shared_pool"));
        assert!(s[0].summary.contains("Migration"));
        assert!(head.iter().any(|h| h == "employes"));
        assert!(role.contains("Migration"));
    }

    // ───────────── YAML ─────────────

    fn yaml(src: &str) -> Vec<Symbol> {
        extract(Lang::Yaml, src)
    }

    const WORKFLOW: &str = "name: CI\non:\n  push:\n    branches: [main]\n  pull_request:\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - name: Build\n        run: |\n          cargo build\n          echo done\n  test:\n    needs: build\n    steps:\n      - run: cargo test\n";

    #[test]
    fn yaml_workflow_github() {
        let s = yaml(WORKFLOW);
        let n = names(&s);
        for want in ["name", "on", "on.push", "on.pull_request", "jobs", "jobs.build", "jobs.build.steps", "jobs.test", "jobs.test.steps"] {
            assert!(n.contains(&want.to_string()), "{} absent de {:?}", want, n);
        }
        // Les scalaires de profondeur 3 (runs-on, needs) ne sont pas retenus ; ni les clés d'éléments de liste.
        assert!(!n.contains(&"jobs.build.runs-on".to_string()));
        assert!(!n.iter().any(|x| x.contains("cargo")));
        let st = find(&s, "jobs.build.steps");
        assert_eq!(st.line, 9);
        assert_eq!(st.end_line, 14);
        assert!(st.doc.iter().any(|d| d == "workflow"));
        let b = find(&s, "jobs.build");
        assert!(b.summary.contains("runs-on") && b.summary.contains("steps"));
    }

    #[test]
    fn yaml_compose() {
        let src = "version: '3.8'\nservices:\n  web:\n    image: nginx\n    ports:\n      - \"80:80\"\n    environment:\n      A: b\n  db:\n    image: postgres\nvolumes:\n  data:\n";
        let s = yaml(src);
        let n = names(&s);
        assert!(
            n.contains(&"services.web".to_string())
                && n.contains(&"services.web.ports".to_string())
                && n.contains(&"services.db".to_string())
        );
        assert!(n.contains(&"volumes.data".to_string()));
        assert!(find(&s, "services.web").doc.iter().any(|d| d == "docker"));
    }

    #[test]
    fn yaml_openapi() {
        let src = "openapi: 3.0.0\ninfo:\n  title: T\npaths:\n  /users/{id}:\n    get:\n      summary: x\n    delete:\n      summary: y\ncomponents:\n  schemas:\n    User:\n      type: object\n";
        let s = yaml(src);
        let n = names(&s);
        assert!(n.contains(&"paths./users/{id}".to_string()), "{:?}", n);
        assert!(n.contains(&"paths./users/{id}.get".to_string()));
        assert!(n.contains(&"components.schemas".to_string()));
        assert!(n.contains(&"components.schemas.User".to_string()));
        assert!(find(&s, "paths./users/{id}.get").doc.iter().any(|d| d == "openapi"));
    }

    #[test]
    fn yaml_kubernetes_multi_documents() {
        let src = "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: web\nspec:\n  replicas: 2\n  template:\n    spec: {}\n---\napiVersion: v1\nkind: Service\nmetadata:\n  name: web-svc\nspec:\n  ports:\n    - port: 80\n";
        let s = yaml(src);
        let n = names(&s);
        assert!(n.contains(&"Deployment.web".to_string()), "{:?}", n);
        assert!(n.contains(&"Service.web-svc".to_string()));
        assert!(n.contains(&"Deployment.web.spec".to_string()));
        assert!(n.contains(&"Service.web-svc.spec.ports".to_string()), "{:?}", n);
        assert!(find(&s, "Deployment.web").doc.iter().any(|d| d == "kubernetes"));
    }

    #[test]
    fn yaml_plafonds_et_lockfile() {
        let mut big = String::new();
        for i in 0..400 {
            big.push_str(&format!("k{}:\n  a: 1\n", i));
        }
        assert!(yaml(&big).is_empty(), "plus de {} clés de premier niveau = donnée", MAX_TOP_KEYS);
        assert!(yaml("lockfileVersion: '9.0'\nimporters:\n  .:\n    dependencies:\n").is_empty());
        let mut many = String::from("root:\n");
        for i in 0..400 {
            many.push_str(&format!("  c{}:\n    x:\n      - a\n", i));
        }
        assert!(yaml(&many).len() <= MAX_SYMS_CONF + 40);
    }

    #[test]
    fn yaml_robuste() {
        for src in [
            "a: [1,\n  2",
            "k: |\n  texte\n  k2: x\nz: 1\n",
            "\"a: b\nc:",
            "- a\n- b: c\n",
            ": x\n",
            "a:\n\tb: 1\n",
            "k: &a\n  x: 1\nm:\n  <<: *a\n",
        ] {
            let _ = yaml(src);
        }
        let s = yaml("k: |\n  texte: pas une clé\n  k2: x\nz: 1\n");
        assert_eq!(names(&s), vec!["k", "z"]);
        let s = yaml("a: 1 # commentaire\n# x: 2\nb: 'x: y'\n");
        assert_eq!(names(&s), vec!["a", "b"]);
    }

    #[test]
    fn yaml_liste_au_meme_niveau_que_la_cle() {
        let s = yaml("steps:\n- run: a\n- run: b\nnext:\n  x: 1\n");
        assert_eq!(names(&s), vec!["steps", "next", "next.x"]);
    }

    // ───────────── TOML ─────────────

    fn toml(src: &str) -> Vec<Symbol> {
        extract(Lang::Toml, src)
    }

    #[test]
    fn toml_cargo() {
        let src = "[package]\nname = \"cortex\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = { version = \"1\", features = [\"derive\"] }\ntokio = \"1\"\n\n[features]\nweb = [\n  \"dep:x\",\n  \"dep:y\",\n]\n\n[[bin]]\nname = \"cortex\"\npath = \"src/main.rs\"\n\n[target.'cfg(windows)'.dependencies]\nwinapi = \"0.3\"\n";
        let s = toml(src);
        let n = names(&s);
        assert_eq!(n, vec!["package", "dependencies", "features", "bin.cortex", "target.'cfg(windows)'.dependencies"], "{:?}", n);
        let d = find(&s, "dependencies");
        assert_eq!((d.line, d.end_line), (5, 7));
        assert!(d.summary.contains("serde") && d.summary.contains("tokio"));
        assert!(d.doc.iter().any(|w| w == "cargo") && d.doc.iter().any(|w| w == "tokio"));
        assert_eq!(find(&s, "features").end_line, 13);
    }

    #[test]
    fn toml_cles_de_premier_niveau_et_chaines_multilignes() {
        let src = "title = \"x\"\nowner = 1\n\n[a]\ntext = \"\"\"\n[faux.header]\n\"\"\"\nk = 1\n[b.c]\n";
        let s = toml(src);
        assert_eq!(names(&s), vec!["title", "owner", "a", "b.c"]);
    }

    #[test]
    fn toml_lockfile_ignore() {
        let mut lock = String::from("version = 3\n");
        for i in 0..10 {
            lock.push_str(&format!("[[package]]\nname = \"p{}\"\nversion = \"1\"\n", i));
        }
        assert!(toml(&lock).is_empty());
    }

    #[test]
    fn toml_robuste() {
        for src in ["[", "[[", "[a", "x = [", "x = \"\"\"abc", "= 3\n[a.]\n", "[ ]\n"] {
            let _ = toml(src);
        }
    }

    // ───────────── JSON ─────────────

    fn json(src: &str) -> Vec<Symbol> {
        extract(Lang::Json, src)
    }

    #[test]
    fn json_package() {
        let src = "{\n  \"name\": \"app\",\n  \"version\": \"1.0.0\",\n  \"scripts\": {\n    \"dev\": \"vite\",\n    \"build\": \"vite build\",\n    \"test\": \"vitest run\"\n  },\n  \"dependencies\": {\n    \"react\": \"^18\"\n  }\n}\n";
        let s = json(src);
        let n = names(&s);
        assert_eq!(n, vec!["name", "version", "scripts", "scripts.dev", "scripts.build", "scripts.test", "dependencies"]);
        let sc = find(&s, "scripts");
        assert_eq!((sc.line, sc.end_line), (4, 8));
        let b = find(&s, "scripts.build");
        assert_eq!(b.line, 6);
        assert!(b.signature.contains("\"build\": \"vite build\""));
        assert!(b.doc.iter().any(|d| d == "npm") && b.doc.iter().any(|d| d == "script"));
    }

    #[test]
    fn json_tsconfig_avec_commentaires() {
        let src = "{\n  // base\n  \"extends\": \"./base.json\",\n  /* options */\n  \"compilerOptions\": {\n    \"target\": \"es2022\",\n    \"paths\": { \"@/*\": [\"src/*\"] },\n  },\n  \"include\": [\"src\"],\n}\n";
        let s = json(src);
        assert_eq!(names(&s), vec!["extends", "compilerOptions", "compilerOptions.target", "compilerOptions.paths", "include"]);
        assert!(find(&s, "compilerOptions").doc.iter().any(|d| d == "tsconfig"));
    }

    #[test]
    fn json_donnees_et_generes_ignores() {
        assert!(json("[1,2,3]").is_empty());
        assert!(json("{\"lockfileVersion\": 3, \"packages\": {}, \"name\": \"x\"}").is_empty());
        assert!(json("{\"common\": {\"ok\": \"OK\"}, \"login\": {\"title\": \"T\"}}").is_empty(), "i18n = donnée");
        let big = format!("{{\"name\": \"x\", \"data\": \"{}\"}}", "a".repeat(MAX_JSON_BYTES));
        assert!(json(&big).is_empty());
        let min = format!("{{\"scripts\": {{{}}}}}", (0..3000).map(|i| format!("\"k{}\":\"v\"", i)).collect::<Vec<_>>().join(","));
        assert!(json(&min).is_empty(), "minifié");
        let mut many = String::from("{\"name\": \"x\",\n");
        for i in 0..200 {
            many.push_str(&format!("\"k{}\": 1,\n", i));
        }
        many.push_str("\"z\": 2}");
        assert!(json(&many).is_empty(), "trop de clés de premier niveau");
    }

    #[test]
    fn json_robuste() {
        for src in [
            "{",
            "{\"a\"",
            "{\"scripts\": {\"a\": \"b",
            "{\"a\": [1, {",
            "{\"a\": \"\\",
            "\u{feff}{\"scripts\": {}}",
            "{,,}",
            "{\"a\": }",
            "{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{",
        ] {
            let _ = json(src);
        }
        let s = json("\u{feff}{\"scripts\": {\"a\": \"b\"}}");
        assert_eq!(names(&s), vec!["scripts", "scripts.a"]);
    }

    #[test]
    fn langages_non_geres_et_noms_sans_espaces() {
        assert!(extract(Lang::Rust, "fn main() {}").is_empty());
        let s = sql("CREATE POLICY \"a b  c\" ON t USING (true);");
        assert_eq!(s[0].name, "t.a_b_c");
        assert!(!s[0].name.contains(' ') && !s[0].name.contains('#'));
    }

    #[test]
    fn migration_reelle_cloud_style() {
        // Extrait anonymisé d'une migration réelle (RLS Cloud OS).
        let src = "-- Isoler les espaces employés (Cloud OS)\nBEGIN;\n\nALTER TABLE public.doc_nodes ENABLE ROW LEVEL SECURITY;\n\nDROP POLICY IF EXISTS doc_nodes_select ON public.doc_nodes;\nCREATE POLICY doc_nodes_select ON public.doc_nodes\n  FOR SELECT TO authenticated\n  USING (\n    owner_id = (SELECT auth.uid())\n    OR EXISTS (SELECT 1 FROM public.doc_shares s WHERE s.node_id = doc_nodes.id AND s.grantee = (SELECT auth.uid()))\n  );\n\nCREATE OR REPLACE FUNCTION public.list_children(p_parent uuid)\nRETURNS SETOF public.doc_nodes\nLANGUAGE sql STABLE SECURITY DEFINER\nSET search_path = public\nAS $fn$\n  SELECT * FROM public.doc_nodes WHERE parent_id = p_parent; -- ; piège\n$fn$;\n\nGRANT EXECUTE ON FUNCTION public.list_children(uuid) TO authenticated;\nCOMMIT;\n";
        let s = sql(src);
        assert_eq!(
            names(&s),
            vec!["doc_nodes.rls", "doc_nodes.doc_nodes_select", "public.list_children", "grants.list_children"],
            "{:?}",
            names(&s)
        );
        assert_eq!(find(&s, "doc_nodes.doc_nodes_select").line, 7);
        assert_eq!(find(&s, "public.list_children").line, 14);
        assert_eq!(find(&s, "public.list_children").end_line, 20);
    }
}
