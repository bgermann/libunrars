fn main() {
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    // 32-bit stdcall exports must be undecorated (`RAROpenArchive`, not `_RAROpenArchive@4`).
    if os == "windows" && env == "gnu" {
        println!("cargo:rustc-link-arg-cdylib=-Wl,--kill-at");
    }
    // Match the original library's SOVERSION on every platform where `-soname`
    // is meaningful: the produced `libunrars.so` records `libunrar.so.5` as its
    // soname, so programs linking `-lunrars` get a runtime dependency on
    // `libunrar.so.5`. Windows (PE) and Apple's Mach-O platforms (macOS, iOS,
    // …) have no `-soname`: they resolve by file/install name instead.
    if os != "windows" && os != "macos" {
        println!("cargo:rustc-cdylib-link-arg=-Wl,-soname,libunrar.so.5");
    }
}
