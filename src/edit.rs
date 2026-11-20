//! `edit` : appliquer un LOT d'éditions sur plusieurs fichiers en UN appel (le « Code Mode » de
//! Cortex : utilisable par Bash — donc par les sous-agents — et par l'outil MCP `cortex_edit`).
//!
//! Garanties :
//!   - ATOMIQUE : toutes les éditions sont localisées et vérifiées AVANT la première écriture ;
//!     une seule erreur (ancre absente ou ambiguë, chemin interdit, syntaxe cassée…) = aucune
//!     écriture, et TOUTES les erreurs sont rendues d'un coup (un tour de correction, pas dix).
//!     Si une écriture échoue malgré tout (disque plein, verrou), les fichiers déjà écrits sont
//!     remis dans leur état d'avant.
//!   - Toutes les ancres sont cherchées dans le contenu ORIGINAL des fichiers (pas de dépendance
//!     d'ordre entre éditions) ; deux éditions qui se recouvrent sont refusées.
//!   - Fins de ligne (CRLF/LF), BOM UTF-8 et dernier retour à la ligne PRÉSERVÉS fichier par
//!     fichier : seules les plages éditées changent, octet pour octet.
//!   - Garde-fou de SYNTAXE (tree-sitter, JSON) : une édition qui fait passer un fichier de
//!     « syntaxe valide » à « erreurs » est refusée, avec la ligne du premier défaut.
//!   - Garde-fous de chemin : `.env*`, `.secrets/`, `.tauri/`, `.git/`, hors de la racine, clés
//!     privées : refus NON contournable. Liste `interdits` (syntaxe gitignore) et tailles
//!     maximales dans `.cortex/edit.toml` : contournables par `--forcer "<raison>"` (journalisé).
//!   - Journal des lots (`~/.cortex/edit-journal/<id>/`) avec le contenu d'avant : `--annuler`.
//!
//! Deux écritures du même lot : JSON (`{"editions":[…]}`) ou format TEXTE sans aucun échappement
//! (`@ fichier` / lignes `<` ancien / lignes `>` nouveau), détecté automatiquement.
//!
//! Le moteur ne connaît pas l'atlas : les identifiants `S:chemin#symbole` passent par le trait
//! `Resolveur` (branché sur l'atlas par `Contexte`, simulé dans les tests).

use crate::atlas::Handle;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

// ─── Limites par défaut ──────────────────────────────────────────────────────

pub const MAX_FICHIER_DEFAUT: u64 = 2 * 1024 * 1024;
pub const MAX_LOT_DEFAUT: u64 = 16 * 1024 * 1024;
pub const MAX_EDITIONS_DEFAUT: usize = 1000;
/// Occurrences remplacées par un seul `renomme`, au-delà desquelles il faut `--forcer`.
pub const MAX_RENOMMAGES: usize = 5000;
/// Nombre de lots gardés dans le journal (les plus anciens sont purgés).
const JOURNAL_GARDE: usize = 200;
/// Fichier de configuration (par projet) : lui-même interdit à l'édition.
pub const CONFIG_REL: &str = ".cortex/edit.toml";

// ─── Format du lot ───────────────────────────────────────────────────────────

/// Une édition telle qu'écrite dans le JSON (clés françaises, alias anglais).
#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
struct EditionBrute {
    #[serde(alias = "file", alias = "path")]
    fichier: Option<String>,
    #[serde(alias = "symbol")]
    symbole: Option<String>,
    #[serde(alias = "old")]
    ancien: Option<String>,
    #[serde(alias = "new")]
    nouveau: Option<String>,
    #[serde(alias = "all")]
    tous: Option<bool>,
    #[serde(alias = "after")]
    apres: Option<String>,
    #[serde(alias = "before")]
    avant: Option<String>,
    #[serde(alias = "insert")]
    insere: Option<String>,
    #[serde(alias = "append")]
    ajoute: Option<String>,
    #[serde(alias = "create")]
    cree: Option<String>,
    #[serde(alias = "replace")]
    remplace: Option<String>,
    #[serde(alias = "overwrite")]
    ecrase: Option<bool>,
    #[serde(alias = "rename")]
    renomme: Option<String>,
    #[serde(alias = "to")]
    en: Option<String>,
    #[serde(alias = "in")]
    dans: Option<String>,
    #[serde(alias = "substring")]
    partiel: Option<bool>,
    eol: Option<String>,
    bom: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Portee {
    Fichier(String),
    Symbole(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eol {
    Lf,
    Crlf,
}

impl Eol {
    fn texte(self) -> &'static str {
        match self {
            Eol::Lf => "\n",
            Eol::Crlf => "\r\n",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    /// Remplace `ancien` par `nouveau` (occurrence unique, sauf `tous`), dans un fichier ou un
    /// symbole. `lignes` : supprime aussi le retour à la ligne qui suit (suppression de lignes).
    Remplacer { portee: Portee, ancien: String, nouveau: String, tous: bool, lignes: bool },
    /// Insère les lignes `texte` juste avant / après la LIGNE qui contient l'ancre.
    Inserer { portee: Portee, apres: bool, ancre: String, texte: String },
    /// Ajoute `texte` à la fin du fichier (retour à la ligne garanti avant).
    Ajouter { fichier: String, texte: String },
    /// Crée un fichier (refus s'il existe, sauf `ecrase`).
    Creer { fichier: String, contenu: String, ecrase: bool, eol: Option<Eol>, bom: Option<bool> },
    /// Remplace TOUT le symbole (signature + corps) par `texte`.
    RemplacerSymbole { id: String, texte: String },
    /// Renomme un identifiant dans tout le projet (mot entier par défaut), sous le préfixe `dans`.
    Renommer { de: String, vers: String, dans: Option<String>, mots: bool },
}

fn convertir_brute(i: usize, b: EditionBrute) -> Result<Op, String> {
    let n = i + 1;
    let fam = [
        b.cree.is_some(),
        b.remplace.is_some(),
        b.ancien.is_some() || b.nouveau.is_some(),
        b.insere.is_some() || b.avant.is_some() || b.apres.is_some(),
        b.ajoute.is_some(),
        b.renomme.is_some(),
    ];
    if fam.iter().filter(|x| **x).count() != 1 {
        return Err(format!(
            "[{n}] une édition = UNE action : ancien+nouveau | apres/avant+insere | ajoute | cree | symbole+remplace | renomme+en"
        ));
    }
    if fam[5] {
        if b.fichier.is_some() || b.symbole.is_some() {
            return Err(format!("[{n}] `renomme` agit sur tout le projet : pas de `fichier`/`symbole` (restreindre : `dans`)"));
        }
        let (Some(de), Some(vers)) = (b.renomme, b.en) else { return Err(format!("[{n}] `renomme` demande `en` (nouveau nom)")) };
        return Ok(Op::Renommer { de, vers, dans: b.dans, mots: !b.partiel.unwrap_or(false) });
    }
    if b.dans.is_some() || b.partiel.is_some() || b.en.is_some() {
        return Err(format!("[{n}] `en`, `dans`, `partiel` ne vont qu'avec `renomme`"));
    }
    let portee = match (&b.fichier, &b.symbole) {
        (Some(f), None) => Some(Portee::Fichier(f.clone())),
        (None, Some(s)) => Some(Portee::Symbole(s.clone())),
        (None, None) => None,
        (Some(_), Some(_)) => return Err(format!("[{n}] `fichier` OU `symbole`, pas les deux")),
    };
    let besoin = |p: Option<Portee>| p.ok_or_else(|| format!("[{n}] `fichier` (ou `symbole`) manquant"));
    let eol = match b.eol.as_deref().map(|s| s.to_ascii_lowercase()) {
        None => None,
        Some(s) if s == "lf" => Some(Eol::Lf),
        Some(s) if s == "crlf" => Some(Eol::Crlf),
        Some(s) => return Err(format!("[{n}] eol « {s} » inconnu (lf | crlf)")),
    };
    if fam[0] {
        let Some(Portee::Fichier(f)) = portee else { return Err(format!("[{n}] `cree` demande `fichier`")) };
        return Ok(Op::Creer { fichier: f, contenu: b.cree.unwrap_or_default(), ecrase: b.ecrase.unwrap_or(false), eol, bom: b.bom });
    }
    if b.ecrase.is_some() || b.eol.is_some() || b.bom.is_some() {
        return Err(format!("[{n}] `ecrase`, `eol`, `bom` ne vont qu'avec `cree`"));
    }
    if fam[1] {
        let Some(Portee::Symbole(id)) = portee else { return Err(format!("[{n}] `remplace` demande `symbole`")) };
        return Ok(Op::RemplacerSymbole { id, texte: b.remplace.unwrap_or_default() });
    }
    if fam[4] {
        let Some(Portee::Fichier(f)) = portee else { return Err(format!("[{n}] `ajoute` demande `fichier`")) };
        return Ok(Op::Ajouter { fichier: f, texte: b.ajoute.unwrap_or_default() });
    }
    let portee = besoin(portee)?;
    if fam[2] {
        let (Some(ancien), Some(nouveau)) = (b.ancien, b.nouveau) else {
            return Err(format!("[{n}] `ancien` ET `nouveau` sont requis (`nouveau` peut être vide pour supprimer)"));
        };
        if ancien.is_empty() {
            return Err(format!("[{n}] `ancien` vide (pour ajouter du texte : `apres`/`avant`+`insere`, ou `ajoute`)"));
        }
        return Ok(Op::Remplacer { portee, ancien, nouveau, tous: b.tous.unwrap_or(false), lignes: false });
    }
    if b.tous.is_some() {
        return Err(format!("[{n}] `tous` ne va qu'avec `ancien`+`nouveau`"));
    }
    let Some(texte) = b.insere else { return Err(format!("[{n}] `insere` manquant")) };
    let (apres, ancre) = match (b.apres, b.avant) {
        (Some(a), None) => (true, a),
        (None, Some(a)) => (false, a),
        _ => return Err(format!("[{n}] `apres` OU `avant` (une seule ancre)")),
    };
    if ancre.is_empty() {
        return Err(format!("[{n}] ancre vide"));
    }
    Ok(Op::Inserer { portee, apres, ancre, texte })
}

/// Analyse un lot JSON : `{"editions":[…]}` (alias `edits`) ou directement `[…]`.
pub fn parser_valeur(v: &Value) -> Result<Vec<Op>, String> {
    let liste = match v {
        Value::Array(a) => a,
        Value::Object(o) => match o.get("editions").or_else(|| o.get("edits")) {
            Some(Value::Array(a)) => a,
            _ => return Err("lot attendu : {\"editions\":[…]} ou [...]".into()),
        },
        _ => return Err("lot attendu : {\"editions\":[…]} ou [...]".into()),
    };
    if liste.is_empty() {
        return Err("lot vide".into());
    }
    let mut ops = Vec::with_capacity(liste.len());
    let mut erreurs = Vec::new();
    for (i, e) in liste.iter().enumerate() {
        match serde_json::from_value::<EditionBrute>(e.clone()) {
            Ok(b) => match convertir_brute(i, b) {
                Ok(o) => ops.push(o),
                Err(m) => erreurs.push(m),
            },
            Err(m) => erreurs.push(format!("[{}] {}", i + 1, m)),
        }
    }
    if erreurs.is_empty() {
        Ok(ops)
    } else {
        Err(erreurs.join("\n"))
    }
}

// ─── Format texte (sans échappement) ─────────────────────────────────────────

/// Découpe une ligne d'en-tête en mots ; les guillemets `"` protègent les espaces (identifiants
/// de symboles qui en contiennent).
fn mots_entete(s: &str) -> Vec<String> {
    let mut v = Vec::new();
    let mut cur = String::new();
    let mut q = false;
    let mut vide_ok = false;
    for c in s.chars() {
        match c {
            '"' => {
                q = !q;
                vide_ok = true;
            }
            c if c.is_whitespace() && !q => {
                if !cur.is_empty() || vide_ok {
                    v.push(std::mem::take(&mut cur));
                    vide_ok = false;
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() || vide_ok {
        v.push(cur);
    }
    v
}

fn est_symbole(cible: &str) -> bool {
    if cible.starts_with("S:") || cible.starts_with("D:") || cible.contains('#') {
        return true;
    }
    // chemin:12-30
    match cible.rsplit_once(':') {
        Some((p, l)) if !p.is_empty() => {
            let mut it = l.splitn(2, '-');
            matches!((it.next(), it.next()), (Some(a), Some(b)) if !a.is_empty() && !b.is_empty() && a.bytes().all(|x| x.is_ascii_digit()) && b.bytes().all(|x| x.is_ascii_digit()))
        }
        _ => false,
    }
}

struct EnCours {
    n: usize,
    mots: Vec<String>,
    vieux: Vec<String>,
    neuf: Vec<String>,
    a_vieux: bool,
    a_neuf: bool,
}

fn finir_bloc(b: EnCours) -> Result<Op, String> {
    let n = b.n;
    let Some(cible) = b.mots.first().cloned() else {
        return Err(format!("ligne {n} : `@` sans cible (@ chemin | @ \"S:id\" | @ rename Ancien Nouveau)"));
    };
    let drapeaux: Vec<String> = b.mots[1..].iter().map(|m| m.to_ascii_lowercase()).collect();
    const CONNUS: &[&str] = &[
        "new",
        "cree",
        "create",
        "overwrite",
        "ecrase",
        "crlf",
        "lf",
        "bom",
        "append",
        "ajoute",
        "after",
        "apres",
        "before",
        "avant",
        "all",
        "tous",
    ];
    if !matches!(cible.to_ascii_lowercase().as_str(), "rename" | "renomme") {
        if let Some(inconnu) = drapeaux.iter().find(|d| !CONNUS.contains(&d.as_str())) {
            return Err(format!("ligne {n} : option « {inconnu} » inconnue (new overwrite crlf lf bom | append | after before | tous)"));
        }
    }
    let a = |noms: &[&str]| drapeaux.iter().any(|d| noms.contains(&d.as_str()));
    let vieux = b.vieux.join("\n");
    let neuf = b.neuf.join("\n");
    if matches!(cible.to_ascii_lowercase().as_str(), "rename" | "renomme") {
        if b.a_vieux || b.a_neuf {
            return Err(format!("ligne {n} : `@ rename` n'a pas de lignes `<` / `>`"));
        }
        let (Some(de), Some(vers)) = (b.mots.get(1).cloned(), b.mots.get(2).cloned()) else {
            return Err(format!("ligne {n} : @ rename Ancien Nouveau [dans <préfixe>] [partiel]"));
        };
        let mut dans = None;
        let mut mots = true;
        let mut i = 3;
        while i < b.mots.len() {
            match b.mots[i].to_ascii_lowercase().as_str() {
                "dans" | "in" => {
                    dans = b.mots.get(i + 1).cloned();
                    i += 1;
                }
                "partiel" | "substring" => mots = false,
                autre => return Err(format!("ligne {n} : option « {autre} » inconnue pour rename (dans <préfixe> | partiel)")),
            }
            i += 1;
        }
        return Ok(Op::Renommer { de, vers, dans, mots });
    }
    let portee = if est_symbole(&cible) { Portee::Symbole(cible.clone()) } else { Portee::Fichier(cible.clone()) };
    let fichier = |p: &Portee| match p {
        Portee::Fichier(f) => Ok(f.clone()),
        Portee::Symbole(_) => Err(format!("ligne {n} : cette action demande un chemin de fichier, pas un symbole")),
    };
    if a(&["new", "cree", "create"]) {
        if b.a_vieux {
            return Err(format!("ligne {n} : `new` n'a que des lignes `>` (le contenu du fichier)"));
        }
        let mut contenu = neuf;
        if !contenu.is_empty() {
            contenu.push('\n');
        }
        let eol = if a(&["crlf"]) {
            Some(Eol::Crlf)
        } else if a(&["lf"]) {
            Some(Eol::Lf)
        } else {
            None
        };
        return Ok(Op::Creer {
            fichier: fichier(&portee)?,
            contenu,
            ecrase: a(&["overwrite", "ecrase"]),
            eol,
            bom: if a(&["bom"]) { Some(true) } else { None },
        });
    }
    if a(&["append", "ajoute"]) {
        if b.a_vieux || !b.a_neuf {
            return Err(format!("ligne {n} : `append` n'a que des lignes `>`"));
        }
        return Ok(Op::Ajouter { fichier: fichier(&portee)?, texte: neuf });
    }
    let apres = a(&["after", "apres"]);
    let avant = a(&["before", "avant"]);
    if apres || avant {
        if !b.a_vieux || !b.a_neuf {
            return Err(format!("ligne {n} : `after`/`before` demande des lignes `<` (ancre) ET `>` (lignes à insérer)"));
        }
        return Ok(Op::Inserer { portee, apres, ancre: vieux, texte: neuf });
    }
    let tous = a(&["all", "tous"]);
    match (b.a_vieux, b.a_neuf) {
        (true, true) => Ok(Op::Remplacer { portee, ancien: vieux, nouveau: neuf, tous, lignes: false }),
        (true, false) => Ok(Op::Remplacer { portee, ancien: vieux, nouveau: String::new(), tous, lignes: true }),
        (false, true) => match portee {
            Portee::Symbole(id) => Ok(Op::RemplacerSymbole { id, texte: neuf }),
            Portee::Fichier(_) => Err(format!("ligne {n} : sur un fichier, donne une ancre `<` (ou `new` / `append`)")),
        },
        (false, false) => Err(format!("ligne {n} : aucune ligne `<` ni `>`")),
    }
}

/// Format texte : `@ cible [options]` puis des lignes `< ancien` et `> nouveau` (un espace après le
/// signe est retiré, le reste est verbatim : aucun échappement). Une ligne `<` seule = ligne vide.
/// Aucune ligne `>` avec des `<` = suppression de lignes. Lignes vides et `# commentaire` ignorées.
pub fn parser_texte(txt: &str) -> Result<Vec<Op>, String> {
    let mut ops = Vec::new();
    let mut erreurs = Vec::new();
    let fin = |c: Option<EnCours>, ops: &mut Vec<Op>, erreurs: &mut Vec<String>| {
        if let Some(c) = c {
            match finir_bloc(c) {
                Ok(o) => ops.push(o),
                Err(e) => erreurs.push(e),
            }
        }
    };
    let mut cur: Option<EnCours> = None;
    let mut precedente: Option<String> = None;
    for (i, brut) in txt.lines().enumerate() {
        let l = brut.strip_suffix('\r').unwrap_or(brut);
        let n = i + 1;
        if let Some(reste) = l.strip_prefix('@') {
            fin(cur.take(), &mut ops, &mut erreurs);
            let mut mots = mots_entete(reste);
            if mots.is_empty() {
                // `@` seul : même cible que l'édition précédente (économise la ligne d'en-tête répétée).
                if let Some(c) = &precedente {
                    mots = vec![c.clone()];
                }
            } else if !matches!(mots[0].to_ascii_lowercase().as_str(), "rename" | "renomme") {
                precedente = Some(mots[0].clone());
            }
            cur = Some(EnCours { n, mots, vieux: vec![], neuf: vec![], a_vieux: false, a_neuf: false });
        } else if let Some(reste) = l.strip_prefix('<') {
            let Some(c) = cur.as_mut() else {
                erreurs.push(format!("ligne {n} : `<` avant tout `@ cible`"));
                continue;
            };
            if c.a_neuf {
                erreurs.push(format!(
                    "ligne {n} : `<` après `>` : une édition = ses lignes `<` PUIS ses lignes `>` (nouvelle édition : `@ cible`)"
                ));
                continue;
            }
            c.a_vieux = true;
            c.vieux.push(reste.strip_prefix(' ').unwrap_or(reste).to_string());
        } else if let Some(reste) = l.strip_prefix('>') {
            let Some(c) = cur.as_mut() else {
                erreurs.push(format!("ligne {n} : `>` avant tout `@ cible`"));
                continue;
            };
            c.a_neuf = true;
            c.neuf.push(reste.strip_prefix(' ').unwrap_or(reste).to_string());
        } else if l.trim().is_empty() || l.trim_start().starts_with('#') {
            continue;
        } else {
            erreurs.push(format!("ligne {n} : « {} » inattendu (une ligne commence par @, < ou >)", apercu(l)));
        }
    }
    fin(cur.take(), &mut ops, &mut erreurs);
    if ops.is_empty() && erreurs.is_empty() {
        erreurs.push("lot vide".into());
    }
    if erreurs.is_empty() {
        Ok(ops)
    } else {
        Err(erreurs.join("\n"))
    }
}

/// Texte d'entrée : JSON (commence par `{` ou `[`) ou format texte.
pub fn parser(texte: &str) -> Result<Vec<Op>, String> {
    let t = texte.trim_start_matches('\u{feff}');
    let debut = t.trim_start();
    if debut.starts_with('{') || debut.starts_with('[') {
        let v: Value = serde_json::from_str(debut.trim()).map_err(|e| format!("JSON invalide : {e}"))?;
        parser_valeur(&v)
    } else {
        parser_texte(t)
    }
}

/// Octets d'entrée (stdin, fichier) → texte : UTF-8, avec BOM UTF-8 ou UTF-16 toléré (PowerShell).
pub fn decoder_entree(octets: &[u8]) -> Result<String, String> {
    if octets.starts_with(&[0xFF, 0xFE]) || octets.starts_with(&[0xFE, 0xFF]) {
        let le = octets[0] == 0xFF;
        let u: Vec<u16> = octets[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) })
            .collect();
        return String::from_utf16(&u).map_err(|_| "entrée UTF-16 invalide".to_string());
    }
    String::from_utf8(octets.to_vec()).map_err(|_| "entrée non UTF-8".to_string())
}

// ─── Résolution des symboles ─────────────────────────────────────────────────

/// Plage de lignes (1-based, bornes incluses) d'un symbole, dans un fichier relatif à la racine.
#[derive(Debug, Clone)]
pub struct CibleSymbole {
    pub fichier: String,
    pub debut: u32,
    pub fin: u32,
}

pub trait Resolveur {
    fn symbole(&self, id: &str) -> Result<CibleSymbole, String>;
    /// (nombre d'appelants, quelques identifiants) d'un symbole : sert à signaler un changement
    /// de signature. Sans atlas : aucun.
    fn appelants(&self, _id: &str) -> (usize, Vec<String>) {
        (0, Vec::new())
    }
}

/// Résolveur sans atlas : toute édition par symbole est refusée.
pub struct SansAtlas;
impl Resolveur for SansAtlas {
    fn symbole(&self, id: &str) -> Result<CibleSymbole, String> {
        Err(format!("« {id} » : aucun projet Cortex indexé ici (cortex index <chemin> --name <Projet>, ou -p <Projet>)"))
    }
}

// ─── Garde-fous de chemin ────────────────────────────────────────────────────

#[derive(Debug)]
pub enum Refus {
    /// Jamais contournable (secrets, .git, hors racine…).
    Dur(String),
    /// Contournable par `--forcer "<raison>"` (liste du projet, tailles).
    Souple(String),
}

#[derive(Debug, Clone, Default)]
struct ConfigEdit {
    interdits: Vec<String>,
    max_fichier: Option<u64>,
    max_lot: Option<u64>,
    max_editions: Option<usize>,
}

fn lire_config(chemin: &Path, c: &mut ConfigEdit) {
    let Ok(txt) = std::fs::read_to_string(chemin) else { return };
    for (_, cle, val) in crate::profile::parse_kv(&txt) {
        match (cle.as_str(), val) {
            ("interdits", crate::profile::Val::List(l)) => c.interdits.extend(l),
            ("interdits", crate::profile::Val::Str(s)) => c.interdits.push(s),
            ("taille_max_fichier", crate::profile::Val::Num(n)) if n > 0.0 => c.max_fichier = Some(n as u64),
            ("taille_max_lot", crate::profile::Val::Num(n)) if n > 0.0 => c.max_lot = Some(n as u64),
            ("max_editions", crate::profile::Val::Num(n)) if n > 0.0 => c.max_editions = Some(n as usize),
            _ => {}
        }
    }
}

struct Garde {
    racine: PathBuf,
    interdits: Option<ignore::gitignore::Gitignore>,
    max_fichier: u64,
    max_lot: u64,
    max_editions: usize,
}

const NOMS_RESERVES_WINDOWS: &[&str] = &["con", "prn", "aux", "nul"];

fn nom_normalise(c: &str) -> String {
    c.trim_end_matches(['.', ' ']).to_lowercase()
}

/// Refus DUR portant sur les composants d'un chemin relatif (déjà normalisé en `/`).
fn refus_dur_composants(rel: &str) -> Option<String> {
    let comps: Vec<String> = rel.split('/').filter(|c| !c.is_empty()).map(nom_normalise).collect();
    for (i, c) in comps.iter().enumerate() {
        let dernier = i + 1 == comps.len();
        let motif = match c.as_str() {
            ".git" => Some("dépôt git"),
            ".secrets" => Some("dossier de secrets"),
            ".tauri" => Some("clés de signature Tauri"),
            ".ssh" => Some("clés SSH"),
            n if n.starts_with(".env") || n.ends_with(".env") => Some("fichier d'environnement"),
            n if NOMS_RESERVES_WINDOWS.contains(&n.split('.').next().unwrap_or("")) => Some("nom réservé Windows"),
            n if dernier
                && (n.ends_with(".pem")
                    || n.ends_with(".pfx")
                    || n.ends_with(".p12")
                    || n.starts_with("id_rsa")
                    || n.starts_with("id_ed25519")
                    || n.starts_with("id_ecdsa")) =>
            {
                Some("clé privée")
            }
            _ => None,
        };
        if let Some(m) = motif {
            return Some(format!("« {rel} » refusé ({m}) : jamais écrit par cortex edit, pas de contournement"));
        }
    }
    if comps.join("/") == CONFIG_REL {
        return Some(format!("« {CONFIG_REL} » refusé : les garde-fous ne se modifient pas par un lot"));
    }
    None
}

fn normaliser_sep(s: &str) -> String {
    s.replace('\\', "/")
}

fn sans_prefixe_verbatim(p: &Path) -> String {
    let s = p.to_string_lossy().to_string();
    normaliser_sep(s.strip_prefix(r"\\?\").unwrap_or(&s))
}

fn est_absolu(s: &str) -> bool {
    s.starts_with('/') || (s.len() >= 2 && s.as_bytes()[1] == b':' && s.as_bytes()[0].is_ascii_alphabetic())
}

impl Garde {
    fn nouveau(racine: &Path, config_globale: Option<&Path>) -> Result<Garde, String> {
        let racine = std::fs::canonicalize(racine).map_err(|e| format!("racine « {} » illisible : {e}", racine.display()))?;
        let mut cfg = ConfigEdit::default();
        if let Some(g) = config_globale {
            lire_config(g, &mut cfg);
        }
        let mut local = ConfigEdit::default();
        lire_config(&racine.join(CONFIG_REL), &mut local);
        cfg.interdits.extend(local.interdits);
        cfg.max_fichier = local.max_fichier.or(cfg.max_fichier);
        cfg.max_lot = local.max_lot.or(cfg.max_lot);
        cfg.max_editions = local.max_editions.or(cfg.max_editions);
        let interdits = if cfg.interdits.is_empty() {
            None
        } else {
            let mut b = ignore::gitignore::GitignoreBuilder::new(&racine);
            for l in &cfg.interdits {
                let _ = b.add_line(None, l);
            }
            b.build().ok()
        };
        Ok(Garde {
            racine,
            interdits,
            max_fichier: cfg.max_fichier.unwrap_or(MAX_FICHIER_DEFAUT),
            max_lot: cfg.max_lot.unwrap_or(MAX_LOT_DEFAUT),
            max_editions: cfg.max_editions.unwrap_or(MAX_EDITIONS_DEFAUT),
        })
    }

    /// Chemin brut (relatif à la racine ou absolu dans la racine) → (relatif en `/`, absolu,
    /// vrai si la liste `interdits` le visait et que `souple_ok` l'a laissé passer).
    fn verifier(&self, brut: &str, souple_ok: bool) -> Result<(String, PathBuf, bool), Refus> {
        let brut = brut.trim();
        if brut.is_empty() || brut.contains('\0') {
            return Err(Refus::Dur("chemin vide ou invalide".into()));
        }
        let mut brut = normaliser_sep(brut);
        if let Some(s) = brut.strip_prefix("//?/") {
            brut = s.to_string(); // préfixe « verbatim » de Windows (\\?\C:\…)
        }
        // Git Bash : /c/Users/… = C:/Users/…
        if cfg!(windows) {
            let b = brut.as_bytes();
            if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b'/' {
                brut = format!("{}:{}", b[1].to_ascii_uppercase() as char, &brut[2..]);
            }
        }
        let rel_brut = if est_absolu(&brut) {
            let racine = sans_prefixe_verbatim(&self.racine);
            let (a, r) = (brut.to_lowercase(), racine.to_lowercase());
            let r = r.trim_end_matches('/');
            if a == r || !a.starts_with(&format!("{r}/")) {
                return Err(Refus::Dur(format!("« {brut} » est hors de la racine du projet ({racine})")));
            }
            match brut.get(r.len() + 1..) {
                Some(s) => s.to_string(),
                None => return Err(Refus::Dur(format!("« {brut} » : chemin non analysable"))),
            }
        } else {
            brut
        };
        let mut comps: Vec<&str> = Vec::new();
        for c in rel_brut.split('/') {
            match c {
                "" | "." => {}
                ".." => {
                    if comps.pop().is_none() {
                        return Err(Refus::Dur(format!("« {rel_brut} » sort de la racine du projet")));
                    }
                }
                c => {
                    if c.contains(':') || c.contains(['<', '>', '|', '*', '?', '"']) {
                        return Err(Refus::Dur(format!("« {rel_brut} » : caractère interdit dans « {c} »")));
                    }
                    comps.push(c);
                }
            }
        }
        if comps.is_empty() {
            return Err(Refus::Dur("chemin vide".into()));
        }
        let rel = comps.join("/");
        if let Some(m) = refus_dur_composants(&rel) {
            return Err(Refus::Dur(m));
        }
        let abs = self.racine.join(&rel);
        // Chemin réel : liens symboliques, jonctions, noms courts 8.3 ne doivent pas échapper aux règles.
        let mut a_resoudre = abs.clone();
        let mut queue: Vec<std::ffi::OsString> = Vec::new();
        let reel = loop {
            match std::fs::canonicalize(&a_resoudre) {
                Ok(c) => break c,
                Err(_) => match (a_resoudre.file_name().map(|n| n.to_os_string()), a_resoudre.parent()) {
                    (Some(n), Some(p)) => {
                        queue.push(n);
                        a_resoudre = p.to_path_buf();
                    }
                    _ => return Err(Refus::Dur(format!("« {rel} » : chemin non résolvable"))),
                },
            }
        };
        let mut reel_complet = reel;
        for q in queue.iter().rev() {
            reel_complet.push(q);
        }
        let reel_s = sans_prefixe_verbatim(&reel_complet);
        let racine_s = sans_prefixe_verbatim(&self.racine);
        let (rl, rr) = (reel_s.to_lowercase(), racine_s.to_lowercase());
        let rr = rr.trim_end_matches('/');
        let Some(rel_reel) = rl.starts_with(&format!("{rr}/")).then(|| reel_s.get(rr.len() + 1..)).flatten() else {
            return Err(Refus::Dur(format!("« {rel} » pointe hors de la racine du projet (lien ou jonction)")));
        };
        if let Some(m) = refus_dur_composants(rel_reel) {
            return Err(Refus::Dur(m));
        }
        if abs.is_dir() {
            return Err(Refus::Dur(format!("« {rel} » est un dossier")));
        }
        let mut vise = false;
        if let Some(gi) = &self.interdits {
            if gi.matched_path_or_any_parents(Path::new(&rel), false).is_ignore() {
                if !souple_ok {
                    return Err(Refus::Souple(format!(
                        "« {rel} » est dans `interdits` de la configuration ({CONFIG_REL} ou ~/.cortex/edit.toml)"
                    )));
                }
                vise = true;
            }
        }
        Ok((rel, abs, vise))
    }
}

// ─── Options et rapport ──────────────────────────────────────────────────────

pub struct Options {
    pub essai: bool,
    /// Rapport détaillé : une ligne par édition (verbe, lignes) et un diff court.
    pub diff: bool,
    pub forcer: Option<String>,
    /// Dossier du journal des lots (défaut : `~/.cortex/edit-journal`).
    pub journal: PathBuf,
    /// Configuration globale (défaut : `~/.cortex/edit.toml`).
    pub config_globale: Option<PathBuf>,
    pub projet: Option<String>,
}

impl Options {
    pub fn par_defaut() -> Options {
        let home = crate::index::cortex_home();
        Options {
            essai: false,
            diff: false,
            forcer: None,
            journal: home.join("edit-journal"),
            config_globale: Some(home.join("edit.toml")),
            projet: None,
        }
    }
}

#[derive(Debug)]
pub struct Rapport {
    pub ok: bool,
    pub texte: String,
    #[allow(dead_code)]
    pub id: Option<String>,
    pub ecrits: Vec<String>,
    pub projet: Option<String>,
}

fn echec(texte: String) -> Rapport {
    Rapport { ok: false, texte, id: None, ecrits: vec![], projet: None }
}

// ─── Fichiers en mémoire ─────────────────────────────────────────────────────

struct Plage {
    debut: usize,
    fin: usize,
    texte: String,
    op: usize,
}

struct Fich {
    rel: String,
    abs: PathBuf,
    existait: bool,
    bom: bool,
    eol: Eol,
    octets: Vec<u8>,
    texte: String,
    meta: Option<(u64, std::time::SystemTime)>,
    starts: Vec<usize>,
    plages: Vec<Plage>,
    creation: Option<Vec<u8>>,
    dirs_a_creer: Vec<PathBuf>,
}

impl Fich {
    fn ligne_de(&self, pos: usize) -> usize {
        self.starts.partition_point(|&s| s <= pos).max(1)
    }
    fn nb_lignes(&self) -> usize {
        if self.texte.is_empty() {
            0
        } else if self.texte.ends_with('\n') {
            self.starts.len() - 1
        } else {
            self.starts.len()
        }
    }
    /// Plage d'octets [début de la ligne a, fin de la ligne b sans son retour à la ligne].
    fn plage_lignes(&self, a: usize, b: usize) -> (usize, usize) {
        let debut = self.starts[a - 1];
        let fin = if b >= self.starts.len() {
            self.texte.len()
        } else {
            let mut f = self.starts[b] - 1;
            if f > debut && self.texte.as_bytes()[f - 1] == b'\r' {
                f -= 1;
            }
            f
        };
        (debut, fin.max(debut))
    }
}

fn starts_de(t: &str) -> Vec<usize> {
    let mut v = vec![0];
    v.extend(memchr::memchr_iter(b'\n', t.as_bytes()).map(|i| i + 1));
    v
}

/// (dominant, mixte) : les fins de ligne du texte.
fn detecter_eol(t: &str) -> (Eol, bool) {
    let total = memchr::memchr_iter(b'\n', t.as_bytes()).count();
    let crlf = memchr::memmem::find_iter(t.as_bytes(), b"\r\n").count();
    let lf = total - crlf;
    (if crlf > lf { Eol::Crlf } else { Eol::Lf }, crlf > 0 && lf > 0)
}

/// Texte de l'agent → fins de ligne du fichier.
fn vers_eol(s: &str, eol: Eol) -> String {
    let lf = s.replace("\r\n", "\n");
    match eol {
        Eol::Lf => lf,
        Eol::Crlf => lf.replace('\n', "\r\n"),
    }
}

/// Lignes entières : texte aux fins de ligne du fichier, terminé par un retour à la ligne.
fn lignes_entieres(s: &str, eol: Eol) -> String {
    let mut t = vers_eol(s, eol);
    if !t.ends_with('\n') {
        t.push_str(eol.texte());
    }
    t
}

fn apercu(s: &str) -> String {
    let t: String = s.chars().take(48).collect::<String>().replace("\r\n", "¶").replace('\n', "¶");
    if s.chars().count() > 48 {
        format!("{t}…")
    } else {
        t
    }
}

fn hash_hex(b: &[u8]) -> String {
    blake3::hash(b).to_hex().to_string()
}

fn lire_fichier(garde: &Garde, rel: &str, abs: &Path, opts: &Options) -> Result<Fich, String> {
    let meta = std::fs::metadata(abs).map_err(|e| format!("« {rel} » illisible : {e}"))?;
    if meta.len() > garde.max_fichier && opts.forcer.is_none() {
        return Err(format!(
            "« {rel} » fait {} Mo (> {} Mo, `taille_max_fichier`) ; contourner : --forcer \"raison\"",
            meta.len() / 1_048_576,
            garde.max_fichier / 1_048_576
        ));
    }
    let octets = std::fs::read(abs).map_err(|e| format!("« {rel} » illisible : {e}"))?;
    if octets.starts_with(&[0xFF, 0xFE]) || octets.starts_with(&[0xFE, 0xFF]) {
        return Err(format!("« {rel} » est en UTF-16 : non pris en charge (conversion à faire avant)"));
    }
    if memchr::memchr(0, &octets).is_some() {
        return Err(format!("« {rel} » est binaire (octet nul) : non pris en charge"));
    }
    let bom = octets.starts_with(&[0xEF, 0xBB, 0xBF]);
    let corps = &octets[if bom { 3 } else { 0 }..];
    let texte = String::from_utf8(corps.to_vec()).map_err(|_| format!("« {rel} » n'est pas de l'UTF-8 valide : non pris en charge"))?;
    let (eol, _) = detecter_eol(&texte);
    let starts = starts_de(&texte);
    Ok(Fich {
        rel: rel.to_string(),
        abs: abs.to_path_buf(),
        existait: true,
        bom,
        eol,
        meta: meta.modified().ok().map(|m| (meta.len(), m)),
        octets,
        texte,
        starts,
        plages: Vec::new(),
        creation: None,
        dirs_a_creer: Vec::new(),
    })
}

fn fichier_neuf(rel: &str, abs: &Path, racine: &Path) -> Fich {
    let mut dirs = Vec::new();
    let mut p = abs.parent();
    while let Some(d) = p {
        if d.exists() || !d.starts_with(racine) {
            break;
        }
        dirs.push(d.to_path_buf());
        p = d.parent();
    }
    Fich {
        rel: rel.to_string(),
        abs: abs.to_path_buf(),
        existait: false,
        bom: false,
        eol: Eol::Lf,
        octets: Vec::new(),
        texte: String::new(),
        meta: None,
        starts: vec![0],
        plages: Vec::new(),
        creation: None,
        dirs_a_creer: dirs,
    }
}

// ─── Localisation des ancres ─────────────────────────────────────────────────

/// Une occurrence : plage d'octets, et (si trouvée par tolérance d'indentation) l'indentation
/// du fichier et celle de l'ancre de l'agent, pour recaler le texte de remplacement.
struct Occ {
    debut: usize,
    fin: usize,
    recale: Option<(String, String)>,
}

fn indentation(l: &str) -> &str {
    &l[..l.len() - l.trim_start_matches([' ', '\t']).len()]
}

/// Positions exactes de `aiguille` dans `f.texte[lo..hi]` (non chevauchantes). Pour un fichier à
/// fins de ligne mixtes, essaie aussi l'aiguille telle quelle.
fn occurrences(f: &Fich, aiguille: &str, lo: usize, hi: usize) -> Vec<Occ> {
    let conv = vers_eol(aiguille, f.eol);
    let zone = &f.texte.as_bytes()[lo..hi];
    let mut v: Vec<Occ> =
        memchr::memmem::find_iter(zone, conv.as_bytes()).map(|p| Occ { debut: lo + p, fin: lo + p + conv.len(), recale: None }).collect();
    if v.is_empty() && conv != aiguille {
        v = memchr::memmem::find_iter(zone, aiguille.as_bytes())
            .map(|p| Occ { debut: lo + p, fin: lo + p + aiguille.len(), recale: None })
            .collect();
    }
    v
}

/// Repli quand l'ancre exacte est absente : blocs de LIGNES ENTIÈRES égales à l'ancre à
/// l'indentation et aux espaces de fin près (cause n°1 des « ancre introuvable »).
fn occurrences_tolerantes(f: &Fich, ancien: &str, lo: usize, hi: usize) -> Vec<Occ> {
    let mut a: Vec<&str> = ancien.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect();
    while a.len() > 1 && a.last().is_some_and(|l| l.trim().is_empty()) {
        a.pop();
    }
    if a.iter().all(|l| l.trim().is_empty()) || f.nb_lignes() == 0 {
        return Vec::new();
    }
    let l0 = f.ligne_de(lo).max(1);
    let l1 = f.ligne_de(hi.saturating_sub(1).max(lo)).min(f.nb_lignes());
    let lignes: Vec<(usize, usize)> = (l0..=l1).map(|k| f.plage_lignes(k, k)).collect();
    let texte = |k: usize| &f.texte[lignes[k].0..lignes[k].1];
    let mut res = Vec::new();
    let mut k = 0;
    while k + a.len() <= lignes.len() {
        if (0..a.len()).all(|j| texte(k + j).trim() == a[j].trim()) {
            let d = lignes[k].0;
            let e = lignes[k + a.len() - 1].1;
            if d >= lo && e <= hi {
                res.push(Occ { debut: d, fin: e, recale: Some((indentation(texte(k)).to_string(), indentation(a[0]).to_string())) });
                k += a.len();
                continue;
            }
        }
        k += 1;
    }
    res
}

/// Recale l'indentation du texte de remplacement : le préfixe `ind_agent` devient `ind_fichier`.
fn recaler(texte: &str, ind_fichier: &str, ind_agent: &str) -> String {
    if ind_fichier == ind_agent {
        return texte.to_string();
    }
    texte
        .split('\n')
        .map(|l| match l.strip_prefix(ind_agent) {
            Some(r) if !l.trim().is_empty() => format!("{ind_fichier}{r}"),
            _ => l.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn lignes_de(f: &Fich, occ: &[Occ]) -> String {
    let mut s: Vec<String> = occ.iter().take(6).map(|o| format!("L{}", f.ligne_de(o.debut))).collect();
    if occ.len() > 6 {
        s.push("…".into());
    }
    s.join(", ")
}

fn indice_absent(f: &Fich, ancien: &str, lo: usize, hi: usize) -> String {
    let premiere = ancien.lines().map(str::trim).find(|l| l.chars().count() >= 3);
    if let Some(l) = premiere {
        let zone = &f.texte.as_bytes()[lo..hi];
        let v: Vec<Occ> =
            memchr::memmem::find_iter(zone, l.as_bytes()).map(|p| Occ { debut: lo + p, fin: lo + p + l.len(), recale: None }).collect();
        if !v.is_empty() {
            return format!(" (sa ligne « {} » apparaît {} : lignes suivantes ou texte différents ?)", apercu(l), lignes_de(f, &v));
        }
    }
    String::new()
}

fn court_diff(ancien: &str, nouveau: &str) -> String {
    let mut s = String::new();
    for (sig, t) in [('-', ancien), ('+', nouveau)] {
        let lignes: Vec<&str> = t.lines().collect();
        for l in lignes.iter().take(4) {
            let l: String = l.chars().take(110).collect();
            s.push_str(&format!("      {sig} {l}\n"));
        }
        if lignes.len() > 4 {
            s.push_str(&format!("      {sig} … (+{} l.)\n", lignes.len() - 4));
        }
    }
    s
}

fn nb_l(s: &str) -> usize {
    if s.is_empty() {
        0
    } else {
        s.lines().count().max(1)
    }
}

fn est_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

/// Occurrences de `de` dans `t`, en mot entier si `mots` (bords identifiants seulement).
fn occurrences_mot(t: &[u8], de: &[u8], mots: bool) -> Vec<(usize, usize)> {
    let (g, d) = (de.first().copied().is_some_and(est_ident), de.last().copied().is_some_and(est_ident));
    memchr::memmem::find_iter(t, de)
        .filter_map(|p| {
            let q = p + de.len();
            if mots && ((g && p > 0 && est_ident(t[p - 1])) || (d && q < t.len() && est_ident(t[q]))) {
                None
            } else {
                Some((p, q))
            }
        })
        .collect()
}

// ─── Rapport par fichier ─────────────────────────────────────────────────────

#[derive(Default)]
struct Resume {
    lignes: BTreeSet<usize>,
    etiquettes: BTreeSet<&'static str>,
    n: usize,
}

fn liste_lignes(l: &BTreeSet<usize>) -> String {
    let mut s: Vec<String> = l.iter().take(8).map(|x| format!("L{x}")).collect();
    if l.len() > 8 {
        s.push(format!("+{}", l.len() - 8));
    }
    s.join(" ")
}

// ─── Application d'un lot ────────────────────────────────────────────────────

pub fn appliquer(racine: &Path, ops: &[Op], opts: &Options, res: &dyn Resolveur) -> Rapport {
    let garde = match Garde::nouveau(racine, opts.config_globale.as_deref()) {
        Ok(g) => g,
        Err(e) => return echec(format!("cortex edit : {e}\n")),
    };
    if ops.len() > garde.max_editions && opts.forcer.is_none() {
        return echec(format!(
            "cortex edit : {} éditions (> {}, `max_editions`) ; contourner : --forcer \"raison\"\n",
            ops.len(),
            garde.max_editions
        ));
    }
    let mut erreurs: Vec<String> = Vec::new();
    let mut fichiers: Vec<Fich> = Vec::new();
    let mut index: BTreeMap<String, usize> = BTreeMap::new();
    let mut details: Vec<String> = Vec::new();
    let mut recaps: Vec<String> = Vec::new();
    let mut resumes: BTreeMap<usize, Resume> = BTreeMap::new();
    let mut avertissements: Vec<String> = Vec::new();
    let mut forces: Vec<String> = Vec::new();

    macro_rules! charger {
        ($n:expr, $brut:expr, $creation:expr) => {{
            match charger(&garde, &mut fichiers, &mut index, $brut, $creation, opts, &mut forces) {
                Ok(i) => i,
                Err(m) => {
                    erreurs.push(format!("[{}] {}", $n, m));
                    continue;
                }
            }
        }};
    }

    for (i, op) in ops.iter().enumerate() {
        let n = i + 1;
        match op {
            Op::Creer { fichier, contenu, ecrase, eol, bom } => {
                let k = charger!(n, fichier, Some(*ecrase));
                let f = &mut fichiers[k];
                if f.creation.is_some() || !f.plages.is_empty() {
                    erreurs.push(format!(
                        "[{n}] « {} » est déjà touché par une autre édition du lot : un fichier créé a UN seul `cree`",
                        f.rel
                    ));
                    continue;
                }
                let eol_final = eol.unwrap_or(if f.existait { f.eol } else { Eol::Lf });
                let corps = if eol.is_some() || f.existait { vers_eol(contenu, eol_final) } else { contenu.clone() };
                let bom_final = bom.unwrap_or(f.bom);
                let mut octets = Vec::new();
                if bom_final {
                    octets.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
                }
                octets.extend_from_slice(corps.as_bytes());
                let r = resumes.entry(k).or_default();
                r.etiquettes.insert(if f.existait { "réécrit" } else { "créé" });
                r.n += 1;
                details.push(format!(
                    "[{n}] {} {} ({} l., {}{})",
                    f.rel,
                    if f.existait { "réécrit" } else { "créé" },
                    nb_l(&corps),
                    if corps.contains("\r\n") { "CRLF" } else { "LF" },
                    if bom_final { ", BOM" } else { "" }
                ));
                f.creation = Some(octets);
            }
            Op::Ajouter { fichier, texte } => {
                let k = charger!(n, fichier, None);
                let f = &mut fichiers[k];
                if f.creation.is_some() {
                    erreurs.push(format!("[{n}] « {} » est créé dans ce lot : mets tout dans son `cree`", f.rel));
                    continue;
                }
                let mut ajout = String::new();
                if !f.texte.is_empty() && !f.texte.ends_with('\n') {
                    ajout.push_str(f.eol.texte());
                }
                ajout.push_str(&lignes_entieres(texte, f.eol));
                let ligne = f.nb_lignes() + 1;
                let r = resumes.entry(k).or_default();
                r.lignes.insert(ligne);
                r.etiquettes.insert("fin");
                r.n += 1;
                details.push(format!("[{n}] {}:L{} ajouté en fin (+{} l.)", f.rel, ligne, nb_l(texte)));
                let fin = f.texte.len();
                f.plages.push(Plage { debut: fin, fin, texte: ajout, op: i });
            }
            Op::RemplacerSymbole { id, texte } => {
                let c = match res.symbole(id) {
                    Ok(c) => c,
                    Err(m) => {
                        erreurs.push(format!("[{n}] {m}"));
                        continue;
                    }
                };
                let k = charger!(n, &c.fichier, None);
                let f = &mut fichiers[k];
                if f.creation.is_some() {
                    erreurs.push(format!("[{n}] « {} » est créé dans ce lot", f.rel));
                    continue;
                }
                let nl = f.nb_lignes();
                let (a, b) = (c.debut as usize, (c.fin as usize).min(nl));
                if a == 0 || a > nl || b < a {
                    erreurs.push(format!("[{n}] « {id} » : plage L{}-{} hors du fichier ({nl} lignes) : index périmé ?", c.debut, c.fin));
                    continue;
                }
                let (d, e) = f.plage_lignes(a, b);
                let mut t = vers_eol(texte, f.eol);
                if t.ends_with(f.eol.texte()) {
                    t.truncate(t.len() - f.eol.texte().len());
                }
                let ancien = f.texte[d..e].to_string();
                let sig_change = ancien.lines().next().map(str::trim) != t.lines().next().map(str::trim);
                let r = resumes.entry(k).or_default();
                r.lignes.insert(a);
                r.etiquettes.insert("symbole");
                r.n += 1;
                details.push(format!("[{n}] {}:L{}-{} symbole remplacé ({}→{} l.)", f.rel, a, b, b - a + 1, nb_l(&t)));
                if opts.diff {
                    details.push(court_diff(&ancien, &t).trim_end().to_string());
                }
                if sig_change {
                    let (nb, ids) = res.appelants(id);
                    if nb > 0 {
                        avertissements.push(format!(
                            "⚠ {id} : signature modifiée, {nb} appelant(s) à revérifier : {}{}",
                            ids.join(" "),
                            if nb > ids.len() { " …" } else { "" }
                        ));
                    }
                }
                f.plages.push(Plage { debut: d, fin: e, texte: t, op: i });
            }
            Op::Remplacer { portee, ancien, nouveau, tous, lignes } => {
                let Some((k, lo, hi, lib)) =
                    portee_resolue(n, portee, res, &garde, &mut fichiers, &mut index, opts, &mut forces, &mut erreurs)
                else {
                    continue;
                };
                let f = &mut fichiers[k];
                let mut occ = occurrences(f, ancien, lo, hi);
                let mut tolerant = false;
                if occ.is_empty() {
                    occ = occurrences_tolerantes(f, ancien, lo, hi);
                    tolerant = !occ.is_empty();
                }
                if occ.is_empty() {
                    erreurs.push(format!(
                        "[{n}] {} : « {} » introuvable{}{}",
                        f.rel,
                        apercu(ancien),
                        lib,
                        indice_absent(f, ancien, lo, hi)
                    ));
                    continue;
                }
                if occ.len() > 1 && !tous {
                    erreurs.push(format!(
                        "[{n}] {} : « {} » trouvé {} fois ({}) : allonge l'ancre pour l'unicité, ou `tous`",
                        f.rel,
                        apercu(ancien),
                        occ.len(),
                        lignes_de(f, &occ)
                    ));
                    continue;
                }
                let rempl = vers_eol(nouveau, f.eol);
                let ls: Vec<usize> = occ.iter().map(|o| f.ligne_de(o.debut)).collect();
                if !tolerant && nouveau.contains('\n') {
                    let debut_ligne = f.starts[f.ligne_de(occ[0].debut) - 1];
                    let avant = &f.texte[debut_ligne..occ[0].debut];
                    let suite_sans_indent = nouveau.split('\n').skip(1).any(|l| !l.trim().is_empty() && !l.starts_with([' ', '\t']));
                    if !avant.is_empty() && avant.chars().all(|c| c == ' ' || c == '\t') && suite_sans_indent {
                        avertissements.push(format!(
                            "⚠ [{n}] {}:L{} : lignes ajoutées sans indentation alors que l'ancre est indentée (ancre en milieu de ligne ?) : ancre = lignes entières avec leur indentation",
                            f.rel, ls[0]
                        ));
                    }
                }
                let r = resumes.entry(k).or_default();
                r.lignes.extend(ls.iter().copied());
                r.n += occ.len();
                if tolerant {
                    r.etiquettes.insert("indentation recalée");
                }
                details.push(format!(
                    "[{n}] {}:L{}{} remplacé{} ({}→{} l.){}{}",
                    f.rel,
                    ls[0],
                    if ls.len() > 1 { format!("… ×{}", ls.len()) } else { String::new() },
                    if *lignes { " (lignes supprimées)" } else { "" },
                    nb_l(ancien),
                    nb_l(nouveau),
                    lib,
                    if tolerant { " [ancre tolérante : indentation/espaces de fin]" } else { "" }
                ));
                if opts.diff {
                    details.push(court_diff(ancien, nouveau).trim_end().to_string());
                }
                for o in occ {
                    let mut d = o.debut;
                    let mut e = o.fin;
                    if *lignes {
                        let b = f.texte.as_bytes();
                        if b.get(e) == Some(&b'\r') && b.get(e + 1) == Some(&b'\n') {
                            e += 2;
                        } else if b.get(e) == Some(&b'\n') {
                            e += 1;
                        } else if d > 0 && b[d - 1] == b'\n' {
                            // Dernière ligne sans retour à la ligne : on retire celui qui précède.
                            d -= 1;
                            if d > 0 && b[d - 1] == b'\r' {
                                d -= 1;
                            }
                        }
                    }
                    let t = match &o.recale {
                        Some((fi, ag)) => recaler(&rempl, fi, ag),
                        None => rempl.clone(),
                    };
                    f.plages.push(Plage { debut: d, fin: e, texte: t, op: i });
                }
            }
            Op::Inserer { portee, apres, ancre, texte } => {
                let Some((k, lo, hi, lib)) =
                    portee_resolue(n, portee, res, &garde, &mut fichiers, &mut index, opts, &mut forces, &mut erreurs)
                else {
                    continue;
                };
                let f = &mut fichiers[k];
                let mut occ = occurrences(f, ancre, lo, hi);
                if occ.is_empty() {
                    occ = occurrences_tolerantes(f, ancre, lo, hi);
                }
                if occ.is_empty() {
                    erreurs.push(format!(
                        "[{n}] {} : ancre « {} » introuvable{}{}",
                        f.rel,
                        apercu(ancre),
                        lib,
                        indice_absent(f, ancre, lo, hi)
                    ));
                    continue;
                }
                if occ.len() > 1 {
                    erreurs.push(format!(
                        "[{n}] {} : ancre « {} » trouvée {} fois ({}) : allonge-la pour l'unicité",
                        f.rel,
                        apercu(ancre),
                        occ.len(),
                        lignes_de(f, &occ)
                    ));
                    continue;
                }
                let (d, e) = (occ[0].debut, occ[0].fin);
                let (pos, t) = if *apres {
                    let r = if e > d { e - 1 } else { e };
                    match memchr::memchr(b'\n', &f.texte.as_bytes()[r..]) {
                        Some(q) => (r + q + 1, lignes_entieres(texte, f.eol)),
                        None => {
                            // Dernière ligne sans retour à la ligne : le retour s'ajoute avant le texte.
                            let mut t = f.eol.texte().to_string();
                            t.push_str(vers_eol(texte, f.eol).trim_end_matches(['\r', '\n']));
                            (f.texte.len(), t)
                        }
                    }
                } else {
                    (f.starts[f.ligne_de(d) - 1], lignes_entieres(texte, f.eol))
                };
                let r = resumes.entry(k).or_default();
                r.lignes.insert(f.ligne_de(d));
                r.etiquettes.insert(if *apres { "inséré après" } else { "inséré avant" });
                r.n += 1;
                details.push(format!(
                    "[{n}] {}:L{} lignes insérées {} l'ancre (+{} l.){}",
                    f.rel,
                    f.ligne_de(d),
                    if *apres { "après" } else { "avant" },
                    nb_l(texte),
                    lib
                ));
                if opts.diff {
                    details.push(court_diff("", texte).trim_end().to_string());
                }
                f.plages.push(Plage { debut: pos, fin: pos, texte: t, op: i });
            }
            Op::Renommer { de, vers, dans, mots } => {
                if de.is_empty() || vers.is_empty() || de == vers || de.contains('\n') || vers.contains('\n') {
                    erreurs.push(format!("[{n}] renomme : noms vides, identiques ou multi-lignes"));
                    continue;
                }
                let sc = scanner_renommage(&garde, de, vers, dans.as_deref(), *mots);
                if sc.fichiers.is_empty() {
                    erreurs.push(format!(
                        "[{n}] renomme « {de} » : aucune occurrence{} dans {} fichiers parcourus ({} écartés : interdits)",
                        if *mots { " (mot entier)" } else { "" },
                        sc.parcourus,
                        sc.ecartes
                    ));
                    continue;
                }
                let mut total = 0usize;
                let mut nb_f = 0usize;
                let mut ok_n = true;
                for (rel, _) in &sc.fichiers {
                    let k = match charger(&garde, &mut fichiers, &mut index, rel, None, opts, &mut forces) {
                        Ok(k) => k,
                        Err(m) => {
                            erreurs.push(format!("[{n}] {m}"));
                            ok_n = false;
                            continue;
                        }
                    };
                    let f = &mut fichiers[k];
                    if f.creation.is_some() {
                        erreurs.push(format!("[{n}] « {} » est créé dans ce lot : renomme ne s'y applique pas", f.rel));
                        ok_n = false;
                        continue;
                    }
                    let vers_c = vers_eol(vers, f.eol);
                    let occ = occurrences_mot(f.texte.as_bytes(), de.as_bytes(), *mots);
                    if occ.is_empty() {
                        continue;
                    }
                    let r = resumes.entry(k).or_default();
                    r.n += occ.len();
                    r.etiquettes.insert("renommé");
                    nb_f += 1;
                    total += occ.len();
                    for (d, e) in occ {
                        r.lignes.insert(f.ligne_de(d));
                        f.plages.push(Plage { debut: d, fin: e, texte: vers_c.clone(), op: i });
                    }
                }
                if !ok_n {
                    continue;
                }
                if total > MAX_RENOMMAGES && opts.forcer.is_none() {
                    erreurs.push(format!("[{n}] renomme : {total} occurrences (> {MAX_RENOMMAGES}) ; contourner : --forcer \"raison\" (ou restreindre avec `dans`)"));
                    continue;
                }
                let recap = format!(
                    "renomme {de}→{vers} : {total} occurrence(s), {nb_f} fichier(s){}",
                    if sc.ecartes > 0 { format!(" ({} fichier(s) interdit(s) écarté(s))", sc.ecartes) } else { String::new() }
                );
                details.push(format!("[{n}] {recap}"));
                recaps.push(recap);
                if sc.collisions > 0 {
                    avertissements.push(format!(
                        "⚠ renomme {de}→{vers} : « {vers} » existe déjà {} fois dans les fichiers parcourus (collision possible)",
                        sc.collisions
                    ));
                }
            }
        }
    }

    // Recouvrements.
    for f in fichiers.iter_mut() {
        f.plages.sort_by_key(|p| (p.debut, p.fin, p.op));
        let mut max_fin = 0usize;
        let mut op_max = 0usize;
        for p in &f.plages {
            if p.debut < max_fin {
                erreurs.push(format!(
                    "[{}] {} : recouvre l'édition [{}] (les ancres sont cherchées dans le fichier d'ORIGINE : fusionne-les en une seule)",
                    p.op + 1,
                    f.rel,
                    op_max + 1
                ));
            }
            if p.fin > max_fin {
                max_fin = p.fin;
                op_max = p.op;
            }
        }
    }

    if !erreurs.is_empty() {
        return echec(refus(erreurs));
    }

    // Contenus finaux.
    let mut finals: Vec<Option<Vec<u8>>> = Vec::with_capacity(fichiers.len()); // None = inchangé
    let mut total: u64 = 0;
    for f in &fichiers {
        let octets = if let Some(c) = &f.creation {
            c.clone()
        } else {
            let mut out = String::with_capacity(f.texte.len() + 64);
            let mut pos = 0;
            for p in &f.plages {
                out.push_str(&f.texte[pos..p.debut]);
                out.push_str(&p.texte);
                pos = p.fin;
            }
            out.push_str(&f.texte[pos..]);
            let mut o = Vec::with_capacity(out.len() + 3);
            if f.bom {
                o.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
            }
            o.extend_from_slice(out.as_bytes());
            o
        };
        if f.existait && octets == f.octets {
            finals.push(None);
            continue;
        }
        if octets.len() as u64 > garde.max_fichier && opts.forcer.is_none() {
            erreurs.push(format!(
                "{} : résultat de {} Mo (> `taille_max_fichier`) ; contourner : --forcer \"raison\"",
                f.rel,
                octets.len() / 1_048_576
            ));
        }
        total += octets.len() as u64;
        finals.push(Some(octets));
    }
    if total > garde.max_lot && opts.forcer.is_none() {
        erreurs.push(format!("lot de {} Mo écrits (> `taille_max_lot`) ; contourner : --forcer \"raison\"", total / 1_048_576));
    }
    // Syntaxe : un fichier qui passe de « valide » à « erreurs » (ou plus d'erreurs) est refusé.
    if opts.forcer.is_none() {
        for (f, fin) in fichiers.iter().zip(&finals) {
            let Some(fin) = fin else { continue };
            let apres = String::from_utf8_lossy(&fin[if fin.starts_with(&[0xEF, 0xBB, 0xBF]) { 3 } else { 0 }..]).to_string();
            let Some((na, ligne)) = controle_syntaxe(&f.rel, &apres) else { continue };
            let avant = if f.existait { controle_syntaxe(&f.rel, &f.texte).map(|(n, _)| n).unwrap_or(0) } else { 0 };
            if na > avant {
                erreurs.push(format!(
                    "{} : l'édition crée {} erreur(s) de syntaxe (1re vers L{ligne}) ; vérifie accolades/parenthèses/guillemets, ou --forcer \"raison\" si voulu",
                    f.rel,
                    na - avant
                ));
            }
        }
    }
    if !erreurs.is_empty() {
        return echec(refus(erreurs));
    }

    let nb_ecrits = finals.iter().filter(|x| x.is_some()).count();
    let mut pied = String::new();
    if opts.diff {
        for d in &details {
            pied.push_str(d);
            pied.push('\n');
        }
        for (k, f) in fichiers.iter().enumerate() {
            if finals[k].is_none() {
                pied.push_str(&format!("= {} inchangé (résultat identique)\n", f.rel));
            }
        }
    } else {
        let mut lignes_f: Vec<String> = Vec::new();
        for (k, f) in fichiers.iter().enumerate() {
            let Some(r) = resumes.get(&k) else { continue };
            if finals[k].is_none() {
                lignes_f.push(format!("= {} inchangé", f.rel));
                continue;
            }
            let mut ligne = f.rel.clone();
            if !r.lignes.is_empty() {
                ligne.push(' ');
                ligne.push_str(&liste_lignes(&r.lignes));
            }
            if r.n > r.lignes.len().max(1) {
                ligne.push_str(&format!(" ×{}", r.n));
            }
            if !r.etiquettes.is_empty() {
                ligne.push_str(&format!(" ({})", r.etiquettes.iter().copied().collect::<Vec<_>>().join(", ")));
            }
            lignes_f.push(ligne);
        }
        let reste = lignes_f.len().saturating_sub(12);
        for l in lignes_f.iter().take(12) {
            pied.push_str(l);
            pied.push('\n');
        }
        if reste > 0 {
            pied.push_str(&format!("… +{reste} fichier(s)\n"));
        }
        for r in &recaps {
            pied.push_str(r);
            pied.push('\n');
        }
    }
    for a in &avertissements {
        pied.push_str(a);
        pied.push('\n');
    }
    if let Some(r) = &opts.forcer {
        pied.push_str(&format!(
            "garde-fous contournés (« {r} »){}\n",
            if forces.is_empty() { String::new() } else { format!(" : {}", forces.join(" ; ")) }
        ));
    }
    if nb_ecrits == 0 && !opts.essai {
        return Rapport {
            ok: true,
            texte: format!("cortex edit : rien à écrire, tous les résultats sont identiques aux fichiers\n{pied}"),
            id: None,
            ecrits: vec![],
            projet: None,
        };
    }
    if opts.essai {
        let s = format!("cortex edit ESSAI : {} éd., {} fichier(s) seraient écrits, RIEN n'est écrit\n{pied}", ops.len(), nb_ecrits);
        return Rapport { ok: true, texte: s, id: None, ecrits: vec![], projet: None };
    }

    // Vérifie qu'aucun fichier n'a bougé depuis la lecture.
    for f in &fichiers {
        if f.existait {
            let m = std::fs::metadata(&f.abs).ok().and_then(|m| m.modified().ok().map(|t| (m.len(), t)));
            if m != f.meta {
                return echec(format!(
                    "cortex edit : REFUSÉ, aucune écriture : « {} » a été modifié pendant l'opération, relance le lot\n",
                    f.rel
                ));
            }
        } else if f.abs.exists() && f.creation.is_some() {
            return echec(format!(
                "cortex edit : REFUSÉ, aucune écriture : « {} » est apparu pendant l'opération, relance le lot\n",
                f.rel
            ));
        }
    }

    // Journal d'abord (le lot reste annulable même si le processus meurt en cours d'écriture).
    let id = nouvel_id(&garde.racine);
    let dir = opts.journal.join(&id);
    let mut jfich: Vec<JournalFichier> = Vec::new();
    let mut dirs_crees: Vec<String> = Vec::new();
    let sauve = (|| -> std::io::Result<()> {
        std::fs::create_dir_all(dir.join("avant"))?;
        for (k, (f, fin)) in fichiers.iter().zip(&finals).enumerate() {
            let Some(fin) = fin else { continue };
            let sauvegarde = if f.existait {
                let nom = format!("{k}");
                std::fs::write(dir.join("avant").join(&nom), &f.octets)?;
                Some(nom)
            } else {
                None
            };
            for d in &f.dirs_a_creer {
                dirs_crees.push(sans_prefixe_verbatim(d));
            }
            jfich.push(JournalFichier {
                rel: f.rel.clone(),
                existait: f.existait,
                sauvegarde,
                hash_avant: f.existait.then(|| hash_hex(&f.octets)),
                hash_apres: hash_hex(fin),
            });
        }
        Ok(())
    })();
    if let Err(e) = sauve {
        let _ = std::fs::remove_dir_all(&dir);
        return echec(format!("cortex edit : REFUSÉ, journal impossible ({e}) : aucune écriture\n"));
    }
    let mut lot = JournalLot {
        id: id.clone(),
        utc: iso_utc(maintenant()),
        racine: sans_prefixe_verbatim(&garde.racine),
        projet: opts.projet.clone(),
        forcer: opts.forcer.clone(),
        etat: "en cours".into(),
        fichiers: jfich,
        dirs_crees,
    };
    if let Err(e) = ecrire_json(&dir.join("lot.json"), &lot) {
        let _ = std::fs::remove_dir_all(&dir);
        return echec(format!("cortex edit : REFUSÉ, journal impossible ({e}) : aucune écriture\n"));
    }

    // Écritures, avec retour arrière si l'une échoue.
    let mut faits: Vec<usize> = Vec::new();
    let mut ecrits: Vec<String> = Vec::new();
    for (k, (f, fin)) in fichiers.iter().zip(&finals).enumerate() {
        let Some(fin) = fin else { continue };
        if let Err(e) = ecrire_atomique(&f.abs, fin) {
            let mut a_remettre = faits.clone();
            a_remettre.push(k);
            for &j in a_remettre.iter().rev() {
                let g = &fichiers[j];
                if g.existait {
                    let _ = ecrire_atomique(&g.abs, &g.octets);
                } else {
                    let _ = std::fs::remove_file(&g.abs);
                }
            }
            for g in &fichiers {
                for d in &g.dirs_a_creer {
                    let _ = std::fs::remove_dir(d);
                }
            }
            let _ = std::fs::remove_dir_all(&dir);
            return echec(format!(
                "cortex edit : ÉCHEC d'écriture sur « {} » ({e}) : {} fichier(s) déjà écrits ont été remis dans leur état d'avant, rien n'a changé\n",
                f.rel,
                faits.len()
            ));
        }
        faits.push(k);
        ecrits.push(f.rel.clone());
    }
    lot.etat = "appliqué".into();
    let _ = ecrire_json(&dir.join("lot.json"), &lot);
    purger_journal(&opts.journal);

    let s = format!("cortex edit {id} : {} éd., {} fichier(s)\n{pied}annuler : cortex edit --annuler {id}\n", ops.len(), nb_ecrits);
    Rapport { ok: true, texte: s, id: Some(id), ecrits, projet: opts.projet.clone() }
}

fn refus(erreurs: Vec<String>) -> String {
    let mut vus = BTreeSet::new();
    let erreurs: Vec<String> = erreurs.into_iter().filter(|e| vus.insert(e.clone())).collect();
    let mut s = format!("cortex edit : REFUSÉ, aucune écriture ({} erreur{})\n", erreurs.len(), if erreurs.len() > 1 { "s" } else { "" });
    for e in &erreurs {
        s.push_str(e);
        s.push('\n');
    }
    s
}

/// (erreurs, ligne de la première) d'un contenu texte, selon l'extension ; `None` : pas de contrôle.
fn controle_syntaxe(rel: &str, contenu: &str) -> Option<(usize, usize)> {
    let bas = rel.to_ascii_lowercase();
    if bas.ends_with(".json") && !bas.contains("tsconfig") && !bas.contains("jsconfig") && !bas.contains(".vscode/") {
        return Some(match serde_json::from_str::<serde::de::IgnoredAny>(contenu) {
            Ok(_) => (0, 0),
            Err(e) => (1, e.line()),
        });
    }
    let lang = crate::lang::Lang::from_path(rel);
    crate::extract::erreurs_syntaxe(lang, contenu)
}

// ─── Renommage à l'échelle du projet ─────────────────────────────────────────

struct Scan {
    fichiers: Vec<(String, PathBuf)>,
    parcourus: usize,
    collisions: usize,
    ecartes: usize,
}

fn scanner_renommage(garde: &Garde, de: &str, vers: &str, dans: Option<&str>, mots: bool) -> Scan {
    let prefixe = dans.map(|d| normaliser_sep(d).trim_matches('/').to_lowercase()).filter(|d| !d.is_empty());
    let mut chemins: Vec<(String, PathBuf)> = Vec::new();
    let marcheur = ignore::WalkBuilder::new(&garde.racine).filter_entry(|e| e.file_name() != "node_modules").build();
    for e in marcheur.flatten() {
        if !e.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let abs = e.path().to_path_buf();
        let Ok(r) = abs.strip_prefix(&garde.racine) else { continue };
        let rel = normaliser_sep(&r.to_string_lossy());
        if let Some(p) = &prefixe {
            let l = rel.to_lowercase();
            if !(l == *p || l.starts_with(&format!("{p}/")) || (!p.contains('/') && l.starts_with(p.as_str()))) {
                continue;
            }
        }
        chemins.push((rel, abs));
    }
    let parcourus = chemins.len();
    let max = garde.max_fichier;
    let trouves: Vec<(String, PathBuf, usize, usize)> = chemins
        .into_par_iter()
        .filter_map(|(rel, abs)| {
            let m = std::fs::metadata(&abs).ok()?;
            if m.len() == 0 || m.len() > max {
                return None;
            }
            let b = std::fs::read(&abs).ok()?;
            if memchr::memchr(0, &b).is_some() {
                return None;
            }
            let n = occurrences_mot(&b, de.as_bytes(), mots).len();
            let c = occurrences_mot(&b, vers.as_bytes(), mots).len();
            if n == 0 && c == 0 {
                return None;
            }
            Some((rel, abs, n, c))
        })
        .collect();
    let collisions = trouves.iter().map(|t| t.3).sum();
    let mut fichiers: Vec<(String, PathBuf)> = trouves.into_iter().filter(|t| t.2 > 0).map(|t| (t.0, t.1)).collect();
    fichiers.sort();
    // Les chemins interdits (secrets, liste du projet) sont écartés en silence, comptés.
    let avant = fichiers.len();
    fichiers.retain(|(rel, _)| garde.verifier(rel, false).is_ok());
    let ecartes = avant - fichiers.len();
    Scan { fichiers, parcourus, collisions, ecartes }
}

/// Résout la portée d'une édition → (index du fichier, début, fin d'octets de la zone, libellé).
#[allow(clippy::too_many_arguments)]
fn portee_resolue(
    n: usize,
    portee: &Portee,
    res: &dyn Resolveur,
    garde: &Garde,
    fichiers: &mut Vec<Fich>,
    index: &mut BTreeMap<String, usize>,
    opts: &Options,
    forces: &mut Vec<String>,
    erreurs: &mut Vec<String>,
) -> Option<(usize, usize, usize, String)> {
    match portee {
        Portee::Fichier(p) => match charger(garde, fichiers, index, p, None, opts, forces) {
            Ok(k) => {
                if fichiers[k].creation.is_some() {
                    erreurs.push(format!("[{n}] « {} » est créé dans ce lot : mets tout dans son `cree`", fichiers[k].rel));
                    return None;
                }
                let l = fichiers[k].texte.len();
                Some((k, 0, l, String::new()))
            }
            Err(m) => {
                erreurs.push(format!("[{n}] {m}"));
                None
            }
        },
        Portee::Symbole(id) => {
            let c = match res.symbole(id) {
                Ok(c) => c,
                Err(m) => {
                    erreurs.push(format!("[{n}] {m}"));
                    return None;
                }
            };
            let k = match charger(garde, fichiers, index, &c.fichier, None, opts, forces) {
                Ok(k) => k,
                Err(m) => {
                    erreurs.push(format!("[{n}] {m}"));
                    return None;
                }
            };
            let f = &fichiers[k];
            let nl = f.nb_lignes();
            let (a, b) = (c.debut as usize, (c.fin as usize).min(nl));
            if a == 0 || a > nl || b < a {
                erreurs.push(format!("[{n}] « {id} » : plage L{}-{} hors du fichier ({nl} lignes) : index périmé ?", c.debut, c.fin));
                return None;
            }
            let (d, e) = f.plage_lignes(a, b);
            let nom = id.rsplit('#').next().unwrap_or(id);
            Some((k, d, e, format!(" dans {nom}")))
        }
    }
}

/// Charge (une fois) le fichier désigné ; `creation` : `Some(ecrase)` pour un `cree`.
fn charger(
    garde: &Garde,
    fichiers: &mut Vec<Fich>,
    index: &mut BTreeMap<String, usize>,
    brut: &str,
    creation: Option<bool>,
    opts: &Options,
    forces: &mut Vec<String>,
) -> Result<usize, String> {
    let (rel, abs, vise) = match garde.verifier(brut, opts.forcer.is_some()) {
        Ok(x) => x,
        Err(Refus::Dur(m)) => return Err(m),
        Err(Refus::Souple(m)) => return Err(format!("{m} ; contourner : --forcer \"raison\" (journalisé)")),
    };
    if vise {
        forces.push(rel.clone());
    }
    let cle = if cfg!(windows) { rel.to_lowercase() } else { rel.clone() };
    if let Some(&k) = index.get(&cle) {
        if creation.is_some() {
            return Err(format!("« {rel} » est déjà touché par une autre édition du lot"));
        }
        return Ok(k);
    }
    let f = match creation {
        None => {
            if !abs.exists() {
                return Err(format!("« {rel} » introuvable (pour créer un fichier : `cree` / `@ chemin new`)"));
            }
            lire_fichier(garde, &rel, &abs, opts)?
        }
        Some(ecrase) => {
            if abs.exists() {
                if !ecrase {
                    return Err(format!("« {rel} » existe déjà (pour le réécrire : `ecrase` / `@ chemin new overwrite`)"));
                }
                lire_fichier(garde, &rel, &abs, opts)?
            } else {
                fichier_neuf(&rel, &abs, &garde.racine)
            }
        }
    };
    fichiers.push(f);
    index.insert(cle, fichiers.len() - 1);
    Ok(fichiers.len() - 1)
}

// ─── Écriture ────────────────────────────────────────────────────────────────

fn ecrire_atomique(abs: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(p) = abs.parent() {
        std::fs::create_dir_all(p)?;
    }
    let lien = std::fs::symlink_metadata(abs).map(|m| m.file_type().is_symlink()).unwrap_or(false);
    if lien {
        return std::fs::write(abs, data);
    }
    let nano = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    let tmp = abs.with_file_name(format!(".cortex-edit-{}-{nano}.tmp", std::process::id()));
    std::fs::write(&tmp, data)?;
    if let Ok(m) = std::fs::metadata(abs) {
        let _ = std::fs::set_permissions(&tmp, m.permissions());
    }
    match std::fs::rename(&tmp, abs) {
        Ok(()) => Ok(()),
        Err(_) => {
            // Fichier tenu ouvert par un autre programme : écriture directe, le retour arrière reste possible.
            let _ = std::fs::remove_file(&tmp);
            std::fs::write(abs, data)
        }
    }
}

// ─── Journal et annulation ───────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Debug)]
struct JournalFichier {
    rel: String,
    existait: bool,
    sauvegarde: Option<String>,
    hash_avant: Option<String>,
    hash_apres: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct JournalLot {
    id: String,
    utc: String,
    racine: String,
    projet: Option<String>,
    forcer: Option<String>,
    etat: String,
    fichiers: Vec<JournalFichier>,
    dirs_crees: Vec<String>,
}

fn ecrire_json<T: Serialize>(p: &Path, v: &T) -> std::io::Result<()> {
    let s = serde_json::to_string_pretty(v).map_err(std::io::Error::other)?;
    std::fs::write(p, s)
}

fn maintenant() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// `AAAAMMJJ-HHMMSS` (UTC) depuis des secondes Unix.
pub fn iso_utc(secs: u64) -> String {
    let (j, r) = ((secs / 86400) as i64, secs % 86400);
    let z = j + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", r / 3600, (r % 3600) / 60, r % 60)
}

fn nouvel_id(racine: &Path) -> String {
    let nano = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let h = blake3::hash(format!("{}{nano}{}", racine.display(), std::process::id()).as_bytes()).to_hex().to_string();
    format!("{}-{}", iso_utc(maintenant()), &h[..4])
}

fn purger_journal(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut ids: Vec<String> = rd.flatten().filter(|e| e.path().is_dir()).map(|e| e.file_name().to_string_lossy().to_string()).collect();
    ids.sort();
    if ids.len() > JOURNAL_GARDE {
        for id in &ids[..ids.len() - JOURNAL_GARDE] {
            let _ = std::fs::remove_dir_all(dir.join(id));
        }
    }
}

fn charger_lot(journal: &Path, id: &str) -> Result<(PathBuf, JournalLot), String> {
    let id = if id == "dernier" || id == "last" {
        let rd = std::fs::read_dir(journal).map_err(|_| "journal vide : aucun lot à annuler".to_string())?;
        let mut ids: Vec<String> =
            rd.flatten().filter(|e| e.path().join("lot.json").exists()).map(|e| e.file_name().to_string_lossy().to_string()).collect();
        ids.sort();
        let mut trouve = None;
        for i in ids.iter().rev() {
            if let Ok((_, l)) = charger_lot_brut(journal, i) {
                if l.etat == "appliqué" {
                    trouve = Some(i.clone());
                    break;
                }
            }
        }
        trouve.ok_or_else(|| "aucun lot à annuler".to_string())?
    } else {
        id.to_string()
    };
    if id.contains(['/', '\\', '.']) {
        return Err(format!("identifiant « {id} » invalide"));
    }
    charger_lot_brut(journal, &id)
}

fn charger_lot_brut(journal: &Path, id: &str) -> Result<(PathBuf, JournalLot), String> {
    let dir = journal.join(id);
    let txt = std::fs::read_to_string(dir.join("lot.json")).map_err(|_| format!("lot « {id} » inconnu (cortex edit --liste)"))?;
    let lot: JournalLot = serde_json::from_str(&txt).map_err(|e| format!("journal du lot « {id} » illisible : {e}"))?;
    Ok((dir, lot))
}

/// Annule un lot appliqué : remet le contenu d'avant (supprime les fichiers créés). Refus si un
/// fichier a changé depuis le lot (sauf `forcer`).
pub fn annuler(id: &str, opts: &Options) -> Rapport {
    let (dir, mut lot) = match charger_lot(&opts.journal, id) {
        Ok(x) => x,
        Err(e) => return echec(format!("cortex edit --annuler : {e}\n")),
    };
    if lot.etat == "annulé" {
        return echec(format!("cortex edit --annuler : le lot {} est déjà annulé\n", lot.id));
    }
    let garde = match Garde::nouveau(Path::new(&lot.racine), opts.config_globale.as_deref()) {
        Ok(g) => g,
        Err(e) => return echec(format!("cortex edit --annuler : {e}\n")),
    };
    let mut erreurs = Vec::new();
    let mut plan: Vec<(PathBuf, Option<Vec<u8>>, String)> = Vec::new();
    for jf in &lot.fichiers {
        let abs = match garde.verifier(&jf.rel, true) {
            Ok((_, a, _)) => a,
            Err(Refus::Dur(m)) | Err(Refus::Souple(m)) => {
                erreurs.push(m);
                continue;
            }
        };
        let actuel = std::fs::read(&abs).ok();
        let conforme = match &actuel {
            Some(b) => hash_hex(b) == jf.hash_apres,
            None => !jf.existait, // créé puis déjà supprimé : rien à défaire
        };
        if !conforme && opts.forcer.is_none() {
            erreurs.push(format!(
                "« {} » a changé depuis le lot (ou a été supprimé) : l'annuler écraserait ce travail ; contourner : --forcer \"raison\"",
                jf.rel
            ));
            continue;
        }
        let avant = match &jf.sauvegarde {
            Some(nom) => match std::fs::read(dir.join("avant").join(nom)) {
                Ok(b) => Some(b),
                Err(e) => {
                    erreurs.push(format!("« {} » : sauvegarde illisible ({e})", jf.rel));
                    continue;
                }
            },
            None => None,
        };
        plan.push((abs, avant, jf.rel.clone()));
    }
    if !erreurs.is_empty() {
        let mut s = format!("cortex edit --annuler {} : REFUSÉ, rien n'a été touché\n", lot.id);
        for e in erreurs {
            s.push_str(&format!("{e}\n"));
        }
        return echec(s);
    }
    let mut s = format!("cortex edit --annuler {} : {} fichier(s) remis dans l'état d'avant le lot\n", lot.id, plan.len());
    let mut ecrits = Vec::new();
    for (abs, avant, rel) in &plan {
        let r = match avant {
            Some(b) => ecrire_atomique(abs, b),
            None => match std::fs::remove_file(abs) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            },
        };
        match r {
            Ok(()) => {
                s.push_str(&format!("{} {}\n", if avant.is_some() { "restauré" } else { "supprimé (créé par le lot)" }, rel));
                ecrits.push(rel.clone());
            }
            Err(e) => s.push_str(&format!("ÉCHEC {rel} : {e}\n")),
        }
    }
    for d in lot.dirs_crees.iter().rev() {
        let _ = std::fs::remove_dir(d);
    }
    lot.etat = "annulé".into();
    let _ = ecrire_json(&dir.join("lot.json"), &lot);
    Rapport { ok: true, texte: s, id: Some(lot.id.clone()), ecrits, projet: lot.projet.clone() }
}

/// Les derniers lots du journal.
pub fn liste(opts: &Options) -> String {
    let Ok(rd) = std::fs::read_dir(&opts.journal) else { return "(aucun lot dans le journal)\n".into() };
    let mut ids: Vec<String> =
        rd.flatten().filter(|e| e.path().join("lot.json").exists()).map(|e| e.file_name().to_string_lossy().to_string()).collect();
    ids.sort();
    ids.reverse();
    let mut s = String::new();
    for id in ids.iter().take(15) {
        if let Ok((_, l)) = charger_lot_brut(&opts.journal, id) {
            let noms: Vec<&str> = l.fichiers.iter().take(3).map(|f| f.rel.as_str()).collect();
            s.push_str(&format!(
                "{}  {}  {} fichier(s)  {}{}\n",
                l.id,
                l.etat,
                l.fichiers.len(),
                noms.join(", "),
                if l.fichiers.len() > 3 { ", …" } else { "" }
            ));
        }
    }
    if s.is_empty() {
        s.push_str("(aucun lot dans le journal)\n");
    }
    s
}

// ─── Branchement sur l'atlas ────────────────────────────────────────────────

/// Racine du projet + atlas ouverts (et à jour) pour résoudre les identifiants de symboles.
pub struct Contexte {
    pub racine: PathBuf,
    pub projet: Option<String>,
    handles: Vec<Handle>,
}

fn norm_cmp(p: &str) -> String {
    sans_prefixe_verbatim(Path::new(p)).to_lowercase().trim_end_matches('/').to_string()
}

impl Contexte {
    /// `project` (nom indexé) > `racine` > projet indexé qui contient un chemin absolu du lot,
    /// sinon le dossier courant > dossier courant (sans atlas).
    pub fn ouvrir(project: Option<&str>, racine: Option<&Path>, indices: &[String]) -> Result<Contexte, String> {
        let nom: Option<String> = match (project, racine) {
            (Some(p), _) => Some(p.to_string()),
            (None, Some(_)) => None,
            (None, None) => {
                let abs: Vec<String> = indices.iter().filter(|p| est_absolu(&normaliser_sep(p))).cloned().collect();
                let cwd: Vec<String> = std::env::current_dir().map(|c| vec![c.to_string_lossy().to_string()]).unwrap_or_default();
                trouver_projet(&abs).or_else(|| trouver_projet(&cwd))
            }
        };
        let mut handles = Vec::new();
        let racine_p: PathBuf = match (&nom, racine) {
            (Some(n), _) => {
                let r = crate::atlas::project_root(n).ok_or_else(|| format!("projet « {n} » inconnu (cortex list)"))?;
                if let Ok(mut h) = crate::atlas::ensure_and_open(n) {
                    crate::atlas::fresh::refresh(&mut h);
                    handles.push(h);
                }
                PathBuf::from(r)
            }
            (None, Some(r)) => r.to_path_buf(),
            (None, None) => std::env::current_dir().map_err(|e| format!("dossier courant illisible : {e}"))?,
        };
        Ok(Contexte { racine: racine_p, projet: nom, handles })
    }

    /// Met l'index à jour pour les fichiers qui viennent d'être écrits.
    pub fn rafraichir(&mut self) {
        for h in self.handles.iter_mut() {
            crate::atlas::fresh::refresh(h);
        }
    }
}

/// Projet indexé dont la racine contient l'un des chemins (la racine la plus profonde l'emporte).
fn trouver_projet(chemins: &[String]) -> Option<String> {
    if chemins.is_empty() {
        return None;
    }
    let home = crate::index::cortex_home();
    let cfg = crate::config::Config::load();
    let mut meilleur: Option<(usize, String)> = None;
    for e in std::fs::read_dir(home).ok()?.flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        if !crate::atlas::is_project_dir(&e.path()) || !cfg.is_enabled(&n) {
            continue;
        }
        let Some(r) = crate::atlas::project_root(&n) else { continue };
        let r = norm_cmp(&r);
        for c in chemins {
            let c = norm_cmp(c);
            if (c == r || c.starts_with(&format!("{r}/"))) && meilleur.as_ref().is_none_or(|(l, _)| r.len() > *l) {
                meilleur = Some((r.len(), n.clone()));
            }
        }
    }
    meilleur.map(|(_, n)| n)
}

impl Contexte {
    fn cible(&self, id: &str) -> Result<(&Handle, crate::outils::Cible), String> {
        use crate::outils::{self, Resolu};
        match outils::resoudre(&self.handles, id) {
            Resolu::Trouve { h, cible, homonymes } => {
                if !homonymes.is_empty() && !id.trim_start().starts_with("S:") && !id.contains('#') {
                    let mut v: Vec<String> = vec![h_id(h, &cible)];
                    v.extend(homonymes.iter().take(5).map(|&g| h.node_id(g)));
                    return Err(format!("« {id} » est ambigu ({} symboles) : recopie un identifiant exact : {}", v.len(), v.join(" | ")));
                }
                Ok((h, cible))
            }
            r => Err(outils::echec(r, id).lines().next().unwrap_or("symbole introuvable").to_string()),
        }
    }
}

impl Resolveur for Contexte {
    fn symbole(&self, id: &str) -> Result<CibleSymbole, String> {
        use crate::outils::Cible;
        if self.handles.is_empty() {
            return SansAtlas.symbole(id);
        }
        let (h, cible) = self.cible(id)?;
        match cible {
            Cible::Lignes { fichier, debut, fin } => Ok(CibleSymbole { fichier: h.path_of(fichier).to_string(), debut, fin }),
            Cible::Noeud(g) => match h.node(g) {
                Some(r) if r.n.kind == 1 => {
                    let mut fin = r.n.end_line.max(r.n.line);
                    if r.kind() == crate::symbol::SymbolKind::Heading {
                        fin = crate::outils::lecture::fin_section(h, r.n.owner_file, g, u32::MAX);
                    }
                    Ok(CibleSymbole { fichier: h.path_of(r.n.owner_file).to_string(), debut: r.n.line, fin })
                }
                _ => Err(format!("« {id} » désigne un fichier, pas un symbole : utilise `fichier` (ou `S:…#nom`)")),
            },
        }
    }

    fn appelants(&self, id: &str) -> (usize, Vec<String>) {
        let Ok((h, crate::outils::Cible::Noeud(g))) = self.cible(id) else { return (0, Vec::new()) };
        let v = crate::outils::appelants(h, g);
        let ids = v.iter().take(4).map(|(c, _)| h.node_id(*c)).collect();
        (v.len(), ids)
    }
}

fn h_id(h: &Handle, c: &crate::outils::Cible) -> String {
    match c {
        crate::outils::Cible::Noeud(g) => h.node_id(*g),
        crate::outils::Cible::Lignes { fichier, .. } => h.path_of(*fichier).to_string(),
    }
}

// ─── Points d'entrée CLI / MCP ───────────────────────────────────────────────

/// Chemins cités par le lot (pour deviner le projet).
fn chemins_du_lot(ops: &[Op]) -> Vec<String> {
    let mut v = Vec::new();
    for o in ops {
        match o {
            Op::Remplacer { portee: Portee::Fichier(f), .. } | Op::Inserer { portee: Portee::Fichier(f), .. } => v.push(f.clone()),
            Op::Ajouter { fichier, .. } | Op::Creer { fichier, .. } => v.push(fichier.clone()),
            _ => {}
        }
    }
    v
}

/// Lot déjà analysé → rapport. `project` / `racine` : voir `Contexte::ouvrir`.
pub fn executer_ops(ops: &[Op], project: Option<&str>, racine: Option<&Path>, mut opts: Options) -> Rapport {
    if opts.forcer.as_deref().is_some_and(|r| r.trim().is_empty()) {
        return echec("cortex edit : --forcer demande une raison (ex. --forcer \"migration générée\")\n".into());
    }
    let mut ctx = match Contexte::ouvrir(project, racine, &chemins_du_lot(ops)) {
        Ok(c) => c,
        Err(e) => return echec(format!("cortex edit : {e}\n")),
    };
    opts.projet = ctx.projet.clone();
    let r = appliquer(&ctx.racine.clone(), ops, &opts, &ctx);
    if r.ok && !opts.essai && !r.ecrits.is_empty() {
        ctx.rafraichir();
    }
    r
}

/// Texte (JSON ou format texte) → rapport.
pub fn executer_texte(entree: &str, project: Option<&str>, racine: Option<&Path>, opts: Options) -> Rapport {
    match parser(entree) {
        Ok(ops) => executer_ops(&ops, project, racine, opts),
        Err(e) => echec(format!("cortex edit : lot invalide, aucune écriture\n{e}\n")),
    }
}

pub fn executer_annulation(id: &str, opts: Options) -> Rapport {
    let r = annuler(id, &opts);
    if let (true, Some(p)) = (r.ok && !r.ecrits.is_empty(), r.projet.as_deref()) {
        if let Ok(mut h) = crate::atlas::ensure_and_open(p) {
            crate::atlas::fresh::refresh(&mut h);
        }
    }
    r
}

/// Appel de l'outil MCP `cortex_edit`.
pub fn appel_mcp(args: &Value) -> String {
    let mut opts = Options::par_defaut();
    opts.essai = args.get("dry_run").and_then(|v| v.as_bool()).unwrap_or(false);
    opts.diff = args.get("diff").and_then(|v| v.as_bool()).unwrap_or(false);
    opts.forcer = args.get("force").and_then(|v| v.as_str()).map(String::from);
    if let Some(id) = args.get("undo").and_then(|v| v.as_str()) {
        return executer_annulation(id, opts).texte;
    }
    let project = args.get("project").and_then(|v| v.as_str());
    let ops = if let Some(t) = args.get("text").and_then(|v| v.as_str()) {
        parser(t)
    } else if let Some(e) = args.get("edits") {
        parser_valeur(e)
    } else {
        return "cortex edit : `edits` (liste d'éditions), `text` ou `undo` (id du lot) requis\n".into();
    };
    match ops {
        Ok(o) => executer_ops(&o, project, None, opts).texte,
        Err(e) => format!("cortex edit : lot invalide, aucune écriture\n{e}\n"),
    }
}

#[cfg(test)]
#[path = "edit_tests.rs"]
mod tests;
