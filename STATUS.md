# Status

The plan is in [PLAN.md](PLAN.md); this file records what's been built and verified. The no-emulation rule in [CLAUDE.md](CLAUDE.md) applies to everything here.

## Phase 0: groundwork (done 2026-11-23)

**Toolchain.** Visual Studio Build Tools 2026 is installed at `D:\VS\BuildTools` (MSVC 14.52, Windows SDK 10.0.26100). Rust 1.99.1 now links. Run `cargo` from PowerShell: Git Bash's `link` shadows the MSVC linker. Bevy is pinned at `=0.20.1` in the workspace and first used in Phase 3.

**`tools/disc`.** XDVDFS reader and extractor. It rejects unsafe names (traversal, reserved device names), writes through `.part` files, and checks every file's size after extracting.
- 568 game files (7.72 GiB) extracted to `private/game` in 87 s at 91 MiB/s.

**`tools/xex`.** XEX2 loader.
- **Header data:** optional headers, security info and page regions.
- **Image:** retail and devkit key decryption, basic decompression, PE sections and `.pdata`.
- **Imports:** resolved through Xenia's ordinal tables (data only).
- **Not yet supported:** LZX and delta (title-update) images return a clear `Unsupported` error. This disc doesn't need them.
- **Cross-check:** the decrypted image is byte-identical (same SHA-256) to an independent C# implementation's output.

**Exit gate: passed.** Real-data tests confirm the 714-file disc tree, plus the xex numbers: 15 sections, 58,508 `.pdata` functions, and imports of 166 + 12 from `xboxkrnl.exe` and 104 from `xam.xex`.

## Phase 1: decoder and analysis (done 2026-11-23)

**`tools/ppc`.** Our own decoder and disassembler for PowerPC, FPU, VMX and VMX128.
- **Generation:** `build.rs` generates the opcode table from Xenia's BSD-licensed instruction list (`data/ppc-instructions.xml`, 455 instructions). No binutils or GPL code is involved.
- **Form-aware operands:** the split 7-bit VMX128 register fields, swapped SPR halves, and MD/MDS shift and mask fields.

**Decode coverage over every code word.**

| Check | Result |
|---|---|
| Non-zero words decoded | 5,057,493 of 5,058,033 |
| Ambiguous encodings | 0 |
| Distinct opcodes used | 325 |
| VMX128 instructions | 267,328 |

The 540 words that don't decode are the 270 import thunks' placeholder records.

**`tools/analyse`.** A library, called directly by the recompiler, plus a CLI. A full pass takes under 1 s.
- **ABI helpers:** all 8 save/restore helpers found by byte pattern, each verified to end in `blr`.
- **Indirect branches:** all **2,960** unconditional `bctr` sites classified.

  | Kind | Count |
  |---|---|
  | Switch tables | 1,159 (absolute 1,090, byte-offset 33, computed 24, short-offset 12), 29,839 case labels |
  | Tail calls through a loaded pointer (vtables) | 1,524 |
  | Tail calls through a register | 274 |
  | Dispatch-table tail calls | 3 |

  - **How tables are found:** by backward data flow from each `bctr` (constant folding, opaque selector at the bounds check), not by fixed instruction sequences.
  - **Inferred bounds:** 7 tables whose bounds check sits in a predecessor block are bounded by the inline table's own extent.
- **Function discovery:** 83,363 functions.

  | Source | Count |
  |---|---|
  | `.pdata` | 58,502 |
  | ABI helpers | 8 |
  | Import thunks | 270 |
  | `bl` targets | 11,973 |
  | Sweep | 12,581 |
  | Secondary entry points | 29 (SEH `__finally` funclets called directly with `bl`) |

  - **Ownership:** every one of the 20,394,336 code bytes is owned exactly once by a function or top-level data.
  - **Embedded tables:** inline switch tables are recorded as embedded data inside their function.
  - **Two-entry routine:** one function (a stack-probe helper) deliberately falls through into the next. The recompiler emits a tail call for it.
- **Cross-check against data:**
  - 29,985 distinct code addresses are stored in data sections. All but 199 are function starts.
  - 129 of those 199 are SEH scope-table entries.
  - The other 70 look like coincidental data. They're listed so any runtime "no function at X" failure can be checked against them.
- **MMIO:** 19 functions load an address in the MMIO window (`generated/analysis/mmio_functions.tsv`). Each must be replaced natively or shown to be a false positive. *(Phase 4 update: 6 of them were float/double NaN and infinity constants (`0x7FC0`, `0x7FF0`) and other `0x7Fxx` values. The real hardware windows are `0x7FC8`–`0x7FCF` (GPU) and `0x7FEA` (audio): 13 functions, 8 in D3D and 5 in the XMA audio code.)*
  - **GPU registers (`0x7FC8`):** 8 functions in `0x82CB28F8`–`0x82CCAAF0`, the statically linked D3D9 library and the anchor for Phase 4.
  - **Audio/XMA block (`0x7FEA`):** 5 functions at `0x82D28xxx` and `0x8343xxxx`, for Phase 5.
  - **`0x7FF0`/`0x7FFx` hits:** probably floating-point constants such as the high word of +∞. Still to be confirmed.
- **`setjmp`/`longjmp`:** two `longjmp` candidates call `RtlUnwind`: `0x82C88B00` (CRT) and `0x83438A50`. The likely `setjmp` follows each.

**Exit gate: passed.**
- `analyse report private\game\default.xex generated\analysis` checks every criterion and exits non-zero on failure: 100% ownership, 0 overlaps, 0 unclassified `bctr`, 0 undecodable or invalid code, every switch inside its own function, and 0 problems.
- The `wwe14_analysis` test asserts the same against the real executable.

**Change from the plan.** No hand-written `config/wwe14.toml` is needed for function bounds or switch tables. The analysis is automatic and deterministic, and the recompiler calls it directly. A config file will come in Phase 2 for things that are genuinely manual: native replacements, mid-asm hooks and overrides. Review exports go to `generated/analysis/` (gitignored, regenerated in under a second).

## Phase 2: compile-time spike (done 2026-11-23)

The gating risk was whether about 5 million instructions of generated Rust could build in reasonable time. **They can.**

**`tools/recomp`** generates a separate Cargo workspace in `generated/rust`.
- **Input to output:** 83,321 functions and 5,038,620 instructions become 249 shard crates holding 326.6 MiB of Rust, generated in 3–7 s.
- **Function shape:** `#[unsafe(no_mangle)] unsafe extern "C" fn sub_XXXXXXXX(c: &mut Ctx, b: *mut u8)`. Straight-line functions are plain code; the rest use a block-dispatch `loop { match pc { .. } }`.
- **Cross-shard calls** resolve at link time through `extern "C"` declarations.
- **Indirect calls** go through a sorted function table (`ppc_rt::call_indirect`). A miss is fatal.
- **Switches** become `match` on the selector register.
- **ABI helpers** become one function per entry point.
- **Fall-through:** the single fall-through function becomes a tail call.
- **Hard gate:** the recompiler refuses to run if the analysis reports any unclassified indirect branch or problem.
- **Write-if-changed:** files are written only when their content changes, so cargo rebuilds only what changed.

**`runtime/ppc-rt`** provides the support code.
- The register context. Vector registers are memory-order `u128`.
- Volatile big-endian guest memory access through an align-1 packed wrapper, so unaligned accesses are defined and still compile to single loads and stores.
- Compare-and-swap reservations for `lwarx`/`stwcx.`, mask and rotate helpers, and the time base at 49.875 MHz.
- Fatal diagnostics: `no_function`, `todo_instr`, `trap`, `unimplemented_import`. There is no fallback path anywhere.

**Coverage.** 95.6% of instructions are translated with real semantics: integer, loads and stores, branches, CR, FPU scalar, atomics, traps and vector loads/stores/bitwise. The remaining 4.43% (223,020) are emitted as `todo_instr`, almost all VMX/VMX128 vector math. In priority order:

| Opcode | Count |
|---|---|
| `vspltw128` | 37,301 |
| `vmaddfp` | 32,259 |
| `vmulfp128` | 30,898 |
| `vsubfp128` | 14,076 |
| `vpermwi128` | 11,877 |
| `vaddfp128` | 8,382 |
| `lvlx128` | 6,574 |
| `vmsum3fp128` | 6,109 |
| `vperm128` | 5,124 |

**Measured on Ryzen 7 5800 (8C/16T), 16 GB RAM, `opt-level=2`, `-j 12`.**

| Measure | Result | Target |
|---|---|---|
| Full build of all shards + table + link | 477–489 s (~8 min) | under 30 min |
| Peak rustc memory (16 shards at `-j 8`) | 2.4 GB | — |
| Linked `spike.exe` | 123.7 MiB; installs the 83,591-entry function table | — |
| No-change rebuild | 0.6 s | — |
| One shard changed, including relink | 15.8 s | under 2 min |
| Warnings / errors | 0 / 0 | — |

## Phase 2: instruction semantics (done 2026-11-23)

**Every instruction in the game is translated.** 0 of 5,038,620 instructions are emitted as `todo_instr` (down from 223,020). Each translation is checked against known answers.

**Assembler (`ppc::asm`).** The inverse of the disassembler, driven by the same table. It handles the extended mnemonics (`li`, `slwi`, `beq cr6`, `twgti`, ...) and checks every word it produces by decoding it again.
- **Round-trip test:** every one of the game's 5,057,161 instructions is disassembled and reassembled to the identical word (`assembler_round_trips_every_instruction`).
- **Decoder fixes it found** (vendored instruction table, see its header comment):
  - `mfocrf` (1,690 uses) was read as `mfcr`.
  - `vupkhsh128`/`vupklsh128` (187 uses) were read as their byte variants.
  - `lwsync` (266) and the `mtmsrd` L bit (367) weren't shown.
  - `twi` printed as `tw`.

**Known-answer tests (`tools/kat`).** A build script assembles test programs and translates them with the game's emitter (`recomp` is now also a library). The test runs the compiled functions on a 4 GiB guest address space and compares registers, CR and memory.
- **Xenia's vectors:** 1,480 tests in 165 files (`data/xenia`, BSD-3). These are static data only; no Xenia code runs.
- **Our own:** 31 tests (`data/local`) for what those don't cover: VMX128 registers v64–v127, VMX128 multiply-add operand orders, `vperm128`'s 3-bit VC field, CR6 from `vcmp*128.`, denormal flushing, FPSCR/MSR/CR ops, and setjmp/longjmp.
- **Result: 1,511 / 1,511 pass.**

**Semantics bugs the tests found and fixed:**
- **CR0 width.** The 360 runs code in 32-bit mode, so every record form sets CR0 from the low 32 bits, doubleword instructions included (`divdu.` of 0xFFFFFFFF sets LT).
- **`addic.`** didn't update CR0: it's a D-form opcode with no record bit.
- **`divw`** leaves the high word zero.
- **`fctiwz`** sign-extends its result, and NaN gives 0x80000000.
- **`bdnz`/`bdz`** test the low 32 bits of CTR.
- **`vmaddfp`/`vnmsubfp`** are not fused: the product is rounded first. The vectors expect `1.1*1.1+1.1` = 0x4013D70A. The scalar `fmadd` family stays fused.

**VMX/VMX128 (`ppc_rt::vmx`).** About 130 instructions, written as portable Rust over the 16-byte register.
- **Lane order:** host lane `i` = guest element `N-1-i`; elementwise ops ignore order.
- **Denormals:** handled as non-Java mode (denormal inputs and results become zero), done explicitly in software. The host MXCSR is never touched.
- **360 vertex formats:** `vpkd3d128`/`vupkd3d128` cover D3DCOLOR, SHORT2/4, 2:10:10:10, FLOAT16_2/4, with the clamping, NaN and placement the vectors define. Other format codes stay untranslated.

**Silent paths made loud.** Anything that would compute wrongly is untranslated or stops with a diagnostic:
- **FPU record forms (CR1)** and **`o` forms (XER[OV])** stay untranslated. The game uses none of them.
- **`mtfsf`** stops if the game asks for a rounding mode other than nearest, non-IEEE mode or FP exception traps.
- **`mtmsrd`/`mfmsr`** only store and read a context field. There are no guest interrupts to mask.

**setjmp/longjmp.** The game has one pair: `setjmp` at `0x83438D70` with 3 call sites, and `longjmp` at `0x83438A50` with 4.
- **Unwind instead of return.** `longjmp` runs as recompiled code, restoring r13–r31, f14–f31, v64–v127, CR, LR and r1. Its final `blr` becomes `longjmp_unwind`, a Rust unwind.
- **Catch at the setjmp caller.** Each function that calls `setjmp` runs its dispatch loop inside `catch_unwind`. It resumes after the call only when the unwind's stack pointer and target match its own activation; otherwise it keeps unwinding.
- **ABI change.** Recompiled functions now use the `extern "C-unwind"` ABI, so unwinds and fatal diagnostics can cross them.
- **Exhaustive check.** `analyse::stack_switch_returns` finds every `blr` that follows an `r1` load through another register. There is exactly one, `0x83438D38`. `recomp` refuses to run unless `config/wwe14.toml` lists exactly that set.
- **Not covered yet.** `0x82C88B00`, an earlier candidate, is the SEH runtime's `RtlUnwind` wrapper. It belongs with SEH and the `RtlUnwind` import in Phase 3.

**Full build with every instruction translated.**

| Measure | Result |
|---|---|
| Generated Rust | 249 shards, 328.2 MiB |
| Full build | 665 s (11 min, up from 8 min now that vector ops are real code) |
| Errors / warnings | 0 / 0 |
| `spike.exe` | 156.8 MiB; links and installs all 83,591 functions |

**`config/wwe14.toml`.** It holds the manual facts, so far only `[nonlocal]`. `recomp` parses it strictly: unknown keys are errors, and every entry is checked against the analysis.

**Not verified yet:**
- No game code has run. The first execution needs the Phase 3 loader and kernel.
- The known-answer tests compile with the release profile of the main workspace, not the generated workspace's profile.
- **Smaller accuracy gaps:**
  - The scalar single-precision ops round twice (to double, then to single).
  - `lfs` quiets signalling NaNs.
  - x86 produces a negative default NaN, where PowerPC produces a positive one.

## Phase 3: loader and kernel (done 2026-11-23)

**The game boots.** The recompiled code runs from the entry point, through CRT startup and every C++ static initializer, into the game's main thread. It sets up worker threads and loads its first files, then stops at D3D device creation (`VdInitializeEngines`). That is the graphics boundary: past it the D3D library drives the GPU's ring buffer and registers, so it must be replaced natively (Phase 4). No unimplemented import is hit on the way. The plan's gate said "the first `VdSwap`", but that call sits behind device creation, so it moved to Phase 4 (PLAN.md updated).

**`runtime/kernel`** is the native xboxkrnl/XAM. The recompiler now emits a `game` binary (`wwe14.exe`) that links every shard and calls `kernel::main`.
- **Imports.** Each import thunk becomes a stub calling `ppc_rt::call_import(index)`. At boot the kernel installs a native function for every import it implements, by library and name. Calling one it doesn't implement stops the game and names it. `--trace imports` logs every call with its arguments, result and caller.
- **Executable check.** `recomp` stores a hash of the decrypted image in the binary. The loader refuses any `default.xex` whose image doesn't match.
- **Loader.** It loads `default.xex` as data: the image at `0x82000000`, the import variables, a copy of the XEX header, and module entries (`LDR_DATA_TABLE_ENTRY`). Then it applies the page protections from the XEX (code and read-only pages become read-only).
- **Guest memory.** The 4 GiB space is reserved up front, and pages are committed only when the guest allocates them. The layout follows the console: 4 KB and 64 KB virtual heaps, stacks, kernel objects at `0x80000000`, the image, and three physical-memory views at `0xA0000000`/`0xC0000000`/`0xE0000000`.
  - **Loud faults.** The null page, the MMIO window and unallocated memory are never committed. Touching them is an access violation, which a vectored handler reports with the guest address, what lives there, the guest registers and the guest call stack.
- **Threads.** One host thread per guest thread, with its own register context, a guest stack, and the PCR/KTHREAD fields the game reads directly. `analyse r13` lists every PCR offset the game uses (`+0`, `+0x100`, `+0x10C`, `+0x150`). XAPI reads KTHREAD `+0x14C` (thread id) and `+0x160` (last error).
- **Synchronisation.** Dispatcher objects (events, semaphores, mutants, timers, threads) live in guest memory as on the console, under one dispatcher lock with host condition variables. This covers waits with timeouts, alertable waits that deliver APCs, critical sections (the NT algorithm on guest memory), spinlocks, a timer service, and DPCs on a kernel worker thread.
- **Files.** Devices map to host folders: `\Device\Cdrom0` is the extracted disc (read-only), and the hard-disk partitions are under `private\hdd`. `game:` and `d:` are symbolic links, as on the console.
- **Audio.** The render-driver API calls the game's XAudio2 back every 5.33 ms (256 samples at 48 kHz) and accepts its frames. There is no host output yet (Phase 5).
- **Graphics boundary.** The GPU-facing `Vd*` calls (ring buffer, interrupts, `VdSwap`) stop with a message saying D3D must be replaced. Display queries (`VdQueryVideoMode` and similar) are answered.
- **ABI facts checked against the game's code** (new `analyse callers`, `refs`, `r13` commands):
  - stack arguments sit at `r1+0x54+8n`;
  - variadic arguments use 8-byte slots;
  - `ExCreateThread` takes the affinity in the top byte of its flags and `CREATE_SUSPENDED` in bit 0;
  - the `LDR_DATA_TABLE_ENTRY` field offsets;
  - `KeTimeStampBundle.TickCount` is at `+0x10`;
  - `XboxHardwareInfo` bit `0x10` means debug hardware.

**What the boot does** (`wwe14.exe --trace imports,threads,files --gpu-boundary park`):
1. CRT startup:
   - XAPI init: heap from `NtAllocateVirtualMemory`, TLS, privilege and region checks;
   - `XexLoadImage("xam.xex")`, then `XexGetProcedureAddress` for six XAM party exports;
   - all static initializers;
   - an XAM startup message (`XMsgStartIORequest(0xFE, 0x22005)`).
2. The game's main thread (`GameMainThreadEvent`):
   - it creates four workers with their priorities and affinities, and reads the time-zone XConfig settings;
   - it enumerates controllers: user 0 is connected, users 1–3 are not;
   - its memory system makes about 1,500 physical allocations.
3. A loader thread reads `plist360.arc` and `plist360.def`, and finds no title update (`UPDATE:` doesn't exist). It then loads `pac\_startup.pac` (3 MB) asynchronously, with a completion APC.
4. XAPI tries the utility drive: it opens the raw `\Device\Harddisk0\partition0` to read the cache-partition header at `0x800`. It isn't provided, so XAPI treats the cache as unavailable and carries on.
5. The main thread creates the D3D device: `VdInitializeEngines`, at the graphics boundary.

`--gpu-boundary park` (diagnostics) parks the thread that reached the GPU instead of stopping, so the other threads keep going. With it, nothing else is reached: every other thread ends up waiting on the main thread.

**Fixed while bringing it up:**
- **Procedure lookups.** `XexGetProcedureAddress` succeeds for any real export, as on the console. An export without an implementation gets an address that stops the game and names the export if it's ever called. CTR holds the target of an indirect call, which identifies it.
- **APC routines.** I/O APC routine addresses carry a flag in bit 0 (`ReadFileEx` does `ori r5, r5, 1`); the kernel clears it before calling.
- **Fatal reports.** A stray `longjmp` reaching a thread's top level is reported and exits; before, it could leave the process hanging.

**Numbers:**
- 178 of the 270 imports are implemented. The other 92 stop the game, naming the import, if called.
- Full rebuild after a `ppc-rt` change: 13 min, with about 2 GB of RAM free. A kernel-only change relinks in a few seconds.
- The kernel has unit tests for time conversion, SHA-1, wildcards and path resolution. The 1,511 known-answer tests still pass.

**Not verified yet:**
- **Untested paths:** SEH dispatch (`RtlRaiseException` beyond thread naming, `RtlUnwind`, `__C_specific_handler`), saves (`XamContent*`, STFS), XAM UI, and most XAM messages. Each stops with a diagnostic when reached.
- **Assumed values:** answers whose console values were inferred rather than observed:
  - the `0x22005` message (Xenia's answer, which later code treats as the ordinary mode);
  - XConfig values;
  - profile-setting defaults;
  - `XboxKrnlVersion` 2.0.17559.
- **Concurrency:** the dispatcher model has only run this boot's workload.

## Phase 4: replacing D3D (started 2026-11-23)

**Direct3D device creation now runs natively, and the boot gets past graphics initialisation.** It starts the game's render threads and its audio engine (Wwise and XAudio2, with the render driver registered), then stops at the XMA audio hardware. That is Phase 5's boundary.

**Map of the game's D3D** (`docs/d3d.md`; new `analyse gpu`, `fns` and `effects` commands):
- **Library and packet builders.** D3D is at `0x82CA4000`–`0x82CCB000`. Every function that builds Xenos command packets is inside it; the game's own code builds none.
- **API surface.** 134 entry points, 604 call sites, called mostly from the game's renderer at `0x82BA`–`0x82C2xxxx`.
- **Device.** 0x6080 bytes. 64-bit dirty masks at `+0x00..+0x28`, then shadow copies of the GPU registers (shader constants at `+0x780` and `+0x1780`, state from `+0x2934`). The command-buffer cursor is `+0x30`. State setters write the shadow and set dirty bits; the draw path flushes them.
- **MMIO, corrected.** The real hardware windows are `0x7FC8`–`0x7FCF` (GPU, 8 functions, all in D3D) and `0x7FEA` (XMA, 5 functions). Six of Phase 1's 19 "MMIO" functions were float/double NaN and infinity constants.

**Replacement mechanism.**
- **Config.** `[[replace]]` in `config/wwe14.toml` turns a function into a stub that calls `ppc_rt::call_replacement`. The original is kept as `__imp__sub_X`, so native code can still run part of it.
- **Providers.** Native providers register implementations by name. `runtime/gpu` is the first; the generated binary passes it to the kernel.
- **Missing implementations.** A replaced function without one stops the game and names it.

**The hardware gate.**
- **What it reports.** `recomp` reports every packet builder, MMIO function and GPU-import caller that recompiled code can still reach without passing through a replaced function, written to `gpu_reachable.txt`. Addresses stored in data (except `.pdata`/`.reloc`) or formed in code count as reachable from anywhere.
- **Now:** 96 such functions, 4 replaced, 89 still reachable.
- **Enforcing it.** `[gpu] enforce = true` will make this a build error once Phase 4 is complete.

**How D3D is being replaced:**
- **Stays recompiled:** the CPU-side bookkeeping, meaning state setters, resource headers and the device constructor.
- **Becomes native:** everything that sets up hardware or writes commands.
- **No command buffer.** The native device leaves the command cursor null, so any recompiled D3D code that still tries to write commands faults at guest address `0x4` and names itself.
- **Each replacement.** `analyse effects` shows what the faulting function stores on the device besides the cursor. The replacement makes exactly those stores.
- **Rejected shortcut.** A throwaway command buffer that D3D writes into and the runtime resets, without ever reading it, would let every setter run unmodified. It would also mean keeping a fake ring buffer, too close to what CLAUDE.md forbids.

**Replaced so far:**
- `D3DDevice_InitializeHardware` (`sub_82CC0DA0`): CPU-side steps only.
- `D3DDevice_WriteViewportScissor` (`sub_82CA8CD0`): nothing to do.
- `D3DDevice_Set2B24Bits` (`sub_82CBAEE0`): shadow word only.
- `D3DDevice_SetShaderAllocation` (`sub_82CB0F58`): shadow word, then D3D's own dirty marking.

**Kernel.**
- **`VdCallGraphicsNotificationRoutines`** is implemented: none are registered. It's a kernel callback list, not GPU hardware.
- **`VdIsHSIOTrainingSucceeded`** reports success.
- **`--gpu-boundary park`** now also parks a thread that touches MMIO.
- **Rebuild cost.** Each new replacement rebuilds one shard and relinks in about 17 s. The `ppc-rt` change for the mechanism cost one full rebuild (14 min).

**Not verified yet:** nothing has been rendered. The draw, clear, resolve and present path, resource locking and fences are untouched. The four native functions are checked only by the boot getting further, not against D3D's own output.

### Phase 4, continued: the game reaches its title screen

**Boot.** The game boots through its legal screen and the Bink intro to the **"PRESS START BUTTON" title screen**, running at 60 Hz. It loads its Wwise banks, strings, menus, fonts, the Lua scripts and wrestler data along the way.
- **New D3D replacements:** `D3DDevice_Swap`, `BeginVertices` and `EndVertices` (see `docs/d3d.md`). That makes 10 replacements; the gate reports 68 hardware-facing functions still reachable.
- **Kernel:**
  - XCTD file information (class 27);
  - XGI contexts, properties and achievements;
  - the XMP playback controller (the title controls music);
  - `XamPartyGetUserList` (not in a party);
  - a `--dump-threads N` diagnostic;
  - an `audio` trace;
  - scripted controller input, `--input BUTTON@SECONDS,...`.

**Texture pipeline (`runtime/gpu/src/texture.rs`, plan §2.4 item 4).** It decodes texture fetch constants, untiles 2D tiled surfaces and converts 8, 8_8, 8_8_8_8, 5_6_5, 4_4_4_4 and DXT1/3/5 to RGBA8. It is checked by unit tests and by the game's real textures (below).

**Shader harvest (plan §2.4 item 6, the "dev miss-log" source).**
1. The native draw calls D3D's own vertex-shader patcher, so the vertex-fetch instructions carry the real formats and strides.
2. `WWE14_SHADER_DUMP=<dir>` writes each new pair of bound shaders, keyed by hash, for the offline translator.
3. The title-screen path uses 3 vertex shaders and 5 pixel shaders (3 to 82 instructions).

The decoded control-flow, fetch and ALU fields match the dumps (for example `alloc colors; exec_end; export oC0 = max(r0, r0)`).

**Previews.** `WWE14_PREVIEW=<dir>` runs a CPU compositor for the game's inline 2D quads, using the decoded textures and Bink's YUV planes. It is a temporary development visualisation, not the renderer; colours are approximate because it doesn't run the shaders. It produced the first images: the legal screen, the intro and the title screen.

### Phase 4, continued: every game shader translated offline

**`tools/pac` (plan §2.4 item 6, the `.pac` reader).** All 367 archives decode (3.5 GB of content from 3.0 GB). Three layers:
- **XCompress** (`0x0FF512ED`, 334 files), which the XDK's file layer undoes transparently in the game (`sub_82C9FE60`, `sub_82CA2440`). The header's flags word gives the LZX window, the table entry count and width. Each entry is one chunk's uncompressed size, and each chunk owns one 128 KiB block of the file: either stored (a zero frame size) or an independent LZX stream of XMemCompress frames. LZX is the `lzxd` crate, with one fix around it: XMemCompress writes no realignment byte after an odd-length uncompressed block that ends a frame.
- **Yuke's containers:** `EPAC` (a directory of 4-character sections and 8-character names), `PACH` (count, then id/offset/size entries) and `EPK8`.
- **`BPE `**: Yuke's byte-pair encoding inside `PACH` entries. It is Philip Gage's BPE with a little-endian block length.

The game reads all of these itself through recompiled code. The reader exists so the build can find data offline.

**`tools/xenos-shader-recomp` (plan §2.4 item 6).** Microcode decoding and disassembly, and a WGSL emitter. Every output is validated with naga.
- **Harvest:**
  - **Pacs:** `scan` finds the XDK shader containers in every pac, inside the BPE streams. They come from `_startup.pac`, `CAB.pac`, `car.pac` and `edit.pac`: 1,935 containers, **1,440 unique shaders, all translated and valid**.
  - **Xex:** 67 shaders: 3 translate; the other 64 are the XDK WMV decoder's memory-export shaders. This game has no WMV or XMV files, so the renderer stops on them if one is ever bound.
  - **Runtime:** every shader the title screen uses at runtime matches one of the static ones.
- **Container layout:** magic, virtual size, physical size, and a shader record at `+0x18`. The microcode is at `virtual size + record[0]`, `record[1]` bytes.
- **Shader identity.** D3D rewrites each vertex fetch for the bound declaration at draw time: fetch constant, format, flags, swizzle, mini bit, stride and offset. This was found by diffing harvested shaders against their shipped originals. So:
  - A shader's identity (`ucode::identity`) hashes its microcode with those fields masked.
  - Translated vertex shaders decode vertices generically from a per-fetch descriptor: fetch constant, format word, stride and offset (`ucode::vertex_fetch_descriptor`). The renderer fills the descriptors from D3D's patched fetches.
  - Registers and predication stay compiled into the shader.
- **Translation:**
  - **Model:** Xenos registers (`r0`..`r63`, `a0`, `aL`, `p0`, the previous scalar) are WGSL locals. Shaders without jumps, calls or loops are straight-line code, and shaders with only forward jumps are forward-only blocks. Shaders with properly nested loops become structured WGSL `loop`s (`loop_pairs`, `emit_structured`). Anything else would become a control-flow state machine; since 2026-11-25 none of the game's 1,440 shaders does.
  - **Semantics:** D3D9 multiply semantics (0 × anything = 0), export write-mask constants, the `*sc` scalar ops, the cube op and cube fetches, `set_texture_lod`, register and explicit LODs.
  - **Checked by hand:** a screen-quad vertex shader and Bink's YUV→RGB shader. Both checks found real bugs (an operand swizzle applied twice, and negation of the `*sc` operands).
- **Tests:** synthetic unit tests cover identity masking, fetch descriptors, translation with validation, BPE and stored XCompress chunks.

### Phase 4, continued: the renderer, title screen and main menu

**Renderer (plan §2.4 item 3, §2.6; `docs/render.md`).**
- **Render IR (`runtime/gpu/src/ir.rs`).** The native draw path records each draw from D3D's shadow state:
  - shader identities, constants, fetch constants and vertex fetch descriptors;
  - the guest vertex bytes and decoded textures;
  - the render-state registers.

  `Swap` sends the frame over a channel. Each shader's literal constants (`def`) are loaded over the constant snapshot, as D3D does when it sets the shader. The container copy sits in D3D's shader objects (pixel `+0x28`, vertex `+0x368`), and the values at the start of the physical part.
- **Executor (`runtime/render`).** A wgpu 30 executor, the same wgpu version Bevy 0.20.1 uses:
  - it looks up translated shaders by identity and caches pipelines by shader pair, topology, blend and colour mask;
  - it maps viewport, blend, colour mask and sampler state;
  - quad lists and fans get generated indices.

  A shader with no translation is reported and its draws are skipped. So far none has been missing: millions of draws, 0 skipped.
- **Host (`runtime/host`).**
  - **Window:** Bevy owns the main thread and the window, and the guest boots on its own threads (`kernel::boot`). `GpuBridgePlugin` renders in Bevy's render world and copies the front buffer to a full-window sprite. Checked on Vulkan.
  - **Headless:** `--headless DIR` runs the same renderer offscreen and writes a GIF and PNG stills.
- **Capture cost:** textures are captured only for the fetch constants the bound shaders sample, and reused within a frame. The game holds 60 fps with the renderer attached.

**What renders correctly:**
- the legal screen;
- the Bink intro;
- the "PRESS START BUTTON" title with its animated effects;
- the save prompts;
- the main menu, reached by scripted input.

**Kernel: saves and the device selector.**
- `XamShowDeviceSelectorUI` picks the hard disk, as the console does with one device.
- `XamContentCreateEx`/`Close`/`Delete`/`GetDeviceState`/`GetDeviceData` mount content packages as folders under `private\hdd\partition1\Content\<xuid>\<title>\<type>\<name>`, using the root name the title chooses (`SaveDrive:`). The disposition is the low nibble of the flags. A missing package for "open existing" is `ERROR_PATH_NOT_FOUND`, which the game reads as "no save yet".
- **First boot:** the game offers to create a save, creates `SaveData.Dat` (3 MB), and loads it on the next boot.

### Phase 4, continued: toward a match

**Host input.** `GpuBridgePlugin` reads the first gamepad (an Xbox controller maps one to one) and the keyboard, in the WWE 2K14-era PC layout (README), and hands controller 1 to `XamInputGetState` (`kernel::set_pad`).

**New D3D replacements (`docs/d3d.md`).** Going from character select into a match stalled the game's render thread. With the command-buffer cursor null, D3D's space allocator (`sub_82CB57F0`) waits forever on the GPU's progress write-back, so each stall named the next command writer to replace:

| Replacement | What the native version does |
|---|---|
| `Resolve` | Recorded and copied between the renderer's textures |
| `Clear` | Recorded; the renderer clears the bound targets |
| `DrawIndexedVertices` | Index buffer read from guest memory, base vertex passed to wgpu |
| `BeginTiling`, `EndTiling` | Predicated tiling as one full pass |
| `SynchronizeToPresentationInterval` | Hi-Z bookkeeping and the pending flag; `Swap` paces the vblank |
| `InsertCallback` | GPU callbacks queued and run after `Swap` |
| `SetPredication` | the predication mask (tiles a draw runs in) and Hi-Z bookkeeping |

**Renderer: render targets (plan §2.4 item 5).**
- Render targets and depth buffers are wgpu textures, keyed by the surface D3D binds (EDRAM base, format, pitch from the shadowed `RB_*_INFO` registers).
- Consecutive draws share a pass. A resolve copies into a texture kept under the destination's address, which later draws sample instead of guest memory.
- The frame shows the texture resolved into the presented front buffer, or else the last target drawn.
- **Fixed-function state:** depth test and write, culling, alpha test (a translator epilogue with a uniform), and the game's reversed depth range (Z scale -1, offset 1, applied in the vertex shader).
- **Vertex data:** one guest-memory range per stream (up to 8), so streams far apart in memory don't make one huge upload.

**D3D reorders vertex fetches.** Its patch moves the fetches between slots and turns some into mini fetches. Shader identity now hashes the fetches as a set, and descriptors are matched to fetches by key, with mini fetches made full. Every vertex shader seen at run time maps to a static one.

**Textures.** The converter applies the fetch constant's component swizzle and decodes the formats the match loading screens use: DXN, DXT3A, DXT5A, CTX1, the `AS_16` variants and 16/32-bit float. Cube maps upload all six faces (each starts on a 4 KB boundary; unit-tested).

**Depth resolves.** A resolve of the depth buffer now runs a small copy shader of ours. It writes the depth values into an `R32Float` texture, which later draws sample. Headless mode requests float filtering, as Bevy's renderer already does.

**Fences were not the stall.** D3D's issued fence (`+0x2B1C`) and the completed one at `*(+0x2B10)` were always equal at `Swap`. `Swap` still marks every issued fence completed, and logs once if they ever differ.

**Match loading: XAM listeners are waitable.** Loading stopped after the wrestler packages (`CH139`, `CH187`). Wwise's event manager thread waits on three handles, one of them a XAM notification listener, about 125 times a second. Every wait failed (`C0000008`), because the kernel kept listeners as unwaitable private objects. On the console a listener is signalled while notifications are queued. Each listener is now backed by a notification event:
- set when a notification is queued;
- reset when `XNotifyGetNext` empties the queue.

Kernel-private objects (`Object::Other`) now carry an optional dispatcher header for this. The same fix made the game's audio non-silent: Wwise's event manager could now process events, and the render driver's frames carry sound (peaks 0.1–0.7).

**Thread affinity and priority are honoured.** `KeSetAffinityThread` and `KeSetBasePriorityThread` used to be recorded only, so every guest thread floated over all host cores. On the console each thread runs only on the hardware threads its affinity names, and on each hardware thread the highest-priority ready thread runs until it blocks. The game lays its threads out around this: for example, a priority-1 worker shares hardware thread 3 with the render thread. The kernel now gives the host scheduler the same contract:
- guest hardware thread n runs on one fixed host logical processor (the last six on hosts with at least eight);
- guest priority increments map to host priorities.

This is the OS API contract, implemented natively; nothing models CPU timing. Effects:
- menus went from 25–37 fps to 60;
- loading reaches the arena intro (2,000+ draws a frame), then the entrances.

**The 3D arena renders (in the main pass).** Four fixes got the arena's 3D scene into the render targets:
- **Predicated tiling.** The native `BeginTiling`/`EndTiling` now make the original's CPU-side state changes (tiles, screen extent, bound-surface snapshot, `+0x2B3C` bit 5, predication). While tiling, the surface is widened to the screen extent (`ir::tiled_surface_info`).
- **Viewport.** The console GPU clips at its guard band, not at the viewport, so the wgpu viewport is the whole target and D3D's scale and offset go into the position transform. (This entry first claimed the game draws through a 320-pixel viewport on purpose, with a projection reaching across the screen. It doesn't; see "The squish" below.)
- **Resolve addresses.** A resolve names its destination by CPU address (`0xF2CBB000`, in the 4 KB view), while texture fetches use the physical address (`0x12CBC000`). Resolved textures are now keyed by physical address. Before this, every resolved scene texture was missed and sampled as black.
- **Cube faces.** The environment cube is resolved face by face (`slice`) and sampled as a cube.

**The entrance renders.** Two more fixes put the 3D scenes on screen. The Undertaker's entrance (stage, titantron video, fog, the wrestler) now renders full-screen:
- **Clear flags.** D3D 360's `Clear` uses bits 0–3 for colour targets 0–3, `0x10` for depth and `0x20` for stencil (`sub_82CB4B00` drops bits 4–7 without a depth buffer). The renderer used PC D3D9's 1/2/4. So the game's depth-only clear after the final composite (`0x10`) erased every finished 3D frame.
- **Scalar exports.** An exporting ALU instruction has one destination, the export register in the vector-destination field; the scalar result goes there too, in the components its mask names. The translator sent it to the scalar-destination field instead. That lost the depth export of the EDRAM restore shader, and any scalar export to a non-zero export register. All 1,440 game shaders were re-translated, and there's a regression test.

Still wrong, in earlier analysis order:
- **The EDRAM restore draws all four tiles' quads in the left 320 pixels.** The game's restore pass (36 quads, `ps ede50d…`) puts each tile's quad at tile-relative position, and predication picks the tile. The one-pass renderer must offset per-tile draws by their tile's origin.
- **Restore shaders' depth export isn't translated.** The translator writes the scalar result to colour 0 instead of the depth export (`writes_depth: false`).

Debugging aids used: `WWE14_DRAW_LOG`, `WWE14_DUMP_TARGETS` and `WWE14_DUMP_OPS_DIR` (a PNG after every pass of the logged frame). The temporary `Debug_*` wrappers used then (`runtime/gpu/src/watch.rs`: the display-list recorder, the call-list writer, `SetViewport`, the texture-release queue) were removed on 2026-11-25.

**Skipping the arena intro crashes.** Pressing A during the arena intro makes the render thread call a cached display list (`object+0x350/0x354`, opcode 81) whose memory was already reused. It fails with the same bad pointer every time. Letting the intro play avoids it. To be fixed after the match renders.

**Root cause (2026-11-24): the render thread walks lists the main thread is re-recording.** By then the crash also happened without any skip, in about half the runs. The game picks the arena at random, so it does not reproduce on every run.

What the watches showed (`WWE14_WATCH_LIST`, `WWE14_WATCH_WORD`, `WWE14_WATCH_WALK`):
- Cached display lists live in an arena the renderer owns (`renderer+0xF8`: capacity, base, offset). The four draw functions `sub_82BD7B50`..`sub_82BD8A60` re-record a list when its slot (`object+0x350`..`+0x368`) is empty. A re-recorded list can start below the old one and run over it.
- The render thread signals the main thread `F8000028` ("start your next frame") and then walks the frame's lists (`sub_82BB5BE8`). Recording and walking overlap by design.
- The crashing list at `0xAF1264E0` had been re-recorded at `0xAF126120` by the main thread. The walk then reached a stale "call list `0xAF1264E0`". The new list's inline data had replaced the old header: the value `0x82C04A68` at `+8` is a return address copied in as data. The walker took it as a size and ran off the arena.

On the console the walk is short and finishes before the main thread re-records anything. Here every draw in the walk is captured, the walk is much longer, and it became longer still while the render thread waited on the renderer. That is why the crash came and went with capture speed.

Fix (`runtime/gpu/src/lists.rs`): recording a list (`sub_82BB4AE0`) waits while another thread is walking (`sub_82BB5BE8`). The walking thread itself is never held, and a wait over 2 s is logged instead of deadlocking. This restores the order the console's timing gave. Verified: three runs through the entrances into the match, one of them skipping an entrance, with no crash and no wait timeouts. Before the fix, four of seven runs had crashed.

**Draw capture is 3× cheaper.** The game's render thread records every draw (`gpu::ir`), and in the arena that took 51–57 ms of a 70 ms frame (`WWE14_CAPTURE_STATS=1` logs the costs every 120 frames). Most of it was hashing 23 MB of texture memory a frame, byte by byte, after copying it. Now:
- texture memory is hashed in place, eight bytes at a time in four lanes, and copied only when its hash is new;
- textures at addresses a resolve has written are skipped: the renderer samples the resolved image there anyway;
- each shader's microcode facts (identity, vertex fetches, sampled fetch constants) are kept per code hash;
- constants are read in one copy.

Capture now costs 15–20 ms a frame in the arena. With the render thread faster, the display-list crash at the arena transition (below) stopped showing up in these runs. It is a race in the game, so it isn't proven gone.

**Match load: physical allocations honour the range by returned address.** After the entrances, the main thread's pool setup (`sub_825116C0` → `sub_8258AC10`) asks for a 6.5 MB block (`0x680000`) inside a window of exactly that size (`0x1AF58000..0x1B5D8000`), with 4 KB pages. The kernel gave it, then the game freed it again and used the null it got back. The game's allocator (`sub_8225F650`) rejects any block whose `address & 0x1FFFFFFF` is below the window's minimum. In the 4 KB-page view (`0xE0000000`) that value is the physical address minus `0x1000`, so a block placed at the window's physical start always failed the check. On the console the check passes, so `MmAllocatePhysicalMemoryEx` applies its range and alignment to the address it returns (the view offset), not to the physical address. `mem::allocate_physical` now does that, and has a unit test.

Earlier theories that this rules out:
- The heaps `0x1F`/`0x20` windows (`0x17DD8000`, `0x18458000`) hold the character-select preview models for the whole match load. That is normal: they aren't the windows the pool setup asks for.
- The per-thread heap table (`0x8392F670`, 23 slots) is full 20 s after boot, with 23 long-lived threads. Threads unregister when their thread function returns (`sub_82C2DAE0(0, id)`). A thread without a slot uses, and sets, the global default heap (slot 0).

With the fix, one run played the entrances and went on into the match (about frame 14,700). It stopped presenting near frame 17,700 without a crash.

**The stall is a lost wake-up between the main thread and a job worker.** Frame-timed input reproduced it (no frames after about 12,600–14,400). The thread dump shows:
- the main thread waiting on a job's done event (`sub_8251D3F0`);
- the worker (`sub_8251D878`) waiting on its start event.

The kick (`sub_8251D1F0`) sets pending, signals start, then clears done. The worker clears start, runs the job and signals done. If the worker gets to run the whole job between the kick's "signal start" and "clear done", done is lost. Windows gives a thread woken by an event a temporary priority boost, so a worker sharing a processor with the main thread can preempt it right there. The console's scheduler has fixed priorities and no such boost. Guest threads now run with priority boosts disabled (`SetThreadPriorityBoost`). The same race didn't show with import tracing on, which changes the timing.

**A full exhibition match runs start to finish (Phase 4 gate).** With boosts off, the frame-timed input script reaches the RAW arena and the match plays to the end, with the WWE LIVE / #RAW broadcast graphics. The post-match screen (Exit / Rematch / Match Highlights) follows, and the game keeps presenting through frame 28,800. Rendering defects left in the match:
- the ring ropes are tangled during play (the post-match close-up draws them correctly);
- the winner glows orange on the post-match screen.

**The ropes come from the game's own rope simulation.** The ring ropes are vertex shader `c007f261`, one bone per vertex, 20 bones per rope. The rope bones the game sets (c76–c135) all share one rotation, but their translations swing about 260 units in height along a rope (y from −203 to +61) while barely moving along it (x stays between −2.4 and 4.8). So the renderer draws a rope the simulation has thrown into arcs. The ropes are calm in the post-match close-up and wild during play, which points at the simulation's step: the game runs at about 20–30 fps here (40–50 ms frames), against the 30–60 fps it was tuned for. This is performance work (Phase 7). Before that, check the solver for a CPU-side difference (VMX, FPSCR) if it persists at full speed.

Input that reaches it, frame-timed: `start@f1600,start@f1850,a@f2450,a@f2800`, then `a` every 300 frames from f5000 to f8300, then `rs@f8540,a@f8780`.

**A match renders: the attract-mode demo.** Left at the title screen, the game starts its demo match (frame ~10,000), with no input needed. It renders: ring, wrestlers, referee, crowd and commentary table in WrestleMania and Extreme Rules arenas. Still wrong there:
- the ring and the arena floor exist only in the first screen tile (x < 320 with four tiles, x < 640 with two); the floor elsewhere is flat green or white;
- the ropes are tangled.

**The squish: BeginTiling must leave a whole-screen viewport.** Every tiled 3D scene (the match, the demo, arena shots) was squeezed horizontally into the left 320 pixels, with off-camera geometry filling the rest. The cause: right after `BeginTiling`, the game reads the viewport back and sets it again with only the depth range changed (`sub_82C224A0`: `GetViewport`, `SetViewport`). D3D's own BeginTiling ends by setting the executable's whole-target viewport (`{0, 0, 65535, 65535, 0, 1}` at `0x82082430`), which `SetViewport` clips to the screen extent while tiling: 1280×720. It then calls `sub_82CAB868(device, 1)` (registers rewritten, state dirty, no packets). Our native BeginTiling skipped both. So the game read back the one-tile render target's 320×736, and the camera's normal 16:9 projection landed in 320 pixels. The native BeginTiling and EndTiling now end as the originals do, through D3D's own `SetViewport` (`sub_82CABBC8`). Checked on the demo: full-width, correct proportions.

**Why the ring "stopped at the first tile" (the other half).** Dumping the target after every draw (`WWE14_DUMP_EACH_DRAW`) also showed exploding geometry. Some wrestler draws (vertex shader `5f05be01…`) throw huge triangles across the screen, with near depth. Everything drawn later behind them fails the depth test: the stands, the barricade, and the ring beyond x≈320. What checked out along the way:
- blend indices stay within the 20 bones each draw sets (c76–c135);
- model-space positions are all finite and sane;
- no strip restarts;
- no D3D command buffers are recorded or run;
- reading vertex memory at `Swap` instead of at the draw call (`WWE14_LATE_VERTICES=1`) changes nothing.

**Shader CF opcodes 13/14 test a bool constant.** The translator decoded CF opcodes 13/14 (`CondExecPredClean`) as predicate tests, like 5/6. They test a bool constant (address bits 34–41, condition bit 42), like 3/4. The skinning shaders pick their four-bone path with them and never set the predicate, so our translation always took the wrong branch. 335 of the 1,440 game shaders change; a regression test covers it.

**Shader literal tables have 8-byte entries.** The rest of the explosion came from missing literals. A container's definition table lists 8-byte entries: (register << 16 | float count), then the byte offset of the values in the physical part, with empty entries as padding. Both the runtime (`ir::load_literals`) and the offline parser read 4-byte entries and stopped at the first zero word. So only the first literal run of each shader was loaded. `5f05be01` keeps its skinning constants in two runs (c240–c243, c252–c255), so c253.y, the `1 − x` for the fourth bone weight, read as 0. With both fixes the demo match renders coherently: wrestlers, referee, crowd, commentary table, the dark arena floor and the ramp. The ring ropes are still tangled. `WWE14_LOG_LITERALS=1` logs each container's table once.

**`SetPredication`, not `UpdateHiZ`.** `sub_82CB7770(device, mask)` writes a `SET_BIN_MASK_LO` packet (opcode `0x60`): it is D3D's `SetPredication`. `EndTiling` replays tile i with bins `3 << 2i`. The capture now records, for a predicated draw under tiling, the tiles it runs in (`ir::Tile`). The renderer draws it once per tile, clipped to the tile, and moves it to the tile's origin when the window offset doesn't apply to it (PA_SU_SC_MODE_CNTL bit 16). None of the demo's main-pass draws is predicated, so this isn't the floor problem.

**Run tooling.**
- `--input BUTTON@fFRAME` presses a button on a presented frame, held 9 frames. Frame times follow the game when it runs slower than real time, as the opening movies sometimes do. Each press is logged with its frame.
- A heartbeat `present #N` line every 1,800 frames.
- Headless stills every 10th capture (frames the renderer can't keep up with are dropped).
- `WWE14_DRAW_LOG=@N` logs the first frame received from frame N on. `WWE14_DUMP_OPS_DIR` now also dumps the last pass and resolve and one-draw passes. `WWE14_DUMP_EACH_DRAW=LO..HI` gives draws a pass each, to see the target after every draw.

## Phase 5: audio (started 2026-11-23)

**No XMA in this game's media.** Every sound in the Wwise packages is Wwise Vorbis (format tag `0xFFFF`): 25,267 in `Game_Sound_M.pck` and 13,904 in `Comm.pck`. `Game_Sound_E_S.pck` holds only bank structure. Vorbis is decoded in software by Wwise's recompiled code.

**XMA hardware boundary.** Two XMA hardware layers are linked in: XAudio2's (`0x82D28250`..) and the one Wwise uses (`0x8343A088`..). Both work through hardware contexts and registers, so they are never modelled and no software decoder sits under them. They touch the hardware without a context in two places, now native:
- the version reads (`sub_83439FB8`, `sub_82D28250`). Only that layer's own context creation reads the value;
- XAudio2's per-frame context sync (`sub_82D28AD8`). It verifies no hardware context exists, and stops the game otherwise.

`XMACreateContext` stops the game with an audio-boundary message. So if anything ever asks for XMA, that exact voice gets replaced with software decoding. A new `runtime/audio` crate holds these replacements.

**Render driver.** XAudio2 hands the kernel a frame 188 times a second, as on the console (256 samples × 6 channels, big-endian float, one block per channel). The frames stopped being silent once XAM listeners became waitable (Phase 4 notes).

**Output (2026-11-24).** Each frame is down-mixed to stereo: the fronts, plus centre and surrounds at −3 dB, with LFE dropped. It then goes to the host:
- **Windowed:** cpal plays it on the default output device. The device's clock paces the render driver: the game's callback runs whenever under 60 ms is queued (never waiting more than 100 ms, in case the device stops). That is how the console's audio hardware asks for frames. A device not at 48 kHz gets linear resampling. Checked opening the monitor's HDMI output (stereo, 48 kHz), with frames requested at 188 a second.
- **Headless with `--wav`:** a thread stands in for a device, takes 48,000 sample pairs a second, writes `DIR\audio.wav`, and logs underruns every ten seconds. From boot to the title screen, the recording is silent for the logos, then music: RMS 0.07–0.10, 3,000–4,000 zero crossings a second (noise would be about 24,000), no clipping.

**A full match with sound (headless, device clock).** Over the 14-minute exhibition run the audio never stops. In the match it rises to RMS 0.13–0.17, peaks 0.6–0.7. A one-minute mid-match clip went to the user to listen for crowd and commentary. The render-client thread is now time-critical: on the console the audio hardware's interrupt services it at once. Underruns:
- about 0.2 s in total, almost all one hiccup during a loading transition (8,322 samples at about 170 s);
- in the match, at most 164 samples (3 ms).

The hiccup looks like the game's own mixer pausing during the load. Not yet investigated.

## Renderer correctness and speed (2026-11-24)

**Every frame is rendered now.** The capture used to drop a frame whenever the renderer was two frames behind (`try_send`), and the windowed host rendered only the newest frame. In the last full match the game made about 35,000 frames and the renderer drew about 8,000. Dropping frames loses GPU work that later frames depend on: any texture a resolve writes in a dropped frame is never made. `end_frame` now waits for the renderer instead, as the game waits for the console's GPU when its command buffer is full, and the window renders every frame it receives, in order. It logs `the game waited N ms on the renderer` every 1,800 frames.

**The renderer keeps up (most of the time).** The profile of the render thread showed 92% of its time in the NVIDIA driver's shader compiler. Each new pipeline was compiled serially on the render thread, and the arena needs hundreds at once. Three changes:
- A frame's missing pipelines are built before its draws, in parallel on every core (`build_missing`). Shaders load in parallel first.
- The driver's compiled pipelines persist in `private/pipeline-cache.bin` (`wgpu::PipelineCache`, saved every 64 new pipelines). It is about 11 MB after the match intro. It derives from the game's shaders, so it stays in `private/`.
- Per-draw buffers are gone. Each draw used to create 9 buffers and 2 bind groups, and copied all its vertex memory. A frame now uploads three buffers:
  - the uniforms, at 256-byte dynamic offsets, with consecutive identical ones shared;
  - the vertex memory, each range once;
  - the indices.

  Texture bind groups are cached by pipeline, views and samplers.

Over 420 s of the match intro the game reached frame 15,930 with no frame dropped (previously, at the same speed, 73% were). It waited 15.5 s in one stretch: a few of the game's largest shaders take the driver over 10 s to compile, 12 pipelines in 11.8 s. Later runs load them from the cache.

**Missing textures fail loudly.** The renderer bound a 1×1 white texture, silently, for any sampled slot it had nothing for. It now names the slot, base and fetch words (`fetch constant N has no texture (white)`). The capture says why it didn't capture one: not a texture, no layout, or not in committed memory.

**Resolve destinations exist before the frame's draws bind them.** The renderer bound every draw's textures while walking the frame, but created a resolve's destination texture only when it encoded the resolve, after the walk. A texture first made by a resolve and sampled later in the same frame came out white in that frame. Destinations are now made during the walk.

**Resolved, then rewritten by the CPU.** The capture skipped any address a resolve had ever written, forever. It now keeps a fingerprint of the guest memory there (64 bytes at the start and middle of each page). If the CPU rewrites it, as when a texture loads into memory a render target used, the CPU's texels are captured again. The renderer prefers a captured texture over a resolved image.

**Texture number conversion.** Textures were all converted to 8-bit unsigned fractions. The fetch constant's per-channel sign mode (unsigned, signed, biased, gamma), its exponent adjust and its integer flag were ignored, and float formats were clamped to 0..1. Fetches that need any of these now convert to RGBA 16-bit floats:
- signed fractions, clamped to −1;
- biased: 2x − 1;
- the piecewise-linear gamma ramp, inverted;
- signed DXN/DXT5A endpoints.

The cache key now includes these fields and the swizzle, which it lacked before. The match's wrestler textures are all unsigned with no exponent adjust, so this is not the skin defect.

**Orange skin: the scalar constant ops read the wrong temporary.** In close-ups (the entrances, the post-match screen) skin came out flat orange-red with a glow. The search took four steps:
- The per-draw dump (`WWE14_DUMP_EACH_DRAW`) showed the face is drawn by `ps a9c88138`.
- New override switches showed the albedo, the cube map, the light maps, the thickness buffer and the clamp constant were not the cause. Replacing c155 (the shader's deep-red subsurface colour) with c158 (the skin tone) removed the orange. The switches are `WWE14_DEBUG_SLOTS=F1-F2:slot=r,g,b,a@ps` (a constant texture) and `WWE14_DEBUG_CONSTS=F1-F2:N=x,y,z,w@ps` (a pixel shader constant).
- So the shader's `mix(c158, c155, w)` had a weight near 1. That weight comes from `mulsc1 r15.w, c199.x, r7.y`.
- The scalar ops that take a constant and a temporary (`mulsc`, `addsc` and `subsc`, opcodes 42–47) pack the temporary's index as bit 0 from the opcode, bit 1 from the src3 select bit, and bits 2–5 from the src3 swizzle. The translator put the select bit at bit 6 and read it as "relative to the loop counter". Every such operand with the bit set read a different register: here `r5`, which the same instruction group was overwriting, instead of `r7`.

703 of the 1,440 game shaders change. The disassembler now prints these operands as constant and temporary (`mulsc1 r15.w, c199.x, r7.y`). A regression test covers this instruction.

**Tangled ropes: `vnmsubfp128` had its operands swapped.** The rope bones the game supplies are displacements from the rope's rest shape: the rope's length is in its vertices. All 12 ropes were displaced by hundreds of units at once, including ropes nobody touched, so the simulation was unstable. Its arithmetic was suspect.

The game normalizes vectors with a reciprocal-square-root estimate and one Newton step. It writes `v10 = 0.5`, then `vnmsubfp128 v10, x/2, y0*y0`, then `y0 + y0 * v10`. Only `vD = -(vA * vB - vD)` makes that the Newton step `0.5 - x/2 * y0^2`, with vD the accumulator as in `vmaddfp128`. The emitter computed `-(vA * vD - vB)`, which normalized every such vector wrongly. Our own known-answer vector had encoded the same assumption; it now checks the corrected order. Verified: a full exhibition match (Money in the Bank arena, run without skipping entrances) has straight ropes throughout. It ends in a pinfall and reaches the post-match screen.

**Jagged, haloed edges: MSAA and pixel centres.** Model outlines were stair-stepped, with a light blocky fringe against blurred backgrounds. Both fixes apply to every scene:
- **MSAA.** The 3D passes are 4× multisampled (`RB_SURFACE_INFO` bits 16–17). Surfaces now get the console's sample count where the format allows it: 2× is drawn as 4×, and `WWE14_MSAA=0` turns it off. Pipelines and clears match the count, and alpha-to-mask maps to alpha-to-coverage. Resolves average the samples in a shader of ours; depth resolves take sample 0. Debug readbacks resolve first.
- **Pixel centres.** The game sets `PA_SU_VTX_CNTL` = 0, Direct3D 9's pixel centres at whole coordinates. Every draw was half a pixel off the host's, so post-processing sampled between texels. Window coordinates now move by half a pixel. Measured on the character-select screen: edge contrast of menu text rises 2–3% in three regions, as correctly aligned texels should.

Texture-fetch instructions' filter overrides (word 1 bits 12–17) are now decoded and listed in a shader's reflection (`// tfetch-filters:`). No game shader uses them, so the renderer does not apply them yet.

**Reported by the user (2026-11-24, windowed build): the game runs in slow motion at times, menus included.** Measured and partly fixed (Phase 7).

**The cause: frame-locked logic below its frame rate.** The game advances one step per frame and sizes its timers as frame rate × seconds. So a frame rate below the target is slow motion, not choppiness.
- Measured: uncapped at 150–246 fps (`WWE14_FPS_CAP=off@N`), the character-select roulette still settles in about 100 frames, as at 60. Unlimited fps would therefore fast-forward the game.
- The game's frame-rate mode is `sub_82BB6BB0(fps)`: 60, 50, 30 or 25. It writes the struct at `0x8392DC38`: the rate at `+8`, the rate as a float at `+0x20`, the per-frame step at `+0x24` and ms per frame at `+0x28`. `sub_82BA84E8` turns the rate into D3D's presentation interval: two blanks for 30 and 25, one otherwise.
- Menus, character select and matches use 60. Entrances run at half rate: when one starts, the scene manager (`sub_8231C9F0`) calls `sub_82331628(1)`, which calls the half-rate switch `sub_82BB6D50` (corrected 2026-11-25; this said loading screens only).
- The struct's other fields (2026-11-25): `+0x1C` seconds per frame, `+0x24` ms per frame (float), `+0x28` ms per frame (integer), `+0x3C` 60 Hz frames per tick (1.0 at 60, 2.0 at 30) and `+0x40` its inverse. About 400 functions read `+0x3C`, so the game's clocks count in 60 Hz frames and advance by 2 a tick at 30 fps.
- **The 60 fps lock (the user's choice):** two hooks keep the game at 60 through its own setters. `Game_SetFrameRate` turns every request into 60. `Game_HalveFrameRate` (`sub_82BB6D50`) is a no-op; it is the game's switch from 60 to 30 for loading screens, and `sub_82BB6DF0` switches back. `WWE14_FRAME_RATE=native` keeps the game's choices. `Swap` now honours the presentation interval (`+0x36B0`) instead of always waiting one blank. It does the frame's closing work before the wait, so the wait absorbs it.

**Where the time went:** the game's render thread, which walks the render lists and has every draw captured. The always-on `perf:` log now splits a frame into walk, capture, vblank wait, list-recording waits and renderer waits. In the arena (800–3,800 draws) the walk took 23–54 ms a frame, of which capture was 13–38 ms. So matches ran at 17–30 fps against a 60 target. Capture fixes:
- **Textures** (5–22 ms): every sampled texture was hashed in full, draw by draw, on the render thread, and converted there when new. The cache cleared itself at 4,096 entries, so video textures (a new entry each frame) triggered a full re-conversion of every texture every minute or so: the 20 ms spikes. Textures are now read when the frame ends, on up to 8 threads. The cache holds one entry per address and layout, and a changed texture replaces its own entry. The renderer's GPU texture cache likewise drops views unused for 10 s instead of clearing at 2,048.
- **Vertices** (3–8 ms, 7–23 MB a frame): a mesh drawn in several passes copied its vertex memory each time. Now each span is read once per frame, with a sampled fingerprint guarding reuse.
- **Constants** (3–11 ms): 512 registers were converted and allocated per draw. Now the previous draw's are reused when unchanged, only changed 4-register blocks are converted, and literals are read once per shader per frame.

Result, same arena: capture 7–8 ms, walk 12–13 ms, matches at 40–51 fps (from 17–30). The renderer takes 11–13 ms a frame, so it keeps up.

**Waiting, not working.** The profiler now finds the exe's PDB (it names itself without a directory) and counts threads parked in ntdll's wait stubs as waiting. With symbols, the render thread is busy only about 5 ms of its 12 ms walk, and no game thread is near saturation. Two fixes:
- **Timer resolution.** Guest timed waits and alertable sleeps wait on a host condition variable, which Windows rounds to its 15.6 ms tick unless the process asks for finer. The kernel now calls `timeBeginPeriod(1)` at boot.
- **Texture hashing overlaps the walk.** A few `texture hasher` threads hash each texture as soon as a draw names it, and `Swap` only collects the results.

`WWE14_WAIT_STATS=1` logs every 5 s the time spent in waits and sleeps, by thread, caller and kind. Result: matches at 50–60 fps, mostly 55–59.

**The user's windowed test (2026-11-24): better, but not good enough.** Reported: random slowdowns, especially at camera cuts; crackling audio; titantron video that stutters or lags; a slow, laggy victory scene; entrances at 60 with random slowdowns. They want a constant 60 with no dips, and 1080p. Their log showed frames a little over 16.7 ms snapping to 33 ms (exactly 30 fps), and the window spinning at 100–300 updates a second. Changes since:
- **Frame pacing kept on average.** Frames are no longer snapped to vertical blanks. Each frame has a slot on a 60 Hz schedule: an early frame sleeps, a late one goes on, and the next frames catch up (`present.rs` `wait_for_vblank`). With frame-locked logic, the snapping had turned a 17 ms frame into half speed for that frame.
- **The window updates per game frame.** Bevy runs reactive: it wakes when the capture hands over a frame (`ir::on_frame`, through Bevy's event-loop proxy), or on input. Before, it updated as fast as it could, which took CPU and GPU from the game.
- **Targeted wakeups in the dispatcher.** Every object change used to wake every blocked guest thread (about 35) through one lock. Now each waiter has its own condition variable, and only waits the change may satisfy are woken.
- **Changing textures are read at the draw.** Hashing at the frame's end read video textures after the decoder had moved on. The ribbon board in the Royal Rumble arena showed white and black blocks; that was the end-of-frame read. Textures whose contents changed in the last 120 frames are now copied at the draw. The others are hashed off-thread, and their bytes are kept from that moment if they changed.
- **Conversion off the game's thread.** Converting changed textures took a fifth of the game's render thread. Converter threads now fill `Texture::pixels` while the renderer works on earlier frames.
- **One GPU texture per texture slot.** A video texture used to make a new GPU texture per frame, so thousands were live. wgpu's per-pass texture tracking (`UsageScope` clear and resize, half of the renderer's `finish`) grows with that count. A slot whose contents change is now rewritten in place.

Measured headless: no audio underruns over 330 s (`--wav`), so the crackle is a windowed-build effect.

**The profiler unwinds stacks now** (`RtlVirtualUnwind` while the thread is stopped). Per thread it shows time by our innermost function, with waits marked, and inclusive time.

**Toward a constant 60 (2026-11-24, night).**
- **Pipeline warm-up.** Every pipeline the renderer builds is listed in `private/pipeline-keys.txt` (`WWE14_PIPELINE_KEYS`). At start, a background thread on half the cores rebuilds them from the driver's cache: 245 pipelines in 267 ms. Draws then rarely wait for a first-time compile (one stalled 839 ms before).
- **Leaner draw loop.** Pipeline, bind groups, viewport and scissor are set only when they change, and the frame's index buffer is bound once per pass (draws start at their list's first index). wgpu validates and replays every command when the encoder finishes.
- **Cheaper bookkeeping.** Finished frames are freed on a background thread (`render::drop_later`; about 8% of the renderer before). The renderer's maps use foldhash (SipHash showed in its profile). Index lists are no longer copied twice.
- **The post-match screen** (a wide arena shot, 2,172 draws) ran at 56–57 fps: walk 15.5 ms, renderer 16 ms, both just over budget. Copying only the constants a shader reads would not help: almost every vertex shader reads `c255`, and skinned ones index by `a0`.
- **Shader identification is cached by contents.** D3D patches a vertex shader's fetches in place for each vertex layout, so a shader alternates between versions draw by draw. The capture kept one version per address and re-identified on most draws: a quarter of the render thread's work in an entrance with about 4,000 draws a frame. It is now keyed by address, length and content hash. The capture's maps use foldhash.
- **mimalloc is the process's allocator** (in `host`, from the local crate cache). The renderer spent a fifth of its time freeing frames behind the Windows heap's lock, and the game thread showed `RtlSizeHeap`. Headless frees frames on the background thread too.
- **The warm-up also loads every translated shader** (about 1,500 modules) after the known pipelines. A pipeline never built before then waits only for the driver's compile, not for WGSL parsing and validation.
- **Guest threads float.** The console packs its six hardware threads on three cores, and the game pins about 35 threads to them. Mapped one to one onto host processors, one run's main menu dropped to 21 fps for 30 s while the game loaded, and starved the audio (1.1 s of underruns). A physical core per hardware thread fixed that, but the hitch watchdog then showed threads that were ready but not running: 15 ms sleeps took 53–82 ms, because two busy threads pinned to one processor wait a whole quantum for each other. Guest threads now ignore the game's affinity and keep its priorities; `WWE14_AFFINITY=pinned` brings back the physical-core mapping. Over a full match (perf29), stalls over 60 ms fell from 32 to 7 and frames over 40 ms from 97 to 18, with no audio underruns.
- **Hitch diagnostics.** `WWE14_HITCH_WATCH=MS`: once no frame has been presented for `MS` ms, every guest thread's stack is sampled every 2 ms until the next frame. The log then names, per working thread, the functions the stall went to, with the waits that were ready but not woken. Timed waits and sleeps that overrun by more than 50 ms are logged too. The dispatcher also has a 1 ms rescue tick for any wait that is satisfied without a wakeup, and counts those.
- **Copies run natively.** The game's three memory-copy routines (`Crt_Memcpy`, `Game_Memcpy`, `Crt_CopyTail`) are replaced by one `memmove` on guest memory (`kernel::crt`). The host's background threads (texture hashing and conversion, warm-up) run below the guest's priorities.

**The last long stalls: two game routines run natively** (`[[replace]]`, same results). The hitch watchdog, now sampling through each whole stall, showed two causes when an entrance starts. Both are replaced, and both are game functions with identical results, not hardware:
- `Game_FindEntryByName` (`sub_82BA9178`, `kernel::crt`): finds a resource-table entry by `"name.ext"`. The original copies and lowercases every entry before `strcmp`, which cost the render thread 60–75 ms. The native version compares in place: an entry matches when its `"name.ext"` is at most 31 characters and equals the lowercased query, which is exactly when the original's `strcmp` succeeds.
- `Game_BakeCubeDepth` (`sub_82F58250`, `kernel::bake`): casts a ray from a mesh's centre through every texel of a 6×N×N cube against every triangle, and packs the farthest hit into a byte. The game bakes about 40 small cubes in a row (18², 26² or 32² texels, 12 triangles each) while the main thread waits: 190–280 ms. It is transcribed operation by operation with the same VMX helpers, on locals, with face rows on rayon's pool: 0.2–0.9 ms a cube, against 2.5–4 ms. The calls to the cube's setup and edge-finishing functions keep the original's arguments and frame. `WWE14_BAKE=verify` runs both: 80 of 80 bakes matched byte for byte. `WWE14_BAKE=original` runs the recompiled one.
- Still open: a decompressor (`sub_82C41918`, on the loader thread) takes about 100 ms during the black screen between skipped entrances.

**Thin dark lines on every model at 1080p (reported by the user, 2026-11-24).** Skin, shirts and jackets showed 1-pixel crack-like lines along creases and curvature, in the windowed build and in the video previews. Isolated step by step:
- Not geometry: the depth buffer under the face is smooth.
- Not the textures: the wrestler's colour and normal atlases, dumped as converted (`WWE14_DUMP_TEXTURE=WxH`), are clean.
- Not MSAA, and not the atlas-space mask (slot 14).
- At 720p (`WWE14_SCALE=1`) the same shot was clean. Flattening the 64×64 environment cube map (slot 13) removed the lines at 1080p.
- **Cause:** coverage of edges that lie on guest pixel centres. The game draws the lighting cube's six faces as one mesh in a 384×64 strip (one 64×64 face every 64 pixels), then resolves each face. D3D9 pixel centres are at whole coordinates, so the faces meet on pixel centres, and the console's tie rule gives each boundary column to the face on its right. With the half-pixel shift scaled by 1.5 (0.75 host pixels), the host column at each boundary is sampled a sixth of a guest pixel left of the edge, so it went to the face on the left. Every face's first column held the previous face's lighting: black for face 0, whose edge is the strip's corner. The user still saw lines after a first fix that covered only the corner, because the internal seams remained.
- **Fix** (`render::prepare`), above the native resolution:
  - Meshes move by half a *host* pixel. Host pixel centres then fall on guest pixel centres at every even guest coordinate, so the tie rule decides as on the console.
  - Quads (6 vertices or fewer: copy, filter and post-processing passes) keep half a guest pixel, so copies still read each host texel at its centre.
  - The viewport of a quad is widened about its centre by a quarter of a host pixel, so a quad drawn from a corner covers its first column and row.
  - Nothing changes at the native resolution.
- **Result:** checked with every cube face and mip level now dumped (`WWE14_DUMP_TARGETS`). Each face's first and last columns and rows continue its interior. Close-ups of Cena, Big Show and The Rock are clean.

**Pale fringes on silhouettes, and sharp patches behind hair, at 1080p (reported by the user, 2026-11-24).** The game's depth of field blurs the background at quarter and eighth resolution. The downsample (`ps 0a24777d`) is depth-aware: it averages only taps whose depth lies beyond a threshold, so foreground colour stays out of the blur. Its depth texture is the resolved depth (`k_24_8_FLOAT`), which the console's texture units can't filter. Our depth resolves are 32-bit floats, and the renderer honoured the fetch constant's bilinear filter. At 1.5× the downsample's taps fall between host texels, so silhouettes mixed foreground and background depths. Arms then bled into the blur as a pale blocky fringe. Depth fetches (formats 22 and 23) are now point-sampled, as on the console, and the fringe is gone from the same close-up.

**Broken hair (reported by the user, 2026-11-24): the alpha test read the wrong register.** Hair was see-through, with dark card-shaped shards, at every distance. Hair cards are drawn with an alpha test (`RB_COLORCONTROL` 0x8700000E: pass when alpha ≥ reference) and alpha blending. The renderer took the reference from 0x210D, which is `RB_STENCILREFMASK` (0x00FFFFFF, about 2e-38 as a float). Almost every texel passed, so the nearly clear parts of every card were drawn and wrote depth, hiding the hair behind them. The reference is `RB_ALPHA_REF` at 0x210E. Found with the new **F9 snapshot** in the window: the next frame's draw log (now with `cc` and `aref`) and a picture after each pass, in `private/f9_<frame>`.

Also in this round:
- **Mip levels.** Captured textures now carry the game's own mip levels: tiled 2D textures from `mip_address`, each level at the power-of-two size halved, in whole 32×32-block tiles on 4 KB boundaries. Levels in the packed tail are not read yet.
- **Type-0 fetch constants.** Some slots the game samples have type 0 in word 0 but a texture's fields. These are read as textures, as the console's texture fetches do; before, they were bound as white.

**Slow stretches in heavy scenes (the user's match, 2026-11-24).** Entrances, the victory scene and camera cuts to crowd-heavy shots ran at 37–55 fps. Those frames have 3,000–3,900 draws, and two CPU costs scale with draw count:
- the renderer: 13–20 ms preparing draws, plus about 9 ms encoding and submitting;
- the game's render thread: 15–29 ms walking and capturing.

Scenes with half the draws hold 60. Next: cut the per-draw cost on both sides. The renderer uploads each draw's full 256-register constant files, about 9 KB of uniforms a draw.

**Draw submission rewritten (2026-11-25), for those scenes.** A profile of a heavy frame put about 29% of the renderer in wgpu's `CommandEncoder::finish`, 9% in creating the frame's buffers and 9% in starting the encoder. Most of that was each draw's group 0 bind with eight dynamic offsets. Now:
- The translated shaders read every draw's data from two storage buffers: `U`, the frame's 16-byte rows, and `DR`, a record per draw of where its blocks start. The blocks are vertex constants, bool and loop constants, fetch constants, the vertex memory windows, the vertex fetch descriptors, pixel constants, the position transform and the alpha test.
- The vertex shader finds its record by instance index: draw `k` is drawn as instance `k`. It passes the number to the pixel shader as a flat 17th inter-stage variable, so the device must allow 17. Bevy asks for the adapter's limits, and the offscreen host now does too; the renderer asserts this at start.
- Group 0 is bound once per pass. The frame's buffers persist between frames and are rewritten with `queue.write_buffer`; they grow when a frame outgrows them.
- Constant files the capture shares between draws (one `Arc`) are written once a frame. Identical consecutive blocks are still shared.
- All shaders were regenerated: 1,440 from the packages, plus the 3 from the xex. The xex's 64 memory-export shaders still fail, as before.
- First check: menus run at 60, with the renderer at about 1 ms a frame.
- The user's match: the renderer went from 22–29 ms to 8–16 ms a frame in heavy shots. The game's render thread still spends 9–13 ms walking and capturing in the same shots, so a few still dip (worst 5-second stretch 48 fps).

**A wrong camera for one frame at entrance and victory cuts (reported by the user, 2026-11-25).** It is not black. `WWE14_RECORD=DIR` (windowed) now records every rendered frame to `DIR\video.mp4` through NVENC, with `frames.txt` naming each video frame's game frame. ffmpeg scene scores found the frames.
- What the bad frame shows: the first frame after a cut comes from a camera matching neither shot. It is a generic framing of the wrestler, and has no depth of field. In one case Brock is still in the previous shot's pose. Then the new shot starts.
- Only in half-rate scenes (entrances; the user also saw it in victory scenes). In-match cuts are clean.
- Ruled out: the render thread's walk overlapping the main thread's next frame. Holding "start your next frame" (`sub_82C2DE28` called from `0x82BB5F2C`) until the walk ended changed nothing, and was removed.
- Working theory: entrance shots are authored at 30 fps, as ranges of 60 Hz frame numbers on even frames. At 30 fps the clock steps by 2, so it never lands on the odd frame between two shots. At 60 it does, and the camera falls back for that frame.
- **Call tracing** (`recomp --trace-calls on` into `generated/rust-trace`; `WWE14_TRACE_CALLS=FILE` logs which functions start or stop being called each frame, `kernel::trace`; the scratchpad's `bursts.awk` finds short bursts). The user's run (2026-11-25), six bad entrance frames:
  - Each cut runs the event camera's command handlers for exactly one frame: `sub_823F1520` → vtable `+0x27C` → `sub_823F6410` → `sub_823FE590` → `sub_823FE500`/`sub_823FC488`, and `sub_823F1C20` → `+0x294` → `sub_823F6818` → `sub_825DB0A8`.
  - They are virtual methods of one class (vtable `0x820105C0`). Its init `sub_823FA... sub_823F1F20` registers (command id, handler) pairs; the shot command is id `0xC2`.
  - `sub_823FC488(camera, mode 0-9, target, flag)` sets the camera mode and targets (`+0x154`..`+0x190`, `+0x588`/`+0x58C`).
  - The handlers run in the logic of the first *good* frame. Nothing distinctive runs on the bad frame itself, so it is an ordinary camera update in a state between shots. Next: find what the camera reads between the old shot's end and the next command, and keep the old shot for that frame.
- **Constants in shared blocks (2026-11-25).** A stage's 256 constants are captured as 16 blocks of 16 registers (`ir::Consts`). A draw that changes one matrix makes one new 256-byte block, and the rest are the previous draw's. The renderer writes each block to `U` once a frame, and a draw's record points at a 4-row table of block offsets (`kc()` reads the table, then the block). In the user's next match, capture in heavy shots fell from 8.5 ms (1,836 draws) to about 6 ms (2,200 draws), with constants at 1.0 µs a draw (from 1.7). The renderer peaked at 8.8 ms a frame. The user noticed only one slowdown: the bell at the start of the match (main-thread stalls of 100–250 ms).
- **The hitch watchdog caused stalls.** The next run (`WWE14_HITCH_WATCH=50`) was much worse: main-thread stalls of 1–9 s at loads, in Kofi Kingston's entrance and mid-match. Every stall got 1–3 samples however long it lasted. The sampler suspended each thread and unwound its host stack (`RtlVirtualUnwind`); a thread stopped while holding a lock the unwinder needs held the sampler, and the game, with it. It now never stops a thread: it reads each working guest thread's link register and walks its guest stack frames (back chain at `0(r1)`, return address at `back-8`), naming functions from the recompiled table without DbgHelp. `--profile` still suspends threads, so use it only when a stall doesn't matter.
- **What the load stalls are (user18, the new sampler, one sample every 2.3 ms).** The arena load stalls for 0.4–3.5 s at a time. In the longest, worker thread `sub_82506A50` (the one that bakes the arena's cubes) works throughout in `sub_825060D0` → `sub_82503608` → `sub_8252F070`/`sub_8252CD38`, plus float code at `0x832D8xxx`/`0x832FBxxx`. The 2 s stalls are the main thread in scene setup across dozens of functions (`sub_82C302A0` → `sub_82C31350` → `sub_825C0718`, `sub_825A16A0`, ...). There is no single hot routine: general recompiled code, much of it float math. The bell at the match start was not sampled (the 60-report cap ran out during the load; now 2,000).
- **x86-64-v3 (2026-11-25).** The scalar `fmadd` family (5,989 sites) is `f64::mul_add`. On the baseline x86-64 target, each one called the C runtime's `fma`. recomp now writes `generated/rust/.cargo/config.toml` with `target-cpu=x86-64-v3` (AVX2, FMA, BMI2; the user's CPU is a Ryzen 7 5800), so it is one instruction with the same fused result. VMX `vmaddfp` is deliberately unfused and inline, so it is unchanged.
- **Result of x86-64-v3 (user19):** 14 slow frames in a full session (load, both entrances, match), against 129 before, and 1.6 s of stalls in all against 12.7 s. The arena load's 1–3.5 s stalls are gone; nothing during the load is over 95 ms. Capture in heavy shots fell to 4.6–4.9 ms.
- **The bell (128 ms): the BPE decoder, now native.** The main thread spent 28 of 32 samples in `sub_82C41918`, the game's decoder for `BPE ` streams (Gage's byte-pair encoding: a pair table, a little-endian 16-bit block length, then the packed bytes, expanded until the output reaches the header's unpacked size at `+0xC`). `Game_BpeExpand` (`kernel::bpe`) does the same over guest memory and returns the output's end in `r3`, as the original leaves it. `WWE14_BPE=verify` runs both: 512 of 512 decodes during boot matched byte for byte (up to 3.1 MB). `WWE14_BPE=original` runs the recompiled one.
- **Menu lists overflowed their panels (reported by the user, 2026-11-25).** The match-type list showed STEEL CAGE and SUBMISSION below the panel, and the Creations list showed LOGO below its panel. The window scissor was never applied to untiled draws. `D3DDevice_WriteViewportScissor` is native and empty, and its arguments were the rectangle. `SetScissorRect` (`sub_82CAB280`) keeps the effective rectangle (the game's, clipped to the viewport; or the viewport when scissoring is off) in the shadow at device `+0x28C4`/`+0x28C8`, which the capture already records as `0x2011`/`0x2012`. The renderer now clips every untiled draw to it (`window_scissor`); tiled draws keep their tile rectangles. Checked: every draw of a title-screen frame carries `[0, 0, 1280, 720]`.
- **Also seen in the recording: a dark crowd for one frame at in-match cuts (TLC arena).** On the first frame of a new shot, the crowd is dark and unlit; on the next frame it is lit red. It happens at most in-match cuts there. Not investigated yet.

**Lost control after a reversal (the user's first windowed match, 2026-11-24).** The game kept running at 60 fps, but the user's controller stopped acting, and they closed the window. That build logged no input, so where it stopped is not known. Windowed runs now pass `--trace threads`, which logs every controller change. A second full match went fine, with input logged to the end.

**Two controllers in scripted runs.** `--input` entries take a controller (`2:a@f6100`), triggers (`lt`, `rt`), stick directions (`lright`, ...), repeats (`x@f1000-2000/24`) and hold lengths (`+40`). Controller 2 is connected only when the script names it (`pad2`). The scratchpad's `short_match.ps1` sets up the user's short test match (2026-11-24): two humans, controller 2 idle in the match, and P1 with INFINITE finishers (the game's own Finishers setting on the attire panel: RT four times). It skips both entrances (each player's A skips only their own), runs a minute of strikes, then finishers and pins. A run won by pinfall (Sandow, then Khali). With a full momentum meter Y does the signature first; RB is meant to switch to the finisher, untested. The user then chose to play the matches themselves.

**Tests pick random wrestlers** (the user asked, 2026-11-24). The scratchpad's `match_input.ps1` presses Y (the game's random roulette) for both slots, with jittered timing. A Y goes before every A that might select P2, because the game sometimes ignores one press while a preview loads.

Found on the way and fixed:
- **Resolves into mip levels.** The texture-space map in slot 14 is resolved three times a frame (1024×512, 512×256, 256×128), one per mip level. Destinations now get the mip levels their fetch constant declares. Each resolve writes the level it names, and samplers use the fetch constant's mip filter.
- **Draws the capture can't record are reported.** Draws with no shader bound or with vertex memory not committed were dropped silently; now they are reported (`draw not captured: ...`).

## Phase 6: completeness (started 2026-11-25)

**Create a Superstar: saving the superstar stopped the game (reported by the user, 2026-11-25, `private/user27.err`).** The rest of that session (Create a Superstar itself, on the build with the direct indirect-call table) worked.
- **What stopped it:** `sub_82CB22D0`, which the config listed as `[[unreachable]]`, was called on a job worker (`thread@82C3A6A8`) from `sub_82D04490`, called from the game's `sub_82BAFE98`.
- **What those are:**
  - `sub_82CB22D0` is `D3DDevice_Release`: an atomic decrement of the device's reference count at `+0x3C` (`sub_82CB22A8`, just before it, is AddRef). Only when the count reaches zero does it shut the GPU down (`sub_82CC0948`, which has no other caller) and free the device. That branch is why the gate fenced the whole function off; it had never been called in a traced session.
  - `sub_82D04490` is the destructor of a D3D helper object in the D3DX/XGRAPHICS range (reference count at `+0x304`, device at `+0x2BC`). `sub_82D03FB8` creates one and takes the device reference.
  - `sub_82BAFE98` releases the old object and builds a new one for a resource (`EDIT/STS` at the crash). Nothing calls it directly, and it never ran in the traced sessions.
- **Fix:** `D3DDevice_Release` is native (`[[replace]]`, `runtime/gpu/src/device.rs`): the same compare-and-swap decrement, returning the new count. Reaching zero is a fatal error naming the teardown, because the port has none. The gate stays at zero: 79 hardware-facing functions, 31 replaced, 15 `[[unreachable]]`.
- **What else that path can reach:** the new `analyse reach <xex> <function> [cut ...]` lists the config's cut points and the hardware-facing functions a function's direct calls and tail calls reach (indirect calls are not followed). From `sub_82BAFE98`, the only other fenced-off function is the command-buffer space allocator `sub_82CB6EA8`, through `sub_82D00580` → `sub_82CADA08` → `sub_82CAD760` → `sub_82CB5A20`. `sub_82D00580` ran in the crashing session without taking that branch.
- **Tested by the user, 2026-11-25 (user30):** saving a created superstar no longer stops the game. What it showed next is below.
- **Where the next one like it can come from (survey, 2026-11-25).** For each `[[unreachable]]` function, its callers and whether they ran in the traced session:
  - `sub_82CB6EA8`, the command-buffer space allocator, is the wide one: 70 D3D functions call it, each a packet writer. 18 of them are replaced or fenced off themselves. Two more ran in the traced session without reaching it (`sub_82CAB868`, `sub_82CB5A20`). The other 50 never ran. Any of them that a new mode calls stops at the allocator, naming it, and gets a native version the usual way.
  - `sub_82CB9D20` (RunCommandBuffer) is called by `sub_82C26CA8` in the game's renderer, which never ran; `sub_82CB2358` by `sub_82BA7C18`, which never ran.
  - `sub_8343AAB0` (XMA hardware layer) has 11 callers in Wwise's XMA source (`0x82F06FC0`–`0x82F085F8`); the game ships no XMA media.
  - The others have no direct caller (address-taken, or reached only through the fenced functions).
- **Also in that session:** every stall over 100 ms was a first-time pipeline build for Create a Superstar's shaders (seven, 147–1,006 ms). They are in the warm-up list now. No audio underruns.

**A saved superstar has no picture, and the next start calls his save "damaged or missing" (reported by the user, 2026-11-25, user30–33).** The user also read this as "my save data keeps getting cleared".
- **The main save is fine.** `SaveData.dat\SaveData.Dat` (zeros, a DDS icon at `0x1980`, one `ZLIB` block at `0x2CA000`) unpacks cleanly in every backup. The message comes from the superstar's own file, `00CustomSuperStar.cas\SaveData.Dat` (a `ZLIB` header, little-endian: magic, `0x3012`, packed size, unpacked size 1,882,028). It holds a 324×860 DXT5 picture at `0xB9200` (278,640 bytes), and in every file the port wrote that picture is blank: the whole file packs to about 10 KB.
- **Where the picture comes from** (found with `WWE14_WATCH_RESOLVE`, below). The game draws the superstar into a 736×1280 8-bit RGBA target and resolves it at `EndTiling` (caller `0x82C22754`; texture memory `0xFE6C3000`, `0x398000` bytes). Four frames later guest thread `thread@82260310` reads that texture's memory in `sub_82CF9F40` (under `sub_8341B6A8` ← `sub_8341C518` ← `sub_82C3BF80`), scales it to 324×860 and compresses it into the save. The game's frames stop while that thread works. In the port a resolve leaves its image with the renderer and writes nothing to guest memory, so the reader got zeros.
- **Fix: a guest-memory readback where the CPU reads the result** (plan §2.4 item 5).
  - `gpu::resolve::hold_for_reader`: after a resolve into a destination the game reads on the CPU, the destination's pages are made inaccessible (`kernel::mem::watch_with`). The rule for "reads on the CPU" is one case so far (`read_on_cpu`): 8-bit RGBA and taller than 736. No scene pass has such a target.
  - The first read faults. The exception handler (`kernel::diag`) restores the pages and calls `fill_from_renderer`, which asks the renderer for the image (`gpu::ir::request_readback`), converts it to the texture's own byte layout (`guest_bytes`) and writes it to guest memory. Then the read goes on. A destination nobody reads costs nothing.
  - The renderer (`render::Renderer::serve_readbacks`, `read_resolved`) copies its resolved texture back and filters it down from the render scale to the guest size.
  - `guest_bytes` is the inverse of `texture::to_rgba_from`, derived from that function by probing (textures whose bytes spell their own offsets), and checked by converting the result back. Unit tests: `resolve::tests`.
  - Capture: the destination's fingerprint is updated after the write (`ir::resolved_memory_written`), and fingerprinting skips watched memory, so the capture neither trips the watch nor takes the filled memory for a texture the CPU loaded.
- **First try failed (user33), and why.** The renderer answered requests only after rendering a frame. The reader holds the game's frames up, so no frame came, and after 3 s the request gave up and the reader got zeros (`slow frame 7904: 3076 ms`). Now the render threads (window and headless) wait for a frame at most 4 ms at a time and answer requests in between, and a failure says which step failed (no answer, no image at that address, wrong format, wrong size, layout not invertible).
- **Works (the user, user34).** A new superstar saved with his picture: the log has `resolve read back: the 736x1280 image at 0xFE6C3000 is in guest memory for its reader (137 ms)`, his file is 71,837 bytes (10,498 with a blank picture), and the user's screenshot shows him on the match-up screen beside Dean Ambrose, the right way up and in the right colours. The save costs one 220 ms frame, 137 ms of it the readback (the copy back from the GPU and five probe conversions of 3.7 MB); the console also holds its frames there while it compresses. He also loads back after a restart with no "damaged or missing" message (user35; that message had its own cause, below).
- **Parked by the user (2026-11-25): sharper pictures ("can the renders be captured in 4K").** The stored pictures are small by the game's save format, as on the console, so a created superstar looks softer than the shipped wrestlers' photos. The idea: keep a sharp copy from the renderer when the picture is taken, and show it where the game draws its small one; the save stays what the game expects. What was found before parking, from `sub_8341B6A8` (the job's worker, run through `sub_8341C518`):
  - It untiles the 736×1280 texture (`sub_82CFAA70` describes it, `sub_82CA8628` locks it, `sub_82CFA318` untiles), then crops the rectangle (15, 130)–(705, 1150), 690×1020, with `sub_822D9FD8`.
  - It makes **two** pictures: the crop scaled to W1×H1 (`sub_822DA730`), and the top half of that scaled to W2×H2. Both are compressed to DXT5 by `sub_822DA9C0` (source format `0x18280086`, destination `0x1A200054`). The job holds them at `+0x10` (pointer, W1, H1) and `+0x1C` (pointer, W2, H2); the sizes are set by whoever creates the job (not read yet; `sub_8341BF58` sets up the scene).
  - In the unpacked save the picture data found so far is one run of 280,201 bytes at `0xB8FFC` (the earlier "324×860 at `0xB9200`" was a guess from a byte count; a decode at that size comes out scrambled, so the size or the layout is wrong).
  - Still to design: how the renderer recognises the game's small picture when it is drawn (its content, compared with the sharp copy cropped the same way), and where the sharp copies are kept.
- **A superstar saved before the fix has no picture:** it was never stored.
- **The "damaged or missing" message had its own cause: the kernel listed no saved content (found 2026-11-25).** The blank picture did not cause it.
  - At start the game lists the profile's saved games and looks for each superstar's package by file name: `sub_829009B0` calls `XContentCreateEnumerator` (user, device, type 1, 354 items a call) and `XEnumerate`, then compares each item's file name (`+0x108`) with the one it wants. `sub_82900B48` and `sub_82901A68` list the same way; `sub_8281BE28` lists downloadable content (type 2, 128 items).
  - `XamContentCreateEnumerator` returned an enumerator with no items, whatever was on the disk. So the game took every saved superstar's package for missing, showed the message, and on YES dropped him from the main save. That is the "save data keeps getting cleared" the user saw. The logs agree: no start ever opened `00CustomSuperStar.cas`.
  - **Fix (`kernel::xam`):** the enumerator lists the folders under `Content\<xuid>\<title>\<type>` as `XCONTENT_DATA` items (device, type, display name, file name), ordered by file name. `XamEnumerate` hands them out in batches (as many as the buffer and the per-call count allow), then `ERROR_NO_MORE_FILES`. A device that isn't the hard disk, a user who isn't signed in, a zero count and a buffer too small for one item get the console's errors or a fatal diagnostic. Each listing is logged (`content: listing type …`).
  - **Display names:** the console keeps a package's display name in its header. Here `XamContentCreateEx` writes the name the title gives to `<package>.displayname` beside the package's folder, and listings read it back; a package without one shows its file name. `XamContentDelete` removes it and logs the deletion.
  - Unit tests: `xam::tests` (item layout, folder listing).
  - **In the game (user35, the first start with the fix):** `content: listing type 1 on device 1 for user 0: 00CustomSuperStar.cas, SaveData.dat [from 0x82900BF0]`, and then the game opened `00CustomSuperStar.cas` at start for the first time. It did not rewrite `SaveData.dat` at start, as it did after every "damaged" prompt. **Confirmed by the user ("it's all saved"):** the superstar made in user34 is there after closing and reopening, with his picture. Create a Superstar's save and load work.
- **Diagnostics added for this:**
  - `kernel::mem::watch(addr, len, what)`: the first read or write of a guest range is reported with the guest thread, its stack and a host backtrace, then the access goes on.
  - `WWE14_WATCH_RESOLVE=WxH|all`: logs every texture that becomes a resolve destination for the first time (size, format, memory, caller) and watches the ones of that size (`all`: taller than the screen).

**What another mode can stop on (static survey, 2026-11-25, before the user plays the other modes).**
- **Graphics.** Game code calls 112 D3D functions that are not native. With the config's cut points given to `analyse reach`, only five of them can reach an `[[unreachable]]` function. Two run all the time without getting there (`sub_82CADA08`, 57 callers, to the command-buffer allocator through `sub_82CAD760` → `sub_82CB5A20`; `sub_82CB2758`, to `sub_82CC0878`). Three have never run:
  - `sub_82CB8798` (1,232 bytes; begins recording a D3D command buffer), called only by the game's `sub_82C27548`. The game's renderer can record an object's draws once and replay them: `sub_82C13A80` records (it runs, and returns at once because the object's recorder at `+0x208` is null), `sub_82C26CA8` replays through `RunCommandBuffer` (`sub_82CB9D20`, fenced) from the draw-list walker `sub_82BB53D0`. A recorder is made in `sub_82BDB460` (`sub_82C272E0`) only for an object whose creation settings have 1 at `+0x44` (the 0x4C-byte block copied to the object's `+0x1EC`; the test is `+0x230 == 1`). No object in any session so far had it: wrestlers, six arenas, crowds, Create a Superstar. Which objects ask for it, if any, is not known.
  - `sub_82CB3F28` (280 bytes; resolve and swap in one call), called only by the game's present `sub_82BA84E8`, on the branch taken when the renderer's `+0x138` is zero. The other branch (the game's own tiled present, `sub_82C227E8`) is the one every frame takes. Presumably the untiled path of a video mode the port never reports (it always reports 1280×720); not checked further.
  - `sub_82CAFBD8` (488 bytes), called only by `sub_827DC770` right after it resolves the back buffer into a new texture the size of the screen, with a block holding the size and the time. `sub_827DC770` runs on the main thread every frame and does nothing unless its object's `+8` is set: a screen capture of some kind. What sets `+8` is not known.
  - None of the three is fenced by name: each stops at the allocator (`sub_82CB6EA8`) with the guest stack, which names it.
- **System calls.** 83 of the 270 imports have no native version by a scan of the sources (a few of those are false alarms: display queries implemented outside the export tables). The ones a mode the user will play can reach:
  - `XamShowKeyboardUI`: the console's on-screen keyboard, through XAPI's wrapper `sub_82C8F058`, called by the game's `sub_823A20D8` (never run). Naming a superstar, a show or a title in the creation suite and in Universe goes through it. **Native since 2026-11-25 (built 17:07, not played yet); see below.**
  - `XamShowSigninUI` (wrapper `sub_82C8F048`; callers `sub_8290F170`, `sub_82918E10`, which has run without reaching it, and `sub_82B05EF0`), `XamShowMarketplaceUI` (`sub_828206B0`, `sub_828215D0`), `XamShowFriendsUI`, `XamShowGamerCardUIForXUID`, `XamShowPlayerReviewUI`, `XamShowGameInviteUI`: the Xbox LIVE menus. Online play is out of scope, but opening one of those menus should not stop the game. **Native since 2026-11-25 (same build, not played):** each logs its call, tells the title the system UI came and went, and returns success with nothing changed, as when the player backs out (`kernel::xam::live_panel`).
  - `XamShowMessageBoxUIEx` (one site, `sub_822622A0`), `XamContentResolve` (XAPI's `sub_82C8FA80` and `sub_82C8FB58`), `XamUserCreateStatsEnumerator` (leaderboards), the `NetDll_*` sockets (27), `XamSession*`, `XamVoice*`: none has run. Sockets and sessions belong to online play.
  - `RtlUnwind` and `__C_specific_handler`: structured exception handling (PLAN's item), still unneeded.

**The on-screen keyboard (`XamShowKeyboardUI`), native (2026-11-25; built, not played yet).**
- **The game's call** (`sub_823A20D8`, a tail call into XAPI's `XShowKeyboardUI`): user 0xFF, flags 4, default text at the object's `+0x28`, title at `+0x428`, description at `+0x628`, result buffer at `+0x228` with its size in characters from `+0x928`, and an overlapped structure at `+0xC` whose event is the handle at `+8`.
- **Kernel (`kernel::xam`, "The on-screen keyboard").** The request (title, description, starting text, room in the buffer) waits in `KEYBOARD`. The overlapped result is `ERROR_IO_PENDING` and its event unsignalled until the player answers; the title gets `XN_SYS_UI` on and off around it. `keyboard_finish` writes the text as UTF-16, cut to the buffer without splitting a surrogate pair, and completes with `ERROR_SUCCESS`, or with `ERROR_CANCELLED`. A second request while one waits gets `ERROR_ACCESS_DENIED`. The blocking form (no overlapped) is a fatal diagnostic. With no window attached (`keyboard_host_attached` not called) a request is answered at once with its starting text, and logged.
- **Input while it is up.** `XamInputGetState` reports controller 1 as untouched while the keyboard waits, and after it closes until every button has been up once (`host_pad_for_title`): the press that answers it (A, B, or Enter and Escape through the key mapping) would otherwise act in the game, usually on the name field again.
- **Window (`host::window::keyboard_prompt`).** A panel over the game's image with the title, the description, the text with a cursor and a line of help, drawn with Bevy's `Text2d` and its built-in font (the host's Bevy features gained `default_font`). Text comes from the PC's keyboard (`KeyboardInput` messages: the key's text, Backspace); Enter or A accepts, Escape or B cancels. A and B count only after both have been up since it opened. It scales with the window's width.
- Unit test: `xam::tests::keyboard_text_is_cut_to_the_buffer_without_splitting_a_pair`.
- **user39 (2026-11-25): Create a Superstar's name field does not use it.** The game drew its own letter grid, worked with the controller; the user renamed and saved that way. No `XamShowKeyboardUI` call in the log. PC keys reach that grid only as the 2K15 control mapping (WASD, Enter...). Typing PC text into the game's own grid is a Phase 8 keyboard item, not a defect. `sub_823891A8` (a menu handler) is the one caller of the system keyboard (`0x8238A7F0`, `0x8238A958`): when its table entry (`[r27][+0x220 index + 0x30]`, +0x14 if `*(0x8376AAF0)+0x68` is set) is zero it opens the system keyboard; otherwise it builds a two-choice prompt (`sub_8236EF38`, strings 0x9AF/0x9B8). Which screen that menu is was not found.
- **30 Years of WrestleMania stopped on opening (user39):** XAM message `0x0007001A` to the music player app (`XMPSetPlaybackController`, `{client 2, controller 0, locked 1}`) from `sub_82E1A848` ← `sub_82AAC100` ← `sub_824F6FC8`. Now logged and accepted (`kernel::xam::app_message`); the playback-controller query still says the title has control. Rebuilt 2026-11-25 (locals build).
- **30 Years of WrestleMania, user40:** the mode loads and its first match (Hogan vs Studd) plays. Two things open:
  - The intro video's sound cut in and out. No device underrun (0 over the session) and 60 fps, so the gap is inside the game's mix. It is the first Bink video with sound the port has played (`season/04xx.bik2`: Bink 2, 1280×704, 29.97 fps, one 48 kHz track; the boot logo `title_logo.bik2` has no audio). `kernel::audio::watch_gaps` now logs every 20 ms+ stretch of digital silence in the mix (`audio mix: N ms of silence ended at game frame F`). user41 runs it at `WWE14_FRAME_RATE=native` to test the 60 lock.
  - A faint dark horizontal line across the wrestlers' legs (user's screenshots). Not found in the two clean F9 frames (`f9_29474`, `f9_35640`; `f9_35255` has subtitles and was set aside at the user's request). The mode has a toggleable VHS-style filter for old matches; to test: the same view with the filter off.
- **The intro's sound gaps: the Bink 2 decoder is too slow (user41-44, 2026-11-25).** Gaps are ~790 ms of digital silence every 3-4 s, also at `WWE14_FRAME_RATE=native`; voices at normal speed, out of sync with the subtitles. File reads are instant (the Bink IO thread idles on its request event). The video thread (`thread@827DFA18`) is 73% busy in Bink (`sub_835A1030` 27.5%: 570 VMX128 float ops, a transform) and sleeps in `Sleep(3)` polls; it runs about three game frames per video frame, so ~20 fps for a 29.97 fps file, and Bink's audio arrives at that rate. Cause: `ppc_rt::vmx` float ops written lane by lane compile to scalar SSE per element with a compare-and-blend flush each (checked in the assembly). **Fix written, not built:** four-lane SSE for `vaddfp`, `vsubfp`, `vmulfp`, `vmaddfp`, `vnmsubfp`, `vcfsx`, `vctsxs`, with the lane forms kept as reference (`vmx::tests::simd_float_matches_lanes`, 400,000 random and edge vectors). Built 2026-11-25 20:20 (full rebuild 11 min; KAT suite passes). **Confirmed by the user 2026-11-26 (user55): "the 30 years intro is good now".** Diagnostics added: `WWE14_TRACE_WAITS=<ids or names>` (every wait of those threads once a "Bink Snd" thread exists).
- **Universe works (user46-47, 2026-11-25):** the user simulated April to SummerSlam; the game autosaved `SaveData.dat` (2,985,259 bytes) and the next start loaded it back at SummerSlam. Simulating gives 40-45 ms frames with one of 296 ms (main-thread work, not sampled).
- **The rest of the modes (user45-54, 2026-11-25).** Universe, Create an Arena, Advanced Entrance Creation, Create a Move-set, Create a Story and an Options change all work and load back after a restart. Two stops on the way, each fixed natively:
  - Advanced Entrance Creation: `XamCreateEnumeratorHandle`, then `XamGetPrivateEnumStructureFromHandle` (the music player listing the console's music). Native in `kernel::xam` (an enumerator with private request memory).
  - 30 Years of WrestleMania: `XMPSetPlaybackController` (above).
- **The music player is native (custom soundtracks, the user's request).** `kernel::music` lists the audio files in `music\` (or `WWE14_MUSIC_DIR`) with tags and length, decodes them with symphonia (pure Rust: MP3, WAV, FLAC, Ogg Vorbis, AAC/ALAC in MP4, AIFF, MKA), resamples to 48 kHz stereo and mixes them over the game's output in `kernel::audio::submit`, as the console's player mixed over a title. The app-0xFA messages, found statically (XAPI `0x82E1A0B8`-`0x82E1AA10`; the game's player object, vtable `0x82060EC8`; its music browser, `0x8334B638`): listing 0x70025 (0x334-byte items, title at +0x23C, 40 wide characters, the only field the game reads; it finds songs by title), play 0x7003B (the whole item), stop 0x70004, pause 0x70005, continue 0x70003, behaviour 0x70008, status 0x70009 (0 idle, 1 playing, 2 paused; the Jukebox moves on at 0), volume 0x7000B/0x7000C, current song 0x7000E (0x3E0 bytes, strings at +0x240/+0x290/+0x2E0), playback controller 0x7001A/0x7001B. Unit test `music::tests::lists_and_plays_a_wav_at_48k`.
  - The Jukebox keeps one user song (save slot 0x96, one title), so every listing starts with "All songs (music folder)", which plays the folder shuffled and reports playing until the last song ends.
  - The game reads its Jukebox setting from the save a moment after the title screen's music starts, so a second of the game's own music plays first, as on the console. An early start and a hold on the game's music were tried (user55) and removed at the user's request: custom music plays only when the game asks for it.
  - Custom songs sounded dry beside the game's entrance themes. **Fixed 2026-11-26, heard by the user (user92):** a custom entrance song is served as Wwise theme 9990 (`kernel::entrance`) and gets the arena reverb. The match's request comes through the music queue `sub_82ACE130` (`entrance::music_job`).
  - Volume: the player starts at 0.45, the level the game sets once its menus begin (it sends 0.45 on Start), so nothing drops (the user's choice). The game's own sound is held quiet from launch until the save loads when custom songs are ticked (`music::start_hold`, released by the first Jukebox pick with the custom slot on). First done by reporting the system's player in control (user64): the game then kept its music off all session, entrance themes included (user65); now the game's mix is silenced in `kernel::audio::submit` instead.
  - When a mode takes playback control (30 Years of WrestleMania, `XMPSetPlaybackController` controller 0 locked), the playing custom song pauses and resumes when control is given back (`music::title_control`; the user, 2026-11-26). The first Jukebox song after start-up is shuffled too (`jukebox::pick` rewrites the original's custom record).
  - A shuffle over the menus (`WWE14_MENU_MUSIC=on`) exists, off by default (the user wanted the Jukebox).
- **mimalloc removed** at the user's request (no C in the build): the process uses Rust's system allocator.
- **Fixed 2026-11-26: Create a Championship's belt showed no colours or logos.** The game's CPU-built belt texture was right; the shader translator's `setp_inv` (scalar op 31) kept 1 as 1 instead of turning it to 0, so the belt shader's if/else-if chain lost its place and multiplied by a zero colour. Fixed in `wgsl.rs`; all 1,440 shaders retranslated (385 use `setp_inv`). Confirmed by the user (user58).
- **To check with the user:** in Create a Superstar, change the name: the panel shows, the typed name appears in the game after Enter, cancelling leaves the old name, and the name is still there after saving and restarting.

**The pause menu shows for a moment when a match starts (reported by the user, 2026-11-25; "was happening before this build").** Settled the same day: a START press on the controller during the lead-in to the bell, not the port (user38, under "Next").
- `XamShowDeviceSelectorUI` now logs each call with the frame and caller; none is made at a match start, so it is not a system-UI notification.
- In user31 the controller log shows a START press at frame 24838, at the end of the entrance. The first guess was that the press that skips the entrance reaches the match as well; it does not (user38: two skips with START, no third press, no pause).

## Next

**State at the handoff of 2026-11-25, 17:15 (the short version is in [HANDOFF.md](HANDOFF.md)).**
- **Current phase: 6, completeness.** Phase 7's listed work is done and played; its one leftover (per-draw cost in crowded shots) is parked by the user, with the measurements below.
- **Built and waiting for the user to try** (they said "leave it for later"): the on-screen keyboard for typing names (`XamShowKeyboardUI`) and the Xbox LIVE panels that now close instead of stopping the game. Both are described in the Phase 6 section. Nothing else is in flight.
- **Then, in the plan's order:** have the user play each mode they have not played in the port (30 Years of WrestleMania, Universe, the rest of the creation suite, options) and fix each stop statically; check that each mode's save loads back after a restart.
- **Open defects, all parked:** John Cena's belt twitch in his entrance; a wrong camera for one frame at entrance and victory cuts; a dark crowd for one frame at cuts in the TLC arena; a short stutter inside the game's own audio mix; sharper created-superstar pictures (the user's "4K renders" idea).
- **Both builds are current** (`generated\rust-locals` 17:07, the one the user plays; `generated\rust` 17:13, the fallback). The last session log is `private\user38.err`.
- The project folder is not a git repository: nothing here is under version control.

**The user's sessions of the afternoon of 2026-11-25 (user28–38), all on the registers-in-locals build.**
- **The locals build holds up.** Matches with entrances, Create a Superstar and saves over six sessions: no fatal stop, no `not_preserved`, no `not_indirect`. It is the build the user plays now. `[locals] enabled` is still `false` in the config; `generated\rust-locals` is made with `--locals on`.
- **Open from those sessions, in the order to take them:**
  1. Done: the created superstar's picture and his loading back after a restart (Phase 6 above), both confirmed by the user in user34–35.
  2. **Parked by the user 2026-11-25 ("not really getting any stutters anymore"; details under "Parked by the user: the per-draw speed work" below).** Slight slowdown in entrances (the user, user29). It is the draw side in crowded shots. The game's render thread takes 13.1 ms to walk a frame (9.2 ms of it capture), and the renderer 17.2 ms a frame (prepare 9.7, encode and submit 7.5): about 52 frames a second there. Scene changes add loading stalls of 50–150 ms. Next: cut capture and prepare cost for crowd draws.
     - Those shots have 4,900–6,550 draws a frame (user29, frames 23253–23256), and the renderer's prepare takes 22–34 ms for them: about 5 µs a draw. Capture is about 1.8 µs a draw.
     - **Measuring it (2026-11-25).** The sampling profiler (`--profile`) now also reports working samples by source line of the innermost frame of the runtime's own code (`SymGetLineFromAddrW64`; code inlined there counts as the line that called it). The renderer and the host are built with line tables for it (`recomp`'s workspace template). `kernel::profile::start_now` starts a 15 s sample at once: on F10 in the window, and with `WWE14_PROFILE_DRAWS=N` whenever 30 frames in a row have N draws or more (three times a run at most), so the user needs to press nothing in an entrance. First run with it: user36.
     - One waste found by reading: `debug_slot` and `debug_consts` formatted the shader id for every texture of every draw though no debug override was set. They return at once now.
  3. **John Cena's belt still twitches** (item 1 below). The test at the original 30 fps (`WWE14_FRAME_RATE=native`) needs a restart on his entrance; not run yet.
  4. Done: the pause flash at match start was a third START press on the controller, not the port (user38, below).
- Builds on disk:
  - `generated\rust-locals\target\release\wwe14.exe`: registers in host locals, with everything up to the on-screen keyboard (built 2026-11-25 17:07). The one to launch.
  - `generated\rust\target\release\wwe14.exe`: registers in the context, the same runtime (rebuilt 2026-11-25 17:13, with the on-screen keyboard). The fallback.
  - `private\builds\2026-11-25-release-fix\wwe14.exe`: the code as played in user27, plus only the `D3DDevice_Release` fix.
- Save backups (copies) are in `private\save-backups\`, one folder per launch. Don't move or delete the user's save files without telling them first: moving the superstar file aside once made the game report it damaged.
- If the locals build stops with `not_preserved` or `not_indirect`, the message names the function and the config key that fixes it (`[locals] untrusted` or `indirect`).
- Done 2026-11-25 (user37, user38, below): Havok, skinning and the rest of the game's own code were profiled in played sessions, and nothing needs a native replacement for speed; no string routine shows up. Left of the plan: Phase 6 (every mode, saves).
- **How the profiles are taken** (the tools stay in the build): launch with `WWE14_PROFILE_AUTO=120` (and `WWE14_PROFILE_DRAWS=2500`, `WWE14_HITCH_WATCH=50`), and have the user play. The profiles cover every thread: the game's own and the renderer by source line.
  - `WWE14_PROFILE_AUTO=SECONDS` (`kernel::profile::auto`, `scene`): a 12 s sample every `SECONDS`, and a 15 s one each time the game switches to half rate (an entrance, cutscene or loading screen starts; `gpu::present::halve_frame_rate`), six of those a run at most. One sample at a time.
- **Parked by the user (2026-11-25): the per-draw speed work.** "Go back to the plan and save this work for later, I'm not really getting any stutters anymore." What is known, for when it is picked up:
  - **user38 measured crowded shots** (`WWE14_PROFILE_DRAWS=2500` fired three times in the first entrance: 3,263, 2,782 and 3,059 draws a frame). The game held 59.5–60.0 fps there with the renderer at 8–11 ms a frame. The one dip of the session: about five seconds at the start of the second entrance, 57.7 fps, 3,580–3,610 draws, the renderer 15.2 ms a frame (prepare 8.6, encode and submit 6.6), six frames in a row of 26–28 ms with prepare at 15–16 ms, and the game's walk 12.3 ms (capture 8.7). That stretch itself was not sampled (the three samples were used up).
  - **Host render thread in those shots: 62% working.** `prepare` 28% inclusive, wgpu's `CommandEncoder::finish` 15.8% (`encode_render_pass` 13.4%), the Vulkan driver 11.6%, `Queue::submit` 4.2%, the C runtime's copy routine 6.3%. By line: `arena.vertex_range` (`lib.rs:2926`) 3.2%, `Arena::consts` line 676 2.3% (4.5% inclusive), `Arena::block` line 650 1.8% (2.9%), `pipeline_key` 3.4% (run twice a draw), the vertex-fetch ordering at 2945 1.2%, the bind-group lookup at 3039 0.9%, growing `Vec<PreparedDraw>` 0.7%, `Arena::indices` 0.6%.
  - **Game render thread: 57% working, capture 33% inclusive.** `stage_consts` 9.1%, the vertex spans (`frame_span` 6.6%, `kernel::mem::read` 4.5%), building `regs` by pushing into an empty `Vec` 4.3% (`ir.rs:753`), `fingerprint` 3.4%, growing the frame's command list from empty 3.2% (a `Command` holds a whole `Draw`, about a kilobyte), `sample_print` 2.3%, `shader_info` 1.7%, the index conversion 1.2%, mimalloc 5.4%.
  - **Cheap steps, in order** (none started): reserve `regs` and the frame's command list; `Arena::indices` as one copy; compute `pipeline_key` once a draw and look registers up without a linear search; fewer per-draw `Vec`s in `prepare`; a faster hasher for the renderer's maps. Expected: about a tenth off each thread.
  - **Larger steps:** write vertex spans straight into wgpu's staging buffer (one copy fewer); prepare a frame on one thread while the previous one is encoded on another; one indirect call for runs of draws that share pipeline and bindings.
  - Tools left in place: the line-level profiler, `WWE14_PROFILE_AUTO`, `WWE14_PROFILE_DRAWS`, F10.
- **user37 (2026-11-25): the first profiles of a played session** (`WWE14_PROFILE_AUTO=120`; character select, two skipped entrances, a singles match and the victory scene; no fatal stop). Four samples: character select and loading, the second entrance into the bell, and two in the match.
  - **Nothing is near its limit in an ordinary match.** Share of the time each thread was working: the game's render thread (`thread@82BB5CC0`) 29–39%, the host's render thread 27–38%, the main thread (`thread@82260588`) 17–25%, `thread@82BB6930` 14–24%, `thread@82506A50` up to 13%, Wwise's `AK::EventManager` 6–9%. That is the answer to "profile Havok, skinning and Bink": none of the game's own code needs a native replacement for speed, and no string routine shows up at all.
  - **The game's own hot functions** (share of all working samples): `sub_82508738` 5.6–7.9% (on `thread@82BB6930`, under `sub_8250BAF0`), `sub_832919C8` 4.3% in the match (on `thread@82506A50`, under `sub_8322AE60` → `sub_83252C98`; the `0x832xxxxx` library), `sub_835A1030` 7.8% while character select loads (`thread@827DFA18`), `sub_82F24960` 1.7–1.9% (Wwise).
  - **The game's render thread: 16% of its time is capture** (`gpu::ir::capture` inclusive, of 29% working): `stage_consts` 3.9%, vertex spans (`frame_span`, `read_span`, `kernel::mem::read`) 4.5%, `fingerprint` 2.3%, `sample_print` 1.5%, `shader_info` 1.1%, growing the frame's command list from empty every frame 1.4% (`RawVec::grow_one<Command>`), converting index lists element by element 0.9%.
  - **The host's render thread** (27–38% working): `prepare` 10–15% inclusive, wgpu's `CommandEncoder::finish` 8–12% (its `encode_render_pass` 7–10%), the Vulkan driver 6–8% (shown as `vkGetInstanceProcAddr`, the nearest export), `Queue::submit` 2.3–2.8%, `write_buffer` 1.5–2.3%. By source line the largest is `render\src\lib.rs:2926`, `arena.vertex_range` in `prepare`: 1.9–3.3%, the copy of each draw's vertex memory into the frame's buffer. Then `Arena::consts` line 676 (0.9–1.0%, copying constant blocks), `Arena::block` line 650 (0.5–0.8%), `pipeline_key` (1.2–1.7% in all; it runs twice a draw, once in the pass that collects missing pipelines and once in `prepare`), the bind-group lookup at line 3039 (0.3–0.7%), and `Arena::indices`'s element-by-element extend (0.4–0.5%).
  - **Copying and allocating are the largest host costs overall:** `_NLG_Return2` and `_intrinsic_setjmpex` (the C runtime's copy and compare routines under their nearest public names) 7–12% of all working samples, mimalloc 4–6% (`mi_bitmap_try_find_and_claim` means blocks of megabytes allocated every frame: the vertex spans and the frame's buffers). Vertex memory is copied three times a frame: guest memory to a span, the span to the arena, the arena to wgpu's staging buffer.
  - **What this means for crowded shots** (user29: 4,900–6,550 draws, renderer 17.2 ms a frame): the two render threads' costs grow with the draw count, and the host's reaches a whole frame first. Its time splits about 40% prepare, 30% wgpu's encoding, 20% driver, 10% submit and uploads. Trimming `prepare` alone buys about a tenth. A real margin needs one of: preparing a frame on one thread while the previous one is encoded on another (textures written during prepare would have to be queued in order), or drawing runs of draws that share pipeline and bindings with one indirect call. Not started; a profile of a crowded arena comes first.
  - Entrances and the match held 60 fps; the renderer's five-second averages stayed under 8 ms a frame.
  - **The pause menu at the bell (the user: "I skipped entrances with START but didn't press anything when the match started").** The game read three START presses: frame 8244 (97 frames before the second entrance's scene began at 8341), frame 8531, and frame 9103, with LB at 9022 between. Nothing else was pressed until frame 11903. The kernel adds nothing to what the host reports (`XamInputGetState` passes the window's pad state), so the host reported START at 9103: from the controller's own START, from another button the host's mapping takes for it, from a second device, or from the keyboard (Escape). Earlier sessions have the same shape: one START close to the bell in user29 (24838) and user36 (38048). `read_input` now logs every gamepad's pressed buttons by the host's names, with the device's name and ids, and the pressed keys, whenever they change (`input source …`). Another explanation is not ruled out: under the 60 lock a skipped scene's frame-counted fade lasts half as long in real time, so a START still held could reach the match; `WWE14_FRAME_RATE=native` would test that.
  - **Settled in user38 (2026-11-25): not a defect of the port.** The user skipped both entrances with START (frames 8598 and 10169), then put the controller down until after the bell: no third START was reported, and no pause menu came up. Every START in the log comes from the one gamepad's own button (`gamepad 0 'HID-compliant game controller' 045e:0b20 [Start]`, an Xbox Wireless Controller over Bluetooth); there is no second device and no key. So the START near the bell in user29, user36 and user37 was a press on the controller, about nine seconds after the last skip, while the game plays the unskippable lead-in to the bell; START then pauses the match, as it does at any time in one. The 60 lock is not involved: with the same skips and no third press the match started clean.
- **user36 (2026-11-25), launched with `WWE14_PROFILE_DRAWS=3000` only: no profile was taken.** About 42,000 frames: nine minutes of menus and character select, two entrances, the start of a match.
  - The entrances held 60.0 fps. The renderer's heaviest five seconds averaged 8.2 ms a frame (prepare 4.4), so no 30 frames in a row reached 3,000 draws. The slow case of user29 (4,900–6,550 draws) needs a fuller arena; which one the user played then is not known.
  - **Stalls at frames 5680–7630 (character select) were caused by this session's own tooling, not the game:** 30 frames of 40–118 ms and one of 355 ms with no guest thread working, waits "ready but not woken", and a 7 ms sleep that took 53 ms. A full `cargo build --release` of `generated\rust` was running on every core at that time, started in the belief that the game was closed. A log watcher had reported it closed after 18 s; it was open. Check with `Get-Process -Name wwe14` immediately before any build, and never build while the user plays.
  - Otherwise: two renderer warm-up frames at boot (108 and 406 ms) and three single frames of 40–44 ms (35006, 38174, 38404). No fatal stop; the superstar's package was listed and opened at start again.
  - A START press at frame 38048, about 3,000 frames after the second entrance began; whether the pause menu flashed at the bell this time was not reported.

1. **Phase 7: a constant 60 fps — met in the user's session of 2026-11-25 (user20).** Boot, arena load, both entrances, the bell and a match gave 7 slow frames in all: one at boot (renderer warm-up, 510 ms) and six single late frames (41–48 ms). There were no audio underruns. Still open from the 60 fps lock:
   - A wrong camera for one frame at entrance and victory cuts (the half-rate scenes; see above).
   - A dark crowd for one frame at in-match cuts in the TLC arena.
   - The six 41–48 ms frames, and the warm-up at boot.
   - **John Cena's title belt twitches through his entrance (reported by the user, 2026-11-25; recorded in `private/rec_cena`, video frames ~4500–6420).**
     - On the stage, the belt in his left hand holds steady for a few frames, then for single frames sits at another angle or offset relative to his hand, then snaps back (video frames 4739, 4751, 4754, 4759, …).
     - Walking down the ramp, holding it by both ends (around 5280–5350), the strap jumps between whole shapes from one frame to the next: dangling, sticking straight out to one side, curled into a hoop.
     - The whole belt moves relative to the hand, so on those frames the belt is placed from a different pose than his arm. The bones come from the game's CPU code, not from vertex decoding: this session's decoder rewrite can't cause it.
     - Seen only in the entrance, a half-rate scene that the 60 lock runs at 60, like the camera-cut frame above.
     - Next test: the same entrance with `WWE14_FRAME_RATE=native`. If the belt holds steady at 30, the lock causes it; if not, look at the recompiled physics or attachment code. The user parked this to get back to the plan.
2. **1080p (the user asked, 2026-11-24): built and recorded headless.** A full match at 1920×1080 renders correctly: menus, character select, entrance, match and victory. The "posterized" look in stills was only how large images are shrunk for viewing; crops at full size and properly filtered downscales are smooth. Not yet tried in the window on the user's GPU (4x MSAA at 1.5x is 2.25 times the pixels).
   - **Video previews:** headless `--video FROM` pipes every frame from `FROM` on through ffmpeg into `DIR\video.mp4` (60 fps H.264, fragmented MP4; `WWE14_FFMPEG` names the binary). With `--wav`, the audio clock follows presented frames (800 samples a frame), so the audio matches the video however slowly the run goes. The logic is frame-locked, so the video plays at real speed. The phone delivery limit is 30 MB per file, so clips go in parts of about 45–55 s at 1080p60.
   - `render::scale()` (default 1.5, `WWE14_SCALE`) and `up()`: surfaces and resolve destinations are allocated at host size and keep their guest size.
   - The position transform stays in guest units, and the wgpu viewport covers the host texture.
   - Scissors, resolve and copy rectangles, clears and the presented image are scaled.
   - Translated shaders measure texel units by the fetch constant's guest size (`tex_size`), but no game shader uses unnormalized or offset fetches.
   - The window opens borderless fullscreen; F11 switches to a window, and `WWE14_WINDOWED` starts windowed.
   - Check edges, post-processing alignment and GPU memory (4x MSAA at 1.5x).
3. **Phase 4, after the gate (a full exhibition match renders):**
   - Done 2026-11-25, not a defect: the crowd's strong red and blue tints (TLC arena) are the arena's lighting. Footage of the original game (a TLC fatal four-way on Xbox 360, YouTube `DL7C50j4VXM`, 2:00, 5:30, 9:20) shows the crowd behind the hard camera lit red and the side sections blue, as in our recording (`rec3`, frames 29–32). The one-frame dark crowd at cuts there is a separate, open defect.
   - Done 2026-11-25: the general scissor (window scissor on untiled draws) and mipmaps for captured textures.
   - **Done 2026-11-25: the GPU gate is at zero and enforced** (`[gpu] enforce = true`). 79 functions are hardware-facing: 30 are replaced natively and 16 are `[[unreachable]]`. Nothing else in recompiled code can reach any of them, and a regression is a build error. Steps:
     - The gate no longer counts calls to imports the kernel implements as plain API contracts: `VdQueryVideoMode`, `VdQueryVideoFlags`, `VdGetCurrentDisplayGamma`, `VdGetCurrentDisplayInformation`, `VdCallGraphicsNotificationRoutines` and `VdPersistDisplay`, plus `MmGetPhysicalAddress` (address translation). The engine and ring-buffer imports still count.
     - `[[unreachable]]` (recomp) turns a hardware-facing function the port never runs into a fatal error naming it. Each one listed never ran in a whole session (the call trace of 2026-11-25), and each is the highest never-run function on its path, inside D3D or the XMA layer.
     - Two D3D functions that do run, `sub_82CADA88` and `sub_82CADBF8`, form a `WAIT_REG_MEM` packet only when the global `0x83946FCC` is set. `D3D_GuardedWaitA`/`B` fail loudly if it is set and run the original otherwise.
     - The temporary `Debug_*` wrappers and `watch.rs` are removed. Of the functions they wrapped, `EndCommandBuffer` and `RunCommandBuffer` (never called; packet writers) are now `[[unreachable]]`.
     - A visible boot reached 60 fps with no stub hit. Then the user's full session (about 54,000 frames: arena loads, four half-rate scenes, matches) hit no stub either.
     - That session (a six-man Elimination Chamber and a singles match with two managers; the user noticed some stutters) had larger stalls, all first-encounter costs in the renderer:
       - Pipelines never built on this machine: 1.1 s and 1.0 s. The shaders changed twice that day, so the driver's cache missed. The 20 new pipelines joined `private/pipeline-keys.txt` (now 432), which the start-up warm-up builds in about 2 s.
       - 114 new textures (40 MB) converted in one frame: 1.0 s in prepare.
- **Not a defect: The Rock's knee pads and boots (2026-11-25).** The user asked whether skin showing between The Rock's knee straps and boots was clipping. Footage of the original game shows they look exactly like that. The theory below was wrong, and the change it led to fixed nothing visible.
  - The change is reverted. Its loop is inlined into every vertex fetch, and the driver's cold compile went from 94 ms to 295 ms a pipeline (441 pipelines in 130 s), which made every first-time pipeline stall worse.
  - What it would have taught: GPU division need not be correctly rounded, and a correctly rounded `v / (2^n − 1)` can be built without dividing (v's bits repeated, rounded by the 25th significant bit).
  - The reasoning that led to it:
  - The F9 snapshots point at the attire vertex shaders (`2ca48933`, `5f05be01`). They read one attribute the body shader doesn't: a `FMT_8_8_8_8` unsigned normalized byte (format word `0x60FD3`, swizzle x←W, y←Z).
  - The shader unpacks it as a code: `trunc(attr × c254.zw)` gives two fields, `low = r2.y − r2.x × c252.y`, and one-hot `== 0` tests against `c255` pick a per-region parameter from `c237`.
  - That gives the byte back only from the correctly rounded `v/255`: one unit in the last place low breaks 255 of the 256 values, and GPU division need not be correctly rounded. Multiplying by a reciprocal doesn't round-trip either at 10, 11 and 16 bits.
  - Now the translator's `vnum` builds every normalized vertex value exactly (`unorm_exact`), without dividing. In binary `v / (2^n − 1)` is v's n bits repeated, so the mantissa is the first 24 significant bits rounded by the 25th; a tie is impossible. Checked against the correctly rounded quotient for every value at 2–16 bits and both signed modes: no differences.
  - All shaders regenerated. Not yet seen in the game.
- **The user's next session (user23): "still stutters, but not as bad".** 73 slow frames, 6.7 s of stalls in all (13.3 s before), 92 frames never shown. By cause:
  - The game waiting on the renderer, about 1.9 s in all. One first-time pipeline took 950 ms (`vs 06fc9eca…`, a 2,058-line straight-line shader, not a state machine). Two waits of 280–310 ms came with no slow renderer frame: the render thread was off the CPU or blocked outside `render()`. It now runs at `THREAD_PRIORITY_HIGHEST` (`kernel::thread::make_urgent`) and logs any 50 ms gap outside rendering. Not tested yet (the user asked to move on).
  - Loading: the arena-geometry worker `sub_82506A50` (436 ms; 3.5 s before x86-64-v3) and the main thread's scene setup (299 ms).
  - Clusters of four 45–80 ms stalls about every 4,000 frames, the same shape each time; the render thread is in `sub_82799A70` → `sub_82798A50` (resource lookups by name). Open.
- **The user's session after the shader changes (user24, 2026-11-25).**
  - **Start-up:** with every shader changed, the warm-up compiled all 445 known pipelines cold in 11.8 s (about 130 s at the last change). There were no first-time pipeline stalls in play.
  - **The first entrance's stutter (frame 7258, 378 ms):** the game waited 366 ms to hand the renderer a frame. The renderer never took over 20 ms on a frame then, and the render thread logged no gap after a frame. So the render thread was neither rendering nor handing over; it was blocked or descheduled between frames, the same unexplained wait as user23. Open: log how long the render thread's `recv` takes and whether frames were queued meanwhile.
  - **The other stutters were loading work at scene changes:** 40–468 ms with the main thread waiting. In the 468 ms one, before the Undertaker's entrance, the arena-geometry worker `sub_82506A50` worked throughout in float code at `0x8308xxxx`–`0x8344xxxx`. In other bursts it was the game's render thread in `sub_82799A70` → `sub_82798A50`, or the main thread's scene setup. All of this is general recompiled code: the performance items' target.
  - **Recorder crash fixed:** a recorded run quit at start. The oldest copy wasn't mapped yet; its callback can run on the window's poll just after the renderer's returns. The recorder then stopped, but it still reused that slot for the next frame, and wgpu rejects a copy into a buffer being mapped, which quits the app. The recorder now waits up to 2 s for the map, and once stopped never touches its slots again.
- **The warm-up gives way to the renderer (2026-11-25).** With a cold driver cache (the first run after shaders change), the start-up warm-up compiled 441 pipelines on half the cores for 130 s. Meanwhile a frame's one missing pipeline, built on the spot, took 3.6 s twice. While the renderer builds pipelines (`RENDER_COMPILING`), warm-up workers start no new build; the renderer logs any pipeline a frame waited 100 ms or more for. Cold compile time per pipeline by shader version (first run after each change): 65 ms (per-draw records), 94 ms (constant blocks), 295 ms (the exact conversion, reverted). Since the decoder rewrite (below) `compile_bench` measures it directly.
- **The renderer runs on its own thread in the window (2026-11-25).** The user saw frame drops in entrances that did not feel like slow motion. The game ran at 57–60 fps and the renderer at 58–60 frames a second, but the window showed only 35–43 game frames a second. The renderer ran inside Bevy's frame, and a heavy shot (13–15 ms to render, plus that run's recording) missed the display refresh, so the window showed every other frame. Now a `render` thread owns the renderer (as headless does), renders every frame in order, records the last frame it finished, and wakes the window. The window's render node only copies the newest finished image (queue order makes the copy see the whole frame). The window's perf line now counts game frames never shown.
- **Texture conversion by whoever needs it first (2026-11-25).** The user's six-man Hell in a Cell (WrestleMania arena, all new wrestlers, frame recording on) stalled the renderer for 0.6 s on 58 new textures, though BC uploads made their conversion cheap. The converters run at the lowest priority, and in a heavy match the game's threads starved them while the renderer waited. A texture now holds its pending job (`Texture::pending`). A converter takes it from the queue, or the renderer takes it at normal priority when it needs the texels first (`Texture::converted`), so it waits only for a conversion already under way. The same session also had main-thread stalls of 0.25–0.5 s mid-match (not sampled) and a 638 ms first build of the new content's pipelines (now in the warm-up list).
- **BC textures uploaded as stored (2026-11-25).** DXT1, DXT2/3, DXT4/5 and DXN go to the GPU as BC1/BC2/BC3/BC5 blocks (`texture::bc_passthrough`, `to_bc_from`): untiled and endian-corrected, with no CPU decode. The conditions: an identity swizzle, plain unsigned fractions, a base level of whole blocks, and a device with `TEXTURE_COMPRESSION_BC` (the renderer sets `texture::BC_UPLOAD`; the offscreen host now requests it). Everything else (DXT3A, DXT5A, CTX1, swizzled, signed or gamma textures) is decoded on the CPU as before. Upload data and GPU memory for those textures shrink 4–8×. Checked: the main menu and its artwork draw correctly.
   - Done 2026-11-25: the last 12 CF state-machine shaders (all pixel shaders with Xenos loops) are structured loops.
     - `loop_pairs` matches each `loop_start` with its `loop_end`. It accepts only nested loops whose ends point at each other, forward jumps, and no jump into a loop body from outside. Calls and returns are refused.
     - `emit_structured` emits each loop as a WGSL `loop` with its own count and `aL` per depth. A zero count skips the body; the `loop_end` predicate breaks out.
     - All 1,440 shaders still translate, and naga validates every output. Only those 12 files changed, so every other pipeline keeps its driver cache. Not yet seen in the game (the user asked not to test this change).
   - Done 2026-11-25: the first compile of the largest pipelines, 2.4 times faster. The 152 distinct shader pairs in the warm-up list compile cold in 41.5 s on one thread, down from 100.0 s. The 12 largest take 8.8 s, down from 25.3 s; the wrestlers' attire pipelines take about 0.7 s each, down from 2–2.8 s.
     - **Measuring:** the renderer's `compile_bench` example (`cargo run --release -p render --example compile_bench -- --top N`) compiles pipelines from `private/pipeline-keys.txt` on one thread, largest shaders first. It writes a nonce into each shader so the driver's cache misses; a second run with the same nonce takes 67 ms for 12, which confirms the miss. `--edit` tries text changes on the real shaders. Within one run the driver reuses a vertex stage it has compiled, so a vertex shader's cost shows only in the first pipeline that uses it.
     - **Phantom loop-relative registers (about 15%).** The translator declared all three operands of every vector op. With a constant scalar op (42–47), the third operand's register field is a constant index, and read as a temp it decoded as `abs(r[61 + aL])` (c253 = `0xFD`). 594 shaders without loops indexed their registers at run time, which keeps all 64 in indexable memory. Only the operands an op reads are declared now. No shader indexes registers at run time any more, and a vector op that reads src3 beside a constant scalar op fails loudly.
     - **The vertex decoder (the rest).** It cost more than half of a skinned vertex shader's compile (1.56 s against 0.67 s with a floats-only decoder). Each format had its own code, and each word its own search of the 8 vertex windows. Inlined into each of 7 fetches, that was about 140 window loops. Now each format is a row of a layout table (`VERTEX_LAYOUTS`: word, first bit and width per component, and the kind of number). A fetch searches the windows once, reads 4 words, swaps them once, and runs one switch on the kind. The fetch swizzle is branch-free.
     - **Correctly rounded normalised values.** Plain division by a run-time width came out an ulp low in some cases, and the attire shaders decode bytes back from these values. `vdiv` corrects the quotient with one fused multiply-add step.
     - **Checked** with the renderer's `decoder_check` example: 360,000 random elements over all 15 formats, 3 unknown formats, all flags, exponent adjusts, endian modes and window edges, old decoder against new on the GPU. All 200,376 normalised components equal the f64 quotient rounded to f32. Everything else (16- and 32-bit floats, 32-bit integers, unknown formats, swizzles) matches the old decoder bit for bit. The old decoder was itself up to 2 ulps off for normalised values, differently for each width, depending on how the driver compiled each division by a constant.
     - Every shader changed, so the first run after this compiles every pipeline cold (the warm-up list on half the cores).
     - Not yet seen in the game (the user asked not to test).
4. **Phase 5:** check the gate (menu music, crowd and commentary in sync, no underruns) on a full match; then latency and channel mapping for 5.1 devices. (No XMA media, so no XMA decoder is needed unless a voice asks for one.)
   - The user's session of 2026-11-25 (user24: two entrances, a Hell in a Cell match, about 6 minutes): the device never ran dry. There were 0 underruns and 0 ms dropped over 300 s, and the queue held at 60–65 ms. The user still heard a short stutter in the Undertaker's entrance theme, with no slow frame around it. So it is inside the game's own mix (Wwise feeding the game's recompiled XAudio2, for example a music voice short of data), which the native side can't see yet. Open.
   - 2026-11-25: the window's audio output now measures the gate; before, only the headless WAV writer counted underruns. The device callback counts silence written after the game's audio started (underrun time and stretches), audio dropped because the queue was full, and the queue's depth range (how far the audio heard lags the mixing). An `audio report` thread logs `host: audio N s played: underruns ...` every 10 s when underruns or drops grow, and every minute anyway. The user's sessions so far (user20–23: full matches with entrances) logged nothing about audio, and the user has not reported audio problems. The gate is checked from the next session's log.
5. **SEH:** `RtlRaiseException`/`RtlUnwind` through the same unwind-and-catch mechanism as `longjmp`, when the game first needs it.
6. **Performance** (once code runs):
   - Done 2026-11-25, played by the user the same day (Create a Superstar, `user27`; nothing wrong but the save crash above): indirect calls go through a direct table (`ppc_rt::lookup`, `DIRECT`): one slot per instruction word from the first function to the last, about 4.7 million slots (38 MB), built at boot. Before, every indirect call binary-searched the 83,000 functions, about 17 dependent loads. The game's C++ makes virtual calls everywhere; the arena-loading worker's hot sort (`sub_8344F598`) calls its comparator through one per comparison. Other addresses and natives registered at run time keep the old path. Unit test `indirect_lookup`; full rebuild 14 min.
   - Registers as locals (2026-11-25): **written, tested outside the game, built, and played by the user the same afternoon (user28–34) with nothing wrong traced to it.** It is off in the config (`[locals] enabled = false`), so `generated/rust` is unchanged in kind. The build with it on is a second workspace, `generated/rust-locals` (`recomp <xex> generated\rust-locals --locals on`).
     - **What it does.** A function keeps the registers it uses in host locals. The context carries a register only where one function really hands it to another: the caller stores its local before the call and the callee loads it on entry, or the callee stores it before returning and the caller reloads it. Always in the context: `r1`–`r10`, `r13`, `f1`–`f13`, `v1`–`v13`, LR, MSR and FPSCR.
     - **How it is decided** (`tools/recomp/src/locals.rs`, `regs.rs`). For direct calls nothing is assumed about the calling convention. Each instruction's register reads and writes are taken from the Rust text the emitter wrote for it, so the analysis can't disagree with the code. Three facts per function are solved together over the whole program: what it reads before writing (itself or through its callees), which context slots it may leave changed, and what some caller reads after it returns. Functions that never return (43) cut the flow after calls to them.
     - **Preserved registers.** The compiler's save/restore helpers (`__savegprlr`, `__restgprlr`, the FPR and VMX ones) are translated into their callers, so every save is visible. A function that saves `r14`–`r31` or `f14`–`f31` on entry and restores it at every exit, provably, gets neither emitted: the caller's local is untouched by construction. Where the save is there but the restore can't be followed (frame pointers, `alloca`, vector registers), the function is taken to restore the register, and every exit checks it against the entry value: a mismatch stops the game and names the function (`ppc_rt::not_preserved`; the static fix is `[locals] untrusted`).
     - **Calls through pointers and to native code** can't know their callee, so they use a fixed contract, the ABI's: the callee reads none of these registers from its caller and returns the preserved ones unchanged. Every function is checked against it. One that doesn't meet it keeps its direct callers, who see its real summary, but its entry in the indirect-call table is a fatal error naming it (`ppc_rt::not_indirect`; the static fix is `[locals] indirect`). That is 301 functions, 33 of them address-taken: 32 exception-handling funclets, which take `r12` or `r31` from the handler that calls them (only the SEH dispatch, not implemented, would call one through a pointer), and one D3DX decoder error handler (`sub_82CCDA10`). No replaced function and none of the 52 guest addresses the runtime calls is among them; `recomp` stops if a replaced one ever is.
     - **Two assumptions the analysis makes, and how each fails:**
       - *No code reads a register the ABI lets a callee change after a call through a pointer.* A function whose own code does stops the build. None does.
       - *Vector registers and `cr1` are not checked against the pointer-call contract.* The game's hand-written vector kernels read lanes they haven't written (history lanes on a loop's first pass). Checked strictly, about fifty address-taken kernels would get fatal entries; supplying the registers properly at every such call was tried and puts about 300,000 vector stores and loads back. They read whatever the context holds instead. This one is not checked anywhere: if a kernel really took a vector register from a caller through a pointer, it would compute with a stale value, silently.
     - **Emitter changes that came with it** (in both kinds of build): `emit.rs` scans a function into blocks, then renders; the output was byte-identical to before at that step. CR and XER accesses are explicit fields (`Cr::nibble`, `Cr::from_nibble`) instead of context methods. Value-independent idioms are constants (`xor x,x,x`, `subf x,x,x`, `subfe x,x,x` as carry − 1, `vxor`/`vandc v,v,v`, integer `vcmpequ*`/`vsubu*m v,v,v`), so they don't read the register. `or rX,rX,rX` emits nothing. `call_indirect`'s slow path sets CTR, which the kernel's missing-export stub reads.
     - **Numbers** (`locals_report.txt` in the output directory; `locals_functions.txt` has a line per function). 76,474 of 83,321 functions keep registers in locals. Of 197,499 recognised saves, 197,480 need no save at all; 5,568 registers are checked at exits. Across 343,963 call sites, 582 stores before and 468 reloads after remain; 57,953 functions load a register on entry, nearly always the sticky overflow bit.
     - **Tests.**
       - The 1,520 known-answer tests pass both ways (`KAT_LOCALS=off` keeps registers in the context). Nine are new (`tools/kat/data/local/locals.s`): preserved registers through helper and inline saves, a funclet-style callee, CR, CTR and the carry across calls, a call through a pointer, a frame-pointer callee, a tail call.
       - New differential test on game code (`tools/kat/tests/game.rs`, `build/game.rs`): 3,000 of the game's functions, with everything they call (3,705 functions), are translated both ways and run on four inputs each, with the real image loaded. Every register and the memory they write must agree. 11,998 of 12,000 runs agree; the other two differ only in a vector lane loaded from stale stack (`sub_831DD1D0`: a 16-byte load around a 4-byte local picks up a save slot one translation never writes). Only functions sure to end are taken (no loops, no calls through pointers, no imports): 19,641 qualify. `KAT_GAME_FUNCTIONS=N` sets the sample. A one-off run of 14,000 (56,000 runs, 13 min to build) found nothing more: 55,989 agreed, 9 were stale-stack vector lanes, and 2 were a made-up NaN whose sign differed, read back through an integer register (the comparison now allows for that there too).
       - What the test taught about comparing two compilations: the sign of a NaN an operation makes up, and which of two NaN operands it hands on, differ between compilations of the same source. The test feeds finite inputs and compares NaNs without their sign.
     - **Build.** `generated/rust-locals` built in 19.5 min from scratch, with no errors or warnings. The binary is 195 MB (233 MB with registers in the context).
     - **Verified only by play:** loops and calls through pointers are outside the differential test; six sessions of the game ran on this build without a fault. A crash report from this build shows stale values for the registers held in locals (`r0`, `r11`, `r12`, `r14`–`r31`, CR, CTR), and says so (`kernel::Program::registers_in_locals`); `--locals off` gives a build whose reports show everything.
   - Done 2026-11-25: `or rX,rX,rX` emits nothing (with the emitter changes above).
   - Done 2026-11-25, played (user28–34): `vperm` is two `pshufb` and a blend where the build has SSSE3 and SSE4.1 (the game's does), instead of sixteen dependent byte lookups (`ppc_rt::vperm_shuffle`). Checked against the byte-by-byte version for every control value in every position and 20,000 random operands (`vperm_shuffle_matches_bytes`).
   - Done 2026-11-25, played (user28–34): the CRT's `memset` (`sub_82C79530`, called by 811 functions) is a native fill (`Crt_Memset`, `kernel::crt`). No string routine stands out statically (the widely called small functions are game helpers and Lua accessors); picking those needs a profile from a session.
   - Done 2026-11-25: `target-cpu=x86-64-v3` (recomp writes `generated/rust/.cargo/config.toml`), so scalar `fmadd` is one FMA instruction.

**Match-up screen: letters drawn as black boxes (reported by the user 2026-11-26, user98; WrestleMania VII, IX and XII arenas).** The title's letters come from 512x512 DXT5 font sheets. A letter on the second sheet (`1DE31000`) was a black box: in the snapshots (`f9_44872`, `f9_48854`) exactly the draws that used that sheet had it uncaptured (1 draw for the one box at WM XII, 3 for the three at WM IX). The renderer took the memory for an image a resolve had once written there (`ir::resolve_dests`). Its check for a later CPU write, a 128-byte-a-page fingerprint, missed the sheet loaded over it, and the renderer had no image for it either, so it drew black. Fix (`gpu::ir`): each resolve destination also records the image's shape (format, width, height). A fetch that reads the memory with another shape is the CPU's texture (a DXT sheet can never be a resolve's output). Not yet tried.
