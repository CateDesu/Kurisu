#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

use std::path::PathBuf;

#[cfg(target_os = "windows")]
fn attach_parent_console_if_needed() {
    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
    const STD_OUTPUT_HANDLE: u32 = (-11i32) as u32;
    const STD_ERROR_HANDLE: u32 = (-12i32) as u32;
    unsafe extern "system" {
        fn GetStdHandle(nStdHandle: u32) -> isize;
        fn AttachConsole(dwProcessId: u32) -> i32;
    }
    // Attach only when stdout and stderr are unattached so redirected pipes stay intact.
    unsafe {
        if GetStdHandle(STD_OUTPUT_HANDLE) == 0 && GetStdHandle(STD_ERROR_HANDLE) == 0 {
            AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

fn main() {
    let mut args = std::env::args_os().skip(1);
    let mut data_dir = None;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--data-dir") if data_dir.is_none() => {
                let Some(path) = args.next() else {
                    #[cfg(target_os = "windows")]
                    attach_parent_console_if_needed();
                    eprintln!("--data-dir requires a path");
                    std::process::exit(2);
                };
                data_dir = Some(PathBuf::from(path));
            }
            Some("--help") => {
                #[cfg(target_os = "windows")]
                attach_parent_console_if_needed();
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
                #[cfg(target_os = "windows")]
                attach_parent_console_if_needed();
                print!("{}", kurisu_native::notices::LICENSE);
                return;
            }
            Some("--notices") => {
                #[cfg(target_os = "windows")]
                attach_parent_console_if_needed();
                print!("{}", kurisu_native::notices::DEPENDENCIES);
                return;
            }
            Some("--version") => {
                #[cfg(target_os = "windows")]
                attach_parent_console_if_needed();
                println!("kurisu-native {}", kurisu_core::updater::current_version());
                return;
            }
            _ => {
                #[cfg(target_os = "windows")]
                attach_parent_console_if_needed();
                eprintln!("Unknown option: {}", arg.to_string_lossy());
                std::process::exit(2);
            }
        }
    }
    env_logger::init();
    #[cfg(target_os = "linux")]
    if let Err(error) = kurisu_core::updater::init_install_path() {
        log::warn!("Could not capture the install path: {error}");
    }
    #[cfg(target_os = "linux")]
    if let Err(error) = kurisu_native::notices::install() {
        log::warn!(
            "Could not create companion notices: {error}. Use --license and --notices to read the bundled texts."
        );
    }
    if let Err(error) = kurisu_native::run(data_dir) {
        #[cfg(target_os = "windows")]
        attach_parent_console_if_needed();
        eprintln!("Kurisu could not start: {error}");
        std::process::exit(1);
    }
}
