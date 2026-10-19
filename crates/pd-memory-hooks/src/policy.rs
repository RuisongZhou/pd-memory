//! Host-agnostic lifecycle policy for `pd-memory hook --host <id>`.
//!
//! Every host gets the same three guarantees; [`HostCaps`] tunes them:
//!
//! - **Host lock**: hooks of one host run one at a time across processes,
//!   including engine construction, so each sees state committed by the
//!   previous capture/flush/recall. The lock is `control/<id>/hooks.lock`
//!   under the workspace data directory; the OS releases it when the process
//!   exits, even if the host kills it. Session end waits at most half of
//!   [`HostCaps::end_budget_ms`].
//! - **First-recall barrier**: the first non-empty prompt of a session drains
//!   the indexing backlog before recall, then writes the session marker
//!   `control/<id>/<sha1(session key)>.checked`. A failed drain leaves no
//!   marker, so the next prompt retries. Session end removes the marker.
//! - **Session end**: captures the transcript. When the host allows enough
//!   time ([`HostCaps::end_can_flush`]) it also runs due maintenance and
//!   drains the backlog; otherwise it only captures and leaves the backlog
//!   to the next session's first recall.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use pd_memory_core::config::MemoryConfig;
use pd_memory_core::engine::MemoryEngine;
use pd_memory_core::error::{MemoryError, Result};
use pd_memory_core::types::{HeartbeatStats, MemoryMessage};
use sha1::{Digest, Sha1};

use crate::events::{self, EngineHooks, HookEngine};
use crate::hosts::{CanonicalEvent, HostAdapter};
use crate::HookInput;

/// Directory holding one host's lock and session markers.
pub fn control_dir(config: &MemoryConfig, host: &dyn HostAdapter) -> PathBuf {
    config
        .store_layout()
        .roots
        .workspace_dir
        .join("control")
        .join(host.id())
}

/// One hook invocation holding its host's lock.
pub struct Session<'a> {
    host: &'a dyn HostAdapter,
    event: CanonicalEvent,
    _lock: File,
    checked: Option<PathBuf>,
}

impl<'a> Session<'a> {
    /// Acquire `host`'s lock under `config`'s workspace data directory.
    pub fn lock(
        host: &'a dyn HostAdapter,
        config: &MemoryConfig,
        input: &HookInput,
    ) -> Result<Self> {
        Self::lock_in(host, &control_dir(config, host), input)
    }

    /// Acquire `host`'s lock in an explicit control directory.
    pub fn lock_in(host: &'a dyn HostAdapter, directory: &Path, input: &HookInput) -> Result<Self> {
        let event = host.normalize(input);
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(directory)
            .map_err(|error| MemoryError::io(directory, error))?;
        let path = directory.join("hooks.lock");
        let lock = private_file(&path)?;
        let timeout = Duration::from_millis(host.caps().lock_wait_ms(event.kind()));
        let started = Instant::now();
        loop {
            match fs2::FileExt::try_lock_exclusive(&lock) {
                Ok(()) => break,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if started.elapsed() >= timeout {
                        return Err(MemoryError::Other(format!(
                            "{} hook lock timed out",
                            host.label()
                        )));
                    }
                    thread::sleep(Duration::from_millis(25));
                }
                Err(error) => return Err(MemoryError::io(&path, error)),
            }
        }
        let checked =
            if input.session_id.trim().is_empty() && input.transcript_path.trim().is_empty() {
                None
            } else {
                let key = events::derive_session_key(input);
                Some(directory.join(format!("{:x}.checked", Sha1::digest(key.as_bytes()))))
            };
        Ok(Self {
            host,
            event,
            _lock: lock,
            checked,
        })
    }

    /// The canonical event this session handles.
    pub fn event(&self) -> &CanonicalEvent {
        &self.event
    }

    /// Run the event against the engine; returns the rendered stdout JSON.
    pub fn dispatch(&self, engine: &MemoryEngine, input: &HookInput) -> String {
        self.dispatch_with(
            &EngineHooks::new(engine),
            engine.config().hooks.max_context_chars,
            input,
        )
    }

    /// [`Session::dispatch`] against an injectable [`HookEngine`].
    pub fn dispatch_with(
        &self,
        hooks: &dyn HookEngine,
        max_context_chars: usize,
        input: &HookInput,
    ) -> String {
        let host = self.host;
        let caps = host.caps();
        let read = |path: &str, max_messages: usize| -> Result<Vec<MemoryMessage>> {
            if caps.has_transcript {
                host.read_transcript(path, max_messages)
            } else {
                Ok(Vec::new())
            }
        };
        if let CanonicalEvent::Recall { prompt } = &self.event {
            if !prompt.trim().is_empty() && !self.checked.as_ref().is_some_and(|p| p.is_file()) {
                match flush_pending(hooks, &format!("{}:first-recall", host.id())) {
                    Ok(()) => {
                        if let Some(path) = &self.checked {
                            if let Err(error) = private_file(path) {
                                log::warn!(
                                    "{} recall check could not be saved: {error}",
                                    host.label()
                                );
                            }
                        }
                    }
                    Err(error) => {
                        // Do not mark this session checked: a later prompt retries.
                        log::warn!(
                            "{} first-recall flush failed (ignored): {error}",
                            host.label()
                        );
                    }
                }
            }
        }
        let output = match self.event {
            // No time for model calls: capture only and rely on the next
            // session's first-recall flush.
            CanonicalEvent::End if !caps.end_can_flush() => {
                events::capture_only_reading(hooks, input, &read)
            }
            _ => events::handle_with(hooks, max_context_chars, input, &self.event, &read),
        };
        if self.event == CanonicalEvent::End {
            if caps.end_can_flush() {
                // Generic handling first captured the final transcript. Its
                // scheduled maintenance may already have processed some backlog.
                if let Err(error) = flush_pending(hooks, &format!("{}:session-end", host.id())) {
                    log::warn!(
                        "{} session-end flush failed (ignored): {error}",
                        host.label()
                    );
                }
            }
            if let Some(path) = &self.checked {
                if let Err(error) = fs::remove_file(path) {
                    if error.kind() != std::io::ErrorKind::NotFound {
                        log::warn!("{} session check cleanup failed: {error}", host.label());
                    }
                }
            }
        }
        host.render(&self.event, output)
    }
}

fn private_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .map_err(|error| MemoryError::io(path, error))
}

fn flush_pending(hooks: &dyn HookEngine, reason: &str) -> Result<()> {
    drain_backlog(
        || Ok(hooks.overview()?.pending_dialogue_turns),
        || hooks.flush(reason),
    )
}

/// Flush until nothing is pending; fail on indexing errors or no progress.
fn drain_backlog(
    mut pending_count: impl FnMut() -> Result<usize>,
    mut flush: impl FnMut() -> Result<HeartbeatStats>,
) -> Result<()> {
    let mut pending = pending_count()?;
    while pending > 0 {
        let stats = flush()?;
        if !stats.errors.is_empty() {
            return Err(MemoryError::Other(format!(
                "flush reported {} indexing error(s)",
                stats.errors.len()
            )));
        }
        let remaining = pending_count()?;
        if remaining >= pending {
            return Err(MemoryError::Other("flush made no progress".into()));
        }
        pending = remaining;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hosts::{self, HostCaps};

    #[test]
    fn empty_backlog_does_not_flush() {
        drain_backlog(|| Ok(0), || panic!("empty backlog must not call flush")).unwrap();
    }

    #[test]
    fn failed_or_stalled_flush_does_not_loop_or_report_success() {
        let failed = drain_backlog(|| Ok(1), || Err(MemoryError::Other("offline".into())));
        assert!(failed.is_err());
        let reported = drain_backlog(
            || Ok(1),
            || {
                Ok(HeartbeatStats {
                    errors: vec!["write failed".into()],
                    ..Default::default()
                })
            },
        );
        assert!(reported.unwrap_err().to_string().contains("indexing error"));
        let stalled = drain_backlog(|| Ok(1), || Ok(HeartbeatStats::default()));
        assert!(stalled.unwrap_err().to_string().contains("no progress"));
    }

    #[test]
    fn lock_and_marker_paths_are_stable() {
        let config = MemoryConfig::for_data_root("/data");
        for host in hosts::all() {
            let directory = control_dir(&config, *host);
            assert!(directory.ends_with(Path::new("control").join(host.id())));
            assert!(directory.starts_with(config.store_layout().roots.workspace_dir));
        }
    }

    #[test]
    fn codex_session_end_waits_briefly_for_the_lock() {
        let codex = hosts::by_id("codex").unwrap().caps();
        let workbuddy = hosts::by_id("workbuddy").unwrap().caps();
        let end = hosts::EventKind::End;
        assert!(codex.lock_wait_ms(end) < 3_000);
        assert_eq!(codex.lock_wait_ms(end), 1_500);
        assert_eq!(
            codex.lock_wait_ms(hosts::EventKind::Recall),
            workbuddy.lock_wait_ms(end)
        );
        assert_eq!(workbuddy.lock_wait_ms(end), HostCaps::DEFAULT_LOCK_WAIT_MS);
    }
}
