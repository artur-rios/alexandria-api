use std::env;
use std::path::Path;

use serde::Deserialize;

use crate::errors::DomainError;

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    External,
    Local,
}

impl AuthMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            AuthMode::External => "external",
            AuthMode::Local => "local",
        }
    }
}

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
pub struct AuthSettings {
    #[serde(default = "default_auth_mode")]
    pub mode: AuthMode,
    #[serde(default)]
    pub jwks_url: String,
    #[serde(default)]
    pub local_db: bool,
    /// How long a session created by local login (UC-34) stays valid.
    #[serde(default = "default_session_ttl_hours")]
    pub session_ttl_hours: u32,
}

fn default_auth_mode() -> AuthMode {
    AuthMode::External
}

fn default_session_ttl_hours() -> u32 {
    24
}

impl Default for AuthSettings {
    fn default() -> Self {
        Self {
            mode: default_auth_mode(),
            jwks_url: String::new(),
            local_db: false,
            session_ttl_hours: default_session_ttl_hours(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct HttpSettings {
    #[serde(default = "default_bind_addr")]
    pub bind_addr: String,
    #[serde(default = "default_http_port")]
    pub port: u16,
}

fn default_bind_addr() -> String {
    "127.0.0.1".to_string()
}

fn default_http_port() -> u16 {
    8080
}

impl Default for HttpSettings {
    fn default() -> Self {
        Self {
            bind_addr: default_bind_addr(),
            port: default_http_port(),
        }
    }
}

impl HttpSettings {
    pub fn socket_addr(&self) -> std::net::SocketAddr {
        format!("{}:{}", self.bind_addr, self.port)
            .parse()
            .expect("invalid http bind address")
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
    #[serde(default = "default_indexing_concurrency")]
    pub concurrency: u32,
}

fn default_indexing_concurrency() -> u32 {
    4
}

impl Default for IndexingSettings {
    fn default() -> Self {
        Self {
            concurrency: default_indexing_concurrency(),
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

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Settings {
    #[serde(default)]
    pub auth: AuthSettings,
    #[serde(default)]
    pub http: HttpSettings,
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
        if let Ok(mode) = env::var("ALEXANDRIA_AUTH_MODE") {
            if let Ok(parsed) = match_mode(&mode) {
                self.auth.mode = parsed;
            }
        }
        if let Ok(jwks_url) = env::var("ALEXANDRIA_AUTH_JWKS_URL") {
            self.auth.jwks_url = jwks_url;
        }
        if let Ok(addr) = env::var("ALEXANDRIA_HTTP_BIND_ADDR") {
            self.http.bind_addr = addr;
        }
        if let Ok(port) = env::var("ALEXANDRIA_HTTP_PORT") {
            if let Ok(parsed) = port.parse::<u16>() {
                self.http.port = parsed;
            }
        }
        if let Ok(path) = env::var("ALEXANDRIA_DATABASE_PATH") {
            self.database.path = path;
        }
        if let Ok(days) = env::var("ALEXANDRIA_DELETION_RETENTION_DAYS") {
            if let Ok(parsed) = days.parse::<u32>() {
                self.deletion.retention_days = parsed;
            }
        }
        if let Ok(level) = env::var("ALEXANDRIA_LOG_LEVEL") {
            if let Ok(parsed) = match_log_level(&level) {
                self.logging.level = parsed;
            }
        }
        if let Ok(root) = env::var("ALEXANDRIA_FILESYSTEM_ROOT") {
            self.filesystem.root = root;
        }
    }
}

fn match_mode(value: &str) -> Result<AuthMode, ()> {
    match value.trim() {
        "external" => Ok(AuthMode::External),
        "local" => Ok(AuthMode::Local),
        _ => Err(()),
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
    fn given_auth_mode_when_external_lowercase_then_parses() {
        assert_eq!(match_mode("external").unwrap(), AuthMode::External);
        assert_eq!(match_mode("local").unwrap(), AuthMode::Local);
        assert!(match_mode("bogus").is_err());
        assert_eq!(AuthMode::External.as_str(), "external");
        assert_eq!(AuthMode::Local.as_str(), "local");
    }

    #[test]
    fn given_default_settings_when_socket_addr_built_then_is_loopback() {
        let settings = Settings::default();
        let addr = settings.http.socket_addr();
        assert!(addr.ip().is_loopback());
        assert_eq!(addr.port(), 8080);
        assert_eq!(settings.logging.level.as_str(), "info");
    }
}
