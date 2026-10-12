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

### Cortex 0.5.0 and graphify 0.8.38 (2026-10-12)

Same 60 questions, same judge, same machine. Cortex 0.5.0 (`cortex find`), top-1 / top-5 / MRR:

| | flask | hono | ripgrep | 60 questions |
|---|---|---|---|---|
| Cortex 0.5.0 | 60 / 100 / 0.771 | 65 / 85 / 0.745 | 45 / 75 / 0.589 | **56.7 / 86.7 / 0.702** |
| Cortex 0.3.0 | 70 / 95 / 0.789 | 60 / 85 / 0.718 | 35 / 85 / 0.549 | 55.0 / 88.3 / 0.685 |
| BM25 over whole files | 60 / 95 / 0.758 | 40 / 70 / 0.517 | 50 / 70 / 0.601 | 50.0 / 78.3 / 0.625 |
| graphify, default | 20 / 30 / 0.28 | 0 / 20 / 0.10 | 10 / 40 / 0.23 | 10.0 / 30.0 / 0.204 |

graphify is built with `graphify update <repo>` (AST only, no LLM, no API key) and asked with
`graphify query`; files are ranked by first appearance among the nodes it prints. Five ways of
asking were tried (natural question or keywords only, breadth-first or
depth-first, budget 2,000 or 10,000): the best, depth-first with keywords only and 2,000 tokens, gets
13.3% / 35.0% / 0.238, which is the figure in the README. Rerun it with
`python bench/public/graphify-bench.py 2000 --dfs --keywords` (the default setting is
`python bench/public/graphify-bench.py`).

Caveats, so nobody has to find them: graphify's `query` is an exploration walk, not a ranked locator;
its semantic layer (docs, images, papers) was not run because it needs an LLM key; the questions are
Cortex's "where is X" questions, written blind to Cortex's output but not to graphify. A graphify
user who knows the graph by heart would ask differently. Per-question latency includes starting a
Python interpreter (graphify, ~570 ms) against starting a native binary (Cortex, ~24 ms).

Side measurements on the same repositories: index build 5 to 16 s (graphify) against under 0.4 s;
answer ~1,500 tokens against ~415; after appending a function to a file, `cortex find` returns it in
18 ms (freshness check and one-file re-index inside the call) while `graphify query` answers
"No matching nodes found" until `graphify update` is run again.

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

## 3. Closing the gaps (unreleased, 2026-10-12)

Goal of this pass: fix the known weaknesses (French questions, vague questions, ripgrep top-1,
top-5) under a stricter adoption rule, and add a per-project profile. Everything below was
re-measured the same day, with binaries built the same way (no LTO, so absolute latencies are not
the release ones), on a **frozen snapshot** of the private monorepo (9,867 files, `_disabled/`
removed so that every version sees the same corpus) and the three pinned public repositories.

### 3.1 Rules of this pass

- A change is kept only if it lowers **none** of five private sets (tuning 50, held-out 37,
  extended 100, vague 37, blind vague 40) and keeps the median `find` latency under 15 ms.
- Nothing was tuned on the 60 public questions; they were run (a) to document the history by version
  and (b) once at the end. Two uses are disclosed: the ripgrep top-5 drop between 0.4.0 and 0.5.0 was
  **diagnosed** on the public set (bisect by version), and the profile ablation of section 3.4 was
  run on it. Neither changed an engine weight; the profile defaults were changed after seeing that
  ablation (3.4), so the "v2" profile figures are guided by the public set.
- The *vague* set (37) was visible to the author of the generalisation branch (its lexicon contains
  words from those questions): it is no longer blind for that branch. The **blind vague** set (40
  questions, written by an agent that never ran Cortex, expected files verified on disk before any
  run) is the unseen check. It is small: one question is 2.5 points.

### 3.2 History by version, same corpus (top-1 / top-5 / MRR)

| Version | tuning (50) | held-out (37) | extended (100) | vague (37) | blind vague (40) | flask | hono | ripgrep | public 60 |
|---|---|---|---|---|---|---|---|---|---|
| 0.3.0 / 0.4.0 (`64cf709`, `0ab381a`) | 82 / 94 / .871 | 59.5 / 78.4 / .676 | 55 / 80 / .665 | 21.6 / 45.9 / .330 | 20 / 32.5 / .237 | 70 / 95 / .789 | 60 / 85 / .718 | 35 / 85 / .549 | 55.0 / 88.3 / .685 |
| 0.5.0 (`c0ad30a` coverage, `3a30397` latency, `9f62b54`) | 90 / 96 / .927 | 67.6 / 94.6 / .782 | 66 / 85 / .745 | 24.3 / 54.1 / .382 | 20 / 27.5 / .243 | 60 / 100 / .771 | 65 / 85 / .745 | 45 / 75 / .589 | 56.7 / 86.7 / .702 |
| + generalisation branch (`029a15a`, not merged until now) | 90 / 96 / .927 | 67.6 / 94.6 / .782 | 66 / 86 / .751 | 27.0 / 59.5 / .414 | 22.5 / 27.5 / .256 | 65 / 95 / .792 | 65 / 90 / .757 | 45 / 75 / .586 | 58.3 / 86.7 / .712 |
| **this pass** | **90 / 96 / .927** | **70.3 / 97.3 / .812** | **69 / 87 / .770** | **29.7 / 62.2 / .451** | **22.5 / 27.5 / .258** | **65 / 95 / .788** | **70 / 90 / .774** | **55 / 75 / .648** | **63.3 / 86.7 / .737** |

Reading, and what the numbers do not say:

- The best private figures were not lost: the tuning set has been at 90 / 96 / .927 since commit
  `c0ad30a`. The 80 / 94 / .861 of section 2 is the same set before that commit.
- **No bug regressed the public figure.** 0.3.0 to 0.5.0 trades top-5 for top-1: the per-file
  coverage multiplier (`c0ad30a`) gains 10 points of ripgrep top-1 and loses 10 of its top-5
  (85 to 75); flask goes 70 to 60 top-1 and 95 to 100 top-5. On the private sets the coverage
  weight 2 is the optimum (0, 1 and 3 are lower), so it was left alone.
- The fusion experiments quoted in section 1 (`cortex+bm25` 63.3 / 91.7 / .756 on 0.3.0;
  `rerank20-bm25` 65.0 / 86.7 / .755 on 0.5.0) are the highest public figures ever measured and
  they are **not** an engine: on the private held-out set the same fusion lowered the score. This
  pass reaches 63.3 / 86.7 / .737 with a different mechanism and no private set lowered.
- Public top-5 is unchanged at 86.7% and ripgrep top-5 is still 75%. Not fixed.
- The blind vague set is where Cortex is weakest: 22.5% top-1, 27.5% top-5 (15 of 40 expected
  files are not in the top 20). Those questions are paraphrases whose words do not occur in the
  expected file ("a gauge showing how well my account is protected" for a security-score card).
  A lexical engine cannot close that; it needs semantics (section 1: dense vectors only as a
  re-ranker, never fused at equal weight).

### 3.3 What was kept and what was refused

Every row is a switch measured on the five private sets (the public sets were not consulted
for these decisions).

| Change | Result on the private sets | Decision |
|---|---|---|
| Merge the generalisation branch (tooling penalty, role bonus, general FR/EN lexicon gated to rare words, filler words) | no set lowered; vague +2.7 top-1 / +5.4 top-5, extended top-5 +1, blind vague top-1 +2.5 | **kept** |
| Folder name as identity of `index.*`, `mod.rs`, `__init__.py`, `lib.rs` (x0.7 idf on matching terms) | held-out +2.7 top-1 and +2.7 top-5; extended +1 / +1; vague +2.7 top-1 | **kept** |
| Same at weight 1.0 and 1.5 | held-out up to 78.4 / 97.3, but tuning set top-1 90 to 88 | refused (rule: no set lowered) |
| Body/identity agreement bonus `1 + 0.1 / rank` over the top 100 body-only ranks | held-out MRR .804 to .812, extended top-1 67 to 69, vague top-5 +2.7; tuning unchanged | **kept** |
| Same at 0.25 / 0.5 / 1.0 | tuning-set MRR .927 to .924 / .924 / .871 (top-1 90 to 80 at 1.0) | refused |
| Single-letter tokens dropped (French elisions) | identical on all five sets | kept (neutral, principled) |
| Body-term coverage added to the file multiplier (0.5, 1, 2) | held-out top-5 -2.7 at 0.5; extended top-1 66 to 64 at 1 and 2 | refused |
| Coverage weight growing with question length | held-out top-5 -2.7, extended top-1 -2, vague top-5 -5.4 | refused |
| File-stem coverage bonus | tuning top-1 90 to 86 at 0.6 and 1.0 | refused |
| General lexicon without the rare-word gate (`CORTEX_GEN=tv`) | vague +2.7 / +8.1, blind vague +2.5 top-1, but held-out top-1 70.3 to 67.6 and extended top-5 87 to 84 | refused (gate kept) |
| Typo tolerance only from 5 letters (`CORTEX_GEN=e`) | tuning 90 to 86, extended 66 to 64 | refused, stays opt-in |
| 190 generic FR/EN software-development groups (`CORTEX_GEN=...d`) | identical on the five private sets; public, one run after the decision: flask top-5 95 to 90, ripgrep top-5 75 to 80, MRR .737 to .739 | neutral, **off by default**, opt-in |

### 3.4 Project profile: normal against personalised

Protocol, fixed before measuring: profiles are written from the README, the folder names and the
documentation file names only, never from the questions; the three public repositories were
copied and their question files were not opened before the profiles were written; each run uses
`cortex bench` (frozen index, so a profile never triggers a re-index) with the same binary,
profile on or off (`CORTEX_NO_PROFILE=1`).

Time to write: `cortex profile init` takes about a second. A hand-written profile of 15 to 25
lines took about one minute each in this run (reading the README and the tree, then writing; the
file write alone was 16 s for the monorepo). That holds when you already know the vocabulary.

Public repositories (top-1 / top-5 / MRR). `v1` is the profile as first written (synonyms
ungated at weight 0.85, entry boost x1.15, project name ignored). `v2` is the same file after
the profile design was corrected (synonyms applied only to rare question words, entry boost
x1.05); that correction was decided after seeing the ablation below, i.e. guided by the public set.

| | normal | `profile init` only | hand-written v1 | hand-written v2 |
|---|---|---|---|---|
| flask | 65 / 95 / .788 | 65 / 100 / .789 | 55 / 100 / .722 | 60 / 100 / .762 |
| hono | 70 / 90 / .774 | 70 / 90 / .774 | 70 / 90 / .768 | 70 / 90 / .758 |
| ripgrep | 55 / 75 / .648 | 55 / 75 / .648 | 35 / 80 / .532 | 50 / 80 / .634 |

Ablation, one section at a time (MRR flask / hono / ripgrep; normal is .788 / .774 / .648; run with
the gated synonyms and the entry boost still at x1.15): synonyms alone .788 / .767 / .648
(neutral), penalties alone .789 / .774 / .648, boosts alone .800 / .774 / .648, **ignore alone
.754 / .777 / .634 (ignoring the project's own name hurt two of three)**, entry alone .762 / .761
/ .646. So folder penalties and gated synonyms are safe; ignoring words and boosting entry files
are not, and `profile init` writes neither as an active rule.

Monorepo, profile written from `CLAUDE.md`, the `docs/apps` file names and the `src/` tree (23
synonym groups, 8 path rules; none of its words was copied from a question file, but the
extended-set failures had been read earlier in the same session, so treat it as indicative).
top-1 / top-5 / MRR:

| | tuning (50) | held-out (37) | extended (100) | vague (37) | blind vague (40) |
|---|---|---|---|---|---|
| normal | 90 / 96 / .927 | 70.3 / 97.3 / .812 | 69 / 87 / .770 | 29.7 / 62.2 / .451 | 22.5 / 27.5 / .258 |
| `profile init` only | 90 / 96 / .927 | 70.3 / 97.3 / .812 | 69 / 87 / .770 | 29.7 / 62.2 / .451 | 22.5 / 27.5 / .258 |
| hand-written v1 | 90 / 96 / .930 | 67.6 / 97.3 / .790 | 66 / 86 / .751 | 29.7 / 56.8 / .453 | 22.5 / 30 / .266 |
| hand-written v2 | 90 / 96 / .927 | 67.6 / 97.3 / .798 | 69 / 86 / .769 | 29.7 / 62.2 / .449 | 22.5 / 30 / .260 |
| v2, synonyms only | 90 / 96 / .927 | 70.3 / 97.3 / .812 | 69 / 87 / .770 | 29.7 / 62.2 / .451 | 22.5 / 30 / .260 |

Honest reading: **personalisation did not produce a measured gain here.** The synonym section is
the only one that helps anywhere (one question of 40 in blind-vague top-5) and costs nothing; the
broad `[boost]` rules (the whole of `src/`) lost one held-out question. The reason is
structural: on the monorepo the built-in glossary already holds the project's vocabulary, and on
the public repositories the questions already use the words of the code. A profile is worth
something when you know words your team says that the code does not contain, and these sets cannot
express that without being written from their questions. What is measured and guaranteed: the
engine without a profile is unchanged, a conservative profile is neutral, and a careless one can
cost 20 points of top-1 (ripgrep v1), which is why `profile init` only writes safe rules. The
mechanism itself is covered by a unit test on a synthetic project (a declared synonym finds a
symbol the question never names; a path penalty lowers its score; removing the file restores the
default engine).

Latency (monorepo, `find` median, same binary, machine shared so +-0.5 ms): 0.5.0 4.4 ms; this
pass without a profile 4.6 to 5.0 ms; with the 23-group profile 5.7 to 6.2 ms.
