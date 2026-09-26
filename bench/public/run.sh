#!/usr/bin/env bash
# Public, reproducible benchmark of Cortex (see docs/BENCHMARKS.md).
#
# For each question file bench/public/<name>.json (repo URL + pinned commit + 20 questions):
#   1. fetch the repository at the pinned commit into bench/public/.repos/<name>
#   2. index it with Cortex under the project name "bench-<name>"
#   3. `cortex bench`          : top-1 / top-5 / MRR of Cortex
#   4. `cortex bench-compare`  : same questions, same judge, same corpus for ripgrep-style
#                                keyword search, BM25, dense RAG and hybrid (the last two only
#                                with a `--features bench` build and the model on disk)
#
# Usage: bench/public/run.sh [name…]          (default: every bench/public/*.json)
# Environment:
#   CORTEX        cortex binary to use (default: `cortex` on PATH)
#   CORTEX_HOME   where the benchmark atlases go (default: bench/public/.cortex-home, so
#                 your own ~/.cortex is never touched)
#   CORTEX_BENCH_MODEL  directory of minishlab/potion-multilingual-128M for the dense baseline
#                 (default: ~/.cortex/models/potion-multilingual-128M when it exists)
#   NO_COMPARE=1  skip bench-compare
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPOS="$HERE/.repos"
RESULTS="$HERE/results"
CORTEX="${CORTEX:-cortex}"
export CORTEX_HOME="${CORTEX_HOME:-$HERE/.cortex-home}"
mkdir -p "$REPOS" "$RESULTS" "$CORTEX_HOME"
# Dense baseline (build `--features bench`): the model stays in the user's ~/.cortex/models.
if [ -z "${CORTEX_BENCH_MODEL:-}" ]; then
  for home in "$HOME" "${USERPROFILE:-}"; do
    if [ -n "$home" ] && [ -d "$home/.cortex/models/potion-multilingual-128M" ]; then
      export CORTEX_BENCH_MODEL="$home/.cortex/models/potion-multilingual-128M"
      break
    fi
  done
fi

json_field() { # json_field <file> <field> : first "field": "value" of a flat JSON field
  sed -n "s/^[[:space:]]*\"$2\"[[:space:]]*:[[:space:]]*\"\([^\"]*\)\".*/\1/p" "$1" | head -n 1
}

fetch() { # fetch <dir> <url> <commit>
  local dir="$1" url="$2" commit="$3"
  if [ -d "$dir/.git" ] && [ "$(git -C "$dir" rev-parse HEAD 2>/dev/null)" = "$commit" ]; then
    return
  fi
  rm -rf "$dir"
  git init -q "$dir"
  git -C "$dir" remote add origin "$url"
  git -C "$dir" -c advice.detachedHead=false fetch -q --depth 1 origin "$commit"
  git -C "$dir" -c advice.detachedHead=false checkout -q FETCH_HEAD
  [ "$(git -C "$dir" rev-parse HEAD)" = "$commit" ] || { echo "commit mismatch for $url" >&2; exit 1; }
}

names=("$@")
if [ ${#names[@]} -eq 0 ]; then
  for f in "$HERE"/*.json; do names+=("$(basename "$f" .json)"); done
fi

"$CORTEX" --version
summary="$RESULTS/summary.txt"
: > "$summary"
for name in "${names[@]}"; do
  file="$HERE/$name.json"
  url="$(json_field "$file" repo)"
  commit="$(json_field "$file" commit)"
  project="$(json_field "$file" project)"
  echo
  echo "=== $name — $url @ ${commit:0:12}"
  fetch "$REPOS/$name" "$url" "$commit"
  "$CORTEX" index "$REPOS/$name" --name "$project"
  "$CORTEX" bench "$file" -p "$project" -v | tee "$RESULTS/$name-bench.txt"
  {
    echo "== $name ($url @ ${commit:0:12})"
    grep -E "^(top-1|  \[)" "$RESULTS/$name-bench.txt"
  } >> "$summary"
  if [ -z "${NO_COMPARE:-}" ]; then
    "$CORTEX" bench-compare "$file" -p "$project" --sortie "$RESULTS" | tee "$RESULTS/$name-compare.txt"
  fi
done

echo
echo "=== Summary (Cortex, top-1 / top-5 / MRR)"
cat "$summary"
