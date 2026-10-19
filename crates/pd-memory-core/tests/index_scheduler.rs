//! Scheduler threshold-matrix tests (`runDueScheduledMaintenance` parity).
//!
//! Reference: `service.ts:915-969` / behaviour spec §6.2-6.3.
//! Defaults: auto-index every 30 min, auto-dream every 60 min,
//! pending backlog threshold 20 rows.

use pd_memory_core::scheduler::{decide, now_millis, AUTO_INDEX_PENDING_DIALOGUE_TURN_THRESHOLD};
use pd_memory_core::types::IndexingSettings;

const MINUTE_MS: i64 = 60_000;
const NOW: i64 = 1_800_000_000_000;

fn settings() -> IndexingSettings {
    IndexingSettings::default()
}

#[test]
fn defaults_match_reference() {
    let settings = settings();
    assert_eq!(settings.auto_index_interval_minutes, 30);
    assert_eq!(settings.auto_dream_interval_minutes, 60);
    assert_eq!(AUTO_INDEX_PENDING_DIALOGUE_TURN_THRESHOLD, 20);
}

#[test]
fn index_fires_on_backlog_threshold() {
    let decision = decide(20, 1, 0, Some(NOW), Some(NOW), &settings(), NOW);
    assert!(decision.index_due);
    assert_eq!(decision.index_reason, "pending_threshold");
    assert_eq!(decision.pending_turns, 20);
}

#[test]
fn index_does_not_fire_below_backlog_without_elapsed_interval() {
    let decision = decide(19, 1, 0, Some(NOW - MINUTE_MS), Some(NOW), &settings(), NOW);
    assert!(!decision.index_due);
    assert_eq!(decision.index_reason, "not_due");
}

#[test]
fn index_fires_when_interval_elapsed_and_sessions_pending() {
    let anchor = NOW - 30 * MINUTE_MS;
    let decision = decide(1, 2, 0, Some(anchor), Some(NOW), &settings(), NOW);
    assert!(decision.index_due);
    assert_eq!(decision.index_reason, "interval_elapsed");

    let just_before = decide(1, 2, 0, Some(anchor + 1), Some(NOW), &settings(), NOW);
    assert!(!just_before.index_due);
}

#[test]
fn index_interval_requires_pending_sessions() {
    // No pending sessions: the interval branch never fires, even with anchor 0.
    let decision = decide(0, 0, 0, None, Some(NOW), &settings(), NOW);
    assert!(!decision.index_due);
    assert_eq!(decision.index_reason, "not_due");
}

#[test]
fn index_missing_anchor_does_not_elapse() {
    let decision = decide(1, 1, 0, None, Some(NOW), &settings(), NOW);
    assert!(!decision.index_due);
    assert_eq!(decision.index_reason, "not_due");
}

#[test]
fn index_zero_interval_never_elapses() {
    let mut settings = settings();
    settings.auto_index_interval_minutes = 0;
    let decision = decide(1, 1, 0, None, None, &settings, NOW);
    assert!(!decision.index_due);

    // A backlog still wins over a disabled interval.
    let backlog = decide(20, 1, 0, None, None, &settings, NOW);
    assert!(backlog.index_due);
    assert_eq!(backlog.index_reason, "pending_threshold");
}

#[test]
fn index_backlog_wins_over_interval_reason() {
    let decision = decide(20, 0, 0, Some(NOW - MINUTE_MS), Some(NOW), &settings(), NOW);
    assert!(decision.index_due);
    assert_eq!(decision.index_reason, "pending_threshold");
}

#[test]
fn index_clock_skew_does_not_elapse() {
    let anchor = NOW + 5 * MINUTE_MS;
    let decision = decide(19, 1, 0, Some(anchor), Some(NOW), &settings(), NOW);
    assert!(!decision.index_due);
}

#[test]
fn dream_requires_changed_files() {
    let decision = decide(0, 0, 0, None, None, &settings(), NOW);
    assert!(!decision.dream_due);
    assert_eq!(decision.dream_reason, "no_changed_files");
}

#[test]
fn dream_missing_anchor_does_not_elapse() {
    let decision = decide(0, 0, 1, None, None, &settings(), NOW);
    assert!(!decision.dream_due);
    assert_eq!(decision.dream_reason, "interval_not_elapsed");
}

#[test]
fn dream_waits_for_interval() {
    let anchor = NOW - 60 * MINUTE_MS + 1;
    let decision = decide(0, 0, 3, None, Some(anchor), &settings(), NOW);
    assert!(!decision.dream_due);
    assert_eq!(decision.dream_reason, "interval_not_elapsed");
}

#[test]
fn dream_boundary_is_inclusive() {
    let anchor = NOW - 60 * MINUTE_MS;
    let decision = decide(0, 0, 3, None, Some(anchor), &settings(), NOW);
    assert!(decision.dream_due);

    let just_before = decide(0, 0, 3, None, Some(anchor + 1), &settings(), NOW);
    assert!(!just_before.dream_due);
}

#[test]
fn dream_zero_interval_never_elapses() {
    let mut settings = settings();
    settings.auto_dream_interval_minutes = 0;
    let decision = decide(0, 0, 5, None, None, &settings, NOW);
    assert!(!decision.dream_due);
}

#[test]
fn index_and_dream_decisions_are_independent() {
    let settings = settings();
    let decision = decide(
        20,
        1,
        1,
        Some(NOW - 31 * MINUTE_MS),
        Some(NOW - 61 * MINUTE_MS),
        &settings,
        NOW,
    );
    assert!(decision.index_due);
    assert!(decision.dream_due);
    assert_eq!(decision.index_reason, "pending_threshold");
    assert_eq!(decision.dream_reason, "interval_elapsed");
}

#[test]
fn now_millis_is_current_epoch_millis() {
    let before = now_millis();
    assert!(before > 1_700_000_000_000, "clock looks wrong: {before}");
    let after = now_millis();
    assert!(after >= before);
}

#[test]
fn epoch_and_pre_epoch_anchors_are_valid_timestamps() {
    for anchor in [0, -60_000] {
        let decision = decide(1, 1, 1, Some(anchor), Some(anchor), &settings(), NOW);
        assert!(decision.index_due);
        assert!(decision.dream_due);
    }
}
