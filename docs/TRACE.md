# `cortex trace` — de la pile d'erreur au symbole source

> Statut : livré dans la prochaine version (non publié). CLI `cortex trace`, outil MCP `cortex_trace`. Code : `src/sourcemap.rs`
> (décodeur de cartes de sources), `src/outils/trace.rs` (lecture des piles, résolution, rendu).

Un navigateur, Node ou vitest rapporte une erreur à des positions qui ne sont PAS celles de ton
code : module transformé par Vite, bundle minifié, JavaScript compilé depuis TypeScript.
`cortex trace` prend la pile, suit les cartes de sources jusqu'à la position d'origine, la
rattache à l'atlas et rend, par cadre : l'identifiant stable du symbole (`S:chemin#symbole`) et
ses lignes, le code autour, les appelants et les tests.

```
$ cortex trace pile.txt -p MonApp
trace TypeError: Cannot read properties of undefined (reading 'replace') — 2 frame(s), 2 in project
#1 S:src/lib/billing/invoice-export.ts#toCompactDate L424-431 · 425:24 · toCompactDate ← invoice-export.ts:255:26
       424 | function toCompactDate(raw: string): string {
    >  425 |   const stripped = raw.replace(/-/g, '')
       426 |   if (stripped.length === 8) return stripped
    callers 1: S:src/lib/billing/invoice-export.ts#exportDataFromInvoice L444 · tests: F:…/invoice-date.test.ts (+3)
#2 S:src/lib/billing/invoice-export.ts#exportDataFromInvoice L435-508 · 465:16 · … ← invoice-export.ts:296:20
…
sources: map inline ×2
next : read S:src/lib/billing/invoice-export.ts#toCompactDate
```

La ligne servie au navigateur était 255 ; la ligne du fichier est 425. Sans carte, l'agent lit au
mauvais endroit. C'est exactement ce que le rendu corrige.

## Syntaxe

```
cortex trace [FICHIER|-] [--texte "<pile>"] [--maps <dossier|fichier.map>]... [--hote <h>]...
             [--contexte N] [--hors-ligne] [-p <Projet>] [-b <jetons>]
```

| Entrée | Effet |
|---|---|
| `FICHIER` | pile en texte ou JSON ; `-` ou rien = entrée standard |
| `--texte` | la pile donnée directement |
| `--maps` | dossiers (ou `.map`) des cartes d'un build : `dist/assets/*.js.map` |
| `--hote` | hôte HTTP en plus de `localhost` autorisé à servir des cartes |
| `--hors-ligne` | aucun accès réseau : cartes sur disque seulement |
| `--contexte` | lignes de code autour (2 par défaut) |

MCP : `cortex_trace { pile, maps?, offline?, contexte?, project?, budget? }`.

## Ce qui est compris

Texte brut (une ligne = un cadre ; plusieurs piles dans un même texte sont séparées) :

| Source | Forme reconnue |
|---|---|
| Chrome / Node | `at fn (http://…/a.tsx?t=1:61:5)`, `at async fn (…)`, `at new X (…)`, `at file:///C:/…/a.js:3:9`, eval imbriqué |
| Firefox / Safari | `fn@http://…/a.tsx:10:7`, `@http://…:5:1`, `fn/<@…` |
| vitest | ` ❯ src/lib/x.ts:12:5`, ` ❯ fn src/x.test.ts:12:20`, `FAIL  fichier > describe > it` |
| Rust | `thread 'main' panicked at src/foo.rs:12:5:`, `--> src/foo.rs:12:5` |
| TypeScript | `src/a.ts(12,5): error TS2322` |

JSON (tolérant, noms français ou anglais) : un objet ou un tableau. Sont lus, à n'importe quelle
profondeur, les tableaux de cadres sous `stack`, `frames`, `cadres`, `pile`, `stackFrames`,
`stackTrace`, `callFrames` ; une chaîne `stack` est lue comme du texte. Cadre : `url|file|fichier|
fileName|source|path`, `line|ligne|lineNumber`, `column|colonne|col|columnNumber`,
`function|fonction|functionName|name`. Message : `message|text|description|error`.

**Format de Spectra** (`spectra.console/1`, voir `Spectra/spectra-mcp/docs/FORMATS.md`) : la sortie de
`browser_console_messages`, et les `console_errors` d'un `proof.json`, se passent TELLES QUELLES
(`cat console.json | cortex trace`). Lignes et colonnes à base 1.

Indexation : base 1 par défaut ; base 0 pour `callFrames` du protocole CDP (ou tout cadre qui porte
`scriptId`) ; forçable avec `"indexation": 0` ou `"base0": true` sur l'objet.

## Comment la position est retrouvée

Pour chaque cadre, dans cet ordre :

1. **Fichier local** (`file:///…`, `C:\…`) : carte inline (`//# sourceMappingURL=data:…`), sinon
   fichier désigné, sinon `<fichier>.map` voisin.
2. **URL HTTP** (seulement `localhost`, `127.0.0.1`, `::1`, `*.localhost` et les `--hote`) : le
   serveur de développement Vite sert chaque module AVEC sa carte inline ; sinon `//# sourceMappingURL=x.map`
   relatif, sinon `<url>.map`. Cinq secondes au plus par requête, 25 Mo au plus, aucune redirection.
3. **Cartes d'un build** : `--maps`, puis `dist/`, `build/`, `out/`, `.next/` du projet, par nom
   (`index-ab12.js` → `index-ab12.js.map`). C'est le mode **hors ligne**.
4. **Sans carte** (serveur éteint, build sans cartes) : le chemin de l'URL est rattaché à l'atlas, le
   nom de la fonction de la pile désigne le symbole, la ligne est celle du fichier servi et le
   rendu le dit (`~ line approximate (no map)`).

Positions : colonne `c` d'une pile (base 1) = colonne `c-1` dans la carte (base 0). Le dernier
segment de la ligne dont la colonne ne dépasse pas la position donne l'origine. Cartes indexées
(`sections`) comprises. Un `sources` relatif est résolu contre le dossier du script (Vite en
développement ne donne que le nom du fichier).

Rattachement à l'atlas : chemin exact sous la racine du projet, sinon le plus long suffixe que l'atlas
connaît ; `node_modules`, dépendances pré-empaquetées de Vite (`.vite/deps`), `@vite/client`, `node:`
sont comptés comme bibliothèques (une ligne de synthèse `libs skipped: react-dom ×4`).

## Garde-fous

- Réseau : uniquement l'hôte de la pile s'il est local (ou un `--hote` explicite), jamais de
  redirection, 60 requêtes au plus par appel.
- Le contenu d'un fichier hors projet n'est jamais affiché ; les fichiers `.env*`, `*.pem`, `*.key`
  ne sont jamais lus.
- Si le fichier a changé depuis l'erreur (la carte porte `sourcesContent`), l'extrait montré est le
  code qui a réellement tourné, avec un avertissement.

## Mesures

Voir `docs/BENCHMARKS.md` § « trace » : piles réelles d'AstroQuest (Chrome sur le serveur Vite,
Node sur un bundle esbuild avec carte), rattachement exact au symbole attendu.
