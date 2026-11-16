fn main() {
    println!("cargo:rerun-if-env-changed=KURISU_BUNDLED_NOTICES");
    let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let notices = if let Some(path) = std::env::var_os("KURISU_BUNDLED_NOTICES") {
        println!(
            "cargo:rerun-if-changed={}",
            std::path::Path::new(&path).display()
        );
        let text = std::fs::read_to_string(path).expect("read bundled dependency notices");
        assert!(!text.trim().is_empty(), "dependency notices are empty");
        text
    } else {
        assert_ne!(
            std::env::var("PROFILE").as_deref(),
            Ok("release"),
            "Release builds require dependency notices. Use python3 native/build.py"
        );
        String::new()
    };
    std::fs::write(output.join("THIRD_PARTY_NOTICES.txt"), notices).unwrap();
    println!("cargo:rerun-if-changed=packaging/kurisu.rc");
    println!("cargo:rerun-if-changed=packaging/kurisu.ico");
    #[cfg(windows)]
    embed_resource::compile(
        "packaging/kurisu.rc",
        embed_resource::ParamsIncludeDirs(["packaging"]),
    )
    .manifest_required()
    .expect("compile Kurisu icon resource");
}
