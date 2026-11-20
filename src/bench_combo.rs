//! Bancs de `pack` et de `impact --tests` sur l'HISTORIQUE GIT d'un dépôt réel.
//!
//! `bench-pack <taches.json>` : pour chaque tâche (message de commit réécrit en demande),
//! le paquet de contexte à budget fixe contient-il les fichiers que le commit a vraiment
//! touchés ? Rappel = fichiers vérité cités dans la sortie / fichiers vérité. Comparé à
//! `find` et `ask` au MÊME budget. L'historique git utilisé par `pack` s'arrête AVANT le
//! commit de la tâche (pas de fuite de la solution).
//!
//! `bench-tests <tests.json>` : pour chaque commit, les tests que `impact --tests`
//! sélectionne à partir des fichiers source modifiés contiennent-ils les tests que le commit
//! a lui-même modifiés ou ajoutés ? Rappel à 5, 10, 30 et sur toute la liste.
//!
//! Limite assumée : l'atlas est celui d'aujourd'hui (HEAD), pas celui du commit.

use crate::outils::{self, Appel};
use serde_json::Value;
use std::path::Path;

fn lire(file: &Path) -> Vec<Value> {
    let t = std::fs::read_to_string(file).unwrap_or_else(|e| {
        eprintln!("bench: « {} » illisible : {e}", file.display());
        std::process::exit(1)
    });
    serde_json::from_str::<Vec<Value>>(&t).unwrap_or_else(|e| {
        eprintln!("bench: JSON invalide : {e}");
        std::process::exit(1)
    })
}

fn liste_de(v: &Value, k: &str) -> Vec<String> {
    v.get(k).and_then(|x| x.as_array()).map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect()).unwrap_or_default()
}

/// Message d'un commit sans le préfixe `type(scope):`.
fn message_commit(root: &str, sha: &str) -> String {
    let mut c = std::process::Command::new("git");
    c.arg("-C").arg(root).args(["log", "-1", "--format=%s", sha]);
    let s = c.output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    match s.split_once("): ").or_else(|| s.split_once(": ")) {
        Some((pre, rest)) if pre.len() < 40 => rest.to_string(),
        _ => s,
    }
}

fn tokens(s: &str) -> usize {
    s.chars().count() / 4
}

/// Fichiers vérité cités (sous-chaîne du chemin) dans une sortie.
fn cites(sortie: &str, verite: &[String]) -> usize {
    verite.iter().filter(|f| sortie.contains(f.as_str())).count()
}

pub fn bench_pack(file: &Path, project: Option<String>, budget: usize, verbose: bool, echantillon: usize, rapide: bool) {
    let handles = crate::atlas_handles(&project, false);
    if handles.is_empty() {
        eprintln!("bench-pack: no project (use -p)");
        std::process::exit(1);
    }
    let h0 = &handles[0];
    let mut taches = lire(file);
    if echantillon > 0 && taches.len() > echantillon {
        let pas = taches.len() / echantillon;
        taches = taches.into_iter().step_by(pas.max(1)).take(echantillon).collect();
    }
    let (mut tot, mut pack_ok, mut find_ok, mut ask_ok, mut rang20, mut rang10) = (0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
    let (mut tok_pack, mut tok_find, mut tok_ask) = (0usize, 0usize, 0usize);
    let mut macro3 = [0f64; 3];
    let mut n_ok = 0usize;
    let (mut rap_pack, mut rap_find, mut rap_ask) = (0f64, 0f64, 0f64);
    if !rapide {
        println!("{:<5} {:>5} {:>7} {:>7} {:>7}   tâche", "id", "vrai", "pack", "find", "ask");
    }
    for (i, t) in taches.iter().enumerate() {
        let commit = t.get("commit").and_then(|x| x.as_str()).unwrap_or("");
        let mut verite = liste_de(t, "fichiers");
        if verite.is_empty() {
            verite = liste_de(t, "sources");
        }
        verite.retain(|f| h0.file_by_path(f).is_some());
        if verite.is_empty() || commit.is_empty() {
            continue;
        }
        let tache = t.get("tache").and_then(|x| x.as_str()).map(String::from).unwrap_or_else(|| message_commit(h0.root(), commit));
        let id = t.get("id").and_then(|x| x.as_str()).map(String::from).unwrap_or_else(|| format!("c{:02}", i));
        if rapide {
            // Classement seul : rappel macro à 10, 20 et 40 fichiers (réglage des poids, sans rendu).
            let classes = outils::pack::fichiers_classes(h0, &tache, Some(commit), outils::pack::Poids::defaut());
            let r = |k: usize| verite.iter().filter(|f| classes.iter().take(k).any(|c| c == *f)).count() as f64 / verite.len() as f64;
            macro3[0] += r(10);
            macro3[1] += r(20);
            macro3[2] += r(40);
            n_ok += 1;
            continue;
        }
        let p = outils::pack::Params { tache: tache.clone(), budget, avant: Some(commit.to_string()) };
        let s_pack = outils::executer(&handles, &Appel::Pack(p.clone()), Some(budget));
        let s_find = outils::executer(&handles, &Appel::Find { question: tache.clone() }, Some(budget));
        let s_ask = outils::executer(&handles, &Appel::Ask { question: tache.clone(), budget: Some(budget) }, Some(budget));
        let (a, b, c) = (cites(&s_pack, &verite), cites(&s_find, &verite), cites(&s_ask, &verite));
        let n = verite.len();
        tot += n;
        pack_ok += a;
        find_ok += b;
        ask_ok += c;
        rap_pack += a as f64 / n as f64;
        rap_find += b as f64 / n as f64;
        rap_ask += c as f64 / n as f64;
        tok_pack += tokens(&s_pack);
        tok_find += tokens(&s_find);
        tok_ask += tokens(&s_ask);
        let classes = outils::pack::fichiers_classes(h0, &tache, Some(commit), outils::pack::Poids::defaut());
        rang10 += verite.iter().filter(|f| classes.iter().take(10).any(|c| c == *f)).count();
        rang20 += verite.iter().filter(|f| classes.iter().take(20).any(|c| c == *f)).count();
        println!(
            "{:<5} {:>5} {:>3}/{:<3} {:>3}/{:<3} {:>3}/{:<3}   {}",
            id,
            n,
            a,
            n,
            b,
            n,
            c,
            n,
            tache.chars().take(70).collect::<String>()
        );
        if verbose {
            let manques: Vec<&String> = verite.iter().filter(|f| !s_pack.contains(f.as_str())).collect();
            println!("      manqués par pack : {:?}", manques);
        }
    }
    if rapide {
        let n = n_ok.max(1) as f64;
        println!(
            "classement seul, {} tâches : rappel moyen @10 {:.1} %  @20 {:.1} %  @40 {:.1} %",
            n_ok,
            100.0 * macro3[0] / n,
            100.0 * macro3[1] / n,
            100.0 * macro3[2] / n
        );
        return;
    }
    let nt = taches.len().max(1) as f64;
    println!("---");
    println!("budget {} jetons · {} tâches · {} fichiers vérité", budget, taches.len(), tot);
    println!(
        "rappel fichiers (micro)  pack {:.1} %  ask {:.1} %  find {:.1} %",
        100.0 * pack_ok as f64 / tot.max(1) as f64,
        100.0 * ask_ok as f64 / tot.max(1) as f64,
        100.0 * find_ok as f64 / tot.max(1) as f64
    );
    println!(
        "rappel moyen par tâche   pack {:.1} %  ask {:.1} %  find {:.1} %",
        100.0 * rap_pack / nt,
        100.0 * rap_ask / nt,
        100.0 * rap_find / nt
    );
    println!(
        "jetons moyens            pack {}  ask {}  find {}",
        tok_pack / taches.len().max(1),
        tok_ask / taches.len().max(1),
        tok_find / taches.len().max(1)
    );
    println!(
        "classement seul (sans rendu) : rappel@10 {:.1} %  rappel@20 {:.1} %",
        100.0 * rang10 as f64 / tot.max(1) as f64,
        100.0 * rang20 as f64 / tot.max(1) as f64
    );
}

pub fn bench_tests(file: &Path, project: Option<String>, echantillon: usize, verbose: bool) {
    let handles = crate::atlas_handles(&project, false);
    if handles.is_empty() {
        eprintln!("bench-tests: no project (use -p)");
        std::process::exit(1);
    }
    let mut commits = lire(file);
    if echantillon > 0 && commits.len() > echantillon {
        let pas = commits.len() / echantillon;
        commits = commits.into_iter().step_by(pas.max(1)).take(echantillon).collect();
    }
    let paliers = [5usize, 10, 30, usize::MAX];
    let mut ok = [0usize; 4];
    let mut complets = [0usize; 4];
    let (mut vrais, mut n_commits, mut taille) = (0usize, 0usize, 0usize);
    let mut nom_seul = 0usize;
    for c in &commits {
        let sources = liste_de(c, "sources");
        let mut verite = liste_de(c, "tests");
        verite.retain(|t| handles.iter().any(|h| h.file_by_path(t).is_some()));
        if sources.is_empty() || verite.is_empty() {
            continue;
        }
        let classes = outils::selection::classement_tests(&handles, &sources);
        n_commits += 1;
        vrais += verite.len();
        taille += classes.len();
        for (k, &pal) in paliers.iter().enumerate() {
            let n = verite.iter().filter(|t| classes.iter().take(pal).any(|x| x == *t)).count();
            ok[k] += n;
            if n == verite.len() {
                complets[k] += 1;
            }
        }
        // Référence naïve : seulement le test de même nom que le fichier.
        let par_nom: Vec<String> = handles
            .iter()
            .flat_map(|h| {
                sources
                    .iter()
                    .filter_map(|s| {
                        h.file_by_path(s)
                            .map(|_| outils::tests_par_nom(h, s, None).into_iter().map(|g| h.path_of(g).to_string()).collect::<Vec<_>>())
                    })
                    .flatten()
                    .collect::<Vec<_>>()
            })
            .collect();
        nom_seul += verite.iter().filter(|t| par_nom.contains(t)).count();
        if verbose {
            let m: Vec<&String> = verite.iter().filter(|t| !classes.contains(t)).collect();
            println!(
                "{} : {} vrais, {} sélectionnés, hors liste : {:?}",
                c.get("commit").and_then(|x| x.as_str()).unwrap_or("?"),
                verite.len(),
                classes.len(),
                m
            );
        }
    }
    println!("{} commits · {} tests modifiés par ces commits · liste moyenne {} tests", n_commits, vrais, taille / n_commits.max(1));
    for (k, &pal) in paliers.iter().enumerate() {
        println!(
            "top {:>5} : rappel {:>5.1} %  ({} commits sur {} avec TOUS les tests)",
            if pal == usize::MAX { "tous".to_string() } else { pal.to_string() },
            100.0 * ok[k] as f64 / vrais.max(1) as f64,
            complets[k],
            n_commits
        );
    }
    println!("référence « test de même nom » seul : rappel {:.1} %", 100.0 * nom_seul as f64 / vrais.max(1) as f64);
}
