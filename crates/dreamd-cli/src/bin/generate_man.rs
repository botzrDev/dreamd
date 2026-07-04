//! Regenerate `doc/dreamd.1` from the clap definition in `cli.rs`.
//!
//! Run: `cargo run -p dreamd --bin generate_man`

use std::fs;
use std::path::PathBuf;

fn main() {
    let buffer = dreamd::cli::render_man_page().expect("render man page");

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_path = manifest_dir.join("../../doc/dreamd.1");
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent).expect("create doc/");
    }
    fs::write(&out_path, &buffer).expect("write doc/dreamd.1");
    eprintln!("wrote {}", out_path.display());
}
