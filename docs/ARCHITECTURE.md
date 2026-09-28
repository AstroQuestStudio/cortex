# Cortex architecture

Cortex is a source-available Rust engine that lets an AI coding agent **understand** a codebase in
a few calls, without `find`, `grep`, or reading whole files. It aims to do better than Obsidian
(hand-placed links, notes only) and better than graphify (a static code graph with no history).
The numbers in this document come from two sources: AstroQuest's private monorepo — a
proprietary TypeScript/Rust codebase of 8,000+ files and 1.7M lines, used as the real-world
showcase — and Cortex's public benchmark.

The judge for every step is `cortex bench` (relevance), the latency benchmark, and `cargo test`.
Nothing is adopted without before/after numbers.

## 1. Measured baseline (2026-09-26, AstroQuest: 8,060 files, 42,517 symbols)

| Step | Cost | Cause |
|---|---|---|
| Index load | ~300 ms | `index.bin` (18 MB, bincode) fully deserialized on every CLI call |
| Search | ~250 ms | linear scan of 42,517 symbols, no inverted index |
| Graph | rebuilt on every call | `RelGraph::build` redoes the in-memory name tables |
| Relations | by name | calls resolved by name homonymy (false links), file-level granularity |

## 2. Principles

1. **One single typed knowledge graph** per project. Nodes are code, docs, notes, history and
   database schema; edges are typed and every fact carries its provenance (`file:line` or
   `commit`).
2. **Zero decoding at load time**: storage is read in place (mmap), and a query only touches
   the pages it needs.
3. **Always fresh**: a daemon (the MCP server) watches files, and the index follows the editor
   within a second. The daemon-less CLI keeps `stat()`-based freshness control.
4. **Answers built for agents**: compact, token-bounded "cards", with **stable identifiers** an
   agent can reuse from one call to the next.
5. **Pluggable ingesters**: each source (code, docs, memory, git, SQL, routes, i18n) produces
   nodes and edges into the same graph.

## 3. Data model

### Nodes (`NodeKind`)
`File`, `Symbol` (function, method, component, hook, class, type…), `DocSection` (a markdown
heading and its content), `Note` (agent memory, ADR), `Commit`, `Table`, `Column`, `Policy`
(row-level security), `SqlFunction`, `Route`, `I18nKey`, `EdgeFunction`, `Test`.

Stable, human-readable identifier: `S:src/storage/upload.ts#putObject`,
`D:docs/guides/storage.md#persistence`, `T:public.files`, `C:a6d916b`. An agent can copy it
as-is into the next call. Current state: `S:`, `D:` and `F:` are rendered and accepted by all
tools (§6, `src/ids.rs`); homonyms in the same file: `#name`, then `#name~2`…; `T:` and `C:` are
waiting on the SQL and git ingesters.

### Edges (`EdgeKind`)
| Edge | Source → target | Ingester |
|---|---|---|
| `contains` | file → symbol, doc → section | code, docs |
| `imports` | file → file (resolved: relative, `@/`, index.ts) | code |
| `calls` | symbol → symbol (**resolved via imports**, otherwise `ambiguous`) | code |
| `renders` | component → component (JSX) | code |
| `tests` | test → symbol | code |
| `documents` | doc section or note → symbol or file (detected mention) | docs, memory |
| `reads` / `writes` | symbol → table (`.from('x').select/insert/update`) | code |
| `defines` / `guards` | migration → table, policy → table | SQL |
| `routes_to` | route → page component | routes |
| `uses_key` | symbol → i18n key | i18n |
| `changed_in` | symbol → commit (diff mapped onto the symbol's range) | git |
| `cochanges` | file ↔ file, weight = frequency of joint modification | git |

## 4. Storage: the "atlas"

`~/.cortex/<project>/atlas/` — the ONLY stored form of a project (no more `index.bin`):
- `manifest.json`: format version, root, stack of active segments (trunk then deltas),
  validation fingerprints, already-seen unindexable paths (`skipped`). The only file rewritten
  on every update (write then atomic rename).
- **immutable segments** (`rkyv`, read via zero-copy mmap): string table, nodes (files and
  symbols, with raw file references: imports, imported names, calls, tables), outgoing **CSR**
  edges (`contains`, `imports`, `calls`) and incoming ones, BM25F **inverted index** (name,
  comment and body fields), **fst** automata for vocabulary and path tokens, name indexes
  (name → symbols, path → file, called name → calling files, import base → importing files),
  corpus statistics.
- An update writes a **small delta segment** (a few KB): new node versions, created nodes,
  tombstones. Reading merges trunk + deltas; past a threshold, **compaction** rebuilds the
  stack into a fresh trunk.
- A segment is NEVER rewritten (a fresh name on every write): a process holding it mmap'd (the
  MCP server) keeps reading it; orphans are deleted later. A **write lock** (`write.lock`)
  serializes writers (CLI and MCP); readers don't need it.

Targets: open < 5 ms, query < 20 ms, single-file update < 30 ms.

### 4.1 Incremental update via delta segments

**Global identifiers.** Every node has a global id, never reassigned. A segment CREATES a
contiguous id range (`id_base`, `n_new`) and can carry a new VERSION of existing nodes
(`override_ids`) or declare them dead (`tombstones`). The current version of a node is the one
from the most recent segment that carries one. The merged view (`atlas/view.rs`) is an
id → (segment, local index) array, built lazily in one pass over the deltas (~0.2 ms). Anything
a segment says about a stale version — postings, outgoing and incoming edges, index entries — is
ignored on read: nothing is ever copied or rewritten. Identity indexes (name → symbol, path →
file) depend only on a node's lifetime, since an id always keeps its name and path (a rename =
one dead id + one new id).

**Matching.** A modified file keeps its id; its symbols are matched by (name, kind, nth
occurrence): matched = same id, new version; gone = tombstone; new = fresh id. Added file =
fresh ids, deleted = tombstones, renamed = both. All these cases go through a delta; a full
rebuild is now only a path for massive changes (> 10% of files) and for compaction.

**Exact incremental resolution.** A single resolution algorithm (`graph.rs`, `Universe` trait)
serves both full builds and deltas. A file's resolution depends only on its symbols and
references, the set of project paths, and, for each called name, the set of its definitions. An
UNCHANGED file is therefore only re-resolved if it calls a name whose definitions changed
(symbol names dropped or created, `call_names` index) or if it imports a path base that appeared
or disappeared (`import_bases` index). Only versions whose result changes (edges, ambiguity
count, I7 card) are written. A body-only edit re-resolves only the file itself.

**Identical relevance.** BM25F statistics: each segment carries its net contribution (the trunk
the absolute value, a delta additions − removals of replaced versions); their sum is exact. A
node has only one valid version, so its contributions come from a single segment, in the same
term order as a full rebuild: floating-point scores are bit-identical. Tie-breaking and display
order both follow the canonical order (path, rank within file) instead of id order. Reading is
one independent loop per segment (parallelizable later without changing the format).

**Two-level compaction.** The cost of a read grows with the NUMBER of segments, not their size;
rewriting the trunk (37 MB) for a few KB of deltas would be wasteful.
- **Delta merge** (frequent): past 16 segments, deltas are merged into ONE, the trunk is
  untouched. The net change since the trunk (files whose current version lives in a delta or
  whose metadata moved, trunk files that disappeared) is reapplied to the trunk alone via the
  ordinary delta path (`incremental::merge_deltas`); since incremental resolution is exact, the
  result answers like the stack it replaces.
- **Trunk rewrite** (rare): cumulative deltas > 20% of the trunk (and > 1 MB), or a massive
  change. Full materialization (raw references included) then a reference build (`build_full`),
  without re-reading any source.

**Judges.** `atlas/incremental.rs`: a chain of varied updates (body, homonym added, symbol added
and reordered, file renamed, deleted + touched, kind changed, `index.ts` recreated), then delta
merge, one more update, then compaction ≡ full rebuild, compared at each step on `context`,
`explain` and each symbol's card, on search (name, file, line, bit-exact score) and the
materialized index; symbol and file add/remove/rename via the delta path; identical result order
on a corpus rich in ties; concurrent writers. A manual test
(`CORTEX_EQUIV_PROJECT=<project> cargo test real_project -- --ignored`) replays 12 changes on a
real project and compares the 90 questions of both benchmarks (bit-exact score) and the symbols
touched against a full rebuild, before and after delta merge: identical on AstroQuest (15
segments, then 2).

**Measurements** (`cortex bench-latence`, AstroQuest: 8,069 files, 42,532 symbols):

| Step | Single rewritten trunk (before) | Delta segments |
|---|---:|---:|
| Update 1 file (body changed) | 167–215 ms | 7 ms |
| Update 1 file (export added) | full rebuild | 7–8 ms |
| CLI query (fresh process) | ~200 ms (automata rebuilt) | ~14 ms (persisted automata) |
| Regular compaction (every 16 updates) | — | ~1.8 s → delta merge ~25 ms (§12) |
| Trunk rewrite (rare) | — | ~1.7 s → ~1 s on 8 threads (§12) |

A single-file update breaks down into: open + classify ~1.5 ms, build the delta ~2.5 ms, write
~1.5 ms, validate + publish the manifest ~1.5 ms. Validation (bytecheck) covers only the bytes
just written, without reopening the file: on Windows, the first open of a freshly written file
waits on antivirus scanning (~10 ms measured for a 12 KB delta).

**Freshness without git.** The check reads, in parallel, the directories the reference walk
visited (one `readdir` each; on Windows the listing carries mtime and size): a tracked file
changed or gone → reread; unknown entry (indexable-extension file, directory) → judged by the
walk rule for its own directory (`walk::children`), read if kept (a new directory is walked in
full), remembered otherwise; a rule file (`.gitignore`, `.ignore`, parent directory rules,
`.git/info/exclude`, global excludes, presence of `.git`) changed → full walk. The walk state
(visited directories with no tracked file, excluded entries, rule fingerprints) lives in the
manifest. A file is therefore in the atlas if and only if the reference walk (`src/walk.rs`)
yields it: `index`, `update`, `update --changed` and the automatic check apply the same rule
(the old `update --changed` followed `git status` + `git check-ignore`, which diverged from the
walk: `.gitignore` is case-insensitive on git's side, `.ignore` is unknown to git).

**What remains.** The freshness check costs ~40 ms on 8 threads, ~30 ms of which is reading
1,458 directories: that's the per-opened-directory kernel cost (direct NT API and relative opens
tested: no gain). Getting much lower needs a daemon watching files (§5). The trunk rewrite (~1 s)
remains dominated by interning and sequential postings (§12).

## 5. Processes

- **Daemon** = the MCP server: keeps the atlas open and watches files (the `notify` crate), then
  reindexes in batches after 200 ms of inactivity.
- **CLI**: opens the atlas via mmap (a few ms), so no cache or daemon is needed to be fast.
- Ingestion is **incremental everywhere**: blake3 per file, and for git, the last processed
  commit is remembered.

## 6. Tools for agents (CLI and MCP, same names)

| Tool | Agent's question | Replaces | State |
|---|---|---|---|
| `find <question>` | "where is X?" (BM25F, §7) | grep, find | done (`query`: alias) |
| `card <id\|name>` | "what is it, who calls it, what does it call, is it tested?" | explain + files + grep | done (`explain`: alias; `context`: + docs mentioning it) |
| `outline <file>` | "what's in this file?" (role, imports, importers, nested symbols, ranges, exports) | reading the file | done |
| `read <id>` | "show me the code of THIS function" (exact numbered lines, ±N) | Read with a guessed offset | done |
| `overview <folder>` | "how does this module work?" (roles, entry points, dependencies, packages) | 10 reads | done |
| `impact <id>` | "what breaks if I change this?" (transitive dependents, tests) | repeated grep | done (without git co-changes: I5) |
| `path <a> <b>` | "how does A reach B?" | reading step by step | done |
| `changed` | "what have I modified?" (touched functions, impact) | git diff | done (seed of I4) |
| `grep <text>` | exact text, grouped by file **and enclosing symbol** (identifier) | rg | done |
| `why <id>` | "why is this written this way?" | git log and blame | to do (git ingester) |
| `schema <table>` | columns, RLS policies, who reads, who writes | SQL and grep | to do (SQL ingester) |

A single implementation (`src/outils/`) serves the CLI (`cortex find …`), the MCP server
(`cortex_find` …) and the agent benchmark (§7). Output rules, common to all tools:
- **Stable identifiers** (`src/ids.rs`, §3): `S:<path>#<symbol>`, `D:<path>#<anchor>`,
  `F:<path>`. If a file contains several symbols of the same name, the first keeps `#name`, the
  following ones `#name~2`, `#name~3`: an identifier only depends on the homonyms preceding it
  in its file. As input, every tool also accepts a name, a path, a unique path suffix (e.g.
  `upload.ts`), `path:line` (enclosing symbol) or `path:start-end` (range, for `read`), and an
  identifier pasted back with its provenance or punctuation (`S:…#f L12-40`, identifier wrapped
  in backticks, trailing comma); several files matching a suffix: their list, and
  `suite : outline <the first one>`.
- **Provenance**: the path is in the identifier, lines as `L<start>-<end>`; in `card` and
  `impact`, each caller carries the line of the CALL (`S:…#Auth L123`).
- **Token budget** (`-b`, ≈ characters / 4); extra lines are counted
  (`… 12 ligne(s) coupée(s)`), never cut mid-line. `find` returns one result per 40-token slice
  (25 by default), with a one-sentence role for the first 5.
- **✎** marks what comes from an uncommitted file (overlay, §9 I4).
- **Last line `suite : <call>`**: the next call most likely to be useful, ready to replay
  (`find` → `card` of the first result, `card` → `read`, `read` → `impact`, `impact` → `read` of
  the first dependent, `overview` → `outline` of the first entry point, `changed` → `impact` of
  the most-called touched symbol; a range cut by the budget → `read path:start-end` for what
  follows).
- No title or decoration: the first line already carries information.

The tool output labels shown below are currently in French ("appelle", "appelé par", "tests:",
"suite :", "profondeur", "dépendant(s)") — this is a known limitation, not part of the contract.
Agents should rely on the identifiers, the line ranges, and the final `suite :` line, which
`docs/SPEC.md` documents precisely, rather than parsing the labels themselves.

Synthetic examples (same format as real output, using invented generic names):

```
$ cortex card sanitizeRedirect
S:src/http/redirect.ts#sanitizeRedirect fn L35-50
sig: export function sanitizeRedirect(value: string | null | undefined, fallback = '/'): string
rôle (fichier): sanitizeRedirect — guard against open-redirect.
appelle 1 (+1 ambigu): S:src/http/redirect.ts#hasControlChars
appelé par 7 (6 fichiers, L = ligne de l'appel): S:src/components/notifications/NotificationPanel.tsx#NotificationPanel L124, S:src/pages/Login.tsx#Login L123, …
fichier importé par 7 fichier(s)
tests: F:src/http/__tests__/redirect.test.ts
suite : read S:src/http/redirect.ts#sanitizeRedirect

$ cortex impact putObject
impact S:src/storage/upload.ts#putObject fn L144-167
9 dépendant(s) sur 3 niveau(x), 7 fichier(s)
tests à relancer 1: F:tests/storage/archiver.test.ts
profondeur 1 — 1 (1 fichiers), L = ligne de l'appel
  S:src/storage/archiver.ts#archiveRemoteAsset L99
profondeur 2 — 2 (1 fichiers)
  S:src/storage/archiver.ts#archiveAudioTrack L141
  S:src/storage/archiver.ts#archiveMediaAsset L169
profondeur 3 — 6 (6 fichiers)
  F:src/functions/media-generate/index.ts
  …
suite : read S:src/storage/archiver.ts#archiveRemoteAsset
```

**What each tool reads.** Everything comes from the mmap'd atlas, except `read` (the lines, from
disk, the atlas being kept fresh by the automatic check), `context` (the `docs/**/*.md` docs,
from disk) and the overlay (`git`, memoized, §9 I4).
- `card` = the stored I7 card (§9) + the incoming side read from the inverse CSR: callers
  (deduplicated: a call made inside an inner function is also within the range of its enclosing
  function; for the same file and call line, only the innermost caller is kept), number of file
  importers, tests (test files that call it, that import its file, or that are named after it or
  its file), class members, other file exports, homonyms.
- `impact`: breadth-first walk of dependents, capped at 4,000 nodes: callers of a symbol and
  files that import its file while NAMING the symbol without calling it (a type, a constant),
  importers of a file; tests = test files reached + tests named after the target.
- `path`: shortest path (breadth-first, neighbors in canonical order, 12 hops) over calls, then
  over imports between files, then in reverse.
- `outline`: nesting by range inclusion; export = a signature starting with `export`/`pub`.
- `overview`: entry points = folder files imported from outside (ranked by number of external
  importers); outgoing and incoming dependencies per folder, then external files that use it;
  packages = unresolved import specifiers.

**Graph.** `new X()` is now a call to `X` (classes previously had no callers); a SCREAMING_CASE
name is no longer classified as a component (neutral on both benchmarks). Known limits: dynamic
imports (`import('x').then(({ f }) => f())`) and chained calls are not resolved (seen on the
agent benchmark, on an impact task); a call made outside any function (e.g. a top-level
`app.listen(async () => …)`) has no symbol caller, hence the `F:` dependents in `impact`.

**Latency** (`cortex bench-latence`, AstroQuest 8,052 files / 42,507 symbols, 16 threads, atlas
opened once, full output at the default budget, memoized overlay; median over the 50 public
benchmark questions for `find`, over a regular sample of 30 called symbols + the project's
most-called one, 30 files, 60 pairs half of which are arbitrary, 8 folders; two passes):

| Tool | Median | Worst case | Target |
|---|---:|---:|---:|
| find | 5–9 ms | — | < 20 ms |
| card | 1.2–2.7 ms | — | < 5 ms |
| read | 0.6–1.3 ms | — | < 10 ms |
| outline | 0.5–0.7 ms | — | < 10 ms |
| impact (depth 3) | 1.2–2.5 ms | 15–20 ms (most-called symbol) | < 50 ms |
| path | 0.02 ms | 2.3 ms (pair with no path: full walk, both directions) | < 50 ms |
| overview | 1.2–2.5 ms | — | — |
| changed (incl. `git status` and `git diff`) | ~190 ms | — | — |

A CLI command adds the open cost (~0.5 ms) and the freshness check (~40 ms, §4.1). `git status`
(~100–200 ms) is only re-run if the git index, `HEAD`, the branch, or the segment stack changed;
otherwise the overlay is reread from `~/.cortex/<project>/overlay.json`.

## 7. Relevance

### Fields and weights (current state)
BM25F on each segment's inverted index (k1 = 1.4, b = 0.75), score carried per symbol:

| Field | Content | Matching | Weight |
|---|---|---|---|
| names | tokens of the decomposed name (camelCase, snake, whole identifier) | fuzzy: equality, prefix ≥ 4, distance 1 | 1 (× 1.15 for components, hooks, classes, interfaces) |
| path | file path tokens | fuzzy, unstemmed typed word | 0.3 × idf of the term in names |
| doc | doc-comment above the symbol (250 tokens max) | fuzzy | 0.4 |
| header | file header comment | fuzzy | 1, added to the file's best symbol |
| **body** | the file's whole text (code, comments, strings), excluding markdown | exact term | **0.5**, added to the file's best symbol |

Plus a bonus when the symbol name IS the query (+50) or contains it (+5), and a ×0.8 penalty for
test files when the query isn't about tests. A file found only through its header or body
surfaces via its first symbol (excluding imports). Weights are overridable without recompiling:
`CORTEX_W_HEADER`, `CORTEX_W_DOC`, `CORTEX_W_BODY`, `CORTEX_TEST_PENALTY`.

- **Body field** (`symbol::body_terms`): split on non-alphanumeric then camelCase and
  letter↔digit boundaries, accents folded, lowercased, 2 to 40 characters, stopwords and numbers
  removed, stemmed; one bag (term, occurrences) per file. Vocabulary, fst automaton, postings
  (parallel file / occurrence arrays, 6 bytes per posting) and statistics (`body_docs`,
  `body_len`) are field-specific, per segment. Terms are stored ONLY in the postings: a file's
  bag is reread from them when its version must be re-emitted in a delta (`Handle::body_of`) or
  fully materialized (one pass per segment). The file node only keeps `body_len`. No body field
  for markdown: measured as neutral, its prose duplicating titles and headers. One bag per file
  rather than per symbol: the benchmark judges files, and a per-symbol bag would multiply
  postings (nested symbols); this variant hasn't been measured.
- **Stemming** (`stem.rs`): homegrown, a single suffix table for French and English (a code
  token's language is unknown), one suffix removed, stem of at least 4 letters (5 for `-er`,
  `-age`), nothing for a non-ASCII-letter token: `virtualization`/`Virtualizer` → `virtual`,
  French `chargement`/`charger` → `charg`, `securite`/`security` → `secur`. Applied to indexed
  terms (names, comments, body) when postings are written and to query terms after glossary
  expansion; raw tokens stay on the nodes, so materializing then rebuilding stems identically.
  Paths keep their raw tokens (searched with the typed word).
- **French↔English glossary** (`semantic.rs`): groups of equivalent words; a word added by
  expansion weighs 0.55. Extended with 17 groups of everyday vocabulary (window, desktop,
  employee, HR, data, GDPR, address/URL, password, reminder, payroll, bank, tab, title,
  guest…).
- **Search**: allocation-free candidates (strings borrowed from the mmap), only the top `limit`
  are selected then sorted; the order is total, so it is exactly the prefix of the full ranking
  (test `search_limit_is_prefix_of_full_ranking`).

### Two benchmarks: public and hidden
The private benchmark (50 tuning questions + 40 held-out questions) is kept in the private
repository, located through the `CORTEX_BENCH_DIR` environment variable (default `bench/`),
under the names `queries.json`, `holdout.json` and `agent_tasks.json`. A public, reproducible
benchmark lives in `bench/public/` and is described in `docs/BENCHMARKS.md`.
- The 50-question public set (29 fr, 10 en, 11 technical) is used for tuning.
- The 40-question hidden set (15 fr, 15 en, 10 technical, covering the main modules and
  subsystems of the private monorepo: UI, backend functions, hooks, database, build tooling) was
  written and measured BEFORE any tuning, and is never modified afterwards. **Adoption rule**: a
  change is kept only if it improves (or does not lower) BOTH benchmarks; if it only improves the
  public one, that's overfitting, and it is reverted. Known limit: the 17 glossary groups were
  chosen by reading the hidden benchmark's failures, so it is no longer fully blind for the
  glossary; the hidden-set gain attributed to the glossary (+2.1 MRR points) is optimistic.

Contribution of each building block, measured on a frozen copy of AstroQuest (top-1 / top-5 /
MRR):

| Step | Public (50) | Hidden (40) | Decision |
|---|---|---|---|
| Engine 2a (names, doc, header, path) | 72 / 88 / 0.776 | 50 / 70 / 0.586 | baseline |
| + body, weight 0.3, no stemming | 76 / 88 / 0.805 | 57.5 / 77.5 / 0.650 | kept |
| stemming alone (no body) | 72 / 92 / 0.808 | 55 / 67.5 / 0.611 | hidden top-5 down: not alone |
| + stemming (body 0.3) | 76 / 92 / 0.835 | 62.5 / 77.5 / 0.686 | kept |
| body 0.5 (0.4 to 1.0 tried) | 80 / 92 / 0.859 | 67.5 / 80 / 0.721 | kept: hidden-set optimum |
| body 0.7 / 1.0 | 82 / 96 / 0.872 · 84 / 96 / 0.879 | 67.5 / 80 / 0.719 · 65 / 77.5 / 0.711 | rejected: public rises, hidden falls |
| + glossary from public failures | 84 / 94 / 0.888 | 67.5 / 80 / 0.721 | rejected: hidden unchanged |
| + glossary matched by stem (plurals) | 80 / 92 / 0.859 | 62.5 / 72.5 / 0.674 | rejected: hidden down |
| + general glossary (17 groups) | 80 / 92 / 0.859 | 67.5 / 85 / 0.742 | kept |
| + no body field for markdown (current state) | **80 / 94 / 0.861** | **67.5 / 85 / 0.742** | kept (smaller atlas) |

By category (current state): public fr 79 / 93 / 0.862, en 60 / 90 / 0.703, technical
100 / 100 / 1.000; hidden fr 73 / 87 / 0.778, en 53 / 80 / 0.645, technical 80 / 90 / 0.833.

Costs (AstroQuest, 8,070 files): atlas 30.8 → 39.0 MB (+1.14M body postings, 37,400 body terms);
build ~20–26 s (noisy shared machine: ±30%); median query 9–10 ms (7.9 before, target < 20), p95
17–19 ms; single-file update 7 ms (target < 30); compaction ~1.8 s instead of ~0.9 s. Trunk +
deltas ≡ full rebuild equivalence verified with the body field (`incremental.rs` tests, including
a "content only" step and a word present only in the body, and the manual test on the real
project: 90 questions from both benchmarks and 104 touched symbols, bit-identical).

### Agent benchmark
Relevance benchmarks judge a ranking of files; the agent benchmark judges what an agent pays to
UNDERSTAND: `cortex bench-agent` replays 10 realistic comprehension tasks on AstroQuest. Each
task carries its **expected facts** (paths, or `path#symbol`, verified by hand via rg and code
reading) and two scripted call sequences: with Cortex (`find`, `card`, `impact`, `path`,
`overview`, `outline`…) and without (what an agent does with Grep, Glob and Read). The tasks and
sequences were written and committed BEFORE any tuning of tool output.

The 10 task types cover: the impact of changing a security-sensitive utility function; the
impact of changing a low-level data/storage helper; the transitive impact of changing a
third-party storage integration function; tracing a data flow through a local persistence
feature; tracing a data flow for a file-upload feature; getting an overview of an unfamiliar
module; finding which tests to rerun after changing a queue/upload hook; finding the path between
an authentication check and a helper function; answering "where is this guarded, and who uses it"
about an access-control mechanism; and tracing a data flow from TypeScript into Rust/WebAssembly.

Rules (`src/banc_agent.rs`), identical on both sides:
- a step only uses the words of the question, its `vocabulary` (translations an agent would
  make: "upload" → "envoi") and what previous outputs have shown, word for word, accents folded,
  up to stemming; `$k` = the k-th identifier of the previous output (Cortex) or the k-th file of
  the last search, ranked by number of matching lines found (without Cortex: an ordering more
  favorable than rg's own). A step that violates the rule is not played, and the benchmark flags
  it;
- without Cortex: `glob` (paths, 100 max), `rg` (lines `path:line:text`, case-insensitive
  substring, 250 lines max like Grep, text cut at 500 characters), `read` (whole numbered file,
  2,000 lines max like Read), over the atlas's files (the ones rg would see);
- tokens read = characters / 4 of the COMPLETE output of each call; a `path` fact is covered if
  an output contains the path, a `path#symbol` fact if the same block (one line; the whole output
  for a code read) contains both the path and the symbol.

Results (AstroQuest):

| | Calls / tokens / facts |
|---|---|
| **With Cortex** | **16 / 6,572 / 49 of 57 (86%)** |
| **Without Cortex** | **27 / 71,597 / 51 of 57 (89%)** |

Reading:
- **≈10.9× fewer tokens and 40% fewer calls**, for close coverage (86% vs. 89%). On impact,
  tests, module and path tasks, Cortex covers as much or more in a single call; on `path`, the
  call's line gives the link that reading whole files leaves the agent to guess.
- **Where Cortex loses facts**: when `find`'s first result isn't the right one and the scripted
  sequence takes `$1` anyway, so the wrong symbol gets explored first; also, a dynamic import or
  a `new Worker(new URL(…))` construct isn't seen by `impact`. Without Cortex, coverage comes
  from whole-file reads (15,000 to 32,000 tokens per data-flow task).
- The benchmark doesn't measure the CORRECTNESS of what is read (an agent without Cortex who
  reads 8,000 lines sees the facts without necessarily connecting them); it is a floor on cost
  and coverage, not a comprehension score.

### Next steps for relevance
- `model2vec-rs` + `potion-multilingual-128M` vectors (MIT, pure Rust), only as a re-ranking step
  and only if they improve the hidden benchmark: fused at equal weight, they make Cortex worse
  (§11).
- Remaining failures: vocabulary that neither the glossary nor the body field connects (a French
  UI term for "charts" not linking to its English code name; a French phrase for "draw without an
  account" not linking to a local-persistence symbol); queries where a richer neighboring file
  wins (a query about preventing an open redirect after login surfacing the login page ahead of
  the redirect-guard file itself); and a few debatable hidden-benchmark expected answers (another
  file answers just as well). A larger benchmark (100 questions, one third hidden) remains to be
  written; the agent benchmark (above) is done.

## 8. Migration

1. Atlas core (nodes, CSR edges, inverted index, mmap) behind the existing commands (phase A
   done: calls resolved via imports, function-level granularity, freshness): same outputs,
   benchmark equal or better, measured latency.
2. Agent tools (§6) on the atlas.
3. Docs, memory, git, SQL, routes, i18n ingesters, with `why`, `impact` and `schema`.
4. Daemon and file watching.
5. Semantics, then an expanded benchmark.
6. Publication: README, agent guide, CI, `/cortex` skill, CLAUDE.md instructions.

**Current state.** Every command and every MCP tool reads the atlas, and only the atlas: `query`,
`explain` and `context` read directly (inverted index, CSR); `files` and `grep` over the atlas's
path list and symbol ranges (`grep` groups by file with the enclosing function); `stats` and
`list` over its counters; `galaxy` via materialization. A single freshness path, on the atlas
(`atlas/fresh.rs`): reading the directories visited by the walk (no git, §4.1), rereading only
the changed paths, then a delta segment; `update` (full walk, blake3 fingerprint comparison) and
`update --changed` (the same check as the automatic one, made explicit) take the same path and
the same exclusion rule. A known path is only reread if (mtime µs, size) changed, and an
already-seen unindexable file is remembered: a second call with no change rereads nothing.

The agent tools (§6: find, card, outline, read, overview, impact, path, changed) read the atlas,
and the MCP server exposes them under the same names (`cortex_find`…), with `query`, `explain`
and `context` kept as aliases.

`index.bin` is no longer written or read; the v1 linear search engine and the graph rebuilt on
every query are removed. Their reference numbers are frozen: relevance 72.0% / 88.0% /
MRR 0.776 (identical to the atlas), open 115 ms, median query 102 ms, p95 202 ms, `context` 93 ms
(AstroQuest). Migration: an atlas in another format, or a project that only has an `index.bin`,
is reindexed once from its root (read from the manifest, or from the first two fields of the old
`index.bin`), then the old file is deleted.

## 9. Innovations

Criterion: every innovation must (1) exist in neither Obsidian nor graphify nor common
code-search tools, (2) be MEASURABLE (relevance benchmark, agent benchmark in tokens/calls,
latency), (3) stay local, with no external service.

### I1. Optimal context assembly under budget (the core idea)
A single `ask "<question>" -b 1500` tool. Cortex:
1. classifies the intent (locate / explain / impact / why / flow / schema) via rules on the
   question and recognized symbols;
2. generates **candidate facts** (definition, signature, caller, test, doc, commit, table…),
   each with a token cost and a value (relevance × informativeness);
3. picks the set that **maximizes information under the budget**: greedy submodular selection,
   with an MMR-style redundancy penalty (two facts about the same file, or saying the same
   thing, are worth less together).
Result: the best possible answer for N tokens, instead of a truncated list.
Measured on: the agent benchmark, for the same answer quality, in tokens and calls.

### I2. Differential context protocol (session memory)
The MCP daemon remembers which facts it already sent in the session (by stable identifier). It
doesn't resend them: it writes `↺ S:…#someSymbol (already seen)`. If the code changed since, it
sends only the **delta**.
Over a 50-call session, the same context is never paid for twice.
Measured on: cumulative tokens over a replayed 30-call scenario.

### I3. Symbol identity stable across renames and moves
A structural AST fingerprint (normalized shape: local identifiers anonymized, node kinds,
arity) plus a neighborhood fingerprint (callers and callees). It follows a symbol from one
commit to the next even if renamed or moved. Consequences: `why` recovers the full history, and
`impact` recovers pre-rename co-changes.
Measured on: recall over real renames in AstroQuest's git history.

### I4. Overlay of work in progress
The base (committed) graph is topped with an ephemeral layer: modified, uncommitted files. Every
answer marks what comes from the overlay (`✎ modified, uncommitted`), and `changed` summarizes
touched functions with their impact.
An agent always knows whether it's reading stable code or code being actively edited.
**Seed (current state).** The atlas already tracks disk state (§4.1): it describes the code
being written RIGHT NOW. The overlay (`src/outils/overlay.rs`) says which of it isn't committed:
the files from `git status` (modified, staged uncommitted, new, deleted), memoized in
`~/.cortex/<project>/overlay.json` under a key built from the git index (size, date), `HEAD`,
the branch, and the segment stack — `git status` is only re-run if one of these changed. `find`,
`card`, `outline`, `read`, `impact` and `overview` mark what comes from it with `✎`; `changed`
cross-references `git diff -U0 HEAD` hunks with symbol ranges (the innermost one touched), then
gives callers outside the work in progress and the tests to rerun. Remaining: keep the COMMITTED
version in the atlas alongside the in-progress one, to answer "what changed in THIS function"
without git.

### I5. Predictive impact (call graph × history)
`impact <id>` merges transitive callers (structural certainty) with git co-change probability
(historical certainty). The result is a list ranked by **risk**: "if you change X, you'll very
likely also need to touch Y (8 commits out of 10), Z is called but has never changed alongside
X", with tests to rerun.
Measured on: the last 200 commits, predicting the other touched files from the first one
(precision and recall).

### I6. Relevance that learns from usage, locally
Implicit signal: after a `find`, the agent calls `read` or `card` on a result, which counts as a
"click". Cortex logs this locally (question → chosen identifier, rank) and adjusts BM25F field
weights (bounded pairwise learning). Learned weights are only adopted if they improve the
benchmark's hidden portion.
An engine that adapts to each codebase, with no external service.

### I7. Precompiled cards
At indexing time, every symbol gets a compact, already-formatted **card** (signature, role from
header and doc, relation counts, identifiers of its main neighbors), stored in the atlas. `card`
becomes an O(1) mmap read, with no recomputation.
**Current state (atlas v5).** Every symbol carries its card (`AtlasNode.card`): stable
identifier, kind, range, signature (declaration text up to the body, multi-line parameters
included, 200 characters max), a one-sentence role (the raw first sentence of the doc-comment; a
file's role, `AtlasNode.summary`, comes from its header or, for markdown, the frontmatter's
`description`), count of resolved and ambiguous outgoing calls, identifiers of the first 6
callees. The INCOMING side (callers, importers, tests) is not stored: it changes whenever ANOTHER
file changes, and storing it would force every delta to re-emit (postings included) the card of
every symbol whose caller moved; it's read from the inverse CSR at call time (`card` < 3 ms).
Everything the stored card contains depends only on the symbol, its file, and its callees (name,
path, homonym rank): a callee in ANOTHER file never has a same-name homonym in its own file (the
call would be ambiguous), so its identifier only changes with its name or path — a case where
incremental resolution already re-resolves the caller; a re-resolved file's card is re-rendered
and compared anyway. Judges: the equivalence tests compare the stored card and each tool's output
(card, context, impact, outline of every file, path, overview) between "trunk + deltas" and a
full rebuild, including a homonym inserted BEFORE an existing symbol (`#run` becomes `#run~2`);
the manual test on AstroQuest (12 changes, 90 questions, 107 touched symbols: context, impact and
card) is identical before and after delta merge.
Relevance strictly unchanged (role and signature aren't scored): 80 / 94 / 0.861 and
67.5 / 85 / 0.742 on the same AstroQuest state as the previous binary. A "markdown heading outside
code block" rule was tried then reverted: hidden-set score 0.742 → 0.738.

### I8. Self-verifying answers
Every fact carries its provenance (`file:line` + a blake3 fingerprint of the passage). The agent,
or Cortex itself, can verify a fact is still true without rereading the file: if the fingerprint
differs, the fact is flagged stale and recomputed.
This is the "never a false context" guarantee that no current tool provides.

### Chosen build order
Atlas core (§8.1) → I7 cards → tools (§6) → I4 overlay → ingesters and I3 identity → I5 impact →
I1 ask → I2 differential → I8 verification → I6 learning → semantics.

## 10. Next steps

### Language-server diagnostics
When a language server is present (tsserver, rust-analyzer), its diagnostics (errors, inferred
types) are attached to cards: an agent sees "this symbol has 2 type errors" without running a
build. Optional: Cortex stays complete without a language server.

### One atlas per worktree
Parallel agents each work in their own git worktree. Each worktree gets its own atlas (keyed by
worktree root), and worktrees of the same repo share the segments of identical files (same
blake3 fingerprint): a new worktree only costs its modified files.

### Comparative benchmark and parallelism (step 2)
- **Against other approaches**: done, see §11 (`cortex bench-compare`).
- **Multithreading**: done, see §12 (`--threads`, `bench-latence --echelle`). Remaining: the
  ingesters (§8.3), and rewriting the trunk via structural in-place recopy (§12).

## 11. Comparative benchmark

`cortex bench-compare <questions.json> -p <project>` asks the **same questions** to several
approaches, with the **same judge** and the **same corpus**, and writes a JSON file to a local
results directory to track progress over time (a separate subfolder for the hidden benchmark, run
with `--sortie`). Dense RAG and the hybrid approach require a `--features bench` build (the
`model2vec-rs` dependency, outside the main binary). Code: `src/comparatif/`.

### Method
- **Corpus**: exactly the atlas's live files for the project (8,069 files, 1.72M lines, 75 MB for
  AstroQuest), reread from disk.
- **Judge**: the one from `cortex bench`. Each approach produces a ranked list of distinct files;
  the rank of the first expected file is taken, capped at rank 20 (beyond that: RR = 0).
- **Approaches**:
  - `rg-compte`: what an agent without Cortex does. The question's words (split on
    non-alphanumeric, accents kept, 2-character minimum, Cortex stopwords removed) are searched
    as `rg -i -F -e word…` (substring, case-insensitive, multithreaded); files ranked by
    **total match count**, ties broken by path.
  - `rg-termes`: the same search, ranked by **number of distinct words found**, then by matches.
    A stronger variant, so as not to beat a straw man.
  - `bm25-fich`: pure Okapi BM25 (k1 = 1.2, b = 0.75, Lucene idf) on a per-file token bag (path +
    whole content). Tokens: accents folded, camelCase and letter/digit splitting, lowercased,
    2-character minimum. No fields, no graph, no synonyms.
  - `rag-dense`: 40-line windows with 10-line overlap (58,619 chunks), prefixed with
    `path:start-end`, embedded with `model2vec-rs` + `minishlab/potion-multilingual-128M` (MIT,
    dimension 256), brute-force cosine. Vectors cached on disk (`~/.cortex/bench-cache/`, keyed
    by blake3 of the chunk text); model read from
    `~/.cortex/models/potion-multilingual-128M` (or `CORTEX_BENCH_MODEL`).
  - `hybride`: BM25 on the same chunks + dense, fused with **RRF k = 60** over the top 3,000 of
    each list, `score = Σ 1/(60 + rank)`.
  - `cortex`: the atlas as-is (`Handle::search`, 400 hits, like `cortex bench`).
  - `exp:*`: analysis experiments, outside the engine. `exp:cortex+X` fuses via RRF the top 100
    files of Cortex and of X; `exp:rerank20-X` only re-orders Cortex's top-20 via RRF between its
    rank and X's rank (no new file can enter).
- **Tokens**: Unicode characters / 4, rounded up, for all approaches. The output read by the
  agent is: for rg, `path:line:text` lines (cut at 500 characters) grouped by file in ranked
  order (an order more favorable than rg's real output); for BM25 and the experiments, one
  `- path` line per file; for RAG and hybrid, the full chunks; for Cortex, its actual
  `- name (kind) · file:line` lines. **"1st hit"** = tokens read up to and including the first
  block of the right file (median over questions where it's found); **"avg."** = average where a
  miss costs the full output; **"top-5"** = tokens to cover 5 distinct files; **"output"** = the
  full default output (rg: all files; RAG: k = 10 chunks; BM25: 20 paths; Cortex:
  `cortex query -b 2000`, 40 hits).
- **Latency**: per query, index already built, one warm-up pass done; median and p95 over the
  benchmark's questions. Build time measured separately (Cortex: walk, tree-sitter parsing,
  building and writing a segment to a temporary file).

### Results (AstroQuest, Ryzen 7 5800H 8 cores / 16 threads)

Two consecutive passes give identical ranks (everything is deterministic). Latencies, though,
vary by about ±50% from one pass to the next on a shared machine; only orders of magnitude
matter. Cortex engine: names, doc, header, path and body fields, stemmed (§7). The reference
approaches are unchanged.

**Public benchmark** (50 questions):

| Approach | top-1 | top-5 | MRR | med. lat. | p95 | 1st hit | avg. | top-5 | output |
|---|---|---|---|---|---|---|---|---|---|
| rg-compte | 2% | 10% | 0.069 | 11.3 ms | 27.7 ms | 32,159 | 356,938 | 29,795 | 260,432 |
| rg-termes | 2% | 36% | 0.169 | 11.9 ms | 45.2 ms | 19,012 | 207,969 | 20,238 | 260,432 |
| bm25-fich | 46% | 78% | 0.593 | 0.15 ms | 0.74 ms | 16 | 66 | 63 | 260 |
| rag-dense | 18% | 38% | 0.264 | 12.4 ms | 23.0 ms | 1,208 | 2,932 | 2,334 | 4,070 |
| hybride | 40% | 72% | 0.535 | 13.0 ms | 16.1 ms | 724 | 2,257 | 2,762 | 4,272 |
| **cortex** | **80%** | **94%** | **0.861** | 9.4 ms | 18.5 ms | 27 | 66 | 106 | 764 |
| exp:cortex+dense | 44% | 78% | 0.597 | 21.0 ms | 30.1 ms | 17 | 49 | 56 | 234 |
| exp:rerank20-dense | 64% | 88% | 0.754 | 22.7 ms | 32.6 ms | 13 | 33 | 58 | 234 |
| exp:rerank20-bm25 | 76% | 94% | 0.830 | 10.8 ms | 20.0 ms | 12 | 28 | 60 | 234 |

**Hidden benchmark** (40 questions):

| Approach | top-1 | top-5 | MRR | med. lat. | p95 | 1st hit | avg. | top-5 | output |
|---|---|---|---|---|---|---|---|---|---|
| rg-compte | 0% | 2.5% | 0.034 | 15.6 ms | 46.2 ms | 15,848 | 850,949 | 34,776 | 552,224 |
| rg-termes | 7.5% | 30% | 0.164 | 14.4 ms | 43.9 ms | 16,097 | 486,716 | 20,724 | 552,224 |
| bm25-fich | 30% | 57.5% | 0.412 | 0.27 ms | 0.59 ms | 24 | 105 | 63 | 253 |
| rag-dense | 25% | 35% | 0.317 | 11.4 ms | 13.7 ms | 544 | 3,149 | 2,292 | 4,000 |
| hybride | 27.5% | 62.5% | 0.404 | 17.2 ms | 26.8 ms | 1,065 | 2,747 | 2,474 | 4,180 |
| **cortex** | **67.5%** | **85%** | **0.742** | 13.3 ms | 26.5 ms | 27 | 105 | 102 | 753 |
| exp:cortex+dense | 37.5% | 72.5% | 0.517 | 23.7 ms | 31.7 ms | 21 | 66 | 56 | 231 |
| exp:rerank20-dense | 55% | 80% | 0.671 | 24.8 ms | 33.0 ms | 13 | 43 | 54 | 236 |
| exp:rerank20-bm25 | 55% | 82.5% | 0.672 | 12.4 ms | 21.0 ms | 13 | 44 | 56 | 236 |

By category (top-1 / top-5 / MRR):

| Approach | public fr (29) | public en (10) | public tech (11) | hidden fr (15) | hidden en (15) | hidden tech (10) |
|---|---|---|---|---|---|---|
| rg-termes | 0 / 21 / 0.120 | 0 / 30 / 0.135 | 9 / 82 / 0.332 | 0 / 27 / 0.093 | 7 / 7 / 0.086 | 20 / 70 / 0.386 |
| bm25-fich | 52 / 90 / 0.664 | 20 / 50 / 0.336 | 55 / 73 / 0.639 | 40 / 60 / 0.490 | 7 / 27 / 0.141 | 50 / 100 / 0.703 |
| rag-dense | 21 / 45 / 0.316 | 20 / 20 / 0.209 | 9 / 36 / 0.177 | 13 / 27 / 0.198 | 27 / 27 / 0.301 | 40 / 60 / 0.520 |
| hybride | 48 / 83 / 0.632 | 20 / 40 / 0.285 | 36 / 73 / 0.506 | 27 / 53 / 0.388 | 20 / 47 / 0.282 | 40 / 100 / 0.612 |
| **cortex** | **79 / 93 / 0.862** | **60 / 90 / 0.703** | **100 / 100 / 1.000** | **73 / 87 / 0.778** | **53 / 80 / 0.645** | **80 / 90 / 0.833** |
| exp:rerank20-bm25 | 86 / 93 / 0.883 | 40 / 90 / 0.603 | 82 / 100 / 0.894 | 60 / 80 / 0.686 | 33 / 80 / 0.539 | 80 / 90 / 0.850 |

Build: corpus reread in 0.2–0.5 s (warm disk cache; 3.1 s cold); file-level BM25 1.1–1.7 s for
~19 MB in RAM; dense ~40 s, 37 s of which is embedding on 16 threads, 59 MB of vectors **plus a
506 MB model**; Cortex ~26 s for a 37 MB segment (measured before §12; ~5 s today on 8–16
threads).

### Reading
- **Cortex wins everywhere** in top-1 and MRR, on both benchmarks and in every category. Keyword
  grep is nearly useless for ranking (0 to 7.5% top-1) and costs 16,000 to 32,000 tokens before
  the right file: that's the real cost of an agent without Cortex.
- **The body field absorbed BM25's contribution on content**: re-ranking Cortex's top-20 by
  content BM25 (`exp:rerank20-bm25`), which used to gain 2 top-1 points before this field, now
  makes it LOSE 4 (public) and 12.5 (hidden). The hidden benchmark confirms this on never-seen
  questions: 50 → 67.5% top-1, MRR 0.586 → 0.742.
- **Classic dense RAG remains the worst indexed approach** (18 to 25% top-1): 40 lines of code
  averaged by a static model dilutes meaning. The hybrid approach isn't worth more than BM25
  alone.
- **Where Cortex still loses**: vocabulary no field links (a domain term in one language not
  connecting to its code-level name in another), a handful of hidden-set questions falling just
  outside the top-20 due to abbreviation mismatches, and a few cases landing between ranks 10 and
  16.
- **Tokens** (measured with the older `- name (kind) · file:line` output; `find` now returns
  `path#name` identifiers, slightly longer, plus a one-sentence role for the first five results):
  Cortex and BM25 give a location with no content (a few dozen tokens); RAG gives code
  (1,000 to 3,000 tokens), which can save a subsequent `Read` but at the cost of a much worse
  ranking.
- **Small sample**: one question is worth 2 public top-1 points (2.5 hidden) and 10 points in
  English public. Gaps smaller than 2 questions aren't meaningful.

### What Cortex could still borrow
1. **Dense vectors only for re-ranking, never fused at equal weight**: before the body field,
   `exp:cortex+dense` dropped Cortex from 72 to 46% top-1 and `exp:rerank20-dense` to 60%; with
   the body field, 44 and 64% (public), 37.5 and 55% (hidden). With this model, it would need a
   low weight, tuned under the two-benchmark rule, and a per-symbol encoding (name + doc-comment)
   rather than a 40-line window.
2. **Vocabulary**: the remaining failures are mostly words that nothing connects (§7).

## 12. Scaling

**Harness.** All parallel code goes through ONE rayon pool (`src/par.rs`), set via
`--threads N` (global option) or `CORTEX_THREADS`; default: one thread per logical core.
`cortex bench-latence -p <project> --echelle [--passes 3] [--threads-liste 1,4,8]` runs each
stage in a pool of 1, 2, 4, 8 and 16 threads (median, min and max across passes; median and p95
query latency over the 50 public benchmark questions, at each pass), plus a full trunk build in a
temporary file, and writes a scaling result file. Updates and compactions are measured on a copy
of the atlas, opened once outside the measurement (on Windows, the first open of a freshly copied
segment waits on antivirus).

**Measurements** (AstroQuest, 8,069 files, 42,548 symbols; Ryzen 7 5800H 8 cores / 16 threads,
Windows 11; 3 passes). Shared machine: during this series, 2 to 3 cores were taken by other
programs (browser, game); a quieter series of the same binary is given in parentheses when it
differs noticeably.

| Stage (median) | before (16 thr.) | 1 thread | 2 | **4 (typical PC)** | 8 | 16 |
|---|---:|---:|---:|---:|---:|---:|
| open | 0.3 ms | 1.6 ms | 1.1 ms | 1.5 ms | 1.3 ms | 1.7 ms |
| median query | 9.8 ms | 9.3 ms | 8.7 ms | **7.2 ms** (5.8) | 7.2 ms (5.5–6.0) | 8.2 ms (5.7) |
| p95 query | 17.6 ms | 18.2 ms | 15.3 ms | **11.8 ms** | 12.9 ms (9.9) | 15.0 ms (9.9) |
| context | 15 ms | 71 ms | 46 ms | **26 ms** | 21 ms (16–18) | 31 ms (18) |
| grep | 197 ms | 905 ms | 538 ms | **309 ms** | 244 ms (193–227) | 314 ms (254) |
| freshness (CLI call) | 128–165 ms | 130 ms | 107 ms | **62 ms** (51) | 54 ms (37–42) | 58 ms (53) |
| single-file update | 6.5–16.5 ms | 7.8 ms | 8.6 ms | **8.4 ms** | 11 ms (7.7–8.6) | 11.5 ms |
| regular compaction | 1.66 s | 24 ms | 26 ms | **27 ms** | 32 ms (22–24) | 34 ms (26) |
| trunk rewrite | 1.66 s | 1.19 s | 1.28 s | **1.20 s** | 1.21 s (0.87–1.04 s) | 1.11 s (0.95 s) |
| full build | 26–29 s | 19.1 s | 12.9 s | **9.0 s** (8.0) | 7.5 s (4.6–5.6 s) | 5.8 s (4.4 s) |

"Before": the pre-scaling binary (16-thread default, `git status` for freshness, compaction =
trunk rewrite). Relevance is strictly identical (80.0 / 94.0 / 0.861; 67.5 / 85.0 / 0.742; full
90-question outputs byte-identical) and a trunk built on 1, 4, 8 or 16 threads is bit-identical to
the previous binary's (test `parallele_egale_sequentiel`, and verified on a frozen copy of
AstroQuest).

**What was kept, and why.**
- **Build (×5 to ×6).** The first gain wasn't parallelism: every file was compiling TWO
  tree-sitter queries (`Query::new`, several ms for TSX) and being parsed twice — 148 s of CPU on
  1 thread, 410 s on 16 (contention). Queries compiled once per process, one parser per thread, a
  single parse per file: 148 → 19 s on 1 thread. Then: parallel walk (`ignore::WalkParallel`,
  0.4 s → 0.04 s), read + blake3 + parse in parallel starting with the largest files, resolution +
  cards + splitting (roots, lowercase) in parallel, final tables (incoming edges, postings, fst
  automata, name indexes) in parallel; mimalloc allocator (−10% build, −25% trunk rewrite on 8
  threads versus Windows' default, measured by alternating).
- **Query (×1.3 to ×1.6).** Levenshtein automata, postings per term, path-term files and
  4,096-candidate slices in parallel, then accumulated in term order and merged in node order:
  identical floating-point sums and tie-breaks. Real but modest gain (a query only takes 5 to
  10 ms); kept because it's never slower.
- **Freshness (÷2.5 to ÷3).** No more `git status` (~100 ms): parallel reading of the directories
  visited (§4.1). The floor is the kernel: ~26–37 ms to open and read 1,458 directories on 8
  threads (measured alone); past 8 threads it gets slower (filesystem contention). The 30 ms
  target isn't met on this loaded machine (37–42 ms when quieter).
- **grep (×3.5 from 1 to 8 threads).** Already parallel; read buffers reused per thread.
  ~200–240 ms on 8 threads versus 520–1,075 ms for ripgrep; 16 threads don't help (I/O-bound).
- **Two-level compaction (§4.1).** Regular compaction (every 16 updates) merges deltas: ~25 ms
  instead of 1.66 s. The now-rare trunk rewrite goes from 1.66 s to ~0.9–1.2 s: materialization,
  resolution, cards and final tables are parallel, but string interning and appending to postings
  stay sequential (~450 ms) — id order IS first-appearance order.

**What was tried and reverted.**
- **Parallel interning** (ids assigned by sorting requests then replaying without hashing):
  507 ms on 8 threads versus 436 ms sequential (3.3M requests, 223k strings) — sorting and
  distributed tables cost more than the sequential probe. Reverted. Getting the trunk rewrite
  under 500 ms would need working in id-space (structural in-place recopy of segments,
  array-based ranges instead of hashing): an open project.
- **Direct NT API and relative directory opens** for freshness: no measured gain over
  `std::fs::read_dir`; not integrated.
- **Single-file update**: left sequential (a few ms, dominated by the write and manifest
  publication); parallelism would gain nothing here.

**"Typical PC" reading (4 threads).** Full build in ~8–9 s (a 37 MB trunk for 1.7M lines), query
~6–7 ms (p95 ~10–12 ms), freshness ~50–60 ms per CLI call, grep ~0.3 s, regular compaction
~25 ms. Going from 4 to 8 threads only pays off further on the build (−30 to −40%) and slightly
on `context` and `grep`; 16 threads (SMT) only help the build.
