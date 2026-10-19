//! Due-maintenance scheduling (dev B).
//!
//! Port of `runDueScheduledMaintenance` + anchor semantics
//! (`src/context/memory/edgeclaw-memory-core/src/service.ts:915-969, 687-735`,
//! behaviour spec §6). The SDK has no timer thread; hosts call
//! [`crate::engine::MemoryEngine::run_due_scheduled_maintenance`] after every
//! turn, or a cron job calls it periodically.
//!
//! Decision rules (exact reference thresholds):
//! - **index** is due when the pending backlog is at least 20 L0 rows
//!   (the reference constant is named "dialogue turns" but counts rows), or
//!   when there is at least one pending session and the auto-index interval has
//!   elapsed since the index anchor;
//! - **dream** is due when at least one memory file changed since the last
//!   dream and the auto-dream interval has elapsed since the dream anchor.

use crate::types::IndexingSettings;

/// Minimum pending L0 rows that trigger an index run regardless of the
/// interval (`AUTO_INDEX_PENDING_DIALOGUE_TURN_THRESHOLD`).
pub const AUTO_INDEX_PENDING_DIALOGUE_TURN_THRESHOLD: usize = 20;

const MILLIS_PER_MINUTE: i64 = 60_000;

/// Result of evaluating scheduling anchors.
#[derive(Debug, Clone, Default)]
pub struct ScheduleDecision {
    pub index_due: bool,
    pub dream_due: bool,
    pub pending_turns: usize,
    /// Short machine-readable reason (`pending_threshold`,
    /// `interval_elapsed`, `not_due`).
    pub index_reason: String,
    /// Short machine-readable reason (`no_changed_files`, `interval_elapsed`,
    /// `interval_not_elapsed`).
    pub dream_reason: String,
}

/// Pure decision function; `now_ms` and the anchors are epoch milliseconds.
///
/// Missing or unparsable anchors are `None` and never elapse. The engine
/// reconciles them from pending captures / the last index before deciding.
pub fn decide(
    pending_turns: usize,
    pending_sessions: usize,
    changed_files: usize,
    index_anchor_ms: Option<i64>,
    dream_anchor_ms: Option<i64>,
    settings: &IndexingSettings,
    now_ms: i64,
) -> ScheduleDecision {
    let index_by_backlog = pending_turns >= AUTO_INDEX_PENDING_DIALOGUE_TURN_THRESHOLD;
    let index_by_interval = pending_sessions > 0
        && interval_elapsed(
            index_anchor_ms,
            settings.auto_index_interval_minutes,
            now_ms,
        );
    let dream_due = changed_files > 0
        && interval_elapsed(
            dream_anchor_ms,
            settings.auto_dream_interval_minutes,
            now_ms,
        );

    let index_reason = if index_by_backlog {
        "pending_threshold"
    } else if index_by_interval {
        "interval_elapsed"
    } else {
        "not_due"
    };
    let dream_reason = if changed_files == 0 {
        "no_changed_files"
    } else if dream_due {
        "interval_elapsed"
    } else {
        "interval_not_elapsed"
    };

    ScheduleDecision {
        index_due: index_by_backlog || index_by_interval,
        dream_due,
        pending_turns,
        index_reason: index_reason.to_string(),
        dream_reason: dream_reason.to_string(),
    }
}

/// Convenience alias used by the engine.
pub fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Reference `hasElapsedMinutes`: disabled intervals and missing anchors never elapse.
fn interval_elapsed(anchor_ms: Option<i64>, interval_minutes: u32, now_ms: i64) -> bool {
    let Some(anchor_ms) = anchor_ms else {
        return false;
    };
    interval_minutes > 0
        && now_ms.saturating_sub(anchor_ms) >= i64::from(interval_minutes) * MILLIS_PER_MINUTE
}
