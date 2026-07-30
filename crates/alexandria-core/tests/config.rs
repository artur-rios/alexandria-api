use alexandria_core::config::{AuthMode, Settings};

#[test]
fn given_example_config_when_parsed_then_defaults_and_overrides_match_spec() {
    let toml = r#"
[auth]
mode = "local"
jwks_url = "https://example.invalid/jwks"
local_db = true

[http]
bind_addr = "127.0.0.1"
port = 9090

[database]
path = "data/alexandria.sqlite"

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
    assert_eq!(settings.http.bind_addr, "127.0.0.1");
    assert_eq!(settings.http.port, 9090);
    assert_eq!(settings.database.path, "data/alexandria.sqlite");
    assert_eq!(settings.indexing.concurrency, 8);
    assert_eq!(settings.deletion.retention_days, 14);
    assert_eq!(settings.logging.level.as_str(), "debug");
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
