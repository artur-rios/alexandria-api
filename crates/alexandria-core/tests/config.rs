use std::sync::Mutex;

use alexandria_core::config::Settings;

/// Environment variables are process-global, so any test that sets one races
/// every other test in this binary — cargo runs them on parallel threads in
/// one process. Every such test takes this lock, and every one restores the
/// variables it touched before releasing it, so the environment outside a
/// guarded section is always the one the process started with.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Restores a set of environment variables to their prior values when
/// dropped — on the panicking path too, so one failing assertion cannot leave
/// a variable set for whatever runs next.
struct EnvGuard {
    _guard: std::sync::MutexGuard<'static, ()>,
    previous: Vec<(&'static str, Option<String>)>,
}

impl EnvGuard {
    /// Take the lock and set `vars`, remembering what was there before.
    fn set(vars: &[(&'static str, &str)]) -> Self {
        // Recover from poisoning rather than propagating it: the lock guards
        // the environment, which this guard restores on unwind anyway, so a
        // panicking test must not turn every later one into an opaque
        // `PoisonError` that buries the real failure.
        let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = vars
            .iter()
            .map(|(key, value)| {
                let before = std::env::var(key).ok();
                std::env::set_var(key, value);
                (*key, before)
            })
            .collect();
        Self {
            _guard: guard,
            previous,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, before) in &self.previous {
            match before {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[test]
fn given_example_config_when_parsed_then_defaults_and_overrides_match_spec() {
    let toml = r#"
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

/// Mirrors `given_no_indexing_section_when_parsed_then_default_concurrency`
/// for the `Low`-priority width (run priority, FR-FC-08): an owner who never
/// sets the key still gets the narrowest useful default rather than an error.
#[test]
fn given_no_configured_low_priority_concurrency_when_loaded_then_it_defaults_to_one() {
    let settings = Settings::default();
    assert_eq!(settings.indexing.low_priority_concurrency, 1);
}

/// Same shape as `given_configured_indexing_concurrency`: a set key lands
/// verbatim.
#[test]
fn given_configured_low_priority_concurrency_when_parsed_then_kept_verbatim() {
    let settings: Settings = toml::from_str("[indexing]\nlow_priority_concurrency = 2\n").unwrap();
    assert_eq!(settings.indexing.low_priority_concurrency, 2);
}

/// Zero is as meaningless here as it is for `concurrency` — see
/// `given_zero_indexing_concurrency_when_parsed_then_kept_verbatim_for_the_handler_to_clamp`.
/// The config layer stores whatever it is given; the handlers clamp it.
#[test]
fn given_zero_low_priority_concurrency_when_parsed_then_kept_verbatim_for_the_handler_to_clamp() {
    let settings: Settings = toml::from_str("[indexing]\nlow_priority_concurrency = 0\n").unwrap();
    assert_eq!(settings.indexing.low_priority_concurrency, 0);
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
    assert_eq!(settings.database.path, "alexandria.sqlite");
    assert_eq!(settings.indexing.concurrency, 4);
    assert_eq!(settings.indexing.low_priority_concurrency, 1);
    assert_eq!(settings.deletion.retention_days, 30);
    assert_eq!(settings.logging.level.as_str(), "info");
}

/// Installs that enabled music enrichment still carry a `[metadata]` section.
/// The feature is gone, but the file must keep loading: unknown sections are
/// ignored rather than refused.
#[test]
fn given_a_config_with_a_legacy_metadata_section_when_parsed_then_it_loads_and_ignores_it() {
    let raw = "[metadata]
enabled = true
contact = \"me@example.com\"
image_cache_dir = \"artist-images\"
";

    let settings: Settings = toml::from_str(raw).expect("legacy section is ignored");

    assert_eq!(
        settings.playback.thumbnail_cache_dir,
        Settings::default().playback.thumbnail_cache_dir
    );
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

/// A 0.6 install's `config.toml` may still hold the `[auth]` and `[http]`
/// tables. Authentication and the server are gone, but the file must keep
/// loading: those tables are ignored rather than refused.
#[test]
fn given_a_0_6_config_with_auth_and_http_tables_when_loaded_then_it_loads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        "[auth]\nmode = \"windows\"\nwindows_owner_sid = \"S-1-5-21-1\"\n\n\
         [http]\nbind_addr = \"127.0.0.1:8080\"\n\n\
         [database]\npath = \"data/catalog.sqlite\"\n",
    )
    .unwrap();

    let settings = Settings::load(&path).expect("a 0.6 config loads");

    assert_eq!(settings.database.path, "data/catalog.sqlite");
}

/// Every key takes an environment override, so the documented deployment
/// where nothing but the environment is set has to be configurable: with no
/// config file at all, the variables still land in `Settings`. Both are set
/// in one test, under one lock, because they share the process-global
/// environment; see `EnvGuard`.
#[test]
fn given_only_environment_variables_when_settings_loaded_then_they_apply() {
    let _env = EnvGuard::set(&[
        ("ALEXANDRIA_DELETION_RETENTION_DAYS", "7"),
        ("ALEXANDRIA_LOGGING_LEVEL", "debug"),
    ]);

    let settings = Settings::load_or_default(std::path::Path::new(
        "no-such-config-file-for-this-test.toml",
    ));

    assert_eq!(settings.deletion.retention_days, 7);
    assert_eq!(settings.logging.level.as_str(), "debug");
}

/// The environment wins over a file that sets the same key, so an owner with
/// a `config.toml` can still change one value for a single run.
#[test]
fn given_a_key_in_both_file_and_environment_when_settings_loaded_then_the_environment_wins() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "[deletion]\nretention_days = 14\n").unwrap();

    let _env = EnvGuard::set(&[("ALEXANDRIA_DELETION_RETENTION_DAYS", "7")]);

    let settings = Settings::load(&path).expect("config parses");

    assert_eq!(settings.deletion.retention_days, 7);
}
