//! Types for the BU (Bottom-Up) behavioral analysis pipeline.
//!
//! These types support the full BU flow:
//! 1. `ChangedFunction` — a function whose body changed between two git refs
//! 2. `TestDiff` — diff of a test file with assertion change detection
//! 3. `FunctionSpec` — template-constrained behavioral specification (LLM output)
//! 4. `BehavioralBreak` — a detected behavioral breaking change
//! 5. `SharedFindings` — concurrent state shared between TD and BU
//! 6. `Caller` — a function that calls another (for call graph walking)

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use super::{SymbolKind, Visibility};

// ── Changed Function (DiffParser output) ────────────────────────────────

/// A function whose body changed between two git refs.
///
/// Produced by `DiffParser::parse_changed_functions()`. Includes both
/// public and private functions — BU processes all of them, using
/// visibility to decide whether to walk UP the call graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangedFunction {
    /// Fully qualified name (e.g., "src/api/users.ts::createUser" or
    /// "src/api/users.ts::UserValidator.validate").
    pub qualified_name: String,

    /// Simple name (e.g., "createUser", "validate").
    pub name: String,

    /// Source file containing this function.
    pub file: PathBuf,

    /// Line number in the NEW version (1-indexed).
    pub line: usize,

    /// What kind of symbol this is.
    pub kind: SymbolKind,

    /// Whether this function is exported.
    pub visibility: Visibility,

    /// The function body source text in the OLD version.
    /// Empty string if the function was added (not present in old).
    pub old_body: String,

    /// The function body source text in the NEW version.
    /// Empty string if the function was removed (not present in new).
    pub new_body: String,

    /// The function signature in the OLD version.
    /// e.g., "function createUser(email: string, options?: CreateUserOptions): Promise<User>"
    pub old_signature: String,

    /// The function signature in the NEW version.
    pub new_signature: String,
}

// ── Test Diff (TestAnalyzer output) ─────────────────────────────────────

/// Diff of a test file between two refs.
///
/// Uses text-based assertion detection (Option B from PLAN.md):
/// no framework-specific parsing — just regex matching on common
/// assertion patterns (`expect`, `assert`, `should`, `toBe`, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestDiff {
    /// Path to the test file.
    pub test_file: PathBuf,

    /// Lines removed from the old version that contain assertions.
    pub removed_assertions: Vec<String>,

    /// Lines added in the new version that contain assertions.
    pub added_assertions: Vec<String>,

    /// True if any assertion-like lines changed.
    /// When true, this is HIGH confidence evidence of behavioral change.
    pub has_assertion_changes: bool,

    /// Raw unified diff for LLM context (Option C).
    /// Even when `has_assertion_changes` is false, the full diff
    /// provides useful context for LLM spec inference.
    pub full_diff: String,
}

// ── Function Spec (LLM output, template-constrained) ────────────────────

/// Inferred behavioral specification for a function.
///
/// Template-constrained: the LLM emits this as a JSON object matching
/// this schema. Each field uses structured sub-types rather than free-text
/// strings, enabling mechanical comparison without a second LLM call.
///
/// Based on Preguss/SESpec finding: template-guided generation drops
/// hallucination from ~30% to ~11-19%.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionSpec {
    /// Input validation rules. Each constraint specifies which parameter,
    /// what condition is checked, and what happens on violation.
    pub preconditions: Vec<Precondition>,

    /// Guaranteed outputs. Each specifies a condition under which a
    /// specific return value/shape is produced.
    pub postconditions: Vec<Postcondition>,

    /// Error/exception behavior. Each specifies a trigger condition,
    /// the error type thrown/rejected, and the error message pattern.
    pub error_behavior: Vec<ErrorBehavior>,

    /// External state changes (DB writes, file I/O, network calls,
    /// event emissions, logging). Each specifies what is mutated and how.
    pub side_effects: Vec<SideEffect>,

    /// Free-text notes for behavioral nuances that don't fit the
    /// structured fields above. Compared via LLM fallback only.
    #[serde(default)]
    pub notes: Vec<String>,
}

/// A precondition: input validation rule for a parameter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Precondition {
    /// Which parameter this constrains (e.g., "email").
    pub parameter: String,

    /// What condition is checked (e.g., "must be non-empty string").
    pub condition: String,

    /// What happens when violated (e.g., "throws TypeError").
    pub on_violation: String,
}

/// A postcondition: guaranteed output for a given condition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Postcondition {
    /// When this output is produced (e.g., "valid email provided").
    pub condition: String,

    /// What is returned/resolved (e.g., "User object with normalized email").
    pub returns: String,
}

/// Error behavior: what errors/exceptions the function throws.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBehavior {
    /// What input/state causes the error (e.g., "email contains invalid chars").
    pub trigger: String,

    /// Error type thrown (e.g., "TypeError", "ValidationError").
    pub error_type: String,

    /// Optional: regex or substring of error message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_pattern: Option<String>,
}

/// A side effect: external state change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SideEffect {
    /// What external state is changed (e.g., "database", "event bus").
    pub target: String,

    /// What action is performed (e.g., "inserts row", "emits event").
    pub action: String,

    /// When this side effect occurs (e.g., "on successful validation").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
}

// ── Behavioral Break (BU pipeline output) ───────────────────────────────

/// A detected behavioral breaking change.
///
/// Produced by the BU pipeline. Records the affected public symbol,
/// the root cause (potentially a private function), the call path,
/// and the evidence that supports the finding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BehavioralBreak {
    /// The affected PUBLIC symbol's qualified name.
    /// This is the symbol consumers interact with.
    pub symbol: String,

    /// The function that actually changed (may be the same as `symbol`
    /// for directly-changed public functions, or a private function
    /// for transitive breaks).
    pub caused_by: String,

    /// The call path from `symbol` to `caused_by`.
    /// e.g., `["createUser", "_processInput", "_normalizeEmail"]`
    /// First element is the public symbol, last is the root cause.
    pub call_path: Vec<String>,

    /// How the behavioral change was detected.
    pub evidence: EvidenceSource,

    /// Confidence score (0.0 to 1.0).
    pub confidence: f64,

    /// Human-readable description of the behavioral change.
    pub description: String,
}

/// How the behavioral change was detected.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum EvidenceSource {
    /// Test assertions changed — developer explicitly declared new behavior.
    /// Highest confidence (0.95), no LLM needed.
    TestDelta { test_diff: TestDiff },

    /// Test exists but didn't change. LLM analyzed with test as context.
    /// Medium confidence (0.70).
    LlmWithTestContext {
        spec_old: FunctionSpec,
        spec_new: FunctionSpec,
    },

    /// No test found. LLM analyzed body diff alone.
    /// Lower confidence (0.55).
    LlmOnly {
        spec_old: FunctionSpec,
        spec_new: FunctionSpec,
    },
}

// ── Call Graph Types ────────────────────────────────────────────────────

/// A function that calls another function (used for call graph walking).
#[derive(Debug, Clone)]
pub struct Caller {
    /// Fully qualified name of the calling function.
    pub qualified_name: String,

    /// Source file containing this caller.
    pub file: PathBuf,

    /// Line number of the caller definition.
    pub line: usize,

    /// Whether this caller is exported.
    pub visibility: Visibility,

    /// The caller's body source text (for propagation analysis).
    pub body: String,

    /// The caller's signature (for propagation analysis).
    pub signature: String,
}

/// A reference to a symbol found by cross-file search (impact analysis).
#[derive(Debug, Clone)]
pub struct Reference {
    /// File that references the symbol.
    pub file: PathBuf,

    /// Line number of the reference.
    pub line: usize,

    /// The local name used at the reference site.
    pub local_binding: String,

    /// The function/class containing the reference (if any).
    pub enclosing_symbol: Option<String>,
}

/// A test file associated with a source file.
#[derive(Debug, Clone)]
pub struct TestFile {
    /// Path to the test file.
    pub path: PathBuf,

    /// How the test file was discovered (naming convention).
    pub convention: TestConvention,
}

/// How a test file is associated with its source file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestConvention {
    /// `foo.test.ts` alongside `foo.ts`
    DotTest,
    /// `foo.spec.ts` alongside `foo.ts`
    DotSpec,
    /// `__tests__/foo.ts` or `__tests__/foo.test.ts`
    TestsDir,
}

/// Verdict from spec comparison: is the behavioral change breaking?
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BreakingVerdict {
    /// Whether the change is breaking.
    pub is_breaking: bool,

    /// What specifically changed (for structured comparisons).
    pub reasons: Vec<String>,

    /// Confidence score for this verdict.
    pub confidence: f64,
}

// ── Shared Findings (TD/BU coordination) ────────────────────────────────
// NOTE: SharedFindings is defined in the orchestrator module (binary crate
// or a dedicated concurrency module) because it requires DashMap and
// tokio::sync which are not core type dependencies. The types above
// (BehavioralBreak, StructuralChange) are what get stored IN the shared
// state, but the concurrent container itself lives elsewhere.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_function_round_trip() {
        let cf = ChangedFunction {
            qualified_name: "src/api/users.ts::createUser".into(),
            name: "createUser".into(),
            file: PathBuf::from("src/api/users.ts"),
            line: 10,
            kind: SymbolKind::Function,
            visibility: Visibility::Exported,
            old_body: "{ return db.insert(email); }".into(),
            new_body: "{ return db.insert(email.toLowerCase()); }".into(),
            old_signature: "function createUser(email: string): Promise<User>".into(),
            new_signature: "function createUser(email: string): Promise<User>".into(),
        };

        let json = serde_json::to_string(&cf).unwrap();
        let deserialized: ChangedFunction = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.qualified_name, cf.qualified_name);
        assert_eq!(deserialized.old_body, cf.old_body);
        assert_eq!(deserialized.new_body, cf.new_body);
    }

    #[test]
    fn function_spec_round_trip() {
        let spec = FunctionSpec {
            preconditions: vec![Precondition {
                parameter: "email".into(),
                condition: "must be non-empty string".into(),
                on_violation: "throws TypeError".into(),
            }],
            postconditions: vec![Postcondition {
                condition: "valid email provided".into(),
                returns: "User object with normalized email".into(),
            }],
            error_behavior: vec![ErrorBehavior {
                trigger: "email is empty".into(),
                error_type: "TypeError".into(),
                message_pattern: Some("email must not be empty".into()),
            }],
            side_effects: vec![SideEffect {
                target: "database".into(),
                action: "inserts user row".into(),
                condition: Some("on successful validation".into()),
            }],
            notes: vec!["Email is lowercased before insertion".into()],
        };

        let json = serde_json::to_string_pretty(&spec).unwrap();
        let deserialized: FunctionSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.preconditions.len(), 1);
        assert_eq!(deserialized.postconditions.len(), 1);
        assert_eq!(deserialized.error_behavior.len(), 1);
        assert_eq!(deserialized.side_effects.len(), 1);
        assert_eq!(deserialized.notes.len(), 1);
    }

    #[test]
    fn evidence_source_variants_serialize() {
        let test_delta = EvidenceSource::TestDelta {
            test_diff: TestDiff {
                test_file: PathBuf::from("src/api/users.test.ts"),
                removed_assertions: vec!["expect(result.email).toBe('A@B.COM')".into()],
                added_assertions: vec!["expect(result.email).toBe('a@b.com')".into()],
                has_assertion_changes: true,
                full_diff: "@@ -10,3 +10,3 @@\n-expect(result.email).toBe('A@B.COM')\n+expect(result.email).toBe('a@b.com')".into(),
            },
        };

        let json = serde_json::to_string(&test_delta).unwrap();
        assert!(json.contains("\"type\":\"TestDelta\""));

        let llm_only = EvidenceSource::LlmOnly {
            spec_old: FunctionSpec {
                preconditions: vec![],
                postconditions: vec![],
                error_behavior: vec![],
                side_effects: vec![],
                notes: vec![],
            },
            spec_new: FunctionSpec {
                preconditions: vec![],
                postconditions: vec![],
                error_behavior: vec![],
                side_effects: vec![],
                notes: vec![],
            },
        };

        let json = serde_json::to_string(&llm_only).unwrap();
        assert!(json.contains("\"type\":\"LlmOnly\""));
    }

    #[test]
    fn behavioral_break_with_call_path() {
        let brk = BehavioralBreak {
            symbol: "createUser".into(),
            caused_by: "_normalizeEmail".into(),
            call_path: vec![
                "createUser".into(),
                "_processInput".into(),
                "_normalizeEmail".into(),
            ],
            evidence: EvidenceSource::LlmOnly {
                spec_old: FunctionSpec {
                    preconditions: vec![],
                    postconditions: vec![Postcondition {
                        condition: "always".into(),
                        returns: "lowercased email".into(),
                    }],
                    error_behavior: vec![],
                    side_effects: vec![],
                    notes: vec![],
                },
                spec_new: FunctionSpec {
                    preconditions: vec![],
                    postconditions: vec![Postcondition {
                        condition: "always".into(),
                        returns: "lowercased email with + alias stripped".into(),
                    }],
                    error_behavior: vec![],
                    side_effects: vec![],
                    notes: vec![],
                },
            },
            confidence: 0.55,
            description: "Email normalization now strips + aliases".into(),
        };

        assert_eq!(brk.call_path.len(), 3);
        assert_eq!(brk.call_path[0], "createUser");
        assert_eq!(brk.call_path[2], "_normalizeEmail");
        assert!(brk.confidence < 0.6);
    }

    #[test]
    fn breaking_verdict_default() {
        let verdict = BreakingVerdict {
            is_breaking: true,
            reasons: vec!["postcondition weakened: email normalization changed".into()],
            confidence: 0.80,
        };

        assert!(verdict.is_breaking);
        assert_eq!(verdict.reasons.len(), 1);
    }
}
