# Changelog

All notable changes to Cortex are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[semantic versioning](https://semver.org/) (pre-1.0: minor versions may break compatibility).

## [Unreleased]

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

[Unreleased]: https://github.com/AstroQuestStudio/cortex/compare/v0.5.1...HEAD
[0.5.1]: https://github.com/AstroQuestStudio/cortex/releases/tag/v0.5.1
[0.5.0]: https://github.com/AstroQuestStudio/cortex/releases/tag/v0.5.0
[0.4.0]: https://github.com/AstroQuestStudio/cortex/releases/tag/v0.4.0
[0.3.0]: https://github.com/AstroQuestStudio/cortex/releases/tag/v0.3.0
