//! Tests de `cortex edit` : atomicité, fins de ligne, garde-fous, annulation, unicité des ancres.

use super::*;
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

static N: AtomicUsize = AtomicUsize::new(0);

struct Env {
    base: PathBuf,
    racine: PathBuf,
}

impl Env {
    fn new(tag: &str) -> Env {
        let base = std::env::temp_dir().join(format!("cortex-edit-{}-{}-{}", tag, std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        let _ = std::fs::remove_dir_all(&base);
        let racine = base.join("proj");
        std::fs::create_dir_all(&racine).unwrap();
        std::fs::create_dir_all(base.join("journal")).unwrap();
        Env { base, racine }
    }
    fn ecrire(&self, rel: &str, contenu: &[u8]) {
        let p = self.racine.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, contenu).unwrap();
    }
    fn lire(&self, rel: &str) -> Vec<u8> {
        std::fs::read(self.racine.join(rel)).unwrap()
    }
    fn texte(&self, rel: &str) -> String {
        String::from_utf8(self.lire(rel)).unwrap()
    }
    fn existe(&self, rel: &str) -> bool {
        self.racine.join(rel).exists()
    }
    fn opts(&self) -> Options {
        Options { essai: false, diff: false, forcer: None, journal: self.base.join("journal"), config_globale: None, projet: None }
    }
    fn lots(&self) -> usize {
        std::fs::read_dir(self.base.join("journal")).map(|r| r.flatten().count()).unwrap_or(0)
    }
    fn run(&self, lot: serde_json::Value) -> Rapport {
        self.run_avec(lot, self.opts(), &SansAtlas)
    }
    fn run_avec(&self, lot: serde_json::Value, opts: Options, res: &dyn Resolveur) -> Rapport {
        let ops = parser(&lot.to_string()).unwrap_or_else(|e| panic!("lot invalide : {e}"));
        appliquer(&self.racine, &ops, &opts, res)
    }
    fn run_texte(&self, txt: &str) -> Rapport {
        let ops = parser(txt).unwrap_or_else(|e| panic!("lot invalide : {e}"));
        appliquer(&self.racine, &ops, &self.opts(), &SansAtlas)
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

struct Faux(HashMap<String, CibleSymbole>);
impl Resolveur for Faux {
    fn symbole(&self, id: &str) -> Result<CibleSymbole, String> {
        self.0.get(id).cloned().ok_or_else(|| format!("symbole « {id} » introuvable"))
    }
    fn appelants(&self, _id: &str) -> (usize, Vec<String>) {
        (3, vec!["S:x.ts#a".into(), "S:y.ts#b".into()])
    }
}

fn faux_foo() -> Faux {
    let mut m = HashMap::new();
    m.insert("S:s.ts#foo".to_string(), CibleSymbole { fichier: "s.ts".into(), debut: 2, fin: 4 });
    Faux(m)
}

const SRC_FOO: &str = "line1\nfunction foo(a) {\n  return a;\n}\nreturn a;\n";

// ─── Remplacements, unicité, atomicité ───────────────────────────────────────

#[test]
fn remplacement_unique_ecrit_et_rend_un_rapport_compact() {
    let e = Env::new("unique");
    e.ecrire("a.txt", b"un deux trois\nquatre\n");
    let r = e.run(json!({"editions":[{"fichier":"a.txt","ancien":"deux","nouveau":"DEUX"}]}));
    assert!(r.ok, "{}", r.texte);
    assert_eq!(e.texte("a.txt"), "un DEUX trois\nquatre\n");
    assert!(r.texte.starts_with("cortex edit "), "{}", r.texte);
    assert!(r.texte.contains("a.txt L1"), "{}", r.texte);
    assert!(r.texte.contains("annuler : cortex edit --annuler "), "{}", r.texte);
    assert!(r.texte.lines().count() <= 4, "rapport trop long : {}", r.texte);
}

#[test]
fn ancre_ambigue_refusee_avec_les_lignes_et_tous_la_leve() {
    let e = Env::new("ambigu");
    e.ecrire("a.txt", b"x\nfoo\ny\nfoo\n");
    let r = e.run(json!([{"fichier":"a.txt","ancien":"foo","nouveau":"bar"}]));
    assert!(!r.ok);
    assert!(r.texte.contains("trouvé 2 fois (L2, L4)"), "{}", r.texte);
    assert_eq!(e.texte("a.txt"), "x\nfoo\ny\nfoo\n");
    let r = e.run(json!([{"fichier":"a.txt","ancien":"foo","nouveau":"bar","tous":true}]));
    assert!(r.ok, "{}", r.texte);
    assert_eq!(e.texte("a.txt"), "x\nbar\ny\nbar\n");
    assert!(r.texte.contains("a.txt L2 L4"), "{}", r.texte);
}

#[test]
fn atomique_une_erreur_et_rien_n_est_ecrit_toutes_les_erreurs_rendues() {
    let e = Env::new("atomique");
    e.ecrire("a.txt", b"alpha\n");
    e.ecrire("b.txt", b"beta\n");
    let r = e.run(json!([
        {"fichier":"a.txt","ancien":"alpha","nouveau":"ALPHA"},
        {"fichier":"b.txt","ancien":"absent","nouveau":"x"},
        {"fichier":"manquant.txt","ancien":"a","nouveau":"b"}
    ]));
    assert!(!r.ok);
    assert!(r.texte.contains("REFUSÉ, aucune écriture (2 erreurs)"), "{}", r.texte);
    assert!(r.texte.contains("[2] b.txt"), "{}", r.texte);
    assert!(r.texte.contains("[3] "), "{}", r.texte);
    assert_eq!(e.texte("a.txt"), "alpha\n", "a.txt ne doit pas être touché");
    assert_eq!(e.texte("b.txt"), "beta\n");
    assert_eq!(e.lots(), 0, "aucun lot journalisé pour un lot refusé");
}

#[test]
fn echec_d_ecriture_remet_les_fichiers_deja_ecrits() {
    let e = Env::new("rollback");
    e.ecrire("a.txt", b"alpha\n");
    e.ecrire("bloc", b"je suis un fichier, pas un dossier");
    let r = e.run(json!([
        {"fichier":"a.txt","ancien":"alpha","nouveau":"ALPHA"},
        {"fichier":"bloc/sous.txt","cree":"x"}
    ]));
    assert!(!r.ok, "{}", r.texte);
    assert!(r.texte.contains("ÉCHEC d'écriture"), "{}", r.texte);
    assert_eq!(e.texte("a.txt"), "alpha\n", "retour arrière : a.txt remis");
    assert_eq!(e.lots(), 0, "le journal d'un lot échoué est retiré");
}

#[test]
fn recouvrement_refuse() {
    let e = Env::new("recouvre");
    e.ecrire("a.txt", b"abcdef\n");
    let r = e.run(json!([
        {"fichier":"a.txt","ancien":"abcd","nouveau":"X"},
        {"fichier":"a.txt","ancien":"cdef","nouveau":"Y"}
    ]));
    assert!(!r.ok);
    assert!(r.texte.contains("recouvre"), "{}", r.texte);
    assert_eq!(e.texte("a.txt"), "abcdef\n");
}

#[test]
fn les_ancres_sont_cherchees_dans_l_original_pas_d_ordre() {
    let e = Env::new("original");
    e.ecrire("a.txt", b"un\ndeux\n");
    // La 2e édition vise du texte qui n'existe que dans le résultat de la 1re : refus net.
    let r = e.run(json!([
        {"fichier":"a.txt","ancien":"un","nouveau":"trois"},
        {"fichier":"a.txt","ancien":"trois","nouveau":"quatre"}
    ]));
    assert!(!r.ok);
    assert_eq!(e.texte("a.txt"), "un\ndeux\n");
}

#[test]
fn essai_n_ecrit_rien_et_ne_journalise_pas() {
    let e = Env::new("essai");
    e.ecrire("a.txt", b"un\n");
    let mut o = e.opts();
    o.essai = true;
    let r = e.run_avec(json!([{"fichier":"a.txt","ancien":"un","nouveau":"deux"},{"fichier":"n.txt","cree":"neuf\n"}]), o, &SansAtlas);
    assert!(r.ok, "{}", r.texte);
    assert!(r.texte.contains("ESSAI") && r.texte.contains("RIEN n'est écrit"), "{}", r.texte);
    assert_eq!(e.texte("a.txt"), "un\n");
    assert!(!e.existe("n.txt"));
    assert_eq!(e.lots(), 0);
}

#[test]
fn ancre_tolerante_recale_l_indentation() {
    let e = Env::new("tolerant");
    e.ecrire("a.rs", b"fn a() {\n    let x = 1;\n    let y = 2;\n}\n");
    let r = e.run(json!([{"fichier":"a.rs","ancien":"let x = 1;\nlet y = 2;","nouveau":"let x = 10;\nlet y = 20;"}]));
    assert!(r.ok, "{}", r.texte);
    assert_eq!(e.texte("a.rs"), "fn a() {\n    let x = 10;\n    let y = 20;\n}\n");
    assert!(r.texte.contains("indentation recalée"), "{}", r.texte);
}

#[test]
fn ancre_introuvable_indique_ou_se_trouve_sa_premiere_ligne() {
    let e = Env::new("indice");
    e.ecrire("a.txt", b"debut\nconst valeur = 42;\nfin\n");
    let r = e.run(json!([{"fichier":"a.txt","ancien":"const valeur = 42;\nautre ligne","nouveau":"x"}]));
    assert!(!r.ok);
    assert!(r.texte.contains("introuvable") && r.texte.contains("L2"), "{}", r.texte);
}

// ─── Insertions, ajout, suppression de lignes, création ──────────────────────

#[test]
fn insertion_en_lignes_apres_et_avant_l_ancre() {
    let e = Env::new("insere");
    e.ecrire("a.ts", b"import a from 'a';\nconst z = 1;\n");
    let r = e.run(json!([
        {"fichier":"a.ts","apres":"import a from 'a';","insere":"import b from 'b';"},
        {"fichier":"a.ts","avant":"const z","insere":"// zone\n// deux"}
    ]));
    assert!(r.ok, "{}", r.texte);
    assert_eq!(e.texte("a.ts"), "import a from 'a';\nimport b from 'b';\n// zone\n// deux\nconst z = 1;\n");
}

#[test]
fn insertion_en_crlf_et_sur_derniere_ligne_sans_retour() {
    let e = Env::new("insere-crlf");
    e.ecrire("a.txt", b"a\r\nb\r\n");
    e.ecrire("b.txt", b"x\ny");
    let r = e.run(json!([
        {"fichier":"a.txt","apres":"a","insere":"A2"},
        {"fichier":"b.txt","apres":"y","insere":"z"}
    ]));
    assert!(r.ok, "{}", r.texte);
    assert_eq!(e.lire("a.txt"), b"a\r\nA2\r\nb\r\n");
    assert_eq!(e.lire("b.txt"), b"x\ny\nz");
}

#[test]
fn ajoute_en_fin_garantit_le_retour_a_la_ligne() {
    let e = Env::new("ajoute");
    e.ecrire("a.txt", b"a");
    e.ecrire("b.txt", b"a\r\n");
    let r = e.run(json!([{"fichier":"a.txt","ajoute":"b"},{"fichier":"b.txt","ajoute":"c\nd"}]));
    assert!(r.ok, "{}", r.texte);
    assert_eq!(e.lire("a.txt"), b"a\nb\n");
    assert_eq!(e.lire("b.txt"), b"a\r\nc\r\nd\r\n");
}

#[test]
fn creation_refuse_un_fichier_existant_sauf_ecrase_et_cree_les_dossiers() {
    let e = Env::new("cree");
    e.ecrire("a.txt", b"ancien\r\nbis\r\n");
    let r = e.run(json!([{"fichier":"a.txt","cree":"neuf\n"}]));
    assert!(!r.ok);
    assert!(r.texte.contains("existe déjà"), "{}", r.texte);
    let r = e.run(json!([{"fichier":"a.txt","cree":"neuf\nbis\n","ecrase":true},{"fichier":"d/e/n.txt","cree":"x\n"}]));
    assert!(r.ok, "{}", r.texte);
    assert_eq!(e.lire("a.txt"), b"neuf\r\nbis\r\n", "un fichier réécrit garde ses fins de ligne");
    assert_eq!(e.lire("d/e/n.txt"), b"x\n");
}

// ─── Fins de ligne, BOM ──────────────────────────────────────────────────────

#[test]
fn crlf_preserve_et_ancre_multiligne_ecrite_en_lf() {
    let e = Env::new("crlf");
    e.ecrire("a.txt", b"a\r\nb\r\nc\r\n");
    let r = e.run(json!([{"fichier":"a.txt","ancien":"b\nc","nouveau":"B\nC\nD"}]));
    assert!(r.ok, "{}", r.texte);
    assert_eq!(e.lire("a.txt"), b"a\r\nB\r\nC\r\nD\r\n");
}

#[test]
fn lf_preserve() {
    let e = Env::new("lf");
    e.ecrire("a.txt", b"a\nb\nc\n");
    let r = e.run(json!([{"fichier":"a.txt","ancien":"b","nouveau":"B\nB2"}]));
    assert!(r.ok, "{}", r.texte);
    assert_eq!(e.lire("a.txt"), b"a\nB\nB2\nc\n");
}

#[test]
fn bom_et_absence_de_retour_final_preserves() {
    let e = Env::new("bom");
    e.ecrire("a.txt", b"\xEF\xBB\xBFx = 1");
    let r = e.run(json!([{"fichier":"a.txt","ancien":"1","nouveau":"2"}]));
    assert!(r.ok, "{}", r.texte);
    assert_eq!(e.lire("a.txt"), b"\xEF\xBB\xBFx = 2");
}

#[test]
fn fins_de_ligne_mixtes_les_lignes_non_touchees_restent_identiques() {
    let e = Env::new("mixte");
    e.ecrire("a.txt", b"a\r\nb\nc\nd\r\n");
    let r = e.run(json!([{"fichier":"a.txt","ancien":"c","nouveau":"C"}]));
    assert!(r.ok, "{}", r.texte);
    assert_eq!(e.lire("a.txt"), b"a\r\nb\nC\nd\r\n");
}

#[test]
fn utf8_accents_et_fichier_non_utf8_ou_binaire_refuses() {
    let e = Env::new("utf8");
    e.ecrire("a.txt", "été\n".as_bytes());
    e.ecrire("latin.txt", b"caf\xE9\n");
    e.ecrire("bin.dat", b"ab\0cd");
    let r = e.run(json!([{"fichier":"a.txt","ancien":"é","nouveau":"è","tous":true}]));
    assert!(r.ok, "{}", r.texte);
    assert_eq!(e.texte("a.txt"), "ètè\n");
    let r = e.run(json!([{"fichier":"latin.txt","ancien":"caf","nouveau":"x"},{"fichier":"bin.dat","ancien":"ab","nouveau":"x"}]));
    assert!(!r.ok);
    assert!(r.texte.contains("UTF-8") && r.texte.contains("binaire"), "{}", r.texte);
}

#[test]
fn utf16_refuse() {
    let e = Env::new("utf16");
    e.ecrire("a.txt", b"\xFF\xFEa\0b\0");
    let r = e.run(json!([{"fichier":"a.txt","ancien":"a","nouveau":"x"}]));
    assert!(!r.ok);
    assert!(r.texte.contains("UTF-16"), "{}", r.texte);
}

// ─── Garde-fous de chemin ────────────────────────────────────────────────────

#[test]
fn chemins_interdits_refus_dur_meme_avec_forcer() {
    let e = Env::new("interdits");
    for rel in [
        ".env",
        ".env.local",
        "sub/.env",
        "sub/.env.",
        ".ENV",
        "prod.env",
        ".secrets/cle.txt",
        ".tauri/signing.key",
        ".git/config",
        "a/.git/hooks/x",
        ".ssh/id",
        "cle.pem",
        "id_rsa",
        ".cortex/edit.toml",
        "nul",
        "a.txt:flux",
    ] {
        let mut o = e.opts();
        o.forcer = Some("test".into());
        let r = e.run_avec(json!([{"fichier":rel,"cree":"x"}]), o, &SansAtlas);
        assert!(!r.ok, "« {rel} » aurait dû être refusé : {}", r.texte);
        assert!(!e.existe(rel.split(':').next().unwrap()) || rel == "nul", "« {rel} » écrit malgré le refus");
    }
}

#[test]
fn hors_racine_refuse() {
    let e = Env::new("horsracine");
    e.ecrire("../dehors.txt", b"secret hors projet");
    for rel in ["../dehors.txt", "a/../../dehors.txt", "..\\dehors.txt"] {
        let r = e.run(json!([{"fichier":rel,"ancien":"secret","nouveau":"x"}]));
        assert!(!r.ok, "{rel} : {}", r.texte);
    }
    let abs = e.base.join("dehors.txt");
    let r = e.run(json!([{"fichier":abs.to_string_lossy(),"ancien":"secret","nouveau":"x"}]));
    assert!(!r.ok, "{}", r.texte);
    assert!(r.texte.contains("hors de la racine"), "{}", r.texte);
    assert_eq!(std::fs::read(&abs).unwrap(), b"secret hors projet");
}

#[test]
fn chemin_absolu_dans_la_racine_accepte() {
    let e = Env::new("absolu");
    e.ecrire("a.txt", b"un\n");
    let abs = std::fs::canonicalize(e.racine.join("a.txt")).unwrap();
    let r = e.run(json!([{"fichier":abs.to_string_lossy(),"ancien":"un","nouveau":"deux"}]));
    assert!(r.ok, "{}", r.texte);
    assert_eq!(e.texte("a.txt"), "deux\n");
}

#[test]
fn liste_interdits_du_projet_souple_contournable_avec_raison_et_journalisee() {
    let e = Env::new("souple");
    e.ecrire(".cortex/edit.toml", b"interdits = [\"gen/**\", \"*.lock\"]\n");
    e.ecrire("gen/a.txt", b"x\n");
    e.ecrire("Cargo.lock", b"v1\n");
    let r = e.run(json!([{"fichier":"gen/a.txt","ancien":"x","nouveau":"y"},{"fichier":"Cargo.lock","ancien":"v1","nouveau":"v2"}]));
    assert!(!r.ok);
    assert!(r.texte.contains("interdits") && r.texte.contains("--forcer"), "{}", r.texte);
    assert_eq!(e.texte("gen/a.txt"), "x\n");
    let mut o = e.opts();
    o.forcer = Some("fichiers générés".into());
    let r = e.run_avec(json!([{"fichier":"gen/a.txt","ancien":"x","nouveau":"y"}]), o, &SansAtlas);
    assert!(r.ok, "{}", r.texte);
    assert!(r.texte.contains("contournés") && r.texte.contains("fichiers générés"), "{}", r.texte);
    assert_eq!(e.texte("gen/a.txt"), "y\n");
    let lot = std::fs::read_dir(e.base.join("journal")).unwrap().flatten().next().unwrap();
    let j = std::fs::read_to_string(lot.path().join("lot.json")).unwrap();
    assert!(j.contains("fichiers générés"), "la raison doit être au journal : {j}");
}

#[test]
fn tailles_maximales_de_la_configuration() {
    let e = Env::new("taille");
    e.ecrire(".cortex/edit.toml", b"taille_max_fichier = 50\nmax_editions = 2\n");
    e.ecrire("gros.txt", &[b'a'; 200]);
    e.ecrire("petit.txt", b"a\n");
    let r = e.run(json!([{"fichier":"gros.txt","ancien":"a","nouveau":"b","tous":true}]));
    assert!(!r.ok);
    assert!(r.texte.contains("taille_max_fichier"), "{}", r.texte);
    let r = e.run(json!([
        {"fichier":"petit.txt","ancien":"a","nouveau":"b"},
        {"fichier":"petit.txt","apres":"a","insere":"c"},
        {"fichier":"petit.txt","ajoute":"d"}
    ]));
    assert!(!r.ok);
    assert!(r.texte.contains("max_editions"), "{}", r.texte);
    // Résultat qui dépasse la taille.
    let r = e.run(json!([{"fichier":"petit.txt","ajoute":"x".repeat(100)}]));
    assert!(!r.ok);
    assert!(r.texte.contains("résultat"), "{}", r.texte);
}

// ─── Syntaxe ─────────────────────────────────────────────────────────────────

#[test]
fn regression_de_syntaxe_refusee_avec_la_ligne_et_forcer_la_leve() {
    let e = Env::new("syntaxe");
    e.ecrire("a.rs", b"fn a() {\n    let x = 1;\n}\n");
    let r = e.run(json!([{"fichier":"a.rs","ancien":"}","nouveau":""}]));
    assert!(!r.ok, "{}", r.texte);
    assert!(r.texte.contains("erreur(s) de syntaxe"), "{}", r.texte);
    assert_eq!(e.texte("a.rs"), "fn a() {\n    let x = 1;\n}\n");
    // Une édition valide passe.
    let r = e.run(json!([{"fichier":"a.rs","ancien":"let x = 1;","nouveau":"let x = 2;"}]));
    assert!(r.ok, "{}", r.texte);
    let mut o = e.opts();
    o.forcer = Some("refactoring en deux temps".into());
    let r = e.run_avec(json!([{"fichier":"a.rs","ancien":"}","nouveau":""}]), o, &SansAtlas);
    assert!(r.ok, "{}", r.texte);
}

#[test]
fn json_casse_refuse_mais_un_json_deja_casse_n_est_pas_bloque() {
    let e = Env::new("json");
    e.ecrire("ok.json", b"{\"a\": 1}\n");
    e.ecrire("deja.json", b"{\"a\": 1,}\n");
    let r = e.run(json!([{"fichier":"ok.json","ancien":"1","nouveau":""}]));
    assert!(!r.ok, "{}", r.texte);
    let r = e.run(json!([{"fichier":"deja.json","ancien":"1","nouveau":"2"}]));
    assert!(r.ok, "{}", r.texte);
}

// ─── Symboles ────────────────────────────────────────────────────────────────

#[test]
fn symbole_remplace_en_entier_et_signale_les_appelants_si_la_signature_change() {
    let e = Env::new("symbole");
    e.ecrire("s.ts", SRC_FOO.as_bytes());
    let r = e.run_avec(json!([{"symbole":"S:s.ts#foo","remplace":"function foo(a, b) {\n  return a + b;\n}"}]), e.opts(), &faux_foo());
    assert!(r.ok, "{}", r.texte);
    assert_eq!(e.texte("s.ts"), "line1\nfunction foo(a, b) {\n  return a + b;\n}\nreturn a;\n");
    assert!(r.texte.contains("signature modifiée") && r.texte.contains("3 appelant"), "{}", r.texte);
}

#[test]
fn symbole_corps_seul_sans_avertissement_et_remplacement_borne_au_symbole() {
    let e = Env::new("symbole-borne");
    e.ecrire("s.ts", SRC_FOO.as_bytes());
    // `return a;` apparaît deux fois dans le fichier : ambigu en portée fichier, unique dans le symbole.
    let r = e.run(json!([{"fichier":"s.ts","ancien":"return a;","nouveau":"x"}]));
    assert!(!r.ok);
    let r = e.run_avec(json!([{"symbole":"S:s.ts#foo","ancien":"return a;","nouveau":"return a * 2;"}]), e.opts(), &faux_foo());
    assert!(r.ok, "{}", r.texte);
    assert_eq!(e.texte("s.ts"), "line1\nfunction foo(a) {\n  return a * 2;\n}\nreturn a;\n");
    let r = e.run_avec(json!([{"symbole":"S:s.ts#foo","remplace":"function foo(a) {\n  return 0;\n}"}]), e.opts(), &faux_foo());
    assert!(r.ok, "{}", r.texte);
    assert!(!r.texte.contains("appelant"), "signature inchangée : pas d'avertissement : {}", r.texte);
}

#[test]
fn symbole_inconnu_sans_atlas() {
    let e = Env::new("symbole-inconnu");
    e.ecrire("s.ts", SRC_FOO.as_bytes());
    let r = e.run(json!([{"symbole":"S:s.ts#foo","remplace":"x"}]));
    assert!(!r.ok);
    assert!(r.texte.contains("aucun projet Cortex"), "{}", r.texte);
}

// ─── Renommage ───────────────────────────────────────────────────────────────

#[test]
fn renomme_mot_entier_dans_le_projet_sans_toucher_aux_secrets() {
    let e = Env::new("renomme");
    e.ecrire("src/a.ts", b"const fooBar = foo + foo2; // foo\n");
    e.ecrire("src/b.ts", b"import {foo} from './a';\r\nfoo();\r\n");
    e.ecrire("src/c.ts", b"const qux = 1;\n");
    e.ecrire(".env", b"foo=1\n");
    e.ecrire("node_modules/m/x.js", b"foo\n");
    let r = e.run(json!([{"renomme":"foo","en":"qux"}]));
    assert!(r.ok, "{}", r.texte);
    assert_eq!(e.texte("src/a.ts"), "const fooBar = qux + foo2; // qux\n");
    assert_eq!(e.lire("src/b.ts"), b"import {qux} from './a';\r\nqux();\r\n");
    assert_eq!(e.texte(".env"), "foo=1\n");
    assert_eq!(e.texte("node_modules/m/x.js"), "foo\n");
    assert!(r.texte.contains("renomme foo→qux : 4 occurrence(s), 2 fichier(s)"), "{}", r.texte);
    assert!(r.texte.contains("collision"), "« qux » existe déjà : {}", r.texte);
}

#[test]
fn renomme_dans_un_prefixe_partiel_et_annulable() {
    let e = Env::new("renomme-dans");
    e.ecrire("src/a.ts", b"foo\n");
    e.ecrire("lib/b.ts", b"foo\n");
    let r = e.run(json!([{"renomme":"foo","en":"bar","dans":"src"}]));
    assert!(r.ok, "{}", r.texte);
    assert_eq!(e.texte("src/a.ts"), "bar\n");
    assert_eq!(e.texte("lib/b.ts"), "foo\n");
    let a = annuler(&r.id.unwrap(), &e.opts());
    assert!(a.ok, "{}", a.texte);
    assert_eq!(e.texte("src/a.ts"), "foo\n");
}

#[test]
fn renomme_partiel_et_aucune_occurrence() {
    let e = Env::new("renomme-partiel");
    e.ecrire("a.txt", b"fooBar foo\n");
    let r = e.run(json!([{"renomme":"foo","en":"x","partiel":true}]));
    assert!(r.ok, "{}", r.texte);
    assert_eq!(e.texte("a.txt"), "xBar x\n");
    let r = e.run(json!([{"renomme":"absent","en":"x"}]));
    assert!(!r.ok);
    assert!(r.texte.contains("aucune occurrence"), "{}", r.texte);
}

// ─── Journal et annulation ───────────────────────────────────────────────────

#[test]
fn annulation_restaure_les_octets_et_supprime_les_fichiers_crees() {
    let e = Env::new("annule");
    e.ecrire("a.txt", b"\xEF\xBB\xBFun\r\ndeux\r\n");
    let r = e.run(json!([{"fichier":"a.txt","ancien":"un","nouveau":"UN"},{"fichier":"neuf/n.txt","cree":"x\n"}]));
    assert!(r.ok, "{}", r.texte);
    let id = r.id.clone().unwrap();
    assert!(e.existe("neuf/n.txt"));
    let a = annuler(&id, &e.opts());
    assert!(a.ok, "{}", a.texte);
    assert_eq!(e.lire("a.txt"), b"\xEF\xBB\xBFun\r\ndeux\r\n", "octets d'avant restitués (BOM + CRLF)");
    assert!(!e.existe("neuf/n.txt"), "le fichier créé est supprimé");
    assert!(!e.existe("neuf"), "le dossier créé aussi");
    let encore = annuler(&id, &e.opts());
    assert!(!encore.ok && encore.texte.contains("déjà annulé"), "{}", encore.texte);
}

#[test]
fn annulation_refusee_si_le_fichier_a_change_depuis_sauf_forcer() {
    let e = Env::new("annule-change");
    e.ecrire("a.txt", b"un\n");
    let r = e.run(json!([{"fichier":"a.txt","ancien":"un","nouveau":"deux"}]));
    let id = r.id.unwrap();
    e.ecrire("a.txt", b"travail posterieur\n");
    let a = annuler(&id, &e.opts());
    assert!(!a.ok);
    assert!(a.texte.contains("a changé depuis le lot"), "{}", a.texte);
    assert_eq!(e.texte("a.txt"), "travail posterieur\n", "rien n'est touché");
    let mut o = e.opts();
    o.forcer = Some("je sais".into());
    let a = annuler(&id, &o);
    assert!(a.ok, "{}", a.texte);
    assert_eq!(e.texte("a.txt"), "un\n");
}

#[test]
fn annuler_dernier_et_liste() {
    let e = Env::new("dernier");
    e.ecrire("a.txt", b"1\n");
    let r1 = e.run(json!([{"fichier":"a.txt","ancien":"1","nouveau":"2"}]));
    assert!(r1.ok);
    std::thread::sleep(std::time::Duration::from_millis(1100)); // ids horodatés à la seconde
    let r2 = e.run(json!([{"fichier":"a.txt","ancien":"2","nouveau":"3"}]));
    assert!(r2.ok);
    let l = liste(&e.opts());
    assert!(l.contains(&r1.id.clone().unwrap()) && l.contains(&r2.id.clone().unwrap()) && l.contains("appliqué"), "{l}");
    let a = annuler("dernier", &e.opts());
    assert!(a.ok, "{}", a.texte);
    assert_eq!(e.texte("a.txt"), "2\n");
    let a = annuler("dernier", &e.opts());
    assert!(a.ok, "{}", a.texte);
    assert_eq!(e.texte("a.txt"), "1\n");
    let a = annuler("dernier", &e.opts());
    assert!(!a.ok);
    let inconnu = annuler("../x", &e.opts());
    assert!(!inconnu.ok);
}

#[test]
fn resultat_identique_n_ecrit_pas() {
    let e = Env::new("identique");
    e.ecrire("a.txt", b"un\n");
    let r = e.run(json!([{"fichier":"a.txt","ancien":"un","nouveau":"un"}]));
    assert!(r.ok, "{}", r.texte);
    assert!(r.texte.contains("rien à écrire") && r.texte.contains("inchangé"), "{}", r.texte);
    assert_eq!(e.lots(), 0, "aucun lot journalisé quand rien n'est écrit");
}

// ─── Analyse des formats ─────────────────────────────────────────────────────

#[test]
fn json_alias_anglais_et_forme_liste() {
    let ops = parser(r#"[{"file":"a","old":"x","new":"y","all":true},{"file":"b","after":"p","insert":"q"},{"path":"c","create":"z"},{"symbol":"S:a#f","replace":"t"},{"rename":"A","to":"B","in":"src"}]"#).unwrap();
    assert_eq!(ops.len(), 5);
    assert!(matches!(&ops[0], Op::Remplacer { tous: true, .. }));
    assert!(matches!(&ops[1], Op::Inserer { apres: true, .. }));
    assert!(matches!(&ops[2], Op::Creer { .. }));
    assert!(matches!(&ops[3], Op::RemplacerSymbole { .. }));
    assert!(matches!(&ops[4], Op::Renommer { mots: true, dans: Some(_), .. }));
}

#[test]
fn json_erreurs_claires() {
    assert!(parser(r#"{"editions":[{"fichier":"a","anciens":"x","nouveau":"y"}]}"#).unwrap_err().contains("unknown field"));
    assert!(parser(r#"[{"fichier":"a","ancien":"x","nouveau":"y","cree":"z"}]"#).unwrap_err().contains("UNE action"));
    assert!(parser(r#"[{"fichier":"a","ancien":"x"}]"#).unwrap_err().contains("`ancien` ET `nouveau`"));
    assert!(parser(r#"[{"fichier":"a","ancien":"","nouveau":"y"}]"#).unwrap_err().contains("`ancien` vide"));
    assert!(parser(r#"[{"fichier":"a","symbole":"S:a#b","ancien":"x","nouveau":"y"}]"#).unwrap_err().contains("pas les deux"));
    assert!(parser("[]").unwrap_err().contains("vide"));
    assert!(parser("{").unwrap_err().contains("JSON invalide"));
}

#[test]
fn texte_sans_echappement_remplacement_insertion_suppression_creation_rename() {
    let ops = parser(
        "# commentaire\n\
         @ src/a.ts\n< const x = \"1\";\n> const x = \"2\";\n\n\
         @ src/b.ts tous\n< foo\n> bar\n\
         @ src/c.ts after\n< import a;\n> import b;\n\
         @ src/d.ts\n< ligne a supprimer\n\
         @ src/e.txt new\n> contenu\n>\n> fin\n\
         @ src/f.txt append\n> pied\n\
         @ \"S:src/g.ts#gros fn\"\n> function g() {}\n\
         @ rename Ancien Nouveau dans src\n",
    )
    .unwrap();
    assert_eq!(ops.len(), 8);
    assert_eq!(
        ops[0],
        Op::Remplacer {
            portee: Portee::Fichier("src/a.ts".into()),
            ancien: "const x = \"1\";".into(),
            nouveau: "const x = \"2\";".into(),
            tous: false,
            lignes: false
        }
    );
    assert!(matches!(&ops[1], Op::Remplacer { tous: true, .. }));
    assert!(matches!(&ops[2], Op::Inserer { apres: true, .. }));
    assert!(matches!(&ops[3], Op::Remplacer { lignes: true, nouveau, .. } if nouveau.is_empty()));
    assert_eq!(ops[4], Op::Creer { fichier: "src/e.txt".into(), contenu: "contenu\n\nfin\n".into(), ecrase: false, eol: None, bom: None });
    assert!(matches!(&ops[5], Op::Ajouter { .. }));
    assert_eq!(ops[6], Op::RemplacerSymbole { id: "S:src/g.ts#gros fn".into(), texte: "function g() {}".into() });
    assert_eq!(ops[7], Op::Renommer { de: "Ancien".into(), vers: "Nouveau".into(), dans: Some("src".into()), mots: true });
}

#[test]
fn texte_erreurs_claires() {
    assert!(parser("@ a.txt\n> x\n< y\n").unwrap_err().contains("`<` après `>`"));
    assert!(parser("n'importe quoi\n").unwrap_err().contains("inattendu"));
    assert!(parser("< x\n").unwrap_err().contains("avant tout `@ cible`"));
    assert!(parser("@ a.txt\n> x\n").unwrap_err().contains("ancre"));
    assert!(parser("@\n< x\n").unwrap_err().contains("sans cible"));
}

#[test]
fn texte_applique_de_bout_en_bout_y_compris_suppression_de_lignes() {
    let e = Env::new("texte");
    e.ecrire("a.txt", b"l1\nl2\nl3\n");
    e.ecrire("b.txt", b"  indent\n    x = \"q\";\n");
    let r = e.run_texte("@ a.txt\n< l2\n\n@ b.txt\n< x = \"q\";\n> x = \"r\";\n@ c.txt new\n> neuf \"avec guillemets\"\n");
    assert!(r.ok, "{}", r.texte);
    assert_eq!(e.texte("a.txt"), "l1\nl3\n");
    assert_eq!(e.texte("b.txt"), "  indent\n    x = \"r\";\n");
    assert_eq!(e.texte("c.txt"), "neuf \"avec guillemets\"\n");
}

#[test]
fn entree_utf16_et_bom_decodees() {
    let mut u16le = vec![0xFF, 0xFE];
    for c in "@ a\n< x\n> y\n".encode_utf16() {
        u16le.extend_from_slice(&c.to_le_bytes());
    }
    assert_eq!(decoder_entree(&u16le).unwrap(), "@ a\n< x\n> y\n");
    assert_eq!(decoder_entree("\u{feff}[]".as_bytes()).unwrap().trim_start_matches('\u{feff}'), "[]");
    assert!(decoder_entree(&[0xC3, 0x28]).is_err());
}

#[test]
fn rapport_detaille_avec_diff() {
    let e = Env::new("diff");
    e.ecrire("a.txt", b"un\ndeux\n");
    let mut o = e.opts();
    o.diff = true;
    let r = e.run_avec(json!([{"fichier":"a.txt","ancien":"deux","nouveau":"DEUX"}]), o, &SansAtlas);
    assert!(r.ok, "{}", r.texte);
    assert!(r.texte.contains("- deux") && r.texte.contains("+ DEUX") && r.texte.contains("[1] a.txt:L2"), "{}", r.texte);
}

#[test]
fn iso_utc_epoque_et_date_connue() {
    assert_eq!(iso_utc(0), "19700101-000000");
    assert_eq!(iso_utc(1_700_000_000), "20231114-221320");
}

#[test]
fn arobase_seul_reprend_la_cible_precedente() {
    let ops = parser("@ a.txt\n< x\n> y\n@\n< p\n> q\n@ b.txt\n< m\n> n\n@ rename A B\n@ c.txt\n< 1\n> 2\n").unwrap();
    assert_eq!(ops.len(), 5);
    let cibles: Vec<String> = ops
        .iter()
        .filter_map(|o| match o {
            Op::Remplacer { portee: Portee::Fichier(f), .. } => Some(f.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(cibles, vec!["a.txt", "a.txt", "b.txt", "c.txt"]);
    assert!(parser("@\n< x\n> y\n").unwrap_err().contains("sans cible"));
}

#[test]
fn avertit_quand_les_lignes_ajoutees_perdent_l_indentation() {
    let e = Env::new("indent-warn");
    e.ecrire("a.rs", b"fn a() {\n    let x = 1;\n}\n");
    let r = e.run(json!([{"fichier":"a.rs","ancien":"let x = 1;","nouveau":"let x = 2;\nlet y = 3;"}]));
    assert!(r.ok, "{}", r.texte);
    assert!(r.texte.contains("sans indentation"), "{}", r.texte);
    let e = Env::new("indent-ok");
    e.ecrire("a.rs", b"fn a() {\n    let x = 1;\n}\n");
    let r = e.run(json!([{"fichier":"a.rs","ancien":"    let x = 1;","nouveau":"    let x = 2;\n    let y = 3;"}]));
    assert!(r.ok && !r.texte.contains("sans indentation"), "{}", r.texte);
    assert_eq!(e.texte("a.rs"), "fn a() {\n    let x = 2;\n    let y = 3;\n}\n");
}

#[test]
fn option_inconnue_sur_la_ligne_arobase_est_une_erreur() {
    let e = parser("@ a.txt tuos\n< x\n> y\n").unwrap_err();
    assert!(e.contains("option « tuos » inconnue"), "{e}");
    assert!(parser("@ a.txt tous\n< x\n> y\n").is_ok());
}
