fn main() {
    tauri_build::build();

    // Windows: Embed Common Controls v6 manifest for unit test binaries
    //
    // Unit tests live in `--lib` (no separate `tests/` target exists), so Cargo's
    // `rustc-link-arg-tests` cannot be used. Apply to non-bin/non-cdylib link targets
    // and override `/MANIFEST:NO` on `bins` (which already get Tauri's manifest) and `cdylib`.
    #[cfg(target_os = "windows")]
    {
        let manifest_path = std::path::PathBuf::from(
            std::env::var("CARGO_MANIFEST_DIR").expect("missing CARGO_MANIFEST_DIR"),
        )
        .join("common-controls.manifest");
        let manifest_arg = format!("/MANIFESTINPUT:{}", manifest_path.display());

        println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg={}", manifest_arg);
        println!("cargo:rustc-link-arg-bins=/MANIFEST:NO");
        println!("cargo:rustc-link-arg-cdylib=/MANIFEST:NO");
        println!("cargo:rerun-if-changed={}", manifest_path.display());
    }
}
