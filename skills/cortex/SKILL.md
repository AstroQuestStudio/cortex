---
name: cortex
description: Understand a codebase with Cortex before grepping or reading files. Use it FIRST to locate code (find), get a symbol's signature, callers, callees and tests (card), read exact lines (read), see what a change breaks (impact), trace how A reaches B (path) and summarise uncommitted work (changed). ~10x fewer tokens than grep + read.
---

# Cortex: code context for agents

Cortex keeps a local index ("atlas") of each project and answers the questions an agent asks
while exploring code, in milliseconds, with compact outputs. Use it **before** Grep, Glob or
reading whole files. Read a full file only when you are about to edit it.

## Which tool

| You want… | MCP tool | CLI |
|---|---|---|
| a whole answer in one call (locate, explain, impact, path, module), token-bounded | `cortex_ask` | `cortex ask "<question>" -b 600` |
| where is X? (natural language, English or French, or keywords) | `cortex_find` | `cortex find "<question>"` |
| what is this symbol: signature, role, callees, callers with call line, tests | `cortex_card` | `cortex card <id>` |
| the exact lines of a symbol, doc section or range; for a BIG symbol only the lines that answer a question | `cortex_read` (`around: "<q>"`, `max_lines`) | `cortex read <id>` · `--autour "<q>"` |
| what a file contains (symbols with ranges, imports, importers, exports) | `cortex_outline` | `cortex outline <file>` |
| how a folder works (roles, entry points, dependencies) | `cortex_overview` | `cortex overview <dir>` |
| what breaks if I change X, and which tests to rerun | `cortex_impact` | `cortex impact <id>` |
| how does A reach B (call chain with call lines) | `cortex_path` | `cortex path <a> <b>` |
| what did I change, what calls it, which tests | `cortex_changed` | `cortex changed` |
| exact text (error message, URL, i18n key, TODO) with the enclosing function | `cortex_grep`, `cortex_ui` (de ce qu'on voit à l'écran au code : texte, clé i18n, data-testid, composant) | `cortex grep "<text>"` |
| the right spot of a BIG text file (test/cargo/server log, JSON; indexed or not) without reading it all | `cortex_peek` (`errors: true` = failure blocks only) | `cortex peek <file> "<q>"` · `--erreurs` · no question = summary (accents either way; vitest, cargo, pytest, jest, go test, eslint) |
| the context of a TASK (likely files incl. what changed together in git, symbols, tests, docs, journeys) under a budget | `cortex_pack` | `cortex pack "<task>" -b 3000` |
| where an ERROR comes from: a stack (console, Node, vitest, Rust, Spectra JSON) to exact source symbols through source maps | `cortex_trace` (`pile`) | `cortex trace <file\|-> [--maps dist]` |
| which tests to run for the changed files (+ Spectra recipes touched) | `cortex_impact` (`tests: true`, `parcours`) | `cortex impact --tests [--parcours] [--liste]` |
| several agents, one mission: skip what a peer already read | `cortex_card` / `cortex_read` (`mission`, `agent`, `partage`), `cortex_registre` | `--mission M --agent A --partage` · `cortex registre` |
| a file by name fragment or glob | `cortex_files` | `cortex files "<fragment>"` |
| offline docs of a library you scraped | `cortex_docs` | `cortex docs query "<q>" --source <Lib>` |

Add `-p <Project>` (MCP: `project`) when several projects are indexed, and `-b <tokens>`
(MCP: `budget`) to change the output budget.

## How to read the outputs

- Every result carries a **stable identifier**: `S:<path>#<symbol>` (code symbol, `#name~2` for a
  second homonym in the same file), `D:<path>#<anchor>` (doc section), `F:<path>` (file).
  Copy it verbatim into the next call. Tools also accept a bare name, a path, a unique path
  suffix (`useAuth.ts`) or `path:line`.
- C and C++ (headers and sources, Unreal macros included): symbol names are qualified
  (`S:Source/Weapons/FoxMissiles.cpp#FoxFM::MissileSystem::Launch`; a bare `Launch` also works),
  `decl` is a prototype without a body (`card` and `impact` say `defined in:` / `declared in:`;
  `find` already shows the definition), data members are not symbols (use `grep`).
- Lua (Recoil / Beyond All Reason): `function widget:DrawWorld()` is the symbol `widget:DrawWorld`
  (a method); `M.foo`, `local function foo`, `M.foo = function` are functions; `return { armck = {...} }`
  (definition tables) gives one `struct` per top-level key; a big top-level `local cfg = {...}` is a
  `const`. A call `obj:foo()` / `M.foo()` resolves on its last segment (`foo`); `Spring.*`, `gl.*` and
  the standard library are not recorded as calls. `VFS.Include("a/b.lua")` and `require` are imports.
  GLSL (`.glsl .vert .frag .geom .comp`, and shaders written in `[[ ... ]]` strings inside Lua):
  functions, `struct`, `uniform` and `#define` with value. Go, Java and shell: symbols only.
- SQL, YAML, TOML and JSON files have symbols: SQL tables, columns, policies, functions,
  triggers, indexes; CI, compose, OpenAPI and Kubernetes YAML. The kind (TABLE, POLICY…) is in the
  signature. They rank below code unless the question says `table`, `policy`, `migration`,
  `workflow`, `yaml`… Kotlin (`.kt`) has symbols, calls and imports. French questions are understood
  (`CORTEX_FR=` disables the French handling). The `cortex_web_*` tools exist only in a build with the
  experimental `web` feature (`docs/WEB.md`).
- Line ranges are `L<start>-<end>`; in `card` and `impact`, callers carry the line of the call.
- `✎` marks code that is not committed yet.
- The last line, `next : <call>`, is the most useful next call (formerly "suite"). Follow it
  when in doubt.
- Output labels are currently in French (`appelle` = calls, `appelé par` = called by,
  `profondeur` = depth, `coupée(s)` = cut by the budget).

## Setup (once)

```sh
cortex index /path/to/project --name MyProject   # a few seconds for 10k files
```

The index stays fresh by itself: every call checks modified files and re-indexes them
incrementally (a few ms per file). If `cortex` is not found, install it (checksum-verified):
`curl -fsSL https://raw.githubusercontent.com/AstroQuestStudio/cortex/main/install.sh | sh`
(Linux, macOS) or
`irm https://raw.githubusercontent.com/AstroQuestStudio/cortex/main/install.ps1 | iex`
(Windows), or grab a binary from https://github.com/AstroQuestStudio/cortex/releases.

## When Cortex misses

If the answer finally comes from grep or reading files, say so in one line
(`Cortex missed: "<question>" → <file:line>`). On an open-source project, report it with the
"Cortex missed" issue template: misses become benchmark questions.
