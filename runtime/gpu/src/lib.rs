//! Native replacement of the game's Direct3D 9 library (XDK 2.0.21250), at its
//! API boundary.
//!
//! The game's D3D library builds Xenos command packets and hands them to the
//! GPU. None of that runs here: every D3D function that would touch the
//! hardware (device setup, command-buffer writes, draws, resolves, present,
//! fences) is replaced through `[[replace]]` in `config/wwe14.toml`, and this
//! crate supplies the native code. D3D's CPU-only bookkeeping (state setters
//! writing the device's shadow state, resource headers) stays recompiled, and
//! the native renderer reads that state at draw time. Command packets are
//! never built, parsed or executed. See `docs/d3d.md` for the map of the
//! library this is based on.

/// Lowers the calling host thread below the guest's (background work; see the kernel).
pub use kernel::thread::make_background;
mod commands;
mod device;
mod draw;
mod game;
mod present;
mod resolve;
pub use resolve::trace_resolves_for;
pub mod texture;
mod preview;
pub mod ir;
pub mod encode;
pub mod shaders;
mod lists;

use ppc_rt::GuestFn;

/// The native implementation of a replaced D3D function, by its config name.
pub fn resolve(name: &str) -> Option<GuestFn> {
    Some(match name {
        "D3DDevice_InitializeHardware" => device::initialize_hardware as GuestFn,
        "D3DDevice_Release" => device::release as GuestFn,
        "D3DDevice_WriteViewportScissor" => commands::write_viewport_scissor as GuestFn,
        "D3D_GuardedWaitA" => commands::guarded_wait_a as GuestFn,
        "D3D_GuardedWaitB" => commands::guarded_wait_b as GuestFn,
        "D3DDevice_Set2B24Bits" => commands::set_2b24_bits as GuestFn,
        "D3DDevice_SetShaderAllocation" => commands::set_shader_allocation as GuestFn,
        "D3DDevice_Swap" => present::swap as GuestFn,
        "D3DDevice_InsertCallback" => present::insert_callback as GuestFn,
        "D3DDevice_SynchronizeToPresentationInterval" => present::synchronize_to_presentation_interval as GuestFn,
        "D3DDevice_BeginVertices" => draw::begin_vertices as GuestFn,
        "D3DDevice_EndVertices" => draw::end_vertices as GuestFn,
        "D3DDevice_Resolve" => resolve::resolve as GuestFn,
        "D3DDevice_Clear" => resolve::clear as GuestFn,
        "D3DDevice_BeginTiling" => resolve::begin_tiling as GuestFn,
        "D3DDevice_EndTiling" => resolve::end_tiling as GuestFn,
        "D3DDevice_SetPredication" => commands::set_predication as GuestFn,
        "D3DDevice_DrawIndexedVertices" => draw::draw_indexed_vertices as GuestFn,
        "RenderLists_Record" => lists::record as GuestFn,
        "RenderLists_Walk" => lists::walk as GuestFn,
        "Game_SetFrameRate" => present::set_frame_rate as GuestFn,
        "Game_HalveFrameRate" => present::halve_frame_rate as GuestFn,
        "Game_RestoreFrameRate" => present::restore_frame_rate as GuestFn,
        "Game_CameraClock" => present::camera_clock as GuestFn,
        "Game_CameraUpdate" => present::camera_update as GuestFn,
        _ => return None,
    })
}
