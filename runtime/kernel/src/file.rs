//! File system: guest object paths → host files.
//!
//! Devices map to host directories: `\Device\Cdrom0` is the extracted disc
//! (read-only), `\Device\Harddisk0\Partition1` and the cache partition are
//! writable folders. Drive names (`game:`, `d:`, `cache:`, content roots) are
//! symbolic links to device paths, created by the kernel at boot or by the game
//! through `ObCreateSymbolicLink`.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::mem;
use crate::object;
use crate::status::*;

struct Device {
    /// Lower-case object path, e.g. `\device\cdrom0`.
    name: String,
    host: PathBuf,
    read_only: bool,
    label: &'static str,
}

#[derive(Default)]
struct Vfs {
    devices: Vec<Device>,
    /// Lower-case link name (`game:`) → target object path.
    links: HashMap<String, String>,
}

static VFS: Mutex<Option<Vfs>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut Vfs) -> R) -> R {
    f(VFS.lock().unwrap().get_or_insert_with(Vfs::default))
}

pub fn add_device(name: &str, host: PathBuf, read_only: bool, label: &'static str) {
    if !read_only {
        let _ = fs::create_dir_all(&host);
    }
    let name = name.to_ascii_lowercase();
    with(|v| {
        v.devices.retain(|d| d.name != name);
        v.devices.push(Device { name, host, read_only, label })
    });
}

/// Removes a device (a content package being closed).
pub fn remove_device(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    with(|v| {
        let n = v.devices.len();
        v.devices.retain(|d| d.name != name);
        v.devices.len() != n
    })
}

pub fn create_link(link: &str, target: &str) {
    with(|v| v.links.insert(normalize_link(link), target.to_string()));
}

pub fn delete_link(link: &str) -> bool {
    with(|v| v.links.remove(&normalize_link(link)).is_some())
}

fn normalize_link(link: &str) -> String {
    let l = link.trim_start_matches("\\??\\").trim_start_matches("\\DosDevices\\");
    l.trim_end_matches('\\').to_ascii_lowercase()
}

/// A resolved guest path.
#[derive(Clone, Debug)]
pub struct Resolved {
    pub host: PathBuf,
    pub read_only: bool,
    /// The device's host root, for volume queries.
    pub label: &'static str,
}

/// The mods folder: `mods\` beside the disc folder (`private\mods` in the
/// project, `mods\` in a release folder). A file there at the same path as one
/// on the disc (`mods\pac\ch\ch139.pac`) is read in its place: data
/// conversion at the file boundary, for mods. Missing folder:
/// no mods.
static MODS: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();

pub fn set_mods(dir: PathBuf) {
    let found = dir.is_dir().then_some(dir);
    if let Some(d) = &found {
        crate::diag::log(&format!("mods: files in {} replace the disc's", d.display()));
    }
    let _ = MODS.set(found);
}

/// Resolves a guest object path (`\??\game:\a\b.pac`, `game:\a`,
/// `\Device\Cdrom0\a`) to a host path. `..` never climbs above the device.
pub fn resolve(path: &str) -> Option<Resolved> {
    with(|v| {
        let mut p = path.replace('/', "\\");
        // Symbolic links, possibly chained.
        for _ in 0..8 {
            let stripped = p.trim_start_matches("\\??\\").trim_start_matches("\\DosDevices\\").to_string();
            let (first, rest) = match stripped.find('\\') {
                Some(i) => (&stripped[..i], &stripped[i..]),
                None => (stripped.as_str(), ""),
            };
            let key = first.to_ascii_lowercase();
            if let Some(target) = v.links.get(&key) {
                p = format!("{}{}", target.trim_end_matches('\\'), rest);
                continue;
            }
            p = stripped.clone();
            break;
        }
        let lower = p.to_ascii_lowercase();
        let dev = v
            .devices
            .iter()
            .filter(|d| lower == d.name || lower.starts_with(&format!("{}\\", d.name)))
            .max_by_key(|d| d.name.len())?;
        let rest = &p[dev.name.len()..];
        let mut host = dev.host.clone();
        let mut parts: Vec<&str> = Vec::new();
        for part in rest.split('\\') {
            match part {
                "" | "." => {}
                ".." => {
                    if parts.pop().is_some() {
                        host.pop();
                    }
                }
                _ => {
                    host.push(part);
                    parts.push(part);
                }
            }
        }
        // A file in the mods folder replaces the disc's file at the same path.
        if dev.read_only && !parts.is_empty() {
            if let Some(Some(mods)) = MODS.get() {
                let alt = parts.iter().fold(mods.clone(), |p, s| p.join(s));
                if alt.is_file() {
                    host = alt;
                }
            }
        }
        Some(Resolved { host, read_only: dev.read_only, label: dev.label })
    })
}

/// An open file or directory.
pub struct File {
    pub guest_path: String,
    pub host: PathBuf,
    pub is_dir: bool,
    pub read_only: bool,
    pub label: &'static str,
    /// Signalled when an I/O on this handle completes (file handles are waitable).
    pub event: u32,
    pub state: Mutex<FileState>,
    /// Bytes served over the file's own (`kernel::entrance`: the sound
    /// packages with the custom entrance song's additions).
    pub overlay: Option<std::sync::Arc<crate::entrance::Overlay>>,
}

pub struct FileState {
    pub file: Option<fs::File>,
    pub position: u64,
    /// Directory enumeration: remaining entries.
    pub listing: Option<Vec<DirEntry>>,
    pub delete_on_close: bool,
}

#[derive(Clone)]
pub struct DirEntry {
    pub name: String,
    pub info: Info,
}

#[derive(Clone, Copy, Default)]
pub struct Info {
    pub size: u64,
    pub is_dir: bool,
    pub read_only: bool,
    pub created: u64,
    pub accessed: u64,
    pub written: u64,
}

pub const FILE_ATTRIBUTE_READONLY: u32 = 0x01;
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

impl Info {
    pub fn attributes(&self) -> u32 {
        let mut a = if self.is_dir { FILE_ATTRIBUTE_DIRECTORY } else { 0 };
        if self.read_only {
            a |= FILE_ATTRIBUTE_READONLY;
        }
        if a == 0 { FILE_ATTRIBUTE_NORMAL } else { a }
    }

    pub fn allocation(&self) -> u64 {
        self.size.next_multiple_of(0x1000)
    }

    /// `FILE_NETWORK_OPEN_INFORMATION` (0x38 bytes).
    pub fn write_network_open(&self, a: u32) {
        mem::w64(a, self.created);
        mem::w64(a + 8, self.accessed);
        mem::w64(a + 0x10, self.written);
        mem::w64(a + 0x18, self.written);
        mem::w64(a + 0x20, self.allocation());
        mem::w64(a + 0x28, self.size);
        mem::w32(a + 0x30, self.attributes());
        mem::w32(a + 0x34, 0);
    }
}

fn filetime(t: std::io::Result<std::time::SystemTime>) -> u64 {
    t.ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| 116_444_736_000_000_000 + (d.as_nanos() / 100) as u64)
}

pub fn info_of(meta: &fs::Metadata, read_only_device: bool) -> Info {
    Info {
        size: if meta.is_dir() { 0 } else { meta.len() },
        is_dir: meta.is_dir(),
        read_only: read_only_device || meta.permissions().readonly(),
        created: filetime(meta.created()),
        accessed: filetime(meta.accessed()),
        written: filetime(meta.modified()),
    }
}

pub const FILE_SUPERSEDE: u32 = 0;
pub const FILE_OPEN: u32 = 1;
pub const FILE_CREATE: u32 = 2;
pub const FILE_OPEN_IF: u32 = 3;
pub const FILE_OVERWRITE: u32 = 4;
pub const FILE_OVERWRITE_IF: u32 = 5;

pub const FILE_DIRECTORY_FILE: u32 = 0x01;
pub const FILE_NON_DIRECTORY_FILE: u32 = 0x40;
pub const FILE_DELETE_ON_CLOSE: u32 = 0x1000;

/// Access bits that mean the caller will modify the file.
const WRITE_ACCESS: u32 = 0x4000_0000 | 0x1000_0000 | 0x0001_0000 | 0x2 | 0x4 | 0x10 | 0x100;

/// `IO_STATUS_BLOCK.Information` values for create.
pub const FILE_SUPERSEDED: u32 = 0;
pub const FILE_OPENED: u32 = 1;
pub const FILE_CREATED: u32 = 2;
pub const FILE_OVERWRITTEN: u32 = 3;
pub const FILE_DOES_NOT_EXIST: u32 = 5;

/// `NtCreateFile` semantics. Returns `(file, information)` or an NTSTATUS.
pub fn open(guest_path: &str, access: u32, disposition: u32, options: u32) -> Result<(File, u32), u32> {
    let r = resolve(guest_path).ok_or(STATUS_OBJECT_PATH_NOT_FOUND)?;
    let wants_write = access & WRITE_ACCESS != 0 || matches!(disposition, FILE_SUPERSEDE | FILE_OVERWRITE | FILE_OVERWRITE_IF)
        || options & FILE_DELETE_ON_CLOSE != 0;
    let meta = fs::metadata(&r.host).ok();
    let exists = meta.is_some();
    if r.read_only && (wants_write || (!exists && matches!(disposition, FILE_CREATE | FILE_OPEN_IF))) {
        return if exists || wants_write { Err(STATUS_MEDIA_WRITE_PROTECTED) } else { Err(STATUS_OBJECT_NAME_NOT_FOUND) };
    }
    let parent_exists = r.host.parent().is_some_and(Path::is_dir);
    let mut info = FILE_OPENED;
    match disposition {
        FILE_OPEN | FILE_OVERWRITE if !exists => {
            return Err(if parent_exists { STATUS_OBJECT_NAME_NOT_FOUND } else { STATUS_OBJECT_PATH_NOT_FOUND });
        }
        FILE_CREATE if exists => return Err(STATUS_OBJECT_NAME_COLLISION),
        _ => {}
    }
    let is_dir = meta.as_ref().is_some_and(|m| m.is_dir()) || (!exists && options & FILE_DIRECTORY_FILE != 0);
    if exists && is_dir && options & FILE_NON_DIRECTORY_FILE != 0 {
        return Err(STATUS_FILE_IS_A_DIRECTORY);
    }
    if exists && !is_dir && options & FILE_DIRECTORY_FILE != 0 {
        return Err(STATUS_NOT_A_DIRECTORY);
    }
    if !exists && !parent_exists {
        return Err(STATUS_OBJECT_PATH_NOT_FOUND);
    }
    let file = if is_dir {
        if !exists {
            fs::create_dir(&r.host).map_err(|_| STATUS_ACCESS_DENIED)?;
            info = FILE_CREATED;
        }
        None
    } else {
        let mut o = fs::OpenOptions::new();
        o.read(true);
        if !r.read_only {
            o.write(true);
        }
        match disposition {
            FILE_SUPERSEDE | FILE_OVERWRITE_IF if !exists => {
                o.create(true);
                info = FILE_CREATED;
            }
            FILE_SUPERSEDE | FILE_OVERWRITE | FILE_OVERWRITE_IF => {
                o.truncate(true);
                info = if disposition == FILE_SUPERSEDE { FILE_SUPERSEDED } else { FILE_OVERWRITTEN };
            }
            FILE_CREATE | FILE_OPEN_IF if !exists => {
                o.create(true);
                info = FILE_CREATED;
            }
            _ => {}
        }
        Some(o.open(&r.host).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => STATUS_OBJECT_NAME_NOT_FOUND,
            std::io::ErrorKind::PermissionDenied => STATUS_ACCESS_DENIED,
            _ => STATUS_UNSUCCESSFUL,
        })?)
    };
    let event = object::new_event(true, false);
    let overlay = if !is_dir && r.read_only { crate::entrance::overlay_for(&r.host) } else { None };
    Ok((
        File {
            overlay,
            guest_path: guest_path.to_string(),
            host: r.host,
            is_dir,
            read_only: r.read_only,
            label: r.label,
            event,
            state: Mutex::new(FileState {
                file,
                position: 0,
                listing: None,
                delete_on_close: options & FILE_DELETE_ON_CLOSE != 0,
            }),
        },
        info,
    ))
}

impl File {
    pub fn info(&self) -> Info {
        let mut info = fs::metadata(&self.host).map(|m| info_of(&m, self.read_only)).unwrap_or_default();
        if let Some(o) = &self.overlay {
            info.size = o.size(info.size);
        }
        info
    }

    /// Reads at `offset` (or the current position) into guest memory.
    pub fn read(&self, buf: u32, len: u32, offset: Option<u64>) -> Result<u32, u32> {
        let mut s = self.state.lock().unwrap();
        let pos = offset.unwrap_or(s.position);
        let f = s.file.as_mut().ok_or(STATUS_INVALID_DEVICE_REQUEST)?;
        let real = f.metadata().map(|m| m.len()).unwrap_or(0);
        let size = self.overlay.as_ref().map_or(real, |o| o.size(real));
        if len > 0 && pos >= size {
            return Err(STATUS_END_OF_FILE);
        }
        let n = (len as u64).min(size - pos) as usize;
        let dst = unsafe { mem::slice_mut(buf, n) };
        // The file's own bytes (none past its end, inside an overlay's addition).
        let own = if pos < real { ((real - pos) as usize).min(n) } else { 0 };
        let mut done = 0;
        if own > 0 {
            f.seek(SeekFrom::Start(pos)).map_err(|_| STATUS_UNSUCCESSFUL)?;
        }
        while done < own {
            match f.read(&mut dst[done..own]) {
                Ok(0) => break,
                Ok(k) => done += k,
                Err(_) => return Err(STATUS_UNSUCCESSFUL),
            }
        }
        if let Some(o) = &self.overlay {
            dst[done..].fill(0);
            o.apply(pos, dst);
            done = n;
        }
        s.position = pos + done as u64;
        Ok(done as u32)
    }

    pub fn write(&self, buf: u32, len: u32, offset: Option<u64>) -> Result<u32, u32> {
        if self.read_only {
            return Err(STATUS_MEDIA_WRITE_PROTECTED);
        }
        let mut s = self.state.lock().unwrap();
        let pos = offset.unwrap_or(s.position);
        let f = s.file.as_mut().ok_or(STATUS_INVALID_DEVICE_REQUEST)?;
        f.seek(SeekFrom::Start(pos)).map_err(|_| STATUS_UNSUCCESSFUL)?;
        let src = mem::read(buf, len as usize);
        f.write_all(&src).map_err(|_| STATUS_DISK_FULL)?;
        s.position = pos + len as u64;
        Ok(len)
    }

    pub fn set_size(&self, size: u64) -> Result<(), u32> {
        if self.read_only {
            return Err(STATUS_MEDIA_WRITE_PROTECTED);
        }
        let s = self.state.lock().unwrap();
        s.file.as_ref().ok_or(STATUS_INVALID_PARAMETER)?.set_len(size).map_err(|_| STATUS_DISK_FULL)
    }

    /// Next directory entry matching `pattern` (`*`, `?` wildcards; case-insensitive).
    pub fn next_entry(&self, pattern: Option<&str>, restart: bool) -> Option<DirEntry> {
        let mut s = self.state.lock().unwrap();
        if restart || s.listing.is_none() {
            let mut entries: Vec<DirEntry> = fs::read_dir(&self.host)
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .filter_map(|e| {
                            let name = e.file_name().to_string_lossy().into_owned();
                            let meta = e.metadata().ok()?;
                            Some(DirEntry { name, info: info_of(&meta, self.read_only) })
                        })
                        .collect()
                })
                .unwrap_or_default();
            entries.sort_by(|a, b| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()));
            let pat = pattern.unwrap_or("*").to_ascii_lowercase();
            entries.retain(|e| wildcard(&pat, &e.name.to_ascii_lowercase()));
            entries.reverse();
            s.listing = Some(entries);
        }
        s.listing.as_mut()?.pop()
    }

    pub fn close(&self) {
        let mut s = self.state.lock().unwrap();
        s.file = None;
        if s.delete_on_close && !self.read_only {
            let _ = if self.is_dir { fs::remove_dir(&self.host) } else { fs::remove_file(&self.host) };
        }
    }
}

/// DOS-style wildcard match (`*`, `?`; `*.*` matches names without a dot too).
pub fn wildcard(pat: &str, name: &str) -> bool {
    if pat == "*" || pat == "*.*" {
        return true;
    }
    let p: Vec<char> = pat.chars().collect();
    let n: Vec<char> = name.chars().collect();
    fn go(p: &[char], n: &[char]) -> bool {
        match (p.first(), n.first()) {
            (None, None) => true,
            (Some('*'), _) => go(&p[1..], n) || (!n.is_empty() && go(p, &n[1..])),
            (Some('?'), Some(_)) => go(&p[1..], &n[1..]),
            (Some(a), Some(b)) => a == b && go(&p[1..], &n[1..]),
            _ => false,
        }
    }
    go(&p, &n)
}

/// `NtQueryFullAttributesFile` / existence checks without opening.
pub fn stat(guest_path: &str) -> Result<Info, u32> {
    let r = resolve(guest_path).ok_or(STATUS_OBJECT_PATH_NOT_FOUND)?;
    match fs::metadata(&r.host) {
        Ok(m) => Ok(info_of(&m, r.read_only)),
        Err(_) => Err(if r.host.parent().is_some_and(Path::is_dir) {
            STATUS_OBJECT_NAME_NOT_FOUND
        } else {
            STATUS_OBJECT_PATH_NOT_FOUND
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcards() {
        assert!(wildcard("*.pac", "chr.pac"));
        assert!(!wildcard("*.pac", "chr.pak"));
        assert!(wildcard("a?c", "abc"));
        assert!(wildcard("*", "anything"));
        assert!(wildcard("*.*", "noext"));
    }

    #[test]
    fn paths_resolve_through_links() {
        let root = std::env::temp_dir().join("wwe14-vfs-test");
        add_device("\\Device\\Cdrom0", root.clone(), true, "DVD");
        create_link("game:", "\\Device\\Cdrom0");
        let r = resolve("\\??\\GAME:\\data\\..\\..\\x.pac").unwrap();
        assert_eq!(r.host, root.join("x.pac"));
        assert!(r.read_only);
        let r = resolve("game:\\a\\b").unwrap();
        assert_eq!(r.host, root.join("a").join("b"));
        assert!(resolve("\\Device\\Nope\\x").is_none());
    }
}
