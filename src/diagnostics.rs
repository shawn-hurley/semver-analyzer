//! CLI error rendering and degradation summary display.
//!
//! This module provides the user-facing error presentation layer:
//!
//! - [`render_error`] — Renders fatal errors with colors, causal chain, and tips
//! - [`print_degradation_summary`] — Shows end-of-run warnings for non-fatal issues
//! - [`print_transitive_bump_summary`] — Shows peer dependency narrowing that requires
//!   transitive dependency bumps in consumers
//!
//! All error formatting goes through this module. Never use `eprintln!`
//! directly for error output in production code.

use std::collections::BTreeSet;

use owo_colors::OwoColorize;
use semver_analyzer_core::diagnostics::DegradationTracker;
use semver_analyzer_core::error::DiagnosedError;
use semver_analyzer_core::types::ManifestChange;
use semver_analyzer_core::Language;

use crate::progress::ProgressReporter;

/// Render a fatal error with colors, causal chain, and remediation tips.
///
/// This is the primary error display function. It:
/// 1. Walks the `anyhow` chain for `Diagnosed` markers (single downcast)
/// 2. Renders colored output: red `error:`, dimmed `caused by:`, cyan `tip:`
/// 3. Falls back to pattern-matching on error text for undiagnosed errors
pub fn render_error(err: &anyhow::Error) {
    let tip = extract_tip(err);

    // Primary error (red, bold)
    eprintln!("\n{} {}", "error:".red().bold(), err);

    // Causal chain (dimmed, indented) — skip the first (already printed)
    // and skip empty Display strings (Diagnosed markers)
    for cause in err.chain().skip(1) {
        let msg = cause.to_string();
        if msg.is_empty() {
            continue; // Skip Diagnosed markers
        }
        eprintln!("  {} {}", "caused by:".dimmed(), msg);
    }

    // Remediation tip (cyan)
    if let Some(tip) = tip {
        eprintln!();
        for (i, line) in tip.lines().enumerate() {
            if i == 0 {
                eprintln!("  {} {}", "tip:".cyan().bold(), line);
            } else {
                eprintln!("       {}", line);
            }
        }
    }

    eprintln!();
}

/// Walk the anyhow error chain looking for `Diagnosed` markers.
///
/// The `Diagnosed` wrapper is added by `.diagnose()` or `.with_diagnosis()`
/// at error boundaries. This function performs a single `downcast_ref` —
/// no per-language-type dispatch needed.
fn extract_tip(err: &anyhow::Error) -> Option<String> {
    // Check the outermost error first (DiagnosedError from .diagnose())
    if let Some(d) = err.downcast_ref::<DiagnosedError>() {
        let tip = d.tip();
        if !tip.is_empty() {
            return Some(tip.to_string());
        }
    }
    // Walk the chain for nested DiagnosedError markers
    for cause in err.chain().skip(1) {
        if let Some(d) = cause.downcast_ref::<DiagnosedError>() {
            let tip = d.tip();
            if !tip.is_empty() {
                return Some(tip.to_string());
            }
        }
    }
    // Fallback: pattern-match on common error messages
    pattern_match_tip(err)
}

/// Last-resort pattern matching for errors without `Diagnosed` markers.
///
/// Catches common OS-level errors that may not have been wrapped with
/// a diagnosis at the call site.
fn pattern_match_tip(err: &anyhow::Error) -> Option<String> {
    let msg = format!("{:#}", err);
    if msg.contains("not a git repository") || msg.contains("Not a git repository") {
        return Some("Check that --repo points to a valid git repository root.".into());
    }
    if msg.contains("Permission denied") {
        return Some("Check file permissions for the target path.".into());
    }
    if msg.contains("No space left on device") {
        return Some("Free up disk space and retry.".into());
    }
    if msg.contains("command not found") || msg.contains("No such file or directory") {
        return Some(
            "A required command was not found. Check that git and Node.js are installed.".into(),
        );
    }
    None
}

/// Print an end-of-run summary of non-fatal degradation issues.
///
/// Called at the end of `cmd_analyze_ts` and `cmd_konveyor_ts` to inform
/// the user about parts of the analysis that may be incomplete.
pub fn print_degradation_summary(tracker: &DegradationTracker, reporter: &ProgressReporter) {
    let issues = tracker.issues();
    if issues.is_empty() {
        return;
    }

    reporter.println("");
    reporter.println(&format!(
        "{} Analysis completed with {} warning(s):",
        "warning:".yellow().bold(),
        issues.len()
    ));
    for issue in &issues {
        reporter.println(&format!(
            "  {} [{}] {} — {}",
            "•".dimmed(),
            issue.phase,
            issue.message,
            issue.impact.dimmed()
        ));
    }
}

/// A transitive dependency bump detected from peer dependency narrowing.
#[derive(Debug)]
struct TransitiveBump {
    /// The dependency that must be bumped (e.g., "react").
    dep_name: String,
    /// The old peer dependency range (e.g., "^17 || ^18").
    old_range: String,
    /// The new peer dependency range (e.g., "^18 || ^19").
    new_range: String,
    /// Major versions that were dropped from the supported range.
    dropped_majors: Vec<u64>,
    /// Major versions that were added to the supported range.
    added_majors: Vec<u64>,
    /// Library packages that require this bump.
    source_packages: Vec<String>,
}

/// Extract major version numbers from an npm semver range string.
///
/// Handles ranges like `^17 || ^18`, `>=5`, `^17.0.1`, `~16.8`.
/// Returns a set of major versions found.
fn extract_major_versions(range: &str) -> BTreeSet<u64> {
    let mut majors = BTreeSet::new();
    for segment in range.split("||") {
        let trimmed = segment.trim();
        // Strip range operators: ^, ~, >=, >, <=, <, =
        let version_part = trimmed
            .trim_start_matches('^')
            .trim_start_matches('~')
            .trim_start_matches(">=")
            .trim_start_matches('>')
            .trim_start_matches("<=")
            .trim_start_matches('<')
            .trim_start_matches('=')
            .trim();
        // Take the first numeric segment as the major version
        if let Some(major_str) = version_part.split('.').next() {
            if let Ok(major) = major_str.parse::<u64>() {
                majors.insert(major);
            }
        }
    }
    majors
}

/// Print a summary of transitive dependency bumps detected from peer
/// dependency narrowing in manifest changes.
///
/// When a library drops support for an older major version of a peer
/// dependency (e.g., `react: ^17 || ^18` → `^18 || ^19`), consumers
/// on the dropped version must bump that dependency. This function
/// detects such cases and prints a notice so the user knows to add
/// a migration ruleset for the bumped dependency.
pub fn print_transitive_bump_summary<L: Language>(
    manifest_changes: &[ManifestChange<L>],
    reporter: &ProgressReporter,
) {
    // Collect peer dependency range changes
    let mut bumps: std::collections::BTreeMap<String, TransitiveBump> =
        std::collections::BTreeMap::new();

    for change in manifest_changes {
        // Only look at peer dependency range changes
        let change_type_str = format!("{:?}", change.change_type);
        if change_type_str != "PeerDependencyRangeChanged" {
            continue;
        }

        let (Some(before), Some(after)) = (&change.before, &change.after) else {
            continue;
        };

        // Extract the dependency name from the field (e.g., "peerDependencies.react" → "react")
        let dep_name = change
            .field
            .strip_prefix("peerDependencies.")
            .unwrap_or(&change.field);

        let old_majors = extract_major_versions(before);
        let new_majors = extract_major_versions(after);

        let dropped: Vec<u64> = old_majors.difference(&new_majors).copied().collect();
        let added: Vec<u64> = new_majors.difference(&old_majors).copied().collect();

        if dropped.is_empty() && added.is_empty() {
            continue;
        }

        let source = change
            .source_package
            .clone()
            .unwrap_or_else(|| "(root)".to_string());

        let bump = bumps
            .entry(dep_name.to_string())
            .or_insert_with(|| TransitiveBump {
                dep_name: dep_name.to_string(),
                old_range: before.clone(),
                new_range: after.clone(),
                dropped_majors: Vec::new(),
                added_majors: Vec::new(),
                source_packages: Vec::new(),
            });

        // Merge dropped/added majors (may come from multiple source packages)
        for d in &dropped {
            if !bump.dropped_majors.contains(d) {
                bump.dropped_majors.push(*d);
            }
        }
        for a in &added {
            if !bump.added_majors.contains(a) {
                bump.added_majors.push(*a);
            }
        }
        if !bump.source_packages.contains(&source) {
            bump.source_packages.push(source);
        }
    }

    if bumps.is_empty() {
        return;
    }

    // Separate narrowing (action required) from widening-only (informational)
    let narrowing: Vec<&TransitiveBump> = bumps.values().filter(|b| !b.dropped_majors.is_empty()).collect();
    let widening_only: Vec<&TransitiveBump> = bumps.values().filter(|b| b.dropped_majors.is_empty()).collect();

    if narrowing.is_empty() && widening_only.is_empty() {
        return;
    }

    reporter.println("");
    reporter.println(&format!(
        "{} Transitive dependency changes detected:",
        "notice:".cyan().bold(),
    ));

    for bump in &narrowing {
        reporter.println("");
        reporter.println(&format!(
            "  {} {}: {} → {}",
            "▶".yellow().bold(),
            bump.dep_name.bold(),
            bump.old_range,
            bump.new_range,
        ));
        let dropped_str: Vec<String> = bump.dropped_majors.iter().map(|v| v.to_string()).collect();
        reporter.println(&format!(
            "    {} support for major version(s) {} was dropped.",
            "⚠".yellow(),
            dropped_str.join(", "),
        ));
        if bump.source_packages.len() <= 3 {
            reporter.println(&format!(
                "    Required by: {}",
                bump.source_packages.join(", "),
            ));
        } else {
            reporter.println(&format!(
                "    Required by: {} (and {} more)",
                bump.source_packages[..2].join(", "),
                bump.source_packages.len() - 2,
            ));
        }
        let min_new = bump
            .dropped_majors
            .iter()
            .copied()
            .max()
            .map(|d| d + 1)
            .unwrap_or(0);
        reporter.println(&format!(
            "    Consumers on {} {} must upgrade to {} {}+.",
            bump.dep_name,
            dropped_str.join("/"),
            bump.dep_name,
            min_new,
        ));
        reporter.println(&format!(
            "    {} Consider adding a migration ruleset for {} {} → {}.",
            "→".cyan(),
            bump.dep_name,
            dropped_str.first().unwrap_or(&"?".to_string()),
            min_new,
        ));
    }

    for bump in &widening_only {
        reporter.println("");
        reporter.println(&format!(
            "  {} {}: {} → {}",
            "·".dimmed(),
            bump.dep_name,
            bump.old_range,
            bump.new_range,
        ));
        let added_str: Vec<String> = bump.added_majors.iter().map(|v| v.to_string()).collect();
        reporter.println(&format!(
            "    {} range widened to include version(s) {} — no action required.",
            "✓".green(),
            added_str.join(", "),
        ));
    }

    reporter.println("");
}
