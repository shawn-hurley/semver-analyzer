//! Concurrent shared state for TD/BU pipeline coordination.
//!
//! `SharedFindings` is the central coordination point between the TD (Top-Down)
//! and BU (Bottom-Up) pipelines running concurrently. It provides:
//!
//! 1. **DashMap** for thread-safe structural and behavioral break storage
//! 2. **Broadcast channel** for real-time TD→BU notifications (avoids
//!    redundant LLM calls on symbols TD has already identified)
//! 3. **OnceCell** for the API surfaces extracted by TD (BU reads these
//!    for visibility resolution)
//!
//! ## Coordination Protocol
//!
//! ```text
//! TD pipeline:                         BU pipeline:
//! 1. Extract API surface at ref A      1. Parse git diff
//! 2. Extract API surface at ref B      2. Extract changed functions
//! 3. diff_surfaces()                   3. For each function:
//! 4. For each structural break:           a. Drain broadcast channel
//!    a. Insert into DashMap               b. Check DashMap + skip set
//!    b. Broadcast qualified_name          c. If not found: analyze
//! ```

use crate::traits::Language;
use crate::types::{ApiSurface, BehavioralBreak, StructuralChange};
use dashmap::DashMap;
use std::collections::HashSet;
use tokio::sync::broadcast;

/// Broadcast channel capacity. Sized for typical project API surfaces.
/// If TD produces more findings than this before BU drains them,
/// older messages are dropped — but BU also checks the DashMap directly,
/// so no findings are lost.
const BROADCAST_CAPACITY: usize = 4096;

/// Concurrent shared state between TD and BU pipelines.
///
/// Generic over `L: Language` so that `BehavioralBreak<L>` carries
/// typed category data instead of stringly-typed labels.
///
/// Thread-safe: all fields use concurrent data structures.
/// Wrapped in `Arc` for sharing between async tasks.
pub struct SharedFindings<L: Language> {
    /// Structural breaks found by TD. Keyed by qualified_name.
    /// TD inserts, BU checks before analyzing each function.
    structural_breaks: DashMap<String, StructuralChange>,

    /// Behavioral breaks found by BU. Keyed by qualified_name.
    /// BU inserts after spec inference confirms a behavioral change.
    behavioral_breaks: DashMap<String, BehavioralBreak<L>>,

    /// Broadcast sender: TD sends qualified names here as it finds
    /// structural breaks. BU subscribes and drains pending messages
    /// into a local skip set before each function analysis.
    ///
    /// This avoids BU redundantly analyzing functions that TD is about
    /// to (or just did) flag structurally. The broadcast is "best effort" —
    /// BU also checks the DashMap directly as a fallback.
    td_broadcast_tx: broadcast::Sender<String>,

    /// API surface from the OLD ref (set by TD after extraction).
    old_surface: tokio::sync::OnceCell<ApiSurface>,

    /// API surface from the NEW ref (set by TD after extraction).
    new_surface: tokio::sync::OnceCell<ApiSurface>,
}

impl<L: Language> SharedFindings<L> {
    /// Create new shared state with an empty broadcast channel.
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            structural_breaks: DashMap::new(),
            behavioral_breaks: DashMap::new(),
            td_broadcast_tx: tx,
            old_surface: tokio::sync::OnceCell::new(),
            new_surface: tokio::sync::OnceCell::new(),
        }
    }

    // ── TD operations ───────────────────────────────────────────────

    /// Insert a structural break found by TD.
    ///
    /// Also broadcasts the qualified name to BU via the channel.
    /// If the broadcast fails (no receivers yet), that's fine — BU
    /// will check the DashMap directly.
    pub fn insert_structural_break(&self, change: StructuralChange) {
        let name = change.qualified_name.clone();
        self.structural_breaks.insert(name.clone(), change);
        // Best-effort broadcast; ignore SendError (no receivers)
        let _ = self.td_broadcast_tx.send(name);
    }

    /// Insert multiple structural breaks (batch operation after diff_surfaces).
    pub fn insert_structural_breaks(&self, changes: Vec<StructuralChange>) {
        for change in changes {
            self.insert_structural_break(change);
        }
    }

    /// Set the old API surface (called by TD after extraction).
    pub fn set_old_surface(&self, surface: ApiSurface) {
        let _ = self.old_surface.set(surface);
    }

    /// Set the new API surface (called by TD after extraction).
    pub fn set_new_surface(&self, surface: ApiSurface) {
        let _ = self.new_surface.set(surface);
    }

    // ── BU operations ───────────────────────────────────────────────

    /// Subscribe to TD's broadcast channel.
    ///
    /// Call this once at the start of BU. Returns a `BuReceiver` that
    /// wraps the broadcast receiver and a local skip set for efficient
    /// repeated checks.
    pub fn subscribe_to_td(&self) -> BuReceiver {
        BuReceiver {
            rx: self.td_broadcast_tx.subscribe(),
            skip_set: HashSet::new(),
        }
    }

    /// Check if TD already found a structural break for this symbol.
    ///
    /// This is the DashMap fallback — always accurate but doesn't
    /// benefit from the broadcast channel's real-time notifications.
    pub fn has_structural_break(&self, qualified_name: &str) -> bool {
        self.structural_breaks.contains_key(qualified_name)
    }

    /// Insert a behavioral break found by BU.
    pub fn insert_behavioral_break(&self, brk: BehavioralBreak<L>) {
        self.behavioral_breaks.insert(brk.symbol.clone(), brk);
    }

    // ── Read operations (post-analysis merge) ───────────────────────

    /// Get all structural breaks (for merge step).
    pub fn structural_breaks(&self) -> &DashMap<String, StructuralChange> {
        &self.structural_breaks
    }

    /// Get all behavioral breaks (for merge step).
    pub fn behavioral_breaks(&self) -> &DashMap<String, BehavioralBreak<L>> {
        &self.behavioral_breaks
    }

    /// Get the old API surface (blocks if TD hasn't set it yet).
    pub async fn get_old_surface(&self) -> &ApiSurface {
        self.old_surface.get_or_init(|| async {
            panic!("TD must set old_surface before BU reads it")
        }).await
    }

    /// Get the new API surface (blocks if TD hasn't set it yet).
    pub async fn get_new_surface(&self) -> &ApiSurface {
        self.new_surface.get_or_init(|| async {
            panic!("TD must set new_surface before BU reads it")
        }).await
    }

    /// Try to get the old surface without blocking (returns None if not set yet).
    pub fn try_get_old_surface(&self) -> Option<&ApiSurface> {
        self.old_surface.get()
    }

    /// Try to get the new surface without blocking (returns None if not set yet).
    pub fn try_get_new_surface(&self) -> Option<&ApiSurface> {
        self.new_surface.get()
    }

    /// Count of structural breaks found so far.
    pub fn structural_break_count(&self) -> usize {
        self.structural_breaks.len()
    }

    /// Count of behavioral breaks found so far.
    pub fn behavioral_break_count(&self) -> usize {
        self.behavioral_breaks.len()
    }

    /// Get all structural break qualified names (for reconciliation).
    pub fn structural_break_names(&self) -> Vec<String> {
        self.structural_breaks
            .iter()
            .map(|entry| entry.key().clone())
            .collect()
    }
}

impl<L: Language> Default for SharedFindings<L> {
    fn default() -> Self {
        Self::new()
    }
}

/// BU-side broadcast receiver with local skip set.
///
/// The skip set accumulates qualified names received from TD's broadcast
/// channel. Before analyzing each function, BU calls `drain_and_check()`
/// which:
/// 1. Drains any pending broadcast messages into the local skip set
/// 2. Checks if the given qualified name is in the skip set
///
/// This is faster than checking the DashMap for every function because
/// it avoids hash map lookups for symbols already seen via broadcast.
pub struct BuReceiver {
    rx: broadcast::Receiver<String>,
    skip_set: HashSet<String>,
}

impl BuReceiver {
    /// Drain pending broadcast messages and check if a symbol should be skipped.
    ///
    /// Returns `true` if the symbol was found in the broadcast skip set
    /// (meaning TD already flagged it). The caller should ALSO check
    /// `SharedFindings::has_structural_break()` as a fallback for messages
    /// that arrived before subscription.
    pub fn drain_and_check(&mut self, qualified_name: &str) -> bool {
        // Drain all pending messages from the broadcast channel
        loop {
            match self.rx.try_recv() {
                Ok(name) => {
                    self.skip_set.insert(name);
                }
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Closed) => break,
                Err(broadcast::error::TryRecvError::Lagged(n)) => {
                    // Channel lagged — some messages were dropped.
                    // This is fine because we also check the DashMap directly.
                    eprintln!(
                        "BU broadcast receiver lagged by {} messages; \
                         falling back to DashMap checks",
                        n
                    );
                    break;
                }
            }
        }

        self.skip_set.contains(qualified_name)
    }

    /// Check if a symbol is in the skip set WITHOUT draining.
    /// Useful for batch checks after an initial drain.
    pub fn is_skipped(&self, qualified_name: &str) -> bool {
        self.skip_set.contains(qualified_name)
    }

    /// Number of symbols in the skip set.
    pub fn skip_set_size(&self) -> usize {
        self.skip_set.len()
    }
}

/// Helper: check if a symbol should be skipped by BU.
///
/// Combines both the broadcast skip set AND the DashMap fallback.
/// This is the recommended way for BU to check before analyzing a function.
pub fn should_skip_for_bu<L: Language>(
    shared: &SharedFindings<L>,
    receiver: &mut BuReceiver,
    qualified_name: &str,
) -> bool {
    receiver.drain_and_check(qualified_name) || shared.has_structural_break(qualified_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestLang;
    use crate::types::{ChangeSubject, StructuralChangeType, SymbolKind};
    use std::sync::Arc;

    fn make_structural_change(name: &str) -> StructuralChange {
        StructuralChange {
            symbol: name.to_string(),
            qualified_name: name.to_string(),
            kind: SymbolKind::Function,
            package: None,
            change_type: StructuralChangeType::Removed(ChangeSubject::Symbol { kind: SymbolKind::Function }),
            before: None,
            after: None,
            description: format!("{} was removed", name),
            is_breaking: true,
            impact: None,
            migration_target: None,
        }
    }

    fn make_behavioral_break(name: &str) -> BehavioralBreak<TestLang> {
        BehavioralBreak {
            symbol: name.to_string(),
            caused_by: name.to_string(),
            call_path: vec![name.to_string()],
            evidence_description: "TestDelta: test assertions changed".to_string(),
            confidence: 0.95,
            description: format!("{} behavior changed", name),
            category: None,
            evidence_type: crate::types::EvidenceType::TestDelta,
            is_internal_only: None,
        }
    }

    #[test]
    fn shared_findings_basic_operations() {
        let shared: SharedFindings<TestLang> = SharedFindings::new();

        // Initially empty
        assert_eq!(shared.structural_break_count(), 0);
        assert_eq!(shared.behavioral_break_count(), 0);

        // Insert structural break
        shared.insert_structural_break(make_structural_change("foo"));
        assert_eq!(shared.structural_break_count(), 1);
        assert!(shared.has_structural_break("foo"));
        assert!(!shared.has_structural_break("bar"));

        // Insert behavioral break
        shared.insert_behavioral_break(make_behavioral_break("bar"));
        assert_eq!(shared.behavioral_break_count(), 1);
    }

    #[test]
    fn shared_findings_batch_insert() {
        let shared: SharedFindings<TestLang> = SharedFindings::new();

        let changes = vec![
            make_structural_change("a"),
            make_structural_change("b"),
            make_structural_change("c"),
        ];
        shared.insert_structural_breaks(changes);

        assert_eq!(shared.structural_break_count(), 3);
        assert!(shared.has_structural_break("a"));
        assert!(shared.has_structural_break("b"));
        assert!(shared.has_structural_break("c"));
    }

    #[test]
    fn broadcast_receiver_skip_set() {
        let shared: SharedFindings<TestLang> = SharedFindings::new();
        let mut receiver = shared.subscribe_to_td();

        // Insert a structural break (also broadcasts)
        shared.insert_structural_break(make_structural_change("foo"));

        // BU drains and checks
        assert!(receiver.drain_and_check("foo"));
        assert!(!receiver.drain_and_check("bar"));

        // After drain, "foo" stays in skip set
        assert!(receiver.is_skipped("foo"));
        assert!(!receiver.is_skipped("bar"));
    }

    #[test]
    fn broadcast_multiple_messages() {
        let shared: SharedFindings<TestLang> = SharedFindings::new();
        let mut receiver = shared.subscribe_to_td();

        // Insert several structural breaks
        shared.insert_structural_break(make_structural_change("alpha"));
        shared.insert_structural_break(make_structural_change("beta"));
        shared.insert_structural_break(make_structural_change("gamma"));

        // First drain picks up all three
        assert!(receiver.drain_and_check("alpha"));
        assert!(receiver.is_skipped("beta"));
        assert!(receiver.is_skipped("gamma"));
        assert_eq!(receiver.skip_set_size(), 3);
    }

    #[test]
    fn should_skip_combines_broadcast_and_dashmap() {
        let shared: SharedFindings<TestLang> = SharedFindings::new();

        // Insert BEFORE subscribing — won't appear in broadcast
        shared.insert_structural_break(make_structural_change("early"));

        let mut receiver = shared.subscribe_to_td();

        // Insert AFTER subscribing — will appear in broadcast
        shared.insert_structural_break(make_structural_change("late"));

        // "early" found via DashMap fallback, "late" via broadcast
        assert!(should_skip_for_bu(&shared, &mut receiver, "early"));
        assert!(should_skip_for_bu(&shared, &mut receiver, "late"));
        assert!(!should_skip_for_bu(&shared, &mut receiver, "unknown"));
    }

    #[test]
    fn structural_break_names() {
        let shared: SharedFindings<TestLang> = SharedFindings::new();
        shared.insert_structural_break(make_structural_change("x"));
        shared.insert_structural_break(make_structural_change("y"));

        let mut names = shared.structural_break_names();
        names.sort();
        assert_eq!(names, vec!["x", "y"]);
    }

    #[test]
    fn surface_try_get_before_set() {
        let shared: SharedFindings<TestLang> = SharedFindings::new();
        assert!(shared.try_get_old_surface().is_none());
        assert!(shared.try_get_new_surface().is_none());
    }

    #[test]
    fn surface_set_and_get() {
        let shared: SharedFindings<TestLang> = SharedFindings::new();

        let surface = ApiSurface {
            symbols: vec![],
        };
        shared.set_old_surface(surface);
        assert!(shared.try_get_old_surface().is_some());
        assert_eq!(shared.try_get_old_surface().unwrap().symbols.len(), 0);
    }

    #[tokio::test]
    async fn surface_async_get() {
        let shared: Arc<SharedFindings<TestLang>> = Arc::new(SharedFindings::new());

        let surface = ApiSurface {
            symbols: vec![],
        };
        shared.set_new_surface(surface);

        let result = shared.get_new_surface().await;
        assert_eq!(result.symbols.len(), 0);
    }

    #[test]
    fn concurrent_inserts() {
        use std::thread;

        let shared: Arc<SharedFindings<TestLang>> = Arc::new(SharedFindings::new());
        let mut handles = Vec::new();

        // Spawn 10 threads, each inserting 100 structural breaks
        for t in 0..10 {
            let shared = shared.clone();
            handles.push(thread::spawn(move || {
                for i in 0..100 {
                    let name = format!("fn_{}_{}", t, i);
                    shared.insert_structural_break(make_structural_change(&name));
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(shared.structural_break_count(), 1000);
    }
}
