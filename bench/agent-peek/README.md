# Banc d'agent « peek » : pourquoi ce test échoue ?

Mesure, de façon reproductible, ce que coûte à un agent la tâche « pourquoi ce test échoue ? »
quand il utilise `cortex peek` plutôt qu'un `Read` complet du log.

Par tâche et au total, le banc rend : jetons du Read complet, jetons de `peek`, ratio, nombre
d'appels d'outil et réussite (les chaînes attendues figurent-elles dans la sortie ?).
Jetons = caractères / 4, arrondi au-dessus, comme le banc d'agent principal.

## Commande

Depuis la racine du dépôt (le fichier de tâches par défaut est `bench/agent-peek/taches.json`) :

```
cortex bench-agent --tache peek
```

Binaire de développement : `target/debug/cortex.exe bench-agent --tache peek` (idem en release).
Aucun projet indexé n'est requis, aucun accès réseau. Autres formes :

```
cortex bench-agent bench/agent-peek/taches.json          # fichier explicite (reconnu par "banc": "peek")
cortex bench-agent --tache peek -v                       # + la sortie de chaque appel peek
cortex bench-agent bench/agent-peek/taches.json --tache gros   # seulement les tâches dont l'id contient « gros »
```

Le JSON des résultats est écrit dans `bench/resultats/` (ou `$CORTEX_BENCH_RESULTS`) :
`agent-peek-<date>-<commit>.json`. Code de sortie : `0` aucune alerte ; `2` alerte inattendue
(régression, tâche invalide, étape non jouée) ; `3` uniquement des échecs déjà connus
(`connu_ko`, voir plus bas).

`cargo test banc_agent` rejoue le banc complet et échoue sur toute alerte inattendue.

## Les gros logs sont générés, pas versionnés

`gen.mjs` (Node, sans dépendance, générateur pseudo-aléatoire à graine fixe : deux exécutions
donnent des fichiers identiques octet pour octet) écrit dans `fixtures/` :

| fichier | lignes | contenu |
|---|---|---|
| `gros-cargo.log` | 20 000 | `cargo test`, 9 400 tests, 2 échecs (panic) en fin de log, ~70 tests réussis dont le nom contient `error`, `failed` ou `panic` (leurres) |
| `gros-vitest.log` | 20 000 | vitest verbeux, 320 fichiers, sorties console, un échec répété 40 fois et un autre |
| `vert-vitest.log` | 1 219 | vitest, tout est vert (38 fichiers, 3 tests ignorés, leurres `error`/`panic` dans des noms de tests) |

Le banc les regénère tout seul (`node bench/agent-peek/gen.mjs`) quand ils manquent. À ignorer
par git : `bench/agent-peek/fixtures/gros-*.log` et `bench/agent-peek/fixtures/vert-*.log`
(à ajouter au `.gitignore` par le propriétaire du dépôt).

Fixtures versionnées (petites) : `pytest-echec.log` et `eslint-echec.log` sont des sorties
RÉELLES (pytest 9.1, eslint 9.39, chemins anonymisés) ; `vitest-echec.log`, `cargo-echec.log`,
`go-echec.log` et `fr-accents.log` sont écrites d'après le format documenté de chaque outil
(ces outils ne produisent pas de panic / d'échec sur demande dans l'environnement du banc) ;
`fr-accents.log` est un log en français avec accents, interrogé SANS accent (« echoue »).

## Format des tâches (`taches.json`)

Proche du banc d'agent principal (`id`, `type`, `question`, `vocabulaire`, `facts`, `cortex`,
`sans_cortex`) avec un champ `fichier` en plus :

- `facts` : chaînes à retrouver dans la sortie (sous-chaîne exacte, sensible à la casse ;
  `ou:a||b` = l'une des alternatives). Toutes doivent être présentes : c'est la réussite.
- `cortex` : étapes `peek <fichier> [-e] [-n N] question…`. Comme pour le banc d'agent
  principal, une étape n'utilise que les mots de la question, de son `vocabulaire`, du nom du
  fichier et des sorties précédentes (une étape qui triche n'est PAS jouée et fait alerte).
- `sans_cortex` : `read <fichier>` = tout lire. Un Read lit 2 000 lignes par appel : le banc
  compte une page par appel (37 appels pour les 10 tâches) et les jetons du fichier entier
  numéroté. La colonne « 1re page » donne, à titre indicatif, ce que verrait un seul Read.
- `connu_ko` (facultatif) : défaut de peek déjà constaté (raison). L'échec reste affiché et
  alerte (code 3), mais ne fait pas échouer `cargo test`. À retirer dès que peek est corrigé.
- `seuils` (en tête du fichier) : `reussite_pct` (100 : toutes les tâches réussies) et
  `ratio_gros` (10 : jetons Read complet / jetons peek, sur les tâches de type `gros`).

La validité des tâches est contrôlée : un fait absent du log complet est une alerte
« tâche invalide ». Ajouter une tâche = un log dans `fixtures/` (ou une génération dans
`gen.mjs`), ses faits vérifiés dans le log, une question dans les mots d'un agent.

## Résultats de référence (binaire debug, 20/11/2026, commit d653b7c)

```
tâche                  type   lignes | Read complet (appels, jetons) | peek (appels, jetons, faits) | ratio
vitest-echec         vitest       64 |   1      635 | 1    484 3/3 |   1.3x
cargo-echec           cargo       86 |   1     1027 | 1    767 3/4 |   1.3x   (connu_ko)
pytest-echec         pytest       42 |   1      450 | 1    430 4/4 |   1.0x
eslint-echec         eslint       19 |   1      224 | 1    185 3/3 |   1.2x
go-echec                 go       35 |   1      400 | 1    352 3/3 |   1.1x
fr-accents         francais       58 |   1     1170 | 1    752 3/3 |   1.6x
gros-cargo             gros    20000 |  10   363516 | 1   1143 0/4 | 318.0x   (connu_ko)
gros-cargo-erreurs     gros    20000 |  10   363516 | 1    582 3/3 | 624.6x
gros-vitest            gros    20000 |  10   327189 | 1    982 3/3 | 333.2x
vert-vitest            vert     1219 |   1    17708 | 1    860 2/2 |  20.6x
TOTAL                                |  37  1075835 | 10  6537 27/32 | 164.6x
```

Lecture honnête :

- Sur les petits logs (moins de ~100 lignes), peek ne fait gagner presque rien (x1,0 à x1,6) : le
  log tient déjà dans quelques centaines de jetons. Le gain est sur les gros logs (x300 à x600)
  et sur un log vert de 1 200 lignes (x20 : « tout est vert » en 860 jetons au lieu de 17 700).
- Réussite actuelle : 8 tâches sur 10, aucune alerte inattendue. Deux défauts de peek, mesurés
  et marqués `connu_ko`, restent à corriger (le banc reste à code 3 tant qu'ils existent) :
  1. `cargo-echec` : le 2e bloc de panic est replié avec le 1er et son message
     (`called Option::unwrap() on a None value`) tombe sous le plafond de 60 lignes.
  2. `gros-cargo` : la question « pourquoi cargo test échoue-t-il ? » ne contient aucun terme
     présent dans le log ; le repli sur les signaux d'échec est noyé par des tests RÉUSSIS dont
     le nom contient error/failed/panic. Avec `--erreurs` (`gros-cargo-erreurs`), la même
     information sort en 582 jetons.
- Le Read complet réussit toujours (10 tâches sur 10 : les faits sont dans le log) mais, sur
  les gros logs, il coûte 10 appels et 330 000 à 360 000 jetons, soit plus que la fenêtre de
  contexte d'un agent ; le Read d'une seule page (2 000 lignes) rate les gros logs : l'échec est
  en fin de fichier.

## Limites

- Les tâches sont scriptées (une question par tâche) : le banc ne mesure pas un agent qui
  reformulerait après un premier échec de peek, ni le coût de la reformulation.
- Jetons = caractères / 4 : estimation commune aux bancs de Cortex, pas un comptage réel d'un
  modèle (les logs avec symboles ou accents coûtent plus de jetons que cette règle).
- Les fixtures `vitest`, `cargo`, `go` et `fr-accents` sont écrites d'après le format documenté,
  pas capturées ; les gros logs sont synthétiques (leurres réalistes mais choisis).
- Le « Read complet » est un plafond de coût (un agent raisonnable lirait la fin du fichier avec
  `tail`) : la comparaison pertinente pour un agent qui connaît son log est `tail -n` ou `grep`,
  non couverte ici.
- Le banc n'est pas rejoué sur AstroQuest (privé) : aucun log du produit n'est utilisé.
