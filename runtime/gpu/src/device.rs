//! Device setup.

use kernel::abi::{arg, ret};
use kernel::mem;
use kernel::thread::call;
use ppc_rt::Ctx;

use crate::game::{self, d3d};

/// `D3DDevice_Release` (`sub_82CB22D0(device) -> count`): drops one reference
/// to the device. The count is the word at `+0x3C`, which `sub_82CB22A8`
/// (AddRef, recompiled) raises the same way.
///
/// The game's D3D helper objects hold a device reference and drop it when
/// they are destroyed: saving a created superstar rebuilds one (`sub_82BAFE98`
/// releases the old object through `sub_82D04490`). That is all this does in
/// practice. When the count reaches zero the original shuts the GPU down
/// (`sub_82CC0948`: the engines and the ring buffer) and frees the device. The
/// port has no hardware to shut down and no native device teardown, and the
/// game holds its own reference for as long as it runs, so reaching zero stops
/// here and says so.
pub extern "C-unwind" fn release(c: &mut Ctx, b: *mut u8) {
    let dev = arg(c, 0);
    let count = loop {
        let old = mem::r32(dev + 0x3C);
        // As the original's `lwarx`/`addi -1`/`stwcx.` loop leaves it in r3.
        let new = (old as u64).wrapping_sub(1);
        if unsafe { ppc_rt::cas32(b, dev + 0x3C, old, new as u32) } {
            break new;
        }
    };
    if count as u32 == 0 {
        panic!(
            "FATAL: D3DDevice_Release (sub_82CB22D0) dropped the last reference to the D3D device at 0x{dev:08X}. \
             The original now shuts the GPU down (sub_82CC0948) and frees the device; the port has no native \
             device teardown. Give it one."
        );
    }
    c.r[3] = count;
}

/// `D3DDevice_InitializeHardware` (`sub_82CC0DA0(device, present_params) ->
/// BOOL`), called once from `Direct3D_CreateDevice`.
///
/// The original interleaves CPU-side setup with hardware setup. This runs the
/// CPU-side steps in the original order (calling D3D's own code where the step
/// is bookkeeping) and leaves out every hardware step: the GPU engines and
/// interrupt handler, the ring buffer and `ME_INIT`, the prerecorded command
/// buffers, the GPU interrupt worker, register writes, EDRAM retraining and bin
/// masks. The command-buffer cursor (`+0x30`) therefore stays null, so any
/// recompiled D3D code that still tries to write commands faults at once and
/// names itself; that is how the next function to replace is found.
pub extern "C-unwind" fn initialize_hardware(c: &mut Ctx, _: *mut u8) {
    let (dev, params) = (arg(c, 0), arg(c, 1));

    // Critical sections and a list head.
    kernel::rtl::init_critical_section(dev + 0x3BF4, 0);
    kernel::rtl::init_critical_section(dev + 0x3C10, 0);
    mem::w8(dev + 0x5F8C, 0);
    mem::w32(dev + 0x5F90, 0);
    mem::w32(dev + 0x5F94, dev + 0x5F94);
    mem::w32(dev + 0x5F98, dev + 0x5F94);
    mem::w8(dev + 0x2B3E, mem::r8(dev + 0x2B3E) | 4);

    // From the engine start-up (`sub_82CC0770`), only this field is CPU state.
    mem::w32(dev + 0x42E0, 0);

    // Publish the device (`VdGlobalDevice`; the title process is not XAM).
    mem::w32(mem::r32(game::VD_GLOBAL_DEVICE_SLOT), dev);

    // Two device-owned buffers, zeroed.
    for (size, attrs, field) in [(0x80, 0xA580_0000u32, 0x2B10), (0x20, 0x9580_0000, 0x2B14)] {
        let p = call(c, game::XMEM_ALLOC, &[size, attrs as u64]) as u32;
        if p == 0 {
            return ret(c, 0x8007_000E); // E_OUTOFMEMORY, as the original returns
        }
        mem::fill(p, size as usize, 0);
        mem::w32(dev + field, p);
    }

    // `ExGetXConfigSetting(XCONFIG_USER_CATEGORY, XCONFIG_USER_VIDEO_FLAGS)`.
    mem::w32(dev + 0x42E4, 0x0004_0000);

    mem::w32(dev + 0x55D0, u32::MAX);
    mem::w32(dev + 0x55D4, u32::MAX);

    // Front and back buffers, render target and initial state.
    if call(c, d3d::CREATE_FRAME_BUFFERS, &[dev as u64, params as u64]) as u32 == 0 {
        return ret(c, 0);
    }
    call(c, d3d::SET_DEFAULT_STATE, &[dev as u64]);
    mem::w32(dev + 0x365C, 0);

    // Timer frequency and its reciprocal (as floats).
    let freq = kernel::time_frequency() as f32;
    mem::w32(dev + 0x55EC, freq.to_bits());
    mem::w32(dev + 0x55F0, (1.0f32 / freq).to_bits());

    call(c, d3d::INIT_DEBUG_HOOKS, &[dev as u64]);
    call(c, d3d::RESET_TABLE_5CF0, &[dev as u64]);
    ret(c, 1);
}
