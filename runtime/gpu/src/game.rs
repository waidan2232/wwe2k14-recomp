//! Addresses in `default.xex` that the native D3D layer calls or reads. They
//! come from static analysis (see `docs/d3d.md`); the loader's image hash check
//! guarantees they refer to the executable they were found in.

/// The title's `XMemAlloc(size, attributes)` override, which D3D allocates
/// through (`XMemFree(ptr, attributes)` is `0x8225F778`).
pub const XMEM_ALLOC: u32 = 0x8225_FB30;

/// Import slot of `VdGlobalDevice`: holds the address of the kernel cell the
/// device pointer is published in.
pub const VD_GLOBAL_DEVICE_SLOT: u32 = 0x8200_0854;

/// D3D functions that only do CPU-side bookkeeping, called from native code.
pub mod d3d {
    /// Creates the front and back buffer surfaces, sets the render target and
    /// the initial state: `(device, present_params) -> BOOL`.
    pub const CREATE_FRAME_BUFFERS: u32 = 0x82CC_0B20;
    /// Default state: `(device)`.
    pub const SET_DEFAULT_STATE: u32 = 0x82CC_0150;
    /// Debugger hooks (none attached here) and `+0x5610..0x5618`: `(device)`.
    pub const INIT_DEBUG_HOOKS: u32 = 0x82CC_26B0;
    /// Fills `+0x5CF0` with 41 × -1: `(device)`.
    pub const RESET_TABLE_5CF0: u32 = 0x82CC_7600;
}
