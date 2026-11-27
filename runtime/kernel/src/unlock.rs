//! Unlock everything in the loaded save (the user's request, 2026-11-26),
//! through the game's own unlock function, once, when `WWE14_UNLOCK_ALL=1` is
//! set for a launch. The game then saves it at its next menu autosave.
//!
//! Found statically (2026-11-26): the unlock list has 196 items (`0xC4`,
//! hard-coded), described by the table at `[[0x836F2C20]]` (24-byte entries,
//! byte +0 "enabled"). `sub_822BCB68(id)` unlocks one (it needs the entry
//! enabled, sets the save byte at `0x83891878 + 0x80394 + id`, unlocks linked
//! ids and marks the save dirty); `sub_822BF598(id)` marks it seen, so the
//! menus show no NEW badge on every item. Ids 0, 1, 2 and 0xA5..0xA9 are
//! downloadable content and are left alone. 30 Years of WrestleMania match
//! progress is kept elsewhere and is not touched.

use std::sync::atomic::{AtomicBool, Ordering};

use ppc_rt::Ctx;

use crate::mem;

const SAVE: u32 = 0x8389_1878;
const ITEMS: u32 = 0xC4;
const UNLOCK: u32 = 0x822B_CB68;
const MARK_SEEN: u32 = 0x822B_F598;

static DONE: AtomicBool = AtomicBool::new(false);

/// The main save package (`SaveData.dat`) has been opened and closed: the
/// save is in memory. (Running before that, user77, the loading save
/// overwrote the unlocks.)
static SAVE_LOADED: AtomicBool = AtomicBool::new(false);

pub fn save_closed() {
    SAVE_LOADED.store(true, Ordering::Relaxed);
}

/// Called from the main thread each frame (the menu-music query): unlocks
/// everything once the save has loaded, if asked for.
pub fn maybe_run(c: &mut Ctx) {
    if DONE.load(Ordering::Relaxed) {
        return;
    }
    static WANTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*WANTED.get_or_init(|| std::env::var("WWE14_UNLOCK_ALL").is_ok_and(|v| v == "1")) {
        DONE.store(true, Ordering::Relaxed);
        return;
    }
    // The save is loaded (as the game's own autosave checks) and the item
    // table is read.
    let table = mem::r32(0x836F_2C20);
    let entries = if table != 0 { mem::r32(table) } else { 0 };
    if !SAVE_LOADED.load(Ordering::Relaxed) || mem::r32(SAVE) == 0 || entries == 0 || mem::r32(table + 4) < ITEMS {
        return;
    }
    let before = (0..ITEMS).filter(|&id| mem::r8(SAVE + 0x80394 + id) != 0).count();
    // The user's save has items unlocked already: until some show, the save
    // hasn't been read into memory yet (the game opens and closes the package
    // once just to check it, user78), and the load would undo the unlocks.
    if before == 0 {
        return;
    }
    // Checked about once a second from here on: the game reads the save into
    // this struct more than once (user81: the unlocks were gone again when
    // the user saved), so whenever an enabled item is locked, unlock again.
    static TICK: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if TICK.fetch_add(1, Ordering::Relaxed) % 60 != 0 {
        return;
    }
    let locked = (3..ITEMS).filter(|&id| !(0xA5..=0xA9).contains(&id) && mem::r8(entries + 0x18 * id) != 0 && mem::r8(SAVE + 0x80394 + id) == 0).count();
    if locked == 0 {
        return;
    }
    let result = c.r[3];
    let mut done = 0;
    for id in 3..ITEMS {
        if (0xA5..=0xA9).contains(&id) || mem::r8(entries + 0x18 * id) == 0 {
            continue;
        }
        crate::thread::call(c, UNLOCK, &[id as u64]);
        crate::thread::call(c, MARK_SEEN, &[id as u64]);
        done += 1;
    }
    c.r[3] = result;
    let after = (0..ITEMS).filter(|&id| mem::r8(SAVE + 0x80394 + id) != 0).count();
    crate::diag::log(&format!(
        "unlock: unlocked every item ({done} asked for; {before} were unlocked, now {after}); the save is marked to be written (dirty {})",
        mem::r8(SAVE + 0x80374)
    ));
}
