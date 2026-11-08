fn main() {
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    // 32-bit stdcall exports must be undecorated (`RAROpenArchive`, not `_RAROpenArchive@4`).
    if os == "windows" && env == "gnu" {
        println!("cargo:rustc-link-arg-cdylib=-Wl,--kill-at");
    }
}
