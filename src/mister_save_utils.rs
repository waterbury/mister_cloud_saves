use flate2::read::ZlibDecoder;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use xxhash_rust::xxh3::xxh3_64;

pub mod logging;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
pub enum SaveFileType {
    GameSave,
    SaveState,
    CoreWatch,
    NvRam,
}

impl Default for SaveFileType {
    fn default() -> Self {
        SaveFileType::GameSave
    }
}

pub struct SaveCategory<'a> {
    pub save_type: SaveFileType,
    pub local_map: &'a mut HashMap<String, SaveFile>,
    pub remote_map: &'a HashMap<String, SaveFile>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum ConflictAction {
    KeepLocal,
    KeepRemote,
    KeepLocalAll,
    KeepRemoteAll,
    /// Keep the local copy on this machine and hand it to the server as a
    /// quarantined copy, to be decided on later in the web interface.
    Quarantine,
    AskUser,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct SaveFile {
    pub name: String,
    pub save_type: SaveFileType,
    pub core: String,
    pub hash: u64,
    pub modified_index: u64,
    pub user_id: String,
    pub data: Option<Vec<u8>>,
    /// In a client's save map: hash of the content this machine last had in
    /// common with the server's current save. None until the save has been
    /// synced once. A copy the server only took into quarantine does not
    /// count: the base stays where the two lines parted.
    ///
    /// On an upload: the content the upload was derived from. The server
    /// takes the upload as the new current save only when this matches what
    /// it currently has; anything else is a conflict and goes to quarantine.
    #[serde(default)]
    pub base_hash: Option<u64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct UserSaveData {
    pub user_id: String,
    pub game_saves: HashMap<String, SaveFile>,
    pub save_states: HashMap<String, SaveFile>,

    #[serde(default)]
    pub nv_ram: HashMap<String, SaveFile>,

    /// Format of a client's save map. 0 is a map written before `base_hash`
    /// existed, whose entries are reconciled once by modified_index.
    #[serde(default)]
    pub map_version: u32,
}

/// `map_version` written by this client.
pub const SAVE_MAP_VERSION: u32 = 1;

/// Request header naming the machine a client runs on. A client that sends
/// it gets conflicting uploads quarantined rather than rejected.
pub const DEVICE_ID_HEADER: &str = "X-Device-Id";

/// Names one save without carrying its metadata.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
pub struct SaveRef {
    pub core: String,
    pub name: String,
    pub save_type: SaveFileType,
}

impl SaveRef {
    pub fn key(&self) -> String {
        format!("{}/{}", self.core, self.name)
    }
}

/// A copy the server is holding in quarantine for the requesting machine.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct QuarantinedSave {
    pub core: String,
    pub name: String,
    pub save_type: SaveFileType,
    pub hash: u64,
}

/// What `/fetch_user_data` returns: the save metadata, plus the decisions
/// made in the web interface that the requesting machine has to honor. The
/// extra fields default to empty so a server from before they existed still
/// parses.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerState {
    #[serde(flatten)]
    pub saves: UserSaveData,
    /// False from a server that predates quarantine and resolves conflicts
    /// by modified_index alone.
    #[serde(default)]
    pub supports_quarantine: bool,
    /// Saves no machine may upload or download.
    #[serde(default)]
    pub no_sync: Vec<SaveRef>,
    /// The requesting machine's copies held in quarantine.
    #[serde(default)]
    pub quarantined: Vec<QuarantinedSave>,
    /// Saves where the requesting machine's copy was discarded: it must take
    /// the server's copy whatever it holds locally, then acknowledge.
    #[serde(default)]
    pub overrides: Vec<SaveRef>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Hash, PartialEq, Eq)]
pub struct FetchSaveRequest {
    pub user_id: String,
    pub core: String,
    pub name: String,
    pub save_type: SaveFileType,
    pub modified_index: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, Hash, PartialEq, Eq)]
pub struct UploadSaveRequest {
    pub path: PathBuf,
    pub save_type: SaveFileType,
    pub modified_index: u64,
    pub server_url: String,
    pub user_id: String,
}

pub async fn read_file_to_bytes(path: &Path) -> std::io::Result<Vec<u8>> {
    let file_bytes = tokio::fs::read(path).await?;
    Ok(file_bytes)
}

pub async fn hash_file(
    path: &Path,
    file_data: Option<&[u8]>,
) -> Result<u64, Box<dyn std::error::Error>> {
    let file_bytes = match file_data {
        Some(data) => data.to_vec(),
        None => read_file_to_bytes(path).await?,
    };
    let hash: u64 = xxh3_64(&file_bytes);

    Ok(hash)
}

pub fn hash_bytes(data: &[u8]) -> u64 {
    xxh3_64(data)
}

pub fn hashes_equal(hash1: u64, hash2: u64) -> bool {
    hash1 == hash2
}

pub fn zlib_decompress(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut decoder = ZlibDecoder::new(data);
    let mut decompressed = Vec::new();
    decoder.read_to_end(&mut decompressed)?;
    Ok(decompressed)
}

/// Size and mtime of a file, for logging. mtime answers "what touched this
/// save?" questions that a hash alone cannot.
#[derive(Debug, Clone, Copy, Default)]
pub struct FileStat {
    pub size: u64,
    pub mtime_secs: u64,
}

impl std::fmt::Display for FileStat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} bytes, mtime {}",
            self.size,
            logging::format_unix_time(self.mtime_secs, 0)
        )
    }
}

pub async fn file_stat(path: &Path) -> Option<FileStat> {
    let metadata = tokio::fs::metadata(path).await.ok()?;
    let mtime_secs = metadata
        .modified()
        .ok()
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());

    Some(FileStat {
        size: metadata.len(),
        mtime_secs,
    })
}

pub fn fmt_stat_opt(stat: Option<FileStat>) -> String {
    match stat {
        Some(s) => s.to_string(),
        None => "<file absent>".to_string(),
    }
}

pub fn is_hidden_path(path: &Path) -> bool {
    path.components().any(|component| {
        if let std::path::Component::Normal(os_str) = component {
            os_str.as_bytes().starts_with(b".")
        } else {
            false
        }
    })
}
