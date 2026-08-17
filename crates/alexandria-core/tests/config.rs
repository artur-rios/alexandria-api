use alexandria_core::config::{AuthMode, AuthSettings, Secret, Settings};

#[test]
fn given_example_config_when_parsed_then_defaults_and_overrides_match_spec() {
    let toml = r#"
[auth]
mode = "local"
jwks_url = "https://example.invalid/jwks"
local_db = true
session_ttl_hours = 12

[http]
bind_addr = "127.0.0.1"
port = 9090

[database]
path = "data/alexandria.sqlite"

[filesystem]
root = "/library"

[indexing]
concurrency = 8

[deletion]
retention_days = 14

[logging]
level = "debug"
"#;
    let settings: Settings = toml::from_str(toml).unwrap();

    assert_eq!(settings.auth.mode, AuthMode::Local);
    assert_eq!(settings.auth.jwks_url, "https://example.invalid/jwks");
    assert!(settings.auth.local_db);
    assert_eq!(settings.auth.session_ttl_hours, 12);
    assert_eq!(settings.http.bind_addr, "127.0.0.1");
    assert_eq!(settings.http.port, 9090);
    assert_eq!(settings.database.path, "data/alexandria.sqlite");
    assert_eq!(settings.filesystem.root, "/library");
    assert_eq!(settings.indexing.concurrency, 8);
    assert_eq!(settings.deletion.retention_days, 14);
    assert_eq!(settings.logging.level.as_str(), "debug");
}

/// Zero concurrency is meaningless — a stream buffered zero deep never makes
/// progress — so the handlers clamp it to sequential rather than hanging.
/// Pinned here because the clamp lives in the handlers, not the setting: the
/// config itself deserializes whatever it is given.
#[test]
fn given_zero_indexing_concurrency_when_parsed_then_kept_verbatim_for_the_handler_to_clamp() {
    let settings: Settings = toml::from_str("[indexing]\nconcurrency = 0\n").unwrap();
    assert_eq!(settings.indexing.concurrency, 0);
}

/// An omitted `[indexing]` section falls back to the documented default.
#[test]
fn given_no_indexing_section_when_parsed_then_default_concurrency() {
    let settings: Settings = toml::from_str("").unwrap();
    assert_eq!(settings.indexing.concurrency, 4);
}

/// `config.toml.example` is the documented full key list (README §Running), so
/// it has to parse into `Settings` and its values have to land in real fields.
/// Unknown keys are ignored by design (the example file says so), so a key the
/// code dropped would otherwise go unnoticed until an operator set it and
/// nothing happened.
#[test]
fn given_shipped_example_config_when_parsed_then_values_land_in_settings() {
    let example = include_str!("../../../config.toml.example");
    let settings: Settings = toml::from_str(example).expect("config.toml.example parses");

    // Spot-check one key per section so a section silently dropped from the
    // example (or renamed in `Settings`) fails here.
    assert_eq!(settings.auth.mode, AuthMode::External);
    assert_eq!(settings.auth.session_ttl_hours, 24);
    assert_eq!(settings.http.port, 8080);
    assert_eq!(settings.database.path, "alexandria.sqlite");
    assert_eq!(settings.indexing.concurrency, 4);
    assert_eq!(settings.deletion.retention_days, 30);
    assert_eq!(settings.logging.level.as_str(), "info");
}

#[test]
fn given_default_settings_when_socket_addr_built_then_is_loopback() {
    let settings = Settings::default();
    let addr = settings.http.socket_addr();
    assert!(addr.ip().is_loopback());
    assert_eq!(addr.port(), 8080);
}

#[test]
fn given_domain_error_when_displayed_then_human_readable() {
    use alexandria_core::errors::DomainError;
    assert_eq!(format!("{}", DomainError::NotFound), "entity not found");
    assert_eq!(
        format!("{}", DomainError::InvalidInput("bad path".into())),
        "invalid input: bad path"
    );
}

/// The external-mode keys parse from the `[auth]` section, including the two
/// secrets, so an operator can configure verification entirely from the file.
#[test]
fn given_heimdall_keys_when_parsed_then_external_settings_match() {
    let toml = r#"
[auth]
mode = "external"
heimdall_token_secret = "current-secret"
heimdall_token_secret_previous = "previous-secret"
heimdall_scope_id = "0b8d3a6e-4a1f-4c2b-9f1e-7c5d2a9b3e40"
heimdall_issuer = "heimdall"
heimdall_audience = "alexandria"
"#;
    let settings: Settings = toml::from_str(toml).unwrap();

    assert_eq!(settings.auth.mode, AuthMode::External);
    assert_eq!(settings.auth.heimdall_token_secret.expose(), "current-secret");
    assert_eq!(
        settings.auth.heimdall_token_secret_previous.expose(),
        "previous-secret"
    );
    assert_eq!(
        settings.auth.heimdall_scope_id,
        "0b8d3a6e-4a1f-4c2b-9f1e-7c5d2a9b3e40"
    );
    assert_eq!(settings.auth.heimdall_issuer, "heimdall");
    assert_eq!(settings.auth.heimdall_audience, "alexandria");
}

/// Omitted external keys are empty rather than an error: a local-mode install
/// never sets them, and `validate` is what refuses an external-mode process
/// that has left them out.
#[test]
fn given_no_heimdall_keys_when_parsed_then_empty() {
    let settings: Settings = toml::from_str("[auth]\nmode = \"local\"\n").unwrap();

    assert!(settings.auth.heimdall_token_secret.is_empty());
    assert!(settings.auth.heimdall_token_secret_previous.is_empty());
    assert_eq!(settings.auth.heimdall_scope_id, "");
}

/// A signing secret must never reach a log. `AuthSettings` derives `Debug`,
/// and a tracing span or a config dump would otherwise emit the one value
/// that grants the whole catalog — the same reasoning as FR-AU-06's ban on
/// logging passwords.
#[test]
fn given_configured_secrets_when_debug_formatted_then_redacted() {
    let mut auth = AuthSettings::default();
    auth.heimdall_token_secret = Secret::new("super-secret-value");
    auth.heimdall_token_secret_previous = Secret::new("older-secret-value");

    let rendered = format!("{auth:?}");

    assert!(!rendered.contains("super-secret-value"));
    assert!(!rendered.contains("older-secret-value"));
    assert!(rendered.contains("redacted"));
}

/// Local mode never reads the Heimdall keys, so it validates whatever it has.
#[test]
fn given_local_mode_when_validated_then_ok_without_heimdall_keys() {
    let auth = AuthSettings {
        mode: AuthMode::Local,
        ..AuthSettings::default()
    };

    assert!(auth.validate().is_ok());
}

/// A process that cannot verify a token must refuse to start, rather than
/// answer 401 to every request forever with no indication why.
#[test]
fn given_external_mode_without_secret_when_validated_then_error_names_the_key() {
    let auth = AuthSettings {
        mode: AuthMode::External,
        heimdall_scope_id: "0b8d3a6e-4a1f-4c2b-9f1e-7c5d2a9b3e40".to_string(),
        ..AuthSettings::default()
    };

    let message = auth.validate().unwrap_err().to_string();

    assert!(message.contains("auth.heimdall_token_secret"), "{message}");
}

/// External mode accepts a token on membership of a named scope, so the
/// configured value has to be a UUID for the comparison to mean anything.
#[test]
fn given_external_mode_with_non_uuid_scope_when_validated_then_error_names_the_key() {
    let auth = AuthSettings {
        mode: AuthMode::External,
        heimdall_token_secret: Secret::new("current-secret"),
        heimdall_scope_id: "not-a-uuid".to_string(),
        ..AuthSettings::default()
    };

    let message = auth.validate().unwrap_err().to_string();

    assert!(message.contains("auth.heimdall_scope_id"), "{message}");
}

#[test]
fn given_external_mode_fully_configured_when_validated_then_ok() {
    let auth = AuthSettings {
        mode: AuthMode::External,
        heimdall_token_secret: Secret::new("current-secret"),
        heimdall_scope_id: "0b8d3a6e-4a1f-4c2b-9f1e-7c5d2a9b3e40".to_string(),
        ..AuthSettings::default()
    };

    assert!(auth.validate().is_ok());
}
