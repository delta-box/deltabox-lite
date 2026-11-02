fn main() {
    println!("cargo:rerun-if-changed=src/windows/winfsp.c");
    println!("cargo:rerun-if-env-changed=WINFSP_INSTALL_DIR");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let sdk = std::env::var_os("WINFSP_INSTALL_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(
                std::env::var_os("ProgramFiles(x86)")
                    .unwrap_or_else(|| "C:\\Program Files (x86)".into()),
            )
            .join("WinFsp")
        });
    assert!(sdk.join("inc/winfsp/winfsp.h").is_file(),
        "WinFsp SDK not found; install WinFsp with its Developer feature or set WINFSP_INSTALL_DIR");
    cc::Build::new()
        .std("c11")
        .file("src/windows/winfsp.c")
        .include(sdk.join("inc"))
        .define("UNICODE", None)
        .define("_UNICODE", None)
        .compile("deltabox_winfsp");
    println!("cargo:rustc-link-lib=advapi32");
}
