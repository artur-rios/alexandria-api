use std::ffi::CString;

use alexandria_ffi::{alexandria_file_playback_source, PLAYBACK_ERR_NOT_INITIALIZED};

// This assertion depends on the FFI's process-global `SERVICES` static never
// having been initialized. That is only true for the *first* code in a
// process to touch it — `tests/smoke.rs` exercises the same static across
// dozens of tests that do call `alexandria_index_init`, so this test lives in
// its own file. Cargo compiles each integration-test file into its own
// binary (and therefore its own process with a fresh copy of every static),
// which is what keeps this deterministic instead of racing smoke.rs's tests
// for who touches the slot first.
#[test]
fn given_uninitialized_services_when_playback_called_then_not_initialized() {
    // Arrange — no `alexandria_index_init` has run in this process.
    let uuid = CString::new("00000000-0000-0000-0000-000000000000").unwrap();
    let token = CString::new("t").unwrap();

    // Act
    let result = alexandria_file_playback_source(uuid.as_ptr(), token.as_ptr());

    // Assert
    assert_eq!(result.status, PLAYBACK_ERR_NOT_INITIALIZED);
    assert!(result.json.is_null());
}
