# Benchmarks

Cortex is developed against measurements. This page describes the **public, reproducible
benchmark** (anyone can rerun it), the **private benchmarks** run on AstroQuest's proprietary
monorepo, and how to read both. The engine was never tuned on the public questions.

## 1. Public benchmark (`bench/public/`)

### What it measures

A developer or an agent asks a question about a repository ("where are signed cookies
verified?"). Each approach returns an ordered list of files; the judge takes the rank of the
first expected file (cut at 20). Reported: **top-1** (share of questions answered by the first
file), **top-5**, **MRR** (mean reciprocal rank), latency, and the tokens an agent reads before
reaching the right file.

### Repositories and questions

| Repository | Language | Pinned commit | Files indexed | Questions |
|---|---|---|---:|---:|
| [pallets/flask](https://github.com/pallets/flask) 3.1.3 | Python | `22d924701a6a` | 127 | 20 |
| [honojs/hono](https://github.com/honojs/hono) v4.13.9 | TypeScript | `7c3b0df96dbf` | 421 | 20 |
| [BurntSushi/ripgrep](https://github.com/BurntSushi/ripgrep) 15.2.0 | Rust | `e89fff89ac9a` | 140 | 20 |

Per repository: 12 natural-language questions in English, 4 in French, 4 technical (an
identifier or an error message). Each question lists its expected file(s) and a one-line
justification (`why`, with `path:line`).

**Protocol.** The 60 questions were written on 2026-09-27 by three independent writers (one per
repository) who read the code with grep and a file reader only. They never ran Cortex, never saw
its output and never read its source. The files were committed (maintainers' history, commit
`5f060f3`) **before** Cortex was first run on these repositories, and the engine has not been
changed since in response to them. A question is only edited when its expected answer is shown to be wrong, in
a separate commit.

### Approaches compared (same corpus, same judge)

| Approach | What it is |
|---|---|
| `rg-count` | what an agent does without an index: every word of the question searched as a case-insensitive literal (like `rg -i -F -e w1 -e w2…`), files ranked by number of matches |
| `rg-terms` | same search, files ranked by number of *distinct* words found (a stronger grep baseline) |
| `bm25-files` | Okapi BM25 over a bag of tokens per file (path + full content, camelCase split, accents folded) |
| `rag-dense` | classic RAG: 40-line chunks (10 lines overlap) embedded with `minishlab/potion-multilingual-128M` (model2vec, 256 dims), cosine similarity, files ranked by their best chunk |
| `hybrid` | BM25 on the same chunks + dense, reciprocal rank fusion (k = 60) |
| **`cortex`** | Cortex 0.3.0, the ranking used by `cortex find` (BM25F over names, paths, doc comments, file headers and file bodies, stemming, FR↔EN glossary) |

`cortex bench-compare` prints them under their original names: `rg-compte`, `rg-termes`,
`bm25-fich`, `rag-dense`, `hybride`, `cortex`.

Experiments (not shipped): `cortex+bm25` fuses Cortex's and BM25's top-100 files by RRF;
`rerank20-bm25` only re-orders Cortex's top 20.

### Results (Cortex 0.3.0, 2026-09-27)

All 60 questions (each repository weighs the same):

| Approach | top-1 | top-5 | MRR | tokens before the right file (median per repo) |
|---|---:|---:|---:|---|
| rg-count | 6.7% | 40.0% | 0.229 | 8,411 / 13,269 / 39,683 |
| rg-terms | 16.7% | 58.3% | 0.345 | 5,141 / 6,898 / 31,710 |
| rag-dense | 38.3% | 68.3% | 0.516 | 417 / 991 / 1,782 |
| hybrid | 41.7% | 81.7% | 0.580 | 403 / 1,234 / 1,270 |
| bm25-files | 50.0% | 78.3% | 0.625 | 8 / 22 / 10 |
| **cortex** | **55.0%** | **88.3%** | **0.685** | 14 / 15 / 29 |
| *exp: cortex+bm25* | *63.3%* | *91.7%* | *0.756* | 6 / 10 / 10 |

Per repository (top-1 / top-5 / MRR):

| Approach | flask (Python) | hono (TypeScript) | ripgrep (Rust) |
|---|---|---|---|
| rg-count | 10 / 65 / 0.347 | 10 / 30 / 0.209 | 0 / 25 / 0.132 |
| rg-terms | 30 / 85 / 0.515 | 10 / 45 / 0.271 | 10 / 45 / 0.250 |
| rag-dense | 55 / 90 / 0.702 | 25 / 55 / 0.381 | 35 / 60 / 0.466 |
| hybrid | 65 / 95 / 0.783 | 30 / 70 / 0.463 | 30 / 80 / 0.493 |
| bm25-files | 60 / 95 / 0.758 | 40 / 70 / 0.517 | **50** / 70 / **0.601** |
| **cortex** | **70 / 95 / 0.789** | **60 / 85 / 0.718** | 35 / **85** / 0.549 |
| *exp: cortex+bm25* | *80 / 95 / 0.867* | *55 / 95 / 0.729* | *55 / 85 / 0.673* |

Latency per question (median): Cortex 7–10 ms, BM25 < 0.1 ms, dense 0.3–1.2 ms, rg 1–4 ms
(Ryzen 7 5800H, Windows 11, warm cache). Index build: Cortex 0.07–0.37 s per repository;
the dense baseline needs a 506 MiB model.

### Reading the numbers honestly

- **Against grep, Cortex wins by far**: 55% vs 7–17% top-1, and the right file comes after
  ~15–30 tokens instead of 5,000–40,000 tokens of matching lines. This is the cost an agent pays
  today when it explores with grep.
- **Against a dense RAG**, Cortex wins on every repository (MRR 0.685 vs 0.516) while returning
  locations instead of 40-line chunks (RAG outputs are 1,000–4,000 tokens per question).
- **Plain BM25 over whole files is a strong baseline on small repositories**: it beats Cortex on
  ripgrep top-1 (50% vs 35%), where Cortex's symbol-level ranking sometimes puts a same-named
  symbol of a neighbouring file first. Cortex keeps a better top-5 everywhere.
- **Fusing Cortex with file-level BM25 scores higher here** (MRR 0.756). On the private
  8,000-file monorepo the same fusion *lowered* the held-out score (see below), so it is not
  shipped. Finding a combination that improves both is open work, and it will be judged on
  new questions, not on these.
- **Small sample**: one question is 5 points of top-1 per repository (1.7 points overall).
  Differences under two questions are noise.

### Run it

```sh
cargo build --release
CORTEX=target/release/cortex bench/public/run.sh          # all repositories
CORTEX=target/release/cortex bench/public/run.sh hono     # one
```

The script fetches each repository at its pinned commit into `bench/public/.repos/`, indexes it
into a separate `CORTEX_HOME` (`bench/public/.cortex-home`, your `~/.cortex` is not touched),
then runs `cortex bench` and `cortex bench-compare`. Outputs go to `bench/public/results/`.
For the dense and hybrid rows, build with `--features bench` and put the model in
`~/.cortex/models/potion-multilingual-128M` (or set `CORTEX_BENCH_MODEL`); without it those rows
are skipped. Requirements: git, bash (Git Bash on Windows).

### Adding a repository

Create `bench/public/<name>.json` with `project`, `repo`, `commit`, `written` and 20 `queries`
(`q`, `expect`, `tag`, `why`). Write the questions **before** running Cortex on that repository
and commit them first; then run the script and add the numbers.

## 2. Private benchmarks (AstroQuest)

Cortex's real-world showcase is AstroQuest's proprietary monorepo: 8,069 files, 1.7 M lines,
42,548 symbols, TypeScript/React, Rust, SQL and Markdown. Its question files cannot be published
(they describe closed code); they live in the private repository and are located through
`CORTEX_BENCH_DIR` (`queries.json`, `holdout.json`, `agent_tasks.json`).

**Relevance.** 50 tuning questions (29 French, 10 English, 11 technical) and 40 held-out
questions written before any tuning (15/15/10). Adoption rule: a change is kept only if it does
not lower **both** sets; improving only the tuning set is overfitting.

| Approach | tuning set (50): top-1 / top-5 / MRR | held-out set (40) |
|---|---|---|
| rg-terms | 2% / 36% / 0.169 | 7.5% / 30% / 0.164 |
| bm25-files | 46% / 78% / 0.593 | 30% / 57.5% / 0.412 |
| rag-dense | 18% / 38% / 0.264 | 25% / 35% / 0.317 |
| hybrid | 40% / 72% / 0.535 | 27.5% / 62.5% / 0.404 |
| **cortex** | **80% / 94% / 0.861** | **67.5% / 85% / 0.742** |

**Agent tasks.** 10 comprehension tasks (what breaks if I change X, how data flows from A to B,
what a module contains and who uses it, which tests to rerun, where Y is handled). Each has
hand-verified expected facts and two scripted call sequences written before any tuning: with
Cortex (`find`, `card`, `impact`, `path`, `overview`, `outline`) and without (glob, rg, reading
files). With Cortex: **16 calls, 6,572 tokens, 49/57 facts (86%)**. Without: 27 calls, 71,597
tokens, 51/57 facts (89%). That is **10.9× fewer tokens and 40% fewer calls** for about the same
coverage.

**Latency** (atlas opened once, median): `find` 5–9 ms, `card` 1.2–2.7 ms, `read` 0.6–1.3 ms,
`outline` 0.5–0.7 ms, `impact` 1.2–2.5 ms (15–20 ms for the most-called symbol), update of one
modified file 7 ms, full build ~5 s at 8–16 threads.

Methods and history of every tuning decision: [ARCHITECTURE.md](ARCHITECTURE.md) §7, §11, §12.
