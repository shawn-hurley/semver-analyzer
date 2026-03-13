//! Rename detection for the diff engine.
//!
//! Detects when a removed symbol and an added symbol are actually a rename
//! by matching on type signature fingerprints and scoring by name similarity.

use crate::types::{Symbol, SymbolKind};
use std::collections::HashMap;

/// Signature fingerprint for matching rename candidates.
///
/// Two symbols with the same fingerprint are considered potential renames.
/// The fingerprint captures: kind, type/return_type, optionality, and
/// parameter count — enough to match renames without false positives.
#[derive(PartialEq, Eq, Hash, Clone, Debug)]
struct MemberFingerprint {
    kind: SymbolKind,
    return_type: Option<String>,
    is_optional: bool,
    param_count: usize,
}

impl MemberFingerprint {
    fn from_symbol(sym: &Symbol) -> Self {
        let (return_type, is_optional, param_count) = match &sym.signature {
            Some(sig) => (
                sig.return_type.clone(),
                sig.parameters.first().map(|p| p.optional).unwrap_or(false),
                sig.parameters.len(),
            ),
            None => (None, false, 0),
        };
        Self {
            kind: sym.kind,
            return_type,
            is_optional,
            param_count,
        }
    }
}

/// A detected rename: old name → new name, with the matched symbols.
pub(super) struct RenameMatch<'a> {
    pub old: &'a Symbol,
    pub new: &'a Symbol,
    #[allow(dead_code)]
    pub similarity: f64,
}

/// Detect renames among removed and added symbol lists.
///
/// Strategy:
/// 1. Build a fingerprint for each removed and added symbol.
/// 2. Group by fingerprint to find candidate pairs.
/// 3. When exactly one removed matches one added → automatic rename.
/// 4. When multiple match, score by name similarity and greedily assign.
/// 5. Require a minimum similarity threshold to avoid false matches.
pub(super) fn detect_renames<'a>(
    removed: &[&'a Symbol],
    added: &[&'a Symbol],
) -> Vec<RenameMatch<'a>> {
    if removed.is_empty() || added.is_empty() {
        return Vec::new();
    }

    // Group added symbols by fingerprint
    let mut added_by_fp: HashMap<MemberFingerprint, Vec<(usize, &'a Symbol)>> = HashMap::new();
    for (ai, sym) in added.iter().enumerate() {
        let fp = MemberFingerprint::from_symbol(sym);
        added_by_fp.entry(fp).or_default().push((ai, sym));
    }

    // Build candidate pairs: (removed_idx, added_idx, similarity)
    let mut candidates: Vec<(usize, usize, f64)> = Vec::new();

    // Cap: skip fingerprint groups that are too large — too ambiguous
    // for meaningful rename detection and would cause O(n*m) explosion.
    const MAX_GROUP_SIZE: usize = 50;

    // Count removed per fingerprint to apply the cap on both sides
    let mut removed_by_fp: HashMap<MemberFingerprint, usize> = HashMap::new();
    for rsym in removed.iter() {
        let fp = MemberFingerprint::from_symbol(rsym);
        *removed_by_fp.entry(fp).or_default() += 1;
    }

    for (ri, rsym) in removed.iter().enumerate() {
        let fp = MemberFingerprint::from_symbol(rsym);

        // Skip if either side of this fingerprint group is too large
        let removed_count = removed_by_fp.get(&fp).copied().unwrap_or(0);
        if let Some(added_syms) = added_by_fp.get(&fp) {
            if removed_count > MAX_GROUP_SIZE || added_syms.len() > MAX_GROUP_SIZE {
                continue;
            }
            for (ai, asym) in added_syms {
                let sim = name_similarity(&rsym.name, &asym.name);
                candidates.push((ri, *ai, sim));
            }
        }
    }

    // Sort by similarity descending (best matches first)
    candidates.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

    // Greedy matching: assign best pairs, each symbol used at most once
    let mut used_removed = vec![false; removed.len()];
    let mut used_added = vec![false; added.len()];
    let mut matches = Vec::new();

    // Minimum similarity threshold: require at least some name overlap.
    // 0.15 catches cases like "isActive" → "isClicked" (share "is" prefix),
    // "chipGroupContentRef" → "labelGroupContentRef" (share "GroupContentRef").
    // Exact type match is the primary signal; name similarity is tiebreaker.
    const MIN_SIMILARITY: f64 = 0.15;

    for (ri, ai, sim) in candidates {
        if sim < MIN_SIMILARITY {
            continue;
        }
        if used_removed[ri] || used_added[ai] {
            continue;
        }
        used_removed[ri] = true;
        used_added[ai] = true;
        matches.push(RenameMatch {
            old: removed[ri],
            new: added[ai],
            similarity: sim,
        });
    }

    matches
}

/// Compute name similarity between two identifiers.
///
/// Uses longest common subsequence ratio, which handles:
/// - Prefix matches: `isActive` / `isClicked` → share "is"
/// - Suffix matches: `chipGroupContentRef` / `labelGroupContentRef` → share "GroupContentRef"
/// - Substring matches: `hasSelectableInput` / `hasClickableInput` → share "has" + "Input"
///
/// Returns a value in [0.0, 1.0] where 1.0 = identical.
pub(super) fn name_similarity(a: &str, b: &str) -> f64 {
    if a == b {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }

    let lcs_len = longest_common_subsequence_len(a, b);
    let max_len = a.len().max(b.len());
    lcs_len as f64 / max_len as f64
}

/// Length of the longest common subsequence of two strings.
pub(super) fn longest_common_subsequence_len(a: &str, b: &str) -> usize {
    let a_bytes = a.as_bytes();
    let b_bytes = b.as_bytes();
    let m = a_bytes.len();
    let n = b_bytes.len();

    // Space-optimized: only keep two rows
    let mut prev = vec![0usize; n + 1];
    let mut curr = vec![0usize; n + 1];

    for i in 1..=m {
        for j in 1..=n {
            if a_bytes[i - 1] == b_bytes[j - 1] {
                curr[j] = prev[j - 1] + 1;
            } else {
                curr[j] = prev[j].max(curr[j - 1]);
            }
        }
        std::mem::swap(&mut prev, &mut curr);
        curr.iter_mut().for_each(|v| *v = 0);
    }

    prev[n]
}
