# `cortex impact --tests` — quels tests lancer pour ce changement

> Statut : livré dans la prochaine version (non publié). CLI `cortex impact --tests [--parcours]`, MCP `cortex_impact` avec `tests: true`.
> Code : `src/outils/selection.rs`. Banc : `cortex bench-tests` (`src/bench_combo.rs`).

`impact <id>` dit déjà « tests à relancer » pour UN symbole. Ici, la question de l'agent est
celle de la fin d'une correction : **« j'ai modifié ces fichiers : lesquels des 1 500 tests
lancer ? »** La réponse est une liste de fichiers de test classés, plus la commande qui les
lance, plus (avec `--parcours`) les recettes Spectra dont l'écran dépend du changement.

```
$ cortex impact --tests --parcours -p MonApp
tests to run: 12 for 3 changed file(s): src/lib/billing/invoice-export.ts, …
  src/lib/billing/__tests__/invoice-date.test.ts — same name, direct import
  src/components/billing/invoice-pdf/__tests__/paymentDetails.test.ts — direct import
  …
run: npm run test:resume -- src/lib/billing/__tests__/invoice-date.test.ts src/components/…
parcours: 2 Spectra recipe(s), d = import hops from the change: factures (d1), page-facturation (d3)
replay: browser_run_recipe factures ; browser_run_recipe page-facturation
```

## Syntaxe

```
cortex impact --tests [CIBLE...] [--parcours] [--depuis <ref>] [--commit <sha>] [--max N] [--liste] [-p <Projet>]
```

| Entrée | Fichiers considérés comme changés |
|---|---|
| (rien) | les fichiers non commités (`git status` + `git diff HEAD`) |
| `CIBLE...` | identifiants (`S:…`, `F:…`), chemins, noms : le fichier de chacun |
| `--depuis <ref>` | modifiés depuis la référence (branche, `HEAD~3`), travail en cours compris |
| `--commit <sha>` | modifiés par ce commit |

- `--liste` : UNIQUEMENT les chemins de test, un par ligne, sans diagnostic :
  `npm run test:resume -- $(cortex impact --tests --liste -p MonApp) --maxWorkers=1`.
- `--max N` : au plus N tests (30 par défaut) ; le reste est compté (« of 54 »).
- MCP : `cortex_impact { cible?, cibles?, tests: true, parcours?, depuis?, commit?, max? }`.

## Classement

Pour chaque fichier changé `s`, un test `t` reçoit des points :

| Signal | Points |
|---|---|
| `t` est lui-même un fichier modifié | 100 (en tête) |
| `t` porte le nom de `s` (`x.test.ts` pour `x.ts`) | 6 |
| `t` importe `s` | 4 |
| `t` importe un fichier qui importe `s` (jusqu'à trois sauts : 1,5 puis 0,5) | 1,5 / 0,5 |
| le TEXTE de `t` cite `s` (chemin, nom de fichier distinctif) : tests « statiques » qui lisent une source | 3 par fichier cité (2 au plus) |
| même dossier, ou nom de fichier contenu | +1,5 / +2 |

Les points d'import sont multipliés par une **spécificité** `1 / (1 + 0,15 × nombre de fichiers
importés par le test)` : un test qui n'importe que 3 fichiers pèse plus qu'un test d'intégration qui
en importe 60. On ne remonte pas à travers un **hub** (fichier importé par plus de 120 fichiers).

Rust : chaque fichier `.rs` changé donne la commande `cargo test --manifest-path <crate>/Cargo.toml
[<module>]` (un seul fichier = filtre sur son nom ; plusieurs = tout le crate).

## `--parcours` : les recettes Spectra

Les recettes d'un projet sont dans `.spectra/recipes/*.json` (pas de base de données à tenir à jour :
l'index de parcours est recalculé à la demande, en un seul passage sur les fichiers d'interface).

1. Signaux d'une recette : textes attendus (`expect`), noms cliqués ou remplis (`click`, `fill`),
   routes visitées (`nav`, `start_url`).
2. Chaque signal est cherché (une seule passe `aho-corasick`) dans les fichiers de code d'interface
   (`src/**/*.{ts,tsx,js,jsx}`, tests exclus). Un texte présent dans plus de 12 fichiers
   (« Annuler ») ou une route dans plus de 8 est trop générique : ignoré.
3. Les fichiers trouvés sont « l'écran » de la recette. Une recette est touchée si un fichier d'écran
   dépend (jusqu'à quatre sauts d'imports) d'un fichier changé. Rendu : `nom (dN)`, N = sauts ;
   les plus proches d'abord, huit au plus, avec la ligne `replay:` prête à coller.

## Limites assumées

- Le graphe d'imports ne voit pas l'`import()` dynamique construit à l'exécution, ni les mocks
  `vi.mock('chemin')` d'un fichier que le test n'importe pas : la citation textuelle rattrape
  une partie de ces cas.
- Un hub très importé (`utils.ts`, 800 importeurs) donne une liste tronquée : le classement privilégie
  les tests les plus spécifiques ; la suite complète reste le filet (`npm test`, une fois en fin).
- Les parcours reposent sur le texte visible et les routes des recettes : une recette qui ne clique
  que sur des icônes sans nom n'est rattachée que par ses routes.

## Mesure

Voir `docs/BENCHMARKS.md` § « tests » (banc sur l'historique git d'AstroQuest).
