//! libunrar-compatible C ABI implemented on top of the `rars` crate.
#![allow(clippy::missing_safety_doc)]

pub mod abi;
mod session;
mod util;

use abi::*;
use util::{wide_decode, wide_encode};
use session::{Handle, Header};
use std::os::raw::{c_char, c_int, c_void};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::{addr_of, addr_of_mut};

fn guard<T>(default: T, f: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(default)
}

unsafe fn cstr_bytes<'a>(p: *const c_char) -> Option<&'a [u8]> {
    (!p.is_null()).then(|| std::ffi::CStr::from_ptr(p).to_bytes())
}

unsafe fn wide_to_utf8(p: *const wchar) -> Option<Vec<u8>> {
    if p.is_null() {
        return None;
    }
    let mut units = Vec::new();
    let mut i = 0;
    loop {
        let c = p.add(i).read();
        if c == 0 {
            break;
        }
        units.push(c);
        i += 1;
    }
    Some(wide_decode(&units).into_bytes())
}

unsafe fn put_narrow(dst: *mut c_char, cap: usize, src: &[u8]) {
    let mut n = src.len().min(cap - 1);
    if let Ok(s) = std::str::from_utf8(src) {
        while n > 0 && !s.is_char_boundary(n) {
            n -= 1;
        }
    }
    std::ptr::copy_nonoverlapping(src.as_ptr() as *const c_char, dst, n);
    dst.add(n).write(0);
}

unsafe fn put_wide(dst: *mut wchar, cap: usize, src: &[u8]) {
    let mut n = 0;
    for ch in String::from_utf8_lossy(src).chars() {
        let mut buf = [0 as wchar; 2];
        let units = wide_encode(ch, &mut buf);
        if n + units.len() >= cap {
            break;
        }
        for &u in units.iter() {
            dst.add(n).write(u);
            n += 1;
        }
    }
    dst.add(n).write(0);
}

unsafe fn fill_comment(h: &Handle, buf: *mut c_char, size: u32, out_size: *mut u32, out_state: *mut u32) {
    out_size.write_unaligned(0);
    out_state.write_unaligned(0);
    if buf.is_null() || size == 0 {
        return;
    }
    match h.comment() {
        Ok(Some(c)) => {
            let n = c.len().min(size as usize - 1);
            std::ptr::copy_nonoverlapping(c.as_ptr() as *const c_char, buf, n);
            buf.add(n).write(0);
            out_size.write_unaligned(n as u32 + 1);
            out_state.write_unaligned(if n < c.len() { ERAR_SMALL_BUF as u32 } else { 1 });
        }
        Ok(None) => {}
        Err(code) => out_state.write_unaligned(code as u32),
    }
}

unsafe fn do_open(
    name: Vec<u8>,
    mode: u32,
    callback: UNRARCALLBACK,
    user: LPARAM,
    op_flags: u32,
) -> Result<Box<Handle>, c_int> {
    Handle::open(name, mode, callback, user, op_flags, None).map(Box::new)
}

#[no_mangle]
pub unsafe extern "system" fn RAROpenArchive(d: *mut RAROpenArchiveData) -> *mut c_void {
    guard(std::ptr::null_mut(), || {
        if d.is_null() {
            return std::ptr::null_mut();
        }
        let Some(name) = cstr_bytes((*d).ArcName) else {
            addr_of_mut!((*d).OpenResult).write_unaligned(ERAR_EOPEN as u32);
            return std::ptr::null_mut();
        };
        match do_open(name.to_vec(), (*d).OpenMode, None, 0, 0) {
            Ok(h) => {
                addr_of_mut!((*d).OpenResult).write_unaligned(0);
                fill_comment(
                    &h,
                    (*d).CmtBuf,
                    (*d).CmtBufSize,
                    addr_of_mut!((*d).CmtSize),
                    addr_of_mut!((*d).CmtState),
                );
                Box::into_raw(h) as *mut c_void
            }
            Err(code) => {
                addr_of_mut!((*d).OpenResult).write_unaligned(code as u32);
                std::ptr::null_mut()
            }
        }
    })
}

#[no_mangle]
pub unsafe extern "system" fn RAROpenArchiveEx(d: *mut RAROpenArchiveDataEx) -> *mut c_void {
    guard(std::ptr::null_mut(), || {
        if d.is_null() {
            return std::ptr::null_mut();
        }
        let name = cstr_bytes((*d).ArcName)
            .map(<[u8]>::to_vec)
            .or_else(|| wide_to_utf8((*d).ArcNameW));
        let Some(name) = name else {
            addr_of_mut!((*d).OpenResult).write_unaligned(ERAR_EOPEN as u32);
            return std::ptr::null_mut();
        };
        let callback = addr_of!((*d).Callback).read_unaligned();
        let user = addr_of!((*d).UserData).read_unaligned();
        match do_open(name, (*d).OpenMode, callback, user, (*d).OpFlags) {
            Ok(mut h) => {
                h.max_disk_write =
                    u64::from(addr_of!((*d).MaxDiskWrite).read_unaligned()) << 20;
                addr_of_mut!((*d).OpenResult).write_unaligned(0);
                addr_of_mut!((*d).Flags).write_unaligned(h.open_flags());
                fill_comment(
                    &h,
                    (*d).CmtBuf,
                    (*d).CmtBufSize,
                    addr_of_mut!((*d).CmtSize),
                    addr_of_mut!((*d).CmtState),
                );
                Box::into_raw(h) as *mut c_void
            }
            Err(code) => {
                addr_of_mut!((*d).OpenResult).write_unaligned(code as u32);
                std::ptr::null_mut()
            }
        }
    })
}

#[no_mangle]
pub unsafe extern "system" fn RARCloseArchive(handle: *mut c_void) -> c_int {
    guard(ERAR_UNKNOWN, || {
        if handle.is_null() {
            return ERAR_ECLOSE;
        }
        drop(Box::from_raw(handle as *mut Handle));
        ERAR_SUCCESS
    })
}

macro_rules! common_header_fields {
    ($d:expr, $h:expr) => {{
        addr_of_mut!((*$d).Flags).write_unaligned($h.flags);
        addr_of_mut!((*$d).PackSize).write_unaligned($h.pack_size as u32);
        addr_of_mut!((*$d).UnpSize).write_unaligned($h.unp_size as u32);
        addr_of_mut!((*$d).HostOS).write_unaligned($h.host_os);
        addr_of_mut!((*$d).FileCRC).write_unaligned($h.crc);
        addr_of_mut!((*$d).FileTime).write_unaligned($h.file_time);
        addr_of_mut!((*$d).UnpVer).write_unaligned($h.unp_ver);
        addr_of_mut!((*$d).Method).write_unaligned($h.method);
        addr_of_mut!((*$d).FileAttr).write_unaligned($h.attr);
        addr_of_mut!((*$d).CmtSize).write_unaligned(0);
        addr_of_mut!((*$d).CmtState).write_unaligned(0);
    }};
}

unsafe fn fill_header(d: *mut RARHeaderData, arc: &[u8], h: &Header) {
    put_narrow(addr_of_mut!((*d).ArcName) as *mut c_char, 260, arc);
    put_narrow(addr_of_mut!((*d).FileName) as *mut c_char, 260, &h.name);
    common_header_fields!(d, h);
}

unsafe fn fill_header_ex(d: *mut RARHeaderDataEx, arc: &[u8], h: &Header) {
    put_narrow(addr_of_mut!((*d).ArcName) as *mut c_char, 1024, arc);
    put_wide(addr_of_mut!((*d).ArcNameW) as *mut wchar, 1024, arc);
    put_narrow(addr_of_mut!((*d).FileName) as *mut c_char, 1024, &h.name);
    put_wide(addr_of_mut!((*d).FileNameW) as *mut wchar, 1024, &h.name);
    common_header_fields!(d, h);
    addr_of_mut!((*d).PackSizeHigh).write_unaligned((h.pack_size >> 32) as u32);
    addr_of_mut!((*d).UnpSizeHigh).write_unaligned((h.unp_size >> 32) as u32);
    addr_of_mut!((*d).DictSize).write_unaligned(h.dict_kb);
    addr_of_mut!((*d).HashType).write_unaligned(h.hash_type);
    std::ptr::copy_nonoverlapping(h.hash.as_ptr(), addr_of_mut!((*d).Hash) as *mut u8, 32);
    addr_of_mut!((*d).RedirType).write_unaligned(h.redir_type);
    addr_of_mut!((*d).DirTarget).write_unaligned(h.dir_target);
    let redir = addr_of!((*d).RedirName).read_unaligned();
    let redir_cap = addr_of!((*d).RedirNameSize).read_unaligned() as usize;
    if !redir.is_null() && redir_cap > 0 {
        put_wide(redir, redir_cap, &h.redir_name);
    }
    addr_of_mut!((*d).MtimeLow).write_unaligned(h.mtime as u32);
    addr_of_mut!((*d).MtimeHigh).write_unaligned((h.mtime >> 32) as u32);
    addr_of_mut!((*d).CtimeLow).write_unaligned(h.ctime as u32);
    addr_of_mut!((*d).CtimeHigh).write_unaligned((h.ctime >> 32) as u32);
    addr_of_mut!((*d).AtimeLow).write_unaligned(h.atime as u32);
    addr_of_mut!((*d).AtimeHigh).write_unaligned((h.atime >> 32) as u32);
    let p = addr_of!((*d).ArcNameEx).read_unaligned();
    let cap = addr_of!((*d).ArcNameExSize).read_unaligned() as usize;
    if !p.is_null() && cap > 0 {
        put_wide(p, cap, arc);
    }
    let p = addr_of!((*d).FileNameEx).read_unaligned();
    let cap = addr_of!((*d).FileNameExSize).read_unaligned() as usize;
    if !p.is_null() && cap > 0 {
        put_wide(p, cap, &h.name);
    }
}

#[no_mangle]
pub unsafe extern "system" fn RARReadHeader(handle: *mut c_void, d: *mut RARHeaderData) -> c_int {
    guard(ERAR_UNKNOWN, || {
        if handle.is_null() || d.is_null() {
            return ERAR_UNKNOWN;
        }
        let h = &mut *(handle as *mut Handle);
        match h.read_header() {
            Ok(hd) => {
                fill_header(d, &h.arc_name, &hd);
                ERAR_SUCCESS
            }
            Err(c) => c,
        }
    })
}

#[no_mangle]
pub unsafe extern "system" fn RARReadHeaderEx(handle: *mut c_void, d: *mut RARHeaderDataEx) -> c_int {
    guard(ERAR_UNKNOWN, || {
        if handle.is_null() || d.is_null() {
            return ERAR_UNKNOWN;
        }
        let h = &mut *(handle as *mut Handle);
        match h.read_header() {
            Ok(hd) => {
                fill_header_ex(d, &h.arc_name, &hd);
                ERAR_SUCCESS
            }
            Err(c) => c,
        }
    })
}

#[no_mangle]
pub unsafe extern "system" fn RARProcessFile(
    handle: *mut c_void,
    op: c_int,
    dest_path: *mut c_char,
    dest_name: *mut c_char,
) -> c_int {
    guard(ERAR_UNKNOWN, || {
        if handle.is_null() {
            return ERAR_UNKNOWN;
        }
        let h = &mut *(handle as *mut Handle);
        h.process_file(op, cstr_bytes(dest_path), cstr_bytes(dest_name))
    })
}

#[no_mangle]
pub unsafe extern "system" fn RARProcessFileW(
    handle: *mut c_void,
    op: c_int,
    dest_path: *mut wchar,
    dest_name: *mut wchar,
) -> c_int {
    guard(ERAR_UNKNOWN, || {
        if handle.is_null() {
            return ERAR_UNKNOWN;
        }
        let h = &mut *(handle as *mut Handle);
        let (p, n) = (wide_to_utf8(dest_path), wide_to_utf8(dest_name));
        h.process_file(op, p.as_deref(), n.as_deref())
    })
}

#[no_mangle]
pub unsafe extern "system" fn RARSetCallback(handle: *mut c_void, cb: UNRARCALLBACK, user: LPARAM) {
    guard((), || {
        if let Some(h) = (handle as *mut Handle).as_mut() {
            h.callback = cb;
            h.user_data = user;
        }
    })
}

#[no_mangle]
pub unsafe extern "system" fn RARSetChangeVolProc(handle: *mut c_void, p: CHANGEVOLPROC) {
    guard((), || {
        if let Some(h) = (handle as *mut Handle).as_mut() {
            h.change_vol = p;
        }
    })
}

#[no_mangle]
pub unsafe extern "system" fn RARSetProcessDataProc(handle: *mut c_void, p: PROCESSDATAPROC) {
    guard((), || {
        if let Some(h) = (handle as *mut Handle).as_mut() {
            h.process_data = p;
        }
    })
}

#[no_mangle]
pub unsafe extern "system" fn RARSetPassword(handle: *mut c_void, password: *mut c_char) {
    guard((), || {
        if let Some(h) = (handle as *mut Handle).as_mut() {
            h.password = cstr_bytes(password).filter(|p| !p.is_empty()).map(<[u8]>::to_vec);
        }
    })
}

#[no_mangle]
pub extern "system" fn RARGetDllVersion() -> c_int {
    RAR_DLL_VERSION
}
