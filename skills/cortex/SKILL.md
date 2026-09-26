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
| where is X? (natural language, English or French, or keywords) | `cortex_find` | `cortex find "<question>"` |
| what is this symbol: signature, role, callees, callers with call line, tests | `cortex_card` | `cortex card <id>` |
| the exact lines of a symbol, doc section or range | `cortex_read` | `cortex read <id>` |
| what a file contains (symbols with ranges, imports, importers, exports) | `cortex_outline` | `cortex outline <file>` |
| how a folder works (roles, entry points, dependencies) | `cortex_overview` | `cortex overview <dir>` |
| what breaks if I change X, and which tests to rerun | `cortex_impact` | `cortex impact <id>` |
| how does A reach B (call chain with call lines) | `cortex_path` | `cortex path <a> <b>` |
| what did I change, what calls it, which tests | `cortex_changed` | `cortex changed` |
| exact text (error message, URL, i18n key, TODO) with the enclosing function | `cortex_grep` | `cortex grep "<text>"` |
| a file by name fragment or glob | `cortex_files` | `cortex files "<fragment>"` |
| offline docs of a library you scraped | `cortex_docs` | `cortex docs query "<q>" --source <Lib>` |

Add `-p <Project>` (MCP: `project`) when several projects are indexed, and `-b <tokens>`
(MCP: `budget`) to change the output budget.

## How to read the outputs

- Every result carries a **stable identifier**: `S:<path>#<symbol>` (code symbol, `#name~2` for a
  second homonym in the same file), `D:<path>#<anchor>` (doc section), `F:<path>` (file).
  Copy it verbatim into the next call. Tools also accept a bare name, a path, a unique path
  suffix (`useAuth.ts`) or `path:line`.
- Line ranges are `L<start>-<end>`; in `card` and `impact`, callers carry the line of the call.
- `✎` marks code that is not committed yet.
- The last line, `suite : <call>`, is the most useful next call ("suite" = "next"). Follow it
  when in doubt.
- Output labels are currently in French (`appelle` = calls, `appelé par` = called by,
  `profondeur` = depth, `coupée(s)` = cut by the budget).

## Setup (once)

```sh
cortex index /path/to/project --name MyProject   # a few seconds for 10k files
```

The index stays fresh by itself: every call checks modified files and re-indexes them
incrementally (a few ms per file). If `cortex` is not found, install it:
`cargo install astroquest-cortex` or grab a binary from
https://github.com/AstroQuestStudio/cortex/releases.

## When Cortex misses

If the answer finally comes from grep or reading files, say so in one line
(`Cortex missed: "<question>" → <file:line>`). On an open-source project, report it with the
"Cortex missed" issue template: misses become benchmark questions.
