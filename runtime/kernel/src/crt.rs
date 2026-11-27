//! Native versions of the game's hottest leaf routines (plan §3, Phase 7:
//! "native versions of hot leaf functions (memcpy/memset/string)").
//!
//! The game copies memory through three routines, all `(dst, src, len)` in
//! r3-r5, returning `dst`:
//! - `sub_82C79000`, the CRT's `memcpy` (1,691 call sites);
//! - `sub_82261340`, the game's VMX copy (zeroes destination cache lines
//!   with `dcbz128` ahead of 16-byte vector stores);
//! - `sub_82C79488`, a byte-then-word copy the VMX one uses for its tails.
//!
//! Recompiled, they ran a few bytes per step; the game's resource handling
//! between frames spends most of its time in them (frame hitches of 40-200
//! ms at camera cuts). Copying forward with line zeroing ahead means none of
//! them supports overlapping ranges, so a host `memmove` does what each does
//! for every call the game can make. Same results in guest memory; nothing of
//! the console's hardware is modelled.

use ppc_rt::{Ctx, GuestFn};

use crate::mem;

/// The native implementation of a replaced routine, by its config name.
pub fn resolve(name: &str) -> Option<GuestFn> {
    Some(match name {
        "Crt_Memcpy" | "Game_Memcpy" | "Crt_CopyTail" => memcpy as GuestFn,
        "Crt_Memset" => memset as GuestFn,
        "Game_FindEntryByName" => find_entry_by_name as GuestFn,
        "Game_BakeCubeDepth" => crate::bake::bake_cube_depth as GuestFn,
        "Game_BpeExpand" => crate::bpe::bpe_expand as GuestFn,
        "Game_JukeboxInput" => crate::jukebox::input as GuestFn,
        "Game_JukeboxRows" => crate::jukebox::rows as GuestFn,
        "Game_JukeboxPick" => crate::jukebox::pick as GuestFn,
        "Game_MenuMusicPlaying" => crate::jukebox::menu_music_playing as GuestFn,
        "Game_MenuMusicStop" => crate::jukebox::menu_music_stop as GuestFn,
        "Game_MenuMusicPlayNamed" => crate::jukebox::menu_music_play_named as GuestFn,
        "Game_ClothStep" => crate::cloth::cloth_step as GuestFn,
        "Game_EntranceMusicRequest" => crate::entrance::entrance_music_request as GuestFn,
        "Game_MusicJob" => crate::entrance::music_job as GuestFn,
        _ => return None,
    })
}

/// `sub_82BA9178(holder, name)`: the index of the entry called `name` in a
/// table of 32-byte entries (a name at +0, an extension at +0x10), or -1.
/// The table is `*(holder + 8)`: its entry count at +0, its entries at +0xC.
///
/// The original copies `name` into a 32-byte buffer (at most 31 characters,
/// `strncpy_s`) and lowercases it (`_strlwr_s`). Then, entry by entry, it
/// builds `"name.ext"`, lowercases that unless it is 32 characters or longer
/// (`_strlwr_s` refuses), and compares with `strcmp`. A string of 32 or more
/// characters can never equal the query (at most 31), so an entry matches
/// exactly when `"name.ext"` is at most 31 characters and equals the query
/// with A-Z lowercased. Recompiled, the copying and lowercasing of every
/// entry took the render thread 60-75 ms when an entrance starts.
/// Same result here, compared in place. The original's scratch buffers are
/// below its stack frame, dead after it returns.
extern "C-unwind" fn find_entry_by_name(c: &mut Ctx, _: *mut u8) {
    let (holder, name) = (c.r[3] as u32, c.r[4] as u32);
    if name == 0 {
        panic!("FATAL: Game_FindEntryByName (0x82BA9178) called with a null name (holder 0x{holder:08X}): the original reads a stale stack buffer");
    }
    let lower = |b: u8| if b.is_ascii_uppercase() { b + 32 } else { b };
    let mut query = [0u8; 31];
    let mut qlen = 0;
    while qlen < 31 {
        let b = mem::r8(name.wrapping_add(qlen as u32));
        if b == 0 {
            break;
        }
        query[qlen] = lower(b);
        qlen += 1;
    }
    let query = &query[..qlen];
    // `"name.ext"` (unbounded reads, as the original's copy loops) against the query.
    let matches = |entry: u32| {
        let mut k = 0usize;
        let part = |start: u32, k: &mut usize| -> bool {
            let mut i = 0u32;
            loop {
                let b = mem::r8(start.wrapping_add(i));
                if b == 0 {
                    return true;
                }
                if *k >= query.len() || lower(b) != query[*k] {
                    return false;
                }
                *k += 1;
                i += 1;
            }
        };
        if !part(entry, &mut k) {
            return false;
        }
        if k >= query.len() || query[k] != b'.' {
            return false;
        }
        k += 1;
        part(entry.wrapping_add(0x10), &mut k) && k == query.len()
    };
    let table = mem::r32(holder.wrapping_add(8));
    let mut found = u64::MAX;
    if table != 0 {
        let count = mem::r32(table) as i32;
        let entries = mem::r32(table.wrapping_add(0xC));
        for i in 0..count.max(0) {
            if matches(entries.wrapping_add(32 * i as u32)) {
                found = i as u64;
                break;
            }
        }
    }
    c.r[3] = found;
}

/// `memset(dst, value, len)` (`sub_82C79530`, the CRT's; 811 functions call
/// it): stores the low byte of `value` `len` times and returns `dst` (r3 is
/// left as it came in). The original stores bytes up to a word boundary, then
/// the byte replicated into words, four and then one at a time, then the last
/// bytes: the same bytes in guest memory as one host fill.
extern "C-unwind" fn memset(c: &mut Ctx, _: *mut u8) {
    let (dst, value, len) = (c.r[3] as u32, c.r[4] as u8, c.r[5] as u32);
    if len != 0 {
        unsafe { core::ptr::write_bytes(mem::base().add(dst as usize), value, len as usize) };
    }
}

/// `memcpy(dst, src, len)`: copies `len` bytes and returns `dst` (r3 is left
/// as it came in).
extern "C-unwind" fn memcpy(c: &mut Ctx, _: *mut u8) {
    let (dst, src, len) = (c.r[3] as u32, c.r[4] as u32, c.r[5] as u32);
    if len != 0 {
        unsafe { core::ptr::copy(mem::base().add(src as usize), mem::base().add(dst as usize), len as usize) };
    }
}
