//! Custom entrance songs through the game's own sound engine (Wwise), so they
//! get the arena reverb, the music bus's EQ and volumes like a built-in
//! entrance theme (the user's request, saved for the end, 2026-11-25).
//!
//! The game plays entrance music through `sub_8334C138(manager, music id,
//! title)`. Id 0x270E is a custom song: the game hands it to the console's
//! music player (`kernel::music`), which mixes it over the game's output,
//! dry. Any other id becomes the Wwise event `Play_MUS_%04d_%d_%d` (music id,
//! mode, variation), which plays a streamed sound under the entrance-music
//! actor-mixer `0x078CE9F2` (RoomVerb insert, music bus).
//!
//! This is data conversion at the file boundary, not emulation: the game's
//! own Wwise reads its packages through `NtReadFile`, and two of them are
//! served with additions (an overlay on the file layer, `kernel::file`):
//!
//! - `Game_Sound_E_S.pck`: the bank `Entrance_Music_E_S` (0xE85DDA21) gains a
//!   music id of its own, [`CARRIER`]: one Sound (PCM, streamed without
//!   prefetch) in one random container under the actor-mixer (added to its
//!   child list), and the events `Play_MUS_9990_<mode>_<variation>` for modes
//!   and variations 0-3, each a Play action copied from theme 0100's. The
//!   grown bank is served past the end of the file; the bank table entry
//!   points there.
//! - `Game_Sound_M.pck`: the streamed-files table gains the Sound's file,
//!   a window past the end of the file. Reads there return a RIFX WAVE
//!   (WAVE_FORMAT_EXTENSIBLE, 16-bit big-endian PCM, 48 kHz stereo; the
//!   format the game's PCM source checks for, `sub_82F06A20`) decoded on
//!   demand from the selected custom song.
//!
//! [`entrance_music_request`] wraps `sub_8334C138`: a custom song that is in
//! the music folder is selected for that window and the request goes on as
//! music id [`CARRIER`]. The Jukebox and other menu songs stay on the native
//! player (menu music bypasses the reverb anyway).
//!
//! Formats (all big-endian; found statically, 2026-11-26): AKPK version 1,
//! header `magic, header size, version, language-map size, banks-table size,
//! streamed-files-table size, externals size`, then those parts; a table is
//! a count and 20-byte entries `id, block size, size, start block,
//! language`, sorted by id. Banks are version 0x41 (65): chunks `BKHD`,
//! `HIRC` (count, then objects `type u8, size u32, data` with the id first),
//! `STID`.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use ppc_rt::Ctx;

use crate::mem;

/// The music id custom entrance songs are played under (any 4-digit id
/// below 0x2710 that is neither 0x270E nor 0x270F and has no theme).
pub const CARRIER: u32 = 9990;
const CUSTOM_TRACK: u32 = 0x270E;
/// `sub_8334C138(manager, music id, title)`.
const REQUEST: u32 = 0x8334_C138;
/// The music browser (its selected title at +0x20094), when the request names no title.
const BROWSER: u32 = 0x839B_FC38;
const BROWSER_TITLE: u32 = 0x2_0094;

const BANK_ID: u32 = 0xE85D_DA21;
const BANK_SIZE: u32 = 0xE0F7;
const BANK_BLOCK: u32 = 0x120D;
const BLOCK: u32 = 0x800;
/// The entrance-music actor-mixer (RoomVerb ShareSet 0x0481C14E, bus 0x26160EEA).
const MIXER: u32 = 0x078C_E9F2;
/// Theme 0100's sound, its random container, and the Play actions of
/// `Play_MUS_0100_0_0` and `Play_MUS_0100_0_2` (the latter delayed, prop 0x0D).
const TEMPLATE_SOUND: u32 = 0x0947_7532;
const TEMPLATE_CONTAINER: u32 = 0x31B3_EAE6;
const TEMPLATE_PLAY: u32 = 0x2B1F_FC9E;
const TEMPLATE_PLAY_LATE: u32 = 0x3925_D73C;
/// The template sound's streamed file (its table entry gives the language).
const TEMPLATE_FILE: u32 = 0x14A0_3673;

const TYPE_SOUND: u8 = 2;
const TYPE_ACTION: u8 = 3;
const TYPE_EVENT: u8 = 4;
const TYPE_RANDOM: u8 = 5;
const TYPE_ACTOR_MIXER: u8 = 7;
const PLUGIN_PCM: u32 = 0x0001_0001;
const PLUGIN_VORBIS: u32 = 0x0004_0001;
const STREAMED: u32 = 1;
const PREFETCH_STREAMED: u32 = 2;
/// Modes and variations given events (the game asks for mode 0 with a
/// variation chosen among a few; all sixteen cost little).
const MODES: u32 = 4;
const VARIATIONS: u32 = 4;

const RATE: u32 = 48_000;
/// The longest song served (20 minutes); the table entry's size.
pub const MAX_FRAMES: u32 = 20 * 60 * RATE;
pub const WAV_HEADER: usize = 52;
pub const STREAM_SIZE: u32 = WAV_HEADER as u32 + MAX_FRAMES * 4;

/// Wwise's id of a name: 32-bit FNV-1 of the lower-cased name.
pub fn fnv1(name: &str) -> u32 {
    let mut h: u32 = 0x811C_9DC5;
    for b in name.to_ascii_lowercase().bytes() {
        h = h.wrapping_mul(0x0100_0193) ^ b as u32;
    }
    h
}

pub fn sound_id() -> u32 {
    fnv1("wwe14_custom_entrance_sound")
}
pub fn container_id() -> u32 {
    fnv1("wwe14_custom_entrance_container")
}
/// The Sound's source and streamed file id.
pub fn stream_id() -> u32 {
    fnv1("wwe14_custom_entrance_stream")
}
pub fn event_id(mode: u32, variation: u32) -> u32 {
    fnv1(&format!("Play_MUS_{CARRIER:04}_{mode}_{variation}"))
}
fn action_id(mode: u32, variation: u32) -> u32 {
    fnv1(&format!("wwe14_custom_entrance_play_{mode}_{variation}"))
}

/// Off with `WWE14_ENTRANCE_WWISE=off`: custom entrance songs go to the native player.
fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| !std::env::var("WWE14_ENTRANCE_WWISE").is_ok_and(|v| v.eq_ignore_ascii_case("off")))
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes(b[at..at + 2].try_into().unwrap())
}

// ---- Packages -------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub id: u32,
    pub block: u32,
    pub size: u32,
    pub start: u32,
    pub lang: u32,
}

#[derive(Clone, Debug)]
pub struct PckHeader {
    pub lang_map: Vec<u8>,
    pub banks: Vec<Entry>,
    pub streams: Vec<Entry>,
    pub externals: Vec<u8>,
}

fn table(b: &[u8], size: usize) -> Result<Vec<Entry>, String> {
    if size < 4 {
        return Err(format!("a table of {size} bytes"));
    }
    let n = be32(b, 0) as usize;
    if 4 + 20 * n != size {
        return Err(format!("a table of {n} entries in {size} bytes"));
    }
    Ok((0..n)
        .map(|i| {
            let e = &b[4 + 20 * i..];
            Entry { id: be32(e, 0), block: be32(e, 4), size: be32(e, 8), start: be32(e, 12), lang: be32(e, 16) }
        })
        .collect())
}

fn write_table(out: &mut Vec<u8>, t: &[Entry]) {
    out.extend((t.len() as u32).to_be_bytes());
    for e in t {
        for v in [e.id, e.block, e.size, e.start, e.lang] {
            out.extend(v.to_be_bytes());
        }
    }
}

/// The header's length (its first 8 bytes and the size they give).
pub fn header_len(first: &[u8]) -> Result<usize, String> {
    if first.len() < 8 || &first[0..4] != b"AKPK" {
        return Err("not an AKPK package".into());
    }
    Ok(8 + be32(first, 4) as usize)
}

pub fn parse_header(raw: &[u8]) -> Result<PckHeader, String> {
    let len = header_len(raw)?;
    if raw.len() < len || len < 0x1C {
        return Err(format!("header of {len} bytes, {} read", raw.len()));
    }
    if be32(raw, 8) != 1 {
        return Err(format!("package version {}", be32(raw, 8)));
    }
    let (lm, bl, sl, ex) = (be32(raw, 12) as usize, be32(raw, 16) as usize, be32(raw, 20) as usize, be32(raw, 24) as usize);
    if 0x14 + lm + bl + sl + ex != len - 8 {
        return Err(format!("header parts {lm}+{bl}+{sl}+{ex} don't add up to {}", len - 8));
    }
    let mut at = 0x1C;
    let lang_map = raw[at..at + lm].to_vec();
    at += lm;
    let banks = table(&raw[at..at + bl], bl)?;
    at += bl;
    let streams = table(&raw[at..at + sl], sl)?;
    at += sl;
    let externals = raw[at..at + ex].to_vec();
    Ok(PckHeader { lang_map, banks, streams, externals })
}

pub fn write_header(h: &PckHeader) -> Vec<u8> {
    let (bl, sl) = (4 + 20 * h.banks.len(), 4 + 20 * h.streams.len());
    let mut out = Vec::new();
    out.extend(b"AKPK");
    out.extend(((0x14 + h.lang_map.len() + bl + sl + h.externals.len()) as u32).to_be_bytes());
    for v in [1, h.lang_map.len(), bl, sl, h.externals.len()] {
        out.extend((v as u32).to_be_bytes());
    }
    out.extend(&h.lang_map);
    write_table(&mut out, &h.banks);
    write_table(&mut out, &h.streams);
    out.extend(&h.externals);
    out
}

// ---- The bank -------------------------------------------------------------------------

pub struct Obj {
    pub ty: u8,
    pub data: Vec<u8>,
}

impl Obj {
    pub fn id(&self) -> u32 {
        be32(&self.data, 0)
    }
}

/// A bank's chunks, in order.
pub fn chunks(bank: &[u8]) -> Result<Vec<([u8; 4], &[u8])>, String> {
    let mut out = Vec::new();
    let mut p = 0;
    while p < bank.len() {
        if p + 8 > bank.len() {
            return Err(format!("a cut chunk header at 0x{p:X}"));
        }
        let size = be32(bank, p + 4) as usize;
        if p + 8 + size > bank.len() {
            return Err(format!("chunk {} at 0x{p:X} runs past the bank", String::from_utf8_lossy(&bank[p..p + 4])));
        }
        out.push((bank[p..p + 4].try_into().unwrap(), &bank[p + 8..p + 8 + size]));
        p += 8 + size;
    }
    Ok(out)
}

pub fn hirc_objects(hirc: &[u8]) -> Result<Vec<Obj>, String> {
    let n = be32(hirc, 0) as usize;
    let mut out = Vec::with_capacity(n);
    let mut q = 4;
    for i in 0..n {
        if q + 5 > hirc.len() {
            return Err(format!("HIRC object {i} of {n} is cut"));
        }
        let size = be32(hirc, q + 1) as usize;
        if size < 4 || q + 5 + size > hirc.len() {
            return Err(format!("HIRC object {i} of {n}: size {size}"));
        }
        out.push(Obj { ty: hirc[q], data: hirc[q + 5..q + 5 + size].to_vec() });
        q += 5 + size;
    }
    if q != hirc.len() {
        return Err(format!("HIRC has {} bytes after its {n} objects", hirc.len() - q));
    }
    Ok(out)
}

/// The child list at the end of an actor-mixer's data: (offset of its count, ids).
/// The list must hold `member`, ascending.
fn child_list(data: &[u8], member: u32) -> Option<(usize, Vec<u32>)> {
    let words = data.len() / 4;
    (1..words).rev().find_map(|n| {
        let at = data.len().checked_sub(4 + 4 * n)?;
        if at < 4 || be32(data, at) as usize != n {
            return None;
        }
        let ids: Vec<u32> = (0..n).map(|i| be32(data, at + 4 + 4 * i)).collect();
        (ids.windows(2).all(|w| w[0] < w[1]) && ids.contains(&member)).then_some((at, ids))
    })
}

/// `Entrance_Music_E_S` with the carrier theme added (see the module).
pub fn patch_bank(bank: &[u8]) -> Result<Vec<u8>, String> {
    let parts = chunks(bank)?;
    let (_, bkhd) = parts.iter().find(|(t, _)| t == b"BKHD").ok_or("no BKHD chunk")?;
    if be32(bkhd, 0) != 0x41 || be32(bkhd, 4) != BANK_ID {
        return Err(format!("BKHD says version 0x{:X}, bank 0x{:08X}", be32(bkhd, 0), be32(bkhd, 4)));
    }
    let (_, hirc) = parts.iter().find(|(t, _)| t == b"HIRC").ok_or("no HIRC chunk")?;
    let objs = hirc_objects(hirc)?;
    let find = |id: u32, ty: u8| -> Result<&Obj, String> {
        let o = objs.iter().find(|o| o.id() == id).ok_or(format!("object 0x{id:08X} is missing"))?;
        if o.ty != ty {
            return Err(format!("object 0x{id:08X} is type {}, not {ty}", o.ty));
        }
        Ok(o)
    };
    let (s, c, f) = (sound_id(), container_id(), stream_id());
    let mut new_ids = vec![s, c];
    for m in 0..MODES {
        for v in 0..VARIATIONS {
            new_ids.push(event_id(m, v));
            new_ids.push(action_id(m, v));
        }
    }
    if let Some(o) = objs.iter().find(|o| new_ids.contains(&o.id())) {
        return Err(format!("the bank already has an object 0x{:08X}", o.id()));
    }

    // The Sound: the template's, as PCM streamed without prefetch (no file
    // offset or in-memory size then), parented to the new container.
    let t = &find(TEMPLATE_SOUND, TYPE_SOUND)?.data;
    if t.len() < 39 || be32(t, 4) != PLUGIN_VORBIS || be32(t, 8) != PREFETCH_STREAMED || t[30] != 0 || be32(t, 35) != TEMPLATE_CONTAINER {
        return Err(format!("template sound 0x{TEMPLATE_SOUND:08X} is not laid out as expected"));
    }
    let mut sound = Vec::with_capacity(t.len());
    for v in [s, PLUGIN_PCM, STREAMED, f, f] {
        sound.extend(v.to_be_bytes());
    }
    sound.extend(&t[28..35]); // source bits; no FX override, no FX, no bus override
    sound.extend(c.to_be_bytes());
    sound.extend(&t[39..]);

    // The container: the template's with its one sound replaced.
    let t = &find(TEMPLATE_CONTAINER, TYPE_RANDOM)?.data;
    if t.len() < 14 || be32(t, 10) != MIXER {
        return Err(format!("template container 0x{TEMPLATE_CONTAINER:08X} is not under the actor-mixer"));
    }
    let mut container = t.clone();
    container[0..4].copy_from_slice(&c.to_be_bytes());
    let old = TEMPLATE_SOUND.to_be_bytes();
    let mut replaced = 0;
    let mut i = 4;
    while i + 4 <= container.len() {
        if container[i..i + 4] == old {
            container[i..i + 4].copy_from_slice(&s.to_be_bytes());
            replaced += 1;
            i += 4;
        } else {
            i += 1;
        }
    }
    if replaced != 2 {
        return Err(format!("template container names its sound {replaced} times, not 2"));
    }

    // The actor-mixer: the container joins its children (kept ascending).
    let t = &find(MIXER, TYPE_ACTOR_MIXER)?.data;
    let (at, mut kids) = child_list(t, TEMPLATE_CONTAINER).ok_or("the actor-mixer's child list wasn't found")?;
    kids.push(c);
    kids.sort_unstable();
    let mut mixer = t[..at].to_vec();
    mixer.extend((kids.len() as u32).to_be_bytes());
    for k in &kids {
        mixer.extend(k.to_be_bytes());
    }

    // Actions and events.
    let mut tail: Vec<Obj> = Vec::new();
    for m in 0..MODES {
        for v in 0..VARIATIONS {
            let template = if v == 2 { TEMPLATE_PLAY_LATE } else { TEMPLATE_PLAY };
            let t = &find(template, TYPE_ACTION)?.data;
            if t.len() < 14 || be16(t, 4) != 0x0403 || be32(t, 6) != TEMPLATE_CONTAINER || be32(t, t.len() - 4) != BANK_ID {
                return Err(format!("template action 0x{template:08X} is not a Play of theme 0100"));
            }
            let mut a = t.clone();
            a[0..4].copy_from_slice(&action_id(m, v).to_be_bytes());
            a[6..10].copy_from_slice(&c.to_be_bytes());
            tail.push(Obj { ty: TYPE_ACTION, data: a });
            let mut e = Vec::new();
            for x in [event_id(m, v), 1, action_id(m, v)] {
                e.extend(x.to_be_bytes());
            }
            tail.push(Obj { ty: TYPE_EVENT, data: e });
        }
    }

    let mut out_objs: Vec<(u8, &[u8])> = Vec::with_capacity(objs.len() + tail.len() + 2);
    for o in &objs {
        if o.id() == MIXER {
            // Children ahead of their parent, as the bank orders them.
            out_objs.push((TYPE_SOUND, &sound));
            out_objs.push((TYPE_RANDOM, &container));
            out_objs.push((TYPE_ACTOR_MIXER, &mixer));
        } else {
            out_objs.push((o.ty, &o.data));
        }
    }
    for o in &tail {
        out_objs.push((o.ty, &o.data));
    }
    let mut new_hirc = Vec::new();
    new_hirc.extend((out_objs.len() as u32).to_be_bytes());
    for (ty, d) in &out_objs {
        new_hirc.push(*ty);
        new_hirc.extend((d.len() as u32).to_be_bytes());
        new_hirc.extend(*d);
    }
    let mut out = Vec::with_capacity(bank.len() + new_hirc.len() - hirc.len());
    for (tag, d) in &parts {
        let d: &[u8] = if tag == b"HIRC" { &new_hirc } else { d };
        out.extend(tag);
        out.extend((d.len() as u32).to_be_bytes());
        out.extend(d);
    }
    Ok(out)
}

// ---- The song's stream ----------------------------------------------------------------

/// The WAVE header of the stream for a song of `frames` frames.
pub fn wav_header(frames: u32) -> [u8; WAV_HEADER] {
    let data = frames * 4;
    let mut h = Vec::with_capacity(WAV_HEADER);
    h.extend(b"RIFX");
    h.extend((WAV_HEADER as u32 - 8 + data).to_be_bytes());
    h.extend(b"WAVEfmt ");
    h.extend(24u32.to_be_bytes());
    h.extend(0xFFFEu16.to_be_bytes()); // WAVE_FORMAT_EXTENSIBLE, as Wwise's PCM source requires
    h.extend(2u16.to_be_bytes());
    h.extend(RATE.to_be_bytes());
    h.extend((RATE * 4).to_be_bytes());
    h.extend(4u16.to_be_bytes()); // block align
    h.extend(16u16.to_be_bytes()); // bits per sample
    h.extend(6u16.to_be_bytes()); // extension size
    h.extend(16u16.to_be_bytes()); // valid bits
    h.extend(3u32.to_be_bytes()); // channel mask: front left, front right
    h.extend(b"data");
    h.extend(data.to_be_bytes());
    h.try_into().unwrap()
}

/// A selected song, decoded as it is read: 48 kHz stereo 16-bit samples.
pub struct Pcm {
    pub title: String,
    state: Mutex<PcmState>,
    more: Condvar,
}

struct PcmState {
    samples: Vec<i16>,
    done: bool,
    /// The length the header gave (frames), fixed at the first read.
    declared: Option<u32>,
    /// The song's length from its tags or container, if known.
    estimate: Option<u32>,
}

impl Pcm {
    pub fn new(title: &str, estimate: Option<u32>) -> Pcm {
        Pcm {
            title: title.to_string(),
            state: Mutex::new(PcmState { samples: Vec::new(), done: false, declared: None, estimate: estimate.map(|e| e.min(MAX_FRAMES)) }),
            more: Condvar::new(),
        }
    }

    /// Adds decoded 48 kHz stereo frames; false once the most a stream holds is reached.
    pub fn push(&self, frames: &[f32]) -> bool {
        let mut s = self.state.lock().unwrap();
        let room = (MAX_FRAMES as usize * 2).saturating_sub(s.samples.len());
        s.samples.extend(frames.iter().take(room).map(|x| (x.clamp(-1.0, 1.0) * 32767.0).round() as i16));
        self.more.notify_all();
        s.samples.len() < MAX_FRAMES as usize * 2
    }

    pub fn finish(&self) {
        self.state.lock().unwrap().done = true;
        self.more.notify_all();
    }

    /// Bytes of the stream (header, then big-endian samples) at `off`.
    pub fn read(&self, off: u64, dst: &mut [u8]) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut s = self.state.lock().unwrap();
        // The length is fixed by the first read: the song's known length, or
        // the whole decode's when it isn't known.
        if s.declared.is_none() {
            while s.estimate.is_none() && !s.done && std::time::Instant::now() < deadline {
                s = self.more.wait_timeout(s, std::time::Duration::from_millis(100)).unwrap().0;
            }
            let frames = if s.done { (s.samples.len() / 2) as u32 } else { s.estimate.unwrap_or((s.samples.len() / 2) as u32) };
            s.declared = Some(frames);
        }
        let frames = s.declared.unwrap();
        let header = wav_header(frames);
        let data_end = WAV_HEADER as u64 + frames as u64 * 4;
        let end = off + dst.len() as u64;
        // Wait for the samples this read covers.
        let need = (end.min(data_end).saturating_sub(WAV_HEADER as u64)).div_ceil(2) as usize;
        while s.samples.len() < need && !s.done {
            if std::time::Instant::now() >= deadline {
                crate::diag::log(&format!("music: entrance song \"{}\": decoding fell behind the game's read; served silence", self.title));
                break;
            }
            s = self.more.wait_timeout(s, std::time::Duration::from_millis(50)).unwrap().0;
        }
        for (i, b) in dst.iter_mut().enumerate() {
            let p = off + i as u64;
            *b = if p < WAV_HEADER as u64 {
                header[p as usize]
            } else if p < data_end {
                let k = (p - WAV_HEADER as u64) as usize;
                let v = s.samples.get(k / 2).copied().unwrap_or(0).to_be_bytes();
                v[k % 2]
            } else {
                0
            };
        }
    }
}

static CURRENT: Mutex<Option<Arc<Pcm>>> = Mutex::new(None);

/// Makes `song` the one the stream serves, decoding it on a thread.
fn select(song: &crate::music::Song) {
    let estimate = (song.duration_ms > 0).then(|| (song.duration_ms as u64 * RATE as u64 / 1000).min(MAX_FRAMES as u64) as u32);
    let pcm = Arc::new(Pcm::new(&song.title, estimate));
    *CURRENT.lock().unwrap() = Some(pcm.clone());
    let path = song.path.clone();
    let _ = std::thread::Builder::new().name("entrance song decoder".into()).spawn(move || {
        let r = crate::music::decode_stereo_48k(&path, |out| {
            let current = CURRENT.lock().unwrap().as_ref().is_some_and(|c| Arc::ptr_eq(c, &pcm));
            current && pcm.push(out)
        });
        if let Err(e) = r {
            crate::diag::log(&format!("music: entrance song {} stopped decoding: {e}", path.display()));
        }
        pcm.finish();
    });
}

fn read_song(off: u64, dst: &mut [u8]) {
    let pcm = CURRENT.lock().unwrap().clone();
    match pcm {
        Some(p) => p.read(off, dst),
        None => {
            static ONCE: AtomicBool = AtomicBool::new(false);
            if !ONCE.swap(true, Ordering::Relaxed) {
                crate::diag::log("music: the game read the custom entrance stream with no song selected; served silence");
            }
            dst.fill(0);
        }
    }
}

// ---- Overlays on the file layer ------------------------------------------------------------

enum Content {
    Bytes(Vec<u8>),
    Song,
}

struct Segment {
    start: u64,
    len: u64,
    content: Content,
    what: String,
    served: AtomicBool,
}

/// Additions served over one package (see the module).
pub struct Overlay {
    segments: Vec<Segment>,
}

/// The patched bank has been read by the game (its events can be posted).
static BANK_SERVED: AtomicBool = AtomicBool::new(false);
/// The streamed-files table with the song's entry has been read.
static TABLE_SERVED: AtomicBool = AtomicBool::new(false);

impl Overlay {
    /// The file's size as served.
    pub fn size(&self, real: u64) -> u64 {
        self.segments.iter().map(|s| s.start + s.len).fold(real, u64::max)
    }

    /// Lays the additions over `dst`, the bytes read at `pos` (zero past the real file).
    pub fn apply(&self, pos: u64, dst: &mut [u8]) {
        let end = pos + dst.len() as u64;
        for s in &self.segments {
            let (a, b) = (pos.max(s.start), end.min(s.start + s.len));
            if a >= b {
                continue;
            }
            let out = &mut dst[(a - pos) as usize..(b - pos) as usize];
            match &s.content {
                Content::Bytes(bytes) => out.copy_from_slice(&bytes[(a - s.start) as usize..(b - s.start) as usize]),
                Content::Song => read_song(a - s.start, out),
            }
            if !s.served.swap(true, Ordering::Relaxed) {
                crate::diag::log(&format!("music: serving {} (first read at 0x{a:X})", s.what));
                if s.what.starts_with("the bank") {
                    BANK_SERVED.store(true, Ordering::Relaxed);
                }
                if s.what.starts_with("the streamed-files table") {
                    TABLE_SERVED.store(true, Ordering::Relaxed);
                }
            }
        }
    }
}

fn read_at(f: &mut std::fs::File, at: u64, len: usize) -> Result<Vec<u8>, String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut b = vec![0u8; len];
    f.seek(SeekFrom::Start(at)).and_then(|_| f.read_exact(&mut b)).map_err(|e| e.to_string())?;
    Ok(b)
}

fn read_header(f: &mut std::fs::File) -> Result<Vec<u8>, String> {
    let first = read_at(f, 0, 8)?;
    let len = header_len(&first)?;
    read_at(f, 0, len)
}

/// The patched header and bank of `Game_Sound_E_S.pck` (file size `real`).
pub fn patch_es(header: &[u8], bank: &[u8], real: u64) -> Result<(Vec<u8>, u64, Vec<u8>), String> {
    let mut h = parse_header(header)?;
    let patched = patch_bank(bank)?;
    let at = real.div_ceil(BLOCK as u64);
    let e = h.banks.iter_mut().find(|e| e.id == BANK_ID).ok_or("no Entrance_Music_E_S in the bank table")?;
    e.size = patched.len() as u32;
    e.start = at as u32;
    let out = write_header(&h);
    if out.len() != header_len(header)? {
        return Err("the rewritten header changed length".into());
    }
    Ok((out, at * BLOCK as u64, patched))
}

/// The patched header of `Game_Sound_M.pck` (file size `real`) and the stream's offset.
pub fn patch_m(header: &[u8], real: u64) -> Result<(Vec<u8>, u64), String> {
    let mut h = parse_header(header)?;
    let f = stream_id();
    if h.streams.iter().any(|e| e.id == f) || h.banks.iter().any(|e| e.id == f) {
        return Err(format!("the package already has a file 0x{f:08X}"));
    }
    let lang = h.streams.iter().find(|e| e.id == TEMPLATE_FILE).ok_or(format!("no streamed file 0x{TEMPLATE_FILE:08X}"))?.lang;
    let first_data = h.banks.iter().chain(&h.streams).map(|e| e.start as u64 * e.block as u64).min().unwrap_or(u64::MAX);
    let at = real.div_ceil(BLOCK as u64);
    let i = h.streams.partition_point(|e| e.id < f);
    h.streams.insert(i, Entry { id: f, block: BLOCK, size: STREAM_SIZE, start: at as u32, lang });
    let out = write_header(&h);
    if out.len() as u64 > first_data {
        return Err(format!("the grown header (0x{:X} bytes) runs into the first file (0x{first_data:X})", out.len()));
    }
    Ok((out, at * BLOCK as u64))
}

fn build_es(path: &Path) -> Result<Overlay, String> {
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let real = f.metadata().map_err(|e| e.to_string())?.len();
    let header = read_header(&mut f)?;
    let h = parse_header(&header)?;
    let e = h.banks.iter().filter(|e| e.id == BANK_ID).collect::<Vec<_>>();
    let [e] = e[..] else { return Err(format!("{} bank-table entries for 0x{BANK_ID:08X}", e.len())) };
    if e.block != BLOCK || e.size != BANK_SIZE || e.start != BANK_BLOCK {
        return Err(format!("Entrance_Music_E_S is at block 0x{:X}, 0x{:X} bytes (blocks of 0x{:X}); expected block 0x{BANK_BLOCK:X}, 0x{BANK_SIZE:X} bytes", e.start, e.size, e.block));
    }
    let bank = read_at(&mut f, e.start as u64 * e.block as u64, e.size as usize)?;
    let (new_header, at, patched) = patch_es(&header, &bank, real)?;
    let (hl, bl) = (new_header.len() as u64, patched.len() as u64);
    Ok(Overlay {
        segments: vec![
            Segment { start: 0, len: hl, content: Content::Bytes(new_header), what: "the bank table of Game_Sound_E_S.pck with Entrance_Music_E_S moved".into(), served: AtomicBool::new(false) },
            Segment {
                start: at,
                len: bl,
                content: Content::Bytes(patched),
                what: format!("the bank Entrance_Music_E_S with theme {CARRIER} ({bl} bytes, was {BANK_SIZE})"),
                served: AtomicBool::new(false),
            },
        ],
    })
}

fn build_m(path: &Path) -> Result<Overlay, String> {
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let real = f.metadata().map_err(|e| e.to_string())?.len();
    let header = read_header(&mut f)?;
    let (new_header, at) = patch_m(&header, real)?;
    let hl = new_header.len() as u64;
    Ok(Overlay {
        segments: vec![
            Segment {
                start: 0,
                len: hl,
                content: Content::Bytes(new_header),
                what: format!("the streamed-files table of Game_Sound_M.pck with the custom entrance stream 0x{:08X}", stream_id()),
                served: AtomicBool::new(false),
            },
            Segment { start: at, len: STREAM_SIZE as u64, content: Content::Song, what: "the custom entrance song's stream".into(), served: AtomicBool::new(false) },
        ],
    })
}

/// The overlay for a host file the game opens, if it is one of the two packages.
pub fn overlay_for(host: &Path) -> Option<Arc<Overlay>> {
    static ES: OnceLock<Arc<Overlay>> = OnceLock::new();
    static M: OnceLock<Arc<Overlay>> = OnceLock::new();
    if !enabled() {
        return None;
    }
    let name = host.file_name()?.to_str()?.to_ascii_lowercase();
    let (cell, build): (&OnceLock<Arc<Overlay>>, fn(&Path) -> Result<Overlay, String>) = match name.as_str() {
        "game_sound_e_s.pck" => (&ES, build_es),
        "game_sound_m.pck" => (&M, build_m),
        _ => return None,
    };
    Some(
        cell.get_or_init(|| {
            Arc::new(build(host).unwrap_or_else(|e| {
                panic!(
                    "custom entrance songs through Wwise: {} is not as expected: {e}. \
                     Fix kernel::entrance for this data, or set WWE14_ENTRANCE_WWISE=off",
                    host.display()
                )
            }))
        })
        .clone(),
    )
}

// ---- The game's entrance-music request -----------------------------------------------------

/// The library's song with this title (as the game keeps titles: at most 39 characters).
fn find_song(title: &str) -> Option<crate::music::Song> {
    let clip = |s: &str| String::from_utf16_lossy(&s.encode_utf16().take(39).collect::<Vec<_>>());
    let want = clip(title);
    let lib = crate::music::library();
    lib.iter()
        .find(|s| clip(&s.title) == want)
        .or_else(|| lib.iter().find(|s| clip(&s.title).to_lowercase() == want.to_lowercase()))
        .cloned()
}

/// `sub_8334C138(manager, music id, title)`, the entrance-music request (see the module).
pub extern "C-unwind" fn entrance_music_request(c: &mut Ctx, b: *mut u8) {
    static CALLS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if CALLS.fetch_add(1, Ordering::Relaxed) < 100 {
        crate::diag::log(&format!("music: entrance-music request: id {:#x}, title pointer 0x{:08X} [from 0x{:08X}]", c.r[4] as u32, c.r[5] as u32, c.lr as u32));
    }
    if c.r[4] as u32 == CUSTOM_TRACK && enabled() {
        redirect(c);
    }
    unsafe { ppc_rt::original(REQUEST).expect("the entrance-music request is replaced")(c, b) };
}

fn redirect(c: &mut Ctx) {
    let (manager, name) = (c.r[3] as u32, c.r[5] as u32);
    // The original ignores requests while this is set.
    if mem::r32(manager + 0x70) != 0 {
        crate::diag::log("music: entrance song request ignored by the game (manager +0x70 set); left as is");
        return;
    }
    let at = if name != 0 {
        name
    } else {
        match mem::r32(BROWSER) {
            0 => {
                crate::diag::log("music: entrance song request with no title and no music browser; left to the native player");
                return;
            }
            browser => browser + BROWSER_TITLE,
        }
    };
    let title = String::from_utf16_lossy(&mem::wstr(at, 40));
    if title.is_empty() {
        crate::diag::log("music: entrance song request with an empty title; left to the native player");
        return;
    }
    if !BANK_SERVED.load(Ordering::Relaxed) || !TABLE_SERVED.load(Ordering::Relaxed) {
        crate::diag::log(&format!(
            "music: entrance song \"{title}\" on the native player: the game hasn't read the added theme yet (bank {}, stream table {})",
            BANK_SERVED.load(Ordering::Relaxed),
            TABLE_SERVED.load(Ordering::Relaxed)
        ));
        return;
    }
    let Some(song) = find_song(&title) else {
        crate::diag::log(&format!("music: entrance song \"{title}\" is not in the music folder; left to the native player"));
        return;
    };
    select(&song);
    crate::diag::log(&format!("music: entrance song \"{}\" through Wwise as theme {CARRIER}", song.title));
    c.r[4] = CARRIER as u64;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sound_dir() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../private/game/sound")
    }

    fn file(name: &str) -> Option<std::path::PathBuf> {
        let p = sound_dir().join(name);
        if p.is_file() {
            Some(p)
        } else {
            eprintln!("{} missing: test skipped", p.display());
            None
        }
    }

    #[test]
    fn ids_are_wwise_hashes() {
        assert_eq!(fnv1("Play_MUS_0100_0_0"), 0xBAC6_BB73);
        assert_eq!(fnv1("PLAY_mus_0100_0_0"), 0xBAC6_BB73);
        assert_eq!(event_id(0, 0), fnv1("play_mus_9990_0_0"));
        assert_ne!(CARRIER, CUSTOM_TRACK);
        assert!(CARRIER < 0x2710);
    }

    /// An independent walk of a bank: (chunk tag, size) and HIRC objects as (type, id, data).
    fn walk(bank: &[u8]) -> (Vec<(String, usize)>, Vec<(u8, u32, Vec<u8>)>) {
        let (mut tags, mut objs) = (Vec::new(), Vec::new());
        let mut p = 0;
        while p + 8 <= bank.len() {
            let tag = String::from_utf8_lossy(&bank[p..p + 4]).into_owned();
            let sz = u32::from_be_bytes(bank[p + 4..p + 8].try_into().unwrap()) as usize;
            if tag == "HIRC" {
                let n = u32::from_be_bytes(bank[p + 8..p + 12].try_into().unwrap());
                let mut q = p + 12;
                for _ in 0..n {
                    let s = u32::from_be_bytes(bank[q + 1..q + 5].try_into().unwrap()) as usize;
                    let d = bank[q + 5..q + 5 + s].to_vec();
                    objs.push((bank[q], u32::from_be_bytes(d[0..4].try_into().unwrap()), d));
                    q += 5 + s;
                }
                assert_eq!(q, p + 8 + sz, "HIRC objects fill the chunk");
            }
            tags.push((tag, sz));
            p += 8 + sz;
        }
        assert_eq!(p, bank.len(), "chunks fill the bank");
        (tags, objs)
    }

    fn real_bank() -> Option<(Vec<u8>, Vec<u8>, u64)> {
        let path = file("Game_Sound_E_S.pck")?;
        let mut f = std::fs::File::open(&path).unwrap();
        let real = f.metadata().unwrap().len();
        let header = read_header(&mut f).unwrap();
        let bank = read_at(&mut f, BANK_BLOCK as u64 * BLOCK as u64, BANK_SIZE as usize).unwrap();
        Some((header, bank, real))
    }

    #[test]
    fn the_bank_gains_the_carrier_theme() {
        let Some((_, bank, _)) = real_bank() else { return };
        let patched = patch_bank(&bank).unwrap();
        let (old_tags, old) = walk(&bank);
        let (new_tags, new) = walk(&patched);
        assert_eq!(old_tags.iter().map(|t| &t.0).collect::<Vec<_>>(), new_tags.iter().map(|t| &t.0).collect::<Vec<_>>());
        assert_eq!(new.len(), old.len() + 2 + 2 * (MODES * VARIATIONS) as usize);
        let added = new.len() - old.len();
        let hirc_growth = new_tags.iter().find(|t| t.0 == "HIRC").unwrap().1 - old_tags.iter().find(|t| t.0 == "HIRC").unwrap().1;
        assert_eq!(patched.len() - bank.len(), hirc_growth);
        // Every new object's id is new; every old object is kept, in order.
        let old_ids: std::collections::HashSet<u32> = old.iter().map(|o| o.1).collect();
        assert_eq!(new.iter().filter(|o| !old_ids.contains(&o.1)).count(), added);
        let kept: Vec<u32> = new.iter().map(|o| o.1).filter(|id| old_ids.contains(id)).collect();
        assert_eq!(kept, old.iter().map(|o| o.1).collect::<Vec<_>>());
        let by = |id: u32| new.iter().find(|o| o.1 == id).unwrap();
        // The sound: PCM, streamed, our file, under our container.
        let (ty, _, d) = by(sound_id());
        assert_eq!(*ty, TYPE_SOUND);
        assert_eq!(&d[4..12], &[0, 1, 0, 1, 0, 0, 0, 1]);
        assert_eq!(be32(d, 12), stream_id());
        assert_eq!(be32(d, 16), stream_id());
        assert_eq!(be32(d, 27), container_id());
        // The container: under the actor-mixer, playing our sound.
        let (ty, _, d) = by(container_id());
        assert_eq!(*ty, TYPE_RANDOM);
        assert_eq!(be32(d, 10), MIXER);
        let names = (0..d.len() - 3).filter(|&i| be32(d, i) == sound_id()).count();
        assert_eq!(names, 2);
        // Sound and container come right before the actor-mixer.
        let mixer_at = new.iter().position(|o| o.1 == MIXER).unwrap();
        assert_eq!(new[mixer_at - 2].1, sound_id());
        assert_eq!(new[mixer_at - 1].1, container_id());
        // The actor-mixer lists the container among its children, ascending.
        let (old_mixer, new_mixer) = (&old.iter().find(|o| o.1 == MIXER).unwrap().2, &new[mixer_at].2);
        assert_eq!(new_mixer.len(), old_mixer.len() + 4);
        let n = be32(old_mixer, old_mixer.len() - 4 - 4 * 0xA5);
        assert_eq!(n, 0xA5, "the template mixer has 165 children");
        let at = new_mixer.len() - 4 - 4 * 0xA6;
        assert_eq!(be32(new_mixer, at), 0xA6);
        assert_eq!(&new_mixer[..at], &old_mixer[..at]);
        let kids: Vec<u32> = (0..0xA6).map(|i| be32(new_mixer, at + 4 + 4 * i)).collect();
        assert!(kids.windows(2).all(|w| w[0] < w[1]));
        assert!(kids.contains(&container_id()) && kids.contains(&TEMPLATE_CONTAINER));
        // Each event plays the container through its action.
        for m in 0..MODES {
            for v in 0..VARIATIONS {
                let (ty, _, e) = by(event_id(m, v));
                assert_eq!(*ty, TYPE_EVENT);
                assert_eq!(be32(e, 4), 1);
                let (ty, _, a) = by(be32(e, 8));
                assert_eq!(*ty, TYPE_ACTION);
                assert_eq!(be16(a, 4), 0x0403);
                assert_eq!(be32(a, 6), container_id());
                assert_eq!(be32(a, a.len() - 4), BANK_ID);
                assert_eq!(a.len() == 22, v == 2, "variation 2 is the delayed play");
            }
        }
    }

    #[test]
    fn the_bank_table_points_past_the_end() {
        let Some((header, bank, real)) = real_bank() else { return };
        let (new_header, at, patched) = patch_es(&header, &bank, real).unwrap();
        assert_eq!(new_header.len(), header.len());
        let (old, new) = (parse_header(&header).unwrap(), parse_header(&new_header).unwrap());
        assert_eq!(old.banks.len(), new.banks.len());
        for (a, b) in old.banks.iter().zip(&new.banks) {
            if a.id == BANK_ID {
                assert_eq!(b.size as usize, patched.len());
                assert_eq!(b.start as u64 * BLOCK as u64, at);
                assert!(at >= real && at % BLOCK as u64 == 0);
            } else {
                assert_eq!(a, b);
            }
        }
    }

    #[test]
    fn the_stream_table_gains_the_song_sorted() {
        let Some(path) = file("Game_Sound_M.pck") else { return };
        let mut f = std::fs::File::open(&path).unwrap();
        let real = f.metadata().unwrap().len();
        let header = read_header(&mut f).unwrap();
        let (new_header, at) = patch_m(&header, real).unwrap();
        assert_eq!(new_header.len(), header.len() + 20);
        let (old, new) = (parse_header(&header).unwrap(), parse_header(&new_header).unwrap());
        assert_eq!(new.streams.len(), old.streams.len() + 1);
        assert!(new.streams.windows(2).all(|w| w[0].id < w[1].id), "sorted");
        let e = new.streams.iter().find(|e| e.id == stream_id()).unwrap();
        assert_eq!((e.block, e.size, e.lang), (BLOCK, STREAM_SIZE, 0));
        assert_eq!(e.start as u64 * BLOCK as u64, at);
        assert!(at >= real);
        assert_eq!(old.banks, new.banks);
        assert_eq!(old.lang_map, new.lang_map);
        assert_eq!(old.externals, new.externals);
        let first = old.streams.iter().map(|e| e.start * e.block).min().unwrap();
        assert!(new_header.len() as u32 <= first);
    }

    #[test]
    fn the_wave_header_is_what_the_pcm_source_reads() {
        let h = wav_header(48_000);
        assert_eq!(&h[0..4], b"RIFX");
        assert_eq!(be32(&h, 4) as usize, WAV_HEADER - 8 + 192_000);
        assert_eq!(&h[8..16], b"WAVEfmt ");
        assert_eq!(be32(&h, 16), 24);
        let fmt = &h[20..44];
        assert_eq!(be16(fmt, 0), 0xFFFE);
        assert_eq!(be16(fmt, 2), 2);
        assert_eq!(be32(fmt, 4), 48_000);
        assert_eq!(be32(fmt, 8), 192_000);
        assert_eq!(be16(fmt, 0xC), 4);
        assert_eq!(be16(fmt, 0xE), 16);
        assert_eq!(be32(fmt, 0x14), 3);
        assert_eq!(&h[44..48], b"data");
        assert_eq!(be32(&h, 48), 192_000);
    }

    #[test]
    fn the_stream_serves_header_then_big_endian_samples() {
        let pcm = Pcm::new("tone", None);
        assert!(pcm.push(&[0.5, -0.5, 1.0, -1.0]));
        pcm.finish();
        let mut b = vec![0xAAu8; WAV_HEADER + 10];
        pcm.read(0, &mut b);
        assert_eq!(&b[..WAV_HEADER], &wav_header(2));
        assert_eq!(&b[WAV_HEADER..WAV_HEADER + 8], &[0x40, 0x00, 0xC0, 0x00, 0x7F, 0xFF, 0x80, 0x01]);
        assert_eq!(&b[WAV_HEADER + 8..], &[0, 0]);
        // A read in the middle of a sample.
        let mut b = [0u8; 3];
        pcm.read(WAV_HEADER as u64 + 1, &mut b);
        assert_eq!(b, [0x00, 0xC0, 0x00]);
    }

    #[test]
    fn overlays_lay_bytes_over_reads() {
        let o = Overlay {
            segments: vec![Segment { start: 4, len: 4, content: Content::Bytes(vec![1, 2, 3, 4]), what: "test".into(), served: AtomicBool::new(false) }],
        };
        assert_eq!(o.size(2), 8);
        assert_eq!(o.size(100), 100);
        let mut b = [9u8; 6];
        o.apply(2, &mut b);
        assert_eq!(b, [9, 9, 1, 2, 3, 4]);
        let mut b = [9u8; 2];
        o.apply(7, &mut b);
        assert_eq!(b, [4, 9]);
    }
}

/// `sub_82ACE130(player, request, name, a, b, c, channel)`: the music
/// player's request queue. In a match the entrance asks for its custom song
/// here directly ({+0 kind 0x270E, +8 title}, a nonzero channel), not through
/// `sub_8334C138` (which only Create an Entrance's preview uses: user90-91
/// heard the song dry and the request hook never ran). The Jukebox's own
/// requests come with channel 0 and stay on the native player. A custom
/// request on another channel becomes theme 9990 ({kind 0, id 9990}), which
/// plays through Wwise with the entrance reverb.
pub extern "C-unwind" fn music_job(c: &mut Ctx, b: *mut u8) {
    let (request, channel) = (c.r[4] as u32, c.r[9] as u32);
    static CALLS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if request != 0 && CALLS.fetch_add(1, Ordering::Relaxed) < 200 {
        crate::diag::log(&format!("music: job: kind {:#x} id {:#x}, args {:#x} {:#x} {:#x}, channel {channel:#x} [from 0x{:08X}]", mem::r32(request), mem::r32(request + 4), c.r[6] as u32, c.r[7] as u32, c.r[8] as u32, c.lr as u32));
    }
    if enabled() && request != 0 && channel != 0 && mem::r32(request) == CUSTOM_TRACK {
        let title = String::from_utf16_lossy(&mem::wstr(request + 8, 40));
        if !BANK_SERVED.load(Ordering::Relaxed) || !TABLE_SERVED.load(Ordering::Relaxed) {
            crate::diag::log(&format!("music: entrance song \"{title}\" on the native player: the game hasn't read the added theme yet"));
        } else {
            match find_song(&title) {
                Some(song) => {
                    select(&song);
                    mem::w32(request, 0);
                    mem::w32(request + 4, CARRIER);
                    crate::diag::log(&format!("music: entrance song \"{}\" through Wwise as theme {CARRIER} (channel {channel:#x})", song.title));
                }
                None => crate::diag::log(&format!("music: entrance song \"{title}\" is not in the music folder; left to the native player")),
            }
        }
    }
    unsafe { ppc_rt::original(0x82AC_E130).expect("the music job queue is replaced")(c, b) };
}
