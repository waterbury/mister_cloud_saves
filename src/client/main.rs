use flate2::{Compression, write::ZlibEncoder};
use glob::glob;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use ini::ini;
use reqwest;
use std::{
    collections::{HashMap, HashSet},
    io::Write,
    path::PathBuf,
    process,
    sync::LazyLock,
    time::Duration,
};
use tokio::sync::Mutex;

use mister_save_utils::logging::{self, fmt_hash, fmt_hash_opt, fmt_index_opt};
use mister_save_utils::{
    ConflictAction, FetchSaveRequest, SaveFile, SaveFileType, UploadSaveRequest, UserSaveData,
    file_stat, fmt_stat_opt, hash_file, hashes_equal, is_hidden_path, log_debug, log_error,
    log_info, log_warn, read_file_to_bytes, zlib_decompress,
};
mod inotify_watcher;
use inotify_watcher::*;

static SERVER_URL: LazyLock<Mutex<String>> = LazyLock::new(|| Mutex::new(String::new()));
static USER_ID: LazyLock<Mutex<String>> = LazyLock::new(|| Mutex::new(String::new()));
static CURRENT_CORE: LazyLock<Mutex<String>> = LazyLock::new(|| Mutex::new(String::new()));
static IS_ONE_SHOT: LazyLock<Mutex<bool>> = LazyLock::new(|| Mutex::new(false));
static SAVE_MAP_PATH: &str = "/media/fat/cloud_saves/mister_save_map.json";
static LOG_PATH: &str = "/media/fat/cloud_saves/cloud_saves.log";

/// Serializes the three places that read-modify-write the save map file
/// (a live filesystem event, a full sync, and a directory rescan) so a slow
/// network retry in one of them can't race another's write and lose an
/// update. Held across the whole read-modify-write cycle of each, including
/// any network calls a sync makes while holding it - a few extra seconds of
/// contention is a much better trade than a corrupted or regressed save map.
static SAVE_MAP_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Shared HTTP client with bounded timeouts. MiSTer's wifi is often slow to
/// associate (on the order of a minute after boot), and a flaky link can
/// leave a request neither completing nor failing - reqwest's default
/// client has no timeout at all, so a single hung connection would stall
/// whichever watcher's event handler is waiting on it indefinitely. A
/// generous but finite timeout turns that into a normal, logged failure
/// that the caller's own retry logic can act on.
static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap_or_else(|e| {
            log_error!(
                "failed to build HTTP client with timeouts, falling back to defaults \
                 (no timeout): {:?}",
                e
            );
            reqwest::Client::new()
        })
});

/// A download that sync decided on, carrying everything needed to explain the
/// decision in the log and to verify the bytes that come back.
struct DownloadTask {
    request: FetchSaveRequest,
    save_key: String,
    /// Hash the server's metadata claims for this save.
    expected_hash: u64,
    /// Hash the save map had for the local copy we are about to replace.
    local_hash: Option<u64>,
    local_index: Option<u64>,
    reason: String,
}

/// An upload that sync decided on, plus the reason for the log.
struct UploadTask {
    request: UploadSaveRequest,
    save_key: String,
    local_hash: u64,
    remote_hash: Option<u64>,
    remote_index: Option<u64>,
    reason: String,
    /// True when the local and remote content hashes are already known to
    /// match (the save map was just refreshed by a scan) and only
    /// modified_index needs to move forward. Lets the upload skip resending
    /// bytes the server already has.
    index_only: bool,
}

/// Outcome of folding an observed file hash into the save map.
struct SaveMapUpdate {
    modified_index: u64,
    previous_hash: Option<u64>,
    previous_index: Option<u64>,
    changed: bool,
}

#[tokio::main]
async fn main() {
    let arg = std::env::args().nth(1);
    if let Some(a) = arg {
        if a.to_lowercase() == "--version" || a.to_lowercase() == "-v" {
            let version: &str = env!("CARGO_PKG_VERSION");
            println!("MiSTer Cloud Saves Client Version v{}", version);
            return;
        } else if a.to_lowercase() == "--one-shot" {
            *IS_ONE_SHOT.lock().await = true;
        }
    }

    let one_shot = *IS_ONE_SHOT.lock().await;

    logging::init("client", Some(LOG_PATH));
    log_info!(
        "==== mister_save_client v{} starting (one_shot={}, pid={}) ====",
        env!("CARGO_PKG_VERSION"),
        one_shot,
        process::id()
    );
    if let Some(path) = logging::log_file_path() {
        log_info!("writing log to {}", path.display());
    }

    create_pid_file().await;
    clean_tmp_files().await;
    read_config().await;
    wait_for_network().await;

    update_save_map().await;
    // wait_for_network() only confirms one health check succeeded; give the
    // startup sync room to retry for close to a minute (2+4+8+16+30s ≈ 60s
    // of backoff across 6 attempts) since MiSTer's wifi is often still
    // settling right after that first success.
    sync_saves_with_retry(6, Duration::from_secs(2), "startup").await;

    if one_shot {
        log_info!("one-shot run finished, exiting");
        return;
    }

    watch_dirs().await;
}

async fn read_config() {
    let config_path = PathBuf::from("/media/fat/cloud_saves.ini");
    if !config_path.exists() {
        log_error!("Config file not found at {:?}", config_path);
        return;
    }

    let config_path_str = match config_path.to_str() {
        Some(s) => s,
        None => {
            log_error!("Failed to convert config path to string");
            return;
        }
    };

    let cloud_saves_ini = ini!(config_path_str);

    let server_map = match cloud_saves_ini.get("server") {
        Some(s) => s,
        None => {
            log_error!("No [server] section in config");
            return;
        }
    };

    let user_map = match cloud_saves_ini.get("user") {
        Some(u) => u,
        None => {
            log_error!("No [user] section in config");
            return;
        }
    };

    let server_url = match server_map.get("server_url") {
        Some(url) => match url {
            Some(u) => u,
            None => {
                log_error!("No server URL specified in config");
                return;
            }
        },
        None => {
            log_error!("No 'server_url' key in [server] section");
            return;
        }
    };

    let user_id = match user_map.get("user_id") {
        Some(id) => match id {
            Some(u) => u,
            None => {
                log_error!("No user ID specified in config");
                return;
            }
        },
        None => {
            log_error!("No 'user_id' key in [user] section");
            return;
        }
    };

    log_info!("config: server_url={} user_id={}", server_url, user_id);

    *SERVER_URL.lock().await = server_url.clone();
    *USER_ID.lock().await = user_id.clone();
}

async fn clean_tmp_files() {
    let pattern = "/media/fat/cloud_saves/tmp/*";
    for entry in glob(pattern).expect("Failed to read glob pattern") {
        match entry {
            Ok(path) => {
                log_info!("removing leftover temp file {}", path.display());
                if let Err(e) = tokio::fs::remove_file(&path).await {
                    log_error!("Failed to remove temp file {:?}: {:?}", path, e);
                }
            }
            Err(e) => log_error!("Glob error: {:?}", e),
        }
    }
}

async fn create_pid_file() {
    if IS_ONE_SHOT.lock().await.clone() == true {
        // No PID file for one-shot runs
        return;
    }

    let pid_path = PathBuf::from("/var/run/mister_save_client.pid");
    let pid = process::id();
    if let Err(e) = tokio::fs::write(&pid_path, pid.to_string()).await {
        log_error!("Failed to write PID file {:?}: {:?}", pid_path, e);
    }
}

async fn wait_for_network() -> bool {
    let server_url = SERVER_URL.lock().await.clone();
    let mut attempts: u64 = 0;
    loop {
        match HTTP_CLIENT
            .get(format!("{}/health", server_url))
            .send()
            .await
        {
            Ok(_) => {
                if attempts > 0 {
                    log_info!(
                        "server {} reachable after {} attempts",
                        server_url,
                        attempts
                    );
                } else {
                    log_info!("server {} reachable", server_url);
                }
                return true;
            }
            Err(e) => {
                attempts += 1;
                // Only log occasionally at warn: this routinely spins for
                // the better part of a minute while MiSTer's wifi
                // associates. The error detail goes to debug every attempt
                // so --log-level debug can show exactly what's failing
                // (DNS, connection refused, timeout, ...) without spamming
                // the normal log.
                log_debug!("waiting for network, attempt {} failed: {:?}", attempts, e);
                if attempts == 1 || attempts % 30 == 0 {
                    log_warn!(
                        "waiting for network, {} not reachable yet (attempt {})",
                        server_url,
                        attempts
                    );
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

pub async fn handle_core_change_event() {
    if CURRENT_CORE.lock().await.as_str() == "MENU" {
        return;
    }

    let core_name_path = PathBuf::from("/tmp/CORENAME");

    let core_name = match tokio::fs::read_to_string(&core_name_path).await {
        Ok(name) => name.trim().to_string(),
        Err(e) => {
            log_error!(
                "Failed to read core name from {:?}: {:?}",
                core_name_path,
                e
            );
            return;
        }
    };

    log_info!(
        "core changed: {} -> {}",
        CURRENT_CORE.lock().await.clone(),
        core_name
    );

    if core_name == "MENU".to_string() {
        log_info!("returned to MENU, rescanning saves and syncing");
        update_save_map().await;
        // A short retry here covers a brief wifi blip; a longer one isn't
        // worth blocking menu navigation for, since the next visit to MENU
        // (or the next local file change) will naturally try again.
        sync_saves_with_retry(3, Duration::from_secs(2), "menu-return").await;
    }

    *CURRENT_CORE.lock().await = core_name;
}

pub async fn handle_file_event(save_type: SaveFileType, path: PathBuf) {
    // Held for the whole read-modify-write below so a concurrent sync can't
    // interleave with it and lose one side's update.
    let _save_map_guard = SAVE_MAP_LOCK.lock().await;

    let save_map_path = PathBuf::from(SAVE_MAP_PATH);

    let mut save_map = match tokio::fs::read_to_string(&save_map_path).await {
        Ok(content) => match serde_json::from_str::<UserSaveData>(&content) {
            Ok(map) => map,
            Err(e) => {
                log_error!("Failed to parse save map JSON: {:?}", e);
                return;
            }
        },
        Err(e) => {
            log_error!("Failed to read save map file: {:?}", e);
            return;
        }
    };

    let save_name = path
        .file_name()
        .map_or("".to_string(), |n| n.to_string_lossy().to_string());

    let core_name = path
        .parent()
        .and_then(|p| p.file_name())
        .map_or("".to_string(), |n| n.to_string_lossy().to_string());

    let file_data = match read_file_to_bytes(&path).await {
        Ok(data) => data,
        Err(e) => {
            log_error!("Failed to read file {:?}: {:?}", path, e);
            return;
        }
    };

    let file_hash = match hash_file(&path, Some(&file_data)).await {
        Ok(hash) => hash,
        Err(e) => {
            log_error!("Failed to hash file {:?}: {:?}", path, e);
            return;
        }
    };

    let save_key = format!("{}/{}", core_name, save_name);
    let stat = file_stat(&path).await;

    log_info!(
        "filesystem event: {:?} {} ({}) hash {}",
        save_type,
        path.display(),
        fmt_stat_opt(stat),
        fmt_hash(file_hash)
    );

    let update: SaveMapUpdate = match save_type {
        SaveFileType::GameSave => insert_save_data(
            &mut save_map.game_saves,
            &save_key,
            &save_name,
            &core_name,
            file_hash,
            SaveFileType::GameSave,
        ),
        SaveFileType::SaveState => insert_save_data(
            &mut save_map.save_states,
            &save_key,
            &save_name,
            &core_name,
            file_hash,
            SaveFileType::SaveState,
        ),
        SaveFileType::NvRam => insert_save_data(
            &mut save_map.nv_ram,
            &save_key,
            &save_name,
            &core_name,
            file_hash,
            SaveFileType::NvRam,
        ),
        SaveFileType::CoreWatch => {
            // Should not happen
            log_warn!("ignoring CoreWatch file event for {}", path.display());
            return;
        }
    };

    let modified_index = update.modified_index;

    if !update.changed {
        log_info!(
            "no content change: {} still hash {} at modified_index {}; \
             the file was touched but the bytes are identical, so nothing claims to be newer; \
             not re-uploading",
            save_key,
            fmt_hash(file_hash),
            modified_index
        );
        // The save map already agrees with what's on disk, so there's
        // nothing to persist and nothing worth sending the server a copy of
        // what it already has.
        return;
    }

    match update.previous_hash {
        Some(previous) => log_info!(
            "local change: {} hash {} -> {}, modified_index {} -> {}; \
             the on-disk content no longer matches the save map, so this machine now \
             claims the newest copy and will upload it",
            save_key,
            fmt_hash(previous),
            fmt_hash(file_hash),
            fmt_index_opt(update.previous_index),
            modified_index
        ),
        None => log_info!(
            "new save: {} hash {} recorded at modified_index {}; not previously in the save map, uploading",
            save_key,
            fmt_hash(file_hash),
            modified_index
        ),
    }

    let server_url = SERVER_URL.lock().await.clone();
    let user_id = USER_ID.lock().await.clone();

    upload_file(
        path,
        save_type,
        modified_index,
        Some(&file_data),
        server_url,
        user_id,
    )
    .await;

    let json_data = match serde_json::to_vec(&save_map) {
        Ok(data) => data,
        Err(e) => {
            log_error!("Failed to serialize save map to JSON: {:?}", e);
            return;
        }
    };

    if let Err(e) = tokio::fs::write(&save_map_path, &json_data).await {
        log_error!(
            "Failed to write save map to file {:?}: {:?}",
            save_map_path,
            e
        );
    }
}

fn insert_save_data(
    saves: &mut HashMap<String, SaveFile>,
    save_key: &str,
    save_name: &str,
    core_name: &str,
    file_hash: u64,
    save_type: SaveFileType,
) -> SaveMapUpdate {
    let previous_hash = saves.get(save_key).map(|s| s.hash);
    let previous_index = saves.get(save_key).map(|s| s.modified_index);

    if saves
        .get(save_key)
        .map_or(false, |s| hashes_equal(s.hash, file_hash))
    {
        // No changes
        return SaveMapUpdate {
            modified_index: saves.get(save_key).map_or(0, |s| s.modified_index),
            previous_hash,
            previous_index,
            changed: false,
        };
    }

    let modified_index = saves.get(save_key).map_or(0, |s| s.modified_index + 1);

    let save_file = SaveFile {
        name: save_name.to_string(),
        save_type,
        core: core_name.to_string(),
        hash: file_hash,
        modified_index,
        user_id: "local".to_string(),
        data: None,
    };

    saves.insert(save_key.to_string(), save_file);

    SaveMapUpdate {
        modified_index,
        previous_hash,
        previous_index,
        changed: true,
    }
}

async fn get_server_data() -> Result<UserSaveData, Box<dyn std::error::Error + Send + Sync>> {
    let server_url = SERVER_URL.lock().await.clone();
    let user_id = USER_ID.lock().await.clone();
    log_info!(
        "fetching save metadata for user {} from {}",
        user_id,
        server_url
    );
    let response = HTTP_CLIENT
        .get(format!("{}/fetch_user_data/{}", server_url, user_id))
        .send()
        .await;

    match response {
        Ok(resp) => {
            if resp.status().is_success() {
                match resp.json::<UserSaveData>().await {
                    Ok(user_data) => {
                        log_info!(
                            "server reports {} game saves, {} save states, {} nvram entries",
                            user_data.game_saves.len(),
                            user_data.save_states.len(),
                            user_data.nv_ram.len()
                        );
                        Ok(user_data)
                    }
                    Err(e) => {
                        log_error!("failed to decode server save metadata: {:?}", e);
                        Err(e.into())
                    }
                }
            } else {
                log_error!("failed to fetch user data: HTTP {}", resp.status());
                Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Failed to fetch user data: HTTP {}", resp.status()),
                )))
            }
        }
        Err(e) => {
            log_error!("failed to reach server for user data: {:?}", e);
            Err(e.into())
        }
    }
}

/// Runs `sync_saves()`, retrying with capped exponential backoff on failure.
///
/// `wait_for_network()` only confirms the server was reachable for one
/// health check; MiSTer's wifi is often still settling down right after
/// that (reassociating, renewing its DHCP lease), so the very next request
/// can still fail. Without this, that single failure would silently skip an
/// entire sync - at startup in particular, that means not noticing saves
/// made on another machine for the rest of the session, since the next
/// chance to catch up is only the next return to MENU.
///
/// `context` names the caller for the log; `max_attempts` and
/// `initial_backoff` size the retry window to how patient that caller can
/// afford to be.
async fn sync_saves_with_retry(
    max_attempts: u32,
    initial_backoff: Duration,
    context: &str,
) -> bool {
    let mut backoff = initial_backoff;

    for attempt in 1..=max_attempts {
        match sync_saves().await {
            Ok(()) => return true,
            Err(e) => {
                if attempt == max_attempts {
                    log_error!(
                        "{}: sync failed on attempt {}/{}, giving up until the next trigger: {:?}",
                        context,
                        attempt,
                        max_attempts,
                        e
                    );
                    return false;
                }

                log_warn!(
                    "{}: sync attempt {}/{} failed, retrying in {:?}: {:?}",
                    context,
                    attempt,
                    max_attempts,
                    backoff,
                    e
                );
                tokio::time::sleep(backoff).await;
                backoff = std::cmp::min(backoff * 2, Duration::from_secs(30));
            }
        }
    }

    false
}

async fn sync_saves() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Held across the network round-trip to the server too: a sync's
    // read-modify-write of the save map isn't done until it writes the
    // reconciled result back out, and letting a live file event interleave
    // partway through would let one side's update silently overwrite the
    // other's.
    let _save_map_guard = SAVE_MAP_LOCK.lock().await;

    log_info!("---- starting save synchronization ----");

    let save_map_path = PathBuf::from(SAVE_MAP_PATH);
    let content = tokio::fs::read_to_string(&save_map_path).await?;
    let mut local_data: UserSaveData = serde_json::from_str(&content)?;
    let remote_data = get_server_data().await?;

    let manage_conflicts = *IS_ONE_SHOT.lock().await;
    let server_url = SERVER_URL.lock().await.clone();
    let user_id = USER_ID.lock().await.clone();

    log_info!(
        "local save map holds {} game saves, {} save states, {} nvram entries (conflict prompts {})",
        local_data.game_saves.len(),
        local_data.save_states.len(),
        local_data.nv_ram.len(),
        if manage_conflicts {
            "enabled"
        } else {
            "disabled, highest modified_index wins"
        }
    );

    let mut download_tasks: Vec<DownloadTask> = Vec::new();
    let mut upload_tasks: Vec<UploadTask> = Vec::new();

    process_category(
        SaveFileType::GameSave,
        &mut local_data.game_saves,
        &remote_data.game_saves,
        manage_conflicts,
        &mut download_tasks,
        &mut upload_tasks,
        server_url.clone(),
        user_id.clone(),
    )
    .await;

    process_category(
        SaveFileType::SaveState,
        &mut local_data.save_states,
        &remote_data.save_states,
        manage_conflicts,
        &mut download_tasks,
        &mut upload_tasks,
        server_url.clone(),
        user_id.clone(),
    )
    .await;

    process_category(
        SaveFileType::NvRam,
        &mut local_data.nv_ram,
        &remote_data.nv_ram,
        manage_conflicts,
        &mut download_tasks,
        &mut upload_tasks,
        server_url.clone(),
        user_id.clone(),
    )
    .await;

    let total_tasks = download_tasks.len() + upload_tasks.len();

    log_info!(
        "sync plan: {} download(s), {} upload(s)",
        download_tasks.len(),
        upload_tasks.len()
    );

    let mp = MultiProgress::new();
    let total_pb = if total_tasks > 0 && manage_conflicts {
        mp.add(ProgressBar::new(total_tasks as u64))
    } else {
        ProgressBar::hidden()
    };

    let download_pb: ProgressBar = if download_tasks.len() > 0 && manage_conflicts {
        mp.add(ProgressBar::new(download_tasks.len() as u64))
    } else {
        ProgressBar::hidden()
    };

    let upload_pb: ProgressBar = if upload_tasks.len() > 0 && manage_conflicts {
        mp.add(ProgressBar::new(upload_tasks.len() as u64))
    } else {
        ProgressBar::hidden()
    };

    let style =
        ProgressStyle::with_template("{prefix:<10} [{bar:40.cyan/blue}] {pos}/{len} ({percent}%)")?;

    total_pb.set_style(style.clone());
    download_pb.set_style(style.clone());
    upload_pb.set_style(style.clone());

    total_pb.set_prefix("Total");
    download_pb.set_prefix("Download");
    upload_pb.set_prefix("Upload");

    for download in download_tasks {
        if let Err(e) = fetch_save_file(&download).await {
            log_error!("download of {} failed: {:?}", download.save_key, e);
        }
        download_pb.inc(1);
        total_pb.inc(1);
    }

    for upload in upload_tasks {
        log_info!(
            "uploading {}: local hash {} (modified_index {}) replacing server hash {} (modified_index {}); reason: {}{}",
            upload.save_key,
            fmt_hash(upload.local_hash),
            upload.request.modified_index,
            fmt_hash_opt(upload.remote_hash),
            fmt_index_opt(upload.remote_index),
            upload.reason,
            if upload.index_only {
                "; hashes already match, sending modified_index only"
            } else {
                ""
            }
        );

        if upload.index_only {
            upload_index_only(
                upload.request.path,
                upload.request.save_type,
                upload.request.modified_index,
                upload.local_hash,
                upload.request.server_url,
                upload.request.user_id,
            )
            .await;
        } else {
            upload_file(
                upload.request.path,
                upload.request.save_type,
                upload.request.modified_index,
                None,
                upload.request.server_url,
                upload.request.user_id,
            )
            .await;
        }

        upload_pb.inc(1);
        total_pb.inc(1);
    }

    download_pb.finish();
    upload_pb.finish();
    total_pb.finish_with_message("Sync complete!");

    let json_data = serde_json::to_vec(&local_data)?;
    tokio::fs::write(&save_map_path, &json_data).await?;

    log_info!("---- save synchronization complete ----");
    Ok(())
}

async fn process_category(
    save_type: SaveFileType,
    local_saves: &mut HashMap<String, SaveFile>,
    remote_saves: &HashMap<String, SaveFile>,
    manage_conflicts: bool,
    download_tasks: &mut Vec<DownloadTask>,
    upload_tasks: &mut Vec<UploadTask>,
    server_url: String,
    user_id: String,
) {
    let mut conflict_state = ConflictAction::AskUser;

    let all_keys: HashSet<String> = local_saves
        .keys()
        .chain(remote_saves.keys())
        .cloned()
        .collect();

    log_debug!(
        "comparing {:?}: {} local, {} remote, {} distinct keys",
        save_type,
        local_saves.len(),
        remote_saves.len(),
        all_keys.len()
    );

    for key in all_keys {
        let local_entry = local_saves.get(&key);
        let remote_entry = remote_saves.get(&key);

        match (local_entry, remote_entry) {
            (Some(local), None) => {
                let local_hash = local.hash;
                let local_index = local.modified_index;
                log_info!(
                    "{:?} {}: local only (hash {}, modified_index {}), server has no copy -> upload",
                    save_type,
                    key,
                    fmt_hash(local_hash),
                    local_index
                );
                queue_upload(
                    upload_tasks,
                    key.clone(),
                    save_type.clone(),
                    local_index,
                    local_hash,
                    None,
                    None,
                    "save exists locally but not on the server".to_string(),
                    server_url.clone(),
                    user_id.clone(),
                    false,
                );
            }

            (None, Some(remote)) => {
                log_info!(
                    "{:?} {}: server only (hash {}, modified_index {}), no local copy -> download",
                    save_type,
                    key,
                    fmt_hash(remote.hash),
                    remote.modified_index
                );
                let remote = remote.clone();
                local_saves.insert(key.clone(), remote.clone());
                queue_download(
                    download_tasks,
                    key.clone(),
                    remote,
                    save_type.clone(),
                    None,
                    None,
                    "save exists on the server but not locally".to_string(),
                );
            }

            (Some(local), Some(remote)) => {
                if hashes_equal(local.hash, remote.hash) {
                    if local.modified_index < remote.modified_index {
                        log_info!(
                            "{:?} {}: content identical (hash {}), adopting remote modified_index {} (was {}); no file touched",
                            save_type,
                            key,
                            fmt_hash(local.hash),
                            remote.modified_index,
                            local.modified_index
                        );
                        // Update local modified index to match remote
                        let remote_index = remote.modified_index;
                        if let Some(l_mut) = local_saves.get_mut(&key) {
                            l_mut.modified_index = remote_index;
                        }
                    } else if local.modified_index > remote.modified_index {
                        log_info!(
                            "{:?} {}: content identical (hash {}) but local modified_index {} > remote {}; re-uploading to settle the index",
                            save_type,
                            key,
                            fmt_hash(local.hash),
                            local.modified_index,
                            remote.modified_index
                        );
                        let local_hash = local.hash;
                        let local_index = local.modified_index;
                        let remote_hash = remote.hash;
                        let remote_index = remote.modified_index;
                        queue_upload(
                            upload_tasks,
                            key.clone(),
                            save_type.clone(),
                            local_index,
                            local_hash,
                            Some(remote_hash),
                            Some(remote_index),
                            "content identical, local modified_index is ahead".to_string(),
                            server_url.clone(),
                            user_id.clone(),
                            true,
                        );
                    } else {
                        log_debug!(
                            "{:?} {}: in sync (hash {}, modified_index {})",
                            save_type,
                            key,
                            fmt_hash(local.hash),
                            local.modified_index
                        );
                    }

                    continue; // Synced
                }

                let local_is_newer = local.modified_index > remote.modified_index;

                if local.modified_index == remote.modified_index {
                    log_warn!(
                        "{:?} {}: DIVERGENT - local hash {} and server hash {} differ but both sit at modified_index {}. \
                         The two copies changed independently and the index cannot break the tie; the server copy will win and the local changes will be lost.",
                        save_type,
                        key,
                        fmt_hash(local.hash),
                        fmt_hash(remote.hash),
                        local.modified_index
                    );
                } else {
                    log_info!(
                        "{:?} {}: content differs - local hash {} (modified_index {}) vs server hash {} (modified_index {}); {} copy is newer",
                        save_type,
                        key,
                        fmt_hash(local.hash),
                        local.modified_index,
                        fmt_hash(remote.hash),
                        remote.modified_index,
                        if local_is_newer { "local" } else { "server" }
                    );
                }

                if !manage_conflicts {
                    if local_is_newer {
                        let local_hash = local.hash;
                        let local_index = local.modified_index;
                        let remote_hash = remote.hash;
                        let remote_index = remote.modified_index;
                        queue_upload(
                            upload_tasks,
                            key.clone(),
                            save_type.clone(),
                            local_index,
                            local_hash,
                            Some(remote_hash),
                            Some(remote_index),
                            format!(
                                "local modified_index {} > server {}",
                                local_index, remote_index
                            ),
                            server_url.clone(),
                            user_id.clone(),
                            false,
                        );
                    } else {
                        let local_hash = local.hash;
                        let local_index = local.modified_index;
                        let remote = remote.clone();
                        let reason = format!(
                            "server modified_index {} >= local {}",
                            remote.modified_index, local_index
                        );
                        local_saves.insert(key.clone(), remote.clone());
                        queue_download(
                            download_tasks,
                            key.clone(),
                            remote,
                            save_type.clone(),
                            Some(local_hash),
                            Some(local_index),
                            reason,
                        );
                    }
                    continue;
                }

                let (primary, secondary) = if local_is_newer {
                    (local, remote)
                } else {
                    (remote, local)
                };

                if conflict_state != ConflictAction::KeepLocalAll
                    && conflict_state != ConflictAction::KeepRemoteAll
                {
                    conflict_state = prompt_user_conflict(primary, secondary, local_is_newer);
                }

                log_info!(
                    "{:?} {}: conflict resolution is {:?}",
                    save_type,
                    key,
                    conflict_state
                );

                match conflict_state {
                    ConflictAction::KeepLocal | ConflictAction::KeepLocalAll => {
                        let local_hash = local.hash;
                        let local_index = local.modified_index;
                        let remote_hash = remote.hash;
                        let remote_index = remote.modified_index;

                        if local_is_newer {
                            // Standard upload
                            queue_upload(
                                upload_tasks,
                                key.clone(),
                                save_type.clone(),
                                local_index,
                                local_hash,
                                Some(remote_hash),
                                Some(remote_index),
                                "user kept the local copy, which was already newer".to_string(),
                                server_url.clone(),
                                user_id.clone(),
                                false,
                            );
                        } else {
                            // Force Local: Remote is newer, but we want local.
                            // Bump local index to Remote + 1 so next sync other clients accepts it.
                            let new_idx = remote_index + 1;
                            log_warn!(
                                "{:?} {}: forcing local copy (hash {}) over newer server copy (hash {}); bumping modified_index {} -> {} so other machines accept it",
                                save_type,
                                key,
                                fmt_hash(local_hash),
                                fmt_hash(remote_hash),
                                local_index,
                                new_idx
                            );
                            if let Some(l_mut) = local_saves.get_mut(&key) {
                                l_mut.modified_index = new_idx;
                            }
                            queue_upload(
                                upload_tasks,
                                key.clone(),
                                save_type.clone(),
                                new_idx,
                                local_hash,
                                Some(remote_hash),
                                Some(remote_index),
                                "user kept the local copy over a newer server copy".to_string(),
                                server_url.clone(),
                                user_id.clone(),
                                false,
                            );
                        }
                    }
                    ConflictAction::KeepRemote | ConflictAction::KeepRemoteAll => {
                        // Standard download (overwrites local entry in map)
                        let local_hash = local.hash;
                        let local_index = local.modified_index;
                        let remote = remote.clone();
                        local_saves.insert(key.clone(), remote.clone());
                        queue_download(
                            download_tasks,
                            key.clone(),
                            remote,
                            save_type.clone(),
                            Some(local_hash),
                            Some(local_index),
                            "user kept the server copy".to_string(),
                        );
                    }
                    _ => {} // Should not happen given logic above
                }
            }
            (None, None) => unreachable!(),
        }
    }
}

fn prompt_user_conflict(
    newer: &SaveFile,
    older: &SaveFile,
    local_is_newer: bool,
) -> ConflictAction {
    println!("Conflict: {}/{}", newer.core, newer.name);
    println!(
        "  Newer ({}): Index {} Hash {}",
        if local_is_newer { "Local" } else { "Remote" },
        newer.modified_index,
        fmt_hash(newer.hash)
    );
    println!(
        "  Older ({}): Index {} Hash {}",
        if local_is_newer { "Remote" } else { "Local" },
        older.modified_index,
        fmt_hash(older.hash)
    );
    println!("Action: (L)ocal, (R)emote, (LALL) Local All, (RALL) Remote All, (A)bort");

    // Falling back to the non-interactive rule when there is nobody to ask.
    // Returning a per-file decision rather than a "...All" one keeps each
    // remaining conflict resolved on its own merits.
    let unattended = || {
        if local_is_newer {
            ConflictAction::KeepLocal
        } else {
            ConflictAction::KeepRemote
        }
    };

    loop {
        print!("> ");
        match std::io::stdout().flush() {
            Ok(_) => {}
            Err(_) => {
                println!("Failed to flush stdout.");
                continue;
            }
        };

        let mut input = String::new();
        match std::io::stdin().read_line(&mut input) {
            // End of input: there is no terminal to answer the prompt, so
            // looping would spin forever and sync nothing.
            Ok(0) => {
                log_warn!(
                    "conflict prompt for {}/{} hit end of input (no interactive stdin); \
                     falling back to the non-interactive rule, so the {} copy wins on \
                     modified_index {} vs {}",
                    newer.core,
                    newer.name,
                    if local_is_newer { "local" } else { "server" },
                    newer.modified_index,
                    older.modified_index
                );
                return unattended();
            }
            Ok(_) => {}
            Err(e) => {
                log_warn!(
                    "conflict prompt for {}/{} could not read stdin ({:?}); falling back to \
                     the non-interactive rule",
                    newer.core,
                    newer.name,
                    e
                );
                return unattended();
            }
        }

        return match input.trim().to_uppercase().as_str() {
            "L" => ConflictAction::KeepLocal,
            "R" => ConflictAction::KeepRemote,
            "LALL" => ConflictAction::KeepLocalAll,
            "RALL" => ConflictAction::KeepRemoteAll,
            "A" => {
                log_info!("user aborted sync at conflict prompt");
                println!("Aborting sync.");
                std::process::exit(0);
            }
            _ => {
                println!("Invalid input.");
                continue;
            }
        };
    }
}

fn queue_upload(
    upload_tasks: &mut Vec<UploadTask>,
    path: String,
    save_type: SaveFileType,
    index: u64,
    local_hash: u64,
    remote_hash: Option<u64>,
    remote_index: Option<u64>,
    reason: String,
    server_url: String,
    user_id: String,
    index_only: bool,
) {
    upload_tasks.push(UploadTask {
        save_key: path.clone(),
        local_hash,
        remote_hash,
        remote_index,
        reason,
        index_only,
        request: UploadSaveRequest {
            path: PathBuf::from(path),
            save_type,
            modified_index: index,
            server_url,
            user_id,
        },
    });
}

fn queue_download(
    download_tasks: &mut Vec<DownloadTask>,
    save_key: String,
    remote: SaveFile,
    save_type: SaveFileType,
    local_hash: Option<u64>,
    local_index: Option<u64>,
    reason: String,
) {
    let expected_hash = remote.hash;
    let req = FetchSaveRequest {
        user_id: remote.user_id,
        core: remote.core,
        name: remote.name,
        save_type,
        modified_index: remote.modified_index,
    };
    download_tasks.push(DownloadTask {
        request: req,
        save_key,
        expected_hash,
        local_hash,
        local_index,
        reason,
    });
}

async fn fetch_save_file(
    task: &DownloadTask,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let request = &task.request;
    let server_url = SERVER_URL.lock().await.clone();

    let save_folder = match request.save_type {
        SaveFileType::GameSave => "saves",
        SaveFileType::SaveState => "savestates",
        SaveFileType::NvRam => "config",
        _ => {
            log_error!(
                "refusing to download {}: unsupported save type {:?}",
                task.save_key,
                request.save_type
            );
            return Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Invalid save type",
            )));
        }
    };

    let base_dir = PathBuf::from("/media/fat");
    let save_dir = base_dir.join(save_folder).join(&request.core);
    let final_save_path = save_dir.join(&request.name);

    // Hash whatever is on disk right now, so the log records exactly what this
    // download is about to replace.
    let before_stat = file_stat(&final_save_path).await;
    let before_hash = match before_stat {
        Some(_) => hash_file(&final_save_path, None).await.ok(),
        None => None,
    };

    log_info!(
        "downloading {}: replacing {} [on disk: {}, hash {}] with server copy \
         [expected hash {}, modified_index {}]; reason: {}",
        task.save_key,
        final_save_path.display(),
        fmt_stat_opt(before_stat),
        fmt_hash_opt(before_hash),
        fmt_hash(task.expected_hash),
        request.modified_index,
        task.reason
    );

    if let (Some(disk), Some(map)) = (before_hash, task.local_hash) {
        if !hashes_equal(disk, map) {
            log_warn!(
                "{}: on-disk hash {} does not match the save map's hash {} (modified_index {}). \
                 Something changed this file without the client noticing, and those changes are \
                 about to be overwritten.",
                task.save_key,
                fmt_hash(disk),
                fmt_hash(map),
                fmt_index_opt(task.local_index)
            );
        }
    }

    let response = HTTP_CLIENT
        .post(&format!("{}/fetch_save", server_url))
        .json(request)
        .send()
        .await;

    match response {
        Ok(resp) => {
            if resp.status().is_success() {
                let bytes = resp.bytes().await?;

                let decompressed_data = match zlib_decompress(&bytes) {
                    Ok(data) => data,
                    Err(e) => {
                        log_error!(
                            "{}: failed to decompress {} bytes from the server, leaving {} untouched: {:?}",
                            task.save_key,
                            bytes.len(),
                            final_save_path.display(),
                            e
                        );
                        return Err(e.into());
                    }
                };

                let downloaded_hash = mister_save_utils::hash_bytes(&decompressed_data);

                if !hashes_equal(downloaded_hash, task.expected_hash) {
                    log_warn!(
                        "{}: downloaded content hash {} does not match the hash {} the server's \
                         metadata advertised ({} bytes compressed, {} bytes decompressed)",
                        task.save_key,
                        fmt_hash(downloaded_hash),
                        fmt_hash(task.expected_hash),
                        bytes.len(),
                        decompressed_data.len()
                    );
                }

                if before_hash.map_or(false, |h| hashes_equal(h, downloaded_hash)) {
                    log_info!(
                        "{}: downloaded content is byte-identical to the local file (hash {}); \
                         writing it anyway, which will update the file's mtime",
                        task.save_key,
                        fmt_hash(downloaded_hash)
                    );
                }

                // Create save directory if it doesn't exist
                tokio::fs::create_dir_all(&save_dir).await?;

                // Write to temp file first
                let temp_dir = base_dir.join("cloud_saves/tmp");
                tokio::fs::create_dir_all(&temp_dir).await?;
                let temp_save_path = temp_dir.join(request.name.clone());

                tokio::fs::write(&temp_save_path, &decompressed_data).await?;

                // Move temp file to final location
                tokio::fs::rename(&temp_save_path, &final_save_path).await?;

                // Re-read the file we just installed: this is the "after" hash,
                // and it catches truncated or partially flushed writes.
                let after_stat = file_stat(&final_save_path).await;
                let after_hash = hash_file(&final_save_path, None).await.ok();

                if after_hash.map_or(true, |h| !hashes_equal(h, downloaded_hash)) {
                    log_error!(
                        "{}: verification FAILED after writing {} - expected hash {}, file now \
                         hashes {} ({})",
                        task.save_key,
                        final_save_path.display(),
                        fmt_hash(downloaded_hash),
                        fmt_hash_opt(after_hash),
                        fmt_stat_opt(after_stat)
                    );
                } else {
                    log_info!(
                        "{}: wrote {} - hash {} -> {} ({}), now at modified_index {}",
                        task.save_key,
                        final_save_path.display(),
                        fmt_hash_opt(before_hash),
                        fmt_hash(downloaded_hash),
                        fmt_stat_opt(after_stat),
                        request.modified_index
                    );
                }

                Ok(())
            } else {
                log_error!(
                    "{}: server refused to send {} ({:?}): HTTP {}; {} left untouched",
                    task.save_key,
                    request.name,
                    request.save_type,
                    resp.status(),
                    final_save_path.display()
                );
                Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Failed to fetch save file: HTTP {}", resp.status()),
                )))
            }
        }
        Err(e) => {
            log_error!(
                "{}: download request failed, {} left untouched: {:?}",
                task.save_key,
                final_save_path.display(),
                e
            );
            Err(e.into())
        }
    }
}

async fn upload_file(
    path: PathBuf,
    save_type: SaveFileType,
    modified_index: u64,
    data: Option<&[u8]>,
    server_url: String,
    user_id: String,
) {
    let base_dir = match save_type {
        SaveFileType::GameSave => PathBuf::from("/media/fat/saves"),
        SaveFileType::SaveState => PathBuf::from("/media/fat/savestates"),
        SaveFileType::NvRam => PathBuf::from("/media/fat/config"),
        _ => {
            log_error!("Unsupported save type for upload: {:?}", save_type);
            return;
        }
    };

    let full_path = if path.is_absolute() {
        path.clone()
    } else {
        base_dir.join(&path)
    };

    let data = match data {
        Some(d) => d.to_vec(),
        None => match read_file_to_bytes(&full_path).await {
            Ok(bytes) => bytes,
            Err(e) => {
                log_error!(
                    "Failed to read file {:?} for upload, nothing sent: {:?}",
                    full_path,
                    e
                );
                return;
            }
        },
    };

    let mut zencode: ZlibEncoder<Vec<u8>> = ZlibEncoder::new(Vec::new(), Compression::default());

    if let Err(e) = zencode.write_all(&data) {
        log_error!("Failed to compress file upload {:?}: {:?}", full_path, e);
        return;
    }

    let compressed_data = match zencode.finish() {
        Ok(data) => data,
        Err(e) => {
            log_error!(
                "Failed to finish compression for file {:?}: {:?}",
                full_path,
                e
            );
            return;
        }
    };

    let file_name = match full_path.file_name() {
        Some(name) => name.to_string_lossy().to_string(),
        None => {
            log_error!("Failed to get file name for {:?}", full_path);
            return;
        }
    };

    let core = match full_path.parent().and_then(|p| p.file_name()) {
        Some(core_name) => core_name.to_string_lossy().to_string(),
        None => {
            log_error!("Failed to get core name for {:?}", full_path);
            return;
        }
    };

    let file_hash = match hash_file(&full_path, Some(&data)).await {
        Ok(hash) => hash,
        Err(e) => {
            log_error!("Failed to hash file upload {:?}: {:?}", full_path, e);
            return;
        }
    };

    let stat = file_stat(&full_path).await;

    log_info!(
        "POST /upload_save {}/{} ({:?}) from {} [{}]: hash {}, {} bytes raw, {} bytes compressed, modified_index {}",
        core,
        file_name,
        save_type,
        full_path.display(),
        fmt_stat_opt(stat),
        fmt_hash(file_hash),
        data.len(),
        compressed_data.len(),
        modified_index
    );

    let save_file = SaveFile {
        name: file_name.clone(),
        save_type,
        core: core.clone(),
        hash: file_hash,
        user_id: user_id.clone(),
        data: Some(compressed_data),
        modified_index,
    };

    match HTTP_CLIENT
        .post(&format!("{}/upload_save/{}", server_url, user_id))
        .json(&save_file)
        .send()
        .await
    {
        Ok(resp) => {
            if resp.status().is_success() {
                log_info!(
                    "server accepted {}/{} at hash {} modified_index {}",
                    core,
                    file_name,
                    fmt_hash(file_hash),
                    modified_index
                );
            } else {
                log_error!(
                    "Failed to upload save file {:?}: HTTP {}",
                    full_path,
                    resp.status()
                );
            }
        }
        Err(e) => {
            log_error!("Failed to upload save file {:?}: {:?}", full_path, e);
        }
    }
}

/// Moves a save's modified_index forward on the server without resending its
/// bytes. Only used when the save map (just refreshed by a scan) already
/// shows the local and remote content hashes matching - the server has the
/// right bytes already and only needs to know this machine has caught up to
/// that content at a given index.
///
/// The server independently re-checks the claimed hash against what it has
/// stored before accepting this, so a stale or wrong local hash can't move
/// the index without the matching content. If the server rejects it (either
/// because the hashes don't actually match, or because it's running an older
/// version that doesn't support index-only updates), this falls back to a
/// normal full upload.
async fn upload_index_only(
    path: PathBuf,
    save_type: SaveFileType,
    modified_index: u64,
    hash: u64,
    server_url: String,
    user_id: String,
) {
    let base_dir = match save_type {
        SaveFileType::GameSave => PathBuf::from("/media/fat/saves"),
        SaveFileType::SaveState => PathBuf::from("/media/fat/savestates"),
        SaveFileType::NvRam => PathBuf::from("/media/fat/config"),
        _ => {
            log_error!(
                "Unsupported save type for index-only upload: {:?}",
                save_type
            );
            return;
        }
    };

    let full_path = if path.is_absolute() {
        path.clone()
    } else {
        base_dir.join(&path)
    };

    let file_name = match full_path.file_name() {
        Some(name) => name.to_string_lossy().to_string(),
        None => {
            log_error!("Failed to get file name for {:?}", full_path);
            return;
        }
    };

    let core = match full_path.parent().and_then(|p| p.file_name()) {
        Some(core_name) => core_name.to_string_lossy().to_string(),
        None => {
            log_error!("Failed to get core name for {:?}", full_path);
            return;
        }
    };

    log_info!(
        "POST /upload_save {}/{} ({:?}) index-only: hash {} unchanged, modified_index -> {}; no file bytes sent",
        core,
        file_name,
        save_type,
        fmt_hash(hash),
        modified_index
    );

    let save_file = SaveFile {
        name: file_name.clone(),
        save_type: save_type.clone(),
        core: core.clone(),
        hash,
        user_id: user_id.clone(),
        data: None,
        modified_index,
    };

    match HTTP_CLIENT
        .post(&format!("{}/upload_save/{}", server_url, user_id))
        .json(&save_file)
        .send()
        .await
    {
        Ok(resp) => {
            if resp.status().is_success() {
                log_info!(
                    "server accepted index-only update for {}/{} at hash {} modified_index {}",
                    core,
                    file_name,
                    fmt_hash(hash),
                    modified_index
                );
            } else {
                log_warn!(
                    "server rejected index-only update for {}/{}: HTTP {}; falling back to a full upload",
                    core,
                    file_name,
                    resp.status()
                );
                upload_file(path, save_type, modified_index, None, server_url, user_id).await;
            }
        }
        Err(e) => {
            log_error!(
                "index-only update request failed for {:?}, nothing sent: {:?}",
                full_path,
                e
            );
        }
    }
}

pub async fn update_save_map() {
    // See the lock in handle_file_event for why this is held across the
    // whole rescan, not just its final write.
    let _save_map_guard = SAVE_MAP_LOCK.lock().await;

    let save_types: Vec<SaveFileType> = vec![
        SaveFileType::GameSave,
        SaveFileType::SaveState,
        SaveFileType::NvRam,
    ];
    let save_map_path = PathBuf::from(SAVE_MAP_PATH);

    log_info!(
        "rescanning local save directories against {}",
        SAVE_MAP_PATH
    );

    let mut result: UserSaveData = UserSaveData::default();
    let mut saves: HashMap<String, SaveFile> = HashMap::new();
    let mut save_states: HashMap<String, SaveFile> = HashMap::new();
    let mut nv_rams: HashMap<String, SaveFile> = HashMap::new();

    let mut existing_map: UserSaveData = tokio::fs::read_to_string(&save_map_path)
        .await
        .ok()
        .and_then(|content| serde_json::from_str::<UserSaveData>(&content).ok())
        .unwrap_or_default();

    // path of each scanned file, so a change can be reported with its mtime
    let mut scanned_paths: HashMap<String, PathBuf> = HashMap::new();

    for save_type in save_types {
        let saves_path = match save_type {
            SaveFileType::GameSave => "/media/fat/saves",
            SaveFileType::SaveState => "/media/fat/savestates",
            SaveFileType::NvRam => "/media/fat/config/nvram",
            SaveFileType::CoreWatch => continue, // skip
        };

        let save_files = match glob(&format!("{}/**/*", saves_path)) {
            Ok(paths) => paths,
            Err(e) => {
                log_error!("Failed to read glob pattern: {:?}", e);
                continue;
            }
        };

        for entry in save_files {
            if let Ok(path) = entry {
                if is_hidden_path(&path) {
                    continue;
                }

                if path.is_file() {
                    let file_name = path
                        .file_name()
                        .map_or("".to_string(), |n| n.to_string_lossy().to_string());
                    let core_name = path
                        .parent()
                        .and_then(|p| p.file_name())
                        .map_or("".to_string(), |n| n.to_string_lossy().to_string());
                    let save_key = format!("{}/{}", core_name, file_name);

                    let file_hash = match hash_file(&path, None).await {
                        Ok(h) => h,
                        Err(e) => {
                            log_error!("Failed to hash file {:?}: {:?}", path, e);
                            continue;
                        }
                    };

                    let save_file = SaveFile {
                        name: file_name.clone(),
                        save_type: save_type.clone(),
                        core: core_name.clone(),
                        hash: file_hash,
                        modified_index: 0,
                        user_id: "local".to_string(),
                        data: None,
                    };

                    scanned_paths.insert(save_key.clone(), path.clone());

                    match save_type {
                        SaveFileType::GameSave => {
                            saves.insert(save_key, save_file);
                        }
                        SaveFileType::SaveState => {
                            save_states.insert(save_key, save_file);
                        }
                        SaveFileType::NvRam => {
                            nv_rams.insert(save_key, save_file);
                        }
                        _ => {}
                    }
                }
            }
        }

        match save_type {
            SaveFileType::GameSave => {
                update_save_files_from(
                    SaveFileType::GameSave,
                    &saves,
                    &mut existing_map.game_saves,
                    &scanned_paths,
                )
                .await;
            }
            SaveFileType::SaveState => {
                update_save_files_from(
                    SaveFileType::SaveState,
                    &save_states,
                    &mut existing_map.save_states,
                    &scanned_paths,
                )
                .await;
            }
            SaveFileType::NvRam => {
                update_save_files_from(
                    SaveFileType::NvRam,
                    &nv_rams,
                    &mut existing_map.nv_ram,
                    &scanned_paths,
                )
                .await;
            }
            _ => {}
        }
    }

    result.game_saves = existing_map.game_saves;
    result.save_states = existing_map.save_states;
    result.nv_ram = existing_map.nv_ram;

    log_info!(
        "rescan complete: {} game saves, {} save states, {} nvram entries in the save map",
        result.game_saves.len(),
        result.save_states.len(),
        result.nv_ram.len()
    );

    if let Ok(json_data) = serde_json::to_vec(&result) {
        if let Err(e) = tokio::fs::write(&save_map_path, &json_data).await {
            log_error!("Failed to write save map: {:?}", e);
        }
    } else {
        log_error!("Failed to serialize save map");
    }
}

async fn update_save_files_from(
    save_type: SaveFileType,
    new_saves: &HashMap<String, SaveFile>,
    existing_map: &mut HashMap<String, SaveFile>,
    scanned_paths: &HashMap<String, PathBuf>,
) {
    for (k, v) in new_saves.iter() {
        if !existing_map.contains_key(k) {
            log_info!(
                "{:?} {}: new file discovered during rescan, hash {} ({}), starting at modified_index {}",
                save_type,
                k,
                fmt_hash(v.hash),
                scanned_stat(scanned_paths, k).await,
                v.modified_index
            );
            existing_map.insert(k.clone(), v.clone());
        } else {
            // Update hash if changed
            let existing_save = existing_map.get_mut(k).unwrap();
            if existing_save.hash != v.hash {
                let previous_hash = existing_save.hash;
                let previous_index = existing_save.modified_index;
                existing_save.hash = v.hash;
                existing_save.modified_index += 1;

                log_info!(
                    "{:?} {}: content changed while the client was not watching - hash {} -> {} ({}), \
                     modified_index {} -> {}. This machine will now be treated as holding the newest copy.",
                    save_type,
                    k,
                    fmt_hash(previous_hash),
                    fmt_hash(v.hash),
                    scanned_stat(scanned_paths, k).await,
                    previous_index,
                    existing_save.modified_index
                );
            }
        }
    }
    // Remove entries no longer present
    let removed: Vec<String> = existing_map
        .keys()
        .filter(|k| !new_saves.contains_key(*k))
        .cloned()
        .collect();

    for key in removed {
        if let Some(entry) = existing_map.get(&key) {
            log_warn!(
                "{:?} {}: file is gone from disk (last known hash {}, modified_index {}); dropping it from the save map",
                save_type,
                key,
                fmt_hash(entry.hash),
                entry.modified_index
            );
        }
    }

    existing_map.retain(|k, _| new_saves.contains_key(k));
}

async fn scanned_stat(scanned_paths: &HashMap<String, PathBuf>, key: &str) -> String {
    match scanned_paths.get(key) {
        Some(path) => fmt_stat_opt(file_stat(path).await),
        None => "<unknown path>".to_string(),
    }
}
