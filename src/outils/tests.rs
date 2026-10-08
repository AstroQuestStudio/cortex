//! Tests des outils pour agents (voir `outils`) sur un petit projet écrit sur
//! disque et passé par la vraie extraction (tree-sitter) : identifiants,
//! résolution des entrées, et chaque outil (find, card, outline, read,
//! overview, impact, path, changed).

use super::*;
use crate::atlas::tests::clean;

/// Projet témoin : homonymes dans un fichier, appels résolus par import, un
/// type importé sans être appelé, un test, une doc markdown.
fn projet(tag: &str) -> (String, std::path::PathBuf, Handle) {
    let name = format!("cortex-test-outils-{}", tag);
    clean(&name);
    let dir = std::env::temp_dir().join(format!("cortex-outils-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for d in ["src/lib", "src/app", "docs"] {
        std::fs::create_dir_all(dir.join(d)).unwrap();
    }
    let w = |p: &str, c: &str| std::fs::write(dir.join(p), c).unwrap();
    w(
        "src/lib/util.ts",
        "// Utilitaires de test : formatage et validation.\n\n/** Formate une date en ISO court. */\nexport function formatDate(d: Date): string {\n  return d.toISOString().slice(0, 10)\n}\n\nexport function valider(x: string): boolean {\n  return formatDate(new Date()) !== x\n}\n\nexport class Service {\n  run(): number { return 1 }\n}\n\nexport class Autre {\n  run(): number { return 2 }\n}\n",
    );
    w("src/lib/types.ts", "/** Options de la page. */\nexport interface Options {\n  a: string\n}\n");
    w(
        "src/app/page.tsx",
        "import { valider, formatDate } from '../lib/util'\nimport type { Options } from '../lib/types'\n\nexport function Page(o: Options) {\n  const ok = valider(o.a)\n  return formatDate(new Date())\n}\n",
    );
    w("src/lib/util.test.ts", "import { valider } from './util'\n\ntest('v', () => {\n  valider('x')\n})\n");
    w("docs/guide.md", "# Guide\nIntro du guide.\n## Installation\nLancer la commande.\n## Utilisation\nAppeler formatDate.\n");
    let (idx, tracked) = crate::index::build_index(&name, &dir).unwrap();
    crate::atlas::rebuild_full(&name, &idx, tracked).unwrap();
    let h = Handle::open(&name).unwrap();
    (name, dir, h)
}

fn fin(name: &str, dir: &std::path::Path) {
    clean(name);
    let _ = std::fs::remove_dir_all(dir);
}

fn run(h: &Handle, appel: &str) -> String {
    executer(std::slice::from_ref(h), &Appel::analyser(appel).expect(appel), None)
}

#[test]
fn identifiants_stables_et_homonymes() {
    let (name, dir, h) = projet("ids");
    let o = run(&h, "outline src/lib/util.ts");
    assert!(o.contains("S:src/lib/util.ts#run method L13"), "{}", o);
    assert!(o.contains("S:src/lib/util.ts#run~2 method L17"), "{}", o);
    // Imbrication : la méthode est indentée sous sa classe ; exports marqués.
    assert!(o.contains("\n    S:src/lib/util.ts#run method"), "{}", o);
    assert!(o.contains("S:src/lib/util.ts#Service class L12-14 export"), "{}", o);
    // Chaque forme d'entrée résout vers le même symbole.
    for e in ["S:src/lib/util.ts#run~2", "`S:src/lib/util.ts#run~2`,", "S:src/lib/util.ts#run~2 L17", "src/lib/util.ts:17", "util.ts:17"] {
        let c = run(&h, &format!("card {}", e));
        assert!(c.starts_with("S:src/lib/util.ts#run~2 method L17"), "{} → {}", e, c);
    }
    // Un identifiant rendu par un outil se recopie tel quel dans le suivant.
    let f = run(&h, "find formater une date");
    let id = f.lines().next().unwrap().split_whitespace().next().unwrap().to_string();
    assert_eq!(id, "S:src/lib/util.ts#formatDate");
    assert!(f.ends_with(&format!("next : card {}\n", id)), "{}", f);
    assert!(run(&h, &format!("read {}", id)).contains("4│export function formatDate(d: Date): string {"));
    // Titres markdown : D:chemin#ancre.
    let md = run(&h, "outline docs/guide.md");
    assert!(md.contains("D:docs/guide.md#guide L1 — Intro du guide."), "{}", md);
    assert!(md.contains("    D:docs/guide.md#installation L3"), "{}", md);
    // Introuvable / ambigu : message + suite.
    assert!(run(&h, "card S:src/lib/util.ts#absent").contains("next : find"));
    drop(h);
    fin(&name, &dir);
}

#[test]
fn carte_precompilee_et_partie_entrante() {
    let (name, dir, h) = projet("card");
    let g = h.find_symbol("formatDate").unwrap();
    let stockee = h.node(g).map(|r| r.str(r.n.card).to_string()).unwrap();
    assert_eq!(
        stockee,
        "S:src/lib/util.ts#formatDate fn L4-6\nsig: export function formatDate(d: Date): string\nrole: Formate une date en ISO court.\n"
    );
    let v = h.node(h.find_symbol("valider").unwrap()).map(|r| r.str(r.n.card).to_string()).unwrap();
    assert!(v.contains("calls 1: S:src/lib/util.ts#formatDate"), "{}", v);
    let c = run(&h, "card formatDate");
    assert!(c.contains("called by 2 (2 files, L = call line): S:src/app/page.tsx#Page L6, S:src/lib/util.ts#valider L9"), "{}", c);
    assert!(c.contains("tests: F:src/lib/util.test.ts"), "{}", c);
    assert!(c.ends_with("next : read S:src/lib/util.ts#formatDate\n"), "{}", c);
    // Sans rôle propre : celui du fichier.
    let c = run(&h, "card valider");
    assert!(c.contains("role (file): Utilitaires de test : formatage et validation."), "{}", c);
    // Alias : explain = card ; query = find.
    assert_eq!(run(&h, "explain formatDate"), run(&h, "card formatDate"));
    assert_eq!(run(&h, "query date"), run(&h, "find date"));
    drop(h);
    fin(&name, &dir);
}

#[test]
fn read_lignes_exactes_section_plage_budget() {
    let (name, dir, h) = projet("read");
    let r = run(&h, "read S:src/lib/util.ts#valider");
    assert_eq!(r.lines().next().unwrap(), "S:src/lib/util.ts#valider fn L8-10");
    assert!(r.contains("8│export function valider(x: string): boolean {\n9│  return formatDate(new Date()) !== x\n10│}\n"), "{}", r);
    let r = run(&h, "read S:src/lib/util.ts#valider -c 1");
    assert!(r.starts_with("S:src/lib/util.ts#valider fn L8-10 (lines 7-11)") && r.contains("7│\n"), "{}", r);
    let r = run(&h, "read D:docs/guide.md#installation");
    assert!(r.contains("3│## Installation\n4│Lancer la commande.\n") && !r.contains("5│"), "{}", r);
    let r = run(&h, "read src/app/page.tsx:4-5");
    assert!(r.starts_with("F:src/app/page.tsx L4-5") && r.contains("5│  const ok = valider(o.a)"), "{}", r);
    let r = executer(std::slice::from_ref(&h), &Appel::Read { cible: "src/lib/util.ts".into(), contexte: 0 }, Some(60));
    assert!(r.contains("cut") && r.contains("next : read src/lib/util.ts:"), "{}", r);
    drop(h);
    fin(&name, &dir);
}

#[test]
fn impact_chemin_apercu() {
    let (name, dir, h) = projet("graphe");
    let i = run(&h, "impact formatDate");
    assert!(i.starts_with("impact S:src/lib/util.ts#formatDate fn L4-6\n"), "{}", i);
    assert!(i.contains("depth 1 — 2 (2 files)"), "{}", i);
    assert!(i.contains("  S:src/lib/util.ts#valider L9\n"), "{}", i);
    assert!(i.contains("tests to re-run 1: F:src/lib/util.test.ts"), "{}", i);
    // Un type importé sans être appelé : le fichier importeur est un dépendant.
    let t = run(&h, "impact Options");
    assert!(t.contains("  F:src/app/page.tsx\n"), "{}", t);
    // Impact d'un fichier : ses importeurs.
    let f = run(&h, "impact src/lib/types.ts");
    assert!(f.contains("F:src/app/page.tsx"), "{}", f);
    // Chemin d'appels, puis sens inverse.
    let p = run(&h, "path src/app/page.tsx formatDate");
    assert!(
        p.contains("calls 1 hop(s):\nS:src/app/page.tsx#Page component L4-7\n  → calls at L6 S:src/lib/util.ts#formatDate fn L4-6\n"),
        "{}",
        p
    );
    let p = run(&h, "path formatDate valider");
    assert!(p.starts_with("no path from S:src/lib/util.ts#formatDate to S:src/lib/util.ts#valider; reverse path:"), "{}", p);
    let o = run(&h, "overview src/lib");
    assert!(o.contains("entry points (imported from outside) 2:"), "{}", o);
    assert!(o.contains("used by (folders): src/app (2)\nused by (files) 1: F:src/app/page.tsx\n"), "{}", o);
    assert!(o.contains("F:src/lib/util.ts 6 sym — Utilitaires de test : formatage et validation."), "{}", o);
    drop(h);
    fin(&name, &dir);
}

#[test]
fn analyse_des_appels_et_budget() {
    assert!(matches!(Appel::analyser("read S:a#b -c 3"), Some(Appel::Read { contexte: 3, .. })));
    assert!(matches!(Appel::analyser("impact x -d 2"), Some(Appel::Impact { profondeur: 2, .. })));
    assert!(matches!(Appel::analyser("path a b"), Some(Appel::Path { .. })));
    assert!(Appel::analyser("inconnu x").is_none());
    let mut o = Out::new(10);
    assert!(o.ligne("court"));
    assert!(!o.ligne(&"x".repeat(80)));
    assert!(!o.ligne("court"));
    let s = o.fin(Some("card X".into()));
    assert_eq!(s, "court\n… 2 line(s) cut: budget -b 10 reached\nnext : card X\n");
}

/// `changed` et la marque ✎ : un dépôt git, un commit, puis une modification.
#[test]
fn changed_et_marque_non_commite() {
    let git_ok = std::process::Command::new("git").arg("--version").output().map(|o| o.status.success()).unwrap_or(false);
    if !git_ok {
        return;
    }
    let (name, dir, h) = projet("changed");
    drop(h);
    let g = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap()
    };
    g(&["init", "-q"]);
    g(&["add", "-A"]);
    g(&["commit", "-q", "-m", "init"]);
    let mut h = Handle::open(&name).unwrap();
    assert!(run(&h, "changed").contains("no uncommitted work"));
    // Modifie le corps de `valider`, ajoute un fichier.
    let p = dir.join("src/lib/util.ts");
    let src = std::fs::read_to_string(&p).unwrap().replace("!== x", "=== x");
    std::fs::write(&p, src).unwrap();
    std::fs::write(dir.join("src/lib/neuf.ts"), "export function neuve() { return 1 }\n").unwrap();
    crate::atlas::fresh::refresh(&mut h);
    let h = Handle::open(&name).unwrap();
    let c = run(&h, "changed");
    assert!(c.contains("F:src/lib/util.ts ✎ modified · 1 symbol(s) touched\n  S:src/lib/util.ts#valider fn L8-10 · 1 caller(s) — e.g. S:src/app/page.tsx#Page L5"), "{}", c);
    assert!(c.contains("F:src/lib/neuf.ts ✎ new · 1 symbol(s)"), "{}", c);
    assert!(c.contains("tests : F:src/lib/util.test.ts"), "{}", c);
    assert!(c.ends_with("next : impact S:src/lib/util.ts#valider\n"), "{}", c);
    assert!(run(&h, "card valider").lines().next().unwrap().ends_with(" ✎"));
    assert!(run(&h, "find valider").lines().next().unwrap().contains(" ✎"));
    drop(h);
    fin(&name, &dir);
}

#[test]
fn ask_un_appel_sous_budget() {
    let (name, dir, h) = projet("ask");
    // Impact : la question nomme un symbole et demande ce qui casse.
    let a = run(&h, "ask Qu'est-ce qui casse si je change formatDate ?");
    assert_eq!(a, run(&h, "impact formatDate"), "{}", a);
    // Chemin : deux cibles et « arrive ».
    let p = run(&h, "ask Comment src/app/page.tsx arrive-t-il jusqu'à formatDate ?");
    assert_eq!(p, run(&h, "path src/app/page.tsx formatDate"), "{}", p);
    // Module : un dossier.
    let m = run(&h, "ask Que contient le module src/lib ?");
    assert_eq!(m, run(&h, "overview src/lib"), "{}", m);
    // Expliquer / localiser : définition, rôle, appelants, identifiants stables, `next`.
    let e = run(&h, "ask Comment formater une date ?");
    assert!(e.starts_with("ask explain"), "{}", e);
    assert!(e.contains("S:src/lib/util.ts#formatDate fn L4-6 — Formate une date en ISO court."), "{}", e);
    assert!(e.contains("called by: S:src/app/page.tsx#Page L6"), "{}", e);
    assert!(e.contains("tests: F:src/lib/util.test.ts"), "{}", e);
    assert!(e.ends_with('\n') && e.lines().last().unwrap().starts_with("next : read S:"), "{}", e);
    let l = run(&h, "ask Où est formatDate et qui l'utilise ?");
    assert!(l.starts_with("ask locate"), "{}", l);
    // Le budget `-b` borne la sortie (≈ caractères / 4), avec une marge pour l'en-tête et `next`.
    for b in [60usize, 150, 400] {
        let s = run(&h, &format!("ask Comment formater une date ? -b {}", b));
        let jetons = s.chars().count().div_ceil(4);
        assert!(jetons <= b + 20, "-b {} → {} jetons\n{}", b, jetons, s);
    }
    // Un budget plus large ne rend jamais moins de faits.
    let petit = run(&h, "ask Comment formater une date ? -b 80");
    let grand = run(&h, "ask Comment formater une date ? -b 800");
    assert!(grand.lines().count() >= petit.lines().count(), "{}\n---\n{}", petit, grand);
    // Rien à trouver : message et suite.
    assert!(run(&h, "ask zzqxv").contains("next : grep"));
    drop(h);
    fin(&name, &dir);
}
