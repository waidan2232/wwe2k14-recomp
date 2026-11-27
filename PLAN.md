# WWE 2K14 → Rust/Bevy: plan for a static-recompilation port

> **Hard rule: absolutely no emulation.** §1 defines exactly what that means. It overrides everything else in this document.

The approach follows [hedge-dev/XenonRecomp](https://github.com/hedge-dev/XenonRecomp), the tool behind Unleashed Recompiled:

1. Translate the game's PowerPC machine code **ahead of time** into native code. Here the output is Rust instead of C++.
2. Replace every Xbox 360 OS and library service the game calls with a **native Rust implementation**.
3. Host everything in a **Bevy** app: window, input, audio output, and a wgpu renderer.

Nothing interprets, JITs, or emulates hardware at runtime.

---

## 0. What's on the disc (measured from `dump.iso`, 2026-11-23)

| | |
|---|---|
| Game | **WWE 2K14**. Title ID `545408B2`, original PE `WWE14_DVD_MASTER.exe`, Yuke's engine |
| Disc | XGD3, game partition at `0x2080000`, 714 files, 7.87 GiB |
| Executable | Single `default.xex` (23.5 MB). AES-encrypted with *basic* compression (no LZX). It decrypts cleanly with XenonRecomp's method; verified (MZ header present, 15 PE sections) |
| Image | Base `0x82000000`, 26.4 MiB. Code: `.text` 19.28 MiB, `BINK` 0.17 MiB, `RADCODE` |
| Code size | **~5.1 M PPC instructions in 58,508 `.pdata` functions** (covering 18.14 MiB). Largest function is 0xBC0C bytes, at `0x834B0D18` |
| Toolchain | XDK 2.0.21250. MSVC C1/C2 16.0.11886, LINK 10.0.11886. This is a newer compiler than Sonic Unleashed's, so jump-table patterns will differ |
| XDK static libs | D3D9, D3DX9, XGRAPHC, XAUDIO2, XAPOBA, XMCORE, XAPILIB, XNET, XONLINE, XMP, XPARTY, XHV2, LIBCPMT |
| Middleware (statically linked) | Havok (physics, behavior, cloth), Wwise (`AKPK` `.pck` banks), Bink 1 + Bink 2, Lua, zlib |
| Imports | **166 xboxkrnl functions + 12 variables, 104 XAM functions** (full list in Appendix A) |
| Vector/FP code | 181k VMX + 156k VMX128 instructions (~6.6% of code). 100k FPU instructions |
| Control flow | 336k `bl`. **35k `bctrl`** (virtual/indirect calls). 2,960 `bctr`. 1,102 `mtctr r0` (jump-table candidates) |
| Concurrency/system | 367 atomic sites (`lwarx/stwcx`), 1,547 `mftb`, 1,995 trap instructions, 56 functions with SEH handlers (`__C_specific_handler`, `RtlUnwind`) |
| ABI helpers | Each found exactly once by byte pattern: `__savegprlr_14` 0x82C78DC0, `__restgprlr_14` 0x82C78E10, `__savefpr_14` 0x82C79F00, `__restfpr_14` 0x82C79F4C, `__savevmx_14` 0x82C7A9D0, `__restvmx_14` 0x82C7AC68, `__savevmx_64` 0x82C7AA64, `__restvmx_64` 0x82C7ACFC |
| MMIO references | `lis 0x7FC8` ×8 (GPU registers) and `lis 0x7FEA` ×7 (audio/XMA block). Every one must sit inside a function we replace |
| Assets | `.pac` archives come in two containers: `EPAC` (plain) and a compressed `0F F5 12 ED` container (Yuke's). Movies: `.bik2` (Bink 2) plus one Bink 1 file (`title_logo`). Audio: Wwise `AKPK` packages (`sound/*.pck`, 1.1 GB). About 68 Xenos shader containers are embedded in the xex; more are inside the compressed `.pac` files |
| Engine paths | `game:\`, `GAME:\`, `D:\`, `cache:\`, `dlc%d:\` |

**Machine prerequisites found:** Rust 1.99.1 (`x86_64-pc-windows-msvc`) is installed. **No MSVC linker or Windows SDK is installed, so nothing in Rust can link yet.** Python is not installed (not needed).

---

## 1. The rule: no emulation

| ✅ Allowed (this is porting) | ❌ Forbidden (this is emulation) |
|---|---|
| Ahead-of-time translation of every guest function to Rust | Interpreter, JIT, dynarec, or any runtime fallback for code we missed |
| Guest register context + big-endian 4 GiB guest address space (the translation ABI, same as XenonRecomp output) | Emulating Xenon CPU hardware: MMU/TLB, caches, interrupts, cycle timing |
| Native Rust implementations of xboxkrnl/XAM API contracts (the Wine model) | — |
| Replacing D3D9-Xbox library functions with a wgpu renderer | Parsing PM4 packets or the ring buffer, modelling Xenos registers or EDRAM |
| Offline Xenos shader → WGSL translation at build time | Translating or interpreting shader microcode at runtime |
| Replacing the audio API boundary with a host mixer and a software XMA2 decoder | Emulating the XMA hardware decoder or audio MMIO |
| Converting data at API boundaries (endianness, untiling, texture formats) | Running Xenia or any emulator in the build, tests, or runtime |
| The game's own Lua VM and Bink decoder running as recompiled game code | — |

**Failure policy.** When the port meets something it can't handle, it **stops with a precise diagnostic** and we fix it statically. Examples: an indirect call to an address that isn't in the function table, an import we haven't implemented, an access to the MMIO range, or a shader that wasn't translated. It never falls back to emulating.

**Enforcement is built into the architecture:**
- The function table contains only recompiled functions and native hooks. A lookup miss is fatal and reports the guest address and caller.
- The MMIO range `0x7FC00000–0x7FFFFFFF` is reserved **no-access** in the guest address space. Any stray hardware poke crashes with the guest function name.
- There is no code path that reads guest instructions at runtime. Guest code bytes stay mapped only because the game reads its own image as data (relocation tables, `RtlImageXexHeaderField`, etc.).
- `CLAUDE.md` carries the rule for every future session.

---

## 2. Architecture

```
 BUILD TIME (offline tools)                          RUN TIME (one native .exe)
 dump.iso ──► disc ──► private/game/ ─────────────►  VFS: game:\ D:\ cache:\ saves
 default.xex ──► xex ──► analyse ──► recomp ──Rust─►  generated/ (58.5k recompiled fns)
                                                        │ direct calls (link-time symbols)
 shader blobs ──► xenos-shader-recomp ──WGSL──┐         │ indirect calls (function table)
 (xex + decompressed .pac + dev miss-logs)    │         ▼
                                              └──►  runtime: ppc-rt · kernel · xam · gpu · audio
                                                        ▼
                                                    host (Bevy): window, gamepad, wgpu render node,
                                                                 cpal audio, overlay UI
```

### Cargo workspace layout

```
CLAUDE.md, PLAN.md, Cargo.toml (workspace), rust-toolchain.toml
config/      wwe14.toml (recompiler config), wwe14_switch_tables.toml, replacements.toml
tools/
  disc/                XDVDFS extractor (iso → private/game)
  xex/                 lib: XEX2 header, AES decrypt, basic + LZX decompress, XEXP patch, imports, PE
  ppc/                 lib: our own PPC + VMX + VMX128 decoder (no binutils/GPL code)
  analyse/             bin: function bounds, jump tables, ABI helpers, setjmp/longjmp, SEH/MMIO reports
  recomp/              bin: Rust code generator (shards + function table)
  pac/                 bin: Yuke's EPAC/compressed .pac reader (read-only; for shader/texture harvest)
  xenos-shader-recomp/ bin: Xenos microcode → WGSL + reflection metadata (naga-validated)
runtime/
  ppc-rt/              support code for generated code: PpcContext, BE memory ops, FPU/VMX helpers, fn table
  kernel/              xboxkrnl: threads, sync, memory, VFS, time, TLS, Rtl*, crypto, Vd* shims
  xam/                 input, content/saves, profile, XamShow*UI, offline network, notifications
  gpu/                 D3D9-Xbox replacement → per-frame render IR
  audio/               XAudio render-driver API + XMA boundary replacement → host mixer
  host/                Bevy plugins: boot, render node, input bridge, audio out (cpal), overlays
generated/   (gitignored) shard crates code-000 … code-NNN + fn_table crate
game/        bin crate: links host + runtime + generated shards
private/     (gitignored) dump.iso symlink, extracted game files, decrypted image, shader cache
```

### 2.1 Recompiler (XenonRecomp reimplemented in Rust, emitting Rust)

**`xex`:** port of `XenonUtils/xex.cpp`. It covers the header and optional headers, AES-128-CBC with the retail key, and basic and LZX decompression (the `lzxd` crate). It resolves import ordinals to names using the BSD-licensed export tables, which come from Xenia via XenonRecomp. Optional XEXP title-update patching is a port of `xex_patcher.cpp`. **Already proven on this disc** by the probe.

**`ppc` decoder:** written from the PowerPC/VMX/VMX128 specs, table-driven. A coverage tool must show that every word in the code sections either decodes or is classified as data (padding, in-text jump tables).

**`analyse`:**
- **Functions** come from four sources: `.pdata` (58,508), `bl` targets, code pointers found in `.rdata`/`.data` (vtables, callbacks), and manual entries in config.
- **Jump tables:** port XenonAnalyse's patterns and add the MSVC 16.0.11886 variants. **Every one of the 2,960 `bctr` sites must be classified** as a jump table, a tail call through CTR, or a computed call. Any unclassified site is a build error.
- **ABI helpers:** detected automatically (the byte patterns above already work).
- **Lists:** `setjmp`/`longjmp` (callers of `RtlUnwind`), the 56 SEH functions, and every MMIO-touching function.

**`recomp`** generates Rust:
- **Function shape.** Each guest function becomes `#[no_mangle] pub unsafe extern "C" fn sub_XXXXXXXX(ctx: &mut PpcContext, base: *mut u8)`.
- **Control flow.** Rust has no `goto`. Phase 2 emits a **block-dispatch** form, `loop { match block { … } }`. It is correct for any control-flow graph, and LLVM jump-threads the constant block assignments into direct branches. A structurizer (relooper/stackifier) comes later to cut compile time.
- **Cross-shard calls.** Shards declare each other's functions in `extern "C"` blocks, resolved at link time. Shards depend only on `ppc-rt`, which avoids crate cycles.
- **Indirect calls.** A lookup table maps guest address to host function, the same idea as XenonRecomp's `PPC_LOOKUP_FUNC` region past the image. A miss is fatal.
- **Memory.** Big-endian load/store helpers. Spin-wait loops need volatile-like accesses (XenonRecomp marks every access volatile). Use `read_volatile`/`write_volatile` through an align-1 `#[repr(packed)]` wrapper. *Phase 2 spike: confirm this compiles to a single `mov` on x86-64.*
- **FPU.** `f64::mul_add` for fused ops. This needs the baseline `-C target-cpu=x86-64-v3` (AVX2 + FMA). Single-precision results are rounded to f32. CR, XER, and the FPSCR rounding modes are implemented exactly.
- **VMX/VMX128.** `core::arch::x86_64` SSE4.1/AVX2 intrinsics, using XenonRecomp's whole-vector reversal convention. Denormal flushing is done by switching MXCSR through `asm!`, toggling only when the mode changes. *Known LLVM caveat; covered by tests.*
- **Atomics.** `lwarx`/`stwcx.` become reserved value + `AtomicU32::compare_exchange`, as XenonRecomp does. `sync`/`lwsync`/`eieio` become fences.
- **Cache and time instructions.** `dcbz`/`dcbz128` zero 32/128 bytes (real semantics; used by memset). `dcbt`/`dcbf` become hints or no-ops. `mftb` reads the host monotonic clock scaled to 49.875 MHz.
- **Traps.** `twi 31` is a guest assert: fatal, with the guest address. Conditional traps become checked conditions.
- **setjmp/longjmp.** Rust can't use `setjmp`. Guest `longjmp` raises a Rust panic carrying `(jmp_buf, value)`. Each function that contains a `setjmp` call site wraps its dispatch loop in `catch_unwind`, restores registers, and resumes at the block after the call. The dispatch form is what makes this possible. Lua's error path is the likely user. Build with `panic = "unwind"`.
- **Hooks.** A `[[replace]]` config entry maps a guest address to a Rust function. The generated symbol becomes a thin call to the native code, and the original body stays available as `__imp__sub_X`. Mid-asm hooks use XenonRecomp's TOML schema.
- **Sharding.** About 150–250 shard crates, sized by instruction count; a `game-code` crate links them all. *Targets: full build under 30 min on 16 cores, one-shard rebuild under 2 min. These are measured in the Phase 2 spike before further investment.*
- **Later optimizations.** XenonRecomp's register-as-local set (CTR, XER, CR, reserved, non-volatile, non-argument) and `skip_lr`, enabled only once the game runs correctly.

**Tests:**
- Per-instruction known-answer tests, covering CR/XER edge cases, saturation, NaN/denormal behavior, and VMX128 dot products, pack and unpack.
- XenonRecomp-style tests driven by Xenia's PPC test *vectors* (`.s` files with expected register values), assembled with a GNU PowerPC cross-assembler at build time only. No emulator runs.

### 2.2 Guest memory and ABI (runtime `ppc-rt` + `kernel`)
- **Address space.** Reserve 4 GiB of host address space. Map the decrypted image at `0x82000000` and zero-fill `.bss`. Heaps come from `NtAllocateVirtualMemory` and `MmAllocatePhysicalMemoryEx`.
- **Physical mirrors.** `0xA0000000`, `0xC0000000`, and `0xE0000000` map the *same* pages through OS file-mapping views (`CreateFileMapping`/`MapViewOfFileEx`). This is an OS feature, not emulation.
- **Reserved regions.** The MMIO range is reserved no-access. The function-table region sits after the image.
- **Per guest thread.** Each guest thread gets a `PpcContext` and a stack inside guest memory, because the game passes 32-bit pointers to locals. `r13` points at a per-thread PCR/TLS block laid out as the game expects (TLS slots, current thread, processor number).

### 2.3 Kernel and XAM (native implementations of the 270 imported functions)

| Group | Approach |
|---|---|
| Threads (`ExCreateThread`, affinity, priority, TLS, APC/DPC) | Host OS threads. Each gets its own guest context, stack, and PCR. Affinity is advisory |
| Sync (`Ke*Event`, `NtWaitFor*`, mutants, semaphores, timers, critical sections, spinlocks) | Guest dispatcher objects (`KEVENT`, etc.) live in guest memory. Map each guest object address to a host primitive, and keep a handle table for `Nt*` handles |
| Memory (`Nt*VirtualMemory`, `Mm*Physical*`, `MmQuery*`) | Allocators over the 4 GiB space; physical allocations go into the mirrored ranges |
| Files (`NtCreateFile`, `NtReadFile`, `NtQueryDirectoryFile`, scatter/gather) | VFS: `game:\` `GAME:\` `D:\` → `private/game`, `cache:\` → host cache dir. `StfsCreateDevice`/`XamContent*` → per-profile host save folders |
| Time (`KeQuerySystemTime`, perf counters, `KeTimeStampBundle`) | Host clocks. Performance frequency matches the 360's |
| Modules (`Xex*`, `RtlImageXexHeaderField`) | Answer from our parsed XEX header. `XexGetProcedureAddress` returns native thunks for known ordinals; anything else fails cleanly. Foreign code is never loaded |
| Rtl/strings/crypto (`XeCryptSha`, `XeKeys*Sign/Verify`) | Native. Signatures are always accepted or produced, so saves work |
| Video (`Vd*`) | D3D is replaced (§2.4), so the ring-buffer calls become inert. `VdSwap` maps to present. `VdSetGraphicsInterruptCallback` gets a host 60 Hz vblank thread that calls the guest callback |
| Audio (`XAudio*RenderDriver*`, `XMA*`) | See §2.5 |
| XAM input | Bevy gamepad/keyboard → `XINPUT_STATE`. Rumble goes to gilrs force feedback |
| XAM user/UI (`XamUser*`, `XamShow*UI`) | One offline local profile. Dialogs (keyboard, message box, device selector) are Bevy UI overlays or auto-answered |
| Network (`NetDll_*`, `XNet*`, QoS, sessions) | Offline: "no link" or clean errors. The online servers no longer exist |
| Loader (`XamLoaderLaunchTitle`, launch data) | Relaunch the process with the launch data |

Each import is **implemented** or **explicitly stubbed** with a documented return value. Calling one that isn't listed is fatal and names the import.

### 2.4 Graphics: replace D3D9-Xbox at its API boundary → Bevy/wgpu

1. **Find the library.** Locate D3D9 21250 inside `.text`, starting from callers of the `Vd*` imports, then the contiguous library region and its call graph. Recover the `D3DDevice` structure: shadow shader constants, dirty flags, and the per-render-state / per-sampler-state function-pointer tables. Xbox D3D inlines a lot into game code, so the game writes that state directly. Use Ghidra with an XEX loader for static analysis only.
2. **Replace the entry points in Rust.** This covers device creation, textures/surfaces/vertex and index buffers, the non-inlined state setters and the state-function tables, draws, `Resolve`, `Swap`/`Present`, predicated tiling (`Begin/EndTiling`), and fences. The renderer reads the inlined shadow state from the guest device struct at draw time.
   *(Refined in Phase 4; see `docs/d3d.md`.)*
   - **What changes.** D3D's CPU-only bookkeeping stays recompiled: state setters writing the shadow, resource headers, the device constructor. Only the functions that set up hardware or write commands are replaced.
   - **No command buffer.** The device never gets one, not even a throwaway buffer that is written and reset unread. Its write cursor stays null, so any command writer still reachable faults and names itself.
   - **Static gate.** `recomp`'s GPU gate lists every hardware-facing function still reachable without a replacement.
3. **Render IR.** Each frame becomes a list of commands (pipeline key, bindings, guest buffer ranges, draw parameters), sent over a channel to Bevy's render world. A custom render-graph node executes it with wgpu. The result is drawn into Bevy's camera target, so Bevy UI can draw overlays on top. Pipelines are cached and precompiled from recorded keys to avoid hitches.
4. **Data at the boundary:**
   - Vertices use **vertex pulling**. Raw big-endian guest bytes go into storage buffers, and the translated shaders contain the fetch and byte-swap logic.
   - Indices are byte-swapped on upload.
   - Textures are **untiled, byte-swapped, and format-mapped** on upload. Mappings: DXT1/3/5 → BC1/2/3, DXN → BC5, CTX1 → RG8/BC5, DXT3A/5A → BC4, 8888, 2_10_10_10, 16F.
   - Caches are keyed by guest address + format + content hash. OS write-watch is an optional dirty-tracking speedup.
5. **Render targets.** Real wgpu textures. `Resolve` becomes a texture copy, plus a guest-memory readback only where the CPU actually reads the result. Predicated tiling becomes a single full draw. MSAA is native.
6. **Shaders, offline only.** `xenos-shader-recomp` translates Xenos microcode to WGSL plus metadata (constants, fetch layouts, samplers). Harvest from three places:
   - the xex image (~68 containers found);
   - all `.pac` archives, after decompression with a Rust reader for Yuke's container (reverse-engineered from the game's own decompressor);
   - miss-logs from dev builds.

   A missing shader at runtime is logged and its draw is skipped in dev builds. It is never translated at runtime.
7. **Bink 1/2.** Runs as recompiled game code; its GPU work goes through our D3D layer.

### 2.5 Audio

On the 360 the chain is: **Wwise → XAudio2 (static library, software mixer on the CPU) → `XAudioSubmitRenderDriverFrame` → speakers.** Compressed XMA voices are decoded by the XMA hardware block (`XMACreateContext` plus the `0x7FEA` MMIO).

The port:
- Implement the **render-driver kernel API** natively. It receives the guest XAudio2 mixer's 256-sample frames and feeds a cpal stream at 48 kHz, downmixing 5.1 to the device layout.
- Find the **highest function boundary that hands compressed XMA packets to hardware**: Wwise's XMA source or XAudio2's XMA voice internals. Replace it with a **native software XMA2 decoder** that writes PCM into the same buffers. Start with FFmpeg's `xma2` decoder through bindings behind a trait, and consider a pure-Rust port later.
- After that, the `0x7FEA` MMIO sites are unreachable. The no-access MMIO mapping proves it.

### 2.6 Bevy host
- **Threads.** Bevy owns the main thread and the window (winit); the game runs on guest threads. `GuestPlugin` boots the image, starts the guest main thread at entry `0x82262638`, and handles shutdown.
- **Plugins.** `GpuBridgePlugin` is the render node. `InputBridgePlugin` and `AudioOutPlugin` (cpal) handle input and sound. `OverlayPlugin` provides the XamShow*UI replacements and a debug HUD: FPS, guest threads, shader misses.
- **Frame pacing.** `VdSwap` hands over a frame and blocks if the game is more than one frame ahead. A vblank callback thread runs at 60 Hz.
- **Versions.** Pin the newest stable Bevy at the start of Phase 0.

---

## 3. Phases (each one ends with a checkable exit gate)

| # | Phase | Main work | Exit gate | Size |
|---|---|---|---|---|
| 0 | Groundwork | Install VS Build Tools. Create the workspace skeleton, `.gitignore`, `disc` extractor, and `xex` crate | `disc extract` reproduces the 714-file tree. `xex info` reproduces §0's numbers (58,508 functions, imports, sections) | S |
| 1 | Decoder + analysis | `ppc` decoder, coverage report, function discovery, jump tables, helpers, setjmp/longjmp, SEH/MMIO lists, `config/wwe14.toml` | 100% of code words decoded or classified. **0 unclassified `bctr`.** Every code byte is owned by exactly one function | M |
| 2 | Recompiler + `ppc-rt` | Codegen for every instruction class, function table, hooks, setjmp/longjmp, sharding. **First a compile-time spike:** naive Rust for all 58,508 functions, timed full build | Full binary compiles and links. Instruction test suite passes. **No `todo!()` in generated code** (the build fails if any remain) | L |
| 3 | Boot to the graphics boundary | Guest memory, threads, sync, VFS, time, Xex/Rtl, XAM basics, stub inventory | Boots from entry through CRT init, threads and file loads, and reaches D3D's first GPU-facing call. Zero unimplemented-import hits on that path. *(Originally "the first `VdSwap`": that call is behind D3D device creation, which needs the GPU, so under the no-emulation rule it can only be reached once D3D is replaced. It moved to Phase 4.)* | L |
| 4 | Graphics | D3D reverse engineering, replacements, render IR → Bevy node, texture/format pipeline, `pac` reader, shader harvest + `xenos-shader-recomp` | First frame presented through the native D3D replacement. Logos, Bink intro, title screen and menus render correctly. Then a full exhibition match | XL |
| 5 | Audio | Render-driver API → cpal; XMA boundary replacement + software decoder | Menu music, crowd, and commentary play in sync with no underruns. The MMIO `0x7FEA` sites are never reached | M |
| 6 | Completeness | Play every mode: exhibition, 2K14 Showcase, Universe, Creation suite. Saves. Fix every fatal diagnostic statically | Each mode is playable start to finish. Saves round-trip | L |
| 7 | Performance | Register-as-local optimizations, profiling (Havok, skinning, Bink), native versions of hot leaf functions (memcpy/memset/string) | Steady 30/60 fps where the original held it, on a mid-range PC | M |
| 8 | Optional | Resolution scaling, widescreen/UI fixes, MSAA, keyboard/mouse, Linux/Steam Deck, mod loading | — | — |

Phases 4 and 5 can overlap once Phase 3 boots. Shader recompilation (4.6) and the `pac` reader can start as soon as Phase 1 is done.

---

## 4. Risks

| Risk | Mitigation |
|---|---|
| **Compile time and memory for ~5.1M instructions of generated Rust** | The Phase 2 spike measures this before anything else. Mitigations: shard crates, lean emitted code (no generics), structurizer, lower opt-level for cold shards, sccache |
| Inlined D3D writes and direct command-buffer use by the engine | Mirror the guest device struct exactly. If the engine writes command buffers directly, replace those engine functions one level higher |
| Where XMA enters hardware (Wwise vs XAudio2) | A Phase 5 investigation picks the highest boundary. Software decoding works either way |
| New compiler, new jump-table patterns | The build fails on any unclassified `bctr`, so nothing slips through silently |
| setjmp/longjmp and SEH in Rust | Unwind-based longjmp (§2.1). Audit the 56 SEH functions; each is either never triggered or natively replaced |
| FP environment (MXCSR) under LLVM | `asm!`-based switching plus targeted tests. Fallback: explicit software denormal flushing in the VMX helpers |
| Multithreading (Havok job system, spin-waits) | Volatile-equivalent memory ops, correct atomics and fences. On x86, TSO is stronger than PPC ordering |
| Dynamic vertex/texture data written by the CPU | Per-draw range uploads with hashing. OS write-watch as an optimization |
| Shader coverage | Three harvest sources (xex, decompressed `.pac`, dev miss-logs). Missing shaders are loud |

## 5. Out of scope
Online play (Xbox LIVE services are gone, so networking is offline-only), Kinect, avatar awards, marketplace and DLC purchases. DLC packages you already own could be mounted later through the content API.

## 6. Hygiene and legal
You supply your own disc. `private/` (disc, extracted files, decrypted image) and `generated/` (recompiled code) are never committed or published. Only the tools, the runtime, and the config go in the repo.

## 7. Next steps
1. **Install Visual Studio 2022 Build Tools** with "Desktop development with C++" and the Windows 11 SDK. Without them nothing in Rust can link on this machine.
2. Create the workspace skeleton, `rust-toolchain.toml`, `.gitignore` (`private/`, `generated/`, `target/`), and pin Bevy.
3. `tools/disc`: XDVDFS extraction into `private/game` (the probe already proved the format logic).
4. `tools/xex`: decrypt, decompress, PE sections, `.pdata`, imports (already proved by the probe).
5. `tools/ppc` decoder plus a coverage report over the 5.1M code words.
6. **Compile-time spike** (gate for Phase 2): emit naive Rust for all functions and time a full `cargo build --release`.

---

## Appendix A: imports the native runtime must provide

**xboxkrnl.exe: 166 functions.**
`__C_specific_handler, _snprintf, _vsnprintf, DbgBreakPoint, DbgPrint, ExAllocatePoolTypeWithTag, ExCreateThread, ExFreePool, ExGetXConfigSetting, ExRegisterTitleTerminateNotification, ExTerminateThread, FscSetCacheElementCount, HalReturnToFirmware, IoCheckShareAccess, IoCompleteRequest, IoCreateDevice, IoDeleteDevice, IoDismountVolume, IoDismountVolumeByFileHandle, IoInvalidDeviceRequest, IoRemoveShareAccess, IoSetShareAccess, KeAcquireSpinLockAtRaisedIrql, KeBugCheck, KeBugCheckEx, KeDelayExecutionThread, KeEnterCriticalRegion, KeGetCurrentProcessType, KeInitializeDpc, KeInsertQueueDpc, KeLeaveCriticalRegion, KeLockL2, KeQueryBasePriorityThread, KeQueryPerformanceFrequency, KeQuerySystemTime, KeReleaseSpinLockFromRaisedIrql, KeResetEvent, KeSetAffinityThread, KeSetBasePriorityThread, KeSetCurrentProcessType, KeSetEvent, KeTlsAlloc, KeTlsFree, KeTlsGetValue, KeTlsSetValue, KeUnlockL2, KeWaitForMultipleObjects, KeWaitForSingleObject, KfAcquireSpinLock, KfReleaseSpinLock, KiApcNormalRoutineNop, MmAllocatePhysicalMemoryEx, MmFreePhysicalMemory, MmGetPhysicalAddress, MmLockAndMapSegmentArray, MmMapIoSpace, MmQueryAddressProtect, MmQueryAllocationSize, MmUnlockAndUnmapSegmentArray, NtAllocateVirtualMemory, NtCancelIoFile, NtCancelTimer, NtClearEvent, NtClose, NtCreateEvent, NtCreateFile, NtCreateMutant, NtCreateSemaphore, NtCreateTimer, NtDeviceIoControlFile, NtDuplicateObject, NtFlushBuffersFile, NtFreeVirtualMemory, NtOpenFile, NtQueryDirectoryFile, NtQueryFullAttributesFile, NtQueryInformationFile, NtQueryVirtualMemory, NtQueryVolumeInformationFile, NtQueueApcThread, NtReadFile, NtReadFileScatter, NtReleaseMutant, NtReleaseSemaphore, NtResumeThread, NtSetEvent, NtSetInformationFile, NtSetTimerEx, NtWaitForMultipleObjectsEx, NtWaitForSingleObjectEx, NtWriteFile, NtWriteFileGather, NtYieldExecution, ObCreateSymbolicLink, ObDeleteSymbolicLink, ObDereferenceObject, ObIsTitleObject, ObReferenceObject, ObReferenceObjectByHandle, RtlCompareMemoryUlong, RtlCompareStringN, RtlEnterCriticalSection, RtlFillMemoryUlong, RtlFreeAnsiString, RtlImageXexHeaderField, RtlInitAnsiString, RtlInitializeCriticalSection, RtlInitializeCriticalSectionAndSpinCount, RtlInitUnicodeString, RtlLeaveCriticalSection, RtlMultiByteToUnicodeN, RtlNtStatusToDosError, RtlRaiseException, RtlTimeFieldsToTime, RtlTimeToTimeFields, RtlTryEnterCriticalSection, RtlUnicodeStringToAnsiString, RtlUnicodeToMultiByteN, RtlUnwind, RtlUpcaseUnicodeChar, sprintf, StfsControlDevice, StfsCreateDevice, VdCallGraphicsNotificationRoutines, VdEnableDisableClockGating, VdEnableRingBufferRPtrWriteBack, VdGetCurrentDisplayGamma, VdGetCurrentDisplayInformation, VdGetSystemCommandBuffer, VdInitializeEngines, VdInitializeRingBuffer, VdInitializeScalerCommandBuffer, VdIsHSIOTrainingSucceeded, VdPersistDisplay, VdQueryVideoFlags, VdQueryVideoMode, VdRetrainEDRAM, VdRetrainEDRAMWorker, VdSetDisplayMode, VdSetDisplayModeOverride, VdSetGraphicsInterruptCallback, VdSetSystemCommandBufferGpuIdentifierAddress, VdShutdownEngines, VdSwap, XAudioEnableDucker, XAudioGetDuckerAttackTime, XAudioGetDuckerHoldTime, XAudioGetDuckerLevel, XAudioGetDuckerReleaseTime, XAudioGetDuckerThreshold, XAudioGetSpeakerConfig, XAudioGetVoiceCategoryVolume, XAudioRegisterRenderDriverClient, XAudioSubmitRenderDriverFrame, XAudioUnregisterRenderDriverClient, XeCryptSha, XeKeysConsolePrivateKeySign, XeKeysConsoleSignatureVerification, XexCheckExecutablePrivilege, XexGetModuleHandle, XexGetProcedureAddress, XexLoadImage, XexLoadImageHeaders, XexUnloadImage, XMACreateContext, XMAReleaseContext`

**xboxkrnl.exe: 12 variables.**
`ExLoadedCommandLine, ExThreadObjectType, KeCertMonitorData, KeDebugMonitorData, KeTimeStampBundle, VdGlobalDevice, VdGlobalXamDevice, VdGpuClockInMHz, VdHSIOCalibrationLock, XboxHardwareInfo, XboxKrnlVersion, XexExecutableModuleHandle`

**xam.xex: 104 functions.**
`NetDll___WSAFDIsSet, NetDll_accept, NetDll_bind, NetDll_closesocket, NetDll_connect, NetDll_inet_addr, NetDll_ioctlsocket, NetDll_listen, NetDll_recv, NetDll_recvfrom, NetDll_select, NetDll_send, NetDll_sendto, NetDll_setsockopt, NetDll_shutdown, NetDll_socket, NetDll_WSACleanup, NetDll_WSAGetLastError, NetDll_WSAStartup, NetDll_XNetCleanup, NetDll_XNetConnect, NetDll_XNetCreateKey, NetDll_XNetGetConnectStatus, NetDll_XNetGetEthernetLinkStatus, NetDll_XNetGetTitleXnAddr, NetDll_XNetInAddrToString, NetDll_XNetInAddrToXnAddr, NetDll_XNetQosListen, NetDll_XNetQosLookup, NetDll_XNetQosRelease, NetDll_XNetQosServiceLookup, NetDll_XNetServerToInAddr, NetDll_XNetStartup, NetDll_XNetUnregisterInAddr, NetDll_XNetXnAddrToInAddr, XamAlloc, XamBackgroundDownloadItemGetHistoryStatus, XamBackgroundDownloadItemGetStatus, XamContentClose, XamContentCreateEnumerator, XamContentCreateEx, XamContentDelete, XamContentGetDeviceData, XamContentGetDeviceState, XamContentResolve, XamCreateEnumeratorHandle, XamEnumerate, XamFree, XamGetExecutionId, XamGetLanguage, XamGetPrivateEnumStructureFromHandle, XamGetSystemVersion, XamInputGetCapabilities, XamInputGetKeystrokeEx, XamInputGetState, XamInputSetState, XamLoaderGetLaunchData, XamLoaderGetLaunchDataSize, XamLoaderLaunchTitle, XamLoaderSetLaunchData, XamLoaderTerminateTitle, XamNotifyCreateListener, XamSessionCreateHandle, XamSessionRefObjByHandle, XamShowDeviceSelectorUI, XamShowDirtyDiscErrorUI, XamShowFriendsUI, XamShowGameInviteUI, XamShowGamerCardUIForXUID, XamShowKeyboardUI, XamShowMarketplaceUI, XamShowMessageBoxUI, XamShowMessageBoxUIEx, XamShowPlayerReviewUI, XamShowSigninUI, XamTaskCloseHandle, XamTaskSchedule, XamTaskShouldExit, XamUserAreUsersFriends, XamUserCheckPrivilege, XamUserCreateStatsEnumerator, XamUserGetDeviceContext, XamUserGetMembershipTierFromXUID, XamUserGetName, XamUserGetOnlineCountryFromXUID, XamUserGetSigninInfo, XamUserGetSigninState, XamUserGetXUID, XamUserReadProfileSettings, XamVoiceClose, XamVoiceCreate, XamVoiceHeadsetPresent, XamVoiceIsActiveProcess, XamVoiceSubmitPacket, XGetAVPack, XGetGameRegion, XGetVideoMode, XMsgCancelIORequest, XMsgInProcessCall, XMsgStartIORequest, XMsgStartIORequestEx, XNetLogonGetTitleID, XNotifyGetNext, XNotifyPositionUI`

## Appendix B: PE sections of the decrypted image

| Section | Range | Size | Kind |
|---|---|---|---|
| .rdata | 0x82000600–0x821D4CEC | 1.83 MiB | read-only |
| .pdata | 0x821D4E00–0x82247260 | 0.45 MiB | function table |
| BINKCONS | 0x82247400–0x8224B180 | 0.02 MiB | read-only |
| .text | 0x82250000–0x83598034 | 19.28 MiB | code |
| BINK | 0x83598200–0x835C2838 | 0.17 MiB | code |
| RADCODE | 0x835C2A00–0x835C34F4 | <0.01 MiB | code |
| BINKBSS | 0x835D0000–0x835D5FC4 | 0.02 MiB | bss |
| .data | 0x835D6000–0x839CC0F8 | 3.96 MiB | data |
| .tls | 0x839CC200–0x839CC629 | <0.01 MiB | TLS template |
| .XBMOVIE / BINKDATA / RADDATA | 0x839CC800–0x839CF624 | 0.01 MiB | data |
| .idata | 0x839D0000–0x839D04E0 | <0.01 MiB | import thunks |
| .XBLD / .reloc | 0x839E0000–0x83B2A120 | 1.29 MiB | build info / relocations |
