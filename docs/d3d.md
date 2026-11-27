# The game's D3D library: map and replacement plan

What we know about the statically linked Direct3D 9 (XDK 2.0.21250) in `default.xex`, and how Phase 4 replaces it. Everything here comes from static analysis of the game's own code (`analyse gpu`, `analyse fns`, `analyse dis`); no symbols are available.

## Where it is

- **Library code:** roughly `0x82CA4000`–`0x82CCB000`. Below it is XAPI (up to about `0x82CA3D60`). Above it, from `0x82CCB000`, are D3DX9 and XGRAPHICS.
- **Packet builders:** every function that builds Xenos command packets is in `0x82CA8CD0`–`0x82CCACE8`. `analyse gpu` finds type-3 PM4 headers (`3<<30 | (n-1)<<16 | opcode<<8`) in 80 functions, all inside D3D. The game's own code builds none.
- **MMIO:** the GPU registers at `0x7FC8xxxx` are touched by 8 functions, all in D3D. The audio/XMA block at `0x7FEAxxxx` is touched by 5 functions in the XMA code (`0x82D28xxx`, `0x8343xxxx`); that is Phase 5.
- **API surface:** 134 functions in the range are called from outside it, at 604 call sites. Most callers are the game's renderer (`0x82BAxxxx`–`0x82C2xxxx`) and D3DX/XGRAPHICS (`0x82CFxxxx`–`0x82D0xxxx`). `analyse gpu <xex> 82CA4000 82CE0000` prints the full list.
- **Outgoing calls:** D3D calls the CRT, XAPI, and the title's own `XMemAlloc`/`XMemFree` (`0x8225FB30`/`0x8225F778`). The title overrides these.

## Device creation

| Function | What it is |
|---|---|
| `sub_82CB2758` | `Direct3D_CreateDevice(adapter, type, window, behavior, params, &device)`. Allocates the device (0x6080 bytes, 128-byte aligned, via `sub_82CB2590` → `XMemAlloc`). Called from the game renderer `sub_82BA8018` and from `sub_82C272E0` |
| `sub_82CC0808` | Device constructor: CPU-side initialisation |
| `sub_82CC0DA0` | Hardware initialisation (below). **First replacement target** |
| `sub_82CC0770` | `VdSetGraphicsInterruptCallback`, `VdInitializeEngines`, title-terminate notification |
| `sub_82CB7228` | Ring buffer: `VdInitializeRingBuffer`, read-pointer write-back, `ME_INIT` |
| `sub_82CC0B20` | Front and back buffers (`dev+0x3B7C/0x3B80/0x3B84`) through D3D's resource functions; render target and initial state; display notification |

**`sub_82CC0DA0`, step by step:**

| Step | Kind |
|---|---|
| Critical sections at `+0x3BF4`, `+0x3C10` | CPU |
| Device pointer published in `VdGlobalDevice` (or `VdGlobalXamDevice` for XAM) | CPU |
| `XMemAlloc` 0x80 → `+0x2B10`, 0x20 → `+0x2B14` | CPU |
| `ExGetXConfigSetting(3, 0xA)` video flags → `+0x42E4` | CPU |
| Ring buffer (`sub_82CB7228`), prerecorded command buffers (`sub_82CB25E0`, `sub_82CB2080`, `sub_82CB10B8`) | hardware |
| GPU interrupt worker thread + DPC (`sub_82CCAAF0`, MMIO) | hardware |
| Register read-modify-write (`sub_82CCACE8`) | hardware |
| `+0x55D0/+0x55D4` = -1 | CPU |
| Front/back buffers (`sub_82CC0B20`) | CPU (D3D resource bookkeeping) |
| `sub_82CC0150` | to check |
| Performance frequency → `+0x55EC`, 1/frequency → `+0x55F0` | CPU |
| Debug-monitor hooks (`sub_82CC26B0`; none attached) | CPU |
| EDRAM retrain (`sub_82CC6380`), bin masks (`sub_82CAB868`) | hardware |
| `+0x5CF0..` = -1 ×41 (`sub_82CC7600`) | CPU |

## Device layout (known so far)

| Offset | Meaning |
|---|---|
| `+0x00`..`+0x28` | 64-bit dirty masks. The draw path flushes each set: `+0x00` vertex-shader constants (registers `0x4000`), `+0x08` pixel-shader constants (`0x4400`), then fetch constants and render state |
| `+0x30` / `+0x34` | Command-buffer write cursor / limit. `sub_82CB6EA8` makes room or kicks (122 callers inside D3D) |
| `+0x780` | Vertex-shader constant shadow (256 × float4) |
| `+0x1780` | Pixel-shader constant shadow |
| `+0x2934`.. | Other shadowed GPU state (register `0x2200` onward) |
| `+0x2B10`, `+0x2B14` | Small device-owned buffers |
| `+0x2B3C`..`+0x2B3E` | Flag bytes |
| `+0x3B7C`/`+0x3B80`/`+0x3B84` | Front and back buffer surfaces |
| `+0x5F48` | Behavior flags from `CreateDevice` |
| size `0x6080` | |

## Replaced so far

| Function | Config name | Native behaviour |
|---|---|---|
| `sub_82CC0DA0` | `D3DDevice_InitializeHardware` | The CPU-side steps above, in order. D3D's own frame-buffer, default-state, debug-hook and table routines run as recompiled code. Every hardware step is left out, so the command cursor stays null |
| `sub_82CA8CD0` | `D3DDevice_WriteViewportScissor` | Nothing. It only turns the shadowed viewport/scissor into packets |
| `sub_82CBAEE0` | `D3DDevice_Set2B24Bits` | Shadow word `+0x2B24` bits 12–13 = value; its packets are dropped |
| `sub_82CB0F58` | `D3DDevice_SetShaderAllocation` | Shadow word `+0x2B28` (two 7-bit allocation fields), then D3D's dirty marking (`sub_82CB04D0`, recompiled). Its shader indirect buffers are dropped |

With these, `Direct3D_CreateDevice` completes. The boot goes on to start the render threads and the Wwise/XAudio2 audio engine, and stops at the XMA hardware (Phase 5).

**Finding each next replacement:**
1. Run with the command cursor null. A D3D function that writes commands faults at guest address `0x4`, and the report names it and its callers.
2. `analyse effects <function>` lists what it stores through the device pointer, apart from the cursor.
3. If it stores only the cursor, the replacement is empty. Otherwise the replacement makes the same shadow-state stores and calls any CPU-only callees as recompiled code.

The type-3 census misses functions that write only type-0 register packets (for example `sub_82CBAEE0`); this loop catches them.

## Shadow state (where the renderer reads D3D's state)

From the draw path's flush (`sub_82CB1520`'s calls to `sub_82CBD428`, `sub_82CBD668` and `sub_82CBD7C0`). Each region is a copy of a GPU register range, in register order.

| Device offset | Registers | Contents |
|---|---|---|
| `+0x480` | `0x4800`.. | Fetch constants: 32 slots × 6 words. Texture fetch = one slot; vertex fetch = 2 words (3 per slot, D3D uses slot 95 − stream) |
| `+0x780` | `0x4000`.. | Vertex shader float constants, 256 × float4 |
| `+0x1780` | `0x4400`.. | Pixel shader float constants, 256 × float4 |
| `+0x2780` | `0x4900`.. | Bool and loop constants |
| `+0x2880` | `0x2000`.. | Render target / surface |
| `+0x28CC` | `0x2100`.. | |
| `+0x2920` | `0x2180`.. | Shader program control |
| `+0x2934` | `0x2200`.. | Depth, blend, color and clip control, … |
| `+0x2964` | `0x2280`.. | |
| `+0x29B8` | `0x2300`.. | |
| `+0x2A50` | `0x2380`.. | |
| `+0x2FD0` | | Bound vertex declaration |
| `+0x3268` | | Inline vertex stride (words) |
| `+0x3310` / `+0x330C` | | Bound vertex / pixel shader objects |

**Texture fetch constants** (`runtime/gpu/src/texture.rs`):
- Word 0: type `2`, pitch in 32-texel units at bits 22–30, tiled at bit 31.
- Word 1: format at bits 0–5, endian at bits 6–7, base at bits 12–31.
- Word 2: the size. For 2D, width−1 at bits 0–12 and height−1 at bits 13–25.
- Word 5: dimension at bits 9–10.

The base is a CPU virtual address or a 29-bit GPU address. The GPU address of CPU address `a` is `(a & 0x1FFFFFFF) + (a >= 0xE0000000 ? 0x1000 : 0)`, as D3D computes it in its shader upload; this confirms the kernel's physical-view layout. Tiled surfaces use the console's 2D tiling, checked to be a permutation for every block size.

**Shader objects** (D3D's upload in `sub_82CBE558`):
- **Pixel shader:** `r = ps + ps[0x40]`. The microcode is at `ps[0x18] + r[0x28]`, `r[0x2C]` bytes.
- **Vertex shader:** variant `i = (vs[0x368] & 0x20) ? 1 : 0`, `r = vs + vs[0x380 + 8i]`. The microcode is at `vs[0x20] + r[0x368]`, `r[0x36C]` bytes.
- **Vertex-fetch patching.** D3D patches each variant's vertex-fetch instructions for the bound declaration and strides (`sub_82CBE418` → `sub_82CBD910`). That is CPU-only, so the native draw calls it before reading the shader. The harvested microcode then carries the real formats, strides and offsets.
- **What the patch changes:**
  - word 0: the fetch constant and the prefetch count;
  - word 1: the swizzle, signed/integer/mode flags, format, exponent adjust and mini-fetch bit;
  - word 2: the stride and offset.

  Registers, operand component and predication stay. The patch also reorders the fetches among their slots, and makes fetches that share a stream mini fetches of the first.
- **How the port copes:**
  - Shader identity (`ucode::identity`) masks the changed fields and hashes the fetches as a sorted set.
  - Each fetch has a key from its fixed fields (`ucode::vertex_fetch_key`). Translated vertex shaders list their fetches' keys in a `// vfetch-keys:` header.
  - The draw captures descriptors from the patched code with mini fetches made full (`ucode::vertex_fetch_descriptors`). The renderer gives each fetch the descriptor with its key.

**Shader containers** (the XDK's compiled-shader format), with magic `0x102A1100` (pixel) or `0x102A1101` (vertex):
- **Header:** `+0x04` virtual (header) size, `+0x08` physical size, `+0x10` constant table offset, `+0x14` definition table offset, `+0x18` shader record offset.
- **Microcode:** the physical part follows the virtual part. The microcode is at `virtual size + record[+0x00]`, `record[+0x04]` bytes. Literal constants can come first.
- **Literal constants (`def`):** the definition table holds 16 bytes, a section size at `+0x10`, then 8-byte entries from `+0x14`: `register << 16 | float count`, then the byte offset of the values in the physical part. Registers 256 and up are the pixel bank; empty entries pad the table. D3D loads them into the stage's constant registers when the shader is set (`ir::load_literals`). Reading the entries as 4 bytes loses every run after the first (the skinning shaders keep their `1 − x` constant in a second run).
- **Where they are:** the executable holds 68 (the XDK WMV decoder's, plus 3 others). The game's 1,440 are in the `.pac` archives, inside BPE-compressed entries (`tools/pac`).

## Replaced since

| Function | Config name | Native behaviour |
|---|---|---|
| `sub_82CB37C0` | `D3DDevice_Swap` | D3D's bookkeeping: swap counter `+0x4248`, front-buffer header copy at `+0x3B90`, issued/completed frame counters advancing together, pending display changes consumed. Waits for the next 60 Hz vertical blank |
| `sub_82CB1520` / `sub_82CB1510` | `D3DDevice_BeginVertices` / `EndVertices` | Inline-vertex draws. Begin returns a scratch buffer and keeps the stride shadow; End is the draw: vertex shader patch, shader harvest, then the renderer |
| `sub_82CB19E0` | `D3DDevice_DrawIndexedVertices` | `(device, primitive, base vertex, start index, index count)`. The original flushes the dirty state into packets, clears the dirty masks `+0x0`..`+0x20`, and writes `VGT_INDX_OFFSET` and the draw (index buffer object at `+0x320C`: word 0 bit 31 = 32-bit indices, bits 29-30 endian; data at `+0x18`). Native: vertex shader patch, indices read from guest memory, dirty masks cleared, the draw to the renderer |
| `sub_82CB52B8` | `D3DDevice_Clear` | `(device, count, rects, flags, ARGB colour, f1 Z, stencil, EDRAM clear)`. Native: records the clear (with the bound surface registers) for the renderer |
| `sub_82CB3300` | `D3DDevice_SynchronizeToPresentationInterval` | Worker `(device, flag)` behind the API entry `sub_82CB37B8` (flag 0); the game calls it before `Resolve` and `Swap`. The original makes the GPU wait for the vertical blank, queues the presentation-statistics callback `sub_82CB2A28` (which calls a game callback at `+0x4230` if one is registered), and refreshes Hi-Z through `sub_82CB7770`. Native: Hi-Z state left off as `sub_82CB7770(device, 0)` leaves it, `+0x2B3D` bit 4 set; `Swap` paces to the vertical blank |
| `sub_82CB78E0` / `sub_82CB7C50` | `D3DDevice_BeginTiling` / `EndTiling` | Predicated tiling. Begin: `(device, flags, count, tile rects, clear colour, f1 Z, stencil)`; End: `(device, resolve flags, resolve rects (else the tile rects at `+0x3348`), dest texture, clear colour, f1 Z, stencil, parameters)`. Native: one full pass. Begin keeps the tile count at `+0x3344` and records the clear unless `flags` bit 0; End records one resolve (clearing colour when a colour is given) and sets the count back to 0. Both end, as the originals do, by setting the whole-target viewport (`{0,0,65535,65535,0,1}` at `0x82082430`) through D3D's `SetViewport` (`sub_82CABBC8`): 1280×720 while tiling. Begin also calls `sub_82CAB868(device, 1)`. The game reads the viewport back after BeginTiling, so skipping this squeezed every tiled scene into one tile |
| `sub_82CB7770` | `D3DDevice_SetPredication` | `(device, mask)`: the predication mask (`+0x331C`, on flag `+0x3324`; tile i is bins `3 << 2i`) and Hi-Z bookkeeping (`+0x2B3C` bit 0, `+0x3320`, `+0x2B34`, `+0x2B38`; see `commands::predication_state`), then a `SET_BIN_MASK_LO` packet. Native: the bookkeeping |
| `sub_82CB71D0` | `D3DDevice_InsertCallback` | `(device, type, callback, context)`: a packet that makes the GPU interrupt the CPU, whose handler calls `callback(context)`. The game uses it to timestamp GPU progress (`0x82C16978`). Native: queued and called at the end of the frame, after `Swap` |
| `sub_82CAFB88` | `D3DDevice_Resolve` | `(device, flags, source rect, dest texture, dest point, level, slice, clear colour, f1 clear Z, clear stencil, parameters)`; worker `sub_82CAEA30`. Its packets would wait forever in the command-buffer space allocator `sub_82CB57F0` (it waits on the GPU's progress write-back at `*(+0x2B10) + 4`). Native: records a resolve (source surface registers, rect, the destination's fetch constant at texture `+0x1C`, clears) for the renderer, which copies between its own textures |

## How Phase 4 replaces it

1. **What stays recompiled.** The CPU-side bookkeeping: the state setters that write the shadow state and dirty bits, and resource headers.
2. **What is native.** `[[replace]]` entries in `config/wwe14.toml` make every function that sets up the hardware or writes commands native (`runtime/gpu`). That covers device setup, draws, clears, resolves, present and fences.
3. **Draws.** The native draw reads the shadow state at draw time and hands it to the wgpu renderer. Command packets are never built, parsed or executed.
4. **The gate.** `recomp` reports every packet builder, MMIO function or GPU-import caller that recompiled code can still reach without passing through a replaced function (`generated/rust/gpu_reachable.txt`). Addresses stored in data or formed in code count as reachable from anywhere. `[gpu] enforce = true` turns the report into a build error once Phase 4 is complete.
5. **Order of work.** The replaced functions leave the command-buffer cursor null. Any recompiled D3D code that still tries to write commands then faults at the null page, and the report names the function. The next replacement target is therefore found at run time, in boot order.
