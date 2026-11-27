//! Calling convention for native exports.
//!
//! Each import is called like a guest function: arguments in r3..r10 (then the
//! caller's parameter area at `r1 + 0x54 + 8*(n-8)`, the low word of each
//! 8-byte slot), result in r3. Results are sign-extended to 64 bits, as the
//! guest's own `li`/`lis` would leave them.

use ppc_rt::Ctx;

use crate::mem;

#[inline]
pub fn arg(c: &Ctx, n: usize) -> u32 {
    if n < 8 { c.r[3 + n] as u32 } else { mem::r32(c.r[1] as u32 + 0x54 + 8 * (n as u32 - 8)) }
}

#[inline]
pub fn arg64(c: &Ctx, n: usize) -> u64 {
    if n < 8 { c.r[3 + n] } else { mem::r64(c.r[1] as u32 + 0x50 + 8 * (n as u32 - 8)) }
}

#[inline]
pub fn ret(c: &mut Ctx, v: u32) {
    c.r[3] = v as i32 as i64 as u64;
}

#[inline]
pub fn ret64(c: &mut Ctx, v: u64) {
    c.r[3] = v;
}

/// Writes `v` to `*p` if `p` is not null.
#[inline]
pub fn out32(p: u32, v: u32) {
    if p != 0 {
        mem::w32(p, v);
    }
}

/// Defines native exports and a `resolve(name)` that finds them.
macro_rules! exports {
    ($( $name:ident => |$c:ident| $body:block )*) => {
        $(
            #[allow(non_snake_case, unused_variables)]
            extern "C-unwind" fn $name($c: &mut ::ppc_rt::Ctx, _b: *mut u8) $body
        )*
        /// The native implementation of export `name`, if there is one.
        pub fn resolve(name: &str) -> Option<::ppc_rt::GuestFn> {
            match name {
                $( stringify!($name) => Some($name as ::ppc_rt::GuestFn), )*
                _ => None,
            }
        }
        /// Every export this module implements.
        #[allow(dead_code)]
        pub const IMPLEMENTED: &[&str] = &[$( stringify!($name) ),*];
    };
}
pub(crate) use exports;
