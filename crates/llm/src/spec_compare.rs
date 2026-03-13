//! Tier 1: Structural comparison of `FunctionSpec` objects.
//!
//! Compares two specs mechanically without LLM. This catches clear-cut
//! breaking changes where:
//! - Preconditions were tightened (new precondition added, existing one changed)
//! - Postconditions were weakened (postcondition removed, return value changed)
//! - Error behavior changed (type changed, new error added for existing input)
//! - Side effects removed or changed
//!
//! ## Matching Strategy
//!
//! Entries are matched by their "identity" field:
//! - `Precondition` entries match on `parameter`
//! - `Postcondition` entries match on `condition`
//! - `ErrorBehavior` entries match on `trigger`
//! - `SideEffect` entries match on `target` + `action` pair
//!
//! String comparisons use normalized lowercase trimmed form.

use semver_analyzer_core::{BreakingVerdict, FunctionSpec};

/// Perform a structural (no-LLM) comparison of two function specs.
///
/// Returns a `BreakingVerdict` with confidence 0.80 for clear structural
/// differences, or 0.0/not-breaking if the specs are structurally equivalent.
pub fn structural_compare(old: &FunctionSpec, new: &FunctionSpec) -> BreakingVerdict {
    let mut reasons = Vec::new();

    // ── Preconditions ───────────────────────────────────────────────
    // Breaking if: new precondition added, or existing one tightened
    // Not breaking if: precondition removed (more permissive)
    compare_preconditions(old, new, &mut reasons);

    // ── Postconditions ──────────────────────────────────────────────
    // Breaking if: postcondition removed, or return value changed
    // Not breaking if: new postcondition added (more guarantees)
    compare_postconditions(old, new, &mut reasons);

    // ── Error behavior ──────────────────────────────────────────────
    // Breaking if: error type changed, or new error for existing input
    // Not breaking if: error removed (more permissive)
    compare_error_behavior(old, new, &mut reasons);

    // ── Side effects ────────────────────────────────────────────────
    // Breaking if: side effect removed or changed
    // Potentially breaking if: new side effect added
    compare_side_effects(old, new, &mut reasons);

    let is_breaking = !reasons.is_empty();
    let confidence = if is_breaking { 0.80 } else { 0.0 };

    BreakingVerdict {
        is_breaking,
        reasons,
        confidence,
    }
}

// ── Precondition Comparison ─────────────────────────────────────────────

fn compare_preconditions(old: &FunctionSpec, new: &FunctionSpec, reasons: &mut Vec<String>) {
    // Check for new preconditions (tightened)
    for new_pre in &new.preconditions {
        let matched = old
            .preconditions
            .iter()
            .find(|old_pre| normalize(&old_pre.parameter) == normalize(&new_pre.parameter));

        match matched {
            None => {
                // New precondition added — function accepts less input
                reasons.push(format!(
                    "New precondition added: parameter '{}' now requires '{}'",
                    new_pre.parameter, new_pre.condition
                ));
            }
            Some(old_pre) => {
                // Existing precondition — check if tightened
                if normalize(&old_pre.condition) != normalize(&new_pre.condition) {
                    reasons.push(format!(
                        "Precondition changed for '{}': '{}' → '{}'",
                        new_pre.parameter, old_pre.condition, new_pre.condition
                    ));
                }
                if normalize(&old_pre.on_violation) != normalize(&new_pre.on_violation) {
                    reasons.push(format!(
                        "Violation behavior changed for '{}': '{}' → '{}'",
                        new_pre.parameter, old_pre.on_violation, new_pre.on_violation
                    ));
                }
            }
        }
    }
}

// ── Postcondition Comparison ────────────────────────────────────────────

fn compare_postconditions(old: &FunctionSpec, new: &FunctionSpec, reasons: &mut Vec<String>) {
    // Check for removed postconditions (weakened)
    for old_post in &old.postconditions {
        let matched = new
            .postconditions
            .iter()
            .find(|new_post| normalize(&new_post.condition) == normalize(&old_post.condition));

        match matched {
            None => {
                // Postcondition removed — function guarantees less
                reasons.push(format!(
                    "Postcondition removed: '{}' no longer guarantees '{}'",
                    old_post.condition, old_post.returns
                ));
            }
            Some(new_post) => {
                // Check if return value changed
                if normalize(&old_post.returns) != normalize(&new_post.returns) {
                    reasons.push(format!(
                        "Postcondition return changed for '{}': '{}' → '{}'",
                        old_post.condition, old_post.returns, new_post.returns
                    ));
                }
            }
        }
    }
}

// ── Error Behavior Comparison ───────────────────────────────────────────

fn compare_error_behavior(old: &FunctionSpec, new: &FunctionSpec, reasons: &mut Vec<String>) {
    // Check for changed error types (breaking)
    for old_err in &old.error_behavior {
        let matched = new
            .error_behavior
            .iter()
            .find(|new_err| normalize(&new_err.trigger) == normalize(&old_err.trigger));

        match matched {
            Some(new_err) => {
                if normalize(&old_err.error_type) != normalize(&new_err.error_type) {
                    reasons.push(format!(
                        "Error type changed for trigger '{}': {} → {}",
                        old_err.trigger, old_err.error_type, new_err.error_type
                    ));
                }
            }
            None => {
                // Error case removed — not breaking (more permissive)
                // Don't add to reasons
            }
        }
    }

    // Check for new error cases (breaking: function throws where it didn't)
    for new_err in &new.error_behavior {
        let existed = old
            .error_behavior
            .iter()
            .any(|old_err| normalize(&old_err.trigger) == normalize(&new_err.trigger));

        if !existed {
            reasons.push(format!(
                "New error case: '{}' now throws {} (was not an error before)",
                new_err.trigger, new_err.error_type
            ));
        }
    }
}

// ── Side Effect Comparison ──────────────────────────────────────────────

fn compare_side_effects(old: &FunctionSpec, new: &FunctionSpec, reasons: &mut Vec<String>) {
    // Check for removed side effects (breaking)
    for old_se in &old.side_effects {
        let matched = new.side_effects.iter().find(|new_se| {
            normalize(&new_se.target) == normalize(&old_se.target)
                && normalize(&new_se.action) == normalize(&old_se.action)
        });

        if matched.is_none() {
            reasons.push(format!(
                "Side effect removed: '{}' no longer '{}'",
                old_se.target, old_se.action
            ));
        }
    }

    // Check for changed side effects (same target, different action)
    for old_se in &old.side_effects {
        for new_se in &new.side_effects {
            if normalize(&old_se.target) == normalize(&new_se.target)
                && normalize(&old_se.action) != normalize(&new_se.action)
            {
                reasons.push(format!(
                    "Side effect changed for '{}': '{}' → '{}'",
                    old_se.target, old_se.action, new_se.action
                ));
            }
        }
    }
}

// ── Normalization ───────────────────────────────────────────────────────

/// Normalize a string for comparison: lowercase, trimmed, collapsed whitespace.
fn normalize(s: &str) -> String {
    s.trim()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use semver_analyzer_core::{ErrorBehavior, Postcondition, Precondition, SideEffect};

    fn empty_spec() -> FunctionSpec {
        FunctionSpec {
            preconditions: vec![],
            postconditions: vec![],
            error_behavior: vec![],
            side_effects: vec![],
            notes: vec![],
        }
    }

    // ── No changes ──────────────────────────────────────────────────

    #[test]
    fn identical_specs_not_breaking() {
        let spec = empty_spec();
        let result = structural_compare(&spec, &spec);
        assert!(!result.is_breaking);
        assert!(result.reasons.is_empty());
    }

    #[test]
    fn both_empty_not_breaking() {
        let result = structural_compare(&empty_spec(), &empty_spec());
        assert!(!result.is_breaking);
    }

    // ── Precondition changes ────────────────────────────────────────

    #[test]
    fn new_precondition_is_breaking() {
        let old = empty_spec();
        let mut new = empty_spec();
        new.preconditions.push(Precondition {
            parameter: "email".into(),
            condition: "must be non-empty".into(),
            on_violation: "throws TypeError".into(),
        });

        let result = structural_compare(&old, &new);
        assert!(result.is_breaking);
        assert!(result.reasons[0].contains("New precondition added"));
    }

    #[test]
    fn tightened_precondition_is_breaking() {
        let mut old = empty_spec();
        old.preconditions.push(Precondition {
            parameter: "email".into(),
            condition: "must be a string".into(),
            on_violation: "throws TypeError".into(),
        });

        let mut new = empty_spec();
        new.preconditions.push(Precondition {
            parameter: "email".into(),
            condition: "must be a non-empty string with @ symbol".into(),
            on_violation: "throws TypeError".into(),
        });

        let result = structural_compare(&old, &new);
        assert!(result.is_breaking);
        assert!(result.reasons[0].contains("Precondition changed"));
    }

    #[test]
    fn removed_precondition_not_breaking() {
        let mut old = empty_spec();
        old.preconditions.push(Precondition {
            parameter: "email".into(),
            condition: "must be non-empty".into(),
            on_violation: "throws TypeError".into(),
        });

        let new = empty_spec();

        let result = structural_compare(&old, &new);
        assert!(!result.is_breaking); // More permissive = not breaking
    }

    // ── Postcondition changes ───────────────────────────────────────

    #[test]
    fn removed_postcondition_is_breaking() {
        let mut old = empty_spec();
        old.postconditions.push(Postcondition {
            condition: "valid input".into(),
            returns: "user object".into(),
        });

        let new = empty_spec();

        let result = structural_compare(&old, &new);
        assert!(result.is_breaking);
        assert!(result.reasons[0].contains("Postcondition removed"));
    }

    #[test]
    fn changed_postcondition_return_is_breaking() {
        let mut old = empty_spec();
        old.postconditions.push(Postcondition {
            condition: "always".into(),
            returns: "lowercased email".into(),
        });

        let mut new = empty_spec();
        new.postconditions.push(Postcondition {
            condition: "always".into(),
            returns: "lowercased email with + alias stripped".into(),
        });

        let result = structural_compare(&old, &new);
        assert!(result.is_breaking);
        assert!(result.reasons[0].contains("Postcondition return changed"));
    }

    #[test]
    fn added_postcondition_not_breaking() {
        let old = empty_spec();

        let mut new = empty_spec();
        new.postconditions.push(Postcondition {
            condition: "valid input".into(),
            returns: "user object".into(),
        });

        let result = structural_compare(&old, &new);
        assert!(!result.is_breaking); // More guarantees = not breaking
    }

    // ── Error behavior changes ──────────────────────────────────────

    #[test]
    fn changed_error_type_is_breaking() {
        let mut old = empty_spec();
        old.error_behavior.push(ErrorBehavior {
            trigger: "invalid email".into(),
            error_type: "TypeError".into(),
            message_pattern: None,
        });

        let mut new = empty_spec();
        new.error_behavior.push(ErrorBehavior {
            trigger: "invalid email".into(),
            error_type: "ValidationError".into(),
            message_pattern: None,
        });

        let result = structural_compare(&old, &new);
        assert!(result.is_breaking);
        assert!(result.reasons[0].contains("Error type changed"));
    }

    #[test]
    fn new_error_case_is_breaking() {
        let old = empty_spec();

        let mut new = empty_spec();
        new.error_behavior.push(ErrorBehavior {
            trigger: "empty input".into(),
            error_type: "Error".into(),
            message_pattern: None,
        });

        let result = structural_compare(&old, &new);
        assert!(result.is_breaking);
        assert!(result.reasons[0].contains("New error case"));
    }

    #[test]
    fn removed_error_case_not_breaking() {
        let mut old = empty_spec();
        old.error_behavior.push(ErrorBehavior {
            trigger: "invalid input".into(),
            error_type: "Error".into(),
            message_pattern: None,
        });

        let new = empty_spec();

        let result = structural_compare(&old, &new);
        assert!(!result.is_breaking); // More permissive = not breaking
    }

    // ── Side effect changes ─────────────────────────────────────────

    #[test]
    fn removed_side_effect_is_breaking() {
        let mut old = empty_spec();
        old.side_effects.push(SideEffect {
            target: "database".into(),
            action: "inserts user row".into(),
            condition: None,
        });

        let new = empty_spec();

        let result = structural_compare(&old, &new);
        assert!(result.is_breaking);
        assert!(result.reasons[0].contains("Side effect removed"));
    }

    #[test]
    fn changed_side_effect_is_breaking() {
        let mut old = empty_spec();
        old.side_effects.push(SideEffect {
            target: "database".into(),
            action: "inserts user row".into(),
            condition: None,
        });

        let mut new = empty_spec();
        new.side_effects.push(SideEffect {
            target: "database".into(),
            action: "upserts user row".into(),
            condition: None,
        });

        let result = structural_compare(&old, &new);
        assert!(result.is_breaking);
        assert!(result
            .reasons
            .iter()
            .any(|r| r.contains("Side effect changed")));
    }

    // ── Normalization ───────────────────────────────────────────────

    #[test]
    fn normalization_handles_whitespace() {
        let mut old = empty_spec();
        old.postconditions.push(Postcondition {
            condition: "  Always  ".into(),
            returns: "  user   object  ".into(),
        });

        let mut new = empty_spec();
        new.postconditions.push(Postcondition {
            condition: "always".into(),
            returns: "user object".into(),
        });

        let result = structural_compare(&old, &new);
        assert!(!result.is_breaking); // Should match after normalization
    }

    #[test]
    fn normalization_handles_case() {
        let mut old = empty_spec();
        old.postconditions.push(Postcondition {
            condition: "ALWAYS".into(),
            returns: "User Object".into(),
        });

        let mut new = empty_spec();
        new.postconditions.push(Postcondition {
            condition: "always".into(),
            returns: "user object".into(),
        });

        let result = structural_compare(&old, &new);
        assert!(!result.is_breaking);
    }

    // ── Multiple changes ────────────────────────────────────────────

    #[test]
    fn multiple_breaking_changes() {
        let mut old = empty_spec();
        old.postconditions.push(Postcondition {
            condition: "valid".into(),
            returns: "true".into(),
        });

        let mut new = empty_spec();
        new.preconditions.push(Precondition {
            parameter: "input".into(),
            condition: "must be non-null".into(),
            on_violation: "throws".into(),
        });
        new.postconditions.push(Postcondition {
            condition: "valid".into(),
            returns: "object".into(),
        });
        new.error_behavior.push(ErrorBehavior {
            trigger: "null input".into(),
            error_type: "TypeError".into(),
            message_pattern: None,
        });

        let result = structural_compare(&old, &new);
        assert!(result.is_breaking);
        assert!(result.reasons.len() >= 3);
    }

    // ── Confidence scoring ──────────────────────────────────────────

    #[test]
    fn breaking_has_high_confidence() {
        let old = empty_spec();
        let mut new = empty_spec();
        new.preconditions.push(Precondition {
            parameter: "x".into(),
            condition: "must be positive".into(),
            on_violation: "throws".into(),
        });

        let result = structural_compare(&old, &new);
        assert!(result.is_breaking);
        assert!((result.confidence - 0.80).abs() < f64::EPSILON);
    }

    #[test]
    fn not_breaking_has_zero_confidence() {
        let result = structural_compare(&empty_spec(), &empty_spec());
        assert!(!result.is_breaking);
        assert!((result.confidence - 0.0).abs() < f64::EPSILON);
    }
}
