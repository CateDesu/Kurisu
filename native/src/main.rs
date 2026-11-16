#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

use std::path::PathBuf;

fn main() {
    let mut args = std::env::args_os().skip(1);
    let mut data_dir = None;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--data-dir") if data_dir.is_none() => {
                let Some(path) = args.next() else {
                    eprintln!("--data-dir requires a path");
                    std::process::exit(2);
                };
                data_dir = Some(PathBuf::from(path));
            }
            Some("--help") => {
                println!(
                    "Usage: kurisu-native [--data-dir PATH] [--help] [--version] [--license] [--notices]"
                );
                println!("  --data-dir PATH  Use an independent native data profile");
                println!("  --help           Show this help");
                println!("  --version        Show the compiled version");
                println!("  --license        Show the bundled program license");
                println!("  --notices        Show the bundled dependency notices");
                return;
            }
            Some("--license") => {
                print!("{}", kurisu_native::notices::LICENSE);
                return;
            }
            Some("--notices") => {
                print!("{}", kurisu_native::notices::DEPENDENCIES);
                return;
            }
            Some("--version") => {
                println!("kurisu-native {}", kurisu_core::updater::current_version());
                return;
            }
            _ => {
                eprintln!("Unknown option: {}", arg.to_string_lossy());
                std::process::exit(2);
            }
        }
    }
    env_logger::init();
    #[cfg(target_os = "linux")]
    if let Err(error) = kurisu_native::notices::install() {
        log::warn!(
            "Could not create companion notices: {error}. Use --license and --notices to read the bundled texts."
        );
    }
    if let Err(error) = kurisu_native::run(data_dir) {
        eprintln!("Kurisu could not start: {error}");
        std::process::exit(1);
    }
}
