// Génère, de façon DÉTERMINISTE (PRNG mulberry32 à graine fixe), les gros logs du banc
// `cortex bench-agent --tache peek`. Aucune dépendance : `node bench/agent-peek/gen.mjs`.
// Fichiers écrits dans bench/agent-peek/fixtures/ (ignorés par git, regénérés au besoin) :
//   gros-cargo.log   ~20 000 lignes : cargo test, 2 échecs (panic) noyés dans les tests réussis
//   gros-vitest.log  ~20 000 lignes : vitest verbeux, un échec répété 40 fois + un autre, sorties console
//   vert-vitest.log  ~1 500 lignes  : vitest, tout est vert (leurres : « error » dans des noms de tests)
// Deux exécutions donnent des fichiers identiques octet pour octet (LF, UTF-8 sans BOM).
import fs from 'node:fs';
import path from 'node:path';
import { createHash } from 'node:crypto';

const dossier = path.join(import.meta.dirname, 'fixtures');
fs.mkdirSync(dossier, { recursive: true });

function prng(graine) {
  let a = graine >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}
const choisir = (r, liste) => liste[Math.floor(r() * liste.length)];
const entier = (r, a, b) => a + Math.floor(r() * (b - a + 1));
const hex = (r, n) => Array.from({ length: n }, () => '0123456789abcdef'[Math.floor(r() * 16)]).join('');

const MOTS = ['segment', 'index', 'cache', 'journal', 'parser', 'router', 'codec', 'session', 'queue', 'schema',
  'verrou', 'bloc', 'flux', 'table', 'jeton', 'lecteur', 'ecrivain', 'tampon', 'horloge', 'filtre'];
const VERBES = ['ouvre', 'ferme', 'lit', 'ecrit', 'vide', 'recharge', 'compacte', 'verifie', 'migre', 'fusionne',
  'serialise', 'decode', 'purge', 'reprend', 'borne'];
const COMPLEMENTS = ['sans erreur', 'apres redemarrage', 'avec un tampon plein', 'en parallele', 'sur entree vide',
  'avec un grand fichier', 'apres timeout', 'dans le bon ordre', 'avec accents', 'en mode strict'];

function ecrire(nom, lignes) {
  const contenu = lignes.join('\n') + '\n';
  fs.writeFileSync(path.join(dossier, nom), contenu);
  const h = createHash('sha256').update(contenu).digest('hex').slice(0, 12);
  console.log(`${nom}: ${lignes.length} lignes, ${contenu.length} caracteres, sha256 ${h}`);
}

// ─── gros-cargo.log ──────────────────────────────────────────────────────────
function grosCargo() {
  const r = prng(20261120);
  const L = [];
  const crates = ['proc-macro2', 'unicode-ident', 'quote', 'syn', 'serde', 'serde_derive', 'itoa', 'ryu', 'memchr',
    'aho-corasick', 'regex-syntax', 'bytes', 'tokio', 'futures-core', 'pin-project-lite', 'smallvec', 'once_cell'];
  crates.forEach((c, i) => L.push(`   Compiling ${c} v1.${i}.${(i * 7) % 40}`));
  L.push('   Compiling stockage v0.9.4 (C:\\proj\\stockage)');
  for (let i = 0; i < 60; i++) {
    const f = `src/${choisir(r, MOTS)}/${choisir(r, MOTS)}.rs`;
    L.push(`warning: unused variable: \`tmp${i}\``, `  --> ${f}:${entier(r, 10, 400)}:${entier(r, 5, 30)}`, '   |',
      `${entier(r, 10, 400)} |     let tmp${i} = lire(&tampon)?;`, '   |         ^^^^ help: prefix it with an underscore', '');
  }
  L.push('warning: `stockage` (lib test) generated 60 warnings', '    Finished `test` profile [unoptimized + debuginfo] target(s) in 48.12s',
    '     Running unittests src/lib.rs (target\\debug\\deps\\stockage-9c1d4e0b7a2f5d33.exe)', '');
  const NB = 9400;
  L.push(`running ${NB} tests`);
  const echecs = new Map([[3127, 'segment::verif::tests::checksum_segment_4412'], [7390, 'journal::reprise::tests::reprise_apres_coupure']]);
  const vus = new Set();
  for (let i = 0; i < NB; i++) {
    let nom;
    if (echecs.has(i)) nom = echecs.get(i);
    else {
      do { nom = `${choisir(r, MOTS)}::${choisir(r, MOTS)}::tests::${choisir(r, VERBES)}_${choisir(r, MOTS)}_${choisir(r, COMPLEMENTS).replace(/ /g, '_')}_${i}`; } while (vus.has(nom));
    }
    vus.add(nom);
    L.push(`test ${nom} ... ${echecs.has(i) ? 'FAILED' : 'ok'}`);
    // leurres : noms de tests qui contiennent error / failed / panic mais réussissent
    if (i % 400 === 0) L.push(`test erreur::tests::display_error_${i} ... ok`, `test reseau::tests::retry_after_failed_connection_${i} ... ok`, `test panic::tests::should_panic_on_overflow_${i} - should panic ... ok`);
  }
  L.push('', 'failures:', '');
  L.push('---- segment::verif::tests::checksum_segment_4412 stdout ----', '');
  L.push("thread 'segment::verif::tests::checksum_segment_4412' panicked at src/segment/verif.rs:213:17:");
  L.push('assertion `left == right` failed: checksum du segment 4412 invalide');
  L.push('  left: 0x9f3a1c', ' right: 0x9f3a1d', 'stack backtrace:');
  for (let i = 0; i < 14; i++) L.push(`  ${String(i).padStart(2)}: stockage::segment::frame${i}`, `             at .\\src\\segment\\verif.rs:${100 + i * 7}:9`);
  L.push('note: Some details are omitted, run with `RUST_BACKTRACE=full` for a verbose backtrace.', '');
  L.push('---- journal::reprise::tests::reprise_apres_coupure stdout ----', '');
  L.push("thread 'journal::reprise::tests::reprise_apres_coupure' panicked at src/journal/reprise.rs:88:14:");
  L.push('called `Result::unwrap()` on an `Err` value: Corrompu { offset: 65536, attendu: "FIN", trouve: "\\0\\0\\0" }');
  L.push('stack backtrace:');
  for (let i = 0; i < 10; i++) L.push(`  ${String(i).padStart(2)}: stockage::journal::frame${i}`, `             at .\\src\\journal\\reprise.rs:${60 + i * 5}:9`);
  L.push('', '', 'failures:', '    journal::reprise::tests::reprise_apres_coupure', '    segment::verif::tests::checksum_segment_4412', '');
  L.push(`test result: FAILED. ${NB - 2} passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 41.07s`, '', 'error: test failed, to rerun pass `--lib`');
  // complément pour atteindre ~20 000 lignes : sortie de tests avec --nocapture entre les lignes de test
  return L;
}

// ─── gros-vitest.log ─────────────────────────────────────────────────────────
function grosVitest() {
  const r = prng(77031);
  const L = [''];
  L.push(' RUN  v3.2.4 C:/proj/plateforme', '');
  const NB_FICHIERS = 320;
  let tests = 0;
  const defaillants = [];
  for (let f = 0; f < NB_FICHIERS; f++) {
    const alea = `${choisir(r, MOTS)}-${choisir(r, MOTS)}-${f}`;
    const mod = f === 143 ? 'reessais-bloc-143' : f === 251 ? 'verrou-bail-251' : alea;
    const chemin = `src/lib/${mod}.test.ts`;
    const n = entier(r, 25, 55);
    const lignesF = [];
    for (let t = 0; t < n; t++) {
      tests++;
      lignesF.push(`   ✓ ${mod} > ${choisir(r, VERBES)} ${choisir(r, MOTS)} ${choisir(r, COMPLEMENTS)} ${entier(r, 0, 9)}ms`);
      if (t % 9 === 0) lignesF.push(`stdout | ${chemin} > ${mod} > cas ${t}`, `[dbg] traitement element ${t} jeton=${hex(r, 8)} etat=ok`);
    }
    let echec = 0;
    if (f === 143) { // un échec répété 40 fois (même cause, indices différents)
      for (let k = 0; k < 40; k++) lignesF.push(`   × reessais > tentative ${k} reussit avant le plafond de 5 essais ${entier(r, 1, 4)}ms`);
      echec = 40;
      defaillants.push(f);
    }
    if (f === 251) {
      lignesF.push('   × verrou > libere le verrou apres expiration du bail 12ms');
      echec = 1;
      defaillants.push(f);
    }
    L.push(` ${echec ? '❯' : '✓'} ${chemin} (${n + echec} tests${echec ? ` | ${echec} failed` : ''}) ${entier(r, 5, 400)}ms`);
    L.push(...lignesF);
    tests += echec;
  }
  L.push('', '⎯⎯⎯⎯⎯⎯⎯ Failed Tests 41 ⎯⎯⎯⎯⎯⎯⎯', '');
  const mod143 = 'reessais-bloc-143';
  for (let k = 0; k < 40; k++) {
    L.push(` FAIL  src/lib/${mod143}.test.ts > reessais > tentative ${k} reussit avant le plafond de 5 essais`);
    L.push(`Error: delai depasse apres ${5000 + k} ms en attendant la reponse du serveur de reessais`);
    L.push(' ❯ attendre src/lib/reessais.ts:' + (40 + (k % 3)) + ':11', ` ❯ src/lib/${mod143}.test.ts:${70 + k}:15`, '');
  }
  L.push(' FAIL  src/lib/verrou-bail-251.test.ts > verrou > libere le verrou apres expiration du bail',
    'AssertionError: expected true to be false // Object.is equality', '', '- Expected', '+ Received', '', '- false', '+ true', '',
    ' ❯ src/lib/verrou-bail-251.test.ts:88:34', '     86|   horloge.avancer(31_000);', '     87|   await verrou.expirer();', '     88|   expect(verrou.estPris()).toBe(false);',
    '       |                            ^', '     89| });', '');
  L.push('', ` Test Files  2 failed | ${NB_FICHIERS - 2} passed (${NB_FICHIERS})`, `      Tests  41 failed | ${tests - 41} passed (${tests})`, '   Start at  09:41:07', '   Duration  93.40s');
  return L;
}

// ─── vert-vitest.log ─────────────────────────────────────────────────────────
function vertVitest() {
  const r = prng(4242);
  const L = ['', ' RUN  v3.2.4 C:/proj/vitrine', ''];
  let tests = 0, ignores = 0;
  const NB_FICHIERS = 38;
  for (let f = 0; f < NB_FICHIERS; f++) {
    const mod = `${choisir(r, MOTS)}-${f}`;
    const n = entier(r, 22, 40);
    L.push(` ✓ src/components/${mod}.test.tsx (${n} tests) ${entier(r, 8, 200)}ms`);
    for (let t = 0; t < n; t++) {
      tests++;
      const leurre = t % 11 === 0 ? ['affiche une erreur de validation', 'gere le cas panic de l\'API', 'renvoie un message failed puis reessaie'][t % 3] : `${choisir(r, VERBES)} ${choisir(r, MOTS)} ${choisir(r, COMPLEMENTS)}`;
      if (f % 13 === 5 && t === 3) { ignores++; L.push(`   ↓ ${mod} > ${leurre} [skipped]`); } else L.push(`   ✓ ${mod} > ${leurre} ${entier(r, 0, 12)}ms`);
    }
  }
  L.push('', ` Test Files  ${NB_FICHIERS} passed (${NB_FICHIERS})`, `      Tests  ${tests - ignores} passed | ${ignores} skipped (${tests})`, '   Start at  10:12:44', '   Duration  21.77s', '');
  return L;
}

// Les gros logs sont complétés avec du bruit de console jusqu'à ~20 000 lignes (avant le bilan).
function completer(lignes, cible, indiceInsertion, graine, gabarit) {
  const r = prng(graine);
  const manque = cible - lignes.length;
  if (manque <= 0) return lignes;
  const bruit = [];
  for (let i = 0; i < manque; i++) bruit.push(gabarit(i, r));
  return [...lignes.slice(0, indiceInsertion), ...bruit, ...lignes.slice(indiceInsertion)];
}

let cargo = grosCargo();
// bruit --nocapture : insérer juste avant la ligne vide qui précède « failures: » (après les lignes de test)
const idxFailures = cargo.indexOf('failures:');
cargo = completer(cargo, 20000, idxFailures - 1, 1, (i, r) => `[trace] worker ${i % 8} segment ${entier(r, 1, 9000)} octets=${entier(r, 100, 90000)} crc=${hex(r, 8)} etat=ok`);
ecrire('gros-cargo.log', cargo);

let vit = grosVitest();
const idxFailed = vit.findIndex((l) => l.includes('Failed Tests'));
vit = completer(vit, 20000, idxFailed - 1, 2, (i, r) => `[dbg] file ${i % 17} message ${hex(r, 12)} latence=${entier(r, 1, 900)}ms statut=ok`);
ecrire('gros-vitest.log', vit);

ecrire('vert-vitest.log', vertVitest());
