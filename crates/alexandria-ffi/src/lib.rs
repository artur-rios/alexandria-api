#![deny(unsafe_code)]

use std::os::raw::c_char;

static VERSION_CSTRING: &[u8] = b"0.1.0\0";

#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn alexandria_version() -> *const c_char {
    VERSION_CSTRING.as_ptr() as *const c_char
}

#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn alexandria_health_status_code() -> i32 {
    200
}
