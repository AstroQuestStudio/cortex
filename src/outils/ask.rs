//! `ask <question> -b <budget>` — idée I1 de l'architecture : UN appel qui
//! assemble le contexte optimal sous un budget de jetons.
//!
//! 1. Intention, par règles sur la question et sur les symboles reconnus :
//!    *impact* (« qu'est-ce qui casse si je change X »), *chemin* (« comment A
//!    arrive-t-il à B »), *module* (un dossier), sinon *expliquer* / *localiser*.
//!    Les trois premières réutilisent `impact`, `path`, `overview` tels quels.
//! 2. Pour les deux autres : graines = meilleurs résultats de `find` (plus les
//!    symboles nommés dans la question), puis FAITS candidats par graine :
//!    définition, signature, appelés, appelants, tests, lignes clés du corps.
//!    Chaque fait a un coût (jetons de sa ligne) et une valeur (pertinence de la
//!    graine × intérêt du genre de fait).
//! 3. Sélection gloutonne du meilleur rapport valeur / coût sous le budget, avec
//!    une pénalité de redondance : le k-ième fait d'un même genre pour une même
//!    graine, ou du même genre sur un même fichier, vaut moins.
//!
//! La sortie suit les règles communes (identifiants `S:chemin#symbole`, plages
//! `L<début>-<fin>`, `next : …`).

use super::graphe;
use super::lecture::exporte;
use super::lecture::{lignes_fichier, tests_du_symbole};
use super::overlay::{self, Overlay};
use super::{appelants, court, entete, fichier, fichier_de, marque, role, Out};
use crate::atlas::Handle;
use crate::symbol::{fold_accents, SymbolKind};
use std::collections::{HashMap, HashSet};

/// Budget par défaut de `ask` (jetons).
pub const BUDGET_DEFAUT: usize = 600;
/// Graines au plus.
const GRAINES_MAX: usize = 5;
/// Une graine doit atteindre cette part du meilleur score de `find`.
const SEUIL_GRAINE: f32 = 0.5;
/// Lignes clés au plus par graine.
const LIGNES_MAX: usize = 2;
/// Autres résultats de `find` proposés en identifiants seuls.
const CANDIDATS_MAX: usize = 40;

// ─── Intention ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intention {
    /// Dépendants de la cible.
    Impact(String),
    /// Chemin de `.0` vers `.1`.
    Chemin(String, String),
    /// Vue d'un dossier.
    Module(String),
    /// Comment ça marche (flux, appelés d'abord).
    Expliquer,
    /// Où est X, qui s'en sert (appelants d'abord).
    Localiser,
}

/// Mots de la question (accents repliés, minuscules, sans ponctuation).
fn mots(question: &str) -> Vec<String> {
    fold_accents(question).to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_string).collect()
}

fn contient_un(mots: &[String], liste: &[&str]) -> bool {
    mots.iter().any(|m| liste.iter().any(|l| m == l || (l.len() >= 5 && m.starts_with(l))))
}

/// Mots qui annoncent une question d'impact.
const MOTS_IMPACT: &[&str] = &[
    "casse", "casser", "cassera", "break", "breaks", "impact", "impacte", "modifie", "modifier", "change", "changer", "revoir", "relancer",
    "affecte", "rename", "renomme", "supprime",
];
/// Mots qui annoncent un chemin d'un point à un autre.
const MOTS_CHEMIN: &[&str] = &["arrive", "arriver", "jusqu", "reach", "reaches", "chemin", "path", "mene", "atteint"];
/// Mots qui annoncent une vue de module.
const MOTS_MODULE: &[&str] = &["module", "dossier", "contient", "contains", "folder", "directory", "repertoire"];
/// Mots qui demandent les utilisateurs d'un symbole.
const MOTS_USAGE: &[&str] =
    &["qui", "utilise", "utilisent", "sert", "appelle", "appelant", "appelants", "used", "uses", "who", "callers", "usage"];
/// Mots qui demandent un mécanisme.
const MOTS_FLUX: &[&str] =
    &["comment", "how", "pourquoi", "why", "flux", "flow", "fonctionne", "marche", "work", "works", "passe", "jusqu"];
/// Mots qui demandent les tests.
const MOTS_TEST: &[&str] = &["test", "tests", "teste", "testes", "couvert", "coverage"];

/// Jetons de la question qui ont la forme d'un chemin, d'un fichier ou d'un
/// identifiant (`a/b`, `useX.ts`, `camelCase`, `snake_case`), dans l'ordre,
/// sans ponctuation autour.
pub fn jetons_code(question: &str) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    for brut in question.replace(['\'', '’'], " ").split_whitespace() {
        let t = brut.trim_matches(|c: char| "?,;:()[]{}«»\"`!".contains(c)).trim_end_matches('.');
        if t.is_empty() {
            continue;
        }
        let ext = t.rsplit_once('.').is_some_and(|(a, e)| {
            !a.is_empty()
                && (1..=5).contains(&e.len())
                && e.chars().all(|c| c.is_ascii_alphanumeric())
                && e.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
                && !a.chars().all(|c| c.is_ascii_digit())
        });
        let camel = t.chars().zip(t.chars().skip(1)).any(|(a, b)| a.is_lowercase() && b.is_uppercase());
        let forme = t.contains('/') || ext || camel || (t.contains('_') && t.len() > 3);
        if forme && !v.iter().any(|x| x == t) {
            v.push(t.to_string());
        }
    }
    v
}

/// Ce que désigne un jeton de code dans les atlas ouverts.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Jeton {
    Dossier(String),
    Code(String),
}

fn reconnaitre(handles: &[Handle], t: &str) -> Option<Jeton> {
    let t = t.trim_start_matches("./").trim_end_matches('/');
    let dernier = t.rsplit('/').next().unwrap_or(t);
    if t.contains('/') && !dernier.contains('.') {
        let pre = format!("{}/", t);
        if handles.iter().any(|h| h.live_files().any(|r| r.name().starts_with(&pre))) {
            return Some(Jeton::Dossier(t.to_string()));
        }
    }
    if t.contains('/') || t.rsplit_once('.').is_some() {
        if handles.iter().any(|h| matches!(fichier(h, t), Ok(Some(_)))) {
            return Some(Jeton::Code(t.to_string()));
        }
        return None;
    }
    let cle = crate::graph::call_key(t);
    handles.iter().any(|h| !h.defs(&cle).is_empty()).then(|| Jeton::Code(t.to_string()))
}

/// Intention d'une question (les jetons de code sont reconnus dans `handles`).
pub fn intention(handles: &[Handle], question: &str) -> Intention {
    let m = mots(question);
    let jetons: Vec<Jeton> = jetons_code(question).iter().filter_map(|t| reconnaitre(handles, t)).collect();
    let codes: Vec<&String> = jetons.iter().filter_map(|j| if let Jeton::Code(c) = j { Some(c) } else { None }).collect();
    let dossier = jetons.iter().find_map(|j| if let Jeton::Dossier(d) = j { Some(d.clone()) } else { None });
    if codes.len() >= 2 && contient_un(&m, MOTS_CHEMIN) {
        return Intention::Chemin(codes[0].clone(), codes[1].clone());
    }
    if let Some(c) = codes.first() {
        if contient_un(&m, MOTS_IMPACT) {
            return Intention::Impact((*c).clone());
        }
    }
    if let Some(d) = dossier {
        if codes.is_empty() || contient_un(&m, MOTS_MODULE) {
            return Intention::Module(d);
        }
    }
    if contient_un(&m, MOTS_FLUX) && !contient_un(&m, MOTS_USAGE) {
        Intention::Expliquer
    } else if contient_un(&m, MOTS_USAGE) || m.first().is_some_and(|w| w == "ou" || w == "where") {
        Intention::Localiser
    } else {
        Intention::Expliquer
    }
}

// ─── Faits candidats et sélection ───────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Genre {
    Def,
    Sig,
    /// Rôle (doc-comment) du symbole, ou de son fichier.
    Role,
    Appele,
    Appelant,
    Test,
    Ligne,
    /// Un autre résultat de `find`, en identifiant seul (le moins cher).
    Candidat,
    /// Autre export du fichier de la graine.
    Voisin,
}

/// Un fait candidat : un morceau de réponse, son coût et sa valeur.
#[derive(Debug, Clone)]
pub struct Fait {
    /// Graine dont il parle.
    pub groupe: usize,
    pub genre: Genre,
    /// Fichier du sujet du fait (pénalité de redondance).
    pub fichier: u64,
    pub valeur: f32,
    /// Jetons de sa ligne rendue.
    pub cout: usize,
    pub texte: String,
    /// Numéro de ligne (genre `Ligne`).
    pub ligne: u32,
}

fn jetons(s: &str) -> usize {
    s.chars().count().div_ceil(4)
}

/// Décroissance de la valeur d'un fait par fait déjà retenu du même genre
/// pour la même graine ; et pour le même genre sur le même fichier.
const DECROIT_GENRE: f32 = 0.85;
/// Les candidats en identifiants seuls s'épuisent plus lentement.
const DECROIT_CANDIDAT: f32 = 0.97;
const DECROIT_FICHIER: f32 = 0.75;

/// Sélection gloutonne sous budget : à chaque pas, le fait de meilleur rapport
/// valeur effective / coût qui tient. Un fait d'une graine n'est éligible que
/// si la définition de cette graine est déjà retenue. Rend les indices retenus
/// dans l'ordre de sélection.
pub fn selectionner(faits: &[Fait], budget: usize) -> Vec<usize> {
    let mut pris: Vec<usize> = Vec::new();
    let mut libre = budget;
    let mut ouverts: HashSet<usize> = HashSet::new();
    let mut par_genre: HashMap<(usize, Genre), u32> = HashMap::new();
    let mut par_fichier: HashMap<(u64, Genre), u32> = HashMap::new();
    let mut dispo: Vec<bool> = vec![true; faits.len()];
    loop {
        let mut meilleur: Option<(usize, f32)> = None;
        for (i, f) in faits.iter().enumerate() {
            if !dispo[i] || f.cout > libre || (!matches!(f.genre, Genre::Def | Genre::Candidat) && !ouverts.contains(&f.groupe)) {
                continue;
            }
            let kg = *par_genre.get(&(f.groupe, f.genre)).unwrap_or(&0);
            let kf = if matches!(f.genre, Genre::Def | Genre::Appele) { *par_fichier.get(&(f.fichier, f.genre)).unwrap_or(&0) } else { 0 };
            let dg = if f.genre == Genre::Candidat { DECROIT_CANDIDAT } else { DECROIT_GENRE };
            let v = f.valeur * dg.powi(kg as i32) * DECROIT_FICHIER.powi(kf as i32);
            let ratio = v / f.cout.max(1) as f32;
            if meilleur.is_none_or(|(_, r)| ratio > r) {
                meilleur = Some((i, ratio));
            }
        }
        let Some((i, _)) = meilleur else { break };
        let f = &faits[i];
        dispo[i] = false;
        libre -= f.cout;
        pris.push(i);
        ouverts.insert(f.groupe);
        *par_genre.entry((f.groupe, f.genre)).or_default() += 1;
        *par_fichier.entry((f.fichier, f.genre)).or_default() += 1;
    }
    pris
}

/// Une graine de la réponse.
struct Graine {
    hi: usize,
    g: u32,
    /// Pertinence relative, 0..=1.
    r: f32,
}

/// Résultats de `find` pour la question, du meilleur au moins bon, avec leur
/// pertinence relative (score / meilleur score).
fn resultats(handles: &[Handle], question: &str) -> Vec<(usize, u32, f32)> {
    let mut hits: Vec<(usize, crate::search::Hit)> = Vec::new();
    for (i, h) in handles.iter().enumerate() {
        hits.extend(h.search(question, CANDIDATS_MAX + 10).into_iter().map(|x| (i, x)));
    }
    hits.sort_by(|a, b| b.1.score.partial_cmp(&a.1.score).unwrap_or(std::cmp::Ordering::Equal));
    let haut = hits.first().map(|h| h.1.score).unwrap_or(1.0).max(f32::MIN_POSITIVE);
    let mut vus: HashSet<(usize, u32)> = HashSet::new();
    hits.into_iter()
        .filter(|(hi, x)| !handles[*hi].node(x.g).is_none_or(|n| n.kind() == SymbolKind::Import) && vus.insert((*hi, x.g)))
        .map(|(hi, x)| (hi, x.g, x.score / haut))
        .collect()
}

/// Graines : les symboles nommés dans la question (pertinence 1), puis les
/// meilleurs résultats dont la pertinence ÉLEVÉE AU CUBE (qui écrase les
/// seconds couteaux) passe le seuil ; pour un « comment », les appelés des
/// deux premières graines dont le nom recoupe la question en deviennent.
fn graines(handles: &[Handle], res: &[(usize, u32, f32)], nommes: &[String], termes: &[String], expliquer: bool) -> Vec<Graine> {
    let mut v: Vec<Graine> = Vec::new();
    for n in nommes {
        let cle = crate::graph::call_key(n);
        for (hi, h) in handles.iter().enumerate() {
            if let Some(&g) = h.defs(&cle).first() {
                if h.node(g).is_some_and(|r| r.n.kind == 1) && !v.iter().any(|x| x.hi == hi && x.g == g) {
                    v.push(Graine { hi, g, r: 1.0 });
                    break;
                }
            }
        }
    }
    // Un type ou une constante n'explique rien (ni appelés ni corps) ; un symbole
    // isolé du graphe dit moins qu'un symbole relié.
    let mut notes: Vec<(usize, u32, f32)> = res
        .iter()
        .take(CANDIDATS_MAX)
        .map(|&(hi, g, lin)| {
            let h = &handles[hi];
            let sans_corps = h.node(g).is_some_and(|n| {
                matches!(
                    n.kind(),
                    SymbolKind::Interface
                        | SymbolKind::Type
                        | SymbolKind::Const
                        | SymbolKind::Struct
                        | SymbolKind::Enum
                        | SymbolKind::Export
                )
            });
            let poids_genre = if expliquer && sans_corps { 0.5 } else { 1.0 };
            let relie = !h.callees(g).is_empty() || !h.callers(g).is_empty();
            (hi, g, lin * poids_genre * if relie { 1.0 } else { 0.8 })
        })
        .collect();
    notes.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    let haut = notes.first().map(|x| x.2).unwrap_or(1.0).max(f32::MIN_POSITIVE);
    for (hi, g, note) in notes {
        let r = (note / haut).powi(3);
        if v.len() >= GRAINES_MAX || r < SEUIL_GRAINE {
            break;
        }
        if !v.iter().any(|x| x.hi == hi && x.g == g) {
            v.push(Graine { hi, g, r });
        }
    }
    if expliquer {
        let tete: Vec<(usize, u32, f32)> = v.iter().take(2).map(|x| (x.hi, x.g, x.r)).collect();
        for (hi, g, r) in tete {
            let h = &handles[hi];
            let mut ajoutes = 0;
            for c in h.callees(g) {
                let Some(cr) = h.node(c) else { continue };
                let recoupe = crate::search::query_terms(cr.name()).iter().any(|t| termes.contains(t));
                if ajoutes < 2 && recoupe && cr.n.kind == 1 && c != g && !v.iter().any(|x| x.hi == hi && x.g == c) {
                    v.push(Graine { hi, g: c, r: 0.7 * r });
                    ajoutes += 1;
                }
            }
        }
    }
    v
}

/// Termes de la question (radicaux courts, minuscules) pour reconnaître les
/// lignes clés d'un corps.
fn termes(question: &str) -> Vec<String> {
    crate::search::query_terms(question).into_iter().filter(|t| t.chars().count() >= 4).collect()
}

fn ligne_cle(ligne: &str, termes: &[String], appeles: &[String]) -> usize {
    let t = ligne.trim();
    if t.len() < 8 || t.starts_with("import ") || t.starts_with("use ") || t.starts_with("//") && t.len() < 20 {
        return 0;
    }
    let bas = fold_accents(t).to_lowercase();
    let n_termes = termes.iter().filter(|x| bas.contains(x.as_str())).count();
    let n_appeles = appeles.iter().filter(|a| bas.contains(a.as_str())).count();
    n_termes + n_appeles
}

struct Contexte<'a> {
    handles: &'a [Handle],
    multi: bool,
    ovs: Vec<Overlay>,
    usage: bool,
    tests: bool,
    termes: Vec<String>,
}

impl Contexte<'_> {
    /// Facteur de valeur d'un nom qui recoupe les termes de la question : 1 + 0,8 par terme (3 au plus).
    fn recoupement(&self, nom: &str) -> f32 {
        let n = crate::search::query_terms(nom).iter().filter(|t| self.termes.contains(t)).count();
        1.0 + 0.8 * n.min(2) as f32
    }
}

fn candidats(cx: &Contexte, graines: &[Graine], res: &[(usize, u32, f32)]) -> Vec<Fait> {
    let mut faits: Vec<Fait> = Vec::new();
    // Les autres résultats de `find`, en identifiants seuls : le fait le moins cher.
    for &(hi, g, lin) in res.iter().filter(|(hi, g, _)| !graines.iter().any(|x| x.hi == *hi && x.g == *g)).take(CANDIDATS_MAX) {
        let h = &cx.handles[hi];
        let t = format!("{}{}", if cx.multi { format!("[{}] ", h.project) } else { String::new() }, h.node_id(g));
        let test = if (cx.usage || cx.tests) && crate::search::is_test_path(h.path_of(fichier_de(h, g))) { 1.5 } else { 1.0 };
        faits.push(Fait {
            groupe: usize::MAX,
            genre: Genre::Candidat,
            fichier: u64::MAX,
            valeur: 0.9 * lin * test,
            cout: jetons(&t) + 1,
            texte: t,
            ligne: 0,
        });
    }
    let graine_ids: HashSet<(usize, u32)> = graines.iter().map(|g| (g.hi, g.g)).collect();
    let mut fichiers_lus: HashMap<(usize, u32), Option<Vec<String>>> = HashMap::new();
    for (gi, gr) in graines.iter().enumerate() {
        let h = &cx.handles[gr.hi];
        let Some(r) = h.node(gr.g) else { continue };
        let f = r.n.owner_file;
        let path = h.path_of(f);
        let cle_f = ((gr.hi as u64) << 32) | f as u64;
        let r_ = gr.r;
        // Définition.
        let def = format!(
            "{}{}{}",
            if cx.multi { format!("[{}] ", h.project) } else { String::new() },
            entete(h, gr.g),
            marque(&cx.ovs[gr.hi], path)
        );
        faits.push(Fait { groupe: gi, genre: Genre::Def, fichier: cle_f, valeur: 0.3 + r_, cout: jetons(&def) + 1, texte: def, ligne: 0 });
        let role_s = {
            let s = r.str(r.n.summary);
            if s.is_empty() {
                role(h, f)
            } else {
                s
            }
        };
        if !role_s.is_empty() {
            let t = court(role_s, 100);
            faits.push(Fait { groupe: gi, genre: Genre::Role, fichier: cle_f, valeur: 0.6 * r_, cout: jetons(&t) + 3, texte: t, ligne: 0 });
        }
        // Signature.
        let sig = r.str(r.n.signature);
        if !sig.is_empty() {
            let t = court(sig, 130);
            faits.push(Fait { groupe: gi, genre: Genre::Sig, fichier: cle_f, valeur: 0.4 * r_, cout: jetons(&t) + 3, texte: t, ligne: 0 });
        }
        // Appelés.
        let nom_graine = r.name().to_lowercase();
        let mut noms_appeles: Vec<String> = Vec::new();
        for c in h.callees(gr.g) {
            let Some(cr) = h.node(c) else { continue };
            if c == gr.g || cr.n.kind != 1 || cr.kind() == SymbolKind::Import || graine_ids.contains(&(gr.hi, c)) {
                continue;
            }
            let cf = cr.n.owner_file;
            let id = h.node_id(c);
            let nom = cr.name().to_lowercase();
            if nom != nom_graine && nom.len() >= 4 {
                noms_appeles.push(nom.clone());
            }
            // Un utilitaire appelé de partout dit peu sur CE flux.
            let n_app = h.callers(c).len() as f32;
            let rarete = 1.0 / (1.0 + (1.0 + n_app).ln() / 2.0);
            let recoupe = cx.recoupement(cr.name());
            let marque_c = marque(&cx.ovs[gr.hi], h.path_of(cf));
            let t = format!("{}{}", id, marque_c);
            let v = (if cx.usage { 0.4 } else { 0.6 }) * r_ * rarete * recoupe;
            faits.push(Fait {
                groupe: gi,
                genre: Genre::Appele,
                fichier: ((gr.hi as u64) << 32) | cf as u64,
                valeur: v,
                cout: jetons(&t) + 1,
                texte: t,
                ligne: 0,
            });
        }
        // Appelants (sans les tests : ils ont leur genre).
        let apps = appelants(h, gr.g);
        let mut a_code: Vec<(u32, Option<u32>)> = apps.iter().copied().filter(|&(c, _)| !est_test(h, c)).collect();
        a_code.sort_by_key(|&(c, _)| h.sort_key(c));
        for (c, site) in a_code {
            let t = match site {
                Some(l) => format!("{} L{}", h.node_id(c), l),
                None => h.node_id(c),
            };
            let cf = fichier_de(h, c);
            faits.push(Fait {
                groupe: gi,
                genre: Genre::Appelant,
                fichier: ((gr.hi as u64) << 32) | cf as u64,
                valeur: (if cx.usage { 0.8 } else { 0.5 }) * r_,
                cout: jetons(&t) + 1,
                texte: t,
                ligne: 0,
            });
        }
        // Tests.
        let ag: Vec<u32> = apps.iter().map(|&(c, _)| c).collect();
        for t in tests_du_symbole(h, gr.g, &ag).into_iter().take(4) {
            let texte = h.node_id(t);
            faits.push(Fait {
                groupe: gi,
                genre: Genre::Test,
                fichier: ((gr.hi as u64) << 32) | t as u64,
                valeur: (if cx.tests { 0.8 } else { 0.25 }) * r_,
                cout: jetons(&texte) + 1,
                texte,
                ligne: 0,
            });
        }
        // Autres exports du fichier (de quoi naviguer sans l'ouvrir).
        let mut voisins = 0;
        for s in h.symbols_of(f) {
            if voisins >= 5 {
                break;
            }
            if s.g == gr.g || s.kind() == SymbolKind::Import || !exporte(s.str(s.n.signature)) || graine_ids.contains(&(gr.hi, s.g)) {
                continue;
            }
            let t = h.node_id(s.g);
            faits.push(Fait {
                groupe: gi,
                genre: Genre::Voisin,
                fichier: cle_f,
                valeur: 0.3 * r_ * cx.recoupement(s.name()),
                cout: jetons(&t) + 1,
                texte: t,
                ligne: 0,
            });
            voisins += 1;
        }
        // Lignes clés du corps.
        let (a, b) = (r.n.line, r.n.end_line.max(r.n.line));
        if b > a && b - a <= 600 {
            let src = fichiers_lus.entry((gr.hi, f)).or_insert_with(|| lignes_fichier(h, path));
            if let Some(src) = src {
                let mut notes: Vec<(usize, u32)> = Vec::new();
                for l in a + 1..=b.min(src.len() as u32) {
                    let n = ligne_cle(&src[l as usize - 1], &cx.termes, &noms_appeles);
                    if n > 0 {
                        notes.push((n, l));
                    }
                }
                notes.sort_by(|x, y| y.0.cmp(&x.0).then(x.1.cmp(&y.1)));
                for (n, l) in notes.into_iter().take(LIGNES_MAX) {
                    let t = format!("L{}│{}", l, court(src[l as usize - 1].trim(), 110));
                    faits.push(Fait {
                        groupe: gi,
                        genre: Genre::Ligne,
                        fichier: cle_f,
                        valeur: 0.3 * r_ * (n.min(3) as f32 / 3.0 + 0.5),
                        cout: jetons(&t) + 3,
                        texte: t,
                        ligne: l,
                    });
                }
            }
        }
    }
    faits
}

fn est_test(h: &Handle, g: u32) -> bool {
    crate::search::is_test_path(h.path_of(fichier_de(h, g)))
}

/// Rend les faits retenus, regroupés par graine.
fn rendre(faits: &[Fait], pris: &[usize], graines: &[Graine], budget: usize, intent: &str) -> String {
    let mut par_groupe: Vec<Vec<&Fait>> = vec![Vec::new(); graines.len()];
    for &i in pris {
        if faits[i].groupe == usize::MAX {
            continue;
        }
        par_groupe[faits[i].groupe].push(&faits[i]);
    }
    // Graines dans l'ordre de pertinence (les nommées d'abord, déjà r = 1).
    let mut out = Out::new(budget);
    out.ligne(&format!("ask {} — {} facts", intent, pris.len()));
    for fs in par_groupe.iter().filter(|v| !v.is_empty()) {
        let du_genre = |g: Genre| fs.iter().filter(|f| f.genre == g).map(|f| f.texte.as_str()).collect::<Vec<_>>();
        for f in fs.iter().filter(|f| f.genre == Genre::Def) {
            match du_genre(Genre::Role).first() {
                Some(r) => out.ligne(&format!("{} — {}", f.texte, r)),
                None => out.ligne(&f.texte),
            };
        }
        if let Some(s) = du_genre(Genre::Sig).first() {
            out.ligne(&format!("  sig: {}", s));
        }
        let mut lignes: Vec<&&Fait> = fs.iter().filter(|f| f.genre == Genre::Ligne).collect();
        lignes.sort_by_key(|f| f.ligne);
        for f in lignes {
            out.ligne(&format!("  {}", f.texte));
        }
        for (genre, etiquette) in
            [(Genre::Appele, "calls"), (Genre::Appelant, "called by"), (Genre::Voisin, "same file"), (Genre::Test, "tests")]
        {
            let v = du_genre(genre);
            if !v.is_empty() {
                out.ligne(&format!("  {}: {}", etiquette, v.join(", ")));
            }
        }
    }
    let autres: Vec<&str> = pris.iter().map(|&i| &faits[i]).filter(|f| f.genre == Genre::Candidat).map(|f| f.texte.as_str()).collect();
    if !autres.is_empty() {
        out.ligne(&format!("also: {}", autres.join(", ")));
    }
    let premier = pris
        .iter()
        .map(|&i| &faits[i])
        .find(|f| f.genre == Genre::Def)
        .and_then(|f| f.texte.split_whitespace().find(|t| t.starts_with("S:") || t.starts_with("D:") || t.starts_with("F:")));
    let suite = premier.map(|id| format!("read {}", id));
    out.fin(suite)
}

// ─── Point d'entrée ─────────────────────────────────────────────────────────

/// `ask <question> -b <budget>`.
pub fn ask(handles: &[Handle], question: &str, budget: usize) -> String {
    let question = question.trim();
    let it = intention(handles, question);
    match &it {
        Intention::Impact(c) => return graphe::impact(handles, c, 3, budget),
        Intention::Chemin(a, b) => return graphe::path(handles, a, b, budget),
        Intention::Module(d) => return graphe::overview(handles, d, budget),
        _ => {}
    }
    let nommes: Vec<String> =
        jetons_code(question).into_iter().filter(|t| !t.contains('/') && !t.contains('.') && reconnaitre(handles, t).is_some()).collect();
    let m = mots(question);
    let tm = termes(question);
    let res = resultats(handles, question);
    let gs = graines(handles, &res, &nommes, &tm, it == Intention::Expliquer);
    if gs.is_empty() && res.is_empty() {
        let mot = crate::search::query_terms(question).into_iter().next().unwrap_or_default();
        return format!("(cortex) no symbol for '{}'\nnext : grep {}\n", question, mot);
    }
    let cx = Contexte {
        handles,
        multi: handles.len() > 1,
        ovs: handles.iter().map(overlay::charger).collect(),
        usage: contient_un(&m, MOTS_USAGE),
        tests: contient_un(&m, MOTS_TEST),
        termes: tm,
    };
    let faits = candidats(&cx, &gs, &res);
    // Marge : l'en-tête, les étiquettes de groupe et la ligne `next`.
    let utile = budget.saturating_sub(12 + 3 * gs.len().min(6));
    let pris = selectionner(&faits, utile);
    let nom = if it == Intention::Localiser { "locate" } else { "explain" };
    rendre(&faits, &pris, &gs, budget, nom)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fait(groupe: usize, genre: Genre, fichier: u64, valeur: f32, cout: usize) -> Fait {
        Fait { groupe, genre, fichier, valeur, cout, texte: String::new(), ligne: 0 }
    }

    #[test]
    fn jetons_de_code_de_la_question() {
        assert_eq!(
            jetons_code("Comment src/pages/Auth.tsx arrive-t-il jusqu'à hasControlChars ?"),
            vec!["src/pages/Auth.tsx", "hasControlChars"]
        );
        assert_eq!(jetons_code("Quels tests relancer si je modifie useUploadQueue.ts ?"), vec!["useUploadQueue.ts"]);
        assert_eq!(jetons_code("Que contient le module src/lib/drawing/recognition ?"), vec!["src/lib/drawing/recognition"]);
        // Des mots ordinaires, une phrase qui finit par un point, un nombre : aucun jeton de code.
        assert!(jetons_code("Où est vérifié l'URL Nexus. Version 1.2 ?").is_empty());
    }

    #[test]
    fn mots_cles_sans_accents() {
        let m = mots("Qu'est-ce qui CASSE… où ça s'arrête ?");
        assert!(contient_un(&m, MOTS_IMPACT));
        assert!(!contient_un(&m, MOTS_CHEMIN));
        assert!(contient_un(&mots("Comment A atteint-il B ?"), MOTS_FLUX));
        // Préfixe seulement pour les mots de 5 lettres et plus.
        assert!(contient_un(&mots("modifiera"), MOTS_IMPACT));
        assert!(!contient_un(&mots("quiche"), MOTS_USAGE));
    }

    #[test]
    fn selection_sous_budget_et_gating() {
        let faits = vec![
            fait(0, Genre::Def, 1, 2.0, 30),
            fait(0, Genre::Appele, 2, 0.6, 10),
            fait(1, Genre::Appele, 3, 5.0, 5), // la graine 1 n'a pas sa définition retenue : inéligible
            fait(1, Genre::Def, 4, 0.1, 100),  // ne tient pas
            fait(usize::MAX, Genre::Candidat, u64::MAX, 0.5, 8),
        ];
        let pris = selectionner(&faits, 50);
        assert!(!pris.contains(&2) && !pris.contains(&3), "{:?}", pris);
        assert_eq!(pris[0], 0, "la définition (meilleur rapport éligible) d'abord : {:?}", pris);
        let cout: usize = pris.iter().map(|&i| faits[i].cout).sum();
        assert!(cout <= 50, "{}", cout);
        assert!(pris.contains(&1) && pris.contains(&4));
        // Budget nul : rien.
        assert!(selectionner(&faits, 0).is_empty());
    }

    #[test]
    fn redondance_penalisee() {
        // Deux appelés de même valeur : le second du même fichier vaut moins que
        // celui d'un autre fichier, à coût égal.
        let faits = vec![
            fait(0, Genre::Def, 1, 2.0, 10),
            fait(0, Genre::Appele, 9, 0.5, 10),
            fait(0, Genre::Appele, 9, 0.5, 10),
            fait(0, Genre::Appele, 8, 0.45, 10),
        ];
        let pris = selectionner(&faits, 30);
        assert_eq!(pris, vec![0, 1, 3], "{:?}", pris);
    }
}
