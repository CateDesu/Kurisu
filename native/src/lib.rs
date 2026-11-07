pub mod components;
pub mod contracts;
pub mod platform;
pub mod screens;
pub mod shell;
mod startup;
pub mod theme;

use std::{
    hash::{Hash, Hasher},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use crate::{platform::Assets, startup::Startup};
use gpui::{
    App, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowDecorations, WindowOptions, px,
    size,
};

pub fn run(data_dir_override: Option<PathBuf>) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(4)
        .build()
        .map_err(|error| error.to_string())?;
    let services = Arc::new(Mutex::new(None::<kurisu_core::Services>));
    let app_id = match &data_dir_override {
        Some(path) => {
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            path.hash(&mut hash);
            format!("kurisu-native-{:016x}", hash.finish())
        }
        None => "com.catedesu.kurisu".into(),
    };
    let runtime_handle = runtime.handle().clone();
    let error = Arc::new(Mutex::new(None));
    let error_out = error.clone();
    gpui_platform::application()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            cx.set_app_identity(&app_id, "Kurisu");
            shell::bind_keys(cx);
            let services_on_quit = services.clone();
            cx.on_app_quit(move |_| {
                let services = services_on_quit.lock().unwrap().take();
                async move {
                    if let Some(services) = services {
                        services.shutdown().await;
                    }
                }
            })
            .detach();
            let bounds = Bounds::centered(None, size(px(1100.), px(750.)), cx);
            let services = services.clone();
            if let Err(open_error) = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("Kurisu".into()),
                        appears_transparent: true,
                        ..Default::default()
                    }),
                    window_decorations: Some(WindowDecorations::Client),
                    app_id: Some(app_id),
                    window_min_size: Some(size(px(800.), px(600.))),
                    ..Default::default()
                },
                |window, cx| {
                    window.set_window_title("Kurisu");
                    crate::theme::init_fonts(&window.text_system().all_font_names());
                    if let Some(gpu) = window.gpu_specs() {
                        log::info!(
                            "GPUI renderer: device={}, driver={}, info={}, software={}",
                            gpu.device_name,
                            gpu.driver_name,
                            gpu.driver_info,
                            gpu.is_software_emulated
                        );
                    } else {
                        log::info!("GPUI renderer: GPU specs unavailable");
                    }
                    cx.new(|cx| {
                        Startup::new(data_dir_override, runtime_handle, services, window, cx)
                    })
                },
            ) {
                *error_out.lock().unwrap() = Some(open_error.to_string());
                cx.quit();
                return;
            }
            cx.activate(true);
        });
    let result = error.lock().unwrap().take();
    if let Some(error) = result {
        Err(error)
    } else {
        Ok(())
    }
}
