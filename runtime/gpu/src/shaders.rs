//! The shaders a draw uses, as D3D holds them, and their harvest for offline
//! translation (`WWE14_SHADER_DUMP=<dir>`).
//!
//! Shaders are translated ahead of time (`tools/xenos-shader-recomp`), never
//! at run time. The game creates most of its shaders from its `.pac` data, and
//! D3D patches each vertex shader's fetch instructions for the vertex
//! declaration in use, so the microcode that a draw actually binds is
//! collected here: each new one is written out once, named by its identity
//! (`ucode::identity`, as the offline harvest names them), for
//! the offline translator.
//!
//! Layout (from D3D's shader upload in the draw path, `sub_82CBE558`):
//! - pixel shader object `ps` (device `+0x330C`): record `r = ps + ps[0x40]`;
//!   microcode at `ps[0x18] + r[0x28]`, `r[0x2C]` bytes.
//! - vertex shader object `vs` (device `+0x3310`): variant `i` = 1 if
//!   `vs[0x368] & 0x20`, else 0; record `r = vs + vs[0x380 + 8i]`; microcode at
//!   `vs[0x20] + r[0x368]`, `r[0x36C]` bytes.

use std::collections::HashSet;
use std::sync::Mutex;

use kernel::{diag, mem};
use ppc_rt::Ctx;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Stage {
    Vertex,
    Pixel,
}

/// The microcode bound for a stage: guest address and size in bytes.
pub fn bound(dev: u32, stage: Stage) -> Option<(u32, u32)> {
    match stage {
        Stage::Pixel => {
            let ps = mem::r32(dev + 0x330C);
            if ps == 0 {
                return None;
            }
            let r = ps.wrapping_add(mem::r32(ps + 0x40));
            Some((mem::r32(ps + 0x18).wrapping_add(mem::r32(r + 0x28)), mem::r32(r + 0x2C)))
        }
        Stage::Vertex => {
            let vs = mem::r32(dev + 0x3310);
            if vs == 0 {
                return None;
            }
            let i = (mem::r32(vs + 0x368) & 0x20 != 0) as u32;
            let r = vs.wrapping_add(mem::r32(vs + 0x380 + 8 * i));
            Some((mem::r32(vs + 0x20).wrapping_add(mem::r32(r + 0x368)), mem::r32(r + 0x36C)))
        }
    }
}


static SEEN: Mutex<Option<HashSet<u64>>> = Mutex::new(None);

/// D3D's vertex-shader patcher (`sub_82CBE418(device, vs, declaration, variant)`):
/// rewrites the variant's vertex-fetch instructions for the bound vertex
/// declaration (`+0x2FD0`) and stream strides (`+0x3268`), with a cache so it
/// only patches on a change. It is CPU-only (it edits microcode in memory) and
/// runs as recompiled code; D3D's draw-time flush, which the native draws
/// replace, is what called it.
const PATCH_VERTEX_SHADER: u32 = 0x82CB_E418;

/// Brings the bound vertex shader up to date for the current vertex
/// declaration, as D3D does before every draw.
pub fn prepare(c: &mut Ctx, dev: u32) {
    let vs = mem::r32(dev + 0x3310);
    if vs == 0 {
        return;
    }
    let decl = mem::r32(dev + 0x2FD0);
    let variant = (mem::r32(vs + 0x368) & 0x20 != 0) as u64;
    if kernel::thread::call(c, PATCH_VERTEX_SHADER, &[dev as u64, vs as u64, decl as u64, variant]) as u32 == 0 {
        diag::fatal("D3D's vertex shader patcher reported the shader in use by the GPU, which can't happen here");
    }
}

/// With `WWE14_SHADER_DUMP` set, writes out each new shader bound at a draw
/// (named by identity) and returns their hashes. Otherwise does nothing: it
/// copied and hashed both shaders' microcode at every draw, 7% of the render
/// thread in a busy entrance, for a result no one used.
pub fn harvest(dev: u32) -> [Option<u64>; 2] {
    static DIR: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    let dir = DIR.get_or_init(|| {
        let d = std::env::var_os("WWE14_SHADER_DUMP").map(std::path::PathBuf::from);
        if let Some(d) = &d {
            let _ = std::fs::create_dir_all(d);
        }
        d
    });
    [Stage::Vertex, Stage::Pixel].map(|stage| {
        let (addr, size) = bound(dev, stage)?;
        if size == 0 || size > 0x2_0000 || size % 12 != 0 {
            diag::fatal(&format!("{stage:?} shader at 0x{addr:08X} has an implausible size of {size} bytes"));
        }
        dir.as_ref()?;
        let code = mem::read(addr, size as usize);
        // Named by identity, as the offline harvest names them.
        let h = xenos_shader_recomp::ucode::identity(&code);
        let mut seen = SEEN.lock().unwrap();
        if seen.get_or_insert_with(HashSet::new).insert(h)
            && let Some(dir) = dir
        {
            let name = format!("{}_{h:016x}.bin", if stage == Stage::Vertex { "vs" } else { "ps" });
            match std::fs::write(dir.join(&name), &code) {
                Ok(()) => diag::log(&format!("shader harvested: {name} ({} instructions)", size / 12)),
                Err(e) => diag::log(&format!("shader dump {name}: {e}")),
            }
        }
        Some(h)
    })
}
