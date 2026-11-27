//! Known-answer tests for the recompiler.
//!
//! The build script assembles instruction test vectors and translates them
//! with the same emitter that translates the game. This crate holds the
//! resulting native functions and a table of what each test feeds in and
//! expects back; `tests/xenia.rs` runs them.

pub use ppc_rt::GuestFn;

/// A register value from a test annotation.
#[derive(Clone, Copy, Debug)]
pub enum Reg {
    R(u8, u64),
    /// Floating-point register, as raw bits.
    F(u8, u64),
    /// Vector register, memory order (word 0 is the most significant).
    V(u8, u128),
    /// The whole condition register.
    Cr(u32),
}

pub struct Test {
    pub file: &'static str,
    pub name: &'static str,
    pub entry: GuestFn,
    /// Instructions in this test the recompiler doesn't translate yet.
    pub todo: &'static [&'static str],
    /// Problems reading or assembling the test.
    pub errors: &'static [&'static str],
    pub regs_in: &'static [Reg],
    pub regs_out: &'static [Reg],
    pub mem_in: &'static [(u32, &'static [u8])],
    pub mem_out: &'static [(u32, &'static [u8])],
}

#[allow(unused, unreachable_code, non_snake_case, clippy::all)]
mod generated {
    use super::{Reg, Test};
    use ppc_rt::*;
    include!(concat!(env!("OUT_DIR"), "/kat.rs"));
}

pub use generated::{FUNCTIONS, SKIPPED, TESTS};

/// Game functions translated both ways, for `tests/game.rs` (empty without
/// the executable).
#[allow(unused, unreachable_code, non_snake_case, clippy::all)]
mod game {
    use ppc_rt::*;
    include!(concat!(env!("OUT_DIR"), "/game.rs"));
}

pub use game::GAME;
