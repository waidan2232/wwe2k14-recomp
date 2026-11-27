//! Native implementation of the Xbox 360 kernel (`xboxkrnl.exe`) and system
//! library (`xam.xex`) API contracts, and the boot sequence that runs the
//! recompiled game on them.
//!
//! This is the Wine model: the game's code was translated ahead of time; the
//! operating system it calls is reimplemented natively. Nothing here
//! interprets guest code or models console hardware. Anything not implemented
//! stops the game with a diagnostic naming it (see CLAUDE.md).

pub mod abi;
pub mod audio;
pub mod diag;
mod file;
mod loader;
pub mod music;
pub mod jukebox;
pub mod entrance;
pub mod cloth;
pub mod unlock;
pub mod mem;
pub mod object;
mod printf;
pub mod bake;
pub mod bpe;
pub mod crt;
pub mod profile;
pub mod rtl;
mod sha1;
pub mod status;
pub mod thread;
pub mod trace;
mod time;
pub mod video;
mod xam;

/// The host reports controller 1 through these (`XamInputGetState` reads them).
pub use xam::{KeyboardRequest, Pad, keyboard_finish, keyboard_host_attached, keyboard_request, set_pad};
mod xboxkrnl;

use std::path::PathBuf;
use std::sync::atomic::Ordering;

use ppc_rt::{Ctx, GuestFn, ImportDef, ReplaceDef};

/// Everything the generated game binary hands to the kernel.
pub struct Program {
    /// Every recompiled function, sorted by guest address.
    pub functions: &'static [(u32, GuestFn)],
    /// Every imported function, in the order of the generated stubs.
    pub imports: &'static [ImportDef],
    /// Functions the config replaces with native code, in the order of their stubs.
    pub replacements: &'static [ReplaceDef],
    /// Native-code providers (the GPU layer, ...), asked in order for each replacement by name.
    pub natives: &'static [fn(&str) -> Option<GuestFn>],
    /// Hash of the decrypted image the code was recompiled from.
    pub image_hash: u64,
    /// Path of `default.xex` at recompile time (overridable with `--xex`).
    pub default_xex: &'static str,
    /// The recompiled functions keep registers in host locals (`recomp`'s
    /// register analysis), so the context a crash report prints is stale for
    /// those registers.
    pub registers_in_locals: bool,
}

/// The native implementation of an import, by library and export name.
pub fn resolve(library: &str, name: &str) -> Option<GuestFn> {
    if library.eq_ignore_ascii_case("xboxkrnl.exe") {
        xboxkrnl::resolve(name)
    } else if library.eq_ignore_ascii_case("xam.xex") {
        xam::resolve(name)
    } else {
        None
    }
}

/// Guest address handed out by `XexGetProcedureAddress` for a native export.
/// These sit in the first kernel page range, which is never allocated, and
/// are only ever called (through the function table), never read.
pub(crate) fn procedure_address(library: &str, ordinal: u32) -> u32 {
    let base = if library.eq_ignore_ascii_case("xam.xex") { 0x8000_4000 } else { 0x8000_1000 };
    base + 4 * (ordinal & 0xFFF)
}

const USAGE: &str = "usage: wwe14 [--xex PATH] [--trace imports,threads,files,audio,draws,memory] [--gpu-boundary stop|park] [--dump-threads SECONDS] [--input BUTTON@SECONDS|BUTTON@fFRAME,...] [--peek ADDR:LEN@SECONDS] [--profile START:SECONDS] [-- game arguments]";

/// Boots the game on this thread's behalf and waits for it. Returns only by
/// exiting the process.
pub fn main(p: Program) -> ! {
    boot(p, std::env::args().skip(1).collect()).wait()
}

/// The running game: its main guest thread.
pub struct Booted {
    main: std::sync::Arc<thread::Thread>,
}

impl Booted {
    /// The guest main thread's exit code, once it has returned.
    pub fn finished(&self) -> Option<u32> {
        self.main.finished.load(Ordering::Relaxed).then(|| self.main.exit_code.load(Ordering::Relaxed))
    }

    /// Waits for the guest main thread, then exits the process with its code.
    pub fn wait(self) -> ! {
        loop {
            std::thread::sleep(std::time::Duration::from_millis(50));
            if let Some(code) = self.finished() {
                diag::log(&format!("the main thread returned 0x{code:08X}"));
                std::process::exit(code as i32);
            }
        }
    }
}

/// Boots the game: loads the executable, installs imports and replacements,
/// mounts the devices and starts the title's main thread, then returns. The
/// host (Bevy) keeps the calling thread. `args` are the kernel's command-line
/// arguments.
pub fn boot(p: Program, args: Vec<String>) -> Booted {
    // The console's kernel times waits and sleeps to the millisecond. Windows
    // rounds a timed wait up to its timer tick, 15.6 ms unless a process asks
    // for finer; the game's short timed waits would then stall for most of a
    // 60 Hz frame. Ask for 1 ms for the whole run.
    #[cfg(windows)]
    {
        #[link(name = "winmm")]
        unsafe extern "system" {
            fn timeBeginPeriod(period: u32) -> u32;
        }
        let r = unsafe { timeBeginPeriod(1) };
        if r != 0 {
            diag::log(&format!("timeBeginPeriod(1) failed ({r}): timed waits round to the system tick"));
        }
    }
    if let Some(ms) = std::env::var("WWE14_HITCH_WATCH").ok().and_then(|v| v.parse::<u64>().ok()) {
        profile::watch_hitches(std::time::Duration::from_millis(ms));
    }
    profile::auto();
    diag::REGISTERS_IN_LOCALS.store(p.registers_in_locals, Ordering::Relaxed);
    let mut xex = PathBuf::from(p.default_xex);
    let mut trace = std::env::var("WWE14_TRACE").unwrap_or_default();
    let mut game_args: Vec<String> = Vec::new();
    let mut args = args.into_iter();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--xex" => xex = args.next().map(PathBuf::from).unwrap_or_else(|| usage()),
            "--trace" => trace = args.next().unwrap_or_else(|| usage()),
            "--input" => {
                let spec = args.next().unwrap_or_else(|| usage());
                if let Err(e) = xam::set_input_script(&spec) {
                    eprintln!("--input: {e}");
                    usage();
                }
            }
            "--dump-threads" => {
                let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or_else(|| usage());
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(secs));
                    diag::dump_threads();
                });
            }
            "--peek" => {
                // ADDR:LEN@SECONDS (hex address and length): logs a hex dump of guest memory then.
                let spec = args.next().unwrap_or_else(|| usage());
                let parsed = (|| {
                    let (range, secs) = spec.split_once('@')?;
                    let (addr, len) = range.split_once(':')?;
                    Some((u32::from_str_radix(addr, 16).ok()?, u32::from_str_radix(len, 16).ok()?, secs.parse::<f64>().ok()?))
                })();
                let Some((addr, len, secs)) = parsed else { usage() };
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs_f64(secs));
                    let bytes = mem::read(addr, len as usize);
                    let mut s = format!("peek 0x{addr:08X} (0x{len:X} bytes) at {secs} s:");
                    for (i, row) in bytes.chunks(16).enumerate() {
                        s.push_str(&format!("\n  {:08X}:", addr + 16 * i as u32));
                        for w in row.chunks(4) {
                            s.push(' ');
                            w.iter().for_each(|b| s.push_str(&format!("{b:02X}")));
                        }
                    }
                    diag::log(&s);
                });
            }
            "--profile" => {
                let spec = args.next().unwrap_or_else(|| usage());
                if let Err(e) = profile::start(&spec) {
                    eprintln!("--profile: {e}");
                    usage();
                }
            }
            "--gpu-boundary" => match args.next().as_deref() {
                Some("park") => video::PARK_AT_BOUNDARY.store(true, Ordering::Relaxed),
                Some("stop") => {}
                _ => usage(),
            },
            "--" => game_args.extend(args.by_ref()),
            "-h" | "--help" => usage(),
            other => {
                eprintln!("unknown argument {other}");
                usage();
            }
        }
    }
    diag::configure_trace(&trace);
    diag::install();
    mem::init();
    ppc_rt::install_function_table(p.functions);
    profile::set_functions(p.functions);

    // The executable's data, imports and kernel variables.
    let mut command_line = String::from("\"default.xex\"");
    for a in &game_args {
        command_line.push(' ');
        command_line.push_str(a);
    }
    let m = loader::load(&xex, p.image_hash, &command_line);

    // Native imports.
    let fns: Vec<Option<GuestFn>> = p.imports.iter().map(|d| resolve(d.library, d.name)).collect();
    let implemented = fns.iter().filter(|f| f.is_some()).count();
    let tracer: Option<fn(&ImportDef, &mut Ctx, *mut u8, GuestFn)> =
        diag::TRACE_IMPORTS.load(Ordering::Relaxed).then_some(trace_import as _);
    ppc_rt::install_imports(p.imports, fns, tracer);

    // Native replacements, by name.
    let rfns: Vec<Option<GuestFn>> =
        p.replacements.iter().map(|r| p.natives.iter().find_map(|resolve| resolve(r.name))).collect();
    let replaced = rfns.iter().filter(|f| f.is_some()).count();
    for (r, f) in p.replacements.iter().zip(&rfns) {
        if f.is_none() {
            diag::log(&format!("note: {} (0x{:08X}) is replaced in the config but has no native implementation yet", r.name, r.address));
        }
    }
    ppc_rt::install_replacements(p.replacements, rfns);

    // Devices: the disc, the hard disk's cache partition and save area.
    let game_dir = xex.parent().map(PathBuf::from).unwrap_or_default();
    let data_dir = game_dir.parent().map(|d| d.join("hdd")).unwrap_or_else(|| PathBuf::from("hdd"));
    file::add_device("\\Device\\Cdrom0", game_dir.clone(), true, "WWE 2K14");
    file::set_mods(game_dir.parent().map(|d| d.join("mods")).unwrap_or_else(|| PathBuf::from("mods")));
    file::add_device("\\Device\\Harddisk0\\Partition1", data_dir.join("partition1"), false, "HDD");
    file::add_device("\\Device\\Harddisk0\\Cache0", data_dir.join("cache0"), false, "Cache0");
    file::add_device("\\Device\\Harddisk0\\Cache1", data_dir.join("cache1"), false, "Cache1");
    file::create_link("game:", "\\Device\\Cdrom0");
    file::create_link("d:", "\\Device\\Cdrom0");

    time::start_bundle_updater(m.timestamp_bundle);
    object::start_timer_service();

    diag::log(&format!(
        "booting {} (title {:08X}): {} recompiled functions, {implemented}/{} imports implemented, \
         {replaced}/{} replacements implemented, entry 0x{:08X}",
        xex.display(),
        m.title_id,
        p.functions.len(),
        p.imports.len(),
        p.replacements.len(),
        m.entry
    ));

    // The title's main thread runs the entry point.
    let main = thread::spawn(
        thread::Entry::Guest { start: m.entry, context: 0, xapi_startup: 0 },
        m.default_stack,
        1,
        false,
        "main".into(),
    );
    Booted { main }
}

fn usage() -> ! {
    eprintln!("{USAGE}");
    std::process::exit(2);
}

/// Import tracing: name, arguments and result of every kernel/XAM call.
fn trace_import(d: &ImportDef, c: &mut Ctx, b: *mut u8, f: GuestFn) {
    // WWE14_TRACE_THREAD=<parts of thread names, comma-separated> traces only matching threads.
    static FILTER: std::sync::OnceLock<Option<Vec<String>>> = std::sync::OnceLock::new();
    if let Some(want) = FILTER.get_or_init(|| std::env::var("WWE14_TRACE_THREAD").ok().map(|v| v.split(',').map(String::from).collect()))
        && !want.iter().any(|w| thread::current().name().contains(w.as_str()))
    {
        unsafe { f(c, b) };
        return;
    }
    // WWE14_TRACE_IMPORT=<parts of import names, comma-separated> traces only those imports.
    static ONLY: std::sync::OnceLock<Option<Vec<String>>> = std::sync::OnceLock::new();
    if let Some(want) = ONLY.get_or_init(|| std::env::var("WWE14_TRACE_IMPORT").ok().map(|v| v.split(',').map(String::from).collect()))
        && !want.iter().any(|w| d.name.contains(w.as_str()))
    {
        unsafe { f(c, b) };
        return;
    }
    let args = format!(
        "{:08X} {:08X} {:08X} {:08X} {:08X} {:08X}",
        c.r[3] as u32, c.r[4] as u32, c.r[5] as u32, c.r[6] as u32, c.r[7] as u32, c.r[8] as u32
    );
    let lr = c.lr as u32;
    unsafe { f(c, b) };
    diag::log(&format!("{}({args}) -> {:08X}  [from 0x{lr:08X}]", d.name, c.r[3] as u32));
}

/// Frequency of the time base (`mftb`) and of `KeQueryPerformanceFrequency`.
pub fn time_frequency() -> u64 {
    time::PERFORMANCE_FREQUENCY
}
