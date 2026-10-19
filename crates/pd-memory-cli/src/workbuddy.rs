//! WorkBuddy-only lifecycle policy. The engine and generic hook contract stay unchanged.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use pd_memory_core::config::MemoryConfig;
use pd_memory_core::engine::{FlushOptions, MemoryEngine};
use pd_memory_core::error::{MemoryError, Result};
use pd_memory_core::types::HeartbeatStats;
use pd_memory_hooks::{HookInput, HookOutput};
use sha1::{Digest, Sha1};

/// Serializes WorkBuddy hooks across processes, including engine construction.
/// Closing the file releases the OS lock even if the host kills the process.
pub struct Session {
    _lock: File,
    checked: Option<PathBuf>,
}

impl Session {
    pub fn lock(config: &MemoryConfig, input: &HookInput) -> Result<Self> {
        let directory = config
            .store_layout()
            .roots
            .workspace_dir
            .join("control/workbuddy");
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
        let path = directory.join("hooks.lock");
        let lock = private_file(&path)?;
        let started = Instant::now();
        loop {
            match fs2::FileExt::try_lock_exclusive(&lock) {
                Ok(()) => break,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if started.elapsed() >= Duration::from_secs(120) {
                        return Err(MemoryError::Other("WorkBuddy hook lock timed out".into()));
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
                let key = pd_memory_hooks::derive_session_key(input);
                Some(directory.join(format!("{:x}.checked", Sha1::digest(key.as_bytes()))))
            };
        Ok(Self {
            _lock: lock,
            checked,
        })
    }

    pub fn dispatch(&self, engine: &MemoryEngine, input: &HookInput) -> Result<HookOutput> {
        if input.event() == "UserPromptSubmit"
            && input
                .prompt
                .as_deref()
                .is_some_and(|p| !p.trim().is_empty())
            && !self.checked.as_ref().is_some_and(|p| p.is_file())
        {
            match flush_pending(engine, "workbuddy:first-recall") {
                Ok(()) => {
                    if let Some(path) = &self.checked {
                        if let Err(error) = private_file(path) {
                            log::warn!("WorkBuddy recall check could not be saved: {error}");
                        }
                    }
                }
                Err(error) => {
                    // Do not mark this session checked: a later prompt retries.
                    log::warn!("WorkBuddy first-recall flush failed (ignored): {error}");
                }
            }
        }
        let output = pd_memory_hooks::dispatch(engine, input)?;
        if input.event() == "SessionEnd" {
            // Generic dispatch first captures the final transcript. Its normal
            // scheduled maintenance may already have processed some backlog.
            if let Err(error) = flush_pending(engine, "workbuddy:session-end") {
                log::warn!("WorkBuddy session-end flush failed (ignored): {error}");
            }
            if let Some(path) = &self.checked {
                if let Err(error) = fs::remove_file(path) {
                    if error.kind() != std::io::ErrorKind::NotFound {
                        log::warn!("WorkBuddy session check cleanup failed: {error}");
                    }
                }
            }
        }
        Ok(output)
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

fn flush_pending(engine: &MemoryEngine, reason: &str) -> Result<()> {
    drain_backlog(
        || Ok(engine.overview()?.pending_dialogue_turns),
        || {
            engine.flush(FlushOptions {
                reason: reason.to_string(),
                ..Default::default()
            })
        },
    )
}

fn drain_backlog(
    mut pending_count: impl FnMut() -> Result<usize>,
    mut flush: impl FnMut() -> Result<HeartbeatStats>,
) -> Result<()> {
    let mut pending = pending_count()?;
    while pending > 0 {
        let stats = flush()?;
        if !stats.errors.is_empty() {
            return Err(MemoryError::Other(format!(
                "WorkBuddy flush reported {} indexing error(s)",
                stats.errors.len()
            )));
        }
        let remaining = pending_count()?;
        if remaining >= pending {
            return Err(MemoryError::Other(
                "WorkBuddy flush made no progress".into(),
            ));
        }
        pending = remaining;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
