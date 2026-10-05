//! Small, dependency free logger shared by the client and the server.
//!
//! The MiSTer launcher starts the client with its stdout pointed at
//! /dev/null, so a log file is the only durable record of what the tool did to
//! a save file. Every line is written to stdout *and* appended to the log file
//! when one is configured, with simple size based rotation so the SD card
//! cannot fill up.
//!
//! Environment overrides:
//!   MISTER_SAVE_LOG_LEVEL      error | warn | info | debug   (default: info)
//!   MISTER_SAVE_LOG_FILE       path to log file, empty string disables
//!   MISTER_SAVE_LOG_MAX_BYTES  rotate above this size, 0 disables rotation

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_MAX_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
}

impl LogLevel {
    fn label(self) -> &'static str {
        match self {
            LogLevel::Error => "ERROR",
            LogLevel::Warn => "WARN ",
            LogLevel::Info => "INFO ",
            LogLevel::Debug => "DEBUG",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "error" => Some(LogLevel::Error),
            "warn" | "warning" => Some(LogLevel::Warn),
            "info" => Some(LogLevel::Info),
            "debug" | "verbose" | "trace" => Some(LogLevel::Debug),
            _ => None,
        }
    }
}

struct Logger {
    component: String,
    level: LogLevel,
    path: Option<PathBuf>,
    file: Option<File>,
    size: u64,
    max_bytes: u64,
}

static LOGGER: OnceLock<Mutex<Logger>> = OnceLock::new();

fn logger() -> &'static Mutex<Logger> {
    LOGGER.get_or_init(|| {
        Mutex::new(Logger {
            component: "mister_cloud_saves".to_string(),
            level: LogLevel::Info,
            path: None,
            file: None,
            size: 0,
            max_bytes: DEFAULT_MAX_BYTES,
        })
    })
}

/// Configure the logger. `default_file` is used unless MISTER_SAVE_LOG_FILE
/// overrides it; pass `None` to log to stdout only.
pub fn init(component: &str, default_file: Option<&str>) {
    let level = std::env::var("MISTER_SAVE_LOG_LEVEL")
        .ok()
        .and_then(|v| LogLevel::parse(&v))
        .unwrap_or(LogLevel::Info);

    let path = match std::env::var("MISTER_SAVE_LOG_FILE") {
        // An explicitly empty value is how you turn file logging off.
        Ok(v) if v.trim().is_empty() => None,
        Ok(v) => Some(PathBuf::from(v.trim())),
        Err(_) => default_file.map(PathBuf::from),
    };

    let max_bytes = std::env::var("MISTER_SAVE_LOG_MAX_BYTES")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_MAX_BYTES);

    if let Ok(mut logger) = logger().lock() {
        logger.component = component.to_string();
        logger.level = level;
        logger.path = path;
        logger.file = None;
        logger.size = 0;
        logger.max_bytes = max_bytes;
    }
}

/// Where the log is being written, for printing to the user.
pub fn log_file_path() -> Option<PathBuf> {
    logger().lock().ok().and_then(|l| l.path.clone())
}

pub fn enabled(level: LogLevel) -> bool {
    logger().lock().map_or(false, |l| level <= l.level)
}

pub fn log(level: LogLevel, message: &str) {
    let Ok(mut logger) = logger().lock() else {
        return;
    };

    if level > logger.level {
        return;
    }

    let line = format!(
        "{} [{}] {}: {}",
        timestamp(),
        level.label(),
        logger.component,
        message
    );

    println!("{}", line);
    let _ = std::io::stdout().flush();
    logger.append(&line);
}

impl Logger {
    fn append(&mut self, line: &str) {
        let Some(path) = self.path.clone() else {
            return;
        };

        // +1 for the newline.
        let incoming = line.len() as u64 + 1;

        if self.file.is_none() && !self.open(&path) {
            return;
        }

        if self.max_bytes > 0 && self.size + incoming > self.max_bytes {
            self.file = None;
            let rotated = PathBuf::from(format!("{}.1", path.display()));
            let _ = fs::rename(&path, &rotated);
            if !self.open(&path) {
                return;
            }
        }

        if let Some(file) = self.file.as_mut() {
            if writeln!(file, "{}", line).is_ok() {
                self.size += incoming;
            } else {
                // Keep running without file logging rather than spamming.
                self.file = None;
                self.path = None;
            }
        }
    }

    fn open(&mut self, path: &PathBuf) -> bool {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = fs::create_dir_all(parent);
            }
        }

        match OpenOptions::new().create(true).append(true).open(path) {
            Ok(file) => {
                self.size = file.metadata().map(|m| m.len()).unwrap_or(0);
                self.file = Some(file);
                true
            }
            Err(e) => {
                eprintln!("logging: cannot open log file {}: {:?}", path.display(), e);
                self.path = None;
                false
            }
        }
    }
}

/// Hashes are xxh3-64; print them as fixed width hex so they are easy to grep
/// and to eyeball against each other.
pub fn fmt_hash(hash: u64) -> String {
    format!("{:016x}", hash)
}

pub fn fmt_hash_opt(hash: Option<u64>) -> String {
    match hash {
        Some(h) => fmt_hash(h),
        None => "<none>".to_string(),
    }
}

pub fn fmt_index_opt(index: Option<u64>) -> String {
    match index {
        Some(i) => i.to_string(),
        None => "<none>".to_string(),
    }
}

pub fn timestamp() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format_unix_time(now.as_secs(), now.subsec_millis())
}

/// Render a unix timestamp as a UTC date so log lines are readable without
/// pulling in a date/time crate.
pub fn format_unix_time(secs: u64, millis: u32) -> String {
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let (year, month, day) = civil_from_days(days);

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year,
        month,
        day,
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60,
        millis
    )
}

/// Howard Hinnant's days-from-civil inverse, public domain algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]

    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        $crate::logging::log($crate::logging::LogLevel::Error, &format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        $crate::logging::log($crate::logging::LogLevel::Warn, &format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::logging::log($crate::logging::LogLevel::Info, &format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_debug {
    ($($arg:tt)*) => {
        if $crate::logging::enabled($crate::logging::LogLevel::Debug) {
            $crate::logging::log($crate::logging::LogLevel::Debug, &format!($($arg)*))
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_unix_time_as_utc() {
        assert_eq!(format_unix_time(0, 0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            format_unix_time(1_700_000_000, 42),
            "2023-11-14T22:13:20.042Z"
        );
        assert_eq!(
            format_unix_time(1_762_318_800, 0),
            "2025-11-05T05:00:00.000Z"
        );
        // Leap day, and a non-leap century boundary.
        assert_eq!(format_unix_time(951_782_400, 0), "2000-02-29T00:00:00.000Z");
        assert_eq!(
            format_unix_time(4_102_444_800, 0),
            "2100-01-01T00:00:00.000Z"
        );
    }

    #[test]
    fn formats_hashes_as_fixed_width_hex() {
        assert_eq!(fmt_hash(0), "0000000000000000");
        assert_eq!(fmt_hash(0x2278869cc9ae6d47), "2278869cc9ae6d47");
        assert_eq!(fmt_hash_opt(None), "<none>");
        assert_eq!(fmt_index_opt(Some(7)), "7");
        assert_eq!(fmt_index_opt(None), "<none>");
    }

    #[test]
    fn parses_log_levels() {
        assert_eq!(LogLevel::parse("INFO"), Some(LogLevel::Info));
        assert_eq!(LogLevel::parse(" debug "), Some(LogLevel::Debug));
        assert_eq!(LogLevel::parse("warning"), Some(LogLevel::Warn));
        assert_eq!(LogLevel::parse("nonsense"), None);
    }

    /// One test drives the whole file logger: the logger is process global, so
    /// splitting this up would just make the tests race each other.
    #[test]
    fn writes_filters_and_rotates() {
        let dir = std::env::temp_dir().join(format!(
            "mister_save_log_test_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let path = dir.join("nested").join("test.log");

        // init() reads the environment, so drive it through the env the same
        // way a real deployment would.
        unsafe {
            std::env::set_var("MISTER_SAVE_LOG_FILE", &path);
            std::env::set_var("MISTER_SAVE_LOG_LEVEL", "info");
            std::env::set_var("MISTER_SAVE_LOG_MAX_BYTES", "0");
        }
        init("test", None);

        assert_eq!(log_file_path().as_deref(), Some(path.as_path()));
        assert!(enabled(LogLevel::Info));
        assert!(!enabled(LogLevel::Debug));

        log_info!("hash {} -> {}", fmt_hash(1), fmt_hash(2));
        log_debug!("this line is below the configured level");

        let contents = fs::read_to_string(&path).expect("log file should exist");
        assert!(
            contents.contains("0000000000000001 -> 0000000000000002"),
            "missing info line: {contents}"
        );
        assert!(
            !contents.contains("below the configured level"),
            "debug line should have been filtered: {contents}"
        );
        assert!(contents.contains("[INFO ] test:"), "bad prefix: {contents}");

        // Now rotate: a tiny cap forces the first write to roll the file.
        unsafe {
            std::env::set_var("MISTER_SAVE_LOG_MAX_BYTES", "80");
        }
        init("test", None);

        for i in 0..6 {
            log_info!("rotation filler line number {}", i);
        }

        let rotated = PathBuf::from(format!("{}.1", path.display()));
        assert!(rotated.exists(), "rotated file should exist");
        let live = fs::read_to_string(&path).expect("live log should exist");
        assert!(
            live.len() <= 160,
            "live log should have been rotated, got {} bytes",
            live.len()
        );
        assert!(live.contains("rotation filler line number 5"));

        // An empty MISTER_SAVE_LOG_FILE disables file logging entirely.
        unsafe {
            std::env::set_var("MISTER_SAVE_LOG_FILE", "");
        }
        init("test", Some("/should/not/be/used.log"));
        assert_eq!(log_file_path(), None);
        log_info!("goes to stdout only");

        unsafe {
            std::env::remove_var("MISTER_SAVE_LOG_FILE");
            std::env::remove_var("MISTER_SAVE_LOG_LEVEL");
            std::env::remove_var("MISTER_SAVE_LOG_MAX_BYTES");
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
