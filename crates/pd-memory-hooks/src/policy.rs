//! Host-agnostic lifecycle policy for `pd-memory hook --host <id>`.
//!
//! Every host gets the same three guarantees; [`HostCaps`] tunes them:
//!
//! - **Workspace lock**: hooks run one at a time per workspace across
//!   processes *and hosts* (WorkBuddy and Codex may share one workspace),
//!   including engine construction, so each sees state committed by the
//!   previous capture/flush/recall. The lock is `control/hooks.lock` under the
//!   workspace data directory, plus `control/<id>/hooks.lock` that earlier
//!   per-host binaries took; the OS releases both when the process exits, even
//!   if the host kills it. Wait limits come from [`HostCaps::lock_wait_ms`].
//! - **First-recall barrier**: the first non-empty prompt of a session drains
//!   the indexing backlog before recall, then writes the session marker
//!   `control/<id>/<sha1(session key)>.checked`. A failed drain leaves no
//!   marker, so the next prompt retries. Session end removes the marker, even
//!   when it gives up waiting for the lock.
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
use crate::hosts::{CanonicalEvent, EventKind, HostAdapter};
use crate::HookInput;

/// Workspace control directory: holds the shared `hooks.lock` and one
/// `<host id>/` directory of session markers per host.
pub fn control_dir(config: &MemoryConfig) -> PathBuf {
    config.store_layout().roots.workspace_dir.join("control")
}

/// One hook invocation holding the workspace hook lock.
pub struct Session<'a> {
    host: &'a dyn HostAdapter,
    event: CanonicalEvent,
    _locks: Vec<File>,
    checked: Option<PathBuf>,
}

impl<'a> Session<'a> {
    /// Acquire the hook lock of `config`'s workspace for `host`.
    pub fn lock(
        host: &'a dyn HostAdapter,
        config: &MemoryConfig,
        input: &HookInput,
    ) -> Result<Self> {
        Self::lock_in(host, &control_dir(config), input)
    }

    /// Acquire the hook lock in an explicit workspace control directory:
    /// `<control>/hooks.lock`, markers under `<control>/<host id>/`.
    pub fn lock_in(host: &'a dyn HostAdapter, control: &Path, input: &HookInput) -> Result<Self> {
        let event = host.normalize(input);
        let directory = control.join(host.id());
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&directory)
            .map_err(|error| MemoryError::io(&directory, error))?;
        let checked =
            if input.session_id.trim().is_empty() && input.transcript_path.trim().is_empty() {
                None
            } else {
                let key = events::derive_session_key(input);
                Some(directory.join(format!("{:x}.checked", Sha1::digest(key.as_bytes()))))
            };
        let timeout = Duration::from_millis(host.caps().lock_wait_ms(event.kind()));
        let started = Instant::now();
        // Shared lock first, then the host's own: binaries that predate the
        // shared lock took only `<control>/<host id>/hooks.lock`, so holding
        // both also excludes hooks still running from a previous install.
        let mut locks = Vec::with_capacity(2);
        for path in [control.join("hooks.lock"), directory.join("hooks.lock")] {
            match lock_until(host, &path, started + timeout) {
                Ok(lock) => locks.push(lock),
                Err(error) => {
                    if event.kind() == EventKind::End {
                        // The session is over even though this hook cannot
                        // run: drop its marker so a resumed session flushes
                        // again on its first prompt. Safe without the lock;
                        // only this session's own recall writes the marker.
                        remove_marker(host, checked.as_deref());
                    }
                    return Err(error);
                }
            }
        }
        Ok(Self {
            host,
            event,
            _locks: locks,
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
            remove_marker(host, self.checked.as_deref());
        }
        host.render(&self.event, output)
    }
}

/// Exclusively lock `path`, polling until `deadline`.
fn lock_until(host: &dyn HostAdapter, path: &Path, deadline: Instant) -> Result<File> {
    let lock = private_file(path)?;
    loop {
        match fs2::FileExt::try_lock_exclusive(&lock) {
            Ok(()) => return Ok(lock),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(MemoryError::Other(format!(
                        "{} hook lock timed out",
                        host.label()
                    )));
                }
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(MemoryError::io(path, error)),
        }
    }
}

fn remove_marker(host: &dyn HostAdapter, marker: Option<&Path>) {
    if let Some(path) = marker {
        if let Err(error) = fs::remove_file(path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                log::warn!("{} session check cleanup failed: {error}", host.label());
            }
        }
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

    fn input(event: &str, session: &str) -> HookInput {
        HookInput {
            hook_event_name: event.into(),
            session_id: session.into(),
            ..Default::default()
        }
    }

    #[test]
    fn lock_is_shared_and_markers_are_per_host() {
        let config = MemoryConfig::for_data_root("/data");
        let control = control_dir(&config);
        assert!(control.ends_with("control"));
        assert!(control.starts_with(config.store_layout().roots.workspace_dir));

        let dir = tempfile::tempdir().unwrap();
        for host in hosts::all() {
            let session = Session::lock_in(*host, dir.path(), &input("Stop", "s")).unwrap();
            let marker = session.checked.clone().unwrap();
            assert_eq!(marker.parent().unwrap(), dir.path().join(host.id()));
            assert!(dir.path().join(host.id()).join("hooks.lock").is_file());
        }
        assert!(dir.path().join("hooks.lock").is_file());
    }

    #[test]
    fn per_host_lock_of_earlier_binaries_still_excludes() {
        let dir = tempfile::tempdir().unwrap();
        let codex = hosts::by_id("codex").unwrap();
        // An earlier binary held only `<control>/<id>/hooks.lock`.
        std::fs::create_dir_all(dir.path().join("codex")).unwrap();
        let legacy = private_file(&dir.path().join("codex/hooks.lock")).unwrap();
        fs2::FileExt::try_lock_exclusive(&legacy).unwrap();
        let blocked = Session::lock_in(codex, dir.path(), &input("SessionEnd", "cx"));
        assert!(blocked.is_err());
        drop(legacy);
        Session::lock_in(codex, dir.path(), &input("SessionEnd", "cx")).unwrap();
    }

    #[test]
    fn session_end_that_cannot_lock_still_removes_its_marker() {
        let dir = tempfile::tempdir().unwrap();
        let workbuddy = hosts::by_id("workbuddy").unwrap();
        let codex = hosts::by_id("codex").unwrap();
        let marker = Session::lock_in(codex, dir.path(), &input("Stop", "cx"))
            .unwrap()
            .checked
            .unwrap();
        private_file(&marker).unwrap();
        let other = Session::lock_in(codex, dir.path(), &input("Stop", "other"))
            .unwrap()
            .checked
            .unwrap();
        private_file(&other).unwrap();

        let held = Session::lock_in(workbuddy, dir.path(), &input("Stop", "wb")).unwrap();
        assert!(Session::lock_in(codex, dir.path(), &input("SessionEnd", "cx")).is_err());
        assert!(!marker.exists());
        assert!(
            other.exists(),
            "only the ending session's marker is removed"
        );
        drop(held);
    }

    #[test]
    fn different_hosts_contend_on_the_same_workspace_lock() {
        let dir = tempfile::tempdir().unwrap();
        let workbuddy = hosts::by_id("workbuddy").unwrap();
        let codex = hosts::by_id("codex").unwrap();
        let held = Session::lock_in(workbuddy, dir.path(), &input("Stop", "wb")).unwrap();
        // Codex SessionEnd gives up after its short wait instead of running
        // concurrently with the WorkBuddy hook.
        let started = Instant::now();
        let blocked = Session::lock_in(codex, dir.path(), &input("SessionEnd", "cx"));
        assert!(blocked
            .err()
            .unwrap()
            .to_string()
            .contains("Codex hook lock timed out"));
        assert!(
            started.elapsed()
                >= Duration::from_millis(codex.caps().lock_wait_ms(hosts::EventKind::End))
        );
        drop(held);
        Session::lock_in(codex, dir.path(), &input("SessionEnd", "cx")).unwrap();
    }

    #[test]
    fn codex_session_end_waits_briefly_for_the_lock() {
        let codex = hosts::by_id("codex").unwrap().caps();
        let workbuddy = hosts::by_id("workbuddy").unwrap().caps();
        let end = hosts::EventKind::End;
        assert_eq!(codex.lock_wait_ms(end), 1_000);
        assert_eq!(
            codex.lock_wait_ms(hosts::EventKind::Recall),
            workbuddy.lock_wait_ms(end)
        );
        assert_eq!(workbuddy.lock_wait_ms(end), HostCaps::DEFAULT_LOCK_WAIT_MS);
    }
}
