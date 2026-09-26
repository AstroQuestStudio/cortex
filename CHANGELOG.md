# Changelog

All notable changes to Cortex are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[semantic versioning](https://semver.org/) (pre-1.0: minor versions may break compatibility).

## [Unreleased]

### Added
- One-line installers: `install.sh` (Linux, macOS, Git Bash) and `install.ps1` (Windows). They
  download the release archive, refuse it unless its SHA-256 matches `SHA256SUMS.txt`, and
  install without administrator rights (`~/.local/bin`, `%LOCALAPPDATA%\cortex\bin`).
- README: animated terminal demo (light and dark themes), installation first, public benchmark
  table near the top; social preview image in `.github/`.

### Changed
- Release archives (from the next release): `.tar.gz` members no longer start with `./`.

### Fixed
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

[Unreleased]: https://github.com/AstroQuestStudio/cortex/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/AstroQuestStudio/cortex/releases/tag/v0.3.0
