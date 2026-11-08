//! C-visible types and constants of the libunrar interface (packed layout).
#![allow(non_snake_case, non_camel_case_types, dead_code)]

use std::os::raw::{c_char, c_int, c_uint};

/// Pointer-sized integer (`LPARAM` in the Windows header, `long` elsewhere).
pub type LPARAM = isize;

#[cfg(windows)]
pub type wchar = u16;
#[cfg(not(windows))]
pub type wchar = u32;

pub const ERAR_SUCCESS: c_int = 0;
pub const ERAR_END_ARCHIVE: c_int = 10;
pub const ERAR_NO_MEMORY: c_int = 11;
pub const ERAR_BAD_DATA: c_int = 12;
pub const ERAR_BAD_ARCHIVE: c_int = 13;
pub const ERAR_UNKNOWN_FORMAT: c_int = 14;
pub const ERAR_EOPEN: c_int = 15;
pub const ERAR_ECREATE: c_int = 16;
pub const ERAR_ECLOSE: c_int = 17;
pub const ERAR_EREAD: c_int = 18;
pub const ERAR_EWRITE: c_int = 19;
pub const ERAR_SMALL_BUF: c_int = 20;
pub const ERAR_UNKNOWN: c_int = 21;
pub const ERAR_MISSING_PASSWORD: c_int = 22;
pub const ERAR_EREFERENCE: c_int = 23;
pub const ERAR_BAD_PASSWORD: c_int = 24;
pub const ERAR_LARGE_DICT: c_int = 25;
pub const ERAR_USER_LIMIT: c_int = 26;

pub const RAR_OM_LIST: u32 = 0;
pub const RAR_OM_EXTRACT: u32 = 1;
pub const RAR_OM_LIST_INCSPLIT: u32 = 2;

pub const RAR_SKIP: c_int = 0;
pub const RAR_TEST: c_int = 1;
pub const RAR_EXTRACT: c_int = 2;

pub const RAR_VOL_ASK: c_int = 0;
pub const RAR_VOL_NOTIFY: c_int = 1;

pub const RAR_DLL_VERSION: c_int = 11;

pub const RAR_HASH_NONE: c_uint = 0;
pub const RAR_HASH_CRC32: c_uint = 1;
pub const RAR_HASH_BLAKE2: c_uint = 2;

pub const RHDF_SPLITBEFORE: u32 = 0x01;
pub const RHDF_SPLITAFTER: u32 = 0x02;
pub const RHDF_ENCRYPTED: u32 = 0x04;
pub const RHDF_SOLID: u32 = 0x10;
pub const RHDF_DIRECTORY: u32 = 0x20;

pub const ROADF_VOLUME: u32 = 0x0001;
pub const ROADF_COMMENT: u32 = 0x0002;
pub const ROADF_LOCK: u32 = 0x0004;
pub const ROADF_SOLID: u32 = 0x0008;
pub const ROADF_NEWNUMBERING: u32 = 0x0010;
pub const ROADF_SIGNED: u32 = 0x0020;
pub const ROADF_RECOVERY: u32 = 0x0040;
pub const ROADF_ENCHEADERS: u32 = 0x0080;
pub const ROADF_FIRSTVOLUME: u32 = 0x0100;

pub const ROADOF_KEEPBROKEN: u32 = 0x0001;
pub const ROADOF_SHARED: u32 = 0x0002;

pub const UCM_CHANGEVOLUME: u32 = 0;
pub const UCM_PROCESSDATA: u32 = 1;
pub const UCM_NEEDPASSWORD: u32 = 2;
pub const UCM_CHANGEVOLUMEW: u32 = 3;
pub const UCM_NEEDPASSWORDW: u32 = 4;
pub const UCM_LARGEDICT: u32 = 5;

pub type UNRARCALLBACK = Option<unsafe extern "system" fn(u32, LPARAM, LPARAM, LPARAM) -> c_int>;
pub type CHANGEVOLPROC = Option<unsafe extern "system" fn(*mut c_char, c_int) -> c_int>;
pub type PROCESSDATAPROC = Option<unsafe extern "system" fn(*mut u8, c_int) -> c_int>;

#[repr(C, packed)]
pub struct RARHeaderData {
    pub ArcName: [c_char; 260],
    pub FileName: [c_char; 260],
    pub Flags: u32,
    pub PackSize: u32,
    pub UnpSize: u32,
    pub HostOS: u32,
    pub FileCRC: u32,
    pub FileTime: u32,
    pub UnpVer: u32,
    pub Method: u32,
    pub FileAttr: u32,
    pub CmtBuf: *mut c_char,
    pub CmtBufSize: u32,
    pub CmtSize: u32,
    pub CmtState: u32,
}

#[repr(C, packed)]
pub struct RARHeaderDataEx {
    pub ArcName: [c_char; 1024],
    pub ArcNameW: [wchar; 1024],
    pub FileName: [c_char; 1024],
    pub FileNameW: [wchar; 1024],
    pub Flags: u32,
    pub PackSize: u32,
    pub PackSizeHigh: u32,
    pub UnpSize: u32,
    pub UnpSizeHigh: u32,
    pub HostOS: u32,
    pub FileCRC: u32,
    pub FileTime: u32,
    pub UnpVer: u32,
    pub Method: u32,
    pub FileAttr: u32,
    pub CmtBuf: *mut c_char,
    pub CmtBufSize: u32,
    pub CmtSize: u32,
    pub CmtState: u32,
    pub DictSize: u32,
    pub HashType: u32,
    pub Hash: [c_char; 32],
    pub RedirType: u32,
    pub RedirName: *mut wchar,
    pub RedirNameSize: u32,
    pub DirTarget: u32,
    pub MtimeLow: u32,
    pub MtimeHigh: u32,
    pub CtimeLow: u32,
    pub CtimeHigh: u32,
    pub AtimeLow: u32,
    pub AtimeHigh: u32,
    pub ArcNameEx: *mut wchar,
    pub ArcNameExSize: u32,
    pub FileNameEx: *mut wchar,
    pub FileNameExSize: u32,
    pub Reserved: [u32; 982],
}

#[repr(C, packed)]
pub struct RAROpenArchiveData {
    pub ArcName: *mut c_char,
    pub OpenMode: u32,
    pub OpenResult: u32,
    pub CmtBuf: *mut c_char,
    pub CmtBufSize: u32,
    pub CmtSize: u32,
    pub CmtState: u32,
}

#[repr(C, packed)]
pub struct RAROpenArchiveDataEx {
    pub ArcName: *mut c_char,
    pub ArcNameW: *mut wchar,
    pub OpenMode: u32,
    pub OpenResult: u32,
    pub CmtBuf: *mut c_char,
    pub CmtBufSize: u32,
    pub CmtSize: u32,
    pub CmtState: u32,
    pub Flags: u32,
    pub Callback: UNRARCALLBACK,
    pub UserData: LPARAM,
    pub OpFlags: u32,
    pub CmtBufW: *mut wchar,
    pub MarkOfTheWeb: *mut wchar,
    pub MaxDiskWrite: u32,
    pub MaxAllocate: u32,
    pub MaxRuntime: u32,
    pub Reserved: [u32; 20],
}
