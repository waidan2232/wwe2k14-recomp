//! Native replacements for the game's XMA audio paths.
//!
//! On the console, compressed XMA audio is decoded by a hardware block that
//! reads "contexts" (64-byte descriptors in memory) and is driven through
//! registers at `0x7FEAxxxx`. Two copies of the XDK's XMA layer drive it here:
//! XAudio2's (`0x82D28250`..) and the one Wwise uses (`0x8343A088`..). Under
//! the no-emulation rule that layer is never modelled. Decoding is replaced
//! one level higher, where the game asks for PCM, with a software XMA2 codec.
//!
//! Until a voice is replaced that way, nothing may reach the hardware layer:
//! `XMACreateContext` stops the game (in the kernel), and the replacements
//! below are the functions that touch the hardware without needing a context.

use kernel::diag;
use kernel::mem;
use ppc_rt::{Ctx, GuestFn};

/// The native implementation of a replaced audio function, by its config name.
pub fn resolve(name: &str) -> Option<GuestFn> {
    Some(match name {
        "Xma_ReadHardwareVersion_Wwise" | "Xma_ReadHardwareVersion_XAudio2" => read_hardware_version as GuestFn,
        "XAudio2_SyncXmaContexts" => xaudio2_sync_xma_contexts as GuestFn,
        _ => return None,
    })
}

/// `sub_83439FB8` / `sub_82D28250`: read the XMA engine's version register into
/// a global. In each XMA layer the only reader of that global is the layer's
/// own context creation, which never succeeds here (`XMACreateContext` stops
/// the game). Nothing to do.
extern "C-unwind" fn read_hardware_version(_c: &mut Ctx, _: *mut u8) {}

/// XAudio2's table of hardware XMA context groups (320 slots).
const XAUDIO2_XMA_SLOTS: u32 = 0x8394_8CD8;

/// `sub_82D28AD8`: called by XAudio2's processing loop every frame. It locks
/// the XMA engine, copies the state of each active hardware context back from
/// hardware memory and re-kicks it, then unlocks. With no hardware contexts
/// (none can be created) there is nothing to sync. If one ever exists, an XMA
/// voice is using the hardware path and must be replaced first.
extern "C-unwind" fn xaudio2_sync_xma_contexts(_c: &mut Ctx, _: *mut u8) {
    for i in 0..0x140 {
        let slot = mem::r32(XAUDIO2_XMA_SLOTS + 4 * i);
        if slot != 0 {
            diag::fatal(&format!(
                "XAudio2 has a hardware XMA context group (slot {i}, 0x{slot:08X}); \
                 its XMA voice must be replaced with software decoding"
            ));
        }
    }
}
