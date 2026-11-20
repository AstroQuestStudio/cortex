# `cortex edit` — le « Code Mode » de Cortex

> Statut : conçu et implémenté dans Cortex (0.6.x, non publié). Code : `src/edit.rs`, tests :
> `src/edit_tests.rs`, banc : `_agents/edit/banc/bench.py`.

## Pourquoi

Un agent qui modifie du code paie trois fois : un appel `Edit` par remplacement (chacun relit le
contexte), une lecture préalable de chaque fichier (`Edit` l'exige), et des tours de correction
quand une ancre ne correspond pas (indentation, doublon, fin de ligne). Le « Code Mode » de
Claude Code règle ça en groupant les éditions dans un script, mais il est désactivé dès qu'un mod à
hooks JavaScript contrôle les appels d'outils — et les sous-agents ne l'ont jamais.

`cortex edit` donne la même capacité à TOUT agent qui a un shell (donc aussi aux sous-agents) et,
en outil MCP, à ceux qui n'en ont pas : **un lot d'éditions sur N fichiers = un appel**, atomique,
avec les garde-fous du dépôt dans le moteur lui-même, annulable.

## Commande

```
cortex edit [lot] [-p <Projet>] [--racine <dossier>] [--essai] [--diff] [--forcer "<raison>"]
cortex edit --annuler <id|dernier>
cortex edit --liste
```

`lot` : fichier, ou l'entrée standard (`-` ou absent). Le format (JSON ou texte) est reconnu seul.
Les chemins sont relatifs à la RACINE du projet (pas au dossier courant), ou absolus dans la
racine. Racine : `-p <Projet>` (projet indexé), sinon le projet indexé qui contient un chemin
absolu du lot ou le dossier courant, sinon le dossier courant (sans atlas : pas d'identifiants de
symboles). `--racine <dossier>` l'impose.

### Format texte (le moins cher en jetons, aucun échappement)

```
cortex edit <<'EOF'
# remplacer : l'ancre doit être UNIQUE dans le fichier
@ src/a.ts
< const x = "1";
> const x = "2";
# toutes les occurrences
@ src/b.ts tous
< vieuxNom
> nouveauNom
# insérer des LIGNES après (after) / avant (before) la ligne qui contient l'ancre
@ src/c.ts after
< import a from 'a';
> import b from 'b';
# aucune ligne `>` : supprime les lignes `<`
@ src/d.ts
< // à supprimer
# ajouter en fin de fichier
@ src/e.ts append
> export {};
# créer (new overwrite : réécrire un fichier existant ; options crlf | lf | bom)
@ src/neuf.ts new
> contenu
# remplacer TOUT le symbole (que des lignes `>`)
@ "S:src/a.ts#maFonction"
> function maFonction(a, b) {
>   return a + b;
> }
# remplacer DANS le symbole (l'ancre n'a besoin d'être unique que dans le symbole)
@ "S:src/a.ts#maFonction"
< return a;
> return a * 2;
# renommer un identifiant, mot entier, dans tout le projet (partiel : sous-chaîne)
@ rename Ancien Nouveau dans src
EOF
```

- Une ligne `< texte` / `> texte` : un espace après le signe est retiré, le reste est verbatim
  (indentation comprise). `<` ou `>` seul = ligne vide. `# commentaire` et lignes vides ignorés.
- Une édition = ses lignes `<` PUIS ses lignes `>`. Nouvelle édition : nouveau `@` ; `@` seul = même
  cible que l'édition précédente (plusieurs remplacements dans un fichier sans répéter son nom).
- Option inconnue sur la ligne `@` (faute de frappe : `tuos`) = erreur, jamais ignorée.
- Les identifiants avec espace se citent : `@ "S:src/a.ts#gros fn"`. `chemin:12-30` désigne une plage.

### Format JSON (outil MCP, ou si on préfère)

```json
{"editions":[
  {"fichier":"src/a.ts","ancien":"foo","nouveau":"bar","tous":false},
  {"fichier":"src/a.ts","apres":"import a;","insere":"import b;"},
  {"fichier":"src/a.ts","avant":"export","insere":"// zone"},
  {"fichier":"src/a.ts","ajoute":"export {};"},
  {"fichier":"src/neuf.ts","cree":"contenu\n","ecrase":false,"eol":"crlf","bom":false},
  {"symbole":"S:src/a.ts#f","remplace":"function f() {}"},
  {"symbole":"S:src/a.ts#f","ancien":"return a;","nouveau":"return a * 2;"},
  {"renomme":"Ancien","en":"Nouveau","dans":"src","partiel":false}
]}
```

Ou directement la liste `[…]`. Alias anglais : `file|path old new all after before insert append
create overwrite symbol replace rename to in substring`. Une clé inconnue est une erreur (une faute
de frappe ne se perd pas en silence).

## Sémantique

- **Ancres cherchées dans le fichier d'ORIGINE.** Aucune dépendance d'ordre entre éditions ; deux
  éditions qui se recouvrent sont refusées (« fusionne-les »). Corollaire : on ne peut pas viser du
  texte produit par une autre édition du même lot.
- **Unicité.** `ancien` doit apparaître UNE fois (dans le fichier, ou dans le symbole si on en
  vise un) sauf `tous`. Sinon : le nombre et les lignes des occurrences sont rendus.
- **Ancre tolérante.** Si l'ancre exacte n'existe pas, repli sur des blocs de LIGNES ENTIÈRES égales
  à l'indentation et aux espaces de fin près (cause n°1 des « introuvable »). S'il y a exactement une
  correspondance, elle est utilisée, l'indentation du texte de remplacement est recalée sur celle
  du fichier, et le rapport le dit (`indentation recalée`).
- **Verbatim, sauf tolérance.** Le texte de l'agent est écrit tel quel (indentation comprise) : une ancre
  qui commence au milieu d'une ligne indentée et un remplacement multi-ligne sans indentation donnent des
  lignes sans indentation. Le rapport l'AVERTIT (`⚠ … lignes ajoutées sans indentation`) ; la bonne
  pratique est une ancre de lignes entières avec leur indentation.
- **Insertion = lignes.** `apres` / `avant` insèrent des lignes entières autour de la LIGNE qui contient
  l'ancre (retour à la ligne ajouté si absent). Pour un texte au milieu d'une ligne : `ancien`/`nouveau`.
- **Symbole.** `remplace` remplace les lignes du symbole (signature comprise) telles que l'atlas les
  connaît ; une section Markdown (`D:…`) va jusqu'au titre suivant de même niveau. L'identifiant ambigu
  (nom nu à homonymes) est refusé avec la liste des identifiants exacts. Si la première ligne
  (signature) change et que le symbole a des appelants, le rapport ajoute
  `⚠ <id> : signature modifiée, N appelant(s) à revérifier : …` — le tour d'`impact` qu'on aurait fait après.
- **Renommage.** Mot entier par défaut (les bords identifiants ne se prolongent pas : `foo` ne touche ni
  `fooBar` ni `foo2`). Parcours du projet comme `ignore` (`.gitignore`, fichiers cachés et
  `node_modules` exclus), fichiers binaires ou non UTF-8 ignorés, chemins interdits écartés et comptés.
  Avertit si le nouveau nom existe déjà (collision). Plus de 5 000 occurrences : `--forcer`.

## Atomicité

1. Lecture et localisation de TOUTES les éditions (fichiers chargés une fois, en mémoire).
2. Vérifications : ancres, recouvrements, garde-fous, tailles, **syntaxe**.
3. Une seule erreur : **aucune écriture**, et TOUTES les erreurs sont rendues d'un coup
   (`[n]` = numéro de l'édition) — un tour de correction, pas un par erreur.
4. Sinon : journal d'abord (contenu d'avant + empreintes), puis écriture fichier par fichier
   (fichier temporaire voisin + renommage ; écriture directe en repli si le fichier est tenu ouvert).
   Si une écriture échoue malgré tout (disque plein, verrou), les fichiers déjà écrits sont remis
   dans leur état d'avant et le journal du lot est retiré : rien n'a changé.
5. Un fichier modifié par un autre programme entre la lecture et l'écriture (taille ou date) fait
   refuser le lot (« relance »). Un résultat identique au fichier n'est pas écrit ; un lot sans aucun
   changement n'est pas journalisé.
6. L'index Cortex est rafraîchi pour les fichiers écrits (les `S:…` restent valables dans l'appel suivant).

## Fins de ligne, encodage

Chaque fichier garde les siens, octet pour octet hors des plages éditées :

- CRLF / LF : le texte de l'agent (toujours écrit en `\n`) est converti vers le style DOMINANT du
  fichier ; les lignes non touchées d'un fichier à fins de ligne mixtes restent identiques.
- BOM UTF-8 conservé ; absence de retour à la ligne final conservée.
- Fichier créé : verbatim, LF, sans BOM, sauf `eol` / `bom` ; fichier réécrit (`ecrase`) : style de
  l'ancien fichier sauf indication.
- Refusés (message clair) : UTF-16, octet nul (binaire), UTF-8 invalide. Entrée du lot en UTF-16 ou avec BOM
  (PowerShell) : acceptée.

## Garde-fous

| Garde-fou | Contournable ? |
|---|---|
| `.env*`, `*.env`, `.secrets/`, `.tauri/`, `.git/`, `.ssh/`, `*.pem *.pfx *.p12 id_rsa* id_ed25519* id_ecdsa*` | **Non, jamais** |
| Hors de la racine (`..`, chemin absolu extérieur, lien symbolique ou jonction qui sort, noms courts 8.3, flux `:`, `.env.` à point final) | **Non** |
| `.cortex/edit.toml` (les garde-fous ne se modifient pas par un lot) ; noms réservés Windows | **Non** |
| Liste `interdits` du projet / globale (syntaxe gitignore) | `--forcer "<raison>"` |
| `taille_max_fichier` (défaut 2 Mo), `taille_max_lot` (16 Mo), `max_editions` (1000) | `--forcer "<raison>"` |
| Régression de syntaxe (tree-sitter, JSON) | `--forcer "<raison>"` |

La raison de `--forcer` est exigée, affichée dans le rapport et inscrite au journal. Aucun refus
« dur » ne se lève : un agent ne peut pas écrire un secret en le justifiant.

Contrôle de syntaxe : comparatif, jamais absolu — un fichier qui passe de « valide » à « N erreurs »
est refusé avec la ligne du premier défaut ; un fichier déjà cassé n'est pas bloqué tant que l'édition
n'ajoute pas d'erreur. Langages : ceux de tree-sitter dans Cortex (hors C/C++ : macros) et `.json`
(hors `tsconfig`/`jsconfig`/`.vscode`, qui acceptent les commentaires).

Configuration (facultative) : `<racine>/.cortex/edit.toml` et `~/.cortex/edit.toml` (le projet l'emporte
pour les nombres, les listes s'additionnent) :

```toml
interdits = ["supabase/migrations/**", "*.lock", "src/generated/**"]   # syntaxe gitignore
taille_max_fichier = 1048576
taille_max_lot = 8388608
max_editions = 500
```

## Journal et annulation

`~/.cortex/edit-journal/<id>/` : `lot.json` (état, fichiers, empreintes avant/après, raison du
`--forcer`, dossiers créés) et `avant/` (contenu d'avant, octet pour octet). `<id>` = `AAAAMMJJ-HHMMSS-xxxx`
(UTC). Les 200 derniers lots sont gardés.

- `cortex edit --annuler <id>` (ou `dernier` : le dernier lot encore appliqué) remet les octets d'avant,
  supprime les fichiers créés (et leurs dossiers). **Refusé, sans rien toucher, si un fichier a changé
  depuis le lot** (on n'écrase pas le travail suivant) ; `--forcer "<raison>"` pour passer outre.
- `cortex edit --liste` : les 15 derniers lots.

## Rapport (compact par défaut)

```
cortex edit 20261120-143055-3f9a : 8 éd., 4 fichier(s)
src/peek.rs L40 L52 (indentation recalée)
src/outils/lecture.rs L521 L525
renomme Appel→Requete : 114 occurrence(s), 17 fichier(s)
⚠ S:src/a.ts#f : signature modifiée, 3 appelant(s) à revérifier : S:x.ts#a S:y.ts#b
annuler : cortex edit --annuler 20261120-143055-3f9a
```

Une ligne par fichier (lignes touchées, nombre d'occurrences, étiquettes) ; au-delà de 12 fichiers :
`… +N fichier(s)`. `--diff` : une ligne par édition (verbe, lignes) et un diff court (4 lignes de
chaque côté). `--essai` : même rapport, rien d'écrit ni journalisé. Code de sortie 1 si refusé.

## Outil MCP `cortex_edit`

Description courte (le coût de chaque session) : `edits` (liste d'objets du format JSON) ou `text`
(format texte), `project`, `dry_run`, `diff`, `undo` (id ou `last`), `force` (raison). Même moteur,
mêmes garde-fous, même journal.

## Décisions et limites

- Pas de lot « conditionnel » ni de commande lancée après coup (test, build) : un lot reste une
  transformation de fichiers, vérifiable en essai ; les vérifications se font par les outils habituels.
- Ordre-indépendant (ancres dans l'ORIGINE) plutôt que séquentiel : déterministe, recouvrements
  détectés, essai exact. Le coût : pas d'ancre sur du texte créé par le même lot.
- Pas de suppression ni de déplacement de fichiers (Bash le fait) : `cortex edit` ne détruit pas.
- Les plages de symboles viennent de l'atlas rafraîchi juste avant ; les fichiers sont revérifiés
  (taille, date) avant écriture.
- L'atomicité concerne les fichiers du lot ; elle ne couvre pas un crash du système en pleine
  écriture (le journal permet alors `--annuler`).

## Mesures

Banc `_agents/edit/banc/bench.py` : trois cas réels sur une copie de `src/` de Cortex, comparés à des
appels `Edit` successifs (simulés au plus juste pour `Edit` : un appel par remplacement, résultat d'une
ligne, `replace_all` pour un renommage). Jetons ≈ caractères / 4 (convention des budgets Cortex), 20 jetons
d'enveloppe par appel et par sens. Les arbres de fichiers obtenus sont comparés octet pour octet.

| Cas | `Edit` : appels / jetons | `cortex edit` : appels / jetons | Gain |
|---|---|---|---|
| 1. renommage d'un identifiant (`Appel`→`Requete`, 12 fichiers, 114 occurrences) | 12 / 1 448 | 1 / 259 | appels ÷12, jetons ÷5,6 |
| 2. 8 remplacements dans 4 fichiers | 8 / 904 | 1 / 280 | appels ÷8, jetons ÷3,2 |
| 3. réécriture du corps d'une fonction de 47 lignes (par `S:id`) | 1 / 1 015 | 1 / 640 | jetons ÷1,6 (l'ancien corps n'est pas renvoyé) |

À lire honnêtement :

- Hors du tableau : `Edit` exige un `Read` préalable de chaque fichier (12, 4 et 1 lectures ici, dont le
  contenu n'est PAS compté) ; `cortex edit` n'en demande aucune. Un agent peut regrouper des `Edit`
  dans un seul tour (appels parallèles) : les TOURS baissent alors, pas les jetons ; les sous-agents
  les enchaînent le plus souvent un par un.
- Cas 1 : le renommage par `Edit(replace_all)` est une substitution de SOUS-CHAÎNE. Il a abîmé
  `Appelants d'un symbole` → `Requeteants`, `Appeler formatDate` → `Requeteer formatDate` (commentaire et
  texte de test). `cortex edit` (mot entier) n'y touche pas : les arbres ne sont donc volontairement PAS
  identiques sur ces 3 lignes ; les cas 2 et 3 sont identiques octet pour octet.
- Cas 3 : sur une modification de quelques lignes au milieu d'une longue fonction, `Edit` avec une petite
  ancre coûte autant que `cortex edit` ; le gain du symbole vient de l'ancien corps qu'on n'a pas à recopier.
- Chaque erreur évitée est un tour de moins : ancre tolérante à l'indentation, toutes les erreurs d'un
  lot rendues d'un coup, refus d'une syntaxe cassée avant écriture, avertissement des appelants quand la
  signature d'un symbole change (le tour d'`impact` qu'on aurait fait ensuite).
