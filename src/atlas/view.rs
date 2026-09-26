//! Lecture FUSIONNÉE « tronc + deltas » (§4) : la version courante de chaque
//! nœud, ses liens, les index de noms/chemins — sans jamais recopier un segment.
//!
//! `View` (construite paresseusement, une fois par `Handle`) donne pour chaque id
//! global le segment et l'index local de sa version COURANTE (ou « mort ») : un
//! passage sur les `override_ids`/`tombstones` des deltas (quelques entrées) et
//! un remplissage identité pour le tronc. Toute entrée d'un segment (posting,
//! lien sortant, lien entrant, nom appelé…) attachée à une version qui n'est plus
//! la courante est ignorée à la lecture — c'est ce qui rend une mise à jour
//! incrémentale correcte sans réécrire les segments plus anciens.
//!
//! Lecture par SEGMENTS : chaque accès fusionné est une boucle indépendante par
//! segment dont les résultats se concatènent (les entrées valides de segments
//! différents portent sur des nœuds différents) — parallélisable plus tard sans
//! changer le format ni les résultats.

use super::schema::*;
use super::Handle;
use crate::index::{FileEntry, ProjectIndex};
use crate::lang::Lang;
use crate::symbol::{CallRef, FileRefs, Symbol, SymbolKind};
use std::collections::HashMap;

pub const DEAD: u16 = u16::MAX;

/// Localisation de la version courante de chaque id global.
pub struct View {
    pub next_id: u32,
    seg_of: Vec<u16>,
    local_of: Vec<u32>,
    /// (mtime, taille) mis à jour sans nouvelle version (fichier touché).
    meta: HashMap<u32, (u64, u64)>,
    pub stats: CorpusStats,
}

impl View {
    pub fn build(segs: &[&ArchivedAtlasSegment]) -> View {
        let next_id = segs.iter().map(|s| s.id_base + s.n_new).max().unwrap_or(0);
        let mut seg_of = vec![DEAD; next_id as usize];
        let mut local_of = vec![0u32; next_id as usize];
        let mut meta: HashMap<u32, (u64, u64)> = HashMap::new();
        let mut stats = CorpusStats::default();
        for (s, seg) in segs.iter().enumerate() {
            let s16 = s as u16;
            for i in 0..seg.n_new {
                let g = (seg.id_base + i) as usize;
                seg_of[g] = s16;
                local_of[g] = i;
            }
            for (j, &g) in seg.override_ids.iter().enumerate() {
                seg_of[g as usize] = s16;
                local_of[g as usize] = seg.n_new + j as u32;
                meta.remove(&g);
            }
            for m in seg.meta_overrides.iter() {
                meta.insert(m.0, (m.1, m.2));
            }
            for &g in seg.tombstones.iter() {
                seg_of[g as usize] = DEAD;
            }
            stats.name_docs += seg.stats.name_docs;
            stats.name_len += seg.stats.name_len;
            stats.com_docs += seg.stats.com_docs;
            stats.com_len += seg.stats.com_len;
            stats.body_docs += seg.stats.body_docs;
            stats.body_len += seg.stats.body_len;
        }
        View { next_id, seg_of, local_of, meta, stats }
    }

    #[inline]
    pub fn loc(&self, g: u32) -> Option<(usize, usize)> {
        let s = *self.seg_of.get(g as usize)?;
        if s == DEAD {
            None
        } else {
            Some((s as usize, self.local_of[g as usize] as usize))
        }
    }

    #[inline]
    pub fn is_current(&self, g: u32, s: usize, local: usize) -> bool {
        self.seg_of.get(g as usize).is_some_and(|&x| x as usize == s) && self.local_of[g as usize] as usize == local
    }
}

/// Une version de nœud : son segment et le nœud archivé.
#[derive(Clone, Copy)]
pub struct NodeRef<'a> {
    pub seg: &'a ArchivedAtlasSegment,
    pub g: u32,
    pub n: &'a ArchivedAtlasNode,
}

impl<'a> NodeRef<'a> {
    #[inline]
    pub fn str(&self, sid: u32) -> &'a str {
        if sid == NONE {
            ""
        } else {
            self.seg.strings[sid as usize].as_str()
        }
    }
    pub fn name(&self) -> &'a str {
        self.str(self.n.name)
    }
    pub fn kind(&self) -> SymbolKind {
        SymbolKind::from_u8(self.n.sym_kind)
    }
    pub fn strs(&self, ids: &'a rkyv::vec::ArchivedVec<u32>) -> Vec<&'a str> {
        ids.iter().map(|&i| self.str(i)).collect()
    }
    pub fn refs(&self) -> Option<&'a ArchivedFileRefsA> {
        if self.n.refs == NONE {
            None
        } else {
            self.seg.refs.get(self.n.refs as usize)
        }
    }
}

/// Recherche d'une clé dans une `KeyTable` (dichotomie sur les chaînes).
pub fn lookup<'a>(seg: &'a ArchivedAtlasSegment, t: &'a ArchivedKeyTable, key: &str) -> &'a [u32] {
    match t.keys.binary_search_by(|&sid| seg.strings[sid as usize].as_str().as_bytes().cmp(key.as_bytes())) {
        Ok(i) => &t.vals[t.offsets[i] as usize..t.offsets[i + 1] as usize],
        Err(_) => &[],
    }
}

impl Handle {
    pub(crate) fn nsegs(&self) -> usize {
        self.opened.len()
    }

    #[inline]
    pub(crate) fn seg(&self, s: usize) -> &ArchivedAtlasSegment {
        self.opened[s].view_unchecked()
    }

    pub(crate) fn v(&self) -> &View {
        self.view.get_or_init(|| {
            let segs: Vec<&ArchivedAtlasSegment> = (0..self.nsegs()).map(|s| self.seg(s)).collect();
            View::build(&segs)
        })
    }

    /// Id global de l'entrée locale `local` du segment `s`.
    #[inline]
    pub(crate) fn gid(&self, s: usize, local: usize) -> u32 {
        let seg = self.seg(s);
        if (local as u32) < seg.n_new {
            seg.id_base + local as u32
        } else {
            seg.override_ids[local - seg.n_new as usize]
        }
    }

    /// Vrai si l'entrée locale `local` du segment `s` est la version courante.
    #[inline]
    pub(crate) fn current_local(&self, s: usize, local: usize) -> Option<u32> {
        let g = self.gid(s, local);
        if self.v().is_current(g, s, local) {
            Some(g)
        } else {
            None
        }
    }

    pub(crate) fn node(&self, g: u32) -> Option<NodeRef<'_>> {
        let (s, local) = self.v().loc(g)?;
        let seg = self.seg(s);
        Some(NodeRef { seg, g, n: &seg.nodes[local] })
    }

    pub(crate) fn alive(&self, g: u32) -> bool {
        self.v().loc(g).is_some()
    }

    /// Toutes les versions COURANTES (fichiers et symboles), segment par segment.
    pub(crate) fn live_nodes(&self) -> impl Iterator<Item = NodeRef<'_>> + '_ {
        (0..self.nsegs()).flat_map(move |s| {
            let seg = self.seg(s);
            (0..seg.nodes.len()).filter_map(move |local| {
                let g = self.current_local(s, local)?;
                Some(NodeRef { seg, g, n: &seg.nodes[local] })
            })
        })
    }

    /// Versions COURANTES des fichiers. Le tronc (segment 0, construction
    /// complète) numérote les fichiers `[0, F)` avant les symboles : on s'y
    /// arrête au premier symbole, sans toucher (ni faire charger par le mmap)
    /// les pages des ~40 k nœuds symboles — le contrôle de fraîcheur en dépend.
    pub(crate) fn live_files(&self) -> impl Iterator<Item = NodeRef<'_>> + '_ {
        (0..self.nsegs()).flat_map(move |s| {
            let seg = self.seg(s);
            let end = if s == 0 { seg.nodes.iter().position(|n| n.kind != 0).unwrap_or(seg.nodes.len()) } else { seg.nodes.len() };
            (0..end).filter_map(move |local| {
                let n = &seg.nodes[local];
                if n.kind != 0 {
                    return None;
                }
                let g = self.current_local(s, local)?;
                Some(NodeRef { seg, g, n })
            })
        })
    }

    /// Rang d'homonymie (1 = premier) d'un symbole vivant dans son fichier —
    /// voir `ids`.
    pub(crate) fn sym_rank(&self, g: u32) -> u32 {
        let Some(r) = self.node(g) else { return 1 };
        if r.n.kind != 1 {
            return 1;
        }
        let heading = r.kind() == SymbolKind::Heading;
        let key = if heading { crate::ids::slug(r.name()) } else { r.name().to_string() };
        let mut k = 0u32;
        for x in self.symbols_of(r.n.owner_file) {
            let same = if heading {
                x.kind() == SymbolKind::Heading && crate::ids::slug(x.name()) == key
            } else {
                x.kind() != SymbolKind::Heading && x.name() == key
            };
            if same {
                k += 1;
            }
            if x.g == g {
                return k.max(1);
            }
        }
        1
    }

    /// Identifiant stable d'un nœud vivant (`S:`, `D:` ou `F:`, voir `ids`).
    pub fn node_id(&self, g: u32) -> String {
        match self.node(g) {
            Some(r) if r.n.kind == 1 => crate::ids::sym_id(self.path_of(r.n.owner_file), r.name(), r.kind(), self.sym_rank(g)),
            Some(r) => crate::ids::file_id(r.name()),
            None => String::new(),
        }
    }

    /// Chemin d'un fichier vivant (chaîne mmap-ée).
    pub(crate) fn path_of(&self, file_g: u32) -> &str {
        self.node(file_g).map(|r| r.name()).unwrap_or("")
    }

    /// Clé d'ordre canonique (chemin, rang) — l'ordre d'une reconstruction complète.
    pub(crate) fn sort_key(&self, g: u32) -> (&str, u32) {
        match self.node(g) {
            Some(r) if r.n.kind == 1 => (self.path_of(r.n.owner_file), r.n.ord),
            Some(r) => (r.name(), 0),
            None => ("", 0),
        }
    }

    /// (mtime µs, taille) courants d'un fichier.
    pub(crate) fn meta(&self, r: &NodeRef) -> (u64, u64) {
        self.v().meta.get(&r.g).copied().unwrap_or((r.n.mtime, r.n.size))
    }

    fn fwd<'a>(&'a self, g: u32, pick: impl Fn(&'a ArchivedAtlasSegment) -> &'a ArchivedCsr) -> &'a [u32] {
        match self.v().loc(g) {
            Some((s, local)) => pick(self.seg(s)).get(local as u32),
            None => &[],
        }
    }

    /// Liens sortants BRUTS (cibles éventuellement mortes) — pour comparer une
    /// re-résolution à l'état courant.
    pub(crate) fn calls_raw(&self, g: u32) -> &[u32] {
        self.fwd(g, |s| &s.calls)
    }
    pub(crate) fn imports_raw(&self, g: u32) -> &[u32] {
        self.fwd(g, |s| &s.imports)
    }
    pub(crate) fn contains_raw(&self, g: u32) -> &[u32] {
        self.fwd(g, |s| &s.contains)
    }

    fn alive_sorted(&self, v: impl Iterator<Item = u32>) -> Vec<u32> {
        let mut out: Vec<u32> = v.filter(|&t| self.alive(t)).collect();
        out.sort_by(|&a, &b| self.sort_key(a).cmp(&self.sort_key(b)));
        out.dedup();
        out
    }

    /// Appelés résolus d'un symbole (vivants, ordre canonique).
    pub(crate) fn callees(&self, g: u32) -> Vec<u32> {
        self.alive_sorted(self.calls_raw(g).iter().copied())
    }
    /// Symboles d'un fichier (vivants, ordre des rangs).
    pub(crate) fn symbols_of(&self, file_g: u32) -> Vec<NodeRef<'_>> {
        let mut v: Vec<NodeRef> = self.contains_raw(file_g).iter().filter_map(|&g| self.node(g)).collect();
        v.sort_by_key(|r| r.n.ord);
        v
    }

    fn rev(&self, g: u32, pick: impl Fn(&ArchivedAtlasSegment) -> &ArchivedCsr) -> Vec<u32> {
        let mut out = Vec::new();
        for s in 0..self.nsegs() {
            for &src in pick(self.seg(s)).get(g) {
                if self.v().loc(src).is_some_and(|(ss, _)| ss == s) {
                    out.push(src);
                }
            }
        }
        out
    }

    /// Appelants (symboles) d'un symbole (ordre canonique).
    pub(crate) fn callers(&self, g: u32) -> Vec<u32> {
        let v = self.rev(g, |s| &s.calls_rev);
        self.alive_sorted(v.into_iter())
    }
    /// Importeurs (fichiers) d'un fichier (ordre canonique).
    pub(crate) fn importers(&self, g: u32) -> Vec<u32> {
        let v = self.rev(g, |s| &s.imports_rev);
        self.alive_sorted(v.into_iter())
    }

    /// Symboles vivants dont le nom en minuscules est `name_lower`.
    pub(crate) fn defs(&self, name_lower: &str) -> Vec<u32> {
        let mut out = Vec::new();
        for s in 0..self.nsegs() {
            let seg = self.seg(s);
            out.extend(lookup(seg, &seg.defs, name_lower).iter().copied().filter(|&g| self.alive(g)));
        }
        out
    }

    pub(crate) fn file_by_path(&self, path: &str) -> Option<u32> {
        (0..self.nsegs()).find_map(|s| {
            let seg = self.seg(s);
            lookup(seg, &seg.paths, path).iter().copied().find(|&g| self.alive(g))
        })
    }

    pub(crate) fn files_by_stem(&self, stem: &str) -> Vec<u32> {
        let mut out = Vec::new();
        for s in 0..self.nsegs() {
            let seg = self.seg(s);
            out.extend(lookup(seg, &seg.stems, stem).iter().copied().filter(|&g| self.alive(g)));
        }
        out
    }

    /// Fichiers (versions courantes) dont les appels contiennent `name_lower`.
    pub(crate) fn files_calling(&self, name_lower: &str) -> Vec<u32> {
        self.content_lookup(name_lower, |s| &s.call_names)
    }

    /// Fichiers (versions courantes) dont un import a pour base `base`.
    pub(crate) fn files_importing_base(&self, base: &str) -> Vec<u32> {
        self.content_lookup(base, |s| &s.import_bases)
    }

    fn content_lookup(&self, key: &str, pick: impl Fn(&ArchivedAtlasSegment) -> &ArchivedKeyTable) -> Vec<u32> {
        let mut out = Vec::new();
        for s in 0..self.nsegs() {
            let seg = self.seg(s);
            for &local in lookup(seg, pick(seg), key) {
                if let Some(g) = self.current_local(s, local as usize) {
                    out.push(g);
                }
            }
        }
        out
    }

    /// Chemins de tous les fichiers vivants (chaînes mmap-ées, sans copie).
    pub fn file_paths(&self) -> Vec<&str> {
        self.live_files().map(|r| r.name()).collect()
    }

    /// Langage de chaque fichier vivant.
    pub fn file_langs(&self) -> Vec<Lang> {
        self.live_files().map(|r| Lang::from_u8(r.n.lang)).collect()
    }

    /// Nombre de (fichiers, symboles, lignes) vivants.
    pub fn counts(&self) -> (usize, usize, u64) {
        let (mut f, mut s, mut l) = (0usize, 0usize, 0u64);
        for r in self.live_nodes() {
            if r.n.kind == 0 {
                f += 1;
                l += r.n.lines as u64;
            } else {
                s += 1;
            }
        }
        (f, s, l)
    }

    /// Symbole englobant la ligne `line` du fichier `path` : le plus INTERNE
    /// (plage la plus courte) hors imports. Rendu : son identifiant stable.
    pub fn enclosing_symbol(&self, path: &str, line: u32) -> Option<String> {
        let fg = self.file_by_path(path)?;
        let mut best: Option<(u32, NodeRef)> = None;
        for r in self.symbols_of(fg) {
            let n = r.n;
            if n.end_line == 0 || matches!(r.kind(), SymbolKind::Import) || line < n.line || line > n.end_line {
                continue;
            }
            let span = n.end_line - n.line;
            if best.as_ref().is_none_or(|(b, _)| span <= *b) {
                best = Some((span, r));
            }
        }
        best.map(|(_, r)| self.node_id(r.g))
    }

    /// `FileEntry` complet (symboles, références brutes) d'un fichier vivant.
    /// Sac de termes du CORPS d'une version de fichier, relu dans les postings
    /// de son segment (une passe sur ce segment), trié par terme — l'ordre de
    /// l'extraction (`symbol::body_terms`).
    pub(crate) fn body_of<'s>(&'s self, r: &NodeRef<'s>) -> Vec<(&'s str, u32)> {
        if r.n.kind != 0 || r.n.body_len == 0 {
            return Vec::new();
        }
        let Some((s, local)) = self.v().loc(r.g) else { return Vec::new() };
        let inv = &self.seg(s).inverted;
        let mut out: Vec<(&str, u32)> = Vec::new();
        for bt in 0..inv.body_vocab.len() {
            let (a, b) = (inv.body_postings_off[bt] as usize, inv.body_postings_off[bt + 1] as usize);
            if let Some(i) = inv.body_post_nodes[a..b].iter().position(|&n| n as usize == local) {
                out.push((r.seg.strings[inv.body_vocab[bt] as usize].as_str(), inv.body_post_tf[a + i] as u32));
            }
        }
        out.sort_unstable_by(|x, y| x.0.cmp(y.0));
        out
    }

    /// Sacs de CORPS de toutes les versions d'un segment (index local → termes),
    /// en une passe — pour la matérialisation complète.
    fn bodies_of_segment(&self, s: usize) -> Vec<Vec<(&str, u32)>> {
        use rayon::prelude::*;
        let seg = self.seg(s);
        let inv = &seg.inverted;
        // Tailles exactes d'abord (pas de réallocation pendant la dispersion).
        let mut count = vec![0u32; seg.nodes.len()];
        for &n in inv.body_post_nodes.iter() {
            count[n as usize] += 1;
        }
        let mut out: Vec<Vec<(&str, u32)>> = count.iter().map(|&c| Vec::with_capacity(c as usize)).collect();
        for bt in 0..inv.body_vocab.len() {
            let term = seg.strings[inv.body_vocab[bt] as usize].as_str();
            let (a, b) = (inv.body_postings_off[bt] as usize, inv.body_postings_off[bt + 1] as usize);
            for (&n, &tf) in inv.body_post_nodes[a..b].iter().zip(&inv.body_post_tf[a..b]) {
                out[n as usize].push((term, tf as u32));
            }
        }
        out.par_iter_mut().for_each(|v| v.sort_unstable_by(|x, y| x.0.cmp(y.0)));
        out
    }

    /// `FileEntry` COMPLET d'une version de fichier (corps compris, relu dans
    /// les postings de son segment).
    pub(crate) fn materialize_file(&self, r: &NodeRef) -> FileEntry {
        let body = self.body_of(r);
        self.materialize_file_with(r, &body)
    }

    /// Une version de fichier SANS relire son corps (laissé vide) : pour un fichier
    /// dont seul (mtime, taille) change — le delta n'en écrit que les métadonnées.
    pub fn materialize_file_meta(&self, r: &NodeRef) -> FileEntry {
        self.materialize_file_with(r, &[])
    }

    fn materialize_file_with(&self, r: &NodeRef, body: &[(&str, u32)]) -> FileEntry {
        let (mtime, size) = self.meta(r);
        let refs = r.refs();
        let symbols: Vec<Symbol> = self
            .symbols_of(r.g)
            .into_iter()
            .map(|sr| Symbol {
                name: sr.name().to_string(),
                kind: sr.kind(),
                line: sr.n.line,
                end_line: sr.n.end_line,
                signature: sr.str(sr.n.signature).to_string(),
                tokens: sr.n.tokens.iter().map(|&t| sr.str(t).to_string()).collect(),
                doc: sr.n.doc.iter().map(|&t| sr.str(t).to_string()).collect(),
                summary: sr.str(sr.n.summary).to_string(),
            })
            .collect();
        let owned = |ids: Option<&rkyv::vec::ArchivedVec<u32>>| -> Vec<String> {
            ids.map(|v| v.iter().map(|&i| r.str(i).to_string()).collect()).unwrap_or_default()
        };
        FileEntry {
            path: r.name().to_string(),
            lang: Lang::from_u8(r.n.lang),
            hash: r.str(r.n.hash).to_string(),
            size,
            mtime,
            lines: r.n.lines,
            symbols,
            refs: FileRefs {
                imports: owned(refs.map(|x| &x.imports)),
                imported_names: owned(refs.map(|x| &x.imported_names)),
                calls: refs
                    .map(|x| x.calls.iter().map(|c| CallRef { name: r.str(c.name).to_string(), line: c.line }).collect())
                    .unwrap_or_default(),
                db_refs: owned(refs.map(|x| &x.db_refs)),
            },
            header: r.n.doc.iter().map(|&t| r.str(t).to_string()).collect(),
            summary: r.str(r.n.summary).to_string(),
            body: body.iter().map(|&(t, n)| (t.to_string(), n)).collect(),
        }
    }

    /// Matérialise un `ProjectIndex` COMPLET (références brutes comprises) —
    /// sert à la compaction (reconstruction exacte), à la galaxie et aux
    /// statistiques. O(nœuds), fichiers triés par chemin, symboles par rang.
    pub fn materialize(&self) -> ProjectIndex {
        // Fichiers indépendants les uns des autres : matérialisés en parallèle,
        // puis triés par chemin (résultat indépendant du nombre de threads).
        use rayon::prelude::*;
        let bodies: Vec<Vec<Vec<(&str, u32)>>> = (0..self.nsegs()).into_par_iter().map(|s| self.bodies_of_segment(s)).collect();
        let refs: Vec<NodeRef> = self.live_files().collect();
        let mut files: Vec<FileEntry> = refs
            .par_iter()
            .map(|r| {
                let (s, local) = self.v().loc(r.g).expect("fichier vivant");
                self.materialize_file_with(r, &bodies[s][local])
            })
            .collect();
        files.par_sort_by(|a, b| a.path.cmp(&b.path));
        ProjectIndex { name: self.project.clone(), root: self.manifest.root.clone(), generated_at: self.manifest.generated_at, files }
    }
}
