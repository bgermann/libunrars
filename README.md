# libunrars

A drop-in implementation of the `libunrar` C API (`RAROpenArchive[Ex]`,
`RARReadHeader[Ex]`, `RARProcessFile[W]`, `RARSetCallback`, `RARSetPassword`,
`RARSetChangeVolProc`, `RARSetProcessDataProc`, `RARCloseArchive`,
`RARGetDllVersion`) written in Rust on top of the [`rars`](https://crates.io/crates/rars) crate.

Licensed under 0BSD (see `LICENSE`).

The unrar-free project has a
[C header](https://gitlab.com/bgermann/unrar-free/-/blob/master/include/unrarchive.h)
with the same license, which you can use instead of the original dll.hpp.

## Clean-room statement

This was written without reading the unrar source code. The exported symbol names of
the library were consulted, and behaviour was checked black-box by running the same
test programs against both libraries.

## Build

    cargo build --release

Produces `target/release/libunrars.so` (soname `libunrar.so.5`) and
`libunrars.a`. Link with `-lunrars`. Even though the file is named
`libunrars.so`, its embedded soname is `libunrar.so.5` (matching the original
library's SOVERSION), so programs linking against it record a runtime
dependency on `libunrar.so.5`.

## Known limitations

- Truncated archives, corrupt headers, middle-volume opens and `MaxDiskWrite` follow libunrar's observed behaviour.
- On corrupt compressed data, the byte counts reported to the data callback (and `KEEPBROKEN` partial output) can differ, because rars' decoders flush at different granularity.
- A wrong password on RAR 2.9/3.x archives delivers no data callbacks (libunrar delivers some garbage).
- `UCM_LARGEDICT`, `MaxAllocate`, `MaxRuntime` and `MarkOfTheWeb` are ignored.

## Windows

The crate compiles for `x86_64-pc-windows-gnu` and `i686-pc-windows-gnu`:
exports and callbacks use the `system` calling convention (`__stdcall` on
32-bit x86), `wchar_t` is 16-bit with UTF-16 conversion, and 32-bit DLL
exports are undecorated (`--kill-at`). Narrow (`char *`) strings are treated as
UTF-8 rather than the ANSI code page. Both DLLs link with MinGW-w64 and were exercised under Wine
(open, list, extract to UTF-16 paths, callbacks) on 64-bit and 32-bit.
