//! Xbox 360 STFS packages ("LIVE", "PIRS", "CON "): the container title
//! updates and downloadable content come in. Read-only: the header's display
//! name and content type, the file table, and each file's bytes.
//!
//! Layout (public format knowledge): a header whose size is at 0x340; the
//! volume descriptor at 0x379 (block separation, file table block count and
//! first block, totals); data in 4 KB blocks after the header, with a hash
//! table block before every 170 data blocks (and a level-1 table every
//! 28,900, a level-2 table above that). Read-only packages (LIVE, PIRS) keep
//! one copy of each hash table; CON packages two. File table entries are 0x40
//! bytes: a name (0x28), flags (name length, 0x40 consecutive blocks, 0x80
//! directory), block counts and the first block (24-bit little-endian), the
//! parent directory index (big-endian, -1 for the root) and the size.

use std::fmt;

#[derive(Debug)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

fn err<T>(m: impl Into<String>) -> Result<T, Error> {
    Err(Error(m.into()))
}

fn be16(d: &[u8], o: usize) -> u16 {
    u16::from_be_bytes([d[o], d[o + 1]])
}
fn be32(d: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]])
}
fn le24(d: &[u8], o: usize) -> u32 {
    d[o] as u32 | (d[o + 1] as u32) << 8 | (d[o + 2] as u32) << 16
}
fn be24(d: &[u8], o: usize) -> u32 {
    (d[o] as u32) << 16 | (d[o + 1] as u32) << 8 | d[o + 2] as u32
}

/// One entry of the file table.
#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub directory: bool,
    pub consecutive: bool,
    pub blocks: u32,
    pub first_block: u32,
    /// Index of the parent directory in the table, or -1.
    pub parent: i16,
    pub size: u32,
}

pub struct Package<'a> {
    data: &'a [u8],
    pub magic: String,
    pub content_type: u32,
    pub title_id: u32,
    pub display_name: String,
    data_start: usize,
    /// 0: one hash table copy per level (read-only packages); 1: two.
    shift: u32,
    pub entries: Vec<Entry>,
}

const BLOCK: usize = 0x1000;

impl<'a> Package<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Self, Error> {
        if data.len() < 0xC000 {
            return err("too small for an STFS package");
        }
        let magic = String::from_utf8_lossy(&data[0..4]).to_string();
        if !["LIVE", "PIRS", "CON "].contains(&magic.as_str()) {
            return err(format!("not an STFS package (magic {magic:?})"));
        }
        let header_size = be32(data, 0x340) as usize;
        let content_type = be32(data, 0x344);
        let title_id = be32(data, 0x360);
        let name: Vec<u16> = (0..0x80).map(|i| be16(data, 0x411 + 2 * i)).take_while(|&c| c != 0).collect();
        let display_name = String::from_utf16_lossy(&name);
        // Volume descriptor at 0x379.
        let vd = 0x379;
        if data[vd] != 0x24 {
            return err(format!("volume descriptor size {:#x}, expected 0x24 (not an STFS volume)", data[vd]));
        }
        let separation = data[vd + 2];
        let table_blocks = u16::from_le_bytes([data[vd + 3], data[vd + 4]]) as u32;
        let table_first = le24(data, vd + 5);
        let shift = if separation & 1 == 1 { 0 } else { 1 };
        let mut p = Package {
            data,
            magic,
            content_type,
            title_id,
            display_name,
            data_start: (header_size + 0xFFF) & !0xFFF,
            shift,
            entries: Vec::new(),
        };
        // The file table: consecutive blocks from its first (read-only packages).
        let mut table = Vec::new();
        for k in 0..table_blocks {
            table.extend_from_slice(p.block(table_first + k)?);
        }
        for e in table.chunks_exact(0x40) {
            if e[0] == 0 {
                break;
            }
            let flags = e[0x28];
            let len = (flags & 0x3F) as usize;
            p.entries.push(Entry {
                name: String::from_utf8_lossy(&e[..len.min(0x28)]).to_string(),
                directory: flags & 0x80 != 0,
                consecutive: flags & 0x40 != 0,
                blocks: le24(e, 0x29),
                first_block: le24(e, 0x2F),
                parent: be16(e, 0x32) as i16,
                size: be32(e, 0x34),
            });
        }
        Ok(p)
    }

    /// The package offset of data block `b`, past the hash tables before it.
    fn offset(&self, b: u32) -> usize {
        let shift = self.shift;
        let mut base = (b / 170 + 1) << shift;
        if b >= 170 {
            base += (b / 28_900 + 1) << shift;
            if b >= 28_900 {
                base += 1 << shift;
            }
        }
        self.data_start + (base + b) as usize * BLOCK
    }

    fn block(&self, b: u32) -> Result<&'a [u8], Error> {
        let o = self.offset(b);
        self.data.get(o..o + BLOCK).ok_or_else(|| Error(format!("block {b} at {o:#x} is past the end of the package")))
    }

    /// The level-0 hash entry of data block `b`: its next block, for files
    /// whose blocks aren't consecutive. The table sits just before the 170
    /// blocks it covers (after any higher-level tables there).
    fn next_block(&self, b: u32) -> Result<u32, Error> {
        let first = b - b % 170;
        let table = self.offset(first) - (BLOCK << self.shift);
        let e = table + (b % 170) as usize * 0x18;
        let entry = self.data.get(e..e + 0x18).ok_or_else(|| Error(format!("hash entry of block {b} is past the end")))?;
        Ok(be24(entry, 0x15))
    }

    /// The full path of entry `i`.
    pub fn path(&self, i: usize) -> String {
        let mut parts = vec![self.entries[i].name.clone()];
        let mut p = self.entries[i].parent;
        let mut guard = 0;
        while p >= 0 && (p as usize) < self.entries.len() && guard < 64 {
            parts.push(self.entries[p as usize].name.clone());
            p = self.entries[p as usize].parent;
            guard += 1;
        }
        parts.reverse();
        parts.join("/")
    }

    /// The bytes of file entry `i`.
    pub fn read(&self, i: usize) -> Result<Vec<u8>, Error> {
        let e = &self.entries[i];
        let mut out = Vec::with_capacity(e.size as usize);
        let mut b = e.first_block;
        for k in 0..e.blocks {
            if out.len() >= e.size as usize {
                break;
            }
            let data = self.block(b)?;
            let take = (e.size as usize - out.len()).min(BLOCK);
            out.extend_from_slice(&data[..take]);
            if k + 1 < e.blocks {
                b = if e.consecutive { b + 1 } else { self.next_block(b)? };
            }
        }
        if out.len() != e.size as usize {
            return err(format!("{}: read {} of {} bytes", e.name, out.len(), e.size));
        }
        Ok(out)
    }
}
