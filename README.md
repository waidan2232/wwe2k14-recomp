# wwe2k14-recomp

A static-recompilation port of WWE 2K14 (Xbox 360) to Rust, with a Bevy host. **No emulation of any kind.** See [CLAUDE.md](CLAUDE.md) for the rule and [PLAN.md](PLAN.md) for the plan.

You must supply your own copy of the game. Everything derived from the disc lives in `private/` and `generated/`. Both are gitignored and must never be published.

Progress and verified results are in [STATUS.md](STATUS.md).

## What you need

This repository holds only the port's source code: no game code, data, art, audio or video. To build and play it you need your own copy of the game, dumped from a disc you own.

### Required: the WWE 2K14 disc image (Xbox 360)

A full disc image (`.iso`, XGD3) of the **WWE 2K14 Xbox 360 retail disc**, the release identified by the table below (its media ID and `default.xex` checksum). The port is a recompile of that disc's exact executable. A disc whose `default.xex` differs (another region's build, a later reprint, a modified file) won't match the addresses in [config/wwe14.toml](config/wwe14.toml) and won't work.

Check your copy after extracting it (step 1 below): `cargo run --release -p xex -- info private\game\default.xex` must show

| | |
|---|---|
| Title ID | `545408B2` |
| Media ID | `0D94D1D2` |
| Version | `0.0.0.2` |
| Original PE name | `WWE14_DVD_MASTER.exe` |
| SHA-1 of `default.xex` | `DA5D47CC46171E03805B3F8453C103DCD8A99A7F` |

The disc has 714 files, 7.87 GiB.

### Optional: Title Update 1

Title Update 1 (version `0.0.1.2`) for title `545408B2`: the LIVE package `545408B2\000B0000\tu00000002_00000000` (16,924,672 bytes, SHA-1 `46D493B0BC5E14C874A37868BEF59774F2818647`). Our own tools apply it (`xex apply-patch`), but **the port does not use it yet**: the build runs the disc's version 0.0.0.2. Support is in progress; see [HANDOFF.md](HANDOFF.md).

### Optional: the downloadable content

The WWE 2K14 add-on packages for title `545408B2` (content type `00000002`, Xbox LIVE packages). These six are known to work:

| Package file | Name in the package | Size (bytes) |
|---|---|---|
| `FEAE5AA46421E10B2A73D22ECC6C6BA98F7151B454` | WWE 2K14 Add-On 1 | 57,344 |
| `267E5576B180BEBEABD7BE8863B0F5D62D7D37DE54` | WWE 2K14 Add-On 2 | 57,344 |
| `745BEE31BC19100A3EE21606E089EC25F739099C54` | WWE 2K14 Add-On 2 | 65,536 |
| `B66F56549929ABAE96EA716DD039B9C2FE86677C54` | WWE 2K14 Add-On 3 | 164,130,816 |
| `27150AA6E9389DC2A725E09B16CA6538ABE9C99754` | WWE 2K14 Add-On 4 | 129,912,832 |
| `A09DECBF32B71FF36D0CBE0AF56B1618CA47CEEB54` | WWE 2K14 Add-On 5 | 111,271,936 |

Unpack each into the port's hard disk with our own reader, keeping the package's file name as the folder name:

```powershell
cargo run --release -p stfs -- extract <package file> private\hdd\partition1\Content\0000000000000000\545408B2\00000002\<package file name>
```

### Your computer

Windows 10 or 11 (64-bit), a CPU with AVX2 (Intel 4th generation / AMD Ryzen or newer; the build targets `x86-64-v3`), a graphics card with Vulkan or DirectX 12, and an Xbox controller (or the keyboard, below). To build: Rust (the version in [rust-toolchain.toml](rust-toolchain.toml)) and Visual Studio Build Tools with the C++ workload and the Windows SDK, for the MSVC linker. About 25 GB free for the extracted disc, the generated code and the build.

### Building and playing, in order

1. Extract the disc: `cargo run --release -p disc -- extract <your .iso> private\game`
2. Translate the shaders (the `xenos-shader-recomp` commands below, after `xex dump-image`).
3. Recompile and build: `cargo run --release -p recomp -- private\game\default.xex generated\rust-locals --locals on`, then `cargo build --release` in `generated\rust-locals`.
4. Run `generated\rust-locals\target\release\WWE2K14.exe`. It has no console window. Its log goes to `WWE14_LOG` if that is set, to stderr if redirected, and otherwise to `logs\last-run.log` beside the exe.

**A release folder** (play without the project): put `WWE2K14.exe` and `vcruntime140.dll` in a folder with `game\` (the extracted disc), `hdd\` (saves and DLC), `shaders\pac-wgsl` and `shaders\xex-wgsl`, and `music\` (optional custom songs). When `game\default.xex` sits beside the exe, the game uses that folder for everything ([runtime/host/src/portable.rs](runtime/host/src/portable.rs)).

Never upload the extracted game, the generated code or a release folder: they contain the game's copyrighted files.

## Custom music

The Xbox 360 let you play your own music in games. The port does the same with a folder of song files on your PC.

- **Where:** a folder named `music`: in the project folder when you run from the project, or beside `WWE2K14.exe` in a release folder. Subfolders work (up to four levels deep). Set `WWE14_MUSIC_DIR` to use another folder.
- **Formats:** MP3, FLAC, WAV, OGG, M4A/MP4 (AAC and ALAC), AIFF and MKA.
- **Names:** songs show under their title tag, or the file name when they have none.
- **In the menus:** open the game's Jukebox and choose the **User Playlist**. Tick any number of your songs. They play shuffled in place of the game's menu music, and the first song is shuffled too. The game's own music stays quiet until your save has loaded, so it doesn't cut in at the title screen. Your selection is kept in `music\.wwe14-playlist`.
- **As entrance music:** pick one of your songs as entrance music with the game's own custom-music choice (in Create an Entrance). In a match it plays through the game's own sound mixer, with the same arena reverb as the built-in themes. Set `WWE14_ENTRANCE_WWISE=off` to play it straight instead.
- The music volume starts at 45%. Add or remove songs while the game is closed; the list is read when the game starts.

## Crates

| Crate | Purpose |
|---|---|
| `tools/disc` | Xbox 360 disc image (XDVDFS) reader and extractor |
| `tools/xex` | XEX2 loader: decrypt, decompress, PE sections, `.pdata`, imports |
| `tools/ppc` | PowerPC / VMX / VMX128 decoder, disassembler and assembler |
| `tools/analyse` | Function boundaries, switch tables, indirect branches, ABI helpers, MMIO/SEH lists |
| `tools/recomp` | Ahead-of-time recompiler: writes the `generated/rust` workspace (shards, import stubs, function table) |
| `tools/kat` | Known-answer tests: instruction test vectors assembled, recompiled by `recomp`'s emitter and run as native code |
| `tools/pac` | Reader for the game's `.pac` archives: XCompress (LZX) chunks, Yuke's `EPAC`/`PACH`/`EPK8` containers and `BPE` compression. Used offline to find data such as shaders |
| `tools/xenos-shader-recomp` | Offline Xenos microcode → WGSL translator: harvest from the xex and pacs, disassembly, translation validated with naga, shader identity and vertex-fetch descriptors (the library is shared with the renderer) |
| `runtime/ppc-rt` | Support code for recompiled functions: register context, guest memory, VMX/VMX128 operations, setjmp/longjmp unwinding, function and import tables, fatal diagnostics |
| `runtime/kernel` | Native xboxkrnl/XAM: loader, guest memory map and allocators, threads, waits and dispatcher objects, file system, time, XEX queries, diagnostics; boots the game |
| `runtime/gpu` | Native replacement of the game's D3D library at its API boundary, wired in through `[[replace]]` entries in the config (map and method in [docs/d3d.md](docs/d3d.md)); records each frame as render IR |
| `runtime/render` | wgpu executor for the render IR: runs the ahead-of-time translated shaders ([docs/render.md](docs/render.md)) |
| `runtime/host` | The game binary's host: Bevy window with `GpuBridgePlugin`, or headless offscreen rendering; boots the guest on its own threads |

## Tools

Run these from PowerShell. In Git Bash, `/usr/bin/link` shadows the MSVC linker.

```powershell
# Disc image → private/game (skips the console $SystemUpdate folder)
cargo run --release -p disc -- info    C:\path\to\dump.iso
cargo run --release -p disc -- extract C:\path\to\dump.iso private\game

# Executable metadata, imports, sections, decrypted image
cargo run --release -p xex -- info       private\game\default.xex
cargo run --release -p xex -- imports    private\game\default.xex
cargo run --release -p xex -- sections   private\game\default.xex
cargo run --release -p xex -- dump-image private\game\default.xex private\default.image.bin

# Code analysis (Phase 1). `report` writes review files and fails if the Phase 1 gate fails.
cargo run --release -p analyse -- report    private\game\default.xex generated\analysis
cargo run --release -p analyse -- coverage  private\game\default.xex
cargo run --release -p analyse -- switches  private\game\default.xex
cargo run --release -p analyse -- functions private\game\default.xex
cargo run --release -p analyse -- dis       private\game\default.xex 82262638 16
cargo run --release -p analyse -- callers   private\game\default.xex NtCreateFile   # call sites, with context
cargo run --release -p analyse -- refs      private\game\default.xex 82000938       # code that forms an address
cargo run --release -p analyse -- r13       private\game\default.xex                # PCR fields the game reads
cargo run --release -p analyse -- gpu       private\game\default.xex 82CA4000 82CE0000   # GPU census, reachability, D3D API surface
cargo run --release -p analyse -- fns       private\game\default.xex 82CA0000 82CA9000   # functions in a range and their callers
cargo run --release -p analyse -- effects   private\game\default.xex 82CB0F58            # what a function stores on its r3 object
cargo run --release -p analyse -- reach     private\game\default.xex 82BAFE98            # hardware-facing functions its direct calls reach (then: addresses to stop at)

# Recompile to Rust (Phase 2), then build the generated workspace. Manual facts
# (setjmp/longjmp, native replacements) come from config\wwe14.toml, which recomp checks
# against the analysis. recomp also writes generated\rust\gpu_reachable.txt: the
# hardware-facing functions recompiled code can still reach without a native replacement.
cargo run --release -p recomp -- private\game\default.xex generated\rust
cd generated\rust; cargo build --release -j 12 -p game

# The same with guest registers kept in host locals (tools/recomp/src/locals.rs), as a
# second workspace beside the first. `--locals on|off` overrides `[locals] enabled` in the
# config; each output directory gets locals_report.txt, what the register analysis found.
cargo run --release -p recomp -- private\game\default.xex generated\rust-locals --locals on
cd generated\rust-locals; cargo build --release -j 12 -p game

# Run. The binary loads default.xex as data and runs the recompiled code on the native
# kernel, in a Bevy window. WWE14_TRACE (or --trace) = imports,threads,files,audio,draws or all.
.\target\release\WWE2K14.exe
$env:WWE14_TRACE = "imports"; .\target\release\WWE2K14.exe
# Offscreen with the same renderer: a GIF of every 30th frame and PNG stills in a folder.
.\target\release\WWE2K14.exe --headless D:\wwe14-recomp\private\frames --capture-every 30
# Scripted controller input (seconds since boot), e.g. into the main menu:
.\target\release\WWE2K14.exe --input start@30,start@34,a@42,a@48
```

Translated shaders are read from `private\pac-wgsl` and `private\xex-wgsl` (or `WWE14_SHADERS`,
separated by `;`), so translate them first (below).

**Controls.** An Xbox controller works as on the console. The keyboard follows the WWE 2K15 PC
layout, mapped onto the 360 pad:

| Key | Action | Pad |
|---|---|---|
| W A S D | Move (with Space held: chain wrestling, reposition) | Left stick (right stick) |
| ↓ | Front facelock / grapple | A |
| → | Irish whip / pin | B |
| ← | Strike | X |
| ↑ | Signature / finisher / OMG | Y |
| F | Reversal / limb targeting | RT |
| Left Shift | Run (hold + move) | LT |
| Q | Pick up objects / apron / enter or exit ring | LB |
| E | Toggle signatures and finishers | RB |
| Left Ctrl | Target another opponent | Right-stick click |
| 1–4 | Taunts | D-pad up / down / left / right |
| Esc | Pause | Start |
| C | Body indicator | Back |
| Enter / Backspace | Menus: accept / back | A / B |

Shaders are translated ahead of time (Phase 4). Nothing is translated while the game runs:

```powershell
# Archives: format and inner contents, unpack one, check every archive decodes
cargo run --release -p pac -- info   private\game\pac\com.pac
cargo run --release -p pac -- unpack private\game\pac\com.pac private\com.epac
cargo run --release -p pac -- census private\game\pac
# Harvest shader microcode (named by identity) from the pacs and the image, then translate
cargo run --release -p xenos-shader-recomp -- scan private\game\pac private\pac-shaders
cargo run --release -p xenos-shader-recomp -- scan private\default.image.bin private\xex-shaders
cargo run --release -p xenos-shader-recomp -- translate private\pac-shaders private\pac-wgsl
cargo run --release -p xenos-shader-recomp -- dis private\pac-shaders\ps_51e4151e755eaec4.bin
```

The kernel maps the disc folder (`private\game`) as `game:` / `d:` (read-only) and keeps
the hard-disk partitions (cache, saves) under `private\hdd`.

The instruction tests need no game data:

```powershell
cargo test --release -p kat -- --nocapture
```

The tests that use real game data run only when these environment variables are set:

```powershell
$env:WWE14_ISO = "C:\path\to\dump.iso"; $env:WWE14_XEX = "D:\wwe14-recomp\private\game\default.xex"; cargo test --release
```
