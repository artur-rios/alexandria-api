use alexandria_core::config::{AuthMode, Settings};

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
