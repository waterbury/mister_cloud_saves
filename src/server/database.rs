use mister_save_utils::{SaveFile, SaveFileType, UserSaveData};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use sled::{Db, Tree};

/// A conflicting upload held apart from the current save until somebody
/// decides in the web interface what to do with it. One per save per machine;
/// further conflicting uploads from that machine replace it.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct QuarantineEntry {
    pub core: String,
    pub name: String,
    pub save_type: SaveFileType,
    pub hash: u64,
    pub modified_index: u64,
    pub device_id: String,
    /// Decompressed size in bytes.
    pub size: u64,
    pub first_at: u64,
    pub updated_at: u64,
}

/// A quarantined copy was discarded: `device_id` has to replace whatever it
/// holds for this save with the server's copy.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OverrideEntry {
    pub core: String,
    pub name: String,
    pub save_type: SaveFileType,
    pub device_id: String,
    pub at: u64,
}

/// A save no machine syncs.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct NoSyncEntry {
    pub core: String,
    pub name: String,
    pub save_type: SaveFileType,
    pub at: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DeviceEntry {
    pub id: String,
    /// Set in the web interface; None shows a name derived from the id.
    pub name: Option<String>,
    pub last_seen: u64,
    /// When the server began recording which saves this machine holds. Only
    /// saves that changed after it can be told to be missing from it. 0 for a
    /// machine that has not checked in since the server learned to.
    #[serde(default)]
    pub since: u64,
}

/// Where the current copy of a save came from.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Origin {
    pub device_id: Option<String>,
    pub at: u64,
}

#[allow(dead_code)]
pub struct Database {
    db: Db,
    user_saves_tree: Tree,
    /// Keyed `<user>/<core>/<name>/<device>`.
    pub quarantine: Tree,
    /// Keyed `<user>/<core>/<name>/<device>`.
    pub overrides: Tree,
    /// Keyed `<user>/<core>/<name>`.
    pub no_sync: Tree,
    /// Keyed `<user>/<device>`.
    pub devices: Tree,
    /// Keyed `<user>/<core>/<name>`.
    pub origins: Tree,
    /// The hash of the copy each machine last sent or was sent, keyed
    /// `<user>/<core>/<name>/<device>`.
    pub held: Tree,
}

pub fn save_key(user_id: &str, core: &str, name: &str) -> String {
    format!("{}/{}/{}", user_id, core, name)
}

pub fn device_save_key(user_id: &str, core: &str, name: &str, device_id: &str) -> String {
    format!("{}/{}/{}/{}", user_id, core, name, device_id)
}

pub fn get<T: DeserializeOwned>(tree: &Tree, key: &str) -> Option<T> {
    match tree.get(key) {
        Ok(Some(value)) => serde_json::from_slice(&value).ok(),
        _ => None,
    }
}

pub fn put<T: Serialize>(tree: &Tree, key: &str, value: &T) -> bool {
    match serde_json::to_vec(value) {
        Ok(bytes) => tree.insert(key, bytes).is_ok(),
        Err(_) => false,
    }
}

pub fn remove(tree: &Tree, key: &str) -> bool {
    matches!(tree.remove(key), Ok(Some(_)))
}

/// Every entry whose key starts with `prefix`. Entries that no longer parse
/// are skipped.
pub fn scan<T: DeserializeOwned>(tree: &Tree, prefix: &str) -> Vec<T> {
    tree.scan_prefix(prefix.as_bytes())
        .filter_map(|item| item.ok())
        .filter_map(|(_, value)| serde_json::from_slice(&value).ok())
        .collect()
}

impl Database {
    pub fn new(path: &str) -> sled::Result<Self> {
        let db = sled::open(path)?;
        let user_saves_tree = db.open_tree("user_saves_sled")?;
        let quarantine = db.open_tree("quarantine")?;
        let overrides = db.open_tree("overrides")?;
        let no_sync = db.open_tree("no_sync")?;
        let devices = db.open_tree("devices")?;
        let origins = db.open_tree("origins")?;
        let held = db.open_tree("held")?;
        Ok(Database {
            db,
            user_saves_tree,
            quarantine,
            overrides,
            no_sync,
            devices,
            origins,
            held,
        })
    }

    pub fn get_user_save_data(&self, user_id: &str) -> Option<UserSaveData> {
        let mut user_data = UserSaveData::default();
        user_data.user_id = user_id.to_string();

        let prefix = format!("{}/", user_id);
        let iter = self.user_saves_tree.scan_prefix(prefix.as_bytes());

        for item in iter {
            match item {
                Ok((_key_ivec, value_ivec)) => {
                    let save_file: SaveFile = match serde_json::from_slice(&value_ivec) {
                        Ok(data) => data,
                        Err(_) => continue,
                    };

                    let save_key = format!("{}/{}", save_file.core, save_file.name);

                    match save_file.save_type {
                        SaveFileType::GameSave => {
                            user_data.game_saves.insert(save_key.clone(), save_file);
                        }
                        SaveFileType::SaveState => {
                            user_data.save_states.insert(save_key.clone(), save_file);
                        }
                        SaveFileType::NvRam => {
                            user_data.nv_ram.insert(save_key.clone(), save_file);
                        }
                        _ => continue,
                    }
                }
                Err(_) => continue,
            }
        }

        Some(user_data)
    }

    /// The single stored entry for one save, used to report what an upload is
    /// about to replace.
    pub fn get_save_file(&self, user_id: &str, core: &str, name: &str) -> Option<SaveFile> {
        let save_key = format!("{}/{}/{}", user_id, core, name);

        match self.user_saves_tree.get(save_key) {
            Ok(Some(value)) => serde_json::from_slice(&value).ok(),
            _ => None,
        }
    }

    pub fn set_user_save_data(&self, user_id: &str, data: &SaveFile) -> Option<bool> {
        let save_key = format!("{}/{}/{}", user_id, data.core, data.name);

        let serialized_data = match serde_json::to_vec(data) {
            Ok(d) => d,
            Err(_) => return None,
        };

        match self.user_saves_tree.insert(save_key, serialized_data) {
            Ok(_) => Some(true),
            Err(_) => None,
        }
    }
}
