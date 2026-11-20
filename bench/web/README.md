# Banc de Cortex Web — comment le rejouer

Corpus : `corpus.json` (10 sources). Questions : `questions-dev.json` (60, pour régler) et
`questions-test.json` (40, à lancer UNE seule fois à la fin, résultat rapporté tel quel).
Le moteur du banc est `src/web/bench.rs` (feature `web`).

```text
cargo build --release --features web      # (dans un dossier cible isolé si besoin : CARGO_TARGET_DIR)

# 1. dev : collecte (réseau, poli : 1 requête/s par hôte, robots.txt respecté) + questions + base A + calibrage du seuil
cortex web bench bench/web/corpus.json bench/web/questions-dev.json --home <dossier isolé> --calibrate
# 2. test, UNE fois, sans recollecter, avec le seuil trouvé sur le dev
cortex web bench bench/web/corpus.json bench/web/questions-test.json --home <même dossier> --skip-crawl --threshold <seuil>
```

Options : `--sources a,b` (sous-ensemble du corpus), `--skip-crawl` (rejoue sur les magasins du
`--home`), `--home D` (CORTEX_HOME isolé ; obligatoire pour collecter, magasins dans `D/web/`, base A
dans `D/docs/`), `--parallel N` (hôtes en parallèle, 3 par défaut, jamais deux sur le même hôte),
`--skip-base` (sans la comparaison `cortex docs add/query`), `--feature wcov|score` (trait de
confiance), `--repeat N` (essais par question pour le temps, médiane), `--out D` (rapports ;
défaut `bench/web/resultats/`).

Sorties : `bench-<fichier de questions>.txt` (tableaux) et `.json` (une ligne détaillée par question :
rangs page et section, version, jetons, temps, diagnostic des échecs, résultats de la base A).

Lecture rapide :

- **Page** = URL normalisée (hôte + chemin, sans `/` final ni fragment) ; **section** = page + ancre.
- **Portée** : `include_paths` / `exclude_paths` du corpus sont passés tels quels à `crawl`
  (`SourceConfig`), qui les honore à la découverte (sitemap, liens, redirections) ; le banc les
  réapplique seulement pour MESURER les pages hors portée et pour la base A. Profondeur adaptative
  (`crawl::adaptive_depth`, jamais plus que `max_pages`). Plafond dur de 150 pages au total.
- **Version** : pour une source à plusieurs versions, la version automatique (celle que la question nomme,
  sinon celle de la source principale) est la mesure principale ; `--any-version` rend le mélange.
- **Abstention** : `--feature conf` (défaut) = `Hit::confidence()` de l'index, seuil `index::MIN_CONFIDENCE`.
- **Extraction** : `CORTEX_WEB_DUMP_RAW=<dossier>` pendant une collecte copie les pages reçues ;
  `--reextract <dossier>` rejoue l'extraction sans réseau (mise au point de l'extracteur).
- **Seuil** : `Index::query` classe toujours ; « rien de fiable » est décidé dans `bench.rs` d'après le
  trait du 1er résultat (`wcov` : part des termes de la question, pondérés par leur rareté dans la
  source, présents dans la section de tête ; `score` : BM25F brut). Le seuil se calibre sur le dev
  seulement (`--calibrate` est refusé sur un fichier dont le nom contient « test »).
- **Base A** : `cortex docs add` (une fois par graine, mêmes plafonds) puis `cortex docs query`
  (`src/scrape.rs`, inchangé) lancés comme processus fils avec le même `CORTEX_HOME` ; le fichier
  `.md` renvoyé est rapporté à son URL par `_manifest.tsv`. La base A n'a ni version ni robots.txt et
  va à ~3 requêtes/s : c'est un défaut de l'existant que le banc mesure, pas une règle du banc.
- **RAM** : crête du jeu de travail du processus, échantillonnée toutes les 200 ms
  (`GetProcessMemoryInfo`, Windows) ; elle englobe les sources collectées en parallèle.
- Docsify (`needs_js`) n'est pas répondable sans rendu : ses questions comptent comme manquées et sont
  aussi rapportées « hors needs_js ».
