//! `peek` : lire EXACTEMENT l'endroit utile d'un gros texte (log de tests, sortie cargo,
//! log serveur, gros JSON, gros fichier source), indexé ou non, au lieu de le lire en entier.
//!
//! Trois modes, un seul moteur :
//!   - question : fenêtres de lignes les mieux classées (BM25 par ligne, termes de la question
//!     découpés comme ceux de `find`, + signaux d'échec), regroupées, dédoublonnées, plafonnées ;
//!   - `--erreurs` : seulement les blocs d'échec (en-tête, assertion, pile utile, `fichier:ligne`) ;
//!   - sans question : résumé (en-tête, lignes de bilan, compte d'erreurs, premiers blocs, fin).
//!
//! Vitesse : le fichier est lu une fois, puis DEUX passages Aho-Corasick en parallèle sur les
//! octets (signaux d'erreur ; termes de la question, insensibles à la casse). Pas de copie en
//! minuscules, pas de découpage en `String` : seules les lignes affichées sont décodées.

use crate::fx::FxHasher;
use aho_corasick::{AhoCorasick, AhoCorasickBuilder, AhoCorasickKind, MatchKind};
use std::collections::HashMap;
use std::hash::Hasher;

pub const MAX_LIGNES_DEFAUT: usize = 60;
/// Largeur max (caractères) d'une ligne affichée.
const LARGEUR: usize = 220;
/// Au-delà, la ligne (JSON minifié…) est montrée par extraits autour des termes.
const LIGNE_LONGUE: usize = 400;
const MAX_FICHIER: u64 = 1 << 30;
/// Longueur maximale d'un bloc d'échec (lignes après l'en-tête).
const BLOC_MAX: usize = 20;
/// Mode question sur une question d'échec : un signal d'échec passe avant toute ligne qui ne
/// fait que contenir un terme (le score BM25 d'une ligne reste très en dessous).
const PRIORITE_ECHEC: f32 = 1000.0;

pub struct Options {
    pub question: String,
    pub erreurs: bool,
    pub max_lignes: usize,
    /// Lignes de contexte de chaque côté (fenêtres de question) ou après l'en-tête (blocs).
    pub contexte: Option<usize>,
}

// ─── Texte : octets + index des débuts de ligne ─────────────────────────────

pub struct Texte {
    buf: Vec<u8>,
    debuts: Vec<u32>,
}

impl Texte {
    pub fn depuis(mut buf: Vec<u8>) -> Result<Texte, String> {
        // UTF-16 (redirection PowerShell 5) ou BOM UTF-8.
        if buf.len() >= 2 && (buf[..2] == [0xFF, 0xFE] || buf[..2] == [0xFE, 0xFF]) {
            let le = buf[0] == 0xFF;
            let u: Vec<u16> = buf[2..]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) })
                .collect();
            buf = String::from_utf16_lossy(&u).into_bytes();
        } else if buf.starts_with(&[0xEF, 0xBB, 0xBF]) {
            buf.drain(..3);
        }
        if buf.is_empty() {
            return Err("empty file".into());
        }
        if buf[..buf.len().min(8192)].contains(&0) {
            return Err("binary file (NUL bytes): peek reads text".into());
        }
        let mut debuts: Vec<u32> = vec![0];
        for p in memchr::memchr_iter(b'\n', &buf) {
            if p + 1 < buf.len() {
                debuts.push((p + 1) as u32);
            }
        }
        Ok(Texte { buf, debuts })
    }

    fn n(&self) -> usize {
        self.debuts.len()
    }

    fn ligne(&self, i: usize) -> &[u8] {
        let a = self.debuts[i] as usize;
        let mut b = if i + 1 < self.debuts.len() { self.debuts[i + 1] as usize - 1 } else { self.buf.len() };
        if i + 1 == self.debuts.len() && b > a && self.buf[b - 1] == b'\n' {
            b -= 1;
        }
        if b > a && self.buf[b - 1] == b'\r' {
            b -= 1;
        }
        &self.buf[a..b.max(a)]
    }

    /// Ligne contenant l'octet `pos` ; `cur` = dernière ligne trouvée (les passages avancent).
    fn ligne_de(&self, pos: usize, cur: &mut usize) -> usize {
        let n = self.debuts.len();
        let c = (*cur).min(n - 1);
        let d = |i: usize| self.debuts[i] as usize;
        if d(c) <= pos && (c + 1 >= n || pos < d(c + 1)) {
            return c;
        }
        if c + 1 < n && d(c + 1) <= pos && (c + 2 >= n || pos < d(c + 2)) {
            *cur = c + 1;
            return c + 1;
        }
        let i = self.debuts.partition_point(|&x| x as usize <= pos).saturating_sub(1);
        *cur = i;
        i
    }
}

fn charger(path: &str) -> Result<(Texte, u64), String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("cannot read {}: {}", path, e))?;
    if meta.is_dir() {
        return Err(format!("{} is a folder (peek reads a file; for a folder: overview)", path));
    }
    if meta.len() > MAX_FICHIER {
        return Err(format!("{} is larger than 1 GiB", path));
    }
    let buf = std::fs::read(path).map_err(|e| format!("cannot read {}: {}", path, e))?;
    Ok((Texte::depuis(buf)?, meta.len()))
}

// ─── Signaux d'échec ─────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum Ancre {
    /// Début de ligne (après l'indentation, jusqu'à 6 octets : puces `×`, `✗`).
    Debut,
    /// Début de ligne strict.
    Ligne,
    /// Le caractère précédent n'est pas alphanumérique.
    Mot,
    Partout,
}

#[derive(Clone, Copy, PartialEq)]
enum Genre {
    /// En-tête d'échec (ouvre un bloc).
    En,
    /// En-tête SI en début de ligne, sinon détail.
    EnSiDebut,
    /// Détail d'échec (assertion, exception) : fait partie d'un bloc.
    De,
    Avert,
    /// Ligne de bilan (début de ligne).
    Bilan,
    /// En-tête d'échec ET ligne de bilan (`FAIL\tpkg\t0.004s` de go test).
    EnBilan,
    /// Déclencheur dont la ligne entière décide (`speciale`) : pytest, jest, eslint, go.
    Spec,
}

use Ancre::*;
use Genre::*;

const SIGNAUX: &[(&str, Genre, Ancre)] = &[
    ("FAIL", En, Ligne),
    ("×", En, Debut),
    ("✗", En, Debut),
    ("✕", En, Debut),
    ("✖", En, Debut),
    ("error[", EnSiDebut, Mot),
    ("error:", EnSiDebut, Mot),
    ("error TS", En, Mot),
    ("panicked at", En, Partout),
    ("Traceback (most recent call last)", En, Partout),
    ("FATAL", En, Mot),
    ("fatal:", En, Mot),
    ("Unhandled", En, Mot),
    ("Segmentation fault", En, Partout),
    ("SIGSEGV", En, Mot),
    ("AssertionError", De, Partout),
    ("Error:", De, Partout),
    ("Exception:", De, Partout),
    ("Caused by", De, Partout),
    ("npm ERR!", De, Partout),
    ("Cannot find module", De, Partout),
    ("ECONNREFUSED", De, Mot),
    ("ECONNRESET", De, Mot),
    ("ETIMEDOUT", De, Mot),
    ("ENOENT", De, Mot),
    ("EADDRINUSE", De, Mot),
    ("EACCES", De, Mot),
    ("Uncaught", De, Mot),
    ("warning:", Avert, Mot),
    ("warning[", Avert, Mot),
    ("Warning:", Avert, Mot),
    ("WARN", Avert, Mot),
    ("Test Files", Bilan, Ligne),
    ("Test Suites", Bilan, Ligne),
    ("Tests", Bilan, Ligne),
    ("Duration", Bilan, Ligne),
    ("test result:", Bilan, Ligne),
    ("could not compile", Bilan, Mot),
    ("warnings emitted", Bilan, Partout),
    ("Finished", Bilan, Ligne),
    // jest : `Tests:       2 failed, 3 passed, 5 total`, `Time:        0.5 s`.
    ("Tests:", Bilan, Ligne),
    ("Time:", Bilan, Ligne),
    // jest : `  ● suite › cas` (tête de bloc) ; `● Console` n'en est pas un.
    ("●", Spec, Debut),
    // pytest : lignes `E   …`, têtes `____ test ____`, bilan `N failed, M passed in 1.2s`.
    ("E   ", Spec, Ligne),
    ("_", Spec, Ligne),
    (" passed", Spec, Partout),
    (" failed", Spec, Partout),
    (" skipped", Spec, Partout),
    (" error in", Spec, Partout),
    (" errors in", Spec, Partout),
    ("ERROR ", Spec, Ligne),
    // go test : `--- FAIL: TestX (0.00s)`, `FAIL\tpkg\t0.004s`, `panic: …`.
    ("--- FAIL:", En, Ligne),
    ("FAIL\t", EnBilan, Ligne),
    ("panic:", Spec, Mot),
    // eslint (stylish) : `  12:5  error  message  règle`, bilan `✖ 3 problems (…)`.
    ("error ", Spec, Mot),
    ("warning ", Spec, Mot),
    ("✖ ", Spec, Debut),
];

/// Verdict d'un déclencheur `Spec` : (classe de la ligne, la ligne est un bilan), ou rien.
/// `l` = la ligne entière, `av` = nombre d'octets avant le déclencheur.
fn speciale(motif: &str, l: &[u8], av: usize) -> Option<(u8, bool)> {
    match motif {
        "●" => {
            let reste = l.get(av + motif.len()..)?;
            if reste.first() != Some(&b' ') || reste.trim_ascii_start().starts_with(b"Console") {
                return None;
            }
            Some((3, false))
        }
        "E   " => (av == 0).then_some((2, false)),
        "_" => pytest_entete(l).then_some((3, false)),
        " passed" | " failed" | " skipped" | " error in" | " errors in" => pytest_bilan(l).then_some((0, true)),
        "ERROR " => (av == 0 && memchr::memmem::find(l, b"::").is_some()).then_some((3, false)),
        "panic:" => Some((if av == 0 { 3 } else { 2 }, false)),
        "error " => eslint_loc(&l[..av]).then_some((3, false)),
        "warning " => eslint_loc(&l[..av]).then_some((1, false)),
        "✖ " => {
            let reste = &l[av + motif.len()..];
            if reste.first().is_some_and(|c| c.is_ascii_digit()) && memchr::memmem::find(reste, b"problem").is_some() {
                Some((0, true))
            } else {
                Some((3, false))
            }
        }
        _ => None,
    }
}

/// `____ test_total ____` : au moins un `_`, un espace, un titre, un espace, au moins un `_`.
fn pytest_entete(l: &[u8]) -> bool {
    let s = l.trim_ascii();
    let g = s.iter().take_while(|c| **c == b'_').count();
    let d = s.iter().rev().take_while(|c| **c == b'_').count();
    g >= 1 && d >= 1 && s.len() > g + d + 2 && s[g] == b' ' && s[s.len() - d - 1] == b' '
}

/// `=== 2 failed, 3 passed in 0.12s ===` ou (mode -q) `2 failed, 3 passed in 0.12s`.
fn pytest_bilan(l: &[u8]) -> bool {
    let s = l.trim_ascii();
    let s = s.trim_ascii_start_matches(b'=').trim_ascii();
    let s = s.trim_ascii_end_matches(b'=').trim_ascii();
    if !s.first().is_some_and(|c| c.is_ascii_digit()) {
        return false;
    }
    let Some(p) = memchr::memmem::rfind(s, b" in ") else { return false };
    let (avant, apres) = (&s[..p], &s[p + 4..]);
    avant.iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, b' ' | b',' | b'/' | b'-'))
        && apres.first().is_some_and(|c| c.is_ascii_digit())
        && apres.iter().all(|c| c.is_ascii_digit() || matches!(c, b'.' | b's' | b' ' | b'(' | b')' | b':'))
}

/// Début d'une ligne eslint : `  12:5  ` juste avant `error` / `warning`.
fn eslint_loc(av: &[u8]) -> bool {
    let s = av.trim_ascii_start();
    let a = s.iter().take_while(|c| c.is_ascii_digit()).count();
    if a == 0 || s.get(a) != Some(&b':') {
        return false;
    }
    let s = &s[a + 1..];
    let b = s.iter().take_while(|c| c.is_ascii_digit()).count();
    b > 0 && s.len() > b && s[b..].iter().all(|c| *c == b' ')
}

trait TrimBytes {
    fn trim_ascii_start_matches(&self, c: u8) -> &[u8];
    fn trim_ascii_end_matches(&self, c: u8) -> &[u8];
}

impl TrimBytes for [u8] {
    fn trim_ascii_start_matches(&self, c: u8) -> &[u8] {
        let n = self.iter().take_while(|x| **x == c).count();
        &self[n..]
    }
    fn trim_ascii_end_matches(&self, c: u8) -> &[u8] {
        let n = self.iter().rev().take_while(|x| **x == c).count();
        &self[..self.len() - n]
    }
}

/// Ligne `  at …` / `❯ …` / frames Go du runtime : du bruit de pile, pas du code utile.
fn cadre_go(l: &[u8]) -> bool {
    let s = l.trim_ascii_start();
    (l.first() == Some(&b'\t')
        && (memchr::memmem::find(s, b"/go/src/").is_some()
            || memchr::memmem::find(s, b"/usr/local/go/").is_some()
            || memchr::memmem::find(s, b"/runtime/").is_some()))
        || s.starts_with(b"testing.tRunner")
        || s.starts_with(b"runtime.")
        || s.starts_with(b"created by testing.")
        || s.starts_with(b"panic({")
}

/// Ligne d'un test RÉUSSI (`test x ... ok`, `✓ x`, `PASS fichier`, `x PASSED`, `--- PASS:`,
/// TAP `ok 1 - x`, `x passed (3ms)`). Son nom peut contenir error / failed / panic : cela ne
/// dit rien d'un échec, la ligne n'est donc JAMAIS un signal d'échec par son seul nom.
fn ligne_reussie(l: &[u8]) -> bool {
    let s = l.trim_ascii();
    if s.is_empty() {
        return false;
    }
    // Rejet rapide (la fonction tourne sur des millions de lignes) : tout succès commence par
    // « ✓ ✔ √ » (0xE2), « P », « o », « - », ou finit par « k » (… ok), « D », « d », « ) », « ] ».
    if !matches!(s[0], 0xE2 | b'P' | b'o' | b'-') && !matches!(s[s.len() - 1], b'k' | b'D' | b'd' | b')' | b']') {
        return false;
    }
    // cargo / libtest : `test chemin ... ok`, `test x - should panic ... ok`.
    if s.ends_with(b"... ok") || s.ends_with(b"...ok") {
        return true;
    }
    // Puces vertes (vitest, jest, mocha) et formes « PASS ».
    if ["✓", "✔", "√"].iter().any(|p| s.starts_with(p.as_bytes())) {
        return true;
    }
    if s == b"PASS" || s.starts_with(b"PASS ") || s.starts_with(b"PASS\t") || s.starts_with(b"PASSED ") || s.starts_with(b"--- PASS") {
        return true;
    }
    // go test : `ok  \tpkg\t0.004s` ; TAP : `ok 1 - titre`.
    if s.starts_with(b"ok ") || s.starts_with(b"ok\t") {
        return true;
    }
    // Le verdict « passed » est en fin de ligne : on ne cherche que dans les 40 derniers octets
    // (coût borné sur les lignes énormes).
    let fin = &s[s.len().saturating_sub(40)..];
    // pytest -v : `tests/test_a.py::test_x PASSED   [ 33%]`.
    if let Some(p) = memchr::memmem::find(fin, b" PASSED") {
        if suite_de_succes(fin[p + 7..].trim_ascii()) {
            return true;
        }
    }
    // `nom passed`, `nom passed (12ms)` — mais pas le bilan `2 failed, 3 passed`.
    if !s[0].is_ascii_digit() && s[0] != b'=' {
        if let Some(p) = memchr::memmem::rfind(fin, b" passed") {
            if suite_de_succes(fin[p + 7..].trim_ascii()) {
                return true;
            }
        }
    }
    false
}

/// Ce qui suit « passed » / « PASSED » dans une ligne de succès : rien, une durée `(12ms)`,
/// `(0.5 s)` ou un pourcentage pytest `[ 33%]` — jamais un effectif comme `(7)` (bilan vitest
/// `Test Files  1 failed | 6 passed (7)`).
fn suite_de_succes(reste: &[u8]) -> bool {
    if reste.is_empty() {
        return true;
    }
    if let Some(r) = reste.strip_prefix(b"(") {
        let Some(f) = r.iter().position(|c| *c == b')') else { return false };
        let d = r[..f].trim_ascii();
        return d.ends_with(b"s")
            && d.iter().any(|c| c.is_ascii_digit())
            && d.iter().all(|c| c.is_ascii_digit() || matches!(c, b'.' | b' ' | b'm' | b's' | b'u' | b'n'));
    }
    reste.starts_with(b"[") && reste.ends_with(b"%]")
}

/// La question parle-t-elle d'un échec (échoue, erreur, plante, pourquoi, why, fail, cassé…) ?
/// Si oui, et que le log contient des signaux d'échec, ceux-ci passent avant la simple
/// correspondance de termes.
fn question_echec(question: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "echou",
        "echec",
        "erreur",
        "plant",
        "crash",
        "cass",
        "bug",
        "probleme",
        "paniqu",
        "planta",
        "fail",
        "error",
        "broken",
        "wrong",
        "panic",
        "exception",
        "rouge",
        "regress",
        "brise",
        "bloqu",
        "stuck",
    ];
    const EXACTS: &[&str] = &["pourquoi", "why", "ko", "fatal"];
    let bas = crate::symbol::fold_accents(&question.to_lowercase());
    bas.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .any(|w| EXACTS.contains(&w) || PREFIXES.iter().any(|p| w.starts_with(p)))
}

/// Classe d'une ligne : 0 rien, 1 avertissement, 2 détail d'échec, 3 en-tête d'échec.
struct Signaux {
    classe: Vec<u8>,
    bilan: Vec<u32>,
}

fn signaux(t: &Texte) -> Signaux {
    let pats: Vec<&str> = SIGNAUX.iter().map(|s| s.0).collect();
    let ac = AhoCorasickBuilder::new()
        .match_kind(MatchKind::LeftmostLongest)
        .kind(Some(AhoCorasickKind::DFA))
        .build(&pats)
        .expect("signal automaton");
    let mut classe = vec![0u8; t.n()];
    let mut bilan: Vec<u32> = Vec::new();
    let mut cur = 0usize;
    // (ligne, la ligne est un test réussi) : calculé une fois par ligne, et seulement si besoin.
    let mut verdict: (usize, bool) = (usize::MAX, false);
    for m in ac.find_iter(&t.buf) {
        let pos = m.start();
        let (motif, genre, ancre) = SIGNAUX[m.pattern().as_usize()];
        let i = t.ligne_de(pos, &mut cur);
        let ls = t.debuts[i] as usize;
        let mut fnw = ls;
        while fnw < pos && matches!(t.buf[fnw], b' ' | b'\t') {
            fnw += 1;
        }
        let ok = match ancre {
            Ligne => pos == fnw,
            Debut => pos - fnw <= 6,
            Mot => pos == ls || !t.buf[pos - 1].is_ascii_alphanumeric(),
            Partout => true,
        };
        if !ok {
            continue;
        }
        let c = match genre {
            En => 3,
            EnBilan => {
                if bilan.last() != Some(&(i as u32)) {
                    bilan.push(i as u32);
                }
                3
            }
            Spec => match speciale(motif, t.ligne(i), pos - ls) {
                Some((c, est_bilan)) => {
                    if est_bilan && bilan.last() != Some(&(i as u32)) {
                        bilan.push(i as u32);
                    }
                    if c == 0 {
                        continue;
                    }
                    c
                }
                None => continue,
            },
            EnSiDebut => {
                if pos == fnw {
                    3
                } else {
                    2
                }
            }
            De => 2,
            Avert => 1,
            Bilan => {
                // « Tests » : suivi d'espaces puis d'un chiffre ; les autres : tels quels.
                if motif == "Tests" {
                    let mut k = pos + motif.len();
                    let mut esp = 0;
                    while k < t.buf.len() && t.buf[k] == b' ' {
                        k += 1;
                        esp += 1;
                    }
                    if esp == 0 || k >= t.buf.len() || !t.buf[k].is_ascii_digit() {
                        continue;
                    }
                }
                if bilan.last() != Some(&(i as u32)) {
                    bilan.push(i as u32);
                }
                continue;
            }
        };
        if classe[i] < c {
            // Un test réussi dont le nom contient error / failed / panic n'est pas un échec.
            if c >= 2 {
                if verdict.0 != i {
                    verdict = (i, ligne_reussie(t.ligne(i)));
                }
                if verdict.1 {
                    continue;
                }
            }
            classe[i] = c;
        }
    }
    Signaux { classe, bilan }
}

// ─── Termes de la question + BM25 par ligne ─────────────────────────────────

struct Terme {
    aiguille: String,
    poids: f32,
}

/// Mots français (accents repliés) → mots usuels des logs anglais.
const LEXIQUE_LOG: &[(&[&str], &[&str])] = &[
    (
        &["erreur", "erreurs", "echec", "echecs", "echoue", "echouent", "echouer", "plantage", "plante", "crash"],
        &["error", "fail", "panic", "exception", "fatal"],
    ),
    (&["avertissement", "avertissements", "alerte"], &["warn"]),
    (&["delai", "expire", "expiration", "lent", "lenteur"], &["timeout", "timed out", "slow"]),
    (&["refuse", "refus", "interdit"], &["refused", "denied", "forbidden"]),
    (&["connexion", "connecte"], &["connect"]),
    (&["introuvable", "manquant", "absent"], &["not found", "missing", "enoent"]),
    (&["memoire"], &["memory", "oom"]),
    (&["attendu", "recu"], &["expected", "received"]),
    (&["pile", "trace"], &["stack", "trace"]),
    (&["lancement", "demarrage", "demarre"], &["start", "listening", "boot"]),
];

fn termes(question: &str) -> Vec<Terme> {
    let mut v: Vec<Terme> = Vec::new();
    let mut ajoute = |s: String, p: f32| {
        if !v.iter().any(|t| t.aiguille == s) {
            v.push(Terme { aiguille: s, poids: p });
        }
    };
    // Mots « exacts » (identifiants, chemins, codes) : cherchés tels quels, plus lourds.
    for w in question.split_whitespace() {
        let w = w.trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | '.' | ',' | ';' | '?' | '!' | '(' | ')' | '[' | ']'));
        let special = w.chars().any(|c| matches!(c, '.' | '/' | '\\' | '_' | '-' | ':' | '#' | '@'))
            || w.chars().skip(1).any(|c| c.is_uppercase())
            || w.chars().any(|c| c.is_ascii_digit());
        if w.chars().count() >= 3 && special {
            ajoute(w.to_lowercase(), 1.5);
        }
    }
    for t in crate::search::query_terms(question) {
        // Question en français sur un log anglais : mots de log équivalents (poids réduit).
        for (fr, en) in LEXIQUE_LOG {
            if fr.contains(&t.as_str()) {
                for e in *en {
                    ajoute(e.to_string(), 0.6);
                }
            }
        }
        let num = t.chars().all(|c| c.is_ascii_digit());
        if t.chars().count() < if num { 3 } else { 2 } {
            continue;
        }
        let s = if t.len() >= 5 { crate::stem::stem(&t) } else { t.clone() };
        let s = if s.chars().count() >= 3 { s } else { t };
        ajoute(s, 1.0);
    }
    v
}

// ─── Accents : « échoué » et « echoue » se répondent dans les deux sens ───────

/// Lettres accentuées françaises (UTF-8 sur deux octets) et leur lettre de base.
const ACCENTS: &[(char, char)] = &[
    ('é', 'e'),
    ('è', 'e'),
    ('ê', 'e'),
    ('ë', 'e'),
    ('à', 'a'),
    ('â', 'a'),
    ('ù', 'u'),
    ('û', 'u'),
    ('ô', 'o'),
    ('î', 'i'),
    ('ï', 'i'),
    ('ç', 'c'),
    ('É', 'e'),
    ('È', 'e'),
    ('Ê', 'e'),
    ('À', 'a'),
    ('Â', 'a'),
    ('Ù', 'u'),
    ('Û', 'u'),
    ('Ô', 'o'),
    ('Î', 'i'),
    ('Ï', 'i'),
    ('Ç', 'c'),
];
/// `œ` / `Œ` : repliés en « oe » (comme `fold_accents`).
const LIGATURES: &[char] = &['œ', 'Œ'];
/// Au plus ce nombre de motifs par terme (sinon : les variantes à une seule substitution).
const VARIANTES_MAX: usize = 64;
/// On cherche les accents présents dans les premiers octets seulement.
const SCAN_ACCENTS: usize = 64 << 20;

/// Lettres accentuées réellement présentes dans le texte (lecture des octets `C3 xx` / `C5 xx`).
/// Un log sans accent rend un ensemble vide : aucun motif supplémentaire, aucun coût.
fn accents_presents(buf: &[u8]) -> Vec<char> {
    let mut vus: Vec<char> = Vec::new();
    let fin = buf.len().min(SCAN_ACCENTS);
    for p in memchr::memchr2_iter(0xC3, 0xC5, &buf[..fin]) {
        let Some(&b1) = buf.get(p + 1) else { break };
        if b1 & 0xC0 != 0x80 {
            continue;
        }
        let cp = (((buf[p] & 0x1F) as u32) << 6) | (b1 & 0x3F) as u32;
        let Some(c) = char::from_u32(cp) else { continue };
        if (ACCENTS.iter().any(|a| a.0 == c) || LIGATURES.contains(&c)) && !vus.contains(&c) {
            vus.push(c);
            if vus.len() == ACCENTS.len() + LIGATURES.len() {
                break;
            }
        }
    }
    vus
}

/// Motifs d'un terme : le terme tel quel, sa forme repliée (sans accent), et ses formes
/// accentuées — seulement avec les accents présents dans le texte, bornées à `VARIANTES_MAX`.
fn motifs_terme(aiguille: &str, presents: &[char]) -> Vec<String> {
    let mut v: Vec<String> = vec![aiguille.to_string()];
    let base = crate::symbol::fold_accents(aiguille);
    if base != aiguille {
        v.push(base.clone());
    }
    if presents.is_empty() {
        return v;
    }
    let chars: Vec<char> = base.chars().collect();
    let alts: Vec<Vec<char>> = chars
        .iter()
        .map(|&c| {
            let mut a = vec![c];
            if c.is_ascii_lowercase() {
                for &(acc, b) in ACCENTS {
                    if b == c && presents.contains(&acc) {
                        a.push(acc);
                    }
                }
            }
            a
        })
        .collect();
    let mut produit = 1usize;
    for a in &alts {
        produit = produit.saturating_mul(a.len());
    }
    let mut reserve: Vec<String> = Vec::new();
    let mut ajoute = |x: String| {
        if v.len() < VARIANTES_MAX + 2 && !v.contains(&x) {
            v.push(x);
        }
    };
    // Trop de combinaisons : d'abord le produit des seuls accents « principaux » (le premier
    // présent de chaque lettre : é, à, ù, ô, î, ç), puis les substitutions d'une seule lettre.
    let mut alts = alts;
    if produit > VARIANTES_MAX {
        let reduit: Vec<Vec<char>> = alts.iter().map(|a| a.iter().take(2).copied().collect()).collect();
        let p2: usize = reduit.iter().fold(1usize, |p, a| p.saturating_mul(a.len()));
        if p2 <= VARIANTES_MAX / 2 {
            let singles = alts.clone();
            alts = reduit;
            produit = p2;
            // Les substitutions d'une lettre (É, è, ê…) viennent ensuite.
            let mut apres: Vec<String> = Vec::new();
            for (k, a) in singles.iter().enumerate() {
                for &c in &a[1..] {
                    apres.push(chars.iter().enumerate().map(|(j, &o)| if j == k { c } else { o }).collect());
                }
            }
            reserve = apres;
        }
    }
    if produit > 1 && produit <= VARIANTES_MAX {
        let mut cour: Vec<String> = vec![String::new()];
        for a in &alts {
            let mut nouv: Vec<String> = Vec::with_capacity(cour.len() * a.len());
            for pre in &cour {
                for &c in a {
                    let mut x = pre.clone();
                    x.push(c);
                    nouv.push(x);
                }
            }
            cour = nouv;
        }
        for x in cour {
            ajoute(x);
        }
        for x in reserve {
            ajoute(x);
        }
    } else if produit > VARIANTES_MAX {
        for (k, a) in alts.iter().enumerate() {
            for &c in &a[1..] {
                let x: String = chars.iter().enumerate().map(|(j, &o)| if j == k { c } else { o }).collect();
                ajoute(x);
            }
        }
    }
    // Ligature : « coeur » → « cœur ».
    if base.contains("oe") {
        for &l in LIGATURES.iter().filter(|l| presents.contains(l)) {
            ajoute(base.replace("oe", &l.to_string()));
        }
    }
    v
}

struct Lexique {
    /// Score BM25 de chaque ligne (vide si aucun terme).
    score: Vec<f32>,
    ac: Option<AhoCorasick>,
    /// Termes sans aucune occurrence.
    absents: Vec<String>,
}

fn lexique(t: &Texte, ts: &[Terme]) -> Lexique {
    if ts.is_empty() {
        return Lexique { score: Vec::new(), ac: None, absents: Vec::new() };
    }
    let presents = accents_presents(&t.buf);
    let mut pats: Vec<String> = Vec::new();
    let mut proprietaire: Vec<usize> = Vec::new();
    for (k, x) in ts.iter().enumerate() {
        for m in motifs_terme(&x.aiguille, &presents) {
            pats.push(m);
            proprietaire.push(k);
        }
    }
    let ac = AhoCorasickBuilder::new()
        .ascii_case_insensitive(true)
        .match_kind(MatchKind::Standard)
        .kind(Some(AhoCorasickKind::DFA))
        .build(&pats)
        .expect("term automaton");
    let n = t.n();
    let mut lignes: Vec<Vec<u32>> = vec![Vec::new(); ts.len()];
    let mut cur = 0usize;
    for m in ac.find_overlapping_iter(&t.buf) {
        let i = t.ligne_de(m.start(), &mut cur);
        lignes[proprietaire[m.pattern().as_usize()]].push(i as u32);
    }
    let mut score = vec![0f32; n];
    let mut couv = vec![0u8; n];
    let moy = (t.buf.len() as f32 / n as f32).max(1.0);
    let mut absents = Vec::new();
    for (k, hits) in lignes.iter().enumerate() {
        if hits.is_empty() {
            if ts[k].poids >= 1.0 {
                absents.push(ts[k].aiguille.clone());
            }
            continue;
        }
        // (ligne, tf) par séquences : les occurrences arrivent presque dans l'ordre.
        let mut paires: Vec<(u32, u32)> = Vec::new();
        for &h in hits {
            match paires.last_mut() {
                Some((l, tf)) if *l == h => *tf += 1,
                _ => paires.push((h, 1)),
            }
        }
        // Désordre local possible : on regroupe tout de même les lignes identiques.
        if !hits.windows(2).all(|w| w[0] <= w[1]) {
            paires.sort_unstable();
            paires.dedup_by(|b, a| {
                if a.0 == b.0 {
                    a.1 += b.1;
                    true
                } else {
                    false
                }
            });
        }
        let df = paires.len() as f32;
        let idf = (1.0 + (n as f32 - df + 0.5) / (df + 0.5)).ln();
        for (l, tf) in paires {
            let l = l as usize;
            let len = (t.ligne(l).len() as f32).max(1.0);
            let tf = tf as f32;
            let (k1, b) = (1.2f32, 0.5f32);
            score[l] += ts[k].poids * idf * tf * (k1 + 1.0) / (tf + k1 * (1.0 - b + b * len / moy));
            couv[l] = couv[l].saturating_add(1);
        }
    }
    for i in 0..n {
        if couv[i] > 1 {
            score[i] *= 1.0 + 0.5 * (couv[i] - 1) as f32;
        }
    }
    Lexique { score, ac: Some(ac), absents }
}

// ─── Fenêtres ────────────────────────────────────────────────────────────────

#[derive(Clone, Default)]
struct Fenetre {
    a: usize,
    b: usize,
    /// Occurrences semblables ailleurs (écartées), avec quelques numéros de ligne.
    rep: u32,
    ex: Vec<usize>,
}

fn indentation(l: &[u8]) -> usize {
    l.iter().take_while(|c| matches!(c, b' ' | b'\t')).count()
}

fn vide(l: &[u8]) -> bool {
    l.iter().all(|c| c.is_ascii_whitespace())
}

fn separateur(l: &[u8]) -> bool {
    let s = String::from_utf8_lossy(l);
    let s = s.trim();
    s.starts_with('⎯') || s.starts_with('═') || s.starts_with('━') || s.starts_with("-----") || s.starts_with("=====")
}

fn marqueur_diff(l: &[u8]) -> bool {
    l.starts_with(b"- ") || l.starts_with(b"+ ") || l.starts_with(b"> ") || l.starts_with(b"< ")
}

/// Ligne de code numérotée d'un compilateur (`10 |   let x`).
fn code_numerote(l: &[u8]) -> bool {
    let d = l.iter().take_while(|c| c.is_ascii_digit()).count();
    d > 0 && l[d..].iter().take_while(|c| **c == b' ').count() + d < l.len() && l[d..].trim_ascii_start().starts_with(b"|")
}

/// `test_calc.py:9: AssertionError` (dernière ligne d'un bloc pytest) ou `file.py:9: in test_x` (--tb=short).
fn pytest_lieu(l: &[u8]) -> bool {
    let Ok(s) = std::str::from_utf8(l) else { return false };
    let Some((lieu, reste)) = s.split_once(": ") else { return false };
    let Some((chemin, n)) = lieu.rsplit_once(':') else { return false };
    !n.is_empty()
        && n.bytes().all(|c| c.is_ascii_digit())
        && chemin.rsplit_once('.').is_some_and(|(_, e)| !e.is_empty() && e.len() <= 5 && e.bytes().all(|c| c.is_ascii_alphanumeric()))
        && !chemin.contains(' ')
        && reste.bytes().next().is_some_and(|c| c.is_ascii_alphabetic())
}

/// Ligne eslint (stylish) : `  12:5  error  message  règle`.
fn ligne_eslint(l: &[u8]) -> bool {
    let tr = l.trim_ascii_start();
    let a = tr.iter().take_while(|c| c.is_ascii_digit()).count();
    if a == 0 || tr.get(a) != Some(&b':') {
        return false;
    }
    let b = tr[a + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
    let reste = tr[a + 1 + b..].trim_ascii_start();
    b > 0 && (reste.starts_with(b"error ") || reste.starts_with(b"warning "))
}

/// Première ligne du bloc d'échec qui contient la ligne `i` (classe >= 2). `fin_prec` : fin du
/// bloc précédent (une ligne déjà prise ne rouvre pas de bloc).
/// Rend aussi la ligne de tête (d'où `bloc_fin` part : elle suit le début visible quand des
/// messages la précèdent) et `true` si la première ligne est le nom de fichier d'un rapport eslint.
fn debut_bloc(t: &Texte, classe: &[u8], i: usize, fin_prec: isize) -> (usize, usize, bool) {
    let mut a = i;
    if classe[i] == 2 {
        for h in (i.saturating_sub(3)..i).rev() {
            if classe[h] == 3 && (h as isize) > fin_prec {
                a = h;
                break;
            }
        }
    }
    let tete = t.ligne(a);
    let h = a;
    // go test -v : les messages `    fichier_test.go:12: …` PRÉCÈDENT la ligne `--- FAIL:`.
    if tete.trim_ascii_start().starts_with(b"--- FAIL") {
        let mut n = 0;
        while a > 0 && n < 8 && (a as isize - 1) > fin_prec && classe[a - 1] < 2 {
            let p = t.ligne(a - 1);
            if indentation(p) < 4 || vide(p) || p.trim_ascii_start().starts_with(b"=== ") {
                break;
            }
            a -= 1;
            n += 1;
        }
        return (a, h, false);
    }
    // eslint : le nom du fichier est la ligne juste avant la première erreur.
    if ligne_eslint(tete) && a > 0 && (a as isize - 1) > fin_prec {
        let p = t.ligne(a - 1);
        if !vide(p) && indentation(p) == 0 && classe[a - 1] == 0 && !ligne_eslint(p) {
            return (a - 1, a - 1, true);
        }
    }
    (a, h, false)
}

/// Fin (incluse) du bloc d'échec ouvert à la ligne `s`.
fn bloc_fin(t: &Texte, classe: &[u8], s: usize, ext: usize) -> usize {
    let n = t.n();
    // Une puce de test (`× nom`) est une ligne seule : seules les lignes PLUS indentées la suivent.
    let tete = t.ligne(s);
    let tr0 = tete.trim_ascii_start();
    let base = if ["×", "✗", "✕", "✖", "●"].iter().any(|b| tr0.starts_with(b.as_bytes())) { indentation(tete) } else { 0 };
    let (mut fin, mut j, mut blancs, mut pile_go, mut puces) = (s, s + 1, 0, false, 0);
    // Panic Rust : `thread 'x' panicked at f.rs:1:2:` est suivi du MESSAGE (une ou plusieurs
    // lignes non indentées : `called Option::unwrap() on a None value`, `assertion … failed`),
    // jusqu'à la ligne vide, `stack backtrace:` ou `note: run with RUST_BACKTRACE`.
    let panic_rust = memchr::memmem::find(tete, b"panicked at").is_some();
    while j < n && j - s <= ext {
        let l = t.ligne(j);
        if separateur(l) {
            break;
        }
        if vide(l) {
            if panic_rust {
                break;
            }
            blancs += 1;
            if blancs >= 2 {
                break;
            }
            j += 1;
            continue;
        }
        let ind = indentation(l);
        let tr = l.trim_ascii_start();
        // jest : la tête `FAIL fichier` emporte sa première puce `● test` ; une autre puce
        // (ou toute puce, si le bloc est déjà une puce) ouvre son propre bloc.
        let puce = tr.starts_with("●".as_bytes());
        if classe[j] == 3 && (ind <= 1 || (puce && (puces > 0 || tr0.starts_with("●".as_bytes())))) {
            break;
        }
        if puce {
            puces += 1;
        }
        if tr.starts_with(b"goroutine ") {
            pile_go = true;
        }
        let message_rust = panic_rust
            && classe[j] < 3
            && !tr.starts_with(b"stack backtrace")
            && !tr.starts_with(b"note: run with")
            && !tr.starts_with(b"note: Some details");
        let suite = ind > base
            || message_rust
            || classe[j] == 2
            || marqueur_diff(l)
            || code_numerote(l)
            || tr.starts_with(b"help:")
            || tr.starts_with(b"note:")
            || tr.starts_with(b"goroutine ")
            || (ind == 0 && pytest_lieu(l))
            // pile go : `pkg.Fonction(...)` puis `\tfichier.go:12 +0x1c`
            || (pile_go && (l.first() == Some(&b'\t') || l.ends_with(b")") || memchr::memmem::find(l, b".go:").is_some()));
        if !suite {
            break;
        }
        fin = j;
        blancs = 0;
        j += 1;
    }
    fin
}

fn normalise(l: &[u8], h: &mut FxHasher) {
    let tr = l.trim_ascii();
    let n = tr.len().min(160);
    let mut i = 0;
    while i < n {
        let c = tr[i];
        if c.is_ascii_digit() {
            let mut j = i;
            while j < n && tr[j].is_ascii_digit() {
                j += 1;
            }
            // Un nombre collé à un mot (`mod100`, `v2`, `utf8`) fait partie du nom : conservé.
            let colle = (i > 0 && (tr[i - 1].is_ascii_alphabetic() || tr[i - 1] == b'_'))
                || (j < n && (tr[j].is_ascii_alphabetic() || tr[j] == b'_'));
            if colle {
                h.write(&tr[i..j]);
            } else {
                h.write_u8(b'#');
            }
            i = j;
        } else if matches!(c, b' ' | b'_') {
            // Remplissages de colonnes (eslint) et titres centrés (pytest `____ test ____`) :
            // leur largeur change d'une ligne à l'autre ; toute série de 2 ou plus compte pareil.
            let mut j = i;
            while j < n && tr[j] == c {
                j += 1;
            }
            h.write_u8(c);
            if j - i > 1 {
                h.write_u8(c);
            }
            i = j;
        } else {
            h.write_u8(c);
            i += 1;
        }
    }
    h.write_u8(b'\n');
}
fn cle_ligne(t: &Texte, i: usize) -> u64 {
    let mut h = FxHasher::default();
    normalise(t.ligne(i), &mut h);
    h.finish()
}

struct Bloc {
    f: Fenetre,
}

/// Clé de dédoublonnage d'un bloc : sa tête et la ligne suivante, chiffres ignorés.
/// pytest (`____ test ____`) et jest (`● test`) : la tête ne dit pas l'erreur, on ajoute la
/// première ligne de détail. eslint : la clé ignore le nom du fichier.
fn cle_bloc(t: &Texte, classe: &[u8], a: usize, b: usize, fichier_eslint: bool) -> u64 {
    let mut h = FxHasher::default();
    let k = if fichier_eslint { a + 1 } else { a };
    normalise(t.ligne(k), &mut h);
    if b > k {
        normalise(t.ligne(k + 1), &mut h);
    }
    let tr = t.ligne(k).trim_ascii_start();
    if tr.starts_with(b"_") || tr.starts_with("●".as_bytes()) {
        if let Some(j) = (k + 1..=b).find(|&j| classe[j] >= 2) {
            normalise(t.ligne(j), &mut h);
        }
    } else if let Some(j) = (k + 2..=b).find(|&j| classe[j] >= 2) {
        // Le MESSAGE distingue deux blocs de même tête (Traceback : l'exception finale).
        normalise(t.ligne(j), &mut h);
    }
    h.finish()
}

/// Blocs d'échec dans l'ordre du fichier, dédoublonnés (chiffres ignorés), avec le nombre
/// de blocs identiques écartés. Rend aussi (blocs au total, blocs distincts).
fn blocs_erreurs(t: &Texte, classe: &[u8], ext: usize) -> (Vec<Bloc>, usize) {
    let mut res: Vec<Bloc> = Vec::new();
    let mut index: HashMap<u64, usize> = HashMap::new();
    let mut total = 0usize;
    let mut fin_prec: isize = -1;
    for i in 0..t.n() {
        if classe[i] < 2 || (i as isize) <= fin_prec {
            continue;
        }
        let (a, h, fichier_eslint) = debut_bloc(t, classe, i, fin_prec);
        let b = bloc_fin(t, classe, h, ext);
        fin_prec = b as isize;
        total += 1;
        let cle = cle_bloc(t, classe, a, b, fichier_eslint);
        match index.get(&cle) {
            Some(&k) => {
                res[k].f.rep += 1;
                if res[k].f.ex.len() < 3 {
                    res[k].f.ex.push(a);
                }
            }
            None => {
                index.insert(cle, res.len());
                res.push(Bloc { f: Fenetre { a, b, rep: 0, ex: Vec::new() } });
            }
        }
    }
    (res, total)
}

/// Union d'intervalles déjà pris ; coût = lignes nouvelles d'une fenêtre.
#[derive(Default)]
struct Pris(Vec<(usize, usize)>);

impl Pris {
    fn cout(&self, a: usize, b: usize) -> usize {
        let mut c = b - a + 1;
        for &(x, y) in &self.0 {
            if x <= b && y >= a {
                c -= y.min(b) - x.max(a) + 1;
            }
        }
        c
    }
    fn couvre(&self, i: usize) -> bool {
        self.0.iter().any(|&(x, y)| x <= i && i <= y)
    }
    fn ajoute(&mut self, a: usize, b: usize) {
        self.0.push((a, b));
        self.0.sort_unstable();
        let mut m: Vec<(usize, usize)> = Vec::new();
        for &(x, y) in &self.0 {
            match m.last_mut() {
                Some(l) if x <= l.1 + 1 => l.1 = l.1.max(y),
                _ => m.push((x, y)),
            }
        }
        self.0 = m;
    }
}

// ─── Rendu ───────────────────────────────────────────────────────────────────

fn sans_ansi(s: &str) -> String {
    if !s.contains('\u{1b}') {
        return s.to_string();
    }
    let mut o = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\u{1b}' {
            if it.peek() == Some(&'[') {
                it.next();
                for d in it.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            o.push(c);
        }
    }
    o
}

fn coupe(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut o: String = s.chars().take(max).collect();
        o.push('…');
        o
    }
}

fn format_ligne(t: &Texte, i: usize, ac: Option<&AhoCorasick>) -> String {
    let raw = t.ligne(i);
    let num = i + 1;
    if raw.len() <= LIGNE_LONGUE {
        return format!("{}│{}", num, coupe(&sans_ansi(&String::from_utf8_lossy(raw)), LARGEUR));
    }
    // Ligne très longue : extraits autour des premières occurrences des termes.
    let mut segs: Vec<String> = Vec::new();
    let mut dernier: Option<usize> = None;
    if let Some(ac) = ac {
        for m in ac.find_iter(raw) {
            let p = m.start();
            if dernier.is_some_and(|d| p < d + 300) {
                continue;
            }
            let (x, y) = (p.saturating_sub(90), (p + 130).min(raw.len()));
            segs.push(format!("[col {}] {}", p + 1, sans_ansi(&String::from_utf8_lossy(&raw[x..y]))));
            dernier = Some(p);
            if segs.len() >= 3 {
                break;
            }
        }
    }
    if segs.is_empty() {
        segs.push(sans_ansi(&String::from_utf8_lossy(&raw[..LARGEUR.min(raw.len())])));
    }
    format!("{}│{} … ({} bytes on this line)", num, segs.join(" … "), raw.len())
}

fn cadre_bruit(l: &[u8]) -> bool {
    let s = l.trim_ascii_start();
    ((s.starts_with(b"at ") || s.starts_with("❯".as_bytes()))
        && (memchr::memmem::find(s, b"node_modules").is_some() || memchr::memmem::find(s, b"node:internal").is_some()))
        || cadre_go(l)
}

fn rendre(t: &Texte, mut fen: Vec<Fenetre>, ac: Option<&AhoCorasick>, out: &mut Vec<String>) {
    fen.sort_by_key(|f| f.a);
    let mut fus: Vec<Fenetre> = Vec::new();
    for f in fen {
        if let Some(l) = fus.last_mut() {
            if f.a <= l.b + 1 {
                l.b = l.b.max(f.b);
                l.rep += f.rep;
                l.ex.extend(f.ex);
                continue;
            }
        }
        fus.push(f);
    }
    for (k, f) in fus.iter().enumerate() {
        if k > 0 {
            out.push("…".into());
        }
        let mut i = f.a;
        while i <= f.b {
            if cadre_bruit(t.ligne(i)) {
                let mut j = i;
                while j <= f.b && cadre_bruit(t.ligne(j)) {
                    j += 1;
                }
                if j - i >= 2 {
                    let ou = if cadre_go(t.ligne(i)) { "the go runtime" } else { "node_modules/internals" };
                    out.push(format!("   … {} stack frame line(s) in {} (L{}-{})", j - i, ou, i + 1, j));
                    i = j;
                    continue;
                }
            }
            out.push(format_ligne(t, i, ac));
            i += 1;
        }
        if f.rep > 0 {
            let mut ex: Vec<usize> = f.ex.clone();
            ex.sort_unstable();
            ex.dedup();
            let l: Vec<String> = ex.iter().take(3).map(|x| format!("L{}", x + 1)).collect();
            out.push(format!(
                "   ↳ +{} similar block(s) skipped{}",
                f.rep,
                if l.is_empty() { String::new() } else { format!(" ({}…)", l.join(", ")) }
            ));
        }
    }
}

/// `fichier:ligne` trouvés dans les lignes (dédoublonnés, dans l'ordre).
fn emplacement(tok: &str) -> Option<String> {
    let tok = tok.trim_matches(|c: char| matches!(c, '(' | ')' | ',' | ';' | '"' | '\'' | '`' | '[' | ']' | '<' | '>'));
    // `test_calc.py:9:` (pytest), `calc_test.go:12:` (go) : le deux-points final n'est pas un numéro.
    let tok = tok.trim_end_matches(':');
    if tok.contains("node_modules") || tok.starts_with("http") || tok.starts_with("node:") || tok.len() < 3 {
        return None;
    }
    let chiffres = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    let (reste, n1) = tok.rsplit_once(':')?;
    if !chiffres(n1) {
        return None;
    }
    let (chemin, ligne) = match reste.rsplit_once(':') {
        Some((r2, n2)) if chiffres(n2) && r2.len() > 1 => (r2, n2),
        _ => (reste, n1),
    };
    let nom = chemin.rsplit(['/', '\\']).next().unwrap_or(chemin);
    let ext = nom.rsplit_once('.')?.1;
    if ext.is_empty() || ext.len() > 5 || !ext.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(format!("{}:{}", chemin, ligne))
}

fn emplacements(lignes: &[String]) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    for l in lignes {
        let corps = l.split_once('│').map(|x| x.1).unwrap_or(l);
        for tok in corps.split_whitespace() {
            if let Some(e) = emplacement(tok) {
                if !v.contains(&e) {
                    v.push(e);
                }
            }
        }
    }
    v
}

fn taille_h(b: u64) -> String {
    if b >= 1 << 20 {
        format!("{:.1} MB", b as f64 / (1u64 << 20) as f64)
    } else if b >= 1 << 10 {
        format!("{:.1} KB", b as f64 / 1024.0)
    } else {
        format!("{} B", b)
    }
}

// ─── Assemblage ──────────────────────────────────────────────────────────────

pub fn peek_fichier(path: &str, opt: &Options) -> Result<String, String> {
    let (t, taille) = charger(path)?;
    Ok(peek_texte(&t, path, taille, opt))
}

/// Classement de lignes pour un appelant externe (`read --autour`) : score de chaque ligne
/// pour `question`, avec le MÊME moteur que `peek` en mode question (termes comme `find`,
/// mots exacts x1,5, lexique FR->EN, accents, BM25 par ligne x couverture de termes).
/// Rend un score par ligne (0,0 = aucun terme), `lignes.len()` valeurs. Ne touche pas à `peek`.
pub fn scores_lignes(lignes: &[&str], question: &str) -> Vec<f32> {
    let n = lignes.len();
    let ts = termes(question);
    if n == 0 || ts.is_empty() {
        return vec![0.0; n];
    }
    let mut buf: Vec<u8> = Vec::with_capacity(lignes.iter().map(|l| l.len() + 1).sum());
    for (i, l) in lignes.iter().enumerate() {
        if i > 0 {
            buf.push(b'\n');
        }
        buf.extend_from_slice(l.as_bytes());
    }
    let Ok(t) = Texte::depuis(buf) else { return vec![0.0; n] };
    let mut s = lexique(&t, &ts).score;
    s.resize(n, 0.0);
    s
}

#[derive(PartialEq, Clone, Copy)]
enum Mode {
    Resume,
    Question,
    Erreurs,
}

pub fn peek_texte(t: &Texte, nom: &str, taille: u64, opt: &Options) -> String {
    let n = t.n();
    let cap = opt.max_lignes.clamp(4, 2000);
    let question = opt.question.trim();
    let mut mode = if opt.erreurs {
        Mode::Erreurs
    } else if question.is_empty() {
        Mode::Resume
    } else {
        Mode::Question
    };
    let ts = if question.is_empty() { Vec::new() } else { termes(question) };

    // Deux passages d'analyse en parallèle sur les octets.
    let (sig, lex) = std::thread::scope(|s| {
        let h = s.spawn(|| signaux(t));
        let lex = lexique(t, &ts);
        (h.join().expect("signal pass"), lex)
    });
    let classe = &sig.classe;
    let nb_err = classe.iter().filter(|&&c| c >= 2).count();
    let nb_avert = classe.iter().filter(|&&c| c == 1).count();
    let ext_bloc = opt.contexte.unwrap_or(BLOC_MAX).clamp(2, 60);
    let (blocs, blocs_total) = if mode != Mode::Question || nb_err > 0 { blocs_erreurs(t, classe, ext_bloc) } else { (Vec::new(), 0) };

    let mut entete = format!("{} — {}, {} lines", nom, taille_h(taille), n);
    let mut notes: Vec<String> = Vec::new();
    if !lex.absents.is_empty() {
        notes.push(format!("(no line contains: {})", lex.absents.join(", ")));
    }
    let mut fen: Vec<Fenetre> = Vec::new();
    let mut cache_blocs: usize = 0; // blocs distincts non montrés
    let mut cache_lignes: usize = 0; // fenêtres de question non montrées
    let mut montres_blocs = 0usize;

    let lex_a_des_hits = lex.score.iter().any(|&x| x > 0.0);
    // La question parle d'échec (échoue, erreur, pourquoi, why…) ET le log contient des signaux
    // d'échec : ceux-ci passent avant toute correspondance de termes, et une ligne de test
    // réussi (`... ok`, `✓`, `PASS`) n'est jamais un résultat, quel que soit son nom.
    let prio = mode == Mode::Question && nb_err > 0 && question_echec(question);
    // Score d'une ligne en mode question : BM25 + bonus d'échec (sans mot de la question, une ligne
    // d'erreur ne compte que si la question ne trouve rien du tout).
    let score_brut = |i: usize| -> f32 {
        let lx = lex.score.get(i).copied().unwrap_or(0.0);
        if prio {
            if classe[i] >= 2 {
                return PRIORITE_ECHEC + lx + if classe[i] == 3 { 4.0 } else { 3.0 };
            }
            if lx > 0.0 && ligne_reussie(t.ligne(i)) {
                return 0.0;
            }
        }
        let bonus = match classe[i] {
            3 => 4.0,
            2 => 3.0,
            1 => 0.3,
            _ => 0.0,
        };
        if lx > 0.0 || !lex_a_des_hits {
            lx + bonus
        } else {
            0.0
        }
    };
    // Mode question : chaque ligne est notée UNE fois (le candidat lit aussi ses deux voisines).
    let scores: Vec<f32> = if mode == Mode::Question { (0..n).map(score_brut).collect() } else { Vec::new() };
    let score_ligne = |i: usize| -> f32 {
        if scores.is_empty() {
            score_brut(i)
        } else {
            scores[i]
        }
    };

    if mode == Mode::Question {
        let (cb, ca) = opt.contexte.map(|c| (c, c)).unwrap_or((2, 3));
        let mut cand: Vec<(f32, u32)> = Vec::new();
        for i in 0..n {
            let s = score_ligne(i);
            if s > 0.0 {
                let l = if i > 0 { score_ligne(i - 1) } else { 0.0 };
                let r = if i + 1 < n { score_ligne(i + 1) } else { 0.0 };
                cand.push((s + 0.25 * (l + r), i as u32));
            }
        }
        if cand.is_empty() || lex.score.is_empty() && nb_err == 0 {
            notes.push(format!("(no line matches \"{}\": summary instead)", question));
            mode = Mode::Resume;
        } else {
            if cand.len() > 4000 {
                cand.select_nth_unstable_by(4000, |a, b| b.0.total_cmp(&a.0));
                cand.truncate(4000);
            }
            cand.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
            let mut pris = Pris::default();
            let mut total = 0usize;
            let mut cles: HashMap<u64, usize> = HashMap::new();
            let mut utilises: Vec<Fenetre> = Vec::new();
            for &(_, i) in &cand {
                let i = i as usize;
                if pris.couvre(i) {
                    continue;
                }
                // Une ligne d'échec se dédoublonne par son BLOC (en-tête + message + première
                // exception) : deux blocs de messages différents ne se replient jamais.
                let bloc = if classe[i] >= 2 {
                    let (a, h, fe) = debut_bloc(t, classe, i, -1);
                    Some((a, bloc_fin(t, classe, h, ext_bloc), fe))
                } else {
                    None
                };
                let k = match bloc {
                    Some((a, b, fe)) => cle_bloc(t, classe, a, b, fe) ^ 0xB10C_B10C_B10C_B10C,
                    None => cle_ligne(t, i),
                };
                if let Some(&w) = cles.get(&k) {
                    if w != usize::MAX {
                        utilises[w].rep += 1;
                        if utilises[w].ex.len() < 3 {
                            utilises[w].ex.push(i);
                        }
                    }
                    continue;
                }
                let (mut a, mut b) = match bloc {
                    Some((a, b, _)) => (a, b),
                    None => {
                        let (mut a, mut b) = (i.saturating_sub(cb), (i + ca).min(n - 1));
                        // Question d'échec : le contexte ne s'étend pas sur des tests réussis.
                        if prio {
                            while a < i && ligne_reussie(t.ligne(a)) {
                                a += 1;
                            }
                            while b > i && ligne_reussie(t.ligne(b)) {
                                b -= 1;
                            }
                        }
                        (a, b)
                    }
                };
                let reste = cap.saturating_sub(total);
                if pris.cout(a, b) > reste {
                    if reste < 3 {
                        cles.insert(k, usize::MAX);
                        cache_lignes += 1;
                        continue;
                    }
                    let avant = (i - a).min(reste / 3);
                    a = i - avant;
                    b = (a + reste - 1).min(b).max(i);
                }
                total += pris.cout(a, b);
                pris.ajoute(a, b);
                cles.insert(k, utilises.len());
                utilises.push(Fenetre { a, b, rep: 0, ex: Vec::new() });
            }
            fen = utilises;
        }
    }

    if mode == Mode::Erreurs {
        // Ordre du fichier, ou classés par la question si elle est donnée.
        let mut ordre: Vec<usize> = (0..blocs.len()).collect();
        if !question.is_empty() && !lex.score.is_empty() {
            let sc: Vec<f32> = blocs.iter().map(|b| (b.f.a..=b.f.b).map(&score_ligne).fold(0.0f32, f32::max)).collect();
            ordre.sort_by(|&x, &y| sc[y].total_cmp(&sc[x]).then(x.cmp(&y)));
        }
        let mut total = 0usize;
        for &k in &ordre {
            let b = &blocs[k].f;
            let reste = cap.saturating_sub(total);
            let taille_b = b.b - b.a + 1;
            if reste < 2 || (taille_b > reste && montres_blocs > 0 && reste < 4) {
                cache_blocs += 1;
                continue;
            }
            let mut f = b.clone();
            if taille_b > reste {
                f.b = f.a + reste - 1;
            }
            total += f.b - f.a + 1;
            fen.push(f);
            montres_blocs += 1;
        }
    }

    if mode == Mode::Resume {
        let tete = cap.min(4).min(n);
        let queue = (cap / 8 + 1).min(6).min(n);
        let bilans: Vec<usize> = sig.bilan.iter().rev().take(4).map(|&x| x as usize).collect();
        let reste = cap.saturating_sub(tete + queue + bilans.len()).max(2);
        fen.push(Fenetre { a: 0, b: tete.max(1) - 1, ..Default::default() });
        fen.push(Fenetre { a: n - queue.max(1), b: n - 1, ..Default::default() });
        for &b in &bilans {
            fen.push(Fenetre { a: b, b, ..Default::default() });
        }
        let mut total = 0usize;
        for bl in &blocs {
            let taille_b = bl.f.b - bl.f.a + 1;
            let r = reste.saturating_sub(total);
            if r < 2 {
                cache_blocs += 1;
                continue;
            }
            let mut f = bl.f.clone();
            if taille_b > r {
                f.b = f.a + r - 1;
            }
            total += f.b - f.a + 1;
            fen.push(f);
            montres_blocs += 1;
        }
    }

    // Rendu.
    let mut lignes: Vec<String> = Vec::new();
    if mode == Mode::Resume || mode == Mode::Erreurs {
        let distincts = blocs.len();
        if nb_err > 0 {
            lignes.push(format!(
                "signals : {} error line(s) in {} block(s) ({} distinct), {} warning line(s)",
                nb_err, blocs_total, distincts, nb_avert
            ));
        } else {
            lignes.push(format!("signals : no error line found, {} warning line(s)", nb_avert));
        }
    }
    let ac = lex.ac.as_ref();
    let mut corps: Vec<String> = Vec::new();
    rendre(t, fen.clone(), ac, &mut corps);
    if mode == Mode::Erreurs && montres_blocs == 0 && nb_err == 0 {
        corps.push("(no failure block: no FAIL, error, panic, AssertionError… line)".into());
    }
    let locs = if mode == Mode::Resume || mode == Mode::Erreurs {
        let e = emplacements(&corps);
        if e.is_empty() {
            None
        } else {
            Some(format!("locations : {}", e.iter().take(10).cloned().collect::<Vec<_>>().join(", ")))
        }
    } else {
        None
    };
    let mut sortie = String::new();
    sortie.push_str(&entete);
    sortie.push('\n');
    entete.clear();
    for l in notes.iter().chain(lignes.iter()).chain(corps.iter()) {
        sortie.push_str(l);
        sortie.push('\n');
    }
    if let Some(l) = locs {
        sortie.push_str(&l);
        sortie.push('\n');
    }
    // Ce qui est caché + suite utile.
    let mut suite: Option<String> = None;
    if cache_blocs > 0 {
        sortie.push_str(&format!("… {} more distinct failure block(s) not shown\n", cache_blocs));
        suite = Some(format!("peek {} --erreurs --max-lignes {}", nom, cap * 2));
    } else if cache_lignes > 0 {
        sortie.push_str(&format!("… {} more matching spot(s) not shown (cap {} lines)\n", cache_lignes, cap));
        suite = Some(format!("peek {} \"{}\" --max-lignes {}", nom, question, cap * 2));
    } else if mode == Mode::Resume && nb_err > 0 {
        suite = Some(format!("peek {} --erreurs", nom));
    } else if mode == Mode::Resume {
        suite = Some(format!("peek {} \"<question>\"", nom));
    }
    if let Some(s) = suite {
        sortie.push_str(&format!("next : {}\n", s));
    }
    sortie
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn opt(q: &str, erreurs: bool, max: usize) -> Options {
        Options { question: q.into(), erreurs, max_lignes: max, contexte: None }
    }

    fn texte(s: &str) -> Texte {
        Texte::depuis(s.as_bytes().to_vec()).unwrap()
    }

    fn run(s: &str, q: &str, erreurs: bool, max: usize) -> String {
        peek_texte(&texte(s), "log.txt", s.len() as u64, &opt(q, erreurs, max))
    }

    const VITEST: &str = "\
 RUN  v4.0.1 C:/proj

 ✓ src/lib/a.test.ts (12 tests) 15ms
 ❯ src/lib/forms.test.ts (3 tests | 1 failed) 40ms
   ✓ forms > validates required 2ms
   × forms > rejects duplicate answers 7ms
   ✓ forms > keeps order 1ms
 ✓ src/lib/b.test.ts (8 tests) 9ms

⎯⎯⎯⎯⎯⎯⎯ Failed Tests 1 ⎯⎯⎯⎯⎯⎯⎯

 FAIL  src/lib/forms.test.ts > forms > rejects duplicate answers
AssertionError: expected 2 to be 1 // Object.is equality

- Expected
+ Received

- 1
+ 2

 ❯ src/lib/forms.test.ts:42:18
     40|   const r = submit(a);
     41|   const s = submit(a);
     42|   expect(s.count).toBe(1);
       |                   ^
 ❯ node_modules/vitest/dist/runner.js:10:2
    at node_modules/vitest/a.js:1:1
    at node_modules/vitest/b.js:2:2
    at node_modules/vitest/c.js:3:3

⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯[1/1]⎯

 Test Files  1 failed | 2 passed (3)
      Tests  1 failed | 22 passed (23)
   Duration  1.20s
";

    #[test]
    fn erreurs_vitest_donne_test_assertion_et_emplacement() {
        let o = run(VITEST, "", true, 60);
        assert!(o.contains("FAIL  src/lib/forms.test.ts > forms > rejects duplicate answers"), "{o}");
        assert!(o.contains("AssertionError: expected 2 to be 1"), "{o}");
        assert!(o.contains("locations : src/lib/forms.test.ts:42"), "{o}");
        assert!(!o.contains("keeps order"), "les tests réussis ne doivent pas apparaître : {o}");
        assert!(o.contains("stack frame line(s)"), "{o}");
    }

    #[test]
    fn resume_sans_question() {
        let o = run(VITEST, "", false, 60);
        assert!(o.contains("RUN  v4.0.1"), "{o}");
        assert!(o.contains("Test Files  1 failed"), "{o}");
        assert!(o.contains("signals :"), "{o}");
        assert!(o.contains("AssertionError"), "{o}");
        assert!(o.contains("Duration"), "{o}");
    }

    #[test]
    fn question_trouve_la_ligne_avec_son_numero() {
        let mut s = String::new();
        for i in 0..5000 {
            s.push_str(&format!("2026-11-19 12:00:{:02} INFO request {} served ok\n", i % 60, i));
        }
        s.push_str("2026-11-19 12:01:00 INFO worker pool saturated, queue length 912\n");
        for i in 0..100 {
            s.push_str(&format!("2026-11-19 12:02:00 INFO request {} served ok\n", i));
        }
        let o = run(&s, "worker pool saturated", false, 60);
        assert!(o.contains("5001│2026-11-19 12:01:00 INFO worker pool saturated"), "{o}");
        assert!(o.lines().count() < 20, "{o}");
    }

    #[test]
    fn plafond_de_lignes_respecte() {
        let mut s = String::new();
        for i in 0..3000 {
            let w: String = i.to_string().chars().map(|c| (b'a' + (c as u8 - b'0')) as char).collect();
            s.push_str(&format!("line {} timeout while connecting to host-{}\n", w, w));
        }
        let o = run(&s, "timeout connecting", false, 20);
        let n = o.lines().filter(|l| l.contains('│')).count();
        assert!(n <= 20, "{n} lignes\n{o}");
        assert!(o.contains("not shown"), "{o}");
    }

    #[test]
    fn erreurs_identiques_dedoublonnees() {
        let mut s = String::new();
        for i in 0..200 {
            s.push_str(&format!("[{}] job ok\n", i));
            s.push_str(&format!("[{}] error: connection refused to 10.0.0.{}\n", i, i % 7));
        }
        let o = run(&s, "", true, 60);
        assert_eq!(o.matches("connection refused").count(), 1, "{o}");
        assert!(o.contains("similar block(s) skipped"), "{o}");
    }

    #[test]
    fn sortie_cargo() {
        let s = "\
   Compiling cortex v0.5.3
warning: unused variable: `y`
 --> src/lib.rs:3:9
  |
3 |     let y = 2;
  |         ^ help: prefix it
error[E0308]: mismatched types
 --> src/main.rs:10:18
  |
10|     let x: u32 = \"a\";
  |            ---   ^^^ expected `u32`, found `&str`
  |            |
  |            expected due to this

error: could not compile `cortex` (bin \"cortex\") due to 1 previous error
";
        let o = run(s, "", true, 60);
        assert!(o.contains("error[E0308]: mismatched types"), "{o}");
        assert!(o.contains("src/main.rs:10"), "{o}");
        assert!(o.contains("locations : "), "{o}");
        assert!(!o.contains("Compiling cortex"), "{o}");
    }

    #[test]
    fn json_minifie_extraits_autour_du_terme() {
        let mut s = String::from("{\"items\":[");
        for i in 0..4000 {
            s.push_str(&format!("{{\"id\":{},\"name\":\"user{}\"}},", i, i));
        }
        s.push_str("{\"id\":9999,\"email\":\"zorglub@example.org\"}]}");
        let o = run(&s, "zorglub", false, 60);
        assert!(o.contains("zorglub@example.org"), "{o}");
        assert!(o.len() < 1500, "ligne énorme non tronquée : {} octets", o.len());
    }

    #[test]
    fn question_et_erreurs_classe_les_blocs() {
        let s = "\
FAIL  a.test.ts > alpha one
AssertionError: expected 1 to be 2
  at a.test.ts:1:1
  at a.test.ts:2:2

FAIL  b.test.ts > beta two
AssertionError: expected 3 to be 4
  at b.test.ts:1:1
  at b.test.ts:2:2

FAIL  c.test.ts > gamma three
AssertionError: expected 5 to be 6
  at c.test.ts:1:1
  at c.test.ts:2:2
";
        let o = run(s, "beta", true, 5);
        assert!(o.contains("beta two"), "{o}");
        assert!(!o.contains("alpha one"), "{o}");
        assert!(o.contains("more distinct failure block(s) not shown"), "{o}");
    }

    #[test]
    fn utf16_crlf_et_binaire() {
        let mut v: Vec<u8> = vec![0xFF, 0xFE];
        for u in "ok\r\n FAIL  x.ts > t\r\nAssertionError: boom\r\n".encode_utf16() {
            v.extend_from_slice(&u.to_le_bytes());
        }
        let t = Texte::depuis(v).unwrap();
        let o = peek_texte(&t, "u16.txt", 10, &opt("", true, 60));
        assert!(o.contains("2│ FAIL  x.ts > t"), "{o}");
        assert!(!o.contains('\r'));
        assert!(Texte::depuis(vec![b'a', 0, b'b']).is_err());
        assert!(Texte::depuis(Vec::new()).is_err());
    }

    #[test]
    fn question_sans_resultat_retombe_sur_le_resume() {
        let o = run("alpha\nbeta\ngamma\n", "zzzqqq", false, 60);
        assert!(o.contains("no line matches"), "{o}");
        assert!(o.contains("1│alpha"), "{o}");
    }

    #[test]
    fn question_francaise_sur_log_anglais() {
        let mut s = String::new();
        for i in 0..2000 {
            s.push_str(&format!("tick {} all good\n", i));
        }
        s.push_str("connection refused by upstream db-7\n");
        for i in 0..50 {
            s.push_str(&format!("tick {} all good\n", i));
        }
        let o = run(&s, "pourquoi la connexion est refusée ?", false, 60);
        assert!(o.contains("2001│connection refused by upstream db-7"), "{o}");
    }

    #[test]
    fn echecs_de_tests_numerotes_ne_sont_pas_fusionnes() {
        let s = " FAIL  a.test.ts > case1\nAssertionError: expected 1 to be 2\n\n FAIL  a.test.ts > case2\nAssertionError: expected 1 to be 2\n";
        let o = run(s, "", true, 60);
        assert!(o.contains("case1") && o.contains("case2"), "{o}");
    }

    #[test]
    fn question_precise_n_ajoute_pas_les_erreurs_sans_rapport() {
        let mut s = String::from(" FAIL  a.test.ts > unrelated\nAssertionError: boom\n\n");
        for i in 0..300 {
            s.push_str(&format!("filler line number {}\n", i));
        }
        s.push_str("the zorglub handler started\n");
        let o = run(&s, "zorglub", false, 60);
        assert!(o.contains("zorglub handler"), "{o}");
        assert!(!o.contains("unrelated"), "{o}");
    }

    #[test]
    fn emplacements_fichier_ligne() {
        assert_eq!(emplacement("src/a.ts:12:5").as_deref(), Some("src/a.ts:12"));
        assert_eq!(emplacement("(C:/x/y.rs:7)").as_deref(), Some("C:/x/y.rs:7"));
        assert_eq!(emplacement("http://x.io:80"), None);
        assert_eq!(emplacement("12:30:45"), None);
        assert_eq!(emplacement("node_modules/a/b.js:1:1"), None);
    }

    // ─── Accents ────────────────────────────────────────────────────────────────

    fn filler(n: usize) -> String {
        (0..n).map(|i| format!("tick {} tout va bien\n", i)).collect()
    }

    #[test]
    fn accents_log_accentue_question_sans_accent() {
        let mut s = filler(1500);
        s.push_str("le test a échoué : délai dépassé sur la passerelle\n");
        s.push_str(&filler(60));
        let o = run(&s, "pourquoi le test a echoue ?", false, 60);
        assert!(o.contains("1501│le test a échoué"), "{o}");
        // Lettre accentuée à l'intérieur d'un terme plus long, et accent circonflexe.
        let mut s = filler(800);
        s.push_str("la fenêtre de déploiement est fermée\n");
        s.push_str(&filler(40));
        assert!(run(&s, "fenetre deploiement", false, 60).contains("801│la fenêtre de déploiement"));
    }

    #[test]
    fn accents_log_sans_accent_question_accentuee() {
        let mut s = filler(1500);
        s.push_str("le test a echoue : delai depasse sur la passerelle\n");
        s.push_str(&filler(60));
        let o = run(&s, "pourquoi le test a échoué ?", false, 60);
        assert!(o.contains("1501│le test a echoue"), "{o}");
        // Mot « exact » (identifiant avec trait de soulignement) écrit avec accent dans la question.
        let mut s = filler(900);
        s.push_str("module cafe_ete charge\n");
        s.push_str(&filler(40));
        assert!(run(&s, "café_été", false, 60).contains("901│module cafe_ete charge"));
    }

    #[test]
    fn accents_majuscule_cedille_et_ligature() {
        let mut s = filler(700);
        s.push_str("Échec de la connexion au serveur\n");
        s.push_str("le cœur du service ne répond plus, façade fermée\n");
        s.push_str(&filler(40));
        assert!(run(&s, "echec connexion", false, 60).contains("701│Échec de la connexion"));
        assert!(run(&s, "coeur", false, 60).contains("702│le cœur du service"));
        assert!(run(&s, "facade", false, 60).contains("façade fermée"));
    }

    #[test]
    fn accents_motifs_bornes_et_sans_cout_sans_accent() {
        // Aucun accent dans le texte : le terme, et sa forme repliée si besoin, rien d'autre.
        assert_eq!(motifs_terme("echoue", &[]), vec!["echoue".to_string()]);
        assert_eq!(motifs_terme("échoué", &[]), vec!["échoué".to_string(), "echoue".to_string()]);
        assert!(accents_presents(b"plain ascii log with \xc3\x97 sign").is_empty());
        let presents = accents_presents("é è ê à ù ô î ï ç É".as_bytes());
        assert!(presents.contains(&'é') && presents.contains(&'É') && presents.contains(&'ç'));
        let v = motifs_terme("echoue", &presents);
        assert!(v.contains(&"échoué".to_string()) && v.contains(&"echoue".to_string()), "{v:?}");
        // Un terme très accentuable reste borné.
        let v = motifs_terme("deconnexion_ete_echeance", &presents);
        assert!(v.len() <= VARIANTES_MAX + 2, "{}", v.len());
    }

    // ─── pytest ─────────────────────────────────────────────────────────────────

    // Sortie RÉELLE de `python -m pytest` (pytest 9.1.0, Python 3.13, Windows), chemin raccourci.
    const PYTEST: &str = r##"============================= test session starts =============================
platform win32 -- Python 3.13.6, pytest-9.1.0, pluggy-1.6.0
rootdir: C:\proj
collected 6 items

test_calc.py F.F..F                                                      [100%]

================================== FAILURES ===================================
_________________________________ test_total __________________________________

    def test_total():
>       assert total([1, 2]) == 4
E       assert 3 == 4
E        +  where 3 = total([1, 2])

test_calc.py:9: AssertionError
_________________________________ test_div[0] _________________________________

n = 0

    @pytest.mark.parametrize("n", range(3))
    def test_div(n):
>       assert 10 // n > 0
               ^^^^^^^
E       ZeroDivisionError: integer division or modulo by zero

test_calc.py:18: ZeroDivisionError
_________________________________ test_lookup _________________________________

    def test_lookup():
        d = {"a": 1}
>       assert d["b"] == 2
               ^^^^^^
E       KeyError: 'b'

test_calc.py:23: KeyError
=========================== short test summary info ===========================
FAILED test_calc.py::test_total - assert 3 == 4
FAILED test_calc.py::test_div[0] - ZeroDivisionError: integer division or mod...
FAILED test_calc.py::test_lookup - KeyError: 'b'
========================= 3 failed, 3 passed in 0.39s =========================
"##;

    fn lignes_de(t: &Texte, i: u32) -> String {
        String::from_utf8_lossy(t.ligne(i as usize)).to_string()
    }

    /// Le fichier a au moins 20 000 lignes, la sortie résumée est au moins 10 fois plus courte,
    /// et la ligne de bilan `bilan` est reconnue comme telle.
    fn verifie_gros(log: &str, bilan: &str) -> String {
        let n = log.lines().count();
        assert!(n >= 20_000, "{n} lignes");
        let t = texte(log);
        let o = peek_texte(&t, "gros.log", log.len() as u64, &opt("", false, 60));
        assert!(o.len() * 10 <= log.len(), "sortie {} octets pour {} octets", o.len(), log.len());
        assert!(o.lines().count() * 10 <= n, "{} lignes pour {n}", o.lines().count());
        let sig = signaux(&t);
        assert!(
            sig.bilan.iter().any(|&b| lignes_de(&t, b).contains(bilan)),
            "bilan {bilan:?} non reconnu : {:?}",
            sig.bilan.iter().map(|&b| lignes_de(&t, b)).collect::<Vec<_>>()
        );
        o
    }

    fn gros_pytest() -> String {
        let (tete, _) = PYTEST.split_once("_________________________________ test_total").unwrap();
        let mut s = tete.to_string();
        let mut resume = String::new();
        let mut i = 0;
        while s.lines().count() + resume.lines().count() < 20_200 {
            // Titre centré sur 80 colonnes comme pytest : la largeur des `_` change avec `{i}`.
            let titre = format!(" test_div[{i}] ");
            let g = (80 - titre.len()) / 2;
            let d = 80 - titre.len() - g;
            s.push_str(&format!(
                "{}{}{}\n\nn = {i}\n\n    @pytest.mark.parametrize(\"n\", range(3000))\n    def test_div(n):\n>       assert 10 // 0 > n\n               ^^^^^^^^\nE       ZeroDivisionError: integer division or modulo by zero\n\ntest_big.py:5: ZeroDivisionError\n",
                "_".repeat(g),
                titre,
                "_".repeat(d)
            ));
            for k in 0..3 {
                s.push_str(&format!("test_big.py::test_ok[{}] PASSED   [{}%]\n", i * 3 + k, i % 100));
            }
            resume.push_str(&format!("FAILED test_big.py::test_div[{i}] - ZeroDivisionError: integer division or mod...\n"));
            i += 1;
        }
        s.push_str("=========================== short test summary info ===========================\n");
        s.push_str(&resume);
        s.push_str(&format!("====================== {i} failed, 3000 passed in 19.69s ======================\n"));
        s
    }

    #[test]
    fn pytest_blocs_d_echec_et_bilan() {
        let o = run(PYTEST, "", true, 60);
        assert!(o.contains("test_total"), "{o}");
        assert!(o.contains("E       assert 3 == 4"), "{o}");
        assert!(o.contains("E       ZeroDivisionError: integer division or modulo by zero"), "{o}");
        assert!(o.contains("E       KeyError: 'b'"), "{o}");
        assert!(o.contains("locations : test_calc.py:9"), "{o}");
        assert!(!o.contains("collected 6 items"), "{o}");
        // Résumé : le bilan pytest est reconnu et affiché.
        let t = texte(PYTEST);
        let sig = signaux(&t);
        assert!(sig.bilan.iter().any(|&b| lignes_de(&t, b).contains("3 failed, 3 passed in 0.39s")));
        let r = run(PYTEST, "", false, 60);
        assert!(r.contains("3 failed, 3 passed"), "{r}");
        assert!(r.contains("signals :"), "{r}");
    }

    #[test]
    fn pytest_mode_silencieux_bilan_sans_egal() {
        let t = texte("test_a.py F.\n\nFAILED test_a.py::test_x - assert 1 == 2\n1 failed, 1 passed in 0.05s\n");
        let sig = signaux(&t);
        assert_eq!(sig.bilan.len(), 1);
        assert!(lignes_de(&t, sig.bilan[0]).starts_with("1 failed, 1 passed"));
        assert_eq!(sig.classe[2], 3, "FAILED est une tête d'échec");
    }

    #[test]
    fn pytest_gros_log_20000_lignes() {
        let log = gros_pytest();
        let o = verifie_gros(&log, "failed, 3000 passed in 19.69s");
        assert!(o.contains("ZeroDivisionError: integer division or modulo by zero"), "{o}");
        assert!(o.contains("test_big.py:5"), "{o}");
        assert!(o.contains("similar block(s) skipped"), "{o}");
        assert_eq!(o.matches("E       ZeroDivisionError").count(), 1, "le même échec une seule fois : {o}");
        let distincts: usize = o.split("block(s) (").nth(1).unwrap().split(' ').next().unwrap().parse().unwrap();
        assert!(distincts <= 3, "les titres centrés de largeurs différentes se dédoublonnent : {o}");
        assert!(o.contains("failed, 3000 passed"), "{o}");
    }

    // ─── jest ───────────────────────────────────────────────────────────────────

    // Format documenté de jest 29 (jest n'est pas installé sur ce poste : fixture écrite à la main).
    const JEST: &str = r##" FAIL  src/sum.test.js
  ● sum › adds numbers

    expect(received).toBe(expected) // Object.is equality

    Expected: 4
    Received: 3

      3 | test('adds numbers', () => {
    > 4 |   expect(sum(1, 2)).toBe(4);
        |                     ^
      5 | });
      6 |

      at Object.<anonymous> (src/sum.test.js:4:21)

  ● sum › handles null

    TypeError: Cannot read properties of null (reading 'length')

      at sum (src/sum.js:3:18)
      at Object.<anonymous> (src/sum.test.js:9:16)

 PASS  src/other.test.js
  ● Console

    console.log
      hello

      at Object.<anonymous> (src/other.test.js:2:9)

Test Suites: 1 failed, 1 passed, 2 total
Tests:       2 failed, 3 passed, 5 total
Snapshots:   0 total
Time:        0.512 s
Ran all test suites.
"##;

    fn gros_jest() -> String {
        let bloc = " FAIL  src/sum.test.js\n  ● sum › adds numbers\n\n    expect(received).toBe(expected) // Object.is equality\n\n    Expected: 4\n    Received: 3\n\n      3 | test('adds numbers', () => {\n    > 4 |   expect(sum(1, 2)).toBe(4);\n        |                     ^\n      5 | });\n      6 |\n\n      at Object.<anonymous> (src/sum.test.js:4:21)\n\n";
        let mut s = String::new();
        let mut i = 0;
        while s.lines().count() < 20_200 {
            s.push_str(bloc);
            for k in 0..2 {
                s.push_str(&format!(" PASS  src/ok{}.test.js\n  console.log\n    run {} step {}\n\n", i % 50, i, k));
            }
            i += 1;
        }
        s.push_str(&format!("Test Suites: {i} failed, 50 passed, {} total\nTests:       {i} failed, 900 passed, {} total\nSnapshots:   0 total\nTime:        93.2 s\n", i + 50, i + 900));
        s
    }

    #[test]
    fn jest_blocs_d_echec_et_bilan() {
        let o = run(JEST, "", true, 60);
        assert!(o.contains("● sum › adds numbers"), "{o}");
        assert!(o.contains("Expected: 4") && o.contains("Received: 3"), "{o}");
        assert!(o.contains("● sum › handles null"), "{o}");
        assert!(o.contains("TypeError: Cannot read properties of null"), "{o}");
        assert!(!o.contains("● Console"), "{o}");
        assert!(!o.contains("console.log"), "{o}");
        assert!(o.contains("locations : "), "{o}");
        assert!(o.contains("src/sum.test.js:4"), "{o}");
        let t = texte(JEST);
        let sig = signaux(&t);
        assert!(sig.bilan.iter().any(|&b| lignes_de(&t, b).starts_with("Tests:")));
        assert!(sig.bilan.iter().any(|&b| lignes_de(&t, b).starts_with("Test Suites:")));
        let r = run(JEST, "", false, 60);
        assert!(r.contains("Tests:       2 failed, 3 passed, 5 total"), "{r}");
    }

    #[test]
    fn jest_gros_log_20000_lignes() {
        let log = gros_jest();
        let o = verifie_gros(&log, "failed, 900 passed");
        assert!(o.contains("● sum › adds numbers"), "{o}");
        assert!(o.contains("Expected: 4"), "{o}");
        assert_eq!(o.matches("● sum › adds numbers").count(), 1, "{o}");
        assert!(o.contains("similar block(s) skipped"), "{o}");
        assert!(o.contains("Tests:"), "{o}");
    }

    // ─── go test ────────────────────────────────────────────────────────────────

    // Format documenté de `go test -v` (go n'est pas installé sur ce poste : fixture écrite à la main).
    const GOTEST: &str = "=== RUN   TestAdd
--- PASS: TestAdd (0.00s)
=== RUN   TestDiv
    calc_test.go:14: Div(7, 2) = 3; want 4
--- FAIL: TestDiv (0.00s)
=== RUN   TestPanic
--- FAIL: TestPanic (0.00s)
panic: runtime error: integer divide by zero [recovered]
\tpanic: runtime error: integer divide by zero

goroutine 7 [running]:
testing.tRunner.func1.2({0x4d6c80, 0x5a1e90})
\t/usr/local/go/src/testing/testing.go:1631 +0x24a
testing.tRunner.func1()
\t/usr/local/go/src/testing/testing.go:1634 +0x377
panic({0x4d6c80?, 0x5a1e90?})
\t/usr/local/go/src/runtime/panic.go:770 +0x132
example.com/calc.Div(...)
\t/home/dev/calc/calc.go:9
example.com/calc.TestPanic(0xc000007a00)
\t/home/dev/calc/calc_test.go:22 +0x18
testing.tRunner(0xc000007a00, 0x5a0f38)
\t/usr/local/go/src/testing/testing.go:1689 +0xfb
created by testing.(*T).Run in goroutine 1
\t/usr/local/go/src/testing/testing.go:1742 +0x390
exit status 2
FAIL\texample.com/calc\t0.006s
";

    fn gros_go() -> String {
        let mut s = String::new();
        let mut i = 0;
        while s.lines().count() < 20_200 {
            s.push_str(&format!("=== RUN   TestOk{i}\n--- PASS: TestOk{i} (0.00s)\n=== RUN   TestDiv\n    calc_test.go:14: Div(7, 2) = 3; want 4\n--- FAIL: TestDiv (0.00s)\n"));
            s.push_str(&format!("=== RUN   TestSlow{i}\n    slow_test.go:3: waiting {i} ms\n--- PASS: TestSlow{i} (0.00s)\n"));
            i += 1;
        }
        s.push_str("FAIL\nexit status 1\nFAIL\texample.com/calc\t0.912s\n");
        s
    }

    #[test]
    fn go_test_blocs_d_echec_et_bilan() {
        let o = run(GOTEST, "", true, 60);
        assert!(o.contains("--- FAIL: TestDiv"), "{o}");
        assert!(o.contains("calc_test.go:14: Div(7, 2) = 3; want 4"), "le message précède la tête en mode -v : {o}");
        assert!(o.contains("--- FAIL: TestPanic"), "{o}");
        assert!(o.contains("panic: runtime error: integer divide by zero"), "{o}");
        assert!(o.contains("calc_test.go:22"), "{o}");
        assert!(!o.contains("PASS: TestAdd"), "{o}");
        assert!(o.contains("locations : "), "{o}");
        assert!(o.contains("calc_test.go:14"), "{o}");
        let t = texte(GOTEST);
        let sig = signaux(&t);
        assert!(sig.bilan.iter().any(|&b| lignes_de(&t, b).starts_with("FAIL\texample.com/calc")));
        let r = run(GOTEST, "", false, 60);
        assert!(r.contains("FAIL\texample.com/calc\t0.006s"), "{r}");
    }

    #[test]
    fn go_test_gros_log_20000_lignes() {
        let log = gros_go();
        let o = verifie_gros(&log, "example.com/calc\t0.912s");
        assert!(o.contains("--- FAIL: TestDiv"), "{o}");
        assert!(o.contains("calc_test.go:14: Div(7, 2) = 3; want 4"), "{o}");
        assert_eq!(o.matches("--- FAIL: TestDiv").count(), 1, "{o}");
        assert!(o.contains("similar block(s) skipped"), "{o}");
    }

    // ─── eslint ─────────────────────────────────────────────────────────────────

    // Sortie RÉELLE d'eslint 9.39 (format stylish), chemins raccourcis.
    const ESLINT: &str = "
C:\\proj\\a.js
  1:7   error    'unused' is assigned a value but never used  no-unused-vars
  2:1   warning  Unexpected console statement                 no-console
  2:1   error    'console' is not defined                     no-undef
  3:5   error    'a' is not defined                           no-undef
  3:7   error    Expected '===' and instead saw '=='          eqeqeq
  3:15  error    'b' is not defined                           no-undef

C:\\proj\\b.js
  1:26  error    Expected '===' and instead saw '=='  eqeqeq
  2:1   warning  Unexpected console statement         no-console
  2:1   error    'console' is not defined             no-undef

C:\\proj\\eslint.config.js
  1:1  error  'module' is not defined  no-undef

\u{2716} 10 problems (8 errors, 2 warnings)
";

    fn gros_eslint() -> String {
        let mut s = String::new();
        let mut i = 0;
        while s.lines().count() < 20_200 {
            s.push_str(&format!("\nC:\\proj\\src\\f{i}.js\n  1:7   error    'unused' is assigned a value but never used  no-unused-vars\n  2:1   warning  Unexpected console statement                 no-console\n  2:1   error    'console' is not defined                     no-undef\n  3:7   error    Expected '===' and instead saw '=='          eqeqeq\n"));
            i += 1;
        }
        s.push_str(&format!("\n\u{2716} {} problems ({} errors, {} warnings)\n", i * 4, i * 3, i));
        s
    }

    #[test]
    fn eslint_blocs_d_echec_et_bilan() {
        let o = run(ESLINT, "", true, 60);
        assert!(o.contains("C:\\proj\\a.js"), "le fichier précède ses erreurs : {o}");
        assert!(o.contains("1:7   error    'unused' is assigned a value but never used  no-unused-vars"), "{o}");
        assert!(o.contains("3:7   error    Expected '===' and instead saw '=='"), "{o}");
        assert!(o.contains("'module' is not defined"), "{o}");
        let t = texte(ESLINT);
        let sig = signaux(&t);
        assert!(sig.bilan.iter().any(|&b| lignes_de(&t, b).contains("10 problems (8 errors, 2 warnings)")));
        assert_eq!(sig.classe.iter().filter(|&&c| c == 1).count(), 2, "deux avertissements");
        let r = run(ESLINT, "", false, 60);
        assert!(r.contains("10 problems (8 errors, 2 warnings)"), "{r}");
    }

    #[test]
    fn eslint_gros_log_20000_lignes() {
        let log = gros_eslint();
        let o = verifie_gros(&log, "problems (");
        assert!(o.contains("is not defined"), "{o}");
        assert!(o.contains("similar block(s) skipped"), "{o}");
        assert!(o.contains("problems ("), "{o}");
    }

    #[test]
    fn formats_ne_declenchent_pas_a_tort() {
        // Du texte courant ne devient ni échec ni bilan.
        let t = texte("_private_name = 1\n__init__ module loaded\nthe job passed in 3s\nerror handling is fine\nwarning level raised\nE   x\nTime: 12:00\n");
        let sig = signaux(&t);
        assert!(sig.classe.iter().all(|&c| c <= 2), "{:?}", sig.classe);
        assert!(sig.classe.iter().filter(|&&c| c == 3).count() == 0, "{:?}", sig.classe);
        assert_eq!(sig.classe[5], 2, "« E   x » est un détail pytest");
        assert!(!sig.bilan.contains(&2), "{:?}", sig.bilan);
    }

    // ─── Lot P1b : tests réussis jamais en signal d'échec ; blocs de messages distincts ───

    #[test]
    fn ligne_reussie_reconnait_les_formes_de_succes() {
        for l in [
            "test erreur::tests::display_error_0 ... ok",
            "test panic::tests::should_panic_on_overflow_0 - should panic ... ok",
            "   ✓ forms > affiche une erreur de validation 3ms",
            "PASS src/lib/error-handling.test.ts",
            "tests/test_err.py::test_error_path PASSED   [ 33%]",
            "--- PASS: TestPanicRecover (0.00s)",
            "ok  \tgithub.com/x/errors\t0.004s",
            "    retry_after_failed_connection passed (12ms)",
            "    retry_after_failed_connection passed (0.5 s)",
        ] {
            assert!(ligne_reussie(l.as_bytes()), "devrait être un succès : {l}");
        }
        for l in [
            "test journal::tests::solde ... FAILED",
            "test result: FAILED. 9398 passed; 2 failed; 0 ignored",
            "2 failed, 3 passed in 0.12s",
            "=== 2 failed, 3 passed in 0.12s ===",
            " Test Files  1 failed | 6 passed (7)",
            "      Tests  2 failed | 47 passed (49)",
            "thread 'x' panicked at src/a.rs:1:2:",
            "   ✕ forms > rejects duplicate answers 7ms",
            "FAIL src/lib/forms.test.ts",
            "",
        ] {
            assert!(!ligne_reussie(l.as_bytes()), "ne doit pas être un succès : {l}");
        }
    }

    #[test]
    fn test_reussi_au_nom_d_erreur_n_est_pas_un_signal() {
        let t = texte("   ✓ forms > message Error: champ requis 3ms\n   ✓ forms > gere le panicked at du worker 1ms\nPASS src/error: fatal.test.ts\n   ✕ forms > vrai echec 4ms\nError: boom\n");
        let sig = signaux(&t);
        assert_eq!(&sig.classe[..3], &[0, 0, 0], "{:?}", sig.classe);
        assert_eq!(sig.classe[3], 3, "la puce ✕ reste un échec");
        assert_eq!(sig.classe[4], 2, "Error: reste un détail d'échec");
    }

    #[test]
    fn question_echec_detecte_les_questions_d_echec() {
        for q in [
            "Pourquoi cargo test échoue-t-il ?",
            "why does it fail",
            "quelles erreurs eslint",
            "ça plante",
            "le build est cassé",
            "test KO",
        ] {
            assert!(question_echec(q), "{q}");
        }
        for q in ["où est configuré le port", "liste les modules chargés", "how many workers start"] {
            assert!(!question_echec(q), "{q}");
        }
    }

    /// Log cargo : beaucoup de tests RÉUSSIS dont le nom contient error / failed / panic, puis
    /// deux panics de messages différents en fin de log.
    fn log_cargo_leurres(n: usize) -> String {
        let mut s = String::from("running 9000 tests\n");
        for i in 0..n {
            s.push_str(&format!("test erreur::tests::display_error_{i} ... ok\n"));
            s.push_str(&format!("test reseau::tests::retry_after_failed_connection_{i} ... ok\n"));
            s.push_str(&format!("test panic::tests::should_panic_on_overflow_{i} - should panic ... ok\n"));
            s.push_str(&format!("test cache::tests::lit_la_ligne_{i} ... ok\n"));
        }
        s.push_str("\nfailures:\n\n---- a::tests::un stdout ----\n\n");
        s.push_str("thread 'a::tests::un' panicked at src/a.rs:10:5:\nassertion `left == right` failed: checksum invalide\n  left: 7\n right: 0\nstack backtrace:\n   0: rust_begin_unwind\n\n");
        s.push_str("---- b::tests::deux stdout ----\n\n");
        s.push_str("thread 'b::tests::deux' panicked at src/b.rs:20:9:\ncalled `Option::unwrap()` on a `None` value\nstack backtrace:\n   0: rust_begin_unwind\n\n");
        s.push_str("failures:\n    a::tests::un\n    b::tests::deux\n\ntest result: FAILED. 8998 passed; 2 failed; 0 ignored\n\nerror: test failed, to rerun pass `--lib`\n");
        s
    }

    #[test]
    fn question_d_echec_sans_terme_dans_le_log_montre_les_vrais_panics() {
        // « échoue-t-il » et « cargo » ne figurent pas dans le log : seuls les signaux d'échec parlent.
        let log = log_cargo_leurres(120);
        let o = run(&log, "pourquoi cargo test échoue-t-il ?", false, 60);
        for attendu in ["src/a.rs:10:5", "checksum invalide", "left: 7", "src/b.rs:20:9", "called `Option::unwrap()` on a `None` value"] {
            assert!(o.contains(attendu), "manque « {attendu} » :\n{o}");
        }
        assert!(!o.contains("display_error_"), "un test réussi n'est pas une réponse :\n{o}");
        assert!(!o.contains("should_panic_on_overflow"), "{o}");
        assert!(!o.contains("... ok"), "{o}");
    }

    #[test]
    fn deux_blocs_de_messages_differents_ne_se_replient_pas_sous_le_plafond() {
        // En-tête volumineux + plafond serré : le message du 2e bloc doit survivre.
        let mut log = String::new();
        for i in 0..40 {
            log.push_str(&format!("   Compiling dep{i} v1.0.{i}\n"));
        }
        log.push_str(&log_cargo_leurres(3));
        let o = run(&log, "Pourquoi cargo test échoue-t-il ?", false, 30);
        assert!(o.contains("checksum invalide"), "{o}");
        assert!(o.contains("called `Option::unwrap()` on a `None` value"), "{o}");
        // Mode erreurs : même exigence (le message fait partie du bloc).
        let e = run(&log, "", true, 30);
        assert!(e.contains("checksum invalide") && e.contains("called `Option::unwrap()`"), "{e}");
    }

    #[test]
    fn blocs_de_meme_message_et_meme_emplacement_type_se_replient() {
        let mut log = String::from("running 3 tests\n");
        for (i, f) in [(1, 11), (2, 12), (3, 13)] {
            log.push_str(&format!("\n---- m::t{i} stdout ----\nthread 'm::t' panicked at src/m.rs:{f}:5:\ncalled `Option::unwrap()` on a `None` value\nstack backtrace:\n   0: x\n"));
        }
        log.push_str("\ntest result: FAILED. 0 passed; 3 failed\n");
        let o = run(&log, "pourquoi les tests échouent", false, 60);
        assert!(o.contains("similar block(s) skipped"), "mêmes message et emplacement-type : repliés\n{o}");
        assert_eq!(o.matches("called `Option::unwrap()`").count(), 1, "{o}");
    }

    #[test]
    fn panic_rust_le_message_fait_partie_du_bloc() {
        let t = texte("thread 'x' panicked at src/a.rs:1:2:\nassertion `left == right` failed: nul\n  left: 1\n right: 0\nstack backtrace:\n   0: f\n");
        let sig = signaux(&t);
        let (a, h, _) = debut_bloc(&t, &sig.classe, 0, -1);
        assert_eq!(bloc_fin(&t, &sig.classe, h, 20), 3, "le bloc va de la tête à `right: 0`");
        assert_eq!(a, 0);
    }
}
