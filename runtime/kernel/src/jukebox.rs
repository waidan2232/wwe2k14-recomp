//! The Jukebox's USER PLAYLIST with several songs ticked (the user's request,
//! 2026-11-26). The game keeps one custom song: its Jukebox save has one
//! 40-character title (slot 0x96) and the list lets one row be ticked. The
//! selected songs are kept here instead (`music\.wwe14-playlist`, one title a
//! line), and three of the game's functions are wrapped (found statically,
//! 2026-11-26):
//!
//! - `sub_82B3A4B8(menu, event)`, the Jukebox's input handler. After the
//!   original, a toggle on a USER PLAYLIST row (its "ticked" word changed)
//!   becomes an add or remove in the selection; every row is then ticked from
//!   the selection and the list redrawn (`sub_82B393C0(menu, 0)`).
//! - `sub_82B397B8(menu)`, which builds the rows: after the original, the
//!   user list's rows are ticked from the selection.
//! - `sub_82ACD5D8(player, record)`, the menu music's next-song pick: when
//!   the shuffle reaches the custom-song slot (0x96), the record names the
//!   next selected song (rotating, shuffled) instead of the one saved title.
//!
//! The save keeps working as the game expects: slot 0x96 is on while any
//! song is selected, with the first selected title as its title, so a lost
//! side file falls back to one song.
//!
//! Layouts (guest, big-endian): the menu: +8 state, +0xC mode (1: the user
//! list), +0x10 rows (0x18 bytes each: +0 slot, +8 ticked), +0x18 row count,
//! +0x1C cursor, +0x34 its Jukebox block (slot 0x96's flag byte at +0xCA,
//! bit 0 set = off; the title at +0xCC, 0x50 bytes). The player: +0xCA and
//! +0xCC the same block's copy, +0x11C shuffle index, +0x178 the shuffled
//! slots (u32), +0x3D4 their count. The pick's record (0x5C bytes): +0 track
//! id (0x270E: a custom song), +4 0, +8 the title (0x50 bytes).

use std::sync::Mutex;

use ppc_rt::Ctx;

use crate::mem;

const USER_SLOT: u32 = 0x96;
const CUSTOM_TRACK: u32 = 0x270E;
/// `sub_829365C8(_, i)`: the title of the user list's row `i` (in r4; r3 is
/// replaced by the music browser), or 0.
const ROW_TITLE: u32 = 0x8293_65C8;
/// `sub_82B393C0(menu, 0)`: redraws the list.
const REDRAW: u32 = 0x82B3_93C0;

struct Selection {
    loaded: bool,
    titles: Vec<String>,
    /// Songs still to play this round, shuffled.
    queue: Vec<String>,
    seed: u64,
}

static SELECTION: Mutex<Selection> = Mutex::new(Selection { loaded: false, titles: Vec::new(), queue: Vec::new(), seed: 0 });

fn file() -> std::path::PathBuf {
    crate::music::folder().join(".wwe14-playlist")
}

fn with<R>(f: impl FnOnce(&mut Selection) -> R) -> R {
    let mut s = SELECTION.lock().unwrap();
    if !s.loaded {
        s.loaded = true;
        s.titles = std::fs::read_to_string(file()).unwrap_or_default().lines().map(str::to_string).filter(|t| !t.is_empty()).collect();
        s.seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64) | 1;
    }
    f(&mut s)
}

fn save(titles: &[String]) {
    let _ = std::fs::write(file(), titles.join("\n"));
}

/// A guest wide string of at most 40 characters, as the game compares titles.
fn read_title(a: u32) -> String {
    String::from_utf16_lossy(&mem::wstr(a, 40))
}

/// Writes `title` into a 0x50-byte field: at most 39 characters, terminated, zero-padded.
fn write_title(a: u32, title: &str) {
    mem::fill(a, 0x50, 0);
    for (i, u) in title.encode_utf16().take(39).enumerate() {
        mem::w16(a + 2 * i as u32, u);
    }
}

fn row_title(c: &mut Ctx, i: u32) -> Option<String> {
    let p = crate::thread::call(c, ROW_TITLE, &[0, i as u64]) as u32;
    (p != 0).then(|| read_title(p)).filter(|t| !t.is_empty())
}

fn is_user_list(menu: u32) -> bool {
    mem::r32(menu + 0xC) == 1
}

/// Ticks every row of the user list from the selection; puts the selection
/// into the menu's Jukebox block (slot 0x96 on, the first title as its title).
fn apply(c: &mut Ctx, menu: u32) {
    let titles = with(|s| s.titles.clone());
    let (rows, n) = (mem::r32(menu + 0x10), mem::r32(menu + 0x18));
    for i in 0..n.min(0x1000) {
        let row = rows + 0x18 * i;
        let ticked = row_title(c, i).is_some_and(|t| titles.contains(&t));
        mem::w32(row, USER_SLOT);
        mem::w32(row + 8, ticked as u32);
    }
    let flag = menu + 0x34 + 0x96;
    match titles.first() {
        Some(first) => {
            mem::w8(flag, mem::r8(flag) & !1);
            write_title(menu + 0x34 + 0x98, first);
        }
        None => {
            mem::w8(flag, mem::r8(flag) | 1);
            mem::fill(menu + 0x34 + 0x98, 0x50, 0);
        }
    }
}

/// `sub_82B3A4B8(menu, event)`, the Jukebox's input handler (see the module).
pub extern "C-unwind" fn input(c: &mut Ctx, b: *mut u8) {
    let menu = c.r[3] as u32;
    let watch = is_user_list(menu) && mem::r32(menu + 8) != 1;
    let before: Vec<u32> = if watch {
        let (rows, n) = (mem::r32(menu + 0x10), mem::r32(menu + 0x18));
        (0..n.min(0x1000)).map(|i| mem::r32(rows + 0x18 * i + 8)).collect()
    } else {
        Vec::new()
    };
    unsafe { ppc_rt::original(0x82B3_A4B8).expect("the Jukebox input handler is replaced")(c, b) };
    if !watch || !is_user_list(menu) {
        return;
    }
    let (rows, n) = (mem::r32(menu + 0x10), mem::r32(menu + 0x18));
    if n as usize != before.len() {
        return;
    }
    let after: Vec<u32> = (0..n).map(|i| mem::r32(rows + 0x18 * i + 8)).collect();
    if after == before {
        return;
    }
    // The original flips the cursor's row and unticks every other one: the
    // row that went on, or else the cursor's row that went off.
    let cursor = mem::r32(menu + 0x1C);
    let toggled = (0..n as usize)
        .find(|&i| before[i] == 0 && after[i] != 0)
        .or_else(|| ((cursor as usize) < before.len() && before[cursor as usize] != after[cursor as usize]).then_some(cursor as usize));
    let result = c.r[3];
    if let Some(i) = toggled
        && let Some(title) = row_title(c, i as u32)
    {
        let on = after[i] != 0;
        let titles = with(|s| {
            s.titles.retain(|t| *t != title);
            if on {
                s.titles.push(title.clone());
            }
            s.queue.clear();
            s.titles.clone()
        });
        save(&titles);
        crate::diag::log(&format!(
            "jukebox: \"{title}\" {} the user playlist ({} selected)",
            if on { "added to" } else { "removed from" },
            titles.len()
        ));
    }
    apply(c, menu);
    crate::thread::call(c, REDRAW, &[menu as u64, 0]);
    c.r[3] = result;
}

/// `sub_82B397B8(menu)`, which builds the list's rows (see the module).
pub extern "C-unwind" fn rows(c: &mut Ctx, b: *mut u8) {
    let menu = c.r[3] as u32;
    unsafe { ppc_rt::original(0x82B3_97B8).expect("the Jukebox row builder is replaced")(c, b) };
    if is_user_list(menu) && with(|s| !s.titles.is_empty()) {
        let result = c.r[3];
        apply(c, menu);
        c.r[3] = result;
    }
}

/// `sub_82ACD5D8(player, record)`, the menu music's next-song pick (see the module).
pub extern "C-unwind" fn pick(c: &mut Ctx, b: *mut u8) {
    let (player, record) = (c.r[3] as u32, c.r[4] as u32);
    let idx = mem::r32(player + 0x11C);
    let count = mem::r32(player + 0x3D4);
    // The custom-song slot is on only once the save is loaded: the game's
    // music, held quiet until then, plays again (`music::release_hold`).
    if mem::r8(player + 0xCA) == 0 {
        crate::music::release_hold();
    }
    let custom_turn = idx < count.min(0x97) && mem::r32(player + 0x178 + 4 * idx) == USER_SLOT && mem::r8(player + 0xCA) == 0;
    if custom_turn
        && let Some(title) = next_title()
    {
        mem::fill(record, 0x5C, 0);
        mem::w32(record, CUSTOM_TRACK);
        write_title(record + 8, &title);
        crate::music::mark_jukebox_next(&clip(&title));
        mem::w32(player + 0x3D8, 0);
        mem::w32(player + 0x3DC, 0);
        c.r[3] = 1;
        return;
    }
    unsafe { ppc_rt::original(0x82AC_D5D8).expect("the Jukebox pick is replaced")(c, b) };
    // The original picked the custom song itself (the first pick after the
    // save loads comes this way, with the one saved title: the same song
    // opened every session, user60-65): shuffle that one too.
    if c.r[3] != 0 && mem::r32(record) == CUSTOM_TRACK {
        if let Some(title) = next_title() {
            write_title(record + 8, &title);
        }
        crate::music::mark_jukebox_next(&read_title(record + 8));
    }
}

/// The next selected song still in the music folder: each round plays every
/// one once, shuffled.
pub fn next_title() -> Option<String> {
    let have: Vec<String> = crate::music::library().iter().map(|s| String::from_utf16_lossy(&s.title.encode_utf16().take(39).collect::<Vec<_>>())).collect();
    with(|s| {
        let available: Vec<String> = s.titles.iter().filter(|t| have.contains(t)).cloned().collect();
        if available.is_empty() {
            return None;
        }
        s.queue.retain(|t| available.contains(t));
        if s.queue.is_empty() {
            let mut q = available;
            for i in (1..q.len()).rev() {
                s.seed ^= s.seed << 13;
                s.seed ^= s.seed >> 7;
                s.seed ^= s.seed << 17;
                q.swap(i, (s.seed % (i as u64 + 1)) as usize);
            }
            s.queue = q;
        }
        s.queue.pop()
    })
}

/// A title as the game keeps it: at most 39 characters.
fn clip(title: &str) -> String {
    String::from_utf16_lossy(&title.encode_utf16().take(39).collect::<Vec<_>>())
}

/// `sub_82ACCFC8(player)`, "is the menu music playing" (3 playing, 5 not):
/// a playing custom song started by the Jukebox counts as playing, so a mode
/// with its own menu music stops it (see `music::stop_jukebox_song`).
pub extern "C-unwind" fn menu_music_playing(c: &mut Ctx, b: *mut u8) {
    unsafe { ppc_rt::original(0x82AC_CFC8).expect("the menu-music query is replaced")(c, b) };
    // Asked on the main thread every frame: a place for one-off work on the
    // loaded save (`unlock`).
    crate::unlock::maybe_run(c);
    {
        // Asked every frame: logged when the answer changes.
        static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);
        let now = c.r[3] << 1 | crate::music::jukebox_song_playing() as u64;
        if LAST.swap(now, std::sync::atomic::Ordering::Relaxed) != now {
            trace_hook(&format!("menu music playing? game says {}, Jukebox song playing {}", c.r[3], crate::music::jukebox_song_playing()));
        }
    }
    if c.r[3] == 5 && crate::music::jukebox_song_playing() {
        c.r[3] = 3;
    }
}

/// `sub_82ACCA50(player, ...)`, stop the menu music: the Jukebox's custom
/// song stops with it.
pub extern "C-unwind" fn menu_music_stop(c: &mut Ctx, b: *mut u8) {
    trace_hook(&format!("menu music stop (Jukebox song playing {})", crate::music::jukebox_song_playing()));
    unsafe { ppc_rt::original(0x82AC_CA50).expect("the menu-music stop is replaced")(c, b) };
    let result = c.r[3];
    crate::music::stop_jukebox_song();
    c.r[3] = result;
}

/// `sub_82ACE298(player, event name, transition, ...)`: the menu-music
/// player starts a named piece of music (a 30 Years of WrestleMania era's
/// theme from a menu script, a mode's menu music). With a licensed track the
/// new music takes the track's place; a custom song plays outside the game's
/// mixer and went on over it, hiding it (user70: Hulk Hogan's theme in the
/// 30 Years menu). The Jukebox's song stops first.
pub extern "C-unwind" fn menu_music_play_named(c: &mut Ctx, b: *mut u8) {
    let name = c.r[4] as u32;
    trace_hook(&format!("menu music plays \"{}\" (Jukebox song playing {})", if name != 0 { mem::latin1(&mem::cstr(name, 96)) } else { String::new() }, crate::music::jukebox_song_playing()));
    if crate::music::jukebox_song_playing() {
        let event = if name != 0 { mem::latin1(&mem::cstr(name, 96)) } else { String::new() };
        crate::diag::log(&format!("jukebox: the game plays \"{event}\" on the menu music"));
        crate::music::stop_jukebox_song();
    }
    unsafe { ppc_rt::original(0x82AC_E298).expect("the named menu-music play is replaced")(c, b) };
}

/// The menu-music hooks' calls, 400 lines at most (finding why a mode's
/// music doesn't take over from a custom song, 2026-11-26).
fn trace_hook(what: &str) {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 400 {
        crate::diag::log(&format!(
            "jukebox hook: {what} (frame {})",
            crate::video::PRESENTED.load(std::sync::atomic::Ordering::Relaxed)
        ));
    }
}
