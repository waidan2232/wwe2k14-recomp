//! The Bevy host (plan §2.6): Bevy owns the main thread and the window; the
//! guest runs on its own threads. `GpuBridgePlugin` runs the renderer in
//! Bevy's render world on the frames the native D3D layer hands over, and
//! shows the front buffer on a full-window sprite (Bevy UI can draw over it).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::sync::mpsc::Receiver;

use bevy::asset::RenderAssetUsages;
use bevy::core_pipeline::schedule::camera_driver;
use bevy::input::ButtonState;
use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::prelude::*;
use bevy::render::extract_resource::{ExtractResource, ExtractResourcePlugin};
use bevy::render::render_asset::RenderAssets;
use bevy::render::render_resource::{Extent3d, TextureFormat, TextureUsages};
use bevy::render::renderer::{RenderContext, RenderDevice, RenderGraph, RenderQueue};
use bevy::render::texture::GpuImage;
use bevy::render::{RenderApp, RenderStartup};
use bevy::window::{MonitorSelection, WindowMode};
use bevy::winit::{EventLoopProxyWrapper, UpdateMode, WinitSettings, WinitUserEvent};

/// The guest, so the app can exit when its main thread returns.
#[derive(Resource)]
struct Guest(kernel::Booted);

/// The image the front buffer is copied into each frame.
#[derive(Resource, Clone, ExtractResource)]
#[extract_app(RenderApp)]
struct FrontBuffer(Handle<Image>);

/// Render world: what the render thread produces, and what the window showed.
#[derive(Resource)]
struct Bridge {
    /// The frame channel, until the render thread takes it.
    frames: Mutex<Option<Receiver<gpu::ir::Frame>>>,
    shader_dirs: Vec<PathBuf>,
    /// The renderer's display image (the same texture for the whole run).
    target: Option<wgpu::Texture>,
    /// The last game frame the render thread finished (0: none yet).
    rendered: Arc<AtomicU64>,
    /// The last game frame copied into the window, and how many were.
    copied: u64,
    shown: u64,
}

/// Wakes the window's event loop (set at startup).
static WAKE: std::sync::OnceLock<Box<dyn Fn() + Send + Sync>> = std::sync::OnceLock::new();

pub struct GpuBridgePlugin {
    pub shader_dirs: Vec<PathBuf>,
}

impl Plugin for GpuBridgePlugin {
    fn build(&self, app: &mut App) {
        // The window updates when the game hands over a frame (60 a second) or
        // on input and window events, not as fast as it can: spinning at 300
        // updates a second to show 60 frames took CPU and GPU time from the game.
        let wait = std::time::Duration::from_millis(250);
        app.insert_resource(WinitSettings { focused_mode: UpdateMode::reactive(wait), unfocused_mode: UpdateMode::reactive(wait) });
        app.add_plugins(ExtractResourcePlugin::<FrontBuffer>::default())
            .init_resource::<KeyboardPrompt>()
            .add_systems(Startup, (setup, wake_on_frames))
            .add_systems(Update, (fit_sprite, exit_with_guest, keyboard_prompt, read_input, toggle_fullscreen, set_window_icon));
        let frames = gpu::ir::subscribe();
        let render_app = app.sub_app_mut(RenderApp);
        render_app
            .insert_resource(Bridge {
                frames: Mutex::new(Some(frames)),
                shader_dirs: self.shader_dirs.clone(),
                target: None,
                rendered: Arc::new(AtomicU64::new(0)),
                copied: 0,
                shown: 0,
            })
            .add_systems(RenderStartup, start_renderer)
            .add_systems(RenderGraph, draw_frame.before(camera_driver));
    }
}

fn setup(mut commands: Commands, mut images: ResMut<Assets<Image>>) {
    let (ow, oh) = render::output_size();
    let mut image = Image::new_fill(
        Extent3d { width: ow, height: oh, depth_or_array_layers: 1 },
        bevy::render::render_resource::TextureDimension::D2,
        &[0, 0, 0, 255],
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD,
    );
    image.texture_descriptor.usage = TextureUsages::COPY_DST | TextureUsages::TEXTURE_BINDING;
    let handle = images.add(image);
    commands.spawn(Camera2d);
    commands.spawn(Sprite { image: handle.clone(), custom_size: Some(Vec2::new(render::WIDTH as f32, render::HEIGHT as f32)), ..default() });
    commands.insert_resource(FrontBuffer(handle));

    // The keyboard prompt, hidden until the game asks for text: a panel over
    // the game's image, a field, and four lines of text.
    commands.spawn((PromptRoot, Transform::from_xyz(0.0, 0.0, 10.0), Visibility::Hidden)).with_children(|p| {
        p.spawn((Sprite { color: Color::srgba(0.02, 0.02, 0.06, 0.94), custom_size: Some(Vec2::new(1000.0, 300.0)), ..default() }, Transform::from_xyz(0.0, 0.0, 0.0)));
        p.spawn((Sprite { color: Color::srgba(1.0, 1.0, 1.0, 0.12), custom_size: Some(Vec2::new(920.0, 64.0)), ..default() }, Transform::from_xyz(0.0, -12.0, 0.1)));
        for (line, y, size, color) in [
            (PromptLine::Title, 105.0, 34.0, Color::WHITE),
            (PromptLine::Description, 58.0, 22.0, Color::srgb(0.8, 0.8, 0.8)),
            (PromptLine::Text, -12.0, 36.0, Color::WHITE),
            (PromptLine::Hint, -105.0, 20.0, Color::srgb(0.7, 0.7, 0.7)),
        ] {
            p.spawn((line, Text2d::new(""), TextFont { font_size: FontSize::Px(size), ..default() }, TextColor(color), Transform::from_xyz(0.0, y, 0.2)));
        }
    });
    kernel::keyboard_host_attached();
}

/// The console's on-screen keyboard (`XamShowKeyboardUI`, `kernel::xam`) as a
/// prompt over the game: the game opens it for every name the player types.
/// The text comes from the PC's keyboard.
#[derive(Resource, Default)]
struct KeyboardPrompt {
    /// The request being answered, and the text so far.
    request: Option<kernel::KeyboardRequest>,
    text: String,
    /// The controller's A and B count once both have been up since the
    /// prompt opened: the press that opened it is still down at first.
    armed: bool,
}

/// The prompt's parent entity (its children are the panel and the lines).
#[derive(Component)]
struct PromptRoot;

/// One of the prompt's lines of text.
#[derive(Component, Clone, Copy, PartialEq, Eq)]
enum PromptLine {
    Title,
    Description,
    Text,
    Hint,
}

/// Shows the keyboard prompt while the game waits for text, edits the text
/// with what is typed, and answers the game: Enter (or A) accepts, Escape (or
/// B) cancels.
fn keyboard_prompt(
    mut prompt: ResMut<KeyboardPrompt>,
    mut typed: MessageReader<KeyboardInput>,
    gamepads: Query<&Gamepad>,
    windows: Query<&Window>,
    mut root: Query<(&mut Visibility, &mut Transform), With<PromptRoot>>,
    mut lines: Query<(&PromptLine, &mut Text2d)>,
) {
    let waiting = kernel::keyboard_request();
    if waiting != prompt.request {
        // A new request: start from its text.
        prompt.text = waiting.as_ref().map(|r| r.text.clone()).unwrap_or_default();
        prompt.armed = false;
        prompt.request = waiting;
    }
    let Some(request) = prompt.request.clone() else {
        typed.clear();
        for (mut visibility, _) in &mut root {
            if *visibility != Visibility::Hidden {
                *visibility = Visibility::Hidden;
            }
        }
        return;
    };
    // Some(true): accepted; Some(false): cancelled.
    let mut answer = None;
    for key in typed.read() {
        if key.state != ButtonState::Pressed {
            continue;
        }
        match &key.logical_key {
            Key::Enter => answer = Some(true),
            Key::Escape => answer = Some(false),
            Key::Backspace => {
                prompt.text.pop();
            }
            _ => {
                for ch in key.text.iter().flat_map(|t| t.chars()).filter(|c| !c.is_control()) {
                    if prompt.text.encode_utf16().count() + ch.len_utf16() <= request.max_chars {
                        prompt.text.push(ch);
                    }
                }
            }
        }
    }
    if let Some(pad) = gamepads.iter().next() {
        if !prompt.armed {
            prompt.armed = !pad.pressed(GamepadButton::South) && !pad.pressed(GamepadButton::East);
        } else if answer.is_none() {
            if pad.just_pressed(GamepadButton::South) {
                answer = Some(true);
            } else if pad.just_pressed(GamepadButton::East) {
                answer = Some(false);
            }
        }
    }
    if let Some(accepted) = answer {
        kernel::keyboard_finish(accepted.then_some(prompt.text.as_str()));
        prompt.request = None;
        for (mut visibility, _) in &mut root {
            *visibility = Visibility::Hidden;
        }
        return;
    }
    // Drawn for a 1280-wide window, and scaled with the window.
    let scale = windows.iter().next().map_or(1.0, |w| (w.width() / 1280.0).clamp(0.5, 4.0));
    for (mut visibility, mut transform) in &mut root {
        if *visibility != Visibility::Visible {
            *visibility = Visibility::Visible;
        }
        if transform.scale.x != scale {
            transform.scale = Vec3::splat(scale);
        }
    }
    // The field holds about 40 characters: a longer text shows its end.
    let chars: Vec<char> = prompt.text.chars().collect();
    let shown: String = if chars.len() > 40 { "...".chars().chain(chars[chars.len() - 37..].iter().copied()).collect() } else { prompt.text.clone() };
    for (line, mut text) in &mut lines {
        let new = match line {
            PromptLine::Title => if request.title.is_empty() { "Enter text".to_string() } else { request.title.clone() },
            PromptLine::Description => request.description.clone(),
            PromptLine::Text => format!("{shown}_"),
            PromptLine::Hint => format!(
                "Type on the keyboard.   Enter or A: OK   Esc or B: cancel   ({} of {})",
                prompt.text.encode_utf16().count(),
                request.max_chars
            ),
        };
        if text.0 != new {
            text.0 = new;
        }
    }
}

/// Wakes the window's event loop whenever the render thread finishes a frame.
fn wake_on_frames(proxy: Res<EventLoopProxyWrapper>) {
    let proxy = Mutex::new((*proxy).clone());
    let _ = WAKE.set(Box::new(move || {
        let _ = proxy.lock().unwrap().send_event(WinitUserEvent::WakeUp);
    }));
}

/// F11: borderless fullscreen or a window, in turn. F9: a snapshot of the next
/// frame for debugging (`render::snapshot_next_frame`). F10: a 15 s sampling
/// profile of every thread from now (`kernel::profile`), reported in the log.
fn toggle_fullscreen(keys: Res<ButtonInput<KeyCode>>, mut windows: Query<&mut Window>) {
    if keys.just_pressed(KeyCode::F9) {
        render::snapshot_next_frame();
    }
    // F6 (diagnostics): watches the guest ranges in `WWE14_WATCH_MEM`
    // ("physical:length" in hex, comma-separated), reporting the first code to
    // touch each (`kernel::mem::watch`).
    if keys.just_pressed(KeyCode::F6) {
        // Also: every resolve for the next 10 seconds goes into the log.
        gpu::trace_resolves_for(600);
        for item in std::env::var("WWE14_WATCH_MEM").unwrap_or_default().split(',').filter(|s| !s.is_empty()) {
            let parsed = item.split_once(':').and_then(|(a, n)| Some((u32::from_str_radix(a.trim(), 16).ok()?, u32::from_str_radix(n.trim(), 16).ok()?)));
            match parsed.and_then(|(a, n)| Some((gpu::texture::physical_to_virtual(a)?, n))) {
                Some((virt, len)) => {
                    eprintln!("F6: watching 0x{virt:08X}+0x{len:X} ({item})");
                    kernel::mem::watch(virt, len, "a WWE14_WATCH_MEM range");
                }
                None => eprintln!("F6: {item} is not committed guest memory"),
            }
        }
    }
    if keys.just_pressed(KeyCode::F10) {
        kernel::profile::start_now(15.0, "F10: sampling for 15 s");
    }
    if !keys.just_pressed(KeyCode::F11) {
        return;
    }
    for mut w in &mut windows {
        w.mode = match w.mode {
            WindowMode::Windowed => WindowMode::BorderlessFullscreen(MonitorSelection::Current),
            _ => WindowMode::Windowed,
        };
    }
}

/// Keeps the 16:9 front buffer as large as the window allows.
fn fit_sprite(windows: Query<&Window>, mut sprites: Query<&mut Sprite>) {
    let Some(w) = windows.iter().next() else { return };
    let (ww, wh) = (w.width(), w.height());
    let scale = (ww / render::WIDTH as f32).min(wh / render::HEIGHT as f32);
    for mut s in &mut sprites {
        s.custom_size = Some(Vec2::new(render::WIDTH as f32 * scale, render::HEIGHT as f32 * scale));
    }
}

/// Controller 1 for the guest (plan §2.6 `InputBridgePlugin`): the first
/// gamepad (an Xbox controller maps one to one), plus the keyboard in the
/// WWE 2K15 PC layout (see README).
fn read_input(gamepads: Query<(&Gamepad, Option<&Name>)>, keys: Res<ButtonInput<KeyCode>>, mut last_sources: Local<String>) {
    let mut p = kernel::Pad::default();
    let mut stick = [0.0f32; 4];
    let (mut lt, mut rt) = (0.0f32, 0.0f32);
    // Where each pressed control comes from, logged when it changes: every
    // gamepad's pressed buttons by the host's own names (not only the first
    // gamepad's, which is the one the game gets) and the mapped keys. The
    // user saw the pause menu at the bell without pressing START, and the
    // game did get a START then (2026-11-25): this tells a controller's
    // START from a mis-mapped button, a second device or the keyboard.
    let mut sources = String::new();
    {
        use GamepadButton as B;
        use std::fmt::Write;
        const ALL: [GamepadButton; 19] = [
            B::South, B::East, B::North, B::West, B::C, B::Z, B::LeftTrigger, B::LeftTrigger2, B::RightTrigger, B::RightTrigger2,
            B::Select, B::Start, B::Mode, B::LeftThumb, B::RightThumb, B::DPadUp, B::DPadDown, B::DPadLeft, B::DPadRight,
        ];
        for (i, (g, name)) in gamepads.iter().enumerate() {
            let down: Vec<String> = ALL.iter().filter(|b| g.pressed(**b)).map(|b| format!("{b:?}")).collect();
            if !down.is_empty() {
                let _ = write!(sources, "gamepad {i} '{}' {:04x}:{:04x} [{}]; ", name.map_or("?", |n| n.as_str()), g.vendor_id().unwrap_or(0), g.product_id().unwrap_or(0), down.join(" "));
            }
        }
        let down: Vec<String> = keys.get_pressed().map(|k| format!("{k:?}")).collect();
        if !down.is_empty() {
            let _ = write!(sources, "keyboard [{}]; ", down.join(" "));
        }
    }
    static LOG_SOURCES: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if sources != *last_sources {
        // WWE14_INPUT_SOURCES: log them (off by default: a line per press).
        if !sources.is_empty() && *LOG_SOURCES.get_or_init(|| std::env::var_os("WWE14_INPUT_SOURCES").is_some()) {
            eprintln!(
                "input source (frame {}, {} gamepads): {sources}",
                kernel::video::PRESENTED.load(Ordering::Relaxed),
                gamepads.iter().count()
            );
        }
        *last_sources = sources;
    }
    if let Some((g, _)) = gamepads.iter().next() {
        use GamepadButton as B;
        for (b, bit) in [
            (B::DPadUp, 0x0001),
            (B::DPadDown, 0x0002),
            (B::DPadLeft, 0x0004),
            (B::DPadRight, 0x0008),
            (B::Start, 0x0010),
            (B::Select, 0x0020),
            (B::LeftThumb, 0x0040),
            (B::RightThumb, 0x0080),
            (B::LeftTrigger, 0x0100),
            (B::RightTrigger, 0x0200),
            (B::South, 0x1000),
            (B::East, 0x2000),
            (B::West, 0x4000),
            (B::North, 0x8000),
        ] {
            if g.pressed(b) {
                p.buttons |= bit;
            }
        }
        let (l, r) = (g.left_stick(), g.right_stick());
        stick = [l.x, l.y, r.x, r.y];
        lt = g.get(B::LeftTrigger2).unwrap_or(0.0);
        rt = g.get(B::RightTrigger2).unwrap_or(0.0);
    }
    // The WWE 2K15 PC keyboard layout, on the pad buttons 2K14 uses.
    use KeyCode as K;
    for (k, bit) in [
        (K::Escape, 0x0010),     // pause: Start
        (K::KeyC, 0x0020),       // body indicator: Back
        (K::ControlLeft, 0x0080), // target another opponent: right-stick click
        (K::KeyQ, 0x0100),       // pick up / apron / enter or exit ring: LB
        (K::KeyE, 0x0200),       // toggle signatures and finishers: RB
        (K::ArrowDown, 0x1000),  // front facelock / grapple: A
        (K::Enter, 0x1000),      // menus: accept
        (K::ArrowRight, 0x2000), // Irish whip / pin: B
        (K::Backspace, 0x2000),  // menus: back
        (K::ArrowLeft, 0x4000),  // strike: X
        (K::ArrowUp, 0x8000),    // signature / finisher / OMG: Y
        (K::Digit1, 0x0001),     // taunts 1-4: D-pad
        (K::Digit2, 0x0002),
        (K::Digit3, 0x0004),
        (K::Digit4, 0x0008),
    ] {
        if keys.pressed(k) {
            p.buttons |= bit;
        }
    }
    // WASD move (left stick); with Space held they work the right stick
    // (chain wrestling, repositioning).
    let axis = |neg: KeyCode, pos: KeyCode| keys.pressed(pos) as i32 as f32 - keys.pressed(neg) as i32 as f32;
    let (x, y) = (axis(K::KeyA, K::KeyD), axis(K::KeyS, K::KeyW));
    let base = if keys.pressed(K::Space) { 2 } else { 0 };
    if x != 0.0 {
        stick[base] = x;
    }
    if y != 0.0 {
        stick[base + 1] = y;
    }
    if keys.pressed(K::ShiftLeft) {
        lt = 1.0; // run (hold + move)
    }
    if keys.pressed(K::KeyF) {
        rt = 1.0; // reversal / limb targeting
    }
    p.thumbs = stick.map(|v| (v.clamp(-1.0, 1.0) * 32767.0) as i16);
    p.left_trigger = (lt.clamp(0.0, 1.0) * 255.0) as u8;
    p.right_trigger = (rt.clamp(0.0, 1.0) * 255.0) as u8;
    kernel::set_pad(p);
}

fn exit_with_guest(guest: Res<Guest>) {
    if let Some(code) = guest.0.finished() {
        kernel::diag::log(&format!("the main thread returned 0x{code:08X}"));
        std::process::exit(code as i32);
    }
}

/// Starts the renderer on its own thread, on Bevy's device and queue. It renders
/// every frame the game finishes, in order (each one's resolves feed later
/// frames, as on the console's GPU), then wakes the window.
///
/// It used to render inside the window's frame. A heavy shot takes 13–15 ms
/// to render, so the window missed the display's refresh and showed only 35–43
/// of the game's 60 frames a second: the game kept time, but the picture
/// skipped (the user's entrances, 2026-11-25). The window now only copies the
/// newest finished image.
fn start_renderer(mut bridge: ResMut<Bridge>, device: Res<RenderDevice>, queue: Res<RenderQueue>) {
    let dirs = bridge.shader_dirs.clone();
    let mut r = render::Renderer::new(device.wgpu_device().clone(), (**queue).clone(), dirs);
    bridge.target = Some(r.target.clone());
    let rx = bridge.frames.lock().unwrap().take().expect("the renderer starts once");
    let rendered = bridge.rendered.clone();
    std::thread::Builder::new()
        .name("render".into())
        .spawn(move || {
            kernel::profile::register_host_thread("render");
            kernel::thread::make_urgent();
            let mut done = 0u64;
            // WWE14_PROFILE_DRAWS=N: a 15 s sampling profile whenever 30 frames
            // in a row have N draws or more (crowded shots), three times a run
            // at most. No key to press while playing.
            let heavy_draws: Option<usize> = std::env::var("WWE14_PROFILE_DRAWS").ok().and_then(|v| v.parse().ok());
            let (mut heavy_run, mut heavy_profiles) = (0u32, 0u32);
            loop {
                // While no frame arrives, images the game reads back are still
                // served: its frames stop while the reader waits.
                let f = match rx.recv_timeout(std::time::Duration::from_millis(4)) {
                    Ok(f) => f,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        r.serve_readbacks(done);
                        continue;
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                };
                if let Some(n) = heavy_draws
                    && heavy_profiles < 3
                {
                    let draws = f.draws().count();
                    heavy_run = if draws >= n { heavy_run + 1 } else { 0 };
                    if heavy_run == 30 && kernel::profile::start_now(15.0, &format!("frame {} is the 30th in a row with {n} draws or more ({draws}): sampling for 15 s", f.number)) {
                        heavy_profiles += 1;
                    }
                }
                let t = std::time::Instant::now();
                r.render(&f);
                done = f.number;
                r.serve_readbacks(done);
                let rendered_in = t.elapsed();
                // Queue order: the window's copy, submitted after this, sees
                // the whole frame.
                rendered.store(f.number, Ordering::Release);
                render::drop_later(f);
                if let Some(wake) = WAKE.get() {
                    wake();
                }
                // The game waited on this thread for 280-310 ms twice while no
                // frame took that long to render: name any such gap.
                let rest = t.elapsed().saturating_sub(rendered_in);
                if rest.as_millis() >= 50 {
                    eprintln!("render thread: {} ms outside rendering after a frame ({} ms rendering)", rest.as_millis(), rendered_in.as_millis());
                }
            }
        })
        .expect("render thread");
}

/// Copies the newest frame the render thread finished into the front-buffer
/// image, before Bevy draws the cameras.
fn draw_frame(mut ctx: RenderContext, mut bridge: ResMut<Bridge>, front: Option<Res<FrontBuffer>>, images: Res<RenderAssets<GpuImage>>) {
    let latest = bridge.rendered.load(Ordering::Acquire);
    let new = latest != 0 && latest != bridge.copied;
    // Every 5 s: window frames drawn, game frames shown in them, and game
    // frames the window never showed (more than one finished between two
    // window frames).
    {
        static WINDOW: std::sync::Mutex<Option<(std::time::Instant, u32, u32, u64)>> = std::sync::Mutex::new(None);
        let mut w = WINDOW.lock().unwrap();
        let (start, drawn, shown, skipped) = w.get_or_insert_with(|| (std::time::Instant::now(), 0, 0, 0));
        *drawn += 1;
        if new {
            *shown += 1;
            *skipped += latest.saturating_sub(bridge.copied).saturating_sub(1);
        }
        let secs = start.elapsed().as_secs_f64();
        if secs >= 5.0 {
            eprintln!(
                "perf: window {:.1} frames/s, game frames shown {:.1}/s, {} game frames never shown",
                *drawn as f64 / secs,
                *shown as f64 / secs,
                skipped
            );
            *w = None;
        }
    }
    if !new {
        return;
    }
    let number = latest;
    let Some(target) = bridge.target.clone() else { return };
    let Some(front) = front else { return };
    let Some(image) = images.get(&front.0) else { return };
    ctx.command_encoder().copy_texture_to_texture(
        target.as_image_copy(),
        image.texture.as_image_copy(),
        Extent3d { width: render::output_size().0, height: render::output_size().1, depth_or_array_layers: 1 },
    );
    bridge.copied = latest;
    bridge.shown += 1;
    if bridge.shown.is_power_of_two() && bridge.shown >= 64 {
        eprintln!("host: {} frames shown in the window (game frame {number})", bridge.shown);
    }
}

/// Runs the Bevy app with the guest booted on its own threads.
pub fn run(p: kernel::Program, kernel_args: Vec<String>, shader_dirs: Vec<PathBuf>) -> ! {
    // Audio first, so the game's first mixed frames already have somewhere to go.
    let _audio = crate::audio::start_output();
    let guest = kernel::boot(p, kernel_args);
    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            // Borderless fullscreen, so the scaled image (1920×1080 by default)
            // shows at full size; F11 switches to a 1280×720 window and back.
            primary_window: Some(Window {
                title: "WWE 2K14".into(),
                resolution: (render::WIDTH, render::HEIGHT).into(),
                mode: if std::env::var("WWE14_WINDOWED").is_ok() {
                    WindowMode::Windowed
                } else {
                    WindowMode::BorderlessFullscreen(MonitorSelection::Current)
                },
                ..default()
            }),
            ..default()
        }))
        .add_plugins(GpuBridgePlugin { shader_dirs })
        .insert_resource(Guest(guest))
        .run();
    std::process::exit(0)
}

/// The window's icon (title bar and taskbar): the executable's own, resource
/// 1, which the build embeds from the title's 64x64 image. Windows shows the
/// exe's icon in Explorer, but a window has none unless the program sets it
/// (the user, 2026-11-26). Set once, when the window exists.
fn set_window_icon(mut done: Local<bool>, _main_thread: bevy::ecs::system::NonSendMarker) {
    if *done {
        return;
    }
    #[cfg(windows)]
    bevy::winit::WINIT_WINDOWS.with_borrow(|windows| {
        use winit::platform::windows::{IconExtWindows, WindowExtWindows};
        for w in windows.windows.values() {
            // The small icon (title bar) and the large one (the taskbar button).
            match (winit::window::Icon::from_resource(1, None), winit::window::Icon::from_resource(1, Some(winit::dpi::PhysicalSize::new(256, 256)))) {
                (Ok(small), Ok(large)) => {
                    w.set_window_icon(Some(small));
                    w.set_taskbar_icon(Some(large));
                }
                (Err(e), _) | (_, Err(e)) => eprintln!("host: no window icon ({e})"),
            }
            *done = true;
        }
    });
}
