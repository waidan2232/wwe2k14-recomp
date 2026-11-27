//! Ordinal → name tables for the system modules a title imports from.
//!
//! The tables are Xenia's (BSD-3-Clause, via XenonRecomp). See THIRD_PARTY_NOTICES.md.
//! They are lookup data only.

use std::sync::OnceLock;

static XBOXKRNL_SRC: &str = include_str!("../data/xboxkrnl_table.inc");
static XAM_SRC: &str = include_str!("../data/xam_table.inc");

type Table = Vec<(u32, &'static str)>;

/// Parses lines of the form `XE_EXPORT(xboxkrnl, 0x00000001, DbgBreakPoint, kFunction),`.
fn parse_table(src: &'static str) -> Table {
    let mut table: Table = src
        .lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("XE_EXPORT(")?;
            let mut fields = rest.split(',').map(str::trim);
            let _module = fields.next()?;
            let ordinal = u32::from_str_radix(fields.next()?.strip_prefix("0x")?, 16).ok()?;
            let name = fields.next()?;
            Some((ordinal, name))
        })
        .collect();
    table.sort_unstable_by_key(|&(ordinal, _)| ordinal);
    table
}

fn table_for(module: &str) -> Option<&'static Table> {
    static XBOXKRNL: OnceLock<Table> = OnceLock::new();
    static XAM: OnceLock<Table> = OnceLock::new();
    if module.eq_ignore_ascii_case("xboxkrnl.exe") {
        Some(XBOXKRNL.get_or_init(|| parse_table(XBOXKRNL_SRC)))
    } else if module.eq_ignore_ascii_case("xam.xex") {
        Some(XAM.get_or_init(|| parse_table(XAM_SRC)))
    } else {
        None
    }
}

/// Name of export `ordinal` of `module` (`xboxkrnl.exe` or `xam.xex`).
pub fn lookup(module: &str, ordinal: u32) -> Option<&'static str> {
    let table = table_for(module)?;
    let i = table.binary_search_by_key(&ordinal, |&(o, _)| o).ok()?;
    Some(table[i].1)
}

/// Number of known exports for `module`.
pub fn known_count(module: &str) -> usize {
    table_for(module).map_or(0, Vec::len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_ordinals_resolve() {
        assert_eq!(lookup("xboxkrnl.exe", 0x1), Some("DbgBreakPoint"));
        assert_eq!(lookup("XBOXKRNL.EXE", 0x3), Some("DbgPrint"));
        assert_eq!(lookup("unknown.xex", 0x1), None);
        assert!(known_count("xboxkrnl.exe") > 800);
        assert!(known_count("xam.xex") > 1000);
    }
}
