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
    sync::{LazyLock, OnceLock},
    time::Duration,
};
use tokio::sync::Mutex;

use mister_save_utils::logging::{self, fmt_hash, fmt_hash_opt, fmt_index_opt};
use mister_save_utils::{
    ConflictAction, DEVICE_ID_HEADER, FetchSaveRequest, SAVE_MAP_VERSION, SaveFile, SaveFileType,
    SaveRef, ServerState, UploadSaveRequest, UserSaveData, file_stat, fmt_stat_opt, hash_file,
    hashes_equal, is_hidden_path, log_debug, log_error, log_info, log_warn, read_file_to_bytes,
    zlib_decompress,
};
mod inotify_watcher;
use inotify_watcher::*;

static SERVER_URL: LazyLock<Mutex<String>> = LazyLock::new(|| Mutex::new(String::new()));
static USER_ID: LazyLock<Mutex<String>> = LazyLock::new(|| Mutex::new(String::new()));
static CURRENT_CORE: LazyLock<Mutex<String>> = LazyLock::new(|| Mutex::new(String::new()));
static IS_ONE_SHOT: LazyLock<Mutex<bool>> = LazyLock::new(|| Mutex::new(false));
static SAVE_MAP_PATH: &str = "/media/fat/cloud_saves/mister_save_map.json";
static CORE_NAME_PATH: &str = "/tmp/CORENAME";
/// Kept out of cloud_saves.ini, which is copied from one MiSTer to the next.
static DEVICE_ID_PATH: &str = "/media/fat/cloud_saves/device_id";

/// Names this machine to the server, so a conflicting save can be held in
/// quarantine for it and decisions made in the web interface can be
/// addressed to it. Sent as a header on every request.
static DEVICE_ID: OnceLock<String> = OnceLock::new();

/// One save, as (type, "<folder>/<file>").
type SaveId = (SaveFileType, String);

/// Saves the server said not to sync, as of the last metadata fetch. Lets a
/// live file event skip the upload without asking the server each time.
static NO_SYNC: LazyLock<Mutex<HashSet<SaveId>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

const DEFAULT_POLL_INTERVAL_SECS: u64 = 60;
const MIN_POLL_INTERVAL_SECS: u64 = 10;

/// Seconds between background checks of the server for saves made on another
/// machine. Set from `poll_interval_seconds` in the `[Sync]` section of
/// cloud_saves.ini; 0 turns the background check off.
static POLL_INTERVAL_SECS: LazyLock<Mutex<u64>> =
    LazyLock::new(|| Mutex::new(DEFAULT_POLL_INTERVAL_SECS));

/// Logs at info for a sync somebody triggered and at debug for a background
/// poll, so a poll that finds nothing to do doesn't write to the SD card
/// every minute.
macro_rules! log_routine {
    ($quiet:expr, $($arg:tt)*) => {
        if $quiet {
            log_debug!($($arg)*)
        } else {
            log_info!($($arg)*)
        }
    };
}
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
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(id) = DEVICE_ID.get() {
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(DEVICE_ID_HEADER.as_bytes()),
            reqwest::header::HeaderValue::from_str(id),
        ) {
            headers.insert(name, value);
        }
    }

    reqwest::Client::builder()
        .default_headers(headers)
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
    /// The server's entry, which becomes the save map's once the file is
    /// actually on disk.
    remote: SaveFile,
    /// The server asked for this download to replace whatever this machine
    /// holds, and wants to hear once it has.
    ack_override: bool,
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
    /// The content this upload claims to be derived from. The server makes
    /// the upload its current save only if that is what it has.
    base_hash: Option<u64>,
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
    /// The entry's base_hash, which a content change leaves as it was.
    base_hash: Option<u64>,
    changed: bool,
}

/// What happened when an upload was sent to the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UploadOutcome {
    /// The server accepted it as the current save.
    Accepted,
    /// The server kept it, but apart from the current save (HTTP 202): it
    /// conflicts, and waits in quarantine for a decision in the web
    /// interface. This machine carries on with its own copy meanwhile.
    Quarantined,
    /// The server refused it because this save is set not to sync (HTTP 423).
    NoSync,
    /// The server rejected it (HTTP 409) because the claimed modified_index
    /// wasn't newer than what it already has, yet the content differs - this
    /// upload is not a continuation of the server's current data.
    Conflict,
    /// Anything else: a network error, a non-2xx/409 HTTP status, or a local
    /// failure before the request was even sent.
    Failed,
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
    init_device_id().await;
    init_current_core().await;
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

    let poll_interval = *POLL_INTERVAL_SECS.lock().await;
    if poll_interval > 0 {
        log_info!(
            "checking the server for saves from other machines every {}s",
            poll_interval
        );
        tokio::spawn(poll_server_forever(Duration::from_secs(poll_interval)));
    } else {
        log_info!("background server check disabled (poll_interval_seconds = 0)");
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

    let configured_interval = cloud_saves_ini
        .get("sync")
        .and_then(|sync_map| sync_map.get("poll_interval_seconds"))
        .and_then(|value| value.as_deref());

    if let Some(raw) = configured_interval {
        match parse_poll_interval(raw) {
            Some(seconds) => {
                log_info!("config: poll_interval_seconds={}", seconds);
                *POLL_INTERVAL_SECS.lock().await = seconds;
            }
            None => log_warn!(
                "config: poll_interval_seconds={:?} is not a whole number of seconds, using the default of {}",
                raw,
                DEFAULT_POLL_INTERVAL_SECS
            ),
        }
    }
}

/// 0 disables polling; anything else is held to a floor so a typo can't have
/// the client hammering the server.
fn parse_poll_interval(raw: &str) -> Option<u64> {
    match raw.trim().parse::<u64>().ok()? {
        0 => Some(0),
        seconds => Some(seconds.max(MIN_POLL_INTERVAL_SECS)),
    }
}

fn valid_device_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Reads this machine's id, making one up the first time. Must run before
/// the first request: the HTTP client picks the id up when it is built.
async fn init_device_id() {
    let stored = tokio::fs::read_to_string(DEVICE_ID_PATH)
        .await
        .map(|id| id.trim().to_string())
        .unwrap_or_default();

    let id = if valid_device_id(&stored) {
        stored
    } else {
        let id = uuid::Uuid::new_v4().to_string();
        if let Err(e) = tokio::fs::write(DEVICE_ID_PATH, &id).await {
            log_error!(
                "Failed to write device id to {}: {:?}. This machine will look like a new one \
                 to the server on every start.",
                DEVICE_ID_PATH,
                e
            );
        }
        id
    };

    log_info!("device id: {}", id);
    let _ = DEVICE_ID.set(id);
}

/// The client can start while a core is already running (the supervisor
/// restarting it after a crash, for one), so the running core has to be read
/// rather than assumed to be MENU.
async fn init_current_core() {
    if let Ok(name) = tokio::fs::read_to_string(CORE_NAME_PATH).await {
        let name = name.trim().to_string();
        log_info!("core at startup: {}", name);
        *CURRENT_CORE.lock().await = name;
    }
}

/// The core whose saves must be left alone right now, if any. A running core
/// holds its game's save in memory and writes it back out later, so a save
/// downloaded underneath it would be overwritten with the stale copy.
async fn busy_core() -> Option<String> {
    if *IS_ONE_SHOT.lock().await {
        return None;
    }

    let core = CURRENT_CORE.lock().await.clone();
    if core.is_empty() || core.eq_ignore_ascii_case("MENU") {
        None
    } else {
        Some(core)
    }
}

/// Whether `save_key` ("<folder>/<file>") could belong to the game loaded in
/// `running_core`. /tmp/CORENAME names the core but not the game, and the
/// save folder is usually but not always spelled the same as the core, so
/// this errs towards treating a save as in use.
fn save_in_use_by(save_type: &SaveFileType, save_key: &str, running_core: &str) -> bool {
    // nvram files are named after the arcade set rather than filed under a
    // core folder, so there is nothing to match the running core against.
    if *save_type == SaveFileType::NvRam {
        return true;
    }

    let folder = save_key.split('/').next().unwrap_or("").to_lowercase();
    let core = running_core.to_lowercase();

    if folder.is_empty() || core.is_empty() {
        return true;
    }

    // One core can file saves under a related folder, e.g. TGFX16 and
    // TGFX16-CD.
    if folder.starts_with(&core) || core.starts_with(&folder) {
        return true;
    }

    const ALIASES: [(&str, &str); 1] = [("genesis", "megadrive")];
    ALIASES
        .iter()
        .any(|(a, b)| (folder == *a && core == *b) || (folder == *b && core == *a))
}

/// Picks up saves made on other machines while this one stays powered on.
/// Without it the only chances to notice them are startup and a return to
/// MENU.
async fn poll_server_forever(interval: Duration) {
    let mut failing = false;

    loop {
        tokio::time::sleep(interval).await;

        match sync_saves(true).await {
            Ok(()) => {
                if failing {
                    log_info!("poll: sync is working again");
                    failing = false;
                }
            }
            Err(e) => {
                if failing {
                    log_debug!("poll: sync still failing: {:?}", e);
                } else {
                    log_warn!(
                        "poll: sync failed, retrying every {:?} (further failures are logged at debug): {:?}",
                        interval,
                        e
                    );
                    failing = true;
                }
            }
        }
    }
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
    let core_name_path = PathBuf::from(CORE_NAME_PATH);

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

    let previous_core = CURRENT_CORE.lock().await.clone();

    // One core switch can raise several modify events.
    if core_name == previous_core {
        return;
    }

    log_info!("core changed: {} -> {}", previous_core, core_name);

    // Recorded before syncing: the sync below leaves the running core's
    // saves alone, and by now that is no longer the core that was just left.
    *CURRENT_CORE.lock().await = core_name.clone();

    if core_name == "MENU" {
        log_info!("returned to MENU, rescanning saves and syncing");
        update_save_map().await;
        // A short retry here covers a brief wifi blip; a longer one isn't
        // worth blocking menu navigation for, since the next visit to MENU
        // (or the next local file change) will naturally try again.
        sync_saves_with_retry(3, Duration::from_secs(2), "menu-return").await;
    }
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
    let base_hash = update.base_hash;

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

    let save_id: SaveId = (save_type.clone(), save_key.clone());

    let outcome = if NO_SYNC.lock().await.contains(&save_id) {
        log_info!(
            "{}: set not to sync; the change is recorded locally and not uploaded",
            save_key
        );
        UploadOutcome::NoSync
    } else {
        upload_file(
            path,
            save_type.clone(),
            modified_index,
            base_hash,
            Some(&file_data),
            server_url.clone(),
            user_id,
        )
        .await
    };

    match outcome {
        UploadOutcome::Accepted => {
            // This content is the server's current save now, so it is what
            // the next change will be derived from.
            if let Some(entry) = category_entry_mut(&mut save_map, &save_type, &save_key) {
                entry.base_hash = Some(file_hash);
            }
        }
        UploadOutcome::Quarantined => {
            // base_hash stays put: this machine's copy and the server's
            // current save still part ways where they did.
            log_warn!(
                "{}: the server is holding this change in quarantine. This machine keeps \
                 playing on its own copy and no other machine is touched; decide which copy \
                 to keep at {}",
                save_key,
                server_url
            );
        }
        UploadOutcome::NoSync => {
            NO_SYNC.lock().await.insert(save_id);
        }
        UploadOutcome::Conflict | UploadOutcome::Failed => {}
    }

    if outcome == UploadOutcome::Conflict {
        // The server has content for this key that this machine never
        // synced down before writing its own - most likely an earlier sync
        // failed (network, or the retries in sync_saves_with_retry were
        // exhausted) and this is autosave or a manual save writing over
        // what the user assumed was a continuation of another machine's
        // save. Uploading now would silently destroy that machine's
        // progress, so instead this re-fetches the server's actual current
        // state and aligns this entry to it (keeping the real on-disk
        // content, but matching the server's modified_index) so the next
        // full sync sees an honest conflict - same index, different hash -
        // and resolves it through the normal conflict path rather than one
        // side quietly overwriting the other here.
        log_warn!(
            "{}: not uploaded - this change is not a continuation of the server's current \
             data. The server's existing save is being preserved; re-fetching its current \
             state so this is resolved as a conflict at the next sync.",
            save_key
        );

        match get_server_data(false).await {
            Ok(remote) => match category_entry(&remote.saves, &save_type, &save_key) {
                Some(remote_entry) => {
                    let remote_index = remote_entry.modified_index;
                    let remote_hash = remote_entry.hash;

                    if let Some(local_entry) =
                        category_entry_mut(&mut save_map, &save_type, &save_key)
                    {
                        local_entry.modified_index = remote_index;
                    }

                    log_warn!(
                        "{}: local modified_index aligned to the server's current {} (server \
                         hash {}, local hash {}); this machine's copy and the server's will be \
                         compared again at the next sync",
                        save_key,
                        remote_index,
                        fmt_hash(remote_hash),
                        fmt_hash(file_hash)
                    );
                }
                None => {
                    log_warn!(
                        "{}: server reported no entry for this key right after rejecting the \
                         upload; leaving the save map unchanged, this will be re-evaluated next \
                         time this file changes or a sync runs",
                        save_key
                    );
                    return;
                }
            },
            Err(e) => {
                log_warn!(
                    "{}: failed to re-fetch the server's current state after the conflict \
                     ({:?}); leaving the save map unchanged, this will be re-evaluated next \
                     time this file changes or a sync runs",
                    save_key,
                    e
                );
                return;
            }
        }
    }

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
    let base_hash = saves.get(save_key).and_then(|s| s.base_hash);

    if saves
        .get(save_key)
        .map_or(false, |s| hashes_equal(s.hash, file_hash))
    {
        // No changes
        return SaveMapUpdate {
            modified_index: saves.get(save_key).map_or(0, |s| s.modified_index),
            previous_hash,
            previous_index,
            base_hash,
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
        base_hash,
    };

    saves.insert(save_key.to_string(), save_file);

    SaveMapUpdate {
        modified_index,
        previous_hash,
        previous_index,
        base_hash,
        changed: true,
    }
}

/// Looks up one save's entry in a `UserSaveData`'s category for `save_type`.
fn category_entry<'a>(
    data: &'a UserSaveData,
    save_type: &SaveFileType,
    save_key: &str,
) -> Option<&'a SaveFile> {
    match save_type {
        SaveFileType::GameSave => data.game_saves.get(save_key),
        SaveFileType::SaveState => data.save_states.get(save_key),
        SaveFileType::NvRam => data.nv_ram.get(save_key),
        SaveFileType::CoreWatch => None,
    }
}

/// Mutable version of [`category_entry`], for adjusting a local save map
/// entry in place.
fn category_entry_mut<'a>(
    data: &'a mut UserSaveData,
    save_type: &SaveFileType,
    save_key: &str,
) -> Option<&'a mut SaveFile> {
    match save_type {
        SaveFileType::GameSave => data.game_saves.get_mut(save_key),
        SaveFileType::SaveState => data.save_states.get_mut(save_key),
        SaveFileType::NvRam => data.nv_ram.get_mut(save_key),
        SaveFileType::CoreWatch => None,
    }
}

fn category_map_mut<'a>(
    data: &'a mut UserSaveData,
    save_type: &SaveFileType,
) -> Option<&'a mut HashMap<String, SaveFile>> {
    match save_type {
        SaveFileType::GameSave => Some(&mut data.game_saves),
        SaveFileType::SaveState => Some(&mut data.save_states),
        SaveFileType::NvRam => Some(&mut data.nv_ram),
        SaveFileType::CoreWatch => None,
    }
}

/// `quiet` is for the background poll: routine lines drop to debug, and
/// failures are left to the caller to report so an unreachable server isn't
/// logged as an error every interval.
async fn get_server_data(
    quiet: bool,
) -> Result<ServerState, Box<dyn std::error::Error + Send + Sync>> {
    let server_url = SERVER_URL.lock().await.clone();
    let user_id = USER_ID.lock().await.clone();
    log_routine!(
        quiet,
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
                match resp.json::<ServerState>().await {
                    Ok(user_data) => {
                        log_routine!(
                            quiet,
                            "server reports {} game saves, {} save states, {} nvram entries",
                            user_data.saves.game_saves.len(),
                            user_data.saves.save_states.len(),
                            user_data.saves.nv_ram.len()
                        );
                        Ok(user_data)
                    }
                    Err(e) => {
                        if !quiet {
                            log_error!("failed to decode server save metadata: {:?}", e);
                        }
                        Err(e.into())
                    }
                }
            } else {
                if !quiet {
                    log_error!("failed to fetch user data: HTTP {}", resp.status());
                }
                Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Failed to fetch user data: HTTP {}", resp.status()),
                )))
            }
        }
        Err(e) => {
            if !quiet {
                log_error!("failed to reach server for user data: {:?}", e);
            }
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
        match sync_saves(false).await {
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

/// What a sync needs, beyond the two save maps, to decide what happens to
/// each save.
#[derive(Default)]
struct SyncContext {
    /// One-shot runs ask at the terminal which copy of a conflict to keep.
    manage_conflicts: bool,
    busy_core: Option<String>,
    /// The server predates quarantine, so the only way to settle two
    /// differing copies is the old one: the higher modified_index wins.
    index_rule_only: bool,
    /// The save map was written by a client from before base_hash. Its
    /// entries that have none yet are settled by modified_index, as that
    /// client would have, rather than all being reported as conflicts.
    legacy_map: bool,
    no_sync: HashSet<SaveId>,
    /// Saves where this machine must take the server's copy regardless.
    overrides: HashSet<SaveId>,
    /// Hash of each copy the server holds in quarantine for this machine.
    quarantined: HashMap<SaveId, u64>,
    server_url: String,
    user_id: String,
}

/// What to do with a save whose local and server copies differ.
enum Plan {
    /// Upload as the continuation of the server's copy.
    Replace(String),
    Download(String),
    /// Upload for the server to hold in quarantine; keep the local file.
    Quarantine(String),
    /// Already in quarantine as it stands; nothing to send.
    Held,
    Ask,
}

/// `quiet` marks a background poll, which logs at info only when it finds
/// something to do.
async fn sync_saves(quiet: bool) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Held across the network round-trip to the server too: a sync's
    // read-modify-write of the save map isn't done until it writes the
    // reconciled result back out, and letting a live file event interleave
    // partway through would let one side's update silently overwrite the
    // other's.
    let _save_map_guard = SAVE_MAP_LOCK.lock().await;

    log_routine!(quiet, "---- starting save synchronization ----");

    let save_map_path = PathBuf::from(SAVE_MAP_PATH);
    let content = tokio::fs::read_to_string(&save_map_path).await?;
    let mut local_data: UserSaveData = serde_json::from_str(&content)?;
    let saved_map = local_data.clone();
    let server = get_server_data(quiet).await?;

    let ctx = SyncContext {
        manage_conflicts: *IS_ONE_SHOT.lock().await,
        busy_core: busy_core().await,
        index_rule_only: !server.supports_quarantine,
        legacy_map: local_data.map_version < SAVE_MAP_VERSION,
        no_sync: server
            .no_sync
            .iter()
            .map(|s| (s.save_type.clone(), s.key()))
            .collect(),
        overrides: server
            .overrides
            .iter()
            .map(|s| (s.save_type.clone(), s.key()))
            .collect(),
        quarantined: server
            .quarantined
            .iter()
            .map(|q| {
                (
                    (q.save_type.clone(), format!("{}/{}", q.core, q.name)),
                    q.hash,
                )
            })
            .collect(),
        server_url: SERVER_URL.lock().await.clone(),
        user_id: USER_ID.lock().await.clone(),
    };

    *NO_SYNC.lock().await = ctx.no_sync.clone();

    log_routine!(
        quiet,
        "local save map holds {} game saves, {} save states, {} nvram entries (conflicts are {})",
        local_data.game_saves.len(),
        local_data.save_states.len(),
        local_data.nv_ram.len(),
        if ctx.manage_conflicts {
            "asked about here"
        } else if ctx.index_rule_only {
            "settled by modified_index, the server has no quarantine"
        } else {
            "sent to quarantine on the server"
        }
    );

    let mut download_tasks: Vec<DownloadTask> = Vec::new();
    let mut upload_tasks: Vec<UploadTask> = Vec::new();
    let mut deferred = 0;

    deferred += process_category(
        SaveFileType::GameSave,
        &mut local_data.game_saves,
        &server.saves.game_saves,
        &ctx,
        &mut download_tasks,
        &mut upload_tasks,
    )
    .await;

    deferred += process_category(
        SaveFileType::SaveState,
        &mut local_data.save_states,
        &server.saves.save_states,
        &ctx,
        &mut download_tasks,
        &mut upload_tasks,
    )
    .await;

    deferred += process_category(
        SaveFileType::NvRam,
        &mut local_data.nv_ram,
        &server.saves.nv_ram,
        &ctx,
        &mut download_tasks,
        &mut upload_tasks,
    )
    .await;

    let total_tasks = download_tasks.len() + upload_tasks.len();

    if let Some(core) = ctx.busy_core.as_deref() {
        if deferred > 0 {
            log_info!(
                "{} save(s) that differ from the server are left alone while {} is running; \
                 they sync on the return to MENU",
                deferred,
                core
            );
        }
    }

    log_routine!(
        quiet && total_tasks == 0,
        "sync plan: {} download(s), {} upload(s)",
        download_tasks.len(),
        upload_tasks.len()
    );

    let mp = MultiProgress::new();
    let total_pb = if total_tasks > 0 && ctx.manage_conflicts {
        mp.add(ProgressBar::new(total_tasks as u64))
    } else {
        ProgressBar::hidden()
    };

    let download_pb: ProgressBar = if download_tasks.len() > 0 && ctx.manage_conflicts {
        mp.add(ProgressBar::new(download_tasks.len() as u64))
    } else {
        ProgressBar::hidden()
    };

    let upload_pb: ProgressBar = if upload_tasks.len() > 0 && ctx.manage_conflicts {
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

    // Whether every save was dealt with, so a save map from an older client
    // can stop being treated as one.
    let mut settled = deferred == 0;

    for download in download_tasks {
        match fetch_save_file(&download).await {
            Ok(written_hash) => {
                // Only now does the save map describe the server's copy. Had
                // it been updated when the download was planned, a failed
                // download would leave the old file looking like a local
                // change to the new one, and it would be uploaded over it.
                if let Some(map) = category_map_mut(&mut local_data, &download.request.save_type)
                {
                    map.insert(
                        download.save_key.clone(),
                        SaveFile {
                            hash: written_hash,
                            base_hash: Some(written_hash),
                            data: None,
                            ..download.remote.clone()
                        },
                    );
                }

                if download.ack_override {
                    ack_override(&download, &ctx).await;
                }
            }
            Err(e) => {
                log_error!("download of {} failed: {:?}", download.save_key, e);
                settled = false;
            }
        }
        download_pb.inc(1);
        total_pb.inc(1);
    }

    for upload in upload_tasks {
        log_info!(
            "uploading {}: local hash {} (modified_index {}), base {}, server hash {} (modified_index {}); reason: {}{}",
            upload.save_key,
            fmt_hash(upload.local_hash),
            upload.request.modified_index,
            fmt_hash_opt(upload.base_hash),
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
                upload.base_hash,
                upload.request.server_url,
                upload.request.user_id,
            )
            .await;
        } else {
            let save_type = upload.request.save_type.clone();
            let outcome = upload_file(
                upload.request.path,
                upload.request.save_type,
                upload.request.modified_index,
                upload.base_hash,
                None,
                upload.request.server_url,
                upload.request.user_id,
            )
            .await;

            match outcome {
                UploadOutcome::Accepted => {
                    if let Some(entry) =
                        category_entry_mut(&mut local_data, &save_type, &upload.save_key)
                    {
                        entry.base_hash = Some(upload.local_hash);
                    }
                }
                UploadOutcome::Quarantined => log_warn!(
                    "{}: held in quarantine on the server. This machine keeps its own copy \
                     and no other machine is touched; decide which copy to keep at {}",
                    upload.save_key,
                    ctx.server_url
                ),
                UploadOutcome::Conflict => {
                    // Either the server changed again after this sync fetched
                    // its data, or it is waiting for this machine to take its
                    // copy. The next sync re-fetches and re-evaluates.
                    log_warn!(
                        "{}: upload rejected as a conflict; this will be re-evaluated on the next sync",
                        upload.save_key
                    );
                    settled = false;
                }
                UploadOutcome::NoSync => {}
                UploadOutcome::Failed => settled = false,
            }
        }

        upload_pb.inc(1);
        total_pb.inc(1);
    }

    download_pb.finish();
    upload_pb.finish();
    total_pb.finish_with_message("Sync complete!");

    if settled {
        local_data.map_version = SAVE_MAP_VERSION;
    }

    // A poll that changed nothing shouldn't rewrite the map every interval:
    // that is needless SD card wear and one more chance for a power cut to
    // leave it truncated.
    if local_data != saved_map {
        let json_data = serde_json::to_vec(&local_data)?;
        tokio::fs::write(&save_map_path, &json_data).await?;
    }

    log_routine!(quiet, "---- save synchronization complete ----");
    Ok(())
}

/// Tells the server this machine replaced its copy as told, so the server
/// stops insisting. If this doesn't get through, the next sync downloads the
/// same copy again and retries.
async fn ack_override(download: &DownloadTask, ctx: &SyncContext) {
    let save = SaveRef {
        core: download.request.core.clone(),
        name: download.request.name.clone(),
        save_type: download.request.save_type.clone(),
    };

    let result = HTTP_CLIENT
        .post(format!("{}/ack_override/{}", ctx.server_url, ctx.user_id))
        .json(&save)
        .send()
        .await;

    match result {
        Ok(resp) if resp.status().is_success() => log_info!(
            "{}: told the server its copy is now in place here",
            download.save_key
        ),
        Ok(resp) => log_warn!(
            "{}: server did not take the override acknowledgement: HTTP {}",
            download.save_key,
            resp.status()
        ),
        Err(e) => log_warn!(
            "{}: override acknowledgement failed: {:?}",
            download.save_key,
            e
        ),
    }
}

async fn process_category(
    save_type: SaveFileType,
    local_saves: &mut HashMap<String, SaveFile>,
    remote_saves: &HashMap<String, SaveFile>,
    ctx: &SyncContext,
    download_tasks: &mut Vec<DownloadTask>,
    upload_tasks: &mut Vec<UploadTask>,
) -> usize {
    let mut conflict_state = ConflictAction::AskUser;
    // Saves that need syncing but belong to the running core.
    let mut deferred = 0;

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
        let id: SaveId = (save_type.clone(), key.clone());
        let local_entry = local_saves.get(&key).cloned();
        let remote_entry = remote_saves.get(&key);

        if ctx.no_sync.contains(&id) {
            log_debug!("{:?} {}: set not to sync, skipping", save_type, key);
            continue;
        }

        if let Some(core) = ctx.busy_core.as_deref() {
            if save_in_use_by(&save_type, &key, core) {
                let in_sync = matches!(
                    (&local_entry, remote_entry),
                    (Some(l), Some(r))
                        if hashes_equal(l.hash, r.hash) && l.modified_index == r.modified_index
                );
                if !in_sync {
                    log_debug!(
                        "{:?} {}: differs from the server but {} is running, leaving it for now",
                        save_type,
                        key,
                        core
                    );
                    deferred += 1;
                }
                continue;
            }
        }

        let overridden = ctx.overrides.contains(&id);

        match (local_entry, remote_entry) {
            (Some(local), None) => {
                log_info!(
                    "{:?} {}: local only (hash {}, modified_index {}), server has no copy -> upload",
                    save_type,
                    key,
                    fmt_hash(local.hash),
                    local.modified_index
                );
                queue_upload(
                    upload_tasks,
                    ctx,
                    &key,
                    &local,
                    None,
                    local.base_hash,
                    "save exists locally but not on the server".to_string(),
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
                queue_download(
                    download_tasks,
                    &key,
                    remote,
                    None,
                    overridden,
                    "save exists on the server but not locally".to_string(),
                );
            }

            (Some(local), Some(remote)) => {
                if overridden {
                    log_warn!(
                        "{:?} {}: this machine's copy was discarded in the web interface; taking \
                         the server's copy (hash {}) over the local one (hash {})",
                        save_type,
                        key,
                        fmt_hash(remote.hash),
                        fmt_hash(local.hash)
                    );
                    queue_download(
                        download_tasks,
                        &key,
                        remote,
                        Some(&local),
                        true,
                        "the server's copy overrules this machine's".to_string(),
                    );
                    continue;
                }

                if hashes_equal(local.hash, remote.hash) {
                    if local.base_hash != Some(local.hash) {
                        if let Some(l_mut) = local_saves.get_mut(&key) {
                            l_mut.base_hash = Some(local.hash);
                        }
                    }

                    if local.modified_index < remote.modified_index {
                        log_info!(
                            "{:?} {}: content identical (hash {}), adopting remote modified_index {} (was {}); no file touched",
                            save_type,
                            key,
                            fmt_hash(local.hash),
                            remote.modified_index,
                            local.modified_index
                        );
                        if let Some(l_mut) = local_saves.get_mut(&key) {
                            l_mut.modified_index = remote.modified_index;
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
                        queue_upload(
                            upload_tasks,
                            ctx,
                            &key,
                            &local,
                            Some(remote),
                            Some(remote.hash),
                            "content identical, local modified_index is ahead".to_string(),
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
                let held = ctx.quarantined.get(&id).copied();
                let by_index =
                    ctx.index_rule_only || (local.base_hash.is_none() && ctx.legacy_map);

                let plan = if by_index {
                    if ctx.manage_conflicts {
                        Plan::Ask
                    } else if local_is_newer {
                        Plan::Replace(format!(
                            "local modified_index {} > server {}",
                            local.modified_index, remote.modified_index
                        ))
                    } else {
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
                        }
                        Plan::Download(format!(
                            "server modified_index {} >= local {}",
                            remote.modified_index, local.modified_index
                        ))
                    }
                } else if held.map_or(false, |h| hashes_equal(h, local.hash)) {
                    Plan::Held
                } else if held.is_some() {
                    Plan::Quarantine(
                        "the copy in quarantine is older than this machine's".to_string(),
                    )
                } else if local.base_hash == Some(remote.hash) {
                    Plan::Replace("local copy was changed from the server's current one".to_string())
                } else if local.base_hash == Some(local.hash) {
                    Plan::Download(
                        "server copy changed, local copy has not since it last synced".to_string(),
                    )
                } else if ctx.manage_conflicts {
                    Plan::Ask
                } else {
                    Plan::Quarantine(
                        "local and server copies both changed; the server decides nothing by itself"
                            .to_string(),
                    )
                };

                let plan = match plan {
                    Plan::Ask => {
                        let (primary, secondary) = if local_is_newer {
                            (&local, remote)
                        } else {
                            (remote, &local)
                        };

                        if conflict_state != ConflictAction::KeepLocalAll
                            && conflict_state != ConflictAction::KeepRemoteAll
                        {
                            conflict_state = prompt_user_conflict(
                                primary,
                                secondary,
                                local_is_newer,
                                !ctx.index_rule_only,
                            );
                        }

                        log_info!(
                            "{:?} {}: conflict resolution is {:?}",
                            save_type,
                            key,
                            conflict_state
                        );

                        match conflict_state {
                            ConflictAction::KeepLocal | ConflictAction::KeepLocalAll => {
                                Plan::Replace("user kept the local copy".to_string())
                            }
                            ConflictAction::KeepRemote | ConflictAction::KeepRemoteAll => {
                                Plan::Download("user kept the server copy".to_string())
                            }
                            ConflictAction::Quarantine => Plan::Quarantine(
                                "left for a decision in the web interface".to_string(),
                            ),
                            ConflictAction::AskUser => continue, // prompt never returns this
                        }
                    }
                    decided => decided,
                };

                match plan {
                    Plan::Replace(reason) => {
                        log_info!(
                            "{:?} {}: local hash {} (modified_index {}) replaces server hash {} (modified_index {}); {}",
                            save_type,
                            key,
                            fmt_hash(local.hash),
                            local.modified_index,
                            fmt_hash(remote.hash),
                            remote.modified_index,
                            reason
                        );

                        // Clients that still go by modified_index only take
                        // this copy if its index is the higher one.
                        let mut local = local;
                        if local.modified_index <= remote.modified_index {
                            local.modified_index = remote.modified_index + 1;
                            if let Some(l_mut) = local_saves.get_mut(&key) {
                                l_mut.modified_index = local.modified_index;
                            }
                        }

                        queue_upload(
                            upload_tasks,
                            ctx,
                            &key,
                            &local,
                            Some(remote),
                            Some(remote.hash),
                            reason,
                            false,
                        );
                    }
                    Plan::Download(reason) => {
                        log_info!(
                            "{:?} {}: server hash {} (modified_index {}) replaces local hash {} (modified_index {}); {}",
                            save_type,
                            key,
                            fmt_hash(remote.hash),
                            remote.modified_index,
                            fmt_hash(local.hash),
                            local.modified_index,
                            reason
                        );
                        queue_download(download_tasks, &key, remote, Some(&local), false, reason);
                    }
                    Plan::Quarantine(reason) => {
                        log_warn!(
                            "{:?} {}: CONFLICT - local hash {} (based on {}) and server hash {} are \
                             different lines of the same save. Keeping the local file and sending \
                             it to quarantine on the server; {}",
                            save_type,
                            key,
                            fmt_hash(local.hash),
                            fmt_hash_opt(local.base_hash),
                            fmt_hash(remote.hash),
                            reason
                        );
                        queue_upload(
                            upload_tasks,
                            ctx,
                            &key,
                            &local,
                            Some(remote),
                            local.base_hash,
                            reason,
                            false,
                        );
                    }
                    Plan::Held => {
                        log_debug!(
                            "{:?} {}: local hash {} is in quarantine on the server awaiting a decision",
                            save_type,
                            key,
                            fmt_hash(local.hash)
                        );
                    }
                    Plan::Ask => {}
                }
            }
            (None, None) => unreachable!(),
        }
    }

    deferred
}

/// `allow_quarantine` is false against a server that has no quarantine to
/// offer.
fn prompt_user_conflict(
    newer: &SaveFile,
    older: &SaveFile,
    local_is_newer: bool,
    allow_quarantine: bool,
) -> ConflictAction {
    println!("Conflict: {}/{}", newer.core, newer.name);
    println!(
        "  Higher index ({}): Index {} Hash {}",
        if local_is_newer { "Local" } else { "Remote" },
        newer.modified_index,
        fmt_hash(newer.hash)
    );
    println!(
        "  Lower index ({}): Index {} Hash {}",
        if local_is_newer { "Remote" } else { "Local" },
        older.modified_index,
        fmt_hash(older.hash)
    );
    if allow_quarantine {
        println!(
            "Action: (L)ocal, (R)emote, (Q)uarantine - keep both and decide later on the web page, \
             (LALL) Local All, (RALL) Remote All, (A)bort"
        );
    } else {
        println!("Action: (L)ocal, (R)emote, (LALL) Local All, (RALL) Remote All, (A)bort");
    }

    // With nobody to ask, nothing is decided here: the local copy goes to
    // quarantine and both survive. A server without quarantine leaves only
    // the old rule, the higher modified_index. Either way it is a per-file
    // answer rather than a "...All" one, so each remaining conflict is still
    // resolved on its own merits.
    let unattended = || {
        if allow_quarantine {
            ConflictAction::Quarantine
        } else if local_is_newer {
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
                let action = unattended();
                log_warn!(
                    "conflict prompt for {}/{} hit end of input (no interactive stdin); \
                     falling back to {:?}",
                    newer.core,
                    newer.name,
                    action
                );
                return action;
            }
            Ok(_) => {}
            Err(e) => {
                let action = unattended();
                log_warn!(
                    "conflict prompt for {}/{} could not read stdin ({:?}); falling back to {:?}",
                    newer.core,
                    newer.name,
                    e,
                    action
                );
                return action;
            }
        }

        return match input.trim().to_uppercase().as_str() {
            "L" => ConflictAction::KeepLocal,
            "R" => ConflictAction::KeepRemote,
            "Q" if allow_quarantine => ConflictAction::Quarantine,
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
    ctx: &SyncContext,
    save_key: &str,
    local: &SaveFile,
    remote: Option<&SaveFile>,
    base_hash: Option<u64>,
    reason: String,
    index_only: bool,
) {
    upload_tasks.push(UploadTask {
        save_key: save_key.to_string(),
        local_hash: local.hash,
        remote_hash: remote.map(|r| r.hash),
        remote_index: remote.map(|r| r.modified_index),
        reason,
        base_hash,
        index_only,
        request: UploadSaveRequest {
            path: PathBuf::from(save_key),
            save_type: local.save_type.clone(),
            modified_index: local.modified_index,
            server_url: ctx.server_url.clone(),
            user_id: ctx.user_id.clone(),
        },
    });
}

fn queue_download(
    download_tasks: &mut Vec<DownloadTask>,
    save_key: &str,
    remote: &SaveFile,
    local: Option<&SaveFile>,
    ack_override: bool,
    reason: String,
) {
    download_tasks.push(DownloadTask {
        request: FetchSaveRequest {
            user_id: remote.user_id.clone(),
            core: remote.core.clone(),
            name: remote.name.clone(),
            save_type: remote.save_type.clone(),
            modified_index: remote.modified_index,
        },
        save_key: save_key.to_string(),
        remote: remote.clone(),
        ack_override,
        expected_hash: remote.hash,
        local_hash: local.map(|l| l.hash),
        local_index: local.map(|l| l.modified_index),
        reason,
    });
}

/// Returns the hash of the content now on disk.
async fn fetch_save_file(
    task: &DownloadTask,
) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
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

                Ok(downloaded_hash)
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
    base_hash: Option<u64>,
    data: Option<&[u8]>,
    server_url: String,
    user_id: String,
) -> UploadOutcome {
    let base_dir = match save_type {
        SaveFileType::GameSave => PathBuf::from("/media/fat/saves"),
        SaveFileType::SaveState => PathBuf::from("/media/fat/savestates"),
        SaveFileType::NvRam => PathBuf::from("/media/fat/config"),
        _ => {
            log_error!("Unsupported save type for upload: {:?}", save_type);
            return UploadOutcome::Failed;
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
                return UploadOutcome::Failed;
            }
        },
    };

    let mut zencode: ZlibEncoder<Vec<u8>> = ZlibEncoder::new(Vec::new(), Compression::default());

    if let Err(e) = zencode.write_all(&data) {
        log_error!("Failed to compress file upload {:?}: {:?}", full_path, e);
        return UploadOutcome::Failed;
    }

    let compressed_data = match zencode.finish() {
        Ok(data) => data,
        Err(e) => {
            log_error!(
                "Failed to finish compression for file {:?}: {:?}",
                full_path,
                e
            );
            return UploadOutcome::Failed;
        }
    };

    let file_name = match full_path.file_name() {
        Some(name) => name.to_string_lossy().to_string(),
        None => {
            log_error!("Failed to get file name for {:?}", full_path);
            return UploadOutcome::Failed;
        }
    };

    let core = match full_path.parent().and_then(|p| p.file_name()) {
        Some(core_name) => core_name.to_string_lossy().to_string(),
        None => {
            log_error!("Failed to get core name for {:?}", full_path);
            return UploadOutcome::Failed;
        }
    };

    let file_hash = match hash_file(&full_path, Some(&data)).await {
        Ok(hash) => hash,
        Err(e) => {
            log_error!("Failed to hash file upload {:?}: {:?}", full_path, e);
            return UploadOutcome::Failed;
        }
    };

    let stat = file_stat(&full_path).await;

    log_info!(
        "POST /upload_save {}/{} ({:?}) from {} [{}]: hash {}, base {}, {} bytes raw, {} bytes compressed, modified_index {}",
        core,
        file_name,
        save_type,
        full_path.display(),
        fmt_stat_opt(stat),
        fmt_hash(file_hash),
        fmt_hash_opt(base_hash),
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
        base_hash,
    };

    match HTTP_CLIENT
        .post(&format!("{}/upload_save/{}", server_url, user_id))
        .json(&save_file)
        .send()
        .await
    {
        Ok(resp) => {
            if resp.status() == reqwest::StatusCode::ACCEPTED {
                log_warn!(
                    "server quarantined {}/{} (hash {}): it is not a continuation of the \
                     server's current save. The current save is untouched.",
                    core,
                    file_name,
                    fmt_hash(file_hash)
                );
                UploadOutcome::Quarantined
            } else if resp.status() == reqwest::StatusCode::LOCKED {
                log_info!(
                    "server refused {}/{}: this save is set not to sync",
                    core,
                    file_name
                );
                UploadOutcome::NoSync
            } else if resp.status().is_success() {
                log_info!(
                    "server accepted {}/{} at hash {} modified_index {}",
                    core,
                    file_name,
                    fmt_hash(file_hash),
                    modified_index
                );
                UploadOutcome::Accepted
            } else if resp.status() == reqwest::StatusCode::CONFLICT {
                log_warn!(
                    "server rejected {}/{} as a conflict: modified_index {} is not a \
                     continuation of what the server currently has (hash {}). This machine's \
                     change will not be uploaded; the server's existing copy is preserved until \
                     this is resolved at the next full sync.",
                    core,
                    file_name,
                    modified_index,
                    fmt_hash(file_hash)
                );
                UploadOutcome::Conflict
            } else {
                log_error!(
                    "Failed to upload save file {:?}: HTTP {}",
                    full_path,
                    resp.status()
                );
                UploadOutcome::Failed
            }
        }
        Err(e) => {
            log_error!("Failed to upload save file {:?}: {:?}", full_path, e);
            UploadOutcome::Failed
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
    base_hash: Option<u64>,
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
        base_hash: None,
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
                upload_file(
                    path,
                    save_type,
                    modified_index,
                    base_hash,
                    None,
                    server_url,
                    user_id,
                )
                .await;
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

    // A map that is missing or unreadable starts over in the current format:
    // nothing in it has been synced, and that is how its entries get treated.
    // Only a map an older client really wrote is reconciled the old way.
    let mut existing_map: UserSaveData = tokio::fs::read_to_string(&save_map_path)
        .await
        .ok()
        .and_then(|content| serde_json::from_str::<UserSaveData>(&content).ok())
        .unwrap_or_else(|| UserSaveData {
            map_version: SAVE_MAP_VERSION,
            ..UserSaveData::default()
        });

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
                        base_hash: None,
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

    result.map_version = existing_map.map_version;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn save(core: &str, name: &str, hash: u64, modified_index: u64) -> (String, SaveFile) {
        (
            format!("{}/{}", core, name),
            SaveFile {
                name: name.to_string(),
                save_type: SaveFileType::GameSave,
                core: core.to_string(),
                hash,
                modified_index,
                user_id: "test".to_string(),
                data: None,
                base_hash: None,
            },
        )
    }

    #[test]
    fn running_core_claims_its_own_folder_only() {
        let game = SaveFileType::GameSave;
        assert!(save_in_use_by(&game, "MegaCD/Sonic CD (USA).sav", "MegaCD"));
        assert!(save_in_use_by(&game, "megacd/Sonic CD (USA).sav", "MEGACD"));
        assert!(!save_in_use_by(&game, "PSX/WipEout 3 (USA).sav", "MegaCD"));
        assert!(!save_in_use_by(&game, "Saturn/Burning Rangers (USA).sav", "PSX"));
    }

    #[test]
    fn running_core_claims_related_folders() {
        let game = SaveFileType::GameSave;
        assert!(save_in_use_by(&game, "TGFX16-CD/Ys.sav", "TGFX16"));
        assert!(save_in_use_by(&game, "MegaDrive/Sonic.sav", "Genesis"));
        assert!(save_in_use_by(&game, "Genesis/Sonic.sav", "MegaDrive"));
    }

    #[test]
    fn running_core_claims_all_nvram() {
        assert!(save_in_use_by(&SaveFileType::NvRam, "nvram/mslug.nvm", "PSX"));
    }

    #[test]
    fn poll_interval_is_parsed_and_floored() {
        assert_eq!(parse_poll_interval("60"), Some(60));
        assert_eq!(parse_poll_interval(" 15 "), Some(15));
        assert_eq!(parse_poll_interval("0"), Some(0));
        assert_eq!(parse_poll_interval("1"), Some(MIN_POLL_INTERVAL_SECS));
        assert_eq!(parse_poll_interval("soon"), None);
        assert_eq!(parse_poll_interval("-5"), None);
    }

    fn based(mut entry: (String, SaveFile), base_hash: u64) -> (String, SaveFile) {
        entry.1.base_hash = Some(base_hash);
        entry
    }

    const KEY: &str = "MegaCD/Sonic CD (USA).sav";

    fn sonic_id() -> SaveId {
        (SaveFileType::GameSave, KEY.to_string())
    }

    /// Plans a sync of one game save and returns (map after, downloads, uploads).
    async fn plan(
        local: (String, SaveFile),
        remote: (String, SaveFile),
        ctx: SyncContext,
    ) -> (HashMap<String, SaveFile>, Vec<DownloadTask>, Vec<UploadTask>) {
        let mut local: HashMap<String, SaveFile> = HashMap::from([local]);
        let remote: HashMap<String, SaveFile> = HashMap::from([remote]);
        let mut downloads = Vec::new();
        let mut uploads = Vec::new();

        process_category(
            SaveFileType::GameSave,
            &mut local,
            &remote,
            &ctx,
            &mut downloads,
            &mut uploads,
        )
        .await;

        (local, downloads, uploads)
    }

    #[tokio::test]
    async fn sync_leaves_the_running_cores_saves_alone() {
        let mut local: HashMap<String, SaveFile> = HashMap::from([
            based(save("MegaCD", "Sonic CD (USA).sav", 1, 0), 1),
            based(save("PSX", "WipEout 3 (USA).sav", 1, 0), 1),
            save("Saturn", "Burning Rangers (USA).sav", 5, 3),
        ]);
        let remote: HashMap<String, SaveFile> = HashMap::from([
            save("MegaCD", "Sonic CD (USA).sav", 2, 1),
            save("PSX", "WipEout 3 (USA).sav", 2, 1),
            save("Saturn", "Burning Rangers (USA).sav", 5, 3),
        ]);
        let mut downloads = Vec::new();
        let mut uploads = Vec::new();
        let ctx = SyncContext {
            busy_core: Some("MegaCD".to_string()),
            ..SyncContext::default()
        };

        let deferred = process_category(
            SaveFileType::GameSave,
            &mut local,
            &remote,
            &ctx,
            &mut downloads,
            &mut uploads,
        )
        .await;

        assert_eq!(deferred, 1);
        assert!(uploads.is_empty());
        assert_eq!(downloads.len(), 1);
        assert_eq!(downloads[0].save_key, "PSX/WipEout 3 (USA).sav");
        // The map must keep describing the file that is really on disk, or
        // the deferred save would look synced and never be fetched.
        assert_eq!(local[KEY].hash, 1);
        assert_eq!(local[KEY].modified_index, 0);
    }

    #[tokio::test]
    async fn unchanged_local_copy_takes_the_servers() {
        let (local, downloads, uploads) = plan(
            based(save("MegaCD", "Sonic CD (USA).sav", 1, 0), 1),
            save("MegaCD", "Sonic CD (USA).sav", 2, 1),
            SyncContext::default(),
        )
        .await;

        assert!(uploads.is_empty());
        assert_eq!(downloads.len(), 1);
        assert!(!downloads[0].ack_override);
        // Planning a download must not touch the map: if the download then
        // fails, the old file would look like a change to the new save.
        assert_eq!(local[KEY].hash, 1);
        assert_eq!(local[KEY].base_hash, Some(1));
    }

    #[tokio::test]
    async fn change_made_from_the_servers_copy_is_uploaded_as_its_successor() {
        // The server's index being the higher one makes no difference.
        let (_, downloads, uploads) = plan(
            based(save("MegaCD", "Sonic CD (USA).sav", 3, 1), 2),
            save("MegaCD", "Sonic CD (USA).sav", 2, 9),
            SyncContext::default(),
        )
        .await;

        assert!(downloads.is_empty());
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].base_hash, Some(2));
        assert_eq!(uploads[0].request.modified_index, 10);
    }

    #[tokio::test]
    async fn copies_that_both_changed_go_to_quarantine_and_the_file_stays() {
        // Local index is far ahead; under the old rule it would have replaced
        // the server's copy outright.
        let (local, downloads, uploads) = plan(
            based(save("MegaCD", "Sonic CD (USA).sav", 7, 40), 1),
            save("MegaCD", "Sonic CD (USA).sav", 2, 5),
            SyncContext::default(),
        )
        .await;

        assert!(downloads.is_empty());
        assert_eq!(uploads.len(), 1);
        // Sent with its true base, which is not the server's copy, so the
        // server files it as a conflict.
        assert_eq!(uploads[0].base_hash, Some(1));
        assert_eq!(local[KEY].hash, 7);
    }

    #[tokio::test]
    async fn never_synced_copy_goes_to_quarantine() {
        let (_, downloads, uploads) = plan(
            save("MegaCD", "Sonic CD (USA).sav", 7, 0),
            save("MegaCD", "Sonic CD (USA).sav", 2, 5),
            SyncContext::default(),
        )
        .await;

        assert!(downloads.is_empty());
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].base_hash, None);
    }

    #[tokio::test]
    async fn copy_already_in_quarantine_is_left_alone() {
        let ctx = SyncContext {
            quarantined: HashMap::from([(sonic_id(), 7)]),
            ..SyncContext::default()
        };
        let (_, downloads, uploads) = plan(
            based(save("MegaCD", "Sonic CD (USA).sav", 7, 4), 1),
            save("MegaCD", "Sonic CD (USA).sav", 2, 5),
            ctx,
        )
        .await;

        assert!(downloads.is_empty());
        assert!(uploads.is_empty());
    }

    #[tokio::test]
    async fn quarantined_copy_follows_further_local_changes() {
        let ctx = SyncContext {
            quarantined: HashMap::from([(sonic_id(), 7)]),
            ..SyncContext::default()
        };
        let (_, downloads, uploads) = plan(
            based(save("MegaCD", "Sonic CD (USA).sav", 8, 5), 1),
            save("MegaCD", "Sonic CD (USA).sav", 2, 5),
            ctx,
        )
        .await;

        assert!(downloads.is_empty());
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].local_hash, 8);
        assert_eq!(uploads[0].base_hash, Some(1));
    }

    #[tokio::test]
    async fn discarded_copy_is_overruled_even_when_newer() {
        let ctx = SyncContext {
            overrides: HashSet::from([sonic_id()]),
            ..SyncContext::default()
        };
        let (_, downloads, uploads) = plan(
            based(save("MegaCD", "Sonic CD (USA).sav", 9, 50), 7),
            save("MegaCD", "Sonic CD (USA).sav", 2, 5),
            ctx,
        )
        .await;

        assert!(uploads.is_empty());
        assert_eq!(downloads.len(), 1);
        assert!(downloads[0].ack_override);
    }

    #[tokio::test]
    async fn save_set_not_to_sync_is_skipped_both_ways() {
        let ctx = || SyncContext {
            no_sync: HashSet::from([sonic_id()]),
            ..SyncContext::default()
        };

        let (_, downloads, uploads) = plan(
            based(save("MegaCD", "Sonic CD (USA).sav", 1, 0), 1),
            save("MegaCD", "Sonic CD (USA).sav", 2, 1),
            ctx(),
        )
        .await;
        assert!(downloads.is_empty() && uploads.is_empty());

        let (_, downloads, uploads) = plan(
            based(save("MegaCD", "Sonic CD (USA).sav", 3, 2), 2),
            save("MegaCD", "Sonic CD (USA).sav", 2, 1),
            ctx(),
        )
        .await;
        assert!(downloads.is_empty() && uploads.is_empty());
    }

    #[tokio::test]
    async fn map_from_an_older_client_is_settled_by_index_once() {
        let ctx = || SyncContext {
            legacy_map: true,
            ..SyncContext::default()
        };

        let (_, downloads, uploads) = plan(
            save("MegaCD", "Sonic CD (USA).sav", 1, 3),
            save("MegaCD", "Sonic CD (USA).sav", 2, 5),
            ctx(),
        )
        .await;
        assert_eq!(downloads.len(), 1);
        assert!(uploads.is_empty());

        let (_, downloads, uploads) = plan(
            save("MegaCD", "Sonic CD (USA).sav", 1, 6),
            save("MegaCD", "Sonic CD (USA).sav", 2, 5),
            ctx(),
        )
        .await;
        assert!(downloads.is_empty());
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].base_hash, Some(2));
    }

    #[tokio::test]
    async fn identical_content_records_the_base_without_transfers() {
        let (local, downloads, uploads) = plan(
            save("MegaCD", "Sonic CD (USA).sav", 2, 0),
            save("MegaCD", "Sonic CD (USA).sav", 2, 5),
            SyncContext::default(),
        )
        .await;

        assert!(downloads.is_empty() && uploads.is_empty());
        assert_eq!(local[KEY].base_hash, Some(2));
        assert_eq!(local[KEY].modified_index, 5);
    }

    #[test]
    fn local_change_keeps_the_base_it_was_made_from() {
        let mut saves: HashMap<String, SaveFile> =
            HashMap::from([based(save("MegaCD", "Sonic CD (USA).sav", 1, 4), 1)]);

        let update = insert_save_data(
            &mut saves,
            KEY,
            "Sonic CD (USA).sav",
            "MegaCD",
            2,
            SaveFileType::GameSave,
        );

        assert!(update.changed);
        assert_eq!(update.base_hash, Some(1));
        assert_eq!(saves[KEY].base_hash, Some(1));
        assert_eq!(saves[KEY].modified_index, 5);
    }
}
