//! Schéma rkyv de l'atlas — la forme sur DISQUE est la forme en MÉMOIRE. Un
//! segment ouvert par mmap ne se désérialise pas : `rkyv::archived_root` rend
//! directement des références vers les octets du fichier (zero-copy, §4 de
//! l'architecture v2).
//!
//! ## Segments, identifiants globaux, versions
//!
//! L'atlas est une pile de segments (manifeste, du plus ancien au plus récent) :
//! un TRONC (base, tout le projet) puis des DELTAS (une mise à jour chacun).
//! Chaque nœud (fichier ou symbole) a un identifiant GLOBAL stable, jamais
//! réattribué : un segment CRÉE les ids `[id_base, id_base + n_new)` (ses
//! `nodes[..n_new]`) et peut porter une NOUVELLE VERSION de nœuds existants
//! (`nodes[n_new..]`, ids dans `override_ids`). La version COURANTE d'un nœud
//! est celle du segment le plus récent qui en porte une ; un id présent dans
//! `tombstones` d'un segment est mort à partir de ce segment.
//!
//! Tout ce qu'un segment dit d'un nœud (contenu, liens SORTANTS, postings BM25F,
//! entrées d'index de noms appelés/d'imports) n'est valide que si CE segment
//! porte la version courante du nœud : une version plus récente ailleurs rend
//! caduques toutes ces entrées d'un coup, sans réécrire l'ancien segment
//! (jamais réécrit : piège Windows, voir `manifest.rs`). Les index d'IDENTITÉ
//! (nom → symboles, chemin → fichier) ne dépendent que de la vie du nœud : le
//! nom d'un symbole et le chemin d'un fichier ne changent jamais pour un id
//! donné (un renommage = un id mort + un id neuf).
//!
//! Voir `atlas::view` (lecture fusionnée) et `atlas::incremental` (écriture
//! d'un delta).

use rkyv::{Archive, Deserialize, Serialize};

/// Version du format — un manifeste qui pointe vers une autre version force une
/// réindexation complète plutôt qu'une lecture d'octets mal interprétés.
/// v3 : ids globaux + deltas lus en fusion, références brutes persistées,
/// automates fst (vocabulaire, chemins) persistés par segment.
/// v4 : champ « corps » (sac de termes racinés par fichier, index inversé et
/// statistiques propres) ; termes des champs noms/commentaires racinés.
/// v5 : rôle en une phrase (`summary`) des symboles et fichiers, signatures,
/// cartes I7 avec identifiants stables (`S:chemin#nom`).
pub const FORMAT_VERSION: u32 = 5;

/// Aucune valeur (id de chaîne absent, pas de fichier propriétaire…).
pub const NONE: u32 = u32::MAX;

/// Un nœud du graphe de connaissance (§3) : fichier ou symbole. Champs à plat
/// (pas d'enum à variantes) pour rester simple à archiver.
#[derive(Archive, Serialize, Deserialize, Debug, Clone)]
#[archive(check_bytes)]
pub struct AtlasNode {
    /// 0 = File, 1 = Symbol.
    pub kind: u8,
    /// `symbol::SymbolKind as u8` (0 si `kind == File`).
    pub sym_kind: u8,
    /// Id de chaîne : chemin (File) ou nom du symbole (Symbol).
    pub name: u32,
    /// Id GLOBAL du fichier propriétaire (`NONE` pour un fichier).
    pub owner_file: u32,
    /// Rang du symbole dans son fichier (ordre d'extraction) — l'ordre
    /// canonique (chemin, rang) remplace l'ordre des ids pour tout affichage et
    /// tout départage : c'est ce qui rend « tronc + deltas » identique à une
    /// reconstruction complète.
    pub ord: u32,
    pub line: u32,
    pub end_line: u32,
    /// Id de chaîne de la signature, `NONE` si absente.
    pub signature: u32,
    /// Tokens du nom décomposé (Symbol uniquement).
    pub tokens: Vec<u32>,
    /// Doc-comment (Symbol) ou en-tête de fichier (File) — corpus "commentaires".
    pub doc: Vec<u32>,
    /// Tokens du chemin (File uniquement — bonus "terme dans le chemin").
    pub path_tokens: Vec<u32>,
    /// Nombre d'appels AMBIGUS depuis ce symbole (jamais résolus en arête).
    pub ambiguous_calls: u32,
    /// Id de chaîne de la carte I7 précompilée (`NONE` si absente).
    pub card: u32,
    /// Id de chaîne du rôle en une phrase (doc-comment d'un symbole, en-tête
    /// d'un fichier), `NONE` si absent.
    pub summary: u32,
    // ── Champs FICHIER (0/NONE pour un symbole) ─────────────────────────────
    /// Dernière modification en MICROsecondes epoch (contrôle de fraîcheur).
    pub mtime: u64,
    pub size: u64,
    pub lines: u32,
    /// Id de chaîne du hash blake3 hex.
    pub hash: u32,
    pub lang: u8,
    /// Index dans `AtlasSegment.refs` (références brutes du fichier), `NONE`
    /// pour un symbole.
    pub refs: u32,
    /// Champ « corps » (FICHIER seulement) : somme des occurrences des termes
    /// du contenu (longueur BM25 du champ). Les termes eux-mêmes ne vivent QUE
    /// dans les postings du segment (`InvertedIndex.body_*`) : le sac d'un
    /// fichier s'y relit quand il faut le ré-émettre (`Handle::body_of`), ce
    /// qui évite de le stocker deux fois (≈ 7 Mo sur AstroQuest).
    pub body_len: u32,
}

/// Un appel/référence brut (nom, ligne) — `symbol::CallRef` en ids de chaîne.
#[derive(Archive, Serialize, Deserialize, Debug, Clone, Copy)]
#[archive(check_bytes)]
pub struct CallRefA {
    pub name: u32,
    pub line: u32,
}

/// Références SORTANTES brutes d'un fichier (`symbol::FileRefs`) : persistées
/// pour pouvoir RE-RÉSOUDRE un fichier inchangé quand un autre change (mise à
/// jour incrémentale) et pour matérialiser un `ProjectIndex` complet (compaction,
/// galaxie) sans relire le disque.
#[derive(Archive, Serialize, Deserialize, Debug, Clone, Default)]
#[archive(check_bytes)]
pub struct FileRefsA {
    pub imports: Vec<u32>,
    pub imported_names: Vec<u32>,
    pub calls: Vec<CallRefA>,
    pub db_refs: Vec<u32>,
}

/// Listes d'adjacence compressées. Deux formes :
/// - DENSE (`keys` vide) : la clé `k` est l'index `k` (liens SORTANTS, indexés
///   par index LOCAL de nœud dans le segment ; liens entrants du tronc, dont les
///   ids globaux valent les index locaux) ;
/// - CREUSE (`keys` trié) : la clé est cherchée par dichotomie (liens entrants
///   d'un delta, indexés par id GLOBAL de la cible).
/// `offsets[i]..offsets[i+1]` indexe `targets` pour la i-ème clé. L'ordre des
/// cibles d'une clé est l'ordre d'insertion (voir `build`).
#[derive(Archive, Serialize, Deserialize, Debug, Clone, Default)]
#[archive(check_bytes)]
pub struct Csr {
    pub keys: Vec<u32>,
    pub offsets: Vec<u32>,
    pub targets: Vec<u32>,
}

impl Csr {
    /// `dense` sans consommer les listes.
    pub fn dense_ref(lists: &[Vec<u32>]) -> Csr {
        let mut offsets = Vec::with_capacity(lists.len() + 1);
        offsets.push(0u32);
        let mut targets = Vec::with_capacity(lists.iter().map(|l| l.len()).sum());
        for l in lists {
            targets.extend_from_slice(l);
            offsets.push(targets.len() as u32);
        }
        Csr { keys: Vec::new(), offsets, targets }
    }

    /// CSR DENSE à `n` clés à partir de listes déjà groupées par clé.
    pub fn dense(lists: Vec<Vec<u32>>) -> Csr {
        let mut offsets = Vec::with_capacity(lists.len() + 1);
        offsets.push(0u32);
        let mut targets = Vec::new();
        for l in lists {
            targets.extend(l);
            offsets.push(targets.len() as u32);
        }
        Csr { keys: Vec::new(), offsets, targets }
    }

    /// CSR CREUSE à partir de paires (clé, cible) — tri STABLE par clé : l'ordre
    /// relatif des cibles d'une même clé est conservé.
    pub fn sparse(mut pairs: Vec<(u32, u32)>) -> Csr {
        pairs.sort_by_key(|&(k, _)| k);
        let mut keys = Vec::new();
        let mut offsets = vec![0u32];
        let mut targets = Vec::with_capacity(pairs.len());
        for (k, t) in pairs {
            if keys.last() != Some(&k) {
                if !keys.is_empty() {
                    offsets.push(targets.len() as u32);
                }
                keys.push(k);
            }
            targets.push(t);
        }
        if !keys.is_empty() {
            offsets.push(targets.len() as u32);
        }
        Csr { keys, offsets, targets }
    }
}

impl ArchivedCsr {
    /// Cibles de la clé `k` (vide si absente).
    pub fn get(&self, k: u32) -> &[u32] {
        let i = if self.keys.is_empty() {
            k as usize
        } else {
            match self.keys.binary_search(&k) {
                Ok(i) => i,
                Err(_) => return &[],
            }
        };
        if i + 1 >= self.offsets.len() {
            return &[];
        }
        &self.targets[self.offsets[i] as usize..self.offsets[i + 1] as usize]
    }
}

/// Un "posting" du champ commentaires : nœud (index LOCAL) + occurrences.
#[derive(Archive, Serialize, Deserialize, Debug, Clone, Copy)]
#[archive(check_bytes)]
pub struct ComPosting {
    pub node: u32,
    pub tf: u32,
}

/// Index inversé BM25F (§4, §7) DU SEGMENT : ses propres termes et postings,
/// sur les versions de nœuds qu'il porte (index LOCAUX). La lecture fusionne
/// les segments (`atlas::query`) en ignorant les postings des versions caduques.
/// Deux champs : "noms" (tokens de symboles, une entrée par occurrence) et
/// "commentaires" (doc de symbole + en-tête de fichier, avec occurrences).
#[derive(Archive, Serialize, Deserialize, Debug, Clone, Default)]
#[archive(check_bytes)]
pub struct InvertedIndex {
    /// Id de chaîne de chaque terme (index = id de terme LOCAL au segment).
    pub vocab: Vec<u32>,
    pub name_postings_off: Vec<u32>,
    pub name_postings: Vec<u32>,
    pub com_postings_off: Vec<u32>,
    pub com_postings: Vec<ComPosting>,
    /// Champ « corps » : vocabulaire PROPRE (ids de chaîne des termes racinés,
    /// index = id de terme de corps local) et postings en tableaux parallèles
    /// (fichier local, occurrences) — 6 octets par posting au lieu de 8.
    pub body_vocab: Vec<u32>,
    pub body_postings_off: Vec<u32>,
    pub body_post_nodes: Vec<u32>,
    pub body_post_tf: Vec<u16>,
}

/// Contribution NETTE du segment aux statistiques de corpus BM25F : absolue
/// pour le tronc, différence (ajouts − retraits des versions remplacées ou
/// mortes) pour un delta. La somme sur les segments actifs est exacte.
#[derive(Archive, Serialize, Deserialize, Debug, Clone, Copy, Default)]
#[archive(check_bytes)]
pub struct CorpusStats {
    pub name_docs: i64,
    pub name_len: i64,
    pub com_docs: i64,
    pub com_len: i64,
    pub body_docs: i64,
    pub body_len: i64,
}

/// Table triée chaîne → valeurs (dichotomie sur les chaînes du segment) : sert
/// aux index nom → symboles, chemin → fichier, nom appelé → fichiers appelants,
/// base d'import → fichiers importeurs.
#[derive(Archive, Serialize, Deserialize, Debug, Clone, Default)]
#[archive(check_bytes)]
pub struct KeyTable {
    /// Ids de chaîne, triés par le contenu de la chaîne (octets).
    pub keys: Vec<u32>,
    pub offsets: Vec<u32>,
    pub vals: Vec<u32>,
}

/// Un segment de l'atlas — voir l'en-tête du fichier.
#[derive(Archive, Serialize, Deserialize, Debug, Clone)]
#[archive(check_bytes)]
pub struct AtlasSegment {
    pub format_version: u32,
    /// Premier id global CRÉÉ par ce segment.
    pub id_base: u32,
    /// `nodes[..n_new]` sont les nœuds créés (ids `id_base..id_base+n_new`).
    pub n_new: u32,
    pub strings: Vec<String>,
    pub nodes: Vec<AtlasNode>,
    /// Id global de `nodes[n_new + i]` (nouvelles versions de nœuds existants).
    pub override_ids: Vec<u32>,
    /// Ids globaux morts à partir de ce segment.
    pub tombstones: Vec<u32>,
    /// Mise à jour de (mtime, taille) SANS nouvelle version (fichier touché mais
    /// contenu identique) : (id global, mtime, taille).
    pub meta_overrides: Vec<(u32, u64, u64)>,
    /// Références brutes des fichiers (`AtlasNode.refs`).
    pub refs: Vec<FileRefsA>,
    // Liens SORTANTS des versions portées ici (DENSE, index local → ids globaux).
    pub contains: Csr,
    pub imports: Csr,
    pub calls: Csr,
    // Liens ENTRANTS déclarés par ce segment (id global cible → ids globaux sources).
    pub imports_rev: Csr,
    pub calls_rev: Csr,
    pub inverted: InvertedIndex,
    /// Automate fst du vocabulaire du segment (terme → id de terme local) —
    /// lu tel quel dans le mmap, jamais reconstruit à la requête.
    pub vocab_fst: Vec<u8>,
    /// Automate fst du vocabulaire du CORPS (terme → id de terme de corps local).
    pub body_fst: Vec<u8>,
    /// Automate fst des tokens de chemin des versions de fichiers du segment
    /// (token → index dans `path_tok_off`), le texte de chaque token (id de
    /// chaîne) et leurs fichiers (index LOCAUX).
    pub path_fst: Vec<u8>,
    pub path_tok_sids: Vec<u32>,
    pub path_tok_off: Vec<u32>,
    pub path_tok_files: Vec<u32>,
    pub stats: CorpusStats,
    /// Nom de symbole en minuscules → ids globaux (symboles CRÉÉS ici).
    pub defs: KeyTable,
    /// Chemin → id global de fichier (fichiers CRÉÉS ici).
    pub paths: KeyTable,
    /// Chemin sans extension connue → ids globaux de fichiers (créés ici).
    pub stems: KeyTable,
    /// Nom appelé (minuscules) → index LOCAUX des versions de fichiers qui l'appellent.
    pub call_names: KeyTable,
    /// Base d'import normalisée → index LOCAUX des versions de fichiers qui l'importent.
    pub import_bases: KeyTable,
}
