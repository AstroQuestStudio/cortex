//! Généralisation du classement : correctifs qui ne dépendent d'aucun projet.
//!
//! Chaque idée est un drapeau d'une variable d'environnement, pour comparer à l'état
//! précédent sans recompiler :
//!   `CORTEX_GEN` absent = `tvgrq` ; `CORTEX_GEN=off` = rien ; sinon une liste de lettres
//!   (`t` outillage, `v` lexique, `g` lexique seulement pour un terme rare, `r` rôle, `q` mots de remplissage, `e` fautes de frappe,
//!   `d` lexique FR/EN du développement logiciel, hors défaut : neutre sur les bancs). Exemple : `CORTEX_GEN=tv`.
//!
//! - `t` : pénalité des dossiers d'OUTILLAGE (configuration d'agents, skills, CI, support de test).
//!   Cause : ces dossiers contiennent des fichiers prolixes (documentation d'outils) qui
//!   couvrent beaucoup de mots d'une question de fonctionnalité et passent devant le code.
//!   Pas de pénalité si la question parle d'agents, de skills, de CI, etc.
//! - `v` : lexique FR/EN courant (voir `LEXIQUE`), ajouté aux groupes de synonymes.
//! - `r` : bonus de RÔLE. Quand la question nomme un rôle (page, serveur, hook...), les fichiers
//!   dont le chemin porte ce rôle passent devant leurs jumeaux d'un autre rôle.

use crate::symbol::fold_accents;

fn flags() -> &'static str {
    static G: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    G.get_or_init(|| match std::env::var("CORTEX_GEN") {
        Ok(v) if v == "off" || v == "0" => String::new(),
        Ok(v) if !v.is_empty() => v,
        _ => "tvgrq".to_string(),
    })
}

pub(crate) fn on(letter: char) -> bool {
    flags().contains(letter)
}

// ───────────────────────────── t : outillage ─────────────────────────────

/// Segments de chemin qui désignent de l'outillage (pas du code produit).
const TOOLING_DIRS: &[&str] = &[
    ".claude",
    ".agents",
    ".agent",
    ".codex",
    ".cursor",
    ".windsurf",
    ".github",
    ".githooks",
    ".gitlab",
    ".husky",
    ".vscode",
    ".idea",
    ".devcontainer",
    "tests_support",
    "test_support",
    "testsupport",
    "testutils",
    "test_utils",
    "fixtures",
    "mocks",
    "__mocks__",
    "__fixtures__",
    "snapshots",
    "__snapshots__",
];

/// Mots de question qui désignent justement ces dossiers : alors pas de pénalité.
const TOOLING_WORDS: &[&str] = &[
    "claude",
    "agent",
    "agents",
    "skill",
    "skills",
    "codex",
    "cursor",
    "github",
    "ci",
    "workflow",
    "workflows",
    "hook",
    "hooks",
    "husky",
    "vscode",
    "devcontainer",
    "gitlab",
    "action",
    "actions",
    "fixture",
    "fixtures",
    "mock",
    "mocks",
    "snapshot",
    "snapshots",
    "support",
    "outillage",
    "memoire",
    "memory",
    "prompt",
    "prompts",
    "commande",
    "commandes",
    "command",
    "commands",
    "subagent",
    "subagents",
    "mcp",
];

pub(crate) const TOOLING_PENALTY: f32 = 0.5;

pub(crate) fn is_tooling_path(path: &str) -> bool {
    path.split(['/', '\\']).any(|seg| TOOLING_DIRS.iter().any(|d| seg.eq_ignore_ascii_case(d)))
}

/// Vrai si la question parle d'outillage (alors on ne pénalise pas).
pub(crate) fn query_wants_tooling(terms: &[String]) -> bool {
    terms.iter().any(|t| TOOLING_WORDS.contains(&t.as_str()))
}

// ───────────────────────────── e : fautes de frappe ─────────────────────────────

/// Longueur minimale d'un terme pour la tolérance d'UNE faute de frappe. Sur 4 lettres, une
/// substitution rapproche des mots sans rapport (`sign`~`sigv`, `user`~`used`) : avec `e`, 5 lettres.
pub(crate) fn lev_min_len() -> usize {
    if on('e') {
        5
    } else {
        4
    }
}

// ───────────────────────────── q : mots de question ─────────────────────────────

/// Mots de remplissage d'une QUESTION (interrogatifs, modaux, « permet de », « utilisé pour »)
/// qui ne désignent aucune partie du code et ne font que diluer la couverture des termes.
const FILLER_WORDS: &[&str] = &[
    "comment",
    "pourquoi",
    "quel",
    "quelle",
    "quels",
    "quelles",
    "where",
    "why",
    "who",
    "whose",
    "does",
    "did",
    "can",
    "could",
    "should",
    "would",
    "will",
    "faut",
    "peut",
    "peuvent",
    "permet",
    "permettre",
    "allows",
    "allow",
    "afin",
    "lorsque",
    "pendant",
    "during",
    "about",
    "quelque",
    "quelques",
    "several",
    "plusieurs",
    "thing",
    "things",
    "chose",
    "choses",
    "utilise",
    "utilisee",
    "used",
    "using",
    "uses",
    "via",
    "etc",
    "ensemble",
    "all",
    "any",
    "other",
    "autre",
    "autres",
    "meme",
    "same",
    "very",
    "just",
    "only",
    "also",
    "puis",
    "then",
    "alors",
    "ainsi",
    "cela",
    "ceci",
    "celui",
    "celle",
    "ceux",
    "elles",
    "they",
    "them",
    "these",
    "those",
    "there",
    "here",
    "has",
    "have",
    "had",
    "been",
    "being",
    "were",
    "than",
];

pub(crate) fn is_filler(t: &str) -> bool {
    on('q') && FILLER_WORDS.contains(&t)
}

// ───────────────────────────── r : rôle ─────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Page,
    Server,
    Hook,
    Store,
}

/// Rôles que nomme la question (mots entiers, accents repliés).
pub(crate) fn query_roles(terms: &[String]) -> Vec<Role> {
    let mut out = Vec::new();
    let has = |ws: &[&str]| terms.iter().any(|t| ws.contains(&t.as_str()));
    if has(&["page", "pages", "ecran", "ecrans", "screen", "screens"]) {
        out.push(Role::Page);
    }
    if has(&["serveur", "server", "backend", "endpoint", "edge", "api", "serverless"]) {
        out.push(Role::Server);
    }
    if has(&["hook", "hooks"]) {
        out.push(Role::Hook);
    }
    if has(&["store", "stores", "zustand", "redux"]) {
        out.push(Role::Store);
    }
    out
}

/// Un chemin porte-t-il le rôle ? (segment de dossier ou fin du nom de fichier)
pub(crate) fn path_has_role(path: &str, r: Role) -> bool {
    let p = path.to_ascii_lowercase().replace('\\', "/");
    let name = p.rsplit('/').next().unwrap_or(&p);
    let stem = name.split('.').next().unwrap_or(name);
    let seg = |s: &str| p.split('/').any(|x| x == s);
    match r {
        Role::Page => seg("pages") || seg("page") || seg("screens") || seg("views") || stem.ends_with("page") || stem.ends_with("screen"),
        Role::Server => {
            seg("functions") || seg("server") || seg("api") || seg("backend") || seg("routes") || seg("crates") || stem.ends_with("server")
        }
        Role::Hook => seg("hooks") || stem.starts_with("use"),
        Role::Store => seg("stores") || seg("store") || stem.ends_with("store"),
    }
}

pub(crate) const ROLE_BONUS: f32 = 1.2;

// ───────────────────────────── v : lexique ─────────────────────────────

/// Lexique FR/EN courant des applications (SaaS, bureautique, gestion). Chaque ligne est un
/// groupe de mots équivalents ; mots simples, minuscules, accents repliés. Groupes courts : un
/// groupe large noie la question dans ses synonymes.
pub(crate) const LEXIQUE: &[&[&str]] = &[
    // Cycle de vie d'un abonnement / compte
    &["resiliation", "resilier", "termination", "terminate", "cancellation"],
    &["abonnement", "subscription", "subscribe", "souscription"],
    &["desabonnement", "desabonner", "desinscription", "desinscrire", "unsubscribe", "optout", "opt"],
    &["retrait", "retirer", "withdrawal", "withdraw"],
    &["remboursement", "rembourser", "refund", "reimburse"],
    &["essai", "trial"],
    &["renouvellement", "renouveler", "renewal", "renew"],
    &["expiration", "expirer", "expire", "echeance", "expiry"],
    &["inscription", "signup", "register", "registration", "enregistrement"],
    &["deconnexion", "logout", "signout"],
    &["invitation", "inviter", "invite"],
    &["tarif", "tarifs", "prix", "price", "pricing"],
    &["devis", "quote", "estimate"],
    &["commande", "order", "purchase"],
    &["panier", "cart", "basket"],
    &["caisse", "checkout"],
    &["recu", "receipt"],
    &["depense", "expense", "depenses", "expenses"],
    // Voix / médias / appels
    &["voix", "voice", "vocal", "speech", "tts", "parole", "dictee", "dictation"],
    &["audio", "son", "sound"],
    &["video", "visio", "visioconference", "livekit", "webrtc", "meeting", "conference"],
    &["appel", "call", "calls"],
    &["micro", "microphone", "mic"],
    &["camera", "webcam"],
    &["ecran", "screen"],
    &["partage", "partager", "share", "sharing"],
    &["image", "photo", "picture", "visuel"],
    &["imprimer", "impression", "print", "printing"],
    &["apercu", "preview"],
    &["miniature", "thumbnail", "vignette"],
    &["enregistrer", "record", "recording"],
    &["traduire", "traduction", "translate", "translation", "i18n", "locale", "langue", "language"],
    &["lecteur", "player"],
    // Communication
    &["notification", "notifications", "notifier", "alerte", "alert"],
    &["courriel", "email", "mail", "emails", "courrier"],
    &["message", "messages", "messagerie", "chat", "discussion", "conversation"],
    &["commentaire", "comment", "comments", "commentaires"],
    &["mention", "mentionner", "mentions"],
    &["sms", "texto"],
    &["abonne", "follower", "subscriber"],
    // Temps
    &["calendrier", "calendar", "agenda"],
    &["conge", "conges", "leave", "vacation", "absence"],
    &["horaire", "horaires", "schedule", "planning"],
    &["echeance", "deadline", "due"],
    &["pointage", "timesheet", "clocking", "presence"],
    &["fuseau", "timezone"],
    &["jour", "day"],
    &["mois", "month"],
    &["annee", "year"],
    &["semaine", "week"],
    // Sécurité
    &["chiffrement", "chiffrer", "encryption", "encrypt", "cipher", "crypto", "cryptage"],
    &["signature", "signer", "sign", "signed", "signing"],
    &["jeton", "token", "jwt"],
    &["verrou", "lock", "verrouiller", "mutex"],
    &["secret", "secrets", "credentials", "identifiants"],
    &["verification", "verify", "verifier", "valider", "validate", "validation"],
    &["consentement", "consent", "cookie", "cookies"],
    &["bloquer", "block", "blocage", "ban", "bannir", "blacklist"],
    &["2fa", "mfa", "totp", "otp", "deuxfacteurs"],
    &["empreinte", "hash", "fingerprint"],
    &["limite", "limit", "limiter", "throttle", "ratelimit", "quota"],
    &["audit", "journal", "log", "logs", "historique", "history", "trace"],
    &["anomalie", "anomaly", "fraude", "fraud"],
    &["pagination", "paginer", "paginate"],
    &["accueil", "landing", "welcome", "bienvenue", "onboarding"],
    &["rendez", "appointment", "booking", "reservation", "reserver"],
    &["tri", "trier", "sort", "sorting"],
    &["lecture", "playback"],
    &["souris", "mouse"],
    // Données / fichiers
    &["sauvegarde", "backup", "archive", "archiver", "archiving"],
    &["restaurer", "restore", "restauration", "recover", "recuperation"],
    &["corbeille", "trash", "bin", "recycle"],
    &["doublon", "duplicate", "duplicates", "dedup", "dedupliquer"],
    &["fusion", "fusionner", "merge", "merging"],
    &["filtre", "filtrer", "filter", "filtering"],
    &["cache", "caching", "memoire", "memoize"],
    &["migration", "migrer", "migrate"],
    &["brouillon", "draft"],
    &["modele", "template", "gabarit", "model"],
    &["version", "versions", "revision", "versionner"],
    &["etiquette", "tag", "label", "libelle"],
    &["categorie", "category"],
    &["favori", "favoris", "favorite", "favorites", "bookmark", "bookmarks"],
    &["recent", "recents", "recemment"],
    &["compteur", "counter"],
    &["statistique", "statistiques", "stats", "metrics", "metriques", "analytics", "kpi"],
    &["tableau", "dashboard", "board"],
    &["rapport", "report", "reporting"],
    &["extrait", "excerpt", "snippet", "extract"],
    &["resume", "summary", "summarize", "synthese"],
    &["ocr", "reconnaissance", "scan", "scanner"],
    &["glisser", "drag", "drop", "deposer", "dnd"],
    &["copier", "copy", "dupliquer", "clone"],
    &["coller", "paste"],
    &["annuler", "undo", "cancel", "revert", "rollback"],
    &["rejouer", "redo", "replay", "rejeu"],
    &["renommer", "rename"],
    &["deplacer", "move", "relocate"],
    &["ouvrir", "open"],
    &["fermer", "close"],
    &["afficher", "display", "show", "render", "afficher"],
    &["masquer", "hide", "cacher"],
    &["agrandir", "zoom", "enlarge", "maximize"],
    &["redimensionner", "resize", "resizing"],
    &["rotation", "tourner", "rotate"],
    &["couleur", "color", "colour", "palette"],
    &["police", "font", "typographie", "typography"],
    &["alignement", "aligner", "align", "alignment"],
    &["icone", "icon", "pictogramme"],
    &["menu", "navigation", "nav", "navbar", "sidebar", "barre"],
    &["tutoriel", "tutorial", "guide", "visite", "tour", "walkthrough"],
    &["aide", "help", "support", "faq"],
    &["contact", "contacts", "carnet", "address"],
    &["client", "customer", "clients", "customers"],
    &["fournisseur", "supplier", "vendor", "provider"],
    &["partenaire", "partner"],
    &["equipe", "team", "groupe", "group"],
    &["role", "roles", "profil", "profile"],
    &["proprietaire", "owner", "ownership"],
    &["administrateur", "admin", "administrator"],
    &["parametre", "preference", "preferences", "option", "options"],
    &["interrupteur", "toggle", "switch", "flag", "bascule"],
    &["lien", "link", "hyperlien", "hyperlink"],
    &["recherche", "search", "recherches", "query"],
    &["suggestion", "suggest", "suggestions", "autocomplete", "autocompletion", "completion"],
    &["correction", "orthographe", "spell", "spellcheck", "grammar", "grammaire"],
    &["import", "importer", "importation"],
    &["export", "exporter", "exportation"],
    &["synchronisation", "synchroniser", "sync", "synchro"],
    &["conflit", "conflict", "conflicts"],
    &["serveur", "server", "backend"],
    &["tache", "task", "tasks", "job", "jobs", "todo"],
    &["planifie", "planifier", "cron", "scheduled", "scheduler", "planificateur"],
    &["relancer", "retry", "reessayer", "retries"],
    &["delai", "timeout", "latence", "latency", "delay"],
    &["erreur", "error", "echec", "failure", "panne", "outage"],
    &["avertissement", "warning", "warn"],
    &["chargement", "loading", "loader", "spinner", "skeleton", "squelette"],
    &["vide", "empty", "blank"],
    &["selection", "select", "selectionner", "selected", "picker", "selecteur"],
    &["case", "checkbox", "cocher"],
    &["infobulle", "tooltip", "bulle", "popover"],
    &["pastille", "badge", "chip"],
    &["carte", "card", "tuile", "tile"],
    &["liste", "list", "listing"],
    &["arbre", "tree", "arborescence", "hierarchy", "hierarchie"],
    &["colonne", "column", "colonnes", "columns"],
    &["cellule", "cell"],
    &["formule", "formula", "calcul", "compute", "calculate", "calculer"],
    &["total", "somme", "sum", "totals", "aggregate", "agregat"],
    &["moyenne", "average", "mean"],
    &["pourcentage", "percent", "percentage", "ratio"],
    &["devise", "currency", "monnaie"],
    &["taxe", "tva", "vat", "tax", "impot", "fiscal"],
    &["comptabilite", "accounting", "comptable", "ledger", "grand", "livre"],
    &["rapprochement", "reconciliation", "reconcile", "lettrage"],
    &["virement", "transfer", "wire"],
    &["prelevement", "debit", "directdebit", "mandat", "mandate"],
    &["encaissement", "collection", "encaisser", "collect"],
    &["impaye", "unpaid", "overdue", "retard", "late"],
    &["solde", "balance", "reste"],
    &["recrutement", "recruitment", "hiring", "candidat", "candidate", "candidature"],
    &["formation", "training", "course", "cours"],
    &["evaluation", "review", "assessment", "entretien", "appraisal"],
    &["objectif", "goal", "objective", "okr"],
    &["projet", "project"],
    &["jalon", "milestone"],
    &["diagramme", "diagram", "gantt", "flowchart", "organigramme"],
    &["tableur", "spreadsheet", "excel", "xlsx"],
    &["traitement", "processor", "processing", "pipeline"],
    &["publication", "publier", "publish", "publishing", "release", "deployer", "deploy", "deployment"],
    &["maintenance", "entretien"],
    &["telephone", "phone", "mobile", "tel"],
    &["tactile", "touch", "gestes", "gesture", "gestures", "swipe"],
    &["clavier", "keyboard", "touche", "key", "keys"],
    &["raccourci", "shortcut", "hotkey"],
    &["mosaique", "grid", "grille"],
    &["calque", "layer", "layers", "couche", "couches"],
    &["forme", "shape", "shapes", "figure"],
    &["gomme", "eraser", "erase", "effacer"],
    &["surligneur", "highlighter", "highlight", "surligner"],
    &["ancre", "anchor"],
    &["logo", "emblem"],
    &["site", "website", "web"],
    &["navigateur", "browser"],
    &["extension", "plugin", "addon", "module"],
    &["pont", "bridge", "passerelle", "gateway"],
    &["webhook", "callback", "rappel"],
    &["flux", "stream", "streaming"],
    &["diffusion", "broadcast", "diffuser"],
    &["canal", "channel", "salon", "room"],
    &["participant", "participants", "attendee", "attendees"],
    &["presentateur", "presenter", "orateur", "speaker"],
];

/// Lexique FR/EN du DÉVELOPPEMENT logiciel (drapeau `d`) : les questions en français sur du code
/// dont les identifiants sont anglais (`chemin` → `path`, `analyseur` → `parser`). Mêmes règles
/// que `LEXIQUE` : groupes courts, mots simples, accents repliés ; appliqué seulement aux mots
/// de question rares dans le projet.
pub(crate) const LEXIQUE_DEV: &[&[&str]] = &[
    &["chemin", "path", "paths"],
    &["repertoire", "directory", "dir", "folder"],
    &["racine", "root"],
    &["ligne", "line", "lines"],
    &["motif", "pattern", "patterns", "regex", "glob"],
    &["correspondance", "correspondre", "match", "matches", "matching", "matcher"],
    &["remplacer", "remplacement", "replace", "replacement", "substitute", "substitution"],
    &["analyseur", "analyser", "analyse", "parse", "parser", "parsing", "analyzer"],
    &["gestionnaire", "handler", "manager"],
    &["ecouteur", "listener", "observer", "subscriber"],
    &["evenement", "evenements", "event", "events"],
    &["crochet", "hook", "hooks"],
    &["intergiciel", "middleware"],
    &["route", "routes", "routage", "routeur", "router", "routing"],
    &["requete", "demande", "request", "requests"],
    &["reponse", "response", "responses", "reply"],
    &["entete", "entetes", "header", "headers"],
    &["corps", "body", "payload"],
    &["contexte", "context", "ctx"],
    &["vue", "view", "views"],
    &["composant", "component", "components", "widget"],
    &["moteur", "engine"],
    &["pilote", "driver"],
    &["bibliotheque", "library", "lib"],
    &["paquet", "package", "crate", "module"],
    &["dependance", "dependances", "dependency", "dependencies", "deps"],
    &["compilation", "compiler", "compile", "build", "construire"],
    &["attendre", "attente", "wait", "await", "awaiting"],
    &["asynchrone", "async", "asynchronous"],
    &["parallele", "parallelisme", "parallel", "concurrent", "concurrency", "thread", "threads"],
    &["processus", "process", "subprocess"],
    &["interruption", "interrupt", "signal", "signals"],
    &["sondage", "poll", "polling"],
    &["pile", "stack"],
    &["dictionnaire", "map", "dict", "hashmap"],
    &["hashset"],
    &["tableau", "array", "vec", "vector"],
    &["chaine", "chaines", "string", "strings", "str"],
    &["entier", "integer", "int"],
    &["booleen", "boolean", "bool"],
    &["nombre", "number", "numeric"],
    &["octet", "octets", "byte", "bytes"],
    &["caractere", "caracteres", "char", "character", "characters"],
    &["encodage", "encoding", "charset", "utf8"],
    &["decodage", "decoder", "decode", "decoding"],
    &["serialisation", "serialiser", "serialize", "serialization", "serde"],
    &["ignorer", "ignore", "ignored", "ignorees", "exclure", "exclude", "excluded"],
    &["inclure", "include", "included"],
    &["binaire", "binary", "bin"],
    &["executable", "exe", "executables"],
    &["licence", "license"],
    &["documentation", "docs", "doc"],
    &["parcours", "parcourir", "walk", "walker", "traverse", "traversal", "iterate"],
    &["recursif", "recursive", "recursion", "recursivement"],
    &["symbolique", "symlink", "symlinks"],
    &["iterateur", "iterator", "iter"],
    &["boucle", "loop"],
    &["fonction", "function", "func", "fn"],
    &["methode", "method"],
    &["classe", "class"],
    &["variable", "var", "variables"],
    &["constante", "constant", "const"],
    &["portee", "scope"],
    &["pointeur", "pointer", "ptr"],
    &["duree", "duration", "lifetime"],
    &["echappement", "echapper", "escape", "escaping", "escaped"],
    &["terminal", "console", "tty", "shell"],
    &["largeur", "width"],
    &["hauteur", "height"],
    &["taille", "size", "length", "len"],
    &["profondeur", "depth"],
    &["horloge", "clock"],
    &["decalage", "offset"],
    &["indice", "index", "indices"],
    &["plage", "intervalle", "range", "interval"],
    &["borne", "bound", "bounds", "boundary"],
    &["defaut", "default", "defaults"],
    &["valeur", "value", "values"],
    &["cle", "cles", "key", "keys"],
    &["resultat", "resultats", "result", "results", "outcome"],
    &["rendu", "render", "rendering", "renderer"],
    &["redirection", "rediriger", "redirect", "redirects"],
    &["rejeter", "rejet", "reject", "rejection"],
    &["morceau", "morceaux", "chunk", "chunks", "piece", "bloc", "block"],
    &["lot", "lots", "batch", "batches"],
    &["hote", "host", "hostname"],
    &["prise", "socket", "sockets"],
    &["transmission", "transmettre", "send", "sending", "emit", "emettre"],
    &["reception", "recevoir", "receive", "receiving"],
    &["lecture", "lire", "read", "reading", "reader"],
    &["ecriture", "ecrire", "write", "writing", "writer"],
    &["tampon", "buffer", "buffered", "buffering"],
    &["sortie", "output", "stdout"],
    &["entree", "input", "stdin"],
    &["argument", "arguments", "args", "arg", "param", "params"],
    &["commande", "commandes", "command", "commands", "cli", "commandline"],
    &["configuration", "config", "configurer", "configure", "settings"],
    &["initialisation", "initialiser", "init", "initialize", "initialise", "setup", "bootstrap"],
    &["instance", "instancier", "instantiate", "instance"],
    &["creation", "creer", "create", "creating", "constructor", "constructeur"],
    &["destruction", "detruire", "destroy", "destructor", "dispose", "cleanup", "nettoyage", "nettoyer"],
    &["abandon", "abandonner", "abort", "abandonne"],
    &["declencher", "declenchement", "trigger", "fire", "dispatch"],
    &["detecter", "detection", "detect", "detecting", "sniff"],
    &["reconnaitre", "reconnaissance", "recognize", "recognition"],
    &["decouvrir", "decouverte", "discover", "discovery", "lookup", "resolve", "resolution", "resoudre"],
    &["charger", "chargement", "load", "loading", "loader"],
    &["reutiliser", "reuse", "reusable", "recycle", "recycler", "pool"],
    &["verrouillage", "lock", "locking", "locked", "mutex"],
    &["partage", "shared", "sharing", "share"],
    &["etat", "state", "status", "statut"],
    &["transition", "transitions", "transition"],
    &["suppression", "supprimer", "delete", "deletion", "removal", "remove"],
    &["ajout", "ajouter", "add", "append", "adding", "insertion", "inserer", "insert"],
    &["modification", "modifier", "modify", "mutation", "mutate", "edit"],
    &["comparaison", "comparer", "compare", "comparison", "diff", "difference"],
    &["conversion", "convertir", "convert", "converter", "cast", "transform", "transformer"],
    &["normalisation", "normaliser", "normalize", "normalization", "normalise"],
    &["troncature", "tronquer", "truncate", "truncation", "clip", "cut", "couper"],
    &["aplatir", "flatten", "flat", "plat"],
    &["trier", "tri", "sort", "sorted", "sorting", "ordering"],
    &["unique", "uniques", "unicite", "unique", "dedup", "distinct"],
    &["compter", "comptage", "count", "counting", "tally"],
    &["somme", "sommer", "sum", "total", "accumulate", "accumuler"],
    &["maximum", "max", "plafond", "ceiling", "cap"],
    &["minimum", "min", "plancher", "floor"],
    &["precedent", "previous", "prev", "avant", "before"],
    &["suivant", "next", "apres", "after", "following"],
    &["premier", "first", "head", "tete"],
    &["dernier", "last", "tail", "queue"],
    &["parent", "parents", "ancetre", "ancestor"],
    &["enfant", "enfants", "child", "children", "descendant"],
    &["noeud", "noeuds", "node", "nodes", "vertex"],
    &["graphe", "graph", "graphs"],
    &["lien", "liens", "link", "links", "edge"],
    &["visiteur", "visitor", "visit", "visiter"],
    &["usine", "factory", "fabrique", "builder", "constructeur"],
    &["adaptateur", "adapter", "adaptor", "wrapper", "enveloppe"],
    &["fournisseur", "provider", "supplier"],
    &["strategie", "strategy", "policy", "politique"],
    &["magasin", "store", "stockage", "storage", "depot", "repository"],
    &["persistance", "persister", "persist", "persistent", "durable"],
    &["journalisation", "journaliser", "logging", "logger", "log", "logs", "trace", "tracing"],
    &["diagnostic", "diagnostics", "debug", "debogage", "deboguer", "debugger"],
    &["performance", "perf", "rapidite", "vitesse", "speed", "fast", "optimisation", "optimiser", "optimize"],
    &["memoire", "memory", "mem", "ram"],
    &["fuite", "leak", "leaks"],
    &["securite", "security", "secure", "securise", "safe"],
    &["fiabilite", "robuste", "robust", "reliable", "resilience"],
    &["compatibilite", "compatible", "compat", "compatibility", "portabilite", "portable", "portability"],
    &["version", "versions", "versionning", "semver"],
    &["plateforme", "platform", "os", "systeme", "system"],
    &["fenetre", "window", "windows"],
];

/// Synonymes du lexique pour UN terme de question (hors le terme lui-même).
pub(crate) fn lexique_synonymes(term: &str) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    if !on('v') {
        return out;
    }
    for g in LEXIQUE {
        if g.contains(&term) {
            for &w in g.iter() {
                if w != term && !out.contains(&w) {
                    out.push(w);
                }
            }
        }
    }
    if on('d') {
        for g in LEXIQUE_DEV {
            if g.contains(&term) {
                for &w in g.iter() {
                    if w != term && !out.contains(&w) {
                        out.push(w);
                    }
                }
            }
        }
    }
    out
}

/// Poids d'un synonyme du lexique (les synonymes des groupes historiques gardent 0,55).
pub(crate) fn poids_lexique() -> f32 {
    static W: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *W.get_or_init(|| std::env::var("CORTEX_GEN_LEXW").ok().and_then(|v| v.parse().ok()).unwrap_or(0.55))
}

/// Seuil de rareté (fréquence documentaire) sous lequel un terme est « absent » du projet : le
/// lexique ne s'applique qu'alors si `g` est actif (écart de vocabulaire = terme introuvable).
pub(crate) fn seuil_rare() -> usize {
    static W: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *W.get_or_init(|| std::env::var("CORTEX_GEN_RARE").ok().and_then(|v| v.parse().ok()).unwrap_or(40))
}

/// Mots d'une question, accents repliés, en minuscules (pour les tests de rôle/outillage).
#[allow(dead_code)]
pub(crate) fn fold(s: &str) -> String {
    fold_accents(s).to_ascii_lowercase()
}
