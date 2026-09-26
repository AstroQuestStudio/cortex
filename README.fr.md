<div align="center">

# Cortex

**Le moteur de contexte code pour agents IA.**<br>
find, card, read, impact : un agent comprend une base de code en quelques appels et avec ~10 fois
moins de tokens que grep + lecture de fichiers. Local, instantané, open source.

**par [AstroQuest](https://astroquest.fr)** · [English](README.md) · [Spécification](docs/SPEC.md) · [Bancs](docs/BENCHMARKS.md) · [Architecture](docs/ARCHITECTURE.md)

</div>

---

Les agents de code dépensent l'essentiel de leurs tokens à *chercher* : grep, ouvrir un fichier,
grep encore, en ouvrir un autre. Cortex indexe le dépôt une fois (quelques secondes pour
10 000 fichiers), tient l'index à jour à chaque appel, et répond aux questions que se posent
vraiment les agents — *où est X, c'est quoi, qui l'appelle, qu'est-ce qui casse si je le change* —
par des sorties courtes et enchaînables.

## Une vraie session

Sur [hono](https://github.com/honojs/hono) (421 fichiers), un agent veut modifier l'analyse des
formulaires. Quatre appels, **~1 070 tokens** en tout :

```console
$ cortex find "where is the request body parsed as form data"
S:src/utils/body.ts#parseFormData fn L126-150 — Parses form data from a request.
S:src/request.ts#formData method L334-336 — Parses the request body as `FormData`.
…
suite : card S:src/utils/body.ts#parseFormData

$ cortex card S:src/utils/body.ts#parseFormData
S:src/utils/body.ts#parseFormData fn L126-150
sig: async function parseFormData<T extends BodyData>( request: HonoRequest | Request, options: ParseBodyOptions ): Promise<T>
rôle: Parses form data from a request.
appelle 3 (+1 ambigu): S:src/utils/body.ts#isRawRequest, S:src/utils/body.ts#convertFormDataToBodyData, S:src/utils/buffer.ts#bufferToFormData
appelé par 1 (1 fichiers, L = ligne de l'appel): S:src/utils/body.ts#parseBody L112
tests: F:src/utils/body.test.ts
suite : read S:src/utils/body.ts#parseFormData

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
ligne `suite :` qui propose l'appel le plus utile ensuite. Format spécifié dans
[docs/SPEC.md](docs/SPEC.md).

## Installation

| Plateforme | En une ligne |
|---|---|
| Toutes (Rust) | `cargo install astroquest-cortex` |
| Toutes (Node ≥ 18) | `npx -y astroquest-cortex --version` (ou `npm i -g astroquest-cortex`) |
| Linux x86_64 | `curl -fsSL https://github.com/AstroQuestStudio/cortex/releases/latest/download/cortex-x86_64-unknown-linux-gnu.tar.gz \| tar -xz -C ~/.local/bin cortex` |
| macOS (Apple silicon) | `curl -fsSL https://github.com/AstroQuestStudio/cortex/releases/latest/download/cortex-aarch64-apple-darwin.tar.gz \| sudo tar -xz -C /usr/local/bin cortex` |
| macOS (Intel) | `curl -fsSL https://github.com/AstroQuestStudio/cortex/releases/latest/download/cortex-x86_64-apple-darwin.tar.gz \| sudo tar -xz -C /usr/local/bin cortex` |
| Windows (PowerShell) | `iwr https://github.com/AstroQuestStudio/cortex/releases/latest/download/cortex-x86_64-pc-windows-msvc.zip -OutFile cortex.zip; Expand-Archive cortex.zip "$env:USERPROFILE\.cortex\bin" -Force` (puis ajouter ce dossier au `PATH`) |

```sh
cd mon-projet
cortex index . --name MonProjet     # quelques secondes ; l'index se tient ensuite à jour seul
cortex find "où sont signées les sessions"
```

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

Sans installation globale : `"command": "npx", "args": ["-y", "astroquest-cortex", "mcp"]`.

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

## Mesuré, pas promis

**Banc public** : 60 questions sur flask, hono et ripgrep (commits épinglés), écrites par des
personnes qui n'ont jamais vu une sortie de Cortex, avant tout passage de Cortex sur ces dépôts.
Rejouable par tous : `bench/public/run.sh`.

| Approche (même corpus, même juge) | top-1 | top-5 | MRR |
|---|---:|---:|---:|
| grep par mots-clés | 16,7 % | 58,3 % | 0,345 |
| RAG dense | 38,3 % | 68,3 % | 0,516 |
| RAG hybride | 41,7 % | 81,7 % | 0,580 |
| BM25 sur les fichiers | 50,0 % | 78,3 % | 0,625 |
| **Cortex 0.3.0** | **55,0 %** | **88,3 %** | **0,685** |

**Vitrine réelle** : le monorepo propriétaire d'AstroQuest (8 069 fichiers, 1,7 M lignes),
questions cachées écrites avant tout réglage : **67,5 % en top-1, 85 % en top-5** (BM25 30 %,
RAG dense 25 %, grep 7,5 %). Sur 10 tâches de compréhension d'agent : 86 % des faits couverts
contre 89 % avec grep + lecture, pour **10,9 fois moins de tokens et 40 % d'appels en moins**.
Détails, et ce que Cortex ne gagne pas encore : [docs/BENCHMARKS.md](docs/BENCHMARKS.md).

## Feuille de route

Ingesteurs git (`why`), SQL (`schema`) et docs ; I1 `ask` sous budget ; I2 contexte
différentiel ; I5 impact prédictif ; I8 faits vérifiables ; langages Go, Java, C, C++, PHP,
Ruby, Kotlin, Swift ; libellés de sortie en anglais et spécification v1.0. Détail dans le
[README anglais](README.md#roadmap).

## Contribuer, licence

Voir [CONTRIBUTING.md](CONTRIBUTING.md) : des chiffres avant et après chaque changement.
Licence [MIT](LICENSE) © 2026 [AstroQuest](https://astroquest.fr), qui s'en sert chaque jour sur
son propre monorepo de 8 000 fichiers.
