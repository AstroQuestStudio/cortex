//! Cartes de sources (source maps v3) : lecture du JSON, décodage des VLQ,
//! recherche « position générée → position d'origine ». Sans dépendance : le
//! décodeur base64 et le décodage `%xx` sont ici. Sert à `cortex trace`.
//!
//! Positions : lignes et colonnes en base 0 dans ce module (comme le format) ; la
//! conversion depuis les piles d'erreur (base 1) se fait chez l'appelant.

use serde_json::Value;

/// Un segment décodé d'une ligne générée.
#[derive(Debug, Clone, Copy)]
struct Segment {
    col: u32,
    /// Index de la source (`u32::MAX` : segment sans origine).
    src: u32,
    lig: u32,
    c: u32,
    /// Index du nom (`u32::MAX` : aucun).
    nom: u32,
}

/// Position d'origine d'une position générée.
#[derive(Debug, Clone)]
pub struct Origine {
    pub source: String,
    pub ligne: u32,
    pub col: u32,
    pub nom: Option<String>,
    pub contenu: Option<String>,
}

enum Corps {
    Simple {
        lignes: Vec<Vec<Segment>>,
    },
    /// Carte indexée : sections triées par décalage (ligne, colonne).
    Sections(Vec<(u32, u32, SourceMap)>),
}

pub struct SourceMap {
    sources: Vec<String>,
    noms: Vec<String>,
    contenus: Vec<Option<String>>,
    racine: String,
    corps: Corps,
}

impl SourceMap {
    /// Analyse le JSON d'une carte (v3, simple ou indexée).
    pub fn analyser(json: &str) -> Result<SourceMap, String> {
        let v: Value = serde_json::from_str(json.trim_start_matches('\u{feff}')).map_err(|e| format!("carte illisible : {e}"))?;
        Self::depuis_valeur(&v, 0)
    }

    fn depuis_valeur(v: &Value, profondeur: usize) -> Result<SourceMap, String> {
        if let Some(sections) = v.get("sections").and_then(|s| s.as_array()) {
            if profondeur > 2 {
                return Err("sections imbriquées trop profondes".into());
            }
            let mut out = Vec::new();
            for s in sections {
                let off = s.get("offset");
                let l = off.and_then(|o| o.get("line")).and_then(|x| x.as_u64()).unwrap_or(0) as u32;
                let c = off.and_then(|o| o.get("column")).and_then(|x| x.as_u64()).unwrap_or(0) as u32;
                let m = s.get("map").ok_or("section sans « map »")?;
                out.push((l, c, Self::depuis_valeur(m, profondeur + 1)?));
            }
            out.sort_by_key(|(l, c, _)| (*l, *c));
            return Ok(SourceMap { sources: vec![], noms: vec![], contenus: vec![], racine: String::new(), corps: Corps::Sections(out) });
        }
        let texte = |k: &str| -> Vec<String> {
            v.get(k)
                .and_then(|x| x.as_array())
                .map(|a| a.iter().map(|s| s.as_str().unwrap_or("").to_string()).collect())
                .unwrap_or_default()
        };
        let sources = texte("sources");
        let noms = texte("names");
        let contenus: Vec<Option<String>> = v
            .get("sourcesContent")
            .and_then(|x| x.as_array())
            .map(|a| a.iter().map(|s| s.as_str().map(|t| t.to_string())).collect())
            .unwrap_or_default();
        let racine = v.get("sourceRoot").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let mappings = v.get("mappings").and_then(|x| x.as_str()).ok_or("carte sans « mappings »")?;
        Ok(SourceMap { sources, noms, contenus, racine, corps: Corps::Simple { lignes: decoder(mappings)? } })
    }

    /// Position d'origine de la position générée `(ligne, col)` (base 0) : le
    /// dernier segment de la ligne dont la colonne ne dépasse pas `col`.
    pub fn origine(&self, ligne: u32, col: u32) -> Option<Origine> {
        match &self.corps {
            Corps::Sections(s) => {
                let i = s.partition_point(|(l, c, _)| (*l, *c) <= (ligne, col));
                let (l0, c0, m) = s.get(i.checked_sub(1)?)?;
                let (l, c) = if ligne == *l0 { (0, col.saturating_sub(*c0)) } else { (ligne - l0, col) };
                m.origine(l, c)
            }
            Corps::Simple { lignes } => {
                let segs = lignes.get(ligne as usize)?;
                let i = segs.partition_point(|s| s.col <= col);
                let s = segs.get(i.checked_sub(1)?)?;
                if s.src == u32::MAX {
                    return None;
                }
                let brut = self.sources.get(s.src as usize)?;
                let source = if self.racine.is_empty() || brut.contains("://") || brut.starts_with('/') {
                    brut.clone()
                } else {
                    format!("{}/{}", self.racine.trim_end_matches('/'), brut)
                };
                Some(Origine {
                    source,
                    ligne: s.lig,
                    col: s.c,
                    nom: (s.nom != u32::MAX).then(|| self.noms.get(s.nom as usize).cloned()).flatten(),
                    contenu: self.contenus.get(s.src as usize).cloned().flatten(),
                })
            }
        }
    }
}

fn valeur_b64(c: u8) -> Option<u32> {
    match c {
        b'A'..=b'Z' => Some((c - b'A') as u32),
        b'a'..=b'z' => Some((c - b'a') as u32 + 26),
        b'0'..=b'9' => Some((c - b'0') as u32 + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    }
}

/// Décode les VLQ base64 du champ `mappings`.
fn decoder(m: &str) -> Result<Vec<Vec<Segment>>, String> {
    let (mut src, mut lig, mut c, mut nom) = (0i64, 0i64, 0i64, 0i64);
    let mut lignes: Vec<Vec<Segment>> = Vec::new();
    for ligne in m.split(';') {
        let mut col = 0i64;
        let mut segs = Vec::new();
        for seg in ligne.split(',') {
            if seg.is_empty() {
                continue;
            }
            let mut champs = [0i64; 5];
            let mut n = 0usize;
            let (mut val, mut decal) = (0i64, 0u32);
            for b in seg.bytes() {
                let d = valeur_b64(b).ok_or_else(|| format!("caractère invalide « {} » dans mappings", b as char))? as i64;
                val += (d & 31) << decal;
                if d & 32 != 0 {
                    decal += 5;
                    if decal > 60 {
                        return Err("VLQ trop long".into());
                    }
                } else {
                    let neg = val & 1 == 1;
                    let x = val >> 1;
                    if n < 5 {
                        champs[n] = if neg { -x } else { x };
                    }
                    n += 1;
                    val = 0;
                    decal = 0;
                }
            }
            col += champs[0];
            if n == 1 || n == 2 || n == 3 {
                // 1 champ : sans origine ; 2 ou 3 : malformé, traité comme sans origine.
                segs.push(Segment { col: col.max(0) as u32, src: u32::MAX, lig: 0, c: 0, nom: u32::MAX });
                continue;
            }
            src += champs[1];
            lig += champs[2];
            c += champs[3];
            let nm = if n >= 5 {
                nom += champs[4];
                nom.max(0) as u32
            } else {
                u32::MAX
            };
            segs.push(Segment { col: col.max(0) as u32, src: src.max(0) as u32, lig: lig.max(0) as u32, c: c.max(0) as u32, nom: nm });
        }
        lignes.push(segs);
    }
    Ok(lignes)
}

/// Décode du base64 (standard ou URL-safe, avec ou sans `=`).
pub fn base64_decoder(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for b in s.bytes() {
        if b == b'=' || b == b'\n' || b == b'\r' || b == b' ' {
            continue;
        }
        acc = (acc << 6) | valeur_b64(b)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Décode `%xx` (URL de données non base64).
pub fn pourcent_decoder(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let h = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|x| u8::from_str_radix(x, 16).ok());
            if let Some(v) = h {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

/// Cherche `//# sourceMappingURL=…` (le dernier) dans un script ; rend la valeur.
pub fn url_de_carte(script: &str) -> Option<&str> {
    let i = script.rfind("sourceMappingURL=")?;
    // Doit suivre `//#` ou `//@` (ou `/*#`) : évite une simple chaîne dans le code.
    let avant = script[..i].trim_end_matches([' ', '\t']);
    if !(avant.ends_with("//#") || avant.ends_with("//@") || avant.ends_with("/*#")) {
        return None;
    }
    let v = script[i + "sourceMappingURL=".len()..].lines().next()?.trim().trim_end_matches("*/").trim();
    (!v.is_empty()).then_some(v)
}

/// Contenu JSON d'une URL `data:application/json[;charset=…][;base64],…`.
pub fn carte_en_ligne(url: &str) -> Option<String> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let octets = if meta.contains(";base64") { base64_decoder(data)? } else { pourcent_decoder(data) };
    String::from_utf8(octets).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vlq_et_recherche() {
        // « AAAA;AACA,EAAE » : ligne 0 col 0 -> (0,0) ; ligne 1 col 0 -> (1,0), col 2 -> (1,2).
        let m = SourceMap::analyser(r#"{"version":3,"sources":["a.ts"],"names":[],"mappings":"AAAA;AACA,EAAE"}"#).unwrap();
        let o = m.origine(1, 5).unwrap();
        assert_eq!((o.source.as_str(), o.ligne, o.col), ("a.ts", 1, 2));
        let o = m.origine(1, 0).unwrap();
        assert_eq!((o.ligne, o.col), (1, 0));
        assert!(m.origine(7, 0).is_none());
    }

    #[test]
    fn noms_et_racine() {
        // « AAAAA » : 5 champs, nom 0.
        let m = SourceMap::analyser(r#"{"version":3,"sourceRoot":"src","sources":["a.ts"],"names":["foo"],"mappings":"AAAAA"}"#).unwrap();
        let o = m.origine(0, 3).unwrap();
        assert_eq!(o.source, "src/a.ts");
        assert_eq!(o.nom.as_deref(), Some("foo"));
    }

    #[test]
    fn vlq_negatif_et_multi_chiffres() {
        // 16 -> « gB » ; -16 -> « hB ».
        let m = SourceMap::analyser(r#"{"version":3,"sources":["a.ts"],"names":[],"mappings":"AAgBA;ADAhB"}"#).unwrap();
        assert_eq!(m.origine(0, 0).unwrap().ligne, 16);
        assert_eq!(m.origine(1, 0).unwrap().col, 0);
    }

    #[test]
    fn carte_indexee() {
        let j = r#"{"version":3,"sections":[{"offset":{"line":0,"column":0},"map":{"version":3,"sources":["x.js"],"names":[],"mappings":"AAAA"}},{"offset":{"line":2,"column":0},"map":{"version":3,"sources":["y.js"],"names":[],"mappings":"AAAA"}}]}"#;
        let m = SourceMap::analyser(j).unwrap();
        assert_eq!(m.origine(0, 0).unwrap().source, "x.js");
        assert_eq!(m.origine(2, 0).unwrap().source, "y.js");
    }

    #[test]
    fn base64_et_url_de_carte() {
        assert_eq!(base64_decoder("SGVsbG8=").unwrap(), b"Hello");
        let js = "code();\n//# sourceMappingURL=data:application/json;base64,eyJhIjoxfQ==\n";
        let u = url_de_carte(js).unwrap();
        assert_eq!(carte_en_ligne(u).unwrap(), "{\"a\":1}");
        assert!(url_de_carte("var s = 'sourceMappingURL=x';").is_none());
        assert_eq!(url_de_carte("x\n//# sourceMappingURL=app.js.map").unwrap(), "app.js.map");
    }
}
