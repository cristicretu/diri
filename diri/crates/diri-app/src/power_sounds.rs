//! Power mode's sounds: a tick per key, a crackle per delete, a zap when the
//! cursor jumps.
//!
//! Synthesized once per variant and played in-process: a key is far too
//! frequent to spawn a player for, which is what the status chimes do. Only
//! keys the user sends make a sound, and only while power mode is on.

use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "macos")]
use std::time::Duration;

use diri_term::power_mode::PowerKey;

use crate::sounds::{SAMPLE_RATE, wav_container};

/// Mirrors `Prefs::terminal_power_mode` for input paths that never see the
/// store; the terminal pane writes it each render.
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Ticks from key repeat are capped at about 30 per second.
#[cfg(target_os = "macos")]
const MIN_GAP: Duration = Duration::from_millis(32);
const TICK_VARIANTS: u32 = 4;
#[cfg(target_os = "macos")]
const VOLUME: f32 = 0.42;

pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

/// The user sent `key` to a terminal.
pub fn note_key(key: PowerKey) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    #[cfg(target_os = "macos")]
    macos::play(key);
    #[cfg(not(target_os = "macos"))]
    let _ = key;
}

/// Which synthesized clip plays for `key`; ticks rotate through a few
/// pitches so a held key does not drone.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Clip {
    Tick(u32),
    Crackle,
    Zap,
}

impl Clip {
    fn for_key(key: PowerKey, sequence: u32) -> Self {
        match key {
            PowerKey::Type => Self::Tick(sequence % TICK_VARIANTS),
            PowerKey::Delete => Self::Crackle,
            PowerKey::Enter => Self::Zap,
        }
    }
}

/// splitmix64 noise: deterministic, so every build makes the same sounds.
struct Noise(u64);

impl Noise {
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        (z >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
    }
}

/// One 44.1 kHz, signed 16-bit, mono RIFF/WAVE clip.
#[must_use]
pub fn synthesize_wav(clip: Clip) -> Vec<u8> {
    use std::f64::consts::TAU;
    let rate = f64::from(SAMPLE_RATE);
    let (seconds, seed) = match clip {
        Clip::Tick(variant) => (0.045, 7 + u64::from(variant)),
        Clip::Crackle => (0.32, 19),
        Clip::Zap => (0.2, 23),
    };
    let frames = (seconds * rate) as usize;
    let mut noise = Noise(seed);
    let mut low = 0.0;
    let mut phase = 0.0;
    let mut samples = Vec::with_capacity(frames);
    for index in 0..frames {
        let t = index as f64 / rate;
        let white = noise.next();
        let value = match clip {
            // A short bright click over a pitched blip: a keycap, not a beep.
            Clip::Tick(variant) => {
                let pitch = 1_900.0 * (1.0 + 0.07 * f64::from(variant));
                let click = white * (-t / 0.004).exp() * 0.45;
                let blip = (TAU * pitch * t).sin() * (-t / 0.012).exp() * 0.30;
                click + blip
            }
            // A low thump under filtered noise that pops as it fades.
            Clip::Crackle => {
                low += (white - low) * 0.08;
                let thump_pitch = 110.0 * (-t / 0.09).exp() + 45.0;
                phase += TAU * thump_pitch / rate;
                let thump = phase.sin() * (-t / 0.07).exp() * 0.55;
                let rumble = low * (-t / 0.11).exp() * 1.6;
                let pops = if noise.next() > 0.996 {
                    noise.next() * 0.5 * (-t / 0.18).exp()
                } else {
                    0.0
                };
                thump + rumble + pops
            }
            // A falling sawtooth with a buzz of noise on it.
            Clip::Zap => {
                let pitch = 1_800.0 * (-t / 0.05).exp() + 160.0;
                phase += pitch / rate;
                let saw = (phase.fract() * 2.0 - 1.0) * 0.32;
                (saw + white * 0.12) * (-t / 0.06).exp()
            }
        };
        // Ramp the first and last 2 ms so no clip starts or ends on a click.
        let edge = (t / 0.002).min((seconds - t) / 0.002).clamp(0.0, 1.0);
        samples.push(((value * edge).clamp(-1.0, 1.0) * f64::from(i16::MAX)) as i16);
    }
    wav_container(&samples)
}

#[cfg(target_os = "macos")]
mod macos {
    use std::cell::RefCell;
    use std::collections::{HashMap, VecDeque};
    use std::time::Instant;

    use objc2::rc::Retained;
    use objc2::{AnyThread, MainThreadMarker};
    use objc2_app_kit::NSSound;
    use objc2_foundation::NSData;

    use super::{Clip, MIN_GAP, PowerKey, VOLUME, synthesize_wav};

    /// Sounds still playing. AppKit does not promise to keep an `NSSound`
    /// alive through playback, so the last few are held here.
    const HELD: usize = 12;

    #[derive(Default)]
    struct Player {
        clips: HashMap<Clip, Retained<NSData>>,
        playing: VecDeque<Retained<NSSound>>,
        last: Option<Instant>,
        sequence: u32,
    }

    thread_local! {
        static PLAYER: RefCell<Player> = RefCell::default();
    }

    pub(super) fn play(key: PowerKey) {
        // Keys arrive on the main thread; NSSound wants it.
        if MainThreadMarker::new().is_none() {
            return;
        }
        PLAYER.with_borrow_mut(|player| {
            let now = Instant::now();
            if key == PowerKey::Type
                && player
                    .last
                    .is_some_and(|last| now.saturating_duration_since(last) < MIN_GAP)
            {
                return;
            }
            player.last = Some(now);
            player.sequence = player.sequence.wrapping_add(1);
            let clip = Clip::for_key(key, player.sequence);
            let data = player
                .clips
                .entry(clip)
                .or_insert_with(|| NSData::with_bytes(&synthesize_wav(clip)))
                .clone();
            let Some(sound) = NSSound::initWithData(NSSound::alloc(), &data) else {
                return;
            };
            sound.setVolume(VOLUME);
            if sound.play() {
                if player.playing.len() >= HELD {
                    player.playing.pop_front();
                }
                player.playing.push_back(sound);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clips_are_short_valid_and_distinct() {
        let clips = [Clip::Tick(0), Clip::Tick(1), Clip::Crackle, Clip::Zap];
        let rendered: Vec<_> = clips.iter().map(|clip| synthesize_wav(*clip)).collect();
        for (clip, wav) in clips.iter().zip(&rendered) {
            assert_eq!(&wav[0..4], b"RIFF", "{clip:?}");
            let seconds = (wav.len() - 44) as f64 / 2.0 / f64::from(SAMPLE_RATE);
            assert!(seconds < 0.35, "{clip:?}: {seconds}s");
            let peak = wav[44..]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]]).unsigned_abs())
                .max()
                .unwrap();
            assert!(peak > 2_000, "{clip:?} is silent: {peak}");
            assert_eq!(&wav[44..46], &[0, 0], "{clip:?} starts on a click");
        }
        for (index, wav) in rendered.iter().enumerate() {
            assert!(rendered[index + 1..].iter().all(|other| other != wav));
        }
    }

    #[test]
    fn keys_pick_their_clip() {
        assert_eq!(Clip::for_key(PowerKey::Delete, 3), Clip::Crackle);
        assert_eq!(Clip::for_key(PowerKey::Enter, 3), Clip::Zap);
        assert_eq!(Clip::for_key(PowerKey::Type, 5), Clip::Tick(1));
    }
}
