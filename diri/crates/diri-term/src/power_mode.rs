//! Power mode: an easter egg that makes typing explode.
//!
//! Typing throws cyan sparks off the character that just landed, deleting
//! burns the erased cell away in gold, and a cursor that jumps (Enter, a
//! prompt redraw after a key) leaves a lightning bolt between where it was
//! and where it went. Like [`crate::cursor_motion`], everything is a pure
//! function of `now`: the element, the tests and frame renders share it, and
//! an idle terminal schedules nothing once the last ember is gone.
//!
//! Only moves the user caused fire an effect: each key arms one, and the
//! first cursor move its echo produces spends it. Agent output streaming
//! through a pane stays quiet.

use std::time::{Duration, Instant};

use crate::cursor_motion::{CursorCell, KEYSTROKE_WINDOW};

/// What the last key did, as far as the effect is concerned.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PowerKey {
    Type,
    Delete,
    Enter,
}

impl PowerKey {
    /// Classifies bytes the pane encoded for the PTY. Printable text comes
    /// through the element's own input handler as [`PowerKey::Type`].
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        match bytes {
            [0x7f] | [0x08] | [0x17] | [0x1b, 0x7f] | b"\x1b[3~" => Some(Self::Delete),
            [b'\r'] | [b'\n'] => Some(Self::Enter),
            [] => None,
            // Arrows and other escapes move the cursor; let them zap.
            [0x1b, ..] => Some(Self::Enter),
            bytes if bytes.iter().all(|byte| !byte.is_ascii_control()) => Some(Self::Type),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Effect {
    Sparks,
    Fire,
    Bolt,
}

/// Longest any burst lives. Past it the burst is dropped.
const SPARK_LIFE: Duration = Duration::from_millis(620);
const FIRE_LIFE: Duration = Duration::from_millis(900);
const BOLT_LIFE: Duration = Duration::from_millis(380);
/// How often the bolt re-forks while it fades: a static zigzag reads as a
/// drawing, a flickering one as electricity.
const BOLT_FLICKER: Duration = Duration::from_millis(40);
/// A held key fires one burst per echo; past this many the oldest go first.
const MAX_BURSTS: usize = 24;

const SPARK_COUNT: u64 = 14;
const FIRE_COUNT: u64 = 26;
const BOLT_SPARKS: u64 = 12;

const CYAN: [f32; 3] = [0.37, 0.96, 1.0];
const WHITE_HOT: [f32; 3] = [1.0, 0.98, 0.86];
const GOLD: [f32; 3] = [1.0, 0.80, 0.25];
const EMBER: [f32; 3] = [1.0, 0.36, 0.08];

#[derive(Clone, Copy, Debug)]
struct Burst {
    effect: Effect,
    /// Cell coordinates, fractional: the centre of a cell is `col + 0.5`.
    from: (f32, f32),
    to: (f32, f32),
    born: Instant,
    seed: u64,
}

/// One thing to paint, in pixels from the grid's top-left corner.
#[derive(Clone, Debug, PartialEq)]
pub enum PowerShape {
    /// A round particle and its halo.
    Dot {
        x: f32,
        y: f32,
        radius: f32,
        rgb: [f32; 3],
        alpha: f32,
    },
    /// A polyline: the bolt's glow pass, then its core.
    Stroke {
        points: Vec<(f32, f32)>,
        width: f32,
        rgb: [f32; 3],
        alpha: f32,
    },
}

#[derive(Debug, Default)]
pub struct PowerMode {
    enabled: bool,
    armed: Option<(PowerKey, Instant)>,
    bursts: Vec<Burst>,
    next_seed: u64,
}

impl PowerMode {
    /// Turning it off drops whatever is still in the air.
    pub fn set_enabled(&mut self, enabled: bool) {
        if !enabled {
            self.armed = None;
            self.bursts.clear();
        }
        self.enabled = enabled;
    }

    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn note_key(&mut self, key: PowerKey, now: Instant) {
        if self.enabled {
            self.armed = Some((key, now));
        }
    }

    /// A grid update moved (or kept) the cursor. Spends the armed key on the
    /// first move its echo makes.
    pub fn note_move(
        &mut self,
        previous: CursorCell,
        next: CursorCell,
        touches_cursor_row: bool,
        now: Instant,
    ) {
        let Some((key, at)) = self.armed else { return };
        if now.saturating_duration_since(at) > KEYSTROKE_WINDOW {
            self.armed = None;
            return;
        }
        let Some((effect, from, to)) = classify(key, previous, next, touches_cursor_row) else {
            return;
        };
        self.armed = None;
        if self.bursts.len() >= MAX_BURSTS {
            self.bursts.remove(0);
        }
        self.next_seed = self.next_seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        self.bursts.push(Burst {
            effect,
            from,
            to,
            born: now,
            seed: self.next_seed,
        });
    }

    /// Whether anything is still in the air at `now`.
    #[must_use]
    pub fn is_running(&self, now: Instant) -> bool {
        self.bursts
            .iter()
            .any(|burst| now.saturating_duration_since(burst.born) < life(burst.effect))
    }

    /// Everything to paint at `now` for cells `cell_w` by `cell_h` pixels.
    /// Drops bursts that have burnt out.
    pub fn sample(&mut self, now: Instant, cell_w: f32, cell_h: f32) -> Vec<PowerShape> {
        self.bursts
            .retain(|burst| now.saturating_duration_since(burst.born) < life(burst.effect));
        let mut shapes = Vec::new();
        for burst in &self.bursts {
            let age = now.saturating_duration_since(burst.born).as_secs_f32();
            match burst.effect {
                Effect::Sparks => {
                    sparks(burst, age, cell_w, cell_h, SPARK_COUNT, &mut shapes);
                }
                Effect::Fire => fire(burst, age, cell_w, cell_h, &mut shapes),
                Effect::Bolt => {
                    bolt(burst, age, cell_w, cell_h, &mut shapes);
                    let landed = Burst {
                        from: burst.to,
                        ..*burst
                    };
                    sparks(&landed, age, cell_w, cell_h, BOLT_SPARKS, &mut shapes);
                }
            }
        }
        shapes
    }
}

fn life(effect: Effect) -> Duration {
    match effect {
        Effect::Sparks => SPARK_LIFE,
        Effect::Fire => FIRE_LIFE,
        Effect::Bolt => SPARK_LIFE,
    }
}

/// A point in cell units: the centre of a cell is `col + 0.5`.
type CellPoint = (f32, f32);

fn centre(cell: CursorCell) -> CellPoint {
    (f32::from(cell.col) + 0.5, f32::from(cell.row) + 0.5)
}

/// Which effect a cursor move after `key` earns, from where to where.
fn classify(
    key: PowerKey,
    previous: CursorCell,
    next: CursorCell,
    touches_cursor_row: bool,
) -> Option<(Effect, CellPoint, CellPoint)> {
    let same_row = previous.row == next.row;
    let cols = previous.col.abs_diff(next.col);
    match key {
        // Backspace lands on the erased cell; forward delete stays on it.
        PowerKey::Delete
            if same_row
                && (next.col < previous.col || (next == previous && touches_cursor_row)) =>
        {
            Some((Effect::Fire, centre(next), centre(next)))
        }
        // A delete whose echo has not redrawn the row yet waits for it.
        PowerKey::Delete if previous == next => None,
        PowerKey::Type if same_row && next.col > previous.col && cols <= 8 => {
            Some((Effect::Sparks, centre(previous), centre(previous)))
        }
        _ if previous == next => None,
        _ => Some((Effect::Bolt, centre(previous), centre(next))),
    }
}

/// splitmix64: cheap, deterministic, and good enough for confetti.
fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A value in `0.0..1.0` for particle `index`, attribute `salt`.
fn unit(seed: u64, index: u64, salt: u64) -> f32 {
    (mix(seed ^ mix(index.wrapping_mul(31).wrapping_add(salt))) >> 40) as f32 / (1u64 << 24) as f32
}

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(2)
}

fn lerp_rgb(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

/// A soft halo: quads have no blur, so a few faint discs stack into a
/// falloff instead of one disc with a hard rim.
fn glow(x: f32, y: f32, radius: f32, rgb: [f32; 3], alpha: f32, out: &mut Vec<PowerShape>) {
    for (scale, strength) in [(4.2, 0.05), (2.9, 0.08), (1.8, 0.16)] {
        out.push(PowerShape::Dot {
            x,
            y,
            radius: radius * scale,
            rgb,
            alpha: alpha * strength,
        });
    }
}

fn sparks(
    burst: &Burst,
    age: f32,
    cell_w: f32,
    cell_h: f32,
    count: u64,
    out: &mut Vec<PowerShape>,
) {
    let (cx, cy) = (burst.from.0 * cell_w, burst.from.1 * cell_h);
    // The landing flash: a bloom over the cell that fades in a few frames.
    let flash = 1.0 - (age / 0.14).min(1.0);
    if flash > 0.0 {
        out.push(PowerShape::Dot {
            x: cx,
            y: cy,
            radius: cell_h * (0.55 + 0.35 * (1.0 - flash)),
            rgb: CYAN,
            alpha: flash * flash * 0.45,
        });
    }
    for index in 0..count {
        let lifetime = 0.34 + 0.28 * unit(burst.seed, index, 1);
        let t = age / lifetime;
        if t >= 1.0 {
            continue;
        }
        // Mostly up and out: sparks that fall straight into the next row
        // read as dirt.
        let angle = -std::f32::consts::FRAC_PI_2 + (unit(burst.seed, index, 2) - 0.5) * 3.4;
        let speed = cell_h * (4.0 + 7.0 * unit(burst.seed, index, 3));
        let travel = ease_out(t) * lifetime;
        let x = cx + angle.cos() * speed * travel;
        let y = cy + angle.sin() * speed * travel + 0.5 * cell_h * 9.0 * age * age;
        let radius = cell_w * (0.14 + 0.16 * unit(burst.seed, index, 4)) * (1.0 - 0.6 * t);
        let alpha = 1.0 - t * t;
        glow(x, y, radius, CYAN, alpha, out);
        out.push(PowerShape::Dot {
            x,
            y,
            radius,
            rgb: lerp_rgb([0.92, 1.0, 1.0], CYAN, t),
            alpha,
        });
    }
}

fn fire(burst: &Burst, age: f32, cell_w: f32, cell_h: f32, out: &mut Vec<PowerShape>) {
    let (cx, cy) = (burst.from.0 * cell_w, burst.from.1 * cell_h);
    for index in 0..FIRE_COUNT {
        let lifetime = 0.45 + 0.45 * unit(burst.seed, index, 1);
        let t = age / lifetime;
        if t >= 1.0 {
            continue;
        }
        // The erased character is blown out to the right, the way the text
        // after it closes in from that side.
        let angle = -0.35 + (unit(burst.seed, index, 2) - 0.5) * 2.2;
        let speed = cell_w * (6.0 + 22.0 * unit(burst.seed, index, 3));
        let travel = ease_out(t) * lifetime;
        let x = cx + angle.cos() * speed * travel;
        // Embers rise a little as they cool.
        let y = cy + angle.sin() * speed * travel * 0.45 - cell_h * 0.8 * age;
        let radius = cell_w * (0.10 + 0.20 * unit(burst.seed, index, 4)) * (1.0 - 0.5 * t);
        let rgb = if t < 0.35 {
            lerp_rgb(WHITE_HOT, GOLD, t / 0.35)
        } else {
            lerp_rgb(GOLD, EMBER, (t - 0.35) / 0.65)
        };
        let alpha = (1.0 - t).powf(1.4);
        glow(x, y, radius, EMBER, alpha * 0.85, out);
        out.push(PowerShape::Dot {
            x,
            y,
            radius,
            rgb,
            alpha,
        });
    }
}

fn bolt(burst: &Burst, age: f32, cell_w: f32, cell_h: f32, out: &mut Vec<PowerShape>) {
    let life = BOLT_LIFE.as_secs_f32();
    if age >= life {
        return;
    }
    let (x0, y0) = (burst.from.0 * cell_w, burst.from.1 * cell_h);
    let (x1, y1) = (burst.to.0 * cell_w, burst.to.1 * cell_h);
    let (dx, dy) = (x1 - x0, y1 - y0);
    let length = dx.hypot(dy).max(1.0);
    let (nx, ny) = (-dy / length, dx / length);
    let segments = ((length / cell_h).ceil() as u64).clamp(3, 18);
    let flicker = (age / BOLT_FLICKER.as_secs_f32()) as u64;
    let seed = mix(burst.seed ^ flicker);
    let reach = cell_w * 2.2;
    let points: Vec<(f32, f32)> = (0..=segments)
        .map(|step| {
            let along = step as f32 / segments as f32;
            // Pinned at both ends, widest in the middle.
            let jitter = if step == 0 || step == segments {
                0.0
            } else {
                (unit(seed, step, 5) - 0.5) * 2.0 * reach * (std::f32::consts::PI * along).sin()
            };
            (x0 + dx * along + nx * jitter, y0 + dy * along + ny * jitter)
        })
        .collect();
    let alpha = 1.0 - (age / life).powi(2);
    // Two glow passes under a white-hot core.
    out.push(PowerShape::Stroke {
        points: points.clone(),
        width: cell_w * 2.2,
        rgb: CYAN,
        alpha: alpha * 0.16,
    });
    out.push(PowerShape::Stroke {
        points: points.clone(),
        width: cell_w * 0.9,
        rgb: CYAN,
        alpha: alpha * 0.45,
    });
    out.push(PowerShape::Stroke {
        points,
        width: (cell_w * 0.3).max(2.0),
        rgb: [0.92, 1.0, 1.0],
        alpha,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(col: u16, row: u16) -> CursorCell {
        CursorCell { col, row }
    }

    fn armed(key: PowerKey, now: Instant) -> PowerMode {
        let mut power = PowerMode::default();
        power.set_enabled(true);
        power.note_key(key, now);
        power
    }

    #[test]
    fn keys_classify_from_their_bytes() {
        assert_eq!(PowerKey::from_bytes(b"\x7f"), Some(PowerKey::Delete));
        assert_eq!(PowerKey::from_bytes(b"\x1b[3~"), Some(PowerKey::Delete));
        assert_eq!(PowerKey::from_bytes(b"\r"), Some(PowerKey::Enter));
        assert_eq!(PowerKey::from_bytes(b"\x1b[A"), Some(PowerKey::Enter));
        assert_eq!(PowerKey::from_bytes(b"hello"), Some(PowerKey::Type));
        assert_eq!(PowerKey::from_bytes(b"\x03"), None);
    }

    #[test]
    fn each_key_earns_one_effect_from_its_echo() {
        let now = Instant::now();
        let mut power = armed(PowerKey::Type, now);
        power.note_move(cell(4, 2), cell(5, 2), true, now);
        assert_eq!(power.bursts.len(), 1);
        assert_eq!(power.bursts[0].effect, Effect::Sparks);
        // Output that follows the echo is not the user's.
        power.note_move(cell(5, 2), cell(0, 9), true, now);
        assert_eq!(power.bursts.len(), 1);
    }

    #[test]
    fn effects_follow_the_key_and_the_move() {
        let now = Instant::now();
        let cases = [
            (PowerKey::Type, cell(4, 2), cell(5, 2), Effect::Sparks),
            (PowerKey::Delete, cell(5, 2), cell(4, 2), Effect::Fire),
            (PowerKey::Delete, cell(5, 2), cell(5, 2), Effect::Fire),
            (PowerKey::Enter, cell(9, 2), cell(2, 3), Effect::Bolt),
            // A keystroke that redraws the prompt elsewhere zaps too.
            (PowerKey::Type, cell(79, 2), cell(0, 3), Effect::Bolt),
        ];
        for (key, previous, next, effect) in cases {
            let mut power = armed(key, now);
            power.note_move(previous, next, true, now);
            assert_eq!(
                power.bursts[0].effect, effect,
                "{key:?} {previous:?}->{next:?}"
            );
        }
    }

    #[test]
    fn output_without_a_key_and_disabled_mode_stay_quiet() {
        let now = Instant::now();
        let mut power = PowerMode::default();
        power.set_enabled(true);
        power.note_move(cell(4, 2), cell(5, 2), true, now);
        assert!(power.bursts.is_empty());

        let mut off = PowerMode::default();
        off.note_key(PowerKey::Type, now);
        off.note_move(cell(4, 2), cell(5, 2), true, now);
        assert!(off.bursts.is_empty());

        let mut stale = armed(PowerKey::Type, now);
        stale.note_move(cell(4, 2), cell(5, 2), true, now + KEYSTROKE_WINDOW * 2);
        assert!(stale.bursts.is_empty());
    }

    #[test]
    fn bursts_burn_out_and_stop_asking_for_frames() {
        let now = Instant::now();
        let mut power = armed(PowerKey::Delete, now);
        power.note_move(cell(5, 2), cell(4, 2), true, now);
        assert!(power.is_running(now));
        assert!(
            !power
                .sample(now + Duration::from_millis(100), 8.0, 16.0)
                .is_empty()
        );
        let later = now + FIRE_LIFE;
        assert!(!power.is_running(later));
        assert!(power.sample(later, 8.0, 16.0).is_empty());
        assert!(power.bursts.is_empty());
    }

    #[test]
    fn a_held_key_keeps_a_bounded_number_of_bursts() {
        let now = Instant::now();
        let mut power = PowerMode::default();
        power.set_enabled(true);
        for col in 0..100 {
            power.note_key(PowerKey::Type, now);
            power.note_move(cell(col, 0), cell(col + 1, 0), true, now);
        }
        assert_eq!(power.bursts.len(), MAX_BURSTS);
    }

    #[test]
    fn bolts_are_pinned_to_both_cursor_cells() {
        let now = Instant::now();
        let mut power = armed(PowerKey::Enter, now);
        power.note_move(cell(10, 1), cell(0, 6), true, now);
        let shapes = power.sample(now + Duration::from_millis(30), 8.0, 16.0);
        let Some(PowerShape::Stroke { points, .. }) = shapes.first() else {
            panic!("bolt first");
        };
        assert_eq!(points.first(), Some(&(84.0, 24.0)));
        assert_eq!(points.last(), Some(&(4.0, 104.0)));
    }
}
