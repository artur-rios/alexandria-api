use std::env;
use std::fmt;
use std::path::Path;

use serde::Deserialize;
use uuid::Uuid;

use crate::errors::DomainError;

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    External,
    Local,
    /// The Windows account this process runs as is the credential (UC-45 /
    /// FR-AU-20). Mutually exclusive with the other two: exactly one mode is
    /// active at runtime (FR-AU-01).
    Windows,
}

impl AuthMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            AuthMode::External => "external",
            AuthMode::Local => "local",
            AuthMode::Windows => "windows",
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

/// A configuration value that must never reach a log.
///
/// `Debug` prints a marker instead of the value, so a config dump or a
/// `tracing` span cannot emit a signing secret. This is FR-AU-06's ban on
/// logging passwords applied to the other secret that grants the whole
/// catalog — with the difference that this one is *shared* with Heimdall, so
/// leaking it compromises more than Alexandria.
#[derive(Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The plaintext. Named so that every read site is obvious at a glance.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether the secret is unset. Whitespace counts as unset: a key
    /// configured to `" "` is a mistake, not a secret.
    pub fn is_empty(&self) -> bool {
        self.0.trim().is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.0.is_empty() {
            "Secret(unset)"
        } else {
            "Secret(redacted)"
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuthSettings {
    #[serde(default = "default_auth_mode")]
    pub mode: AuthMode,
    /// External mode only: the HS256 secret Heimdall signs its tokens with
    /// (its `HEIMDALL_AUTH_TOKEN_SECRET`). Required in external mode —
    /// Heimdall publishes no keys, so this is the only way to verify one of
    /// its tokens.
    #[serde(default)]
    pub heimdall_token_secret: Secret,
    /// External mode only: the secret Heimdall is rotating away from (its
    /// `HEIMDALL_AUTH_TOKEN_SECRET_PREVIOUS`). Accepted alongside the current
    /// one, mirroring Heimdall's own two-key scheme, so a rotation there does
    /// not black out Alexandria until this file is edited and the process
    /// restarted. Ignored when equal to the current secret: the same value
    /// under two names is not a rotation.
    #[serde(default)]
    pub heimdall_token_secret_previous: Secret,
    /// External mode only: the UUID of the Heimdall scope Alexandria is
    /// registered in. A token is accepted when it names this scope.
    #[serde(default)]
    pub heimdall_scope_id: String,
    /// External mode only: the `iss` claim to require, checked only when set.
    /// Heimdall reads its issuer from an environment variable that defaults
    /// to empty and then signs tokens carrying no `iss` at all, so requiring
    /// one unconditionally would reject every token from a default install.
    #[serde(default)]
    pub heimdall_issuer: String,
    /// External mode only: the `aud` claim to require, checked only when set,
    /// for the same reason as `heimdall_issuer`.
    #[serde(default)]
    pub heimdall_audience: String,
    #[serde(default)]
    pub local_db: bool,
    /// How long a session created by local login (UC-34) stays valid.
    #[serde(default = "default_session_ttl_hours")]
    pub session_ttl_hours: u32,
    /// Windows mode only: the SID of the account this process must run as,
    /// e.g. `S-1-5-21-1004336348-1177238915-682003330-1001`.
    ///
    /// A SID rather than a username because usernames are renameable and
    /// reusable, while a SID is neither. Not a secret — an identifier — so it
    /// is a plain `String` rather than a `Secret`.
    #[serde(default)]
    pub windows_owner_sid: String,
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
            heimdall_token_secret: Secret::default(),
            heimdall_token_secret_previous: Secret::default(),
            heimdall_scope_id: String::new(),
            heimdall_issuer: String::new(),
            heimdall_audience: String::new(),
            local_db: false,
            session_ttl_hours: default_session_ttl_hours(),
            windows_owner_sid: String::new(),
        }
    }
}

impl AuthSettings {
    /// Startup validation for external mode (UC-36). Each binary calls this
    /// before building services: a process that cannot verify a token must
    /// refuse to start, rather than answer `401` to every request forever
    /// with nothing to say why. Heimdall makes the same choice about the same
    /// secret, and for the same reason.
    ///
    /// Local mode reads none of these keys and always passes.
    pub fn validate(&self) -> Result<(), DomainError> {
        match self.mode {
            AuthMode::Local => Ok(()),
            AuthMode::External => {
                if self.heimdall_token_secret.is_empty() {
                    return Err(DomainError::Config(
                        "auth.heimdall_token_secret is unset: external mode verifies \
                         Heimdall's tokens against the secret it signs them with, and \
                         Heimdall publishes no keys to fetch instead"
                            .to_string(),
                    ));
                }

                let scope_id = Uuid::parse_str(self.heimdall_scope_id.trim()).map_err(|_| {
                    DomainError::Config(format!(
                        "auth.heimdall_scope_id is not a UUID: {:?}. External mode accepts a \
                         token on membership of this Heimdall scope, so it must name one.",
                        self.heimdall_scope_id
                    ))
                })?;

                // The nil UUID parses, so it would otherwise start a process
                // that accepts any token whose `scopeId` is also nil. It is a
                // placeholder an operator leaves behind, never a scope
                // Heimdall issued.
                if scope_id.is_nil() {
                    return Err(DomainError::Config(
                        "auth.heimdall_scope_id is the nil UUID: that is a placeholder, not \
                         the Heimdall scope Alexandria is registered in"
                            .to_string(),
                    ));
                }

                Ok(())
            }
            AuthMode::Windows => {
                if self.windows_owner_sid.trim().is_empty() {
                    return Err(DomainError::Config(
                        "auth.windows_owner_sid is unset: Windows mode authenticates by the \
                         account this process runs as, so it must name the account it expects"
                            .to_string(),
                    ));
                }
                Ok(())
            }
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
    /// How many files UC-01's index walk and UC-02's re-index walk process at
    /// a time. The per-file cost is dominated by hashing the bytes, which runs
    /// on Tokio's blocking pool, so this is a real parallelism knob — but the
    /// database half of each file's work still serializes behind SQLite's
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
    /// Cache entries are keyed by content hash, so a re-index that changes
    /// a file's bytes invalidates its thumbnail for free. There is no
    /// eviction policy: inventing one before anyone has a full cache would
    /// be guessing.
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
        if let Ok(mode) = env::var("ALEXANDRIA_AUTH_MODE") {
            if let Ok(parsed) = match_mode(&mode) {
                self.auth.mode = parsed;
            }
        }
        if let Ok(hours) = env::var("ALEXANDRIA_AUTH_SESSION_TTL_HOURS") {
            if let Ok(parsed) = hours.parse::<u32>() {
                self.auth.session_ttl_hours = parsed;
            }
        }
        if let Ok(sid) = env::var("ALEXANDRIA_AUTH_WINDOWS_OWNER_SID") {
            self.auth.windows_owner_sid = sid;
        }
        // The external-mode keys take the same overrides as everything else,
        // and the two secrets especially: a deployment must be able to hand
        // Alexandria the shared signing key through the environment rather
        // than write to disk the one value that can mint a token every
        // Heimdall-backed application will accept.
        if let Ok(secret) = env::var("ALEXANDRIA_AUTH_HEIMDALL_TOKEN_SECRET") {
            self.auth.heimdall_token_secret = Secret::new(secret);
        }
        if let Ok(secret) = env::var("ALEXANDRIA_AUTH_HEIMDALL_TOKEN_SECRET_PREVIOUS") {
            self.auth.heimdall_token_secret_previous = Secret::new(secret);
        }
        if let Ok(scope_id) = env::var("ALEXANDRIA_AUTH_HEIMDALL_SCOPE_ID") {
            self.auth.heimdall_scope_id = scope_id;
        }
        if let Ok(issuer) = env::var("ALEXANDRIA_AUTH_HEIMDALL_ISSUER") {
            self.auth.heimdall_issuer = issuer;
        }
        if let Ok(audience) = env::var("ALEXANDRIA_AUTH_HEIMDALL_AUDIENCE") {
            self.auth.heimdall_audience = audience;
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
        if let Ok(level) = env::var("ALEXANDRIA_LOG_LEVEL") {
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

fn match_mode(value: &str) -> Result<AuthMode, ()> {
    match value.trim() {
        "external" => Ok(AuthMode::External),
        "local" => Ok(AuthMode::Local),
        "windows" => Ok(AuthMode::Windows),
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
        assert_eq!(match_mode("windows").unwrap(), AuthMode::Windows);
        assert!(match_mode("bogus").is_err());
        assert_eq!(AuthMode::External.as_str(), "external");
        assert_eq!(AuthMode::Local.as_str(), "local");
        assert_eq!(AuthMode::Windows.as_str(), "windows");
    }

    #[test]
    fn given_default_settings_when_socket_addr_built_then_is_loopback() {
        let settings = Settings::default();
        let addr = settings.http.socket_addr();
        assert!(addr.ip().is_loopback());
        assert_eq!(addr.port(), 8080);
        assert_eq!(settings.logging.level.as_str(), "info");
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
