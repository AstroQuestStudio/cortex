<div align="center">

# Cortex

**Le moteur de contexte code pour agents IA.**<br>
find, card, read, impact : un agent comprend une base de code en quelques appels et avec ~10 fois
moins de tokens que grep + lecture de fichiers. Local, instantané, open source.

[![CI](https://github.com/AstroQuestStudio/cortex/actions/workflows/ci.yml/badge.svg)](https://github.com/AstroQuestStudio/cortex/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/AstroQuestStudio/cortex?sort=semver)](https://github.com/AstroQuestStudio/cortex/releases/latest)
[![Téléchargements](https://img.shields.io/github/downloads/AstroQuestStudio/cortex/total?label=t%C3%A9l%C3%A9chargements)](https://github.com/AstroQuestStudio/cortex/releases)
[![Licence : MIT](https://img.shields.io/badge/licence-MIT-blue.svg)](LICENSE)
[![Serveur MCP](https://img.shields.io/badge/MCP-serveur-8A2BE2)](#brancher-son-agent)

**par [AstroQuest](https://astroquest.fr)** · [astroquest.fr/cortex](https://astroquest.fr/cortex) · [English](README.md) · [Spécification](docs/SPEC.md) · [Bancs](docs/BENCHMARKS.md) · [Architecture](docs/ARCHITECTURE.md)

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/demo-dark.svg">
  <img src="docs/assets/demo-light.svg" width="900" alt="Une session Cortex sur hono : find trouve parseFormData, card montre sa signature, ses appelants et ses tests, read affiche la fonction, impact liste 10 dépendants et 4 tests à relancer. Environ 1 070 tokens, contre environ 9 000 avec grep et lecture de fichiers.">
</picture>

<sub>Une vraie session sur <a href="https://github.com/honojs/hono">hono</a> (421 fichiers), rejouée à partir des sorties de Cortex 0.3.0. Transcription complète <a href="#une-vraie-session">plus bas</a>.</sub>

</div>

---

Les agents de code dépensent l'essentiel de leurs tokens à *chercher* : grep, ouvrir un fichier,
grep encore, en ouvrir un autre. Cortex indexe le dépôt une fois (quelques secondes pour
10 000 fichiers), tient l'index à jour à chaque appel, et répond aux questions que se posent
vraiment les agents — *où est X, c'est quoi, qui l'appelle, qu'est-ce qui casse si je le change* —
par des sorties courtes et enchaînables.

## Installation

**Linux, macOS**

```sh
curl -fsSL https://raw.githubusercontent.com/AstroQuestStudio/cortex/main/install.sh | sh
```

**Windows (PowerShell)**

```powershell
irm https://raw.githubusercontent.com/AstroQuestStudio/cortex/main/install.ps1 | iex
```

Les deux scripts téléchargent le binaire de votre plateforme depuis la
[dernière release](https://github.com/AstroQuestStudio/cortex/releases/latest), **refusent de
l'installer si son SHA-256 ne correspond pas à `SHA256SUMS.txt`**, et ne demandent jamais de
droits administrateur : `~/.local/bin` sous Linux et macOS (le script signale si ce dossier
n'est pas dans le `PATH`, il ne modifie pas votre profil shell), `%LOCALAPPDATA%\cortex\bin`
sous Windows (ajouté au `PATH` *utilisateur*). À lire avant si vous le souhaitez :
[install.sh](install.sh), [install.ps1](install.ps1). Options : `CORTEX_INSTALL_DIR`,
`CORTEX_VERSION` (et `CORTEX_NO_MODIFY_PATH=1` sous Windows).

<details>
<summary>Binaires précompilés, ou compilation depuis les sources</summary>

| Plateforme | Archive (de la [dernière release](https://github.com/AstroQuestStudio/cortex/releases/latest)) |
|---|---|
| Linux x86_64 (glibc 2.35+ : Ubuntu 22.04, Debian 12 ou plus récent) | `cortex-x86_64-unknown-linux-gnu.tar.gz` |
| macOS Apple silicon | `cortex-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `cortex-x86_64-apple-darwin.tar.gz` |
| Windows x86_64 | `cortex-x86_64-pc-windows-msvc.zip` |

Chaque archive contient le binaire `cortex`, le README, la licence et le changelog ; les sommes
de contrôle sont dans `SHA256SUMS.txt`. Depuis les sources (Rust stable et un compilateur C,
pour les grammaires tree-sitter) :

```sh
cargo install --locked --git https://github.com/AstroQuestStudio/cortex
```

Les paquets crates.io et npm (`astroquest-cortex`) sont prêts dans ce dépôt mais pas encore
publiés.

</details>

Puis indexer un projet et l'interroger :

```sh
cd mon-projet
cortex index . --name MonProjet     # quelques secondes ; l'index se tient ensuite à jour seul
cortex find "où sont signées les sessions"
```

## Mesuré, pas promis

**Banc public** : 60 questions sur [flask](https://github.com/pallets/flask) (Python),
[hono](https://github.com/honojs/hono) (TypeScript) et
[ripgrep](https://github.com/BurntSushi/ripgrep) (Rust) à des commits épinglés, écrites par des
personnes qui n'ont jamais vu une sortie de Cortex, avant tout passage de Cortex sur ces dépôts.
Même corpus, même juge pour chaque approche. Rejouable par tous :
[`bench/public/run.sh`](bench/public/run.sh).

| Approche | top-1 | top-5 | MRR | tokens lus avant le bon fichier (médiane par dépôt) |
|---|---:|---:|---:|---:|
| **graphify 0.8.38** (`graphify query`, meilleur de 5 réglages essayés) | 13,3 % | 35,0 % | 0,238 | pas de liste classée : ~1 500 tokens de graphe par question |
| grep par mots-clés (`rg`, fichiers classés par mots distincts) | 16,7 % | 58,3 % | 0,345 | 5 141 – 31 710 |
| BM25 sur les fichiers entiers | 50,0 % | 78,3 % | 0,625 | 8 – 22 |
| **Cortex 0.5.0** | **56,7 %** | **86,7 %** | **0,702** | **14 – 29** |

Les références RAG dense (38,3 % / 68,3 %) et hybride (41,7 % / 81,7 %), mesurées avec Cortex 0.3.0
(elles demandent un modèle de 506 Mio), sont dans [docs/BENCHMARKS.md](docs/BENCHMARKS.md).

### Cortex contre graphify, face à face

Mêmes 60 questions, même juge (rang du premier fichier attendu, coupé à 20). graphify est construit
avec son propre `graphify update` et interrogé avec son propre `graphify query`, sur la même machine
(Ryzen 7 5800H, Windows 11). Pour être juste avec graphify, nous avons essayé cinq façons de
l'interroger (question naturelle ou mots-clés seuls, largeur ou profondeur, budget 2 000 ou 10 000
tokens) et retenu la meilleure : profondeur, mots-clés seuls. Son réglage par défaut (question
naturelle, largeur) donne 10,0 % / 30,0 % / 0,204. Rejouable : [`bench/public/graphify-bench.py`](bench/public/graphify-bench.py).

| | graphify 0.8.38 | **Cortex 0.5.0** | |
|---|---:|---:|---|
| top-1 / top-5 (60 questions) | 13,3 % / 35,0 % | **56,7 % / 86,7 %** | **4,3× / 2,5×** |
| MRR | 0,238 | **0,702** | 3,0× |
| Une question, processus complet | ~570 ms | **~24 ms** | **~24× plus vite** |
| Construire l'index (flask / hono / ripgrep) | 5–7 s / 14–16 s / 8–9 s | **moins de 0,4 s chacun** | 20× ou plus |
| Taille de la réponse | ~1 500 tokens | **~415 tokens** | 3,6× plus petite |
| On vient d'ajouter une fonction, on la demande | *No matching nodes found* tant qu'on ne relance pas `graphify update` | **trouvée en 18 ms** (vérification et réindexation du seul fichier dans l'appel) | toujours à jour |
| Impact de `parseFormData` dans hono | 4 nœuds, profondeur 2 par défaut, pas de liste de tests | **10 dépendants sur 3 niveaux, 4 tests à relancer** | |
| Clé d'API LLM | pour les docs et les liens sémantiques | **jamais** | 100 % local |

Ce que graphify a et pas Cortex : une page de graphe interactive (`graph.html`), la détection de
communautés et une couche sémantique pour docs, images et articles (elle demande un LLM ; les
chiffres ci-dessus sont son mode AST seul, celui que donnent `graphify update` et `graphify query`
sans clé d'API). Son `query` est un parcours du graphe fait pour explorer, pas un localisateur
classé « où est X », qui est la question de ce banc. Ce que Cortex a et pas graphify : une réponse
classée avec identifiants stables, `card`, `read` (les lignes exactes d'un symbole), `ask` (tout
pour une question en un appel sous budget), `grep` avec la fonction englobante, `ui` (du texte
affiché au code), les docs de bibliothèques hors ligne, un index qui se rafraîchit à chaque appel.

### Là où Cortex ne gagne **pas**

BM25 sur les fichiers entiers bat encore Cortex en top-1 sur ripgrep (50 % contre 45 %). Face à la
0.3.0, la 0.5.0 bouge peu sur ce banc public : +1,7 point de top-1, −1,7 de top-5, +0,017 de MRR
(flask top-1 de 70 % à 60 %, ripgrep top-5 de 85 % à 75 %, hono et ripgrep top-1 en hausse). C'est
dans le bruit de 20 questions par dépôt (une question = 5 points) : les gains de la 0.5.0 se voient
sur le gros monorepo privé ci-dessous. Détail : [docs/BENCHMARKS.md](docs/BENCHMARKS.md).

**Vitrine réelle** : le monorepo propriétaire d'AstroQuest (8 069 fichiers, 1,7 M lignes de
TypeScript, Rust et SQL), questions cachées écrites avant tout réglage. Cortex 0.5.0 :
**67,6 % en top-1, 94,6 % en top-5** sur les 37 questions cachées (0.3.0 sur les mêmes : 59,5 % /
81,1 %), et 66 % / 85 % sur un jeu étendu de 100 questions (BM25 sur fichiers, ancien jeu de 40
questions : 30 % / 57,5 % ; RAG dense : 25 % / 35 % ; grep : 7,5 % / 30 %). Sur 37 questions plus
vagues jamais utilisées pour régler quoi que ce soit : 21,6 % / 51,4 % (0.3.0 : 18,9 % / 45,9 %) :
la formulation vague reste son point faible. Sur 10 tâches de compréhension d'agent (impact, flux
de données, vue d'un module, tests à relancer) : 86 % des faits couverts contre 89 % avec grep +
lecture, pour **10,9 fois moins de tokens et 40 % d'appels en moins**. Latence : `find` 5–9 ms,
`card` ~2 ms, `impact` ~2 ms, mise à jour d'un fichier modifié 7 ms, index complet ~5 s.

## Une vraie session

La session animée plus haut, en texte. Sur [hono](https://github.com/honojs/hono) (421 fichiers), un agent veut modifier l'analyse des
formulaires. Quatre appels, **~1 070 tokens** en tout :

```console
$ cortex find "where is the request body parsed as form data"
S:src/utils/body.ts#parseFormData fn L126-150 — Parses form data from a request.
S:src/request.ts#formData method L334-336 — Parses the request body as `FormData`.
…
next : card S:src/utils/body.ts#parseFormData

$ cortex card S:src/utils/body.ts#parseFormData
S:src/utils/body.ts#parseFormData fn L126-150
sig: async function parseFormData<T extends BodyData>( request: HonoRequest | Request, options: ParseBodyOptions ): Promise<T>
rôle: Parses form data from a request.
appelle 3 (+1 ambigu): S:src/utils/body.ts#isRawRequest, S:src/utils/body.ts#convertFormDataToBodyData, S:src/utils/buffer.ts#bufferToFormData
appelé par 1 (1 fichiers, L = ligne de l'appel): S:src/utils/body.ts#parseBody L112
tests: F:src/utils/body.test.ts
next : read S:src/utils/body.ts#parseFormData

$ cortex read S:src/utils/body.ts#parseFormData      # les lignes 126 à 150, numérotées
$ cortex impact S:src/utils/body.ts#parseFormData
impact S:src/utils/body.ts#parseFormData fn L126-150
10 dépendant(s) sur 3 niveau(x), 9 fichier(s)
tests à relancer 4: F:src/middleware/method-override/index.test.ts, F:src/request.test.ts, F:src/utils/body.test.ts, F:src/validator/validator.test.ts
…
```

Le même chemin par grep et lecture (grep « form data », lire `body.ts`, grep de ses appelants)
coûte **~9 000 tokens**, sans les dépendants transitifs ni les tests à relancer.

Chaque résultat porte un **identifiant stable** (`S:chemin#symbole`, `F:chemin`,
`D:doc#section`) que l'agent recopie dans l'appel suivant, une plage de lignes, et une dernière
ligne `next :` qui propose l'appel le plus utile ensuite. Format spécifié dans
[docs/SPEC.md](docs/SPEC.md).

## Brancher son agent

Cortex est un serveur [MCP](https://modelcontextprotocol.io) en stdio : `cortex mcp`.

- **Claude Code** : `/plugin marketplace add AstroQuestStudio/cortex` puis
  `/plugin install cortex@astroquest` (serveur MCP + skill `/cortex`), ou
  `claude mcp add --scope user cortex -- cortex mcp`.
- **Cursor** (`~/.cursor/mcp.json`), **Gemini CLI** (`~/.gemini/settings.json`) :
  `{ "mcpServers": { "cortex": { "command": "cortex", "args": ["mcp"] } } }`
- **Codex CLI** (`~/.codex/config.toml`) : `[mcp_servers.cortex]` avec `command = "cortex"` et
  `args = ["mcp"]`.
- **VS Code** (`.vscode/mcp.json`) :
  `{ "servers": { "cortex": { "type": "stdio", "command": "cortex", "args": ["mcp"] } } }`

## Les outils

| Outil (CLI / MCP) | La question de l'agent |
|---|---|
| `find` / `cortex_find` | Où est X ? (langage naturel FR ou EN, ou mots-clés) |
| `card` / `cortex_card` | C'est quoi : signature, rôle, appelés, appelants avec la ligne de l'appel, tests ? |
| `read` / `cortex_read` | Montre exactement cette fonction (ou section de doc, ou plage) |
| `outline` / `cortex_outline` | Que contient ce fichier ? |
| `overview` / `cortex_overview` | Comment marche ce dossier ? |
| `impact` / `cortex_impact` | Qu'est-ce qui casse si je change ça, quels tests relancer ? |
| `path` / `cortex_path` | Comment A arrive-t-il à B ? |
| `changed` / `cortex_changed` | Qu'ai-je modifié, qui l'appelle, quels tests ? |
| `grep`, `files`, `docs` | Texte exact avec la fonction englobante, fichiers par nom, docs hors ligne |

## Feuille de route

Ingesteurs git (`why`), SQL (`schema`) et docs ; I1 `ask` sous budget ; I2 contexte
différentiel ; I5 impact prédictif ; I8 faits vérifiables ; langages Go, Java, PHP,
Ruby, Kotlin, Swift ; libellés de sortie en anglais et spécification v1.0. Détail dans le
[README anglais](README.md#roadmap).

## Contribuer, licence

Si Cortex fait économiser des tokens à votre agent, **mettez une étoile au dépôt** : c'est ainsi
que d'autres développeurs le trouvent. Voir [CONTRIBUTING.md](CONTRIBUTING.md) : des chiffres
avant et après chaque changement.
Licence [MIT](LICENSE) © 2026 [AstroQuest](https://astroquest.fr), qui s'en sert chaque jour sur
son propre monorepo de 8 000 fichiers.
