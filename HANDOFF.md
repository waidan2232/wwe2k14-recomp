# Handoff

The note the next session starts from. Keep it current while working (the user asked, 2026-11-25): update it and [STATUS.md](STATUS.md) in the same step as anything that changes what is built, what is in flight, or what comes next.

Last updated: 2026-11-25, 17:20. Written at the end of a long session, as the user switched models.

## Read first

1. [CLAUDE.md](CLAUDE.md): the no-emulation rule. It overrides everything.
2. [PLAN.md](PLAN.md): the phases. Follow their order; no side projects unless the user asks.
3. [STATUS.md](STATUS.md): the full record. Start at "## Next" (its first block is the state at this handoff), then "## Phase 6".

## Where things stand

- **Current phase: 6, completeness.** Every mode playable start to finish; saves load back.
- **Phase 7 (performance) is done as the plan lists it.** Registers as host locals, `pshufb` for `vperm`, native `memcpy`/`memset` are built and played. Havok, skinning and the rest of the game's own code were profiled in played sessions (user37, user38): the busiest thread works about 40% of the time in a match, so nothing more needs a native version for speed.
- **Parked by the user, not to be picked up unasked:**
  - Trimming the per-draw cost in crowded shots. "I'm not really getting any stutters anymore." The measurements and the list of steps are in STATUS, "Parked by the user: the per-draw speed work".
  - Sharper created-superstar pictures (their "4K renders" idea). What was found is in STATUS, Phase 6.

## Built, waiting for the user to try

The user said "leave it for later". Do not open the game for this unprompted; offer it when they are ready to play.

- **The on-screen keyboard (`XamShowKeyboardUI`).** The game opens it for every name the player types. Before, that stopped the game. Now a panel shows over the game and the name is typed on the PC keyboard: Enter or A accepts, Escape or B cancels.
  - What to ask the user to do: Create a Superstar, change the name, check the panel shows what they type, check the game shows the new name, save, restart, check it is still there.
  - Nobody has seen the panel yet. If it shows no text, suspect the font first (`default_font` in `runtime/host/Cargo.toml`; `Text2d` in `runtime/host/src/window.rs`, `keyboard_prompt`).
  - Code: `runtime/kernel/src/xam.rs` ("The on-screen keyboard"), `runtime/host/src/window.rs` (`KeyboardPrompt`, `keyboard_prompt`).
- **The Xbox LIVE panels** (sign-in, friends, gamer card, player review, game invite, marketplace): they now open and close with nothing changed instead of stopping the game (`kernel::xam::live_panel`). Not tried.

Nothing else is in flight. Both workspaces build clean from the current sources.

## State at the end of 2026-11-25 (evening)

- **Every mode in the plan has been played** (30 Years of WrestleMania, Universe, the creation suite, Options) and their saves load back. Open: Create a Championship's belt shows no chosen colours or logos (below).
- **Built and on disk** (`generated\rust-locals`, 19:58): the music player (custom entrance music, Jukebox custom soundtrack), the F9 texture dump (`tex_*.png` in each `private\f9_*` folder), no mimalloc.
- **Championship picture fixed 2026-11-26, confirmed by the user (user63):** the picture is a 256x128 8888 resolve drawn after Accept (found with F6, a new 10 s resolve trace), read back by the CPU; `resolve::read_on_cpu` now holds 256x128 (and 64x64) format-6 destinations. Playlist hook fix in the same build: `sub_829365C8` takes the row in r4, not r3.
- **Was open: the custom championship's picture is missing on the Championship screen (user58, 2026-11-26).** Likely a resolved image the CPU reads when the championship is accepted (as the superstar portrait), outside `resolve::read_on_cpu`'s one rule. Next: run with `WWE14_WATCH_RESOLVE=every`, have the user accept the championship again, find the destination the CPU reads, widen `read_on_cpu`.
- **Custom entrance songs through Wwise: done, heard by the user 2026-11-26 (user92: "reverb is on my custom songs now").** In a match the entrance asks the music player's queue `sub_82ACE130` (Game_MusicJob) directly with {kind 0x270E, title} on channel 0x17 (from 0x824F745C); the Jukebox uses channel 0. `entrance::music_job` rewrites a nonzero-channel custom request to {kind 0, id 9990}. Details: `kernel::entrance` serves a patched `Entrance_Music_E_S` bank (carrier theme 9990: 16 events `Play_MUS_9990_m_v`, a random container under actor-mixer 0x078CE9F2, one PCM Sound) past the end of `Game_Sound_E_S.pck`, adds a stream-table entry to `Game_Sound_M.pck`, and serves the selected song as RIFX PCM (format 0xFFFE, 48 kHz stereo, big-endian) through a file overlay (`file.rs`). `sub_8334C138` (Game_EntranceMusicRequest, used only by Create an Entrance's preview) redirects id 0x270E to 9990 too. `WWE14_ENTRANCE_WWISE=off` restores the dry native path. 7 new tests.
- **Unlock everything (the user's request, 2026-11-26): done.** `kernel::unlock` (`WWE14_UNLOCK_ALL=1`) calls the game's `sub_822BCB68(id)` + `sub_822BF598` (seen) for every enabled id 3..0xC3 except the DLC-gated 0xA5..0xA9; re-applied about once a second because the game reads the save into `0x83891878` more than once. The user saved in user82: the file has all 160 unlock bytes (checked by decompressing `SaveData.Dat`, unlock bytes at 0x1C+0x1224AC+0x80394). Not needed any more for this save.
- **DLC (2026-11-26): six add-on packs installed and listed.** New tool `tools/stfs` (`stfs info|extract <package> [out]`, our own STFS reader). Packs extracted to `Content\0000000000000000\545408B2\00000002\<42-char name>` with `.displayname` files; `xam` now lists 42-character names and reports type-2 content fully licensed.
- **Exe renamed `WWE2K14.exe` with the title's icon (2026-11-26):** recomp writes `game/build.rs` (winresource, `private\icon\wwe2k14.ico` from the XDBF title image 0x8000); `host::window::set_window_icon` sets the small and taskbar icons from resource 1. Process name is now `WWE2K14`.
- **Title Update 1: patched executable built and verified (2026-11-26).** `tools/xex` now has our own LZX decoder (`lzx.rs`, delta windows), SHA-1, and `patch.rs`; `xex apply-patch <base> <xexp> <out>` writes a plain xex. `private\tu1\default.xex` (v0.0.1.2) matches all 422 page hashes of the TU's signed headers. Code moved almost everywhere (.text +0x3730; 58,524 functions; only the first 43 unchanged), so every hard-coded guest address in config and runtime must be remapped before recompiling from it. **Matcher built (tools/analyse `match`, `map-data`, `remap`):** all 279 literals mapped (278 at confidence ≥ 0.9, one 0.85 with matching contents); data addresses move by different amounts (+0x40, +0x50, .rdata 0..+0x4F0), so use the per-address map in `private\tu1\remap.tsv`. Next: after the mixer-songs work lands, rerun `analyse remap`, substitute all literals, point recomp at `private\tu1\default.xex`, install the TU's pac/movies/plist files (back up the disc's), re-harvest shaders from the updated pacs, regenerate, rebuild, test. The TU's updated pac/movies/plist files (in the package, extracted to `private\tu1\pkg`) must also replace the disc's. Earlier note:
- **Title Update 1 (the user wants it applied):** package at `%USERPROFILE%\Downloads\WWE2K14_TU1\545408B2\000B0000\tu00000002_00000000` (LIVE; `default.xexp` 2 MB plus updated pac/movies/plist). Needs our own LZX-delta decoder in `tools/xex` (none exists; the disc xex uses basic compression), then a re-check of every hard-coded guest address in the config and natives against the patched image, a full recompile and retest.
- **Camera cut frame fixed 2026-11-26 (user89: "I think it's gone"; 11 cuts caught):** `gpu::present::camera_update` wraps the camera update `sub_825B69E0` (vtable slot `0x8201EFA4`); on an odd scene frame in a half-rate scene, a position jump over 8x the previous frame's motion is the 50/50 blend before a cut, and the next two frames are rendered but not shown (`ir::Frame::hold`, `ir::HOLD_FRAMES`), so the window keeps the previous picture. Restoring the camera's own state did nothing (the picture is decided earlier). Earlier: the `sub_825B6848` wrapper (`gpu::present::camera_clock`) runs, but the event tables at `[0x836EE3B0]` are empty during entrances (user82), so the event lookup never finds cuts. Next: a lookahead instead (evaluate the pose at n+1 and n-1 on odd frames; a large jump means a cut; show n-1).
- **Custom arena thumbnail fixed 2026-11-26 (user86, confirmed):** `resolve::read_on_cpu` now also holds 1280x720 format-6 destinations; a destination read 3 times in 10 s, or read by a host thread (the renderer's hasher: that caused a 3 s freeze in user86), is never held again. The thumbnail read back from `0xFF1E8000` (148 ms). Earlier notes: user83-85: no new picture size after Accept; 512x512 holds never read. With full-screen resolves traced (F6 now logs each destination once per window), Accept showed a 1280x720 format-6 resolve at an unusual address (`0xFF1E8000`, caller `0x82BB4528`), likely the thumbnail screen capture. Holding every 1280x720 destination is unsafe (the renderer's hasher reads some scene ones every frame; each read would cost a 3.6 MB readback). Next: hold 1280x720 destinations from `0x82BB4528` with a per-address limit (drop the hold after repeated reads), or find the capture call (`sub_827DC770`'s screen capture, survey in STATUS Phase 6).
- **Multi-select USER PLAYLIST (the user's request, 2026-11-26): built and confirmed by the user (user60, "jukebox works").** `runtime/kernel/src/jukebox.rs` wraps `sub_82B3A4B8` (input), `sub_82B397B8` (rows) and `sub_82ACD5D8` (next-song pick) via three new `[[replace]]` entries (`Game_Jukebox*` in `config/wwe14.toml`, resolved in `kernel::crt`). Selection kept in `music\.wwe14-playlist`. Needs a regenerate (`cargo run --release -p recomp -- private\game\default.xex generated\rust-locals --locals on`) and a full rebuild, with the game closed. Not type-checked yet.
- **Belt cause found 2026-11-26 (user55-57):** the CPU-baked texture is right (red with the flag, `tex_*.png`), the region colours c16-c19 are right, and forcing t0 white still drew black. The shader translator's `setp_inv` (scalar op 31) left 1 as 1 instead of 0, so the belt shader's if/else-if chain (`setp_*_push`/`setp_inv`/`setp_pop` on r0.w) lost its place and fell through to `r6 = r6.x * c16` = black. Fixed in `tools/xenos-shader-recomp/src/wgsl.rs`; all 1,440 shaders retranslated (385 use `setp_inv`; old ones in `private\pac-wgsl.before-setp-inv`). **Confirmed by the user in user58: "the belt is good now".** The other 384 changed shaders are not yet seen in play; watch for changed looks.
- **Earlier next step for the belt (superseded):** launch, open Create a Championship, set a bright plate colour and add a logo, press F9 on the belt and on the logo placement screen, then look at the `tex_*.png` of the belt draw (vs `65b119fa33b4486d`): empty atlases or layers mean the inputs are missing; coloured layers with a blank DXT result point at `vpkd3d128`/`vupkd3d128` in the conversion code.
- **Built while the user was away (20:20, full rebuild 11 min), confirmed by the user 2026-11-26 (user55):** the 30 Years intro sound fix, four-lane SSE vector float ops in `runtime\ppc-rt\src\vmx.rs` (`vmx::tests::simd_float_matches_lanes` and the KAT suite pass). On the next launch, check the 30 Years intro: no sound gaps, voices in sync with the subtitles, and the `audio mix: N ms of silence` lines gone from the log. The build before it is kept at `private\builds\2026-11-25-before-simd\wwe14.exe` in case anything regresses.
- **Saved for the end by the user:** PC keyboard typing into the game's name grid; the dark line/VHS filter in 30 Years; custom songs through the game's mixer (arena reverb).
- The last log is `private\user54.err`; the next is `user55`.

## Next, in order

1. Keyboard check, user39: Create a Superstar's name uses the game's own letter grid (controller), not the system keyboard; renaming and saving worked. The PC typing panel is still unseen: it shows only where the game calls `XamShowKeyboardUI` (`sub_823891A8`; screen not identified). PC typing into the game's grid is Phase 8.
   - **Written, set aside, not built (the user said "not now", 2026-11-25):** four-lane SSE vector float ops, the fix for the 30 Years intro's sound gaps (STATUS Phase 6). The full file is `private\pending\vmx.rs.simd`; `runtime\ppc-rt\src\vmx.rs` is back to the original (mtime set to 08:00 so kernel-only builds stay quick). To apply: copy it over `vmx.rs`, `cargo test -p ppc-rt`, the KAT suite, then the ~20 min full rebuild of `generated\rust-locals`. Only when the user agrees (it blocks play).
   - **Advanced Entrance Creation stopped the game (user47):** `XamCreateEnumeratorHandle` (the music player listing the console's own music: app 0xFA, messages 0x70026/0x70027, items 0x15A8, 80 a call). Now native in `kernel::xam`, answering an empty list. Built 2026-11-25 18:5x (kernel-only build, 19 s); being tried in user48.
   - **Custom music (the user asked for it after all, 2026-11-25): built and working (user49: "it works").** The console's music player is native: `kernel::music` (folder `music\` or `WWE14_MUSIC_DIR`, symphonia decoders for MP3/WAV/FLAC/Ogg/AAC/ALAC/AIFF/MKA, mixed over the game's output in `kernel::audio::submit`) and the app-0xFA messages in `kernel::xam` (listing 0x70025 with 0x334-byte items titled at +0x23C, play 0x7003B by title, stop/pause/continue, status 0x70009, volume, current song 0x7000E, playback controller). Used by Advanced Entrance Creation (custom entrance music) and the Jukebox's custom-soundtrack option.
     - The "All songs" entry was removed (the user: the Jukebox holds several songs); the listing is the folder's songs only.
     - A shuffle over the menus (`WWE14_MENU_MUSIC=on`) exists but is off: the user wants the Jukebox instead.
     - Custom songs sound dry next to the game's own entrance music: they don't pass through Wwise's arena reverb (the console's music player mixed outside the title too; not checked against footage). **Saved by the user for the end:** "have custom songs go through the game's mixer" (Wwise, for the arena reverb). Idea: the game's sounds are Wwise packages (`private\game\sound\*.pck`, AKPK; music in `Game_Sound_M.pck`, 818 MB); a custom entrance would play a built-in theme's Wwise event with that stream's data swapped for the song, converted to the codec Wwise expects (format of those streams not yet checked).
   - 30 Years of WrestleMania: opens (music-player fix built), first match played.
   - Universe (user46-47): simulated April to SummerSlam, autosaved, and loaded back at SummerSlam after a restart. No fatal stop. Done.
   - Creation suite (user47-52): Create an Arena, Advanced Entrance Creation (after the music-player fixes), Create a Move-set and Create a Story (with preview) all saved. Create a Championship is the open one (belt colours and logos, below). All of them, and an Options change (Jukebox set to the custom soundtrack), loaded back after a restart (user53, "all kept").
   - **mimalloc removed (the user asked for no C in the build, 2026-11-25):** the process uses Rust's system allocator. The profiles had shown mimalloc at 4-6%; watch for slower crowded shots.
   - **Create a Championship: the belt stays black and grey, no colours or logos (user49-51).** The belt draws with two 512x512 DXT5 textures (`0x1E70C000`, `0x1E790000`) plus 8888 textures at `0x1EC96000` (128x128) and `0x1ECA6000` (1x1); no pass redraws them per frame. Changing a colour costs a 214 ms main-thread burst in `sub_83336898` <- `sub_83335428` <- `sub_833427C0` (CPU work). Leading guess: a GPU-drawn picture the CPU reads back (like the superstar portrait), or a CPU-written texture the capture doesn't refresh. user51 ran `WWE14_WATCH_RESOLVE=every` (new: watches each resolve destination once and reports the first CPU access): no CPU read. Static trace (2026-11-25): colours and logos are baked on the CPU. `sub_83336898` stores 4 colours, `sub_83335428` rebuilds each part: `sub_8332F760` composites layer images (paletted layers tinted by `sub_8332D288`), `sub_8332FC20` creates a LIN_DXT5 texture and `sub_822B5808` compresses into it mip by mip (LockRect `82CA8628`, conversion `82CFD8F0` with vpkd3d/vupkd3d, UnlockRect). Logos go into two 512x512 LIN_A8R8G8B8 atlases (`sub_83330080`), which looked empty in F9 frame 51407. Suspects: the layer images are empty in guest memory, or `vpkd3d128`/`vupkd3d128` formats 1-5 are wrong. Next: F9 now also writes every sampled texture as `tex_*.png` (render crate, written, not built) to see the atlases and the DXT output.
   - **Parked by the user for the end (2026-11-26): custom Jukebox songs keep playing in the 30 Years of WrestleMania menu, hiding its era music (Hulk Hogan's theme).** Tried and not working: pausing on `XMPSetPlaybackController` (only sent with the intro video), and hooks on `sub_82ACCFC8` (menu music playing?), `sub_82ACCA50` (stop) and `sub_82ACE298` (play a named event). user72: the game already reports the custom song as playing (3); the stop and named-play hooks didn't fire on entering the menu. user73's log (`private\user73.err`, `jukebox hook:` lines) is unread. Next: find what actually starts the era music (a script opcode `sub_82B57B78`? a different player) before hooking anything more.
   - **Saved by the user for the end (2026-11-25):** PC typing into the game's grid, and the faint dark line across wrestlers' legs in 30 Years of WrestleMania (maybe the mode's VHS filter; see STATUS Phase 6). Don't pick them up until the modes are done.
2. Have the user play each mode they have not played in the port, one at a time, and fix each stop statically (config, a native function, a rebuild): 30 Years of WrestleMania (one match), Universe (advance one show), the creation suite (arena, entrance, move-set, championship, story), options (change one, restart, check it kept).
3. For each mode, check that its save loads back after a restart.
4. What a new mode can stop on was surveyed statically; the result is in STATUS, Phase 6, "What another mode can stop on". In short: three never-run D3D calls (recording a command buffer, the untiled present, a screen capture) and the unanswered system calls listed there. None is known to be reachable from a mode; each stops loudly and names itself if it is.

## Builds

- `generated\rust-locals\target\release\wwe14.exe`: registers in host locals. **The one the user plays.** Built 2026-11-25 17:07.
- `generated\rust\target\release\wwe14.exe`: registers in the context; the fallback. Built 2026-11-25 17:13 from the same sources.
- To rebuild after a runtime change: `cargo build --release` inside `generated\rust-locals` (about 20 s for a kernel or host change; minutes if Bevy's features change). Run cargo from PowerShell, not Git Bash.
- To regenerate the recompiled code (only after a recompiler or config change): `cargo run --release -p recomp -- private\game\default.xex generated\rust-locals --locals on`. The config has `[locals] enabled = false`; the switch on the command line makes the locals build.
- The project folder is not a git repository. Nothing is under version control, so there is no way back from a bad edit except these notes.

## Portable release (2026-11-26)

The user asked for a portable copy for their laptop: `D:\WWE2K14-Portable`.
- The game's own `WWE2K14.exe` sits at its top with `vcruntime140.dll` (the user didn't want a launcher). `runtime/host/src/portable.rs`: when `game\default.xex` is beside the exe, that folder becomes the working directory and `WWE14_SHADERS`, `WWE14_MUSIC_DIR`, `WWE14_PIPELINE_KEYS`, `WWE14_PIPELINE_CACHE` default to its `shaders\`, `music\`, `cache\`. The kernel finds `hdd\` from the xex's folder.
- **The exe has no console now (`windows_subsystem = "windows"`, in the generated main).** The log goes to `WWE14_LOG` if set, else to a redirected stderr, else `logs\last-run.log` beside the exe. In the release, a fatal stop also shows a message box (`kernel::diag::FATAL_LOG`).
- `source\` is the source tree without `target`, `generated`, `private`, `music`. Zipped (stored, no compression) to `D:\WWE2K14-Portable.zip`.
- **GitHub:** the source is public at `waidan2232/wwe14-recomp` (pushed 2026-11-26, the user's request). Commit as `waidan2232` with the GitHub noreply address (local git config). `.gitignore` excludes `private/`, `generated/`, `target/`, `music/`.
- To refresh it after a build: copy both exes again (and `private\pac-wgsl` if shaders changed).
- The build needs AVX2 (`target-cpu=x86-64-v3`); the launcher checks and says so.

## Launching for the user

In a window the user can see. Never headless, never hidden.

```powershell
Start-Process powershell -WorkingDirectory D:\wwe14-recomp -ArgumentList '-NoExit','-Command',"`$env:WWE14_HITCH_WATCH='50'; .\generated\rust-locals\target\release\wwe14.exe --trace threads 2> private\userNN.err"
```

- The last log is `private\user38.err`; the next is `user39`.
- First copy `private\hdd\partition1\Content` to `private\save-backups\<timestamp>\Content`. Copy only: never move or delete the user's saves without telling them first.
- Check `Get-Process -Name wwe14` in the same command, and that no `cargo` or `rustc` is running.
- Switches: `WWE14_PROFILE_AUTO=120` (a profile every two minutes and at each entrance; play is slightly uneven while it samples), `WWE14_PROFILE_DRAWS=2500` (a profile in crowded shots), F10 (a 15 s profile now), F9 (a snapshot of the next frame), `WWE14_INPUT_SOURCES=1` (which physical button each press comes from), `WWE14_WATCH_RESOLVE=all`, `WWE14_FRAME_RATE=native`.
- To know when a session ends, run a background watcher that first sees the process and then sees it gone several checks in a row. A watcher that trusted one check reported the game closed while the user was in it.

## Working with the user

- Plain language: what changed for the game and what they need to do. They do not follow recompiler or graphics terms.
- They play the tests. Tell them exactly what to do and what to report, in a few lines. They have done many test runs today; ask for one only when the work needs it.
- **Never build while the game is open.** A full build made the game stutter for them (user36).
- They want autonomy: pick defaults and keep going. But follow the plan's order, and stop a line of work when they say to park it.
- Name every visible defect. Check the original game before calling a look a bug.
- The user's saved notes for the assistant (the memory folder for this project) hold these rules with their reasons.

## Things that cost time today

- **Bevy is 0.20.1 and newer than what the assistant may know.** Read its real API in the cargo registry before writing host code: `%USERPROFILE%\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\bevy_*-0.20.1`. Example: keyboard events are `MessageReader<KeyboardInput>`, and font sizes are `FontSize::Px(..)`.
- `cargo check -p host` in the main workspace compiles all of Bevy. Build inside `generated\rust-locals` instead; it reuses what is built.
- The game's `XamContentCreateEnumerator` listing was a silent stub that returned nothing, and the game concluded saved superstars were missing. When a native function answers "nothing" without looking, treat it as a suspect.
- A fix can work and still fail: the picture readback waited for the game's next frame while the game waited for the picture. Check the log for the line that says a step succeeded, not only that it ran.

## Tools

- `analyse` (`target\release\analyse.exe`, or `cargo run --release -p analyse --`): `dis <xex> <addr> <count>`, `callers <xex> <addr or import>`, `refs`, `fns <xex> <lo> <hi>`, `reach <xex> <function> [cut ...]`, `effects`. `callers` counts `bl` only: XAPI wrappers reach imports with a plain `b`.
- `private\trace3_called_addr.txt`: every function that ran in the traced session of 2026-11-25 (27,479 of 83,321). A function missing from it has not been seen running.
- The sampling profiler reports by function and by source line of the runtime's own code; see STATUS, "Measuring it".
- Unit tests: `cargo test --release -p kernel`, `-p gpu`; the 1,520 instruction tests in `tools/kat`.

## Open defects (all parked; details in STATUS)

- John Cena's title belt twitches through his entrance. **Confirmed 2026-11-26 (user74): at `WWE14_FRAME_RATE=native` the belt is fine, so the 60 lock causes it.** `sub_82BB6D50` (Game_HalveFrameRate, a native no-op under the lock) sets the frame-rate struct at `0x8392DC38`: +8 rate 30, +0x28 33 ms, and five floats (+0x1C, +0x20, +0x24, +0x3C, +0x40). Some system stepping the belt misbehaves at the 60 values in entrances. **Parked by the user for the end (2026-11-26).** **user75: still twitches, and the settle path never ran (no `cloth: settling` line), so the cut-settle theory is wrong; the wrapper is harmless and stays.** Remaining suspects: the warm-up frame counter in `sub_826427E8` (+0xDC, counts frames), or the belt cloth simply unstable at dt=1/60. Built earlier: the strap is Havok cloth; at a camera cut `sub_824F6350` resets it and settles it with a fixed 14 steps of 1/rate via `sub_824F56E8`, half the settling at 60. `kernel::cloth` wraps `sub_824F56E8` and gives those settle calls (return addresses `0x824F63E8`, `0x824F6450`) a 30 rate; logs `cloth: settling after a cut`. If the twitch remains, next suspect: the per-instance warm-up frame counter in `sub_826427E8` (+0xDC). Earlier note: find which reader of those fields drives the belt (attachment/cloth/physics step) and give it the 30-rate values only.
- A wrong camera for one frame at entrance and victory cuts.
- A dark crowd for one frame at in-match cuts in the TLC arena.
- A short stutter inside the game's own audio mix (heard once, no underrun on the device).

## Settled today (2026-11-25)

- Create a Superstar: save, picture, and loading back after a restart all work. Two fixes: the picture is read back from the renderer when the game reads it, and the saved-content listing now lists what is on the disk.
- "Save data keeps getting cleared": the listing bug above.
- The pause menu at the bell: a third START press on the controller during the lead-in to the bell, not the port (user38: hands off, no press, no pause).
- Stutter at character select in user36: a background build started by the assistant, not the game.
