//! Gentle status-chime synthesis plus an opt-in playback boundary.

use std::io;
use std::time::{Duration, Instant};

pub const SAMPLE_RATE: u32 = 44_100;
pub const CHANNELS: u16 = 1;
pub const BITS_PER_SAMPLE: u16 = 16;
/// Leave plenty of headroom below the user's system volume. These are ambient
/// confirmations, not alarms.
pub const PLAYBACK_VOLUME: f32 = 0.52;

const TAIL_DECAYS: f64 = 4.0;
const TAIL_FADE_SECONDS: f64 = 0.065;
const MIN_CHIME_INTERVAL: Duration = Duration::from_millis(900);
const SAME_CHIME_COOLDOWN: Duration = Duration::from_secs(4);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum StatusSound {
    NeedsInput,
    Done,
    Frozen,
}

#[derive(Clone, Copy)]
struct Strike {
    frequency: f64,
    start: f64,
    attack: f64,
    decay: f64,
    amplitude: f64,
    brightness: f64,
}

// A quiet subharmonic and restrained upper partials make the notes feel more
// like a felt mallet than the buzzy additive beeps this replaced.
const PARTIALS: [(f64, f64); 4] = [(0.5, 0.055), (1.0, 1.0), (2.0, 0.12), (3.0, 0.025)];

// A small rising major third: noticeable enough to ask for attention without
// repeating the same high note twice.
const NEEDS_INPUT: [Strike; 2] = [
    Strike {
        frequency: 392.00,
        start: 0.0,
        attack: 0.014,
        decay: 0.14,
        amplitude: 0.155,
        brightness: 0.70,
    },
    Strike {
        frequency: 493.88,
        start: 0.16,
        attack: 0.016,
        decay: 0.18,
        amplitude: 0.135,
        brightness: 0.62,
    },
];

// A compact C-major arpeggio supplies the bit of earned, game-like delight.
// Each successive note is quieter so the sound resolves instead of demanding
// another look.
const DONE: [Strike; 3] = [
    Strike {
        frequency: 523.25,
        start: 0.0,
        attack: 0.016,
        decay: 0.14,
        amplitude: 0.135,
        brightness: 0.82,
    },
    Strike {
        frequency: 659.25,
        start: 0.08,
        attack: 0.017,
        decay: 0.17,
        amplitude: 0.115,
        brightness: 0.72,
    },
    Strike {
        frequency: 783.99,
        start: 0.16,
        attack: 0.019,
        decay: 0.20,
        amplitude: 0.090,
        brightness: 0.60,
    },
];

// A low falling fifth distinguishes memory pressure from positive events, but
// stays consonant and soft rather than sounding like an error buzzer.
const FROZEN: [Strike; 2] = [
    Strike {
        frequency: 293.66,
        start: 0.0,
        attack: 0.022,
        decay: 0.21,
        amplitude: 0.145,
        brightness: 0.38,
    },
    Strike {
        frequency: 220.00,
        start: 0.07,
        attack: 0.024,
        decay: 0.24,
        amplitude: 0.115,
        brightness: 0.30,
    },
];

/// Prevent a group of agents finishing together from turning into a cascade of
/// overlapping chimes. The authoritative state and notification still arrive;
/// only redundant audio is dropped.
#[derive(Debug, Default)]
pub struct SoundGate {
    last_any: Option<Instant>,
    last_by_kind: [Option<Instant>; 3],
}

impl SoundGate {
    #[must_use]
    pub fn should_play(&mut self, event: StatusSound, now: Instant) -> bool {
        let index = event.index();
        let repeated_too_soon = self.last_by_kind[index]
            .is_some_and(|previous| now.duration_since(previous) < SAME_CHIME_COOLDOWN);
        let burst_too_close = self
            .last_any
            .is_some_and(|previous| now.duration_since(previous) < MIN_CHIME_INTERVAL);
        if repeated_too_soon || burst_too_close {
            return false;
        }

        self.last_any = Some(now);
        self.last_by_kind[index] = Some(now);
        true
    }
}

impl StatusSound {
    const fn index(self) -> usize {
        match self {
            Self::NeedsInput => 0,
            Self::Done => 1,
            Self::Frozen => 2,
        }
    }
}

/// Render one 44.1 kHz, signed 16-bit, mono PCM RIFF/WAVE chime.
#[must_use]
pub fn synthesize_wav(event: StatusSound) -> Vec<u8> {
    let strikes: &[Strike] = match event {
        StatusSound::NeedsInput => &NEEDS_INPUT,
        StatusSound::Done => &DONE,
        StatusSound::Frozen => &FROZEN,
    };
    let tail = strikes
        .iter()
        .map(|strike| strike.start + strike.attack + strike.decay * TAIL_DECAYS)
        .fold(0.0_f64, f64::max);
    let frame_count = (tail * f64::from(SAMPLE_RATE)) as usize;
    let mut samples = Vec::with_capacity(frame_count);

    for index in 0..frame_count {
        let time = index as f64 / f64::from(SAMPLE_RATE);
        let mut value = 0.0;
        for strike in strikes.iter().filter(|strike| time >= strike.start) {
            let local = time - strike.start;
            let attack_progress = (local / strike.attack).clamp(0.0, 1.0);
            let attack = smoothstep(attack_progress);
            let decay_time = (local - strike.attack).max(0.0);
            let envelope = attack * (-decay_time / strike.decay).exp() * strike.amplitude;
            for (partial, weight) in PARTIALS {
                let color = if partial > 1.0 {
                    strike.brightness
                } else {
                    1.0
                };
                value += envelope
                    * weight
                    * color
                    * (2.0 * std::f64::consts::PI * strike.frequency * partial * local).sin();
            }
        }
        let fade = smoothstep(((tail - time) / TAIL_FADE_SECONDS).clamp(0.0, 1.0));
        samples.push(((value * fade).clamp(-1.0, 1.0) * f64::from(i16::MAX)) as i16);
    }
    wav_container(&samples)
}

fn smoothstep(value: f64) -> f64 {
    value * value * (3.0 - 2.0 * value)
}

fn wav_container(samples: &[i16]) -> Vec<u8> {
    let data_size = u32::try_from(samples.len() * 2).expect("status sound fits in a WAV");
    let mut wav = Vec::with_capacity(44 + data_size as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_size).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&CHANNELS.to_le_bytes());
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    wav.extend_from_slice(&2_u16.to_le_bytes());
    wav.extend_from_slice(&BITS_PER_SAMPLE.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_size.to_le_bytes());
    for sample in samples {
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    wav
}

/// Small playback seam: tests can inject a recorder and never open an audio device.
pub trait Player {
    fn play(&self, wav: &[u8], volume: f32) -> io::Result<()>;
}

pub fn play<P: Player>(player: &P, event: StatusSound) -> io::Result<()> {
    player.play(&synthesize_wav(event), PLAYBACK_VOLUME)
}

/// macOS's built-in player, available only when explicitly requested.
#[cfg(all(target_os = "macos", feature = "audio-playback"))]
#[derive(Clone, Copy, Debug, Default)]
pub struct AfplayPlayer;

#[cfg(all(target_os = "macos", feature = "audio-playback"))]
impl Player for AfplayPlayer {
    fn play(&self, wav: &[u8], volume: f32) -> io::Result<()> {
        use std::{
            fs::{self, OpenOptions},
            io::Write,
            process::Command,
            sync::atomic::{AtomicU64, Ordering},
            thread,
        };

        static NEXT_FILE: AtomicU64 = AtomicU64::new(0);
        let sequence = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "diri-status-sound-{}-{sequence}.wav",
            std::process::id()
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        file.write_all(wav)?;
        drop(file);

        let child = Command::new("/usr/bin/afplay")
            .arg("--volume")
            .arg(volume.to_string())
            .arg(&path)
            .spawn();
        match child {
            Ok(mut child) => {
                thread::spawn(move || {
                    let _ = child.wait();
                    let _ = fs::remove_file(path);
                });
                Ok(())
            }
            Err(error) => {
                let _ = fs::remove_file(path);
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_headers_and_gentle_output_levels_are_valid() {
        for (event, duration_range) in [
            (StatusSound::NeedsInput, 0.80..0.95),
            (StatusSound::Done, 0.90..1.05),
            (StatusSound::Frozen, 1.00..1.10),
        ] {
            let wav = synthesize_wav(event);
            assert_eq!(&wav[0..4], b"RIFF");
            assert_eq!(&wav[8..12], b"WAVE");
            assert_eq!(&wav[36..40], b"data");
            assert_eq!(
                u32::from_le_bytes(wav[24..28].try_into().unwrap()),
                SAMPLE_RATE
            );
            assert_eq!(u16::from_le_bytes(wav[34..36].try_into().unwrap()), 16);
            let frames = (wav.len() - 44) / 2;
            let seconds = frames as f64 / f64::from(SAMPLE_RATE);
            assert!(duration_range.contains(&seconds), "{event:?}: {seconds}s");
            let peak = wav[44..]
                .chunks_exact(2)
                .map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]]).unsigned_abs())
                .max()
                .unwrap();
            assert!(
                (3_000..=10_000).contains(&peak),
                "{event:?}: unexpected peak {peak}"
            );
            assert_eq!(&wav[44..46], &[0, 0]);
        }
    }

    #[test]
    fn each_status_has_its_own_audio_signature() {
        let needs_input = synthesize_wav(StatusSound::NeedsInput);
        let done = synthesize_wav(StatusSound::Done);
        let frozen = synthesize_wav(StatusSound::Frozen);
        assert_ne!(needs_input, done);
        assert_ne!(needs_input, frozen);
        assert_ne!(done, frozen);
    }

    #[test]
    fn sound_gate_quiets_bursts_and_repeated_events() {
        let start = Instant::now();
        let mut gate = SoundGate::default();

        assert!(gate.should_play(StatusSound::Done, start));
        assert!(!gate.should_play(StatusSound::NeedsInput, start + Duration::from_millis(500)));
        assert!(gate.should_play(StatusSound::NeedsInput, start + Duration::from_millis(900)));
        assert!(!gate.should_play(StatusSound::Done, start + Duration::from_secs(2)));
        assert!(gate.should_play(
            StatusSound::Done,
            start + SAME_CHIME_COOLDOWN + Duration::from_millis(1)
        ));
    }

    #[test]
    fn playback_uses_synthesized_bytes_and_gentle_volume() {
        use std::cell::RefCell;

        #[derive(Default)]
        struct Recorder(RefCell<Option<(Vec<u8>, f32)>>);

        impl Player for Recorder {
            fn play(&self, wav: &[u8], volume: f32) -> io::Result<()> {
                self.0.replace(Some((wav.to_vec(), volume)));
                Ok(())
            }
        }

        let recorder = Recorder::default();
        play(&recorder, StatusSound::Done).unwrap();
        let recorded = recorder.0.borrow();
        let (wav, volume) = recorded.as_ref().unwrap();
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(*volume, PLAYBACK_VOLUME);
    }
}
