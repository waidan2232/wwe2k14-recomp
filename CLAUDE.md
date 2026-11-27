# CLAUDE.md

## HARD RULE: ABSOLUTELY NO EMULATION

This is the user's standing instruction for this project. It overrides every other consideration, including convenience, schedule, and "just to get it booting".

**Never add:**
- An interpreter, JIT, dynarec, or any "fallback" that executes PPC code at runtime.
- Hardware emulation of any Xbox 360 component: the Xenos GPU (PM4 packets, ring buffer, register file, EDRAM), the XMA decoder, MMIO devices, or the CPU's MMU, caches, interrupts or cycle timing.
- Runtime shader translation or interpretation.
- Any use of an emulator (Xenia or others) in the build, the tests, or the runtime.

**Allowed (this is porting, not emulation):**
- Ahead-of-time PPC → Rust recompilation (XenonRecomp-style). The guest register context struct and the big-endian 4 GiB guest address space are part of the translation ABI.
- Native Rust implementations of OS and library API contracts: xboxkrnl, XAM, D3D9-Xbox, XAudio2 (the Wine model).
- Offline Xenos shader → WGSL translation, done at build time.
- Data conversion at API boundaries: endianness, texture untiling, format mapping.
- Software codecs, such as XMA2 decoding in software.
- The game's own embedded interpreters (Lua). They are game code and are recompiled like everything else.

**When something is unhandled,** it must fail loudly with a diagnostic that names the guest address, function, or import. It then gets fixed statically through config, a hook, or a rebuild. This covers an unknown indirect-call target, an unimplemented import, an MMIO touch, and a missing shader. Never add a "temporary" emulation path. If you are unsure whether something counts as emulation, stop and ask the user.

## Project

- The plan is in [PLAN.md](PLAN.md). Read it before working.
- `private/` (disc image, extracted game files, decrypted xex) and `generated/` (recompiled code) are derived from copyrighted material. Never commit them or publish them.
- Windows builds need the MSVC linker (Visual Studio Build Tools with the C++ workload and the Windows SDK). Git Bash's `/usr/bin/link` shadows MSVC `link.exe`, so run `cargo` from PowerShell.
- Write our own PPC decoder. XenonRecomp is MIT, but its bundled disassembler is binutils-derived GPL code.
