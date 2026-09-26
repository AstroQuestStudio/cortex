//! I7 — cartes précompilées (§9, innovation I7).
//!
//! À l'indexation, CHAQUE symbole reçoit un texte compact déjà formaté, rangé
//! dans l'atlas (`AtlasNode.card`) : identifiant stable, genre et plage de
//! lignes, signature, rôle (première phrase du doc-comment), compteur d'appels
//! sortants et identifiants des appelés principaux. `card` le lit tel quel
//! (mmap) et n'y ajoute que la partie ENTRANTE (appelants, importeurs, tests),
//! lue dans les CSR inverses au moment de l'appel.
//!
//! Pourquoi la partie entrante n'est pas stockée : elle change quand un AUTRE
//! fichier change. La stocker obligerait chaque delta à ré-émettre la carte (et
//! donc une nouvelle version, postings compris) de tous les symboles dont un
//! appelant a bougé — un utilitaire appelé 1 000 fois serait réécrit à presque
//! chaque mise à jour —, pour un gain nul : la lecture des CSR inverses coûte
//! moins d'une milliseconde.
//!
//! Tout ce que la carte stockée contient ne dépend que du symbole, de son
//! fichier et de ses appelés résolus (nom, chemin, rang d'homonymie). Un appelé
//! d'un AUTRE fichier n'a jamais d'homonyme de même nom dans son fichier (sinon
//! l'appel serait ambigu, `graph::resolve_call`), donc son identifiant ne change
//! qu'avec son nom ou son chemin — cas où la résolution incrémentale re-résout
//! déjà l'appelant (`incremental.rs`). La même fonction sert à la construction
//! complète et aux deltas : une carte est identique à celle d'une reconstruction.

use crate::symbol::SymbolKind;

/// Nombre d'appelés listés par identifiant (les autres sont comptés).
pub const MAX_CALLEES: usize = 6;

/// Plage de lignes compacte : `L12` ou `L12-40`.
pub fn lines(line: u32, end_line: u32) -> String {
    if end_line > line {
        format!("L{}-{}", line, end_line)
    } else {
        format!("L{}", line)
    }
}

/// Rend la carte d'un symbole. `callees` : identifiants des appelés résolus,
/// dans l'ordre des appels.
pub fn render_card(
    id: &str,
    kind: SymbolKind,
    line: u32,
    end_line: u32,
    signature: &str,
    summary: &str,
    callees: &[String],
    ambiguous: usize,
) -> String {
    let mut out = format!("{} {} {}\n", id, kind.as_str(), lines(line, end_line));
    if !signature.is_empty() && kind != SymbolKind::Heading {
        out.push_str("sig: ");
        out.push_str(signature);
        out.push('\n');
    }
    if !summary.is_empty() {
        out.push_str("rôle: ");
        out.push_str(summary);
        out.push('\n');
    }
    if !callees.is_empty() || ambiguous > 0 {
        out.push_str(&format!("appelle {}", callees.len()));
        if ambiguous > 0 {
            out.push_str(&format!(" (+{} ambigu{})", ambiguous, if ambiguous > 1 { "s" } else { "" }));
        }
        if !callees.is_empty() {
            out.push_str(": ");
            out.push_str(&callees.iter().take(MAX_CALLEES).cloned().collect::<Vec<_>>().join(", "));
            if callees.len() > MAX_CALLEES {
                out.push_str(&format!(" (+{})", callees.len() - MAX_CALLEES));
            }
        }
        out.push('\n');
    }
    out
}
