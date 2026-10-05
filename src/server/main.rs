#[macro_use]
extern crate rocket;

use rocket::State;
use rocket::http::Status;
use rocket::response::status::NotFound;
use rocket::{fs::NamedFile, serde::json::Json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs::File;
use uuid::Uuid;

mod database;

use database::Database;

use mister_save_utils::logging::{self, fmt_hash, fmt_hash_opt, fmt_index_opt};
use mister_save_utils::{
    FetchSaveRequest, SaveFile, SaveFileType, UserSaveData, file_stat, fmt_stat_opt, hash_bytes,
    hashes_equal, log_error, log_info, log_warn, zlib_decompress,
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

#[post("/upload_save/<user_id>", data = "<save_file>")]
async fn upload_save(
    user_id: &str,
    save_file: Json<SaveFile>,
    db: &State<Arc<Database>>,
) -> Status {
    let mut _save_file = save_file.clone().into_inner();
    let save_key = format!("{}/{}", _save_file.core, _save_file.name);

    let user_dir = PathBuf::from(format!("user_saves/{}", user_id));

    if !user_dir.exists() {
        log_warn!(
            "upload of {} rejected: no directory for user {}",
            save_key,
            user_id
        );
        return Status::NotFound;
    };

    let saves_folder = match _save_file.save_type {
        SaveFileType::GameSave => "saves",
        SaveFileType::SaveState => "savestates",
        SaveFileType::NvRam => "nvram",
        _ => {
            log_error!(
                "upload of {} from user {} rejected: unsupported save file type {:?}",
                save_key,
                user_id,
                _save_file.save_type
            );
            return Status::BadRequest;
        }
    };

    let saves_dir = user_dir.join(saves_folder);
    let core_path = saves_dir.join(&_save_file.core);
    let file_path = core_path.join(&format!("{}", &_save_file.name));

    // A client that already knows its content hash matches what the server
    // has (per its own save map) can ask to move modified_index forward
    // without resending the file - omitting `data` is how it asks. Only
    // honor that when the server's own stored metadata confirms the hash
    // truly matches; otherwise this would be a way to bump the index on
    // content the server never actually received, so send it back to do a
    // normal full upload instead.
    if _save_file.data.is_none() {
        let existing_entry = db.get_save_file(user_id, &_save_file.core, &_save_file.name);

        return match existing_entry {
            Some(existing) if hashes_equal(existing.hash, _save_file.hash) => {
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
                    Some(true) => Status::Ok,
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
    }

    // _save_file.data is Some from here on - None was handled above.
    let incoming_data = _save_file.data.clone().unwrap();

    // What the client says it is sending.
    let (incoming_hash, incoming_len) = content_hash(&incoming_data);

    if incoming_hash.map_or(true, |h| hashes_equal(h, _save_file.hash)) {
        log_info!(
            "upload {} from user {} ({:?}): claimed hash {}, {} bytes compressed, {} bytes decompressed, modified_index {}",
            save_key,
            user_id,
            _save_file.save_type,
            fmt_hash(_save_file.hash),
            incoming_data.len(),
            incoming_len,
            _save_file.modified_index
        );
    } else {
        log_warn!(
            "upload {} from user {}: payload hashes {} but the client claimed {}. Storing the \
             payload as sent; the metadata hash other machines compare against will be wrong.",
            save_key,
            user_id,
            fmt_hash_opt(incoming_hash),
            fmt_hash(_save_file.hash)
        );
    }

    // What is already stored, both the metadata and the bytes on disk.
    let existing_entry = db.get_save_file(user_id, &_save_file.core, &_save_file.name);
    let existing_stat = file_stat(&file_path).await;
    let (existing_hash, existing_len) = stored_content_hash(&file_path).await;

    match &existing_entry {
        Some(existing) => {
            log_info!(
                "upload {} replaces stored copy: hash {} -> {} (on-disk content hash {}, {} bytes decompressed, file {}), modified_index {} -> {}",
                save_key,
                fmt_hash(existing.hash),
                fmt_hash(_save_file.hash),
                fmt_hash_opt(existing_hash),
                existing_len,
                fmt_stat_opt(existing_stat),
                existing.modified_index,
                _save_file.modified_index
            );

            if existing_hash.map_or(false, |h| !hashes_equal(h, existing.hash)) {
                log_warn!(
                    "{}: stored file content hashes {} but the stored metadata says {}. The \
                     database and the files on disk disagree.",
                    save_key,
                    fmt_hash_opt(existing_hash),
                    fmt_hash(existing.hash)
                );
            }

            if hashes_equal(existing.hash, _save_file.hash) {
                log_info!(
                    "{}: content unchanged (hash {}), only modified_index moves {} -> {}",
                    save_key,
                    fmt_hash(_save_file.hash),
                    existing.modified_index,
                    _save_file.modified_index
                );
            } else if _save_file.modified_index <= existing.modified_index {
                // The client's claimed modified_index is the only thing
                // establishing that its upload is a continuation of what
                // the server already has - a client that never synced the
                // server's current content (sync failed, or it's a machine
                // that's never seen this save before) computes its own
                // index purely from its own local history, with no
                // knowledge that the server has since moved on. That can
                // coincidentally produce a claimed index that isn't newer
                // even though the content is a completely different,
                // unrelated branch. Refuse rather than silently destroy the
                // server's existing continuity - the two copies will be
                // reconciled as a proper conflict on the next full sync
                // instead of one quietly overwriting the other here.
                log_warn!(
                    "{}: REJECTED - incoming modified_index {} is not newer than the stored {}, \
                     yet the content differs (stored hash {}, incoming hash {}). This upload is \
                     not a continuation of the server's current data - refusing to overwrite it.",
                    save_key,
                    _save_file.modified_index,
                    existing.modified_index,
                    fmt_hash(existing.hash),
                    fmt_hash(_save_file.hash)
                );
                return Status::Conflict;
            }
        }
        None => {
            log_info!(
                "upload {} is new for user {}: no previous entry, storing hash {} at modified_index {}",
                save_key,
                user_id,
                fmt_hash(_save_file.hash),
                _save_file.modified_index
            );

            if existing_stat.is_some() {
                log_warn!(
                    "{}: a file already exists at {} ({}) with content hash {} but no database \
                     entry; it is being overwritten.",
                    save_key,
                    file_path.display(),
                    fmt_stat_opt(existing_stat),
                    fmt_hash_opt(existing_hash)
                );
            }
        }
    }

    if let Err(e) = tokio::fs::create_dir_all(&core_path).await {
        log_error!("Failed to create core directory {:?}: {:?}", core_path, e);
        return Status::InternalServerError;
    }

    match File::create(&file_path).await {
        Ok(mut file) => {
            if let Err(e) = tokio::io::AsyncWriteExt::write_all(&mut file, &incoming_data).await {
                log_error!("Failed to write save file {:?}: {:?}", file_path, e);
                return Status::InternalServerError;
            }

            // tokio's File buffers writes and completes them in the
            // background, so dropping the handle can leave a save we have
            // already acknowledged short or empty on disk. Flush and fsync
            // before recording the metadata or answering the client.
            if let Err(e) = tokio::io::AsyncWriteExt::flush(&mut file).await {
                log_error!("Failed to flush save file {:?}: {:?}", file_path, e);
                return Status::InternalServerError;
            }

            if let Err(e) = file.sync_all().await {
                log_error!("Failed to fsync save file {:?}: {:?}", file_path, e);
                return Status::InternalServerError;
            }
        }
        Err(e) => {
            log_error!("Failed to create save file {:?}: {:?}", file_path, e);
            return Status::InternalServerError;
        }
    }

    // Read the file back so the log records what is actually stored, not just
    // what we intended to store.
    let after_stat = file_stat(&file_path).await;
    let (after_hash, after_len) = stored_content_hash(&file_path).await;

    if after_hash.map_or(true, |h| !hashes_equal(h, _save_file.hash)) {
        log_error!(
            "{}: verification FAILED after writing {} - expected content hash {}, stored file \
             hashes {} ({} bytes decompressed, file {})",
            save_key,
            file_path.display(),
            fmt_hash(_save_file.hash),
            fmt_hash_opt(after_hash),
            after_len,
            fmt_stat_opt(after_stat)
        );
    } else {
        log_info!(
            "{}: wrote {} - content hash {} -> {} ({}), modified_index {} -> {}",
            save_key,
            file_path.display(),
            fmt_hash_opt(existing_hash),
            fmt_hash_opt(after_hash),
            fmt_stat_opt(after_stat),
            fmt_index_opt(existing_entry.as_ref().map(|e| e.modified_index)),
            _save_file.modified_index
        );
    }

    _save_file.data = None; // Clear data before storing metadata

    match db.set_user_save_data(user_id, &_save_file) {
        Some(true) => {}
        _ => {
            log_error!("Failed to update user save data for user_id: {}", user_id);
            return Status::InternalServerError;
        }
    };

    Status::Ok
}

#[post("/fetch_save", data = "<save_request>")]
async fn fetch_save(
    save_request: Json<FetchSaveRequest>,
    db: &State<Arc<Database>>,
) -> Result<NamedFile, NotFound<String>> {
    let save_key = format!("{}/{}", &save_request.core, &save_request.name);
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
    db: &State<Arc<Database>>,
) -> Result<Json<UserSaveData>, NotFound<String>> {
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

    log_info!(
        "serving metadata for user {}: {} game saves, {} save states, {} nvram entries",
        user_id,
        user_save_data.game_saves.len(),
        user_save_data.save_states.len(),
        user_save_data.nv_ram.len()
    );

    Ok(Json(user_save_data))
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

    rocket::build().manage(db).mount(
        "/",
        routes![
            generate_user_id,
            upload_save,
            fetch_save,
            fetch_user_data,
            health
        ],
    )
}
