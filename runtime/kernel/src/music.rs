//! Custom soundtracks: the console's music player (XMP, system app 0xFA)
//! as a native library and player over a folder on the PC.
//!
//! On the console the player is a system service. A title lists the user's
//! songs through it and asks it to play one; the system decodes the song and
//! mixes it into the audio output over the title's own mix. Here the songs
//! are the audio files in the music folder (`WWE14_MUSIC_DIR`, default
//! `music` in the working directory), decoded natively (symphonia: MP3, WAV,
//! FLAC, Ogg Vorbis, AAC and ALAC in MP4/M4A, AIFF, Matroska audio) and mixed
//! into each frame the render driver hands to the host (`kernel::audio`).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTagKey};
use symphonia::core::probe::Hint;

/// Extensions taken for songs.
const EXTENSIONS: [&str; 13] = ["mp3", "wav", "wave", "flac", "ogg", "oga", "m4a", "mp4", "aac", "alac", "aif", "aiff", "mka"];

/// The output rate of the render driver.
const RATE: u32 = 48_000;

/// One song of the library.
#[derive(Clone, Debug)]
pub struct Song {
    pub path: PathBuf,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub genre: String,
    pub track: u32,
    pub duration_ms: u32,
}

/// The music folder.
pub fn folder() -> PathBuf {
    std::env::var_os("WWE14_MUSIC_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("music"))
}

/// The library as last listed: songs are named to the title by their index
/// here (plus one; 0 is no song), so a listing must not reorder songs a
/// title still holds. It is read once, at the first listing, and again only
/// when the folder's contents change.
static LIBRARY: Mutex<(Vec<Song>, Vec<PathBuf>)> = Mutex::new((Vec::new(), Vec::new()));

/// Every song in the music folder and its subfolders, ordered by path.
pub fn library() -> Vec<Song> {
    let dir = folder();
    if !dir.is_dir() {
        let _ = std::fs::create_dir_all(&dir);
    }
    let mut paths = Vec::new();
    collect(&dir, &mut paths, 0);
    paths.sort();
    let mut lib = LIBRARY.lock().unwrap();
    if lib.1 != paths {
        // Keep the songs already known at their indices; add new ones after.
        let mut songs: Vec<Song> = lib.0.iter().filter(|s| paths.contains(&s.path)).cloned().collect();
        for p in &paths {
            if !songs.iter().any(|s| &s.path == p) {
                match probe(p) {
                    Some(s) => songs.push(s),
                    None => crate::diag::log(&format!("music player: {} is not a song it can read; skipped", p.display())),
                }
            }
        }
        crate::diag::log(&format!("music player: {} song(s) in {}", songs.len(), dir.display()));
        *lib = (songs, paths);
    }
    lib.0.clone()
}

/// The song with this id (index + 1 in the library).
pub fn song(id: u32) -> Option<Song> {
    let lib = LIBRARY.lock().unwrap();
    id.checked_sub(1).and_then(|i| lib.0.get(i as usize)).cloned()
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>, depth: u32) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            if depth < 4 {
                collect(&p, out, depth + 1);
            }
        } else if p.extension().and_then(|x| x.to_str()).is_some_and(|x| EXTENSIONS.contains(&x.to_ascii_lowercase().as_str())) {
            out.push(p);
        }
    }
}

fn open(path: &Path) -> Option<Box<dyn FormatReader>> {
    let file = std::fs::File::open(path).ok()?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(x) = path.extension().and_then(|x| x.to_str()) {
        hint.with_extension(x);
    }
    let probed = symphonia::default::get_probe().format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default()).ok()?;
    Some(probed.format)
}

/// Reads a song's tags and length.
fn probe(path: &Path) -> Option<Song> {
    let file = std::fs::File::open(path).ok()?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(x) = path.extension().and_then(|x| x.to_str()) {
        hint.with_extension(x);
    }
    let mut probed = symphonia::default::get_probe().format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default()).ok()?;
    let track = probed.format.default_track()?.clone();
    if track.codec_params.codec == CODEC_TYPE_NULL {
        return None;
    }
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut song = Song { path: path.to_path_buf(), title: stem, artist: String::new(), album: String::new(), genre: String::new(), track: 0, duration_ms: 0 };
    let mut take = |rev: &symphonia::core::meta::MetadataRevision| {
        for tag in rev.tags() {
            let v = tag.value.to_string();
            match tag.std_key {
                Some(StandardTagKey::TrackTitle) if !v.is_empty() => song.title = v,
                Some(StandardTagKey::Artist) => song.artist = v,
                Some(StandardTagKey::Album) => song.album = v,
                Some(StandardTagKey::Genre) => song.genre = v,
                Some(StandardTagKey::TrackNumber) => song.track = v.split('/').next().and_then(|n| n.trim().parse().ok()).unwrap_or(0),
                _ => {}
            }
        }
    };
    if let Some(rev) = probed.metadata.get().as_ref().and_then(|m| m.current()) {
        take(rev);
    }
    if let Some(rev) = probed.format.metadata().current() {
        take(rev);
    }
    if let (Some(n), Some(rate)) = (track.codec_params.n_frames, track.codec_params.sample_rate) {
        song.duration_ms = (n * 1000 / rate.max(1) as u64) as u32;
    }
    Some(song)
}

// ---- Playback ------------------------------------------------------------------

/// The player's state, as the title queries it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Idle,
    Playing,
    Paused,
}

struct Player {
    state: State,
    /// The song playing or paused (its id), 0 for none.
    song: u32,
    volume: f32,
    /// The song is the player's own background music (menu music), not one
    /// the title asked for.
    background: bool,
}

/// Starts at 0.45, the volume the game sets once its menus begin (user64): the
/// user's songs play at that level from the title screen on, with no drop
/// when Start is pressed (the user's choice, 2026-11-26).
static PLAYER: Mutex<Player> = Mutex::new(Player { state: State::Idle, song: 0, volume: 0.45, background: false });

// ---- Background music --------------------------------------------------------------
//
// On the console the player could play the user's music over a game: the
// game's own music bus went quiet while the user had control
// (`XMPTitleHasPlaybackControl` false), and a scene that needs its own music
// took control (`XMPOverrideBackgroundMusic`: controller set to the title,
// locked) and gave it back afterwards (`XMPRestoreBackgroundMusic`). Here the
// music folder is played shuffled as that background music while the title
// hasn't taken control. F7 skips to another song, F8 turns it off and on
// (`WWE14_MENU_MUSIC=off` starts with it off).

/// The song playing when the title took control, paused for it.
static PAUSED_FOR_TITLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Background music is wanted.
static MENU_MUSIC: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// The title has taken playback control (a scene with its own music).
static TITLE_CONTROL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Starts the background-music picker (once; when the game's audio starts).
pub fn start_background() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        // Off unless asked for: the user wants the folder in the game's jukebox
        // instead (2026-11-25). `WWE14_MENU_MUSIC=on` turns the shuffle on.
        if std::env::var("WWE14_MENU_MUSIC").is_ok_and(|v| v.eq_ignore_ascii_case("on") || v == "1") {
            MENU_MUSIC.store(true, Ordering::SeqCst);
        }
        start_hold();
        let _ = std::thread::Builder::new().name("music shuffle".into()).spawn(|| {
            let mut seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64) | 1;
            let mut last = 0u32;
            loop {
                std::thread::sleep(std::time::Duration::from_millis(300));
                if !MENU_MUSIC.load(Ordering::SeqCst) || TITLE_CONTROL.load(Ordering::SeqCst) || state().0 != State::Idle {
                    continue;
                }
                let n = library().len() as u32;
                if n == 0 {
                    continue;
                }
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let mut id = (seed % n as u64) as u32 + 1;
                if id == last && n > 1 {
                    id = id % n + 1;
                }
                last = id;
                play_inner(id, true);
            }
        });
    });
}

/// The title takes playback control (`take`) or gives it back. Background
/// music pauses while the title holds it and goes on afterwards.
pub fn title_control(take: bool) {
    // A mode with its own music (30 Years of WrestleMania) takes control:
    // whatever song is playing pauses, the Jukebox's custom songs included,
    // and goes on when control is given back (the user, 2026-11-26: "I don't
    // want my custom music playing when I'm in that mode"). A song the title
    // starts while it holds control plays as usual.
    TITLE_CONTROL.store(take, Ordering::SeqCst);
    let mut p = PLAYER.lock().unwrap();
    match (take, p.state) {
        (true, State::Playing) => {
            p.state = State::Paused;
            PAUSED_FOR_TITLE.store(true, Ordering::SeqCst);
        }
        (false, State::Paused) if PAUSED_FOR_TITLE.swap(false, Ordering::SeqCst) => p.state = State::Playing,
        _ => {}
    }
    crate::diag::log(&format!("music player: the title {} playback control", if take { "takes" } else { "gives back" }));
}

/// The user's music is playing over the title (what the title's
/// playback-controller query reports).
pub fn user_has_control() -> bool {
    let p = PLAYER.lock().unwrap();
    p.background && p.state != State::Idle && !TITLE_CONTROL.load(Ordering::SeqCst)
}

/// The title's stop, pause and continue: they act on a song the title
/// started, not on the background music.
pub fn title_stop() {
    if !PLAYER.lock().unwrap().background {
        stop();
    }
}

pub fn title_pause() {
    if !PLAYER.lock().unwrap().background {
        pause();
    }
}

pub fn title_resume() {
    if !PLAYER.lock().unwrap().background {
        resume();
    }
}

// ---- Quiet until the save is loaded ----------------------------------------------
//
// The title screen starts the game's own music before the save, with the
// Jukebox's choices, has loaded: a second of it played before the user's
// songs (the original does the same). At the user's request (2026-11-26),
// when the Jukebox has custom songs ticked (`jukebox`'s selection file), the
// game's sound is held quiet from the start: the frames it mixes are
// silenced before the user's music is added (`held`, used by
// `kernel::audio::submit`). Reporting the system's player in control instead
// (as a console does for the user's music) made the game keep its own music
// off for the whole session, entrance themes included (user65). The Jukebox's first pick with the custom-song
// slot on (the save is loaded) ends the hold (`release_hold`); two minutes
// end it anyway. Nothing of the user's music starts early.

static HOLD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn start_hold() {
    let ticked = std::fs::read_to_string(folder().join(".wwe14-playlist")).is_ok_and(|s| s.lines().any(|l| !l.trim().is_empty()));
    if !ticked {
        return;
    }
    HOLD.store(true, Ordering::SeqCst);
    crate::diag::log("music player: custom songs are ticked in the Jukebox; the game's own music is held quiet until the save loads");
    let _ = std::thread::Builder::new().name("music hold".into()).spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(120));
        if HOLD.swap(false, Ordering::SeqCst) {
            crate::diag::log("music player: no Jukebox pick within 2 minutes; the game's music is no longer held");
        }
    });
}

/// The game's own sound is held quiet (title screen, before the save loads).
pub fn held() -> bool {
    HOLD.load(Ordering::SeqCst)
}

/// The Jukebox picks a song with the save loaded: the game's music plays again.
pub fn release_hold() {
    if HOLD.swap(false, Ordering::SeqCst) {
        crate::diag::log("music player: the save is loaded; the game's music is no longer held");
    }
}

// ---- The Jukebox's songs, as the menu-music system sees them -----------------------
//
// A mode with its own menu music (30 Years of WrestleMania) asks the menu-music
// system whether the Jukebox is playing (`sub_82ACCFC8`: 3 playing, 5 not) and
// stops it if so (`sub_82ACCA50`) before starting its music. For a custom song
// the answer came back "not playing", so the mode's music never started and the
// user's song played on (user68). `jukebox` marks the title it picks; the song
// that starts with that title is the Jukebox's, reported as playing and
// stopped with the Jukebox.

static JUKEBOX_NEXT: Mutex<Option<String>> = Mutex::new(None);
static JUKEBOX_SONG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The Jukebox picked `title`: the next song started with it is the Jukebox's.
pub fn mark_jukebox_next(title: &str) {
    *JUKEBOX_NEXT.lock().unwrap() = Some(title.to_string());
}

/// The title the Jukebox's pick named, if it hasn't been played yet.
pub fn take_jukebox_next() -> Option<String> {
    JUKEBOX_NEXT.lock().unwrap().take()
}

/// Whether the song just started is the Jukebox's (`xam`: its player waits on
/// its play request; entrance music doesn't).
pub fn set_jukebox_song(yes: bool) {
    JUKEBOX_SONG.store(yes, Ordering::SeqCst);
}

/// A song the Jukebox started is playing (or paused).
pub fn jukebox_song_playing() -> bool {
    JUKEBOX_SONG.load(Ordering::SeqCst) && state().0 != State::Idle
}

/// The game stopped the Jukebox: its song stops too.
pub fn stop_jukebox_song() {
    if jukebox_song_playing() {
        crate::diag::log("music player: the game stops the Jukebox; its song stops");
        stop();
    }
}

/// F7: another background song now.
pub fn skip() {
    let background = PLAYER.lock().unwrap().background;
    if background && MENU_MUSIC.load(Ordering::SeqCst) {
        stop(); // the picker starts another one
    }
}

/// F8: background music off or on.
pub fn toggle_menu_music() {
    let on = !MENU_MUSIC.fetch_xor(true, Ordering::SeqCst);
    crate::diag::log(&format!("music player: menu music {}", if on { "on" } else { "off" }));
    if !on && PLAYER.lock().unwrap().background {
        stop();
    }
}

/// Decoded stereo at 48 kHz waiting to be mixed.
static RING: Mutex<VecDeque<f32>> = Mutex::new(VecDeque::new());

/// Bumped on every play and stop: a decoder thread whose number is no
/// longer current stops.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// The decoder runs ahead of the mix by at most this many stereo samples.
const AHEAD: usize = 2 * RATE as usize;

pub fn state() -> (State, u32) {
    let p = PLAYER.lock().unwrap();
    (p.state, p.song)
}

pub fn volume() -> f32 {
    PLAYER.lock().unwrap().volume
}

pub fn set_volume(v: f32) {
    PLAYER.lock().unwrap().volume = v.clamp(0.0, 1.0);
}

/// Starts the song with this id from the start, for the title. False if there is none.
pub fn play(id: u32) -> bool {
    play_inner(id, false)
}

fn play_inner(id: u32, background: bool) -> bool {
    PAUSED_FOR_TITLE.store(false, Ordering::SeqCst);
    let Some(song) = song(id) else {
        crate::diag::log(&format!("music player: play song {id}: no such song"));
        return false;
    };
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    RING.lock().unwrap().clear();
    {
        let mut p = PLAYER.lock().unwrap();
        p.state = State::Playing;
        p.song = id;
        p.background = background;
    }
    crate::diag::log(&format!(
        "music player: playing \"{}\" ({}){}",
        song.title,
        song.path.display(),
        if background { " as menu music" } else { " for the title" }
    ));
    let _ = std::thread::Builder::new().name("music decoder".into()).spawn(move || {
        if let Err(e) = decode(&song.path, generation) {
            crate::diag::log(&format!("music player: {} stopped: {e}", song.path.display()));
        }
        // At the end of the song (not after a stop or another play).
        if GENERATION.load(Ordering::SeqCst) == generation {
            // Let the mix drain what is queued, then go idle.
            while !RING.lock().unwrap().is_empty() && GENERATION.load(Ordering::SeqCst) == generation {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let mut p = PLAYER.lock().unwrap();
            if GENERATION.load(Ordering::SeqCst) == generation {
                p.state = State::Idle;
                p.song = 0;
                p.background = false;
            }
        }
    });
    true
}

pub fn stop() {
    GENERATION.fetch_add(1, Ordering::SeqCst);
    RING.lock().unwrap().clear();
    let mut p = PLAYER.lock().unwrap();
    if p.state != State::Idle {
        crate::diag::log("music player: stopped");
    }
    p.state = State::Idle;
    p.song = 0;
    p.background = false;
}

pub fn pause() {
    let mut p = PLAYER.lock().unwrap();
    if p.state == State::Playing {
        p.state = State::Paused;
    }
}

pub fn resume() {
    let mut p = PLAYER.lock().unwrap();
    if p.state == State::Paused {
        p.state = State::Playing;
    }
}

/// Decodes `path` into the ring at 48 kHz stereo while `generation` is current.
fn decode(path: &Path, generation: u64) -> Result<(), String> {
    decode_stereo_48k(path, |out| {
        // Wait for room.
        loop {
            if GENERATION.load(Ordering::SeqCst) != generation {
                return false;
            }
            let mut ring = RING.lock().unwrap();
            if ring.len() < AHEAD {
                ring.extend(out.iter().copied());
                return true;
            }
            drop(ring);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    })
}

/// Decodes the song at `path` as 48 kHz stereo (interleaved f32), handing
/// each packet's frames to `sink` in order; `sink` returns false to stop.
/// Mono is doubled; beyond two channels, the first two are taken. Also used
/// by `kernel::entrance` (custom entrance songs through the game's mixer).
pub fn decode_stereo_48k(path: &Path, mut sink: impl FnMut(&[f32]) -> bool) -> Result<(), String> {
    let mut format = open(path).ok_or("can't open it")?;
    let track = format.default_track().ok_or("no audio track")?.clone();
    let mut decoder = symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default()).map_err(|e| e.to_string())?;
    let mut buf: Option<SampleBuffer<f32>> = None;
    // Linear resampling over [last frame of the previous packet, this
    // packet's frames...]: `pos` is the next output frame's position in that
    // list (1.0 at the start: the song's first frame).
    let (mut pos, mut prev) = (1.0f64, [0.0f32; 2]);
    let mut src: Vec<[f32; 2]> = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(symphonia::core::errors::Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e.to_string()),
        };
        if packet.track_id() != track.id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(symphonia::core::errors::Error::DecodeError(_)) => continue, // a damaged packet: skip it
            Err(e) => return Err(e.to_string()),
        };
        let spec = *decoded.spec();
        let channels = spec.channels.count().max(1);
        let b = buf.get_or_insert_with(|| SampleBuffer::new(decoded.capacity() as u64, spec));
        if b.capacity() < decoded.capacity() * channels {
            *b = SampleBuffer::new(decoded.capacity() as u64, spec);
        }
        b.copy_interleaved_ref(decoded);
        let s = b.samples();
        // Source frames as stereo (mono doubled; beyond two, the first two).
        src.clear();
        src.push(prev);
        src.extend(s.chunks_exact(channels).map(|f| if channels == 1 { [f[0]; 2] } else { [f[0], f[1]] }));
        let step = spec.rate as f64 / RATE as f64;
        let last = (src.len() - 1) as f64;
        let mut out = Vec::with_capacity(((last / step) as usize + 2) * 2);
        while pos <= last {
            let i = pos.floor() as usize;
            let t = (pos - i as f64) as f32;
            let (a, b2) = (src[i], src[(i + 1).min(src.len() - 1)]);
            out.push(a[0] + (b2[0] - a[0]) * t);
            out.push(a[1] + (b2[1] - a[1]) * t);
            pos += step;
        }
        pos -= last;
        prev = src[src.len() - 1];
        if !sink(&out) {
            return Ok(());
        }
    }
}

/// Adds the playing song into one mixed stereo frame (interleaved, 48 kHz).
pub fn mix(out: &mut [f32]) {
    let (state, volume) = {
        let p = PLAYER.lock().unwrap();
        (p.state, p.volume)
    };
    if state != State::Playing {
        return;
    }
    let mut ring = RING.lock().unwrap();
    for x in out.iter_mut() {
        match ring.pop_front() {
            Some(s) => *x = (*x + s * volume).clamp(-1.0, 1.0),
            None => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-second 440 Hz stereo tone at 44.1 kHz, as a 16-bit WAV.
    fn tone(path: &Path) {
        let (rate, n) = (44_100u32, 44_100u32);
        let mut w = Vec::new();
        w.extend(b"RIFF");
        w.extend((36 + n * 4).to_le_bytes());
        w.extend(b"WAVEfmt ");
        w.extend(16u32.to_le_bytes());
        w.extend(1u16.to_le_bytes());
        w.extend(2u16.to_le_bytes());
        w.extend(rate.to_le_bytes());
        w.extend((rate * 4).to_le_bytes());
        w.extend(4u16.to_le_bytes());
        w.extend(16u16.to_le_bytes());
        w.extend(b"data");
        w.extend((n * 4).to_le_bytes());
        for i in 0..n {
            let s = ((i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 16_000.0) as i16;
            w.extend(s.to_le_bytes());
            w.extend(s.to_le_bytes());
        }
        std::fs::write(path, w).unwrap();
    }

    #[test]
    fn lists_and_plays_a_wav_at_48k() {
        let dir = std::env::temp_dir().join(format!("wwe14-music-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        tone(&dir.join("Test Tone.wav"));
        std::fs::write(dir.join("notes.txt"), b"not a song").unwrap();
        unsafe { std::env::set_var("WWE14_MUSIC_DIR", &dir) };
        let lib = library();
        assert_eq!(lib.len(), 1);
        assert_eq!(lib[0].title, "Test Tone");
        assert!((990..=1010).contains(&lib[0].duration_ms), "{}", lib[0].duration_ms);
        set_volume(1.0);
        assert!(play(1));
        // Pull frames as the render driver does until the song ends.
        let (mut total, mut peak) = (0usize, 0f32);
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_secs(10) {
            let mut frame = [0f32; 512];
            mix(&mut frame);
            let heard = frame.iter().rposition(|x| *x != 0.0).map_or(0, |i| i + 1);
            total += heard;
            peak = frame.iter().fold(peak, |m, x| m.max(x.abs()));
            if state().0 == State::Idle {
                break;
            }
            if heard == 0 {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
        assert_eq!(state().0, State::Idle, "the song ended");
        // One second at 48 kHz, stereo: 96,000 samples, give or take a frame.
        assert!((95_000..=96_100).contains(&total), "{total} samples");
        assert!((0.45..0.5).contains(&peak), "peak {peak}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
