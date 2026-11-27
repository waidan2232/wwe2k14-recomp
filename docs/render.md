# Rendering

How frames get from the game's D3D calls to the screen (plan §2.4 items 3–6, §2.6). No GPU hardware is modelled at any point: D3D's shadow state is read at its API boundary, and shaders were translated ahead of time.

## Path

1. **Native D3D entry points** (`runtime/gpu`, `docs/d3d.md`). A draw (today `EndVertices`) captures one `ir::Draw` from the device's shadow state. `Swap` closes the frame and sends an `ir::Frame` over a channel. When the renderer is behind, frames are dropped; the game never waits on the host. Frame pacing is `Swap`'s 60 Hz vertical blank.
2. **Renderer** (`runtime/render`): a wgpu executor. It runs each draw with the translated shaders, found by identity, into a 1280×720 RGBA8 front buffer.
3. **Host** (`runtime/host`):
   - **Window:** Bevy owns the main thread and the window, and the guest runs on its own threads. `GpuBridgePlugin` (`runtime/host/src/window.rs`) runs the renderer in a `RenderGraph` system before Bevy draws its cameras, and copies the front buffer into an image shown on a full-window sprite.
   - **Headless:** `--headless DIR` uses the same renderer offscreen and writes previews.

## What a draw carries (`runtime/gpu/src/ir.rs`)

| Field | Source |
|---|---|
| `vs`, `ps` | Shader identity (`ucode::identity`) of the bound microcode (`shaders::bound`) |
| `vs_consts`, `ps_consts` | Device `+0x780` / `+0x1780`, 256 × float4 each, converted to host floats. Each shader's literal constants (`def`) are loaded over them, as D3D does when it sets a shader: the definition table is in the container copy inside D3D's shader object (pixel `+0x28`, vertex `+0x368`), and the values are at the start of its physical part (pixel `+0x18`, vertex `+0x20`). Translated shaders therefore never bake literals in, and shaders that differ only in literals share one translation |
| `control` | `+0x2780`: 8 bool-constant words, then 32 loop-constant words |
| `fetch` | `+0x480`: 32 fetch constants × 6 words. For inline vertices, the vertex streams the shader uses point at the inline data (endian 8-in-32). Guest state is untouched |
| `vfetch` | One descriptor per vertex fetch, from D3D's patched fetch (`ucode::vertex_fetch_descriptor`) |
| `vertex_ranges` | The guest memory the vertex streams cover: one range per stream (overlaps merged, at most 8), each with its physical word address. The renderer packs them into one storage buffer with a table the shader searches |
| `indices`, `base_vertex` | An indexed draw's indices, read from the bound index buffer as the CPU wrote them (16-bit `0xFFFF` widened to the strip restart value), and `VGT_INDX_OFFSET` |
| `textures` | Every fetch-constant slot holding a texture, untiled and converted to RGBA8 (`texture.rs`), cached by address, layout and content hash |
| `regs` | Shadowed register blocks `0x2000`, `0x2100`, `0x2180` and `0x2200` |

## Shader interface (`tools/xenos-shader-recomp/src/wgsl.rs`)

Group 0:

| Binding | Stage | Contents |
|---|---|---|
| 0 | vertex | Float constants (`array<vec4<f32>, 256>`) |
| 1 | both | Bool and loop constants |
| 2 | both | Fetch constants (`array<vec4<u32>, 48>`) |
| 3 | vertex | Guest vertex memory (`array<u32>`, bytes as stored) |
| 4 | vertex | Its ranges: first physical word, word count, and offset in binding 3 (up to 8) |
| 5 | vertex | Vertex fetch descriptors: fetch constant, format word, stride and offset in words |
| 6 | pixel | Float constants |
| 7 | vertex | Position transform `m`: `pos.xy * m.xy + m.zw * pos.w` |

Group 1: texture `2n` and sampler `2n + 1` for fetch constant `n`. A texture is `texture_cube` when the shader fetches it as a cube.

## State the renderer interprets

| What | Register | Notes |
|---|---|---|
| Primitive | — | List, strip, point and line types map directly. Fans and quad lists (13) get generated indices. Rect lists are not drawn yet and are reported |
| Blend | `RB_BLENDCONTROL0` `0x2201` | Source/destination factors and operations for colour and alpha. `0x00010001` (1·src + 0·dst) means blending is off |
| Colour mask | `RB_COLOR_MASK` `0x2104` | Bits 0–3 for render target 0 |
| Viewport | `PA_CL_VTE_CNTL` `0x2206`, `PA_CL_VPORT_*` `0x210F`–`0x2114` | With the scale and offset enabled, the wgpu viewport, and `m` handles mirrored scales. With them off, `m` maps screen space to NDC |
| Samplers | Texture fetch words 0 and 3 | Clamp modes and min/mag filters |
| Depth | `RB_DEPTHCONTROL` `0x2200` | Test, write and compare function. The game's reversed range (Z scale -1, offset 1) is applied in the vertex shader |
| Culling | `PA_SU_SC_MODE_CNTL` `0x2205` | Front, back, and which winding is front |
| Alpha test | `0x2202`, `0x210D` | A translator epilogue reads the function and reference from binding 8 |

## Render targets and resolves

- Each surface D3D binds (EDRAM base, format and pitch, from the shadowed `RB_SURFACE_INFO`, `RB_COLOR_INFO` and `RB_DEPTH_INFO`) is a wgpu texture. Depth buffers are `Depth24PlusStencil8`.
- A colour resolve copies the rectangle into a texture kept under the destination's guest address. Later draws sample it in place of guest memory.
- A depth resolve runs a small copy shader of ours (not the game's). It writes the depth values into a float texture (`R32Float` where the device can filter it, else `R16Float`), with depth in every component.
- A resolve into a cube map writes one face: the resolve's slice argument is the face. The game renders its environment cube this way (six 64×64 faces side by side, then six resolves). Lit materials sample it, so without it they come out black. A cube fetch prefers a cube the game resolved over guest memory, which never holds GPU-rendered faces.
- Rectangle clears and the clear after a resolve cover only their rectangles: a scissored full-screen triangle of ours writes the colour, depth and stencil.
- The frame shows the texture resolved into the presented front buffer, or else the last target drawn.

**Debugging** (headless mode):
- `WWE14_DUMP_TARGETS=N` saves every resolved texture and colour surface as PNGs with each still from frame N on.
- `WWE14_DRAW_LOG=N` logs every draw, clear and resolve of a frame, with the state the renderer uses. The forms select the frame:
  - `N`: frame N;
  - `>N`: the first frame with more than N draws;
  - `@N`: the first frame from N on;
  - `@N>M`: the first frame from N on with more than M draws.

  Several, comma-separated, each log one frame. Logs after the first go to `WWE14_DRAW_LOG_FILE.<frame>`. Each draw lists every texture slot: format, per-channel sign modes, exponent adjust and size. A `*` marks a slot that is resolved or not captured.
- `WWE14_DRAW_LOG_PS_CONSTS=LO..HI` adds those pixel shader constants to every logged draw.
- `WWE14_DEBUG_SLOTS=F1-F2:N=r,g,b,a@ps;...` makes fetch constant N read one constant colour in frames F1 to F2. `@ps` limits it to pixel shaders whose id starts with that prefix.
- `WWE14_DEBUG_CONSTS=F1-F2:N=x,y,z,w@ps;...` does the same for pixel shader constant N. With per-draw dumps, these two switches find which input of a shader changes the image.
- `WWE14_DUMP_EACH_DRAW=LO..HI` (with `WWE14_DUMP_OPS_DIR`) gives the logged frame's draws LO to HI (0-based) a pass each, so the dumps show the target after every one of them (`op…_pass_dN_…`).
- `WWE14_DRAW_LOG_CONSTS=LO..HI` adds those vertex shader constants to every logged draw. Each logged draw also gets its vertex fetches, captured ranges, index span, strip restarts, the largest integer 8_8_8_8 component (blend indices), the position range and its bool constants.
- `WWE14_LOG_LITERALS=1` logs each shader container's literal (`def`) table once.

## Textures

- The base level is converted to RGBA8 from guest memory: untiled, endian-swapped, decoded, then the fetch constant's channel swizzle is applied.
- Cube maps upload all six faces; each face starts on a 4 KB boundary. A shader that fetches a cube from a slot holding no cube map gets a white cube, and this is reported.

**Not yet:**
- scissor;
- MSAA (targets are single-sampled; the 4× MSAA scene renders without it);
- mip levels beyond the base, and BCn passthrough;
- the stencil part of a depth resolve.

## Missing shaders

A draw whose shader has no translation is skipped. The shader is reported once by name (`vs_<identity>.wgsl`), to be added to the offline harvest. It is never translated at run time.
