#[macro_use]
extern crate rocket;

use rocket::State;
use rocket::data::{Limits, ToByteUnit};
use rocket::request::{FromRequest, Outcome, Request};
use rocket::figment::providers::{Env, Format, Toml};
use rocket::figment::{Figment, Profile};
use rocket::http::Status;
use rocket::response::status::NotFound;
use rocket::{fs::NamedFile, serde::json::Json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use uuid::Uuid;

mod database;
mod web;

use database::{Database, DeviceEntry, NoSyncEntry, Origin, OverrideEntry, QuarantineEntry};

/// Default cap on the JSON body of an upload, in MiB.
const MAX_UPLOAD_JSON_MIB: u64 = 256;

use mister_save_utils::logging::{self, fmt_hash, fmt_hash_opt, fmt_index_opt};
use mister_save_utils::{
    DEVICE_ID_HEADER, FetchSaveRequest, QuarantinedSave, SaveFile, SaveFileType, SaveRef,
    ServerState, UserSaveData, file_stat, fmt_stat_opt, hash_bytes, hashes_equal, log_debug,
    log_error, log_info, log_warn, zlib_decompress,
};

/// Saves are stored on the server exactly as the client sent them: zlib
/// compressed. To log a content hash that is comparable to the client's, the
/// payload has to be decompressed first.
fn content_hash(compressed: &[u8]) -> (Option<u64>, usize) {
    match zlib_decompress(compressed) {
        Ok(data) => (Some(hash_bytes(&data)), data.len()),
        Err(_) => (None, 0),
    }
}

async fn stored_content_hash(path: &Path) -> (Option<u64>, usize) {
    match tokio::fs::read(path).await {
        Ok(compressed) => content_hash(&compressed),
        Err(_) => (None, 0),
    }
}

#[get("/health")]
async fn health() -> Status {
    Status::Ok
}

/// Held across every read-check-write of a save and its metadata. Two
/// machines uploading the same save at once would otherwise both pass the
/// check and leave one's bytes on disk under the other's metadata. Saves are
/// small, so one lock for the whole server is plenty.
struct WriteLock(Mutex<()>);

/// The machine a request comes from, when the client says. A client from
/// before this existed sends nothing and is treated the old way: conflicts
/// are rejected rather than quarantined.
struct Device(String);

#[rocket::async_trait]
impl<'r> FromRequest<'r> for Device {
    type Error = ();

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, ()> {
        match request.headers().get_one(DEVICE_ID_HEADER) {
            Some(id) if valid_device_id(id) => Outcome::Success(Device(id.to_string())),
            _ => Outcome::Forward(Status::BadRequest),
        }
    }
}

/// Device ids become a directory name and part of a database key.
fn valid_device_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Whether `part` is safe to use as one path component and as one segment
/// of a `/`-separated database key.
fn safe_component(part: &str) -> bool {
    !part.is_empty()
        && part != "."
        && part != ".."
        && !part.contains(|c| c == '/' || c == '\\' || c == '\0')
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn folder_for(save_type: &SaveFileType) -> Option<&'static str> {
    match save_type {
        SaveFileType::GameSave => Some("saves"),
        SaveFileType::SaveState => Some("savestates"),
        SaveFileType::NvRam => Some("nvram"),
        SaveFileType::CoreWatch => None,
    }
}

fn head_path(user_id: &str, folder: &str, core: &str, name: &str) -> PathBuf {
    PathBuf::from(format!("user_saves/{}/{}/{}/{}", user_id, folder, core, name))
}

fn quarantine_path(user_id: &str, device_id: &str, folder: &str, core: &str, name: &str) -> PathBuf {
    PathBuf::from(format!(
        "user_saves/{}/quarantine/{}/{}/{}/{}",
        user_id, device_id, folder, core, name
    ))
}

/// Writes `bytes` beside `path` and renames it into place once it is on
/// disk, so a fetch running at the same moment sees the old file or the new
/// one and never a partial write.
async fn write_durable(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let mut partial = path.as_os_str().to_owned();
    partial.push(".partial");
    let partial = PathBuf::from(partial);

    let mut file = File::create(&partial).await?;
    file.write_all(bytes).await?;
    // tokio's File completes writes in the background; flush and fsync
    // before the rename makes them the save we acknowledge.
    file.flush().await?;
    file.sync_all().await?;
    drop(file);

    tokio::fs::rename(&partial, path).await
}

fn note_device(db: &Database, user_id: &str, device: &Option<Device>) {
    let Some(Device(id)) = device else { return };
    let key = format!("{}/{}", user_id, id);
    let mut entry: DeviceEntry = database::get(&db.devices, &key).unwrap_or(DeviceEntry {
        id: id.clone(),
        name: None,
        last_seen: 0,
    });
    entry.last_seen = now();
    database::put(&db.devices, &key, &entry);
}

/// Drops whatever the server holds apart for one machine's copy of a save:
/// that machine's copy and the server's are the same thing again.
async fn clear_device_state(db: &Database, user_id: &str, save: &SaveFile, device_id: &str) {
    let key = database::device_save_key(user_id, &save.core, &save.name, device_id);
    database::remove(&db.overrides, &key);

    if database::remove(&db.quarantine, &key) {
        if let Some(folder) = folder_for(&save.save_type) {
            let path = quarantine_path(user_id, device_id, folder, &save.core, &save.name);
            let _ = tokio::fs::remove_file(&path).await;
        }
        log_info!(
            "{}/{}: quarantined copy from device {} dropped, that machine is back on the current save",
            save.core,
            save.name,
            device_id
        );
    }
}

/// Makes `compressed` the current copy of a save. `save` is its metadata,
/// with `data` already cleared.
async fn store_current(
    db: &Database,
    user_id: &str,
    folder: &str,
    save: &SaveFile,
    compressed: &[u8],
    device_id: Option<&str>,
) -> Status {
    let save_key = format!("{}/{}", save.core, save.name);
    let file_path = head_path(user_id, folder, &save.core, &save.name);

    if let Err(e) = write_durable(&file_path, compressed).await {
        log_error!("Failed to write save file {:?}: {:?}", file_path, e);
        return Status::InternalServerError;
    }

    // Read the file back so the log records what is actually stored, not just
    // what we intended to store.
    let after_stat = file_stat(&file_path).await;
    let (after_hash, after_len) = stored_content_hash(&file_path).await;

    if after_hash.map_or(true, |h| !hashes_equal(h, save.hash)) {
        log_error!(
            "{}: verification FAILED after writing {} - expected content hash {}, stored file \
             hashes {} ({} bytes decompressed, file {})",
            save_key,
            file_path.display(),
            fmt_hash(save.hash),
            fmt_hash_opt(after_hash),
            after_len,
            fmt_stat_opt(after_stat)
        );
    } else {
        log_info!(
            "{}: wrote {} - content hash {} ({}), modified_index {}",
            save_key,
            file_path.display(),
            fmt_hash_opt(after_hash),
            fmt_stat_opt(after_stat),
            save.modified_index
        );
    }

    if db.set_user_save_data(user_id, save) != Some(true) {
        log_error!("Failed to update user save data for user_id: {}", user_id);
        return Status::InternalServerError;
    }

    database::put(
        &db.origins,
        &database::save_key(user_id, &save.core, &save.name),
        &Origin {
            device_id: device_id.map(str::to_string),
            at: now(),
        },
    );

    Status::Ok
}

/// 200: stored as the current save. 202: the upload conflicts with the
/// current save and is held in quarantine. 409: conflicts and was not kept.
/// 423: this save is set not to sync.
#[post("/upload_save/<user_id>", data = "<save_file>")]
async fn upload_save(
    user_id: &str,
    save_file: Json<SaveFile>,
    device: Option<Device>,
    db: &State<Arc<Database>>,
    lock: &State<WriteLock>,
) -> Status {
    let mut _save_file = save_file.into_inner();
    let save_key = format!("{}/{}", _save_file.core, _save_file.name);

    if !safe_component(user_id)
        || !safe_component(&_save_file.core)
        || !safe_component(&_save_file.name)
    {
        log_warn!(
            "upload of {:?} from user {:?} rejected: not a valid save name",
            save_key,
            user_id
        );
        return Status::BadRequest;
    }

    let user_dir = PathBuf::from(format!("user_saves/{}", user_id));

    if !user_dir.exists() {
        log_warn!(
            "upload of {} rejected: no directory for user {}",
            save_key,
            user_id
        );
        return Status::NotFound;
    };

    let Some(saves_folder) = folder_for(&_save_file.save_type) else {
        log_error!(
            "upload of {} from user {} rejected: unsupported save file type {:?}",
            save_key,
            user_id,
            _save_file.save_type
        );
        return Status::BadRequest;
    };

    let _guard = lock.0.lock().await;

    note_device(db, user_id, &device);
    let device_id = device.as_ref().map(|d| d.0.as_str());

    let db_key = database::save_key(user_id, &_save_file.core, &_save_file.name);

    if db.no_sync.contains_key(&db_key).unwrap_or(false) {
        log_info!(
            "upload of {} from user {} refused: this save is set not to sync",
            save_key,
            user_id
        );
        return Status::Locked;
    }

    let existing_entry = db.get_save_file(user_id, &_save_file.core, &_save_file.name);
    let incoming_data = _save_file.data.take();
    let base_hash = _save_file.base_hash.take();

    // A client that already knows its content hash matches what the server
    // has (per its own save map) can ask to move modified_index forward
    // without resending the file - omitting `data` is how it asks. Only
    // honor that when the server's own stored metadata confirms the hash
    // truly matches; otherwise this would be a way to bump the index on
    // content the server never actually received, so send it back to do a
    // normal full upload instead.
    let Some(incoming_data) = incoming_data else {
        return match existing_entry {
            Some(existing) if hashes_equal(existing.hash, _save_file.hash) => {
                // Never backwards: a machine that only just met this content
                // can hold a lower index than the one the others agreed on.
                _save_file.modified_index = _save_file.modified_index.max(existing.modified_index);

                log_info!(
                    "upload {} from user {} ({:?}): index-only update, hash {} confirmed \
                     unchanged, modified_index {} -> {}; no file bytes sent",
                    save_key,
                    user_id,
                    _save_file.save_type,
                    fmt_hash(_save_file.hash),
                    existing.modified_index,
                    _save_file.modified_index
                );

                match db.set_user_save_data(user_id, &_save_file) {
                    Some(true) => {
                        if let Some(id) = device_id {
                            clear_device_state(db, user_id, &_save_file, id).await;
                        }
                        Status::Ok
                    }
                    _ => {
                        log_error!(
                            "Failed to apply index-only update for user_id: {} save: {}",
                            user_id,
                            save_key
                        );
                        Status::InternalServerError
                    }
                }
            }
            Some(existing) => {
                log_warn!(
                    "upload {} from user {} rejected: index-only update claimed hash {} but \
                     the server has {} stored; the client must send the full file.",
                    save_key,
                    user_id,
                    fmt_hash(_save_file.hash),
                    fmt_hash(existing.hash)
                );
                Status::BadRequest
            }
            None => {
                log_warn!(
                    "upload {} from user {} rejected: index-only update but the server has no \
                     existing copy; the client must send the full file.",
                    save_key,
                    user_id
                );
                Status::BadRequest
            }
        };
    };

    // Whether an upload continues the current save is decided on hashes, so
    // a payload that isn't what its hash claims can't be filed anywhere.
    let (incoming_hash, incoming_len) = content_hash(&incoming_data);

    if incoming_hash.map_or(true, |h| !hashes_equal(h, _save_file.hash)) {
        log_warn!(
            "upload {} from user {} rejected: payload hashes {} but the client claimed {}",
            save_key,
            user_id,
            fmt_hash_opt(incoming_hash),
            fmt_hash(_save_file.hash)
        );
        return Status::BadRequest;
    }

    log_info!(
        "upload {} from user {} device {} ({:?}): hash {}, base {}, {} bytes compressed, {} bytes decompressed, modified_index {}",
        save_key,
        user_id,
        device_id.unwrap_or("<not given>"),
        _save_file.save_type,
        fmt_hash(_save_file.hash),
        fmt_hash_opt(base_hash),
        incoming_data.len(),
        incoming_len,
        _save_file.modified_index
    );

    let quarantine = match &existing_entry {
        None => {
            log_info!(
                "{}: new for user {}, no previous entry",
                save_key,
                user_id
            );
            false
        }
        Some(existing) if hashes_equal(existing.hash, _save_file.hash) => {
            _save_file.modified_index = _save_file.modified_index.max(existing.modified_index);
            log_info!(
                "{}: content unchanged (hash {}), modified_index {} -> {}",
                save_key,
                fmt_hash(_save_file.hash),
                existing.modified_index,
                _save_file.modified_index
            );
            false
        }
        // The second case is a machine whose quarantined copy was made the
        // current save while it kept playing: its base is still from before
        // the conflict, but a machine only ever sends one line of a save, so
        // what it sends after the current save came from it continues it.
        Some(existing)
            if base_hash.map_or(false, |b| hashes_equal(b, existing.hash))
                || (device_id.is_some()
                    && database::get::<Origin>(&db.origins, &db_key)
                        .map_or(false, |origin| origin.device_id.as_deref() == device_id)) =>
        {
            _save_file.modified_index = _save_file
                .modified_index
                .max(existing.modified_index + 1);
            log_info!(
                "{}: continues the current save (hash {} -> {}), modified_index {} -> {}",
                save_key,
                fmt_hash(existing.hash),
                fmt_hash(_save_file.hash),
                existing.modified_index,
                _save_file.modified_index
            );
            false
        }
        Some(existing) => {
            // Not a continuation of what the server has: this machine changed
            // a copy the server has since moved on from, or one it never
            // synced at all.
            let Some(id) = device_id else {
                // A client that doesn't name its machine can't have a copy
                // quarantined for it, so it keeps the rule it was written
                // for: the higher modified_index wins.
                if _save_file.modified_index > existing.modified_index {
                    log_warn!(
                        "{}: client without a device id replaces hash {} with {} on modified_index {} > {}",
                        save_key,
                        fmt_hash(existing.hash),
                        fmt_hash(_save_file.hash),
                        _save_file.modified_index,
                        existing.modified_index
                    );
                    return store_current(db, user_id, saves_folder, &_save_file, &incoming_data, None)
                        .await;
                }

                log_warn!(
                    "{}: REJECTED - incoming modified_index {} is not newer than the stored {}, \
                     yet the content differs (stored hash {}, incoming hash {}).",
                    save_key,
                    _save_file.modified_index,
                    existing.modified_index,
                    fmt_hash(existing.hash),
                    fmt_hash(_save_file.hash)
                );
                return Status::Conflict;
            };

            let device_key =
                database::device_save_key(user_id, &_save_file.core, &_save_file.name, id);

            if db.overrides.contains_key(&device_key).unwrap_or(false) {
                log_warn!(
                    "{}: REJECTED - device {} was told to take the server's copy and has not yet; \
                     its hash {} is not kept",
                    save_key,
                    id,
                    fmt_hash(_save_file.hash)
                );
                return Status::Conflict;
            }

            log_warn!(
                "{}: CONFLICT - device {} sent hash {} based on {}, but the current save is {}. \
                 Holding it in quarantine; the current save and every other machine are untouched.",
                save_key,
                id,
                fmt_hash(_save_file.hash),
                fmt_hash_opt(base_hash),
                fmt_hash(existing.hash)
            );
            true
        }
    };

    if !quarantine {
        let status = store_current(
            db,
            user_id,
            saves_folder,
            &_save_file,
            &incoming_data,
            device_id,
        )
        .await;

        if status == Status::Ok {
            if let Some(id) = device_id {
                clear_device_state(db, user_id, &_save_file, id).await;
            }
        }

        return status;
    }

    // Only reachable with a device id, see above.
    let Some(id) = device_id else {
        return Status::InternalServerError;
    };

    let path = quarantine_path(user_id, id, saves_folder, &_save_file.core, &_save_file.name);

    if let Err(e) = write_durable(&path, &incoming_data).await {
        log_error!("Failed to write quarantined save {:?}: {:?}", path, e);
        return Status::InternalServerError;
    }

    let device_key = database::device_save_key(user_id, &_save_file.core, &_save_file.name, id);
    let timestamp = now();
    let first_at = database::get::<QuarantineEntry>(&db.quarantine, &device_key)
        .map_or(timestamp, |previous| previous.first_at);

    let entry = QuarantineEntry {
        core: _save_file.core.clone(),
        name: _save_file.name.clone(),
        save_type: _save_file.save_type.clone(),
        hash: _save_file.hash,
        modified_index: _save_file.modified_index,
        device_id: id.to_string(),
        size: incoming_len as u64,
        first_at,
        updated_at: timestamp,
    };

    if !database::put(&db.quarantine, &device_key, &entry) {
        log_error!("Failed to record quarantined save {}", save_key);
        return Status::InternalServerError;
    }

    Status::Accepted
}

#[post("/fetch_save", data = "<save_request>")]
async fn fetch_save(
    save_request: Json<FetchSaveRequest>,
    db: &State<Arc<Database>>,
) -> Result<NamedFile, NotFound<String>> {
    let save_key = format!("{}/{}", &save_request.core, &save_request.name);

    if !safe_component(&save_request.user_id)
        || !safe_component(&save_request.core)
        || !safe_component(&save_request.name)
    {
        log_warn!("fetch of {:?} rejected: not a valid save name", save_key);
        return Err(NotFound("Save file not found".to_string()));
    }

    let user_dir = PathBuf::from(format!("user_saves/{}", &save_request.user_id));

    if !user_dir.exists() {
        log_warn!(
            "fetch of {} rejected: no directory for user {}",
            save_key,
            save_request.user_id
        );
        return Err(NotFound(format!(
            "User directory not found for user_id: {}",
            save_request.user_id
        )));
    };

    let save_folder = match save_request.save_type {
        SaveFileType::GameSave => "saves",
        SaveFileType::SaveState => "savestates",
        SaveFileType::NvRam => "nvram",
        _ => {
            log_error!(
                "fetch of {} rejected: unsupported save file type {:?}",
                save_key,
                save_request.save_type
            );
            return Err(NotFound(format!(
                "Unsupported save file type: {:?}",
                save_request.save_type
            )));
        }
    };

    let path = format!(
        "user_saves/{}/{}/{}/{}",
        &save_request.user_id, save_folder, &save_request.core, &save_request.name
    );
    let path = PathBuf::from(&path);

    let stat = file_stat(&path).await;
    let (stored_hash, stored_len) = stored_content_hash(&path).await;
    let entry = db.get_save_file(
        &save_request.user_id,
        &save_request.core,
        &save_request.name,
    );

    match NamedFile::open(&path).await {
        Ok(file) => {
            log_info!(
                "fetch {} for user {} ({:?}): serving {} ({}), content hash {}, {} bytes decompressed; \
                 stored metadata hash {} at modified_index {}, client asked for modified_index {}",
                save_key,
                save_request.user_id,
                save_request.save_type,
                path.display(),
                fmt_stat_opt(stat),
                fmt_hash_opt(stored_hash),
                stored_len,
                fmt_hash_opt(entry.as_ref().map(|e| e.hash)),
                fmt_index_opt(entry.as_ref().map(|e| e.modified_index)),
                save_request.modified_index
            );

            if let (Some(stored), Some(entry)) = (stored_hash, entry.as_ref()) {
                if !hashes_equal(stored, entry.hash) {
                    log_warn!(
                        "{}: serving content that hashes {} while the metadata advertises {}; the \
                         client will report a hash mismatch.",
                        save_key,
                        fmt_hash(stored),
                        fmt_hash(entry.hash)
                    );
                }
            }

            Ok(file)
        }
        Err(e) => {
            log_error!(
                "fetch {} for user {}: {} could not be opened: {:?}",
                save_key,
                save_request.user_id,
                path.display(),
                e
            );
            Err(NotFound(format!("Save file not found")))
        }
    }
}

#[get("/fetch_user_data/<user_id>")]
async fn fetch_user_data(
    user_id: &str,
    device: Option<Device>,
    db: &State<Arc<Database>>,
) -> Result<Json<ServerState>, NotFound<String>> {
    let user_save_data: UserSaveData = match db.get_user_save_data(user_id) {
        Some(data) => data,
        None => {
            log_warn!("no save data found for user_id: {}", user_id);
            return Err(NotFound(format!(
                "No save data found for user_id: {}",
                user_id
            )));
        }
    };

    note_device(db, user_id, &device);

    let prefix = format!("{}/", user_id);
    let mine = |id: &str| device.as_ref().map_or(false, |d| d.0 == id);

    let state = ServerState {
        supports_quarantine: true,
        no_sync: database::scan::<NoSyncEntry>(&db.no_sync, &prefix)
            .into_iter()
            .map(|e| SaveRef {
                core: e.core,
                name: e.name,
                save_type: e.save_type,
            })
            .collect(),
        quarantined: database::scan::<QuarantineEntry>(&db.quarantine, &prefix)
            .into_iter()
            .filter(|e| mine(&e.device_id))
            .map(|e| QuarantinedSave {
                core: e.core,
                name: e.name,
                save_type: e.save_type,
                hash: e.hash,
            })
            .collect(),
        overrides: database::scan::<OverrideEntry>(&db.overrides, &prefix)
            .into_iter()
            .filter(|e| mine(&e.device_id))
            .map(|e| SaveRef {
                core: e.core,
                name: e.name,
                save_type: e.save_type,
            })
            .collect(),
        saves: user_save_data,
    };

    // Every client asks for this on a timer, so it is too frequent for info.
    log_debug!(
        "serving metadata for user {}: {} game saves, {} save states, {} nvram entries, \
         {} not syncing, {} quarantined and {} overridden for this device",
        user_id,
        state.saves.game_saves.len(),
        state.saves.save_states.len(),
        state.saves.nv_ram.len(),
        state.no_sync.len(),
        state.quarantined.len(),
        state.overrides.len()
    );

    Ok(Json(state))
}

/// A machine reporting that it replaced its copy with the server's, as a
/// discarded quarantine told it to.
#[post("/ack_override/<user_id>", data = "<save>")]
async fn ack_override(
    user_id: &str,
    save: Json<SaveRef>,
    device: Device,
    db: &State<Arc<Database>>,
    lock: &State<WriteLock>,
) -> Status {
    let _guard = lock.0.lock().await;
    let key = database::device_save_key(user_id, &save.core, &save.name, &device.0);

    if database::remove(&db.overrides, &key) {
        log_info!(
            "{}/{}: device {} took the server's copy, override cleared",
            save.core,
            save.name,
            device.0
        );
    }

    Status::Ok
}

#[get("/generate_user_id")]
async fn generate_user_id() -> Result<String, Status> {
    let user_id = Uuid::new_v4();

    let user_dir = PathBuf::from(format!("user_saves/{}", user_id));

    if let Err(e) = tokio::fs::create_dir_all(&user_dir).await {
        log_error!("Failed to create user directory: {:?}", e);
        return Err(Status::InternalServerError);
    }

    match tokio::fs::create_dir_all(user_dir.join("saves")).await {
        Ok(_) => {}
        Err(e) => {
            log_error!("Failed to create saves directory: {:?}", e);
            return Err(Status::InternalServerError);
        }
    }

    match tokio::fs::create_dir_all(user_dir.join("savestates")).await {
        Ok(_) => {}
        Err(e) => {
            log_error!("Failed to create states directory: {:?}", e);
            return Err(Status::InternalServerError);
        }
    }

    match tokio::fs::create_dir_all(user_dir.join("nvram")).await {
        Ok(_) => {}
        Err(e) => {
            log_error!("Failed to create nvram directory: {:?}", e);
            return Err(Status::InternalServerError);
        }
    }

    log_info!("generated new user_id {}", user_id);

    Ok(user_id.to_string())
}

#[launch]
fn rocket() -> _ {
    logging::init("server", None);
    log_info!(
        "==== mister_save_server v{} starting ====",
        env!("CARGO_PKG_VERSION")
    );
    if let Some(path) = logging::log_file_path() {
        log_info!("writing log to {}", path.display());
    }

    let db = Arc::new(Database::new("user_saves_sled").expect("Failed to open database"));

    // The upload limit lives here rather than in Rocket.toml so it still applies
    // when the binary is started from a directory without that file (Rocket's
    // own default is 1 MiB). Save data travels as a JSON array of byte values,
    // which is roughly 3.6x the size of the compressed save itself, so this
    // allows saves of about 70 MiB compressed. Rocket.toml and ROCKET_LIMITS
    // still override it.
    let defaults = rocket::Config {
        limits: Limits::default().limit("json", MAX_UPLOAD_JSON_MIB.mebibytes()),
        ..rocket::Config::default()
    };
    let figment = Figment::from(defaults)
        .merge(Toml::file(Env::var_or("ROCKET_CONFIG", "Rocket.toml")).nested())
        .merge(Env::prefixed("ROCKET_").ignore(&["PROFILE"]).global())
        .select(Profile::from_env_or(
            "ROCKET_PROFILE",
            rocket::Config::DEFAULT_PROFILE,
        ));
    match figment.extract_inner::<Limits>("limits") {
        Ok(limits) => log_info!(
            "upload body limit: {}",
            limits.get("json").unwrap_or(Limits::JSON)
        ),
        Err(e) => log_warn!("could not read configured limits: {}", e),
    }

    build(figment, db)
}

fn build(figment: Figment, db: Arc<Database>) -> rocket::Rocket<rocket::Build> {
    rocket::custom(figment)
        .manage(db)
        .manage(WriteLock(Mutex::new(())))
        .mount(
            "/",
            routes![
                generate_user_id,
                upload_save,
                fetch_save,
                fetch_user_data,
                ack_override,
                health
            ],
        )
        .mount("/", web::routes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::ZlibEncoder};
    use rocket::http::{ContentType, Header};
    use rocket::local::asynchronous::Client;
    use std::io::Write;

    fn compress(data: &[u8]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    struct Harness {
        client: Client,
        user: String,
    }

    impl Harness {
        /// Uploads `content` as Genesis/Sonic.sav and returns the HTTP status.
        async fn upload(
            &self,
            device: Option<&str>,
            content: &str,
            base: Option<&str>,
            modified_index: u64,
        ) -> u16 {
            let save = SaveFile {
                name: "Sonic.sav".to_string(),
                save_type: SaveFileType::GameSave,
                core: "Genesis".to_string(),
                hash: hash_bytes(content.as_bytes()),
                modified_index,
                user_id: self.user.clone(),
                data: Some(compress(content.as_bytes())),
                base_hash: base.map(|b| hash_bytes(b.as_bytes())),
            };

            let mut request = self
                .client
                .post(format!("/upload_save/{}", self.user))
                .header(ContentType::JSON)
                .body(serde_json::to_vec(&save).unwrap());
            if let Some(id) = device {
                request = request.header(Header::new(DEVICE_ID_HEADER, id.to_string()));
            }
            request.dispatch().await.status().code
        }

        /// What `device` is told when it asks for the save metadata.
        async fn state(&self, device: &str) -> ServerState {
            self.client
                .get(format!("/fetch_user_data/{}", self.user))
                .header(Header::new(DEVICE_ID_HEADER, device.to_string()))
                .dispatch()
                .await
                .into_json()
                .await
                .unwrap()
        }

        async fn current_hash(&self) -> u64 {
            self.state("dev-a").await.saves.game_saves["Genesis/Sonic.sav"].hash
        }

        async fn post(&self, path: &str, body: serde_json::Value) -> u16 {
            self.client
                .post(format!("/api/{}/{}", self.user, path))
                .header(ContentType::JSON)
                .body(body.to_string())
                .dispatch()
                .await
                .status()
                .code
        }

        async fn overview(&self) -> serde_json::Value {
            self.client
                .get(format!("/api/{}/overview", self.user))
                .dispatch()
                .await
                .into_json()
                .await
                .unwrap()
        }

        async fn download(&self, device: Option<&str>) -> String {
            let mut url = format!("/api/{}/download?core=Genesis&name=Sonic.sav", self.user);
            if let Some(id) = device {
                url.push_str(&format!("&device={}", id));
            }
            self.client
                .get(url)
                .dispatch()
                .await
                .into_string()
                .await
                .unwrap()
        }
    }

    fn h(content: &str) -> u64 {
        hash_bytes(content.as_bytes())
    }

    /// One test for the whole flow: the server keeps its files under the
    /// working directory, which only one test at a time can point elsewhere.
    #[rocket::async_test]
    async fn quarantine_flow() {
        let dir = std::env::temp_dir().join(format!("mister_save_server_test_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_current_dir(&dir).unwrap();

        let db = Arc::new(Database::new("user_saves_sled").unwrap());
        let figment = Figment::from(rocket::Config::debug_default());
        let client = Client::tracked(build(figment, db)).await.unwrap();
        let user = client
            .get("/generate_user_id")
            .dispatch()
            .await
            .into_string()
            .await
            .unwrap();
        let t = Harness { client, user };
        let sonic = serde_json::json!({ "core": "Genesis", "name": "Sonic.sav" });
        let with_device = |id: &str| {
            serde_json::json!({ "core": "Genesis", "name": "Sonic.sav", "device_id": id })
        };

        // A's first upload is the current save; A continues it.
        assert_eq!(t.upload(Some("dev-a"), "a1", None, 0).await, 200);
        assert_eq!(t.upload(Some("dev-a"), "a2", Some("a1"), 1).await, 200);
        assert_eq!(t.current_hash().await, h("a2"));

        // B never had A's save. Even with a far higher index its copy is
        // held apart, and nothing A or anyone else sees changes.
        assert_eq!(t.upload(Some("dev-b"), "b1", None, 50).await, 202);
        assert_eq!(t.current_hash().await, h("a2"));
        assert_eq!(t.state("dev-b").await.quarantined[0].hash, h("b1"));
        assert!(t.state("dev-a").await.quarantined.is_empty());

        // B keeps playing: the quarantined copy follows it, as one entry.
        assert_eq!(t.upload(Some("dev-b"), "b2", Some("b1"), 51).await, 202);
        let overview = t.overview().await;
        assert_eq!(overview["quarantine"].as_array().unwrap().len(), 1);
        assert_eq!(overview["quarantine"][0]["device_id"], "dev-b");
        assert_eq!(t.state("dev-b").await.quarantined[0].hash, h("b2"));
        assert_eq!(t.download(Some("dev-b")).await, "b2");
        assert_eq!(t.download(None).await, "a2");

        // A is unaffected and carries on.
        assert_eq!(t.upload(Some("dev-a"), "a3", Some("a2"), 2).await, 200);

        // Decision 1: B's copy becomes the truth.
        assert_eq!(t.post("quarantine/promote", with_device("dev-b")).await, 200);
        assert_eq!(t.current_hash().await, h("b2"));
        assert!(t.state("dev-b").await.quarantined.is_empty());
        assert!(t.overview().await["quarantine"].as_array().unwrap().is_empty());
        // Its index is above both, for clients that still go by index.
        assert!(t.state("dev-a").await.saves.game_saves["Genesis/Sonic.sav"].modified_index > 51);
        // B's next change continues it, though B's own base is still from
        // before the conflict.
        assert_eq!(t.upload(Some("dev-b"), "b3", None, 52).await, 200);

        // A, still on a3, saves again: now A is the one in conflict.
        assert_eq!(t.upload(Some("dev-a"), "a4", Some("a3"), 3).await, 202);
        assert_eq!(t.current_hash().await, h("b3"));

        // Decision 2: discard A's copy. A is told to take the server's, and
        // anything else it sends for this save meanwhile is turned away.
        assert_eq!(t.post("quarantine/discard", with_device("dev-a")).await, 200);
        assert!(t.state("dev-a").await.quarantined.is_empty());
        assert_eq!(t.state("dev-a").await.overrides.len(), 1);
        assert!(t.state("dev-b").await.overrides.is_empty());
        assert_eq!(t.upload(Some("dev-a"), "a5", Some("a4"), 4).await, 409);
        assert!(t.overview().await["quarantine"].as_array().unwrap().is_empty());
        assert_eq!(t.current_hash().await, h("b3"));

        let ack = t
            .client
            .post(format!("/ack_override/{}", t.user))
            .header(ContentType::JSON)
            .header(Header::new(DEVICE_ID_HEADER, "dev-a"))
            .body(r#"{"core":"Genesis","name":"Sonic.sav","save_type":"GameSave"}"#)
            .dispatch()
            .await
            .status()
            .code;
        assert_eq!(ack, 200);
        assert!(t.state("dev-a").await.overrides.is_empty());
        assert_eq!(t.upload(Some("dev-a"), "a6", Some("b3"), 60).await, 200);

        // Decision 3: stop syncing. A pending conflict for it is dropped and
        // nobody can upload it until syncing is resumed.
        assert_eq!(t.upload(Some("dev-b"), "b4", Some("b3"), 53).await, 202);
        let mut no_sync = sonic.clone();
        no_sync["save_type"] = "GameSave".into();
        no_sync["enabled"] = true.into();
        assert_eq!(t.post("no_sync", no_sync.clone()).await, 200);
        assert_eq!(t.state("dev-b").await.no_sync.len(), 1);
        assert!(t.state("dev-b").await.quarantined.is_empty());
        assert_eq!(t.upload(Some("dev-a"), "a7", Some("a6"), 61).await, 423);
        assert_eq!(t.current_hash().await, h("a6"));

        no_sync["enabled"] = false.into();
        assert_eq!(t.post("no_sync", no_sync).await, 200);
        assert_eq!(t.upload(Some("dev-a"), "a7", Some("a6"), 61).await, 200);

        // A client from before device ids keeps the modified_index rule.
        let index = t.state("dev-a").await.saves.game_saves["Genesis/Sonic.sav"].modified_index;
        assert_eq!(t.upload(None, "old1", None, index).await, 409);
        assert_eq!(t.upload(None, "old1", None, index + 1).await, 200);

        // Names that would escape the user's directory are refused.
        let escape = SaveFile {
            name: "..".to_string(),
            core: "..".to_string(),
            hash: h("x"),
            data: Some(compress(b"x")),
            ..SaveFile::default()
        };
        let refused = t
            .client
            .post(format!("/upload_save/{}", t.user))
            .header(ContentType::JSON)
            .body(serde_json::to_vec(&escape).unwrap())
            .dispatch()
            .await
            .status()
            .code;
        assert_eq!(refused, 400);

        assert_eq!(t.post("device", serde_json::json!({ "device_id": "dev-a", "name": "Den" })).await, 200);
        let overview = t.overview().await;
        assert!(
            overview["devices"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d["name"] == "Den")
        );
        assert!(t.client.get("/").dispatch().await.into_string().await.unwrap().contains("<title>"));

        drop(t);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
