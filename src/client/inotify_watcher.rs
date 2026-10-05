use mister_save_utils::{SaveFileType, is_hidden_path, log_debug, log_error, log_info, log_warn};
use notify_debouncer_full::{new_debouncer, notify::RecursiveMode};
use std::{
    path::Path,
    time::{Duration, Instant},
};
use tokio::{sync::mpsc, task::JoinHandle};

use crate::{handle_core_change_event, handle_file_event};

/// How often to re-check a watch target that doesn't exist yet.
const PATH_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// How many poll intervals between "still waiting" log lines, so a long
/// boot-time wait doesn't spam the log (roughly every 30s).
const PATH_POLL_LOG_EVERY: u32 = 60;

/// Initial and max delay between attempts to (re)establish a watch once the
/// target exists but `watch()` itself keeps failing or exiting.
const RETRY_BACKOFF_START: Duration = Duration::from_secs(1);
const RETRY_BACKOFF_MAX: Duration = Duration::from_secs(30);
/// A watch that stayed up at least this long before failing is treated as
/// having recovered, so backoff resets instead of staying maxed out forever.
const STABLE_RUN: Duration = Duration::from_secs(60);

pub async fn watch_dirs() {
    let save_types: Vec<SaveFileType> = vec![
        SaveFileType::GameSave,
        SaveFileType::CoreWatch,
        SaveFileType::SaveState,
        SaveFileType::NvRam,
    ];
    let mut handles: Vec<JoinHandle<()>> = Vec::new();

    for save_type in save_types {
        let path = match save_type {
            SaveFileType::GameSave => "/media/fat/saves",
            SaveFileType::SaveState => "/media/fat/savestates",
            SaveFileType::CoreWatch => "/tmp/CORENAME",
            SaveFileType::NvRam => "/media/fat/config/nvram",
        };

        let path_clone: String = path.to_string();
        let handle: JoinHandle<()> = tokio::spawn(async move {
            watch_forever(&path_clone, save_type).await;
        });
        handles.push(handle);
    }

    log_info!("watchers started, waiting for filesystem events");

    // watch_forever never returns - this just keeps the task set alive for
    // the life of the process.
    for handle in handles {
        let _ = handle.await;
    }

    log_error!("all filesystem watchers exited, no further saves will be synced");
}

/// Keeps a single watch alive for the life of the process.
///
/// `watch()` previously ran once: if the target didn't exist yet (routine at
/// boot - /tmp/CORENAME in particular is only written once a core loads,
/// which can race the client starting from user-startup.sh) or the watch was
/// lost for any other reason, that save type was never synced again until
/// the client was restarted. This retries both the initial setup and any
/// later failure, with capped exponential backoff.
async fn watch_forever(path: &str, save_type: SaveFileType) {
    let mut backoff = RETRY_BACKOFF_START;

    loop {
        wait_for_path(path, save_type.clone()).await;

        log_info!("watching {} for {:?} changes", path, save_type);
        let started = Instant::now();
        let result = watch(path, save_type.clone()).await;
        let ran_for = started.elapsed();

        match result {
            Ok(()) => log_warn!(
                "{:?} watcher on {} stopped after {:?} with no error reported",
                save_type,
                path,
                ran_for
            ),
            Err(error) => log_error!(
                "{:?} watcher on {} failed after {:?}: {:?}",
                save_type,
                path,
                ran_for,
                error
            ),
        }

        if ran_for >= STABLE_RUN {
            backoff = RETRY_BACKOFF_START;
        }

        log_warn!(
            "retrying {:?} watch on {} in {:?}",
            save_type,
            path,
            backoff
        );
        tokio::time::sleep(backoff).await;
        backoff = std::cmp::min(backoff * 2, RETRY_BACKOFF_MAX);
    }
}

/// `notify` fails outright (ENOENT) if asked to watch a path that doesn't
/// exist yet. For every target here that is routine at some point: a core
/// may not have loaded yet (so /tmp/CORENAME doesn't exist), or a save
/// category's directory has never been created (a user who never uses
/// save states has no /media/fat/savestates). Poll until it appears instead
/// of giving up.
async fn wait_for_path(path: &str, save_type: SaveFileType) {
    if tokio::fs::try_exists(path).await.unwrap_or(false) {
        return;
    }

    log_warn!(
        "{:?} watch target {} does not exist yet, waiting for it to appear \
         (expected in the first seconds after boot, or if this save type \
         has never been used)",
        save_type,
        path
    );

    let mut attempt: u32 = 0;
    loop {
        tokio::time::sleep(PATH_POLL_INTERVAL).await;
        attempt += 1;

        if tokio::fs::try_exists(path).await.unwrap_or(false) {
            log_info!(
                "{:?} watch target {} appeared after {:?}",
                save_type,
                path,
                PATH_POLL_INTERVAL * attempt
            );
            return;
        }

        if attempt % PATH_POLL_LOG_EVERY == 0 {
            log_warn!(
                "{:?} watch target {} still missing after {:?}",
                save_type,
                path,
                PATH_POLL_INTERVAL * attempt
            );
        }
    }
}

pub async fn watch<P: AsRef<Path>>(path: P, save_type: SaveFileType) -> notify::Result<()> {
    let (tx_blocking, rx_blocking) = std::sync::mpsc::channel();

    let (tx_async, mut rx_async) = mpsc::unbounded_channel();

    let mut debouncer = new_debouncer(Duration::from_millis(2500), None, tx_blocking)?;
    debouncer.watch(path.as_ref(), RecursiveMode::Recursive)?;

    tokio::task::spawn_blocking(move || {
        for result in rx_blocking {
            if tx_async.send(result).is_err() {
                break;
            }
        }
    });

    while let Some(result) = rx_async.recv().await {
        match result {
            Ok(events) => {
                for event in events {
                    log_debug!(
                        "raw {:?} event: {:?} paths={:?}",
                        save_type,
                        event.kind,
                        event.paths
                    );
                    match &event.kind {
                        notify::EventKind::Create(_) => {
                            if save_type == SaveFileType::CoreWatch {
                                continue;
                            }

                            for path in &event.paths {
                                if is_hidden_path(&path) {
                                    continue;
                                }
                                handle_file_event(save_type.clone(), path.clone()).await;
                            }
                        }
                        notify::EventKind::Modify(_) => {
                            if save_type == SaveFileType::CoreWatch {
                                handle_core_change_event().await;
                                continue;
                            }

                            for path in &event.paths {
                                if is_hidden_path(&path) {
                                    continue;
                                }
                                handle_file_event(save_type.clone(), path.clone()).await;
                            }
                        }
                        notify::EventKind::Remove(_) => {
                            for path in &event.paths {
                                if is_hidden_path(&path) {
                                    continue;
                                }
                                log_info!(
                                    "{:?} removed from disk: {} (ignored, removals are not synced)",
                                    save_type,
                                    path.display()
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }
            Err(errors) => {
                for error in errors {
                    log_error!("watch error on {:?}: {:?}", save_type, error);
                }
            }
        }
    }

    Ok(())
}
