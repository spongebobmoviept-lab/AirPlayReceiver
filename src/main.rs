//! AirPlay speaker for Windows: an iPhone sees it in Control Center with nothing
//! installed on the phone. Shows the track and artwork, sends play/pause/skip back
//! to the phone, and plays on whichever sound device is picked. Closing the window
//! leaves just the speaker running in the taskbar tray.
#![windows_subsystem = "windows"]

mod audio;

use audio::{Command, Player};
use eframe::egui::{self, Color32, FontFamily, FontId, RichText, Stroke, Vec2};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use shairplay::raop::TrackMetadata;
use shairplay::{Ap1Encryption, AudioFormat, AudioHandler, AudioSession, RaopServer, RemoteCommand, RemoteControl, ShairplayError};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMSBT_TRANSIENTWINDOW, DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE, DwmExtendFrameIntoClientArea, DwmSetWindowAttribute};
use windows::Win32::System::ProcessStatus::K32EmptyWorkingSet;
use windows::Win32::System::Threading::{CreateEventW, GetCurrentProcess, GetCurrentThreadId, INFINITE, SetEvent, WaitForSingleObject};
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, GetMessageW, MSG, PostThreadMessageW, TranslateMessage, WM_NULL};
use windows::core::w;

const ICON_PLAY: &str = "\u{E768}";
const ICON_PAUSE: &str = "\u{E769}";
const ICON_PREVIOUS: &str = "\u{E892}";
const ICON_NEXT: &str = "\u{E893}";
const ICON_SHUFFLE: &str = "\u{E8B1}";
const ICON_MUSIC: &str = "\u{EC4F}";
const ICON_VOLUME: &str = "\u{E767}";
const ICON_SPEAKER: &str = "\u{E7F5}";
const ICON_SETTINGS: &str = "\u{E713}";

/// Default target 50 ms short of the 2 s the phone expects, to allow for the sound device's own delay.
const DEFAULT_DELAY_MS: u32 = 1950;
const FORGET_PHONE_AFTER: Duration = Duration::from_secs(15 * 60);
const THEME_CHECK: Duration = Duration::from_secs(2);
/// One press of an iPhone's volume buttons, which is all it takes over the remote.
const PHONE_STEP: f32 = 1.0 / 16.0;
/// The phone usually reports a press within milliseconds but can take over a second.
/// Pressing again before that overshoots, so each press waits this long for its report.
const ECHO_WAIT: Duration = Duration::from_millis(1500);
/// Reports this soon after the last press are its echo; anything later is someone
/// moving the phone's own slider.
const ECHO_WINDOW: Duration = Duration::from_millis(300);
/// A phone that answers none of this many presses in a row isn't listening just now.
const GIVE_UP_AFTER: u32 = 4;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const STARTUP_APPROVED_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
const RUN_VALUE: &str = "AirPlay Receiver";
/// Started this way it goes straight to the tray, with no window.
const BACKGROUND_FLAG: &str = "--background";

fn starts_with_windows() -> bool {
    windows_registry::CURRENT_USER.open(RUN_KEY).and_then(|key| key.get_string(RUN_VALUE)).is_ok()
}

fn set_starts_with_windows(on: bool) -> Result<(), String> {
    let key = windows_registry::CURRENT_USER.create(RUN_KEY).map_err(|error| error.to_string())?;
    if on {
        let exe = std::env::current_exe().map_err(|error| error.to_string())?;
        key.set_string(RUN_VALUE, format!("\"{}\" {BACKGROUND_FLAG}", exe.display())).map_err(|error| error.to_string())?;
        // An old "disabled" mark from Task Manager would silently win over the new entry.
        if let Ok(approved) = windows_registry::CURRENT_USER.create(STARTUP_APPROVED_KEY) {
            let _ = approved.remove_value(RUN_VALUE);
        }
        Ok(())
    } else {
        key.remove_value(RUN_VALUE).map_err(|error| error.to_string())
    }
}

/// What Windows is set to right now: light or dark, and the accent shades Windows itself
/// uses, Light 2 on dark mode and Dark 1 on light mode.
#[derive(Clone, Copy, PartialEq)]
struct WindowsTheme {
    light: bool,
    accent_on_dark: Color32,
    accent_on_light: Color32,
}

impl WindowsTheme {
    fn from_palette(light: bool, palette: &[u8]) -> Self {
        let shade = |index: usize| match palette.get(index * 4..index * 4 + 3) {
            Some(rgb) => Color32::from_rgb(rgb[0], rgb[1], rgb[2]),
            None => Color32::from_rgb(0x00, 0x78, 0xD4), // Windows' default blue
        };
        Self { light, accent_on_dark: shade(1), accent_on_light: shade(4) }
    }

    fn read() -> Self {
        const PERSONALIZE: &str = r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";
        const ACCENT: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\Accent";
        let user = windows_registry::CURRENT_USER;
        let light = match std::env::var("AIRPLAY_THEME").as_deref() {
            Ok(theme) => theme == "light",
            Err(_) => user.open(PERSONALIZE).and_then(|key| key.get_u32("AppsUseLightTheme")).is_ok_and(|value| value != 0),
        };
        let palette = user.open(ACCENT).and_then(|key| key.get_value("AccentPalette"));
        match palette {
            Ok(palette) => Self::from_palette(light, &palette),
            Err(_) => Self::from_palette(light, &[]),
        }
    }

    fn apply(self, ctx: &egui::Context) {
        ctx.style_mut_of(egui::Theme::Dark, |style| style_like_windows(style, true, self.accent_on_dark));
        ctx.style_mut_of(egui::Theme::Light, |style| style_like_windows(style, false, self.accent_on_light));
        ctx.set_theme(if self.light { egui::Theme::Light } else { egui::Theme::Dark });
    }
}

/// Windows 11's own colors.
fn style_like_windows(style: &mut egui::Style, dark: bool, accent: Color32) {
    let rgba = Color32::from_rgba_unmultiplied;
    let (text, secondary, tint, paper, border, subtle, fill, hover) = if dark {
        (
            Color32::WHITE,
            rgba(255, 255, 255, 200),
            rgba(32, 32, 32, 153),
            Color32::from_rgb(0x2C, 0x2C, 0x2C),
            rgba(255, 255, 255, 71),
            rgba(255, 255, 255, 23),
            rgba(255, 255, 255, 15),
            rgba(255, 255, 255, 23),
        )
    } else {
        (
            rgba(0, 0, 0, 228),
            rgba(0, 0, 0, 155),
            rgba(243, 243, 243, 153),
            Color32::from_rgb(0xF9, 0xF9, 0xF9),
            rgba(0, 0, 0, 82),
            rgba(0, 0, 0, 23),
            rgba(255, 255, 255, 179),
            rgba(249, 249, 249, 128),
        )
    };
    let mut visuals = if dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    visuals.panel_fill = tint; // tints the acrylic to match Windows' own panels
    visuals.window_fill = paper;
    visuals.window_stroke = Stroke::new(1.0, subtle);
    visuals.window_corner_radius = 8.into();
    visuals.menu_corner_radius = 8.into();
    visuals.extreme_bg_color = fill;
    visuals.text_edit_bg_color = Some(fill);
    visuals.faint_bg_color = subtle;
    visuals.weak_text_color = Some(secondary);
    visuals.selection.bg_fill = accent;
    visuals.selection.stroke = Stroke::new(1.0, text);
    visuals.slider_trailing_fill = true;
    for widget in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widget.fg_stroke = Stroke::new(1.0, text);
        widget.corner_radius = 4.into();
        widget.expansion = 0.0;
    }
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, subtle);
    visuals.widgets.noninteractive.bg_fill = paper;
    visuals.widgets.noninteractive.weak_bg_fill = paper;
    visuals.widgets.inactive.bg_fill = border; // slider rails and handles
    visuals.widgets.inactive.weak_bg_fill = fill; // buttons and drop-downs
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, subtle);
    visuals.widgets.hovered.bg_fill = text;
    visuals.widgets.hovered.weak_bg_fill = hover;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, subtle);
    visuals.widgets.active.bg_fill = text;
    visuals.widgets.active.weak_bg_fill = fill;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0, subtle);
    visuals.widgets.open.bg_fill = hover;
    visuals.widgets.open.weak_bg_fill = hover;
    visuals.widgets.open.bg_stroke = Stroke::new(1.0, subtle);
    style.visuals = visuals;
    style.spacing.item_spacing = Vec2::new(8.0, 8.0);
    style.spacing.button_padding = Vec2::new(11.0, 5.0);
    style.text_styles.insert(egui::TextStyle::Body, FontId::proportional(14.0));
    style.text_styles.insert(egui::TextStyle::Button, FontId::proportional(14.0));
    style.text_styles.insert(egui::TextStyle::Small, FontId::proportional(12.0));
}

/// Gives the window Windows' acrylic backdrop, showing through wherever the app draws
/// nothing. Skipped quietly on a Windows too old for it; the tint still looks right.
fn use_acrylic(hwnd: HWND) {
    let margins = MARGINS { cxLeftWidth: -1, cxRightWidth: -1, cyTopHeight: -1, cyBottomHeight: -1 };
    let backdrop = DWMSBT_TRANSIENTWINDOW;
    // SAFETY: a live window of ours, and values that outlive the calls.
    unsafe {
        let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, (&raw const backdrop).cast(), size_of_val(&backdrop) as u32);
    }
}

/// Matches the title bar and the acrylic's tint to light or dark.
fn set_dark_frame(hwnd: HWND, dark: bool) {
    let value = i32::from(dark);
    // SAFETY: a live window of ours, and a value that outlives the call.
    let _ = unsafe { DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, (&raw const value).cast(), size_of_val(&value) as u32) };
}

/// Windows' toggle switch.
fn toggle_switch(ui: &mut egui::Ui, on: &mut bool, label: &str) -> egui::Response {
    let (rect, mut response) = ui.allocate_exact_size(Vec2::new(40.0, 20.0), egui::Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    response.widget_info(|| egui::WidgetInfo::selected(egui::Role::CheckBox, true, *on, label));
    let visuals = ui.visuals().clone();
    let how_on = ui.ctx().animate_bool_responsive(response.id, *on);
    let radius = rect.height() / 2.0;
    if *on {
        ui.painter().rect_filled(rect, radius, visuals.selection.bg_fill);
    } else {
        ui.painter().rect_stroke(rect, radius, Stroke::new(1.0, visuals.widgets.inactive.bg_fill), egui::StrokeKind::Inside);
    }
    let thumb = egui::pos2(egui::lerp(rect.left() + radius..=rect.right() - radius, how_on), rect.center().y);
    let thumb_color = match (*on, visuals.dark_mode) {
        (true, true) => Color32::BLACK,
        (true, false) => Color32::WHITE,
        (false, true) => Color32::from_rgb(0xCF, 0xCF, 0xCF),
        (false, false) => Color32::from_rgb(0x5D, 0x5D, 0x5D),
    };
    ui.painter().circle_filled(thumb, if response.hovered() { 7.0 } else { 6.0 }, thumb_color);
    response
}

/// A name the network can carry: no control characters, and short enough to fit in one
/// DNS label (63 bytes) along with the 13-byte address prefix.
fn clean_name(name: &str) -> String {
    let mut clean: String = name.chars().filter(|c| !c.is_control()).collect::<String>().trim().to_string();
    while clean.len() > 50 {
        clean.pop();
    }
    clean.trim_end().to_string()
}

struct Settings {
    name: String,
    device: Option<String>,
    delay_ms: u32,
}

impl Settings {
    fn dir() -> PathBuf {
        PathBuf::from(std::env::var_os("APPDATA").unwrap_or_default()).join("AirPlay Receiver")
    }

    fn parse(text: &str) -> Self {
        let mut settings = Self {
            name: "PC Speaker".to_string(),
            device: None,
            delay_ms: DEFAULT_DELAY_MS,
        };
        for (key, value) in text.lines().filter_map(|line| line.split_once('=')) {
            match key {
                "name" if !clean_name(value).is_empty() => settings.name = clean_name(value),
                "device" if !value.is_empty() => settings.device = Some(value.to_string()),
                "delay_ms" => settings.delay_ms = value.parse().unwrap_or(DEFAULT_DELAY_MS).clamp(200, 5000),
                _ => {}
            }
        }
        settings
    }

    fn load() -> Self {
        Self::parse(&std::fs::read_to_string(Self::dir().join("settings.txt")).unwrap_or_default())
    }

    fn save(&self) {
        let text = format!(
            "name={}\ndevice={}\ndelay_ms={}\n",
            self.name,
            self.device.as_deref().unwrap_or(""),
            self.delay_ms
        );
        if let Err(error) = std::fs::write(Self::dir().join("settings.txt"), text) {
            tracing::warn!(%error, "couldn't save settings");
        }
    }
}

/// What the AirPlay engine knows, for the window to draw.
#[derive(Default)]
struct NowPlaying {
    connections: usize,
    /// When the phone hung up. It often does that on pause and still takes commands.
    hung_up: Option<Instant>,
    /// Other Apple devices on the network probe the speaker too, so this isn't the client
    /// until it sets up a stream.
    last_connection: String,
    client: String,
    remote: Option<Arc<dyn RemoteControl>>,
    title: String,
    artist: String,
    album: String,
    art: Option<Arc<Vec<u8>>>,
    /// Goes up with each new picture, so a window knows when to redraw it.
    art_version: u64,
    /// Seconds into the track when `position_frames` of audio had been received.
    position: f64,
    position_frames: u64,
    duration: f64,
    message: String,
    /// Where the phone's volume slider is, 0 to 1.
    phone_level: Option<f32>,
    /// The next volume report is the phone's opening one.
    awaiting_first_volume: bool,
    /// While we step the phone, its reports are it following us, not the user moving it.
    following_until: Option<Instant>,
}

impl NowPlaying {
    fn forget_phone(&mut self) {
        *self = Self { art_version: self.art_version + 1, ..Default::default() };
    }
}

/// Seconds into the track and its length, from a progress report in 44.1 kHz ticks.
/// After a long pause an iPhone can report a position past the end, which means nothing.
fn read_progress(start: u32, current: u32, end: u32) -> (Option<f64>, f64) {
    let (into, length) = (current.wrapping_sub(start), end.wrapping_sub(start));
    ((into <= length).then_some(f64::from(into) / 44100.0), f64::from(length) / 44100.0)
}

/// What a sender's play status means. Seen from an iPhone: 1 playing, 2 paused. iTunes-style
/// senders use 4 and 3 for the same, with 2 meaning stopped. Anything else says nothing.
fn phone_is_playing(status: u8) -> Option<bool> {
    match status {
        1 | 4 => Some(true),
        2 | 3 => Some(false),
        _ => None,
    }
}

/// An AirPlay volume in dB, as a slider position: -30 to 0 is the whole slider, -144 is mute.
fn level_from_db(db: f32) -> f32 {
    if db <= -144.0 { 0.0 } else { ((db + 30.0) / 30.0).clamp(0.0, 1.0) }
}

/// Keeps the phone's slider on the output's level.
#[derive(Clone)]
struct PhoneVolume {
    now: Arc<Mutex<NowPlaying>>,
    player: Arc<Player>,
    busy: Arc<AtomicBool>,
}

impl PhoneVolume {
    /// Step the phone to the current level, live. A run already going picks up changes itself.
    fn follow_output(&self) {
        if self.busy.swap(true, SeqCst) {
            return;
        }
        let this = self.clone();
        std::thread::spawn(move || {
            loop {
                let target = this.step_phone();
                this.busy.store(false, SeqCst);
                // A move in the instant before `busy` cleared would have been turned away.
                if this.player.level() == target || this.busy.swap(true, SeqCst) {
                    return;
                }
            }
        });
    }

    /// Returns the level it was last heading for.
    fn step_phone(&self) -> f32 {
        let mut unanswered = 0;
        loop {
            let target = self.player.level();
            let (remote, phone) = {
                let now = self.now.lock().unwrap();
                // A hung-up phone says OK to a press, then applies it when it comes back.
                if now.connections == 0 {
                    return target;
                }
                (now.remote.clone(), now.phone_level)
            };
            let (Some(remote), Some(phone)) = (remote, phone) else { return target };
            let difference = target - phone;
            if difference.abs() < PHONE_STEP / 2.0 {
                return target;
            }
            self.now.lock().unwrap().following_until = Some(Instant::now() + ECHO_WAIT + ECHO_WINDOW);
            let press = if difference > 0.0 { "volumeup" } else { "volumedown" };
            if let Err(error) = remote.send_command(RemoteCommand::Raw(press.to_string())) {
                tracing::warn!(%error, "phone refused a volume step");
                return target;
            }
            let pressed_at = Instant::now();
            let answered = loop {
                if self.now.lock().unwrap().phone_level != Some(phone) {
                    break true;
                }
                if pressed_at.elapsed() > ECHO_WAIT {
                    break false;
                }
                std::thread::sleep(Duration::from_millis(5));
            };
            self.now.lock().unwrap().following_until = Some(Instant::now() + ECHO_WINDOW);
            unanswered = if answered { 0 } else { unanswered + 1 };
            if unanswered == GIVE_UP_AFTER {
                tracing::warn!("phone isn't answering volume steps");
                return target;
            }
        }
    }
}

/// The window's context while one is open, for anything that wants it redrawn.
#[derive(Clone, Default)]
struct Screen(Arc<Mutex<Option<egui::Context>>>);

impl Screen {
    fn repaint(&self) {
        if let Some(ctx) = &*self.0.lock().unwrap() {
            ctx.request_repaint();
        }
    }

    /// Brings an open window forward. False if there isn't one.
    fn focus(&self) -> bool {
        let screen = self.0.lock().unwrap();
        if let Some(ctx) = &*screen {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        screen.is_some()
    }

    fn close(&self) {
        if let Some(ctx) = &*self.0.lock().unwrap() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn set(&self, ctx: Option<egui::Context>) {
        *self.0.lock().unwrap() = ctx;
    }
}

struct Engine {
    now: Arc<Mutex<NowPlaying>>,
    player: Arc<Player>,
    commands: Sender<Command>,
    screen: Screen,
}

impl Engine {
    fn update(&self, change: impl FnOnce(&mut NowPlaying)) {
        change(&mut self.now.lock().unwrap());
        self.screen.repaint();
    }
}

impl AudioHandler for Engine {
    fn audio_init(&self, format: AudioFormat) -> Box<dyn AudioSession> {
        tracing::info!(channels = format.channels, rate = format.sample_rate, "stream started");
        self.now.lock().unwrap().awaiting_first_volume = true;
        Box::new(self.player.new_session(format.channels, self.commands.clone()))
    }

    fn on_volume(&self, db: f32) {
        let level = level_from_db(db);
        tracing::info!(db, level, "phone volume");
        let (first, following) = {
            let mut now = self.now.lock().unwrap();
            now.phone_level = Some(level);
            let following = now.following_until.is_some_and(|until| Instant::now() < until);
            (std::mem::take(&mut now.awaiting_first_volume), following)
        };
        // A phone's opening report only says where it is. The output keeps its level and the
        // phone is brought to it once sound starts, so connecting a phone left at full
        // volume can't blast anything.
        if !first && !following {
            self.player.set_level(level);
            let _ = self.commands.send(Command::SetLevel(level));
            self.screen.repaint();
        }
    }

    fn on_metadata(&self, track: &TrackMetadata) {
        tracing::info!(title = ?track.title, artist = ?track.artist, album = ?track.album, status = ?track.play_status, "track");
        if let Some(playing) = track.play_status.and_then(phone_is_playing) {
            self.player.set_phone_playing(Some(playing));
        }
        self.update(|now| {
            now.title = track.title.clone().unwrap_or_default();
            now.artist = track.artist.clone().unwrap_or_default();
            now.album = track.album.clone().unwrap_or_default();
            if let Some(ms) = track.duration_ms {
                now.duration = f64::from(ms) / 1000.0;
            }
        });
    }

    fn on_coverart(&self, art: &[u8]) {
        tracing::info!(bytes = art.len(), "artwork");
        self.update(|now| {
            // A paused iPhone resends the same picture every 10 s.
            if now.art.as_ref().map_or(&[][..], |current| current.as_slice()) != art {
                now.art = (!art.is_empty()).then(|| Arc::new(art.to_vec()));
                now.art_version += 1;
            }
        });
    }

    fn on_progress(&self, start: u32, current: u32, end: u32) {
        tracing::info!(start, current, end, "progress");
        let frames = self.player.frames_received();
        let (position, length) = read_progress(start, current, end);
        self.update(|now| {
            now.duration = length;
            if let Some(position) = position {
                now.position = position;
                now.position_frames = frames;
            }
        });
    }

    fn on_remote_control(&self, remote: Arc<dyn RemoteControl>) {
        tracing::info!("remote control available");
        self.update(|now| {
            now.remote = Some(remote);
            now.client = now.last_connection.clone();
        });
    }

    fn on_client_connected(&self, addr: &str) {
        tracing::info!(addr, "connected");
        self.update(|now| {
            now.connections += 1;
            now.hung_up = None;
            now.last_connection = addr.to_string();
        });
    }

    fn on_client_disconnected(&self, addr: &str) {
        tracing::info!(addr, "disconnected");
        // Phones open extra short-lived connections, so only the last one closing means gone.
        // The track and the remote stay, so Play can still bring the phone back.
        let mut gone = false;
        self.update(|now| {
            now.connections = now.connections.saturating_sub(1);
            gone = now.connections == 0;
            if gone {
                now.hung_up = Some(Instant::now());
            }
        });
        if gone {
            self.player.stop();
            self.player.set_phone_playing(None);
        }
    }

    fn on_error(&self, error: &ShairplayError) {
        tracing::warn!(%error, "AirPlay error");
        self.update(|now| now.message = error.to_string());
    }
}

/// Stable per PC and per name. Phones remember speakers by this address, including what
/// they were called, so a new name needs a new address or the old name sticks.
fn hardware_address(name: &str) -> [u8; 6] {
    let mut hasher = DefaultHasher::new();
    std::env::var("COMPUTERNAME").unwrap_or_default().hash(&mut hasher);
    name.hash(&mut hasher);
    let bytes = hasher.finish().to_be_bytes();
    // Locally administered, unicast.
    [(bytes[0] | 0x02) & !0x01, bytes[1], bytes[2], bytes[3], bytes[4], bytes[5]]
}

/// Everything that keeps running with no window open.
struct Core {
    now: Arc<Mutex<NowPlaying>>,
    player: Arc<Player>,
    commands: Sender<Command>,
    phone_volume: PhoneVolume,
    screen: Screen,
    settings: Mutex<Settings>,
    runtime: tokio::runtime::Runtime,
    engine: Arc<Engine>,
    server: Mutex<Option<RaopServer>>,
}

impl Core {
    fn start() -> Arc<Self> {
        let settings = Settings::load();
        let player = Player::new(settings.delay_ms);
        let now = Arc::new(Mutex::new(NowPlaying::default()));
        let screen = Screen::default();
        let phone_volume = PhoneVolume { now: now.clone(), player: player.clone(), busy: Arc::default() };

        let (commands, command_rx) = std::sync::mpsc::channel();
        let output_player = player.clone();
        let output_device = settings.device.clone();
        let output_screen = screen.clone();
        let output_follow = phone_volume.clone();
        std::thread::spawn(move || {
            audio::run_output(
                output_player,
                output_device,
                command_rx,
                move || output_screen.repaint(),
                move || output_follow.follow_output(),
            )
        });

        // One phone at a time doesn't need a thread per CPU core.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("can't start async runtime");
        let engine = Arc::new(Engine {
            now: now.clone(),
            player: player.clone(),
            commands: commands.clone(),
            screen: screen.clone(),
        });
        let core = Arc::new(Self {
            now,
            player,
            commands,
            phone_volume,
            screen,
            settings: Mutex::new(settings),
            runtime,
            engine,
            server: Mutex::new(None),
        });
        core.start_server();
        core
    }

    fn start_server(&self) {
        let name = self.settings.lock().unwrap().name.clone();
        let engine = self.engine.clone();
        let server = self.runtime.block_on(async {
            let mut server = RaopServer::builder()
                .name(name.as_str())
                .hwaddr(hardware_address(&name))
                // The same pair of modes real AirPlay 1 speakers offer. iPhones pick RSA.
                .advertise_encryption([Ap1Encryption::None, Ap1Encryption::Rsa])
                .build(engine)?;
            server.start().await?;
            Ok::<_, ShairplayError>(server)
        });
        match server {
            Ok(server) => {
                tracing::info!(port = server.service_info().port, name, "AirPlay speaker running");
                *self.server.lock().unwrap() = Some(server);
            }
            Err(error) => {
                tracing::error!(%error, "couldn't start");
                self.now.lock().unwrap().message = format!("Couldn't start AirPlay: {error}");
            }
        }
    }

    fn stop_server(&self) {
        if let Some(mut server) = self.server.lock().unwrap().take() {
            self.runtime.block_on(server.stop());
        }
    }

    fn rename(&self, name: String) {
        tracing::info!(name, "renaming");
        {
            let mut settings = self.settings.lock().unwrap();
            settings.name = name;
            settings.save();
        }
        self.stop_server();
        self.start_server();
    }

    fn shutdown(&self) {
        self.stop_server();
        // Time for the goodbye to reach the network, so phones drop the speaker at once.
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn install_fonts(ctx: &egui::Context) {
    let fonts_dir = PathBuf::from(std::env::var_os("WINDIR").unwrap_or_default()).join("Fonts");
    let mut fonts = egui::FontDefinitions::default();
    let mut load = |file: &str| -> bool {
        let Ok(bytes) = std::fs::read(fonts_dir.join(file)) else { return false };
        fonts.font_data.insert(file.to_string(), Arc::new(egui::FontData::from_owned(bytes)));
        true
    };
    // Segoe for text, then the system fonts that cover Japanese, Korean and Chinese titles.
    let text: Vec<String> = ["segoeui.ttf", "YuGothM.ttc", "malgun.ttf", "msyh.ttc"]
        .into_iter()
        .filter(|file| load(file))
        .map(str::to_string)
        .collect();
    let has_semibold = load("seguisb.ttf");
    let has_icons = load("segmdl2.ttf");

    let proportional = fonts.families.entry(FontFamily::Proportional).or_default();
    proportional.splice(0..0, text.iter().cloned());
    let mut semibold = text;
    if has_semibold {
        semibold.insert(0, "seguisb.ttf".to_string());
    }
    fonts.families.insert(FontFamily::Name("semibold".into()), semibold);
    fonts.families.insert(
        FontFamily::Name("icons".into()),
        if has_icons { vec!["segmdl2.ttf".to_string()] } else { Vec::new() },
    );
    ctx.set_fonts(fonts);
}

fn icon(glyph: &str, size: f32) -> RichText {
    RichText::new(glyph).font(FontId::new(size, FontFamily::Name("icons".into())))
}

fn format_time(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    format!("{}:{:02}", total / 60, total % 60)
}

/// An icon button that screen readers can still name.
fn icon_button(ui: &mut egui::Ui, glyph: &str, label: &str, size: f32, enabled: bool) -> egui::Response {
    let button = egui::Button::new(icon(glyph, size)).frame(false).min_size(Vec2::splat(size + 22.0));
    let response = ui.add_enabled(enabled, button).on_hover_text(label);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, enabled, label));
    response
}

/// The window. It only exists while open; closing it frees all of this.
struct Window {
    core: Arc<Core>,
    requests: Requests,
    art: Option<egui::TextureHandle>,
    art_version: u64,
    scrub: Option<f64>,
    name_draft: String,
    rename_to: Option<String>,
    hwnd: Option<HWND>,
    theme: WindowsTheme,
    theme_checked: Instant,
    autostart: bool,
    settings_problem: String,
}

impl Window {
    fn new(cc: &eframe::CreationContext<'_>, core: Arc<Core>, requests: Requests) -> Self {
        install_fonts(&cc.egui_ctx);
        let theme = WindowsTheme::read();
        theme.apply(&cc.egui_ctx);
        let hwnd = cc.window_handle().ok().and_then(|handle| match handle.as_raw() {
            RawWindowHandle::Win32(window) => Some(HWND(window.hwnd.get() as *mut _)),
            _ => None,
        });
        if let Some(hwnd) = hwnd {
            set_dark_frame(hwnd, !theme.light);
            use_acrylic(hwnd);
        }
        core.screen.set(Some(cc.egui_ctx.clone()));
        let name_draft = core.settings.lock().unwrap().name.clone();
        Self {
            core,
            requests,
            art: None,
            art_version: u64::MAX,
            scrub: None,
            name_draft,
            rename_to: None,
            hwnd,
            theme,
            theme_checked: Instant::now(),
            autostart: starts_with_windows(),
            settings_problem: String::new(),
        }
    }

    /// Send a command to the phone without freezing the window if the phone is slow.
    fn send(&self, remote: &Arc<dyn RemoteControl>, command: RemoteCommand) {
        let (remote, now, screen) = (remote.clone(), self.core.now.clone(), self.core.screen.clone());
        std::thread::spawn(move || {
            let result = remote.send_command(command.clone());
            tracing::info!(?command, ok = result.is_ok(), "command");
            let mut now = now.lock().unwrap();
            match result {
                Ok(()) => now.message.clear(),
                Err(error) => {
                    tracing::warn!(%error, "command failed");
                    if now.connections == 0 {
                        // Hung up and no longer answering: the phone has really gone.
                        now.forget_phone();
                    }
                    now.message = "The phone didn't take that command".to_string();
                }
            }
            screen.repaint();
        });
    }

    fn draw_art(&self, ui: &mut egui::Ui, size: f32) {
        let square = Vec2::splat(size);
        if let Some(art) = &self.art {
            ui.add(egui::Image::new((art.id(), square)).corner_radius(10));
        } else {
            let (rect, _) = ui.allocate_exact_size(square, egui::Sense::hover());
            let visuals = ui.visuals();
            ui.painter().rect_filled(rect, 10, visuals.faint_bg_color);
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                ICON_MUSIC,
                FontId::new(size * 0.3, FontFamily::Name("icons".into())),
                visuals.weak_text_color(),
            );
        }
    }

    /// The little popup behind the gear.
    fn draw_settings(&mut self, ui: &mut egui::Ui) {
        ui.label("Speaker name");
        let edit = ui.add(egui::TextEdit::singleline(&mut self.name_draft).desired_width(f32::INFINITY));
        edit.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Speaker name"));
        if edit.lost_focus() {
            self.apply_name();
        }
        ui.label(RichText::new("What the iPhone lists it as. Renaming disconnects a phone that's playing.").small().weak());
        ui.add_space(4.0);

        ui.horizontal(|ui| {
            ui.label("Start with Windows");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if toggle_switch(ui, &mut self.autostart, "Start with Windows").changed() {
                    match set_starts_with_windows(self.autostart) {
                        Ok(()) => self.settings_problem.clear(),
                        Err(error) => {
                            tracing::warn!(error, "couldn't change start with Windows");
                            self.settings_problem = format!("Couldn't change that: {error}");
                            self.autostart = starts_with_windows();
                        }
                    }
                }
            });
        });
        if !self.settings_problem.is_empty() {
            ui.label(RichText::new(&self.settings_problem).small());
        }
        ui.separator();

        ui.label(RichText::new("Closing the window keeps the speaker running in the taskbar tray.").small().weak());
        if ui.button("Quit AirPlay Receiver").clicked() {
            self.requests.quit();
        }
    }

    /// Takes up what was typed in the name box. The rename itself waits for the end of the frame.
    fn apply_name(&mut self) {
        let name = clean_name(&self.name_draft);
        let current = self.core.settings.lock().unwrap().name.clone();
        if name.is_empty() || name == current {
            self.name_draft = current;
        } else {
            self.rename_to = Some(name);
        }
    }

    fn draw_output_picker(&mut self, ui: &mut egui::Ui) {
        let current = self.core.settings.lock().unwrap().device.clone();
        let devices = self.core.player.output.lock().unwrap().devices.clone();
        let selected = match &current {
            None => "Windows default".to_string(),
            Some(id) => devices.iter().find(|device| &device.id == id).map_or("Unavailable device".to_string(), |device| device.name.clone()),
        };

        let mut picked = None;
        let combo = egui::ComboBox::from_id_salt("output")
            .selected_text(selected)
            .width(ui.available_width())
            .truncate()
            .show_ui(ui, |ui| {
                if ui.selectable_label(current.is_none(), "Windows default").clicked() {
                    picked = Some(None);
                }
                for device in &devices {
                    if ui.selectable_label(current.as_ref() == Some(&device.id), &device.name).clicked() {
                        picked = Some(Some(device.id.clone()));
                    }
                }
            });
        if combo.response.clicked() {
            let _ = self.core.commands.send(Command::RefreshDevices);
        }
        if let Some(device) = picked {
            {
                let mut settings = self.core.settings.lock().unwrap();
                settings.device = device.clone();
                settings.save();
            }
            let _ = self.core.commands.send(Command::Select(device));
        }
    }
}

impl eframe::App for Window {
    // Nothing behind the panel, so the acrylic shows through its tint.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0; 4]
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let core = self.core.clone();
        // Windows changing light/dark or the accent shows up here within a moment.
        if self.theme_checked.elapsed() >= THEME_CHECK {
            self.theme_checked = Instant::now();
            let theme = WindowsTheme::read();
            if theme != self.theme {
                self.theme = theme;
                theme.apply(&ctx);
                if let Some(hwnd) = self.hwnd {
                    set_dark_frame(hwnd, !theme.light);
                }
            }
        }
        let receiving = core.player.receiving();
        let frames = core.player.frames_received();
        let speaker_name = core.settings.lock().unwrap().name.clone();

        let mut now = core.now.lock().unwrap();
        // Long after a hang-up, stop offering a phone that has probably moved on.
        if now.hung_up.is_some_and(|since| since.elapsed() > FORGET_PHONE_AFTER) {
            tracing::info!("forgetting the phone");
            now.forget_phone();
        }
        if self.art_version != now.art_version {
            self.art_version = now.art_version;
            self.art = now.art.as_ref().and_then(|bytes| image::load_from_memory(bytes).ok()).map(|art| {
                let art = art.to_rgba8();
                let size = [art.width() as usize, art.height() as usize];
                let pixels = egui::ColorImage::from_rgba_unmultiplied(size, art.as_flat_samples().as_slice());
                ctx.load_texture("art", pixels, egui::TextureOptions::LINEAR)
            });
        }
        let mut position = now.position + frames.saturating_sub(now.position_frames) as f64 / 44100.0;
        if now.duration > 0.0 {
            position = position.min(now.duration);
        }
        let connected = now.connections > 0;
        let known = connected || now.hung_up.is_some();
        ctx.request_repaint_after(if connected { Duration::from_millis(250) } else { THEME_CHECK });
        let remote = now.remote.clone();
        let has_remote = remote.is_some();

        let margin = egui::Frame::central_panel(ui.style()).inner_margin(18);
        egui::CentralPanel::default().frame(margin).show(ui, |ui| {
            let size = ui.available_width().min(ui.available_height() - 270.0).max(96.0);
            ui.vertical_centered(|ui| self.draw_art(ui, size));
            ui.add_space(6.0);

            let (title, subtitle) = if !known || (now.title.is_empty() && !receiving) {
                ("Waiting for your iPhone".to_string(), format!("Control Center \u{203A} AirPlay \u{203A} {speaker_name}"))
            } else if now.title.is_empty() {
                ("Playing".to_string(), now.client.clone())
            } else {
                let subtitle = [now.artist.as_str(), now.album.as_str()]
                    .into_iter()
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join(" \u{2014} ");
                (now.title.clone(), subtitle)
            };
            ui.add(egui::Label::new(RichText::new(title).font(FontId::new(20.0, FontFamily::Name("semibold".into())))).truncate());
            ui.add(egui::Label::new(RichText::new(subtitle).weak()).truncate());

            if now.duration > 0.0 {
                ui.spacing_mut().slider_width = ui.available_width();
                let mut shown = self.scrub.unwrap_or(position);
                let slider = egui::Slider::new(&mut shown, 0.0..=now.duration).show_value(false);
                let response = ui.add_enabled(has_remote, slider);
                if response.dragged() {
                    self.scrub = Some(shown);
                }
                if response.drag_stopped() {
                    self.scrub = None;
                    now.position = shown;
                    now.position_frames = frames;
                    if let Some(remote) = &remote {
                        let path = format!("setproperty?dacp.playingtime={}", (shown * 1000.0) as u64);
                        self.send(remote, RemoteCommand::Raw(path));
                    }
                }
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format_time(shown)).small().weak());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new(format!("-{}", format_time(now.duration - shown))).small().weak());
                    });
                });
            }

            ui.horizontal(|ui| {
                ui.add_space(((ui.available_width() - 262.0) / 2.0).max(0.0));
                let play_glyph = if receiving { ICON_PAUSE } else { ICON_PLAY };
                let play_label = if receiving { "Pause" } else { "Play" };
                let mut command = None;
                if icon_button(ui, ICON_SHUFFLE, "Shuffle", 18.0, has_remote).clicked() {
                    command = Some(RemoteCommand::Raw("shuffle_songs".to_string()));
                }
                if icon_button(ui, ICON_PREVIOUS, "Previous", 22.0, has_remote).clicked() {
                    command = Some(RemoteCommand::PreviousTrack);
                }
                if icon_button(ui, play_glyph, play_label, 30.0, has_remote).clicked() {
                    command = Some(RemoteCommand::Pause);
                }
                if icon_button(ui, ICON_NEXT, "Next", 22.0, has_remote).clicked() {
                    command = Some(RemoteCommand::NextTrack);
                }
                if let (Some(command), Some(remote)) = (command, &remote) {
                    self.send(remote, command);
                }
            });

            ui.horizontal(|ui| {
                ui.label(icon(ICON_VOLUME, 16.0)).on_hover_text("One volume: the phone, this slider and the output picked below move together.");
                ui.spacing_mut().slider_width = ui.available_width();
                let mut level = core.player.level();
                let response = ui.add(egui::Slider::new(&mut level, 0.0..=1.0).show_value(false));
                response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Slider, true, "Volume"));
                if response.changed() {
                    core.player.set_level(level);
                    let _ = core.commands.send(Command::SetLevel(level));
                    core.phone_volume.follow_output();
                }
            });

            ui.horizontal(|ui| {
                ui.label(icon(ICON_SPEAKER, 16.0)).on_hover_text("Sound output");
                self.draw_output_picker(ui);
            });

            let problem = core.player.output.lock().unwrap().problem.clone();
            let status = problem.or_else(|| (!now.message.is_empty()).then(|| now.message.clone())).unwrap_or_else(|| {
                if !known {
                    format!("Ready as \u{201C}{speaker_name}\u{201D}")
                } else if receiving {
                    format!("Playing from {}", now.client)
                } else {
                    format!("Paused on {}", now.client)
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let gear = icon_button(ui, ICON_SETTINGS, "Settings", 14.0, true);
                let popup = egui::Popup::from_toggle_button_response(&gear)
                    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                    .align(egui::RectAlign::TOP_END)
                    .width(270.0)
                    .show(|ui| self.draw_settings(ui));
                // Clicking away closes the popup without the name box ever reporting it lost focus.
                if popup.is_none() && self.rename_to.is_none() {
                    self.apply_name();
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add(egui::Label::new(RichText::new(status).small().weak()).truncate());
                });
            });
        });

        // Restarting the speaker can touch `now`, so it has to be let go of first.
        drop(now);
        if let Some(name) = self.rename_to.take() {
            core.rename(name);
            self.name_draft = core.settings.lock().unwrap().name.clone();
        }
    }
}

enum Request {
    Open,
    Quit,
}

/// Asks the main thread for something, waking its message loop to notice.
#[derive(Clone)]
struct Requests {
    tx: Sender<Request>,
    main_thread: u32,
    screen: Screen,
}

impl Requests {
    fn send(&self, request: Request) {
        let _ = self.tx.send(request);
        // SAFETY: posting an empty message to a thread that runs a message loop.
        let _ = unsafe { PostThreadMessageW(self.main_thread, WM_NULL, WPARAM(0), LPARAM(0)) };
    }

    fn open(&self) {
        if !self.screen.focus() {
            self.send(Request::Open);
        }
    }

    fn quit(&self) {
        self.send(Request::Quit);
        self.screen.close();
    }
}

fn build_tray(requests: &Requests) -> Result<TrayIcon, String> {
    let open = MenuItem::new("Open", true, None);
    let quit = MenuItem::new("Quit", true, None);
    let menu = Menu::new();
    menu.append_items(&[&open, &quit]).map_err(|error| error.to_string())?;

    let (open_id, quit_id) = (open.id().clone(), quit.id().clone());
    let on_menu = requests.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if event.id == open_id {
            on_menu.open();
        } else if event.id == quit_id {
            on_menu.quit();
        }
    }));
    let on_click = requests.clone();
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
            on_click.open();
        }
    }));

    // The icon built into the exe, which has a proper small size for the tray.
    let icon = Icon::from_resource(1, None).map_err(|error| error.to_string())?;
    TrayIconBuilder::new()
        .with_tooltip("AirPlay Receiver")
        .with_icon(icon)
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .build()
        .map_err(|error| error.to_string())
}

/// Runs one window until it's closed, then hands everything it used back to Windows.
fn open_window(core: &Arc<Core>, requests: &Requests) {
    core.player.set_window_open(true);
    let _ = core.commands.send(Command::RefreshDevices);
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")).expect("bad icon");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("AirPlay Receiver")
            .with_inner_size([380.0, 680.0])
            .with_min_inner_size([320.0, 470.0])
            .with_transparent(true)
            .with_icon(icon),
        ..Default::default()
    };
    let (window_core, window_requests) = (core.clone(), requests.clone());
    let result = eframe::run_native(
        "AirPlay Receiver",
        options,
        Box::new(move |cc| Ok(Box::new(Window::new(cc, window_core, window_requests)))),
    );
    if let Err(error) = result {
        tracing::error!(%error, "window failed");
    }
    core.screen.set(None);
    core.player.set_window_open(false);
    // SAFETY: the current process's own pseudo-handle, which is always valid.
    let _ = unsafe { K32EmptyWorkingSet(GetCurrentProcess()) };
}

fn start_logging() {
    let _ = std::fs::create_dir_all(Settings::dir());
    let Ok(file) = std::fs::File::create(Settings::dir().join("log.txt")) else { return };
    let level = match std::env::var("AIRPLAY_LOG").as_deref() {
        Ok("debug") => tracing::Level::DEBUG,
        _ => tracing::Level::INFO,
    };
    tracing_subscriber::fmt().with_ansi(false).with_max_level(level).with_writer(Mutex::new(file)).init();
    std::panic::set_hook(Box::new(|panic| tracing::error!("{panic}")));
}

fn main() {
    // A second launch just asks the running copy to show its window.
    // SAFETY: a named event with default security, and a static name.
    let Ok(open_signal) = (unsafe { CreateEventW(None, false, false, w!("Local\\AirPlayReceiver.Open")) }) else {
        return;
    };
    // SAFETY: reads the error from the call just above.
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        // SAFETY: the handle was just created.
        let _ = unsafe { SetEvent(open_signal) };
        return;
    }

    start_logging();
    let core = Core::start();
    let (tx, rx) = std::sync::mpsc::channel();
    // SAFETY: no arguments and no failure case.
    let main_thread = unsafe { GetCurrentThreadId() };
    let requests = Requests { tx, main_thread, screen: core.screen.clone() };

    let signal_handle = open_signal.0 as usize;
    let on_signal = requests.clone();
    std::thread::spawn(move || {
        loop {
            // SAFETY: the event stays open for the life of the process.
            let _ = unsafe { WaitForSingleObject(HANDLE(signal_handle as *mut _), INFINITE) };
            on_signal.open();
        }
    });

    let tray = build_tray(&requests);
    if let Err(error) = &tray {
        tracing::error!(error, "no tray icon");
    }
    if !std::env::args().any(|arg| arg == BACKGROUND_FLAG) {
        open_window(&core, &requests);
    }

    let mut message = MSG::default();
    loop {
        while let Ok(request) = rx.try_recv() {
            match request {
                Request::Open => open_window(&core, &requests),
                Request::Quit => {
                    core.shutdown();
                    return;
                }
            }
        }
        // With no tray icon there'd be no way back to a closed window.
        if tray.is_err() {
            core.shutdown();
            return;
        }
        // This thread owns the tray icon, so it has to keep answering Windows.
        // SAFETY: `message` outlives each call.
        unsafe {
            if GetMessageW(&mut message, None, 0, 0).0 > 0 {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_survive_a_save_and_load() {
        let parsed = Settings::parse("name=Den\ndevice={0.0.0.00000000}.{abc=def}\ndelay_ms=1500\n");
        assert_eq!(parsed.name, "Den");
        assert_eq!(parsed.device.as_deref(), Some("{0.0.0.00000000}.{abc=def}"), "ids may contain =");
        assert_eq!(parsed.delay_ms, 1500);
    }

    #[test]
    fn bad_settings_fall_back_to_defaults() {
        let parsed = Settings::parse("delay_ms=9999999\ndevice=\nvolume=old\nname=   \n");
        assert_eq!(parsed.name, "PC Speaker");
        assert_eq!(parsed.device, None);
        assert_eq!(parsed.delay_ms, 5000);
    }

    #[test]
    fn names_are_made_safe_for_the_network() {
        assert_eq!(clean_name("  Living Room  "), "Living Room");
        assert_eq!(clean_name("a\nb\tc"), "abc");
        assert_eq!(clean_name("   "), "");
        let long = clean_name(&"é".repeat(60));
        assert!(long.len() <= 50, "{} bytes", long.len());
        assert_eq!(long, "é".repeat(25), "cuts between characters");
    }

    #[test]
    fn times_read_like_a_music_player() {
        assert_eq!(format_time(0.0), "0:00");
        assert_eq!(format_time(83.9), "1:23");
        assert_eq!(format_time(-2.0), "0:00");
    }

    #[test]
    fn hardware_address_is_a_local_unicast_mac() {
        let mac = hardware_address("Den");
        assert_eq!(mac[0] & 0x03, 0x02);
        assert_eq!(mac, hardware_address("Den"), "stable, so the phone knows it next time");
    }

    /// An iPhone remembers speakers by address, so without this a renamed speaker kept
    /// showing its old name.
    #[test]
    fn a_new_name_gets_a_new_address() {
        assert_ne!(hardware_address("Den"), hardware_address("Kitchen"));
    }

    /// Real registry: the switch must add and remove the entry Windows reads at login, and
    /// leave things as they were. Run with `--ignored`.
    #[test]
    #[ignore]
    fn start_with_windows_switch_works() {
        let before = starts_with_windows();
        set_starts_with_windows(true).unwrap();
        assert!(starts_with_windows());
        let key = windows_registry::CURRENT_USER.open(RUN_KEY).unwrap();
        let command = key.get_string(RUN_VALUE).unwrap();
        assert!(command.starts_with('"') && command.ends_with(BACKGROUND_FLAG), "{command}");
        set_starts_with_windows(false).unwrap();
        assert!(!starts_with_windows());
        if before {
            set_starts_with_windows(true).unwrap();
        }
    }

    /// A real palette, as Windows stores it.
    #[test]
    fn accent_uses_the_shades_windows_uses() {
        let palette = [
            0xCB, 0x83, 0x0A, 0xFF, 0xA9, 0x6D, 0x09, 0xFF, 0x91, 0x5E, 0x07, 0xFF, 0x78, 0x4E, 0x06, 0xFF,
            0x60, 0x3E, 0x05, 0xFF, 0x48, 0x2F, 0x03, 0xFF, 0x26, 0x19, 0x02, 0xFF, 0x88, 0x17, 0x98, 0x00,
        ];
        let theme = WindowsTheme::from_palette(false, &palette);
        assert_eq!(theme.accent_on_dark, Color32::from_rgb(0xA9, 0x6D, 0x09));
        assert_eq!(theme.accent_on_light, Color32::from_rgb(0x60, 0x3E, 0x05));
    }

    #[test]
    fn a_missing_palette_falls_back_to_windows_blue() {
        let theme = WindowsTheme::from_palette(true, &[]);
        assert!(theme.light);
        assert_eq!(theme.accent_on_dark, Color32::from_rgb(0x00, 0x78, 0xD4));
    }

    /// Read off a real iPhone: 2 paused, 1 the moment play was pressed, 2 again on pause.
    #[test]
    fn play_status_reads_as_the_phone_means_it() {
        assert_eq!(phone_is_playing(1), Some(true));
        assert_eq!(phone_is_playing(2), Some(false));
        assert_eq!(phone_is_playing(3), Some(false));
        assert_eq!(phone_is_playing(4), Some(true));
        assert_eq!(phone_is_playing(0), None);
    }

    #[test]
    fn airplay_volume_maps_to_the_slider() {
        assert_eq!(level_from_db(0.0), 1.0);
        assert_eq!(level_from_db(-15.0), 0.5);
        assert_eq!(level_from_db(-30.0), 0.0);
        assert_eq!(level_from_db(-144.0), 0.0, "mute");
    }

    /// A stand-in iPhone: each press moves its slider one step and reports back.
    struct FakePhone {
        now: Arc<Mutex<NowPlaying>>,
        presses: Mutex<Vec<String>>,
        ignores_presses: bool,
    }

    impl RemoteControl for FakePhone {
        fn send_command(&self, command: RemoteCommand) -> Result<(), ShairplayError> {
            let RemoteCommand::Raw(press) = command else { panic!("expected a raw command") };
            if !self.ignores_presses {
                let step = if press == "volumeup" { PHONE_STEP } else { -PHONE_STEP };
                let mut now = self.now.lock().unwrap();
                now.phone_level = now.phone_level.map(|level| level + step);
            }
            self.presses.lock().unwrap().push(press);
            Ok(())
        }

        fn available_commands(&self) -> Vec<RemoteCommand> {
            Vec::new()
        }
    }

    fn fake_phone(phone_at: f32, output_at: f32, ignores_presses: bool) -> (Arc<FakePhone>, PhoneVolume) {
        let now = Arc::new(Mutex::new(NowPlaying::default()));
        let phone = Arc::new(FakePhone { now: now.clone(), presses: Mutex::default(), ignores_presses });
        {
            let mut now = now.lock().unwrap();
            now.connections = 1;
            now.remote = Some(phone.clone());
            now.phone_level = Some(phone_at);
        }
        let player = Player::new(2000);
        player.set_level(output_at);
        (phone, PhoneVolume { now, player, busy: Arc::default() })
    }

    fn step_fake_phone(phone_at: f32, output_at: f32, ignores_presses: bool) -> (Vec<String>, f32) {
        let (phone, volume) = fake_phone(phone_at, output_at, ignores_presses);
        volume.step_phone();
        let level = phone.now.lock().unwrap().phone_level.unwrap();
        let presses = phone.presses.lock().unwrap().clone();
        (presses, level)
    }

    #[test]
    fn phone_steps_up_to_the_output() {
        let (presses, level) = step_fake_phone(0.5, 0.75, false);
        assert_eq!(presses, vec!["volumeup"; 4]);
        assert!((level - 0.75).abs() < PHONE_STEP / 2.0);
    }

    #[test]
    fn a_hung_up_phone_gets_no_presses() {
        let (phone, volume) = fake_phone(0.5, 0.75, false);
        phone.now.lock().unwrap().connections = 0;
        volume.step_phone();
        assert!(phone.presses.lock().unwrap().is_empty());
    }

    #[test]
    fn phone_steps_down_to_the_output_and_stops_when_close() {
        assert_eq!(step_fake_phone(0.75, 0.5, false).0, vec!["volumedown"; 4]);
        assert!(step_fake_phone(0.5, 0.52, false).0.is_empty(), "already within a step");
    }

    #[test]
    fn a_phone_ignoring_the_presses_doesnt_hang_us() {
        let (presses, level) = step_fake_phone(0.5, 0.75, true);
        assert_eq!(presses.len(), GIVE_UP_AFTER as usize);
        assert_eq!(level, 0.5);
    }

    /// What the real iPhone did: it applied a press over a second late, and the app, not
    /// waiting, pressed 12 times for a 7-step move and overshot. One press per report now.
    #[test]
    fn a_slow_phone_gets_no_extra_presses() {
        struct SlowPhone {
            now: Arc<Mutex<NowPlaying>>,
            presses: Arc<Mutex<u32>>,
        }
        impl RemoteControl for SlowPhone {
            fn send_command(&self, _: RemoteCommand) -> Result<(), ShairplayError> {
                *self.presses.lock().unwrap() += 1;
                let now = self.now.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(400));
                    let mut now = now.lock().unwrap();
                    now.phone_level = now.phone_level.map(|level| level + PHONE_STEP);
                });
                Ok(())
            }
            fn available_commands(&self) -> Vec<RemoteCommand> {
                Vec::new()
            }
        }
        let now = Arc::new(Mutex::new(NowPlaying::default()));
        let presses = Arc::new(Mutex::new(0));
        {
            let mut guard = now.lock().unwrap();
            guard.connections = 1;
            guard.remote = Some(Arc::new(SlowPhone { now: now.clone(), presses: presses.clone() }));
            guard.phone_level = Some(0.5);
        }
        let player = Player::new(2000);
        player.set_level(0.75);
        PhoneVolume { now, player, busy: Arc::default() }.step_phone();
        assert_eq!(*presses.lock().unwrap(), 4, "one press per step, however late the reports");
    }

    #[test]
    fn track_position_comes_from_the_progress_report() {
        // Real numbers from the iPhone's first session: 4.88 s into a 161.9 s song.
        let (position, length) = read_progress(2076548643, 2076763951, 2083689314);
        assert!((position.unwrap() - 4.88).abs() < 0.01);
        assert!((length - 161.9).abs() < 0.1);
    }

    /// Real numbers from the iPhone after a long pause: "596 s" into a 162 s song.
    #[test]
    fn a_position_past_the_end_of_the_track_is_ignored() {
        let (position, length) = read_progress(2863094028, 2889399894, 2870234699);
        assert_eq!(position, None);
        assert!((length - 161.9).abs() < 0.1);
    }

    #[test]
    fn progress_survives_the_rtp_clock_wrapping() {
        let (position, length) = read_progress(u32::MAX - 44100, 44100, 441000);
        assert!((position.unwrap() - 2.0).abs() < 0.01);
        assert!((length - 11.0).abs() < 0.01);
    }

    /// Dragging must move the phone live, including a slider that keeps moving during a
    /// run and one that stops the moment a run ends.
    #[test]
    fn phone_follows_a_slider_still_moving() {
        let (phone, volume) = fake_phone(0.0, 0.25, false);
        volume.follow_output();
        for level in [0.5, 0.75, 1.0] {
            std::thread::sleep(Duration::from_millis(30));
            volume.player.set_level(level);
            volume.follow_output();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while volume.busy.load(SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let level = phone.now.lock().unwrap().phone_level.unwrap();
        assert!((level - 1.0).abs() < PHONE_STEP / 2.0, "phone stopped at {level}");
    }
}
