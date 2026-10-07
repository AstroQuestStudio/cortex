# Contributing to Cortex

Thanks for helping! Cortex is maintained by [AstroQuest](https://astroquest.fr) and released
under the MIT license; by contributing you agree that your contribution is licensed under it too.
Please read the [Code of Conduct](CODE_OF_CONDUCT.md).

## Build and test

```sh
cargo build --release        # binary: target/release/cortex
cargo test                   # the whole suite runs in seconds
cargo fmt --all
cargo clippy --all-targets
```

On Windows, plain `cargo` works with the MSVC toolchain. Without Visual Studio, `./build.ps1`
uses the GNU toolchain with a portable MinGW-w64 (see the script header).

The optional dense-retrieval baseline of `cortex bench-compare` needs `--features bench` and a
local copy of the `potion-multilingual-128M` model (see [docs/BENCHMARKS.md](docs/BENCHMARKS.md)).

## The one rule: numbers before and after

Cortex is judged by measurements, not by taste. A change to ranking, extraction, the graph or an
agent tool is accepted when it comes with before/after numbers and does not lower them:

| What you touch | Run | Report |
|---|---|---|
| ranking (`search.rs`, `stem.rs`, `semantic.rs`, `atlas/query.rs`) | `bench/public/run.sh` | top-1 / top-5 / MRR per repository |
| indexing, atlas, freshness | `cargo test` + `cortex bench-latence -p <project>` | build time, query p50/p95, update of one file |
| tool outputs (`src/outils/`) | `cargo test` + a before/after transcript | tokens and facts, as in the README demo |

- **Never tune on the questions you report.** The public benchmark questions were written before
  Cortex was ever run on those repositories and are frozen: fix a question only if its expected
  answer is wrong, in a separate PR that explains why. If you want Cortex to answer a new kind of
  question, add questions to a *new* file, commit them first, and only then change the engine.
- Maintainers also run a private held-out benchmark on a proprietary monorepo; a change that
  improves the public numbers but lowers the held-out ones is overfitting and will be reverted.
- Determinism matters: an incremental update must give the same answers, bit for bit, as a full
  rebuild (see the equivalence tests in `src/atlas/incremental.rs`).

## Reporting a miss

When Cortex fails to find something that `grep` + reading found, open a
**"Cortex missed"** issue with the question, the expected file and what Cortex returned.
Misses on open-source repositories become benchmark questions.

## Pull requests

- One topic per PR; keep unrelated formatting out of it.
- Add or update tests (`cargo test` must pass on Linux, macOS and Windows: CI checks it).
- Output formats of the agent tools are specified in [docs/SPEC.md](docs/SPEC.md): a change there
  needs a spec update in the same PR.
- New languages are welcome: tree-sitter grammar in `Cargo.toml`, extension in `src/lang.rs`,
  queries in `src/extract.rs` (or a dedicated walker like `src/cpp.rs` when a language needs a
  preprocessing step), tests with a small fixture.
- Dependencies must be permissively licensed (MIT, Apache-2.0, BSD, ISC, Zlib, Unicode,
  Unlicense, MPL-2.0 at most). No GPL, LGPL or AGPL.

## Commit messages

`<type>(<area>): <summary>`, e.g. `feat(outils): impact lists re-exports`,
`fix(atlas): tombstone renamed files`. Types: feat, fix, perf, docs, test, refactor, chore, ci.
