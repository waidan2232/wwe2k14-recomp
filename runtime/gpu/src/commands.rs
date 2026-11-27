//! D3D functions that only turn state the device already shadows into GPU
//! commands. `analyse effects` shows they store nothing on the device except
//! the command-buffer cursor (`+0x30`): the state they would send is already in
//! the device's shadow registers, where the native renderer reads it at draw
//! time. Their replacements therefore do nothing.

use kernel::abi::arg;
use kernel::mem;
use kernel::thread::call;
use ppc_rt::Ctx;

/// The D3D global that sends `sub_82CADA88` and `sub_82CADBF8` down their
/// command-packet branch (a `WAIT_REG_MEM`: the GPU waits on memory) when it
/// is set. Nothing in the port sets it, and in a whole session both functions
/// ran only their CPU-side branch (call tracing, 2026-11-25): `sub_82CADA88`
/// marks state dirty or stores a value at `+0x2FE0`; `sub_82CADBF8` calls it.
const WAIT_PACKET_MODE: u32 = 0x8394_6FCC;

/// Runs the original of a function whose only command packet sits behind
/// `WAIT_PACKET_MODE`, or fails loudly if that branch would be taken.
fn without_wait_packet(c: &mut Ctx, b: *mut u8, address: u32, name: &str) {
    let mode = mem::r32(WAIT_PACKET_MODE);
    if mode != 0 {
        panic!(
            "FATAL: {name} (sub_{address:08X}) would write a WAIT_REG_MEM command packet: D3D global {WAIT_PACKET_MODE:08X} is {mode:#x}. \
             Give it a native version."
        );
    }
    unsafe { ppc_rt::original(address).expect("the function is replaced")(c, b) };
}

/// `sub_82CADA88(device, a, b, flags)`: see `WAIT_PACKET_MODE`.
pub extern "C-unwind" fn guarded_wait_a(c: &mut Ctx, b: *mut u8) {
    without_wait_packet(c, b, 0x82CA_DA88, "D3D_GuardedWaitA");
}

/// `sub_82CADBF8(device, ...)`: see `WAIT_PACKET_MODE`.
pub extern "C-unwind" fn guarded_wait_b(c: &mut Ctx, b: *mut u8) {
    without_wait_packet(c, b, 0x82CA_DBF8, "D3D_GuardedWaitB");
}

/// `sub_82CA8CD0(device)`: writes the window scissor and viewport registers
/// (shadowed at `+0x28C4/+0x28C8` and `+0x3210..+0x3344`) as packets.
/// Called after every scissor/viewport change (`sub_82CAB280`).
pub extern "C-unwind" fn write_viewport_scissor(_c: &mut Ctx, _: *mut u8) {}

/// `sub_82CBAEE0(device, value)`: puts `value` into bits 12–13 of the shadow
/// word at `+0x2B24`, then writes that word (and a companion register) as
/// packets. Native: the shadow update only.
pub extern "C-unwind" fn set_2b24_bits(c: &mut Ctx, _: *mut u8) {
    let (dev, v) = (arg(c, 0), arg(c, 1));
    mem::w32(dev + 0x2B24, (mem::r32(dev + 0x2B24) & !0x3000) | ((v << 12) & 0x3000));
}

/// `sub_82CB0F58(device, flags, a, b)`: sets the shader register-allocation
/// shadow word at `+0x2B28` (two 7-bit fields from `a` and `b`, both 0x40 when
/// neither is given; all zero when `flags & 1`), writes indirect-buffer packets
/// for the shader microcode, then marks the device state dirty
/// (`sub_82CB04D0`, CPU-only, run as recompiled code). Native: the shadow update
/// and the dirty marking.
pub extern "C-unwind" fn set_shader_allocation(c: &mut Ctx, _: *mut u8) {
    let (dev, flags, mut a, mut b) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3));
    if a == 0 && b == 0 {
        a = 0x40;
        b = 0x40;
    }
    let v = if flags & 1 != 0 {
        0
    } else {
        let packed = (b & !0x7F00) | ((a << 8) & 0x7F00);
        let r = packed.rotate_left(4);
        (mem::r32(dev + 0x2B28) & !0x7_F7F0) | (r & 0x7_F7F0)
    };
    mem::w32(dev + 0x2B28, v);
    call(c, MARK_STATE_DIRTY, &[dev as u64]);
}

/// `sub_82CB04D0(device)`: sets the dirty bits that make the next draw resend
/// all state.
const MARK_STATE_DIRTY: u32 = 0x82CB_04D0;

/// D3D's predication state, as `D3DDevice_SetPredication` (`sub_82CB7770(device,
/// mask)`) keeps it. The original then writes the mask as a `SET_BIN_MASK_LO`
/// packet (type 3, opcode `0x60`); under predicated tiling, a draw then runs
/// only in the tiles whose bins the mask selects (`EndTiling` replays tile i
/// with `3 << 2i`; see `ir::Tile`). Hi-Z is off while a mask is set:
/// - `mask == 0` (no predication): Hi-Z stays usable (`+0x2B3C` bit 0) unless
///   bit 2 or 3 of `+0x2B3C` or `+0x3113` forbid it; with bit 4 it is usable
///   outright, else with bit 5 only if the five words at `+0x3330` match
///   `+0x3210` (or those are zero). `+0x331C` = -1, `+0x3324` = 0.
/// - otherwise: Hi-Z off (`+0x2B3C` bit 0 clear), `+0x331C` = mask,
///   `+0x3324` = 1.
///
/// Both: `+0x3320` = 0, `+0x2B34` = mask, `+0x2B38` = 0.
pub fn predication_state(dev: u32, value: u32) {
    let flags = mem::r8(dev + 0x2B3C);
    if value == 0 {
        mem::w32(dev + 0x3324, 0);
        let usable = if flags & 0x0C != 0 || mem::r8(dev + 0x3113) != 0 {
            false
        } else if flags & 0x10 != 0 {
            true
        } else if flags & 0x20 == 0 {
            false
        } else {
            (0..5).all(|i| {
                let (cur, hiz) = (mem::r32(dev + 0x3210 + 4 * i), mem::r32(dev + 0x3330 + 4 * i));
                hiz == cur || cur == 0
            })
        };
        mem::w8(dev + 0x2B3C, (flags & !1) | usable as u8);
        mem::w32(dev + 0x331C, u32::MAX);
    } else {
        mem::w32(dev + 0x331C, value);
        mem::w32(dev + 0x3324, 1);
        mem::w8(dev + 0x2B3C, flags & !1);
    }
    mem::w32(dev + 0x3320, 0);
    mem::w32(dev + 0x2B34, value);
    mem::w32(dev + 0x2B38, 0);
}

/// `D3DDevice_SetPredication` (`sub_82CB7770(device, mask)`): the state
/// (`predication_state`), then a packet.
pub extern "C-unwind" fn set_predication(c: &mut Ctx, _: *mut u8) {
    predication_state(arg(c, 0), arg(c, 1));
}
