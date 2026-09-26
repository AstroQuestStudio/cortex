//! `cortex bench-latence` : ouverture, requête (médiane/p95 sur le banc de
//! pertinence), `context`, `files`, `grep`, contrôle de fraîcheur, mise à jour
//! d'un fichier par delta (corps modifié / export ajouté) et compaction. Les
//! mises à jour sont mesurées sur une COPIE de l'atlas (jamais le vrai).
//!
//! `--echelle` : chaque poste à 1, 2, 4, 8 et 16 threads (pool rayon de N
//! threads, `par::with_threads` : tout le code parallèle de Cortex s'y
//! exécute), plusieurs passes (médiane, min, max), plus la construction
//! complète (parcours, lecture, analyse, écriture d'un tronc dans un fichier
//! temporaire). Tableau + JSON dans `bench/resultats/`.

use crate::{atlas, bench, index, par, symbol};
use std::path::Path;
use std::time::Instant;

/// Les postes mesurés, dans l'ordre d'affichage : (clé JSON, libellé).
const POSTES: &[(&str, &str)] = &[
    ("ouverture", "ouverture"),
    ("requete_mediane", "requête médiane"),
    ("requete_p95", "requête p95"),
    ("context", "context"),
    ("find", "find (outil, médiane)"),
    ("card", "card (médiane)"),
    ("read", "read (médiane)"),
    ("outline", "outline (médiane)"),
    ("impact", "impact (médiane)"),
    ("impact_max", "impact (pire cas)"),
    ("path", "path (médiane)"),
    ("path_max", "path (pire cas)"),
    ("overview", "overview (médiane)"),
    ("files", "files"),
    ("grep", "grep"),
    ("fraicheur", "fraîcheur (appel CLI)"),
    ("maj_corps", "mise à jour 1 fichier (corps)"),
    ("maj_export", "mise à jour 1 fichier (+export)"),
    ("fusion", "compaction courante (fusion deltas)"),
    ("compaction", "compaction complète (tronc)"),
    ("construction", "construction complète"),
];

const ECHELLE: &[usize] = &[1, 2, 4, 8, 16];

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

pub(crate) fn median_p95(v: &mut [f64]) -> (f64, f64) {
    if v.is_empty() {
        return (0.0, 0.0);
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let med = v[v.len() / 2];
    let p95 = v[(((v.len() as f64) * 0.95) as usize).min(v.len() - 1)];
    (med, p95)
}

/// Copie récursive d'un dossier — isole la mesure sur une COPIE de l'atlas.
fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let dst_path = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &dst_path)?;
        } else {
            std::fs::copy(entry.path(), &dst_path)?;
        }
    }
    Ok(())
}

/// Ce qui ne change pas d'une passe à l'autre : questions, fichier et symbole
/// témoins (choisis sur l'index matérialisé une fois).
struct Setup {
    project: String,
    questions: Vec<String>,
    sample_file: Option<index::FileEntry>,
    sample_sym: Option<String>,
    frag: Option<String>,
    /// Échantillons des outils pour agents : symboles appelés, fichiers,
    /// paires (appelant, appelé de l'appelé) pour `path`, dossiers.
    outils_sym: Vec<String>,
    outils_fic: Vec<String>,
    outils_paires: Vec<(String, String)>,
    outils_dos: Vec<String>,
}

fn setup(project: &str, questions: &Path) -> Setup {
    let questions: Vec<String> = std::fs::read_to_string(questions)
        .ok()
        .and_then(|c| bench::parse(&c).ok())
        .map(|b| b.queries.into_iter().map(|q| q.q).collect())
        .unwrap_or_else(|| vec!["recherche".into()]);
    let h = atlas::Handle::open(project).expect("atlas");
    let idx = h.materialize();
    let fi = idx
        .files
        .iter()
        .position(|f| f.symbols.len() >= 2 && f.refs.calls.len() >= 2)
        .or_else(|| idx.files.iter().position(|f| !f.symbols.is_empty()));
    let sample_file = fi.map(|i| idx.files[i].clone());
    let sample_sym = sample_file.as_ref().map(|f| f.symbols[0].name.clone());
    let frag = sample_file.as_ref().map(|f| {
        let name = f.path.rsplit('/').next().unwrap_or(&f.path);
        name.split('.').next().unwrap_or(name).to_string()
    });
    // Échantillon régulier (1 sur k) des symboles qui ont au moins un appelant.
    let appeles: Vec<u32> = h.live_nodes().filter(|r| r.n.kind == 1).map(|r| r.g).filter(|&g| !h.callers(g).is_empty()).collect();
    let pas = (appeles.len() / 30).max(1);
    let choix: Vec<u32> = appeles.iter().step_by(pas).take(30).copied().collect();
    let outils_sym: Vec<String> = choix.iter().map(|&g| h.node_id(g)).collect();
    // `grep` : un nom de symbole assez long pour être une recherche réaliste.
    let sample_sym = sample_sym.filter(|n| n.len() >= 6).or_else(|| Some("useState".to_string()));
    let mut outils_fic: Vec<String> =
        choix.iter().map(|&g| h.node(g).map(|r| crate::ids::file_id(h.path_of(r.n.owner_file))).unwrap_or_default()).collect();
    outils_fic.dedup();
    // `path` : paires reliées (appelant → appelé de l'appelé) ET paires
    // quelconques (souvent sans chemin : parcours complet dans les deux sens).
    let mut outils_paires: Vec<(String, String)> = choix
        .iter()
        .filter_map(|&g| {
            let c = *h.callers(g).first()?;
            let fin = h.callees(g).first().copied().unwrap_or(g);
            Some((h.node_id(c), h.node_id(fin)))
        })
        .collect();
    let n = choix.len();
    outils_paires.extend((0..n).map(|i| (h.node_id(choix[i]), h.node_id(choix[(i + n / 2) % n.max(1)]))));
    // `impact` : l'échantillon + le symbole le plus appelé du projet (pire cas).
    let mut outils_sym = outils_sym;
    if let Some(&g) = appeles.iter().max_by_key(|&&g| h.callers(g).len()) {
        outils_sym.push(h.node_id(g));
    }
    let mut outils_dos: Vec<String> =
        outils_fic.iter().filter_map(|f| f.trim_start_matches("F:").rsplit_once('/').map(|(d, _)| d.to_string())).collect();
    outils_dos.sort();
    outils_dos.dedup();
    outils_dos.truncate(8);
    Setup { project: project.to_string(), questions, sample_file, sample_sym, frag, outils_sym, outils_fic, outils_paires, outils_dos }
}

/// Une passe de mesure (ms par poste ; `NAN` si non mesurable).
#[derive(Default, Clone)]
struct Pass {
    v: Vec<f64>,
    fresh_first: f64,
    fresh_first_examined: usize,
    fresh_examined: usize,
}

fn one_pass(s: &Setup, with_build: bool) -> Pass {
    let project = s.project.as_str();
    let t = Instant::now();
    let mut handle = atlas::Handle::open(project).expect("atlas");
    let open_ms = ms(t);
    let mut qs: Vec<f64> = s
        .questions
        .iter()
        .map(|q| {
            let t = Instant::now();
            let _ = handle.search(q, 40);
            ms(t)
        })
        .collect();
    let (q_med, q_p95) = median_p95(&mut qs);
    // Overlay git mémorisé avant toute mesure (le premier calcul lance git status).
    let _ = crate::outils::executer(std::slice::from_ref(&handle), &crate::outils::Appel::Changed, None);
    let ctx_ms = match &s.sample_sym {
        Some(n) => {
            let t = Instant::now();
            let _ = crate::outils::executer(std::slice::from_ref(&handle), &crate::outils::Appel::Context { cible: n.clone() }, None);
            ms(t)
        }
        None => f64::NAN,
    };
    // Outils pour agents (sortie complète, budget par défaut), overlay git
    // mémorisé (un premier appel hors mesure le calcule).
    let hs = std::slice::from_ref(&handle);
    let mesure = |appels: Vec<crate::outils::Appel>| -> (f64, f64) {
        let mut v: Vec<f64> = appels
            .iter()
            .map(|a| {
                let t = Instant::now();
                let _ = crate::outils::executer(hs, a, None);
                ms(t)
            })
            .collect();
        let max = v.iter().cloned().fold(f64::NAN, f64::max);
        (median_p95(&mut v).0, max)
    };
    use crate::outils::Appel;
    let (find_ms, _) = mesure(s.questions.iter().map(|q| Appel::Find { question: q.clone() }).collect());
    let (card_ms, _) = mesure(s.outils_sym.iter().map(|x| Appel::Card { cible: x.clone() }).collect());
    let (read_ms, _) = mesure(s.outils_sym.iter().map(|x| Appel::Read { cible: x.clone(), contexte: 0 }).collect());
    let (outline_ms, _) = mesure(s.outils_fic.iter().map(|x| Appel::Outline { cible: x.clone() }).collect());
    let (impact_ms, impact_max) = mesure(s.outils_sym.iter().map(|x| Appel::Impact { cible: x.clone(), profondeur: 3 }).collect());
    let (path_ms, path_max) = mesure(s.outils_paires.iter().map(|(a, b)| Appel::Path { de: a.clone(), vers: b.clone() }).collect());
    let (overview_ms, _) = mesure(s.outils_dos.iter().map(|x| Appel::Overview { dossier: x.clone() }).collect());
    let t = Instant::now();
    let _ = crate::run_files_on(std::slice::from_ref(&handle), s.frag.as_deref().unwrap_or("index"), 80);
    let files_ms = ms(t);
    let t = Instant::now();
    let _ = crate::run_grep_on(std::slice::from_ref(&handle), s.sample_sym.as_deref().unwrap_or("function"), false, 60, 2000);
    let grep_ms = ms(t);
    // Fraîcheur : le premier contrôle peut mettre l'atlas à jour (travail en
    // cours sur le projet) ; le second mesure le coût d'un appel CLI courant.
    let t = Instant::now();
    let f1 = atlas::fresh::refresh(&mut handle);
    let fresh_first = ms(t);
    let t = Instant::now();
    let f2 = atlas::fresh::refresh(&mut handle);
    let fresh_ms = ms(t);

    // Mises à jour et compaction sur une copie isolée de l'atlas.
    let tmp = format!("cortex-bench-latence-tmp-{}", std::process::id());
    let tmp_dir = atlas::atlas_root_for(&tmp);
    let upd = |label: &str, mutate: &dyn Fn(&mut index::FileEntry)| -> f64 {
        let _ = std::fs::remove_dir_all(tmp_dir.parent().unwrap());
        // Copie « chaude » : empreintes revalidées, comme l'atlas réel.
        if copy_dir_all(&atlas::atlas_root_for(project), &tmp_dir).is_err() || atlas::revalidate(&tmp).is_err() {
            return f64::NAN;
        }
        // Première ouverture hors mesure : sous Windows, elle attend l'analyse
        // antivirus des segments tout juste copiés (artefact de la copie).
        let _ = atlas::Handle::open(&tmp).map(|h| h.counts());
        let Some(mut e) = s.sample_file.clone() else { return f64::NAN };
        mutate(&mut e);
        e.hash = format!("{}-{}", e.hash, label);
        let t = Instant::now();
        let r = atlas::incremental::apply(&tmp, vec![atlas::incremental::Change::Upsert(e)], None);
        let d = ms(t);
        if std::env::var("CORTEX_DEBUG_TIMING").is_ok() {
            eprintln!("[timing] mise à jour {} -> {:?} en {:.2}ms", label, r, d);
        }
        d
    };
    let body_ms = upd("corps", &|e| e.symbols[0].doc.push("edite_pour_le_banc".into()));
    let export_ms = upd("export", &|e| {
        let mut sym = e.symbols[0].clone();
        sym.name = "exportAjoutePourLeBanc".into();
        sym.tokens = symbol::tokenize_identifier(&sym.name);
        e.symbols.push(sym);
    });
    // Compaction courante : fusion des deltas de la copie (ceux de l'atlas
    // réel + le delta « export »), puis compaction complète du tronc.
    let t = Instant::now();
    let merge_ms = if atlas::incremental::merge_deltas(&tmp).is_ok() { ms(t) } else { f64::NAN };
    let t = Instant::now();
    let compact_ms = if atlas::compact(&tmp).is_ok() { ms(t) } else { f64::NAN };
    let _ = std::fs::remove_dir_all(tmp_dir.parent().unwrap());
    let build_ms = if with_build { construction(Path::new(handle.root())) } else { f64::NAN };
    Pass {
        v: vec![
            open_ms,
            q_med,
            q_p95,
            ctx_ms,
            find_ms,
            card_ms,
            read_ms,
            outline_ms,
            impact_ms,
            impact_max,
            path_ms,
            path_max,
            overview_ms,
            files_ms,
            grep_ms,
            fresh_ms,
            body_ms,
            export_ms,
            merge_ms,
            compact_ms,
            build_ms,
        ],
        fresh_first,
        fresh_first_examined: f1.examined,
        fresh_examined: f2.examined,
    }
}

/// Construction complète d'un tronc (parcours, lecture, empreintes, analyse,
/// résolution, écriture) dans un fichier TEMPORAIRE, supprimé ensuite.
fn construction(root: &Path) -> f64 {
    let out = std::env::temp_dir().join(format!("cortex-echelle-{}.atlas", std::process::id()));
    let t = Instant::now();
    let ok = index::build_index("cortex-echelle", root)
        .map(|(idx, _)| atlas::segment::write_segment(&out, &atlas::build::build_full(&idx)).is_ok())
        .unwrap_or(false);
    let d = ms(t);
    let _ = std::fs::remove_file(&out);
    if ok {
        d
    } else {
        f64::NAN
    }
}

/// `cortex bench-latence -p <projet>` (une passe, sur le pool par défaut).
pub fn run(project: &str, questions: &Path) {
    println!("== Banc de latence — {} ==", project);
    if let Err(e) = atlas::ensure_and_open(project) {
        eprintln!("cortex: atlas '{}': {}", project, e);
        std::process::exit(1);
    }
    let s = setup(project, questions);
    let p = one_pass(&s, false);
    let h = atlas::Handle::open(project).expect("atlas");
    let (nf, ns, _) = h.counts();
    println!("{} fichiers, {} symboles, {} segment(s), {} threads", nf, ns, h.segment_count(), par::threads());
    println!(
        "outils : {} symboles appelés (échantillon régulier + le plus appelé), {} fichiers, {} paires path (moitié reliées), {} dossiers ; grep « {} »\n",
        s.outils_sym.len(),
        s.outils_fic.len(),
        s.outils_paires.len(),
        s.outils_dos.len(),
        s.sample_sym.as_deref().unwrap_or("")
    );
    println!("{:<34} {:>10}", "", "atlas");
    let cible = |k: &str| match k {
        "ouverture" => "  (cible <5ms)",
        "requete_mediane" | "files" | "find" => "  (cible <20ms)",
        "card" => "  (cible <5ms)",
        "read" | "outline" => "  (cible <10ms)",
        "impact" | "path" => "  (cible <50ms)",
        "fraicheur" | "maj_corps" => "  (cible <30ms)",
        "fusion" | "compaction" => "  (cible <500ms)",
        _ => "",
    };
    for (i, (k, label)) in POSTES.iter().enumerate() {
        if *k == "construction" {
            continue;
        }
        if *k == "fraicheur" {
            println!("{:<34} {:>8.2}ms  ({} chemin(s) relu(s))", "fraîcheur (1er appel)", p.fresh_first, p.fresh_first_examined);
            println!("{:<34} {:>8.2}ms  ({} chemin(s) relu(s)){}", "fraîcheur (appel suivant)", p.v[i], p.fresh_examined, cible(k));
            continue;
        }
        println!("{:<34} {:>8.2}ms{}", label, p.v[i], cible(k));
    }
    println!("\nRéférence v1 figée (index.bin + moteur linéaire, retirés ; mesurée le 26/09/2026 sur");
    println!("AstroQuest) : ouverture 115 ms, requête médiane 102 ms, p95 202 ms, context 93 ms.");
    println!("{} questions du banc de pertinence utilisées. Courbe par threads : --echelle.", s.questions.len());
}

/// Médiane, min, max d'une série (valeurs NAN ignorées).
fn stats(v: &[f64]) -> (f64, f64, f64) {
    let mut x: Vec<f64> = v.iter().copied().filter(|a| a.is_finite()).collect();
    if x.is_empty() {
        return (f64::NAN, f64::NAN, f64::NAN);
    }
    x.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (x[x.len() / 2], x[0], x[x.len() - 1])
}

fn fmt_ms(v: f64) -> String {
    if !v.is_finite() {
        "—".into()
    } else if v >= 1000.0 {
        format!("{:.2} s", v / 1000.0)
    } else if v >= 100.0 {
        format!("{:.0} ms", v)
    } else if v >= 10.0 {
        format!("{:.1} ms", v)
    } else {
        format!("{:.2} ms", v)
    }
}

/// `cortex bench-latence -p <projet> --echelle` : courbe 1/2/4/8/16 threads.
pub fn run_echelle(project: &str, questions: &Path, passes: usize, sans_construction: bool, liste: &[usize]) {
    let echelle: Vec<usize> = if liste.is_empty() { ECHELLE.to_vec() } else { liste.to_vec() };
    if let Err(e) = atlas::ensure_and_open(project) {
        eprintln!("cortex: atlas '{}': {}", project, e);
        std::process::exit(1);
    }
    let passes = passes.max(1);
    let s = setup(project, questions);
    let logical = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0);
    // Mise en route : un premier contrôle de fraîcheur (et le cache disque) hors mesure.
    {
        let mut h = atlas::Handle::open(project).expect("atlas");
        let _ = atlas::fresh::refresh(&mut h);
    }
    let h = atlas::Handle::open(project).expect("atlas");
    let (nf, ns, _) = h.counts();
    println!("== Passage à l'échelle — {} ({} fichiers, {} symboles, {} segment(s)) ==", project, nf, ns, h.segment_count());
    println!("{} passe(s) par nombre de threads ; machine : {} threads logiques.\n", passes, logical);
    drop(h);
    // per_threads[n][poste] = valeurs des passes
    let mut table: Vec<(usize, Vec<Vec<f64>>)> = Vec::new();
    for &n in &echelle {
        let t = Instant::now();
        let mut vals: Vec<Vec<f64>> = vec![Vec::new(); POSTES.len()];
        par::with_threads(n, || {
            for _ in 0..passes {
                let p = one_pass(&s, !sans_construction);
                for (i, v) in p.v.into_iter().enumerate() {
                    vals[i].push(v);
                }
            }
        });
        eprintln!("[cortex] {} thread(s) mesuré(s) en {:.1}s", n, t.elapsed().as_secs_f64());
        table.push((n, vals));
    }
    // Tableau : médiane (min–max) par poste et par nombre de threads.
    print!("{:<32}", "poste (médiane)");
    for &n in &echelle {
        let head = if n == 4 { "4 (PC ordinaire)".to_string() } else { format!("{} thread{}", n, if n > 1 { "s" } else { "" }) };
        print!(" {:>16}", head);
    }
    println!(" {:>8}", "1→8");
    let mut postes_json = serde_json::Map::new();
    for (i, (k, label)) in POSTES.iter().enumerate() {
        if sans_construction && *k == "construction" {
            continue;
        }
        print!("{:<32}", label);
        let mut per = serde_json::Map::new();
        let mut med_1 = f64::NAN;
        let mut med_8 = f64::NAN;
        for (n, vals) in &table {
            let (med, min, max) = stats(&vals[i]);
            print!(" {:>16}", fmt_ms(med));
            if *n == 1 {
                med_1 = med;
            }
            if *n == 8 {
                med_8 = med;
            }
            per.insert(n.to_string(), serde_json::json!({"mediane_ms": med, "min_ms": min, "max_ms": max, "passes": vals[i]}));
        }
        let gain = med_1 / med_8;
        println!(" {:>7}", if gain.is_finite() { format!("×{:.1}", gain) } else { "—".into() });
        postes_json.insert(k.to_string(), serde_json::Value::Object(per));
    }
    println!("\nÉcart (min–max des passes) :");
    for (i, (k, label)) in POSTES.iter().enumerate() {
        if sans_construction && *k == "construction" {
            continue;
        }
        print!("{:<32}", label);
        for (_, vals) in &table {
            let (_, min, max) = stats(&vals[i]);
            print!(" {:>16}", if min.is_finite() { format!("{}–{}", fmt_ms(min).replace(" ms", ""), fmt_ms(max)) } else { "—".into() });
        }
        println!();
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (y, m, d) = crate::comparatif::civil((now / 86_400) as i64);
    let commit = crate::comparatif::git_head();
    let json = serde_json::json!({
        "banc": "bench-latence --echelle",
        "project": project,
        "files": nf,
        "symbols": ns,
        "commit": commit,
        "date": format!("{y:04}-{m:02}-{d:02}"),
        "logical_threads": logical,
        "passes": passes,
        "threads": echelle,
        "note": "médiane, min et max des passes ; requête médiane/p95 = sur les questions du banc public, à chaque passe",
        "postes": postes_json,
    });
    let dir = bench::results_dir();
    let dir = dir.as_path();
    let _ = std::fs::create_dir_all(dir);
    let out = dir.join(format!("echelle-{y:04}-{m:02}-{d:02}-{commit}.json"));
    match std::fs::write(&out, serde_json::to_string_pretty(&json).unwrap_or_default()) {
        Ok(()) => println!("\nrésultats : {}", out.display()),
        Err(e) => eprintln!("cortex: écriture de {} impossible : {e}", out.display()),
    }
}
