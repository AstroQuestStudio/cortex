# `cortex pack` — le contexte minimal d'une tâche

> Statut : livré dans la prochaine version (non publié). CLI `cortex pack "<tâche>" [-b <jetons>]`, MCP `cortex_pack`. Code :
> `src/outils/pack.rs`. Banc : `cortex bench-pack` (`src/bench_combo.rs`).

`ask` répond à une QUESTION sur le code. `pack` répond à une TÂCHE : « envoie les petits fichiers du
drive en un seul PUT présigné avec repli sur le multipart ». L'agent n'a pas encore de question :
il a besoin de savoir **quels fichiers il va toucher**, quels symboles lire en premier, quels tests
lancer, quelles docs et quels parcours d'écran concernent le sujet. `pack` rend cela d'un coup, en
markdown compact, sous un budget de jetons (`--budget`, 3000 par défaut), avec des identifiants à
recopier dans `card` / `read` / `edit`.

```
$ cortex pack "Envoie les petits fichiers du drive en un seul PUT présigné" -p MonApp -b 1500
# pack — Envoie les petits fichiers du drive en un seul PUT présigné (≤ 1500 tokens)
## files (most likely first; t=text, c=changed together before, n=neighbour)
- F:src/lib/drive/envoiUnPut.ts [tn]
- F:src/lib/drive/client.ts [tcn]
- F:server/functions/ecriture.ts [tc]
…
## key symbols
- S:src/lib/drive/envoiUnPut.ts#envoyerUnPut fn L12-60 — Envoie un fichier en un seul PUT présigné · callers 3: …
## tests (5): F:…, …
## docs: D:docs/drive-envoi.md#…
## recent changes: F:… (4d: Envoi adaptatif…)
## journeys (Spectra recipes): drive-annuler
next : read S:src/lib/drive/envoiUnPut.ts#envoyerUnPut
```

## Comment les fichiers sont choisis

1. **Texte** : la tâche est posée au moteur de `find` (BM25F, FR/EN, synonymes). Chaque fichier reçoit
   la somme pondérée de ses trois meilleurs symboles (1 / 0,5 / 0,25) ; docs et tests mis de côté.
2. **Historique git** (le plus fort signal de « qu'est-ce qui change avec quoi ») : les 3 000 derniers
   commits ; pour chaque commit qui contient un des 5 fichiers de tête, les AUTRES fichiers du commit
   reçoivent un poids (divisé par la racine de la taille du commit ; commits de plus de 30 fichiers
   ignorés : renommages, versions). Les fichiers qui changent toujours ensemble remontent même sans
   partager un mot avec la tâche.
3. **Voisins** (importeurs et importés des deux premiers), **même dossier**, **fichiers chauds**
   (souvent modifiés récemment) : petits bonus de départage.
4. Score = texte + 0,6 × co-changement + 0,12 × voisin + 0,1 × chaud + 0,1 × dossier (poids réglés sur le
   banc ; `CORTEX_PACK_CO`, `CORTEX_PACK_VOISIN`, `CORTEX_PACK_CHAUD`, `CORTEX_PACK_DOSSIER`,
   `CORTEX_PACK_GRAINES` pour les rejouer).

Le rendu remplit le budget dans l'ordre : fichiers (un par ligne, ~10 jetons), symboles clés avec appelants,
tests (par nom, par import, ou remontés par le texte), docs (titres), changements récents (30 jours), parcours
Spectra (recettes dont l'écran dépend des fichiers de tête). Ce qui ne tient pas est compté (`… N line(s) cut`).

## Options

| | |
|---|---|
| `-b, --budget` | jetons (≈ caractères / 4) ; 3000 par défaut |
| `-p` | projet (sinon : celui dont la meilleure réponse à la tâche est la plus forte) |
| `--avant <sha>` | historique limité aux commits qui précèdent celui-ci (banc : pas de fuite de la solution) |

## Mesure

Voir `docs/BENCHMARKS.md` § « pack ».
