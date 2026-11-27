//! Running from a release folder, and where the log goes.
//!
//! The release is a folder with the game's exe at its top:
//!
//! ```text
//! WWE2K14.exe        the game
//! game\              the disc's files (default.xex, pac, sound, movies, ...)
//! hdd\               the console's hard disk: saves, downloaded content (the kernel finds it beside game\)
//! shaders\           pac-wgsl\ and xex-wgsl\, translated ahead of time
//! cache\             the pipeline list and the driver's compiled pipelines
//! music\             custom songs
//! logs\              last-run.log
//! ```
//!
//! When `game\default.xex` sits beside the exe, the game runs from there:
//! that folder becomes the working directory and the xex, shader, pipeline and
//! music paths point into it (environment variables already set win). In the
//! project the exe lives under `generated\`, nothing is beside it, and the
//! recompile-time paths stay.
//!
//! The exe has no console (`windows_subsystem = "windows"`). Its log goes to
//! `WWE14_LOG` when that is set; otherwise to stderr when the starter gave it
//! one (a redirect); otherwise to `logs\last-run.log` beside the exe.

use std::os::windows::io::IntoRawHandle;
use std::path::{Path, PathBuf};

const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
const STD_ERROR_HANDLE: u32 = -12i32 as u32;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetStdHandle(which: u32) -> isize;
    fn SetStdHandle(which: u32, handle: isize) -> i32;
}

/// Before anything logs or starts a thread.
pub fn setup(p: &mut kernel::Program) {
    let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) else { return };
    let xex = dir.join("game").join("default.xex");
    let release = xex.is_file();

    let log = std::env::var_os("WWE14_LOG").map(PathBuf::from).or_else(|| {
        // SAFETY: reads the process's standard handle.
        let stderr = unsafe { GetStdHandle(STD_ERROR_HANDLE) };
        (stderr == 0 || stderr == -1).then(|| dir.join("logs").join("last-run.log"))
    });
    if let Some(path) = &log {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(f) = std::fs::File::create(path) {
            // Rust's stdout and stderr look the handle up on every write.
            let h = f.into_raw_handle() as isize;
            // SAFETY: the handle is a file opened for writing that stays open for the life of the process.
            unsafe {
                SetStdHandle(STD_ERROR_HANDLE, h);
                SetStdHandle(STD_OUTPUT_HANDLE, h);
            }
            let shown = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
            let shown = shown.to_string_lossy().trim_start_matches(r"\\?\").to_owned();
            if release {
                let _ = kernel::diag::FATAL_LOG.set(shown);
            }
        }
    }

    if !release {
        return;
    }
    let _ = std::env::set_current_dir(&dir);
    p.default_xex = Box::leak(xex.to_string_lossy().into_owned().into_boxed_str());
    let shaders = std::env::join_paths([dir.join("shaders").join("pac-wgsl"), dir.join("shaders").join("xex-wgsl")]).expect("the shader folders");
    let _ = std::fs::create_dir_all(dir.join("cache"));
    for (name, value) in [
        ("WWE14_SHADERS", shaders),
        ("WWE14_MUSIC_DIR", dir.join("music").into_os_string()),
        ("WWE14_PIPELINE_KEYS", dir.join("cache").join("pipeline-keys.txt").into_os_string()),
        ("WWE14_PIPELINE_CACHE", dir.join("cache").join("pipeline-cache.bin").into_os_string()),
    ] {
        if std::env::var_os(name).is_none() {
            // SAFETY: called first thing in main, before any other thread exists.
            unsafe { std::env::set_var(name, value) };
        }
    }
    eprintln!("release folder: {}", dir.display());
}
