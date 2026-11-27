//! Draw calls. Each native draw entry point records the draw into the render
//! IR (`ir.rs`) for the host's renderer; nothing here touches a GPU.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use kernel::abi::{arg, ret};
use kernel::{diag, mem};
use ppc_rt::Ctx;

pub static DRAWS: AtomicU64 = AtomicU64::new(0);

/// Guest memory the game writes inline vertices into (`BeginVertices`).
const SCRATCH_SIZE: u32 = 4 << 20;

struct Inline {
    scratch: u32,
    pending: Option<(u32, u32, u32)>,
}

static INLINE: Mutex<Inline> = Mutex::new(Inline { scratch: 0, pending: None });

/// `D3DDevice_BeginVertices` (`sub_82CB1520(device, primitive, count, stride)
/// -> void*`): returns space for `count * stride` bytes of vertices, which the
/// caller fills before `EndVertices`. The original flushes the dirty state into
/// the command buffer and returns space inside it, after a draw packet; here
/// the state stays in the device's shadow and the space is a scratch buffer.
/// Kept from the original: the inline-vertex stride (in words) at `+0x3268`.
pub extern "C-unwind" fn begin_vertices(c: &mut Ctx, _: *mut u8) {
    let (dev, prim, count, stride) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3));
    mem::w8(dev + 0x3268, ((stride >> 2) & 0xFF) as u8);
    let size = count.checked_mul(stride).filter(|&n| n <= SCRATCH_SIZE).unwrap_or_else(|| {
        diag::fatal(&format!("BeginVertices: {count} vertices of {stride} bytes exceed the 4 MB inline buffer"))
    });
    let mut s = INLINE.lock().unwrap();
    if s.scratch == 0 {
        s.scratch = mem::kalloc(SCRATCH_SIZE, 128);
    }
    if s.pending.is_some() {
        diag::fatal("BeginVertices called twice without EndVertices");
    }
    s.pending = Some((prim, count, stride));
    let _ = size;
    ret(c, s.scratch);
}

/// `D3DDevice_EndVertices` (`sub_82CB1510(device)`): the inline vertices are
/// complete; this is the draw. The original only restores the command cursor.
pub extern "C-unwind" fn end_vertices(c: &mut Ctx, _: *mut u8) {
    let dev = arg(c, 0);
    let Some((prim, count, stride)) = INLINE.lock().unwrap().pending.take() else {
        diag::fatal("EndVertices without BeginVertices")
    };
    // `--trace draws` only: formatting the vertices every draw cost the render thread.
    if kernel::diag::TRACE_DRAWS.load(Ordering::Relaxed) {
        let verts: Vec<String> = (0..(count * stride / 4).min(24))
            .map(|i| {
                let v = mem::r32(INLINE.lock().unwrap().scratch + 4 * i);
                format!("{:.3}", f32::from_bits(v))
            })
            .collect();
        trace_bindings(dev, &format!("inline prim {prim} x{count} stride {stride} [{}]", verts.join(" ")));
    }
    let scratch = INLINE.lock().unwrap().scratch;
    crate::preview::record_inline(dev, prim, count, stride, scratch);
    crate::shaders::prepare(c, dev);
    crate::shaders::harvest(dev);
    crate::ir::inline_draw(dev, prim, count, stride, scratch);
    let n = DRAWS.fetch_add(1, Ordering::Relaxed) + 1;
    if n <= 3 || n.is_power_of_two() {
        diag::log(&format!(
            "draw #{n}: inline primitive type {prim}, {count} vertices of {stride} bytes"
        ));
    }
}

/// `D3DDevice_DrawIndexedVertices` (`sub_82CB19E0(device, primitive,
/// base_vertex, start_index, index_count)`).
///
/// The original flushes the dirty state into the command buffer (constants,
/// shader upload with the vertex-fetch patch, register blocks), clears the
/// dirty masks, and writes the draw packets (`VGT_INDX_OFFSET` = base vertex,
/// the index buffer's GPU address, size and endianness). Here the vertex
/// shader is patched as D3D's upload would, the dirty masks are cleared as the
/// original leaves them, and the draw goes to the renderer.
pub extern "C-unwind" fn draw_indexed_vertices(c: &mut Ctx, _: *mut u8) {
    let (dev, prim, base, start, count) = (arg(c, 0), arg(c, 1), arg(c, 2) as i32, arg(c, 3), arg(c, 4));
    crate::shaders::prepare(c, dev);
    crate::shaders::harvest(dev);
    crate::ir::indexed_draw(dev, prim, base, start, count);
    for off in [0x0, 0x8, 0x10, 0x18, 0x20] {
        mem::w64(dev + off, 0);
    }
    let n = DRAWS.fetch_add(1, Ordering::Relaxed) + 1;
    if n.is_power_of_two() {
        diag::log(&format!("draw #{n}: indexed primitive type {prim}, {count} indices from {start}, base vertex {base}"));
    }
}

/// `--trace draws`: the textures bound in the fetch-constant shadow, reported
/// the first time each combination is seen.
fn trace_bindings(dev: u32, what: &str) {
    if !kernel::diag::TRACE_DRAWS.load(Ordering::Relaxed) {
        return;
    }
    static SEEN: Mutex<Option<std::collections::HashSet<String>>> = Mutex::new(None);
    let mut parts = Vec::new();
    for slot in 0..32 {
        if let Some(f) = crate::texture::Fetch::from_device(dev, slot) {
            parts.push(format!(
                "t{slot}={}x{} {} @{:08X}{}",
                f.width,
                f.height,
                crate::texture::format_name(f.format),
                f.base,
                if f.tiled { " tiled" } else { "" }
            ));
        }
    }
    let key = format!("{what}: {}", parts.join(", "));
    let mut seen = SEEN.lock().unwrap();
    if seen.get_or_insert_with(Default::default).insert(key.clone()) {
        diag::log(&key);
    }
}
