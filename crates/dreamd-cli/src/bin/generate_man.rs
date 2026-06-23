//! Regenerate `doc/dreamd.1` from the clap definition in `cli.rs`.
//!
//! Run: `cargo run -p dreamd --bin generate_man`

use std::fs;
use std::path::PathBuf;

use clap::CommandFactory;
use clap_mangen::Man;

fn main() {
    let man = Man::new(dreamd::cli::Cli::command());
    let mut buffer: Vec<u8> = Vec::new();
    man.render(&mut buffer).expect("render man page");

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_path = manifest_dir.join("../../doc/dreamd.1");
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent).expect("create doc/");
    }
    fs::write(&out_path, &buffer).expect("write doc/dreamd.1");
    eprintln!("wrote {}", out_path.display());
}
