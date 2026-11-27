//! Checks against the real WWE 2K14 disc. Runs only when `WWE14_ISO` points at the image.

use disc::{Disc, Layout};

#[test]
fn wwe14_disc_layout_and_tree() {
    let Ok(path) = std::env::var("WWE14_ISO") else {
        eprintln!("WWE14_ISO not set; skipping");
        return;
    };
    let mut disc = Disc::open(&path).expect("open disc image");
    assert_eq!(disc.layout(), Layout::Xgd3);
    let entries = disc.entries().expect("walk directory tree");
    let files = entries.iter().filter(|e| !e.is_dir).count();
    let dirs = entries.iter().filter(|e| e.is_dir).count();
    assert_eq!((files, dirs), (714, 16));

    let xex = entries.iter().find(|e| e.path == "default.xex").expect("default.xex present");
    assert_eq!(xex.size, 24_657_920);
    let head = disc.read_file(xex).expect("read default.xex");
    assert_eq!(&head[..4], b"XEX2");
}
