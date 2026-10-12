#!/usr/bin/env python3
"""Cortex vs graphify on the public benchmark: same questions, same judge.

Prerequisites: `bench/public/run.sh` has fetched the repositories into `.repos/`, and graphify is
installed (`pip install graphifyy`). Builds each graph with `graphify update <repo>` (AST only,
no LLM, no API key), asks every question with `graphify query "<q>" --budget N`, and ranks the
files by their first appearance in the nodes graphify prints. The judge is the one of
`cortex bench`: rank of the first expected file, cut at 20.

Usage: python bench/public/graphify-bench.py [budget] [--dfs] [--keywords]
  budget      graphify --budget (default 2000 = graphify's own default)
  --dfs       depth-first traversal instead of breadth-first
  --keywords  ask with the question's content words only (no stop words)
The README figure is the best of five settings: `2000 --dfs --keywords`.
Output: bench/public/results/graphify-<budget>[-dfs][-keywords].json and a summary on stdout.
"""
import json
import os
import re
import statistics
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ARGS = [a for a in sys.argv[1:] if not a.startswith("--")]
BUDGET = ARGS[0] if ARGS else "2000"
DFS = "--dfs" in sys.argv
KEYWORDS = "--keywords" in sys.argv
STOP = set("a an the of to in on for and or is are does do how where what which with from into by it its that this be as at via when who whose se la le les de du des un une et ou dans pour sur par comment est sont quel quelle quels fait faire cette ce ces au aux avec sans il elle".split())


def mots(q):
    return " ".join(w for w in re.findall(r"[A-Za-z_][A-Za-z0-9_]+", q) if w.lower() not in STOP)
NODE = re.compile(r"^NODE .*\[src=(.+?) loc=")


def rang(fichiers, attendus):
    att = {a.replace("\\", "/").lower() for a in attendus}
    for i, f in enumerate(fichiers[:20], 1):
        if f.replace("\\", "/").lower() in att:
            return i
    return None


def stats(ls):
    n = len(ls)
    t1 = sum(1 for x in ls if x["rang"] == 1) / n * 100
    t5 = sum(1 for x in ls if x["rang"] and x["rang"] <= 5) / n * 100
    mrr = sum(1 / x["rang"] for x in ls if x["rang"]) / n
    return t1, t5, mrr


res = {}
for nom in ["flask", "hono", "ripgrep"]:
    spec = json.load(open(os.path.join(HERE, f"{nom}.json"), encoding="utf8"))
    repo = os.path.join(HERE, ".repos", nom)
    t = time.time()
    subprocess.run(["graphify", "update", repo], capture_output=True)
    build = time.time() - t
    lignes = []
    for q in spec["queries"]:
        t = time.time()
        p = subprocess.run(
            ["graphify", "query", mots(q["q"]) if KEYWORDS else q["q"], "--budget", BUDGET] + (["--dfs"] if DFS else []),
            cwd=repo, capture_output=True, text=True, encoding="utf8", errors="replace",
        )
        dt = (time.time() - t) * 1000
        fichiers = []
        for l in p.stdout.splitlines():
            m = NODE.match(l)
            if m and m.group(1).strip() not in fichiers:
                fichiers.append(m.group(1).strip())
        lignes.append({"q": q["q"], "tag": q["tag"], "rang": rang(fichiers, q["expect"]),
                       "ms": dt, "sortie_car": len(p.stdout)})
    res[nom] = {"build_s": build, "questions": lignes}

os.makedirs(os.path.join(HERE, "results"), exist_ok=True)
json.dump(res, open(os.path.join(HERE, "results", f"graphify-{BUDGET}{'-dfs' if DFS else ''}{'-keywords' if KEYWORDS else ''}.json"), "w", encoding="utf8"),
          ensure_ascii=False, indent=1)

tout = []
for nom, r in res.items():
    ls = r["questions"]
    t1, t5, m = stats(ls)
    car = statistics.median(x["sortie_car"] for x in ls)
    print(f"{nom:8s} top-1 {t1:5.1f}%  top-5 {t5:5.1f}%  MRR {m:.3f}  build {r['build_s']:.0f} s  "
          f"{statistics.median(x['ms'] for x in ls):.0f} ms/question  ~{car / 4:.0f} tokens")
    tout += ls
t1, t5, m = stats(tout)
print(f"TOTAL {len(tout)} questions: top-1 {t1:.1f}%  top-5 {t5:.1f}%  MRR {m:.3f}")
