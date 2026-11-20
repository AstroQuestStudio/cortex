---
name: aq-boucle
description: Boucle voir → localiser → corriger → prouver pour réparer un bug d'interface ou une erreur d'exécution avec Spectra + Cortex. À lire dès qu'un écran est cassé, qu'une erreur de console ou une pile (stack trace, vitest, Node) est donnée, qu'une recette Spectra échoue, ou avant de dire « corrigé » pour un correctif visible. Pas pour une tâche sans exécution (refactor pur, doc).
---

# Boucle Spectra × Cortex : voir → localiser → corriger → prouver

Quatre temps, chacun avec UN outil. Ne pas grepper ni lire des fichiers entiers pour localiser.
Chaque étape rend des identifiants (`S:chemin#symbole`) à recopier dans la suivante.

## 1. VOIR (Spectra)
- Rejouer la recette qui échoue : `browser_run_recipe <nom>` (recettes : `.spectra/recipes/*.json`), ou
  `browser_navigate <url>` puis `browser_console_messages level=error` (JSON `spectra.console/1`, avec la
  pile de chaque erreur) ; `browser_snapshot` pour l'état, `browser_screenshot` seulement pour juger le visuel.
- Garder l'élément qui prouve le bug : le JSON de la console (enregistré dans un fichier du projet,
  `_agents/<sujet>/pile.json`), le texte visible qui est faux, le bouton qui ne répond pas.

## 2. LOCALISER (Cortex)
| Preuve en main | Commande |
|---|---|
| Pile d'erreur : console Chrome/Firefox, Node, vitest, panique Rust, ou JSON de Spectra | `cortex trace _agents/<sujet>/pile.json -p <Projet>` (ou `cat pile.json \| cortex trace -p <Projet>`) |
| Texte, `data-testid`, `aria-label`, composant vus à l'écran | `cortex ui "<texte>" --testid x --component A,B -p <Projet>` |
| Seulement une description du bug ou de la tâche | `cortex pack "<description>" -p <Projet>` (fichiers probables, symboles, tests, docs, parcours) |
| Un symbole déjà identifié | `cortex card <id>` puis `cortex read <id>` ; `cortex impact <id>` AVANT de modifier |

`cortex trace` résout les cartes de sources (le serveur de dev Vite sert les cartes ; `--maps dist/` hors ligne) et
rend le symbole, les lignes, le code autour, les appelants et les tests. Premier cadre du PROJET = point de départ.
Plusieurs agents sur la même mission : `CORTEX_MISSION=<id> CORTEX_AGENT=<nom>` dans l'environnement, puis
`--partage` sur `card`/`read` (« déjà lu par l'agent X : résumé ») ; `cortex registre` liste ce qui a été lu.

## 3. CORRIGER (Cortex)
`cortex edit` : un lot atomique, plusieurs fichiers en un appel (tout ou rien, CRLF/BOM gardés, garde de syntaxe,
annulable) :
```
cortex edit <<'EOF'
@ "S:src/lib/x.ts#fn"
< ancien texte
> nouveau texte
EOF
```
`--essai` pour prévisualiser, `--annuler dernier` pour défaire. Un seul lot par correctif ; pas de Read complet
avant d'éditer un symbole localisé.

## 4. PROUVER (Cortex + Spectra, dans cet ordre)
1. `cortex impact --tests --parcours -p <Projet>` : les tests à lancer pour les fichiers modifiés (classés) et les
   recettes Spectra dont l'écran dépend du changement. `--liste` ne rend que les chemins.
2. Lancer ces tests : `npm run test:resume -- $(cortex impact --tests --liste -p <Projet>) --maxWorkers=1`
   (lire le résumé exact : `Test Files N passed`, un `exit 0` seul ne prouve rien).
3. Rejouer la recette de l'étape 1 (puis celles de `replay:`) : `browser_run_recipe <nom>`, console propre
   (`browser_console_messages level=error` vide). Pour une interface : Chrome ET Firefox.
4. Un test qui échoue : `cortex peek <log> -e` pour le bloc d'échec, `cortex trace` sur sa pile, retour à l'étape 3.

Dire « corrigé » seulement avec : la pile ou le symptôme AVANT, les tests lancés (nombre passés), la recette rejouée.

## Si un outil manque
- Pas de cartes (serveur de dev éteint, build sans `.map`) : `trace` rend la fonction de la pile avec « line approximate ».
- Atlas pas à jour : il se met à jour seul à chaque appel (pas de `cortex update`).
- Projet inconnu : `cortex index <chemin> --name <Projet>`.
- Cortex a raté : noter `Cortex raté : "<question>" → <fichier:ligne>`.
