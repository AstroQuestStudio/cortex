//! Écriture d'un segment : `SegBuilder` (commun au tronc et aux deltas) et
//! `build_full` (tronc complet à partir d'un `ProjectIndex` déjà extrait par
//! tree-sitter). La résolution des relations est `graph` (un seul algorithme
//! pour la base et pour les deltas) ; ce module ne fait que PERSISTER.

use super::cards::render_card;
use super::schema::*;
use crate::fx::{FxHashMap, FxHashSet};
use crate::graph::{self, Cand, FileCalls, IndexUniverse};
use crate::index::ProjectIndex;
use crate::symbol::tokenize_identifier;

/// Table de chaînes dédupliquées en construction.
#[derive(Default)]
pub struct StringInterner {
    map: FxHashMap<String, u32>,
    pub strings: Vec<String>,
}

impl StringInterner {
    /// Ajoute une chaîne SANS déduplication (texte unique par construction,
    /// ex. la carte I7 d'un symbole) : pas de hachage.
    pub fn push_unique(&mut self, s: String) -> u32 {
        self.strings.push(s);
        (self.strings.len() - 1) as u32
    }

    pub fn intern(&mut self, s: &str) -> u32 {
        if let Some(&id) = self.map.get(s) {
            return id;
        }
        let id = self.strings.len() as u32;
        self.strings.push(s.to_string());
        self.map.insert(s.to_string(), id);
        id
    }
    pub fn intern_many<S: AsRef<str>>(&mut self, ss: &[S]) -> Vec<u32> {
        ss.iter().map(|s| self.intern(s.as_ref())).collect()
    }
}

/// Contenu d'une version de FICHIER à écrire.
pub struct FileData<'a> {
    pub path: &'a str,
    pub lang: u8,
    pub hash: &'a str,
    pub size: u64,
    pub mtime: u64,
    pub lines: u32,
    pub header: Vec<&'a str>,
    /// Rôle en une phrase (vide si aucun).
    pub summary: &'a str,
    pub imports: Vec<&'a str>,
    pub imported_names: Vec<&'a str>,
    pub calls: Vec<(&'a str, u32)>,
    pub db_refs: Vec<&'a str>,
    /// Champ « corps » : (terme déjà raciné, occurrences), trié par terme.
    pub body: Vec<(&'a str, u32)>,
}

/// Contenu d'une version de SYMBOLE à écrire.
pub struct SymData<'a> {
    pub name: &'a str,
    pub kind: u8,
    pub ord: u32,
    pub owner_file: u32,
    pub line: u32,
    pub end_line: u32,
    pub signature: &'a str,
    pub tokens: Vec<&'a str>,
    pub doc: Vec<&'a str>,
    /// Rôle en une phrase (vide si aucun).
    pub summary: &'a str,
    pub ambiguous: u32,
    pub card: String,
}

/// Ce que `add_file` calcule SANS toucher au segment (découpes, racines,
/// minuscules) : calculable en parallèle, fichier par fichier.
pub struct FilePre {
    pub path_tokens: Vec<String>,
    /// Tokens de l'en-tête, racinés (`stem::stem`), dans l'ordre.
    pub header_stems: Vec<String>,
    /// Noms appelés distincts (minuscules ASCII), ordre de première apparition.
    pub call_keys: Vec<String>,
    /// Bases d'import distinctes (`graph::spec_base`), ordre de première apparition.
    pub import_bases: Vec<String>,
}

impl FilePre {
    pub fn of(f: &FileData) -> FilePre {
        let mut call_keys: Vec<String> = Vec::new();
        let mut seen: FxHashSet<String> = FxHashSet::default();
        for &(n, _) in &f.calls {
            let k = n.to_ascii_lowercase();
            if !seen.contains(&k) {
                seen.insert(k.clone());
                call_keys.push(k);
            }
        }
        let dir = graph::dir_of(f.path);
        let mut import_bases: Vec<String> = Vec::new();
        for spec in &f.imports {
            if let Some(b) = graph::spec_base(dir, spec) {
                if !import_bases.contains(&b) {
                    import_bases.push(b);
                }
            }
        }
        FilePre {
            path_tokens: tokenize_identifier(f.path),
            header_stems: f.header.iter().map(|t| crate::stem::stem(t)).collect(),
            call_keys,
            import_bases,
        }
    }
}

/// Ce que `add_symbol` calcule sans toucher au segment.
pub struct SymPre {
    pub token_stems: Vec<String>,
    pub doc_stems: Vec<String>,
    pub name_lower: String,
}

impl SymPre {
    pub fn of(s: &SymData) -> SymPre {
        SymPre {
            token_stems: s.tokens.iter().map(|t| crate::stem::stem(t)).collect(),
            doc_stems: s.doc.iter().map(|t| crate::stem::stem(t)).collect(),
            name_lower: graph::call_key(s.name),
        }
    }
}

/// Contribution d'un nœud aux statistiques de corpus (voir `CorpusStats`) —
/// mêmes règles que l'accumulation de postings ci-dessous.
pub fn contribution(kind: u8, n_tokens: usize, n_doc: usize, body_len: u32) -> CorpusStats {
    let mut c = CorpusStats::default();
    if kind == 0 && body_len > 0 {
        c.body_docs = 1;
        c.body_len = body_len as i64;
    }
    if kind == 1 && n_tokens > 0 {
        c.name_docs = 1;
        c.name_len = n_tokens as i64;
    }
    if n_doc > 0 {
        c.com_docs = 1;
        c.com_len = n_doc as i64;
    }
    c
}

/// Constructeur de segment. Les nœuds CRÉÉS doivent être ajoutés avant les
/// nouvelles versions de nœuds existants (`nodes[..n_new]` puis le reste).
pub struct SegBuilder {
    pub st: StringInterner,
    id_base: u32,
    nodes: Vec<AtlasNode>,
    gids: Vec<u32>,
    n_new: u32,
    override_ids: Vec<u32>,
    refs: Vec<FileRefsA>,
    contains: Vec<Vec<u32>>,
    imports: Vec<Vec<u32>>,
    calls: Vec<Vec<u32>>,
    pub tombstones: Vec<u32>,
    pub meta_overrides: Vec<(u32, u64, u64)>,
    /// Id de terme par id de chaîne (`u32::MAX` : pas un terme).
    vocab: Vec<u32>,
    vocab_list: Vec<u32>,
    name_post: Vec<Vec<u32>>,
    com_post: Vec<Vec<ComPosting>>,
    /// Id de terme de CORPS par id de chaîne (`u32::MAX` : pas un terme de corps).
    body_vocab: Vec<u32>,
    body_vocab_list: Vec<u32>,
    body_post: Vec<Vec<(u32, u16)>>,
    pub stats: CorpusStats,
    defs: Vec<(u32, u32)>,
    paths: Vec<(u32, u32)>,
    stems: Vec<(u32, u32)>,
    call_names: Vec<(u32, u32)>,
    import_bases: Vec<(u32, u32)>,
}

impl SegBuilder {
    pub fn new(id_base: u32) -> Self {
        SegBuilder {
            st: StringInterner::default(),
            id_base,
            nodes: Vec::new(),
            gids: Vec::new(),
            n_new: 0,
            override_ids: Vec::new(),
            refs: Vec::new(),
            contains: Vec::new(),
            imports: Vec::new(),
            calls: Vec::new(),
            tombstones: Vec::new(),
            meta_overrides: Vec::new(),
            vocab: Vec::new(),
            vocab_list: Vec::new(),
            name_post: Vec::new(),
            com_post: Vec::new(),
            body_vocab: Vec::new(),
            body_vocab_list: Vec::new(),
            body_post: Vec::new(),
            stats: CorpusStats::default(),
            defs: Vec::new(),
            paths: Vec::new(),
            stems: Vec::new(),
            call_names: Vec::new(),
            import_bases: Vec::new(),
        }
    }

    /// Soustrait la contribution d'une version remplacée ou d'un nœud mort.
    pub fn sub_stats(&mut self, c: CorpusStats) {
        self.stats.name_docs -= c.name_docs;
        self.stats.name_len -= c.name_len;
        self.stats.com_docs -= c.com_docs;
        self.stats.com_len -= c.com_len;
        self.stats.body_docs -= c.body_docs;
        self.stats.body_len -= c.body_len;
    }

    fn add_stats(&mut self, c: CorpusStats) {
        self.stats.name_docs += c.name_docs;
        self.stats.name_len += c.name_len;
        self.stats.com_docs += c.com_docs;
        self.stats.com_len += c.com_len;
        self.stats.body_docs += c.body_docs;
        self.stats.body_len += c.body_len;
    }

    /// Id de terme d'un token DÉJÀ raciné (`stem::stem`).
    fn term(&mut self, stemmed: &str) -> usize {
        let sid = self.st.intern(stemmed) as usize;
        if sid >= self.vocab.len() {
            self.vocab.resize(self.st.strings.len().max(sid + 1), u32::MAX);
        }
        if self.vocab[sid] != u32::MAX {
            return self.vocab[sid] as usize;
        }
        let t = self.vocab_list.len();
        self.vocab[sid] = t as u32;
        let sid = sid as u32;
        self.vocab_list.push(sid);
        self.name_post.push(Vec::new());
        self.com_post.push(Vec::new());
        t
    }

    /// Id de terme de CORPS d'une chaîne déjà internée (vocabulaire propre au champ).
    fn body_term(&mut self, sid: u32) -> usize {
        let s = sid as usize;
        if s >= self.body_vocab.len() {
            self.body_vocab.resize(self.st.strings.len().max(s + 1), u32::MAX);
        }
        if self.body_vocab[s] != u32::MAX {
            return self.body_vocab[s] as usize;
        }
        let t = self.body_vocab_list.len();
        self.body_vocab[s] = t as u32;
        self.body_vocab_list.push(sid);
        self.body_post.push(Vec::new());
        t
    }

    /// Postings "commentaires" : (terme, occurrences) — ordre de première
    /// apparition. `stems` : tokens déjà racinés.
    fn add_com(&mut self, local: u32, stems: &[String]) {
        let mut counts: Vec<(usize, u32)> = Vec::new();
        let mut pos: FxHashMap<usize, usize> = FxHashMap::default();
        for t in stems {
            let tid = self.term(t);
            match pos.get(&tid) {
                Some(&i) => counts[i].1 += 1,
                None => {
                    pos.insert(tid, counts.len());
                    counts.push((tid, 1));
                }
            }
        }
        for (tid, tf) in counts {
            self.com_post[tid].push(ComPosting { node: local, tf });
        }
    }

    fn push_node(&mut self, gid: u32, is_new: bool, node: AtlasNode) -> u32 {
        let local = self.nodes.len() as u32;
        if is_new {
            assert_eq!(self.n_new as usize, self.nodes.len(), "atlas: nœuds créés à ajouter avant les nouvelles versions");
            assert_eq!(gid, self.id_base + self.n_new, "atlas: ids créés contigus");
            self.n_new += 1;
        } else {
            self.override_ids.push(gid);
        }
        self.nodes.push(node);
        self.gids.push(gid);
        self.contains.push(Vec::new());
        self.imports.push(Vec::new());
        self.calls.push(Vec::new());
        local
    }

    pub fn add_file(&mut self, gid: u32, is_new: bool, f: &FileData, contains: Vec<u32>, imports: Vec<u32>) -> u32 {
        self.add_file_pre(gid, is_new, f, &FilePre::of(f), contains, imports)
    }

    /// `add_file` avec les calculs sans état déjà faits (`FilePre`, calculable
    /// en parallèle) : ne reste que l'internement, dans le même ordre.
    pub fn add_file_pre(&mut self, gid: u32, is_new: bool, f: &FileData, pre: &FilePre, contains: Vec<u32>, imports: Vec<u32>) -> u32 {
        let path_tokens = self.st.intern_many(&pre.path_tokens);
        let refs = FileRefsA {
            imports: self.st.intern_many(&f.imports),
            imported_names: self.st.intern_many(&f.imported_names),
            calls: f.calls.iter().map(|&(n, line)| CallRefA { name: self.st.intern(n), line }).collect(),
            db_refs: self.st.intern_many(&f.db_refs),
        };
        let refs_idx = self.refs.len() as u32;
        self.refs.push(refs);
        let body: Vec<u32> = f.body.iter().map(|&(t, _)| self.st.intern(t)).collect();
        let body_tf: Vec<u16> = f.body.iter().map(|&(_, n)| n.min(u16::MAX as u32) as u16).collect();
        let body_len: u32 = body_tf.iter().map(|&n| n as u32).sum();
        let node = AtlasNode {
            kind: 0,
            sym_kind: 0,
            name: self.st.intern(f.path),
            owner_file: NONE,
            ord: 0,
            line: 0,
            end_line: 0,
            signature: NONE,
            tokens: Vec::new(),
            doc: self.st.intern_many(&f.header),
            path_tokens,
            ambiguous_calls: 0,
            card: NONE,
            summary: if f.summary.is_empty() { NONE } else { self.st.intern(f.summary) },
            mtime: f.mtime,
            size: f.size,
            lines: f.lines,
            hash: self.st.intern(f.hash),
            lang: f.lang,
            refs: refs_idx,
            body_len,
        };
        let local = self.push_node(gid, is_new, node);
        self.contains[local as usize] = contains;
        self.imports[local as usize] = imports;
        if !f.header.is_empty() {
            self.add_com(local, &pre.header_stems);
        }
        for (sid, tf) in body.into_iter().zip(body_tf) {
            let bt = self.body_term(sid);
            self.body_post[bt].push((local, tf));
        }
        self.add_stats(contribution(0, 0, f.header.len(), body_len));
        if is_new {
            let p = self.st.intern(f.path);
            self.paths.push((p, gid));
            let stem = self.st.intern(graph::strip_known_ext(f.path));
            self.stems.push((stem, gid));
        }
        // Index de re-résolution : noms appelés et bases d'import de CETTE version.
        for k in &pre.call_keys {
            let sid = self.st.intern(k);
            self.call_names.push((sid, local));
        }
        for b in &pre.import_bases {
            let sid = self.st.intern(b);
            self.import_bases.push((sid, local));
        }
        local
    }

    pub fn add_symbol(&mut self, gid: u32, is_new: bool, s: &SymData, calls: Vec<u32>) -> u32 {
        self.add_symbol_pre(gid, is_new, s, &SymPre::of(s), calls)
    }

    /// `add_symbol` avec les racines et le nom en minuscules déjà calculés.
    pub fn add_symbol_pre(&mut self, gid: u32, is_new: bool, s: &SymData, pre: &SymPre, calls: Vec<u32>) -> u32 {
        let signature = if s.signature.is_empty() { NONE } else { self.st.intern(s.signature) };
        let node = AtlasNode {
            kind: 1,
            sym_kind: s.kind,
            name: self.st.intern(s.name),
            owner_file: s.owner_file,
            ord: s.ord,
            line: s.line,
            end_line: s.end_line,
            signature,
            tokens: self.st.intern_many(&s.tokens),
            doc: self.st.intern_many(&s.doc),
            path_tokens: Vec::new(),
            ambiguous_calls: s.ambiguous,
            card: self.st.push_unique(s.card.clone()),
            summary: if s.summary.is_empty() { NONE } else { self.st.intern(s.summary) },
            mtime: 0,
            size: 0,
            lines: 0,
            hash: NONE,
            lang: 0,
            refs: NONE,
            body_len: 0,
        };
        let local = self.push_node(gid, is_new, node);
        self.calls[local as usize] = calls;
        // Champ "noms" : une entrée par occurrence de token (tf implicite).
        for t in &pre.token_stems {
            let tid = self.term(t);
            self.name_post[tid].push(local);
        }
        if !s.doc.is_empty() {
            self.add_com(local, &pre.doc_stems);
        }
        self.add_stats(contribution(1, s.tokens.len(), s.doc.len(), 0));
        if is_new {
            let l = self.st.intern(&pre.name_lower);
            self.defs.push((l, gid));
        }
        local
    }

    fn key_table(st: &StringInterner, mut pairs: Vec<(u32, u32)>) -> KeyTable {
        // Tri des CLÉS distinctes par contenu (peu nombreuses), puis tri stable
        // des paires par rang entier — au lieu de comparer des chaînes pour
        // chacune des centaines de milliers de paires.
        let mut uniq: Vec<u32> = pairs.iter().map(|p| p.0).collect();
        uniq.sort_unstable();
        uniq.dedup();
        uniq.sort_unstable_by(|&a, &b| st.strings[a as usize].as_bytes().cmp(st.strings[b as usize].as_bytes()));
        let mut rank: FxHashMap<u32, u32> = FxHashMap::default();
        for (i, &k) in uniq.iter().enumerate() {
            rank.insert(k, i as u32);
        }
        pairs.sort_by_key(|p| rank[&p.0]);
        let mut keys = Vec::new();
        let mut offsets = vec![0u32];
        let mut vals = Vec::with_capacity(pairs.len());
        for (k, v) in pairs {
            if keys.last() != Some(&k) {
                if !keys.is_empty() {
                    offsets.push(vals.len() as u32);
                }
                keys.push(k);
            }
            vals.push(v);
        }
        if !keys.is_empty() {
            offsets.push(vals.len() as u32);
        }
        KeyTable { keys, offsets, vals }
    }

    /// Tables finales du segment. Les morceaux sont indépendants (liens
    /// entrants, postings, automates fst, index de noms) : construits en
    /// parallèle, chacun de façon déterministe.
    pub fn finish(self) -> AtlasSegment {
        let SegBuilder {
            st,
            id_base,
            nodes,
            gids,
            n_new,
            override_ids,
            refs,
            contains,
            imports,
            calls,
            tombstones,
            meta_overrides,
            vocab: _,
            vocab_list,
            name_post,
            com_post,
            body_vocab: _,
            body_vocab_list,
            body_post,
            stats,
            defs,
            paths,
            stems,
            call_names,
            import_bases,
        } = self;
        // Liens entrants : dense pour un tronc (ids == index locaux), creux sinon.
        let is_base = id_base == 0 && override_ids.is_empty();
        let rev = |lists: &Vec<Vec<u32>>, gids: &Vec<u32>| -> Csr {
            let pairs: Vec<(u32, u32)> = lists.iter().enumerate().flat_map(|(l, ts)| ts.iter().map(move |&t| (t, gids[l]))).collect();
            if is_base {
                let mut dense: Vec<Vec<u32>> = vec![Vec::new(); gids.len()];
                for (t, s) in pairs {
                    dense[t as usize].push(s);
                }
                Csr::dense(dense)
            } else {
                Csr::sparse(pairs)
            }
        };
        let strings = &st.strings;
        let term_fst =
            |list: &[u32]| fst_bytes(list.iter().enumerate().map(|(tid, &sid)| (strings[sid as usize].as_bytes(), tid as u64)).collect());

        let (mut imports_rev, mut calls_rev, mut contains_c, mut imports_c, mut calls_c) = (None, None, None, None, None);
        let (mut names, mut body, mut vocab_fst, mut body_fst, mut path_part) = (None, None, None, None, None);
        let (mut defs_t, mut paths_t, mut stems_t, mut call_names_t, mut import_bases_t) = (None, None, None, None, None);
        rayon::scope(|s| {
            let (imports, calls, gids, nodes) = (&imports, &calls, &gids, &nodes);
            let (vocab_list, body_vocab_list) = (&vocab_list, &body_vocab_list);
            let (rev, term_fst, st) = (&rev, &term_fst, &st);
            s.spawn(|_| imports_rev = Some(rev(imports, gids)));
            s.spawn(|_| calls_rev = Some(rev(calls, gids)));
            s.spawn(|_| vocab_fst = Some(term_fst(vocab_list)));
            s.spawn(|_| body_fst = Some(term_fst(body_vocab_list)));
            s.spawn(|_| path_part = Some(path_tokens_index(nodes, strings)));
            s.spawn(|_| defs_t = Some(Self::key_table(st, defs)));
            s.spawn(|_| paths_t = Some(Self::key_table(st, paths)));
            s.spawn(|_| stems_t = Some(Self::key_table(st, stems)));
            s.spawn(|_| call_names_t = Some(Self::key_table(st, call_names)));
            s.spawn(|_| import_bases_t = Some(Self::key_table(st, import_bases)));
            s.spawn(|_| {
                let v = name_post.len();
                let mut name_postings_off = Vec::with_capacity(v + 1);
                let mut com_postings_off = Vec::with_capacity(v + 1);
                name_postings_off.push(0);
                com_postings_off.push(0);
                let mut name_postings = Vec::new();
                let mut com_postings = Vec::new();
                for (nl, cl) in name_post.into_iter().zip(com_post) {
                    name_postings.extend(nl);
                    name_postings_off.push(name_postings.len() as u32);
                    com_postings.extend(cl);
                    com_postings_off.push(com_postings.len() as u32);
                }
                names = Some((name_postings_off, name_postings, com_postings_off, com_postings));
            });
            s.spawn(|_| {
                let mut body_postings_off = Vec::with_capacity(body_post.len() + 1);
                body_postings_off.push(0);
                let n_body: usize = body_post.iter().map(|l| l.len()).sum();
                let mut body_post_nodes = Vec::with_capacity(n_body);
                let mut body_post_tf = Vec::with_capacity(n_body);
                for bl in body_post {
                    for (node, tf) in bl {
                        body_post_nodes.push(node);
                        body_post_tf.push(tf);
                    }
                    body_postings_off.push(body_post_nodes.len() as u32);
                }
                body = Some((body_postings_off, body_post_nodes, body_post_tf));
            });
            s.spawn(|_| contains_c = Some(Csr::dense(contains)));
            s.spawn(|_| imports_c = Some(Csr::dense_ref(imports)));
            s.spawn(|_| calls_c = Some(Csr::dense_ref(calls)));
        });
        let (name_postings_off, name_postings, com_postings_off, com_postings) = names.unwrap();
        let (body_postings_off, body_post_nodes, body_post_tf) = body.unwrap();
        let (path_tok_sids, path_tok_off, path_tok_files, path_fst) = path_part.unwrap();
        let inverted = InvertedIndex {
            vocab: vocab_list,
            name_postings_off,
            name_postings,
            com_postings_off,
            com_postings,
            body_vocab: body_vocab_list,
            body_postings_off,
            body_post_nodes,
            body_post_tf,
        };
        AtlasSegment {
            format_version: FORMAT_VERSION,
            id_base,
            n_new,
            strings: st.strings,
            nodes,
            override_ids,
            tombstones,
            meta_overrides,
            refs,
            contains: contains_c.unwrap(),
            imports: imports_c.unwrap(),
            calls: calls_c.unwrap(),
            imports_rev: imports_rev.unwrap(),
            calls_rev: calls_rev.unwrap(),
            inverted,
            vocab_fst: vocab_fst.unwrap(),
            body_fst: body_fst.unwrap(),
            path_fst,
            path_tok_sids,
            path_tok_off,
            path_tok_files,
            stats,
            defs: defs_t.unwrap(),
            paths: paths_t.unwrap(),
            stems: stems_t.unwrap(),
            call_names: call_names_t.unwrap(),
            import_bases: import_bases_t.unwrap(),
        }
    }
}

/// Tokens de chemin → fichiers (versions portées par ce segment) : ids de
/// chaîne des tokens (ordre des octets), CSR des fichiers, automate fst.
fn path_tokens_index(nodes: &[AtlasNode], strings: &[String]) -> (Vec<u32>, Vec<u32>, Vec<u32>, Vec<u8>) {
    let mut by_tok: std::collections::BTreeMap<&[u8], (u32, Vec<u32>)> = std::collections::BTreeMap::new();
    for (local, n) in nodes.iter().enumerate() {
        if n.kind != 0 {
            continue;
        }
        for &pt in &n.path_tokens {
            let e = by_tok.entry(strings[pt as usize].as_bytes()).or_insert((pt, Vec::new()));
            if e.1.last() != Some(&(local as u32)) {
                e.1.push(local as u32);
            }
        }
    }
    let mut path_tok_off = vec![0u32];
    let mut path_tok_files = Vec::new();
    let mut path_tok_sids = Vec::with_capacity(by_tok.len());
    let mut path_pairs = Vec::with_capacity(by_tok.len());
    for (i, (tok, (sid, files))) in by_tok.into_iter().enumerate() {
        path_pairs.push((tok, i as u64));
        path_tok_sids.push(sid);
        path_tok_files.extend(files);
        path_tok_off.push(path_tok_files.len() as u32);
    }
    (path_tok_sids, path_tok_off, path_tok_files, fst_bytes(path_pairs))
}

/// Octets d'un automate fst (clés distinctes, triées ici).
fn fst_bytes(mut pairs: Vec<(&[u8], u64)>) -> Vec<u8> {
    pairs.sort_unstable_by(|a, b| a.0.cmp(b.0));
    let mut b = fst::MapBuilder::memory();
    for (k, v) in pairs {
        let _ = b.insert(k, v);
    }
    b.into_inner().expect("fst: construction")
}

/// `FileData` d'une entrée d'index en mémoire.
pub fn file_data(f: &crate::index::FileEntry) -> FileData<'_> {
    FileData {
        path: &f.path,
        lang: f.lang.as_u8(),
        hash: &f.hash,
        size: f.size,
        mtime: f.mtime,
        lines: f.lines,
        header: f.header.iter().map(|s| s.as_str()).collect(),
        summary: &f.summary,
        imports: f.refs.imports.iter().map(|s| s.as_str()).collect(),
        imported_names: f.refs.imported_names.iter().map(|s| s.as_str()).collect(),
        calls: f.refs.calls.iter().map(|c| (c.name.as_str(), c.line)).collect(),
        db_refs: f.refs.db_refs.iter().map(|s| s.as_str()).collect(),
        body: f.body.iter().map(|(t, n)| (t.as_str(), *n)).collect(),
    }
}

/// `FileCalls` (vue de résolution) d'une entrée d'index en mémoire.
pub fn file_calls(f: &crate::index::FileEntry) -> FileCalls<'_> {
    FileCalls::new(
        f.symbols.iter().map(|s| s.name.as_str()),
        f.refs.imported_names.iter().map(|s| s.as_str()),
        f.refs.calls.iter().map(|c| (c.name.as_str(), c.line)).collect(),
    )
}

/// Construit un segment TRONC complet à partir de l'index déjà extrait.
/// Numérotation canonique : fichiers `[0, F)` dans l'ordre des chemins, puis
/// symboles dans l'ordre (fichier, rang).
pub fn build_full(idx: &ProjectIndex) -> AtlasSegment {
    let dbg = std::env::var("CORTEX_DEBUG_TIMING").is_ok();
    let t0 = std::time::Instant::now();
    let u = IndexUniverse::new(idx);
    if dbg {
        eprintln!("[timing] resolution universe: {:.2}ms", t0.elapsed().as_secs_f64() * 1000.0);
    }

    let n_files = idx.files.len();
    let mut first_sym = vec![0u32; n_files];
    let mut cursor = n_files as u32;
    for (fi, f) in idx.files.iter().enumerate() {
        first_sym[fi] = cursor;
        cursor += f.symbols.len() as u32;
    }
    let sym_node = |c: &Cand<usize, (usize, usize)>| first_sym[c.sym.0] + c.sym.1 as u32;
    // Rang d'homonymie de chaque symbole dans son fichier (identifiants stables).
    let ranks: Vec<Vec<u32>> = {
        use rayon::prelude::*;
        idx.files.par_iter().map(|f| crate::ids::ranks(f.symbols.iter().map(|s| (s.name.as_str(), s.kind)))).collect()
    };
    let sym_id = |fi: usize, si: usize| {
        let s = &idx.files[fi].symbols[si];
        crate::ids::sym_id(&idx.files[fi].path, &s.name, s.kind, ranks[fi][si])
    };

    // Résolution (imports, appels), cartes I7 et tout calcul sans état
    // (découpes, racines : `FilePre`, `SymPre`) : indépendants d'un fichier à
    // l'autre (l'univers est en lecture seule) → en parallèle. L'écriture qui
    // suit (internement des chaînes, postings) reste séquentielle, dans l'ordre
    // canonique : le segment est identique au bit près quel que soit le nombre
    // de threads.
    let t1 = std::time::Instant::now();
    use rayon::prelude::*;
    type SymItem<'a> = (SymData<'a>, SymPre, Vec<u32>);
    let prepared: Vec<(FileData, FilePre, Vec<u32>, Vec<SymItem>)> = idx
        .files
        .par_iter()
        .enumerate()
        .map(|(fi, f)| {
            let imported = graph::resolve_imports(&u, fi, &f.path, f.refs.imports.iter().map(|s| s.as_str()));
            let fc = file_calls(f);
            let mut cache = crate::fx::FxHashMap::default();
            let syms = f
                .symbols
                .iter()
                .enumerate()
                .map(|(si, s)| {
                    let (callees, amb) = graph::resolve_symbol_calls(&u, fi, &imported, &fc, s.line, s.end_line, &mut cache);
                    let named: Vec<String> = callees.iter().map(|c| sym_id(c.file, c.sym.1)).collect();
                    let doc: Vec<&str> = s.doc.iter().map(|x| x.as_str()).collect();
                    let card = render_card(&sym_id(fi, si), s.kind, s.line, s.end_line, &s.signature, &s.summary, &named, amb);
                    let sd = SymData {
                        name: &s.name,
                        kind: s.kind.as_u8(),
                        ord: si as u32,
                        owner_file: fi as u32,
                        line: s.line,
                        end_line: s.end_line,
                        signature: &s.signature,
                        tokens: s.tokens.iter().map(|x| x.as_str()).collect(),
                        doc,
                        summary: &s.summary,
                        ambiguous: amb as u32,
                        card,
                    };
                    let pre = SymPre::of(&sd);
                    (sd, pre, callees.iter().map(sym_node).collect())
                })
                .collect();
            let fd = file_data(f);
            let pre = FilePre::of(&fd);
            (fd, pre, imported.iter().map(|&t| t as u32).collect(), syms)
        })
        .collect();
    if dbg {
        eprintln!("[timing] resolution + cards + splits (parallel): {:.2}ms", t1.elapsed().as_secs_f64() * 1000.0);
    }
    let mut sym_items: Vec<(u32, SymItem)> = Vec::with_capacity(cursor as usize - n_files);
    let mut files: Vec<(FileData, FilePre, Vec<u32>)> = Vec::with_capacity(n_files);
    for (fi, (fd, pre, imported, syms)) in prepared.into_iter().enumerate() {
        sym_items.extend(syms.into_iter().enumerate().map(|(si, it)| (first_sym[fi] + si as u32, it)));
        files.push((fd, pre, imported));
    }
    // Internement et postings : séquentiels, dans l'ordre canonique. (Un
    // internement parallèle — ids attribués par tri puis rejoués — a été
    // mesuré PLUS LENT que ce passage séquentiel : 507 ms contre 436 ms à
    // 8 threads sur AstroQuest ; retiré, voir docs/ARCHITECTURE.md §12.)
    let mut b = SegBuilder::new(0);
    let t1 = std::time::Instant::now();
    for (fi, (fd, pre, imported)) in files.into_iter().enumerate() {
        let contains: Vec<u32> = (0..idx.files[fi].symbols.len() as u32).map(|si| first_sym[fi] + si).collect();
        b.add_file_pre(fi as u32, true, &fd, &pre, contains, imported);
    }
    if dbg {
        eprintln!("[timing] file nodes + body postings: {:.2}ms", t1.elapsed().as_secs_f64() * 1000.0);
    }
    let t2 = std::time::Instant::now();
    for (gid, (sd, pre, calls)) in sym_items {
        b.add_symbol_pre(gid, true, &sd, &pre, calls);
    }
    if dbg {
        eprintln!("[timing] symbol nodes + postings: {:.2}ms", t2.elapsed().as_secs_f64() * 1000.0);
    }
    let t3 = std::time::Instant::now();
    let seg = b.finish();
    if dbg {
        eprintln!("[timing] finish (tables, CSR): {:.2}ms", t3.elapsed().as_secs_f64() * 1000.0);
        eprintln!(
            "[size] strings {} · name vocabulary {} · name postings {} · comment postings {} · body vocabulary {} · body postings {}",
            seg.strings.len(),
            seg.inverted.vocab.len(),
            seg.inverted.name_postings.len(),
            seg.inverted.com_postings.len(),
            seg.inverted.body_vocab.len(),
            seg.inverted.body_post_nodes.len()
        );
    }
    seg
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Petit projet varié (imports croisés, appels, homonymes, commentaires,
    /// JSX, Rust, markdown, un binaire) écrit dans un dossier temporaire.
    fn corpus(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cortex-corpus-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for i in 0..40 {
            let sub = format!("src/mod{}", i % 5);
            std::fs::create_dir_all(dir.join(&sub)).unwrap();
            let next = (i + 1) % 40;
            let src = format!(
                "// Module {i} : calcule la valeur numéro {i} et l'état partagé.\nimport {{ valeur{next}, commun }} from '../mod{m}/fichier{next}';\n\n/** Renvoie la valeur {i} (doublée). */\nexport function valeur{i}(x: number) {{\n  return valeur{next}(x) + commun(x) * {i};\n}}\n\nexport const commun = (y: number) => y + {i};\nexport class Service{i} {{ run() {{ return valeur{i}(1); }} }}\n",
                m = next % 5
            );
            std::fs::write(dir.join(format!("{sub}/fichier{i}.ts")), src).unwrap();
        }
        std::fs::write(dir.join("src/App.tsx"), "import { valeur1 } from './mod1/fichier1';\nexport function App() { return <Bouton onClick={() => valeur1(2)} />; }\nfunction Bouton() { return null; }\n").unwrap();
        std::fs::write(dir.join("src/lib.rs"), "//! Bibliothèque de test.\nuse std::fmt;\n/// Structure témoin.\npub struct Temoin;\nimpl Temoin { pub fn neuf() -> Self { Temoin } }\n").unwrap();
        std::fs::write(dir.join("README.md"), "# Projet témoin\n## Construction\nTexte.\n").unwrap();
        std::fs::write(dir.join("src/binaire.ts"), [0u8, 159, 146, 150]).unwrap();
        dir
    }

    /// La construction complète (parcours, analyse, résolution, cartes,
    /// découpes, tables finales) donne le même index et le même segment AU BIT
    /// PRÈS sur 1 thread et sur 8.
    #[test]
    fn parallele_egale_sequentiel() {
        let dir = corpus("par");
        let run = |n: usize| {
            crate::par::with_threads(n, || {
                let (idx, tracked) = crate::index::build_index("cortex-test-par", &dir).unwrap();
                let seg = build_full(&idx);
                let bytes = rkyv::to_bytes::<_, 4096>(&seg).unwrap().to_vec();
                (serde_json::to_string(&idx.files).unwrap(), tracked, bytes)
            })
        };
        let (f1, t1, s1) = run(1);
        let (f8, t8, s8) = run(8);
        assert!(f1.contains("valeur39") && f1.contains("Temoin"));
        assert_eq!(f1, f8, "index différent selon le nombre de threads");
        assert_eq!(t1, t8, "état de parcours différent selon le nombre de threads");
        assert!(s1 == s8, "segment différent selon le nombre de threads ({} / {} octets)", s1.len(), s8.len());
        std::fs::remove_dir_all(&dir).ok();
    }
}
