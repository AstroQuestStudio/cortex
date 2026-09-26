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

pub fn run(file: &Path, project: Option<String>, verbose: bool, filtre: Option<String>) {
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
        "{:<26} {:>16} | {:>6} {:>7} {:>6} | {:>6} {:>7} {:>6}",
        "tâche", "type", "appels", "tokens", "faits", "appels", "tokens", "faits"
    );
    println!("{:<26} {:>16} | {:^22} | {:^22}", "", "", "avec Cortex", "sans Cortex");
    let (mut tc, mut ts) = ((0usize, 0usize, 0usize), (0usize, 0usize, 0usize));
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
        let (fc, fs) = (couverts(t, &c), couverts(t, &s));
        let (nc, ns) = (fc.iter().filter(|&&x| x).count(), fs.iter().filter(|&&x| x).count());
        n_faits += t.facts.len();
        tc = (tc.0 + c.appels, tc.1 + c.tokens, tc.2 + nc);
        ts = (ts.0 + s.appels, ts.1 + s.tokens, ts.2 + ns);
        println!(
            "{:<26} {:>16} | {:>6} {:>7} {:>3}/{:<2} | {:>6} {:>7} {:>3}/{:<2}",
            t.id,
            t.genre,
            c.appels,
            c.tokens,
            nc,
            t.facts.len(),
            s.appels,
            s.tokens,
            ns,
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
        }));
    }
    println!(
        "{:<26} {:>16} | {:>6} {:>7} {:>3}/{:<2} | {:>6} {:>7} {:>3}/{:<2}",
        "TOTAL", "", tc.0, tc.1, tc.2, n_faits, ts.0, ts.1, ts.2, n_faits
    );
    println!(
        "\nfaits couverts : {:.0} % avec Cortex, {:.0} % sans ; tokens lus : ×{:.1} de moins avec Cortex",
        100.0 * tc.2 as f64 / n_faits.max(1) as f64,
        100.0 * ts.2 as f64 / n_faits.max(1) as f64,
        ts.1 as f64 / tc.1.max(1) as f64
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

#[cfg(test)]
mod tests {
    use super::*;

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
