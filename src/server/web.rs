//! The web interface: one page, and the JSON routes it calls.
//!
//! Like the rest of the server it has no accounts. Knowing a user id is what
//! grants access to that user's saves, here exactly as on the sync routes.
//! The page asks for it once and a long-lived cookie remembers it after that.

use rocket::http::{Cookie, CookieJar, Header, SameSite, Status};
use rocket::response::content::{RawHtml, RawJavaScript};
use rocket::response::stream::{Event, EventStream};
use rocket::serde::json::Json;
use rocket::{Shutdown, State};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use mister_save_utils::logging::fmt_hash;
use mister_save_utils::{SaveFile, SaveFileType, log_info, log_warn, zlib_decompress};

use crate::database::{
    self, Database, DeviceEntry, NoSyncEntry, Origin, OverrideEntry, QuarantineEntry,
};
use crate::{
    WriteLock, folder_for, head_path, now, quarantine_path, safe_component, store_current,
    valid_device_id,
};

const MAX_DEVICE_NAME_CHARS: usize = 40;

/// A machine that checked in this recently is shown as connected. Machines
/// hold no connection open; by default each asks every 60 seconds, and this
/// allows it to miss one.
const CONNECTED_SECS: u64 = 150;

const SESSION_COOKIE: &str = "mister_cloud_saves_user";
/// Browsers cap a cookie's life at about 400 days. Every visit starts the
/// count again, so a browser that is used at all is never asked twice.
const SESSION_DAYS: i64 = 400;

pub fn routes() -> Vec<rocket::Route> {
    routes![
        page,
        alpine,
        login,
        session,
        logout,
        overview,
        events,
        resolve_quarantine,
        set_no_sync,
        rename_device,
        download
    ]
}

#[get("/")]
fn page() -> RawHtml<&'static str> {
    RawHtml(include_str!("web.html"))
}

#[derive(Responder)]
struct Script {
    body: RawJavaScript<&'static str>,
    cache: Header<'static>,
}

/// Alpine.js, which the page is written in. It is served from the binary so
/// the page works on a network with no way out to a CDN.
#[get("/alpine.min.js")]
fn alpine() -> Script {
    Script {
        body: RawJavaScript(include_str!("alpine.min.js")),
        cache: Header::new("Cache-Control", "public, max-age=86400"),
    }
}

#[derive(Serialize)]
struct DeviceRow {
    id: String,
    name: String,
    last_seen: u64,
    connected: bool,
    /// Saves, as `<core>/<name>`, whose current copy this machine has yet to
    /// take. None while the server has no record of what the machine holds.
    behind: Option<Vec<String>>,
}

/// Hashes go out as hex strings: a u64 doesn't survive a JavaScript number.
#[derive(Serialize)]
struct SaveRow {
    core: String,
    name: String,
    save_type: SaveFileType,
    hash: String,
    modified_index: u64,
    updated_at: Option<u64>,
    updated_by: Option<String>,
    no_sync: bool,
}

#[derive(Serialize)]
struct QuarantineRow {
    core: String,
    name: String,
    save_type: SaveFileType,
    device_id: String,
    device_name: String,
    hash: String,
    size: u64,
    first_at: u64,
    updated_at: u64,
    /// The save every other machine has. None if it has since vanished.
    current: Option<SaveRow>,
    /// Decompressed size of the current save.
    current_size: Option<u64>,
}

#[derive(Serialize)]
struct OverrideRow {
    core: String,
    name: String,
    device_name: String,
    at: u64,
}

#[derive(Serialize)]
struct NoSyncRow {
    core: String,
    name: String,
    save_type: SaveFileType,
    at: u64,
}

#[derive(Serialize)]
struct Overview {
    devices: Vec<DeviceRow>,
    quarantine: Vec<QuarantineRow>,
    overrides: Vec<OverrideRow>,
    no_sync: Vec<NoSyncRow>,
    saves: Vec<SaveRow>,
}

fn user_exists(user_id: &str) -> bool {
    safe_component(user_id) && PathBuf::from(format!("user_saves/{}", user_id)).is_dir()
}

fn default_device_name(id: &str) -> String {
    format!("MiSTer-{}", id.chars().take(4).collect::<String>())
}

fn device_name(db: &Database, user_id: &str, device_id: &str) -> String {
    database::get::<DeviceEntry>(&db.devices, &format!("{}/{}", user_id, device_id))
        .and_then(|d| d.name)
        .unwrap_or_else(|| default_device_name(device_id))
}

/// When the file holding the current copy was last written. Saves stored
/// before origins were recorded have nothing else to date them by.
fn stored_at(user_id: &str, save: &SaveFile) -> Option<u64> {
    let folder = folder_for(&save.save_type)?;
    let modified = std::fs::metadata(head_path(user_id, folder, &save.core, &save.name))
        .ok()?
        .modified()
        .ok()?;
    modified.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs())
}

fn save_row(db: &Database, user_id: &str, save: &SaveFile) -> SaveRow {
    let key = database::save_key(user_id, &save.core, &save.name);
    let origin: Option<Origin> = database::get(&db.origins, &key);

    SaveRow {
        core: save.core.clone(),
        name: save.name.clone(),
        save_type: save.save_type.clone(),
        hash: fmt_hash(save.hash),
        modified_index: save.modified_index,
        updated_at: origin
            .as_ref()
            .map(|o| o.at)
            .or_else(|| stored_at(user_id, save)),
        updated_by: origin
            .and_then(|o| o.device_id)
            .map(|id| device_name(db, user_id, &id)),
        no_sync: db.no_sync.contains_key(&key).unwrap_or(false),
    }
}

/// The saves `device` has yet to take the current copy of: those another
/// machine has changed since the server began recording what this one holds,
/// or whose copy here it was told to replace, and that it has not been sent.
/// A copy of its own held for a decision is a conflict, not a save it is
/// behind on.
fn behind(db: &Database, user_id: &str, device: &DeviceEntry, saves: &[&SaveFile]) -> Vec<String> {
    let mut names: Vec<String> = saves
        .iter()
        .filter(|save| {
            let key = database::save_key(user_id, &save.core, &save.name);
            let device_key =
                database::device_save_key(user_id, &save.core, &save.name, &device.id);

            let changed_since = database::get::<Origin>(&db.origins, &key)
                .is_some_and(|origin| origin.at >= device.since);
            let overridden = db.overrides.contains_key(&device_key).unwrap_or(false);

            (changed_since || overridden)
                && database::get::<u64>(&db.held, &device_key) != Some(save.hash)
                && !db.no_sync.contains_key(&key).unwrap_or(false)
                && !db.quarantine.contains_key(&device_key).unwrap_or(false)
        })
        .map(|save| format!("{}/{}", save.core, save.name))
        .collect();
    names.sort();
    names
}

async fn decompressed(path: &PathBuf) -> Option<Vec<u8>> {
    let compressed = tokio::fs::read(path).await.ok()?;
    zlib_decompress(&compressed).ok()
}

/// Not marked Secure: the server is normally reached over plain HTTP on a
/// home network, where such a cookie would never be sent back.
fn remember_user(cookies: &CookieJar<'_>, user_id: &str) {
    cookies.add(
        Cookie::build((SESSION_COOKIE, user_id.to_string()))
            .path("/")
            .http_only(true)
            .same_site(SameSite::Lax)
            .max_age(rocket::time::Duration::days(SESSION_DAYS)),
    );
}

#[derive(Deserialize)]
struct LoginRequest {
    user_id: String,
}

/// Checks the user id and has the browser remember it.
#[post("/api/login", data = "<request>")]
fn login(request: Json<LoginRequest>, cookies: &CookieJar<'_>) -> Status {
    let user_id = request.user_id.trim();
    if !user_exists(user_id) {
        return Status::NotFound;
    }

    remember_user(cookies, user_id);
    Status::Ok
}

/// The user id this browser is remembered as, if any. The page needs it to
/// address the other routes.
#[get("/api/session")]
fn session(cookies: &CookieJar<'_>) -> Json<Option<String>> {
    let user_id = cookies
        .get(SESSION_COOKIE)
        .map(|cookie| cookie.value().to_string())
        .filter(|id| user_exists(id));

    match &user_id {
        Some(id) => remember_user(cookies, id),
        None => cookies.remove(Cookie::build(SESSION_COOKIE).path("/")),
    }

    Json(user_id)
}

#[post("/api/logout")]
fn logout(cookies: &CookieJar<'_>) -> Status {
    cookies.remove(Cookie::build(SESSION_COOKIE).path("/"));
    Status::Ok
}

#[get("/api/<user_id>/overview")]
async fn overview(user_id: &str, db: &State<Arc<Database>>) -> Result<Json<Overview>, Status> {
    if !user_exists(user_id) {
        return Err(Status::NotFound);
    }

    let prefix = format!("{}/", user_id);
    let data = db.get_user_save_data(user_id).ok_or(Status::NotFound)?;

    let current: Vec<&SaveFile> = data
        .game_saves
        .values()
        .chain(data.save_states.values())
        .chain(data.nv_ram.values())
        .collect();

    let mut devices: Vec<DeviceRow> = database::scan::<DeviceEntry>(&db.devices, &prefix)
        .into_iter()
        .map(|d| DeviceRow {
            connected: now().saturating_sub(d.last_seen) <= CONNECTED_SECS,
            behind: (d.since != 0).then(|| behind(db, user_id, &d, &current)),
            name: d.name.unwrap_or_else(|| default_device_name(&d.id)),
            id: d.id,
            last_seen: d.last_seen,
        })
        .collect();
    devices.sort_by(|a, b| a.name.cmp(&b.name));

    let mut quarantine = Vec::new();
    for entry in database::scan::<QuarantineEntry>(&db.quarantine, &prefix) {
        let current = db.get_save_file(user_id, &entry.core, &entry.name);
        let current_size = match (&current, folder_for(&entry.save_type)) {
            (Some(_), Some(folder)) => {
                decompressed(&head_path(user_id, folder, &entry.core, &entry.name))
                    .await
                    .map(|bytes| bytes.len() as u64)
            }
            _ => None,
        };

        quarantine.push(QuarantineRow {
            device_name: device_name(db, user_id, &entry.device_id),
            current: current.as_ref().map(|save| save_row(db, user_id, save)),
            current_size,
            core: entry.core,
            name: entry.name,
            save_type: entry.save_type,
            device_id: entry.device_id,
            hash: fmt_hash(entry.hash),
            size: entry.size,
            first_at: entry.first_at,
            updated_at: entry.updated_at,
        });
    }
    quarantine.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

    let overrides = database::scan::<OverrideEntry>(&db.overrides, &prefix)
        .into_iter()
        .map(|o| OverrideRow {
            device_name: device_name(db, user_id, &o.device_id),
            core: o.core,
            name: o.name,
            at: o.at,
        })
        .collect();

    let mut no_sync: Vec<NoSyncRow> = database::scan::<NoSyncEntry>(&db.no_sync, &prefix)
        .into_iter()
        .map(|n| NoSyncRow {
            core: n.core,
            name: n.name,
            save_type: n.save_type,
            at: n.at,
        })
        .collect();
    no_sync.sort_by(|a, b| (&a.core, &a.name).cmp(&(&b.core, &b.name)));

    let mut saves: Vec<SaveRow> = current
        .iter()
        .map(|save| save_row(db, user_id, save))
        .collect();
    saves.sort_by(|a, b| (&a.core, &a.name).cmp(&(&b.core, &b.name)));

    Ok(Json(Overview {
        devices,
        quarantine,
        overrides,
        no_sync,
        saves,
    }))
}

/// Sends a message each time something the overview shows changes for this
/// user, so an open page reloads it at once instead of at its next poll.
/// Machines checking in are left out: they would fire it on every sync, so
/// one coming or going shows at the page's next poll.
#[get("/api/<user_id>/events")]
fn events(
    user_id: &str,
    db: &State<Arc<Database>>,
    mut shutdown: Shutdown,
) -> Result<EventStream![], Status> {
    if !user_exists(user_id) {
        return Err(Status::NotFound);
    }

    let prefix = format!("{}/", user_id);
    let mut origins = db.origins.watch_prefix(prefix.as_bytes());
    let mut quarantine = db.quarantine.watch_prefix(prefix.as_bytes());
    let mut overrides = db.overrides.watch_prefix(prefix.as_bytes());
    let mut no_sync = db.no_sync.watch_prefix(prefix.as_bytes());
    let mut held = db.held.watch_prefix(prefix.as_bytes());

    Ok(EventStream! {
        loop {
            let change = tokio::select! {
                change = &mut origins => change,
                change = &mut quarantine => change,
                change = &mut overrides => change,
                change = &mut no_sync => change,
                change = &mut held => change,
                _ = &mut shutdown => None,
            };
            if change.is_none() {
                break;
            }
            yield Event::data("changed");
        }
    })
}

#[derive(Deserialize)]
struct QuarantineRequest {
    core: String,
    name: String,
    device_id: String,
}

/// `promote` makes the quarantined copy the current save, which every other
/// machine then downloads. `discard` deletes it and tells the machine it
/// came from to take the current save over whatever it holds by then.
#[post("/api/<user_id>/quarantine/<action>", data = "<request>")]
async fn resolve_quarantine(
    user_id: &str,
    action: &str,
    request: Json<QuarantineRequest>,
    db: &State<Arc<Database>>,
    lock: &State<WriteLock>,
) -> Status {
    if !user_exists(user_id)
        || !safe_component(&request.core)
        || !safe_component(&request.name)
        || !valid_device_id(&request.device_id)
    {
        return Status::NotFound;
    }

    let _guard = lock.0.lock().await;

    let key = database::device_save_key(user_id, &request.core, &request.name, &request.device_id);
    let Some(entry) = database::get::<QuarantineEntry>(&db.quarantine, &key) else {
        return Status::NotFound;
    };
    let Some(folder) = folder_for(&entry.save_type) else {
        return Status::NotFound;
    };
    let path = quarantine_path(
        user_id,
        &entry.device_id,
        folder,
        &entry.core,
        &entry.name,
    );
    let current = db.get_save_file(user_id, &entry.core, &entry.name);

    match action {
        "promote" => {
            let compressed = match tokio::fs::read(&path).await {
                Ok(bytes) => bytes,
                Err(e) => {
                    log_warn!(
                        "{}/{}: quarantined copy {} could not be read: {:?}",
                        entry.core,
                        entry.name,
                        path.display(),
                        e
                    );
                    return Status::InternalServerError;
                }
            };

            // Above both indexes, so clients that still decide by
            // modified_index take it too.
            let modified_index = current
                .as_ref()
                .map_or(entry.modified_index, |c| {
                    c.modified_index.max(entry.modified_index) + 1
                });

            let save = SaveFile {
                name: entry.name.clone(),
                save_type: entry.save_type.clone(),
                core: entry.core.clone(),
                hash: entry.hash,
                modified_index,
                user_id: user_id.to_string(),
                data: None,
                base_hash: None,
            };

            log_warn!(
                "{}/{}: quarantined copy from device {} (hash {}) made the current save in the \
                 web interface, replacing hash {}; modified_index {}",
                entry.core,
                entry.name,
                entry.device_id,
                fmt_hash(entry.hash),
                current.map_or("<none>".to_string(), |c| fmt_hash(c.hash)),
                modified_index
            );

            let status = store_current(
                db,
                user_id,
                folder,
                &save,
                &compressed,
                Some(&entry.device_id),
            )
            .await;
            if status != Status::Ok {
                return status;
            }

            database::remove(&db.quarantine, &key);
            database::remove(&db.overrides, &key);
            let _ = tokio::fs::remove_file(&path).await;
            Status::Ok
        }
        "discard" => {
            log_warn!(
                "{}/{}: quarantined copy from device {} (hash {}) discarded in the web interface; \
                 that machine will be made to take the current save",
                entry.core,
                entry.name,
                entry.device_id,
                fmt_hash(entry.hash)
            );

            // The order matters if this is interrupted: an override with the
            // quarantined copy still listed can be discarded again, while a
            // missing override would let the machine re-quarantine its copy.
            if current.is_some() {
                let override_entry = OverrideEntry {
                    core: entry.core.clone(),
                    name: entry.name.clone(),
                    save_type: entry.save_type.clone(),
                    device_id: entry.device_id.clone(),
                    at: now(),
                };
                if !database::put(&db.overrides, &key, &override_entry) {
                    return Status::InternalServerError;
                }
            }

            database::remove(&db.quarantine, &key);
            let _ = tokio::fs::remove_file(&path).await;
            Status::Ok
        }
        _ => Status::NotFound,
    }
}

#[derive(Deserialize)]
struct NoSyncRequest {
    core: String,
    name: String,
    save_type: SaveFileType,
    enabled: bool,
}

/// Turning this on stops every machine uploading or downloading the save.
/// Each keeps the copy it has; the server's copy stays as it is. Quarantined
/// copies of it are dropped, since there is no longer one save to reconcile
/// them with.
#[post("/api/<user_id>/no_sync", data = "<request>")]
async fn set_no_sync(
    user_id: &str,
    request: Json<NoSyncRequest>,
    db: &State<Arc<Database>>,
    lock: &State<WriteLock>,
) -> Status {
    if !user_exists(user_id) || !safe_component(&request.core) || !safe_component(&request.name) {
        return Status::NotFound;
    }
    let Some(folder) = folder_for(&request.save_type) else {
        return Status::BadRequest;
    };

    let _guard = lock.0.lock().await;
    let key = database::save_key(user_id, &request.core, &request.name);

    if !request.enabled {
        database::remove(&db.no_sync, &key);
        log_info!(
            "{}/{}: syncing resumed in the web interface",
            request.core,
            request.name
        );
        return Status::Ok;
    }

    let entry = NoSyncEntry {
        core: request.core.clone(),
        name: request.name.clone(),
        save_type: request.save_type.clone(),
        at: now(),
    };
    if !database::put(&db.no_sync, &key, &entry) {
        return Status::InternalServerError;
    }

    let per_device = format!("{}/", key);
    for held in database::scan::<QuarantineEntry>(&db.quarantine, &per_device) {
        let held_key =
            database::device_save_key(user_id, &held.core, &held.name, &held.device_id);
        database::remove(&db.quarantine, &held_key);
        let path = quarantine_path(user_id, &held.device_id, folder, &held.core, &held.name);
        let _ = tokio::fs::remove_file(&path).await;
    }
    for pending in database::scan::<OverrideEntry>(&db.overrides, &per_device) {
        let pending_key =
            database::device_save_key(user_id, &pending.core, &pending.name, &pending.device_id);
        database::remove(&db.overrides, &pending_key);
    }

    log_warn!(
        "{}/{}: set not to sync in the web interface; no machine will upload or download it",
        request.core,
        request.name
    );
    Status::Ok
}

#[derive(Deserialize)]
struct RenameRequest {
    device_id: String,
    name: String,
}

#[post("/api/<user_id>/device", data = "<request>")]
async fn rename_device(
    user_id: &str,
    request: Json<RenameRequest>,
    db: &State<Arc<Database>>,
) -> Status {
    if !user_exists(user_id) || !valid_device_id(&request.device_id) {
        return Status::NotFound;
    }

    let key = format!("{}/{}", user_id, request.device_id);
    let Some(mut entry) = database::get::<DeviceEntry>(&db.devices, &key) else {
        return Status::NotFound;
    };

    let name: String = request
        .name
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_DEVICE_NAME_CHARS)
        .collect();
    entry.name = if name.is_empty() { None } else { Some(name) };

    if database::put(&db.devices, &key, &entry) {
        Status::Ok
    } else {
        Status::InternalServerError
    }
}

#[derive(Responder)]
#[response(content_type = "binary")]
struct Download {
    bytes: Vec<u8>,
    disposition: Header<'static>,
}

/// The save as a MiSTer would write it to its SD card, to inspect or keep
/// before deciding. With `device` it is that machine's quarantined copy,
/// without it the current save.
#[get("/api/<user_id>/download?<core>&<name>&<device>")]
async fn download(
    user_id: &str,
    core: &str,
    name: &str,
    device: Option<&str>,
    db: &State<Arc<Database>>,
) -> Result<Download, Status> {
    if !user_exists(user_id) || !safe_component(core) || !safe_component(name) {
        return Err(Status::NotFound);
    }

    let path = match device {
        Some(device_id) => {
            if !valid_device_id(device_id) {
                return Err(Status::NotFound);
            }
            let key = database::device_save_key(user_id, core, name, device_id);
            let entry: QuarantineEntry =
                database::get(&db.quarantine, &key).ok_or(Status::NotFound)?;
            let folder = folder_for(&entry.save_type).ok_or(Status::NotFound)?;
            quarantine_path(user_id, device_id, folder, core, name)
        }
        None => {
            let save = db
                .get_save_file(user_id, core, name)
                .ok_or(Status::NotFound)?;
            let folder = folder_for(&save.save_type).ok_or(Status::NotFound)?;
            head_path(user_id, folder, core, name)
        }
    };

    let bytes = decompressed(&path).await.ok_or(Status::NotFound)?;

    // The name goes into a quoted header value.
    let file_name: String = name
        .chars()
        .map(|c| {
            if c.is_ascii() && !c.is_ascii_control() && c != '"' && c != '\\' {
                c
            } else {
                '_'
            }
        })
        .collect();

    Ok(Download {
        bytes,
        disposition: Header::new(
            "Content-Disposition",
            format!("attachment; filename=\"{}\"", file_name),
        ),
    })
}
