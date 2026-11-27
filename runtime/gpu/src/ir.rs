//! The render IR (plan §2.4 item 3). The native draw entry points turn D3D's
//! shadow state into a list of draws; `Swap` closes the frame and hands it to
//! the host's renderer over a channel. Nothing here touches a GPU: this is
//! the data at the API boundary, converted to host form (little-endian
//! floats, decoded textures, the guest vertex bytes a draw reads).
//!
//! Shaders are named by identity (`xenos_shader_recomp::ucode::identity`) and
//! were translated ahead of time; the renderer looks them up and never
//! translates anything.

// foldhash: the capture looks up shaders, textures and vertex spans for every draw.
use foldhash::{HashMap, HashMapExt};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};

use kernel::{diag, mem};
use xenos_shader_recomp::ucode;

use crate::shaders::{self, Stage};
use crate::texture::{self, Fetch};

/// One presented frame: the draws and resolves in the order the game issued
/// them, and the front buffer it presented.
#[derive(Default)]
pub struct Frame {
    pub number: u64,
    pub commands: Vec<Command>,
    /// The presented front buffer's texture fetch constant.
    pub front: Option<[u32; 6]>,
    /// Rendered but not shown: the window keeps the previous picture (the
    /// blended in-between frame before a camera cut in a half-rate scene,
    /// `present::camera_update`).
    pub hold: bool,
}

/// Frames still to hold (`Frame::hold`), counted down by `end_frame`.
pub static HOLD_FRAMES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub enum Command {
    Draw(Draw),
    Resolve(Resolve),
    Clear(Clear),
}

/// A clear (`D3DDevice_Clear`) of the bound targets.
pub struct Clear {
    /// D3D 360 `D3DCLEAR_*`: bits 0-3 colour targets 0-3, 0x10 depth, 0x20
    /// stencil (D3D's packet builder `sub_82CB4B00` drops bits 4-7 without a
    /// depth buffer, and `sub_82CB5180` uses the Z value only with 0x10).
    pub flags: u32,
    /// Rectangles x1, y1, x2, y2 (none: the whole target).
    pub rects: Vec<[u32; 4]>,
    /// RGBA.
    pub color: [f32; 4],
    pub z: f32,
    pub stencil: u32,
    pub surface: [u32; 3],
}

/// Records a clear.
pub fn clear(c: Clear) {
    let mut guard = BUILDER.lock().unwrap();
    let b = guard.get_or_insert_with(new_builder);
    if b.sink.is_some() {
        b.frame.commands.push(Command::Clear(c));
    }
}

impl Frame {
    pub fn draws(&self) -> impl Iterator<Item = &Draw> {
        self.commands.iter().filter_map(|c| if let Command::Draw(d) = c { Some(d) } else { None })
    }
}

/// A resolve (`D3DDevice_Resolve`): copy part of a render target (colour
/// target `flags & 3`, or depth with `RESOLVE_DEPTHSTENCIL`) into a texture,
/// then optionally clear the target.
pub struct Resolve {
    pub flags: u32,
    /// Source rectangle x1, y1, x2, y2 (whole surface when absent).
    pub rect: Option<[u32; 4]>,
    /// The destination texture's fetch constant (none: clear only).
    pub dest: Option<[u32; 6]>,
    pub dest_point: [u32; 2],
    pub level: u32,
    pub slice: u32,
    pub clear_color: [f32; 4],
    pub clear_z: f32,
    pub clear_stencil: u32,
    /// The source surface: `RB_SURFACE_INFO`, `RB_COLOR_INFO`, `RB_DEPTH_INFO`.
    pub surface: [u32; 3],
}

/// Resolve flags (XDK `D3DRESOLVE_*`): colour target index in bits 0-1.
pub const RESOLVE_DEPTHSTENCIL: u32 = 0x4;
pub const RESOLVE_CLEAR_TARGET: u32 = 0x100;
pub const RESOLVE_CLEAR_DEPTH: u32 = 0x200;

/// The shadowed surface registers (`0x2000`-`0x2002`) that say which render
/// target and depth buffer are bound.
pub fn surface_regs(dev: u32) -> [u32; 3] {
    let mut r: [u32; 3] = core::array::from_fn(|i| mem::r32(dev + 0x2880 + 4 * i as u32));
    r[0] = tiled_surface_info(dev, r[0]);
    r
}

/// `RB_SURFACE_INFO` as the renderer should see it. Under predicated tiling
/// (`BeginTiling` .. `EndTiling`) the EDRAM surface holds one tile, and the
/// console replays the commands once per tile at that tile's screen offset.
/// The renderer draws the pass once, so the surface it draws into must cover
/// the whole screen extent (`+0x34F4`): the pitch becomes the extent's width.
pub fn tiled_surface_info(dev: u32, info: u32) -> u32 {
    if mem::r32(dev + 0x3344) == 0 {
        return info;
    }
    let extent = mem::r32(dev + 0x34F4).min(0x3FFF);
    if extent > info & 0x3FFF { (info & !0x3FFF) | extent } else { info }
}

/// Records a resolve.
pub fn resolve(r: Resolve) {
    let mut guard = BUILDER.lock().unwrap();
    let b = guard.get_or_insert_with(new_builder);
    if let Some(d) = r.dest.and_then(texture::Fetch::decode) {
        // The guest memory there keeps what the CPU last wrote (the image is the
        // renderer's): its fingerprint tells a later CPU write apart.
        let range = texture::guest_range(&d).unwrap_or((0, 0));
        // (A range under a diagnostic watch is not read: that would end the watch.)
        if !mem::watched(range.0) {
            b.resolve_dests.insert(texture::physical(d.base), (range.0, range.1, fingerprint(range)));
        }
    }
    if b.sink.is_some() {
        b.frame.commands.push(Command::Resolve(r));
    }
}

// ---- Resolved images the game reads back ------------------------------------------
//
// A resolve leaves its image with the renderer; guest memory at the
// destination is not written. The game reads a few of them on the CPU (a
// created superstar's picture, which it compresses into the save). For those
// the native resolve watches the destination's memory (`resolve.rs`), and
// the first read asks the renderer for the pixels here.

/// A request for a resolved image's pixels.
pub struct Readback {
    /// The destination texture's physical address: the renderer's key.
    pub phys: u32,
    /// The request is served once the renderer has finished a frame after
    /// this one (the frame count when the resolve was recorded).
    pub after_frame: u64,
    reply: SyncSender<Result<ReadImage, String>>,
}

/// A resolved image at its guest size: width, height, RGBA8 rows.
pub type ReadImage = (u32, u32, Vec<u8>);

impl Readback {
    /// Answers with the image, or with why there is none.
    pub fn reply(self, image: Result<ReadImage, String>) {
        let _ = self.reply.send(image);
    }
}

static READBACKS: Mutex<Vec<Readback>> = Mutex::new(Vec::new());
/// Requests waiting, so the renderer's idle check costs one load.
static READBACKS_WAITING: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// The last frame the renderer said it had finished (`take_readbacks`).
static READBACK_DONE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The requests the renderer can serve now that it has finished frame `done`.
/// The renderer asks after every frame and, while no frame arrives, every few
/// milliseconds: the game's frames stop while its reader waits (the superstar
/// picture's reader holds the game for the length of the copy), so a renderer
/// that only looked after a frame would never answer.
pub fn take_readbacks(done: u64) -> Vec<Readback> {
    use std::sync::atomic::Ordering;
    READBACK_DONE.store(done, Ordering::Relaxed);
    if READBACKS_WAITING.load(Ordering::Acquire) == 0 {
        return Vec::new();
    }
    let mut q = READBACKS.lock().unwrap();
    let (ready, later): (Vec<Readback>, Vec<Readback>) = std::mem::take(&mut *q).into_iter().partition(|r| done > r.after_frame);
    READBACKS_WAITING.store(later.len() as u32, Ordering::Release);
    *q = later;
    ready
}

/// Asks the renderer for a resolved image and waits for it. The error says
/// why there is none: no renderer answered in time, or what it found instead.
pub fn request_readback(phys: u32, after_frame: u64) -> Result<ReadImage, String> {
    use std::sync::atomic::Ordering;
    let (tx, rx) = sync_channel(1);
    {
        let mut q = READBACKS.lock().unwrap();
        q.push(Readback { phys, after_frame, reply: tx });
        READBACKS_WAITING.store(q.len() as u32, Ordering::Release);
    }
    match rx.recv_timeout(std::time::Duration::from_secs(3)) {
        Ok(answer) => answer,
        Err(_) => {
            // Withdrawn, so a late answer isn't built for nobody.
            let mut q = READBACKS.lock().unwrap();
            q.retain(|r| !(r.phys == phys && r.after_frame == after_frame));
            READBACKS_WAITING.store(q.len() as u32, Ordering::Release);
            Err(format!(
                "no renderer answered within 3 s (the resolve was recorded during frame {after_frame}; the last frame the renderer finished is {})",
                READBACK_DONE.load(Ordering::Relaxed)
            ))
        }
    }
}

/// The image of a resolve destination is in guest memory now (read back for
/// the game): its fingerprint is the memory's, so the next draw that samples
/// it doesn't take that for a texture the CPU loaded there.
pub fn resolved_memory_written(phys: u32, virt: u32, len: u32) {
    if let Some(b) = BUILDER.lock().unwrap().as_mut() {
        b.resolve_dests.insert(phys, (virt, len, fingerprint((virt, len))));
    }
}

/// One draw: everything the renderer needs, copied out of guest memory.
pub struct Draw {
    /// Shader identities.
    pub vs: u64,
    pub ps: u64,
    /// Primitive type (D3D/Xenos numbering: 4 list, 5 fan, 6 strip, 13 quads...).
    pub primitive: u32,
    /// Vertices drawn (indices, for an indexed draw).
    pub vertex_count: u32,
    /// An indexed draw's indices (as the CPU wrote them) and the base vertex
    /// added to each (`VGT_INDX_OFFSET`).
    pub indices: Option<Arc<Vec<u32>>>,
    pub base_vertex: i32,
    /// Float constants, 256 per stage, in shared blocks (`Consts`).
    pub vs_consts: Arc<Consts>,
    pub ps_consts: Arc<Consts>,
    /// Bool constants (8 words) then loop constants (32 words).
    pub control: [u32; 40],
    /// Fetch constants (32 × 6 words), with inline-vertex overrides applied.
    pub fetch: [u32; 192],
    /// Vertex fetch descriptors from D3D's patched microcode, in its program
    /// order, keyed (`ucode::vertex_fetch_descriptors`; mini fetches made full).
    /// The renderer matches them to the translated shader's fetches by key.
    pub vfetch: Vec<(u32, [u32; 4])>,
    /// The guest memory the vertex streams cover: each range's physical word
    /// address and its bytes as stored (at most 8 ranges).
    pub vertex_ranges: Vec<(u32, Arc<Vec<u8>>)>,
    /// The ranges (physical word address, words) still to be read when the
    /// frame ends (`late_vertices`); `vertex_ranges` is filled from them.
    pub vertex_spans: Vec<(u32, u32)>,
    /// Textures in the fetch-constant slots, decoded.
    pub textures: Vec<(u32, Arc<Texture>)>,
    /// Until the frame ends: the slots whose textures are still to be read
    /// (slot, index into the frame's texture jobs), moved into `textures` then.
    pending_textures: Vec<(u32, usize)>,
    /// Render-state registers (register number, value) the renderer interprets.
    pub regs: Vec<(u32, u32)>,
    /// Under predicated tiling with a predication mask set (`SetPredication`),
    /// the tiles the draw runs in; `None` when it runs everywhere.
    pub tiles: Option<Vec<Tile>>,
}

/// A tile of predicated tiling: its screen rectangle (x1, y1, x2, y2) and
/// the origin of its EDRAM surface (`x1`, `y1` rounded down to 32), which is
/// the window offset D3D gives the tile's replay.
#[derive(Clone, Copy, Debug)]
pub struct Tile {
    pub rect: [u32; 4],
    pub origin: [u32; 2],
}

/// The tiles a draw runs in. While tiling (`+0x3344` tiles), a predicated
/// draw (`+0x3324` = 1) runs only in the tiles whose bins its mask
/// (`+0x331C`) selects: `EndTiling` replays tile i with bins `3 << 2i`.
fn predicated_tiles(dev: u32) -> Option<Vec<Tile>> {
    let count = mem::r32(dev + 0x3344).min(15);
    if count == 0 || mem::r32(dev + 0x3324) != 1 {
        return None;
    }
    let mask = mem::r32(dev + 0x331C);
    Some(
        (0..count)
            .filter(|i| (mask >> (2 * i)) & 3 != 0)
            .map(|i| Tile {
                rect: core::array::from_fn(|k| mem::r32(dev + 0x3348 + 16 * i + 4 * k as u32)),
                origin: [mem::r32(dev + 0x3438 + 8 * i), mem::r32(dev + 0x343C + 8 * i)],
            })
            .collect(),
    )
}

/// A decoded texture: `layers` images of `width` x `height`, one after another
/// (a cube map has six): RGBA8, RGBA 16-bit floats (`wide`), or the BC blocks
/// as stored (`bc`).
pub struct Texture {
    pub width: u32,
    pub height: u32,
    pub layers: u32,
    /// RGBA8 unsigned fractions, or RGBA 16-bit floats when `wide`
    /// (`texture::Fetch::wide`), every mip level one after another (base
    /// first), with each level's size; none for a format with no conversion
    /// yet. Filled by a converter thread after the frame is captured (`pixels`).
    rgba: std::sync::OnceLock<Option<(Vec<u8>, Vec<(u32, u32)>)>>,
    pub wide: bool,
    /// Uploaded as BC blocks (`texture::bc_passthrough`: 1, 2, 3 or 5) instead of decoded.
    pub bc: Option<u8>,
    /// The guest bytes and how to read them, until a thread takes the conversion
    /// (a converter, or whoever needs the texels first: `converted`).
    pending: Mutex<Option<(texture::Fetch, Arc<Vec<u8>>)>>,
    /// The contents' key: address, layout and content hash.
    pub key: u64,
    /// The texture slot: guest address and layout, not contents. A slot whose
    /// contents change (a video) keeps its slot.
    pub slot: u64,
}

impl Texture {
    /// The converted texels, waiting for the converter if it is still busy.
    pub fn pixels(&self) -> Option<&[u8]> {
        self.converted().as_ref().map(|(px, _)| &px[..])
    }

    /// Each mip level's size (the base first), as converted; one level when the
    /// texture has no mips the capture reads.
    pub fn levels(&self) -> &[(u32, u32)] {
        self.converted().as_ref().map_or(&[], |(_, l)| &l[..])
    }

    /// The conversion: done here when no converter has taken it yet, else
    /// waited for. The converters run at the lowest priority, and in a heavy
    /// match the game's threads starved them while the renderer waited: 0.6 s
    /// for a scene's 58 new textures (2026-11-25).
    fn converted(&self) -> &Option<(Vec<u8>, Vec<(u32, u32)>)> {
        if self.rgba.get().is_none() {
            let job = self.pending.lock().unwrap().take();
            if let Some((f, raw)) = job {
                let _ = self.rgba.set(converter::conversion(self, &f, &raw));
            }
        }
        self.rgba.wait()
    }
}

/// Device shadow layout (`docs/d3d.md`).
const VS_CONSTS: u32 = 0x780;
const PS_CONSTS: u32 = 0x1780;
const CONTROL: u32 = 0x2780;
const FETCH: u32 = 0x480;
/// Shadowed register blocks: (first register, device offset, count).
const REG_BLOCKS: [(u32, u32, u32); 5] =
    [(0x2000, 0x2880, 19), (0x2100, 0x28CC, 21), (0x2180, 0x2920, 5), (0x2200, 0x2934, 12), (0x2300, 0x29B8, 4)];

struct Builder {
    frame: Frame,
    sink: Option<SyncSender<Frame>>,
    /// Converted textures, one per guest address and layout (`TextureJob::slot_key`):
    /// the content hash it was converted from, the texture, and the frame that last
    /// used it. A texture whose contents change (video) replaces its own entry.
    textures: HashMap<u64, (u64, Arc<Texture>, u64)>,
    /// This frame's textures to read when it ends (`end_frame`), in parallel,
    /// and the job for each cache key (`TextureJob::slot_key`).
    jobs: Vec<TextureJob>,
    job_slots: HashMap<u64, usize>,
    /// The jobs' content hashes, computed off this thread (`hasher`).
    hashes: Arc<hasher::Batch>,
    /// Textures whose contents changed, with the frame they last did: read at
    /// the draw while that was under 120 frames ago (`texture_job`).
    dynamic: HashMap<u64, u64>,
    /// The number of the frame being captured (the last one ended, plus one).
    frame_count: u64,
    /// The previous draw's float constants per stage (vertex, pixel): reused
    /// while the guest bytes and the shader are unchanged.
    last_consts: [Option<StageConsts>; 2],
    /// Each bound shader's literal constants (`literals`), read once per frame:
    /// a shader object is not freed and replaced within a frame.
    literals: HashMap<(u32, u32, u64), Arc<Literals>>,
    /// Constant blocks with a shader's literals loaded over them, this frame:
    /// (block's address, shader, block number) -> (that block, the result).
    overlays: HashMap<(usize, (u32, u32, u64), usize), (Arc<ConstBlock>, Arc<ConstBlock>)>,
    /// Vertex spans read this frame, with a sample of their memory (`sample_print`).
    frame_spans: HashMap<(u32, u32), (u64, Option<(u32, Arc<Vec<u8>>)>)>,
    /// Index lists converted this frame, by address, count and width, with a
    /// sample of their memory (`indexed_draw`).
    frame_indices: HashMap<(u32, u32, bool), (u64, Arc<Vec<u32>>)>,
    /// What the draws need from each bound shader's microcode, by address and
    /// size, with the hash of the code it was read from (D3D patches vertex
    /// fetches in place, and freed shaders' addresses are reused).
    shaders: HashMap<(u32, u32, u64), Arc<ShaderInfo>>,
    /// Textures sampled this frame, by fetch constant: the job that reads it
    /// (none: nothing to read, as for a resolve's destination). Within a frame
    /// an unchanged fetch constant is taken to name unchanged texels.
    frame_textures: HashMap<[u32; 6], Option<usize>>,
    /// Physical addresses resolves have written, with the guest range there
    /// and its fingerprint at the resolve: the renderer samples the rendered
    /// image there, so guest memory is not captured, until the CPU rewrites
    /// it (a new texture loaded into memory a render target used).
    resolve_dests: HashMap<u32, (u32, u32, u64)>,
    /// Sampled textures reported as not captured (base, reason).
    missing: std::collections::HashSet<(u32, String)>,
}

struct ShaderInfo {
    identity: u64,
    /// Vertex fetch descriptors (`ucode::vertex_fetch_descriptors`).
    vfetch: Vec<(u32, [u32; 4])>,
    /// Texture fetch constants sampled.
    tfetch: Vec<u32>,
}

static BUILDER: Mutex<Option<Builder>> = Mutex::new(None);

/// Called after each frame reaches the channel (the window wakes on it).
static ON_FRAME: std::sync::OnceLock<Box<dyn Fn() + Send + Sync>> = std::sync::OnceLock::new();

/// Registers the host's "a frame is ready" callback (once).
pub fn on_frame(f: impl Fn() + Send + Sync + 'static) {
    let _ = ON_FRAME.set(Box::new(f));
}

/// The host's end of the frame channel: two frames deep. Every frame is
/// delivered; the game waits in `end_frame` while the renderer is two frames
/// behind (`VdSwap` still paces frames to the display).
pub fn subscribe() -> Receiver<Frame> {
    let (tx, rx) = sync_channel(2);
    let mut b = BUILDER.lock().unwrap();
    b.get_or_insert_with(new_builder).sink = Some(tx);
    rx
}

fn new_builder() -> Builder {
    Builder {
        frame: Frame::default(),
        sink: None,
        textures: HashMap::new(),
        jobs: Vec::new(),
        job_slots: HashMap::new(),
        hashes: Default::default(),
        dynamic: HashMap::new(),
        frame_count: 0,
        last_consts: [None, None],
        literals: HashMap::new(),
        overlays: HashMap::new(),
        frame_spans: HashMap::new(),
        frame_indices: HashMap::new(),
        shaders: HashMap::new(),
        frame_textures: HashMap::new(),
        resolve_dests: Default::default(),
        missing: Default::default(),
    }
}

/// A fast hash of `len` bytes of guest memory, read in place: four
/// independent rotate-xor-multiply lanes over 8-byte words. A cache key for
/// "these bytes changed", not a checksum.
fn hash_guest(a: u32, len: usize, seed: u64) -> u64 {
    hash_bytes(unsafe { std::slice::from_raw_parts(mem::base().add(a as usize), len) }, seed)
}

/// `hash_guest` over bytes already copied out (the same key for the same bytes).
fn hash_bytes(bytes: &[u8], seed: u64) -> u64 {
    const K: [u64; 4] = [0x9E37_79B9_7F4A_7C15, 0xC2B2_AE3D_27D4_EB4F, 0x1656_67B1_9E37_79F9, 0x85EB_CA77_C2B2_AE63];
    let len = bytes.len();
    let p = bytes.as_ptr();
    let mut h = [seed ^ K[0], seed.rotate_left(16) ^ K[1], seed.rotate_left(32) ^ K[2], seed.rotate_left(48) ^ K[3]];
    let blocks = len / 32;
    for i in 0..blocks {
        for (l, h) in h.iter_mut().enumerate() {
            let w = unsafe { (p.add(32 * i + 8 * l) as *const u64).read_unaligned() };
            *h = (h.rotate_left(23) ^ w).wrapping_mul(K[l]);
        }
    }
    for i in 32 * blocks..len {
        h[0] = (h[0].rotate_left(23) ^ unsafe { *p.add(i) } as u64).wrapping_mul(K[0]);
    }
    let mut r = len as u64;
    for (l, h) in h.iter().enumerate() {
        r = (r ^ h).wrapping_mul(K[(l + 1) % 4]).rotate_left(31);
    }
    r ^ (r >> 29)
}

/// The microcode facts draws need, read once per shader version.
///
/// Cached by address, length and contents: D3D patches a vertex shader's
/// fetches in place for each vertex layout it is drawn with, so one shader
/// alternates between versions draw by draw. Keeping only the last version
/// made most draws of a busy scene re-identify their shaders (a quarter of
/// the render thread's work in an entrance).
fn shader_info(b: &mut Builder, addr: u32, len: u32) -> Arc<ShaderInfo> {
    let h = hash_guest(addr, len as usize, 0);
    if let Some(info) = b.shaders.get(&(addr, len, h)) {
        return info.clone();
    }
    let code = mem::read(addr, len as usize);
    let s = ucode::slots(&code);
    let info = Arc::new(ShaderInfo {
        identity: ucode::identity(&code),
        vfetch: ucode::vertex_fetch_descriptors(&s),
        tfetch: ucode::texture_fetch_constants(&s),
    });
    if b.shaders.len() > 16384 {
        b.shaders.clear();
    }
    b.shaders.insert((addr, len, h), info.clone());
    info
}

/// The GPU (physical) address of a CPU address, as D3D computes it.
pub fn gpu_address(a: u32) -> u32 {
    (a & 0x1FFF_FFFF) + if a >= 0xE000_0000 { 0x1000 } else { 0 }
}

/// Records an inline-vertex draw (`EndVertices`): `count` vertices of
/// `stride` bytes at guest address `data`. D3D would point the vertex fetch
/// constant at the inline data in its command buffer; the draw's copy of the
/// fetch constants points it at `data` instead (guest state is untouched).
pub fn inline_draw(dev: u32, primitive: u32, count: u32, stride: u32, data: u32) {
    let mut guard = BUILDER.lock().unwrap();
    let b = guard.get_or_insert_with(new_builder);
    if b.sink.is_none() {
        return;
    }
    let t0 = std::time::Instant::now();
    let Some(draw) = capture(b, dev, primitive, count, Some((data, count * stride)), None) else { return };
    let spent = t0.elapsed().as_nanos() as u64;
    stats::add(&stats::CAPTURE_NS, spent);
    perf::add(&perf::CAPTURE_NS, &perf::CAPTURE_TOTAL, spent);
    stats::add(&stats::DRAWS, 1);
    b.frame.commands.push(Command::Draw(draw));
}

/// Records an indexed draw (`DrawIndexedVertices`): `count` indices from
/// `start` in the bound index buffer, plus `base_vertex`. The index buffer
/// object is at device `+0x320C`: word 0 bit 31 = 32-bit indices, `+0x18` the
/// data's CPU address. Indices are read as the CPU wrote them (big-endian).
pub fn indexed_draw(dev: u32, primitive: u32, base_vertex: i32, start: u32, count: u32) {
    let mut guard = BUILDER.lock().unwrap();
    let b = guard.get_or_insert_with(new_builder);
    if b.sink.is_none() {
        return;
    }
    let ib = mem::r32(dev + 0x320C);
    if ib == 0 {
        diag::log("render: indexed draw with no index buffer (skipped)");
        return;
    }
    let t0 = std::time::Instant::now();
    let wide = mem::r32(ib) >> 31 != 0;
    let size = if wide { 4 } else { 2 };
    let at = mem::r32(ib + 0x18).wrapping_add(start * size);
    // Converted once per frame for each list: a crowd draws the same few meshes,
    // with the same ranges of the same index buffers, many times a frame. A
    // sample of the memory guards the reuse, as for vertex spans.
    let print = sample_print(at, count * size);
    let indices = match b.frame_indices.get(&(at, count, wide)) {
        Some((p, list)) if *p == print => list.clone(),
        _ => {
            let bytes = unsafe { std::slice::from_raw_parts(mem::base().add(at as usize), (count * size) as usize) };
            let list: Vec<u32> = if wide {
                bytes.chunks_exact(4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]])).collect()
            } else {
                // 0xFFFF restarts a strip, as 0xFFFFFFFF does for the renderer.
                bytes.chunks_exact(2).map(|b| u16::from_be_bytes([b[0], b[1]])).map(|i| if i == 0xFFFF { u32::MAX } else { i as u32 }).collect()
            };
            let list = Arc::new(list);
            b.frame_indices.insert((at, count, wide), (print, list.clone()));
            list
        }
    };
    let Some(draw) = capture(b, dev, primitive, count, None, Some((indices, base_vertex))) else { return };
    let spent = t0.elapsed().as_nanos() as u64;
    stats::add(&stats::CAPTURE_NS, spent);
    perf::add(&perf::CAPTURE_NS, &perf::CAPTURE_TOTAL, spent);
    stats::add(&stats::DRAWS, 1);
    b.frame.commands.push(Command::Draw(draw));
}

/// Closes the frame (`Swap`) and sends it to the renderer, if one listens.
pub fn end_frame(number: u64, front: Option<[u32; 6]>) {
    let mut guard = BUILDER.lock().unwrap();
    let Some(b) = guard.as_mut() else { return };
    let mut frame = std::mem::take(&mut b.frame);
    frame.number = number;
    frame.front = front;
    frame.hold = HOLD_FRAMES.try_update(std::sync::atomic::Ordering::Relaxed, std::sync::atomic::Ordering::Relaxed, |n| n.checked_sub(1)).is_ok();
    // Late vertex reads: each span once, and draws whose memory is gone are dropped
    // (as at the draw call).
    let mut read: HashMap<(u32, u32), Option<(u32, Arc<Vec<u8>>)>> = HashMap::new();
    frame.commands.retain_mut(|c| {
        let Command::Draw(d) = c else { return true };
        if d.vertex_spans.is_empty() {
            return true;
        }
        let ranges: Option<Vec<_>> = d.vertex_spans.iter().map(|&s| read.entry(s).or_insert_with(|| read_span(s)).clone()).collect();
        match ranges {
            Some(r) => {
                d.vertex_ranges = r;
                true
            }
            None => false,
        }
    });
    b.frame_textures.clear();
    b.frame_spans.clear();
    b.overlays.clear();
    b.frame_indices.clear();
    b.literals.clear();
    // The frame's textures, read together (the console's GPU reads them after
    // the frame is kicked off, too).
    let t = std::time::Instant::now();
    let read = run_texture_jobs(b, number);
    b.frame_count = number + 1;
    for c in &mut frame.commands {
        if let Command::Draw(d) = c {
            for (slot, job) in d.pending_textures.drain(..) {
                if let Some(t) = &read[job] {
                    d.textures.push((slot, t.clone()));
                }
            }
        }
    }
    let spent = t.elapsed().as_nanos() as u64;
    stats::add(&stats::CAPTURE_NS, spent);
    stats::add(&stats::TEXTURE_NS, spent);
    perf::add(&perf::CAPTURE_NS, &perf::CAPTURE_TOTAL, spent);
    stats::frame(number);
    perf::frame();
    // Every frame reaches the renderer: its resolves write textures later frames
    // sample (some only once, like lighting cubes made when a scene loads). When
    // the renderer is behind, the game waits here, as it waits for the console's
    // GPU when its command buffer is full. The capture lock is released first.
    let Some(tx) = b.sink.clone() else { return };
    drop(guard);
    static WAITED_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let t = std::time::Instant::now();
    let sent = match tx.try_send(frame) {
        Ok(()) => true,
        Err(TrySendError::Full(frame)) => {
            let ok = tx.send(frame).is_ok();
            WAITED_NS.fetch_add(t.elapsed().as_nanos() as u64, std::sync::atomic::Ordering::Relaxed);
            perf::add(&perf::WAIT_NS, &perf::WAIT_TOTAL, t.elapsed().as_nanos() as u64);
            ok
        }
        Err(TrySendError::Disconnected(_)) => false,
    };
    if !sent && let Some(b) = BUILDER.lock().unwrap().as_mut() {
        b.sink = None;
    }
    if sent && let Some(f) = ON_FRAME.get() {
        f();
    }
    if number % 1800 == 0 && number > 0 {
        let ms = WAITED_NS.swap(0, std::sync::atomic::Ordering::Relaxed) / 1_000_000;
        diag::log(&format!("render: the game waited {ms} ms on the renderer over the last 1800 frames"));
    }
}

fn capture(
    b: &mut Builder,
    dev: u32,
    primitive: u32,
    count: u32,
    inline: Option<(u32, u32)>,
    indexed: Option<(Arc<Vec<u32>>, i32)>,
) -> Option<Draw> {
    // A draw that can't be captured is reported (once per reason), never dropped quietly.
    fn skipped(why: String) -> Option<Draw> {
        static SEEN: Mutex<Option<std::collections::HashSet<String>>> = Mutex::new(None);
        let mut seen = SEEN.lock().unwrap();
        let seen = seen.get_or_insert_with(Default::default);
        if seen.len() < 100 && seen.insert(why.clone()) {
            diag::log(&format!("render: draw not captured: {why}"));
        }
        None
    }
    let Some((vs_addr, vs_len)) = shaders::bound(dev, Stage::Vertex) else {
        return skipped(format!("no vertex shader bound (primitive {primitive}, {count} vertices)"));
    };
    let Some((ps_addr, ps_len)) = shaders::bound(dev, Stage::Pixel) else {
        return skipped(format!("no pixel shader bound (vertex shader at {vs_addr:08X}, primitive {primitive}, {count} vertices)"));
    };
    let mut lap = std::time::Instant::now();
    let mut split = |counter: &std::sync::atomic::AtomicU64| {
        if stats::enabled() {
            let now = std::time::Instant::now();
            stats::add(counter, (now - lap).as_nanos() as u64);
            lap = now;
        }
    };
    let vsi = shader_info(b, vs_addr, vs_len);
    let psi = shader_info(b, ps_addr, ps_len);
    let vfetch = vsi.vfetch.clone();
    split(&stats::SHADER_NS);

    let mut fetch = [0u32; 192];
    for (i, w) in fetch.iter_mut().enumerate() {
        *w = mem::r32(dev + FETCH + 4 * i as u32);
    }
    // Vertex data: inline data, or the ranges the fetch constants describe
    // (read now, or when the frame ends).
    let mut spans = Vec::new();
    let ranges = match inline {
        Some((data, len)) => {
            let base = gpu_address(data);
            for (_, d) in vfetch.iter().filter(|(_, d)| (d[1] >> 30) & 1 == 0) {
                let fc = d[0] as usize;
                // Vertex fetch constant: type 3, base in words, endian 8-in-32, size in words.
                fetch[2 * fc] = base | 3;
                fetch[2 * fc + 1] = 2 | (len.div_ceil(4) << 2);
            }
            vec![(base >> 2, Arc::new(mem::read(data, len as usize)))]
        }
        None if late_vertices() => {
            spans = vertex_spans(&fetch, &vfetch);
            Vec::new()
        }
        None => {
            let spans = vertex_spans(&fetch, &vfetch);
            match spans.iter().map(|&s| frame_span(b, s)).collect::<Option<Vec<_>>>() {
                Some(r) => r,
                None => {
                    let gone: Vec<String> = spans.iter().filter(|&&(lo, _)| texture::physical_to_virtual(lo << 2).is_none()).map(|&(lo, hi)| format!("{:08X}..{:08X}", lo << 2, hi << 2)).collect();
                    return skipped(format!("vertex memory not committed: {} (vertex shader at {vs_addr:08X})", gone.join(", ")));
                }
            }
        }
    };
    split(&stats::VERTEX_NS);

    // Textures: only the fetch constants the shaders sample.
    let mut sampled = vsi.tfetch.clone();
    for &n in &psi.tfetch {
        if !sampled.contains(&n) {
            sampled.push(n);
        }
    }
    // Read when the frame ends (`run_texture_jobs`), all at once on several threads.
    let mut pending_textures = Vec::new();
    for &slot in sampled.iter() {
        let words: [u32; 6] = core::array::from_fn(|i| fetch[6 * slot as usize + i]);
        let job = match b.frame_textures.get(&words) {
            Some(&job) => job,
            None => {
                let job = texture_job(b, slot, &words);
                b.frame_textures.insert(words, job);
                job
            }
        };
        if let Some(job) = job {
            pending_textures.push((slot, job));
        }
    }
    split(&stats::TEXTURE_NS);
    let mut regs = Vec::new();
    for (first, off, n) in REG_BLOCKS {
        for i in 0..n {
            let v = mem::r32(dev + off + 4 * i);
            regs.push((first + i, if first + i == 0x2000 { tiled_surface_info(dev, v) } else { v }));
        }
    }
    // Constants, with each shader's literals loaded over them as D3D does
    // when it sets the shader.
    let (vs, ps) = (mem::r32(dev + 0x3310), mem::r32(dev + 0x330C));
    let vs_consts = stage_consts(b, 0, dev + VS_CONSTS, (vs + VS_CONTAINER, mem::r32(vs + 0x20), vsi.identity));
    let ps_consts = stage_consts(b, 1, dev + PS_CONSTS, (ps + PS_CONTAINER, mem::r32(ps + 0x18), psi.identity));
    split(&stats::CONST_NS);
    Some(Draw {
        vs: vsi.identity,
        ps: psi.identity,
        primitive,
        vertex_count: count,
        base_vertex: indexed.as_ref().map_or(0, |i| i.1),
        indices: indexed.map(|i| i.0),
        vs_consts,
        ps_consts,
        control: core::array::from_fn(|i| mem::r32(dev + CONTROL + 4 * i as u32)),
        fetch,
        vfetch,
        vertex_ranges: ranges,
        vertex_spans: spans,
        textures: Vec::new(),
        pending_textures,
        regs,
        tiles: predicated_tiles(dev),
    })
}

/// Where D3D's shader objects keep their copy of the shader container (the
/// compiled shader's header, `xenos_shader_recomp::containers`): after a
/// 0x28-byte header for pixel shaders, at `+0x368` for vertex shaders. Their
/// physical part (literals, then microcode) is at `+0x18` / `+0x20`.
const PS_CONTAINER: u32 = 0x28;
const VS_CONTAINER: u32 = 0x368;

/// A shader's literal constants (`def`), which D3D loads into the stage's
/// constant registers when it sets the shader: the container's definition
/// table lists runs of (register, float count, byte offset of the values in
/// the physical part, where the literals precede the microcode). Returns
/// (register, value) pairs.
fn literals(container: u32, physical: u32, id: u64) -> Vec<(usize, [f32; 4])> {
    let mut out = Vec::new();
    if mem::r32(container) & !1 != 0x102A_1100 {
        return out;
    }
    let def = mem::r32(container + 0x14);
    if def == 0 {
        return out;
    }
    let end = container + def + 0x10 + mem::r32(container + def + 0x10).min(0x100);
    // WWE14_LOG_LITERALS: each container's definition table once (debugging).
    if std::env::var_os("WWE14_LOG_LITERALS").is_some() {
        static SEEN: Mutex<Option<std::collections::HashSet<u32>>> = Mutex::new(None);
        if SEEN.lock().unwrap().get_or_insert_with(Default::default).insert(container) {
            let words: Vec<String> = (0..((end - (container + def)) / 4 + 1).min(40))
                .map(|i| format!("{:08X}", mem::r32(container + def + 4 * i)))
                .collect();
            diag::log(&format!("literals: shader {id:016x} container {container:08X} (def at +{def:X}, physical {physical:08X}): [{}]", words.join(" ")));
        }
    }
    // Entries are 8 bytes: (register << 16 | float count), then the values'
    // byte offset in the physical part. Empty entries pad the table.
    let mut entry = container + def + 0x14;
    while entry + 8 <= end {
        let (e, offset) = (mem::r32(entry), mem::r32(entry + 4));
        let (reg, count) = ((e >> 16) & 0xFF, e & 0xFFFF);
        let value = physical + offset;
        for k in 0..count / 4 {
            if reg + k < 256 {
                out.push(((reg + k) as usize, core::array::from_fn(|i| f32::from_bits(mem::r32(value + 16 * k + 4 * i as u32)))));
            }
        }
        entry += 8;
    }
    out
}

/// The guest memory the draw's vertex streams cover, as (physical word
/// address, end): one range per stream its full fetches name (vertex fetch
/// constant: base word address in word 0 bits 2-31, size in words in word 1
/// bits 2-25), overlapping ones merged, at most 8 (the closest are merged
/// beyond that).
fn vertex_spans(fetch: &[u32; 192], vfetch: &[(u32, [u32; 4])]) -> Vec<(u32, u32)> {
    let mut spans: Vec<(u32, u32)> = Vec::new();
    for (_, d) in vfetch.iter().filter(|(_, d)| (d[1] >> 30) & 1 == 0) {
        let fc = d[0] as usize;
        let (w0, w1) = (fetch[2 * fc], fetch[2 * fc + 1]);
        let (base, size) = (w0 >> 2, (w1 >> 2) & 0xFF_FFFF);
        if size > 0 {
            spans.push((base, base + size));
        }
    }
    spans.sort();
    let mut merged: Vec<(u32, u32)> = Vec::new();
    for (lo, hi) in spans {
        match merged.last_mut() {
            Some(m) if lo <= m.1 => m.1 = m.1.max(hi),
            _ => merged.push((lo, hi)),
        }
    }
    while merged.len() > 8 {
        let i = (0..merged.len() - 1).min_by_key(|&i| merged[i + 1].0 - merged[i].1).unwrap();
        merged[i].1 = merged[i + 1].1;
        merged.remove(i + 1);
    }
    merged
}

/// The bytes of one span, or `None` if no committed memory holds it.
fn read_span((lo, hi): (u32, u32)) -> Option<(u32, Arc<Vec<u8>>)> {
    let virt = texture::physical_to_virtual(lo << 2)?;
    stats::add(&stats::VERTEX_BYTES, ((hi - lo) * 4) as u64);
    Some((lo, Arc::new(mem::read(virt, ((hi - lo) * 4) as usize))))
}

/// `WWE14_LATE_VERTICES=1`: read vertex memory when the frame ends (`Swap`)
/// instead of at the draw call. The console GPU reads it after the command
/// buffer is kicked off, which can be after the game has finished writing
/// dynamic vertices it issued the draw for.
fn late_vertices() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("WWE14_LATE_VERTICES").is_ok_and(|v| v != "0"))
}

/// Says (once per fetch constant base and reason) why a texture a shader
/// samples was not captured, unless a resolve wrote it (the renderer has it).
fn report_missing(b: &mut Builder, slot: u32, w: &[u32; 6], f: Option<&Fetch>) {
    let reason = match f {
        None => format!("is not a texture (type {})", w[0] & 3),
        Some(f) if b.resolve_dests.contains_key(&texture::physical(f.base)) => return,
        Some(f) if texture::format_layout(f.format).is_none() => format!("has format {} with no layout", f.format),
        Some(f) if texture::guest_range(f).is_none() => format!("{:08X} ({}x{}, dimension {}) is not in committed memory", f.base, f.width, f.height, f.dimension),
        // Conversion failures are reported by `texture`.
        Some(_) => return,
    };
    if b.missing.insert((w[1] & 0xFFFF_F000, reason.clone())) && b.missing.len() < 200 {
        diag::log(&format!("render: texture fetch constant {slot} {reason}; words {w:08X?}"));
    }
}

/// A cheap fingerprint of a guest range: 64 bytes at the start and middle of
/// each 4 KB page (a texture loaded over it rewrites them all).
fn fingerprint((virt, len): (u32, u32)) -> u64 {
    let mut h = 0x9E37_79B9_7F4A_7C15u64 ^ len as u64;
    let mut at = 0;
    while at < len {
        for off in [0, 2048] {
            if at + off + 64 <= len {
                h = hash_guest(virt + at + off, 64, h);
            }
        }
        at += 4096;
    }
    h
}

/// A texture to read when the frame ends: its fetch constant decoded, and the
/// guest memory it covers.
struct TextureJob {
    fetch: Fetch,
    virt: u32,
    len: u32,
}

impl TextureJob {
    /// The converted-texture cache's key: the guest address and everything
    /// about how the memory there is read (not the contents).
    fn slot_key(&self) -> u64 {
        let f = &self.fetch;
        let layout = (f.base as u64) << 32 ^ (f.format as u64) << 24 ^ (f.width as u64) << 12 ^ f.height as u64 ^ (f.dimension as u64) << 60;
        layout ^ f.conversion().rotate_left(29) ^ (self.len as u64).rotate_left(47)
    }
}


/// Decides at the draw how the texture a fetch constant names is read: a job
/// for the frame's end, or nothing (a resolve's image, which the renderer has,
/// or a fetch constant that names no readable texture, reported).
fn texture_job(b: &mut Builder, slot: u32, words: &[u32; 6]) -> Option<usize> {
    let Some(f) = Fetch::decode(*words) else {
        report_missing(b, slot, words, None);
        return None;
    };
    let phys = texture::physical(f.base);
    if let Some(&(virt, len, print)) = b.resolve_dests.get(&phys) {
        if mem::watched(virt) || fingerprint((virt, len)) == print {
            return None;
        }
        // The CPU wrote there after the resolve: its texels are the texture now.
        b.resolve_dests.remove(&phys);
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 50 {
            diag::log(&format!(
                "render: {phys:08X} ({}x{} format {}) was a resolve destination and has been rewritten by the CPU; capturing it",
                f.width,
                f.height,
                f.format
            ));
        }
    }
    let Some((virt, len)) = texture::guest_range(&f) else {
        report_missing(b, slot, words, Some(&f));
        return None;
    };
    let job = TextureJob { fetch: f, virt, len };
    // Two fetch constants that differ only in sampling read the same texture once.
    let slot_key = job.slot_key();
    if let Some(&i) = b.job_slots.get(&slot_key) {
        return Some(i);
    }
    let frame = b.frame_count;
    let hashed = if b.dynamic.get(&slot_key).is_some_and(|&f| frame.saturating_sub(f) < 120) {
        // Changed lately (video frames, CPU-drawn images): copied here, at the
        // draw, as the console's GPU would read them, since the CPU may
        // rewrite them before the frame ends.
        let raw = mem::read(virt, len as usize);
        b.hashes.ready(hash_bytes(&raw, slot_key), Some(Arc::new(raw)))
    } else {
        // Hashed now, off this thread, while the walk goes on; copied then too
        // if the contents are not the cached ones.
        let known = b.textures.get(&slot_key).map(|e| e.0);
        b.hashes.push(virt, len, slot_key, known)
    };
    b.jobs.push(job);
    debug_assert_eq!(hashed, b.jobs.len() - 1);
    b.job_slots.insert(slot_key, b.jobs.len() - 1);
    Some(b.jobs.len() - 1)
}

/// Texture hashing off the render thread. Hashing every sampled texture's
/// memory each frame (10–15 MB in an arena) is most of the texture cost; a
/// draw's textures are hashed by a few workers as soon as the draw names them,
/// while the game's walk goes on, and the frame's end collects the hashes.
mod hasher {
    use std::sync::mpsc::{Receiver, Sender, channel};
    use std::sync::{Arc, Condvar, Mutex, OnceLock};

    /// A texture's content hash, and its bytes when they were copied (the
    /// contents were not the cached ones, or the texture is dynamic).
    pub type Read = (u64, Option<Arc<Vec<u8>>>);

    /// One frame's hashes, in job order.
    #[derive(Default)]
    pub struct Batch {
        state: Mutex<(Vec<Option<Read>>, usize)>,
        done: Condvar,
    }

    /// (batch, index, address, length, seed, the cached contents' hash).
    type Work = (Arc<Batch>, usize, u32, u32, u64, Option<u64>);

    impl Batch {
        /// Queues a hash of `len` guest bytes at `virt` (seeded); the bytes are
        /// copied too unless they hash to `known`. Returns its index.
        pub fn push(self: &Arc<Self>, virt: u32, len: u32, seed: u64, known: Option<u64>) -> usize {
            let i = {
                let mut s = self.state.lock().unwrap();
                s.0.push(None);
                s.1 += 1;
                s.0.len() - 1
            };
            pool().lock().unwrap().send((self.clone(), i, virt, len, seed, known)).expect("texture hashers");
            i
        }

        /// Adds a read done already. Returns its index.
        pub fn ready(&self, hash: u64, raw: Option<Arc<Vec<u8>>>) -> usize {
            let mut s = self.state.lock().unwrap();
            s.0.push(Some((hash, raw)));
            s.0.len() - 1
        }

        /// Waits for every queued hash.
        pub fn wait(&self) -> Vec<Read> {
            let mut s = self.state.lock().unwrap();
            while s.1 > 0 {
                s = self.done.wait(s).unwrap();
            }
            s.0.iter_mut().map(|h| h.take().expect("hashed")).collect()
        }
    }

    fn pool() -> &'static Mutex<Sender<Work>> {
        static POOL: OnceLock<Mutex<Sender<Work>>> = OnceLock::new();
        POOL.get_or_init(|| {
            let (tx, rx) = channel::<Work>();
            let rx = Arc::new(Mutex::new(rx));
            let n = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(2, 6);
            for k in 0..n {
                let rx: Arc<Mutex<Receiver<Work>>> = rx.clone();
                std::thread::Builder::new()
                    .name(format!("texture hasher {k}"))
                    .spawn(move || {
                        kernel::thread::make_background();
                        loop {
                            let Ok((batch, i, virt, len, seed, known)) = rx.lock().unwrap().recv() else { return };
                            let mut h = super::hash_guest(virt, len as usize, seed);
                            let mut raw = None;
                            if known != Some(h) {
                                // Changed: keep these bytes (and the key of exactly these).
                                let bytes = kernel::mem::read(virt, len as usize);
                                h = super::hash_bytes(&bytes, seed);
                                raw = Some(Arc::new(bytes));
                            }
                            let mut s = batch.state.lock().unwrap();
                            s.0[i] = Some((h, raw));
                            s.1 -= 1;
                            if s.1 == 0 {
                                batch.done.notify_all();
                            }
                        }
                    })
                    .expect("texture hasher thread");
            }
            Mutex::new(tx)
        })
    }
}

/// Texture conversion off the game's threads. Converting changed textures
/// (video frames, a new scene's textures) took a fifth of the game's render
/// thread in an arena. A few converter threads now turn the guest bytes into
/// host texels while the renderer is still on earlier frames; the renderer
/// waits for them (`Texture::pixels`) only if it gets there first.
mod converter {
    use std::sync::mpsc::{Receiver, Sender, channel};
    use std::sync::{Arc, Mutex, OnceLock};

    use super::{Texture, stats};
    use crate::texture::{self, Fetch};

    type Work = Arc<Texture>;

    /// Queues `t`'s conversion (its pending job), unless a thread that needs
    /// the texels does it first (`Texture::converted`).
    pub fn convert(t: Arc<Texture>) {
        pool().lock().unwrap().send(t).expect("texture converters");
    }

    /// One level's texels in the texture's upload format.
    fn level(t: &Texture, f: &Fetch, raw: &[u8]) -> Option<Vec<u8>> {
        match t.bc {
            Some(_) => texture::to_bc_from(f, raw),
            None if f.wide() => texture::to_half_from(f, raw),
            None => texture::to_rgba_from(f, raw),
        }
    }

    fn run(t: Work) {
        let job = t.pending.lock().unwrap().take();
        if let Some((f, raw)) = job {
            let _ = t.rgba.set(conversion(&t, &f, &raw));
        }
    }

    /// `t`'s texels and mip levels from `raw`, read with `f`.
    pub fn conversion(t: &Texture, f: &Fetch, raw: &[u8]) -> Option<(Vec<u8>, Vec<(u32, u32)>)> {
        let f = *f;
        let converted = level(t, &f, raw);
        match &converted {
            Some(px) => stats::add(&stats::CONVERTED_BYTES, px.len() as u64),
            None => {
                static REPORTED: Mutex<Vec<u32>> = Mutex::new(Vec::new());
                let mut r = REPORTED.lock().unwrap();
                if !r.contains(&f.format) {
                    r.push(f.format);
                    kernel::diag::log(&format!("render: texture format {} ({}) is not converted yet", f.format, texture::format_name(f.format)));
                }
            }
        }
        // WWE14_DUMP_TEXTURE=WxH: writes the first 16 textures of that size, as
        // converted, to `texture_ADDR_N.png` in the working directory.
        static DUMP: OnceLock<Option<(u32, u32)>> = OnceLock::new();
        let dump = DUMP.get_or_init(|| {
            let v = std::env::var("WWE14_DUMP_TEXTURE").ok()?;
            let (w, h) = v.split_once('x')?;
            Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
        });
        if let Some(px) = &converted
            && !f.wide()
            && t.bc.is_none()
            && *dump == Some((f.width, f.height))
        {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n < 16 {
                let name = format!("texture_{:08X}_{n}.png", f.base);
                let _ = crate::encode::write_png(std::path::Path::new(&name), px, f.width as usize, f.height as usize);
                let alpha: Vec<u8> = px.chunks_exact(4).flat_map(|p| [p[3], p[3], p[3], 255]).collect();
                let _ = crate::encode::write_png(std::path::Path::new(&format!("texture_{:08X}_{n}_alpha.png", f.base)), &alpha, f.width as usize, f.height as usize);
                kernel::diag::log(&format!("render: dumped texture {:08X} ({}x{}, format {}) to {name}", f.base, f.width, f.height, f.format));
            }
        }
        // The mip levels after the base, read from guest memory now: they are
        // written with the base, which is what the capture compares. A level
        // that is not readable or converts to nothing ends the chain.
        converted.map(|mut px| {
            let mut levels = vec![(f.width, f.height)];
            if t.layers == 1 {
                for lf in f.mip_levels() {
                    let Some(raw) = texture::snapshot(&lf) else { break };
                    let Some(level) = level(t, &lf, &raw) else { break };
                    px.extend_from_slice(&level);
                    levels.push((lf.width, lf.height));
                }
            }
            (px, levels)
        })
    }

    fn pool() -> &'static Mutex<Sender<Work>> {
        static POOL: OnceLock<Mutex<Sender<Work>>> = OnceLock::new();
        POOL.get_or_init(|| {
            let (tx, rx) = channel::<Work>();
            let rx = Arc::new(Mutex::new(rx));
            let n = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(2, 6);
            for k in 0..n {
                let rx: Arc<Mutex<Receiver<Work>>> = rx.clone();
                std::thread::Builder::new()
                    .name(format!("texture converter {k}"))
                    .spawn(move || {
                        kernel::thread::make_background();
                        loop {
                            let Ok(work) = rx.lock().unwrap().recv() else { return };
                            run(work);
                        }
                    })
                    .expect("texture converter thread");
            }
            Mutex::new(tx)
        })
    }
}

/// Collects the frame's textures when it ends: their hashes (`hasher`), the
/// cached texture where the contents are unchanged, and for the rest a new
/// texture the converters fill in. Returns each job's texture.
fn run_texture_jobs(b: &mut Builder, frame: u64) -> Vec<Option<Arc<Texture>>> {
    let jobs = std::mem::take(&mut b.jobs);
    b.job_slots.clear();
    let keys = std::mem::take(&mut b.hashes).wait();
    if jobs.is_empty() {
        return Vec::new();
    }
    stats::add(&stats::TEXTURE_BYTES, jobs.iter().map(|j| j.len as u64).sum());
    let mut results = vec![None; jobs.len()];
    for (i, (j, (key, raw))) in jobs.iter().zip(keys).enumerate() {
        let slot = j.slot_key();
        if let Some(e) = b.textures.get_mut(&slot)
            && e.0 == key
        {
            e.2 = frame;
            results[i] = Some(e.1.clone());
            continue;
        }
        let f = j.fetch;
        let raw = raw.unwrap_or_else(|| Arc::new(mem::read(j.virt, j.len as usize)));
        let t = Arc::new(Texture { width: f.width, height: f.height, layers: texture::layers(&f), rgba: Default::default(), wide: f.wide(), bc: texture::bc_passthrough(&f), pending: Mutex::new(Some((f, raw))), key, slot });
        converter::convert(t.clone());
        // New contents where others were: read it at the draw from now on (`texture_job`).
        if b.textures.insert(slot, (key, t.clone(), frame)).is_some() {
            b.dynamic.insert(slot, frame);
        }
        results[i] = Some(t);
    }
    // Every 10 s: forget textures no frame sampled in the last 20 s.
    if frame % 600 == 0 {
        b.dynamic.retain(|_, f| frame.saturating_sub(*f) < 120);
        b.textures.retain(|_, e| frame.saturating_sub(e.2) < 1200);
    }
    results
}

/// 16 float constant registers: one block of a stage's 256.
pub type ConstBlock = [[f32; 4]; 16];

/// A stage's 256 float constants as 16 blocks of 16 registers. Consecutive
/// draws usually change a matrix or two, so most of a draw's blocks are the
/// previous draw's, shared (the same `Arc`): the capture converts, and the
/// renderer uploads, only the blocks that changed.
#[derive(Clone)]
pub struct Consts(pub [Arc<ConstBlock>; 16]);

impl Consts {
    /// Register `i`.
    pub fn get(&self, i: usize) -> [f32; 4] {
        self.0[i / 16][i % 16]
    }
}

/// A shader's literal constants (register, value), and which blocks they
/// fall in (bit k: block k).
struct Literals {
    values: Vec<(usize, [f32; 4])>,
    blocks: u16,
}

/// One block of guest constant registers (256 big-endian bytes) as host floats.
fn const_block(bytes: &[u8]) -> ConstBlock {
    core::array::from_fn(|r| core::array::from_fn(|i| f32::from_bits(u32::from_be_bytes(bytes[16 * r + 4 * i..16 * r + 4 * i + 4].try_into().unwrap()))))
}

/// The float constants of one stage (`stage` 0 vertex, 1 pixel) at guest
/// address `at`, as host floats, with the bound shader's literals loaded over
/// them (`shader`: its container, physical part and identity). Consecutive
/// draws often set the same ones: the previous draw's are reused when the
/// guest bytes and the shader are unchanged, and otherwise only the blocks
/// whose bytes changed are new.
fn stage_consts(b: &mut Builder, stage: usize, at: u32, shader: (u32, u32, u64)) -> Arc<Consts> {
    const BYTES: usize = 256 * 16;
    const BLOCK: usize = 16 * 16;
    let raw = unsafe { std::slice::from_raw_parts(mem::base().add(at as usize), BYTES) };
    let last = b.last_consts[stage].get_or_insert_with(|| {
        let base: [Arc<ConstBlock>; 16] = core::array::from_fn(|k| Arc::new(const_block(&raw[k * BLOCK..(k + 1) * BLOCK])));
        StageConsts { raw: raw.to_vec(), result: Arc::new(Consts(base.clone())), base, shader: None }
    });
    let mut changed = false;
    if last.raw[..] != *raw {
        for (k, (new, old)) in raw.chunks_exact(BLOCK).zip(last.raw.chunks_exact_mut(BLOCK)).enumerate() {
            if new != old {
                old.copy_from_slice(new);
                last.base[k] = Arc::new(const_block(new));
                changed = true;
            }
        }
    }
    if !changed && last.shader == Some(shader) {
        return last.result.clone();
    }
    let literals = b.literals.entry(shader).or_insert_with(|| {
        let values = literals(shader.0, shader.1, shader.2);
        let blocks = values.iter().fold(0u16, |m, &(reg, _)| m | 1 << (reg / 16));
        Arc::new(Literals { values, blocks })
    });
    let mut blocks = last.base.clone();
    for (k, block) in blocks.iter_mut().enumerate().filter(|(k, _)| literals.blocks & (1 << k) != 0) {
        // Shared too: the same block under the same shader is the same result.
        let key = (Arc::as_ptr(block) as usize, shader, k);
        let entry = b.overlays.entry(key).or_insert_with(|| {
            let mut out = **block;
            for &(reg, v) in literals.values.iter().filter(|(reg, _)| reg / 16 == k) {
                out[reg % 16] = v;
            }
            (block.clone(), Arc::new(out))
        });
        *block = entry.1.clone();
    }
    last.shader = Some(shader);
    last.result = Arc::new(Consts(blocks));
    last.result.clone()
}

/// A stage's constants as the previous draw captured them (`stage_consts`):
/// the guest bytes, their blocks as floats, the shader whose literals the
/// captured constants have, and the captured constants.
struct StageConsts {
    raw: Vec<u8>,
    base: [Arc<ConstBlock>; 16],
    shader: Option<(u32, u32, u64)>,
    result: Arc<Consts>,
}

/// A cheap sample of a guest range: 32 bytes of every KB, and the last 32.
/// Dynamic vertex buffers are rewritten whole, which this always sees.
fn sample_print(virt: u32, len: u32) -> u64 {
    let mut h = len as u64;
    let mut at = 0;
    while at + 32 <= len {
        h = hash_guest(virt + at, 32, h);
        at += 1024;
    }
    if len >= 32 {
        h = hash_guest(virt + len - 32, 32, h);
    }
    h
}

/// A span's bytes, read once per frame: later draws of the same span reuse
/// them while the memory's sample (`sample_print`) is unchanged. Meshes drawn
/// in several passes (depth, shadow, colour) copied their vertices each time.
fn frame_span(b: &mut Builder, s: (u32, u32)) -> Option<(u32, Arc<Vec<u8>>)> {
    let print = texture::physical_to_virtual(s.0 << 2).map(|virt| sample_print(virt, (s.1 - s.0) * 4));
    if let Some((p, read)) = b.frame_spans.get(&s)
        && Some(*p) == print
    {
        return read.clone();
    }
    let read = read_span(s);
    if let Some(p) = print {
        b.frame_spans.insert(s, (p, read.clone()));
    }
    read
}

/// Frame pacing, always on and logged every 5 s: the game's frame rate, the
/// time its render thread spends capturing draws, and the time it waits for
/// the renderer (which slows the game down when the renderer is behind).
pub mod perf {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    pub static CAPTURE_NS: AtomicU64 = AtomicU64::new(0);
    pub static WAIT_NS: AtomicU64 = AtomicU64::new(0);
    /// Render thread: walking the render lists, and waiting for the vertical blank in `Swap`.
    pub static WALK_NS: AtomicU64 = AtomicU64::new(0);
    pub static VBLANK_NS: AtomicU64 = AtomicU64::new(0);
    /// Other threads: recording a list while blocked behind a walk (`lists.rs`).
    pub static LIST_WAIT_NS: AtomicU64 = AtomicU64::new(0);
    /// Running totals of the same, never reset (per-frame breakdowns).
    pub static WALK_TOTAL: AtomicU64 = AtomicU64::new(0);
    pub static CAPTURE_TOTAL: AtomicU64 = AtomicU64::new(0);
    pub static VBLANK_TOTAL: AtomicU64 = AtomicU64::new(0);
    pub static WAIT_TOTAL: AtomicU64 = AtomicU64::new(0);

    /// Adds to a window counter and its running total.
    pub fn add(window: &AtomicU64, total: &AtomicU64, ns: u64) {
        window.fetch_add(ns, Ordering::Relaxed);
        total.fetch_add(ns, Ordering::Relaxed);
    }
    static FRAMES: AtomicU64 = AtomicU64::new(0);
    /// The window's start, the last frame's end, the longest frame, and the
    /// frames over 25 ms (a frame and a half at 60 Hz).
    static WINDOW: std::sync::Mutex<Option<(Instant, Instant, f64, u32)>> = std::sync::Mutex::new(None);

    /// One frame ended (`Swap`).
    pub fn frame() {
        FRAMES.fetch_add(1, Ordering::Relaxed);
        let now = Instant::now();
        let mut w = WINDOW.lock().unwrap();
        let (start, last, longest, slow) = w.get_or_insert((now, now, 0.0, 0));
        let interval = (now - *last).as_secs_f64() * 1000.0;
        *last = now;
        *longest = longest.max(interval);
        *slow += (interval > 25.0) as u32;
        // Each frame over 40 ms (a hitch), broken down, the first 300.
        {
            static PREV: std::sync::Mutex<[u64; 4]> = std::sync::Mutex::new([0; 4]);
            static TOTALS: [&AtomicU64; 4] = [&WALK_TOTAL, &CAPTURE_TOTAL, &VBLANK_TOTAL, &WAIT_TOTAL];
            static LOGGED: AtomicU64 = AtomicU64::new(0);
            let now_totals = TOTALS.map(|t| t.load(Ordering::Relaxed));
            let mut prev = PREV.lock().unwrap();
            let d: [f64; 4] = core::array::from_fn(|i| now_totals[i].saturating_sub(prev[i]) as f64 / 1e6);
            *prev = now_totals;
            if interval > 40.0 && LOGGED.fetch_add(1, Ordering::Relaxed) < 300 {
                kernel::diag::log(&format!(
                    "slow frame {}: {interval:.0} ms (walk {:.1}, capture {:.1}, vblank wait {:.1}, renderer wait {:.1}, elsewhere {:.1})",
                    kernel::video::PRESENTED.load(Ordering::Relaxed),
                    d[0],
                    d[1],
                    d[2],
                    d[3],
                    (interval - d[0] - d[2] - d[3]).max(0.0)
                ));
            }
        }
        let secs = start.elapsed().as_secs_f64();
        if secs < 5.0 {
            return;
        }
        let (longest, slow) = (*longest, *slow);
        *w = Some((now, now, 0.0, 0));
        let n = FRAMES.swap(0, Ordering::Relaxed).max(1);
        let ms = |a: &AtomicU64| a.swap(0, Ordering::Relaxed) as f64 / 1e6 / n as f64;
        let (walk, vblank, wait) = (ms(&WALK_NS), ms(&VBLANK_NS), ms(&WAIT_NS));
        // The rest of the render thread's frame: waiting for the game's other
        // threads (its lists, its jobs) outside the walk.
        let other = (secs * 1000.0 / n as f64 - walk - vblank - wait).max(0.0);
        kernel::diag::log(&format!(
            "perf: game {:.1} fps; per frame: walk {walk:.1} ms (capture {:.1}), vblank wait {vblank:.1} ms, list recording blocked {:.1} ms, waiting on the renderer {wait:.1} ms, elsewhere {other:.1} ms; {slow} frames over 25 ms, longest {longest:.0} ms",
            n as f64 / secs,
            ms(&CAPTURE_NS),
            ms(&LIST_WAIT_NS),
        ));
    }
}

/// Capture cost statistics (`WWE14_CAPTURE_STATS`): time in draw capture and
/// bytes copied, logged every 120 frames.
pub(crate) mod stats {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    pub static CAPTURE_NS: AtomicU64 = AtomicU64::new(0);
    pub static DRAWS: AtomicU64 = AtomicU64::new(0);
    pub static VERTEX_BYTES: AtomicU64 = AtomicU64::new(0);
    pub static TEXTURE_BYTES: AtomicU64 = AtomicU64::new(0);
    pub static CONVERTED_BYTES: AtomicU64 = AtomicU64::new(0);
    /// Parts of the capture time: shaders, vertices, textures, constants.
    pub static SHADER_NS: AtomicU64 = AtomicU64::new(0);
    pub static VERTEX_NS: AtomicU64 = AtomicU64::new(0);
    pub static TEXTURE_NS: AtomicU64 = AtomicU64::new(0);
    pub static CONST_NS: AtomicU64 = AtomicU64::new(0);
    static FRAME_START: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

    pub fn enabled() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("WWE14_CAPTURE_STATS").is_some())
    }

    pub fn add(counter: &AtomicU64, n: u64) {
        if enabled() {
            counter.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// At each frame's end: every 120 frames, the averages per frame.
    pub fn frame(number: u64) {
        static SEEN: AtomicBool = AtomicBool::new(false);
        if !enabled() || number % 120 != 0 {
            return;
        }
        let mut start = FRAME_START.lock().unwrap();
        let now = std::time::Instant::now();
        if let Some(s) = *start
            && SEEN.load(Ordering::Relaxed)
        {
            let per = |c: &AtomicU64| c.swap(0, Ordering::Relaxed) / 120;
            kernel::diag::log(&format!(
                "capture stats: {:.1} ms/frame wall, {:.1} ms/frame capturing (shaders {:.1}, vertices {:.1}, textures {:.1}, constants {:.1}) {} draws, {} KB vertices, {} KB textures read, {} KB converted",
                (now - s).as_secs_f64() * 1000.0 / 120.0,
                per(&CAPTURE_NS) as f64 / 1e6,
                per(&SHADER_NS) as f64 / 1e6,
                per(&VERTEX_NS) as f64 / 1e6,
                per(&TEXTURE_NS) as f64 / 1e6,
                per(&CONST_NS) as f64 / 1e6,
                per(&DRAWS),
                per(&VERTEX_BYTES) / 1024,
                per(&TEXTURE_BYTES) / 1024,
                per(&CONVERTED_BYTES) / 1024
            ));
        } else {
            for c in [&CAPTURE_NS, &DRAWS, &VERTEX_BYTES, &TEXTURE_BYTES, &CONVERTED_BYTES, &SHADER_NS, &VERTEX_NS, &TEXTURE_NS, &CONST_NS] {
                c.store(0, Ordering::Relaxed);
            }
        }
        SEEN.store(true, Ordering::Relaxed);
        *start = Some(now);
    }
}
