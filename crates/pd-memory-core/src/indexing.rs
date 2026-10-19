//! Heartbeat indexing pipeline (dev B).
//!
//! Port of `heartbeat.ts`: turns queued L0 sessions into Markdown memory files.
//! Process (reference `runHeartbeat`, spec §3.4):
//!
//! 1. take up to `batch_size` sessions that have pending L0 rows, ordered by the
//!    timestamp of their earliest pending row;
//! 2. for each session, load the latest L0 row *before* the pending batch as the
//!    previously indexed prefix, then rolling-merge the pending rows to diff out
//!    the new messages ("session snapshots");
//! 3. split the merged conversation into turns and treat every new `user` message
//!    as a focus turn (assistant text stays context-only in the prompts);
//! 4. for every focus turn: classify → create one note per category → fill
//!    `captured_at` / `source_session_key` from the L0 row → upsert;
//! 5. mark successful L0 records indexed and repair `MEMORY.md`. Persistence
//!    failures stay pending and are reported in stats; model failures degrade
//!    to no classification / no note, as in the reference.
//!
//! The pipeline never calls the model on the capture path; it is invoked by
//! [`crate::engine::MemoryEngine::flush`] / scheduled maintenance.

use serde_json::json;

use crate::config::MemoryConfig;
use crate::error::Result;
use crate::extraction::{self, LlmExtractor};
use crate::ports::{Clock, MemoryStore, QueueStore, Redactor, StateStore, Telemetry};
use crate::types::{HeartbeatStats, L0SessionRecord, MemoryMessage};

/// Dependencies injected by [`crate::engine::MemoryEngine`].
pub struct IndexerDeps<'a> {
    pub config: &'a MemoryConfig,
    pub store: &'a mut dyn MemoryStore,
    pub queue: &'a dyn QueueStore,
    pub state: &'a dyn StateStore,
    pub extractor: &'a LlmExtractor,
    pub clock: &'a dyn Clock,
    pub telemetry: &'a dyn Telemetry,
    pub redactor: &'a dyn Redactor,
    /// Restrict the batch to these sessions (`None` = all pending).
    pub session_keys: Option<Vec<String>>,
}

/// Run one heartbeat batch. Failed records stay pending; successful records
/// continue independently and update their capture-time cursors.
pub fn run_heartbeat(deps: IndexerDeps<'_>, batch_size: usize) -> Result<HeartbeatStats> {
    let IndexerDeps {
        config,
        store,
        queue,
        state,
        extractor,
        clock,
        telemetry,
        redactor,
        session_keys,
    } = deps;

    let started_at = clock.now_iso();
    let mut stats = HeartbeatStats {
        started_at: started_at.clone(),
        ..Default::default()
    };

    // 1. Select the pending sessions for this batch, oldest first.
    let session_cap = batch_size.max(1);
    let wanted_keys = normalized_session_keys(session_keys.as_deref());
    let mut pending_records = queue.pending(wanted_keys.as_deref(), usize::MAX)?;
    if let Some(keys) = wanted_keys.as_deref() {
        // Defensive: honour the filter even if a QueueStore implementation
        // ignores it. An empty filter list means "all sessions" (reference
        // behaviour for `preferredSessionKeys`).
        pending_records.retain(|record| keys.iter().any(|key| key == &record.session_key));
    }
    pending_records.sort_by(|left, right| {
        left.timestamp
            .cmp(&right.timestamp)
            .then_with(|| left.created_at.cmp(&right.created_at))
            .then_with(|| left.l0_index_id.cmp(&right.l0_index_id))
    });
    if pending_records.is_empty() {
        stats.finished_at = clock.now_iso();
        return Ok(stats);
    }

    let mut groups = group_records_by_session(pending_records);
    groups.sort_by(|(left_key, left), (right_key, right)| {
        left[0]
            .timestamp
            .cmp(&right[0].timestamp)
            .then_with(|| left_key.cmp(right_key))
    });
    telemetry.event(
        "memory.index.batch_start",
        json!({
            "batch_size": session_cap,
            "pending_sessions": groups.len(),
        }),
    );

    for (session_key, records) in groups.iter().take(session_cap) {
        let written_before = stats.written_files;
        let processed_ids = process_session(
            config,
            store,
            queue,
            state,
            extractor,
            redactor,
            telemetry,
            session_key,
            records,
            &mut stats,
        )?;
        if !processed_ids.is_empty() {
            queue.mark_indexed(&processed_ids)?;
        }
        telemetry.event(
            "memory.index.session_done",
            json!({
                "session_key": session_key,
                "segments": records.len(),
                "written_files": stats.written_files - written_before,
            }),
        );
    }
    if stats.processed_sessions > 0 || stats.written_files > 0 {
        store.rebuild_manifest()?;
    }

    stats.finished_at = clock.now_iso();
    telemetry.event(
        "memory.index.finish",
        json!({
            "processed_sessions": stats.processed_sessions,
            "processed_turns": stats.processed_turns,
            "written_files": stats.written_files,
            "skipped_turns": stats.skipped_turns,
            "failed_sessions": stats.errors.len(),
        }),
    );

    Ok(stats)
}

/// Split session messages into turns (one user message + following assistant
/// messages), mirroring `buildConversationTurns` in the reference.
pub fn split_turns(messages: &[MemoryMessage]) -> Vec<Vec<MemoryMessage>> {
    extraction::conversation_turns(messages)
}

#[allow(clippy::too_many_arguments)]
fn process_session(
    config: &MemoryConfig,
    store: &mut dyn MemoryStore,
    queue: &dyn QueueStore,
    state: &dyn StateStore,
    extractor: &LlmExtractor,
    redactor: &dyn Redactor,
    telemetry: &dyn Telemetry,
    session_key: &str,
    records: &[L0SessionRecord],
    stats: &mut HeartbeatStats,
) -> Result<Vec<String>> {
    let Some(first_record) = records.first() else {
        return Ok(Vec::new());
    };
    let previous_messages = previous_prefix(queue, session_key, first_record)?;
    let (focus_turns_by_record, batch_context) = derive_batch(&previous_messages, records);

    // Redact defensively: capture already applies the redactor, but prompts
    // must never see raw secrets even if a custom QueueStore stores plain text.
    let redacted_context: Vec<MemoryMessage> = batch_context
        .iter()
        .map(|message| redact_message(redactor, message))
        .collect();
    let turns = extraction::conversation_turns(&redacted_context);

    let mut turn_cursor = 0usize;
    let mut processed_ids: Vec<String> = Vec::with_capacity(records.len());
    for (record, focus_users) in records.iter().zip(focus_turns_by_record.iter()) {
        if focus_users.is_empty() {
            processed_ids.push(record.l0_index_id.clone());
            stats.processed_sessions += 1;
            continue;
        }
        let outcome = (|| -> Result<()> {
            for focus_user in focus_users {
                let redacted_focus = redact_message(redactor, focus_user);
                let turn_index = match turns[turn_cursor..]
                    .iter()
                    .position(|turn| {
                        turn.first()
                            .map(|message| extraction::same_message(message, &redacted_focus))
                            .unwrap_or(false)
                    })
                    .map(|offset| turn_cursor + offset)
                {
                    Some(index) => index,
                    None => {
                        stats.skipped_turns += 1;
                        telemetry.event(
                            "memory.index.turn_skipped",
                            json!({
                                "session_key": session_key,
                                "reason": "focus_turn_not_found",
                            }),
                        );
                        continue;
                    }
                };
                turn_cursor = turn_index + 1;
                let focus_turn = &turns[turn_index];

                stats.processed_turns += 1;
                let project_meta = store.get_project_meta()?;
                let classification = extractor.classify_turn(
                    config,
                    focus_turn,
                    &redacted_context,
                    project_meta.as_ref(),
                )?;
                telemetry.event(
                    "memory.index.classified",
                    json!({
                        "session_key": session_key,
                        "should_store": classification.should_store,
                        "labels": classification
                            .labels
                            .iter()
                            .map(|label| label.kind.as_str())
                            .collect::<Vec<_>>(),
                    }),
                );
                if !classification.should_store {
                    continue;
                }

                let mut candidates = Vec::new();
                for label in &classification.labels {
                    let created = extractor.create_note(
                        config,
                        label.kind,
                        label,
                        focus_turn,
                        &redacted_context,
                        project_meta.as_ref(),
                    )?;
                    let Some(mut candidate) = created else {
                        stats.skipped_turns += 1;
                        telemetry.event(
                            "memory.index.note_skipped",
                            json!({
                                "session_key": session_key,
                                "kind": label.kind.as_str(),
                            }),
                        );
                        continue;
                    };
                    // Spec §3.9: origin identity comes from the L0 row, not from
                    // the model or the write clock.
                    candidate.captured_at = Some(record.timestamp.clone());
                    candidate.source_session_key = Some(record.session_key.clone());
                    candidates.push(candidate);
                }
                for candidate in candidates {
                    let entry = store.upsert_candidate(&candidate)?;
                    stats.written_files += 1;
                    telemetry.event(
                        "memory.index.note_written",
                        json!({
                            "session_key": session_key,
                            "kind": candidate.kind.as_str(),
                            "name": candidate.name,
                            "path": entry.relative_path,
                        }),
                    );
                }
            }
            state.set("lastIndexedAt", json!(record.timestamp))?;
            state.set(
                &format!("lastIndexedCursor:{}", record.session_key),
                json!(record.timestamp),
            )?;
            Ok(())
        })();
        match outcome {
            Ok(()) => {
                processed_ids.push(record.l0_index_id.clone());
                stats.processed_sessions += 1;
            }
            Err(error) => {
                telemetry.event(
                    "memory.index.session_error",
                    json!({
                        "session_key": session_key,
                        "l0_index_id": record.l0_index_id,
                        "message": error.to_string(),
                    }),
                );
                stats
                    .errors
                    .push(format!("{}: {error}", record.l0_index_id));
            }
        }
    }
    Ok(processed_ids)
}

/// Group pending rows by session while preserving oldest-first session order.
fn group_records_by_session(records: Vec<L0SessionRecord>) -> Vec<(String, Vec<L0SessionRecord>)> {
    let mut groups: Vec<(String, Vec<L0SessionRecord>)> = Vec::new();
    for record in records {
        if let Some((_, session_records)) = groups
            .iter_mut()
            .find(|(session_key, _)| session_key == &record.session_key)
        {
            session_records.push(record);
        } else {
            groups.push((record.session_key.clone(), vec![record]));
        }
    }
    groups
}

pub(crate) fn normalized_session_keys(keys: Option<&[String]>) -> Option<Vec<String>> {
    keys.map(|keys| {
        keys.iter()
            .filter(|key| !key.trim().is_empty())
            .cloned()
            .collect::<Vec<_>>()
    })
    .filter(|keys| !keys.is_empty())
}

/// Latest L0 row strictly before the first pending row of the batch.
///
/// Mirrors `getLatestL0Before`: it does **not** require `indexed = 1` (spec
/// §3.4.2); a row is "previous" when `(timestamp, created_at)` is smaller.
fn previous_prefix(
    queue: &dyn QueueStore,
    session_key: &str,
    first_pending: &L0SessionRecord,
) -> Result<Vec<MemoryMessage>> {
    let mut recent = queue.recent_for_session(session_key, usize::MAX)?;
    recent.sort_by(|left, right| {
        left.timestamp
            .cmp(&right.timestamp)
            .then_with(|| left.created_at.cmp(&right.created_at))
            .then_with(|| left.l0_index_id.cmp(&right.l0_index_id))
    });
    let first_key = (
        first_pending.timestamp.as_str(),
        first_pending.created_at.as_str(),
    );
    let latest = recent.iter().rev().find(|candidate| {
        candidate.l0_index_id != first_pending.l0_index_id
            && (candidate.timestamp.as_str(), candidate.created_at.as_str()) < first_key
    });
    Ok(latest
        .map(|record| record.messages.clone())
        .unwrap_or_default())
}

/// Rolling-merge the pending rows (session snapshots) and collect, per row, the
/// new `user` messages that become focus turns. Returns
/// `(focus_turns_per_record, merged_batch_context)`.
fn derive_batch(
    previous_messages: &[MemoryMessage],
    records: &[L0SessionRecord],
) -> (Vec<Vec<MemoryMessage>>, Vec<MemoryMessage>) {
    let mut cursor: Vec<MemoryMessage> = previous_messages.to_vec();
    let mut focus_turns: Vec<Vec<MemoryMessage>> = Vec::with_capacity(records.len());
    for record in records {
        let (merged, new_messages) = merge_messages(&cursor, &record.messages);
        focus_turns.push(
            new_messages
                .into_iter()
                .filter(|message| message.role == "user")
                .collect(),
        );
        cursor = merged;
    }
    (focus_turns, cursor)
}

/// Port of `mergeSessionMessages`: if `incoming` extends `previous` by a common
/// prefix the new messages are the suffix; otherwise the two snapshots are
/// concatenated and every incoming message is new.
fn merge_messages(
    previous: &[MemoryMessage],
    incoming: &[MemoryMessage],
) -> (Vec<MemoryMessage>, Vec<MemoryMessage>) {
    if previous.is_empty() {
        return (incoming.to_vec(), incoming.to_vec());
    }
    let prefix_length = common_prefix_length(previous, incoming);
    if prefix_length > 0 {
        return (incoming.to_vec(), incoming[prefix_length..].to_vec());
    }
    let mut merged = previous.to_vec();
    merged.extend_from_slice(incoming);
    (merged, incoming.to_vec())
}

fn common_prefix_length(left: &[MemoryMessage], right: &[MemoryMessage]) -> usize {
    let limit = left.len().min(right.len());
    let mut index = 0usize;
    while index < limit && extraction::same_message(&left[index], &right[index]) {
        index += 1;
    }
    index
}

fn redact_message(redactor: &dyn Redactor, message: &MemoryMessage) -> MemoryMessage {
    let mut redacted = message.clone();
    redacted.content = redactor.redact(&message.content);
    redacted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(role: &str, content: &str) -> MemoryMessage {
        MemoryMessage::new(role, content)
    }

    fn session(id: &str, messages: Vec<MemoryMessage>) -> L0SessionRecord {
        L0SessionRecord {
            l0_index_id: id.to_string(),
            session_key: "sess".into(),
            timestamp: format!("2026-10-18T09:0{id}:00.000Z"),
            messages,
            source: "test".into(),
            indexed: false,
            created_at: format!("2026-10-18T09:0{id}:00.100Z"),
        }
    }

    #[test]
    fn split_turns_matches_reference_shape() {
        let messages = vec![
            message("user", "u1"),
            message("assistant", "a1"),
            message("assistant", "a2"),
            message("user", "u2"),
        ];
        let turns = split_turns(&messages);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].len(), 3);
        assert_eq!(turns[1].len(), 1);
    }

    #[test]
    fn merge_extends_when_prefix_matches() {
        let previous = vec![message("user", "u1"), message("assistant", "a1")];
        let incoming = vec![
            message("user", "u1"),
            message("assistant", "a1"),
            message("user", "u2"),
        ];
        let (merged, new_messages) = merge_messages(&previous, &incoming);
        assert_eq!(merged, incoming);
        assert_eq!(new_messages, vec![message("user", "u2")]);
    }

    #[test]
    fn merge_concatenates_without_common_prefix() {
        let previous = vec![message("user", "u1"), message("assistant", "a1")];
        let incoming = vec![message("user", "u2"), message("assistant", "a2")];
        let (merged, new_messages) = merge_messages(&previous, &incoming);
        assert_eq!(merged.len(), 4);
        assert_eq!(new_messages, incoming);
    }

    #[test]
    fn derive_batch_only_returns_new_user_turns() {
        let previous = vec![message("user", "u1"), message("assistant", "a1")];
        let records = vec![
            session(
                "1",
                vec![
                    message("user", "u1"),
                    message("assistant", "a1"),
                    message("user", "u2"),
                    message("assistant", "a2"),
                ],
            ),
            session(
                "2",
                vec![
                    message("user", "u1"),
                    message("assistant", "a1"),
                    message("user", "u2"),
                    message("assistant", "a2"),
                    message("user", "u3"),
                    message("assistant", "a3"),
                ],
            ),
        ];
        let (focus_turns, context) = derive_batch(&previous, &records);
        assert_eq!(focus_turns[0].len(), 1);
        assert_eq!(focus_turns[0][0].content, "u2");
        assert_eq!(focus_turns[1].len(), 1);
        assert_eq!(focus_turns[1][0].content, "u3");
        assert_eq!(context.len(), 6);
    }
}
