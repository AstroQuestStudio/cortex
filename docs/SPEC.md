# Cortex tools specification — v0.1 (draft)

This document specifies the **identifiers**, the **tools** and the **outputs** that Cortex offers
to AI agents, so that other code-intelligence engines can implement the same contract and agents
(or their prompts, skills and benchmarks) can rely on it. Cortex 0.3.0 is the reference
implementation.

Status: **draft**. v0.1 describes Cortex 0.3.0 as it is. It becomes stable as v1.0, together with
Cortex 1.0; until then, incompatible changes are possible and are listed in the changelog.
The key words MUST, SHOULD and MAY are used as in RFC 2119.

## 1. Design goals

1. **Chainable.** Every output carries identifiers that the agent copies verbatim into its next
   call. No tool asks the agent to guess an offset, a path spelling or a line.
2. **Bounded.** Every output fits a token budget chosen by the caller and says what it cut.
3. **Provenanced.** Every fact points to a file and a line range, so it can be checked.
4. **Dense.** Plain text, one fact per line, no banners, no blank lines, no colours. The first
   line is already information.
5. **Guided.** The last line proposes the most useful next call.

## 2. Identifiers

### 2.1 Grammar

```
id        = symbol-id / doc-id / file-id
symbol-id = "S:" path "#" name [ "~" rank ]
doc-id    = "D:" path "#" anchor [ "~" rank ]
file-id   = "F:" path
path      = project-relative path, "/" as separator, no leading "./"
name      = exact symbol name as written in the source (case-sensitive)
anchor    = slug of a markdown heading (see 2.3)
rank      = decimal integer >= 2
```

Reserved prefixes for future node kinds: `T:` (database table, e.g. `T:public.users`), `C:`
(commit, e.g. `C:a6d916b`). An implementation MUST NOT use other meanings for them.

### 2.2 Homonyms and stability

When a file contains several symbols with the **same exact name** (or headings with the same
anchor), the first one in file order is `#name`, the next ones `#name~2`, `#name~3`… An
identifier therefore depends only on the path, the name, and the homonyms that *precede* the
symbol in its file: editing another file, or adding a symbol after it, never changes it.
Renaming or moving a symbol changes its identifier.

### 2.3 Anchors

`anchor = slug(heading text)`: fold accents to ASCII, lower-case, keep alphanumeric characters,
replace every run of other characters by a single `-`, strip leading and trailing `-`.
Example: `## Mise à jour (v2)` → `mise-a-jour-v2`.

### 2.4 Accepted inputs

Tools that take a target (`card`, `read`, `outline`, `impact`, `path`) MUST accept:

| Input | Meaning |
|---|---|
| `S:…`, `D:…`, `F:…` | the node designated by the identifier |
| identifier followed by provenance or punctuation copied by mistake: `S:a.ts#f L12-40`, `` `S:a.ts#f` ``, `S:a.ts#f,` | the identifier alone |
| `S:a.ts#f:12` | the identifier (the line is ignored) |
| a bare symbol name `parseBody` | the best-ranked symbol of that name; other definitions are listed as homonyms |
| a path `src/a.ts` or a **unique** path suffix `a.ts` | the file; an ambiguous suffix returns the candidate files |
| `path:line` | the innermost symbol enclosing that line (for `read`: that line) |
| `path:start-end` | that line range (`read`) or the enclosing symbol (other tools) |

Paths use `/`; implementations SHOULD also accept `\` on input.

## 3. Common output rules

- **Encoding**: UTF-8 plain text, `\n` line endings, no ANSI escapes.
- **Line ranges**: `L<start>-<end>` (1-based, inclusive); a single line is `L<n>`.
- **Symbol lines**: `<id> <kind> <range>[ export][ — <role>]`. `kind` is one of `fn`, `method`,
  `class`, `interface`, `struct`, `enum`, `type`, `const`, `component`, `hook`, `heading`, `decl`
  (a C/C++ prototype without a body; `import`/`export` are internal and not listed). `role` is the first sentence of the doc
  comment, or of the file header for a file, cut with `…` (90 characters in `find`, 70 in
  `outline`).
- **Call sites**: in `card`, `impact` and `path`, a caller is followed by the line of the call:
  `S:src/body.ts#parseBody L112`.
- **Work in progress**: `✎` after an identifier marks a file that differs from git `HEAD`
  (modified, staged, untracked). An implementation without git support omits the marker.
- **Budget**: callers pass a budget in tokens, where 1 token ≈ 4 characters. Output is cut at
  line boundaries, never inside a line. When lines are cut, the line before the last one is
  `… <n> ligne(s) coupée(s) : budget -b <budget> atteint` (the count MAY be omitted when unknown:
  `… budget -b atteint …`). Default budgets: `find` 1000, `card` 800, `path` 800, `read` 4000,
  others 1500.
- **Next call**: the last line is `next : <tool> <argument…>`, a complete call that can be
  replayed as is ("suite" means "next"). Implementations MUST end every successful output and
  every error with such a line when a useful next call exists.
- **Errors**: an error is a normal output whose first line starts with `(cortex) `, followed by
  a `next :` line (for instance `next : find <name>` when a symbol is unknown). Tools do not
  fail at the protocol level for a missing symbol.
- **Labels**: v0.1 labels are French (Cortex's first users were French); their meaning is fixed
  by the table in section 6. Agents MUST NOT depend on labels for parsing: identifiers, ranges
  and the `next :` line are the stable surface. A future version will standardise English
  labels.

## 4. Tools

Every tool exists under the same name in the CLI (`cortex <tool> …`) and in MCP
(`cortex_<tool>`). MCP inputs are JSON objects; every tool that searches accepts the optional
`project` (string: restrict to one indexed project) and `budget` (integer, tokens).

### 4.1 `find` — where is X?

Input: `question` (string: natural language in English or French, or keywords).
Output: one symbol line per result, best first, one result per ~40 tokens of budget (25 by
default); the first five carry their role. Last line: `next : card <first id>`.

```
S:src/utils/body.ts#parseFormData fn L126-150 — Parses form data from a request.
S:src/request.ts#formData method L334-336 — Parses the request body as `FormData`.
S:src/utils/body.ts#convertFormDataToBodyData fn L160-189 — Converts form data to body data based on the provided options.
next : card S:src/utils/body.ts#parseFormData
```

Ranking is implementation-defined. Implementations SHOULD publish their numbers on the public
benchmark (`bench/public/`, see [BENCHMARKS.md](BENCHMARKS.md)).

### 4.2 `card` — what is this symbol?

Input: `cible` (target, 2.4). A file target returns its `outline`.
Output, in this order, each line present only when non-empty:

```
<symbol line>
sig: <declaration up to the body, whitespace collapsed, ≤ 200 chars>
rôle: <first sentence of the doc comment>            | rôle (fichier): <file role>
appelle <n>[ (+<k> ambigu)]: <callee ids, resolved through imports>
appelé par <n> (<f> fichiers, L = ligne de l'appel): <caller id> L<call line>, …
fichier importé par <n> fichier(s)
tests: <test file ids>
membres <n>: <member ids>                           (classes)
autres exports du fichier <n>: <ids>
homonymes: <ids of other definitions with the same name>
next : read <id>
```

Calls that cannot be resolved to a single definition through the file's imports are counted as
*ambiguous*, never guessed.

### 4.3 `read` — show me the code

Input: `cible`, optional `contexte` (lines of context on each side, default 0).
Output: a header line (`<id> <kind> <range>` or `F:<path> L<a>-<b>`), then the lines, each
prefixed by its number and `│`. A doc-section target returns the whole section. When the budget
cuts the range, the `next :` line reads the rest: `next : read <path>:<next>-<end>`; otherwise
`next : impact <id>` (or `outline` for a file range).

```
S:src/utils/body.ts#parseFormData fn L126-150
126│async function parseFormData<T extends BodyData>(
…
150│}
next : impact S:src/utils/body.ts#parseFormData
```

### 4.4 `outline` — what does this file contain?

Input: `cible` (a file, or a symbol whose file is outlined).
Output: `F:<path> <language> <n> l.`, the file role, imports and importers (as `F:` ids),
then `symboles <n> (<k> exportés):` and one indented symbol line per symbol, nested symbols
indented further, `export` after exported ones.

### 4.5 `overview` — how does this folder work?

Input: `dossier` (project-relative folder, `.` for the whole project).
Output: `overview <dir>/ — <n> fichiers, <m> symboles`; entry points (files of the folder
imported from outside, `×<importers>`), outgoing and incoming dependencies by folder, external
files using it, external packages (unresolved import specifiers), sub-folders. Last line:
`next : outline <first entry point>`.

### 4.6 `impact` — what breaks if I change this?

Input: `cible`, optional `profondeur` (1–6, default 3).
Output: `impact <symbol line>`, a count line (`<n> dépendant(s) sur <d> niveau(x), <f>
fichier(s)`), `tests à relancer <n>: <file ids>`, then one block per depth:
`profondeur <k> — <n> (<f> fichiers)` followed by indented dependants. Dependants of a symbol
are its callers (with call line) and the files that import it by name without calling it;
dependants of a file are its importers. Tests are the test files reached, plus tests named after
the target. Dynamic imports and calls by string are not seen.

### 4.7 `path` — how does A reach B?

Input: `de`, `vers` (targets). Output: `appels <n> saut(s):` or `imports <n> saut(s):`, the start
node, then one indented hop per line: `→ appelle en L<call line> <node>` (calls) or
`→ importe <node>` (imports). The chain is the shortest one over calls, else over imports between
the files, else in the reverse direction:

```
imports 2 saut(s):
F:src/request.ts
  → importe F:src/utils/body.ts
  → importe F:src/utils/buffer.ts
next : read F:src/utils/body.ts
```

### 4.8 `changed` — what did I change?

No target. Output: the uncommitted files (git status against `HEAD`), the symbols whose ranges
intersect the diff hunks (innermost symbol), their callers outside the work in progress, and the
tests to rerun. Last line: `next : impact <most-called changed symbol>`.

### 4.9 `grep`, `files`, `list`, `docs`, `docs_list`

- `grep` — input `needle` (literal substring), `case_sensitive`, `max`. Output grouped by file
  (`F:` line), then by enclosing symbol (indented id), then `L<n>: <text>` lines.
- `files` — input `pattern` (case-insensitive fragment, or glob when it contains `*` or `?`).
  One project-relative path per line; paths are accepted as is by the other tools.
- `list` — indexed projects with file and symbol counts.
- `docs` / `docs_list` — search and list offline documentation sets scraped with
  `cortex docs add`.

### 4.10 Aliases

`query` = `find`; `explain` = `card` (`depth` accepted and ignored); `context` = `card` plus
the markdown docs (`docs/**/*.md`) that mention the symbol or its file. Aliases exist for
compatibility and MAY be dropped in v1.0.

## 5. MCP binding

- Server name `cortex`, transport stdio, JSON-RPC 2.0, tools capability only.
- Tool names are `cortex_<tool>`; input schemas are those of section 4 (`required` lists the
  target fields).
- A tool result is a single `text` content item holding the output of section 3.
- Descriptions MAY be localised; they SHOULD state the question the tool answers.

## 6. Label glossary (v0.1)

| Label | Meaning |
|---|---|
| `next :` | next call |
| `sig:` | signature |
| `rôle:` / `rôle (fichier):` | role (of the symbol / of its file) |
| `appelle <n> (+<k> ambigu)` | calls n resolved callees (+k ambiguous calls) |
| `appelé par <n> (<f> fichiers, L = ligne de l'appel)` | called by n callers in f files; `L` = line of the call |
| `fichier importé par <n> fichier(s)` | the symbol's file is imported by n files |
| `tests:` / `tests à relancer` | related tests / tests to rerun |
| `membres` / `autres exports du fichier` / `homonymes` | members / other exports of the file / same-name definitions |
| `importe` / `importé par` | imports / imported by |
| `symboles <n> (<k> exportés)` | n symbols (k exported) |
| `points d'entrée (importés de l'extérieur)` | entry points (imported from outside) |
| `dépend de` / `utilisé par (dossiers)` / `utilisé par (fichiers)` / `paquets` / `sous-dossiers` | depends on / used by (folders) / used by (files) / packages / sub-folders |
| `dépendant(s) sur <d> niveau(x)` / `profondeur <k>` | dependants over d levels / depth k |
| `appels` / `imports` / `saut(s)` / `→ appelle en L<n>` / `→ importe` | calls / imports / hop(s) / calls at line n / imports |
| `… <n> ligne(s) coupée(s) : budget -b <b> atteint` | n lines cut: budget reached |
| `(cortex) … introuvable` | … not found |

## 7. Conformance

| Level | Requirements |
|---|---|
| **Core** | identifiers (section 2), common rules (section 3), `find`, `card`, `read` |
| **Graph** | Core + `outline`, `overview`, `impact`, `path` with call sites resolved through imports |
| **Full** | Graph + `changed` and the `✎` marker, `grep`, `files`, MCP binding (section 5) |

An implementation claiming a level SHOULD publish its results on the public benchmark
(`bench/public/`) with the exact commit of the benchmark files.

## 8. Changes

- **v0.1 (Cortex 0.3.0)** — first public draft.
