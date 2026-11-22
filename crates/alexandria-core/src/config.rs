use std::env;
use std::path::Path;

use serde::Deserialize;

use crate::errors::DomainError;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseSettings {
    #[serde(default = "default_database_path")]
    pub path: String,
}

fn default_database_path() -> String {
    "alexandria.sqlite".to_string()
}

impl Default for DatabaseSettings {
    fn default() -> Self {
        Self {
            path: default_database_path(),
        }
    }
}

// The connection URL is built in one place only — `migrate_database` — so the
// scheme, the `mode=rwc` flag, and Windows path handling cannot drift between
// two spellings. A second builder here previously went unused for exactly that
// reason.

#[derive(Debug, Clone, Deserialize)]
pub struct IndexingSettings {
    /// How many files UC-01's index walk and UC-02's re-index walk process at
    /// a time. The per-file cost is a stat plus, for a supported extension, an
    /// embedded-metadata read — no file bytes are hashed (FR-FC-09/FR-FC-10) — and
    /// both run on Tokio's blocking pool, so this is a real parallelism knob.
    /// The database half of each file's work still serializes behind SQLite's
    /// single writer and the pool's 8 connections, so values far above that
    /// buy nothing. Zero is clamped to 1 (sequential) by the handlers.
    #[serde(default = "default_indexing_concurrency")]
    pub concurrency: u32,
    /// How many files a `Low`-priority run (`RunPriority::Low`, FR-FC-08)
    /// processes at a time. A large scan started at low priority is meant to
    /// stay out of the way of browsing and playback rather than to finish
    /// fast, so the default is the narrowest useful width — sequential is 1,
    /// not 0, for the reason `concurrency` itself is never let land there.
    /// Zero is clamped to 1 by the handlers, exactly as `concurrency` is.
    #[serde(default = "default_indexing_low_priority_concurrency")]
    pub low_priority_concurrency: u32,
}

fn default_indexing_concurrency() -> u32 {
    4
}

fn default_indexing_low_priority_concurrency() -> u32 {
    1
}

impl Default for IndexingSettings {
    fn default() -> Self {
        Self {
            concurrency: default_indexing_concurrency(),
            low_priority_concurrency: default_indexing_low_priority_concurrency(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeletionSettings {
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
}

fn default_retention_days() -> u32 {
    30
}

impl Default for DeletionSettings {
    fn default() -> Self {
        Self {
            retention_days: default_retention_days(),
        }
    }
}

/// The on-disk library root the health check probes for reachability
/// (UC-37 / IR-03). Distinct from the per-request `root` UC-01/UC-02 take —
/// this is the single configured location an operator points the health
/// probe at.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct FilesystemSettings {
    #[serde(default)]
    pub root: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoggingSettings {
    #[serde(default = "default_log_level")]
    pub level: LogLevel,
}

fn default_log_level() -> LogLevel {
    LogLevel::Info
}

impl Default for LoggingSettings {
    fn default() -> Self {
        Self {
            level: default_log_level(),
        }
    }
}

/// Media playback settings (F-10).
#[derive(Debug, Clone, Deserialize)]
pub struct PlaybackSettings {
    /// Directory holding generated thumbnails (UC-40), created on first
    /// use. Relative by default, matching `database.path`.
    ///
    /// Cache entries are keyed by uuid and mtime (see
    /// `playback::thumbnail`), so a file that changes on disk invalidates its
    /// thumbnail for free. Not by content hash, which is what this said until
    /// indexing stopped computing one (FR-FC-09): keying on a hash would make
    /// the first thumbnail of a multi-gigabyte video pay to read the whole
    /// file. There is no eviction policy: inventing one before anyone has a
    /// full cache would be guessing.
    #[serde(default = "default_thumbnail_cache_dir")]
    pub thumbnail_cache_dir: String,
}

fn default_thumbnail_cache_dir() -> String {
    "thumbnails".to_string()
}

impl Default for PlaybackSettings {
    fn default() -> Self {
        Self {
            thumbnail_cache_dir: default_thumbnail_cache_dir(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Settings {
    #[serde(default)]
    pub database: DatabaseSettings,
    #[serde(default)]
    pub indexing: IndexingSettings,
    #[serde(default)]
    pub deletion: DeletionSettings,
    #[serde(default)]
    pub logging: LoggingSettings,
    #[serde(default)]
    pub filesystem: FilesystemSettings,
    #[serde(default)]
    pub playback: PlaybackSettings,
}

impl Settings {
    pub fn load(path: &Path) -> Result<Self, DomainError> {
        let contents = std::fs::read_to_string(path).map_err(|e| {
            DomainError::Config(format!("failed to read config {}: {}", path.display(), e))
        })?;
        let mut settings: Settings = toml::from_str(&contents)
            .map_err(|e| DomainError::Config(format!("failed to parse config: {e}")))?;
        settings.apply_env_overrides();
        Ok(settings)
    }

    pub fn load_or_default(path: &Path) -> Self {
        match Self::load(path) {
            Ok(mut settings) => {
                settings.apply_env_overrides();
                settings
            }
            Err(_) => {
                let mut settings = Settings::default();
                settings.apply_env_overrides();
                settings
            }
        }
    }

    fn apply_env_overrides(&mut self) {
        if let Ok(path) = env::var("ALEXANDRIA_DATABASE_PATH") {
            self.database.path = path;
        }
        if let Ok(concurrency) = env::var("ALEXANDRIA_INDEXING_CONCURRENCY") {
            if let Ok(parsed) = concurrency.parse::<u32>() {
                self.indexing.concurrency = parsed;
            }
        }
        if let Ok(concurrency) = env::var("ALEXANDRIA_INDEXING_LOW_PRIORITY_CONCURRENCY") {
            if let Ok(parsed) = concurrency.parse::<u32>() {
                self.indexing.low_priority_concurrency = parsed;
            }
        }
        if let Ok(days) = env::var("ALEXANDRIA_DELETION_RETENTION_DAYS") {
            if let Ok(parsed) = days.parse::<u32>() {
                self.deletion.retention_days = parsed;
            }
        }
        if let Ok(level) = env::var("ALEXANDRIA_LOGGING_LEVEL") {
            if let Ok(parsed) = match_log_level(&level) {
                self.logging.level = parsed;
            }
        }
        if let Ok(root) = env::var("ALEXANDRIA_FILESYSTEM_ROOT") {
            self.filesystem.root = root;
        }
        if let Ok(dir) = env::var("ALEXANDRIA_PLAYBACK_THUMBNAIL_CACHE_DIR") {
            self.playback.thumbnail_cache_dir = dir;
        }
    }
}

fn match_log_level(value: &str) -> Result<LogLevel, ()> {
    match value.trim() {
        "error" => Ok(LogLevel::Error),
        "warn" => Ok(LogLevel::Warn),
        "info" => Ok(LogLevel::Info),
        "debug" => Ok(LogLevel::Debug),
        "trace" => Ok(LogLevel::Trace),
        _ => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_default_settings_when_read_then_the_log_level_is_info() {
        assert_eq!(Settings::default().logging.level.as_str(), "info");
    }

    #[test]
    fn given_config_without_playback_section_when_loaded_then_default_cache_dir() {
        // Arrange — every other section omitted too; playback must not become
        // a required section for existing deployments.
        let toml = "";

        // Act
        let settings: Settings = toml::from_str(toml).expect("parses");

        // Assert
        assert_eq!(settings.playback.thumbnail_cache_dir, "thumbnails");
    }

    #[test]
    fn given_config_with_playback_section_when_loaded_then_cache_dir_read() {
        // Arrange
        let toml = "[playback]\nthumbnail_cache_dir = \"/var/cache/alexandria\"\n";

        // Act
        let settings: Settings = toml::from_str(toml).expect("parses");

        // Assert
        assert_eq!(
            settings.playback.thumbnail_cache_dir,
            "/var/cache/alexandria"
        );
    }
}
