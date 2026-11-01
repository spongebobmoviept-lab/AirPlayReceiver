//! Holds decoded audio for about 2 s (the delay an iPhone expects of an AirPlay 1
//! speaker) and plays it on the chosen sound device. Everything stays 44.1 kHz stereo:
//! Windows converts it to whatever the device runs at.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use shairplay::AudioSession;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{IAudioSessionManager, IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator, eConsole, eRender};
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx};
use windows::core::HSTRING;

const RATE: u32 = 44100;
/// Wi-Fi delivers in bursts, so buffer depth is averaged this long before it means anything.
const SMOOTHING_SECS: f64 = 3.0;
/// The depth averaged over the first seconds of playback is what correction holds to.
/// A start-up burst just becomes slightly more delay instead of minutes of correcting.
const SETTLE_SECS: f64 = 10.0;
/// Depth this close to the target is left alone.
const DEADBAND_SECS: f64 = 0.02;
/// How long correction takes to pull the buffer back once outside the deadband.
const CATCH_UP_SECS: f64 = 30.0;
/// The most correction may do: 500 ppm is about 22 frames a second, well past what
/// real clocks need (100 ppm is a bad one).
const MAX_CORRECTION: f64 = 0.0005;
/// How long the sound device stays open after the audio stops. A resume within this
/// is instant. After it the device is let go: an open stream can keep Windows awake.
const DEVICE_LINGER_MS: u64 = 30_000;
/// A paused iPhone keeps its connection up by sending silence, which shouldn't hold the
/// sound device open. Below this (about -80 dB) is silence.
const SILENCE: f32 = 0.0001;
/// For a sender that doesn't report its play status: sound this recent counts as playing,
/// so a quiet gap inside a song isn't a pause.
const SOUND_HOLD_MS: u64 = 3000;

const PHONE_UNKNOWN: u8 = 0;
const PHONE_PLAYING: u8 = 1;
const PHONE_PAUSED: u8 = 2;

/// The buffer between the network (bursty, phone's clock) and the sound card (steady, its own clock).
#[derive(Default)]
struct Ring {
    frames: VecDeque<[f32; 2]>,
    playing: bool,
    played: u64,
    smoothed: f64,
    target: f64,
    drift: f64,
    last: [f32; 2],
    peak: f32,
    skipped: u64,
    repeated: u64,
    underruns: u64,
}

fn mix(a: [f32; 2], b: [f32; 2]) -> [f32; 2] {
    [(a[0] + b[0]) / 2.0, (a[1] + b[1]) / 2.0]
}

impl Ring {
    fn push(&mut self, samples: &[f32], channels: usize, delay: usize) {
        let channels = channels.max(1);
        for frame in samples.chunks_exact(channels) {
            self.frames.push_back([frame[0], frame[channels.min(2) - 1]]);
            self.peak = self.peak.max(frame[0].abs());
        }
        // Way over target means the device stalled: jump back rather than lag forever.
        if self.frames.len() > delay * 2 {
            let extra = self.frames.len() - delay;
            self.frames.drain(..extra);
        }
    }

    /// The next frame to play, or None while the buffer fills.
    fn next(&mut self, delay: usize) -> Option<[f32; 2]> {
        if !self.playing {
            if self.frames.len() < delay {
                return None;
            }
            self.playing = true;
            self.played = 0;
            self.drift = 0.0;
            self.smoothed = self.frames.len() as f64;
        }
        // The phone's clock and the sound card's never quite agree. Merge or add a
        // frame now and then so the buffer holds its size.
        // ponytail: frame stuffing, swap in a sinc resampler if anyone can hear it.
        let rate = f64::from(RATE);
        self.played += 1;
        self.smoothed += (self.frames.len() as f64 - self.smoothed) / (rate * SMOOTHING_SECS);
        if (self.played as f64) < rate * SETTLE_SECS {
            self.target = self.smoothed;
        } else {
            let error = self.smoothed - self.target;
            let outside = error.abs() - rate * DEADBAND_SECS;
            if outside > 0.0 {
                let correction = (outside / (rate * CATCH_UP_SECS)).min(MAX_CORRECTION);
                self.drift += correction.copysign(error);
            }
        }

        if self.drift <= -1.0 {
            self.drift += 1.0;
            self.repeated += 1;
            if let Some(&next) = self.frames.front() {
                return Some(mix(self.last, next));
            }
        }
        let Some(mut frame) = self.frames.pop_front() else {
            self.playing = false;
            self.underruns += 1;
            return None;
        };
        if self.drift >= 1.0 {
            self.drift -= 1.0;
            self.skipped += 1;
            if let Some(next) = self.frames.pop_front() {
                frame = mix(frame, next);
            }
        }
        self.last = frame;
        Some(frame)
    }

    fn flush(&mut self) {
        self.frames.clear();
        self.playing = false;
    }
}

#[derive(Clone)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
}

/// What the output thread tells the window.
#[derive(Default)]
pub struct OutputStatus {
    pub devices: Vec<DeviceInfo>,
    pub problem: Option<String>,
}

pub struct Player {
    ring: Mutex<Ring>,
    delay: usize,
    /// The one volume, 0 to 1, shared by the phone, the app and the picked output.
    level: AtomicU32,
    /// Volume applied to the sound itself, for outputs whose drivers don't apply theirs.
    gain: AtomicU32,
    session: AtomicU64,
    started: Instant,
    last_sound_ms: AtomicU64,
    paused: AtomicBool,
    phone_state: AtomicU8,
    frames_received: AtomicU64,
    window_open: AtomicBool,
    stream_failed: AtomicBool,
    pub output: Mutex<OutputStatus>,
}

impl Player {
    pub fn new(delay_ms: u32) -> Arc<Self> {
        Arc::new(Self {
            ring: Mutex::default(),
            delay: (u64::from(delay_ms) * u64::from(RATE) / 1000) as usize,
            level: AtomicU32::new(0),
            gain: AtomicU32::new(1f32.to_bits()),
            session: AtomicU64::new(0),
            started: Instant::now(),
            last_sound_ms: AtomicU64::new(0),
            paused: AtomicBool::new(false),
            phone_state: AtomicU8::new(PHONE_UNKNOWN),
            frames_received: AtomicU64::new(0),
            window_open: AtomicBool::new(false),
            stream_failed: AtomicBool::new(false),
            output: Mutex::default(),
        })
    }

    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Relaxed))
    }

    pub fn set_level(&self, level: f32) {
        self.level.store(level.to_bits(), Relaxed);
    }

    /// Whether the sender is playing. An iPhone says so itself in its track updates. Only a
    /// sender that hasn't said gets judged by whether sound is arriving.
    pub fn receiving(&self) -> bool {
        match self.phone_state.load(Relaxed) {
            PHONE_PLAYING => true,
            PHONE_PAUSED => false,
            _ => !self.paused.load(Relaxed) && self.sound_within(SOUND_HOLD_MS),
        }
    }

    pub fn set_phone_playing(&self, playing: Option<bool>) {
        let state = match playing {
            Some(true) => PHONE_PLAYING,
            Some(false) => PHONE_PAUSED,
            None => PHONE_UNKNOWN,
        };
        self.phone_state.store(state, Relaxed);
    }

    /// Audio received so far, which counts track position with no clock and stops on pause.
    pub fn frames_received(&self) -> u64 {
        self.frames_received.load(Relaxed)
    }

    pub fn set_window_open(&self, open: bool) {
        self.window_open.store(open, Relaxed);
    }

    fn sound_within(&self, ms: u64) -> bool {
        let last = self.last_sound_ms.load(Relaxed);
        last != 0 && self.now_ms().saturating_sub(last) < ms
    }

    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64 + 1
    }

    pub fn stop(&self) {
        self.ring.lock().unwrap().flush();
        self.paused.store(true, Relaxed);
    }

    /// Latest phone in wins; an older session goes quiet.
    pub fn new_session(self: &Arc<Self>, channels: u8, commands: Sender<Command>) -> Session {
        self.stop();
        self.set_phone_playing(None);
        Session {
            player: self.clone(),
            id: self.session.fetch_add(1, Relaxed) + 1,
            channels: channels.into(),
            commands,
        }
    }

    fn log_stats(&self) {
        let mut ring = self.ring.lock().unwrap();
        let peak = std::mem::take(&mut ring.peak);
        if ring.playing {
            tracing::info!(
                buffered_ms = ring.frames.len() as u64 * 1000 / u64::from(RATE),
                played_secs = ring.played / u64::from(RATE),
                peak,
                skipped = ring.skipped,
                repeated = ring.repeated,
                underruns = ring.underruns,
                "audio buffer"
            );
        }
    }
}

pub struct Session {
    player: Arc<Player>,
    id: u64,
    channels: usize,
    commands: Sender<Command>,
}

impl Session {
    fn current(&self) -> bool {
        self.player.session.load(Relaxed) == self.id
    }
}

impl AudioSession for Session {
    fn audio_process(&mut self, samples: &[f32]) {
        if !self.current() {
            return;
        }
        let player = &self.player;
        if samples.iter().any(|sample| sample.abs() > SILENCE) {
            let now = player.now_ms();
            let before = player.last_sound_ms.swap(now, Relaxed);
            let was_paused = player.paused.swap(false, Relaxed);
            // The first sound in a while: the output thread may be asleep with the device closed.
            if before == 0 || now - before > DEVICE_LINGER_MS / 2 {
                let _ = self.commands.send(Command::Wake);
            }
            // Playing again, so now the phone will take volume steps. Bring it to the output.
            if was_paused || before == 0 || now - before > SOUND_HOLD_MS {
                let _ = self.commands.send(Command::FollowLevel);
            }
        } else if !player.receiving() {
            return; // a paused phone's keep-alive, not music
        }
        player.ring.lock().unwrap().push(samples, self.channels, player.delay);
        player.frames_received.fetch_add((samples.len() / self.channels.max(1)) as u64, Relaxed);
    }

    fn audio_flush(&mut self) {
        if self.current() {
            self.player.stop();
        }
    }
}

pub enum Command {
    Select(Option<String>),
    RefreshDevices,
    SetLevel(f32),
    /// Sound just started: read the output's level fresh, then bring the phone to it.
    FollowLevel,
    Wake,
}

/// What the one volume moves. A device picked in the app: that device's own level.
/// Windows default: only this app's own slider in the Windows mixer, so the main
/// Windows volume is never touched. Windows remembers both across restarts.
enum VolumeControl {
    Device(IAudioEndpointVolume),
    App(ISimpleAudioVolume),
}

// SAFETY for all of these: COM calls on the output thread, which made the objects,
// with arguments that outlive the call. A null event context is allowed.
impl VolumeControl {
    fn open(picked: Option<&str>) -> Option<Self> {
        unsafe {
            let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
            match picked {
                Some(id) => {
                    let device = devices.GetDevice(&HSTRING::from(id.strip_prefix("wasapi:").unwrap_or(id))).ok()?;
                    device.Activate(CLSCTX_ALL, None).ok().map(Self::Device)
                }
                None => {
                    let device = devices.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
                    let sessions: IAudioSessionManager = device.Activate(CLSCTX_ALL, None).ok()?;
                    // No session id and no cross-process flag: this program's own default session.
                    sessions.GetSimpleAudioVolume(None, 0).ok().map(Self::App)
                }
            }
        }
    }

    fn get(&self) -> Option<f32> {
        unsafe {
            match self {
                Self::Device(volume) => volume.GetMasterVolumeLevelScalar().ok(),
                Self::App(volume) => volume.GetMasterVolume().ok(),
            }
        }
    }

    fn set(&self, level: f32) {
        let result = unsafe {
            match self {
                Self::Device(volume) => volume.SetMasterVolumeLevelScalar(level, std::ptr::null()),
                Self::App(volume) => volume.SetMasterVolume(level, std::ptr::null()),
            }
        };
        if let Err(error) = result {
            tracing::warn!(%error, "couldn't set the volume");
        }
    }
}

fn list_devices(host: &cpal::Host) -> Vec<DeviceInfo> {
    let Ok(devices) = host.output_devices() else {
        return Vec::new();
    };
    let mut list: Vec<_> = devices
        .filter_map(|device| {
            Some(DeviceInfo {
                id: device.id().ok()?.to_string(),
                name: device.description().ok()?.name().to_string(),
            })
        })
        .collect();
    list.sort_by(|a, b| a.name.cmp(&b.name));
    list
}

fn find_device(host: &cpal::Host, id: &str) -> Option<cpal::Device> {
    host.output_devices()
        .ok()?
        .find(|device| device.id().is_ok_and(|found| found.to_string() == id))
}

fn open_stream(device: &cpal::Device, player: &Arc<Player>) -> Result<cpal::Stream, cpal::Error> {
    let config = cpal::StreamConfig {
        channels: 2,
        sample_rate: RATE,
        buffer_size: cpal::BufferSize::Default,
    };
    let audio = player.clone();
    let failed = player.clone();
    let (mut last, mut envelope, mut gain) = ([0f32; 2], 0f32, f32::from_bits(player.gain.load(Relaxed)));
    let stream = device.build_output_stream(
        config,
        move |out: &mut [f32], _| {
            let target = f32::from_bits(audio.gain.load(Relaxed));
            let mut ring = audio.ring.lock().unwrap();
            for frame in out.chunks_exact_mut(2) {
                let next = ring.next(audio.delay);
                last = next.unwrap_or(last);
                // A few milliseconds of fade, so stopping mid-waveform doesn't click.
                envelope += (if next.is_some() { 1.0 } else { 0.0 } - envelope) * 0.01;
                gain += (target - gain) * 0.001; // eases over a few ms, so moves don't click
                frame[0] = last[0] * envelope * gain;
                frame[1] = last[1] * envelope * gain;
            }
        },
        move |error| {
            tracing::warn!(%error, "sound device error");
            use cpal::ErrorKind::{DeviceNotAvailable, StreamInvalidated};
            if matches!(error.kind(), DeviceNotAvailable | StreamInvalidated) {
                failed.stream_failed.store(true, Relaxed);
            }
        },
        None,
    )?;
    stream.play()?;
    Ok(stream)
}

/// Measured 2026-11-01: VB-Audio's virtual devices claim a hardware volume control but only
/// store the number. Their sound came out identical at Windows levels of 100% and 25%, so
/// the app has to apply the level itself.
/// ponytail: matched by name, measure and add any other driver found doing the same.
fn driver_ignores_volume(host: &cpal::Host, id: Option<&str>) -> bool {
    id.and_then(|id| find_device(host, id))
        .and_then(|device| device.description().ok())
        .is_some_and(|description| description.name().contains("VB-Audio"))
}

/// A slider position as loudness, on the same scale AirPlay uses: the whole slider is
/// -30 dB to 0 dB, and all the way down is silent.
fn software_gain(level: f32) -> f32 {
    if level <= 0.0 { 0.0 } else { 10f32.powf((level.min(1.0) - 1.0) * 30.0 / 20.0) }
}

/// AUDCLNT_E_DEVICE_IN_USE (0x8889000A): another app holds the device in exclusive mode.
const DEVICE_IN_USE: &str = "-2004287478";

fn describe(error: &cpal::Error) -> String {
    let text = error.to_string();
    if text.contains(DEVICE_IN_USE) {
        "Another app has that sound device locked".to_string()
    } else {
        format!("Can't open that sound device: {text}")
    }
}

/// The picked device if it will open; otherwise keep the music going on the default.
/// Returns the stream, whether it fell back, and what to tell the user.
fn open_output(host: &cpal::Host, wanted: Option<&str>, player: &Arc<Player>) -> (Option<cpal::Stream>, bool, Option<String>) {
    let (picked, mut problem) = match wanted {
        None => (None, None),
        Some(id) => match find_device(host, id) {
            None => (None, Some("That sound device is gone, using the Windows default".to_string())),
            Some(device) => match open_stream(&device, player) {
                Ok(stream) => (Some(stream), None),
                Err(error) => (None, Some(format!("{}, using the Windows default", describe(&error)))),
            },
        },
    };
    let on_fallback = wanted.is_some() && picked.is_none();
    let stream = picked.or_else(|| {
        let opened = host
            .default_output_device()
            .ok_or("No sound device found".to_string())
            .and_then(|device| open_stream(&device, player).map_err(|error| describe(&error)));
        opened.map_err(|error| problem = Some(error)).ok()
    });
    if let Some(problem) = &problem {
        tracing::warn!(problem);
    }
    (stream, on_fallback, problem)
}

/// Owns the sound stream on its own thread, since opening one can take a moment and
/// the sound system wants its own threading setup. `None` means the Windows default,
/// which also follows the per-app choice in the Windows volume mixer. The device is only
/// open around actual audio, and with nothing playing and no window this thread sleeps.
/// `level_moved` fires when the output's volume changes by anything but
/// `Command::SetLevel`: its slider in Windows, or a switch to an output set differently.
pub fn run_output(
    player: Arc<Player>,
    mut wanted: Option<String>,
    commands: Receiver<Command>,
    repaint: impl Fn() + Send + 'static,
    level_moved: impl Fn() + Send + 'static,
) {
    let host = cpal::default_host();
    // SAFETY: first COM use on this thread; cpal uses the same mode, so a repeat is harmless.
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    let mut stream: Option<cpal::Stream> = None;
    let mut on_fallback = false;
    let mut last_open_try: Option<Instant> = None;
    let mut volume_of = wanted.clone();
    let mut volume = VolumeControl::open(volume_of.as_deref());
    let mut apply_level_here = driver_ignores_volume(&host, volume_of.as_deref());
    let mut known_level = -1f32;
    let mut ticks = 0u32;

    let set_problem = |problem: Option<String>| {
        player.output.lock().unwrap().problem = problem;
        repaint();
    };

    loop {
        let active = player.sound_within(DEVICE_LINGER_MS);
        if !active && stream.is_some() {
            tracing::info!("nothing playing, letting go of the sound device");
            stream = None;
            on_fallback = false;
            set_problem(None);
        }
        if active && stream.is_none() && last_open_try.is_none_or(|at| at.elapsed() > Duration::from_secs(2)) {
            last_open_try = Some(Instant::now());
            let (opened, fell_back, problem) = open_output(&host, wanted.as_deref(), &player);
            stream = opened;
            on_fallback = fell_back;
            set_problem(problem);
        }
        let should_control = if on_fallback { None } else { wanted.clone() };
        if should_control != volume_of {
            volume_of = should_control;
            volume = VolumeControl::open(volume_of.as_deref());
            apply_level_here = driver_ignores_volume(&host, volume_of.as_deref());
        }
        let gain = if apply_level_here && known_level >= 0.0 { software_gain(known_level) } else { 1.0 };
        player.gain.store(gain.to_bits(), Relaxed);

        let watching = active || player.window_open.load(Relaxed);
        let wait = if watching { Duration::from_millis(200) } else { Duration::from_secs(3600) };
        match commands.recv_timeout(wait) {
            Ok(Command::Select(id)) => {
                wanted = id;
                stream = None;
                on_fallback = false;
                last_open_try = None;
                set_problem(None);
            }
            Ok(Command::RefreshDevices) => {
                player.output.lock().unwrap().devices = list_devices(&host);
                repaint();
            }
            Ok(Command::SetLevel(level)) => {
                if let Some(volume) = &volume {
                    volume.set(level);
                }
                known_level = volume.as_ref().and_then(VolumeControl::get).unwrap_or(level);
            }
            Ok(Command::FollowLevel) => {
                // This thread may have slept through changes, so the level it has can be stale.
                if let Some(level) = volume.as_ref().and_then(VolumeControl::get) {
                    known_level = level;
                    player.set_level(level);
                    repaint();
                    level_moved();
                }
            }
            Ok(Command::Wake) => last_open_try = None,
            Err(RecvTimeoutError::Timeout) if watching => {
                ticks += 1;
                // ponytail: polled, since a change callback is a lot of COM for one number.
                if volume_of.is_none() && ticks.is_multiple_of(5) {
                    volume = VolumeControl::open(None); // the Windows default may have moved
                }
                if let Some(level) = volume.as_ref().and_then(VolumeControl::get)
                    && (level - known_level).abs() > 0.01
                {
                    known_level = level;
                    player.set_level(level);
                    repaint();
                    level_moved();
                }
                if ticks.is_multiple_of(300) {
                    player.log_stats();
                }
                let picked_is_back = on_fallback
                    && ticks.is_multiple_of(10)
                    && wanted.as_deref().and_then(|id| find_device(&host, id)).is_some_and(|device| open_stream(&device, &player).is_ok());
                if picked_is_back || player.stream_failed.swap(false, Relaxed) {
                    stream = None;
                    on_fallback = false;
                    last_open_try = None;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real hardware: every device on this PC must accept our 44.1 kHz stereo, whatever
    /// it natively runs at. Silent, since nothing is buffered. Run with `--ignored --nocapture`.
    #[test]
    #[ignore]
    fn every_sound_device_opens() {
        let host = cpal::default_host();
        let player = Player::new(2000);
        let mut failed = Vec::new();
        for device in host.output_devices().unwrap() {
            let name = device.description().unwrap().name().to_string();
            let native = device.default_output_config().unwrap();
            let opened = open_stream(&device, &player);
            println!(
                "{} | {} Hz {} ch | {} | {}",
                if opened.is_ok() { "ok  " } else { "FAIL" },
                native.sample_rate(),
                native.channels(),
                name,
                device.id().unwrap()
            );
            // Locked by another app isn't our failure, and the app handles it by falling back.
            if let Err(error) = opened.as_ref().map(|_| ()).map_err(ToString::to_string)
                && !error.contains(DEVICE_IN_USE)
            {
                failed.push(format!("{name}: {error}"));
            }
        }
        assert!(failed.is_empty(), "{failed:?}");
    }

    /// Records `capture` for a moment and returns its average level in dB.
    fn recorded_level(capture: &str) -> Option<f32> {
        let output = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-f", "dshow", "-t", "1.5", "-i", &format!("audio={capture}"), "-af", "volumedetect", "-f", "null", "-"])
            .output()
            .ok()?;
        let log = String::from_utf8_lossy(&output.stderr).to_string();
        let value = log.split("mean_volume:").nth(1)?.split_whitespace().next()?.to_string();
        value.parse().ok()
    }

    /// Real hardware: VB-Audio keeps the Windows volume as a number and never applies it. Plays a very quiet tone through the virtual cable, records
    /// the far side, and checks that Windows' level changes nothing while the app's own gain
    /// does. Puts the level back. Run with `--ignored --nocapture`.
    #[test]
    #[ignore]
    fn vb_audio_ignores_windows_volume_so_the_app_applies_it() {
        const CAPTURE: &str = "CABLE Output (VB-Audio Virtual Cable)";
        let host = cpal::default_host();
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let device = host
            .output_devices()
            .unwrap()
            .find(|device| device.description().unwrap().name().starts_with("CABLE Input"))
            .expect("this check needs the VB-Audio cable");
        let id = device.id().unwrap().to_string();
        assert!(driver_ignores_volume(&host, Some(&id)));
        assert!(!driver_ignores_volume(&host, None), "the Windows default uses the mixer slider instead");
        let volume = VolumeControl::open(Some(&id)).expect("no volume");
        let before = volume.get().unwrap();

        let player = Player::new(100);
        let feeder_player = player.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let feeder_stop = stop.clone();
        let feeder = std::thread::spawn(move || {
            let mut phase = 0f32;
            while !feeder_stop.load(Relaxed) {
                let mut chunk = Vec::with_capacity(882);
                for _ in 0..441 {
                    phase += std::f32::consts::TAU * 1000.0 / RATE as f32;
                    let sample = phase.sin() * 0.01; // -40 dB, barely there
                    chunk.extend([sample, sample]);
                }
                feeder_player.ring.lock().unwrap().push(&chunk, 2, feeder_player.delay);
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        let stream = open_stream(&device, &player).expect("couldn't open");
        std::thread::sleep(Duration::from_millis(400));

        volume.set(1.0);
        std::thread::sleep(Duration::from_millis(200));
        let full = recorded_level(CAPTURE).expect("no recording");
        volume.set(0.25);
        std::thread::sleep(Duration::from_millis(200));
        let windows_low = recorded_level(CAPTURE).expect("no recording");
        volume.set(before);
        player.gain.store(software_gain(0.5).to_bits(), Relaxed);
        std::thread::sleep(Duration::from_millis(200));
        let app_half = recorded_level(CAPTURE).expect("no recording");

        stop.store(true, Relaxed);
        feeder.join().unwrap();
        drop(stream);
        println!("at Windows 100%: {full} dB, at Windows 25%: {windows_low} dB, app gain at half: {app_half} dB");
        assert!((full - windows_low).abs() < 1.0, "VB-Audio applies the Windows volume now, so remove the name match");
        assert!((full - app_half - 15.0).abs() < 2.0, "half the slider should be 15 dB down, measured {}", full - app_half);
    }

    fn moves_and_restores(volume: &VolumeControl) {
        let before = volume.get().expect("no volume reading");
        volume.set(0.25);
        let moved = volume.get().unwrap();
        volume.set(before);
        assert!((moved - 0.25).abs() < 0.02, "level read back as {moved}");
        assert!((volume.get().unwrap() - before).abs() < 0.02, "level wasn't restored");
    }

    /// Real hardware: a picked output moves its own level, and the Windows default path
    /// moves only this program's mixer slider. Puts everything back. Run with `--ignored`.
    #[test]
    #[ignore]
    fn volume_moves_the_right_thing_and_leaves_main_alone() {
        let host = cpal::default_host();
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let cable = host
            .output_devices()
            .unwrap()
            .find(|device| device.description().unwrap().name().starts_with("CABLE Input"))
            .expect("this check needs the VB-Audio cable");
        let picked = VolumeControl::open(Some(&cable.id().unwrap().to_string())).expect("no device volume");
        assert!(matches!(picked, VolumeControl::Device(_)));
        moves_and_restores(&picked);

        // Reads the main volume straight from Windows, to prove the app slider leaves it alone.
        let main = unsafe {
            let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).unwrap();
            let device = devices.GetDefaultAudioEndpoint(eRender, eConsole).unwrap();
            device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None).unwrap()
        };
        let main_before = unsafe { main.GetMasterVolumeLevelScalar() }.unwrap();
        let app = VolumeControl::open(None).expect("no app volume");
        assert!(matches!(app, VolumeControl::App(_)));
        app.set(0.25);
        let main_during = unsafe { main.GetMasterVolumeLevelScalar() }.unwrap();
        moves_and_restores(&app);
        app.set(1.0);
        assert_eq!(main_during, main_before, "the main Windows volume moved");
    }

    fn drain_silent(ring: &mut Ring, delay: usize) -> usize {
        std::iter::from_fn(|| ring.next(delay)).count()
    }

    #[test]
    fn waits_for_the_full_delay_then_plays_in_order() {
        let mut ring = Ring::default();
        ring.push(&[0.1, 0.2, 0.3, 0.4], 2, 3);
        assert_eq!(ring.next(3), None, "still filling");
        ring.push(&[0.5, 0.6], 2, 3);
        assert_eq!(ring.next(3), Some([0.1, 0.2]));
        assert_eq!(ring.next(3), Some([0.3, 0.4]));
        assert_eq!(ring.next(3), Some([0.5, 0.6]));
        assert_eq!(ring.next(3), None, "ran dry");
        assert_eq!(ring.underruns, 1);
    }

    #[test]
    fn mono_plays_on_both_sides() {
        let mut ring = Ring::default();
        ring.push(&[0.7], 1, 1);
        assert_eq!(ring.next(1), Some([0.7, 0.7]));
    }

    #[test]
    fn flush_empties_and_refills() {
        let mut ring = Ring::default();
        ring.push(&[0.1; 8], 2, 2);
        assert!(ring.next(2).is_some());
        ring.flush();
        assert_eq!(ring.next(2), None);
        assert_eq!(drain_silent(&mut ring, 2), 0);
    }

    #[test]
    fn device_stall_jumps_back_to_the_delay() {
        let mut ring = Ring::default();
        ring.push(&[0.0; 2000], 2, 100);
        assert_eq!(ring.frames.len(), 100);
    }

    #[test]
    fn software_gain_uses_the_airplay_scale() {
        assert_eq!(software_gain(1.0), 1.0);
        assert!((software_gain(0.5) - 0.1778).abs() < 0.001, "half the slider is -15 dB");
        assert!((software_gain(0.01) - 0.0325).abs() < 0.002, "near the bottom is about -30 dB");
        assert_eq!(software_gain(0.0), 0.0, "all the way down is silent");
    }

    /// Pause must freeze the "playing" signal at once, and audio counts as track position.
    #[test]
    fn player_tracks_receiving_and_position() {
        let player = Player::new(2000);
        let (commands, wakes) = std::sync::mpsc::channel();
        let mut session = player.new_session(2, commands);
        assert!(!player.receiving());
        session.audio_process(&[0.1; 704]);
        assert!(player.receiving());
        assert_eq!(player.frames_received(), 352);
        assert!(matches!(wakes.try_recv(), Ok(Command::Wake)), "first sound wakes the output thread");
        assert!(matches!(wakes.try_recv(), Ok(Command::FollowLevel)), "and brings the phone to the output");
        session.audio_process(&[0.1; 704]);
        assert!(wakes.try_recv().is_err(), "later sound does neither");
        session.audio_flush();
        assert!(!player.receiving());
        assert!(player.sound_within(DEVICE_LINGER_MS), "the device stays open through a short pause");
        session.audio_process(&[0.1; 704]);
        assert!(player.receiving());
        assert!(matches!(wakes.try_recv(), Ok(Command::FollowLevel)), "resuming syncs the phone again");
    }

    /// A paused iPhone keeps streaming silence, which must not read as playing or move
    /// the track position.
    #[test]
    fn a_paused_phones_silence_is_not_playing() {
        let player = Player::new(2000);
        let (commands, wakes) = std::sync::mpsc::channel();
        let mut session = player.new_session(2, commands);
        session.audio_process(&[0.1; 704]);
        session.audio_flush();
        while wakes.try_recv().is_ok() {}
        let position = player.frames_received();
        for _ in 0..100 {
            session.audio_process(&[0.0; 704]);
        }
        assert!(!player.receiving());
        assert_eq!(player.frames_received(), position, "the position stays put");
        assert!(player.ring.lock().unwrap().frames.is_empty(), "nothing buffered");
        assert!(wakes.try_recv().is_err(), "and no volume steps for the phone");
    }

    /// What the phone reports beats what the audio suggests, both ways.
    #[test]
    fn what_the_phone_says_beats_the_audio() {
        let player = Player::new(2000);
        let (commands, _wakes) = std::sync::mpsc::channel();
        let mut session = player.new_session(2, commands);

        // Playing through a long silent stretch: still playing, and still counting.
        session.audio_process(&[0.1; 704]);
        player.set_phone_playing(Some(true));
        player.last_sound_ms.store(0, Relaxed);
        session.audio_process(&[0.0; 704]);
        assert!(player.receiving());
        assert_eq!(player.frames_received(), 704);

        // Skipping a track flushes the buffer, but the phone is still playing.
        session.audio_flush();
        assert!(player.receiving(), "no flicker to Play on a track change");

        // Paused, with sound still in flight: paused at once.
        player.set_phone_playing(Some(false));
        assert!(!player.receiving());
        session.audio_process(&[0.0; 704]);
        assert_eq!(player.frames_received(), 704, "the position stops");

        // A new connection starts knowing nothing.
        let (commands, _wakes) = std::sync::mpsc::channel();
        let _session = player.new_session(2, commands);
        assert_eq!(player.phone_state.load(Relaxed), PHONE_UNKNOWN);
    }

    /// A session that opens with only silence (the phone's keep-alive) never looks like playing.
    #[test]
    fn a_silent_session_never_starts_playing() {
        let player = Player::new(2000);
        let (commands, wakes) = std::sync::mpsc::channel();
        let mut session = player.new_session(2, commands);
        session.audio_process(&[0.0; 704]);
        assert!(!player.receiving());
        assert!(wakes.try_recv().is_err());
    }

    #[test]
    fn a_quiet_gap_inside_a_song_keeps_playing() {
        let player = Player::new(2000);
        let (commands, _wakes) = std::sync::mpsc::channel();
        let mut session = player.new_session(2, commands);
        session.audio_process(&[0.1; 704]);
        session.audio_process(&[0.0; 704]);
        assert!(player.receiving());
        assert_eq!(player.frames_received(), 704, "the gap counts as track time");
    }

    /// A newer phone takes over; the old one must not feed the buffer.
    #[test]
    fn only_the_latest_session_plays() {
        let player = Player::new(2000);
        let (commands, _wakes) = std::sync::mpsc::channel();
        let mut old = player.new_session(2, commands.clone());
        let _new = player.new_session(2, commands);
        old.audio_process(&[0.1; 704]);
        assert_eq!(player.frames_received(), 0);
    }

    /// Ten minutes of playback with the phone's clock `ppm` off the sound card's, its
    /// packets arriving in Wi-Fi style bursts, and `extra` frames of start-up burst.
    fn simulate(ppm: f64, extra: usize) -> Ring {
        let delay = 88200;
        let mut ring = Ring::default();
        ring.push(&vec![0.0; (delay + extra) * 2], 2, delay);
        let packet = vec![0f32; 704];
        let frames_per_chunk = 441.0 * (1.0 + ppm / 1e6); // the card pulls 10 ms at a time
        let (mut owed, mut chunk) = (0.0, 0u32);
        for _ in 0..(600 * 100) {
            owed += frames_per_chunk;
            chunk += 1;
            // A burst every 100 ms, as seen from the phone's real log.
            if chunk.is_multiple_of(10) {
                while owed >= 352.0 {
                    ring.push(&packet, 2, delay);
                    owed -= 352.0;
                }
            }
            for _ in 0..441 {
                assert!(ring.next(delay).is_some(), "underrun at {ppm} ppm");
            }
        }
        ring
    }

    /// A clock this far off must not move the buffer away from where it settled.
    #[test]
    fn drift_correction_holds_the_buffer() {
        for ppm in [-200.0, 200.0] {
            let ring = simulate(ppm, 0);
            let off_ms = (ring.smoothed - ring.target).abs() * 1000.0 / 44100.0;
            assert!(off_ms < 40.0, "buffer drifted {off_ms} ms at {ppm} ppm");
            let corrected = if ppm > 0.0 { ring.skipped } else { ring.repeated };
            assert!(corrected > 0, "correction never ran at {ppm} ppm");
        }
    }

    /// The bug the real phone exposed: Wi-Fi bursts and a start-up burst made it correct
    /// constantly. With matching clocks there is nothing to fix, so it must touch nothing.
    #[test]
    fn matching_clocks_get_no_correction() {
        let ring = simulate(0.0, 4410);
        assert_eq!((ring.skipped, ring.repeated), (0, 0));
    }

    /// Correction only runs as hard as the clocks disagree: 50 ppm for 10 minutes
    /// is about 1300 frames, not the 26000 that constant correction would be.
    #[test]
    fn correction_matches_the_drift() {
        let ring = simulate(50.0, 0);
        assert!(ring.skipped < 2500, "corrected {} frames", ring.skipped);
        assert_eq!(ring.repeated, 0, "never corrects the wrong way");
    }

    #[test]
    fn merged_and_added_frames_blend_with_their_neighbours() {
        let mut ring = Ring::default();
        ring.push(&[0.0, 0.0, 0.25, 0.25, 0.5, 0.5, 0.75, 0.75], 2, 2);
        assert_eq!(ring.next(2), Some([0.0, 0.0]));
        ring.drift = 1.0;
        assert_eq!(ring.next(2), Some([0.375, 0.375]), "merged 0.25 and 0.5");
        ring.drift = -1.0;
        assert_eq!(ring.next(2), Some([0.5625, 0.5625]), "added between 0.375 and 0.75");
        assert_eq!(ring.next(2), Some([0.75, 0.75]), "and nothing was lost");
    }
}
