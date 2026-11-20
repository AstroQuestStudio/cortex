//! `cortex bench-agent` — banc d'AGENT (architecture v2 §7).
//!
//! Chaque tâche (`bench/agent_tasks.json`) est une question de compréhension
//! réaliste avec ses FAITS attendus (chemins, ou `chemin#symbole`) et deux
//! séquences d'appels scriptées : avec Cortex (`find`, `card`, `impact`…) et
//! sans (ce que fait un agent avec Grep, Glob et Read). Le banc les rejoue et
//! compte, pour chacune : appels, tokens lus (caractères / 4, arrondi au-dessus,
//! sortie complète de chaque appel) et faits couverts.
//!
//! Règles, identiques des deux côtés :
//! - une étape n'utilise que les mots de la question, son `vocabulaire`
//!   (traductions qu'un agent ferait) et ce que les sorties PRÉCÉDENTES ont
//!   montré ; `$k` = k-ième identifiant (`S:`/`D:`/`F:`) de la sortie
//!   précédente côté Cortex, k-ième fichier de la dernière recherche côté sans
//!   Cortex. Une étape qui viole la règle n'est PAS jouée (et signalée) ;
//! - un fait `chemin` est couvert si une sortie contient le chemin ; un fait
//!   `chemin#symbole` si un même bloc contient le chemin et le symbole (mot
//!   entier). Bloc = une ligne, sauf pour une lecture de code (`read`) : toute
//!   la sortie.
//!
//! Outils « sans Cortex » (sur les fichiers de l'atlas, c'est-à-dire ceux que
//! rg verrait avec les règles .gitignore) :
//! - `glob <fragment>` : chemins contenant le fragment (comme Glob), 100 au plus ;
//! - `rg <motif>[|<motif>…] [dossier]` : lignes `chemin:ligne:texte`
//!   (sous-chaîne, insensible à la casse, comme Grep en mode contenu), 250
//!   lignes au plus (limite par défaut de Grep), texte coupé à 500 caractères ;
//!   `$k` y désigne le k-ième fichier par nombre de lignes trouvées (ordre plus
//!   favorable que celui de rg) ;
//! - `rgl <motif> [dossier]` : fichiers seulement ;
//! - `read <chemin|$k>` : fichier entier numéroté (comme Read), 2 000 lignes au plus.

use crate::atlas::Handle;
use crate::outils::{self, Appel};
use crate::symbol::fold_accents;
use serde::Deserialize;
use std::path::Path;

#[derive(Deserialize)]
struct Fichier {
    #[serde(default)]
    project: Option<String>,
    tasks: Vec<Tache>,
}

#[derive(Deserialize)]
struct Tache {
    id: String,
    #[serde(rename = "type", default)]
    genre: String,
    question: String,
    #[serde(default)]
    vocabulaire: Vec<String>,
    facts: Vec<String>,
    cortex: Vec<String>,
    sans_cortex: Vec<String>,
}

/// Une séquence rejouée.
#[derive(Default)]
struct Trace {
    appels: usize,
    tokens: usize,
    /// (étape, sortie, lecture de code ?)
    sorties: Vec<(String, String, bool)>,
    violations: Vec<String>,
}

fn tokens(s: &str) -> usize {
    s.chars().count().div_ceil(4)
}

fn plier(s: &str) -> String {
    fold_accents(s).to_lowercase()
}

/// Mots (accents repliés, minuscules) d'un texte, et leurs radicaux.
fn mots_vus(vu: &str) -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    for w in plier(vu).split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()) {
        set.insert(crate::stem::stem(w));
        set.insert(w.to_string());
    }
    set
}

/// Une étape n'utilise que des mots déjà vus (question, vocabulaire, sorties) :
/// chaque argument figure tel quel dans ce qui a été vu, ou chacun de ses mots
/// (découpe sur les séparateurs : « rate-limit » ← « Rate Limit ») y figure,
/// au radical près (« reprise » ← « repris »). Un identifiant (`camelCase`)
/// n'est pas découpé : il doit avoir été vu.
fn justifiee(etape: &str, vu: &str) -> Result<(), String> {
    let texte = plier(vu);
    let set = mots_vus(vu);
    let mut mots = etape.split_whitespace();
    mots.next(); // l'outil
    for m in mots {
        if m.starts_with('$') || m.starts_with('-') || m.parse::<u32>().is_ok() {
            continue;
        }
        for alt in m.split('|') {
            let a = plier(alt.trim_matches(|c: char| c == '?' || c == ',' || c == '«' || c == '»' || c == '"'));
            if a.is_empty() || texte.contains(&a) {
                continue;
            }
            let ok = a
                .split(|c: char| !c.is_alphanumeric())
                .filter(|w| w.len() >= 2)
                .all(|w| set.contains(w) || set.contains(&crate::stem::stem(w)));
            if !ok {
                return Err(format!("« {} » n'apparaît ni dans la question ni dans une sortie précédente", alt));
            }
        }
    }
    Ok(())
}

/// Identifiants (`S:`, `D:`, `F:`) d'une sortie Cortex, dans l'ordre, sans doublon.
fn identifiants(sortie: &str) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    for tok in sortie.split(|c: char| c.is_whitespace() || c == ',' || c == '(' || c == ')') {
        let t = tok.trim_end_matches([':', '.', ';']);
        if (t.starts_with("S:") || t.starts_with("D:") || t.starts_with("F:")) && t.len() > 2 && !v.iter().any(|x| x == t) {
            v.push(t.to_string());
        }
    }
    v
}

/// Remplace `$k` par le k-ième élément de `liste`.
fn substituer(etape: &str, liste: &[String]) -> Result<String, String> {
    let mut out: Vec<String> = Vec::new();
    for m in etape.split_whitespace() {
        if let Some(k) = m.strip_prefix('$').and_then(|k| k.parse::<usize>().ok()) {
            match liste.get(k.wrapping_sub(1)) {
                Some(x) => out.push(x.clone()),
                None => return Err(format!("{} : la sortie précédente n'en a que {}", m, liste.len())),
            }
        } else {
            out.push(m.to_string());
        }
    }
    Ok(out.join(" "))
}

fn jouer_cortex(handles: &[Handle], t: &Tache) -> Trace {
    let mut tr = Trace::default();
    let mut vu = format!("{} {}", t.question, t.vocabulaire.join(" "));
    let mut precedents: Vec<String> = Vec::new();
    for etape in &t.cortex {
        if let Err(e) = justifiee(etape, &vu) {
            tr.violations.push(format!("{} : {}", etape, e));
            continue;
        }
        let e = match substituer(etape, &precedents) {
            Ok(e) => e,
            Err(e) => {
                tr.violations.push(format!("{} : {}", etape, e));
                continue;
            }
        };
        let Some(appel) = Appel::analyser(&e) else {
            tr.violations.push(format!("{} : outil inconnu", e));
            continue;
        };
        let sortie = outils::executer(handles, &appel, None);
        tr.appels += 1;
        tr.tokens += tokens(&sortie);
        precedents = identifiants(&sortie);
        vu.push('\n');
        vu.push_str(&sortie);
        tr.sorties.push((e, sortie, matches!(appel, Appel::Read { .. })));
    }
    tr
}

/// Bras « ask » : UN appel `ask <question> -b <budget>`, avec les seuls mots de la question.
fn jouer_ask(handles: &[Handle], t: &Tache, budget: usize) -> Trace {
    let mut tr = Trace::default();
    let appel = Appel::Ask { question: t.question.clone(), budget: Some(budget) };
    let sortie = outils::executer(handles, &appel, None);
    tr.appels = 1;
    tr.tokens = tokens(&sortie);
    tr.sorties.push((format!("ask {} -b {}", t.question, budget), sortie, false));
    tr
}

/// Fichiers de l'atlas (chemins relatifs) et racine.
struct Corpus<'a> {
    root: &'a str,
    project: &'a str,
    paths: Vec<&'a str>,
}

fn rg(c: &Corpus, motif: &str, dossier: Option<&str>) -> (String, Vec<String>) {
    let paths: Vec<&str> = match dossier {
        Some(d) => {
            let d = d.trim_end_matches('/');
            c.paths.iter().copied().filter(|p| p.starts_with(d) && p[d.len()..].starts_with('/')).collect()
        }
        None => c.paths.clone(),
    };
    let sets = vec![crate::textsearch::FileSet { project: c.project, root: c.root, paths }];
    let mut hits: Vec<(String, u32, String)> = Vec::new();
    for alt in motif.split('|').filter(|a| !a.is_empty()) {
        for h in crate::textsearch::grep(&sets, alt, false, 1_000_000) {
            hits.push((h.file, h.line, h.text));
        }
    }
    hits.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    hits.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
    let mut compte: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for h in &hits {
        *compte.entry(h.0.as_str()).or_default() += 1;
    }
    let mut classes: Vec<(&str, usize)> = compte.into_iter().collect();
    classes.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let classement: Vec<String> = classes.iter().map(|(p, _)| p.to_string()).collect();
    let mut sortie = String::new();
    for (i, (p, l, t)) in hits.iter().enumerate() {
        if i >= 250 {
            sortie.push_str(&format!("[{} lignes de plus non affichées]\n", hits.len() - 250));
            break;
        }
        let t: String = t.chars().take(500).collect();
        sortie.push_str(&format!("{}:{}:{}\n", p, l, t));
    }
    if hits.is_empty() {
        sortie.push_str("No matches found\n");
    }
    (sortie, classement)
}

fn jouer_sans(c: &Corpus, t: &Tache) -> Trace {
    let mut tr = Trace::default();
    let mut vu = format!("{} {}", t.question, t.vocabulaire.join(" "));
    let mut classement: Vec<String> = Vec::new();
    for etape in &t.sans_cortex {
        if let Err(e) = justifiee(etape, &vu) {
            tr.violations.push(format!("{} : {}", etape, e));
            continue;
        }
        let e = match substituer(etape, &classement) {
            Ok(e) => e,
            Err(e) => {
                tr.violations.push(format!("{} : {}", etape, e));
                continue;
            }
        };
        let mots: Vec<&str> = e.split_whitespace().collect();
        let (sortie, lecture) = match mots.as_slice() {
            ["glob", frag] => {
                let f = frag.to_ascii_lowercase();
                let mut v: Vec<&str> = c.paths.iter().copied().filter(|p| p.to_ascii_lowercase().contains(&f)).collect();
                v.sort();
                let n = v.len();
                classement = v.iter().map(|s| s.to_string()).collect();
                let mut s: String = v.iter().take(100).map(|p| format!("{}\n", p)).collect();
                if n > 100 {
                    s.push_str(&format!("(Results are truncated: {} more files)\n", n - 100));
                }
                (s, false)
            }
            ["rg", motif] | ["rg", motif, _] => {
                let (s, cl) = rg(c, motif, mots.get(2).copied());
                classement = cl;
                (s, false)
            }
            ["rgl", motif] | ["rgl", motif, _] => {
                let (_, cl) = rg(c, motif, mots.get(2).copied());
                let s =
                    if cl.is_empty() { "No files found\n".to_string() } else { cl.iter().take(250).map(|p| format!("{}\n", p)).collect() };
                classement = cl;
                (s, false)
            }
            ["read", chemin] => match std::fs::read(Path::new(c.root).join(chemin)) {
                Ok(b) => {
                    let txt = String::from_utf8_lossy(&b);
                    let mut s = format!("{}\n", chemin);
                    for (i, l) in txt.lines().take(2000).enumerate() {
                        s.push_str(&format!("{:>6}\t{}\n", i + 1, l));
                    }
                    (s, true)
                }
                Err(_) => (format!("File does not exist: {}\n", chemin), true),
            },
            _ => {
                tr.violations.push(format!("{} : outil inconnu", e));
                continue;
            }
        };
        tr.appels += 1;
        tr.tokens += tokens(&sortie);
        vu.push('\n');
        vu.push_str(&sortie);
        tr.sorties.push((e, sortie, lecture));
    }
    tr
}

/// `sym` apparaît comme mot entier dans `bloc`.
fn mot_entier(bloc: &str, sym: &str) -> bool {
    let est_id = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    bloc.match_indices(sym).any(|(i, _)| {
        let avant = bloc[..i].chars().next_back();
        let apres = bloc[i + sym.len()..].chars().next();
        !avant.is_some_and(est_id) && !apres.is_some_and(est_id)
    })
}

/// Faits couverts par une trace.
fn couverts(t: &Tache, tr: &Trace) -> Vec<bool> {
    t.facts
        .iter()
        .map(|f| {
            let (chemin, sym) = match f.split_once('#') {
                Some((p, s)) => (p, Some(s)),
                None => (f.as_str(), None),
            };
            tr.sorties.iter().any(|(_, sortie, lecture)| {
                let blocs: Vec<&str> = if *lecture { vec![sortie.as_str()] } else { sortie.lines().collect() };
                blocs.iter().any(|b| b.contains(chemin) && sym.is_none_or(|s| mot_entier(b, s)))
            })
        })
        .collect()
}

pub fn run(file: &Path, project: Option<String>, verbose: bool, filtre: Option<String>, budget_ask: usize) {
    if est_banc_peek(file) {
        run_peek(file, verbose, filtre);
        return;
    }
    let f: Fichier =
        match std::fs::read_to_string(file).map_err(|e| e.to_string()).and_then(|c| serde_json::from_str(&c).map_err(|e| e.to_string())) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("cortex: tâches illisibles ({}): {}", file.display(), e);
                std::process::exit(1);
            }
        };
    let Some(project) = project.or(f.project.clone()) else {
        eprintln!("cortex: précise le projet (-p) ou le champ \"project\" du fichier de tâches");
        std::process::exit(1);
    };
    // Atlas STABLE : pas de contrôle de fraîcheur (reproductible).
    let handles = crate::require_handles(&Some(project.clone()), true);
    let h = &handles[0];
    let corpus = Corpus { root: h.root(), project: &h.project, paths: h.file_paths() };
    println!("== Banc d'agent — {} ({} tâches) ==", project, f.tasks.len());
    println!(
        "{:<26} {:>16} | {:>6} {:>7} {:>6} | {:>6} {:>7} {:>6} | {:>6} {:>7} {:>6}",
        "tâche", "type", "appels", "tokens", "faits", "appels", "tokens", "faits", "appels", "tokens", "faits"
    );
    println!("{:<26} {:>16} | {:^22} | {:^22} | {:^22}", "", "", "avec Cortex", "sans Cortex", format!("ask -b {}", budget_ask));
    let (mut tc, mut ts, mut ta) = ((0usize, 0usize, 0usize), (0usize, 0usize, 0usize), (0usize, 0usize, 0usize));
    let mut n_faits = 0usize;
    let mut json_taches = Vec::new();
    let mut toutes_violations: Vec<String> = Vec::new();
    for t in f.tasks.iter().filter(|t| filtre.as_deref().is_none_or(|x| t.id.contains(x))) {
        let t0 = std::time::Instant::now();
        let c = jouer_cortex(&handles, t);
        let ms_c = t0.elapsed().as_secs_f64() * 1000.0;
        let t1 = std::time::Instant::now();
        let s = jouer_sans(&corpus, t);
        let ms_s = t1.elapsed().as_secs_f64() * 1000.0;
        let a = jouer_ask(&handles, t, budget_ask);
        let fa = couverts(t, &a);
        let na = fa.iter().filter(|&&x| x).count();
        ta = (ta.0 + a.appels, ta.1 + a.tokens, ta.2 + na);
        let (fc, fs) = (couverts(t, &c), couverts(t, &s));
        let (nc, ns) = (fc.iter().filter(|&&x| x).count(), fs.iter().filter(|&&x| x).count());
        n_faits += t.facts.len();
        tc = (tc.0 + c.appels, tc.1 + c.tokens, tc.2 + nc);
        ts = (ts.0 + s.appels, ts.1 + s.tokens, ts.2 + ns);
        println!(
            "{:<26} {:>16} | {:>6} {:>7} {:>3}/{:<2} | {:>6} {:>7} {:>3}/{:<2} | {:>6} {:>7} {:>3}/{:<2}",
            t.id,
            t.genre,
            c.appels,
            c.tokens,
            nc,
            t.facts.len(),
            s.appels,
            s.tokens,
            ns,
            t.facts.len(),
            a.appels,
            a.tokens,
            na,
            t.facts.len()
        );
        let manques = |v: &[bool]| t.facts.iter().zip(v).filter(|(_, &ok)| !ok).map(|(f, _)| f.clone()).collect::<Vec<_>>();
        if verbose {
            for (e, out, _) in &c.sorties {
                println!("\n--- cortex {} ({} tokens)\n{}", e, tokens(out), out);
            }
            for (e, out, _) in &s.sorties {
                println!("\n--- sans {} ({} tokens, {} lignes)", e, tokens(out), out.lines().count());
            }
            println!("  manqués avec Cortex : {:?}\n  manqués sans Cortex : {:?}\n", manques(&fc), manques(&fs));
            for (e, out, _) in &a.sorties {
                println!(
                    "
--- ask {} ({} tokens)
{}",
                    e,
                    tokens(out),
                    out
                );
            }
            println!(
                "  manqués avec ask : {:?}
",
                manques(&fa)
            );
        }
        for v in c
            .violations
            .iter()
            .map(|v| format!("[{}] cortex : {}", t.id, v))
            .chain(s.violations.iter().map(|v| format!("[{}] sans : {}", t.id, v)))
        {
            toutes_violations.push(v);
        }
        json_taches.push(serde_json::json!({
            "id": t.id, "type": t.genre, "faits": t.facts.len(),
            "cortex": { "appels": c.appels, "tokens": c.tokens, "faits": nc, "manques": manques(&fc), "ms": ms_c, "etapes": c.sorties.iter().map(|x| &x.0).collect::<Vec<_>>(), "violations": c.violations },
            "sans_cortex": { "appels": s.appels, "tokens": s.tokens, "faits": ns, "manques": manques(&fs), "ms": ms_s, "etapes": s.sorties.iter().map(|x| &x.0).collect::<Vec<_>>(), "violations": s.violations },
            "ask": { "appels": a.appels, "tokens": a.tokens, "faits": na, "manques": manques(&fa), "budget": budget_ask },
        }));
    }
    println!(
        "{:<26} {:>16} | {:>6} {:>7} {:>3}/{:<2} | {:>6} {:>7} {:>3}/{:<2} | {:>6} {:>7} {:>3}/{:<2}",
        "TOTAL", "", tc.0, tc.1, tc.2, n_faits, ts.0, ts.1, ts.2, n_faits, ta.0, ta.1, ta.2, n_faits
    );
    println!(
        "\nfaits couverts : {:.0} % avec Cortex, {:.0} % sans ; tokens lus : ×{:.1} de moins avec Cortex",
        100.0 * tc.2 as f64 / n_faits.max(1) as f64,
        100.0 * ts.2 as f64 / n_faits.max(1) as f64,
        ts.1 as f64 / tc.1.max(1) as f64
    );
    println!(
        "ask -b {} : tokens lus {:+.0} % vs find→card→read ({} → {}), appels {} → {}, faits {} → {} sur {}",
        budget_ask,
        100.0 * (ta.1 as f64 - tc.1 as f64) / tc.1.max(1) as f64,
        tc.1,
        ta.1,
        tc.0,
        ta.0,
        tc.2,
        ta.2,
        n_faits
    );
    if !toutes_violations.is_empty() {
        println!("\nétapes NON jouées (règle : rien que la question, son vocabulaire et les sorties précédentes) :");
        for v in &toutes_violations {
            println!("  {}", v);
        }
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (y, m, d) = crate::comparatif::civil((now / 86_400) as i64);
    let commit = crate::comparatif::git_head();
    let json = serde_json::json!({
        "banc": "bench-agent", "project": project, "commit": commit, "date": format!("{y:04}-{m:02}-{d:02}"),
        "total": {
            "faits": n_faits,
            "cortex": { "appels": tc.0, "tokens": tc.1, "faits": tc.2 },
            "sans_cortex": { "appels": ts.0, "tokens": ts.1, "faits": ts.2 },
            "ask": { "appels": ta.0, "tokens": ta.1, "faits": ta.2, "budget": budget_ask },
        },
        "taches": json_taches,
    });
    let dir = crate::bench::results_dir();
    let dir = dir.as_path();
    let _ = std::fs::create_dir_all(dir);
    let out = dir.join(format!("agent-{y:04}-{m:02}-{d:02}-{commit}.json"));
    match std::fs::write(&out, serde_json::to_string_pretty(&json).unwrap_or_default()) {
        Ok(()) => println!("\nrésultats : {}", out.display()),
        Err(e) => eprintln!("cortex: écriture de {} impossible : {e}", out.display()),
    }
}

// ─── Banc « peek » : lire un gros log en entier, ou avec `cortex peek` ───────
//
// `cortex bench-agent --tache peek` rejoue `bench/agent-peek/taches.json` (fichier marqué
// `"banc": "peek"`, reconnu automatiquement ; aucun projet indexé requis). Chaque tâche est
// « pourquoi ce test échoue ? » sur un log FIGÉ (`fichier`, relatif au fichier de tâches) :
// - `facts` : chaînes à retrouver dans la sortie (sous-chaîne exacte ; `ou:a||b` = l'une des deux) ;
// - `cortex` : étapes `peek <fichier> [-e] [-n N] question…` (mêmes règles de vocabulaire que le
//   banc d'agent : la question, son `vocabulaire`, le fichier, les sorties précédentes) ;
// - `sans_cortex` : `read <fichier>` = TOUT lire, par pages de 2 000 lignes (limite d'un Read) :
//   jetons de tout le fichier numéroté, un appel par page.
// Rendu : par tâche et au total, jetons Read complet vs peek, ratio, réussite ; la 1re page
// seule (ce que verrait un Read sans pagination) est donnée à titre indicatif. Seuils (dans le
// fichier) : toute tâche réussie à 100 % ; ratio >= `ratio_gros` sur les tâches de type « gros ».
// Une alerte fait sortir en code 2.

#[derive(Deserialize)]
struct FichierPeek {
    tasks: Vec<TachePeek>,
    #[serde(default)]
    seuils: Seuils,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(default)]
struct Seuils {
    /// Part minimale de tâches réussies (100 = toutes).
    reussite_pct: f64,
    /// Ratio minimal (jetons Read complet / jetons peek) sur les tâches de type « gros ».
    ratio_gros: f64,
}

impl Default for Seuils {
    fn default() -> Self {
        Seuils { reussite_pct: 100.0, ratio_gros: 10.0 }
    }
}

#[derive(Deserialize)]
struct TachePeek {
    id: String,
    #[serde(rename = "type", default)]
    genre: String,
    question: String,
    #[serde(default)]
    vocabulaire: Vec<String>,
    fichier: String,
    facts: Vec<String>,
    cortex: Vec<String>,
    sans_cortex: Vec<String>,
    /// Défaut de peek DÉJÀ constaté (raison) : l'échec reste signalé, mais il est « connu »
    /// (code de sortie 3 au lieu de 2, et toléré par `cargo test` : seule une régression d'une
    /// tâche non marquée le fait échouer). À retirer quand peek est corrigé.
    #[serde(default)]
    connu_ko: Option<String>,
}

/// Lignes d'une page de Read (limite par défaut de l'outil Read).
const PAGE_READ: usize = 2000;

#[derive(Default)]
struct MesurePeek {
    appels: usize,
    tokens: usize,
    faits: Vec<bool>,
    violations: Vec<String>,
    /// (étape, sortie)
    sorties: Vec<(String, String)>,
}

struct ResultatPeek {
    id: String,
    genre: String,
    lignes: usize,
    octets: usize,
    lecture: MesurePeek,
    /// Jetons et faits de la seule 1re page (Read sans pagination).
    page1_tokens: usize,
    page1_faits: Vec<bool>,
    peek: MesurePeek,
    facts: Vec<String>,
    connu_ko: Option<String>,
}

impl ResultatPeek {
    fn ratio(&self) -> f64 {
        self.lecture.tokens as f64 / self.peek.tokens.max(1) as f64
    }
    fn reussi(&self) -> bool {
        self.peek.faits.iter().all(|&b| b)
    }
}

/// Une alerte du banc ; `connu` : l'échec correspond à un `connu_ko` du fichier de tâches.
pub struct Alerte {
    pub texte: String,
    pub connu: bool,
}

pub struct RapportPeek {
    taches: Vec<ResultatPeek>,
    seuils: Seuils,
    pub alertes: Vec<Alerte>,
}

fn est_banc_peek(file: &Path) -> bool {
    std::fs::read_to_string(file)
        .ok()
        .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
        .is_some_and(|v| v.get("banc").and_then(|b| b.as_str()) == Some("peek"))
}

/// Un fait est retrouvé : sous-chaîne exacte, ou `ou:a||b` (l'une des alternatives).
fn fait_trouve(fait: &str, sortie: &str) -> bool {
    match fait.strip_prefix("ou:") {
        Some(alts) => alts.split("||").any(|a| !a.is_empty() && sortie.contains(a)),
        None => sortie.contains(fait),
    }
}

fn faits_dans(facts: &[String], textes: &[&str]) -> Vec<bool> {
    facts.iter().map(|f| textes.iter().any(|t| fait_trouve(f, t))).collect()
}

/// Fichier entier numéroté comme Read (`{:>6}\t{ligne}`), puis sa 1re page.
fn numeroter(txt: &str) -> (String, String, usize) {
    let (mut tout, mut page1, mut n) = (String::new(), String::new(), 0usize);
    for (i, l) in txt.lines().enumerate() {
        let ligne = format!("{:>6}\t{}\n", i + 1, l);
        tout.push_str(&ligne);
        if i < PAGE_READ {
            page1.push_str(&ligne);
        }
        n = i + 1;
    }
    (tout, page1, n)
}

fn vocabulaire_vu(t: &TachePeek) -> String {
    format!("{} {} {}", t.question, t.vocabulaire.join(" "), t.fichier)
}

/// `sans_cortex` : `read <fichier>` lit TOUT le log, page de 2 000 lignes par appel.
fn jouer_lecture(dossier: &Path, t: &TachePeek) -> (MesurePeek, usize, Vec<bool>, usize, usize) {
    let mut m = MesurePeek::default();
    let (mut page1_tokens, mut page1_texte, mut lignes, mut octets) = (0usize, String::new(), 0usize, 0usize);
    let vu = vocabulaire_vu(t);
    for etape in &t.sans_cortex {
        if let Err(e) = justifiee(etape, &vu) {
            m.violations.push(format!("{} : {}", etape, e));
            continue;
        }
        let mots: Vec<&str> = etape.split_whitespace().collect();
        let ["read", chemin] = mots.as_slice() else {
            m.violations.push(format!("{} : outil inconnu (seul `read <fichier>` existe ici)", etape));
            continue;
        };
        let Ok(b) = std::fs::read(dossier.join(chemin)) else {
            m.violations.push(format!("{} : fichier illisible", etape));
            continue;
        };
        let txt = String::from_utf8_lossy(&b);
        let (tout, page1, n) = numeroter(&txt);
        m.appels += n.div_ceil(PAGE_READ).max(1);
        m.tokens += tokens(&tout);
        page1_tokens += tokens(&page1);
        page1_texte.push_str(&page1);
        lignes += n;
        octets += b.len();
        m.sorties.push((etape.clone(), tout));
    }
    let textes: Vec<&str> = m.sorties.iter().map(|(_, s)| s.as_str()).collect();
    m.faits = faits_dans(&t.facts, &textes);
    let page1_faits = faits_dans(&t.facts, &[page1_texte.as_str()]);
    (m, page1_tokens, page1_faits, lignes, octets)
}

/// `cortex` : étapes `peek …`, exécutées dans le processus (aucun atlas).
fn jouer_peek(dossier: &Path, t: &TachePeek) -> MesurePeek {
    let mut m = MesurePeek::default();
    let mut vu = vocabulaire_vu(t);
    for etape in &t.cortex {
        if let Err(e) = justifiee(etape, &vu) {
            m.violations.push(format!("{} : {}", etape, e));
            continue;
        }
        let Some(Appel::Peek { fichier, question, erreurs, max_lignes }) = Appel::analyser(etape) else {
            m.violations.push(format!("{} : seul `peek <fichier> …` existe ici", etape));
            continue;
        };
        let chemin = dossier.join(&fichier).to_string_lossy().to_string();
        let sortie = outils::executer(&[], &Appel::Peek { fichier: chemin, question, erreurs, max_lignes }, None);
        m.appels += 1;
        m.tokens += tokens(&sortie);
        vu.push('\n');
        vu.push_str(&sortie);
        m.sorties.push((etape.clone(), sortie));
    }
    let textes: Vec<&str> = m.sorties.iter().map(|(_, s)| s.as_str()).collect();
    m.faits = faits_dans(&t.facts, &textes);
    m
}

/// Les gros logs ne sont pas versionnés : `gen.mjs` (à côté du fichier de tâches) les regénère.
fn assurer_fixtures(dossier: &Path, taches: &[&TachePeek]) -> Result<(), String> {
    let manque = |d: &Path| taches.iter().filter(|t| !d.join(&t.fichier).is_file()).map(|t| t.fichier.clone()).collect::<Vec<_>>();
    let absents = manque(dossier);
    if absents.is_empty() {
        return Ok(());
    }
    let gen = dossier.join("gen.mjs");
    if gen.is_file() {
        let ok = std::process::Command::new("node").arg(&gen).stdout(std::process::Stdio::null()).status().is_ok_and(|s| s.success());
        if !ok {
            return Err(format!("fixtures absentes ({}) et `node {}` a échoué (Node.js requis)", absents.join(", "), gen.display()));
        }
    }
    let encore = manque(dossier);
    if encore.is_empty() {
        Ok(())
    } else {
        Err(format!("fixtures absentes : {}", encore.join(", ")))
    }
}

pub fn rejouer_peek(file: &Path, filtre: Option<&str>) -> Result<RapportPeek, String> {
    let f: FichierPeek = std::fs::read_to_string(file)
        .map_err(|e| e.to_string())
        .and_then(|c| serde_json::from_str(&c).map_err(|e| e.to_string()))
        .map_err(|e| format!("tâches illisibles ({}) : {}", file.display(), e))?;
    let dossier = file.parent().unwrap_or(Path::new("."));
    let choisies: Vec<&TachePeek> = f.tasks.iter().filter(|t| filtre.is_none_or(|x| x == "peek" || t.id.contains(x))).collect();
    assurer_fixtures(dossier, &choisies)?;
    let mut taches = Vec::new();
    let mut alertes = Vec::new();
    for t in choisies {
        let (lecture, page1_tokens, page1_faits, lignes, octets) = jouer_lecture(dossier, t);
        let peek = jouer_peek(dossier, t);
        let r = ResultatPeek {
            id: t.id.clone(),
            genre: t.genre.clone(),
            lignes,
            octets,
            lecture,
            page1_tokens,
            page1_faits,
            peek,
            facts: t.facts.clone(),
            connu_ko: t.connu_ko.clone(),
        };
        let mut alerte = |texte: String, connu: bool| alertes.push(Alerte { texte, connu });
        for v in r.lecture.violations.iter().chain(r.peek.violations.iter()) {
            alerte(format!("[{}] étape non jouée : {}", r.id, v), false);
        }
        let absents: Vec<&String> = r.facts.iter().zip(&r.lecture.faits).filter(|(_, &ok)| !ok).map(|(f, _)| f).collect();
        if !absents.is_empty() {
            alerte(format!("[{}] tâche invalide : fait absent du log complet {:?}", r.id, absents), false);
        }
        let rates: Vec<&String> = r.facts.iter().zip(&r.peek.faits).filter(|(_, &ok)| !ok).map(|(f, _)| f).collect();
        if !rates.is_empty() {
            alerte(format!("[{}] peek rate : {:?} absent de la sortie", r.id, rates), r.connu_ko.is_some());
        }
        if r.genre == "gros" && r.ratio() < f.seuils.ratio_gros {
            alerte(
                format!("[{}] ratio ×{:.1} < ×{:.0} exigé sur les gros logs", r.id, r.ratio(), f.seuils.ratio_gros),
                r.connu_ko.is_some(),
            );
        }
        taches.push(r);
    }
    let reussies = taches.iter().filter(|r| r.reussi()).count();
    let pct = 100.0 * reussies as f64 / taches.len().max(1) as f64;
    if pct < f.seuils.reussite_pct {
        let connu = taches.iter().filter(|r| !r.reussi()).all(|r| r.connu_ko.is_some());
        alertes.push(Alerte {
            texte: format!("réussite {:.0} % < {:.0} % exigé ({}/{} tâches)", pct, f.seuils.reussite_pct, reussies, taches.len()),
            connu,
        });
    }
    Ok(RapportPeek { taches, seuils: f.seuils, alertes })
}

fn run_peek(file: &Path, verbose: bool, filtre: Option<String>) {
    let rapport = match rejouer_peek(file, filtre.as_deref()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("cortex: {e}");
            std::process::exit(1);
        }
    };
    println!("== Banc d'agent PEEK — « pourquoi ce test échoue ? » ({} tâches) ==", rapport.taches.len());
    println!(
        "Read complet = tout le log numéroté, {} lignes par appel ; peek = `cortex peek <log> <question>` ; jetons = caractères / 4.",
        PAGE_READ
    );
    println!(
        "{:<20} {:>6} {:>8} | {:>6} {:>8} {:>5} | {:>6} {:>6} {:>5} | {:>8} {:>5} | {:>8}",
        "tâche", "type", "lignes", "appels", "tokens", "ok", "appels", "tokens", "faits", "1re page", "ok", "ratio"
    );
    println!("{:<20} {:>6} {:>8} | {:^21} | {:^19} | {:^14} |", "", "", "", "Read complet", "cortex peek", "Read 1re page");
    let (mut lt, mut la, mut pt, mut pa) = (0usize, 0usize, 0usize, 0usize);
    let (mut faits, mut faits_ok, mut reussies, mut lecture_ok, mut page1_ok) = (0usize, 0usize, 0usize, 0usize, 0usize);
    let (mut gros_l, mut gros_p) = (0usize, 0usize);
    let mut json_taches = Vec::new();
    for r in &rapport.taches {
        let nf = r.facts.len();
        let nok = r.peek.faits.iter().filter(|&&b| b).count();
        let lok = r.lecture.faits.iter().all(|&b| b);
        let p1 = r.page1_faits.iter().all(|&b| b);
        println!(
            "{:<20} {:>6} {:>8} | {:>6} {:>8} {:>5} | {:>6} {:>6} {:>2}/{:<2} | {:>8} {:>5} | {:>7.1}×",
            r.id,
            r.genre,
            r.lignes,
            r.lecture.appels,
            r.lecture.tokens,
            if lok { "oui" } else { "NON" },
            r.peek.appels,
            r.peek.tokens,
            nok,
            nf,
            r.page1_tokens,
            if p1 { "oui" } else { "non" },
            r.ratio()
        );
        if verbose {
            for (e, s) in &r.peek.sorties {
                println!("\n--- {} ({} tokens)\n{}", e, tokens(s), s);
            }
        }
        lt += r.lecture.tokens;
        la += r.lecture.appels;
        pt += r.peek.tokens;
        pa += r.peek.appels;
        faits += nf;
        faits_ok += nok;
        reussies += r.reussi() as usize;
        lecture_ok += lok as usize;
        page1_ok += p1 as usize;
        if r.genre == "gros" {
            gros_l += r.lecture.tokens;
            gros_p += r.peek.tokens;
        }
        json_taches.push(serde_json::json!({
            "id": r.id, "type": r.genre, "lignes": r.lignes, "octets": r.octets, "faits": nf,
            "read_complet": { "appels": r.lecture.appels, "tokens": r.lecture.tokens, "reussite": lok,
                              "page1_tokens": r.page1_tokens, "page1_reussite": p1 },
            "peek": { "appels": r.peek.appels, "tokens": r.peek.tokens, "faits": nok, "reussite": r.reussi(),
                      "etapes": r.peek.sorties.iter().map(|x| &x.0).collect::<Vec<_>>() },
            "ratio": r.ratio(),
        }));
    }
    let n = rapport.taches.len();
    println!(
        "{:<20} {:>6} {:>8} | {:>6} {:>8} {:>2}/{:<2} | {:>6} {:>6} {:>2}/{:<2} | {:>8} {:>2}/{:<2} | {:>7.1}×",
        "TOTAL",
        "",
        "",
        la,
        lt,
        lecture_ok,
        n,
        pa,
        pt,
        faits_ok,
        faits,
        "",
        page1_ok,
        n,
        lt as f64 / pt.max(1) as f64
    );
    let ratio_gros = if gros_p > 0 { gros_l as f64 / gros_p as f64 } else { 0.0 };
    println!(
        "\nréussite peek : {}/{} tâches ({} faits sur {}) ; Read complet : {}/{} ; 1re page seule : {}/{}.",
        reussies, n, faits_ok, faits, lecture_ok, n, page1_ok, n
    );
    println!(
        "jetons : ×{:.1} de moins avec peek au total ; ×{:.1} sur les gros logs (seuil ×{:.0}) ; appels {} → {}.",
        lt as f64 / pt.max(1) as f64,
        ratio_gros,
        rapport.seuils.ratio_gros,
        la,
        pa
    );
    if rapport.alertes.is_empty() {
        println!("ALERTES : aucune.");
    } else {
        println!("ALERTES ({}) :", rapport.alertes.len());
        for a in &rapport.alertes {
            println!("  {}{}", a.texte, if a.connu { "  [échec connu]" } else { "" });
        }
        for r in rapport.taches.iter().filter(|r| r.connu_ko.is_some()) {
            println!("  échec connu {} : {}", r.id, r.connu_ko.as_deref().unwrap_or(""));
        }
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (y, m, d) = crate::comparatif::civil((now / 86_400) as i64);
    let commit = crate::comparatif::git_head();
    let json = serde_json::json!({
        "banc": "bench-agent-peek", "commit": commit, "date": format!("{y:04}-{m:02}-{d:02}"),
        "seuils": { "reussite_pct": rapport.seuils.reussite_pct, "ratio_gros": rapport.seuils.ratio_gros },
        "total": {
            "taches": n, "reussies": reussies, "faits": faits, "faits_peek": faits_ok,
            "read_complet": { "appels": la, "tokens": lt }, "peek": { "appels": pa, "tokens": pt },
            "ratio": lt as f64 / pt.max(1) as f64, "ratio_gros": ratio_gros,
        },
        "alertes": rapport.alertes.iter().map(|a| serde_json::json!({ "texte": a.texte, "connu": a.connu })).collect::<Vec<_>>(),
        "taches": json_taches,
    });
    let dir = crate::bench::results_dir();
    let _ = std::fs::create_dir_all(&dir);
    let out = dir.join(format!("agent-peek-{y:04}-{m:02}-{d:02}-{commit}.json"));
    match std::fs::write(&out, serde_json::to_string_pretty(&json).unwrap_or_default()) {
        Ok(()) => println!("\nrésultats : {}", out.display()),
        Err(e) => eprintln!("cortex: écriture de {} impossible : {e}", out.display()),
    }
    // 0 : rien ; 2 : alerte inattendue (régression, tâche invalide) ; 3 : seulement des échecs connus.
    if rapport.alertes.iter().any(|a| !a.connu) {
        std::process::exit(2);
    } else if !rapport.alertes.is_empty() {
        std::process::exit(3);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn faits_peek_alternatives_et_numerotation() {
        assert!(fait_trouve("expected 3", "x expected 3 y"));
        assert!(!fait_trouve("expected 3", "Expected 3"));
        assert!(fait_trouve("ou:abc||def", "un def"));
        assert!(!fait_trouve("ou:abc||", "rien"));
        let (tout, page1, n) = numeroter(&"l\n".repeat(2500));
        assert_eq!(n, 2500);
        assert_eq!(page1.lines().count(), PAGE_READ);
        assert_eq!(tout.lines().count(), 2500);
        assert!(tout.starts_with("     1\tl\n"));
    }

    #[test]
    fn appel_peek_analyse_et_execute_sans_atlas() {
        let Some(Appel::Peek { fichier, question, erreurs, max_lignes }) = Appel::analyser("peek dir/a.log -e -n 20 pourquoi ça échoue ?")
        else {
            panic!("peek non reconnu");
        };
        assert_eq!((fichier.as_str(), question.as_str(), erreurs, max_lignes), ("dir/a.log", "pourquoi ça échoue ?", true, Some(20)));
        let Some(Appel::Peek { fichier, question, erreurs, max_lignes }) = Appel::analyser("peek x.log") else { panic!() };
        assert_eq!((fichier.as_str(), question.as_str(), erreurs, max_lignes), ("x.log", "", false, None));
        assert!(Appel::analyser("peek").is_none());
        // fichier absent : message, pas de panique, aucun atlas requis
        let s = outils::executer(
            &[],
            &Appel::Peek { fichier: "/n/existe/pas.log".into(), question: String::new(), erreurs: false, max_lignes: None },
            None,
        );
        assert!(s.starts_with("(cortex) peek:"), "{s}");
    }

    /// Le banc peek complet sur les fixtures figées (les gros logs sont regénérés par `gen.mjs`
    /// si Node est là, sinon le test est ignoré) : aucune alerte tolérée, hors tâches marquées
    /// `connu_ko` (défauts de peek déjà constatés : leur échec ne fait pas échouer le test).
    #[test]
    fn banc_peek_sans_alerte() {
        let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("bench/agent-peek/taches.json");
        if !file.is_file() {
            return;
        }
        let r = match rejouer_peek(&file, None) {
            Ok(r) => r,
            Err(e) if e.contains("Node.js requis") => return,
            Err(e) => panic!("{e}"),
        };
        let inattendues: Vec<&str> = r.alertes.iter().filter(|a| !a.connu).map(|a| a.texte.as_str()).collect();
        assert!(inattendues.is_empty(), "alertes inattendues : {:#?}", inattendues);
        assert!(r.taches.len() >= 8);
        assert!(r.taches.iter().any(|t| t.genre == "gros"));
    }

    #[test]
    fn regle_des_etapes() {
        assert!(justifiee("find tableau local", "Comment un tableau est-il enregistré en local ?").is_ok());
        assert!(justifiee("card brancherCacheLocal", "question").is_err());
        assert!(justifiee("glob rate-limit", "un plafond Rate Limit").is_ok());
        assert!(justifiee("find reprise envois", "les envois sont-ils repris ?").is_ok());
        assert!(justifiee("rg companyId", "identifiant d'entreprise company").is_err());
        assert!(justifiee("rg a|b src/x", "a b src/x/y.ts").is_ok());
        assert!(justifiee("read $1 -c 3", "").is_ok());
        assert_eq!(
            identifiants("S:src/a.ts#f fn L1\nappelle 2: S:src/b.ts#g, F:src/c.ts (+1)\n"),
            vec!["S:src/a.ts#f", "S:src/b.ts#g", "F:src/c.ts"]
        );
        assert_eq!(substituer("card $2", &["a".into(), "b".into()]).unwrap(), "card b");
        assert!(substituer("card $3", &["a".into()]).is_err());
        assert!(mot_entier("x = safeInternalPath(p)", "safeInternalPath"));
        assert!(!mot_entier("safeInternalPathX", "safeInternalPath"));
    }
}
