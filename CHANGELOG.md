# Changelog

All notable changes to Cortex are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[semantic versioning](https://semver.org/) (pre-1.0: minor versions may break compatibility).

## [Unreleased]

## [0.7.0] — 2026-11-20

### Added
- **`cortex edit` / MCP `cortex_edit`: many edits on many files in ONE call** ("Code Mode" for any
  agent that has a shell, subagents included; `docs/EDIT.md`, `src/edit.rs`). Exact replacements
  (unique anchor unless `all`; falls back to whole-line matching tolerant to indentation and trailing
  spaces, re-indenting the replacement), line insertions before/after an anchor line, append, file
  creation, whole-symbol replacement by Cortex id (`S:path#symbol`: the callers to recheck are listed
  when the signature changes), edits scoped to a symbol, and a project-wide whole-word rename
  (`@ rename Old New`). Escape-free text format (`@ file`, `< old`, `> new`, read from a heredoc) or
  JSON. **Atomic**: every edit is located and checked before the first write, any error means no
  write and all errors are returned at once; a failed write rolls the written files back. CRLF/LF,
  BOM and final newline preserved per file, byte for byte outside the edited ranges. Guards: `.env*`,
  `.secrets/`, `.tauri/`, `.git/`, `.ssh/`, private keys, paths outside the root (links, 8.3 names,
  `:` streams) are refused with no way around; the project's `interdits` list, size limits and
  **syntax regressions** (tree-sitter, JSON) are refused unless `--forcer "<reason>"` (journaled).
  Journal with the previous bytes (`~/.cortex/edit-journal/`), `--annuler <id|dernier>`,
  `--liste`, `--essai` (dry run). The Cortex index is refreshed for the touched files.

## [0.6.0] — 2026-11-20

### Added
- **`cortex peek <file> ["<question>"]` / MCP `cortex_peek`**: reads the right spot of ANY big text
  file (vitest/cargo/server log, big JSON, big source), indexed or not, instead of all of it. With a
  question: the best-ranked numbered line windows (BM25 per line, question terms split like `find`,
  identifiers and paths searched as written, plus failure signals `FAIL`, `error[E…]`, `panicked at`,
  `AssertionError`, `×`/`✗`…), merged, deduplicated (digits ignored, « +N similar »), capped
  (`--max-lignes`, default 60; `-C` context). `--erreurs`: only failure blocks with test name,
  assertion, useful stack and a `locations : file:line` line (framework frames in `node_modules`
  collapsed). No question: summary (header, totals line, first failure blocks, end of file). Very
  long lines (minified JSON) are shown as excerpts around the terms. UTF-16 and CRLF files are read.
  Two parallel Aho-Corasick passes over the bytes (`src/peek.rs`); new dependency `aho-corasick`
  (already pulled by `ignore`).
- **`peek`: accents, more test runners, passing tests, Rust panics, `ask`.** Accents are folded
  in both directions: a French log with an unaccented question (`echoue` finds `échoue`, `échoué`)
  and the reverse, capitals, `ç`, `œ`; a log with no accented letter costs nothing (no extra
  pattern). Failure blocks and summary lines of pytest (`E   ` details, `FAILED x::y`,
  `N failed, M passed`), jest (`●` blocks, `Tests:`), `go test` (`--- FAIL`, `FAIL pkg`, `panic:`,
  goroutine stacks) and eslint (`12:5  error …` lines grouped under their file, `✖ N problems`), on
  top of vitest and cargo. When the question asks why something fails and the log holds real
  failures, a passing test line (`... ok`, `✓`, `PASS`) is no longer returned as an answer, whatever
  its name. Rust panics come with their message (`called `Option::unwrap()` on a `None` value`,
  ``assertion `left == right` failed`` with its left/right lines). `ask` hands a question that names
  a log file (`.log .txt .out .json`, or any file outside the index) to `peek`. On generated logs of
  20,000+ lines: pytest 678,732 bytes → 35 lines (1,986 bytes), eslint 1,383,437 bytes → 68 lines
  (4,513 bytes). The pytest 9.1.0 and eslint 9.39 fixtures are real output; the jest and `go test`
  fixtures were written from the documented format (those tools were not installed here).
- **`cortex bench-agent --tache peek`**: agent benchmark for "why does this test fail?", 10 tasks
  (vitest, cargo, pytest, eslint, go, a French log, three tasks on 20,000-line logs, a passing run),
  no index and no network. Full `Read` against `peek`, tokens = characters / 4: 10/10 tasks and
  32/32 expected facts; 1,075,835 → 5,197 tokens (×207 overall, ×677 on the three 20,000-line
  tasks, whose logs are generated with a fixed seed), 37 tool calls → 10. On short logs (19 to 86
  lines) the saving is only ×1.0 to ×1.6. The first version of `peek` scored 8/10 tasks (27/32
  facts) on the same tasks.
- **`read <id> --autour "<question>"`** (MCP `cortex_read`: `around`, `max_lines`): for a big symbol
  (more than 80 lines), its signature plus the ranked windows that answer the question, with
  absolute line numbers and `… N lines omitted`, capped at min(60, a quarter of the symbol) lines.
  Measured on 6 questions about symbols of this repository: the expected line was in the output
  6 times out of 6, for 3,022 tokens instead of 15,488 to read the whole symbols (×5.1). Small sample.
- **MCP freshness probe**: before answering, the server `stat`s the files the call names (path,
  `path:line`, identifier, text to search) and re-checks the project if one changed, instead of
  waiting for the periodic check: a file saved a second ago is seen. A named path the atlas never
  follows is not probed again for 5 seconds. 50 `cortex_find` calls on the 8,000-file private
  monorepo: median 36.9 ms against 36.3 to 38.3 ms with the previous binary (61.0 ms against 65.4
  to 66.8 ms when the question names a file), i.e. no measurable cost.
- **French questions** (`src/semantic.rs`, `src/stem.rs`, `src/search.rs`), without changing the index
  or the BM25F weights: a one-way French → code table (168 entries, at most 3 code words each; an
  English question is never widened) for everyday words the code does not contain (« requête »,
  « répertoire », « taille », « identifiant »…); base forms for conjugated and plural words
  (« déclarées », « gabarits »); locutions (« en-tête », « mise à jour »); French question stop
  words; negation (« sans X »: X is not searched); location verbs (« où est défini ») no longer
  treated as content words. Every entry was kept or dropped by measurement (ablation per component,
  then leave-one-out, then adding candidates one by one). `CORTEX_FR` turns them off to compare:
  `CORTEX_FR=` (empty) disables all, a list of letters keeps some (`l` base forms, `p` locutions,
  `s` stop words, `t` table, `n` negation, `w` location verbs; default `lpstnw`).
- **French benchmark `bench/public-fr/`**: 100 questions written blind in French on the same pinned
  flask, hono and ripgrep commits, dev 60 / test 40. Dev (the set the table was tuned on): top-1 /
  top-5 / MRR 65.0 / 88.3 / 0.746 → 73.3 / 95.0 / 0.820 (0.822 right after the French lot). Test, run twice: 47.5 / 82.5 / 0.624 →
  55.0 / 80.0 / 0.656 at the end of the French lot (top-1 +3 questions out of 40, **top-5 −1 question**),
  then 55.0 / 82.5 / 0.661 with the release binary (top-5 unchanged); the French tables were not touched
  in between, so the test set is no longer an untouched control. The gain is real but smaller than on
  dev, which is where it was tuned. Replay: `cortex bench bench/public-fr/<file>.json
  --project bench-<repo>`.
- **Symbols for SQL, YAML, TOML and JSON files** (`src/confsym.rs`): hand-written scanners, no grammar
  and no dependency (tree-sitter 0.22 blocks the recent ones), no index format change; such a file
  used to yield no symbol. SQL: tables and their columns, views, functions, procedures, triggers,
  indexes, enum and domain types, RLS policies, extensions, schemas (`find "policy RLS on table X"`
  finds the migration). YAML (depth 3 at most): GitHub Actions, docker compose, OpenAPI and
  Kubernetes files are recognised, one symbol per Kubernetes resource; TOML and JSON config keys.
  Lockfiles, generated files and big data files are skipped; sizes and symbol counts are capped.
  These symbols are ranked at ×0.5 unless the question names them (`table`, `policy`, `rls`,
  `migration`, `workflow`, `yaml`, `column`, `trigger`, `schema`…): reachable, but they do not push
  code out of the results. Private AstroQuest SQL benchmark (20 questions on 919 migrations, not
  reproducible outside the company): top-1 30% → 90%, top-5 45% → 100%, MRR 0.369 → 0.942. Cost:
  about +10.7 MB of index for 8,242 SQL symbols.
- **Extraction version** (`EXTRACT_VERSION`, now 2): the manifest records which extractor built the
  index. An index built by an older one is rebuilt ONCE from its sources, automatically, on the next
  call (`find`, MCP, `update`, `update-all`), with one line on stderr. To be bumped whenever a change
  makes a file yield other symbols.
- **Kotlin** (`.kt`, `.kts`): classes, interfaces, objects, enums, functions and extension functions,
  top-level and `const` properties, `typealias`, calls, parent types, imports (`src/kotlin.rs`,
  tree-sitter-kotlin 0.3.8, MIT). The release binary grows by 4,040,192 bytes (25,609,728 →
  29,649,920, +15.8%): the grammar table. On Clikt (Apache-2.0, 146 `.kt` files), 8 questions written
  before the run: top-1 4/8, top-5 7/8, MRR 0.682 (small sample). The public benchmark is identical
  before and after this step.
- **Cortex Web (EXPERIMENTAL; Cargo feature `web`, disabled by default)**: documentation and web
  search for agents, built with `cargo build --release --features web`; without the feature the
  binary and the MCP tool list are unchanged. CLI:
  `cortex web add|update|list|find|read|fetch|search|ingest|lib|bench`; MCP: `cortex_web_lib`,
  `cortex_web_find`, `cortex_web_read`, `cortex_web_fetch`, `cortex_web_search`. Guarded fetching (private-address
  refusal, size caps, manual redirects, header allow-list, robots.txt per RFC 9309, page text handed
  to the agent as untrusted data), incremental crawl at one request per second per host, extraction
  for 10 documentation families, `lib` reads the exact version from the project's lockfile, results
  link back to the project's symbols. Design, security model and measurements:
  [docs/WEB.md](docs/WEB.md). Documentation benchmark (10 real doc sites, 100 questions written
  blind, dev 60 / test 40, test run once): page top-1 on test 72.2% (the current `cortex docs`
  base: 22.2%), top-5 86.1%, median answer about 535 tokens; **section top-1 only 36.1%** (known
  weak point). It answers « nothing reliable » when no collected page covers the question: 3 of 4
  unanswerable test questions refused, 2 of 36 answerable ones refused by mistake (both docsify).
  Limits: pages that need JavaScript are not read (docsify returns nothing); DuckDuckGo answered
  HTTP 202 (a verification page) in a real test, so keyless general search falls back to Hacker News
  and Wikipedia; the abstention threshold rests on 6 negative questions. Not ready for production use.

## [0.5.3] — 2026-11-08

### Added
- **Rust call graph** (`src/rust_refs.rs`, on the tree already parsed for symbols). Until now a Rust
  file only contributed a line scan of its `use` lines: no call was recorded, `card` said "called by 0"
  and `impact` "no resolved caller" for every Rust symbol. Recorded now: calls `f()`, `a::b::f()`,
  methods `x.f()` (standard-library methods dropped unless called on `self`), `f::<T>()`, calls inside
  macro arguments (`assert!(f(x))`); type references (signatures, fields, patterns, struct
  expressions, `SimEvent::Hit` → `SimEvent`, `Type::new()` → `new` and `Type`), constants in capitals.
  Enum variants (`Mode::Search(x)`) and standard types are not references.
- **Complete `use`**: `pub use` and `pub(crate) use` (kept as re-exports), nested groups
  `a::{b::{C, D}, E}`, `self`, `*`, `as` aliases, multi-line declarations; `mod x;` imports its file;
  `self::`/`super::` inside an inline `mod tests { … }` are brought back to the file's module.
- **Rust paths resolved to files**, from file paths only (no Cargo.toml): `crate::` (deepest `src`
  folder of the file, else every parent folder), `self::`, `super::`, child modules (2018 paths) and
  workspace crates (`rs:<crate>/<module>` key of `<dir>/src/<module>.rs`, crate = folder name with
  `-` → `_`, also the part after the first `_`: `grep_searcher` → `searcher`); `a/b.rs`, `a/b/mod.rs`,
  `lib.rs`; the longest existing prefix wins. **Re-exports are followed**: `use game_sim::SimEvent` goes
  through the `pub use world::{SimEvent, …}` of `lib.rs` to `world.rs` (3 hops at most), and every file
  crossed counts as imported.
- Resolution rules for Rust files: an `impl Foo` block yields to the `struct`/`enum`/`type Foo` (for
  every language, like a C++ prototype yields to its definition); case class (`Foo` never targets
  `fn foo`); several homonyms in the caller's own file are ambiguous instead of "the first one".
- Incremental update stays exact: when a file with re-exports changes, is re-resolved, or a Rust file
  disappears, its Rust importers are re-resolved too (test `rust_reexport_chain_matches_full_rebuild`,
  plus a two-crate workspace fixture in the tool tests).

### Changed
- Atlas format v6 (`reexports` persisted, `rs:` keys in the stem index): existing atlases are rebuilt
  on first open; run `cortex update-all` once.
- `site_appel` (call line shown by `card`/`impact`) prefers an exact-case match.
- `overview` lists a Rust import under its crate name (`serde`), never `crate`/`self`/`super`.

### Benchmarks (same machine, 0.5.2 binary then 0.5.3 binary, same commands, frozen source snapshots)
- Public benchmark (`bench/public/run.sh`): flask 65.0 / 95.0 / 0.788, hono 70.0 / 90.0 / 0.774,
  ripgrep 55.0 / 75.0 / 0.648 — unchanged, identical rank for every question. Private: `queries.json`
  78.0 / 90.0 / 0.841, `holdout.json` 54.1 / 73.0 / 0.625 — unchanged, identical ranks.
- Callers found (grep as ground truth): private Rust game workspace `apply_damage` 0 → 3 (3/3), `take_control` 0 → 2
  (2/2), `SimEvent` 0 → 57 direct users in 24 files + `lib.rs` (every file that names it); ripgrep
  `build_parallel` 0 → 4 (4/4), `search_parallel` 0 → 1 (1/1), `GlobSet` 0 → 14; Cortex `impact` 0 → 2
  (2/2), `spec_base` 0 → 3 (3/3). Name-level false positives: 0 of 738 call sites over 240 random
  symbols (ripgrep, Cortex, a private Rust game workspace).
- `bench-latence --echelle` (16 threads, median of 5): full build ripgrep 253 → 268 ms, Cortex 136 →
  159 ms, private game workspace 162 → 160 ms; query p50 / p95 unchanged within noise; update of one file 7–12 ms
  before and after. `card` / `impact` on Rust symbols are now measured: 0.9–1.4 / 1.0–1.2 ms median.
  Atlas: ripgrep 2.03 → 2.73 MB.

### Known limits
- Method calls are resolved by name, without type inference: `Regex::builder().build(p)` may point to
  the project's only `build`. `Type::new()` keeps `new` and `Type` as two references (the model has
  simple names only), so `new` is often ambiguous.
- Crate names come from folder names; `#[path]`, `include!` and macro-generated items are not seen.
- `impact` at depth ≥ 2 follows type references through struct fields and can be broad.

## [0.5.2] — 2026-10-29

### Added
- **Lua support** (`.lua`, tree-sitter-lua 0.1, Lua 5.1 of Recoil / Beyond All Reason): `function
  widget:DrawWorld()` is the method `widget:DrawWorld` (same for `gadget:GameFrame`...), `function
  M.foo()`, `local function foo()`, `M.foo = function()` and `local M = { run = function() end }` are
  functions (`M.foo`, `foo`, `M.run`), `return { armck = { ... } }` (definition tables of `units/*.lua`)
  gives one `struct` per top-level key, a big top-level `local cfg = { ... }` is a `const`. Call graph:
  `obj:foo()`, `M.foo()` and `foo()` all resolve on `foo`; `Spring.*`, `gl.*`, the standard library and
  local aliases of engine functions (`local GetUnitPosition = Spring.GetUnitPosition`) are not recorded
  as calls. `VFS.Include("a/b.lua")`, `include(...)`, `require(...)` are imports. `--` comments, `---@`
  annotations and `--[[ ]]` blocks feed roles and file headers; for widgets and gadgets the `name` and
  `desc` of `GetInfo()` stand for the missing header comment.
- **GLSL support** (`.glsl .vert .frag .geom .comp .tesc .tese`, tree-sitter-glsl 0.1.9): functions
  (with call graph), `struct`, `uniform` (text pass, robust to `#if` / `#line` / interface blocks) and
  `#define` with a value. Shaders written in `[[ ... ]]` strings inside Lua are parsed too, at their real
  line numbers in the Lua file.
- **Go, Java and shell** (`.go`, `.java`, `.sh .bash .zsh`, tree-sitter-go / java / bash 0.21): symbols
  only (functions, methods, types, structs, interfaces, constants; classes, interfaces, enums, records,
  methods, constructors; shell functions), no call graph yet.
- `graph::call_key` also cuts qualified names at `.` and `:` (names without spaces), so Lua names match
  the calls that target them.

### Notes
- New `Lang` codes in the atlas (Lua 16, GLSL 17, Go 18, Java 19, shell 20): older atlases stay valid;
  run `cortex update-all` once to index the newly supported files of existing projects.
- Grammar constraint: tree-sitter stays at 0.22. The next series of these grammars (lua 0.2+, glsl 0.2,
  go / java / bash 0.23+) go through `tree-sitter-language` and need tree-sitter >= 0.23.

### Benchmarks
- Private benchmark `queries.json` (AstroQuest, 50 questions): 78.0% / 90.0% / MRR 0.841 before and
  after (0.5.1 binary then 0.5.2 binary, on the same index; also unchanged after re-indexing AstroQuest
  with its new Lua, shell and Java files: 18,248 -> 18,376 files, 170,754 -> 170,963 symbols).
- `bench-latence -p AstroQuest`, 0.5.1 then 0.5.2 binary on the same index, back to back (shared machine,
  noise of about 1 ms): query median 11.06 -> 11.59 ms, p95 18.47 -> 19.25 ms, find 11.23 -> 12.14 ms,
  card 4.26 -> 4.37 ms. Earlier run on the live index: 10.97 / 21.11 ms (0.5.1) against 10.60 / 20.79 ms (0.5.2).
- Beyond All Reason (indexed from its game folder): 270 files, 3,077 symbols, 0.32 s before; 2,486 files
  (2,143 Lua, 68 GLSL), 922,926 lines, 24,200 symbols, about 2.0 s after.
- Binary: 22.1 MB -> 25.1 MB.

## [0.5.1] — 2026-10-12

### Added
- **Project profile**: an optional `.cortex/profile.toml` at the project root, read on every
  `find` / `ask` without re-indexing and without effect when absent: `[synonyms]` (project
  vocabulary, applied to question words that are rare in the project), `[boost]` / `[penalty]`
  (folder multipliers), `[ignore]` (generic question words), `[entry]` (entry files).
  `cortex profile init` writes a skeleton from folder names, README and usual entry files;
  `cortex profile check` lists what was understood.
- Private benchmark `cache2.json` (40 vague questions written blind, never used to tune).

### Changed
- Relevance (merged from the unreleased generalisation branch, all measured not to lower the
  private benchmarks): tooling folders (`.claude`, `.github`, fixtures, mocks) are penalised unless
  the question is about tooling; a role named in the question (page, server, hook, store)
  favours files of that role; a general FR/EN app lexicon expands question words that are rare in
  the project; question filler words are ignored.
- Relevance: for `foo/index.ts`, `foo/mod.rs`, `foo/__init__.py`, `foo/lib.rs`, the folder name is the
  file's identity (path terms that name the folder weigh 0.7 x idf more); a file that is also at the
  head of the body-only BM25 ranking gets a decaying bonus (`1 + 0.1 / rank`).
- Single-letter question tokens (French elisions `l'`, `d'`, `qu'`) are ignored.
- New environment overrides: `CORTEX_IDX_DIR`, `CORTEX_BODY_RR`, `CORTEX_BODY_N`,
  `CORTEX_NO_PROFILE`, `CORTEX_PROFILE_SYN`, `CORTEX_PROFILE_GATE`.

### Benchmarks
- Public benchmark with this branch, one measurement, not used for tuning: 63.3% / 86.7% /
  MRR 0.737 on the 60 questions (0.5.0: 56.7 / 86.7 / 0.702). See docs/BENCHMARKS.md section 3
  for the private benchmarks, the history by version and the profile measurements.
- Public benchmark re-run with Cortex 0.5.0 (60 questions): 56.7% top-1, 86.7% top-5, MRR 0.702
  (0.3.0: 55.0% / 88.3% / 0.685). Head to head with graphify 0.8.38 on the same questions and judge:
  best of five settings 13.3% / 35.0% / 0.238 (default 10.0% / 30.0% / 0.204), against 63.3% / 86.7% /
  0.737 for Cortex `main` (56.7% / 86.7% / 0.702 for the released 0.5.0); ~570 ms per question against
  ~24 ms, and no fresh index after an edit. Rerun with `bench/public/graphify-bench.py`.

## [0.5.0] — 2026-10-08

### Changed
- Search relevance: a file that covers ALL the words of the question (path, symbol names, doc-comments,
  header; synonyms count for the word they come from) now ranks ahead of one that matches a single word.
- Files without any symbol (inline server functions, top-level scripts) are now returned as `F:` results
  instead of being invisible to `find`.
- Possessives and time prepositions (`my`, `own`, `after`, `mon`, `apres`...) are ignored in questions.
- A few more FR/EN glossary pairs (text, native, hidden, transfer, automatic, reminder forms).

### Added
- `cortex ask "<question>" -b <tokens>` / MCP `cortex_ask`: one call assembles the answer (rule-based intent, candidate facts with token cost, greedy selection under the budget with a redundancy penalty); `bench-agent` gains an `ask` arm.

## [0.4.0] — 2026-10-07

### Added
### Added
- `cortex ui` / MCP `cortex_ui` — WHERE IS THIS UI? From what is visible on screen (button text,
  `data-testid`, `aria-label`, id, React component names) to the code that renders it. The visible text is
  resolved through i18n files to its KEY, then to the places that use the key; comments, docs and tests are
  ignored; design-system primitives (`components/ui/`) rank behind real screens. ~270 ms on a 7 500-file
  project. Used by Spectra's `browser_source`.
- **C and C++ support** (`.h .hpp .hh .hxx .inl .ipp .tpp .c .cc .cpp .cxx`, tree-sitter-cpp
  0.22): classes, structs, unions, enums, namespaces, functions and methods with their
  **qualified name** (`AFoxMissile::Launch`, `FoxUi::BuildMenu`), `typedef`/`using`, `#define`;
  call graph (calls, `new`, `&Class::Method` delegate bindings, template type arguments, base
  classes), `#include "…"` as imports (resolved from the file's folder and each parent folder,
  `Foo.h` and `Foo.cpp` share a stem), `//`, `///`, `/** */` doc-comments and trailing `//`
  comments. **Unreal Engine** code parses cleanly: `UCLASS(...)`, `USTRUCT`, `UENUM`,
  `UINTERFACE`, `UFUNCTION(...)`, `UPROPERTY(...)`, `UMETA`, `GENERATED_BODY()`, `*_API`
  and `FORCEINLINE` are blanked out before parsing (same byte offsets and line numbers), and
  `DECLARE_*DELEGATE*` / `DECLARE_LOG_CATEGORY*` names become symbols.
- New symbol kind `decl` (a prototype without a body, usually in a header). The call graph
  prefers the definition to its prototype; `card` and `impact` show `defined in:` /
  `declared in:`, `card` borrows the header's comment as the role of the definition, `find`
  shows the definition (with the header's role) instead of listing both, and `read` of a
  prototype points to the definition.
- One-line installers: `install.sh` (Linux, macOS, Git Bash) and `install.ps1` (Windows). They
  download the release archive, refuse it unless its SHA-256 matches `SHA256SUMS.txt`, and
  install without administrator rights (`~/.local/bin`, `%LOCALAPPDATA%\cortex\bin`).
- README: animated terminal demo (light and dark themes), installation first, public benchmark
  table near the top; social preview image in `.github/`.

### Changed
- `cortex ui` runs in a companion binary, `cortex-ui`, built with `cargo build --release --features ui --bin cortex-ui`. `cortex ui` and the MCP tool `cortex_ui` call it; release archives and both installers ship it next to `cortex`. The main `cortex` binary no longer contains the UI locator.
- Resolution key of a qualified name is its last segment (`graph::call_key`): `Launch()` finds
  `AFoxMissile::Launch`. Names without `::` (every other language) are unchanged.
- Synonym groups: `airbase`/`aerodrome`/`aerienne`, `replicate`/`replication`/`repliquer`,
  `network`/`net`/`reseau`, `multiplayer`/`multijoueur`.
- **Existing indexes stay valid** (no format change). Projects that contain C/C++ files get
  them indexed on the next call (the freshness check sees the new files); run
  `cortex update-all` to do it for every project at once, then `/mcp` → reconnect in running
  agent sessions (an already running server keeps the old binary in memory).
- Release archives (from the next release): `.tar.gz` members no longer start with `./`.

### Fixed
- Search latency regression on the Windows build (the new UI locator code changed the compiled search path): the 40-question AstroQuest benchmark is back to 0.19 s, same as 0.3.0, with identical precision (top-1 57.5%, MRR 0.651).
- README: the manual `tar -xz … cortex` one-liners did not work with the 0.3.0 archives, whose
  members are named `./cortex`; they are replaced by the installers.

## [0.3.0] — 2026-09-27

First public release of **Cortex by AstroQuest**.

### Agent tools
- `find`, `card`, `read`, `outline`, `overview`, `impact`, `path`, `changed`, plus `grep`,
  `files`, `docs`, `list`, with the same names in the CLI and in the MCP server (`cortex_find`…).
- Stable identifiers `S:path#symbol` (`~k` for homonyms), `D:path#anchor`, `F:path`; inputs also
  accept names, paths, unique path suffixes, `path:line` and `path:start-end`.
- Outputs bounded by a token budget (`-b`), with line ranges, call sites, `✎` for uncommitted
  code and a final `suite :` line proposing the next call.
- Open specification of identifiers, tools and outputs: `docs/SPEC.md` (v0.1 draft).

### Engine
- Atlas storage: immutable memory-mapped rkyv segments plus small delta segments; opening an
  atlas takes under a millisecond; updating one modified file ~7 ms; two-level compaction.
- Automatic freshness on every call without git (directory reads, sizes and dates), same
  exclusion rules as a full walk (`.gitignore`, `.ignore`, git excludes).
- BM25F ranking over symbol names (camelCase/snake split, typo-tolerant via fst automata),
  paths, doc comments, file headers and file bodies; FR/EN stemming and an FR↔EN glossary.
- Call graph resolved through real imports at function granularity; ambiguous calls are counted,
  never guessed. Precompiled cards per symbol.
- Parallel build (one rayon pool, `--threads`/`CORTEX_THREADS`): ~5 s for 1.7 M lines.
- Languages: TypeScript/TSX, JavaScript, Python, Rust, C#, Markdown.

### Benchmarks
- Public reproducible benchmark `bench/public/`: 60 questions on flask, hono and ripgrep at
  pinned commits, written before any run of Cortex; `run.sh` fetches, indexes and compares Cortex
  with grep, BM25, dense RAG and hybrid RAG. Cortex 0.3.0: 55.0% top-1, 88.3% top-5, MRR 0.685.
- `cortex bench`, `bench-compare`, `bench-agent`, `bench-latence`; private benchmark files are
  located with `CORTEX_BENCH_DIR`.

### Distribution
- Crate `astroquest-cortex` (binary `cortex`), npm package `astroquest-cortex` (downloads the
  release binary, SHA-256 checked), Claude Code plugin and marketplace (`.claude-plugin/`),
  MCP registry manifest (`server.json`).
- Release binaries for Linux x86_64, macOS arm64 and x86_64, Windows x86_64 with SHA-256 sums.
- `cortex --version` prints `cortex 0.3.0 — by AstroQuest`.

### Changed
- The 3D viewer is embedded in the binary (no file to install next to it).
- `cortex infra` reads any number of servers from a `.env` (`<P>_HOST` or `<P>_IPV4`, optional
  `<P>_SSH_PORT`, `<P>_LOGIN`, `<P>_SSH_KEY_PATH`, `<P>_LABEL`) and never copies other keys.
- `CORTEX_HOME` overrides the data directory (`~/.cortex`).

[Unreleased]: https://github.com/AstroQuestStudio/cortex/compare/v0.7.0...HEAD
[0.7.0]: https://github.com/AstroQuestStudio/cortex/releases/tag/v0.7.0
[0.6.0]: https://github.com/AstroQuestStudio/cortex/releases/tag/v0.6.0
[0.5.3]: https://github.com/AstroQuestStudio/cortex/releases/tag/v0.5.3
[0.5.2]: https://github.com/AstroQuestStudio/cortex/releases/tag/v0.5.2
[0.5.1]: https://github.com/AstroQuestStudio/cortex/releases/tag/v0.5.1
[0.5.0]: https://github.com/AstroQuestStudio/cortex/releases/tag/v0.5.0
[0.4.0]: https://github.com/AstroQuestStudio/cortex/releases/tag/v0.4.0
[0.3.0]: https://github.com/AstroQuestStudio/cortex/releases/tag/v0.3.0
