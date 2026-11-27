//! The wgpu executor for the render IR (plan §2.4 items 3 and 5).
//!
//! It runs the shaders `xenos-shader-recomp` translated ahead of time, looked
//! up by identity, on the state each draw captured from D3D's shadow. It
//! never translates or interprets shader code: a shader with no translation
//! is reported once and its draws are skipped (plan §2.4 item 6).
//!
//! Render targets are wgpu textures, one per surface D3D binds (the shadowed
//! `RB_SURFACE_INFO`/`RB_COLOR_INFO`/`RB_DEPTH_INFO` name it). A resolve
//! copies a target into a texture kept under the destination's address, which
//! later draws sample in place of guest memory. The frame shows the texture
//! resolved into the presented front buffer, or else the last target drawn.
//!
//! Bindings (see `tools/xenos-shader-recomp/src/wgsl.rs`):
//! - group 0: 0 vertex constants, 1 bool/loop constants, 2 fetch constants,
//!   3 guest vertex memory, 4 its window, 5 vertex fetch descriptors,
//!   6 pixel constants, 7 position transform, 8 alpha test;
//! - group 1: texture `2n` and sampler `2n + 1` for fetch constant `n`.

// foldhash: the renderer looks up pipelines, bind groups, textures and vertex
// ranges for every draw; std's SipHash showed in its profile.
use foldhash::{HashMap, HashMapExt, HashSet, HashSetExt};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use gpu::ir::{Command, Draw, Frame, Resolve};
use wgpu::util::DeviceExt;

mod record;

/// The console's output: 720p, in guest pixels.
pub const WIDTH: u32 = 1280;
pub const HEIGHT: u32 = 720;

static SNAPSHOT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Asks for a snapshot of the next frame rendered (F9 in the window): its draw
/// log and a picture after each pass, in `private/f9_<frame>`.
pub fn snapshot_next_frame() {
    SNAPSHOT.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Frees a rendered frame on a background thread. Its thousands of small
/// allocations (per-draw state, vertex and index copies) took the renderer's
/// thread close to a tenth of its time to free.
pub fn drop_later(frame: Frame) {
    use std::sync::mpsc::{Sender, channel};
    static TX: std::sync::OnceLock<std::sync::Mutex<Sender<Frame>>> = std::sync::OnceLock::new();
    let tx = TX.get_or_init(|| {
        let (tx, rx) = channel::<Frame>();
        std::thread::Builder::new()
            .name("frame drop".into())
            .spawn(move || {
                gpu::make_background();
                while let Ok(f) = rx.recv() {
                    drop(f);
                }
            })
            .expect("frame drop thread");
        std::sync::Mutex::new(tx)
    });
    let _ = tx.lock().unwrap().send(frame);
}

/// The render resolution scale: host pixels per guest pixel for render targets
/// and the textures resolves write. The console draws 1280×720; at the
/// default 1.5 the game draws at 1920×1080 (`WWE14_SCALE`, 1 to 4; 1 is the
/// console's resolution). Guest coordinates stay the game's: the position
/// transform maps them into the larger targets, and shaders measure texels
/// in the fetch constant's (guest) size (`tex_size` in the translated shaders).
pub fn scale() -> f64 {
    static SCALE: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
    *SCALE.get_or_init(|| std::env::var("WWE14_SCALE").ok().and_then(|v| v.parse().ok()).unwrap_or(1.5f64).clamp(1.0, 4.0))
}

/// A guest pixel coordinate or size in host pixels. Every conversion rounds
/// down, so rectangles that share an edge in the guest share it here.
pub fn up(x: u32) -> u32 {
    (x as f64 * scale()).floor() as u32
}

/// The displayed image's size: the console's output, scaled.
pub fn output_size() -> (u32, u32) {
    (up(WIDTH), up(HEIGHT))
}
pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth24PlusStencil8;

struct Shader {
    module: wgpu::ShaderModule,
    /// Textures sampled: fetch constant and whether it's a cube.
    textures: Vec<(u32, bool)>,
    /// Vertex shaders: the key of each fetch `VF.d[k]` is for (`vfetch-keys` header).
    vfetch_keys: Vec<u32>,
}

struct Pipe {
    pipeline: wgpu::RenderPipeline,
    group1: wgpu::BindGroupLayout,
    textures: Vec<(u32, bool)>,
    vfetch_keys: Vec<u32>,
}

impl PipelineKey {
    /// One line of the pipeline list (`warm`).
    fn to_line(&self) -> String {
        format!(
            "{:016x} {:016x} {} {:x} {:x} {:?} {:x} {:x} {} {}",
            self.vs,
            self.ps,
            self.topology,
            self.blend,
            self.color_mask,
            self.format,
            self.depth,
            self.cull,
            self.samples,
            self.alpha_to_coverage as u8
        )
    }

    fn from_line(l: &str) -> Option<PipelineKey> {
        use wgpu::TextureFormat as T;
        let w: Vec<&str> = l.split_whitespace().collect();
        let hex = |i: usize| w.get(i).and_then(|s| u64::from_str_radix(s, 16).ok());
        let format = match *w.get(5)? {
            "Rgba8Unorm" => T::Rgba8Unorm,
            "Rgb10a2Unorm" => T::Rgb10a2Unorm,
            "R32Float" => T::R32Float,
            "Rg32Float" => T::Rg32Float,
            "Rgba16Float" => T::Rgba16Float,
            _ => return None,
        };
        Some(PipelineKey {
            vs: hex(0)?,
            ps: hex(1)?,
            topology: w.get(2)?.parse().ok()?,
            blend: hex(3)? as u32,
            color_mask: hex(4)? as u32,
            format,
            depth: hex(6)? as u32,
            cull: hex(7)? as u32,
            samples: w.get(8)?.parse().ok()?,
            alpha_to_coverage: *w.get(9)? == "1",
        })
    }
}

/// Pipelines the renderer is building for a frame that waits on them. The
/// warm-up starts no build while any is: the driver's compiles share the CPU,
/// and with a cold driver cache (after the shaders change) the warm-up ran for
/// two minutes and a frame's one missing pipeline took 3.6 s behind it
/// (2026-11-25).
static RENDER_COMPILING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Marks the renderer as building pipelines while it lives.
struct Compiling;

impl Compiling {
    fn start() -> Compiling {
        RENDER_COMPILING.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Compiling
    }
}

impl Drop for Compiling {
    fn drop(&mut self) {
        RENDER_COMPILING.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

/// Waits while the renderer is building pipelines (the warm-up's turn comes after).
fn yield_to_renderer() {
    while RENDER_COMPILING.load(std::sync::atomic::Ordering::Acquire) > 0 {
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

/// The pipelines earlier runs built, rebuilt in the background at start (the
/// driver's cache makes that quick) so that draws don't wait for a first-time
/// compile mid-game. The list (`private/pipeline-keys.txt`, or
/// `WWE14_PIPELINE_KEYS`) grows as new pipelines are built; it derives from the
/// game's shaders, so it stays in `private/`.
struct Warm {
    path: PathBuf,
    /// Keys in the list, so each is written once.
    listed: HashSet<PipelineKey>,
    /// Pipelines the background thread built, for the renderer to take (in a
    /// mutex: the window host keeps the renderer in a shared resource).
    built: std::sync::Mutex<std::sync::mpsc::Receiver<(PipelineKey, Arc<Pipe>)>>,
    /// Every translated shader's module, created after those pipelines: a
    /// pipeline never built before then waits only for the driver.
    loaded: std::sync::Mutex<std::sync::mpsc::Receiver<((u64, bool), Arc<Shader>)>>,
}

impl Warm {
    fn start(device: &wgpu::Device, group0: &wgpu::BindGroupLayout, cache: Option<&wgpu::PipelineCache>, dirs: &[PathBuf]) -> Warm {
        let path = std::env::var_os("WWE14_PIPELINE_KEYS").map_or_else(|| PathBuf::from("private/pipeline-keys.txt"), PathBuf::from);
        let keys: Vec<PipelineKey> = std::fs::read_to_string(&path).unwrap_or_default().lines().filter_map(PipelineKey::from_line).collect();
        let listed: HashSet<PipelineKey> = keys.iter().copied().collect();
        let (tx, built) = std::sync::mpsc::channel();
        let (shader_tx, loaded) = std::sync::mpsc::channel();
        let (device, group0, cache, dirs) = (device.clone(), group0.clone(), cache.cloned(), dirs.to_vec());
        {
            let _ = std::thread::Builder::new().name("pipeline warm-up".into()).spawn(move || {
                gpu::make_background();
                let t = std::time::Instant::now();
                // Half the cores: the game is starting up on the others.
                let workers = std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).max(1));
                let shaders: std::sync::Mutex<HashMap<(u64, bool), Option<Arc<Shader>>>> = Default::default();
                let shader = |id: u64, vertex: bool| -> Option<Arc<Shader>> {
                    if let Some(s) = shaders.lock().unwrap().get(&(id, vertex)) {
                        return s.clone();
                    }
                    let s = load_shader(&device, &dirs, id, vertex).ok().map(Arc::new);
                    shaders.lock().unwrap().insert((id, vertex), s.clone());
                    s
                };
                let next = std::sync::atomic::AtomicUsize::new(0);
                let done = std::sync::atomic::AtomicUsize::new(0);
                std::thread::scope(|s| {
                    for _ in 0..workers {
                        s.spawn(|| {
                            gpu::make_background();
                            loop {
                                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                let Some(k) = keys.get(i) else { break };
                                yield_to_renderer();
                                let (Some(vs), Some(ps)) = (shader(k.vs, true), shader(k.ps, false)) else { continue };
                                if let Ok(p) = build_pipeline(&device, &group0, cache.as_ref(), &vs, &ps, *k) {
                                    done.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    if tx.send((*k, Arc::new(p))).is_err() {
                                        break;
                                    }
                                }
                            }
                        });
                    }
                });
                eprintln!("render: warmed {} of {} known pipelines in {} ms", done.into_inner(), keys.len(), t.elapsed().as_millis());
                // Then every other translated shader, in the same way.
                let t = std::time::Instant::now();
                let mut all: Vec<(u64, bool)> = Vec::new();
                for d in &dirs {
                    for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
                        let name = e.file_name().to_string_lossy().to_string();
                        let Some(stem) = name.strip_suffix(".wgsl") else { continue };
                        let (vertex, id) = match stem.split_once('_') {
                            Some(("vs", id)) => (true, id),
                            Some(("ps", id)) => (false, id),
                            _ => continue,
                        };
                        if let Ok(id) = u64::from_str_radix(id, 16) {
                            all.push((id, vertex));
                        }
                    }
                }
                let next = std::sync::atomic::AtomicUsize::new(0);
                std::thread::scope(|s| {
                    for _ in 0..workers {
                        s.spawn(|| {
                            gpu::make_background();
                            loop {
                                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                let Some(&(id, vertex)) = all.get(i) else { break };
                                if let Some(sh) = shader(id, vertex)
                                    && shader_tx.send(((id, vertex), sh)).is_err()
                                {
                                    break;
                                }
                            }
                        });
                    }
                });
                eprintln!("render: loaded {} shaders in {} ms", all.len(), t.elapsed().as_millis());
            });
        }
        Warm { path, listed, built: std::sync::Mutex::new(built), loaded: std::sync::Mutex::new(loaded) }
    }

    /// Adds newly built pipelines to the list.
    fn note(&mut self, keys: &[PipelineKey]) {
        let new: Vec<String> = keys.iter().filter(|k| self.listed.insert(**k)).map(|k| k.to_line() + "\n").collect();
        if new.is_empty() {
            return;
        }
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.path) {
            let _ = f.write_all(new.concat().as_bytes());
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct PipelineKey {
    vs: u64,
    ps: u64,
    topology: u8,
    blend: u32,
    color_mask: u32,
    format: wgpu::TextureFormat,
    /// `RB_DEPTHCONTROL` bits used: z enable (1), z write (2), z function (4-6).
    depth: u32,
    /// `PA_SU_SC_MODE_CNTL` bits 0-2: cull front, cull back, clockwise front.
    cull: u32,
    /// The target's samples, and alpha to coverage (`RB_COLORCONTROL` bit 4,
    /// alpha to mask, when multisampled).
    samples: u32,
    alpha_to_coverage: bool,
}

/// A render target or depth buffer: which surface D3D bound.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct SurfaceKey {
    /// EDRAM base (tiles), as `RB_COLOR_INFO`/`RB_DEPTH_INFO` give it.
    base: u32,
    /// Colour format (`RB_COLOR_INFO` bits 16-19), or 0x100 for depth.
    format: u32,
    /// Pitch in pixels (`RB_SURFACE_INFO` bits 0-13).
    pitch: u32,
    /// Samples per pixel (`RB_SURFACE_INFO` bits 16-17), as the host draws them.
    samples: u32,
}

struct Surface {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    /// Depth buffers: a depth-only view, read by depth resolves.
    depth_view: Option<wgpu::TextureView>,
    format: wgpu::TextureFormat,
    /// The texture's size in host pixels (`up` of the guest size).
    width: u32,
    height: u32,
    /// Its size in guest pixels: the game's coordinate space for it.
    guest: (u32, u32),
    samples: u32,
}

/// Host samples for a surface: the console's MSAA (`RB_SURFACE_INFO` bits
/// 16-17: 1, 2 or 4 samples) where the format can be multisampled. Core wgpu
/// offers 1 or 4 samples, so 2x is drawn with 4. `WWE14_MSAA=0` turns it off.
fn host_samples(surface_info: u32, color: wgpu::TextureFormat) -> u32 {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *OFF.get_or_init(|| std::env::var("WWE14_MSAA").is_ok_and(|v| v == "0")) {
        return 1;
    }
    let multisampled = matches!(color, wgpu::TextureFormat::Rgba8Unorm | wgpu::TextureFormat::Rgb10a2Unorm | wgpu::TextureFormat::Rgba16Float);
    match (surface_info >> 16) & 3 {
        1 | 2 if multisampled => 4,
        _ => 1,
    }
}

/// A texture a resolve wrote, by its guest address.
struct Resolved {
    /// Array layers: six for a cube map (one per face), else one.
    layers: u32,
    /// Mip levels (the fetch constant's), each written by a resolve that names it.
    mips: u32,
    /// Cube view, for a cube map sampled as a cube.
    cube: Option<Arc<wgpu::TextureView>>,
    texture: wgpu::Texture,
    view: Arc<wgpu::TextureView>,
    format: wgpu::TextureFormat,
    /// Host pixels (`up` of the guest size), and the fetch constant's size.
    width: u32,
    height: u32,
    guest: (u32, u32),
}

pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    shader_dirs: Vec<PathBuf>,
    shaders: HashMap<(u64, bool), Option<Arc<Shader>>>,
    pipelines: HashMap<PipelineKey, Option<Arc<Pipe>>>,
    group0: wgpu::BindGroupLayout,
    /// Uploaded textures, by texture slot (`texture_view`).
    textures: HashMap<u64, Uploaded>,
    samplers: HashMap<u32, Arc<wgpu::Sampler>>,
    surfaces: HashMap<SurfaceKey, Surface>,
    resolved: HashMap<u32, Resolved>,
    /// The colour target drawn last (shown when nothing was resolved to the front buffer).
    last_target: Option<SurfaceKey>,
    blit: Blit,
    depth_copy: DepthCopy,
    ms_resolve: MsResolve,
    clear_module: wgpu::ShaderModule,
    clear_layout: wgpu::BindGroupLayout,
    clear_pipelines: HashMap<ClearKey, Arc<wgpu::RenderPipeline>>,
    dummy_2d: Arc<wgpu::TextureView>,
    dummy_cube: Arc<wgpu::TextureView>,
    reported: HashSet<String>,
    /// The frame being prepared's draw data, and its number.
    arena: Arena,
    frame_buffers: FrameBuffers,
    /// `WWE14_RECORD`: every rendered frame to a video (debugging).
    recorder: Option<record::Recorder>,
    frame_number: u64,
    perf: RenderPerf,
    /// The driver's compiled pipelines, kept on disk between runs (where the
    /// device supports it).
    pipeline_cache: Option<wgpu::PipelineCache>,
    /// Earlier runs' pipelines, rebuilt in the background (`Warm`).
    warm: Warm,
    pipeline_cache_path: Option<PathBuf>,
    /// Pipelines built, and how many were when the cache was last saved.
    pipelines_built: u64,
    pipelines_saved: u64,
    /// Texture bind groups by pipeline and bound views and samplers (whose
    /// `Arc`s the entry keeps, so the addresses in the key stay theirs).
    group1s: HashMap<Vec<usize>, (Arc<wgpu::BindGroup>, Vec<Arc<wgpu::TextureView>>, Vec<Arc<wgpu::Sampler>>, Arc<Pipe>)>,
    /// The displayed image (`WIDTH` × `HEIGHT`, `FORMAT`).
    pub target: wgpu::Texture,
    target_view: wgpu::TextureView,
    pub frames: u64,
    pub draws_drawn: u64,
    pub draws_skipped: u64,
    /// This frame's new textures, texel bytes uploaded and new bind groups
    /// (slow-frame logs).
    made: [u64; 3],
    slow_logged: u32,
}

/// The translated shaders' per-draw blocks, in their `DrawRec` order, as rows
/// (16 bytes) of the frame's row buffer `U`: the vertex constants' block
/// table, bool and loop constants, fetch constants, the vertex memory window
/// table, the vertex fetch descriptors (one row per fetch), the pixel
/// constants' block table, the position transform and the alpha test. A
/// short block is padded with zeros to this many rows (what the shaders may
/// index). A block table is 16 first rows, one per block of 16 constant
/// registers (`ir::Consts`), each block in `U` once a frame.
const BLOCK_ROWS: [usize; 8] = [4, 10, 48, 8, 0, 4, 2, 1];

/// A draw's window scissor (x1, y1, x2, y2, guest pixels, right and bottom
/// exclusive). D3D's `SetScissorRect` (`sub_82CAB280`) keeps the effective
/// rectangle in its register shadow at device `+0x28C4`/`+0x28C8` (`y << 16 |
/// x`, 15 bits each), which the capture records as the first block's words 17
/// and 18 (`0x2011`, `0x2012`): the game's rectangle clipped to the viewport,
/// or the viewport itself when scissoring is off. Its packet writer
/// (`D3DDevice_WriteViewportScissor`, native and empty) would have sent the
/// same rectangle to `PA_SC_WINDOW_SCISSOR` with a zero window offset. Menus
/// clip their scrolling lists with it: without it, list items past a panel's
/// edge were drawn outside the panel (reported by the user, 2026-11-25).
fn window_scissor(reg: &impl Fn(u32) -> u32) -> [u32; 4] {
    let (tl, br) = (reg(0x2011), reg(0x2012));
    [tl & 0x7FFF, (tl >> 16) & 0x7FFF, br & 0x7FFF, (br >> 16) & 0x7FFF]
}

/// Most vertex fetches a draw can have (rows of its descriptor block).
const MAX_VFETCH: usize = 16;

fn uniform(binding: u32, visibility: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
        count: None,
    }
}

/// A read-only storage buffer of the translated shaders' group 0.
fn storage(binding: u32, visibility: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only: true }, has_dynamic_offset: false, min_binding_size: None },
        count: None,
    }
}

/// A buffer kept from frame to frame and rewritten each frame (`queue.write_buffer`),
/// replaced by a larger one when a frame's data outgrows it.
struct Persistent {
    buffer: wgpu::Buffer,
    usage: wgpu::BufferUsages,
    label: &'static str,
}

impl Persistent {
    fn new(device: &wgpu::Device, label: &'static str, usage: wgpu::BufferUsages) -> Persistent {
        let usage = usage | wgpu::BufferUsages::COPY_DST;
        Persistent { buffer: device.create_buffer(&wgpu::BufferDescriptor { label: Some(label), size: 1 << 20, usage, mapped_at_creation: false }), usage, label }
    }

    /// Writes `bytes` at the start; true when the buffer had to be replaced
    /// (bind groups that hold it must be made again).
    fn write(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, bytes: &[u8]) -> bool {
        let need = (bytes.len() as u64).next_multiple_of(wgpu::COPY_BUFFER_ALIGNMENT).max(16);
        let grown = need > self.buffer.size();
        if grown {
            let size = need.next_power_of_two().min(device.limits().max_buffer_size).max(need);
            self.buffer = device.create_buffer(&wgpu::BufferDescriptor { label: Some(self.label), size, usage: self.usage, mapped_at_creation: false });
        }
        if !bytes.is_empty() {
            if bytes.len() % 4 == 0 {
                queue.write_buffer(&self.buffer, 0, bytes);
            } else {
                let mut padded = bytes.to_vec();
                padded.resize(bytes.len().next_multiple_of(4), 0);
                queue.write_buffer(&self.buffer, 0, &padded);
            }
        }
        grown
    }
}

/// The frame data buffers the draws read (kept between frames) and the group
/// 0 bind group over them.
struct FrameBuffers {
    rows: Persistent,
    draws: Persistent,
    vertex: Persistent,
    indices: Persistent,
    group0: wgpu::BindGroup,
}

impl FrameBuffers {
    fn new(device: &wgpu::Device, layout: &wgpu::BindGroupLayout) -> FrameBuffers {
        let rows = Persistent::new(device, "draw rows", wgpu::BufferUsages::STORAGE);
        let draws = Persistent::new(device, "draw records", wgpu::BufferUsages::STORAGE);
        let vertex = Persistent::new(device, "vertex memory", wgpu::BufferUsages::STORAGE);
        let indices = Persistent::new(device, "indices", wgpu::BufferUsages::INDEX);
        let group0 = Self::group0(device, layout, &rows, &draws, &vertex);
        FrameBuffers { rows, draws, vertex, indices, group0 }
    }

    fn group0(device: &wgpu::Device, layout: &wgpu::BindGroupLayout, rows: &Persistent, draws: &Persistent, vertex: &Persistent) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("frame draws"),
            layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: rows.buffer.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: draws.buffer.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: vertex.buffer.as_entire_binding() },
            ],
        })
    }

    /// Uploads a frame's arena.
    fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, layout: &wgpu::BindGroupLayout, a: &Arena) {
        let mut grown = self.rows.write(device, queue, as_bytes(&a.rows));
        grown |= self.draws.write(device, queue, as_bytes(&a.draws));
        grown |= self.vertex.write(device, queue, &a.vertex);
        self.indices.write(device, queue, &a.indices);
        if grown {
            self.group0 = Self::group0(device, layout, &self.rows, &self.draws, &self.vertex);
        }
    }
}

/// The renderer's frame times, logged every 5 s: pipelines built, draws
/// prepared, and encoding and submission, per frame, and the longest frame.
#[derive(Default)]
struct RenderPerf {
    start: Option<std::time::Instant>,
    frames: u32,
    total: std::time::Duration,
    compile: std::time::Duration,
    prepare: std::time::Duration,
    longest: std::time::Duration,
}

impl RenderPerf {
    fn frame(&mut self, total: std::time::Duration, compile: std::time::Duration, prepare: std::time::Duration) {
        let start = *self.start.get_or_insert_with(std::time::Instant::now);
        self.frames += 1;
        self.total += total;
        self.compile += compile;
        self.prepare += prepare;
        self.longest = self.longest.max(total);
        let secs = start.elapsed().as_secs_f64();
        if secs >= 5.0 {
            let n = self.frames as f64;
            let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0 / n;
            eprintln!(
                "perf: renderer {:.1} frames/s, {:.1} ms/frame (pipelines {:.1}, prepare {:.1}, encode and submit {:.1}), longest {:.0} ms",
                n / secs,
                ms(self.total),
                ms(self.compile),
                ms(self.prepare),
                ms(self.total.saturating_sub(self.compile + self.prepare)),
                self.longest.as_secs_f64() * 1000.0
            );
            *self = RenderPerf::default();
        }
    }
}

/// A captured texture on the GPU: the texture and its view, the contents it
/// holds (`ir::Texture::key`), its size and kind, and the frame that last used it.
struct Uploaded {
    texture: wgpu::Texture,
    view: Arc<wgpu::TextureView>,
    content: u64,
    size: (u32, u32, u32),
    format: wgpu::TextureFormat,
    used: u64,
}

/// One frame's draw data, uploaded in four buffers: the rows every draw's
/// blocks are in (identical consecutive blocks, and constant files the
/// capture shares, written once), a record per draw of where its blocks
/// start, the vertex memory the draws read (each range once), and the index
/// lists. The shaders find their draw's record by instance index.
#[derive(Default)]
struct Arena {
    rows: Vec<[u32; 4]>,
    /// Per draw: the first row of each block (`BLOCK_ROWS` order).
    draws: Vec<[u32; 8]>,
    /// Per block kind: the last one written (first row, rows).
    last: [Option<(u32, u32)>; 8],
    /// Constant files the capture shares between draws (`Arc` address, block
    /// kind): their block table's first row.
    shared: HashMap<(usize, usize), u32>,
    /// Constant blocks written this frame (`Arc` address): their first row.
    const_blocks: HashMap<usize, u32>,
    /// Per stage, the previous draw's blocks (address, first row): most of a
    /// draw's blocks are the previous draw's.
    last_blocks: [[(usize, u32); 16]; 2],
    /// Debug overrides' constants (`WWE14_DEBUG_CONSTS`), kept to the frame's end
    /// so no other block can take their addresses.
    keep: Vec<gpu::ir::Consts>,
    vertex: Vec<u8>,
    /// Vertex ranges written: (first word, bytes) -> (word offset, data).
    ranges: HashMap<(u32, usize), Vec<(u32, Arc<Vec<u8>>)>>,
    indices: Vec<u8>,
    /// Index lists the capture shares between draws (one `Arc` per list and
    /// frame): where each was written.
    shared_indices: HashMap<usize, u32>,
}

impl Arena {
    /// Adds an index list the capture shares (`key`: its `Arc`'s address),
    /// written once a frame however many draws use it.
    fn shared_indices(&mut self, key: usize, list: &[u32]) -> u32 {
        if let Some(&off) = self.shared_indices.get(&key) {
            return off;
        }
        let off = self.indices(list);
        self.shared_indices.insert(key, off);
        off
    }

    /// Adds a block of kind `k` (`BLOCK_ROWS` index); returns its first row.
    fn block(&mut self, k: usize, rows: &[[u32; 4]]) -> u32 {
        if let Some((off, len)) = self.last[k]
            && len as usize == rows.len()
            && self.rows[off as usize..off as usize + rows.len()] == *rows
        {
            return off;
        }
        let off = self.rows.len();
        self.rows.extend_from_slice(rows);
        self.rows.resize(off + rows.len().max(BLOCK_ROWS[k]), [0; 4]);
        self.last[k] = Some((off as u32, rows.len() as u32));
        off as u32
    }

    /// Adds a stage's constants (`stage` 0 vertex, 1 pixel; block kind `k`):
    /// each block not yet written this frame, then the draw's table of where
    /// its 16 blocks are. Returns the table's first row.
    fn consts(&mut self, stage: usize, k: usize, c: &gpu::ir::Consts) -> u32 {
        let mut table = [0u32; 16];
        for (i, block) in c.0.iter().enumerate() {
            let key = Arc::as_ptr(block) as usize;
            let (last_key, last_row) = self.last_blocks[stage][i];
            table[i] = if last_key == key {
                last_row
            } else {
                let row = match self.const_blocks.get(&key) {
                    Some(&row) => row,
                    None => {
                        let row = self.rows.len() as u32;
                        self.rows.extend_from_slice(as_rows(&block[..]));
                        self.const_blocks.insert(key, row);
                        row
                    }
                };
                self.last_blocks[stage][i] = (key, row);
                row
            };
        }
        self.block(k, as_rows(&table))
    }

    /// `consts` for a file the capture shares between draws: its table is
    /// written once a frame however many draws use it.
    fn shared_consts(&mut self, stage: usize, k: usize, c: &Arc<gpu::ir::Consts>) -> u32 {
        let key = (Arc::as_ptr(c) as usize, k);
        if let Some(&off) = self.shared.get(&key) {
            return off;
        }
        let off = self.consts(stage, k, c);
        self.shared.insert(key, off);
        off
    }

    /// Adds a draw's record; returns its number (the instance it's drawn as).
    fn draw(&mut self, record: [u32; 8]) -> u32 {
        self.draws.push(record);
        (self.draws.len() - 1) as u32
    }

    /// Empties it for the next frame (keeping the allocations).
    fn clear(&mut self) {
        self.rows.clear();
        self.draws.clear();
        self.last = Default::default();
        self.shared.clear();
        self.const_blocks.clear();
        self.last_blocks = Default::default();
        self.keep.clear();
        self.vertex.clear();
        self.ranges.clear();
        self.indices.clear();
        self.shared_indices.clear();
    }

    /// Adds a vertex memory range; returns where it starts, in words.
    fn vertex_range(&mut self, first: u32, bytes: &Arc<Vec<u8>>) -> u32 {
        let entry = self.ranges.entry((first, bytes.len())).or_default();
        if let Some((off, _)) = entry.iter().find(|(_, b)| Arc::ptr_eq(b, bytes) || **b == **bytes) {
            return *off;
        }
        let off = (self.vertex.len() / 4) as u32;
        self.vertex.extend_from_slice(bytes);
        self.vertex.resize(self.vertex.len().next_multiple_of(4), 0);
        entry.push((off, bytes.clone()));
        off
    }

    /// Adds an index list; returns its byte offset.
    fn indices(&mut self, list: &[u32]) -> u32 {
        let off = self.indices.len() as u32;
        self.indices.extend(list.iter().flat_map(|i| i.to_le_bytes()));
        off
    }
}

/// Little-endian bytes of plain 32-bit values (the host is little-endian).
fn as_bytes<T: Copy>(v: &[T]) -> &[u8] {
    // SAFETY: T is plain data (f32, u32 and arrays of them) here; any byte pattern is valid to read.
    unsafe { std::slice::from_raw_parts(v.as_ptr().cast::<u8>(), std::mem::size_of_val(v)) }
}

/// Plain 32-bit values (f32, u32 and arrays of them) as 16-byte rows of words.
fn as_rows<T: Copy>(v: &[T]) -> &[[u32; 4]] {
    assert!(std::mem::align_of::<T>() >= 4 && std::mem::size_of_val(v) % 16 == 0, "rows of 32-bit values");
    // SAFETY: T is 4-byte aligned plain data whose size is a whole number of rows; any bit pattern is a valid u32.
    unsafe { std::slice::from_raw_parts(v.as_ptr().cast::<[u32; 4]>(), std::mem::size_of_val(v) / 16) }
}

/// The wgpu format for a Xenos colour render-target format.
fn color_format(f: u32) -> wgpu::TextureFormat {
    use wgpu::TextureFormat as T;
    match f {
        0 | 1 => T::Rgba8Unorm,
        2 | 10 => T::Rgb10a2Unorm,
        14 => T::R32Float,
        15 => T::Rg32Float,
        3..=7 | 12 => T::Rgba16Float,
        _ => T::Rgba8Unorm,
    }
}

fn blendable(f: wgpu::TextureFormat) -> bool {
    !matches!(f, wgpu::TextureFormat::R32Float | wgpu::TextureFormat::Rg32Float | wgpu::TextureFormat::Rgba32Float)
}

/// The mip levels a resolve destination's fetch constant declares: levels 0
/// to `mip_max_level` (word 4 bits 6-9) when it has a mip address (word 5
/// bits 12-31), within what its size allows.
fn resolve_mips(w: [u32; 6], width: u32, height: u32) -> u32 {
    let full = 32 - width.max(height).max(1).leading_zeros();
    if w[5] >> 12 == 0 { 1 } else { (((w[4] >> 6) & 0xF) + 1).clamp(1, full) }
}

/// A fullscreen copy (our own small shader, not the game's): samples the top
/// left `scale` part of a texture into the displayed image.
struct Blit {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
}

const BLIT_WGSL: &str = r#"
struct Params { scale: vec4<f32> }
@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var smp: sampler;
@group(0) @binding(2) var<uniform> P: Params;
struct Out { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> }
@vertex fn vs(@builtin(vertex_index) i: u32) -> Out {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var o: Out;
    o.pos = vec4<f32>(p * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0), 0.0, 1.0);
    o.uv = p * P.scale.xy;
    return o;
}
@fragment fn fs(i: Out) -> @location(0) vec4<f32> {
    let c = textureSample(src, smp, i.uv);
    return vec4<f32>(c.rgb, 1.0);
}
"#;

/// Rectangle clears (our own small shader, not the game's): a full-screen
/// triangle, scissored to each rectangle, writes the clear colour, the clear
/// depth (as its Z) and the clear stencil (reference value, replace).
const CLEAR_WGSL: &str = r#"
struct Params { color: vec4<f32>, depth: vec4<f32> }
@group(0) @binding(0) var<uniform> P: Params;
@vertex fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(p * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0), P.depth.x, 1.0);
}
@fragment fn fs() -> @location(0) vec4<f32> {
    return P.color;
}
"#;

/// Which parts of the bound targets a rectangle clear writes.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct ClearKey {
    format: wgpu::TextureFormat,
    color: bool,
    depth: bool,
    stencil: bool,
    samples: u32,
}

/// A depth resolve (our own small shader, not the game's): copies a depth
/// buffer's values, offset by `P.offset`, into a single-channel float
/// texture the game's shaders then sample (depth in every component).
struct DepthCopy {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    /// The same from a multisampled depth buffer (its first sample).
    ms_pipeline: wgpu::RenderPipeline,
    ms_layout: wgpu::BindGroupLayout,
    /// `R32Float` where the device can filter it, else `R16Float`.
    format: wgpu::TextureFormat,
}

const DEPTH_COPY_WGSL: &str = r#"
struct Params { offset: vec4<i32> }
@group(0) @binding(0) var src: texture_depth_2d;
@group(0) @binding(1) var<uniform> P: Params;
@vertex fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(p * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0), 0.0, 1.0);
}
@fragment fn fs(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let d = textureLoad(src, vec2<i32>(pos.xy) + P.offset.xy, 0);
    return vec4<f32>(d, d, d, d);
}
"#;

/// A full-screen copy pipeline of ours: binding 0 the source texture (of
/// `sample_type`, multisampled or not), binding 1 the offset uniform, drawing
/// into `format`.
fn copy_pipeline(
    device: &wgpu::Device,
    label: &str,
    wgsl: &str,
    sample_type: wgpu::TextureSampleType,
    multisampled: bool,
    format: wgpu::TextureFormat,
) -> (wgpu::RenderPipeline, wgpu::BindGroupLayout) {
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some(label), source: wgpu::ShaderSource::Wgsl(wgsl.into()) });
    let f = wgpu::ShaderStages::FRAGMENT;
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some(label),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: f,
                ty: wgpu::BindingType::Texture { sample_type, view_dimension: wgpu::TextureViewDimension::D2, multisampled },
                count: None,
            },
            uniform(1, f),
        ],
    });
    let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: None, bind_group_layouts: &[Some(&layout)], immediate_size: 0 });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(&pl),
        vertex: wgpu::VertexState { module: &module, entry_point: Some("vs"), compilation_options: Default::default(), buffers: &[] },
        primitive: Default::default(),
        depth_stencil: None,
        multisample: Default::default(),
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some("fs"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState { format, blend: None, write_mask: wgpu::ColorWrites::ALL })],
        }),
        multiview_mask: None,
        cache: None,
    });
    (pipeline, layout)
}

/// A resolve of a multisampled surface (our own small shader): each pixel is
/// the average of its samples, as the console's resolve writes it.
const MS_RESOLVE_WGSL: &str = r#"
struct Params { offset: vec4<i32> }
@group(0) @binding(0) var src: texture_multisampled_2d<f32>;
@group(0) @binding(1) var<uniform> P: Params;
@vertex fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(p * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0), 0.0, 1.0);
}
@fragment fn fs(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let c = vec2<i32>(pos.xy) + P.offset.xy;
    let n = textureNumSamples(src);
    var sum = vec4<f32>(0.0);
    for (var s = 0u; s < n; s = s + 1u) {
        sum = sum + textureLoad(src, c, i32(s));
    }
    return sum / f32(n);
}
"#;

/// Multisampled colour resolves, one pipeline (and its layout) per format that
/// can be multisampled.
struct MsResolve {
    pipelines: HashMap<wgpu::TextureFormat, (wgpu::RenderPipeline, wgpu::BindGroupLayout)>,
}

impl MsResolve {
    fn new(device: &wgpu::Device) -> MsResolve {
        let pipelines = [wgpu::TextureFormat::Rgba8Unorm, wgpu::TextureFormat::Rgb10a2Unorm, wgpu::TextureFormat::Rgba16Float]
            .into_iter()
            .map(|format| (format, copy_pipeline(device, "msaa resolve", MS_RESOLVE_WGSL, wgpu::TextureSampleType::Float { filterable: false }, true, format)))
            .collect();
        MsResolve { pipelines }
    }

    /// Averages `src` (multisampled) into `dst` (a single-sampled view of the
    /// same format) over `dst_rect` (x, y, w, h), reading from `dst_rect`'s
    /// corner plus `offset` in the source.
    fn encode(
        &self,
        device: &wgpu::Device,
        enc: &mut wgpu::CommandEncoder,
        src: &wgpu::TextureView,
        format: wgpu::TextureFormat,
        dst: &wgpu::TextureView,
        dst_rect: [u32; 4],
        offset: [i32; 2],
    ) -> bool {
        let Some((pipeline, layout)) = self.pipelines.get(&format) else { return false };
        let params = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: None,
            contents: &[offset[0], offset[1], 0, 0].iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>(),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(src) },
                wgpu::BindGroupEntry { binding: 1, resource: params.as_entire_binding() },
            ],
        });
        let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("msaa resolve"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: dst,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &group, &[]);
        let [x, y, w, h] = dst_rect;
        pass.set_scissor_rect(x, y, w, h);
        pass.draw(0..3, 0..1);
        true
    }
}

impl DepthCopy {
    fn new(device: &wgpu::Device) -> DepthCopy {
        let format = if device.features().contains(wgpu::Features::FLOAT32_FILTERABLE) {
            wgpu::TextureFormat::R32Float
        } else {
            wgpu::TextureFormat::R16Float
        };
        let (pipeline, layout) = copy_pipeline(device, "depth copy", DEPTH_COPY_WGSL, wgpu::TextureSampleType::Depth, false, format);
        // A multisampled depth buffer resolves its first sample.
        let ms_wgsl = DEPTH_COPY_WGSL.replace("texture_depth_2d", "texture_depth_multisampled_2d");
        let (ms_pipeline, ms_layout) = copy_pipeline(device, "depth copy (msaa)", &ms_wgsl, wgpu::TextureSampleType::Depth, true, format);
        DepthCopy { pipeline, layout, ms_pipeline, ms_layout, format }
    }
}

impl Renderer {
    /// `shader_dirs`: where the translated shaders are (`vs_<id>.wgsl`, `ps_<id>.wgsl`).
    pub fn new(device: wgpu::Device, queue: wgpu::Queue, shader_dirs: Vec<PathBuf>) -> Renderer {
        let (v, f) = (wgpu::ShaderStages::VERTEX, wgpu::ShaderStages::FRAGMENT);
        let group0 = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("xenos group 0"),
            entries: &[storage(0, v | f), storage(1, v | f), storage(3, v)],
        });
        // The pixel shaders get their draw's number from the vertex shader, past
        // the 16 interpolators: one more inter-stage variable than WebGPU's minimum.
        let inter_stage = device.limits().max_inter_stage_shader_variables;
        assert!(inter_stage >= 17, "render: the device passes {inter_stage} inter-stage variables; the translated shaders need 17");
        let frame_buffers = FrameBuffers::new(&device, &group0);
        // Compressed textures go up as BC blocks when the device samples them
        // (`gpu::texture::bc_passthrough`); otherwise everything is decoded on the CPU.
        let bc = device.features().contains(wgpu::Features::TEXTURE_COMPRESSION_BC);
        gpu::texture::BC_UPLOAD.store(bc, std::sync::atomic::Ordering::Relaxed);
        eprintln!("render: BC textures {}", if bc { "uploaded as stored" } else { "decoded on the CPU (no BC support)" });
        let recorder = std::env::var_os("WWE14_RECORD").and_then(|d| record::Recorder::start(&device, std::path::Path::new(&d), output_size().0, output_size().1));
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("display"),
            size: wgpu::Extent3d { width: output_size().0, height: output_size().1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let target_view = target.create_view(&Default::default());
        let white = |layers: u32, dim: wgpu::TextureViewDimension| {
            let t = device.create_texture_with_data(
                &queue,
                &wgpu::TextureDescriptor {
                    label: Some("missing texture"),
                    size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: layers },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
                wgpu::util::TextureDataOrder::LayerMajor,
                &vec![255u8; 4 * layers as usize],
            );
            Arc::new(t.create_view(&wgpu::TextureViewDescriptor { dimension: Some(dim), ..Default::default() }))
        };
        let dummy_2d = white(1, wgpu::TextureViewDimension::D2);
        let dummy_cube = white(6, wgpu::TextureViewDimension::Cube);
        let blit = {
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("blit"),
                source: wgpu::ShaderSource::Wgsl(BLIT_WGSL.into()),
            });
            let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("blit"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: f,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: f,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                    uniform(2, v),
                ],
            });
            let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: None,
                bind_group_layouts: &[Some(&layout)],
                immediate_size: 0,
            });
            let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("blit"),
                layout: Some(&pl),
                vertex: wgpu::VertexState { module: &module, entry_point: Some("vs"), compilation_options: Default::default(), buffers: &[] },
                primitive: Default::default(),
                depth_stencil: None,
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState { format: FORMAT, blend: None, write_mask: wgpu::ColorWrites::ALL })],
                }),
                multiview_mask: None,
                cache: None,
            });
            let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            });
            Blit { pipeline, layout, sampler }
        };
        let depth_copy = DepthCopy::new(&device);
        let ms_resolve = MsResolve::new(&device);
        let clear_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rectangle clear"),
            source: wgpu::ShaderSource::Wgsl(CLEAR_WGSL.into()),
        });
        let clear_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("rectangle clear"),
            entries: &[uniform(0, v | f)],
        });
        // The driver's compiled pipelines from earlier runs. They derive from the
        // game's shaders, so they live under `private/` (or `WWE14_PIPELINE_CACHE`).
        let pipeline_cache_path = device
            .features()
            .contains(wgpu::Features::PIPELINE_CACHE)
            .then(|| std::env::var_os("WWE14_PIPELINE_CACHE").map_or_else(|| PathBuf::from("private/pipeline-cache.bin"), PathBuf::from));
        let pipeline_cache = pipeline_cache_path.as_ref().map(|p| {
            let data = std::fs::read(p).ok();
            eprintln!("render: pipeline cache {} ({} KB)", p.display(), data.as_ref().map_or(0, |d| d.len() / 1024));
            // SAFETY: the data is what `get_data` saved; with `fallback` wgpu starts empty when it doesn't suit this driver.
            unsafe { device.create_pipeline_cache(&wgpu::PipelineCacheDescriptor { label: Some("pipelines"), data: data.as_deref(), fallback: true }) }
        });
        let warm = Warm::start(&device, &group0, pipeline_cache.as_ref(), &shader_dirs);
        Renderer {
            warm,
            device,
            queue,
            shader_dirs,
            shaders: HashMap::new(),
            pipelines: HashMap::new(),
            group0,
            textures: HashMap::new(),
            samplers: HashMap::new(),
            surfaces: HashMap::new(),
            resolved: HashMap::new(),
            last_target: None,
            blit,
            depth_copy,
            ms_resolve,
            clear_module,
            clear_layout,
            clear_pipelines: HashMap::new(),
            dummy_2d,
            dummy_cube,
            reported: HashSet::new(),
            arena: Arena::default(),
            frame_buffers,
            recorder,
            frame_number: 0,
            perf: RenderPerf::default(),
            pipeline_cache,
            pipeline_cache_path,
            pipelines_built: 0,
            pipelines_saved: 0,
            group1s: HashMap::new(),
            target,
            target_view,
            frames: 0,
            draws_drawn: 0,
            draws_skipped: 0,
            made: [0; 3],
            slow_logged: 0,
        }
    }

    /// Reports a problem once (per distinct message).
    fn report(&mut self, msg: String) {
        if self.reported.insert(msg.clone()) {
            eprintln!("render: {msg}");
        }
    }

    fn shader(&mut self, id: u64, vertex: bool) -> Option<Arc<Shader>> {
        if let Some(s) = self.shaders.get(&(id, vertex)) {
            return s.clone();
        }
        let s = match load_shader(&self.device, &self.shader_dirs, id, vertex) {
            Ok(s) => Some(Arc::new(s)),
            Err(e) => {
                self.report(e);
                None
            }
        };
        self.shaders.insert((id, vertex), s.clone());
        s
    }

    fn pipeline(&mut self, key: PipelineKey) -> Option<Arc<Pipe>> {
        if let Some(p) = self.pipelines.get(&key) {
            return p.clone();
        }
        let _compiling = Compiling::start();
        let t = std::time::Instant::now();
        let p = (|| {
            let vs = self.shader(key.vs, true)?;
            let ps = self.shader(key.ps, false)?;
            match build_pipeline(&self.device, &self.group0, self.pipeline_cache.as_ref(), &vs, &ps, key) {
                Ok(p) => Some(Arc::new(p)),
                Err(e) => {
                    self.report(e);
                    None
                }
            }
        })();
        if t.elapsed().as_millis() >= 100 {
            eprintln!("render: a frame waited {} ms for pipeline vs {:016x} ps {:016x}", t.elapsed().as_millis(), key.vs, key.ps);
        }
        if p.is_some() {
            self.warm.note(&[key]);
        }
        self.pipelines.insert(key, p.clone());
        self.pipelines_built += 1;
        p
    }

    /// Builds the pipelines a frame needs that don't exist yet, on worker
    /// threads (the driver compiles each one, which takes long), so the
    /// frame's draws find them ready. Their shaders are loaded first, also in
    /// parallel.
    fn build_missing(&mut self, keys: &HashSet<PipelineKey>) {
        let missing: Vec<PipelineKey> = keys.iter().filter(|k| !self.pipelines.contains_key(k)).copied().collect();
        if missing.len() < 2 {
            return;
        }
        let _compiling = Compiling::start();
        let t = std::time::Instant::now();
        let shaders: Vec<(u64, bool)> = missing
            .iter()
            .flat_map(|k| [(k.vs, true), (k.ps, false)])
            .filter(|s| !self.shaders.contains_key(s))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let (device, dirs) = (&self.device, &self.shader_dirs);
        for (s, r) in shaders.iter().zip(parallel(&shaders, |&(id, vertex)| load_shader(device, dirs, id, vertex))) {
            let s2 = match r {
                Ok(sh) => Some(Arc::new(sh)),
                Err(e) => {
                    self.report(e);
                    None
                }
            };
            self.shaders.insert(*s, s2);
        }
        let jobs: Vec<(PipelineKey, Arc<Shader>, Arc<Shader>)> = missing
            .iter()
            .filter_map(|k| Some((*k, self.shaders.get(&(k.vs, true))?.clone()?, self.shaders.get(&(k.ps, false))?.clone()?)))
            .collect();
        let (device, group0, cache) = (&self.device, &self.group0, self.pipeline_cache.as_ref());
        let built = parallel(&jobs, |(k, vs, ps)| {
            let t = std::time::Instant::now();
            let p = build_pipeline(device, group0, cache, vs, ps, *k);
            // The driver takes seconds over a few shaders: name them.
            if t.elapsed().as_millis() >= 500 {
                eprintln!("render: pipeline vs {:016x} ps {:016x} took {} ms to build", k.vs, k.ps, t.elapsed().as_millis());
            }
            p
        });
        let mut ok = Vec::new();
        for ((k, _, _), r) in jobs.iter().zip(built) {
            let p = match r {
                Ok(p) => {
                    ok.push(*k);
                    Some(Arc::new(p))
                }
                Err(e) => {
                    self.report(e);
                    None
                }
            };
            self.pipelines.insert(*k, p);
        }
        self.warm.note(&ok);
        self.pipelines_built += jobs.len() as u64;
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 100 {
            eprintln!("render: built {} pipelines ({} shaders) in {} ms", jobs.len(), shaders.len(), t.elapsed().as_millis());
        }
    }
}

/// Runs `f` on every item on worker threads (one per core, less one), in order.
fn parallel<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get().saturating_sub(1).max(1)).min(items.len());
    let next = std::sync::atomic::AtomicUsize::new(0);
    let mut out: Vec<Option<R>> = (0..items.len()).map(|_| None).collect();
    let results = std::sync::Mutex::new(&mut out);
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(item) = items.get(i) else { break };
                    let r = f(item);
                    results.lock().unwrap()[i] = Some(r);
                }
            });
        }
    });
    out.into_iter().map(|r| r.expect("every item ran")).collect()
}

/// Reads a translated shader and creates its module.
fn load_shader(device: &wgpu::Device, dirs: &[PathBuf], id: u64, vertex: bool) -> Result<Shader, String> {
    let name = format!("{}_{id:016x}.wgsl", if vertex { "vs" } else { "ps" });
    let src = dirs
        .iter()
        .find_map(|d| std::fs::read_to_string(d.join(&name)).ok())
        .ok_or_else(|| format!("no translated shader {name}: its draws are skipped"))?;
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some(&name), source: wgpu::ShaderSource::Wgsl(src.as_str().into()) });
    Ok(shader_interface(&src, module))
}

/// A shader's interface, read from its declarations.
fn shader_interface(src: &str, module: wgpu::ShaderModule) -> Shader {
    let mut textures = Vec::new();
    for l in src.lines().filter(|l| l.starts_with("@group(1)") && l.contains(" var t")) {
        let n: u32 = l.split(" var t").nth(1).and_then(|s| s.split(':').next()).and_then(|s| s.parse().ok()).unwrap_or(0);
        textures.push((n, l.contains("texture_cube")));
    }
    let vfetch_keys = src
        .lines()
        .find_map(|l| l.strip_prefix("// vfetch-keys:"))
        .map(|k| k.split_whitespace().filter_map(|w| u32::from_str_radix(w, 16).ok()).collect())
        .unwrap_or_default();
    Shader { module, textures, vfetch_keys }
}

/// One pipeline's compile times in `bench_compile`.
#[doc(hidden)]
pub struct CompileTime {
    pub key: String,
    /// Creating both shader modules (wgpu's validation and SPIR-V output), in µs.
    pub modules_us: u128,
    /// Creating the pipeline (the driver's compile), in µs.
    pub pipeline_us: u128,
}

/// Measures the driver's compile of pipelines from the warm-up list's lines
/// (`private/pipeline-keys.txt`), one at a time on this thread, for the
/// compile-time work of Phase 4 (`examples/compile_bench.rs`). `edit` rewrites
/// each shader's WGSL first, to try a translation change on the real shaders.
/// `nonce` is written into each shader as a local's initial value, so the
/// driver's cache, keyed by the SPIR-V, misses unless the same nonce ran before.
/// `checked: false` drops wgpu's bounds checks.
#[doc(hidden)]
pub fn bench_compile(device: &wgpu::Device, dirs: &[PathBuf], lines: &[&str], nonce: u32, checked: bool, edit: &dyn Fn(&str, bool) -> String) -> Vec<CompileTime> {
    let (v, f) = (wgpu::ShaderStages::VERTEX, wgpu::ShaderStages::FRAGMENT);
    let group0 = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("xenos group 0"),
        entries: &[storage(0, v | f), storage(1, v | f), storage(3, v)],
    });
    let load = |id: u64, vertex: bool| -> Option<Shader> {
        let name = format!("{}_{id:016x}.wgsl", if vertex { "vs" } else { "ps" });
        let src = dirs.iter().find_map(|d| std::fs::read_to_string(d.join(&name)).ok())?;
        let mut src = edit(&src, vertex);
        let at = src.find("fn main(")?;
        let at = at + src[at..].find("{\n")? + 2;
        src.insert_str(at, &format!("    var bench_nonce: u32 = {nonce}u;\n"));
        let desc = wgpu::ShaderModuleDescriptor { label: Some(&name), source: wgpu::ShaderSource::Wgsl(src.as_str().into()) };
        let module = if checked {
            device.create_shader_module(desc)
        } else {
            // SAFETY: a benchmark only; nothing runs these modules.
            unsafe { device.create_shader_module_trusted(desc, wgpu::ShaderRuntimeChecks::unchecked()) }
        };
        Some(shader_interface(&src, module))
    };
    let mut out = Vec::new();
    for l in lines {
        let Some(key) = PipelineKey::from_line(l) else { continue };
        let t = std::time::Instant::now();
        let (Some(vs), Some(ps)) = (load(key.vs, true), load(key.ps, false)) else { continue };
        let modules_us = t.elapsed().as_micros();
        let t = std::time::Instant::now();
        if let Err(e) = build_pipeline(device, &group0, None, &vs, &ps, key) {
            eprintln!("bench: {l}: {e}");
            continue;
        }
        out.push(CompileTime { key: l.to_string(), modules_us, pipeline_us: t.elapsed().as_micros() });
    }
    out
}

/// The render pipeline for a draw state with its shaders.
fn build_pipeline(
    device: &wgpu::Device,
    group0: &wgpu::BindGroupLayout,
    cache: Option<&wgpu::PipelineCache>,
    vs: &Shader,
    ps: &Shader,
    key: PipelineKey,
) -> Result<Pipe, String> {
    {
        {
            let mut textures = vs.textures.clone();
            for t in &ps.textures {
                if let Some(o) = textures.iter().find(|o| o.0 == t.0) {
                    if o.1 != t.1 {
                        return Err(format!("vs {:016x} and ps {:016x} sample fetch constant {} as 2D and cube", key.vs, key.ps, t.0));
                    }
                } else {
                    textures.push(*t);
                }
            }
            let mut entries = Vec::new();
            for &(n, cube) in &textures {
                entries.push(wgpu::BindGroupLayoutEntry {
                    binding: 2 * n,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: if cube { wgpu::TextureViewDimension::Cube } else { wgpu::TextureViewDimension::D2 },
                        multisampled: false,
                    },
                    count: None,
                });
                entries.push(wgpu::BindGroupLayoutEntry {
                    binding: 2 * n + 1,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                });
            }
            let group1 = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some("xenos textures"), entries: &entries });
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: None,
                bind_group_layouts: &[Some(group0), Some(&group1)],
                immediate_size: 0,
            });
            let topology = match key.topology {
                0 => wgpu::PrimitiveTopology::PointList,
                1 => wgpu::PrimitiveTopology::LineList,
                2 => wgpu::PrimitiveTopology::LineStrip,
                3 => wgpu::PrimitiveTopology::TriangleList,
                _ => wgpu::PrimitiveTopology::TriangleStrip,
            };
            let cull_mode = match key.cull & 3 {
                1 => Some(wgpu::Face::Front),
                2 => Some(wgpu::Face::Back),
                _ => None,
            };
            let front_face = if key.cull & 4 != 0 { wgpu::FrontFace::Cw } else { wgpu::FrontFace::Ccw };
            let compare = |f: u32| match f {
                0 => wgpu::CompareFunction::Never,
                1 => wgpu::CompareFunction::Less,
                2 => wgpu::CompareFunction::Equal,
                3 => wgpu::CompareFunction::LessEqual,
                4 => wgpu::CompareFunction::Greater,
                5 => wgpu::CompareFunction::NotEqual,
                6 => wgpu::CompareFunction::GreaterEqual,
                _ => wgpu::CompareFunction::Always,
            };
            let z_enable = key.depth & 2 != 0;
            let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(&format!("vs {:016x} ps {:016x}", key.vs, key.ps)),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &vs.module,
                    entry_point: Some("main"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                // Strips restart at index 0xFFFFFFFF (16-bit 0xFFFF is widened to it).
                primitive: wgpu::PrimitiveState {
                    topology,
                    strip_index_format: topology.is_strip().then_some(wgpu::IndexFormat::Uint32),
                    cull_mode,
                    front_face,
                    ..Default::default()
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: Some(z_enable && key.depth & 4 != 0),
                    depth_compare: Some(if z_enable { compare((key.depth >> 4) & 7) } else { wgpu::CompareFunction::Always }),
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample: wgpu::MultisampleState { count: key.samples, mask: !0, alpha_to_coverage_enabled: key.alpha_to_coverage },
                fragment: Some(wgpu::FragmentState {
                    module: &ps.module,
                    entry_point: Some("main"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: key.format,
                        blend: if blendable(key.format) { blend_state(key.blend) } else { None },
                        write_mask: wgpu::ColorWrites::from_bits_truncate(key.color_mask & 0xF),
                    })],
                }),
                multiview_mask: None,
                cache,
            });
            Ok(Pipe { pipeline, group1, textures, vfetch_keys: vs.vfetch_keys.clone() })
        }
    }
}

impl Renderer {
    /// The texture uploaded as a 2D view, or as a cube view when `cube` (a
    /// cube map's six faces). A 2D view of a cube map shows its first face.
    ///
    /// One GPU texture per texture slot (guest address and layout): when the
    /// slot's contents change (a video frame), the texture is rewritten in
    /// place. A new texture per change made thousands of textures live, and
    /// wgpu's per-pass tracking costs grow with that number.
    fn texture_view(&mut self, t: &gpu::ir::Texture, cube: bool) -> Arc<wgpu::TextureView> {
        let key = t.slot ^ (cube as u64) << 63;
        let size = (t.width, t.height, t.layers);
        if let Some(u) = self.textures.get_mut(&key)
            && u.content == t.key
        {
            u.used = self.frame_number;
            return u.view.clone();
        }
        // Converted on the capture's converter threads; usually done by now.
        let Some(rgba) = t.pixels() else {
            self.report(format!("texture slot {:016x} has a format with no conversion (white)", t.slot));
            return self.constant_view([255; 4], cube);
        };
        self.made[1] += rgba.len() as u64;
        // The game's own mip levels (`ir::Texture::levels`), each written to its
        // level: a minified texture samples them as the console's GPU does,
        // instead of thinning to scattered texels of the base (hair cards vanished
        // at a distance).
        let levels = t.levels().to_vec();
        // The upload format, and its block (texels square, bytes): BC blocks are
        // uploaded as the game stored them (`texture::bc_passthrough`).
        let (format, block, block_bytes) = match t.bc {
            Some(1) => (wgpu::TextureFormat::Bc1RgbaUnorm, 4, 8),
            Some(2) => (wgpu::TextureFormat::Bc2RgbaUnorm, 4, 16),
            Some(3) => (wgpu::TextureFormat::Bc3RgbaUnorm, 4, 16),
            Some(5) => (wgpu::TextureFormat::Bc5RgUnorm, 4, 16),
            Some(other) => panic!("render: texture slot {:016x} has unknown BC kind {other}", t.slot),
            None if t.wide => (wgpu::TextureFormat::Rgba16Float, 1, 8),
            None => (wgpu::TextureFormat::Rgba8Unorm, 1, 4),
        };
        let write_levels = |queue: &wgpu::Queue, texture: &wgpu::Texture| {
            let mut offset = 0usize;
            for (level, &(w, h)) in levels.iter().enumerate() {
                let (bw, bh) = (w.div_ceil(block), h.div_ceil(block));
                let len = (bw * bh * block_bytes * t.layers) as usize;
                let Some(data) = rgba.get(offset..offset + len) else { break };
                queue.write_texture(
                    wgpu::TexelCopyTextureInfo { texture, mip_level: level as u32, origin: Default::default(), aspect: Default::default() },
                    data,
                    wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(bw * block_bytes), rows_per_image: Some(bh) },
                    // A level smaller than a block is copied at its physical (block) size.
                    wgpu::Extent3d { width: bw * block, height: bh * block, depth_or_array_layers: t.layers },
                );
                offset += len;
            }
        };
        let mips = levels.len().max(1) as u32;
        if let Some(u) = self.textures.get_mut(&key) {
            u.used = self.frame_number;
            if u.size == size && u.format == format && u.texture.mip_level_count() == mips {
                write_levels(&self.queue, &u.texture);
                u.content = t.key;
                return u.view.clone();
            }
        }
        self.made[0] += 1;
        let tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d { width: t.width, height: t.height, depth_or_array_layers: t.layers },
            mip_level_count: mips,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        write_levels(&self.queue, &tex);
        let v = Arc::new(tex.create_view(&wgpu::TextureViewDescriptor {
            dimension: Some(if cube { wgpu::TextureViewDimension::Cube } else { wgpu::TextureViewDimension::D2 }),
            array_layer_count: Some(if cube { 6 } else { 1 }),
            ..Default::default()
        }));
        // Over 1024 textures: drop the ones no frame used in the last 10
        // seconds (streamed-out textures), never the whole cache at once,
        // which made every scene's textures upload again in one frame.
        if self.textures.len() > 1024 {
            let now = self.frame_number;
            self.textures.retain(|_, u| now.saturating_sub(u.used) < 600);
        }
        self.textures.insert(key, Uploaded { texture: tex, view: v.clone(), content: t.key, size, format, used: self.frame_number });
        v
    }

    /// The sampler a texture fetch constant describes (word 0 clamp modes,
    /// word 3 filters).
    fn sampler(&mut self, w0: u32, w3: u32) -> Arc<wgpu::Sampler> {
        let key = ((w0 >> 10) & 0x1FF) | ((w3 >> 19) & 0x3F) << 9;
        if let Some(s) = self.samplers.get(&key) {
            return s.clone();
        }
        let address = |m: u32| match m {
            0 => wgpu::AddressMode::Repeat,
            1 | 3 | 5 | 7 => wgpu::AddressMode::MirrorRepeat,
            _ => wgpu::AddressMode::ClampToEdge,
        };
        let filter = |f: u32| if f == 0 { wgpu::FilterMode::Nearest } else { wgpu::FilterMode::Linear };
        // Mip filter (bits 23-24): point, linear, or 2 = the base level only.
        let mip = (w3 >> 23) & 3;
        let s = Arc::new(self.device.create_sampler(&wgpu::SamplerDescriptor {
            label: None,
            address_mode_u: address((w0 >> 10) & 7),
            address_mode_v: address((w0 >> 13) & 7),
            address_mode_w: address((w0 >> 16) & 7),
            mag_filter: filter((w3 >> 19) & 3),
            min_filter: filter((w3 >> 21) & 3),
            mipmap_filter: if mip == 1 { wgpu::MipmapFilterMode::Linear } else { wgpu::MipmapFilterMode::Nearest },
            lod_max_clamp: if mip == 2 { 0.0 } else { 32.0 },
            ..Default::default()
        }));
        self.samplers.insert(key, s.clone());
        s
    }

    /// The colour and depth surfaces the surface registers name, created on first use.
    fn surfaces_for(&mut self, regs: [u32; 3]) -> (SurfaceKey, SurfaceKey) {
        let [surface, color, depth] = regs;
        let pitch = match surface & 0x3FFF {
            0 => WIDTH,
            p => p.min(8192),
        };
        // The colour target and its depth buffer have the same samples.
        let samples = host_samples(surface, color_format((color >> 16) & 0xF));
        let ck = SurfaceKey { base: color & 0xFFF, format: (color >> 16) & 0xF, pitch, samples };
        let dk = SurfaceKey { base: depth & 0xFFF, format: 0x100, pitch, samples };
        for k in [ck, dk] {
            if !self.surfaces.contains_key(&k) {
                let format = if k.format == 0x100 { DEPTH_FORMAT } else { color_format(k.format) };
                // EDRAM is 10 MiB: no surface is taller than fits in it at 4
                // bytes a pixel. The height depends on the pitch only, so a
                // colour target and its depth buffer always match in size.
                let height = ((10 << 20) / 4 / k.pitch).clamp(HEIGHT, 4096);
                // A multisampled surface is never copied: resolves average it in a shader.
                let mut usage = wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING;
                if k.samples == 1 {
                    usage |= wgpu::TextureUsages::COPY_SRC;
                }
                // Drawn at the render scale; the guest size stays its coordinate space.
                let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some(&format!("surface {k:?}")),
                    size: wgpu::Extent3d { width: up(k.pitch), height: up(height), depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: k.samples,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage,
                    view_formats: &[],
                });
                let view = texture.create_view(&Default::default());
                let depth_view = (format == DEPTH_FORMAT).then(|| {
                    texture.create_view(&wgpu::TextureViewDescriptor { aspect: wgpu::TextureAspect::DepthOnly, ..Default::default() })
                });
                self.surfaces.insert(
                    k,
                    Surface { texture, view, depth_view, format, width: up(k.pitch), height: up(height), guest: (k.pitch, height), samples: k.samples },
                );
            }
        }
        (ck, dk)
    }

    /// Renders a frame: its draws and resolves in order, then the image to show.
    pub fn render(&mut self, frame: &Frame) {
        let t_start = std::time::Instant::now();
        // Pipelines the warm-up has built since the last frame.
        let warmed: Vec<_> = self.warm.built.lock().unwrap().try_iter().collect();
        for (k, p) in warmed {
            self.pipelines.entry(k).or_insert(Some(p));
        }
        let loaded: Vec<_> = self.warm.loaded.lock().unwrap().try_iter().collect();
        for (k, s) in loaded {
            self.shaders.entry(k).or_insert(Some(s));
        }
        enum Op<'a> {
            Pass { color: SurfaceKey, depth: SurfaceKey, draws: Vec<PreparedDraw>, first: usize, solo: bool },
            Resolve(&'a Resolve),
            Clear(&'a gpu::ir::Clear),
        }
        let mut ops: Vec<Op> = Vec::new();
        // WWE14_DRAW_LOG=N: log every command of frame N with the state the renderer
        // uses; WWE14_DRAW_LOG=>N: of the first frame with more than N draws;
        // `@N`: the first frame from N on; `@N>M`: the first from frame N on with
        // more than M draws. Several, separated by commas, each log once; the file
        // of every log after the first gets the frame number in its name.
        static LOG_FRAME: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
        static LOGGED: std::sync::Mutex<Vec<bool>> = std::sync::Mutex::new(Vec::new());
        let triggers = LOG_FRAME.get_or_init(|| std::env::var("WWE14_DRAW_LOG").map(|v| v.split(',').map(str::to_string).collect()).unwrap_or_default());
        let log = {
            let mut logged = LOGGED.lock().unwrap();
            logged.resize(triggers.len(), false);
            let draws = frame.draws().count();
            let hit = triggers.iter().enumerate().position(|(i, v)| {
                if logged[i] {
                    false
                } else if let Some(n) = v.strip_prefix('>') {
                    draws > n.parse().unwrap_or(usize::MAX)
                } else if let Some(v) = v.strip_prefix('@') {
                    let (n, m) = v.split_once('>').unwrap_or((v, "0"));
                    frame.number >= n.parse().unwrap_or(u64::MAX) && draws > m.parse().unwrap_or(0)
                } else {
                    v.parse::<u64>().ok() == Some(frame.number)
                }
            });
            match hit {
                Some(i) => {
                    logged[i] = true;
                    true
                }
                None => false,
            }
        };
        // F9 in the window (`snapshot_next_frame`): this frame's draw log and a
        // picture after each pass, in `private/f9_<frame>`.
        let snapshot = SNAPSHOT.swap(false, std::sync::atomic::Ordering::Relaxed);
        let snapshot_dir = snapshot.then(|| PathBuf::from(format!("private/f9_{}", frame.number)));
        if let Some(dir) = &snapshot_dir {
            let _ = std::fs::create_dir_all(dir);
            eprintln!("render: F9 snapshot of frame {} to {}", frame.number, dir.display());
        }
        let log = log || snapshot;
        let mut logbuf = String::new();
        if log {
            let _ = writeln!(logbuf, "drawlog: frame {} ({} draws)", frame.number, frame.draws().count());
        }
        let each_draw: Option<(usize, usize)> = log
            .then(|| std::env::var("WWE14_DUMP_EACH_DRAW").ok())
            .flatten()
            .and_then(|v| v.split_once("..").and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?))));
        self.frame_number = frame.number;
        // The pipelines this frame needs that aren't built yet: all at once, in parallel.
        let mut keys = HashSet::new();
        for c in &frame.commands {
            if let Command::Draw(d) = c {
                let regs = [0x2000, 0x2001, 0x2002].map(|r| d.regs.iter().find(|x| x.0 == r).map(|x| x.1).unwrap_or(0));
                let (color, _) = self.surfaces_for(regs);
                if let Some(k) = self.pipeline_key(d, color) {
                    keys.insert(k);
                }
            }
        }
        let t_compile = std::time::Instant::now();
        self.build_missing(&keys);
        let compile = t_compile.elapsed();
        let t_prepare = std::time::Instant::now();
        let mut draw_no = 0usize;
        for c in &frame.commands {
            match c {
                Command::Draw(d) => {
                    draw_no += 1;
                    let draw_no = draw_no - 1;
                    let regs = [0x2000, 0x2001, 0x2002].map(|r| d.regs.iter().find(|x| x.0 == r).map(|x| x.1).unwrap_or(0));
                    let (color, depth) = self.surfaces_for(regs);
                    // A predicated draw under tiling runs once per tile it selects, clipped to
                    // the tile. Without the window offset (PA_SU_SC_MODE_CNTL bit 16) its
                    // coordinates are the tile's own EDRAM surface's, so it moves to the tile.
                    let reg = |r: u32| d.regs.iter().find(|x| x.0 == r).map(|x| x.1).unwrap_or(0);
                    let offset_applies = reg(0x2205) & 0x1_0000 != 0;
                    let placements: Vec<([f32; 2], Option<[u32; 4]>)> = match &d.tiles {
                        None => vec![([0.0; 2], Some(window_scissor(&reg)))],
                        Some(tiles) => tiles
                            .iter()
                            .map(|t| {
                                let shift = if offset_applies { [0.0; 2] } else { [t.origin[0] as f32, t.origin[1] as f32] };
                                (shift, Some(t.rect))
                            })
                            .collect(),
                    };
                    let mut all = Vec::new();
                    for (shift, scissor) in placements {
                        if let Some(p) = self.prepare(d, color, shift, scissor) {
                            all.push(p);
                        }
                    }
                    let prepared = all.first().map(|p| p.viewport);
                    if log {
                        let reg = |r: u32| d.regs.iter().find(|x| x.0 == r).map(|x| x.1).unwrap_or(0);
                        let vp: Vec<String> = (0x210F..=0x2114).map(|r| format!("{:.1}", f32::from_bits(reg(r)))).collect();
                        let _ = writeln!(
                            logbuf,
                            "drawlog: draw vs {:016x} ps {:016x} prim {} n {} surf {:08X} color {:08X} depth {:08X} scissor {:08X}-{:08X} window {:?} \
                             vport [{}] vte {:08X} zctl {:08X} sc_mode {:08X} clip {:08X} mask {:08X} cc {:08X} aref {} blend {:08X} tex [{}] -> {}",
                            d.vs,
                            d.ps,
                            d.primitive,
                            d.vertex_count,
                            regs[0],
                            regs[1],
                            regs[2],
                            reg(0x200E),
                            reg(0x200F),
                            window_scissor(&reg),
                            vp.join(" "),
                            reg(0x2206),
                            reg(0x2200),
                            reg(0x2205),
                            reg(0x2208),
                            reg(0x2104),
                            reg(0x2202),
                            f32::from_bits(reg(0x210E)),
                            reg(0x2201),
                            (0..4)
                                .map(|n| {
                                    let w1 = d.fetch[6 * n + 1];
                                    let base = physical(w1 & 0xFFFF_F000);
                                    format!("{n}:{w1:08X}{}", if self.resolved.contains_key(&base) { "*" } else { "" })
                                })
                                .collect::<Vec<_>>()
                                .join(" "),
                            match (&prepared, &d.tiles) {
                                (Some(v), None) => format!("viewport {v:?}"),
                                (Some(v), Some(t)) => format!("viewport {v:?} tiles {t:?}"),
                                (None, _) => "skipped".into(),
                            }
                        );
                        // Every draw: its vertex fetches (key, descriptor words), the fetch
                        // constants they name, the captured ranges and the index span.
                        {
                            let fetches: Vec<String> = d
                                .vfetch
                                .iter()
                                .map(|(k, desc)| {
                                    let fc = desc[0] as usize;
                                    format!("{k:08X}:{desc:X?}@{:08X}/{:08X}", d.fetch[2 * fc], d.fetch[2 * fc + 1])
                                })
                                .collect();
                            let ranges: Vec<String> = d.vertex_ranges.iter().map(|(f, b)| format!("{:08X}+{:X}", f * 4, b.len())).collect();
                            let span = d.indices.as_ref().map(|i| {
                                let valid = i.iter().copied().filter(|&x| x != u32::MAX);
                                (valid.clone().min().unwrap_or(0), valid.max().unwrap_or(0))
                            });
                            // The first two vertices' words, from the first full fetch's stream.
                            let first: Vec<String> = d
                                .vfetch
                                .iter()
                                .find(|(_, desc)| (desc[1] >> 30) & 1 == 0)
                                .and_then(|(_, desc)| {
                                    let (fc, stride) = (desc[0] as usize, desc[2]);
                                    let base = d.fetch[2 * fc] >> 2;
                                    let first_index = span.map_or(0, |s| s.0) as i64 + d.base_vertex as i64;
                                    let at = base as i64 + first_index * stride as i64;
                                    let (f, bytes) = d.vertex_ranges.iter().find(|(f, b)| (*f as i64) <= at && at < *f as i64 + (b.len() / 4) as i64)?;
                                    let o = ((at - *f as i64) * 4) as usize;
                                    Some(
                                        bytes
                                            .get(o..(o + 8 * stride as usize).min(bytes.len()))?
                                            .chunks_exact(4)
                                            .map(|w| format!("{:08X}", u32::from_be_bytes([w[0], w[1], w[2], w[3]])))
                                            .collect(),
                                    )
                                })
                                .unwrap_or_default();
                            // Integer 8_8_8_8 elements (blend indices): the largest byte over the
                            // vertices the draw uses, 0xFF aside; and the strip restarts.
                            let restarts = d.indices.as_ref().map_or(0, |i| i.iter().filter(|&&x| x == u32::MAX).count());
                            let mut int_max: Vec<String> = Vec::new();
                            for (k, desc) in d.vfetch.iter() {
                                let f = desc[1];
                                if (f >> 16) & 0x3F != 6 || (f >> 13) & 1 == 0 {
                                    continue;
                                }
                                let (fc, stride, off) = (desc[0] as usize, desc[2] as i64, desc[3] as i64);
                                let base = (d.fetch[2 * fc] >> 2) as i64;
                                let mut max = [0u8; 4];
                                let verts: Vec<i64> = match &d.indices {
                                    Some(i) => i.iter().filter(|&&x| x != u32::MAX).map(|&x| x as i64 + d.base_vertex as i64).collect(),
                                    None => (0..d.vertex_count as i64).collect(),
                                };
                                for v in verts {
                                    let at = base + v * stride + off;
                                    if let Some((f0, bytes)) = d.vertex_ranges.iter().find(|(f0, b)| (*f0 as i64) <= at && at < *f0 as i64 + (b.len() / 4) as i64) {
                                        let o = ((at - *f0 as i64) * 4) as usize;
                                        for (m, &b) in max.iter_mut().zip(&bytes[o..o + 4]) {
                                            if b != 0xFF {
                                                *m = (*m).max(b);
                                            }
                                        }
                                    }
                                }
                                int_max.push(format!("{k:08X}:{max:?}"));
                            }
                            // The first 32_32_32_32_FLOAT element (positions): its range over the
                            // vertices the draw uses, and how many are NaN or beyond 1e5.
                            let mut pos_range = String::new();
                            if let Some((_, desc)) = d.vfetch.iter().find(|(_, desc)| (desc[1] >> 16) & 0x3F == 38) {
                                let (fc, stride, off) = (desc[0] as usize, desc[2] as i64, desc[3] as i64);
                                let base = (d.fetch[2 * fc] >> 2) as i64;
                                let (mut lo, mut hi, mut wild) = ([f32::MAX; 4], [f32::MIN; 4], 0usize);
                                let verts: Vec<i64> = match &d.indices {
                                    Some(i) => i.iter().filter(|&&x| x != u32::MAX).map(|&x| x as i64 + d.base_vertex as i64).collect(),
                                    None => (0..d.vertex_count as i64).collect(),
                                };
                                for v in verts {
                                    let at = base + v * stride + off;
                                    if let Some((f0, bytes)) = d.vertex_ranges.iter().find(|(f0, b)| (*f0 as i64) <= at && at + 4 <= *f0 as i64 + (b.len() / 4) as i64) {
                                        let o = ((at - *f0 as i64) * 4) as usize;
                                        for c in 0..4 {
                                            let x = f32::from_be_bytes(bytes[o + 4 * c..o + 4 * c + 4].try_into().unwrap());
                                            if !x.is_finite() || x.abs() > 1e5 {
                                                wild += 1;
                                            } else {
                                                lo[c] = lo[c].min(x);
                                                hi[c] = hi[c].max(x);
                                            }
                                        }
                                    }
                                }
                                pos_range = format!(" pos lo {lo:.1?} hi {hi:.1?} wild {wild}");
                            }
                            let _ = writeln!(
                                logbuf,
                                "drawlog:   vfetch [{}] ranges [{}] indices {:?} base vertex {} restarts {restarts} int8 max [{}]{pos_range} bools {:08X?} first vertices [{}]",
                                fetches.join(" "),
                                ranges.join(" "),
                                span,
                                d.base_vertex,
                                int_max.join(" "),
                                &d.control[..8],
                                first.join(" ")
                            );
                        }
                        // Every texture the draw samples: slot, format, per-channel sign modes
                        // (0 unsigned, 1 signed, 2 biased, 3 gamma), exponent adjust, size.
                        {
                            // Captured ones, then (`*`) the other texture slots: resolved images and stale constants.
                            let captured: Vec<u32> = d.textures.iter().map(|t| t.0).collect();
                            let others = (0..32u32).filter(|n| !captured.contains(n) && d.fetch[6 * *n as usize] & 3 == 2);
                            let t: Vec<String> = captured
                                .iter()
                                .copied()
                                .chain(others)
                                .map(|n| (n, if captured.contains(&n) { "" } else { "*" }))
                                .map(|(slot, mark)| {
                                    let slot = &slot;
                                    let w: [u32; 6] = core::array::from_fn(|k| d.fetch[6 * *slot as usize + k]);
                                    let signs: String = (0..4).map(|c| char::from(b'0' + ((w[0] >> (2 + 2 * c)) & 3) as u8)).collect();
                                    let exp = (((w[3] >> 13) & 0x3F) as i32) << 26 >> 26;
                                    format!(
                                        "{mark}{slot}:fmt{} sign{signs} exp{exp} int{} swz{:03X} {}x{} dim{} @{:08X}",
                                        w[1] & 0x3F,
                                        w[3] & 1,
                                        (w[3] >> 1) & 0xFFF,
                                        (w[2] & 0x1FFF) + 1,
                                        ((w[2] >> 13) & 0x1FFF) + 1,
                                        (w[5] >> 9) & 3,
                                        w[1] & 0xFFFF_F000
                                    )
                                })
                                .collect();
                            let _ = writeln!(logbuf, "drawlog:   textures [{}]", t.join(" "));
                            // F9: each texture the frame samples, as the guest
                            // memory holds it now, converted to a PNG once
                            // (`tex_<address>_fmt<f>_<w>x<h>.png`).
                            if let Some(dir) = &snapshot_dir {
                                for slot in &captured {
                                    let w: [u32; 6] = core::array::from_fn(|k| d.fetch[6 * *slot as usize + k]);
                                    let Some(f) = gpu::texture::Fetch::decode(w) else { continue };
                                    let name = format!("tex_{:08X}_fmt{}_{}x{}.png", w[1] & 0xFFFF_F000, f.format, f.width, f.height);
                                    let path = dir.join(&name);
                                    if path.exists() {
                                        continue;
                                    }
                                    let Some((virt, len)) = gpu::texture::guest_range(&f) else { continue };
                                    let raw = kernel::mem::read(virt, len as usize);
                                    match gpu::texture::to_rgba_from(&f, &raw) {
                                        Some(px) if px.len() >= (f.width * f.height * 4) as usize => {
                                            let _ = gpu::encode::write_png(&path, &px[..(f.width * f.height * 4) as usize], f.width as usize, f.height as usize);
                                        }
                                        _ => {
                                            let _ = std::fs::write(dir.join(name.replace(".png", ".bin")), &raw);
                                        }
                                    }
                                }
                            }
                        }
                        // WWE14_DRAW_LOG_PS_CONSTS=lo..hi: those pixel shader constants.
                        static PS_CONSTS: std::sync::OnceLock<Option<(usize, usize)>> = std::sync::OnceLock::new();
                        if let Some((lo, hi)) = *PS_CONSTS.get_or_init(|| {
                            std::env::var("WWE14_DRAW_LOG_PS_CONSTS")
                                .ok()
                                .and_then(|v| v.split_once("..").and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?))))
                        }) {
                            let c: Vec<String> = (lo..=hi.min(255))
                                .map(|i| {
                                    let v = d.ps_consts.get(i);
                                    format!("c{i}=({:.3} {:.3} {:.3} {:.3})", v[0], v[1], v[2], v[3])
                                })
                                .collect();
                            let _ = writeln!(logbuf, "drawlog:   ps consts {}", c.join(" "));
                        }
                        // WWE14_DRAW_LOG_CONSTS=lo..hi: those vertex shader constants.
                        static CONSTS: std::sync::OnceLock<Option<(usize, usize)>> = std::sync::OnceLock::new();
                        if let Some((lo, hi)) = *CONSTS.get_or_init(|| {
                            std::env::var("WWE14_DRAW_LOG_CONSTS")
                                .ok()
                                .and_then(|v| v.split_once("..").and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?))))
                        }) {
                            let c: Vec<String> = (lo..=hi.min(255))
                                .map(|i| {
                                    let v = d.vs_consts.get(i);
                                    format!("c{i}=({:.3} {:.3} {:.3} {:.3})", v[0], v[1], v[2], v[3])
                                })
                                .collect();
                            let _ = writeln!(logbuf, "drawlog:   consts {}", c.join(" "));
                        }
                        // Small draws (quads): their vertex data as floats, from each stream.
                        if d.vertex_count <= 8 {
                            for n in 0..2 {
                                let _ = writeln!(logbuf, "drawlog:   texture {n}: {:08X?}", &d.fetch[6 * n..6 * n + 6]);
                            }
                            for (k, desc) in d.vfetch.iter().take(3) {
                                let [fc, _, stride, offset] = *desc;
                                let base = d.fetch[2 * fc as usize] >> 2;
                                let Some((first, bytes)) = d.vertex_ranges.iter().find(|(f, b)| *f <= base && base < f + (b.len() / 4) as u32) else { continue };
                                let at = ((base - first + offset) * 4) as usize;
                                let floats: Vec<String> = (0..d.vertex_count as usize)
                                    .flat_map(|v| (0..4).map(move |i| at + v * stride as usize * 4 + i * 4))
                                    .filter_map(|o| bytes.get(o..o + 4))
                                    .map(|w| format!("{:.1}", f32::from_be_bytes([w[0], w[1], w[2], w[3]])))
                                    .collect();
                                let _ = writeln!(logbuf, "drawlog:   fetch key {k:08X} stride {stride} offset {offset}: {}", floats.join(" "));
                            }
                        }
                    }
                    if all.is_empty() && d.tiles.as_ref().is_none_or(|t| !t.is_empty()) {
                        self.draws_skipped += 1;
                    }
                    for p in all {
                        // WWE14_DUMP_EACH_DRAW=lo..hi: those draws of the logged frame get a
                        // pass each, so the dump shows the target after every one of them.
                        let solo = each_draw.is_some_and(|(lo, hi)| (lo..=hi).contains(&draw_no));
                        if let Some(Op::Pass { color: c, depth: z, draws, solo: false, .. }) = ops.last_mut()
                            && *c == color
                            && *z == depth
                            && !solo
                        {
                            draws.push(p);
                        } else {
                            ops.push(Op::Pass { color, depth, draws: vec![p], first: draw_no, solo });
                        }
                    }
                }
                Command::Clear(c) => {
                    if log {
                        let _ = writeln!(logbuf, "drawlog: clear flags {:X} rects {:?} surface {:08X?} color {:?} z {}", c.flags, c.rects, c.surface, c.color, c.z);
                    }
                    self.surfaces_for(c.surface);
                    ops.push(Op::Clear(c));
                }
                Command::Resolve(r) => {
                    if log {
                        let _ = writeln!(
                            logbuf,
                            "drawlog: resolve flags {:X} rect {:?} dest {:08X?} point {:?} surface {:08X?}",
                            r.flags, r.rect, r.dest, r.dest_point, r.surface
                        );
                    }
                    self.resolve_dest(r);
                    ops.push(Op::Resolve(r));
                }
            }
        }
        if log {
            // Written in one go, so a crash of the game can't cut it short.
            let path = match &snapshot_dir {
                Some(dir) => dir.join("drawlog.txt").display().to_string(),
                None => {
                    let path = std::env::var("WWE14_DRAW_LOG_FILE").unwrap_or_else(|_| "drawlog.txt".into());
                    if std::path::Path::new(&path).exists() { format!("{path}.{}", frame.number) } else { path }
                }
            };
            let _ = std::fs::write(&path, &logbuf);
        }
        let prepare = t_prepare.elapsed();
        // The frame's draw data into the buffers kept between frames; the draws
        // share one group 0 bind group, bound once a pass.
        let mut arena = std::mem::take(&mut self.arena);
        let max_storage = self.device.limits().max_storage_buffer_binding_size;
        for (what, bytes) in [("vertex memory", arena.vertex.len()), ("draw rows", arena.rows.len() * 16)] {
            if bytes as u64 > max_storage {
                self.report(format!("frame {}: {bytes} bytes of {what}, more than one storage binding holds ({max_storage})", frame.number));
            }
        }
        self.frame_buffers.upload(&self.device, &self.queue, &self.group0, &arena);
        arena.clear();
        self.arena = arena;
        let group0 = self.frame_buffers.group0.clone();
        let index_buffer = self.frame_buffers.indices.buffer.clone();
        let mut enc = self.device.create_command_encoder(&Default::default());
        // WWE14_DUMP_OPS_DIR: with the draw log's frame, a PNG of the colour
        // target after every pass of 20 draws or more (or of one draw, or the last), and of each resolve's
        // destination (debugging).
        let dump_ops = if log { snapshot_dir.clone().or_else(|| std::env::var("WWE14_DUMP_OPS_DIR").ok().map(PathBuf::from)) } else { None };
        // One step past the end, so the last operation is dumped too.
        for i in 0..=ops.len() {
            if let Some(dir) = &dump_ops {
                // Everything so far must run before reading back.
                self.queue.submit([std::mem::replace(&mut enc, self.device.create_command_encoder(&Default::default())).finish()]);
                if i > 0 {
                    let (name, img) = match &ops[i - 1] {
                        Op::Pass { color, draws, first, .. } if draws.len() >= 20 || i == ops.len() || draws.len() == 1 => {
                            let s = &self.surfaces[color];
                            (format!("op{:04}_pass_d{first}_{}draws_{:03X}_{}", i - 1, draws.len(), color.base, color.pitch), self.read_surface(s, s.height.min(720)).map(|p| (p, s.width, s.height.min(720))))
                        }
                        Op::Resolve(r) => match r.dest.and_then(gpu::texture::Fetch::decode).and_then(|f| self.resolved.get(&physical(f.base)).map(|t| (f.base, t))) {
                            Some((base, t)) => (format!("op{:04}_resolve_{base:08X}_flags{:X}", i - 1, r.flags), self.read_rgba(&t.texture, t.format, t.width, t.height).map(|p| (p, t.width, t.height))),
                            None => (String::new(), None),
                        },
                        _ => (String::new(), None),
                    };
                    if let Some((px, w, h)) = img {
                        let _ = gpu::encode::write_png(&dir.join(format!("{name}.png")), &px, w as usize, h as usize);
                    }
                }
            }
            let Some(op) = ops.get(i) else { break };
            match op {
                Op::Pass { color, depth, draws, .. } => {
                    let (cs, ds) = (&self.surfaces[color], &self.surfaces[depth]);
                    let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("draws"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &cs.view,
                            depth_slice: None,
                            resolve_target: None,
                            ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                        })],
                        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                            view: &ds.view,
                            depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store }),
                            stencil_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store }),
                        }),
                        timestamp_writes: None,
                        occlusion_query_set: None,
                        multiview_mask: None,
                    });
                    // State is set only when it changes: wgpu validates and replays
                    // every command when the encoder finishes, and consecutive draws
                    // mostly share their pipeline, textures, viewport and scissor.
                    // The frame's index buffer is bound once; draws start at their
                    // list's first index.
                    let mut last_pipe: Option<*const Pipe> = None;
                    let mut last_group1: Option<*const wgpu::BindGroup> = None;
                    pass.set_bind_group(0, &group0, &[]);
                    let mut last_viewport: Option<[f32; 6]> = None;
                    let mut last_scissor: Option<[u32; 4]> = None;
                    let mut indexed = false;
                    for p in draws {
                        let [sx, sy, sw, sh] = match p.scissor.map(|r| r.map(up)) {
                            Some([x1, y1, x2, y2]) => {
                                let (x1, y1) = (x1.min(cs.width), y1.min(cs.height));
                                [x1, y1, x2.min(cs.width).saturating_sub(x1), y2.min(cs.height).saturating_sub(y1)]
                            }
                            None => [0, 0, cs.width, cs.height],
                        };
                        if sw == 0 || sh == 0 {
                            continue;
                        }
                        if last_viewport != Some(p.viewport) {
                            let [x, y, w, h, z0, z1] = p.viewport;
                            pass.set_viewport(x, y, w, h, z0, z1);
                            last_viewport = Some(p.viewport);
                        }
                        if last_scissor != Some([sx, sy, sw, sh]) {
                            pass.set_scissor_rect(sx, sy, sw, sh);
                            last_scissor = Some([sx, sy, sw, sh]);
                        }
                        if last_pipe != Some(Arc::as_ptr(&p.pipe)) {
                            pass.set_pipeline(&p.pipe.pipeline);
                            last_pipe = Some(Arc::as_ptr(&p.pipe));
                        }
                        if last_group1 != Some(Arc::as_ptr(&p.group1)) {
                            pass.set_bind_group(1, &*p.group1, &[]);
                            last_group1 = Some(Arc::as_ptr(&p.group1));
                        }
                        match p.index {
                            Some((off, n)) => {
                                if !indexed {
                                    pass.set_index_buffer(index_buffer.slice(..), wgpu::IndexFormat::Uint32);
                                    indexed = true;
                                }
                                let first = off / 4;
                                pass.draw_indexed(first..first + n, p.base_vertex, p.draw..p.draw + 1);
                            }
                            None => pass.draw(0..p.count, p.draw..p.draw + 1),
                        }
                    }
                    self.draws_drawn += draws.len() as u64;
                    self.last_target = Some(*color);
                }
                Op::Resolve(r) => self.encode_resolve(&mut enc, r),
                Op::Clear(c) => self.encode_clear(&mut enc, c),
            }
        }
        // A held frame (`gpu::ir::Frame::hold`) is rendered but not shown.
        if !frame.hold {
            self.present(&mut enc, frame.front);
        }
        self.queue.submit([enc.finish()]);
        if let Some(rec) = &mut self.recorder {
            let front = frame.front.and_then(gpu::texture::Fetch::decode).map(|f| physical(f.base));
            let shown = match front {
                Some(b) if self.resolved.contains_key(&b) => format!("front {b:08X}"),
                Some(b) => format!("front {b:08X} not resolved: last target {:?}", self.last_target.map(|k| k.base)),
                None => format!("no front buffer: last target {:?}", self.last_target.map(|k| k.base)),
            };
            rec.capture(&self.device, &self.queue, &self.target, frame.number, format!("{} draws, {shown}", frame.draws().count()));
        }
        self.frames += 1;
        self.perf.frame(t_start.elapsed(), compile, prepare);
        // Each frame over 25 ms (a hitch), with what it had to create, the first 300.
        let made = std::mem::take(&mut self.made);
        if t_start.elapsed().as_millis() > 25 && self.slow_logged < 300 {
            self.slow_logged += 1;
            eprintln!(
                "render: slow frame {}: {} ms (pipelines {} ms, prepare {} ms; {} draws; {} new textures, {} KB of texels uploaded, {} new bind groups)",
                frame.number,
                t_start.elapsed().as_millis(),
                compile.as_millis(),
                prepare.as_millis(),
                frame.draws().count(),
                made[0],
                made[1] / 1024,
                made[2]
            );
        }
        // New pipelines go to the disk cache now and then (runs end by being killed).
        if self.pipelines_built >= self.pipelines_saved + 64 || (self.frames % 600 == 0 && self.pipelines_built > self.pipelines_saved) {
            self.save_pipeline_cache();
        }
    }

    /// Writes the driver's compiled pipelines to the cache file.
    pub fn save_pipeline_cache(&mut self) {
        self.pipelines_saved = self.pipelines_built;
        let (Some(cache), Some(path)) = (&self.pipeline_cache, &self.pipeline_cache_path) else { return };
        let Some(data) = cache.get_data() else { return };
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, &data).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }

    /// Makes sure a resolve's destination texture exists (and has the format,
    /// size and layers this resolve writes). Called while the frame's commands
    /// are walked, before draws after the resolve bind their textures: a
    /// texture first made by a resolve is sampled in the same frame.
    fn resolve_dest(&mut self, r: &Resolve) {
        use gpu::ir::RESOLVE_DEPTHSTENCIL;
        let (ck, dk) = self.surfaces_for(r.surface);
        if let Some(f) = r.dest.and_then(gpu::texture::Fetch::decode) {
            let depth = r.flags & RESOLVE_DEPTHSTENCIL != 0;
            let src_format = self.surfaces[if depth { &dk } else { &ck }].format;
            // A depth buffer resolves to a float texture holding its depth values.
            let format = if depth { self.depth_copy.format } else { src_format };
            // Keyed by physical address: the destination may be named by any view.
            let base = physical(f.base);
            let layers = gpu::texture::layers(&f);
            let mips = resolve_mips(r.dest.unwrap_or_default(), f.width, f.height);
            let fresh = self
                .resolved
                .get(&base)
                .is_none_or(|t| t.format != format || t.guest != (f.width, f.height) || t.layers != layers || t.mips != mips);
            if fresh {
                // At the render scale, as the targets it copies from.
                let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some(&format!("resolved {base:08X}")),
                    size: wgpu::Extent3d { width: up(f.width), height: up(f.height), depth_or_array_layers: layers },
                    mip_level_count: mips,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING
                        | wgpu::TextureUsages::COPY_DST
                        | wgpu::TextureUsages::COPY_SRC
                        | wgpu::TextureUsages::RENDER_ATTACHMENT,
                    view_formats: &[],
                });
                // Sampled as 2D: the first layer. Sampled as a cube: all six faces.
                let view = Arc::new(texture.create_view(&wgpu::TextureViewDescriptor {
                    dimension: Some(wgpu::TextureViewDimension::D2),
                    array_layer_count: Some(1),
                    ..Default::default()
                }));
                let cube = (layers == 6 && f.width == f.height).then(|| {
                    Arc::new(texture.create_view(&wgpu::TextureViewDescriptor {
                        dimension: Some(wgpu::TextureViewDimension::Cube),
                        array_layer_count: Some(6),
                        ..Default::default()
                    }))
                });
                self.resolved.insert(base, Resolved { layers, mips, cube, texture, view, format, width: up(f.width), height: up(f.height), guest: (f.width, f.height) });
            }
        }
    }

    /// A resolve: copy the source rectangle of the target into the
    /// destination texture (kept under its guest address, made by
    /// `resolve_dest`), then clear.
    fn encode_resolve(&mut self, enc: &mut wgpu::CommandEncoder, r: &Resolve) {
        use gpu::ir::{RESOLVE_CLEAR_DEPTH, RESOLVE_CLEAR_TARGET, RESOLVE_DEPTHSTENCIL};
        let (ck, dk) = self.surfaces_for(r.surface);
        if let Some(f) = r.dest.and_then(gpu::texture::Fetch::decode) {
            let depth = r.flags & RESOLVE_DEPTHSTENCIL != 0;
            let src = &self.surfaces[if depth { &dk } else { &ck }];
            let (sw, sh) = (src.width, src.height);
            let base = physical(f.base);
            // A cube map is resolved one face at a time: the resolve's slice is the face.
            let layers = gpu::texture::layers(&f);
            let layer = r.slice.min(layers - 1);
            // Usually already made while walking the commands; again here in case a
            // later resolve to the same address in this frame changed its format.
            self.resolve_dest(r);
            let dst = &self.resolved[&base];
            // A mipmapped destination is resolved one level at a time, each from the
            // top left of the source at that level's size.
            let level = r.level.min(dst.mips - 1);
            // Host pixels: the level's size, and the guest's rectangle and point scaled.
            let (lw, lh) = ((dst.width >> level).max(1), (dst.height >> level).max(1));
            let [x1, y1, x2, y2] = r.rect.map(|r| r.map(up)).unwrap_or([0, 0, lw, lh]);
            let [dx, dy] = r.dest_point.map(up);
            // The face and level being written, as a render attachment (depth resolves).
            let layer_view = dst.texture.create_view(&wgpu::TextureViewDescriptor {
                dimension: Some(wgpu::TextureViewDimension::D2),
                base_array_layer: layer,
                array_layer_count: Some(1),
                base_mip_level: level,
                mip_level_count: Some(1),
                ..Default::default()
            });
            let w = x2.saturating_sub(x1).min(sw.saturating_sub(x1)).min(lw.saturating_sub(dx));
            let h = y2.saturating_sub(y1).min(sh.saturating_sub(y1)).min(lh.saturating_sub(dy));
            if w > 0 && h > 0 {
                if depth {
                    let offset = [x1 as i32 - dx as i32, y1 as i32 - dy as i32, 0, 0];
                    let params = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: None,
                        contents: &offset.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>(),
                        usage: wgpu::BufferUsages::UNIFORM,
                    });
                    let ms = self.surfaces[&dk].samples > 1;
                    let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: None,
                        layout: if ms { &self.depth_copy.ms_layout } else { &self.depth_copy.layout },
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: wgpu::BindingResource::TextureView(self.surfaces[&dk].depth_view.as_ref().unwrap()),
                            },
                            wgpu::BindGroupEntry { binding: 1, resource: params.as_entire_binding() },
                        ],
                    });
                    let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("depth resolve"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &layer_view,
                            depth_slice: None,
                            resolve_target: None,
                            ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                        })],
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                        multiview_mask: None,
                    });
                    pass.set_pipeline(if ms { &self.depth_copy.ms_pipeline } else { &self.depth_copy.pipeline });
                    pass.set_bind_group(0, &group, &[]);
                    pass.set_scissor_rect(dx, dy, w, h);
                    pass.draw(0..3, 0..1);
                } else if self.surfaces[&ck].samples > 1 {
                    // Multisampled: the average of each pixel's samples.
                    let offset = [x1 as i32 - dx as i32, y1 as i32 - dy as i32];
                    let src = &self.surfaces[&ck].view;
                    if !self.ms_resolve.encode(&self.device, enc, src, dst.format, &layer_view, [dx, dy, w, h], offset) {
                        let msg = format!("no multisampled resolve into {:?}", dst.format);
                        if self.reported.insert(msg.clone()) {
                            eprintln!("render: {msg}");
                        }
                    }
                } else {
                    enc.copy_texture_to_texture(
                        wgpu::TexelCopyTextureInfo {
                            texture: &self.surfaces[&ck].texture,
                            mip_level: 0,
                            origin: wgpu::Origin3d { x: x1, y: y1, z: 0 },
                            aspect: wgpu::TextureAspect::All,
                        },
                        wgpu::TexelCopyTextureInfo {
                            texture: &dst.texture,
                            mip_level: level,
                            origin: wgpu::Origin3d { x: dx, y: dy, z: layer },
                            aspect: wgpu::TextureAspect::All,
                        },
                        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                    );
                }
            }
        }
        // The clear that follows a resolve covers the resolved rectangle.
        let color = (r.flags & RESOLVE_CLEAR_TARGET != 0).then_some(r.clear_color);
        let depth = (r.flags & RESOLVE_CLEAR_DEPTH != 0).then_some((r.clear_z, r.clear_stencil));
        let rects: Vec<[u32; 4]> = r.rect.into_iter().collect();
        self.clear(enc, ck, dk, &rects, color, depth.map(|d| d.0), depth.map(|d| d.1));
    }

    /// A clear (`D3DDevice_Clear`) of the bound targets, whole or by rectangles.
    fn encode_clear(&mut self, enc: &mut wgpu::CommandEncoder, c: &gpu::ir::Clear) {
        let (ck, dk) = self.surfaces_for(c.surface);
        // D3D 360 clear flags: bits 0-3 colour targets 0-3, 0x10 depth, 0x20 stencil
        // (not PC D3D9's 1/2/4). Targets 1-3 are not bound here.
        let color = (c.flags & 0x1 != 0).then_some(c.color);
        let depth = (c.flags & 0x10 != 0).then_some(c.z);
        let stencil = (c.flags & 0x20 != 0).then_some(c.stencil);
        self.clear(enc, ck, dk, &c.rects, color, depth, stencil);
    }

    /// Clears the bound targets: whole surfaces with load-op clears, or only
    /// `rects` (x1, y1, x2, y2) with the rectangle-clear shader. A rectangle
    /// that covers the surface is a whole clear.
    #[allow(clippy::too_many_arguments)]
    fn clear(
        &mut self,
        enc: &mut wgpu::CommandEncoder,
        ck: SurfaceKey,
        dk: SurfaceKey,
        rects: &[[u32; 4]],
        color: Option<[f32; 4]>,
        depth: Option<f32>,
        stencil: Option<u32>,
    ) {
        if color.is_none() && depth.is_none() && stencil.is_none() {
            return;
        }
        // Guest rectangles, in host pixels.
        let (w, h) = (self.surfaces[&ck].width, self.surfaces[&ck].height);
        let rects: Vec<[u32; 4]> = rects
            .iter()
            .map(|r| r.map(up))
            .map(|[x1, y1, x2, y2]| [x1.min(w), y1.min(h), x2.min(w), y2.min(h)])
            .filter(|[x1, y1, x2, y2]| x2 > x1 && y2 > y1)
            .collect();
        if rects.is_empty() || rects.iter().any(|&[x1, y1, x2, y2]| x1 == 0 && y1 == 0 && x2 >= w && y2 >= h) {
            return self.clear_surfaces(enc, ck, dk, color, depth, stencil);
        }
        let (format, samples) = (self.surfaces[&ck].format, self.surfaces[&ck].samples);
        let key = ClearKey { format, color: color.is_some(), depth: depth.is_some(), stencil: stencil.is_some(), samples };
        let pipeline = self.clear_pipeline(key);
        let params: Vec<u8> = color
            .unwrap_or_default()
            .into_iter()
            .chain([depth.unwrap_or(0.0).clamp(0.0, 1.0), 0.0, 0.0, 0.0])
            .flat_map(|x| x.to_le_bytes())
            .collect();
        let buffer = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: None,
            contents: &params,
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.clear_layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: buffer.as_entire_binding() }],
        });
        let (cs, ds) = (&self.surfaces[&ck], &self.surfaces[&dk]);
        let load = wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store };
        let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("rectangle clear"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &cs.view,
                depth_slice: None,
                resolve_target: None,
                ops: load,
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &ds.view,
                depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store }),
                stencil_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store }),
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &group, &[]);
        pass.set_stencil_reference(stencil.unwrap_or(0) & 0xFF);
        for [x1, y1, x2, y2] in rects {
            pass.set_scissor_rect(x1, y1, x2 - x1, y2 - y1);
            pass.draw(0..3, 0..1);
        }
    }

    fn clear_pipeline(&mut self, key: ClearKey) -> Arc<wgpu::RenderPipeline> {
        if let Some(p) = self.clear_pipelines.get(&key) {
            return p.clone();
        }
        let layout = self.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&self.clear_layout)],
            immediate_size: 0,
        });
        let stencil_face = wgpu::StencilFaceState {
            compare: wgpu::CompareFunction::Always,
            fail_op: wgpu::StencilOperation::Keep,
            depth_fail_op: wgpu::StencilOperation::Keep,
            pass_op: wgpu::StencilOperation::Replace,
        };
        let p = Arc::new(self.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("rectangle clear"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &self.clear_module,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: Default::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(key.depth),
                depth_compare: Some(wgpu::CompareFunction::Always),
                stencil: wgpu::StencilState {
                    front: stencil_face,
                    back: stencil_face,
                    read_mask: 0xFF,
                    write_mask: if key.stencil { 0xFF } else { 0 },
                },
                bias: Default::default(),
            }),
            multisample: wgpu::MultisampleState { count: key.samples, ..Default::default() },
            fragment: Some(wgpu::FragmentState {
                module: &self.clear_module,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: key.format,
                    blend: None,
                    write_mask: if key.color { wgpu::ColorWrites::ALL } else { wgpu::ColorWrites::empty() },
                })],
            }),
            multiview_mask: None,
            cache: None,
        }));
        self.clear_pipelines.insert(key, p.clone());
        p
    }

    fn clear_surfaces(
        &self,
        enc: &mut wgpu::CommandEncoder,
        ck: SurfaceKey,
        dk: SurfaceKey,
        color: Option<[f32; 4]>,
        depth: Option<f32>,
        stencil: Option<u32>,
    ) {
        if color.is_none() && depth.is_none() && stencil.is_none() {
            return;
        }
        let (cs, ds) = (&self.surfaces[&ck], &self.surfaces[&dk]);
        let _ = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("clear"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &cs.view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: match color {
                        Some([r, g, b, a]) => wgpu::LoadOp::Clear(wgpu::Color { r: r as f64, g: g as f64, b: b as f64, a: a as f64 }),
                        None => wgpu::LoadOp::Load,
                    },
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &ds.view,
                depth_ops: Some(wgpu::Operations {
                    load: depth.map_or(wgpu::LoadOp::Load, |z| wgpu::LoadOp::Clear(z.clamp(0.0, 1.0))),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: Some(wgpu::Operations {
                    load: stencil.map_or(wgpu::LoadOp::Load, |s| wgpu::LoadOp::Clear(s & 0xFF)),
                    store: wgpu::StoreOp::Store,
                }),
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
    }

    /// A single-sampled copy of the top `rows` of a multisampled surface (each
    /// pixel the average of its samples), for showing or reading it back.
    fn resolve_whole(&self, enc: &mut wgpu::CommandEncoder, s: &Surface, rows: u32) -> Option<wgpu::Texture> {
        let t = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("msaa surface, resolved"),
            size: wgpu::Extent3d { width: s.width, height: rows, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: s.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let v = t.create_view(&Default::default());
        self.ms_resolve.encode(&self.device, enc, &s.view, s.format, &v, [0, 0, s.width, rows], [0, 0]).then_some(t)
    }

    /// Reads back the top `rows` of a surface as RGBA8, resolving it first when multisampled.
    fn read_surface(&self, s: &Surface, rows: u32) -> Option<Vec<u8>> {
        if s.samples == 1 {
            return self.read_rgba(&s.texture, s.format, s.width, rows);
        }
        let mut enc = self.device.create_command_encoder(&Default::default());
        let t = self.resolve_whole(&mut enc, s, rows)?;
        self.queue.submit([enc.finish()]);
        self.read_rgba(&t, s.format, s.width, rows)
    }

    /// Draws the frame's image into the display target: the texture resolved
    /// into the presented front buffer, or else the last target drawn.
    fn present(&mut self, enc: &mut wgpu::CommandEncoder, front: Option<[u32; 6]>) {
        let front = front.and_then(gpu::texture::Fetch::decode).map(|f| physical(f.base));
        let fallback;
        let (view, format, w, h): (&wgpu::TextureView, wgpu::TextureFormat, u32, u32) = match front.and_then(|b| self.resolved.get(&b)) {
            Some(t) => (&t.view, t.format, t.width, t.height),
            None => match self.last_target.and_then(|k| self.surfaces.get(&k)) {
                Some(s) if s.samples > 1 => {
                    let Some(t) = self.resolve_whole(enc, s, s.height) else { return };
                    fallback = t.create_view(&Default::default());
                    (&fallback, s.format, s.width, s.height)
                }
                Some(s) => (&s.view, s.format, s.width, s.height),
                None => return,
            },
        };
        // 32-bit float images can't be sampled with filtering; they are never shown.
        if !blendable(format) {
            return;
        }
        let (ow, oh) = output_size();
        let scale = [(ow as f32 / w as f32).min(1.0), (oh as f32 / h as f32).min(1.0), 0.0, 0.0];
        let params = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: None,
            contents: &scale.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>(),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.blit.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.blit.sampler) },
                wgpu::BindGroupEntry { binding: 2, resource: params.as_entire_binding() },
            ],
        });
        let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("present"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &self.target_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.blit.pipeline);
        pass.set_bind_group(0, &group, &[]);
        pass.draw(0..3, 0..1);
    }

    /// `shift` moves the draw in window coordinates; `scissor` clips it.
    /// `WWE14_DEBUG_SLOTS`: the constant colour fetch constant `n` reads in the
    /// frame being rendered, if any.
    fn debug_slot(&self, n: u32, ps: u64) -> Option<[u8; 4]> {
        // (first frame, last frame, slot, colour, pixel shader id prefix)
        type Spec = Vec<(u64, u64, u32, [u8; 4], String)>;
        static SPEC: std::sync::OnceLock<Spec> = std::sync::OnceLock::new();
        let spec = SPEC.get_or_init(|| {
            let parse = || -> Option<Spec> {
                let v = std::env::var("WWE14_DEBUG_SLOTS").ok()?;
                v.split(';')
                    .filter(|s| !s.is_empty())
                    .map(|e| {
                        let (frames, rest) = e.split_once(':')?;
                        let (a, b) = frames.split_once('-')?;
                        let (slot, rest) = rest.split_once('=')?;
                        let (rgba, only) = rest.split_once('@').unwrap_or((rest, ""));
                        let c: Vec<u8> = rgba.split(',').filter_map(|x| x.parse().ok()).collect();
                        Some((a.parse().ok()?, b.parse().ok()?, slot.parse().ok()?, [*c.first()?, *c.get(1)?, *c.get(2)?, *c.get(3)?], only.to_string()))
                    })
                    .collect()
            };
            parse().unwrap_or_default()
        });
        // Asked for every texture of every draw: nothing to format when unset.
        if spec.is_empty() {
            return None;
        }
        let id = format!("{ps:016x}");
        spec.iter().find(|s| (s.0..=s.1).contains(&self.frame_number) && s.2 == n && id.starts_with(&s.4)).map(|s| s.3)
    }

    /// `WWE14_DEBUG_CONSTS`: the pixel shader constants replaced in the frame
    /// being rendered for pixel shader `ps`, if any.
    fn debug_consts(&self, ps: u64) -> Option<Vec<(usize, [f32; 4])>> {
        type Spec = Vec<(u64, u64, usize, [f32; 4], String)>;
        static SPEC: std::sync::OnceLock<Spec> = std::sync::OnceLock::new();
        let spec = SPEC.get_or_init(|| {
            let parse = || -> Option<Spec> {
                let v = std::env::var("WWE14_DEBUG_CONSTS").ok()?;
                v.split(';')
                    .filter(|s| !s.is_empty())
                    .map(|e| {
                        let (frames, rest) = e.split_once(':')?;
                        let (a, b) = frames.split_once('-')?;
                        let (n, rest) = rest.split_once('=')?;
                        let (xyzw, only) = rest.split_once('@').unwrap_or((rest, ""));
                        let c: Vec<f32> = xyzw.split(',').filter_map(|x| x.parse().ok()).collect();
                        Some((a.parse().ok()?, b.parse().ok()?, n.parse().ok()?, [*c.first()?, *c.get(1)?, *c.get(2)?, *c.get(3)?], only.to_string()))
                    })
                    .collect()
            };
            parse().unwrap_or_default()
        });
        // Asked for every draw: nothing to format when unset.
        if spec.is_empty() {
            return None;
        }
        let id = format!("{ps:016x}");
        let hits: Vec<(usize, [f32; 4])> =
            spec.iter().filter(|s| (s.0..=s.1).contains(&self.frame_number) && id.starts_with(&s.4) && s.2 < 256).map(|s| (s.2, s.3)).collect();
        (!hits.is_empty()).then_some(hits)
    }

    /// A 1×1 texture (or cube) of one colour, cached.
    fn constant_view(&mut self, rgba: [u8; 4], cube: bool) -> Arc<wgpu::TextureView> {
        let key = u32::from_le_bytes(rgba) as u64 | (cube as u64) << 32 | 0xDEB0 << 40;
        if let Some(u) = self.textures.get_mut(&key) {
            u.used = self.frame_number;
            return u.view.clone();
        }
        let layers = if cube { 6 } else { 1 };
        let data: Vec<u8> = (0..layers).flat_map(|_| rgba).collect();
        let t = self.device.create_texture_with_data(
            &self.queue,
            &wgpu::TextureDescriptor {
                label: Some("debug constant"),
                size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: layers },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            &data,
        );
        let v = Arc::new(t.create_view(&wgpu::TextureViewDescriptor {
            dimension: Some(if cube { wgpu::TextureViewDimension::Cube } else { wgpu::TextureViewDimension::D2 }),
            ..Default::default()
        }));
        self.textures.insert(key, Uploaded { texture: t, view: v.clone(), content: 0, size: (1, 1, layers), format: wgpu::TextureFormat::Rgba8Unorm, used: self.frame_number });
        v
    }

    /// The pipeline a draw into `color` needs: its shaders and render state.
    fn pipeline_key(&self, d: &Draw, color: SurfaceKey) -> Option<PipelineKey> {
        let reg = |r: u32| d.regs.iter().find(|x| x.0 == r).map(|x| x.1).unwrap_or(0);
        // Fans and quads become triangle lists (`prepare` makes their indices).
        let topology = match d.primitive {
            1 => 0,
            2 => 1,
            3 => 2,
            4 | 5 | 13 => 3,
            6 => 4,
            _ => return None,
        };
        // Blending is off when it would be src*1 + dst*0 for colour and alpha.
        let blend = match reg(0x2201) {
            0x0001_0001 => 0,
            b => b,
        };
        let surface = self.surfaces.get(&color)?;
        Some(PipelineKey {
            vs: d.vs,
            ps: d.ps,
            topology,
            blend,
            color_mask: reg(0x2104) & 0xF,
            format: surface.format,
            depth: reg(0x2200) & 0x76,
            cull: reg(0x2205) & 7,
            samples: surface.samples,
            alpha_to_coverage: surface.samples > 1 && reg(0x2202) & 0x10 != 0,
        })
    }

    fn prepare(&mut self, d: &Draw, color: SurfaceKey, shift: [f32; 2], scissor: Option<[u32; 4]>) -> Option<PreparedDraw> {
        let reg = |r: u32| d.regs.iter().find(|x| x.0 == r).map(|x| x.1).unwrap_or(0);
        // The viewport covers the host texture; positions map through the guest size.
        let (tw, th, gw, gh) = {
            let s = &self.surfaces[&color];
            (s.width, s.height, s.guest.0, s.guest.1)
        };
        // Primitive: topology, and indices for the types wgpu lacks. Indexed
        // draws map their own indices the same way.
        let n = d.vertex_count;
        let idx = |i: u32| d.indices.as_ref().map_or(i, |v| v.get(i as usize).copied().unwrap_or(0));
        let own = |list: Vec<u32>| Some(std::borrow::Cow::Owned(list.into_iter().map(idx).collect::<Vec<u32>>()));
        // The draw's own index list, borrowed (no copy before the arena's).
        let given = d.indices.as_ref().map(|v| std::borrow::Cow::Borrowed(&v[..]));
        let (topology, indices): (u8, Option<std::borrow::Cow<[u32]>>) = match d.primitive {
            1 => (0, given),
            2 => (1, given),
            3 => (2, given),
            4 => (3, given),
            5 => (3, own((1..n.saturating_sub(1)).flat_map(|i| [0, i, i + 1]).collect())),
            6 => (4, given),
            13 => (3, own((0..n / 4).flat_map(|q| [4 * q, 4 * q + 1, 4 * q + 2, 4 * q, 4 * q + 2, 4 * q + 3]).collect())),
            p => {
                self.report(format!("primitive type {p} is not drawn yet"));
                return None;
            }
        };
        let key = self.pipeline_key(d, color)?;
        debug_assert_eq!(key.topology, topology);
        let pipe = self.pipeline(key)?;

        // Viewport: D3D's transform registers, or screen space when it's off.
        let vte = reg(0x2206);
        let f = |r: u32| f32::from_bits(reg(r));
        static RENDER_DEBUG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *RENDER_DEBUG.get_or_init(|| std::env::var_os("WWE14_RENDER_DEBUG").is_some()) {
            let regs: Vec<String> = (0x210Du32..=0x2114).map(|r| format!("{r:04X}={}", f(r))).collect();
            self.report(format!(
                "vs {:016x} ps {:016x} prim {} vte {vte:08X} su_sc {:08X} {} blend {:08X} depth {:08X} surface {:08X} {:08X} {:08X}",
                d.vs,
                d.ps,
                d.primitive,
                reg(0x2205),
                regs.join(" "),
                reg(0x2201),
                reg(0x2200),
                reg(0x2000),
                reg(0x2001),
                reg(0x2002)
            ));
        }
        // The console's viewport is only a transform to window coordinates: the
        // GPU clips at its guard band, not at the viewport rectangle, and the
        // scissor limits drawing. Predicated tiling depends on this: the game
        // draws its scene through a one-tile (320-pixel) viewport with a
        // projection that reaches the whole 1280-pixel screen, and each tile's
        // window offset picks its part. So the wgpu viewport is the whole target
        // and D3D's scale and offset go into the position transform `m`
        // (`pos.xy * m.xy + m.zw * pos.w`), placing every vertex at the window
        // coordinates the console would.
        let viewport = [0.0, 0.0, tw as f32, th as f32, 0.0, 1.0];
        let (w, h) = (gw as f32, gh as f32);
        // Pixel centres: `PA_SU_VTX_CNTL` bit 0 clear is Direct3D 9's convention,
        // centres at whole window coordinates; the host's are at +0.5, so window
        // coordinates move by half a pixel to cover the same pixels.
        let d3d9 = reg(0x2302) & 1 == 0;
        // Logged when it changes (the first 20 times).
        static LAST: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(u32::MAX);
        static CHANGES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if LAST.swap(reg(0x2302), std::sync::atomic::Ordering::Relaxed) != reg(0x2302) && CHANGES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 20 {
            eprintln!(
                "render: PA_SU_VTX_CNTL {:08X} (pixel centres {}) from vs {:016x} ps {:016x}",
                reg(0x2302),
                if d3d9 { "Direct3D 9" } else { "half-pixel" },
                d.vs,
                d.ps
            );
        }
        // Above the native resolution (`s` host pixels per guest pixel) the console's
        // coverage of edges on guest pixel centres has to be kept. Its tie rule gives
        // a pixel whose centre is on a left or top edge to that edge's triangle.
        // Moving by half a guest pixel (s/2 host pixels) samples each host pixel a
        // quarter of a host pixel (at 1.5x) left of where its guest pixel's
        // triangles meet: the host column at such an edge went to the triangle
        // before it. The game draws its lighting cube's six faces as one mesh in a
        // strip, so every face's first column held the previous face's lighting
        // (or black, at the strip's corner), and lighting sampled those seams as
        // thin dark lines along the models.
        // - Meshes move by half a host pixel instead: host pixel centres then fall
        //   on guest pixel centres wherever an edge on an even guest coordinate is
        //   (every layout the game uses), so the tie rule decides as the console's.
        // - Quads (copy, filter and post-processing passes, 6 vertices or fewer)
        //   keep half a guest pixel, so a copy reads each host texel at its centre
        //   as at the native resolution; their viewport is widened about its centre
        //   by a quarter of a host pixel (and a 64th) at its edges, so a quad drawn
        //   from a corner covers its first host column and row.
        // Nothing changes at the native resolution.
        let (sx, sy) = (tw as f32 / w, th as f32 / h);
        let scaled = sx != 1.0 || sy != 1.0;
        let quad = d.vertex_count <= 6;
        let (hx, hy) = match (d3d9, scaled && !quad) {
            (false, _) => (0.0, 0.0),
            (true, false) => (0.5, 0.5),
            (true, true) => (0.5 / sx, 0.5 / sy),
        };
        let (dx, dy) = (shift[0] + hx, shift[1] + hy);
        let (zx, zy) = if d3d9 && scaled && quad {
            let reach = 0.25 + 1.0 / 64.0;
            if vte & 0xF == 0xF {
                (1.0 + reach / (f(0x210F).abs().max(1.0) * sx), 1.0 + reach / (f(0x2111).abs().max(1.0) * sy))
            } else {
                (1.0 + reach / (w * 0.5 * sx), 1.0 + reach / (h * 0.5 * sy))
            }
        } else {
            (1.0, 1.0)
        };
        let xform = if vte & 0xF == 0xF {
            // The viewport's scale widened about its centre (`zx`, `zy`).
            let (xs, xo, ys, yo) = (f(0x210F), f(0x2110), f(0x2111), f(0x2112));
            [2.0 * xs * zx / w, -2.0 * ys * zy / h, 2.0 * (xo + dx) / w - 1.0, 1.0 - 2.0 * (yo + dy) / h]
        } else {
            // Screen space: positions are already window coordinates (widened
            // about the target's centre).
            [2.0 * zx / w, -2.0 * zy / h, (2.0 * dx / w - 1.0) * zx, (1.0 - 2.0 * dy / h) * zy]
        };
        // Depth through D3D's scale and offset (the game reverses it).
        let zform = if vte & 0x30 == 0x30 { [f(0x2113), f(0x2114), 0.0, 0.0] } else { [1.0, 0.0, 0.0, 0.0] };
        // Alpha test: RB_COLORCONTROL bits 0-2 function, bit 3 enable; reference
        // RB_ALPHA_REF (0x210E, a float). 0x210D is RB_STENCILREFMASK: read as the
        // reference (0x00FFFFFF, about 2e-38), every alpha above zero passed, so
        // the nearly clear parts of hair cards were drawn and wrote depth, hiding
        // the hair behind them as dark shards.
        let cc = reg(0x2202);
        let alpha_test = [if cc & 8 != 0 { cc & 7 } else { 7 }, reg(0x210E), 0, 0];

        // Vertex memory: the draw's ranges in the frame's vertex buffer, and where each starts.
        let mut window = [0u32; 32];
        for (k, (first, bytes)) in d.vertex_ranges.iter().take(8).enumerate() {
            let words = bytes.len().div_ceil(4);
            let at = self.arena.vertex_range(*first, bytes);
            window[4 * k..4 * k + 3].copy_from_slice(&[*first, words as u32, at]);
        }
        // Fetch descriptors in the translated shader's order: each fetch takes
        // the first unused descriptor with its key (D3D reorders fetches).
        let mut used = vec![false; d.vfetch.len()];
        let mut ordered = Vec::with_capacity(pipe.vfetch_keys.len());
        for (k, key) in pipe.vfetch_keys.iter().enumerate() {
            let hit = (0..d.vfetch.len()).find(|&i| !used[i] && d.vfetch[i].0 == *key);
            let desc = match hit {
                Some(i) => {
                    used[i] = true;
                    d.vfetch[i].1
                }
                None => {
                    self.report(format!("vs {:016x}: no patched fetch matches fetch {k} (key {key:08x})", d.vs));
                    d.vfetch.get(k).map_or([0; 4], |v| v.1)
                }
            };
            ordered.extend_from_slice(&desc);
        }
        if ordered.len() > 4 * MAX_VFETCH {
            self.report(format!("vs {:016x}: {} vertex fetches, more than the 16 the renderer passes", d.vs, ordered.len() / 4));
            return None;
        }
        // WWE14_DEBUG_CONSTS=F1-F2:N=x,y,z,w@ps;...: in frames F1 to F2, pixel shader
        // constant N reads that value (debugging: which constant changes the image).
        let ps_override = self.debug_consts(d.ps).map(|over| {
            let mut c = (*d.ps_consts).clone();
            for (n, v) in over {
                let mut block = *c.0[n / 16];
                block[n % 16] = v;
                c.0[n / 16] = Arc::new(block);
            }
            c
        });
        let xform_floats: [f32; 8] = [xform[0], xform[1], xform[2], xform[3], zform[0], zform[1], zform[2], zform[3]];
        // The draw's record, in `BLOCK_ROWS` order. The constant files the
        // capture shares between draws (one `Arc` each) are written once.
        let a = &mut self.arena;
        let ps_rows = match &ps_override {
            None => a.shared_consts(1, 5, &d.ps_consts),
            Some(c) => {
                let row = a.consts(1, 5, c);
                a.keep.push(c.clone());
                row
            }
        };
        let record = [
            a.shared_consts(0, 0, &d.vs_consts),
            a.block(1, as_rows(&d.control)),
            a.block(2, as_rows(&d.fetch)),
            a.block(3, as_rows(&window)),
            a.block(4, as_rows(&ordered)),
            ps_rows,
            a.block(6, as_rows(&xform_floats)),
            a.block(7, as_rows(&alpha_test)),
        ];
        let draw = a.draw(record);

        let mut views = Vec::new();
        for &(n, cube) in &pipe.textures {
            let w = &d.fetch[6 * n as usize..6 * n as usize + 6];
            let base = physical(w[1] & 0xFFFF_F000);
            let captured = d.textures.iter().find(|t| t.0 == n).map(|t| t.1.clone());
            let resolved_cube = self.resolved.get(&base).and_then(|r| r.cube.clone());
            // The capture skips what a resolve wrote last, so a captured texture is
            // newer than any resolved image under the same address.
            let view = if cube {
                match captured {
                    Some(t) if t.layers == 6 && t.width == t.height => self.texture_view(&t, true),
                    // A cube map the game rendered (resolved face by face).
                    _ if resolved_cube.is_some() => resolved_cube.unwrap(),
                    _ => {
                        self.report(format!(
                            "fetch constant {n} is sampled as a cube but holds no cube map (white): base {base:08X}, words {w:08X?}, {}",
                            match (&captured, self.resolved.get(&base)) {
                                (Some(t), _) => format!("captured {}x{} with {} layers", t.width, t.height, t.layers),
                                (None, Some(_)) => "a resolved image without cube faces".into(),
                                (None, None) => "nothing captured or resolved there".into(),
                            }
                        ));
                        self.dummy_cube.clone()
                    }
                }
            } else if let Some(t) = &captured {
                self.texture_view(t, false)
            } else if let Some(r) = self.resolved.get(&base) {
                // A texture a resolve wrote: the rendered image, not guest memory.
                r.view.clone()
            } else {
                self.report(format!("fetch constant {n} has no texture (white): base {base:08X}, words {w:08X?}"));
                self.dummy_2d.clone()
            };
            // WWE14_DEBUG_SLOTS=F1-F2:N=r,g,b,a;...: in frames F1 to F2, fetch constant N
            // reads that constant colour (debugging: which input changes the image).
            let view = match self.debug_slot(n, d.ps) {
                Some(rgba) => self.constant_view(rgba, cube),
                None => view,
            };
            // Depth (24:8 and 24:8 float) can't be filtered on the console: the game
            // reads exact depths. Our depth resolves are 32-bit floats a host sampler
            // could filter, which at 1.5x mixed foreground and background depths at
            // silhouettes (the depth-of-field downsample's taps land between host
            // texels) and let models bleed into the blurred background as a pale
            // blocky fringe. Point sampling, as on the console.
            let w3 = if matches!(w[1] & 0x3F, 22 | 23) { w[3] & !(0xF << 19) } else { w[3] };
            views.push((n, view, self.sampler(w[0], w3)));
        }
        // The same pipeline with the same views and samplers: the same bind group.
        let key: Vec<usize> = std::iter::once(Arc::as_ptr(&pipe) as usize)
            .chain(views.iter().flat_map(|(n, v, s)| [*n as usize, Arc::as_ptr(v) as usize, Arc::as_ptr(s) as usize]))
            .collect();
        let group1 = match self.group1s.get(&key) {
            Some(g) => g.0.clone(),
            None => {
                let mut entries = Vec::new();
                for (n, view, sampler) in &views {
                    entries.push(wgpu::BindGroupEntry { binding: 2 * n, resource: wgpu::BindingResource::TextureView(view) });
                    entries.push(wgpu::BindGroupEntry { binding: 2 * n + 1, resource: wgpu::BindingResource::Sampler(sampler) });
                }
                self.made[2] += 1;
                let g = Arc::new(self.device.create_bind_group(&wgpu::BindGroupDescriptor { label: None, layout: &pipe.group1, entries: &entries }));
                if self.group1s.len() > 8192 {
                    self.group1s.clear();
                }
                let keep = (g.clone(), views.iter().map(|v| v.1.clone()).collect(), views.iter().map(|v| v.2.clone()).collect(), pipe.clone());
                self.group1s.insert(key, keep);
                g
            }
        };
        // A list used as the capture gave it is shared: written once a frame.
        let shared = match (&indices, &d.indices) {
            (Some(std::borrow::Cow::Borrowed(_)), Some(a)) => Some(Arc::as_ptr(a) as usize),
            _ => None,
        };
        let index = indices.map(|i| {
            let off = match shared {
                Some(key) => self.arena.shared_indices(key, &i),
                None => self.arena.indices(&i),
            };
            (off, i.len() as u32)
        });
        Some(PreparedDraw { pipe, draw, group1, index, count: n, base_vertex: d.base_vertex, viewport, scissor })
    }

    /// The display image's pixels (RGBA8, `WIDTH * HEIGHT * 4` bytes).
    /// Debugging: every resolved texture and colour surface as RGBA8 (name,
    /// width, height, pixels). Float formats are clamped to 0..1, single
    /// channels shown as grey.
    pub fn debug_images(&self) -> Vec<(String, u32, u32, Vec<u8>)> {
        let mut out = Vec::new();
        for (base, r) in &self.resolved {
            // Every face of a cube and every mip level: each is rendered on its own.
            for layer in 0..r.layers {
                for mip in 0..r.mips {
                    let (w, h) = ((r.width >> mip).max(1), (r.height >> mip).max(1));
                    let suffix = if r.layers > 1 || r.mips > 1 { format!("_L{layer}M{mip}") } else { String::new() };
                    if let Some(px) = self.read_rgba_at(&r.texture, r.format, w, h, layer, mip) {
                        out.push((format!("resolved_{base:08X}{suffix}_{:?}", r.format), w, h, px));
                    }
                }
            }
        }
        for (k, s) in &self.surfaces {
            if s.format != DEPTH_FORMAT {
                let rows = s.height.min(1024);
                if let Some(px) = self.read_surface(s, rows) {
                    out.push((format!("surface_{:03X}_{}_{}x{}_{:?}", k.base, k.format, k.pitch, k.samples, s.format), s.width, rows, px));
                }
            }
        }
        out
    }

    /// Reads back the top-left `w` x `h` of a texture as RGBA8.
    fn read_rgba(&self, texture: &wgpu::Texture, format: wgpu::TextureFormat, w: u32, h: u32) -> Option<Vec<u8>> {
        self.read_rgba_at(texture, format, w, h, 0, 0)
    }

    /// Reads back the top-left `w` x `h` of one array layer and mip level as RGBA8.
    fn read_rgba_at(&self, texture: &wgpu::Texture, format: wgpu::TextureFormat, w: u32, h: u32, layer: u32, mip: u32) -> Option<Vec<u8>> {
        use wgpu::TextureFormat as T;
        let bpp = match format {
            T::Rgba8Unorm | T::Rgb10a2Unorm | T::R32Float => 4,
            T::Rgba16Float | T::Rg32Float => 8,
            T::R16Float => 2,
            _ => return None,
        };
        let row = (w * bpp).next_multiple_of(256);
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("debug readback"),
            size: (row * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = self.device.create_command_encoder(&Default::default());
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture, mip_level: mip, origin: wgpu::Origin3d { x: 0, y: 0, z: layer }, aspect: Default::default() },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(h) },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        self.queue.submit([enc.finish()]);
        buffer.map_async(wgpu::MapMode::Read, .., |_| {});
        let _ = self.device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None });
        let data = buffer.get_mapped_range(..).map(|v| v.to_vec()).ok()?;
        buffer.unmap();
        let unit = |x: f32| (x.clamp(0.0, 1.0) * 255.0) as u8;
        let half = |b: &[u8]| f16_to_f32(u16::from_le_bytes([b[0], b[1]]));
        let mut px = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let p = &data[(y * row + x * bpp) as usize..][..bpp as usize];
                let rgba = match format {
                    T::Rgba8Unorm => [p[0], p[1], p[2], p[3]],
                    T::Rgb10a2Unorm => {
                        let v = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
                        let c = |s: u32| ((v >> s & 0x3FF) * 255 / 0x3FF) as u8;
                        [c(0), c(10), c(20), ((v >> 30) * 85) as u8]
                    }
                    T::Rgba16Float => [unit(half(&p[0..])), unit(half(&p[2..])), unit(half(&p[4..])), unit(half(&p[6..]))],
                    T::R16Float => [unit(half(p)); 4],
                    _ => [unit(f32::from_le_bytes([p[0], p[1], p[2], p[3]])); 4],
                };
                px.extend_from_slice(&[rgba[0], rgba[1], rgba[2], 255]);
            }
        }
        Some(px)
    }

    /// Answers the game's requests for resolved images it reads on the CPU
    /// (`gpu::ir::request_readback`), `done` being the last frame rendered.
    /// The host calls this after each frame, and every few milliseconds while
    /// no frame arrives (the game's frames stop while its reader waits).
    pub fn serve_readbacks(&self, done: u64) {
        for request in gpu::ir::take_readbacks(done) {
            let image = self.read_resolved(request.phys);
            request.reply(image);
        }
    }

    /// The image a resolve left at guest address `phys`, at its guest size as
    /// RGBA8 rows (alpha included). The renderer keeps it at host size; above
    /// the native resolution it is filtered down.
    fn read_resolved(&self, phys: u32) -> Result<gpu::ir::ReadImage, String> {
        let Some(t) = self.resolved.get(&phys) else {
            let mut near: Vec<u32> = self.resolved.keys().copied().collect();
            near.sort_by_key(|k| k.abs_diff(phys));
            near.truncate(4);
            return Err(format!("the renderer has no resolved image at physical 0x{phys:08X} (of {}; the nearest are {near:08X?})", self.resolved.len()));
        };
        if t.format != wgpu::TextureFormat::Rgba8Unorm || t.layers != 1 {
            return Err(format!("the renderer's image there is {:?} with {} layers; only one layer of 8-bit RGBA is read back", t.format, t.layers));
        }
        let (w, h) = (t.width, t.height);
        let row = (w * 4).next_multiple_of(256);
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("resolve readback"),
            size: (row * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = self.device.create_command_encoder(&Default::default());
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &t.texture, mip_level: 0, origin: Default::default(), aspect: Default::default() },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(h) },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        self.queue.submit([enc.finish()]);
        buffer.map_async(wgpu::MapMode::Read, .., |_| {});
        let _ = self.device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None });
        let data = buffer.get_mapped_range(..).map(|v| v.to_vec()).map_err(|e| format!("the copy of the image could not be read from the GPU: {e:?}"))?;
        buffer.unmap();
        let (gw, gh) = t.guest;
        let texel = |x: u32, y: u32| -> [f32; 4] {
            let p = &data[(y * row + x * 4) as usize..][..4];
            [p[0] as f32, p[1] as f32, p[2] as f32, p[3] as f32]
        };
        let mut px = Vec::with_capacity((gw * gh * 4) as usize);
        for y in 0..gh {
            // The host texel centres around this guest pixel's centre.
            let fy = ((y as f32 + 0.5) * h as f32 / gh as f32 - 0.5).clamp(0.0, (h - 1) as f32);
            let (y0, ty) = (fy as u32, fy.fract());
            let y1 = (y0 + 1).min(h - 1);
            for x in 0..gw {
                let fx = ((x as f32 + 0.5) * w as f32 / gw as f32 - 0.5).clamp(0.0, (w - 1) as f32);
                let (x0, tx) = (fx as u32, fx.fract());
                let x1 = (x0 + 1).min(w - 1);
                let (a, b, c, d) = (texel(x0, y0), texel(x1, y0), texel(x0, y1), texel(x1, y1));
                for k in 0..4 {
                    let top = a[k] + (b[k] - a[k]) * tx;
                    let bottom = c[k] + (d[k] - c[k]) * tx;
                    px.push((top + (bottom - top) * ty + 0.5) as u8);
                }
            }
        }
        Ok((gw, gh, px))
    }

    /// The display image's pixels (RGBA8, `output_size`).
    pub fn read_target(&self) -> Vec<u8> {
        let (width, height) = output_size();
        let row = width * 4;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (row * height) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = self.device.create_command_encoder(&Default::default());
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &self.target, mip_level: 0, origin: Default::default(), aspect: Default::default() },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(height) },
            },
            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        );
        self.queue.submit([enc.finish()]);
        buffer.map_async(wgpu::MapMode::Read, .., |_| {});
        let _ = self.device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None });
        let data = buffer.get_mapped_range(..).map(|v| v.to_vec()).unwrap_or_default();
        buffer.unmap();
        data
    }
}

struct PreparedDraw {
    pipe: Arc<Pipe>,
    /// The draw's record in the frame's buffer (drawn as this instance).
    draw: u32,
    group1: Arc<wgpu::BindGroup>,
    /// Byte offset of the index list in the frame's index buffer, and its length.
    index: Option<(u32, u32)>,
    count: u32,
    base_vertex: i32,
    viewport: [f32; 6],
    /// Clip rectangle x1, y1, x2, y2 (a predicated draw's tile); none: the target.
    scissor: Option<[u32; 4]>,
}

/// Blend state from `RB_BLENDCONTROL` (0 = blending off).
fn blend_state(b: u32) -> Option<wgpu::BlendState> {
    if b == 0 {
        return None;
    }
    use wgpu::BlendFactor as F;
    let factor = |v: u32| match v {
        0 => F::Zero,
        1 => F::One,
        4 => F::Src,
        5 => F::OneMinusSrc,
        6 => F::SrcAlpha,
        7 => F::OneMinusSrcAlpha,
        8 => F::Dst,
        9 => F::OneMinusDst,
        10 => F::DstAlpha,
        11 => F::OneMinusDstAlpha,
        12 | 14 => F::Constant,
        13 | 15 => F::OneMinusConstant,
        16 => F::SrcAlphaSaturated,
        _ => F::One,
    };
    let op = |v: u32| match v {
        1 => wgpu::BlendOperation::Subtract,
        2 => wgpu::BlendOperation::Min,
        3 => wgpu::BlendOperation::Max,
        4 => wgpu::BlendOperation::ReverseSubtract,
        _ => wgpu::BlendOperation::Add,
    };
    let comp = |src: u32, fcn: u32, dst: u32| {
        let o = op(fcn);
        // wgpu requires One factors for min and max.
        if matches!(o, wgpu::BlendOperation::Min | wgpu::BlendOperation::Max) {
            wgpu::BlendComponent { src_factor: F::One, dst_factor: F::One, operation: o }
        } else {
            wgpu::BlendComponent { src_factor: factor(src), dst_factor: factor(dst), operation: o }
        }
    };
    Some(wgpu::BlendState {
        color: comp(b & 0x1F, (b >> 5) & 7, (b >> 8) & 0x1F),
        alpha: comp((b >> 16) & 0x1F, (b >> 21) & 7, (b >> 24) & 0x1F),
    })
}

/// IEEE half to single precision (debug readback).
fn f16_to_f32(h: u16) -> f32 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = (h >> 10 & 0x1F) as i32;
    let man = (h & 0x3FF) as f32;
    sign * match exp {
        0 => man * 2f32.powi(-24),
        31 => if man == 0.0 { f32::INFINITY } else { f32::NAN },
        e => (1.0 + man / 1024.0) * 2f32.powi(e - 15),
    }
}

/// The physical address of a texture base given in any of the guest's views:
/// resolved textures are kept by physical address, because a resolve names its
/// destination by CPU address (`0xA0000000`, `0xC0000000` or the `0xE0000000`
/// view, which is offset by 0x1000) while texture fetches give the physical
/// address the GPU uses.
fn physical(base: u32) -> u32 {
    gpu::texture::physical(base)
}
