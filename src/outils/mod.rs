//! Outils pour agents (architecture v2 §6) — UNE implémentation, partagée par
//! la CLI (`cortex find …`), le serveur MCP (`cortex_find` …) et le banc
//! d'agent (`cortex bench-agent`).
//!
//! | Outil | Question de l'agent |
//! |---|---|
//! | `find <question>` | où est X ? |
//! | `card <id\|nom>` | c'est quoi, qui l'appelle, qu'appelle-t-il, testé ? |
//! | `outline <fichier>` | que contient ce fichier ? |
//! | `read <id>` | montre le code de CETTE fonction |
//! | `overview <dossier>` | comment marche ce module ? |
//! | `impact <id>` | qu'est-ce qui casse si je change ça ? |
//! | `path <a> <b>` | comment A arrive-t-il à B ? |
//! | `changed` | qu'ai-je modifié, et avec quel impact ? |
//!
//! Règles de sortie, communes : identifiants stables (`ids`) recopiables tels
//! quels, provenance (chemin dans l'identifiant + `L<début>-<fin>`), budget en
//! tokens (≈ caractères / 4, `-b`), `✎` sur ce qui vient du travail non commité
//! (`overlay`), et une dernière ligne `suite : <appel le plus utile ensuite>`.
//! Aucune décoration : pas de titre, pas de cadre, pas de ligne vide.

pub mod graphe;
pub mod lecture;
pub mod overlay;

use crate::atlas::Handle;
use crate::ids::{self, Target};
use crate::search::is_test_path;
use crate::symbol::SymbolKind;

/// Un appel d'outil, tel que le reçoivent la CLI, le MCP et le banc d'agent.
#[derive(Debug, Clone)]
pub enum Appel {
    Find {
        question: String,
    },
    Card {
        cible: String,
    },
    Outline {
        cible: String,
    },
    Read {
        cible: String,
        contexte: u32,
    },
    Overview {
        dossier: String,
    },
    Impact {
        cible: String,
        profondeur: usize,
    },
    Path {
        de: String,
        vers: String,
    },
    Changed,
    /// Alias compatible de l'ancien `context` : la carte + les docs qui citent le symbole.
    Context {
        cible: String,
    },
}

impl Appel {
    /// Budget par défaut (tokens) de chaque outil.
    pub fn budget_defaut(&self) -> usize {
        match self {
            Appel::Find { .. } => 1000,
            Appel::Card { .. } | Appel::Context { .. } => 800,
            Appel::Read { .. } => 4000,
            Appel::Path { .. } => 800,
            _ => 1500,
        }
    }

    /// Analyse une ligne d'appel (`find mots…`, `card S:…`, `read S:… -c 3`,
    /// `path a b`, `impact x -d 2`) — sert au banc d'agent.
    pub fn analyser(ligne: &str) -> Option<Appel> {
        let ligne = ligne.trim();
        let (outil, reste) = ligne.split_once(' ').unwrap_or((ligne, ""));
        let reste = reste.trim();
        let mut mots: Vec<&str> = reste.split_whitespace().collect();
        let mut option = |flag: &str| -> Option<u32> {
            let i = mots.iter().position(|m| *m == flag)?;
            let v = mots.get(i + 1)?.parse().ok()?;
            mots.drain(i..i + 2);
            Some(v)
        };
        Some(match outil {
            "find" | "query" => Appel::Find { question: reste.to_string() },
            "card" | "explain" => Appel::Card { cible: reste.to_string() },
            "context" => Appel::Context { cible: reste.to_string() },
            "outline" => Appel::Outline { cible: reste.to_string() },
            "read" => {
                let c = option("-c").or_else(|| option("-C")).unwrap_or(0);
                Appel::Read { cible: mots.join(" "), contexte: c }
            }
            "overview" => Appel::Overview { dossier: reste.to_string() },
            "impact" => {
                let d = option("-d").unwrap_or(3) as usize;
                Appel::Impact { cible: mots.join(" "), profondeur: d }
            }
            "path" => {
                let (a, b) = (mots.first()?.to_string(), mots.get(1)?.to_string());
                Appel::Path { de: a, vers: b }
            }
            "changed" => Appel::Changed,
            _ => return None,
        })
    }
}

/// Exécute un appel sur les atlas ouverts (un par projet).
pub fn executer(handles: &[Handle], appel: &Appel, budget: Option<usize>) -> String {
    let budget = budget.unwrap_or_else(|| appel.budget_defaut()).max(50);
    if handles.is_empty() {
        return "(cortex) aucun projet indexé — cortex index <chemin> --name <Projet>\n".into();
    }
    match appel {
        Appel::Find { question } => lecture::find(handles, question, budget),
        Appel::Card { cible } => lecture::card(handles, cible, budget, false),
        Appel::Context { cible } => lecture::card(handles, cible, budget, true),
        Appel::Outline { cible } => lecture::outline(handles, cible, budget),
        Appel::Read { cible, contexte } => lecture::read(handles, cible, *contexte, budget),
        Appel::Overview { dossier } => graphe::overview(handles, dossier, budget),
        Appel::Impact { cible, profondeur } => graphe::impact(handles, cible, (*profondeur).clamp(1, 6), budget),
        Appel::Path { de, vers } => graphe::path(handles, de, vers, budget),
        Appel::Changed => overlay::changed(handles, budget),
    }
}

// ─── Sortie bornée en tokens ────────────────────────────────────────────────

/// Écrit des lignes jusqu'au budget (≈ caractères / 4) ; les lignes en trop
/// sont comptées, et `fin` ajoute la ligne `suite`.
pub struct Out {
    buf: String,
    chars: usize,
    max_chars: usize,
    budget: usize,
    coupees: usize,
}

impl Out {
    pub fn new(budget_tokens: usize) -> Out {
        Out { buf: String::new(), chars: 0, max_chars: budget_tokens * 4, budget: budget_tokens, coupees: 0 }
    }

    /// Ajoute une ligne si elle tient dans le budget (sinon la compte comme
    /// coupée, ainsi que toutes les suivantes). Rend vrai si elle a été écrite.
    pub fn ligne(&mut self, l: &str) -> bool {
        let n = l.chars().count() + 1;
        if self.coupees > 0 || (self.chars + n > self.max_chars && self.chars > 0) {
            self.coupees += 1;
            return false;
        }
        self.buf.push_str(l);
        self.buf.push('\n');
        self.chars += n;
        true
    }

    /// Termine la sortie : mention des lignes coupées, puis `suite : …`.
    pub fn fin(mut self, suite: Option<String>) -> String {
        if self.coupees > 0 {
            self.buf.push_str(&format!("… {} ligne(s) coupée(s) : budget -b {} atteint\n", self.coupees, self.budget));
        }
        if let Some(s) = suite {
            self.buf.push_str("suite : ");
            self.buf.push_str(&s);
            self.buf.push('\n');
        }
        self.buf
    }
}

// ─── Résolution d'une entrée (identifiant, chemin, nom) ─────────────────────

/// Ce qu'une entrée désigne dans un atlas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cible {
    /// Un symbole ou un fichier (id global).
    Noeud(u32),
    /// Une plage de lignes d'un fichier.
    Lignes { fichier: u32, debut: u32, fin: u32 },
}

/// Résultat de la résolution d'une entrée sur plusieurs projets.
pub enum Resolu<'h> {
    Trouve { h: &'h Handle, cible: Cible, homonymes: Vec<u32> },
    Ambigu(Vec<String>),
    Introuvable(String),
}

/// Fichier par chemin exact, sinon par suffixe de chemin UNIQUE (`useX.ts`,
/// `hooks/useX.ts`). Plusieurs suffixes : `Err(candidats)`.
pub fn fichier(h: &Handle, chemin: &str) -> Result<Option<u32>, Vec<String>> {
    let chemin = chemin.trim_start_matches("./").trim_end_matches('/');
    if let Some(g) = h.file_by_path(chemin) {
        return Ok(Some(g));
    }
    let suffixe = format!("/{}", chemin.to_ascii_lowercase());
    let mut trouves: Vec<(&str, u32)> =
        h.live_files().filter(|r| r.name().to_ascii_lowercase().ends_with(&suffixe)).map(|r| (r.name(), r.g)).collect();
    trouves.sort();
    match trouves.len() {
        0 => Ok(None),
        1 => Ok(Some(trouves[0].1)),
        _ => Err(trouves.iter().map(|(p, _)| ids::file_id(p)).collect()),
    }
}

/// Symbole `frag` (rang `rang`) du fichier `f`.
fn symbole_du_fichier(h: &Handle, f: u32, frag: &str, rang: u32, doc: bool) -> Option<u32> {
    let syms = h.symbols_of(f);
    let cle = |r: &crate::atlas::view::NodeRef| if r.kind() == SymbolKind::Heading { ids::slug(r.name()) } else { r.name().to_string() };
    let exacts: Vec<u32> = syms.iter().filter(|r| (r.kind() == SymbolKind::Heading) == doc && cle(r) == frag).map(|r| r.g).collect();
    if let Some(&g) = exacts.get(rang as usize - 1) {
        return Some(g);
    }
    // Casse différente ou identifiant d'une version antérieure : premier approchant.
    let fl = frag.to_lowercase();
    syms.iter().find(|r| cle(r).to_lowercase() == fl).map(|r| r.g)
}

/// Résout une entrée dans UN atlas.
fn resoudre_un<'h>(h: &'h Handle, t: &Target) -> Resolu<'h> {
    let trouve = |cible| Resolu::Trouve { h, cible, homonymes: Vec::new() };
    match t {
        Target::Sym { path, frag, rank, doc } => match fichier(h, path) {
            Ok(Some(f)) => match symbole_du_fichier(h, f, frag, *rank, *doc) {
                Some(g) => trouve(Cible::Noeud(g)),
                None => Resolu::Introuvable(format!(
                    "« {} » introuvable dans {} (outline {} pour la liste)",
                    frag,
                    ids::file_id(h.path_of(f)),
                    ids::file_id(h.path_of(f))
                )),
            },
            Ok(None) => Resolu::Introuvable(format!("fichier « {} » introuvable", path)),
            Err(c) => Resolu::Ambigu(c),
        },
        Target::File(p) => match fichier(h, p) {
            Ok(Some(f)) => trouve(Cible::Noeud(f)),
            Ok(None) => {
                // Un nom qui ressemble à un chemin (`Foo.bar`) : essai comme nom.
                if !p.contains('/') {
                    if let r @ Resolu::Trouve { .. } = resoudre_un(h, &Target::Name(p.clone())) {
                        return r;
                    }
                }
                Resolu::Introuvable(format!("fichier « {} » introuvable", p))
            }
            Err(c) => Resolu::Ambigu(c),
        },
        Target::Lines { path, start, end } => match fichier(h, path) {
            Ok(Some(f)) => trouve(Cible::Lignes { fichier: f, debut: *start, fin: *end }),
            Ok(None) => Resolu::Introuvable(format!("fichier « {} » introuvable", path)),
            Err(c) => Resolu::Ambigu(c),
        },
        Target::Name(n) => {
            let Some(g) = h.find_symbol(n) else { return Resolu::Introuvable(format!("symbole « {} » introuvable", n)) };
            let mut homonymes: Vec<u32> = h.defs(&n.to_ascii_lowercase()).into_iter().filter(|&x| x != g).collect();
            homonymes.sort_by(|&a, &b| h.sort_key(a).cmp(&h.sort_key(b)));
            Resolu::Trouve { h, cible: Cible::Noeud(g), homonymes }
        }
    }
}

/// Résout une entrée sur les projets ouverts (le premier qui la connaît).
pub fn resoudre<'h>(handles: &'h [Handle], entree: &str) -> Resolu<'h> {
    let t = ids::parse(entree);
    let mut premier_echec: Option<Resolu> = None;
    for h in handles {
        match resoudre_un(h, &t) {
            r @ Resolu::Trouve { .. } => return r,
            r => {
                if premier_echec.is_none() || matches!(r, Resolu::Ambigu(_)) {
                    premier_echec = Some(r);
                }
            }
        }
    }
    premier_echec.unwrap_or_else(|| Resolu::Introuvable(format!("« {} » introuvable", entree)))
}

/// Message d'échec de résolution, avec la suite utile.
pub fn echec(r: Resolu, entree: &str) -> String {
    match r {
        Resolu::Ambigu(c) => {
            let mut out = Out::new(400);
            out.ligne(&format!("« {} » désigne {} fichiers :", entree, c.len()));
            for x in c.iter().take(12) {
                out.ligne(x);
            }
            out.fin(c.first().map(|x| format!("outline {}", x)))
        }
        Resolu::Introuvable(m) => format!("(cortex) {}\nsuite : find {}\n", m, entree.trim_start_matches("S:").trim_start_matches("F:")),
        Resolu::Trouve { .. } => String::new(),
    }
}

// ─── Briques de rendu partagées ─────────────────────────────────────────────

/// `S:…#nom genre L12-40` d'un symbole, `F:chemin` d'un fichier.
pub fn entete(h: &Handle, g: u32) -> String {
    match h.node(g) {
        Some(r) if r.n.kind == 1 => {
            format!("{} {} {}", h.node_id(g), r.kind().as_str(), crate::atlas::cards::lines(r.n.line, r.n.end_line))
        }
        Some(_) => h.node_id(g),
        None => String::new(),
    }
}

/// Rôle d'un nœud (vide si aucun).
pub fn role(h: &Handle, g: u32) -> &str {
    h.node(g).map(|r| r.str(r.n.summary)).unwrap_or("")
}

/// Coupe un texte à `n` caractères (« … »).
pub fn court(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let c: String = s.chars().take(n.saturating_sub(1)).collect();
        format!("{}…", c.trim_end())
    }
}

/// Fichier propriétaire d'un nœud (lui-même pour un fichier).
pub fn fichier_de(h: &Handle, g: u32) -> u32 {
    match h.node(g) {
        Some(r) if r.n.kind == 1 => r.n.owner_file,
        _ => g,
    }
}

/// Première ligne où le fichier de `appelant` appelle `nom` dans la plage du
/// symbole `appelant` (site d'appel), si connue.
pub fn site_appel(h: &Handle, appelant: u32, nom: &str) -> Option<u32> {
    let r = h.node(appelant)?;
    if r.n.kind != 1 {
        return None;
    }
    let f = h.node(r.n.owner_file)?;
    let refs = f.refs()?;
    let (a, b) = (r.n.line, if r.n.end_line == 0 { u32::MAX } else { r.n.end_line });
    refs.calls.iter().filter(|c| c.line >= a && c.line <= b && f.str(c.name).eq_ignore_ascii_case(nom)).map(|c| c.line).min()
}

/// Appelants d'un symbole avec leur site d'appel, sans doublon d'imbrication :
/// un appel fait dans une fonction interne est aussi dans la plage de la
/// fonction qui l'englobe (le graphe relie les deux) ; pour un même fichier et
/// une même ligne d'appel, seul l'appelant le plus INTERNE est gardé.
pub fn appelants(h: &Handle, g: u32) -> Vec<(u32, Option<u32>)> {
    let Some(r) = h.node(g) else { return Vec::new() };
    let nom = r.name();
    let v: Vec<(u32, Option<u32>)> = h.callers(g).into_iter().map(|c| (c, site_appel(h, c, nom))).collect();
    let etendue = |c: u32| h.node(c).map_or(u32::MAX, |x| if x.n.end_line == 0 { u32::MAX } else { x.n.end_line - x.n.line });
    let mut meilleur: std::collections::HashMap<(u32, u32), (u32, u32)> = std::collections::HashMap::new();
    for &(c, site) in &v {
        if let Some(l) = site {
            let e = (etendue(c), c);
            meilleur.entry((fichier_de(h, c), l)).and_modify(|b| *b = (*b).min(e)).or_insert(e);
        }
    }
    v.into_iter().filter(|&(c, site)| site.is_none_or(|l| meilleur[&(fichier_de(h, c), l)].1 == c)).collect()
}

/// Nom de base d'un fichier sans extensions (`a/b/useX.test.ts` → `usex`).
fn tige(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.split('.').next().unwrap_or(name).to_ascii_lowercase()
}

/// Fichiers de test liés à un fichier (et à un nom de symbole) par leur nom :
/// `x.test.ts`, `x.spec.tsx`, `__tests__/x.test.ts`, `tests/…/x.test.ts`.
pub fn tests_par_nom(h: &Handle, path: &str, nom: Option<&str>) -> Vec<u32> {
    let t = tige(path);
    let n = nom.map(|x| x.to_ascii_lowercase());
    let mut out: Vec<(&str, u32)> = Vec::new();
    for r in h.live_files() {
        let p = r.name();
        let fname = p.rsplit('/').next().unwrap_or(p);
        let base = fname.split('.').next().unwrap_or(fname);
        let ok = base.eq_ignore_ascii_case(&t) || n.as_deref().is_some_and(|n| base.eq_ignore_ascii_case(n));
        if ok && p != path && is_test_path(p) {
            out.push((p, r.g));
        }
    }
    out.sort();
    out.into_iter().map(|(_, g)| g).collect()
}

/// Marque `✎` d'un fichier modifié et non commité.
pub fn marque(ov: &overlay::Overlay, path: &str) -> &'static str {
    if ov.contient(path) {
        " ✎"
    } else {
        ""
    }
}

/// Liste d'identifiants séparés par des virgules, bornée à `n` (+reste).
pub fn liste(ids: &[String], n: usize) -> String {
    let mut s = ids.iter().take(n).cloned().collect::<Vec<_>>().join(", ");
    if ids.len() > n {
        s.push_str(&format!(" (+{})", ids.len() - n));
    }
    s
}

#[cfg(test)]
mod tests;
