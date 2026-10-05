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
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct UserSaveData {
    pub user_id: String,
    pub game_saves: HashMap<String, SaveFile>,
    pub save_states: HashMap<String, SaveFile>,

    #[serde(default)]
    pub nv_ram: HashMap<String, SaveFile>,
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
