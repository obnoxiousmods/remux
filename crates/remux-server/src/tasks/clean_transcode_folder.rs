use anyhow::Result;
use async_trait::async_trait;
#[cfg(unix)]
use libc;
use std::{collections::HashSet, sync::Arc};
use tracing::{info, warn};

use super::{ProgressReporter, Task, TaskCategory, TaskService};
use crate::AppContext;

/// How long a transcode directory is protected from cleanup after creation,
/// covering the window between `create_dir_all` and session registration.
const ORPHAN_GRACE_SECS: u64 = 120;

/// Seconds since the directory was created, or `None` if unavailable.
fn dir_age_secs(path: &std::path::Path) -> Option<u64> {
    let meta = std::fs::metadata(path).ok()?;
    let created = meta
        .created()
        .or_else(|_| meta.modified())
        .ok()?;
    created
        .elapsed()
        .ok()
        .map(|d| d.as_secs())
}

pub struct CleanTranscodeFolderTask;

#[async_trait]
impl Task for CleanTranscodeFolderTask {
    fn key(&self) -> &str {
        "CleanTranscodeFolder"
    }
    fn name(&self) -> &str {
        "Clean Transcode Folder"
    }
    fn description(&self) -> &str {
        "Deletes temporary files left over from transcoding sessions."
    }
    fn short_description(&self) -> &str {
        "Deletes leftover temp transcode files"
    }
    fn category(&self) -> TaskCategory {
        TaskCategory::Maintenance
    }

    async fn run(
        &self,
        ctx: AppContext,
        _tasks: Arc<TaskService>,
        progress: ProgressReporter,
    ) -> Result<()> {
        let active: HashSet<String> = ctx
            .sessions
            .active_session_ids()
            .into_iter()
            .collect();
        let base = ctx
            .sessions
            .base_dir();
        let mut removed = 0usize;

        for entry in super::iter_dir(base) {
            let name = entry
                .file_name()
                .to_string_lossy()
                .into_owned();
            if !active.contains(&name) {
                // A session's directory is created before the session is
                // registered as active, so a dir that is merely young may be a
                // starting session rather than an orphan. Reaping it mid-startup
                // kills the transcode and (previously) got misread as a hardware
                // encoder failure. Leave recent dirs for the next run.
                if dir_age_secs(&entry.path())
                    .is_some_and(|age| age < ORPHAN_GRACE_SECS)
                {
                    continue;
                }
                // Kill any orphaned ffmpeg process before removing the dir.
                #[cfg(unix)]
                if let Ok(pid_str) = std::fs::read_to_string(
                    entry
                        .path()
                        .join(".pid"),
                ) {
                    if let Ok(pid) = pid_str
                        .trim()
                        .parse::<libc::pid_t>()
                    {
                        if pid > 0 {
                            unsafe {
                                libc::kill(pid, libc::SIGCONT);
                                libc::kill(pid, libc::SIGKILL);
                            }
                        }
                    }
                }
                if let Err(e) = std::fs::remove_dir_all(entry.path()) {
                    warn!(
                        "failed to remove transcode dir {}: {e:#}",
                        entry
                            .path()
                            .display()
                    );
                } else {
                    removed += 1;
                }
            }
        }
        info!(removed, "cleaned orphaned transcode dirs");

        progress.set(50.0);

        // Collect torrent IDs currently being streamed by active sessions so we
        // don't pull the rug out from under an in-progress playback.
        let mut active_torrent_ids = HashSet::new();
        for session in ctx
            .sessions
            .get_all()
        {
            if let Some(tc) = session.transcode {
                let input_url = tc
                    .read()
                    .await
                    .input_url
                    .clone();
                if let Some(id) =
                    crate::torrent::TorrentManager::torrent_id_from_url(&input_url)
                {
                    active_torrent_ids.insert(id);
                }
            }
        }

        let deleted = {
            let mgr = &ctx.torrent;
            mgr.delete_unused_with_files(&active_torrent_ids)
                .await
                .unwrap_or_else(|e| {
                    warn!("failed to clean torrents: {e:#}");
                    0
                })
        };
        info!(deleted, "cleaned torrent sessions");

        progress.set(100.0);
        Ok(())
    }
}
