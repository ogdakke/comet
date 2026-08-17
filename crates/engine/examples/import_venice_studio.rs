//! Import a Venice Studio image dump into the local Zeron studio store.
//!
//! Usage:
//!   cargo run -p zeron-engine --example import_venice_studio -- ~/Downloads/venice_dump
//!
//! Optional:
//!   --data-dir ~/.zeron

use std::path::{Path, PathBuf};

use zeron_engine::{EngineProfile, StudioStore, load_venice_image_dump};
use zeron_studio::ProviderId;

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut dump_dir = None;
    let mut data_dir = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--data-dir" => {
                data_dir = Some(PathBuf::from(
                    args.get(index + 1).ok_or("--data-dir needs a path")?,
                ));
                index += 2;
            }
            "--help" | "-h" => {
                println!("import_venice_studio <dump-dir> [--data-dir <zeron-data-dir>]");
                return Ok(());
            }
            flag if flag.starts_with('-') => {
                return Err(format!("unknown flag {flag}").into());
            }
            path => {
                dump_dir = Some(PathBuf::from(path));
                index += 1;
            }
        }
    }
    let dump_dir = dump_dir.ok_or("usage: import_venice_studio <dump-dir> [--data-dir <path>]")?;
    let data_dir = data_dir.unwrap_or_else(default_data_dir);
    let profile = EngineProfile::local(&data_dir)?;
    let store = StudioStore::open(profile.store_root(), 512 * 1024 * 1024)?;
    let catalog = store
        .cached_models(&ProviderId::from("venice"))?
        .map(|response| response.models);
    let history = load_venice_image_dump(&dump_dir, catalog.as_deref())?;
    let device_id = read_device_id(&data_dir);
    let report = store.import_completed_history(&history, &device_id)?;
    println!("data dir: {}", data_dir.display());
    println!("studio:   {}", store.database_path().display());
    println!(
        "imported {} conversation(s), skipped {}, turns {}, artifacts {}",
        report.conversations_imported,
        report.conversations_skipped,
        report.turns_imported,
        report.artifacts_imported
    );
    if report.missing_files > 0 {
        println!("missing files: {}", report.missing_files);
    }
    if report.failed_turns > 0 {
        println!("turns without outputs: {}", report.failed_turns);
    }
    Ok(())
}

fn default_data_dir() -> PathBuf {
    std::env::var_os("ZERON_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = PathBuf::from(std::env::var_os("HOME").expect("HOME not set"));
            home.join(".zeron")
        })
}

fn read_device_id(data_dir: &Path) -> String {
    std::fs::read_to_string(data_dir.join("device-id"))
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "venice-import".into())
}
