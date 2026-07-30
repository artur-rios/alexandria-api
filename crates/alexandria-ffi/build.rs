use std::env;
use std::path::PathBuf;

fn main() {
    let crate_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let output_file = PathBuf::from(&crate_dir).join("src").join("header.h");

    match cbindgen::Builder::new()
        .with_crate(&crate_dir)
        .with_language(cbindgen::Language::C)
        .generate()
    {
        Ok(bindings) => {
            if !bindings.write_to_file(&output_file) {
                eprintln!(
                    "cbindgen: failed to write header to {}",
                    output_file.display()
                );
            }
        }
        Err(err) => {
            eprintln!("cbindgen: {err}");
        }
    }

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/lib.rs");
}
