//! `xam.xex` exports: the system library's API contract for one offline
//! console with one local profile signed in.
//!
//! Exports not defined here have no implementation yet: calling one stops the
//! game and names it.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use ppc_rt::Ctx;

use crate::abi::{arg, arg64, exports, out32, ret};
use crate::loader::module;
use crate::mem;
use crate::object::{self, Object};
use crate::status::*;
use crate::thread::{self, Apc};
use crate::{diag, video};

/// The one signed-in profile.
pub const XUID: u64 = 0xE000_0000_1234_5678;
pub const GAMERTAG: &str = "Player";

/// `XamGetSystemVersion` / `XboxKrnlVersion`: 2.0.17559.0.
const SYSTEM_VERSION: u32 = (2 << 28) | (17559 << 8);

/// Completes an `XOVERLAPPED { InternalLow (result), InternalHigh (length),
/// InternalContext, hEvent, pCompletionRoutine, dwCompletionContext,
/// dwExtendedError }`: sets its event and queues its completion routine.
pub fn complete_overlapped(ov: u32, result: u32, length: u32, extended: u32) {
    mem::w32(ov, result);
    mem::w32(ov + 4, length);
    mem::w32(ov + 0x18, extended);
    let event = mem::r32(ov + 0xC);
    if event != 0 && let Some(o) = object::get(event).and_then(|o| o.waitable()) {
        object::set_event(o);
    }
    let routine = mem::r32(ov + 0x10);
    if routine != 0 {
        thread::current().queue_apc(Apc { routine, context: result, arg1: length, arg2: ov });
    }
}

/// Returns a XAM result: synchronously, or through the overlapped structure
/// (then the call itself returns `ERROR_IO_PENDING`).
fn finish(c: &mut Ctx, ov: u32, result: u32, length: u32) {
    if ov != 0 {
        complete_overlapped(ov, result, length, result);
        ret(c, ERROR_IO_PENDING);
    } else {
        ret(c, result);
    }
}

/// The hard disk's content device id.
const HDD_DEVICE: u32 = 1;
const ERROR_DEVICE_NOT_CONNECTED: u32 = 1167;

/// Bytes of an `XCONTENT_DATA`: device id, content type, display name (128
/// UTF-16 units at +8), file name (42 bytes at +0x108), padded to a word.
const XCONTENT_DATA_SIZE: u32 = 0x134;

/// The host folder holding the packages of one content type:
/// `Content\<xuid>\<title id>\<content type>` on the hard disk, with a zero
/// XUID for content not tied to a profile.
fn content_type_dir(ty: u32) -> Option<std::path::PathBuf> {
    let xuid = if ty == 1 { XUID } else { 0 };
    let root = crate::file::resolve("\\Device\\Harddisk0\\Partition1")?.host;
    Some(root.join("Content").join(format!("{xuid:016X}")).join(format!("{:08X}", module().title_id)).join(format!("{ty:08X}")))
}

/// The host folder holding a content package (`XCONTENT_DATA`): its file name
/// under its content type's folder.
fn content_dir(data: u32) -> Option<std::path::PathBuf> {
    if mem::r32(data) != HDD_DEVICE {
        return None;
    }
    let name = mem::latin1(&mem::cstr(data + 0x108, 42));
    if name.is_empty() || name.contains(['\\', '/', ':']) || name.contains("..") {
        return None;
    }
    Some(content_type_dir(mem::r32(data + 4))?.join(name))
}

/// Where a package's display name is kept: a text file beside its folder.
/// The console keeps it in the package's header; here a package is a plain
/// folder, and a file inside it would show up among the title's own.
fn display_name_file(dir: &std::path::Path) -> std::path::PathBuf {
    let mut name = dir.file_name().unwrap_or_default().to_os_string();
    name.push(".displayname");
    dir.with_file_name(name)
}

/// The packages in a content type's folder, as (file name, display name),
/// ordered by file name. A package made before display names were kept shows
/// its file name.
fn packages_in(dir: &std::path::Path) -> Vec<(String, String)> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<(String, String)> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        // Only names the 42-byte field holds: nothing else is a package a title
        // made. Marketplace content names fill it exactly (42 characters, no
        // terminator); `< 42` hid every downloadable pack (user78).
        .filter(|name| name.len() <= 42 && name.is_ascii())
        .map(|name| {
            let display = std::fs::read_to_string(display_name_file(&dir.join(&name))).ok().filter(|s| !s.is_empty());
            (name.clone(), display.unwrap_or(name))
        })
        .collect();
    out.sort();
    out
}

/// One `XCONTENT_DATA` for the hard disk, as the guest reads it (big-endian).
fn content_item(ty: u32, display: &str, file: &str) -> Vec<u8> {
    let mut d = vec![0u8; XCONTENT_DATA_SIZE as usize];
    d[0..4].copy_from_slice(&HDD_DEVICE.to_be_bytes());
    d[4..8].copy_from_slice(&ty.to_be_bytes());
    for (i, unit) in display.encode_utf16().take(127).enumerate() {
        d[8 + 2 * i..10 + 2 * i].copy_from_slice(&unit.to_be_bytes());
    }
    for (i, b) in file.bytes().take(42).enumerate() {
        d[0x108 + i] = b;
    }
    d
}

fn signed_in(user: u32) -> bool {
    user == 0
}

/// A fresh profile's value for a setting: `(source, value)`. Source 1 =
/// `XSOURCE_DEFAULT`; 0 = no value (the title's own settings, which a new
/// profile doesn't have).
fn profile_setting(title: u32, id: u32) -> (u32, u64) {
    let v = match id {
        0x1004_0001 => 0,  // gamer type
        0x1004_0002 => 0,  // Y axis inversion: off
        0x1004_0003 => 3,  // controller vibration: on
        0x1004_0004..=0x1004_0008 => 0, // gamercard zone, region, cred, presence, vision
        0x5004_000B => 0,  // reputation (float 0.0)
        0x1004_000C => 0,  // voice muted
        0x1004_000D => 0,  // voice through speakers
        0x1004_000E => 0x64, // voice volume
        0x4064_000F | 0x4064_0010 | 0x402C_0011 => 0, // picture keys, motto (empty strings)
        0x1004_0012 | 0x1004_0013 => 0, // titles played, achievements earned
        0x1004_0015 => 0,  // difficulty: normal
        0x1004_0018 => 0,  // control sensitivity: medium
        0x1004_001D | 0x1004_001E => 0, // preferred colours
        0x1004_0022 => 1,  // auto aim: on
        0x1004_0023 => 0,  // auto centre: off
        0x1004_0024 => 1,  // movement control: left stick
        0x1004_0026..=0x1004_0029 => 0, // racing: automatic, behind, trigger, trigger
        0x1004_0038 | 0x1004_0039 => 0, // title cred / achievements earned
        // Title-specific settings: a new profile has none.
        0x63E8_3FFF | 0x63E8_3FFE | 0x63E8_3FFD => return (0, 0),
        _ => diag::fatal(&format!(
            "XamUserReadProfileSettings: no default for setting 0x{id:08X} (title {title:08X}); add it deliberately"
        )),
    };
    (1, v)
}

// ---- Notifications ------------------------------------------------------------------

/// A notification listener. Titles wait on its handle: on the console it is
/// signalled while notifications are queued, so `event` (a notification
/// event) is set whenever the queue is non-empty and reset when
/// `XNotifyGetNext` empties it.
struct Listener {
    mask: u64,
    queue: Mutex<VecDeque<(u32, u32)>>,
    event: u32,
}

impl Listener {
    fn push(&self, items: impl IntoIterator<Item = (u32, u32)>) {
        let mut q = self.queue.lock().unwrap();
        q.extend(items);
        if !q.is_empty() {
            object::set_event(self.event);
        }
    }
}

static LISTENERS: Mutex<Vec<Arc<Listener>>> = Mutex::new(Vec::new());
static STARTUP_SENT: Mutex<bool> = Mutex::new(false);

const XN_SYS_UI: u32 = 0x0000_0009;
const XN_SYS_SIGNINCHANGED: u32 = 0x0000_000A;

fn area_bit(id: u32) -> u64 {
    1 << ((id >> 25) & 0x3F)
}

/// Sends a notification to every listener whose mask covers its area.
pub fn notify(id: u32, param: u32) {
    for l in LISTENERS.lock().unwrap().iter() {
        if l.mask & area_bit(id) != 0 {
            l.push([(id, param)]);
        }
    }
}

/// An Xbox LIVE panel the title asked the system to show (see the exports):
/// it comes and goes with nothing changed.
fn live_panel(c: &mut Ctx, name: &str) {
    diag::log(&format!(
        "{name}(0x{:X}, 0x{:X}, 0x{:X}, 0x{:X}): there is no Xbox LIVE; the panel closes at once [from 0x{:08X}]",
        arg(c, 0),
        arg(c, 1),
        arg(c, 2),
        arg(c, 3),
        c.lr as u32
    ));
    notify(XN_SYS_UI, 1);
    notify(XN_SYS_UI, 0);
    ret(c, ERROR_SUCCESS);
}

// ---- The on-screen keyboard ---------------------------------------------------------
//
// `XamShowKeyboardUI` opens the console's keyboard over the title: the game
// uses it for every name the player types (a created superstar, a show, a
// title belt). It is a system service, so it is native here: the request
// waits in `KEYBOARD`, the host shows a prompt and takes the text from the
// PC's keyboard (`keyboard_request`, `keyboard_finish`), and the title's
// overlapped request completes with the text or as cancelled. While it is
// up the title is told the system UI is showing and its controller reads as
// untouched, as on the console.

/// What the title asked the keyboard for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyboardRequest {
    pub title: String,
    pub description: String,
    /// The text the field starts with.
    pub text: String,
    /// The most characters the title's buffer takes, its terminator not counted.
    pub max_chars: usize,
}

struct KeyboardPending {
    request: KeyboardRequest,
    /// The title's result buffer (UTF-16) and its overlapped structure.
    buffer: u32,
    overlapped: u32,
}

static KEYBOARD: Mutex<Option<KeyboardPending>> = Mutex::new(None);
/// A host that shows the prompt is attached (the window). Without one
/// (scripted runs with no window) a request is answered with its own
/// starting text at once.
static KEYBOARD_HOST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The host shows keyboard prompts (called once by the window).
pub fn keyboard_host_attached() {
    KEYBOARD_HOST.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// The keyboard request waiting for an answer, if any.
pub fn keyboard_request() -> Option<KeyboardRequest> {
    KEYBOARD.lock().unwrap().as_ref().map(|p| p.request.clone())
}

/// Answers the waiting request: the text the player entered, or `None` if
/// they cancelled. The text is cut to what the title's buffer holds.
pub fn keyboard_finish(text: Option<&str>) {
    let Some(p) = KEYBOARD.lock().unwrap().take() else { return };
    let result = match text {
        Some(t) => {
            let units: Vec<u16> = keyboard_units(t, p.request.max_chars);
            for (i, unit) in units.iter().chain([&0]).enumerate() {
                mem::w16(p.buffer + 2 * i as u32, *unit);
            }
            diag::log(&format!("XamShowKeyboardUI: the player entered \"{}\"", String::from_utf16_lossy(&units)));
            ERROR_SUCCESS
        }
        None => {
            diag::log("XamShowKeyboardUI: the player cancelled");
            ERROR_CANCELLED
        }
    };
    KEYBOARD_RELEASE.store(true, std::sync::atomic::Ordering::Relaxed);
    complete_overlapped(p.overlapped, result, 0, result);
    notify(XN_SYS_UI, 0);
}

/// Set when the keyboard closes: the controller keeps reading as untouched
/// until every button is up. The press that answered the keyboard (A, B,
/// or Enter and Escape through the key mapping) belongs to the system; seen
/// by the title it would act on whatever is under the cursor, usually the
/// name field again.
static KEYBOARD_RELEASE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Controller 1 as the title may see it now: the host's state, or untouched
/// while the system keyboard has the input.
fn host_pad_for_title() -> Pad {
    use std::sync::atomic::Ordering;
    if KEYBOARD.lock().unwrap().is_some() {
        return Pad::default();
    }
    let pad = *HOST_PAD.lock().unwrap();
    if KEYBOARD_RELEASE.load(Ordering::Relaxed) {
        if pad.buttons != 0 {
            return Pad::default();
        }
        KEYBOARD_RELEASE.store(false, Ordering::Relaxed);
    }
    pad
}

/// `text` as the UTF-16 the title's buffer takes: at most `max_chars` units,
/// never ending in half of a surrogate pair.
fn keyboard_units(text: &str, max_chars: usize) -> Vec<u16> {
    let mut units: Vec<u16> = text.encode_utf16().take(max_chars).collect();
    if units.last().is_some_and(|u| (0xD800..0xDC00).contains(u)) {
        units.pop();
    }
    units
}

// ---- Enumerators -------------------------------------------------------------------

/// A XAM enumerator: items of one size, handed out a batch at a time by
/// `XamEnumerate`.
struct Enumerator {
    item_size: u32,
    /// The most items one call returns (the title's count at creation).
    per_call: u32,
    items: Vec<Vec<u8>>,
    next: std::sync::atomic::AtomicUsize,
    /// Guest memory for `XamGetPrivateEnumStructureFromHandle` (0 for none):
    /// a 0x18-byte header, then the extra bytes asked for at creation.
    private: u32,
}

// ---- Music player items -----------------------------------------------------------------

/// A song as the music player lists it (0x334 bytes). The game reads only the
/// title at +0x23C (40 wide characters), finds songs by it, and hands the
/// whole item back to play one. Here +0 holds the song's id (its index in
/// `music::library` plus one); artist, album, track and length follow the
/// title in the console's song-info order (not read by the game).
const MUSIC_ITEM_SIZE: u32 = 0x334;
const MUSIC_TITLE: u32 = 0x23C;

fn music_item(id: u32, s: &crate::music::Song) -> Vec<u8> {
    let mut b = vec![0u8; MUSIC_ITEM_SIZE as usize];
    b[0..4].copy_from_slice(&id.to_be_bytes());
    let mut put = |at: usize, text: &str| {
        for (i, ch) in text.encode_utf16().take(39).enumerate() {
            b[at + 2 * i..at + 2 * i + 2].copy_from_slice(&ch.to_be_bytes());
        }
    };
    put(MUSIC_TITLE as usize, &s.title);
    put(0x28C, &s.artist);
    put(0x2DC, &s.album);
    b[0x32C..0x330].copy_from_slice(&s.track.to_be_bytes());
    b[0x330..0x334].copy_from_slice(&s.duration_ms.to_be_bytes());
    b
}

/// A title as it fits the game's 40-character field (39 and the terminator).
fn clip40(s: &str) -> String {
    String::from_utf16_lossy(&s.encode_utf16().take(39).collect::<Vec<_>>())
}

/// A big-endian wide string of at most `max` characters at `a`.
fn wide_at(a: u32, max: usize) -> String {
    String::from_utf16_lossy(&mem::wstr(a, max))
}

/// Writes `text` as a terminated big-endian wide string in a field of `max` characters.
fn put_wide(a: u32, text: &str, max: usize) {
    let units: Vec<u16> = text.encode_utf16().take(max - 1).chain([0]).collect();
    for (i, u) in units.iter().enumerate() {
        mem::w16(a + 2 * i as u32, *u);
    }
}

// ---- System app messages ----------------------------------------------------------------
//
// XAPI reaches several XAM services by message: `XMsgStartIORequest(app,
// message, overlapped, buffer, size)` and `XMsgInProcessCall(app, message,
// p1, p2)`. Each (app, message) pair is implemented here deliberately; an
// unknown one stops with its numbers.

/// Content packages open now, by the root name the title gave them.
static OPEN_PACKAGES: Mutex<Vec<(String, std::path::PathBuf)>> = Mutex::new(Vec::new());

thread_local! {
    /// The message being handled came through `XMsgStartIORequest` without an
    /// overlapped structure (the caller waits). The Jukebox's player plays its
    /// songs that way; entrance music (Create an Entrance's preview, matches)
    /// through an overlapped request.
    static MESSAGE_WAITED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Rich-presence contexts and properties set by the title, per (user, id).
static USER_CONTEXTS: Mutex<std::collections::BTreeMap<(u32, u32), u32>> = Mutex::new(std::collections::BTreeMap::new());
static USER_PROPERTIES: Mutex<std::collections::BTreeMap<(u32, u32), Vec<u8>>> =
    Mutex::new(std::collections::BTreeMap::new());

/// Handles one message; returns its result (an HRESULT / Win32 code).
fn app_message(app: u32, message: u32, buffer: u32, size: u32) -> u32 {
    match (app, message) {
        // XAM app: title state query used by XAPI at startup. The first word
        // of the buffer points at the answer: 0 is the ordinary state (later
        // code treats 2 as a special mode that unlocks content).
        (0xFE, 0x0002_2005) if size >= 4 => {
            out32(mem::r32(buffer), 0);
            ERROR_SUCCESS
        }
        // XMP app (music player). XMPGetPlaybackController:
        // { u32 client; u32 *controller; BOOL *locked; }. No custom soundtrack is
        // playing: the title controls playback (0) and it isn't locked. XAPI's
        // XMPTitleHasPlaybackControl is exactly `controller == 0 && !locked`.
        // XMPSetPlaybackController: { u32 client; u32 controller; u32 locked; }.
        // 30 Years of WrestleMania takes playback for the title (user39:
        // client 2, controller 0, locked 1) before its own soundtrack. Background
        // music (`music::title_control`) pauses while the title holds control.
        (0xFA, 0x0007_001A) if size >= 12 => {
            diag::log(&format!(
                "XMPSetPlaybackController: client {}, controller {}, locked {}",
                mem::r32(buffer),
                mem::r32(buffer + 4),
                mem::r32(buffer + 8)
            ));
            // Controller 0 locked: the title takes control (XMPOverrideBackgroundMusic);
            // anything else gives it back (XMPRestoreBackgroundMusic sends 0 or 4, unlocked).
            crate::music::title_control(mem::r32(buffer + 4) == 0 && mem::r32(buffer + 8) != 0);
            ERROR_SUCCESS
        }
        // ---- The music player (custom soundtracks, `kernel::music`). Message
        // blocks start with the client (2, the title). Found statically in
        // XAPI 0x82E1A0B8-0x82E1AA10 and the game's player object (vtable
        // 0x82060EC8) and music browser (0x8334B638), 2026-11-25.
        //
        // "Start listing" for an enumerator made by XamCreateEnumeratorHandle:
        // { u32 2; u32 user; void *request; }. The songs were read when the
        // enumerator was made.
        (0xFA, 0x0007_0025) => ERROR_SUCCESS,
        // Play: { u32 client; item[0x334] } with an item the listing returned.
        // The game finds songs by title (the only thing its saves keep), so
        // the title decides, and the id stored in the item only breaks ties.
        (0xFA, 0x0007_003B) => {
            let item = buffer + 4;
            let mut id = mem::r32(item);
            let mut title = wide_at(item + MUSIC_TITLE, 40);
            // The Jukebox's player waits on its request. A song it plays that
            // its pick didn't name (the first one after the save loads, which
            // the game takes straight from the save: the same song every
            // session, user60-69) is shuffled like the rest.
            // Only the session's first: later ones the pick didn't name are
            // the User Playlist's previews, which play the song asked for.
            static FIRST_DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            let jukebox = MESSAGE_WAITED.get();
            let named = crate::music::take_jukebox_next().is_some();
            let first = jukebox && !FIRST_DONE.swap(true, std::sync::atomic::Ordering::SeqCst);
            if first
                && !named
                && let Some(shuffled) = crate::jukebox::next_title()
            {
                title = shuffled;
                id = 0;
            }
            let lib = crate::music::library();
            let pick = lib
                .iter()
                .enumerate()
                .filter(|(_, s)| clip40(&s.title) == title)
                .min_by_key(|(i, _)| (*i as u32 + 1 != id) as u8)
                .map(|(i, _)| i as u32 + 1)
                .or_else(|| (id != 0 && crate::music::song(id).is_some()).then_some(id));
            match pick {
                Some(id) if crate::music::play(id) => {
                    crate::music::set_jukebox_song(jukebox);
                    diag::log(&format!("music player: \"{title}\" is {} song", if jukebox { "the Jukebox's" } else { "an entrance (or other overlapped)" }));
                    ERROR_SUCCESS
                }
                _ => {
                    diag::log(&format!("music player: play \"{title}\": no such song in {}", crate::music::folder().display()));
                    ERROR_FILE_NOT_FOUND
                }
            }
        }
        // Continue, pause, stop: { u32 client; ... }.
        (0xFA, 0x0007_0003) => {
            diag::log("music player: the game asks to continue");
            crate::music::title_resume();
            ERROR_SUCCESS
        }
        (0xFA, 0x0007_0005) => {
            diag::log("music player: the game asks to pause");
            crate::music::title_pause();
            ERROR_SUCCESS
        }
        (0xFA, 0x0007_0004) => {
            diag::log("music player: the game asks to stop");
            crate::music::title_stop();
            ERROR_SUCCESS
        }
        // Playback behaviour { client, mode, repeat, flags }: the game sends
        // (1, 1, 0) after a play. Songs play once here.
        (0xFA, 0x0007_0008) => ERROR_SUCCESS,
        // Status { client; u32 *state }: 0 idle, 1 playing, 2 paused.
        (0xFA, 0x0007_0009) => {
            let state = match crate::music::state().0 {
                crate::music::State::Idle => 0,
                crate::music::State::Playing => 1,
                crate::music::State::Paused => 2,
            };
            out32(mem::r32(buffer + 4), state);
            ERROR_SUCCESS
        }
        // Volume { client; float *volume } and { client; float volume }.
        (0xFA, 0x0007_000B) => {
            out32(mem::r32(buffer + 4), crate::music::volume().to_bits());
            ERROR_SUCCESS
        }
        (0xFA, 0x0007_000C) => {
            let v = f32::from_bits(mem::r32(buffer + 4));
            diag::log(&format!("music player: the game sets the volume to {v:.3}"));
            crate::music::set_volume(v);
            ERROR_SUCCESS
        }
        // The current song { client; u32 0; info *out }: a 0x3E0-byte record
        // whose title, artist and album the game shows (40 wide characters
        // each at +0x240, +0x290, +0x2E0).
        (0xFA, 0x0007_000E) => {
            let out = mem::r32(buffer + 8);
            let (state, id) = crate::music::state();
            match crate::music::song(id).filter(|_| state != crate::music::State::Idle) {
                Some(s) => {
                    mem::fill(out, 0x3E0, 0);
                    mem::w32(out, id);
                    put_wide(out + 0x240, &s.title, 40);
                    put_wide(out + 0x290, &s.artist, 40);
                    put_wide(out + 0x2E0, &s.album, 40);
                    ERROR_SUCCESS
                }
                None => ERROR_NOT_FOUND,
            }
        }
        (0xFA, 0x0007_001B) => {
            // While the user's music plays over the title, the controller is the
            // system (4, what XMPRestoreBackgroundMusic hands back), so the
            // title's own music stays quiet, as on the console.
            out32(mem::r32(buffer + 4), if crate::music::user_has_control() { 4 } else { 0 });
            out32(mem::r32(buffer + 8), 0);
            ERROR_SUCCESS
        }
        // XGI app: Live title services. Offline, the console accepts and keeps them.
        // XUserSetContextEx: { u32 user; u32; u64 0; u32 context id; u32 value; }
        (0xFB, 0x000B_0006) if size >= 0x18 => {
            let (user, id, value) = (mem::r32(buffer), mem::r32(buffer + 0x10), mem::r32(buffer + 0x14));
            USER_CONTEXTS.lock().unwrap().insert((user, id), value);
            ERROR_SUCCESS
        }
        // XUserSetPropertyEx: { u32 user; u32; u64 0; u32 property id; u32 size; void *value; }
        (0xFB, 0x000B_0007) if size >= 0x1C => {
            let (user, id, n, p) = (mem::r32(buffer), mem::r32(buffer + 0x10), mem::r32(buffer + 0x14), mem::r32(buffer + 0x18));
            USER_PROPERTIES.lock().unwrap().insert((user, id), mem::read(p, n.min(0x1000) as usize));
            ERROR_SUCCESS
        }
        // XUserWriteAchievements: { u32 count; XUSER_ACHIEVEMENT *list; } where each is { u32 user; u32 id; }.
        (0xFB, 0x000B_0008) if size >= 8 => {
            let (n, list) = (mem::r32(buffer), mem::r32(buffer + 4));
            for i in 0..n.min(64) {
                diag::log(&format!("achievement {} unlocked (user {})", mem::r32(list + 8 * i + 4), mem::r32(list + 8 * i)));
            }
            ERROR_SUCCESS
        }
        _ => diag::fatal(&format!(
            "XAM message 0x{message:08X} to app 0x{app:02X} (buffer 0x{buffer:08X}, {size} bytes) is not implemented yet"
        )),
    }
}

// ---- Launch data ----------------------------------------------------------------------

static LAUNCH_DATA: Mutex<Option<Vec<u8>>> = Mutex::new(None);

exports! {
    // ---- Memory

    XamAlloc => |c| {
        let (flags, size, out) = (arg(c, 0), arg(c, 1), arg(c, 2));
        if flags & 0x0010_0000 != 0 {
            diag::fatal(&format!("XamAlloc flags 0x{flags:08X}: physical XAM allocations are not implemented"));
        }
        out32(out, mem::kalloc(size, 16));
        ret(c, ERROR_SUCCESS);
    }
    XamFree => |c| {
        mem::kfree(arg(c, 0));
        ret(c, ERROR_SUCCESS);
    }

    // ---- System information

    XamGetSystemVersion => |c| { ret(c, SYSTEM_VERSION); }
    XamGetLanguage => |c| { ret(c, 1); }
    XGetGameRegion => |c| { ret(c, 0xFFFF); }
    XGetAVPack => |c| { ret(c, 6); }
    XGetVideoMode => |c| { video::write_video_mode(arg(c, 0)); }
    XamGetExecutionId => |c| {
        out32(arg(c, 0), module().execution_id);
        ret(c, ERROR_SUCCESS);
    }
    XNetLogonGetTitleID => |c| { ret(c, module().title_id); }

    // ---- Loader

    XamLoaderGetLaunchDataSize => |c| {
        let p = arg(c, 0);
        match &*LAUNCH_DATA.lock().unwrap() {
            Some(d) => {
                out32(p, d.len() as u32);
                ret(c, ERROR_SUCCESS);
            }
            None => {
                out32(p, 0);
                ret(c, ERROR_NOT_FOUND);
            }
        }
    }
    XamLoaderGetLaunchData => |c| {
        let (buf, size) = (arg(c, 0), arg(c, 1));
        match &*LAUNCH_DATA.lock().unwrap() {
            Some(d) => {
                let n = d.len().min(size as usize);
                mem::write(buf, &d[..n]);
                ret(c, ERROR_SUCCESS);
            }
            None => ret(c, ERROR_NOT_FOUND),
        }
    }
    XamLoaderSetLaunchData => |c| {
        let (buf, size) = (arg(c, 0), arg(c, 1));
        *LAUNCH_DATA.lock().unwrap() = Some(mem::read(buf, size as usize));
        ret(c, ERROR_SUCCESS);
    }
    XamLoaderTerminateTitle => |c| {
        diag::log("XamLoaderTerminateTitle: the game exited");
        std::process::exit(0);
    }
    XamLoaderLaunchTitle => |c| {
        let path = mem::latin1(&mem::cstr(arg(c, 0), 260));
        diag::fatal(&format!("XamLoaderLaunchTitle(\"{path}\", 0x{:X}): relaunching is not implemented yet", arg(c, 1)))
    }

    // ---- Users

    XamUserGetSigninState => |c| { ret(c, signed_in(arg(c, 0)) as u32); }
    XamUserGetXUID => |c| {
        let (user, out) = (arg(c, 0), arg(c, 2));
        if !signed_in(user) {
            return ret(c, ERROR_NO_SUCH_USER);
        }
        if out != 0 {
            mem::w64(out, XUID);
        }
        ret(c, ERROR_SUCCESS);
    }
    XamUserGetName => |c| {
        let (user, buf, size) = (arg(c, 0), arg(c, 1), arg(c, 2));
        if !signed_in(user) {
            return ret(c, ERROR_NO_SUCH_USER);
        }
        let name = GAMERTAG.as_bytes();
        let n = name.len().min(size.saturating_sub(1) as usize);
        mem::write(buf, &name[..n]);
        mem::w8(buf + n as u32, 0);
        ret(c, ERROR_SUCCESS);
    }
    XamUserGetSigninInfo => |c| {
        let (user, out) = (arg(c, 0), arg(c, 2));
        mem::fill(out, 0x28, 0);
        if !signed_in(user) {
            return ret(c, ERROR_NO_SUCH_USER);
        }
        mem::w64(out, XUID);
        mem::w32(out + 8, 0);
        mem::w32(out + 0xC, 1); // eXUserSigninState_SignedInLocally
        mem::w32(out + 0x10, 0);
        mem::w32(out + 0x14, u32::MAX);
        mem::write(out + 0x18, GAMERTAG.as_bytes());
        ret(c, ERROR_SUCCESS);
    }
    XamUserCheckPrivilege => |c| {
        let (user, out) = (arg(c, 0), arg(c, 2));
        if !signed_in(user) {
            return ret(c, ERROR_NOT_LOGGED_ON);
        }
        out32(out, 1);
        ret(c, ERROR_SUCCESS);
    }
    XamUserReadProfileSettings => |c| {
        let (title, user, xuid_count, xuids, count, ids, size_ptr, buf, ov) =
            (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4), arg(c, 5), arg(c, 6), arg(c, 7), arg(c, 8));
        if xuid_count > 1 || (xuid_count == 1 && mem::r64(xuids) != XUID) || (xuid_count == 0 && !signed_in(user)) {
            return finish(c, ov, ERROR_NO_SUCH_USER, 0);
        }
        let ids: Vec<u32> = (0..count).map(|i| mem::r32(ids + 4 * i)).collect();
        // XUSER_READ_PROFILE_SETTING_RESULT { u32 count; XUSER_PROFILE_SETTING *settings; }
        // then the settings (0x28 bytes each), then any string data.
        let strings: u32 = ids.iter().map(|&id| if id >> 28 == 4 { 2 } else { 0 }).sum();
        let need = 8 + 0x28 * count + strings;
        let have = if size_ptr != 0 { mem::r32(size_ptr) } else { 0 };
        if buf == 0 || have < need {
            out32(size_ptr, need);
            return ret(c, ERROR_INSUFFICIENT_BUFFER);
        }
        mem::fill(buf, need as usize, 0);
        mem::w32(buf, count);
        mem::w32(buf + 4, buf + 8);
        let mut extra = buf + 8 + 0x28 * count;
        for (i, &id) in ids.iter().enumerate() {
            let s = buf + 8 + 0x28 * i as u32;
            let (source, value) = profile_setting(title, id);
            mem::w32(s, source);
            if xuid_count == 1 { mem::w64(s + 8, XUID) } else { mem::w32(s + 8, user) }
            mem::w32(s + 0x10, id);
            mem::w8(s + 0x18, (id >> 28) as u8);
            match id >> 28 {
                1 | 5 => mem::w32(s + 0x20, value as u32),
                2 | 3 | 7 => mem::w64(s + 0x20, value),
                4 => {
                    // An empty string: { u32 cbData; wchar_t *pwszData; }
                    mem::w32(s + 0x20, 2);
                    mem::w32(s + 0x24, extra);
                    extra += 2;
                }
                6 => {} // binary: { cbData = 0, pbData = null }
                t => diag::fatal(&format!("XamUserReadProfileSettings: setting 0x{id:08X} has unknown data type {t}")),
            }
        }
        finish(c, ov, ERROR_SUCCESS, 0);
    }
    XamUserGetDeviceContext => |c| {
        let (user, out) = (arg(c, 0), arg(c, 2));
        if !signed_in(user) {
            return ret(c, ERROR_NO_SUCH_USER);
        }
        out32(out, 0);
        ret(c, ERROR_SUCCESS);
    }

    // ---- Input (a controller is connected on user 0; host input comes with the Bevy host)

    XamInputGetCapabilities => |c| {
        let (user, out) = (arg(c, 0), arg(c, 2));
        if !pad_connected(user) {
            return ret(c, ERROR_DEVICE_NOT_CONNECTED);
        }
        mem::fill(out, 0x14, 0);
        mem::w8(out, 1); // XINPUT_DEVTYPE_GAMEPAD
        mem::w8(out + 1, 1); // XINPUT_DEVSUBTYPE_GAMEPAD
        mem::w16(out + 4, 0xF3FF);
        mem::w8(out + 6, 0xFF);
        mem::w8(out + 7, 0xFF);
        for k in 0..4 {
            mem::w16(out + 8 + 2 * k, 0xFFC0);
        }
        mem::w16(out + 0x10, 0xFFFF);
        mem::w16(out + 0x12, 0xFFFF);
        ret(c, ERROR_SUCCESS);
    }
    XamInputGetState => |c| {
        let (user, out) = (arg(c, 0), arg(c, 2));
        if !pad_connected(user) {
            return ret(c, ERROR_DEVICE_NOT_CONNECTED);
        }
        // XINPUT_STATE { u32 dwPacketNumber; XINPUT_GAMEPAD { u16 wButtons; u8 LT, RT; i16 LX, LY, RX, RY; } }
        let index = if user == 0xFF { 0 } else { user as usize };
        // While the on-screen keyboard is up the system has the input: the
        // title's controller reads as untouched (and what is typed on the
        // PC's keyboard doesn't reach the game through the key mapping).
        let host = if index == 0 { host_pad_for_title() } else { Pad::default() };
        let pad = scripted_pad(index, host);
        mem::w32(out, input_packet(index, pad));
        mem::w16(out + 4, pad.buttons);
        mem::w8(out + 6, pad.left_trigger);
        mem::w8(out + 7, pad.right_trigger);
        for (i, v) in pad.thumbs.iter().enumerate() {
            mem::w16(out + 8 + 2 * i as u32, *v as u16);
        }
        ret(c, ERROR_SUCCESS);
    }
    XamInputSetState => |c| {
        let user = arg(c, 0);
        ret(c, if pad_connected(user) { ERROR_SUCCESS } else { ERROR_DEVICE_NOT_CONNECTED });
    }
    XamInputGetKeystrokeEx => |c| { ret(c, ERROR_EMPTY); }

    // ---- Notifications

    XamNotifyCreateListener => |c| {
        let mask = arg64(c, 0);
        let l = Arc::new(Listener { mask, queue: Mutex::new(VecDeque::new()), event: object::new_event(true, false) });
        let mut sent = STARTUP_SENT.lock().unwrap();
        if !*sent && mask & 1 != 0 {
            // What the console's XAM reports to a title's first system listener.
            *sent = true;
            l.push([(XN_SYS_UI, 1), (XN_SYS_UI, 0), (XN_SYS_SIGNINCHANGED, 1), (XN_SYS_SIGNINCHANGED, 1)]);
        }
        LISTENERS.lock().unwrap().push(l.clone());
        let event = l.event;
        ret(c, object::insert(Object::Other(l, Some(event))));
    }
    XNotifyGetNext => |c| {
        let (h, match_id, id_out, param_out) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3));
        let Some(l) = object::get_other::<Listener>(h) else { return ret(c, 0) };
        let mut q = l.queue.lock().unwrap();
        let pos = if match_id != 0 { q.iter().position(|&(id, _)| id == match_id) } else if q.is_empty() { None } else { Some(0) };
        let found = pos.and_then(|i| q.remove(i));
        if q.is_empty() {
            object::reset_event(l.event);
        }
        match found {
            Some((id, param)) => {
                out32(id_out, id);
                out32(param_out, param);
                ret(c, 1);
            }
            None => ret(c, 0),
        }
    }
    XNotifyPositionUI => |c| {}

    // ---- UI (answered without showing anything; the text is logged)

    XamShowMessageBoxUI => |c| {
        let (title, text, buttons, focus, result_ptr, ov) = (arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 5), arg(c, 7), arg(c, 8));
        let w = |p: u32| if p == 0 { String::new() } else { String::from_utf16_lossy(&mem::wstr(p, 4096)) };
        notify(XN_SYS_UI, 1);
        diag::log(&format!("XamShowMessageBoxUI: \"{}\": \"{}\" ({buttons} buttons, answering {focus})", w(title), w(text)));
        notify(XN_SYS_UI, 0);
        out32(result_ptr, focus);
        finish(c, ov, ERROR_SUCCESS, 0);
    }
    // The hard disk is the only storage device, so it is chosen without asking,
    // as the console does when one device is present.
    XamShowDeviceSelectorUI => |c| {
        let (device_out, ov) = (arg(c, 4), arg(c, 5));
        // Logged with the frame: each call tells the game the system UI opened
        // and closed, and the game pauses a match while it is open.
        let frame = crate::video::PRESENTED.load(std::sync::atomic::Ordering::Relaxed);
        diag::log(&format!("XamShowDeviceSelectorUI: the hard disk, without asking (frame {frame}) [from 0x{:08X}]", c.lr as u32));
        notify(XN_SYS_UI, 1);
        out32(device_out, HDD_DEVICE);
        notify(XN_SYS_UI, 0);
        finish(c, ov, ERROR_SUCCESS, 0);
    }
    // The Xbox LIVE panels: sign-in, friends, a gamer card, a player review,
    // a game invite, the marketplace. There is no LIVE service (PLAN §5), so
    // each opens and closes at once with nothing changed, as when the
    // player backs out of it: the title sees the system UI come and go and
    // then finds the profile still offline. Each call is logged.
    XamShowSigninUI => |c| { live_panel(c, "XamShowSigninUI"); }
    XamShowFriendsUI => |c| { live_panel(c, "XamShowFriendsUI"); }
    XamShowGamerCardUIForXUID => |c| { live_panel(c, "XamShowGamerCardUIForXUID"); }
    XamShowPlayerReviewUI => |c| { live_panel(c, "XamShowPlayerReviewUI"); }
    XamShowGameInviteUI => |c| { live_panel(c, "XamShowGameInviteUI"); }
    XamShowMarketplaceUI => |c| { live_panel(c, "XamShowMarketplaceUI"); }

    // XamShowKeyboardUI(user, flags, default text, title, description, result
    // buffer, buffer size in characters, overlapped): see "The on-screen
    // keyboard" above. Pending until the player answers; then the result is
    // ERROR_SUCCESS with the text in the buffer, or ERROR_CANCELLED.
    XamShowKeyboardUI => |c| {
        let (user, flags, default, title, description) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4));
        let (buffer, chars, ov) = (arg(c, 5), arg(c, 6), arg(c, 7));
        if buffer == 0 || chars == 0 || ov == 0 {
            diag::fatal(&format!(
                "XamShowKeyboardUI without a result buffer or an overlapped structure (buffer 0x{buffer:08X}, {chars} characters, overlapped 0x{ov:08X}) \
                 [from 0x{:08X}]: the blocking form is not implemented",
                c.lr as u32
            ));
        }
        let w = |p: u32| if p == 0 { String::new() } else { String::from_utf16_lossy(&mem::wstr(p, 1024)) };
        let max_chars = (chars - 1) as usize;
        let text = String::from_utf16_lossy(&keyboard_units(&w(default), max_chars));
        let request = KeyboardRequest { title: w(title), description: w(description), text, max_chars };
        let mut pending = KEYBOARD.lock().unwrap();
        if pending.is_some() {
            // One system UI at a time, as on the console.
            drop(pending);
            return ret(c, ERROR_ACCESS_DENIED);
        }
        diag::log(&format!(
            "XamShowKeyboardUI: \"{}\": \"{}\", starting with \"{}\", up to {max_chars} characters (user {user}, flags {flags:#x}) [from 0x{:08X}]",
            request.title, request.description, request.text, c.lr as u32
        ));
        let default_text = request.text.clone();
        *pending = Some(KeyboardPending { request, buffer, overlapped: ov });
        drop(pending);
        // XOVERLAPPED: the result at +0 is ERROR_IO_PENDING until it completes;
        // its event (+0xC), if any, is unsignalled until then.
        mem::w32(ov, ERROR_IO_PENDING);
        mem::w32(ov + 4, 0);
        mem::w32(ov + 0x18, 0);
        let event = mem::r32(ov + 0xC);
        if event != 0 && let Some(o) = object::get(event).and_then(|o| o.waitable()) {
            object::reset_event(o);
        }
        notify(XN_SYS_UI, 1);
        if !KEYBOARD_HOST.load(std::sync::atomic::Ordering::Relaxed) {
            diag::log("XamShowKeyboardUI: no window to type in; answering with the starting text");
            keyboard_finish(Some(&default_text));
        }
        ret(c, ERROR_IO_PENDING);
    }
    XamShowDirtyDiscErrorUI => |c| {
        diag::fatal("XamShowDirtyDiscErrorUI: the game reported a disc read error (a file it needs is missing or unreadable)")
    }

    // ---- System app messages

    XMsgStartIORequest => |c| {
        let (app, message, ov, buffer, size) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4));
        MESSAGE_WAITED.set(ov == 0);
        let r = app_message(app, message, buffer, size);
        MESSAGE_WAITED.set(false);
        finish(c, ov, r, 0);
    }
    XMsgStartIORequestEx => |c| {
        let (app, message, ov, buffer, size) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4));
        let r = app_message(app, message, buffer, size);
        finish(c, ov, r, 0);
    }
    XMsgInProcessCall => |c| {
        let (app, message, p1, p2) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3));
        let r = app_message(app, message, p1, p2);
        ret(c, r);
    }

    // ---- Enumeration

    // XamEnumerate(handle, flags, buffer, buffer size, items returned, overlapped):
    // the next batch of items, as many as the buffer and the enumerator's
    // count allow. With none left, ERROR_NO_MORE_FILES. An overlapped call
    // reports the item count as its length.
    XamEnumerate => |c| {
        use std::sync::atomic::Ordering;
        let (h, buffer, size, items_out, ov) = (arg(c, 0), arg(c, 2), arg(c, 3), arg(c, 4), arg(c, 5));
        let Some(e) = object::get_other::<Enumerator>(h) else {
            diag::fatal(&format!("XamEnumerate on handle 0x{h:08X}, which is not an enumerator this kernel made"))
        };
        let fit = size / e.item_size;
        if buffer == 0 || fit == 0 {
            diag::fatal(&format!(
                "XamEnumerate with a buffer (0x{buffer:08X}, {size:#x} bytes) that holds no item of {:#x} bytes [from 0x{:08X}]",
                e.item_size, c.lr as u32
            ));
        }
        let start = e.next.load(Ordering::Relaxed);
        let count = (e.items.len() - start).min(e.per_call as usize).min(fit as usize);
        if count == 0 {
            out32(items_out, 0);
            return finish(c, ov, ERROR_NO_MORE_FILES, 0);
        }
        for (i, item) in e.items[start..start + count].iter().enumerate() {
            mem::write(buffer + i as u32 * e.item_size, item);
        }
        e.next.store(start + count, Ordering::Relaxed);
        out32(items_out, count as u32);
        finish(c, ov, ERROR_SUCCESS, count as u32);
    }
    // ---- Content packages (saves): folders on the hard disk, mounted under
    // the root name the title chooses (`save:`), as the console mounts them.

    XamContentGetDeviceState => |c| {
        let (device, ov) = (arg(c, 0), arg(c, 1));
        finish(c, ov, if device == HDD_DEVICE { ERROR_SUCCESS } else { ERROR_DEVICE_NOT_CONNECTED }, 0);
    }
    XamContentGetDeviceData => |c| {
        let (device, data) = (arg(c, 0), arg(c, 1));
        if device != HDD_DEVICE {
            return ret(c, ERROR_DEVICE_NOT_CONNECTED);
        }
        // XDEVICE_DATA: id, type (1 = hard disk), total and free bytes, name.
        mem::w32(data, HDD_DEVICE);
        mem::w32(data + 4, 1);
        mem::w64(data + 8, 20 << 30);
        mem::w64(data + 0x10, 16 << 30);
        for (i, ch) in "Hard Drive".encode_utf16().chain([0]).enumerate() {
            mem::w16(data + 0x18 + 2 * i as u32, ch);
        }
        ret(c, ERROR_SUCCESS);
    }
    XamContentCreateEx => |c| {
        let (root, data, flags, disposition_out, license_out, ov) = (arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4), arg(c, 5), arg(c, 8));
        let root = mem::latin1(&mem::cstr(root, 64));
        let Some(dir) = content_dir(data) else { return finish(c, ov, ERROR_PATH_NOT_FOUND, 0) };
        let exists = dir.is_dir();
        // The low nibble is the creation disposition, as for files (1 create
        // new, 2 create always, 3 open existing, 4 open always, 5 truncate);
        // higher bits are transfer and signing flags.
        let result = match (flags & 0xF, exists) {
            (1, true) => Err(ERROR_ALREADY_EXISTS),
            // A missing package is a missing path, which titles read as "no save yet".
            (3 | 5, false) => Err(ERROR_PATH_NOT_FOUND),
            (2 | 5, true) => {
                let _ = std::fs::remove_dir_all(&dir);
                Ok(1)
            }
            (_, true) => Ok(2),
            (_, false) => Ok(1),
        };
        let r = match result {
            Err(e) => e,
            Ok(disposition) => {
                if std::fs::create_dir_all(&dir).is_err() {
                    return finish(c, ov, ERROR_ACCESS_DENIED, 0);
                }
                let device = format!("\\Device\\Content\\{root}");
                crate::file::add_device(&device, dir.clone(), false, "Content");
                crate::file::create_link(&format!("{root}:"), &device);
                out32(disposition_out, disposition);
                // The licence mask: downloadable content (type 2) is the user's own
                // purchases, fully licensed; saves have none.
                out32(license_out, if mem::r32(data + 4) == 2 { 0xFFFF_FFFF } else { 0 });
                // The display name the title gives, for enumerations.
                let display = String::from_utf16_lossy(&mem::wstr(data + 8, 128));
                let name_file = display_name_file(&dir);
                if !display.is_empty() && (disposition == 1 || !name_file.exists()) {
                    let _ = std::fs::write(&name_file, &display);
                }
                diag::log(&format!("content {root}: {} (flags {flags:#x}: {})", dir.display(), if disposition == 1 { "created" } else { "opened" }));
                {
                    let mut open = OPEN_PACKAGES.lock().unwrap();
                    open.retain(|(r, _)| *r != root);
                    open.push((root.clone(), dir.clone()));
                }
                ERROR_SUCCESS
            }
        };
        finish(c, ov, r, 0);
    }
    XamContentClose => |c| {
        let (root, ov) = (mem::latin1(&mem::cstr(arg(c, 0), 64)), arg(c, 1));
        // The main save read (or written) and closed: one-off work on it can run (`unlock`).
        let closed = {
            let mut open = OPEN_PACKAGES.lock().unwrap();
            open.iter().position(|(r, _)| *r == root).map(|i| open.remove(i).1)
        };
        if closed.is_some_and(|d| d.file_name().is_some_and(|n| n.eq_ignore_ascii_case("SaveData.dat"))) {
            crate::unlock::save_closed();
        }
        crate::file::delete_link(&format!("{root}:"));
        let r = if crate::file::remove_device(&format!("\\Device\\Content\\{root}")) { ERROR_SUCCESS } else { ERROR_NOT_FOUND };
        finish(c, ov, r, 0);
    }
    XamContentDelete => |c| {
        let (data, ov) = (arg(c, 1), arg(c, 2));
        let r = match content_dir(data) {
            Some(dir) if dir.is_dir() => {
                let _ = std::fs::remove_file(display_name_file(&dir));
                diag::log(&format!("content: {} deleted by the title", dir.display()));
                if std::fs::remove_dir_all(&dir).is_ok() { ERROR_SUCCESS } else { ERROR_ACCESS_DENIED }
            }
            _ => ERROR_FILE_NOT_FOUND,
        };
        finish(c, ov, r, 0);
    }
    // XamContentCreateEnumerator(user, device, content type, flags, items per
    // call, buffer size out, handle out): the packages of that type on the
    // device (0: every device), as XCONTENT_DATA items. The game finds its
    // created superstars this way (`sub_829009B0`: saved games, 354 a call,
    // matched by file name); with an empty list it reported a saved
    // superstar's data as "damaged or missing" at the next start.
    XamContentCreateEnumerator => |c| {
        let (user, device, ty, per_call) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 4));
        let (size_out, handle_out) = (arg(c, 5), arg(c, 6));
        if per_call == 0 {
            return ret(c, ERROR_INVALID_PARAMETER);
        }
        if device != 0 && device != HDD_DEVICE {
            diag::log(&format!("content: listing type {ty} refused: device {device} is not connected [from 0x{:08X}]", c.lr as u32));
            return ret(c, ERROR_DEVICE_NOT_CONNECTED);
        }
        // Saved games belong to a profile (0xFF: any signed-in user).
        if ty == 1 && user != 0xFF && !signed_in(user) {
            diag::log(&format!("content: listing type {ty} refused: user {user} is not signed in [from 0x{:08X}]", c.lr as u32));
            return ret(c, ERROR_NO_SUCH_USER);
        }
        let packages = content_type_dir(ty).map(|dir| packages_in(&dir)).unwrap_or_default();
        diag::log(&format!(
            "content: listing type {ty} on device {device} for user {user}: {} [from 0x{:08X}]",
            if packages.is_empty() { "nothing".to_string() } else { packages.iter().map(|(file, _)| file.as_str()).collect::<Vec<_>>().join(", ") },
            c.lr as u32
        ));
        let items = packages.iter().map(|(file, display)| content_item(ty, display, file)).collect();
        out32(size_out, XCONTENT_DATA_SIZE * per_call);
        let e = Enumerator { item_size: XCONTENT_DATA_SIZE, per_call, items, next: std::sync::atomic::AtomicUsize::new(0), private: 0 };
        out32(handle_out, object::insert(Object::Other(Arc::new(e), None)));
        ret(c, ERROR_SUCCESS);
    }

    // XamCreateEnumeratorHandle(user, app, open message, close message, extra
    // size, items per call, flags, handle out): an enumerator served by a
    // system app, with `extra` bytes of private request data. XAPI's music
    // player (`sub_82E1A458`) uses it to list the console's own music: app
    // 0xFA, messages 0x70026/0x70027, 0x15A8 extra bytes, 80 items a call of
    // 0x334 bytes. It then fills the private data
    // (`XamGetPrivateEnumStructureFromHandle`) and sends 0x70025 to the app
    // (`XMsgInProcessCall`, below). Advanced Entrance Creation asks for it to
    // offer custom soundtracks (user47-48). The port has no music library,
    // so the list is empty: the first XamEnumerate answers
    // ERROR_NO_MORE_FILES, as a console with no music on it does. Other apps
    // stop loudly.
    XamCreateEnumeratorHandle => |c| {
        let (user, app, open, close, extra, per_call, flags, handle_out) =
            (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4), arg(c, 5), arg(c, 6), arg(c, 7));
        if app != 0xFA {
            diag::fatal(&format!(
                "XamCreateEnumeratorHandle for app 0x{app:02X} (messages 0x{open:08X}/0x{close:08X}, {extra:#x} extra bytes) is not implemented yet [from 0x{:08X}]",
                c.lr as u32
            ));
        }
        if per_call == 0 || handle_out == 0 || extra > 0x10_0000 {
            return ret(c, ERROR_INVALID_PARAMETER);
        }
        diag::log(&format!("music player: listing (messages 0x{open:08X}/0x{close:08X}, user {user:#x}, flags {flags:#x}) [from 0x{:08X}]", c.lr as u32));
        let private = mem::kalloc(0x18 + extra, 16);
        let items: Vec<Vec<u8>> = crate::music::library().iter().enumerate().map(|(i, s)| music_item(i as u32 + 1, s)).collect();
        diag::log(&format!("music player: listing {} song(s) from {}", items.len(), crate::music::folder().display()));
        let e = Enumerator { item_size: MUSIC_ITEM_SIZE, per_call, items, next: std::sync::atomic::AtomicUsize::new(0), private };
        out32(handle_out, object::insert(Object::Other(Arc::new(e), None)));
        ret(c, ERROR_SUCCESS);
    }
    // XamGetPrivateEnumStructureFromHandle(handle, pointer out): the private
    // data of an enumerator made above. XAPI releases it with
    // ObDereferenceObject, which holds nothing here.
    XamGetPrivateEnumStructureFromHandle => |c| {
        let (h, out) = (arg(c, 0), arg(c, 1));
        let private = object::get_other::<Enumerator>(h).map_or(0, |e| e.private);
        if private == 0 {
            diag::fatal(&format!("XamGetPrivateEnumStructureFromHandle on handle 0x{h:08X}, which has no private data [from 0x{:08X}]", c.lr as u32));
        }
        out32(out, private);
        ret(c, ERROR_SUCCESS);
    }

    // ---- Tasks

    XamTaskShouldExit => |c| { ret(c, 0); }
    XamTaskCloseHandle => |c| { ret(c, if object::close(arg(c, 0)) { ERROR_SUCCESS } else { ERROR_INVALID_HANDLE }); }

    // ---- Party (offline: never in a party)

    // XamPartyGetUserList(version, list): XPARTY_E_NOT_IN_PARTY, the value the
    // game itself checks for (`cmp r3, 0x807D0003`).
    XamPartyGetUserList => |c| { ret(c, 0x807D_0003); }

    // ---- Voice (no headset)

    XamVoiceHeadsetPresent => |c| { ret(c, 0); }
    XamVoiceIsActiveProcess => |c| { ret(c, 0); }

    // ---- Networking: offline (the console has no network link)

    NetDll_WSAStartup => |c| {
        let data = arg(c, 2);
        if data != 0 {
            mem::fill(data, 0x190, 0);
            mem::w16(data, 0x0202);
            mem::w16(data + 2, 0x0202);
        }
        ret(c, 0);
    }
    NetDll_WSACleanup => |c| { ret(c, 0); }
    NetDll_WSAGetLastError => |c| { ret(c, 10050); } // WSAENETDOWN
    NetDll_XNetStartup => |c| { ret(c, 0); }
    NetDll_XNetStartupEx => |c| { ret(c, 0); }
    NetDll_XNetCleanup => |c| { ret(c, 0); }
    NetDll_XNetGetEthernetLinkStatus => |c| { ret(c, 0); }
    NetDll_XNetGetTitleXnAddr => |c| {
        let out = arg(c, 1);
        if out != 0 {
            mem::fill(out, 0x24, 0);
        }
        ret(c, 1); // XNET_GET_XNADDR_NONE
    }
    NetDll_socket => |c| { ret(c, u32::MAX); } // INVALID_SOCKET (network down)
}


// ---- Scripted input (`--input`) ---------------------------------------------------------
//
// For automated runs, a script presses controller buttons at given times
// after boot (`start@35`, `dpad_down@42.5`, held 150 ms) or on given
// presented frames (`a@f2400`, held 9 frames). Frame times follow the game
// even when it runs slower than real time (the opening movies do).
//
// An entry is `[PAD:]CONTROL@WHEN[+HOLD]`:
// - `PAD` is 1 (the default) or 2. Controller 2 is connected only when the
//   script names it (a bare `pad2` entry connects it without pressing
//   anything), and only the script drives it.
// - `CONTROL` is a button, `lt`/`rt` (a full trigger pull) or a stick pushed
//   all the way (`lup`, `ldown`, `lleft`, `lright`, `rup`, ...).
// - `WHEN` is seconds, `fFRAME`, or `fFIRST-LAST/STEP` (a press every `STEP`
//   frames from `FIRST` through `LAST`).
// - `HOLD` is how many frames each press lasts (default 9).

/// When a scripted press happens.
#[derive(Clone, Copy)]
enum At {
    Time(std::time::Duration),
    /// First frame, last frame, step.
    Frames(u64, u64, u64),
}

/// A scripted control: a button, a trigger or a stick direction.
#[derive(Clone, Copy)]
enum Control {
    Button(u16),
    Trigger { right: bool },
    /// Thumb axis (left X, left Y, right X, right Y) and its value.
    Stick(usize, i16),
}

#[derive(Clone, Copy)]
struct Press {
    pad: usize,
    at: At,
    hold: u64,
    control: Control,
}

static INPUT_SCRIPT: Mutex<Vec<Press>> = Mutex::new(Vec::new());
static SECOND_PAD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
const NO_PAD: Pad = Pad { buttons: 0, left_trigger: 0, right_trigger: 0, thumbs: [0; 4] };
static INPUT_PACKET: Mutex<[(Pad, u32); 2]> = Mutex::new([(NO_PAD, 0); 2]);

fn control(name: &str) -> Option<Control> {
    Some(Control::Button(match name.to_ascii_lowercase().as_str() {
        "up" | "dpad_up" => 0x0001,
        "down" | "dpad_down" => 0x0002,
        "left" | "dpad_left" => 0x0004,
        "right" | "dpad_right" => 0x0008,
        "start" => 0x0010,
        "back" => 0x0020,
        "ls" => 0x0040,
        "rs" => 0x0080,
        "lb" => 0x0100,
        "rb" => 0x0200,
        "a" => 0x1000,
        "b" => 0x2000,
        "x" => 0x4000,
        "y" => 0x8000,
        "lt" => return Some(Control::Trigger { right: false }),
        "rt" => return Some(Control::Trigger { right: true }),
        "lleft" => return Some(Control::Stick(0, -32768)),
        "lright" => return Some(Control::Stick(0, 32767)),
        "ldown" => return Some(Control::Stick(1, -32768)),
        "lup" => return Some(Control::Stick(1, 32767)),
        "rleft" => return Some(Control::Stick(2, -32768)),
        "rright" => return Some(Control::Stick(2, 32767)),
        "rdown" => return Some(Control::Stick(3, -32768)),
        "rup" => return Some(Control::Stick(3, 32767)),
        _ => return None,
    }))
}

/// Parses the script (see above). Returns an error message for a bad entry.
pub fn set_input_script(spec: &str) -> Result<(), String> {
    let mut v = Vec::new();
    for item in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if item.eq_ignore_ascii_case("pad2") {
            SECOND_PAD.store(true, std::sync::atomic::Ordering::Relaxed);
            continue;
        }
        let (b, t) = item.split_once('@').ok_or_else(|| format!("input script entry {item:?} has no @time"))?;
        let (pad, b) = match b.split_once(':') {
            Some((p, b)) => {
                let p: usize = p.parse().ok().filter(|p| (1..=2).contains(p)).ok_or_else(|| format!("bad controller in {item:?}"))?;
                (p - 1, b)
            }
            None => (0, b),
        };
        if pad == 1 {
            SECOND_PAD.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let control = control(b).ok_or_else(|| format!("unknown control {b:?}"))?;
        let (t, hold) = match t.split_once('+') {
            Some((t, h)) => (t, h.parse().map_err(|_| format!("bad hold in {item:?}"))?),
            None => (t, 9),
        };
        let bad = || format!("bad time in {item:?}");
        let at = match t.strip_prefix('f') {
            Some(f) => match f.split_once('-') {
                Some((first, rest)) => {
                    let (last, step) = rest.split_once('/').ok_or_else(bad)?;
                    let (first, last): (u64, u64) = (first.parse().map_err(|_| bad())?, last.parse().map_err(|_| bad())?);
                    if last < first {
                        return Err(bad());
                    }
                    At::Frames(first, last, step.parse::<u64>().map_err(|_| bad())?.max(1))
                }
                None => {
                    let f = f.parse().map_err(|_| bad())?;
                    At::Frames(f, f, 1)
                }
            },
            None => At::Time(std::time::Duration::from_secs_f64(t.parse().map_err(|_| bad())?)),
        };
        v.push(Press { pad, at, hold, control });
    }
    *INPUT_SCRIPT.lock().unwrap() = v;
    Ok(())
}

/// Controller 1 is always connected; controller 2 when the script names it.
fn pad_connected(user: u32) -> bool {
    user == 0 || user == 0xFF || (user == 1 && SECOND_PAD.load(std::sync::atomic::Ordering::Relaxed))
}

/// `base` plus the scripted presses of controller `pad` that are down now.
fn scripted_pad(pad: usize, mut base: Pad) -> Pad {
    let t = crate::time::since_boot();
    let frame = crate::video::PRESENTED.load(std::sync::atomic::Ordering::Relaxed);
    let hold_time = std::time::Duration::from_millis(150);
    for p in INPUT_SCRIPT.lock().unwrap().iter().filter(|p| p.pad == pad) {
        let down = match p.at {
            At::Time(at) => t >= at && t < at + hold_time,
            At::Frames(first, last, step) => {
                // The latest press at or before this frame.
                frame >= first && frame < first + ((frame - first) / step).min((last - first) / step) * step + p.hold
            }
        };
        if down {
            match p.control {
                Control::Button(bit) => base.buttons |= bit,
                Control::Trigger { right: false } => base.left_trigger = 255,
                Control::Trigger { right: true } => base.right_trigger = 255,
                Control::Stick(axis, v) => base.thumbs[axis] = v,
            }
        }
    }
    base
}

/// Controller 1 as the host reads it (`set_pad`): the window's gamepad or
/// keyboard. Scripted presses (`--input`) are added on top.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pad {
    /// XINPUT button bits (D-pad 0x1-0x8, Start 0x10, Back 0x20, thumbs 0x40/0x80,
    /// shoulders 0x100/0x200, A 0x1000, B 0x2000, X 0x4000, Y 0x8000).
    pub buttons: u16,
    pub left_trigger: u8,
    pub right_trigger: u8,
    /// Left X, left Y, right X, right Y (up is positive).
    pub thumbs: [i16; 4],
}

static HOST_PAD: Mutex<Pad> = Mutex::new(NO_PAD);

/// The host's controller state, read by `XamInputGetState`.
pub fn set_pad(p: Pad) {
    *HOST_PAD.lock().unwrap() = p;
}

/// The packet number of controller `index`: it changes whenever the state does.
fn input_packet(index: usize, pad: Pad) -> u32 {
    let mut all = INPUT_PACKET.lock().unwrap();
    let p = &mut all[index.min(1)];
    if p.0 != pad {
        if p.0.buttons != pad.buttons && pad.buttons != 0 && crate::diag::TRACE_THREADS.load(std::sync::atomic::Ordering::Relaxed) {
            let frame = crate::video::PRESENTED.load(std::sync::atomic::Ordering::Relaxed);
            crate::diag::log(&format!("input: controller {} buttons 0x{:04X} (frame {frame})", index + 1, pad.buttons));
        }
        p.0 = pad;
        p.1 += 1;
    }
    p.1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_content_item_is_laid_out_as_the_guest_reads_it() {
        let d = content_item(1, "Superstar 1", "00CustomSuperStar.cas");
        assert_eq!(d.len(), 0x134);
        assert_eq!(&d[0..8], &[0, 0, 0, 1, 0, 0, 0, 1]);
        assert_eq!(&d[8..12], &[0, b'S', 0, b'u']);
        assert_eq!(&d[0x108..0x108 + 21], b"00CustomSuperStar.cas");
        assert_eq!(d[0x108 + 21], 0);
        // Display names keep their terminator; file names fill the 42-byte
        // field, as marketplace content names do (42 characters).
        let long = content_item(2, &"x".repeat(200), &"y".repeat(60));
        assert_eq!(&long[8 + 2 * 126..8 + 2 * 128], &[0, b'x', 0, 0]);
        assert_eq!(&long[0x108 + 40..0x108 + 43], &[b'y', b'y', 0]);
    }

    #[test]
    fn keyboard_text_is_cut_to_the_buffer_without_splitting_a_pair() {
        let units = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
        assert_eq!(keyboard_units("John", 10), units("John"));
        assert_eq!(keyboard_units("Undertaker", 5), units("Under"));
        // U+1F600 takes two units: cut between them, neither stays.
        assert_eq!(keyboard_units("ab\u{1F600}", 3), units("ab"));
        assert_eq!(keyboard_units("ab\u{1F600}", 4), units("ab\u{1F600}"));
        assert!(keyboard_units("anything", 0).is_empty());
    }

    #[test]
    fn packages_are_the_folders_with_their_display_names() {
        let dir = std::env::temp_dir().join(format!("wwe14-content-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("SaveData.dat")).unwrap();
        std::fs::create_dir_all(dir.join("00CustomSuperStar.cas")).unwrap();
        std::fs::write(dir.join("stray.txt"), "not a package").unwrap();
        assert_eq!(display_name_file(&dir.join("SaveData.dat")), dir.join("SaveData.dat.displayname"));
        std::fs::write(display_name_file(&dir.join("SaveData.dat")), "WWE 2K14 Save").unwrap();
        assert_eq!(
            packages_in(&dir),
            [("00CustomSuperStar.cas".to_string(), "00CustomSuperStar.cas".to_string()), ("SaveData.dat".to_string(), "WWE 2K14 Save".to_string())]
        );
        assert!(packages_in(&dir.join("missing")).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
