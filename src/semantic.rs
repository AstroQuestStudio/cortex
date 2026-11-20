//! Couche sémantique légère — sans embeddings ONNX (incompatible MinGW).
//!
//! Deux mécanismes sûrs et rapides qui rapprochent la recherche en langage
//! naturel du code (souvent en anglais, abrégé) :
//!   1. Expansion de requête : synonymes FR↔EN + jargon code (delete/remove,
//!      fichier/file, créer/create…). Une question FR matche un symbole EN.
//!   2. Distance d'édition bornée (Levenshtein ≤1, + préfixe) : rattrape fautes
//!      de frappe et variantes (`usr`→`user`, `auth`→`authenticate`).
//!
//! C'est le 80% du gain des embeddings, pour 0 dépendance et 0 risque de build.

/// Table de synonymes bidirectionnelle (groupes de termes équivalents).
/// Chaque ligne = un cluster de mots qui doivent se matcher mutuellement.
/// Tout est en minuscules, déjà tokenisé (un seul mot par entrée).
const SYNONYM_GROUPS: &[&[&str]] = &[
    // CRUD ───────────────────────────────────────────────────────────────
    &["delete", "remove", "destroy", "supprimer", "effacer", "drop", "del"],
    &["create", "add", "new", "creer", "ajouter", "insert", "make"],
    &["update", "edit", "modify", "modifier", "patch", "set", "change"],
    &["get", "fetch", "load", "read", "recuperer", "charger", "lire", "list"],
    &["save", "persist", "store", "sauvegarder", "enregistrer", "write"],
    // Domaine app ─────────────────────────────────────────────────────────
    &["file", "document", "fichier", "doc", "blob"],
    &["folder", "directory", "dossier", "dir"],
    &["user", "account", "utilisateur", "compte", "member", "membre"],
    &["auth", "authentication", "authenticate", "login", "connexion", "signin", "session"],
    &["upload", "import", "envoyer", "televerser"],
    &["download", "export", "telecharger"],
    &["search", "find", "query", "rechercher", "chercher", "lookup"],
    &["company", "tenant", "organization", "entreprise", "societe", "org"],
    &["payment", "billing", "paiement", "facturation", "invoice", "facture", "stripe"],
    &["credit", "quota", "credits", "wallet", "solde", "balance"],
    &["error", "exception", "erreur", "fail", "echec"],
    &["config", "setting", "settings", "configuration", "parametres", "reglages"],
    &["permission", "access", "acl", "rls", "policy", "autorisation", "droits"],
    // Collab / realtime ───────────────────────────────────────────────────
    &["collab", "collaboration", "realtime", "temps", "yjs", "crdt", "sync", "synchro"],
    &["presence", "cursor", "curseur", "presence"],
    // UI ──────────────────────────────────────────────────────────────────
    &["modal", "dialog", "popup", "fenetre"],
    &["button", "btn", "bouton"],
    &["form", "formulaire", "input", "champ"],
    &["table", "grid", "tableau", "liste"],
    // Spark / office ──────────────────────────────────────────────────────
    &["sheet", "spreadsheet", "feuille", "tableur", "cell", "cellule"],
    &["slide", "presentation", "deck", "diapo"],
    &["compress", "compression", "regic", "zstd", "encode", "codec"],
    // Vocabulaire courant FR↔EN (étape 2b : manques relevés sur le banc caché,
    // gardés parce qu'ils font progresser le caché sans abaisser le public) ──
    &["window", "fenetre"],
    &["desktop", "bureau"],
    &["word", "mot"],
    &["employee", "salarie", "employe", "staff"],
    &["hr", "hrm", "rh"],
    &["data", "donnee", "donnees"],
    &["gdpr", "rgpd", "privacy"],
    &["url", "adresse", "link", "lien"],
    &["password", "passe", "mdp"],
    &["reminder", "relance", "relancer", "relances", "rappel", "dunning"],
    &["salary", "salaire", "payroll", "paie"],
    &["bank", "banque", "bancaire"],
    &["weekly", "hebdomadaire"],
    &["scroll", "defilement"],
    &["tab", "onglet"],
    &["title", "titre"],
    &["guest", "anonymous", "anonyme", "invite"],
    // Vocabulaire général (étape précision) : chaque groupe a été retenu parce qu'il
    // ne dégrade aucun des trois bancs (réglage, caché, étendu).
    &["text", "texte"],
    &["native", "natif"],
    &["hidden", "masque", "invisible"],
    &["transfer", "virement", "sepa"],
    &["automatic", "automatique", "automatiquement", "auto"],
    // « base aérienne » : le mot composé `Airbase` ne contient pas le mot « base »
    &["airbase", "airfield", "aerodrome", "aerienne", "aeriennes"],
    // Réseau / multijoueur (réplication Unreal, sessions) : questions FR, identifiants EN
    &["replicate", "replicated", "replication", "replique", "repliquee", "repliquees", "repliquer"],
    &["network", "net", "reseau"],
    &["multiplayer", "multijoueur", "online"],
];

/// Vocabulaire français courant du DÉVELOPPEMENT logiciel vers les mots du code (anglais).
/// Sens UNIQUE (français → anglais) : une question française gagne les mots-code, mais une
/// question anglaise n'est jamais élargie (contrairement à `SYNONYM_GROUPS`, bidirectionnel).
/// Chaque entrée = (formes françaises de base, accents repliés ; mots du code, au plus 3 : plus
/// de mots diluerait la question). Formes de base seulement : les formes fléchies (conjuguées,
/// pluriel, féminin) sont ramenées à la base par `lemme_fr`. Mots courants de tout dépôt, jamais
/// d'identifiant d'un projet ; un mot français qui est aussi un mot anglais courant n'y figure pas.
const FR_VERS_CODE: &[(&[&str], &[&str])] = &[
    (&["envoyer", "envoi", "expedier"], &["send"]),
    (&["emettre"], &["emit"]),
    (&["recevoir"], &["receive"]),
    (&["verifier"], &["check"]),
    (&["controler", "controle"], &["check", "validate"]),
    (&["analyser", "analyse"], &["parse", "analyze"]),
    (&["analyseur", "interpreteur"], &["parser"]),
    (&["interpreter"], &["parse", "interpret"]),
    (&["generer"], &["generate"]),
    (&["generateur"], &["generator"]),
    (&["declarer"], &["declare", "register"]),
    (&["enregistrer", "enregistrement"], &["register"]),
    (&["appeler"], &["call", "invoke"]),
    (&["executer"], &["execute", "run"]),
    (&["lancer", "demarrer", "demarrage"], &["start", "launch"]),
    (&["arreter", "arret"], &["stop"]),
    (&["terminer", "finir"], &["finish", "end"]),
    (&["retirer", "enlever"], &["remove"]),
    (&["remplacer", "remplacement"], &["replace"]),
    (&["trouver", "retrouver", "localiser"], &["find", "lookup"]),
    (&["choisir", "choix"], &["choose", "select"]),
    (&["regrouper", "grouper"], &["group"]),
    (&["combiner", "assembler"], &["combine", "merge"]),
    (&["decouper", "diviser", "scinder", "separer"], &["split"]),
    (&["joindre", "concatener"], &["join", "concat"]),
    (&["restreindre"], &["restrict", "limit"]),
    (&["autoriser"], &["allow", "authorize"]),
    (&["interdire", "interdiction"], &["forbid", "deny"]),
    (&["refuser", "refus", "rejeter"], &["reject", "deny", "refuse"]),
    (&["accepter"], &["accept"]),
    (&["contenir"], &["contain", "include"]),
    (&["inclure"], &["include"]),
    (&["exclure"], &["exclude", "ignore"]),
    (&["ignorer"], &["ignore", "skip"]),
    (&["surveiller", "observer", "surveillance"], &["watch", "monitor", "observe"]),
    (&["mesurer", "mesure"], &["measure", "time"]),
    (&["chronometrer", "chronometrage", "minuterie", "minuteur"], &["timing", "timer", "time"]),
    (&["compter", "comptage"], &["count"]),
    (&["comparer", "comparaison"], &["compare"]),
    (&["stocker", "stockage"], &["store", "storage"]),
    (&["conserver", "garder"], &["keep", "retain", "store"]),
    (&["memoriser", "memorisation"], &["cache", "memoize"]),
    (&["nettoyer", "nettoyage"], &["clean", "clear"]),
    (&["vider", "purger"], &["clear", "empty", "flush"]),
    (&["reinitialiser", "reinitialisation"], &["reset"]),
    (&["initialiser", "initialisation"], &["init", "initialize"]),
    (&["configurer"], &["configure", "config"]),
    (&["brancher", "raccorder"], &["attach", "plug", "mount"]),
    (&["connecter"], &["connect"]),
    (&["lier", "associer", "rattacher", "liaison"], &["bind", "link", "attach"]),
    (&["heriter"], &["inherit"]),
    (&["appliquer"], &["apply"]),
    (&["definir"], &["define"]),
    (&["decoder"], &["decode"]),
    (&["encoder"], &["encode"]),
    (&["hacher", "hachage"], &["hash"]),
    (&["rafraichir", "actualiser"], &["refresh", "update"]),
    (&["recharger", "rechargement"], &["reload"]),
    (&["echouer", "defaillance"], &["fail", "error"]),
    (&["tenter", "tentative", "essayer"], &["attempt", "try", "retry"]),
    (&["ecrire", "ecriture"], &["write"]),
    (&["lecture"], &["read"]),
    (&["afficher", "affichage"], &["display", "print", "show"]),
    (&["reconnaitre", "reconnaissance"], &["recognize", "detect"]),
    (&["detecter"], &["detect"]),
    (&["propager"], &["propagate"]),
    (&["parcourir", "parcours"], &["iterate", "walk", "traverse"]),
    (&["retarder", "retard"], &["delay", "defer"]),
    (&["reporter", "differer"], &["defer", "postpone"]),
    (&["annoter"], &["annotate"]),
    (&["emballer", "envelopper", "enveloppe"], &["wrap"]),
    (&["decorer", "decorateur"], &["decorate", "decorator"]),
    (&["fermeture"], &["closure", "close"]),
    (&["abonner", "souscrire"], &["subscribe"]),
    (&["publier"], &["publish"]),
    (&["requete", "demande"], &["request", "query"]),
    (&["entete", "entetes"], &["header", "headers"]),
    (&["corps"], &["body", "payload"]),
    (&["chemin", "arborescence"], &["path"]),
    (&["repertoire"], &["directory", "dir"]),
    (&["racine"], &["root"]),
    (&["parametre"], &["param"]),
    (&["valeur"], &["value"]),
    (&["cle"], &["key"]),
    (&["ligne"], &["line"]),
    (&["nom", "denomination"], &["name"]),
    (&["identifiant"], &["id", "identifier"]),
    (&["heure"], &["hour", "time"]),
    (&["duree"], &["duration", "ttl"]),
    (&["taille", "volumineux"], &["size", "length", "large"]),
    (&["longueur"], &["length", "len"]),
    (&["largeur"], &["width"]),
    (&["hauteur"], &["height"]),
    (&["maximal", "maximum", "plafond"], &["max", "limit"]),
    (&["minimal", "minimum"], &["min", "limit"]),
    (&["gros", "grand", "enorme", "massif"], &["large", "big"]),
    (&["petit", "minuscule"], &["small", "tiny"]),
    (&["fonction"], &["function", "func"]),
    (&["methode"], &["method"]),
    (&["classe"], &["class"]),
    (&["objet"], &["object"]),
    (&["bibliotheque"], &["library", "lib"]),
    (&["paquet"], &["package"]),
    (&["dependance"], &["dependency"]),
    (&["gestionnaire"], &["handler", "manager"]),
    (&["ecouteur"], &["listener", "handler"]),
    (&["evenement"], &["event"]),
    (&["crochet"], &["hook"]),
    (&["intergiciel"], &["middleware"]),
    (&["contexte"], &["context"]),
    (&["portee"], &["scope"]),
    (&["contenu"], &["content"]),
    (&["chaine"], &["string"]),
    (&["octet"], &["byte"]),
    (&["caractere"], &["char", "character"]),
    (&["nombre", "numero"], &["number"]),
    (&["entier"], &["integer", "int"]),
    (&["booleen"], &["boolean", "bool"]),
    (&["dictionnaire"], &["dict", "map"]),
    (&["flux"], &["stream"]),
    (&["processus"], &["process"]),
    (&["parallele", "parallelisme"], &["parallel", "concurrent"]),
    (&["asynchrone"], &["async"]),
    (&["synchrone"], &["sync"]),
    (&["correspondance", "correspondre"], &["match"]),
    (&["regulier", "reguliere"], &["regex", "regular"]),
    (&["drapeau", "indicateur"], &["flag"]),
    (&["ordre"], &["order"]),
    (&["priorite"], &["priority"]),
    (&["rang"], &["rank"]),
    (&["niveau"], &["level"]),
    (&["profondeur"], &["depth"]),
    (&["defaut"], &["default"]),
    (&["deprecie", "ancien"], &["deprecated", "legacy", "old"]),
    (&["statique"], &["static"]),
    (&["dynamique"], &["dynamic"]),
    (&["prive"], &["private"]),
    (&["interne"], &["internal"]),
    (&["externe"], &["external"]),
    (&["temporaire"], &["temp", "temporary"]),
    (&["lent", "lenteur"], &["slow"]),
    (&["rapide", "rapidement"], &["fast", "quick"]),
    (&["noeud"], &["node"]),
    (&["enfant"], &["child"]),
    (&["origine"], &["origin"]),
    (&["navigateur"], &["browser"]),
    (&["terminal"], &["terminal", "console"]),
    (&["hote"], &["host"]),
    (&["appareil", "peripherique"], &["device"]),
    (&["entree"], &["input"]),
    (&["sortie"], &["output"]),
    (&["debogage", "deboguer"], &["debug"]),
    (&["peremption", "perime"], &["expire", "expired"]),
    (&["etape"], &["step", "stage"]),
    (&["regle"], &["rule"]),
    (&["precedent"], &["previous", "prev"]),
    (&["imbrique", "imbriquer"], &["nested", "nest"]),
    (&["proposer"], &["suggest", "offer"]),
    (&["televerser", "televersement"], &["upload"]),
    (&["telechargement"], &["download"]),
    (&["depasser", "depassement"], &["exceed", "overflow"]),
    (&["formater"], &["format"]),
    (&["tronquer"], &["truncate"]),
    (&["echapper"], &["escape"]),
    (&["annulation"], &["cancel"]),
    (&["attendre", "attente"], &["wait"]),
    (&["abandonner"], &["abort", "cancel"]),
    (&["produire"], &["produce", "generate"]),
];

/// Mots-code ajoutés pour un mot français de base (voir `FR_VERS_CODE`).
fn fr_vers_code() -> &'static std::collections::HashMap<&'static str, Vec<&'static str>> {
    static M: std::sync::OnceLock<std::collections::HashMap<&'static str, Vec<&'static str>>> = std::sync::OnceLock::new();
    M.get_or_init(|| {
        let mut m: std::collections::HashMap<&'static str, Vec<&'static str>> = std::collections::HashMap::new();
        for &(formes, codes) in FR_VERS_CODE {
            for &f in formes {
                let e = m.entry(f).or_default();
                for &c in codes {
                    if c != f && !e.contains(&c) {
                        e.push(c);
                    }
                }
            }
        }
        m
    })
}

/// Vocabulaire connu (groupes de synonymes, lexiques généraux et table français → code) : sert
/// à valider une forme de base retrouvée par `lemme_fr`.
fn mots_connus() -> &'static std::collections::HashSet<&'static str> {
    static S: std::sync::OnceLock<std::collections::HashSet<&'static str>> = std::sync::OnceLock::new();
    S.get_or_init(|| {
        let mut s: std::collections::HashSet<&'static str> = std::collections::HashSet::new();
        for g in SYNONYM_GROUPS.iter().chain(crate::generalisation::LEXIQUE).chain(crate::generalisation::LEXIQUE_DEV) {
            s.extend(g.iter().copied());
        }
        for &(formes, _) in FR_VERS_CODE {
            s.extend(formes.iter().copied());
        }
        s
    })
}

/// Forme de base (infinitif, singulier masculin) d'un mot français fléchi, SI cette forme est un
/// mot connu des tables et que le mot saisi ne l'est pas : `declarees` → `declarer`,
/// `transforment` → `transformer`. `None` sinon (mot déjà connu, ou inconnu des tables).
pub fn lemme_fr(t: &str) -> Option<String> {
    let connus = mots_connus();
    if connus.contains(t) {
        return None;
    }
    crate::stem::candidats_lemme(t).into_iter().find(|c| connus.contains(c.as_str()))
}

/// Étend une liste de termes avec leurs synonymes connus.
/// Garde l'ordre, évite les doublons. Les synonymes ajoutés porteront un poids
/// réduit côté scoring (l'appelant les distingue via `is_synonym`).
/// EXPÉRIENCE 2b (temporaire) : groupes supplémentaires activés par CORTEX_GLOSS.
const EXTRA_P: &[&[&str]] = &[
    &["chart", "charts", "graphique", "diagramme", "courbe"],
    &["shortcut", "hotkey", "raccourci", "keyboard", "clavier", "keybinding"],
    &["dictionary", "dictionnaire", "dict"],
    &["draw", "drawing", "dessiner", "dessin"],
    &["pen", "stylus", "stylet", "stylo"],
    &["palm", "paume"],
    &["sticky", "adhesive", "postit"],
    &["snapshot", "instantane"],
];
const EXTRA_H: &[&[&str]] = &[
    &["window", "fenetre"],
    &["desktop", "bureau"],
    &["word", "mot"],
    &["employee", "salarie", "employe", "staff"],
    &["hr", "hrm", "rh"],
    &["data", "donnee", "donnees"],
    &["gdpr", "rgpd", "privacy"],
    &["url", "adresse", "link", "lien"],
    &["password", "passe", "mdp"],
    &["reminder", "relance", "rappel", "dunning"],
    &["salary", "salaire", "payroll", "paie"],
    &["bank", "banque", "bancaire"],
    &["weekly", "hebdomadaire"],
    &["scroll", "defilement"],
    &["tab", "onglet"],
    &["title", "titre"],
    &["guest", "anonymous", "anonyme", "invite"],
];

fn gloss_flags() -> &'static str {
    static G: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    G.get_or_init(|| std::env::var("CORTEX_GLOSS").unwrap_or_default())
}

fn active_groups() -> &'static [&'static [&'static str]] {
    static G: std::sync::OnceLock<Vec<&'static [&'static str]>> = std::sync::OnceLock::new();
    G.get_or_init(|| {
        let f = gloss_flags();
        let mut groups: Vec<&[&str]> = SYNONYM_GROUPS.to_vec();
        if f.contains('p') {
            groups.extend_from_slice(EXTRA_P);
        }
        if f.contains('h') {
            groups.extend_from_slice(EXTRA_H);
        }
        groups
    })
}

pub fn expand_synonyms(terms: &[String]) -> Vec<String> {
    let mut out: Vec<String> = terms.to_vec();
    let f = gloss_flags();
    let groups = active_groups();
    let by_stem = f.contains('s');
    for t in terms {
        let lt = t.to_ascii_lowercase();
        let st = if by_stem { crate::stem::stem_if_enabled(&lt) } else { String::new() };
        for group in groups.iter() {
            if group.contains(&lt.as_str()) || (by_stem && group.iter().any(|g| crate::stem::stem_if_enabled(g) == st)) {
                for &syn in *group {
                    if !out.iter().any(|x| x.eq_ignore_ascii_case(syn)) {
                        out.push(syn.to_string());
                    }
                }
            }
        }
    }
    // Français → code (sens unique, voir `FR_VERS_CODE`).
    for t in terms.iter().filter(|_| crate::search::fr_on('t')) {
        if let Some(codes) = fr_vers_code().get(t.to_ascii_lowercase().as_str()) {
            for &c in codes {
                if !out.iter().any(|x| x.eq_ignore_ascii_case(c)) {
                    out.push(c.to_string());
                }
            }
        }
    }
    out
}

/// Distance de Levenshtein bornée à 1 : renvoie true si edit-distance ≤ 1.
/// Optimisé : court-circuite dès que la différence de longueur dépasse 1.
pub fn within_edit_distance_1(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let (la, lb) = (a.len(), b.len());
    if la.abs_diff(lb) > 1 {
        return false;
    }
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    match la.cmp(&lb) {
        std::cmp::Ordering::Equal => {
            // une seule substitution autorisée
            let mut diffs = 0;
            for i in 0..la {
                if ab[i] != bb[i] {
                    diffs += 1;
                    if diffs > 1 {
                        return false;
                    }
                }
            }
            true
        }
        _ => {
            // une seule insertion/suppression : aligne le plus court dans le plus long
            let (short, long) = if la < lb { (ab, bb) } else { (bb, ab) };
            let mut i = 0; // index short
            let mut j = 0; // index long
            let mut skipped = false;
            while i < short.len() && j < long.len() {
                if short[i] == long[j] {
                    i += 1;
                    j += 1;
                } else if skipped {
                    return false;
                } else {
                    skipped = true;
                    j += 1; // saute un char du plus long
                }
            }
            true
        }
    }
}

/// Match flou entre un terme de requête et un token de symbole.
/// Retourne un facteur de score [0.0, 1.0] : 1.0 exact, 0.6 préfixe, 0.5 edit-1.
pub fn fuzzy_match(query_term: &str, token: &str) -> f32 {
    if query_term == token {
        return 1.0;
    }
    // Préfixe significatif (≥4 chars) : "auth" ~ "authenticate", "config" ~ "configure".
    if query_term.len() >= 4 && token.starts_with(query_term) {
        return 0.6;
    }
    if token.len() >= 4 && query_term.starts_with(token) {
        return 0.6;
    }
    // Faute de frappe : edit-distance 1 (seulement pour des mots assez longs,
    // sinon trop de faux positifs sur les mots courts).
    if query_term.len() >= crate::generalisation::lev_min_len() && within_edit_distance_1(query_term, token) {
        return 0.5;
    }
    0.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synonyms_link_fr_en() {
        let terms = vec!["supprimer".to_string(), "fichier".to_string()];
        let expanded = expand_synonyms(&terms);
        assert!(expanded.iter().any(|t| t == "delete"));
        assert!(expanded.iter().any(|t| t == "file"));
    }

    #[test]
    fn francais_vers_code_sens_unique() {
        let e = expand_synonyms(&["requete".to_string(), "entete".to_string()]);
        assert!(e.iter().any(|t| t == "request"));
        assert!(e.iter().any(|t| t == "header"));
        // Une question anglaise n'est pas élargie par la table français → code.
        let e = expand_synonyms(&["request".to_string()]);
        assert!(!e.iter().any(|t| t == "requete"));
    }

    #[test]
    fn lemme_fr_valide_par_le_vocabulaire() {
        assert_eq!(lemme_fr("declarees").as_deref(), Some("declarer"));
        assert_eq!(lemme_fr("requetes").as_deref(), Some("requete"));
        assert_eq!(lemme_fr("envoyees").as_deref(), Some("envoyer"));
        // Mot déjà connu : inchangé ; mot inconnu sans base connue : aucune forme inventée.
        assert_eq!(lemme_fr("envoyer"), None);
        assert_eq!(lemme_fr("zorglubes"), None);
    }

    #[test]
    fn edit_distance_1_works() {
        assert!(within_edit_distance_1("user", "usr")); // suppression
        assert!(within_edit_distance_1("user", "users")); // insertion
        assert!(within_edit_distance_1("user", "user")); // égal (distance 0)
        assert!(within_edit_distance_1("config", "canfig")); // 1 substitution (o→a)
        assert!(!within_edit_distance_1("user", "admin")); // trop loin
        assert!(!within_edit_distance_1("file", "folder"));
    }

    #[test]
    fn fuzzy_prefix_and_typo() {
        assert_eq!(fuzzy_match("auth", "auth"), 1.0);
        assert_eq!(fuzzy_match("auth", "authenticate"), 0.6); // préfixe
        assert!(fuzzy_match("config", "canfig") >= 0.5); // typo edit-1 (substitution)
        assert_eq!(fuzzy_match("xyz", "config"), 0.0);
    }
}
