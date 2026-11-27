//! Ahead-of-time PowerPC -> Rust translation, as a library.
//!
//! The `recomp` binary drives it over the whole game; the known-answer test
//! crate (`tools/kat`) drives it over small test programs. Either way the
//! translation happens at build time and nothing is interpreted at runtime.

pub mod emit;
pub mod locals;
pub mod regs;
