//! Registers every `loader/strings/<code>.json` beyond English, so a
//! language Weblate fills in ships in the next DLL build with no Rust
//! edit. `cargo:rerun-if-changed` keeps rebuilds to actual edits.

use std::{env, fs, path::PathBuf};

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let dir = manifest.join("../../strings");
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("loader/strings is missing")
        .map(|e| e.expect("strings entry unreadable").path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();
    let mut src = String::from("const EXTRA: &[(&str, &str)] = &[\n");
    for path in &files {
        let code = path
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("strings file without a language code");
        if code == "en" {
            continue;
        }
        src.push_str(&format!(
            "    ({code:?}, include_str!({path:?})),\n",
            path = path.to_str().expect("strings path is not UTF-8"),
        ));
    }
    src.push_str("];\n");
    fs::write(
        PathBuf::from(env::var("OUT_DIR").unwrap()).join("extra_strings.rs"),
        src,
    )
    .expect("extra_strings.rs unwritable");
    println!("cargo:rerun-if-changed=../../strings");
}
