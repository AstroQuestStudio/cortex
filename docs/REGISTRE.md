# Registre de lectures partagé

> Statut : livré dans la prochaine version (non publié). Code : `src/outils/registre.rs`. CLI : `cortex registre`, options `--mission`,
> `--agent`, `--partage`, `--frais` sur tous les outils ; MCP : `cortex_registre` et les arguments
> `mission`, `agent`, `partage`, `frais` de `cortex_card` / `cortex_read`.

Plusieurs agents sur le même projet relisent les mêmes fonctions : chacun paie `card` et `read` en
entier, alors qu'un autre les a lus il y a deux minutes. Le registre garde, le temps d'une
**mission**, ce que chaque agent a lu, et rend « déjà lu par l'agent X : résumé » à la place du
contenu complet, si l'appelant le demande.

## Usage

```
# Chaque agent déclare la mission (une fois, dans son environnement) et son nom :
export CORTEX_MISSION=refonte-factures CORTEX_AGENT=agent-1

cortex card S:src/lib/billing/invoice-export.ts#toCompactDate -p MonApp            # lu, noté
CORTEX_AGENT=agent-2 cortex card S:src/lib/billing/invoice-export.ts#toCompactDate --partage
already read by agent agent-1 41s ago (~187 tokens, file unchanged since): S:src/lib/billing/invoice-export.ts#toCompactDate
S:src/lib/billing/invoice-export.ts#toCompactDate fn L424-431
…
next : same call with --frais (MCP frais:true) for the full output

cortex registre            # qui a lu quoi (30 dernières lectures), jetons cumulés
cortex registre --clear    # oublie la mission
```

- **Rien ne change sans mission.** Sans `--mission` / `CORTEX_MISSION`, aucun fichier n'est écrit et
  les sorties sont celles de toujours.
- **Noter** : avec une mission, chaque `card` et `read` réussi est noté (agent, identifiant stable,
  état du fichier, résumé de 4 lignes).
- **Réutiliser** : seulement avec `--partage` (ou `CORTEX_PARTAGE=1`, ou `partage: true` en MCP).
  Un identifiant déjà lu pour le MÊME outil (card ≠ read) et le même contexte, par n'importe quel agent
  de la mission, dont le fichier n'a pas changé (taille et date) depuis la lecture, revient en résumé.
  Un fichier modifié (par `cortex edit` ou à la main) invalide l'entrée : l'agent relit en entier.
- **Forcer** : `--frais` (MCP `frais: true`).
- **MCP** : poser `CORTEX_MISSION` et `CORTEX_AGENT` dans l'environnement du serveur de chaque agent
  suffit ; sinon passer `mission`, `agent` dans l'appel.

## Garanties

- **Local** : un fichier JSONL par (projet, mission) sous `~/.cortex/registre/<projet>/<mission>.jsonl`,
  jamais lu en dehors de sa mission. Aucune fuite entre projets (le projet fait partie de la clé et
  du chemin) ni entre missions.
- **Court terme** : entrées ignorées au bout de 12 h, fichiers de mission effacés après 48 h
  d'inactivité, fichier tronqué de moitié au-delà de 400 Ko.
- **Concurrent** : ajout d'une ligne par écriture (`O_APPEND`) ; une ligne illisible est ignorée.
- Les noms de mission et de projet sont assainis (`[A-Za-z0-9_-]`, 80 caractères) : aucun chemin
  fourni par l'appelant ne sort du dossier du registre.

## Mesure

Voir `docs/BENCHMARKS.md` § « registre » : trois agents relisant les mêmes symboles d'AstroQuest,
jetons avec et sans `--partage`.
