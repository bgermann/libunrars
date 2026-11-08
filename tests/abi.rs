use rars::{ArchiveVersion, Builder};
use std::ffi::CString;
use std::mem::size_of;
use std::os::raw::{c_char, c_void};
use unrars::abi::*;
use unrars::*;

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("unrars-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn payload() -> Vec<u8> {
    (0..200_000u32).flat_map(|i| (i % 251).to_le_bytes()).collect()
}

fn build(format: ArchiveVersion, solid: bool, password: Option<&[u8]>) -> Vec<u8> {
    let mut b = Builder::new(format).solid(solid).password(password.map(<[u8]>::to_vec));
    b.add_bytes(b"one.bin".to_vec(), payload(), Some(1_600_000_000), Some(0o100640)).unwrap();
    b.add_bytes(b"dir/two.txt".to_vec(), b"second".to_vec(), Some(1_600_000_000), Some(0o100644))
        .unwrap();
    b.to_bytes().unwrap()
}

unsafe fn open(path: &std::path::Path, mode: u32) -> (*mut c_void, u32) {
    let name = CString::new(path.to_str().unwrap()).unwrap();
    let mut d: RAROpenArchiveDataEx = std::mem::zeroed();
    d.ArcName = name.as_ptr() as *mut c_char;
    d.OpenMode = mode;
    let h = RAROpenArchiveEx(&mut d);
    (h, d.OpenResult)
}

unsafe fn header(h: *mut c_void) -> Result<Box<RARHeaderDataEx>, i32> {
    let mut hd: Box<RARHeaderDataEx> = Box::new(std::mem::zeroed());
    match RARReadHeaderEx(h, &mut *hd) {
        0 => Ok(hd),
        e => Err(e),
    }
}

fn name_of(hd: &RARHeaderDataEx) -> String {
    let n = hd.FileName;
    let bytes: Vec<u8> = n.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8(bytes).unwrap()
}

#[test]
fn struct_layout_matches_libunrar() {
    assert_eq!(size_of::<RARHeaderData>(), 576);
    assert_eq!(size_of::<RARHeaderDataEx>(), 14340);
    assert_eq!(size_of::<RAROpenArchiveData>(), 36);
    assert_eq!(size_of::<RAROpenArchiveDataEx>(), 176);
}

#[test]
fn extracts_every_format() {
    for (i, (format, solid)) in [
        (ArchiveVersion::Rar50, false),
        (ArchiveVersion::Rar50, true),
        (ArchiveVersion::Rar29, false),
        (ArchiveVersion::Rar29, true),
    ]
    .into_iter()
    .enumerate()
    {
        let dir = scratch(&format!("extract{i}"));
        let arc = dir.join("a.rar");
        std::fs::write(&arc, build(format, solid, None)).unwrap();
        let out = dir.join("out");
        let out_c = CString::new(out.to_str().unwrap()).unwrap();
        unsafe {
            let (h, res) = open(&arc, RAR_OM_EXTRACT);
            assert_eq!(res, 0);
            let mut names = Vec::new();
            loop {
                let hd = match header(h) {
                    Ok(hd) => hd,
                    Err(e) => {
                        assert_eq!(e, ERAR_END_ARCHIVE);
                        break;
                    }
                };
                names.push(name_of(&hd));
                assert_eq!(RARProcessFile(h, RAR_EXTRACT, out_c.as_ptr() as *mut _, std::ptr::null_mut()), 0);
            }
            assert_eq!(RARCloseArchive(h), 0);
            assert_eq!(names, ["one.bin", "dir/two.txt"]);
        }
        assert_eq!(std::fs::read(out.join("one.bin")).unwrap(), payload());
        assert_eq!(std::fs::read(out.join("dir/two.txt")).unwrap(), b"second");
        let meta = std::fs::metadata(out.join("one.bin")).unwrap();
        let mtime = meta.modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap();
        if format == ArchiveVersion::Rar50 { assert!(mtime.as_secs().abs_diff(1_600_000_000) <= 2, "{format:?} {}", mtime.as_secs()); }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn skipping_then_extracting_later_member_works_when_solid() {
    let dir = scratch("solid-skip");
    let arc = dir.join("a.rar");
    std::fs::write(&arc, build(ArchiveVersion::Rar50, true, None)).unwrap();
    let out = CString::new(dir.join("out").to_str().unwrap()).unwrap();
    unsafe {
        let (h, _) = open(&arc, RAR_OM_EXTRACT);
        header(h).unwrap();
        assert_eq!(RARProcessFile(h, RAR_SKIP, std::ptr::null_mut(), std::ptr::null_mut()), 0);
        header(h).unwrap();
        assert_eq!(RARProcessFile(h, RAR_EXTRACT, out.as_ptr() as *mut _, std::ptr::null_mut()), 0);
        RARCloseArchive(h);
    }
    assert_eq!(std::fs::read(dir.join("out/dir/two.txt")).unwrap(), b"second");
    assert!(!dir.join("out/one.bin").exists());
}

#[test]
fn passwords() {
    let dir = scratch("password");
    let arc = dir.join("a.rar");
    std::fs::write(&arc, build(ArchiveVersion::Rar50, false, Some(b"pw"))).unwrap();
    let out = CString::new(dir.join("out").to_str().unwrap()).unwrap();
    unsafe {
        for (pw, expected) in [("pw", 0), ("nope", ERAR_BAD_PASSWORD), ("", ERAR_MISSING_PASSWORD)] {
            let (h, res) = open(&arc, RAR_OM_EXTRACT);
            assert_eq!(res, 0);
            let pw = CString::new(pw).unwrap();
            RARSetPassword(h, pw.as_ptr() as *mut _);
            let hd = header(h).unwrap();
            assert_ne!(hd.Flags & RHDF_ENCRYPTED, 0);
            let code = RARProcessFile(h, RAR_TEST, out.as_ptr() as *mut _, std::ptr::null_mut());
            assert_eq!(code, expected);
            RARCloseArchive(h);
        }
    }
}

#[test]
fn processdata_callback_can_abort() {
    unsafe extern "system" fn cb(msg: u32, _ud: isize, _p1: isize, _p2: isize) -> i32 {
        if msg == UCM_PROCESSDATA {
            -1
        } else {
            0
        }
    }
    let dir = scratch("abort");
    let arc = dir.join("a.rar");
    std::fs::write(&arc, build(ArchiveVersion::Rar29, false, None)).unwrap();
    unsafe {
        let (h, _) = open(&arc, RAR_OM_EXTRACT);
        RARSetCallback(h, Some(cb), 0);
        header(h).unwrap();
        assert_eq!(
            RARProcessFile(h, RAR_TEST, std::ptr::null_mut(), std::ptr::null_mut()),
            ERAR_UNKNOWN
        );
        RARCloseArchive(h);
    }
}

#[test]
fn legacy_unix_symlink_is_reported_and_extracted() {
    use rars::rar15_40::{write_stored_archive, StoredEntry, WriterOptions};

    unsafe extern "system" fn cb(msg: u32, ud: isize, _p1: isize, _p2: isize) -> i32 {
        if msg == UCM_PROCESSDATA {
            unsafe { *(ud as *mut u32) += 1 };
        }
        0
    }

    let dir = scratch("legacy-link");
    let arc = dir.join("a.rar");
    let entry = StoredEntry {
        name: b"link",
        data: b"target.txt",
        file_time: 0,
        file_attr: 0o120777,
        host_os: 3,
        password: None,
        file_comment: None,
    };
    let bytes = write_stored_archive(
        &[entry],
        WriterOptions::new(ArchiveVersion::Rar40, rars::FeatureSet::store_only()),
    )
    .unwrap();
    std::fs::write(&arc, bytes).unwrap();

    let out = dir.join("out");
    let out_c = CString::new(out.to_str().unwrap()).unwrap();
    let mut chunks = 0u32;
    unsafe {
        let (h, res) = open(&arc, RAR_OM_EXTRACT);
        assert_eq!(res, 0);
        RARSetCallback(h, Some(cb), &mut chunks as *mut u32 as isize);
        let hd = header(h).unwrap();
        assert_eq!(name_of(&hd), "link");
        let redir_type = hd.RedirType;
        assert_eq!(redir_type, 1);
        // Link payloads are consumed silently, as in libunrar.
        assert_eq!(RARProcessFile(h, RAR_TEST, out_c.as_ptr() as *mut _, std::ptr::null_mut()), 0);
        assert_eq!(chunks, 0);
        assert_eq!(RARCloseArchive(h), 0);
    }
    unsafe {
        let (h, res) = open(&arc, RAR_OM_EXTRACT);
        assert_eq!(res, 0);
        header(h).unwrap();
        assert_eq!(RARProcessFile(h, RAR_EXTRACT, out_c.as_ptr() as *mut _, std::ptr::null_mut()), 0);
        assert_eq!(RARCloseArchive(h), 0);
    }
    #[cfg(unix)]
    assert_eq!(std::fs::read_link(out.join("link")).unwrap(), std::path::Path::new("target.txt"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn open_errors() {
    let dir = scratch("errors");
    unsafe {
        let (h, res) = open(&dir.join("missing.rar"), RAR_OM_EXTRACT);
        assert!(h.is_null());
        assert_eq!(res as i32, ERAR_EOPEN);
        std::fs::write(dir.join("junk.rar"), vec![7u8; 4096]).unwrap();
        let (h, res) = open(&dir.join("junk.rar"), RAR_OM_EXTRACT);
        assert!(h.is_null());
        assert_eq!(res as i32, ERAR_BAD_ARCHIVE);
    }
    assert_eq!(RARGetDllVersion(), RAR_DLL_VERSION);
}
