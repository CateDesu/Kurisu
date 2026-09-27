fn main() {
    println!("cargo:rerun-if-env-changed=KURISU_BUILD_VERSION");
    tauri_build::build()
}
