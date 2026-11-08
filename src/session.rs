//! Archive handle: bridges libunrar's pull-style header/process API onto rars'
//! push-style extraction by running the extraction on a worker thread that
//! pauses at every member until the caller decides what to do with it.

use crate::abi::*;
use crate::util::*;
use rars::{
    Archive, ArchiveFamily, ArchiveMember, ArchiveMemberDetail, ArchiveMemberHash,
    ArchiveReadOptions, ExtractionDecision, ExtractionErrorAction, FileTimestamp,
};
use std::io::{BufWriter, Write};
use std::os::raw::{c_char, c_int};
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

enum Event {
    Member,
    Chunk(Vec<u8>),
    Failed(rars::Error),
    Done(Result<(), rars::Error>),
}

#[derive(Clone, Copy, PartialEq)]
enum Cmd {
    Skip,
    Extract,
}

struct Worker {
    cmd_tx: Sender<Cmd>,
    ack_tx: Sender<bool>,
    ev_rx: Receiver<Event>,
    join: Option<JoinHandle<()>>,
    password: Option<Vec<u8>>,
    consumed: usize,
    pending: Option<Event>,
    finished: bool,
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Closing the channels makes the worker's next callback fail, ending it.
        let (dead_tx, _) = channel();
        self.cmd_tx = dead_tx;
        let (dead_ack, _) = channel();
        self.ack_tx = dead_ack;
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

struct ChunkWriter {
    ev_tx: Sender<Event>,
    ack_rx: Arc<Mutex<Receiver<bool>>>,
}

impl Write for ChunkWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let aborted = || std::io::Error::new(std::io::ErrorKind::Other, "extraction aborted");
        self.ev_tx.send(Event::Chunk(buf.to_vec())).map_err(|_| aborted())?;
        match self.ack_rx.lock().unwrap().recv() {
            Ok(true) => Ok(buf.len()),
            _ => Err(aborted()),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn worker_main(
    archives: Vec<Archive>,
    password: Option<Vec<u8>>,
    solid: bool,
    ev_tx: Sender<Event>,
    cmd_rx: Receiver<Cmd>,
    ack_rx: Receiver<bool>,
) {
    let options =
        ArchiveReadOptions::with_optional_password(password.as_deref()).with_rar50_buffered_decode_limit(0);
    let ack_rx = Arc::new(Mutex::new(ack_rx));
    let ask = |ev_tx: &Sender<Event>| -> rars::Result<Cmd> {
        ev_tx.send(Event::Member).map_err(|_| rars::Error::Cancelled)?;
        cmd_rx.recv().map_err(|_| rars::Error::Cancelled)
    };
    let writer = |ev_tx: &Sender<Event>| -> Box<dyn Write> {
        Box::new(ChunkWriter { ev_tx: ev_tx.clone(), ack_rx: ack_rx.clone() })
    };
    let result = if archives.len() == 1 {
        let err_tx = ev_tx.clone();
        archives[0]
            .extract_with_control_and_errors(
                options,
                |m| {
                    Ok(match ask(&ev_tx)? {
                        Cmd::Extract => ExtractionDecision::Extract(writer(&ev_tx)),
                        Cmd::Skip if m.meta.is_directory || !solid => ExtractionDecision::Skip,
                        Cmd::Skip => ExtractionDecision::Extract(Box::new(std::io::sink())),
                    })
                },
                |_, e| {
                    let _ = err_tx.send(Event::Failed(e.clone()));
                    Ok(ExtractionErrorAction::Continue)
                },
            )
            .map(|_| ())
    } else {
        rars::extract_volumes_to_with_options(&archives, options, |_| {
            Ok(match ask(&ev_tx)? {
                Cmd::Extract => writer(&ev_tx),
                Cmd::Skip => Box::new(std::io::sink()) as Box<dyn Write>,
            })
        })
    };
    let _ = ev_tx.send(Event::Done(result));
}

pub struct Handle {
    pub arc_name: Vec<u8>,
    pub open_mode: u32,
    pub op_flags: u32,
    pub password: Option<Vec<u8>>,
    pub callback: UNRARCALLBACK,
    pub user_data: LPARAM,
    pub change_vol: CHANGEVOLPROC,
    pub process_data: PROCESSDATAPROC,
    archives: Vec<Archive>,
    headers: Vec<ArchiveMember>,
    members: Vec<ArchiveMember>,
    worker_index: Vec<usize>,
    ordinal: Vec<Option<usize>>,
    legacy_flags: Vec<Option<u16>>,
    volume_paths: Vec<Vec<u8>>,
    header_volume: Vec<usize>,
    header_end_volume: Vec<usize>,
    notified: usize,
    poison: Option<c_int>,
    /// Cap on bytes written to disk by this handle; zero means unlimited.
    pub max_disk_write: u64,
    disk_written: u64,
    cut_tail: bool,
    no_main: bool,
    solid: bool,
    volumes_loaded: bool,
    cursor: usize,
    current: Option<usize>,
    worker: Option<Worker>,
    abort_code: Option<c_int>,
}

pub struct Header {
    pub name: Vec<u8>,
    pub flags: u32,
    pub pack_size: u64,
    pub unp_size: u64,
    pub host_os: u32,
    pub crc: u32,
    pub file_time: u32,
    pub unp_ver: u32,
    pub method: u32,
    pub attr: u32,
    pub dict_kb: u32,
    pub hash_type: u32,
    pub hash: [u8; 32],
    pub redir_type: u32,
    pub redir_name: Vec<u8>,
    pub dir_target: u32,
    pub mtime: u64,
    pub ctime: u64,
    pub atime: u64,
}

#[cfg(unix)]
fn path_of(bytes: &[u8]) -> PathBuf {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
fn path_of(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

fn archive_solid(a: &Archive) -> bool {
    match a {
        Archive::Rar13(x) => x.main.is_solid(),
        Archive::Rar15To40(x) => x.main.is_solid(),
        Archive::Rar50Plus(x) => x.main.is_solid(),
        _ => false,
    }
}

fn has_next_volume(a: &Archive) -> bool {
    if a.members().last().is_some_and(|m| m.meta.is_split_after) {
        return true;
    }
    match a {
        Archive::Rar50Plus(x) => x
            .blocks
            .iter()
            .any(|b| matches!(b, rars::rar50::Block::End(e) if e.flags & 1 != 0)),
        Archive::Rar15To40(x) => x
            .blocks
            .iter()
            .any(|b| matches!(b, rars::rar15_40::Block::End(h) if h.flags & 1 != 0)),
        _ => false,
    }
}

/// Presents a file followed by a long run of zero bytes, with an end-of-archive
/// block spliced in at `end_at`, so that an archive cut short inside a member
/// still parses; extracting that member then fails its integrity check
/// instead of the whole archive failing to open.
struct Completed {
    file: std::fs::File,
    len: u64,
    pos: u64,
    end_at: u64,
    end_block: &'static [u8],
    // Once parsing is over, reading the missing payload is an error.
    strict: Arc<AtomicBool>,
}

const PAD: u64 = 1 << 36;
const RAR5_END: &[u8] = &[0x1D, 0x77, 0x56, 0x51, 0x03, 0x05, 0x04, 0x00];
const RAR4_END: &[u8] = &[0xC4, 0x3D, 0x7B, 0x00, 0x40, 0x07, 0x00];

impl std::io::Read for Completed {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let total = self.len + PAD;
        if self.pos >= total {
            return Ok(0);
        }
        let want = buf.len().min((total - self.pos) as usize);
        let buf = &mut buf[..want];
        let n = if self.pos < self.len {
            let n = want.min((self.len - self.pos) as usize);
            std::io::Seek::seek(&mut self.file, std::io::SeekFrom::Start(self.pos))?;
            let n = std::io::Read::read(&mut self.file, &mut buf[..n])?;
            if n == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            n
        } else {
            buf.fill(0);
            let end = self.end_at + self.end_block.len() as u64;
            if (self.pos < self.end_at || self.pos >= end) && self.strict.load(Ordering::Relaxed) {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            if self.pos >= self.end_at && self.pos < end {
                want.min((end - self.pos) as usize)
            } else {
                want
            }
        };
        let end = self.end_at + self.end_block.len() as u64;
        for (i, byte) in buf[..n].iter_mut().enumerate() {
            let at = self.pos + i as u64;
            if at >= self.end_at && at < end {
                *byte = self.end_block[(at - self.end_at) as usize];
            }
        }
        self.pos += n as u64;
        Ok(n)
    }
}

impl std::io::Seek for Completed {
    fn seek(&mut self, to: std::io::SeekFrom) -> std::io::Result<u64> {
        let total = (self.len + PAD) as i128;
        let target = match to {
            std::io::SeekFrom::Start(n) => n as i128,
            std::io::SeekFrom::End(d) => total + d as i128,
            std::io::SeekFrom::Current(d) => self.pos as i128 + d as i128,
        };
        if target < 0 {
            return Err(std::io::ErrorKind::InvalidInput.into());
        }
        self.pos = target as u64;
        Ok(self.pos)
    }
}

/// Walks the block chain of a RAR 1.5-4.x archive and returns the offset at
/// which the (possibly truncated) chain stops, and whether a header was cut.
fn legacy_end_of_blocks(
    file: &mut std::fs::File,
    len: u64,
    verify: bool,
) -> Option<(u64, bool)> {
    use std::io::{Read, Seek, SeekFrom};
    let mut pos = 7u64;
    loop {
        let mut head = [0u8; 11];
        file.seek(SeekFrom::Start(pos)).ok()?;
        let avail = len.saturating_sub(pos).min(11) as usize;
        file.read_exact(&mut head[..avail]).ok()?;
        if avail < 7 {
            return Some((pos, avail > 0));
        }
        let kind = head[2];
        let flags = u16::from_le_bytes([head[3], head[4]]);
        let size = u64::from(u16::from_le_bytes([head[5], head[6]]));
        if kind == 0x7B || size < 7 {
            return Some((pos, false));
        }
        if pos + size > len {
            return Some((pos, true));
        }
        if verify {
            let mut block = vec![0u8; size as usize];
            file.seek(SeekFrom::Start(pos)).ok()?;
            file.read_exact(&mut block).ok()?;
            let crc = rars::crc32::crc32(&block[2..]) as u16;
            if crc != u16::from_le_bytes([block[0], block[1]]) {
                return Some((pos, true));
            }
        }
        let add = if flags & 0x8000 != 0 {
            u64::from(u32::from_le_bytes([head[7], head[8], head[9], head[10]]))
        } else {
            0
        };
        pos += size + add;
        if pos >= len {
            return Some((pos, false));
        }
    }
}

fn legacy_cut(path: &Path) -> bool {
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let Ok(len) = file.metadata().map(|m| m.len()) else {
        return false;
    };
    let mut sig = [0u8; 7];
    if std::io::Read::read_exact(&mut file, &mut sig).is_err() || &sig != b"Rar!\x1a\x07\x00" {
        return false;
    }
    matches!(legacy_end_of_blocks(&mut file, len, false), Some((_, true)))
}

/// A minimal valid archive standing in for one whose first header is cut off.
fn empty_archive(rar5: bool) -> Option<Archive> {
    let mut bytes = Vec::new();
    if rar5 {
        bytes.extend_from_slice(b"Rar!\x1a\x07\x01\x00");
        let body = [3u8, 1, 0, 0];
        bytes.extend_from_slice(&rars::crc32::crc32(&body).to_le_bytes());
        bytes.extend_from_slice(&body);
        bytes.extend_from_slice(RAR5_END);
    } else {
        bytes.extend_from_slice(b"Rar!\x1a\x07\x00");
        let mut body = vec![0x73u8, 0, 0, 13, 0, 0, 0, 0, 0, 0, 0];
        body.truncate(11);
        let crc = rars::crc32::crc32(&body) as u16;
        bytes.extend_from_slice(&crc.to_le_bytes());
        bytes.extend_from_slice(&body);
        bytes.extend_from_slice(RAR4_END);
    }
    rars::ArchiveReader::read_owned(bytes).ok()
}

/// Retries a failed parse on a completed copy of a damaged archive, ending it
/// where the parser gave up.
fn open_damaged(
    path: &Path,
    options: ArchiveReadOptions<'_>,
    first: &rars::Error,
) -> Option<(Archive, u8)> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut sig = [0u8; 8];
    std::io::Read::read_exact(&mut file, &mut sig).ok()?;
    let end_block = match &sig {
        [b'R', b'a', b'r', b'!', 0x1A, 0x07, 0x01, 0x00] => RAR5_END,
        [b'R', b'a', b'r', b'!', 0x1A, 0x07, 0x00, _] => RAR4_END,
        _ => return None,
    };
    let rar5 = end_block == RAR5_END;
    let corrupt_at = match first {
        rars::Error::AtArchiveOffset { offset, source }
            if !matches!(
                source.root_cause(),
                rars::Error::TooShort | rars::Error::Io(_)
            ) =>
        {
            Some(*offset as u64)
        }
        _ => None,
    };
    let (mut end_at, partial) = match corrupt_at {
        Some(at) => (at, true),
        None if rar5 => (len, false),
        None => legacy_end_of_blocks(
            &mut file,
            len,
            matches!(first, rars::Error::CrcMismatch { .. }),
        )?,
    };
    if !rar5 && end_at == 7 && partial {
        return empty_archive(false).map(|a| (a, 1));
    }
    for _ in 0..4 {
        let strict = Arc::new(AtomicBool::new(false));
        let source = Completed {
            strict: strict.clone(),
            file: file.try_clone().ok()?,
            len,
            pos: 0,
            end_at,
            end_block,
        };
        match rars::ArchiveReader::read_reader_with_options(source, options) {
            Ok(a) => {
                strict.store(true, Ordering::Relaxed);
                return Some((a, u8::from(partial)));
            }
            Err(rars::Error::AtArchiveOffset { offset, .. }) if offset as u64 != end_at => {
                end_at = offset as u64;
            }
            Err(_) => break,
        }
    }
    if rar5 {
        return empty_archive(true).map(|a| (a, if corrupt_at.is_some() { 1 } else { 2 }));
    }
    None
}

fn open_path(path: &Path, password: Option<&[u8]>) -> rars::Result<(Archive, u8)> {
    let options = ArchiveReadOptions::with_optional_password(password);
    let first = match rars::ArchiveReader::read_path_with_options(path, options) {
        Ok(a) => return Ok((a, u8::from(legacy_cut(path)))),
        Err(e) => e,
    };
    if !matches!(
        first.kind(),
        rars::ErrorKind::InvalidArchive | rars::ErrorKind::ChecksumMismatch
    ) {
        return Err(first);
    }
    open_damaged(path, options, &first).ok_or(first)
}

impl Handle {
    /// Opens the first volume, asking for a password if headers are encrypted.
    pub fn open(
        name: Vec<u8>,
        mode: u32,
        callback: UNRARCALLBACK,
        user_data: LPARAM,
        op_flags: u32,
        password: Option<Vec<u8>>,
    ) -> Result<Handle, c_int> {
        if mode > RAR_OM_LIST_INCSPLIT {
            return Err(ERAR_UNKNOWN);
        }
        let mut h = Handle {
            arc_name: name,
            open_mode: mode,
            op_flags,
            password,
            callback,
            user_data,
            change_vol: None,
            process_data: None,
            archives: Vec::new(),
            headers: Vec::new(),
            members: Vec::new(),
            worker_index: Vec::new(),
            ordinal: Vec::new(),
            legacy_flags: Vec::new(),
            volume_paths: Vec::new(),
            header_volume: Vec::new(),
            header_end_volume: Vec::new(),
            notified: 0,
            poison: None,
            max_disk_write: 0,
            disk_written: 0,
            cut_tail: false,
            no_main: false,
            solid: false,
            volumes_loaded: false,
            cursor: 0,
            current: None,
            worker: None,
            abort_code: None,
        };
        let first = loop {
            match open_path(&path_of(&h.arc_name), h.password.as_deref()) {
                Ok((a, cut)) => {
                    h.cut_tail = cut == 1;
                    h.no_main = cut == 2;
                    break a;
                }
                Err(e) if e.kind() == rars::ErrorKind::PasswordRequired && h.password.is_none() => {
                    if !h.ask_password() {
                        return Err(ERAR_MISSING_PASSWORD);
                    }
                }
                Err(e) => return Err(map_error(&e, true)),
            }
        };
        h.archives.push(first);
        h.volume_paths.push(h.arc_name.clone());
        if !has_next_volume(&h.archives[0]) {
            h.finish_loading();
        }
        Ok(h)
    }

    pub fn first_archive(&self) -> &Archive {
        &self.archives[0]
    }

    pub fn open_flags(&self) -> u32 {
        match self.first_archive() {
            Archive::Rar50Plus(x) => {
                let m = &x.main;
                let mut f = if self.no_main { 0 } else { ROADF_NEWNUMBERING };
                if m.is_volume() {
                    f |= ROADF_VOLUME;
                    if m.volume_number.unwrap_or(0) == 0 {
                        f |= ROADF_FIRSTVOLUME;
                    }
                }
                if m.is_solid() {
                    f |= ROADF_SOLID;
                }
                if m.is_locked() {
                    f |= ROADF_LOCK;
                }
                if m.has_recovery_record() {
                    f |= ROADF_RECOVERY;
                }
                if m.encrypted_headers {
                    f |= ROADF_ENCHEADERS;
                }
                if self.comment().ok().flatten().is_some() {
                    f |= ROADF_COMMENT;
                }
                f
            }
            Archive::Rar15To40(x) => u32::from(x.main.flags) & 0x1ff,
            Archive::Rar13(x) => {
                if x.main.is_solid() {
                    ROADF_SOLID
                } else {
                    0
                }
            }
            _ => 0,
        }
    }

    fn ask_password(&mut self) -> bool {
        let Some(cb) = self.callback else { return false };
        const CAP: usize = 128;
        let mut wide = [0u32; CAP];
        let r = unsafe {
            cb(UCM_NEEDPASSWORDW, self.user_data, wide.as_mut_ptr() as LPARAM, CAP as LPARAM)
        };
        let mut pw: Option<Vec<u8>> = None;
        if r != -1 && wide[0] != 0 {
            let s: String = wide
                .iter()
                .take_while(|&&c| c != 0)
                .map(|&c| char::from_u32(c).unwrap_or('\u{fffd}'))
                .collect();
            pw = Some(s.into_bytes());
        } else if r != -1 {
            let mut buf = [0u8; CAP * 4];
            let r = unsafe {
                cb(UCM_NEEDPASSWORD, self.user_data, buf.as_mut_ptr() as LPARAM, CAP as LPARAM)
            };
            if r != -1 && buf[0] != 0 {
                let n = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                pw = Some(buf[..n].to_vec());
            }
        }
        match pw {
            Some(p) => {
                self.password = Some(p);
                true
            }
            None => false,
        }
    }

    fn volume_callback(&mut self, path: &mut Vec<u8>, mode: c_int) -> bool {
        const NM: usize = 2048;
        let mut buf = vec![0u8; NM];
        let n = path.len().min(NM - 1);
        buf[..n].copy_from_slice(&path[..n]);
        let ok = unsafe {
            if let Some(cb) = self.callback {
                let msg = UCM_CHANGEVOLUME;
                cb(msg, self.user_data, buf.as_mut_ptr() as LPARAM, mode as LPARAM) != -1
            } else if let Some(p) = self.change_vol {
                p(buf.as_mut_ptr() as *mut c_char, mode) != 0
            } else {
                mode != RAR_VOL_ASK
            }
        };
        if ok && mode == RAR_VOL_ASK {
            let n = buf.iter().position(|&b| b == 0).unwrap_or(NM);
            *path = buf[..n].to_vec();
        }
        ok
    }

    /// Opens following volumes for extraction (or full split listing).
    fn load_volumes(&mut self) -> Result<(), c_int> {
        let mut path = self.arc_name.clone();
        while has_next_volume(self.archives.last().unwrap()) {
            let text = String::from_utf8_lossy(&path).into_owned();
            let Some(next) = next_volume_name(&text) else { break };
            path = next.into_bytes();
            while !path_of(&path).exists() {
                if !self.volume_callback(&mut path, RAR_VOL_ASK) {
                    return Err(ERAR_EOPEN);
                }
            }
            let archive = loop {
                match open_path(&path_of(&path), self.password.as_deref()) {
                    Ok((a, cut)) => {
                        self.cut_tail |= cut == 1;
                        break a;
                    }
                    Err(e) if e.kind() == rars::ErrorKind::PasswordRequired
                        && self.password.is_none() =>
                    {
                        if !self.ask_password() {
                            return Err(ERAR_MISSING_PASSWORD);
                        }
                    }
                    Err(e) => return Err(map_error(&e, true)),
                }
            };
            self.archives.push(archive);
            self.volume_paths.push(path.clone());
        }
        Ok(())
    }

    // Only RAR 5 can tell a wrong password from damaged data.
    fn process_error(&self, e: &rars::Error) -> c_int {
        match map_error(e, false) {
            ERAR_BAD_PASSWORD if !matches!(self.first_archive(), Archive::Rar50Plus(_)) => {
                ERAR_BAD_DATA
            }
            code => code,
        }
    }

    fn notify_volumes(&mut self, upto: usize) {
        while self.notified < upto && self.notified + 1 < self.volume_paths.len() {
            self.notified += 1;
            let mut path = self.volume_paths[self.notified].clone();
            self.volume_callback(&mut path, RAR_VOL_NOTIFY);
        }
    }

    fn finish_loading(&mut self) {
        let mut volume_of = Vec::new();
        let physical: Vec<ArchiveMember> = self
            .archives
            .iter()
            .enumerate()
            .flat_map(|(v, a)| {
                a.members().map(move |m| (v, m))
            })
            .map(|(v, m)| {
                volume_of.push(v);
                m
            })
            .collect();
        let flags: Vec<Option<u16>> = self
            .archives
            .iter()
            .flat_map(|a| match a {
                Archive::Rar15To40(x) => x
                    .blocks
                    .iter()
                    .filter_map(|b| match b {
                        rars::rar15_40::Block::File(f) => Some(Some(f.block.flags)),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                other => other.members().map(|_| None).collect(),
            })
            .collect();
        let flags = if flags.len() == physical.len() { flags } else { vec![None; physical.len()] };
        // Like libunrar, list one header per split file unless asked for every fragment.
        let keep: Vec<bool> = physical
            .iter()
            .enumerate()
            .map(|(i, m)| {
                self.open_mode == RAR_OM_LIST_INCSPLIT
                    || !m.meta.is_split_before
                    || (i == 0 && self.open_mode == RAR_OM_EXTRACT)
            })
            .collect();
        let pick = |v: &[ArchiveMember]| -> Vec<ArchiveMember> {
            v.iter().zip(&keep).filter(|(_, k)| **k).map(|(m, _)| m.clone()).collect()
        };
        self.headers = pick(&physical);
        self.header_volume =
            volume_of.iter().zip(&keep).filter(|(_, k)| **k).map(|(v, _)| *v).collect();
        let list = self.open_mode == RAR_OM_LIST;
        self.header_end_volume = (0..physical.len())
            .filter(|&i| keep[i])
            .map(|i| {
                let mut j = i;
                if self.open_mode != RAR_OM_LIST_INCSPLIT {
                    while physical[j].meta.is_split_after && j + 1 < physical.len() {
                        j += 1;
                        // Listing only steps into the volume after the first fragment.
                        if list {
                            break;
                        }
                    }
                }
                volume_of[j]
            })
            .collect();
        if !list {
            for i in 0..self.header_end_volume.len().saturating_sub(1) {
                self.header_end_volume[i] =
                    self.header_end_volume[i].max(self.header_volume[i + 1]);
            }
        }
        self.legacy_flags =
            flags.iter().zip(&keep).filter(|(_, k)| **k).map(|(f, _)| *f).collect();
        self.members = if self.open_mode == RAR_OM_EXTRACT && self.archives.len() > 1 {
            rars::volume_members(&self.archives).unwrap_or_default()
        } else {
            self.headers.clone()
        };
        if self.members.len() != self.headers.len() {
            self.members = self.headers.clone();
        }
        self.solid = self.archives.iter().any(archive_solid)
            || self.members.iter().any(|m| m.is_solid());
        let mut ord = 0;
        self.worker_index.clear();
        self.ordinal = self
            .members
            .iter()
            .enumerate()
            .map(|(i, m)| {
                if m.meta.is_redirection || m.meta.is_split_before {
                    None
                } else {
                    self.worker_index.push(i);
                    ord += 1;
                    Some(ord - 1)
                }
            })
            .collect();
        self.volumes_loaded = true;
    }

    fn ensure_loaded(&mut self) -> Result<(), c_int> {
        if self.volumes_loaded {
            return Ok(());
        }
        self.load_volumes()?;
        self.finish_loading();
        Ok(())
    }

    pub fn comment(&self) -> Result<Option<Vec<u8>>, c_int> {
        self.first_archive()
            .comment(self.password.as_deref())
            .map_err(|e| map_error(&e, false))
    }

    pub fn read_header(&mut self) -> Result<Header, c_int> {
        if let Some(code) = self.poison {
            return Err(code);
        }
        self.ensure_loaded()?;
        let Some(_) = self.headers.get(self.cursor) else {
            return Err(if self.cut_tail { ERAR_BAD_DATA } else { ERAR_END_ARCHIVE });
        };
        self.notify_volumes(self.header_volume[self.cursor]);
        let m = &self.headers[self.cursor];
        let header = make_header(m, self.legacy_flags.get(self.cursor).copied().flatten());
        self.current = Some(self.cursor);
        self.cursor += 1;
        Ok(header)
    }

    fn workable(&self) -> bool {
        self.open_mode == RAR_OM_EXTRACT
    }

    pub fn process_file(&mut self, op: c_int, dest_path: Option<&[u8]>, dest_name: Option<&[u8]>) -> c_int {
        let idx = self.current;
        let written = self.disk_written;
        let code = self.process_inner(op, dest_path, dest_name);
        // As in libunrar, a data or password failure leaves the archive unreadable.
        match code {
            ERAR_BAD_DATA | ERAR_MISSING_PASSWORD => self.poison = Some(ERAR_BAD_DATA),
            ERAR_USER_LIMIT => {
                // The next header read resumes wherever the payload read stopped.
                let stored = idx.is_some_and(|i| self.members[i].meta.is_stored);
                let rar5 = matches!(self.first_archive(), Archive::Rar50Plus(_));
                let read_payload = self.disk_written > written || (rar5 && !stored);
                self.poison = Some(if !read_payload {
                    ERAR_BAD_DATA
                } else if self.cursor < self.headers.len() {
                    ERAR_USER_LIMIT
                } else {
                    ERAR_END_ARCHIVE
                });
            }
            ERAR_BAD_PASSWORD | ERAR_UNKNOWN => self.poison = Some(code),
            _ => {}
        }
        code
    }

    fn process_inner(&mut self, op: c_int, dest_path: Option<&[u8]>, dest_name: Option<&[u8]>) -> c_int {
        let Some(idx) = self.current.take() else {
            return ERAR_UNKNOWN;
        };
        if !(RAR_SKIP..=RAR_EXTRACT).contains(&op) {
            return ERAR_UNKNOWN;
        }
        if self.workable() && op != RAR_SKIP && self.members[idx].meta.is_split_before {
            return ERAR_BAD_DATA;
        }
        self.notify_volumes(self.header_end_volume[idx]);
        if !self.workable() {
            return ERAR_SUCCESS;
        }
        let member = self.members[idx].clone();
        let dest = if op == RAR_EXTRACT {
            match dest_name {
                Some(n) => path_of(n),
                None => {
                    let Ok(rel) = rars::entry_relative_path(&member.meta.name) else {
                        return ERAR_ECREATE;
                    };
                    path_of(dest_path.unwrap_or(b".")).join(rel)
                }
            }
        } else {
            PathBuf::new()
        };

        if member.meta.is_redirection {
            return if op == RAR_EXTRACT { make_redirect(&member, &dest) } else { ERAR_SUCCESS };
        }

        if op != RAR_SKIP && member.meta.is_encrypted && self.password.is_none() {
            self.ask_password();
            if self.password.is_none() {
                return ERAR_MISSING_PASSWORD;
            }
        }

        if member.meta.is_directory {
            if op == RAR_EXTRACT {
                if std::fs::create_dir_all(&dest).is_err() {
                    return ERAR_ECREATE;
                }
                apply_metadata(&dest, &member);
            }
            return match self.sync(idx, Cmd::Skip, None) {
                Ok(()) => ERAR_SUCCESS,
                Err(c) => c,
            };
        }
        if op == RAR_SKIP {
            return match self.sync(idx, Cmd::Skip, None) {
                Ok(()) => ERAR_SUCCESS,
                Err(c) => c,
            };
        }

        let mut out = None;
        if op == RAR_EXTRACT {
            if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
                if std::fs::create_dir_all(parent).is_err() {
                    return ERAR_ECREATE;
                }
            }
            match std::fs::File::create(&dest) {
                Ok(f) => out = Some(BufWriter::with_capacity(1 << 16, f)),
                Err(_) => {
                    let _ = self.sync(idx, Cmd::Skip, None);
                    return ERAR_ECREATE;
                }
            }
        }
        let mut sink = Sink { file: out, block: Vec::new() };
        let result = self.sync(idx, Cmd::Extract, Some(&mut sink));
        let mut code = match result {
            Ok(()) => ERAR_SUCCESS,
            Err(c) => c,
        };
        if let Some(w) = sink.file.take() {
            match w.into_inner() {
                Ok(f) => {
                    drop(f);
                    if code == ERAR_SUCCESS {
                        apply_metadata(&dest, &member);
                    }
                }
                Err(_) if code == ERAR_SUCCESS => code = ERAR_EWRITE,
                Err(_) => {}
            }
            if code != ERAR_SUCCESS && self.op_flags & ROADOF_KEEPBROKEN == 0 {
                let _ = std::fs::remove_file(&dest);
            }
        }
        code
    }

    fn start_worker(&mut self) -> Result<(), c_int> {
        let stale = self
            .worker
            .as_ref()
            .is_some_and(|w| w.password != self.password);
        if stale {
            self.worker = None;
        }
        if self.worker.is_some() {
            return Ok(());
        }
        let (cmd_tx, cmd_rx) = channel();
        let (ack_tx, ack_rx) = channel();
        let (ev_tx, ev_rx) = channel();
        let archives = self.archives.clone();
        let password = self.password.clone();
        let solid = self.solid;
        let join = std::thread::Builder::new()
            .name("unrars-worker".into())
            .stack_size(16 << 20)
            .spawn(move || worker_main(archives, password, solid, ev_tx, cmd_rx, ack_rx))
            .map_err(|_| ERAR_NO_MEMORY)?;
        self.worker = Some(Worker {
            cmd_tx,
            ack_tx,
            ev_rx,
            join: Some(join),
            password: self.password.clone(),
            consumed: 0,
            pending: None,
            finished: false,
        });
        Ok(())
    }

    fn skip_is_real(&self, idx: usize) -> bool {
        self.archives.len() == 1 && (self.members[idx].meta.is_directory || !self.solid)
    }

    /// Brings the worker to `idx` (skipping earlier members) and applies `cmd` there.
    fn sync(&mut self, idx: usize, cmd: Cmd, sink: Option<&mut Sink>) -> Result<(), c_int> {
        let ord = self.ordinal[idx].unwrap();
        if cmd == Cmd::Skip && self.skip_is_real(idx) && self.worker.is_none() {
            return Ok(());
        }
        self.start_worker()?;
        while self.worker.as_ref().unwrap().consumed < ord {
            let j = self.worker_index[self.worker.as_ref().unwrap().consumed];
            self.step(j, Cmd::Skip, None)?;
        }
        if self.worker.as_ref().unwrap().consumed > ord {
            return Ok(());
        }
        self.step(idx, cmd, sink)
    }

    fn step(&mut self, idx: usize, cmd: Cmd, mut sink: Option<&mut Sink>) -> Result<(), c_int> {
        let real_skip = self.skip_is_real(idx);
        let w = self.worker.as_mut().unwrap();
        let first = match w.pending.take() {
            Some(e) => e,
            None if w.finished => Event::Done(Ok(())),
            None => w.ev_rx.recv().unwrap_or(Event::Done(Err(rars::Error::Cancelled))),
        };
        match first {
            Event::Member => {}
            Event::Done(r) => {
                w.finished = true;
                return Err(match r {
                    Err(e) => self.process_error(&e),
                    Ok(()) => ERAR_BAD_DATA,
                });
            }
            Event::Failed(e) => return Err(self.process_error(&e)),
            Event::Chunk(_) => unreachable!(),
        }
        if w.cmd_tx.send(cmd).is_err() {
            return Err(ERAR_UNKNOWN);
        }
        w.consumed += 1;
        if cmd == Cmd::Skip && real_skip {
            return Ok(());
        }
        self.abort_code = None;
        loop {
            let w = self.worker.as_mut().unwrap();
            let ev = w.ev_rx.recv().unwrap_or(Event::Done(Err(rars::Error::Cancelled)));
            match ev {
                Event::Chunk(data) => {
                    let mut ok = true;
                    let limited = self.max_disk_write > 0
                        && sink.as_deref().is_some_and(|s| s.file.is_some());
                    if limited {
                        let s = sink.as_deref_mut().unwrap();
                        s.block.extend_from_slice(&data);
                        while ok && s.block.len() >= DISK_BLOCK {
                            let block: Vec<u8> = s.block.drain(..DISK_BLOCK).collect();
                            ok = self.admit_block(s, &block);
                        }
                    } else {
                        if let Some(f) = sink.as_deref_mut().and_then(|s| s.file.as_mut()) {
                            if f.write_all(&data).is_err() {
                                self.abort_code = Some(ERAR_EWRITE);
                                ok = false;
                            }
                        }
                        if ok && !self.report_data(&data) {
                            self.abort_code = Some(ERAR_UNKNOWN);
                            ok = false;
                        }
                    }
                    let _ = self.worker.as_ref().unwrap().ack_tx.send(ok);
                }
                Event::Member => {
                    self.worker.as_mut().unwrap().pending = Some(Event::Member);
                    return self.flush_block(sink);
                }
                Event::Failed(e) => return Err(self.abort_code.unwrap_or_else(|| self.process_error(&e))),
                Event::Done(r) => {
                    let w = self.worker.as_mut().unwrap();
                    w.finished = true;
                    r.map_err(|e| self.abort_code.unwrap_or_else(|| self.process_error(&e)))?;
                    return self.flush_block(sink);
                }
            }
        }
    }

    /// Writes one block to disk unless that would exceed the disk limit.
    fn admit_block(&mut self, s: &mut Sink, block: &[u8]) -> bool {
        if self.disk_written + block.len() as u64 > self.max_disk_write {
            self.abort_code = Some(ERAR_USER_LIMIT);
            return false;
        }
        if let Some(f) = s.file.as_mut() {
            if f.write_all(block).is_err() {
                self.abort_code = Some(ERAR_EWRITE);
                return false;
            }
        }
        self.disk_written += block.len() as u64;
        if !self.report_data(block) {
            self.abort_code = Some(ERAR_UNKNOWN);
            return false;
        }
        true
    }

    fn flush_block(&mut self, sink: Option<&mut Sink>) -> Result<(), c_int> {
        let Some(s) = sink.filter(|s| !s.block.is_empty()) else {
            return Ok(());
        };
        let block = std::mem::take(&mut s.block);
        if self.admit_block(s, &block) {
            Ok(())
        } else {
            Err(self.abort_code.unwrap_or(ERAR_UNKNOWN))
        }
    }

    fn report_data(&self, data: &[u8]) -> bool {
        unsafe {
            if let Some(cb) = self.callback {
                cb(UCM_PROCESSDATA, self.user_data, data.as_ptr() as LPARAM, data.len() as LPARAM)
                    != -1
            } else if let Some(p) = self.process_data {
                p(data.as_ptr() as *mut u8, data.len() as c_int) != 0
            } else {
                true
            }
        }
    }
}

struct Sink {
    file: Option<BufWriter<std::fs::File>>,
    block: Vec<u8>,
}

// Writes are admitted against the disk limit in blocks of this size.
const DISK_BLOCK: usize = 4 << 20;

fn apply_metadata(path: &Path, m: &ArchiveMember) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if m.meta.attr_source() == rars::AttrSource::Unix {
            let mode = (m.meta.file_attr & 0o7777) as u32;
            if mode != 0 {
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
            }
        }
    }
    if let Some(t) = m.meta.modification_time() {
        if let Ok(f) = std::fs::File::open(path) {
            let _ = f.set_modified(t);
        }
    }
}

#[cfg(unix)]
fn symlink(target: &Path, dest: &Path, _dir: bool) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, dest)
}

#[cfg(windows)]
fn symlink(target: &Path, dest: &Path, dir: bool) -> std::io::Result<()> {
    if dir {
        std::os::windows::fs::symlink_dir(target, dest)
    } else {
        std::os::windows::fs::symlink_file(target, dest)
    }
}

fn make_redirect(m: &ArchiveMember, dest: &Path) -> c_int {
    let ArchiveMemberDetail::Rar50Plus { redirection: Some(r), .. } = &m.detail else {
        return ERAR_SUCCESS;
    };
    if r.redirection_type != 1 && r.redirection_type != 2 {
        return ERAR_SUCCESS;
    }
    let target = path_of(&r.target_name);
    let unsafe_target = target.is_absolute()
        || target.components().any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)));
    if unsafe_target {
        return ERAR_EREFERENCE;
    }
    if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
        if std::fs::create_dir_all(parent).is_err() {
            return ERAR_ECREATE;
        }
    }
    let _ = std::fs::remove_file(dest);
    match symlink(&target, dest, m.meta.is_directory) {
        Ok(()) => ERAR_SUCCESS,
        Err(_) => ERAR_ECREATE,
    }
}

fn make_header(m: &ArchiveMember, legacy_flags: Option<u16>) -> Header {
    let meta = &m.meta;
    let mut flags = 0;
    if meta.is_split_before {
        flags |= RHDF_SPLITBEFORE;
    }
    if meta.is_split_after {
        flags |= RHDF_SPLITAFTER;
    }
    if meta.is_encrypted {
        flags |= RHDF_ENCRYPTED;
    }
    if m.is_solid() {
        flags |= RHDF_SOLID;
    }
    if meta.is_directory {
        flags |= RHDF_DIRECTORY;
    }
    let mut name = meta.name.clone();
    if meta.family != ArchiveFamily::Rar50Plus {
        for b in name.iter_mut().filter(|b| **b == b'\\') {
            *b = b'/';
        }
    }
    let mut h = Header {
        name,
        flags,
        pack_size: meta.packed_size,
        unp_size: meta.unpacked_size,
        host_os: 0,
        crc: 0,
        file_time: 0,
        unp_ver: 0,
        method: 0,
        attr: meta.file_attr as u32,
        dict_kb: 0,
        hash_type: RAR_HASH_NONE,
        hash: [0; 32],
        redir_type: 0,
        redir_name: Vec::new(),
        dir_target: 0,
        mtime: 0,
        ctime: 0,
        atime: 0,
    };
    match &m.detail {
        ArchiveMemberDetail::Rar13 { method, unpack_version, file_checksum, .. } => {
            let _ = unpack_version;
            h.host_os = 2;
            h.method = 0x30 + u32::from(*method);
            h.unp_ver = 13;
            h.dict_kb = 64;
            h.crc = u32::from(*file_checksum);
            h.hash_type = RAR_HASH_CRC32;
            h.hash[..4].copy_from_slice(&h.crc.to_le_bytes());
            h.file_time = meta.file_time.unwrap_or(0);
        }
        ArchiveMemberDetail::Rar15To40 { method, unpack_version, crc32, .. } => {
            h.host_os = match meta.host_os.unwrap_or(0) {
                0 => 2,
                n => n as u32,
            };
            h.method = u32::from(*method);
            h.unp_ver = u32::from(*unpack_version);
            h.crc = *crc32;
            h.hash_type = RAR_HASH_CRC32;
            h.hash[..4].copy_from_slice(&crc32.to_le_bytes());
            h.file_time = meta.file_time.unwrap_or(0);
            if let Some(f) = legacy_flags.map(|f| (f >> 5) & 7).filter(|&w| w != 7) {
                h.dict_kb = 64 << f;
            }
        }
        ArchiveMemberDetail::Rar50Plus { compression_info: ci, crc32, file_times, redirection, hash, .. } => {
            h.host_os = match meta.host_os {
                Some(0) => 2,
                Some(1) => 3,
                _ => 3,
            };
            h.method = 0x30 + ((*ci >> 7) & 7) as u32;
            h.unp_ver = match ci & 0x3f {
                0 => 50,
                v => 50 + 20 * v as u32,
            };
            if !meta.is_directory {
                let base = 128u64 << ((ci >> 10) & 0xf);
                let size = base + base / 32 * ((ci >> 15) & 0x1f);
                h.dict_kb = size as u32;
            }
            h.crc = crc32.unwrap_or(0);
            match hash {
                Some(ArchiveMemberHash::Blake2sp(d)) => {
                    h.hash_type = RAR_HASH_BLAKE2;
                    h.hash = *d;
                    h.crc = u32::from_le_bytes([d[0], d[1], d[2], d[3]]);
                }
                _ => {
                    if let Some(c) = crc32 {
                        h.hash_type = RAR_HASH_CRC32;
                        h.hash[..4].copy_from_slice(&c.to_le_bytes());
                    }
                }
            }
            h.file_time = meta.file_time.map_or(0, |t| dos_from_unix(i64::from(t)));
            if let Some(r) = redirection {
                h.redir_type = r.redirection_type as u32;
                h.redir_name = r.target_name.clone();
                h.dir_target = (r.flags & 1) as u32;
            }
            if let Some(t) = file_times {
                let ft = |x: &Option<FileTimestamp>| {
                    x.map_or(0, |x| filetime_from_unix_nanos(x.unix_nanoseconds()))
                };
                h.ctime = ft(&t.created);
                h.atime = ft(&t.accessed);
            }
        }
        _ => {}
    }
    if let Some(t) = meta.modification_time() {
        if let Ok(d) = t.duration_since(std::time::UNIX_EPOCH) {
            h.mtime = filetime_from_unix_nanos(d.as_nanos() as i128);
        }
    }
    h
}
