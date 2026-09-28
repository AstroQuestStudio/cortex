## What and why

<!-- One topic per PR. Link the issue it fixes, if any. -->

## Measurements

<!-- Required when ranking, indexing, the graph or tool outputs change. Before → after. -->

| | before | after |
|---|---|---|
| public bench (top-1 / top-5 / MRR) | | |
| latency (query p50 / p95, update of one file) | | |

## Checklist

- [ ] `cargo fmt --all` and `cargo clippy --all-targets -- -D warnings` are clean
- [ ] `cargo test` passes
- [ ] no benchmark question was edited to make this change look better
- [ ] docs/SPEC.md updated if a tool input or output format changed
- [ ] CHANGELOG.md updated (Unreleased section)
- [ ] new dependencies are permissively licensed (no GPL/LGPL/AGPL)
- [ ] I have the right to contribute this code and agree to the terms in CONTRIBUTING.md
