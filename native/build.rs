fn main() {
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
