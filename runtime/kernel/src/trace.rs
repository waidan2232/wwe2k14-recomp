//! Debugging: which recompiled functions ran in each frame. A build
//! recompiled with `--trace-calls on` marks every function called in
//! `WWE14_TRACE_CALLED` (by its index in address order; the build's
//! `trace_index.txt` maps indices to addresses). With `WWE14_TRACE_CALLS=FILE`
//! set, `frame` writes, once a frame, the functions that started or stopped
//! being called relative to the frame before:
//!
//! `F <frame> +<index> ... -<index> ...`
//!
//! Comparing a bad frame with its neighbours names the code that ran only
//! on it. Normal builds never touch the table.

use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};

/// Room for every function's index (the game has about 83,600).
const SLOTS: usize = 131072;

/// Which half of `WWE14_TRACE_CALLED` the running frame marks.
#[unsafe(no_mangle)]
pub static WWE14_TRACE_PARITY: AtomicU32 = AtomicU32::new(0);

/// Two tables of "called" flags, one per frame parity.
#[unsafe(no_mangle)]
pub static WWE14_TRACE_CALLED: [AtomicU8; 2 * SLOTS] = [const { AtomicU8::new(0) }; 2 * SLOTS];

struct Writer {
    out: std::io::BufWriter<std::fs::File>,
    last: Vec<bool>,
}

static WRITER: Mutex<Option<Option<Writer>>> = Mutex::new(None);

/// Ends frame `n`: the marks so far belong to it, and later calls mark the
/// other table.
pub fn frame(n: u64) {
    let mut w = WRITER.lock().unwrap_or_else(|e| e.into_inner());
    let w = w.get_or_insert_with(|| {
        let path = std::env::var_os("WWE14_TRACE_CALLS")?;
        let file = std::fs::File::create(&path).ok()?;
        crate::diag::log(&format!("trace: functions called per frame go to {}", std::path::Path::new(&path).display()));
        Some(Writer { out: std::io::BufWriter::new(file), last: vec![false; SLOTS] })
    });
    let Some(w) = w else { return };
    let p = WWE14_TRACE_PARITY.fetch_xor(1, Ordering::Relaxed) as usize & 1;
    let table = &WWE14_TRACE_CALLED[p * SLOTS..(p + 1) * SLOTS];
    let mut line = format!("F {n}");
    for (i, flag) in table.iter().enumerate() {
        let now = flag.swap(0, Ordering::Relaxed) != 0;
        if now != w.last[i] {
            line.push_str(if now { " +" } else { " -" });
            line.push_str(&i.to_string());
            w.last[i] = now;
        }
    }
    line.push('\n');
    let _ = w.out.write_all(line.as_bytes());
    if n % 60 == 0 {
        let _ = w.out.flush();
    }
}
