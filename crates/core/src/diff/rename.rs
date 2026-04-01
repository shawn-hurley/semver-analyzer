//! Rename detection for the diff engine.
//!
//! Detects when a removed symbol and an added symbol are actually a rename
//! by matching on type signature fingerprints and scoring by name similarity.

use crate::types::{Symbol, SymbolKind};
use std::collections::{BTreeSet, HashMap};

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
        });
    }

    matches
}

/// Detect renames among constant/variable symbols using segment-based fuzzy matching.
///
/// Design tokens (e.g., `global_Color_dark_100` → `t_color_dark_100`) can't be
/// matched by type fingerprinting because all tokens share the same shape.
/// Instead, split names on `_`, lowercase, and match by segment set overlap
/// using Jaccard similarity.
///
/// Uses an inverted index for efficiency: each segment maps to the added tokens
/// that contain it, so we only compute Jaccard for candidates sharing segments.
pub(super) fn detect_token_renames<'a>(
    removed: &[&'a Symbol],
    added: &[&'a Symbol],
) -> Vec<RenameMatch<'a>> {
    use std::collections::{BTreeSet, HashSet};

    // Filter to constant/variable symbols only
    let removed_tokens: Vec<(usize, &Symbol, BTreeSet<String>)> = removed
        .iter()
        .enumerate()
        .filter(|(_, s)| matches!(s.kind, SymbolKind::Constant | SymbolKind::Variable))
        .map(|(i, s)| {
            let segments = tokenize_name(&s.name);
            (i, *s, segments)
        })
        .collect();

    let added_tokens: Vec<(usize, &Symbol, BTreeSet<String>)> = added
        .iter()
        .enumerate()
        .filter(|(_, s)| matches!(s.kind, SymbolKind::Constant | SymbolKind::Variable))
        .map(|(i, s)| {
            let segments = tokenize_name(&s.name);
            (i, *s, segments)
        })
        .collect();

    if removed_tokens.is_empty() || added_tokens.is_empty() {
        return Vec::new();
    }

    tracing::debug!(
        removed = removed_tokens.len(),
        added = added_tokens.len(),
        "Starting token rename detection"
    );

    // Build inverted index: segment → list of added token indices
    let mut segment_index: HashMap<String, Vec<usize>> = HashMap::new();
    for (idx, (_, _, segments)) in added_tokens.iter().enumerate() {
        for seg in segments {
            segment_index.entry(seg.clone()).or_default().push(idx);
        }
    }

    // Minimum segment overlap ratio (60% of the smaller set)
    const MIN_JACCARD: f64 = 0.6;

    // For each removed token, find candidates via inverted index
    let mut candidates: Vec<(usize, usize, f64)> = Vec::new(); // (removed_idx, added_idx, jaccard)

    for (ri_local, (_, _, r_segments)) in removed_tokens.iter().enumerate() {
        if r_segments.is_empty() {
            continue;
        }

        // Count hits per added token via inverted index
        let mut hit_counts: HashMap<usize, usize> = HashMap::new();
        for seg in r_segments {
            if let Some(added_indices) = segment_index.get(seg) {
                for &ai in added_indices {
                    *hit_counts.entry(ai).or_default() += 1;
                }
            }
        }

        // Minimum shared segments: 60% of the removed token's segment count,
        // but at least 2 to avoid matching on single common segments like "100"
        let min_shared = (r_segments.len() as f64 * 0.6).ceil() as usize;
        let min_shared = min_shared.max(2);

        for (ai_local, hits) in hit_counts {
            if hits < min_shared {
                continue;
            }

            let a_segments = &added_tokens[ai_local].2;
            let intersection = r_segments.intersection(a_segments).count();
            let union = r_segments.union(a_segments).count();
            let jaccard = if union > 0 {
                intersection as f64 / union as f64
            } else {
                0.0
            };

            if jaccard >= MIN_JACCARD {
                candidates.push((ri_local, ai_local, jaccard));
            }
        }
    }

    // Sort by Jaccard descending (best matches first)
    candidates.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

    // Greedy assignment: each symbol used at most once
    let mut used_removed = HashSet::new();
    let mut used_added = HashSet::new();
    let mut matches = Vec::new();

    for (ri_local, ai_local, jaccard) in &candidates {
        if used_removed.contains(ri_local) || used_added.contains(ai_local) {
            continue;
        }
        used_removed.insert(*ri_local);
        used_added.insert(*ai_local);

        let old_sym = removed_tokens[*ri_local].1;
        let new_sym = added_tokens[*ai_local].1;

        tracing::debug!(
            old = %old_sym.name,
            new = %new_sym.name,
            jaccard = %jaccard,
            "Token rename matched"
        );

        matches.push(RenameMatch {
            old: old_sym,
            new: new_sym,
        });
    }

    tracing::info!(
        matched = matches.len(),
        removed = removed_tokens.len(),
        added = added_tokens.len(),
        "Token rename detection complete"
    );

    matches
}

/// Split a token name into lowercase segments for fuzzy matching.
///
/// `global_Color_dark_100` → `{"global", "color", "dark", "100"}`
fn tokenize_name(name: &str) -> BTreeSet<String> {
    name.split('_')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .collect()
}

#[cfg(test)]
mod token_tests {
    use super::*;
    use crate::types::{Symbol, SymbolKind, Visibility};
    use std::path::PathBuf;

    fn make_token(name: &str, package: &str) -> Symbol {
        Symbol {
            name: name.to_string(),
            qualified_name: format!("{}/{}.{}", package, name, name),
            kind: SymbolKind::Constant,
            visibility: Visibility::Public,
            file: PathBuf::from(format!("{}/{}.d.ts", package, name)),
            package: Some(package.to_string()),
            import_path: None,
            line: 1,
            signature: None,
            extends: None,
            implements: vec![],
            is_abstract: false,
            type_dependencies: vec![],
            is_readonly: false,
            is_static: false,
            accessor_kind: None,
            members: vec![],
            rendered_components: vec![],
        }
    }

    #[test]
    fn test_tokenize_name() {
        let segs = tokenize_name("global_Color_dark_100");
        assert!(segs.contains("global"));
        assert!(segs.contains("color")); // lowercased
        assert!(segs.contains("dark"));
        assert!(segs.contains("100"));
        assert_eq!(segs.len(), 4);
    }

    #[test]
    fn test_token_rename_basic() {
        let old = make_token("global_Color_dark_100", "@patternfly/react-tokens");
        let new = make_token("t_color_dark_100", "@patternfly/react-tokens");

        let removed = vec![&old];
        let added = vec![&new];

        let matches = detect_token_renames(&removed, &added);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].old.name, "global_Color_dark_100");
        assert_eq!(matches[0].new.name, "t_color_dark_100");
    }

    #[test]
    fn test_token_rename_chart_prefix() {
        let old = make_token("global_success_color_100", "@patternfly/react-tokens");
        let new = make_token(
            "t_chart_global_success_color_100",
            "@patternfly/react-tokens",
        );

        let removed = vec![&old];
        let added = vec![&new];

        let matches = detect_token_renames(&removed, &added);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].new.name, "t_chart_global_success_color_100");
    }

    #[test]
    fn test_token_rename_no_false_positive() {
        // Two tokens that share only 1 segment should NOT match
        let old = make_token("global_Color_dark_100", "@patternfly/react-tokens");
        let new = make_token("c_button_FontSize_100", "@patternfly/react-tokens");

        let removed = vec![&old];
        let added = vec![&new];

        let matches = detect_token_renames(&removed, &added);
        assert!(matches.is_empty(), "Should not match unrelated tokens");
    }

    #[test]
    fn test_token_rename_greedy_best_match() {
        // Two removed tokens competing for the same added token
        let old1 = make_token("global_Color_dark_100", "@patternfly/react-tokens");
        let old2 = make_token("global_Color_dark_200", "@patternfly/react-tokens");
        let new1 = make_token("t_color_dark_100", "@patternfly/react-tokens");
        let new2 = make_token("t_color_dark_200", "@patternfly/react-tokens");

        let removed = vec![&old1, &old2];
        let added = vec![&new1, &new2];

        let matches = detect_token_renames(&removed, &added);
        assert_eq!(matches.len(), 2);

        // Each old should match its corresponding new (100→100, 200→200)
        let match_map: HashMap<&str, &str> = matches
            .iter()
            .map(|m| (m.old.name.as_str(), m.new.name.as_str()))
            .collect();
        assert_eq!(
            match_map.get("global_Color_dark_100"),
            Some(&"t_color_dark_100")
        );
        assert_eq!(
            match_map.get("global_Color_dark_200"),
            Some(&"t_color_dark_200")
        );
    }

    #[test]
    fn test_token_rename_many_to_one_resolved() {
        // Multiple removed tokens could match the same added token,
        // but greedy assignment picks the best one
        let old1 = make_token("global_Color_dark_100", "@patternfly/react-tokens");
        let old2 = make_token(
            "global_BackgroundColor_dark_100",
            "@patternfly/react-tokens",
        );
        let new = make_token("t_color_dark_100", "@patternfly/react-tokens");

        let removed = vec![&old1, &old2];
        let added = vec![&new];

        let matches = detect_token_renames(&removed, &added);
        // Only one can match — the one with higher Jaccard wins
        assert_eq!(matches.len(), 1);
        // global_Color_dark_100 has Jaccard 3/5=0.6, global_BackgroundColor_dark_100 has 3/6=0.5
        assert_eq!(matches[0].old.name, "global_Color_dark_100");
    }

    #[test]
    fn test_token_rename_skips_non_constants() {
        // Interface symbols should be skipped (handled by detect_renames)
        let old = Symbol {
            kind: SymbolKind::Interface,
            ..make_token("ModalProps", "@patternfly/react-core")
        };
        let new = Symbol {
            kind: SymbolKind::Interface,
            ..make_token("ContentProps", "@patternfly/react-core")
        };

        let removed = vec![&old];
        let added = vec![&new];

        let matches = detect_token_renames(&removed, &added);
        assert!(matches.is_empty());
    }

    #[test]
    fn test_token_rename_case_insensitive() {
        // BackgroundColor vs backgroundcolor should match (lowercased)
        let old = make_token(
            "global_BackgroundColor_dark_100",
            "@patternfly/react-tokens",
        );
        let new = make_token("t_backgroundcolor_dark_100", "@patternfly/react-tokens");

        let removed = vec![&old];
        let added = vec![&new];

        let matches = detect_token_renames(&removed, &added);
        // Segments: {global, backgroundcolor, dark, 100} vs {t, backgroundcolor, dark, 100}
        // Intersection: {backgroundcolor, dark, 100} = 3, Union = 5, Jaccard = 0.6
        assert_eq!(matches.len(), 1);
    }
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
