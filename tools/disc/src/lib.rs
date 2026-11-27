//! Read-only access to Xbox 360 disc images in XDVDFS format.
//!
//! Supports full XGD1/XGD2/XGD3 images and game-partition-only images. The
//! volume descriptor sits at sector 32 of the game partition. It starts with
//! `MICROSOFT*XBOX*MEDIA`, followed by the root directory's sector and size.
//!
//! Each directory is a binary tree of entries stored in its own sectors:
//!
//! | offset | field                                                  |
//! |--------|--------------------------------------------------------|
//! | 0      | `u16` left subtree offset, in 4-byte units (0 = none)  |
//! | 2      | `u16` right subtree offset, in 4-byte units (0 = none) |
//! | 4      | `u32` first sector                                     |
//! | 8      | `u32` size in bytes                                    |
//! | 12     | `u8` attributes (`0x10` = directory)                   |
//! | 13     | `u8` name length, followed by the name bytes           |
//!
//! All integers are little-endian. Entries are padded to 4 bytes, and unused
//! sector tails are filled with `0xFF`.
//!
//! The image is untrusted input. Names are validated before anything is written
//! to the host file system, so a crafted image can't escape the output directory.

use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const SECTOR_SIZE: u64 = 2048;
const VOLUME_DESCRIPTOR_SECTOR: u64 = 32;
const MAGIC: &[u8; 20] = b"MICROSOFT*XBOX*MEDIA";
const ATTR_DIRECTORY: u8 = 0x10;
const ENTRY_HEADER_LEN: usize = 14;
const MAX_DEPTH: usize = 64;
const COPY_BUFFER: usize = 4 << 20;

/// Where the game partition starts inside the image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Xgd3,
    Xgd2,
    Xgd1,
    /// The image is the game partition itself (for example, produced by extract-xiso).
    PartitionOnly,
}

impl Layout {
    pub const ALL: [Layout; 4] = [Layout::Xgd3, Layout::Xgd2, Layout::Xgd1, Layout::PartitionOnly];

    pub fn partition_offset(self) -> u64 {
        match self {
            Layout::Xgd3 => 0x0208_0000,
            Layout::Xgd2 => 0x0FD9_0000,
            Layout::Xgd1 => 0x1830_0000,
            Layout::PartitionOnly => 0,
        }
    }
}

impl fmt::Display for Layout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Layout::Xgd3 => "XGD3",
            Layout::Xgd2 => "XGD2",
            Layout::Xgd1 => "XGD1",
            Layout::PartitionOnly => "game partition only",
        })
    }
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// No XDVDFS volume descriptor at any known partition offset.
    NotXdvdfs,
    /// The file system structures are inconsistent or unsafe to use.
    Corrupt(String),
    /// A file operation was requested on a directory entry.
    NotAFile(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::NotXdvdfs => f.write_str("not an Xbox 360 disc image (no XDVDFS volume descriptor found)"),
            Error::Corrupt(s) => write!(f, "corrupt disc image: {s}"),
            Error::NotAFile(p) => write!(f, "{p} is a directory, not a file"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// One file or directory on the disc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Path relative to the disc root, separated by `/`, e.g. `pac/bg/bg00.pac`.
    pub path: String,
    pub sector: u32,
    pub size: u32,
    pub is_dir: bool,
}

impl Entry {
    /// True for the console system-update folder, which isn't part of the game.
    pub fn is_system_update(&self) -> bool {
        self.path
            .split('/')
            .next()
            .is_some_and(|first| first.eq_ignore_ascii_case("$SystemUpdate"))
    }

    /// Where this entry lands when extracted under `root`.
    pub fn host_path(&self, root: &Path) -> PathBuf {
        let mut p = root.to_path_buf();
        p.extend(self.path.split('/'));
        p
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ExtractOptions {
    /// Also extract `$SystemUpdate` (console firmware, not needed for the game).
    pub include_system_update: bool,
    /// Rewrite files that already exist with the right size.
    pub overwrite: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ExtractReport {
    pub files_written: u64,
    pub files_skipped: u64,
    pub bytes_written: u64,
}

pub struct Disc<R> {
    reader: R,
    layout: Layout,
    len: u64,
    root_sector: u32,
    root_size: u32,
}

impl Disc<File> {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::new(File::open(path)?)
    }
}

impl<R: Read + Seek> Disc<R> {
    pub fn new(mut reader: R) -> Result<Self> {
        let len = reader.seek(SeekFrom::End(0))?;
        for layout in Layout::ALL {
            let vd = layout.partition_offset() + VOLUME_DESCRIPTOR_SECTOR * SECTOR_SIZE;
            if vd + SECTOR_SIZE > len {
                continue;
            }
            let mut buf = [0u8; 28];
            reader.seek(SeekFrom::Start(vd))?;
            reader.read_exact(&mut buf)?;
            if &buf[..20] == MAGIC {
                return Ok(Disc {
                    reader,
                    layout,
                    len,
                    root_sector: u32::from_le_bytes(buf[20..24].try_into().unwrap()),
                    root_size: u32::from_le_bytes(buf[24..28].try_into().unwrap()),
                });
            }
        }
        Err(Error::NotXdvdfs)
    }

    pub fn layout(&self) -> Layout {
        self.layout
    }

    pub fn image_len(&self) -> u64 {
        self.len
    }

    /// Every file and directory on the disc, sorted by path (ASCII case-insensitive),
    /// so parents always come before their children.
    pub fn entries(&mut self) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        let mut visited = HashSet::new();
        let (sector, size) = (self.root_sector, self.root_size);
        self.walk_dir(sector, size, "", 0, &mut visited, &mut out)?;
        out.sort_by_cached_key(|e| e.path.to_ascii_lowercase());
        Ok(out)
    }

    pub fn read_file(&mut self, entry: &Entry) -> Result<Vec<u8>> {
        if entry.is_dir {
            return Err(Error::NotAFile(entry.path.clone()));
        }
        self.read_extent(entry.sector, entry.size, &entry.path)
    }

    /// Streams one file's bytes into `w`.
    pub fn copy_file<W: Write>(&mut self, entry: &Entry, w: &mut W) -> Result<u64> {
        if entry.is_dir {
            return Err(Error::NotAFile(entry.path.clone()));
        }
        let offset = self.check_extent(entry.sector, entry.size, &entry.path)?;
        self.reader.seek(SeekFrom::Start(offset))?;
        let mut remaining = entry.size as usize;
        let mut buf = vec![0u8; COPY_BUFFER.min(remaining).max(1)];
        while remaining > 0 {
            let n = buf.len().min(remaining);
            self.reader.read_exact(&mut buf[..n])?;
            w.write_all(&buf[..n])?;
            remaining -= n;
        }
        Ok(entry.size as u64)
    }

    /// Extracts `entries` under `out_dir`. Each file is written to `<name>.part` and
    /// renamed into place when complete, so an interrupted run never leaves a
    /// truncated file under its real name. Files that already exist with the
    /// expected size are skipped unless `opts.overwrite` is set.
    pub fn extract_all(
        &mut self,
        entries: &[Entry],
        out_dir: &Path,
        opts: ExtractOptions,
        mut progress: impl FnMut(&Entry, &ExtractReport),
    ) -> Result<ExtractReport> {
        let mut report = ExtractReport::default();
        fs::create_dir_all(out_dir)?;
        for entry in entries {
            if entry.is_system_update() && !opts.include_system_update {
                continue;
            }
            let dest = entry.host_path(out_dir);
            if entry.is_dir {
                fs::create_dir_all(&dest)?;
                continue;
            }
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            if !opts.overwrite
                && fs::metadata(&dest).is_ok_and(|m| m.is_file() && m.len() == entry.size as u64)
            {
                report.files_skipped += 1;
                progress(entry, &report);
                continue;
            }
            let mut part = dest.clone().into_os_string();
            part.push(".part");
            let part = PathBuf::from(part);
            {
                let mut file = File::create(&part)?;
                self.copy_file(entry, &mut file)?;
                file.flush()?;
            }
            fs::rename(&part, &dest)?;
            report.files_written += 1;
            report.bytes_written += entry.size as u64;
            progress(entry, &report);
        }
        Ok(report)
    }

    fn check_extent(&self, sector: u32, size: u32, what: &str) -> Result<u64> {
        let offset = self.layout.partition_offset() + sector as u64 * SECTOR_SIZE;
        if offset + size as u64 > self.len {
            return Err(Error::Corrupt(format!(
                "{what}: sector {sector} + {size} bytes runs past the end of the image"
            )));
        }
        Ok(offset)
    }

    fn read_extent(&mut self, sector: u32, size: u32, what: &str) -> Result<Vec<u8>> {
        let offset = self.check_extent(sector, size, what)?;
        let mut buf = vec![0u8; size as usize];
        self.reader.seek(SeekFrom::Start(offset))?;
        self.reader.read_exact(&mut buf)?;
        Ok(buf)
    }

    fn walk_dir(
        &mut self,
        sector: u32,
        size: u32,
        prefix: &str,
        depth: usize,
        visited: &mut HashSet<u32>,
        out: &mut Vec<Entry>,
    ) -> Result<()> {
        if size == 0 {
            return Ok(());
        }
        let where_ = if prefix.is_empty() { "<root>" } else { prefix };
        if depth > MAX_DEPTH {
            return Err(Error::Corrupt(format!("directory nesting deeper than {MAX_DEPTH} at {where_}")));
        }
        if !visited.insert(sector) {
            return Err(Error::Corrupt(format!("directory sector {sector} reached twice (cycle) at {where_}")));
        }
        let data = self.read_extent(sector, size, where_)?;

        let mut subdirs = Vec::new();
        let mut stack = vec![0usize];
        let mut seen = HashSet::new();
        while let Some(off) = stack.pop() {
            if !seen.insert(off) {
                return Err(Error::Corrupt(format!("directory tree in {where_} links entry 0x{off:X} twice")));
            }
            let header = data.get(off..off + ENTRY_HEADER_LEN).ok_or_else(|| {
                Error::Corrupt(format!("entry at 0x{off:X} runs past the end of directory {where_}"))
            })?;
            let left = u16::from_le_bytes([header[0], header[1]]);
            let right = u16::from_le_bytes([header[2], header[3]]);
            if left == 0xFFFF && right == 0xFFFF {
                // Sector padding. Only legitimate as the (empty) first slot.
                continue;
            }
            let entry_sector = u32::from_le_bytes(header[4..8].try_into().unwrap());
            let entry_size = u32::from_le_bytes(header[8..12].try_into().unwrap());
            let attributes = header[12];
            let name_len = header[13] as usize;
            let name_start = off + ENTRY_HEADER_LEN;
            let raw_name = data.get(name_start..name_start + name_len).ok_or_else(|| {
                Error::Corrupt(format!("name of entry 0x{off:X} runs past the end of directory {where_}"))
            })?;
            let name = validate_name(raw_name)
                .map_err(|why| Error::Corrupt(format!("unsafe name in {where_}: {why}")))?;
            let path = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
            let is_dir = attributes & ATTR_DIRECTORY != 0;
            if is_dir {
                subdirs.push((entry_sector, entry_size, path.clone()));
            } else {
                self.check_extent(entry_sector, entry_size, &path)?;
            }
            out.push(Entry { path, sector: entry_sector, size: entry_size, is_dir });
            for child in [left, right] {
                if child != 0 && child != 0xFFFF {
                    stack.push(child as usize * 4);
                }
            }
        }
        for (s, z, p) in subdirs {
            self.walk_dir(s, z, &p, depth + 1, visited, out)?;
        }
        Ok(())
    }
}

/// Accepts only names that are a single, ordinary path component on every host OS.
fn validate_name(raw: &[u8]) -> std::result::Result<String, String> {
    // XDVDFS names are single-byte. Latin-1 decoding keeps every byte value distinct.
    let name: String = raw.iter().map(|&b| b as char).collect();
    if name.is_empty() {
        return Err("empty name".into());
    }
    if name == "." || name == ".." {
        return Err(format!("{name:?} is a relative path component"));
    }
    if let Some(c) = name
        .chars()
        .find(|&c| c < ' ' || c == '\u{7F}' || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
    {
        return Err(format!("{name:?} contains forbidden character {c:?}"));
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err(format!("{name:?} ends with a dot or space"));
    }
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        return Err(format!("{name:?} is a reserved device name on Windows"));
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn dirent(left: u16, right: u16, sector: u32, size: u32, attr: u8, name: &str) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend(left.to_le_bytes());
        v.extend(right.to_le_bytes());
        v.extend(sector.to_le_bytes());
        v.extend(size.to_le_bytes());
        v.push(attr);
        v.push(name.len() as u8);
        v.extend(name.as_bytes());
        while v.len() % 4 != 0 {
            v.push(0xFF);
        }
        v
    }

    /// Partition-only image. Volume descriptor at sector 32, root directory at 33,
    /// subdirectory at 34, file data at 35 and 36.
    fn sample_image(subdir_name: &str) -> Vec<u8> {
        let mut img = vec![0u8; 37 * SECTOR_SIZE as usize];
        let sec = |n: usize| n * SECTOR_SIZE as usize;

        let first = dirent(0, 0, 35, 5, 0x20, "a.txt");
        let right = (first.len() / 4) as u16;
        let mut root = dirent(0, right, 35, 5, 0x20, "a.txt");
        let sub = dirent(0, 0, 36, 3, 0x20, "b.bin");
        root.extend(dirent(0, 0, 34, sub.len() as u32, ATTR_DIRECTORY, subdir_name));

        let vd = sec(32);
        img[vd..vd + 20].copy_from_slice(MAGIC);
        img[vd + 20..vd + 24].copy_from_slice(&33u32.to_le_bytes());
        img[vd + 24..vd + 28].copy_from_slice(&(root.len() as u32).to_le_bytes());
        img[sec(33)..sec(33) + root.len()].copy_from_slice(&root);
        img[sec(34)..sec(34) + sub.len()].copy_from_slice(&sub);
        img[sec(35)..sec(35) + 5].copy_from_slice(b"hello");
        img[sec(36)..sec(36) + 3].copy_from_slice(&[1, 2, 3]);
        img
    }

    #[test]
    fn walks_tree_and_reads_files() {
        let mut disc = Disc::new(Cursor::new(sample_image("dir"))).unwrap();
        assert_eq!(disc.layout(), Layout::PartitionOnly);
        let entries = disc.entries().unwrap();
        let paths: Vec<_> = entries.iter().map(|e| (e.path.as_str(), e.is_dir)).collect();
        assert_eq!(paths, [("a.txt", false), ("dir", true), ("dir/b.bin", false)]);
        assert_eq!(disc.read_file(&entries[0]).unwrap(), b"hello");
        assert_eq!(disc.read_file(&entries[2]).unwrap(), [1, 2, 3]);
        assert!(matches!(disc.read_file(&entries[1]), Err(Error::NotAFile(_))));
    }

    #[test]
    fn rejects_path_traversal_names() {
        for bad in ["..", "a\\b", "c:", "x/y", "NUL", "com1.txt", "trailing."] {
            let mut disc = Disc::new(Cursor::new(sample_image(bad))).unwrap();
            assert!(matches!(disc.entries(), Err(Error::Corrupt(_))), "accepted {bad:?}");
        }
    }

    #[test]
    fn rejects_non_xdvdfs() {
        let junk = vec![0xAAu8; 40 * SECTOR_SIZE as usize];
        assert!(matches!(Disc::new(Cursor::new(junk)), Err(Error::NotXdvdfs)));
    }

    #[test]
    fn rejects_extent_past_end() {
        let mut img = sample_image("dir");
        img.truncate(36 * SECTOR_SIZE as usize); // drop b.bin's sector
        let mut disc = Disc::new(Cursor::new(img)).unwrap();
        assert!(matches!(disc.entries(), Err(Error::Corrupt(_))));
    }

    #[test]
    fn extracts_and_skips_existing() {
        let out = std::env::temp_dir().join(format!("disc-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&out);
        let mut disc = Disc::new(Cursor::new(sample_image("dir"))).unwrap();
        let entries = disc.entries().unwrap();

        let first = disc.extract_all(&entries, &out, ExtractOptions::default(), |_, _| {}).unwrap();
        assert_eq!((first.files_written, first.bytes_written), (2, 8));
        assert_eq!(fs::read(out.join("a.txt")).unwrap(), b"hello");
        assert_eq!(fs::read(out.join("dir").join("b.bin")).unwrap(), [1, 2, 3]);

        let again = disc.extract_all(&entries, &out, ExtractOptions::default(), |_, _| {}).unwrap();
        assert_eq!((again.files_written, again.files_skipped), (0, 2));
        fs::remove_dir_all(&out).unwrap();
    }
}
