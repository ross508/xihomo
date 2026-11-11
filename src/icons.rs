//! Bundled menu bar icons, drawn in code as template images (black plus alpha). State is carried by
//! shape, not color: stopped is hollow, running is solid, error is hollow with a slash.
use crate::backend::Phase;

pub const SETS: &[&str] = &["orbit", "shield", "bars"];
/// 18pt at 2x, the width of most menu bar icons; AppKit sizes the item, and its highlight, from it.
const SIZE: u32 = 36;
pub const POINTS: f64 = (SIZE / 2) as f64;
/// Shapes reach 0.9 at most, so the canvas starts there instead of leaving an empty margin.
const EXTENT: f32 = 0.9;
const SAMPLES: u32 = 4;
const STROKE: f32 = 0.17;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum State {
    Stopped,
    Running,
    Error,
}

impl State {
    pub fn of(phase: &Phase) -> Self {
        match phase {
            Phase::Running => Self::Running,
            Phase::Failed(_) => Self::Error,
            _ => Self::Stopped,
        }
    }
}

/// Is (x, y) in -1..1 inside the shape shrunk by `inset`? Hollow forms subtract an inset copy.
fn inside(set: &str, x: f32, y: f32, inset: f32) -> bool {
    match set {
        "shield" => {
            let top = -0.82 + inset;
            let half_width = if y < 0.05 {
                0.72
            } else {
                0.72 * (1.0 - (y - 0.05) / 0.85)
            } - inset;
            y > top && y < 0.9 - inset * 1.6 && x.abs() < half_width
        }
        "bars" => [(-0.6, 0.2), (0.0, -0.3), (0.6, -0.8)]
            .iter()
            .any(|(center, top)| {
                (x - center).abs() < 0.24 - inset && y > top + inset && y < 0.82 - inset
            }),
        _ => (x * x + y * y).sqrt() < 0.8 - inset,
    }
}

fn covered(set: &str, state: State, x: f32, y: f32) -> bool {
    let solid = inside(set, x, y, 0.0);
    let body = match state {
        State::Running => solid,
        _ => solid && !inside(set, x, y, if set == "bars" { 0.09 } else { STROKE }),
    };
    if state != State::Error {
        return body;
    }
    // Cut a gap around the slash so it stays legible at menu bar size.
    let distance = (x + y).abs() / std::f32::consts::SQRT_2;
    let on_slash = distance < 0.1 && x.abs() < 0.9 && y.abs() < 0.9;
    on_slash || (body && distance > 0.2)
}

pub fn rgba(set: &str, state: State) -> Vec<u8> {
    let mut out = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for py in 0..SIZE {
        for px in 0..SIZE {
            let mut hits = 0;
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let map = |p: u32, s: u32| {
                        (((p as f32 + (s as f32 + 0.5) / SAMPLES as f32) / SIZE as f32) * 2.0 - 1.0)
                            * EXTENT
                    };
                    hits += covered(set, state, map(px, sx), map(py, sy)) as u32;
                }
            }
            out.extend_from_slice(&[0, 0, 0, (hits * 255 / (SAMPLES * SAMPLES)) as u8]);
        }
    }
    out
}

pub fn tray_icon(set: &str, state: State) -> tray_icon::Icon {
    tray_icon::Icon::from_rgba(rgba(set, state), SIZE, SIZE)
        .expect("icon dimensions match the buffer")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_set_distinguishes_states_by_shape_alone() {
        for set in SETS {
            let [a, b, c] = [State::Stopped, State::Running, State::Error].map(|s| rgba(set, s));
            assert!(a != b && b != c && a != c, "{set}");
            for icon in [&a, &b, &c] {
                assert_eq!(icon.len(), (SIZE * SIZE * 4) as usize);
                assert!(
                    icon.chunks(4).all(|p| p[..3] == [0, 0, 0]),
                    "template images are black"
                );
                let ink = icon.chunks(4).filter(|p| p[3] > 128).count();
                assert!(ink > 60, "{set} drew almost nothing");
            }
            let ink = |icon: &[u8]| icon.chunks(4).filter(|p| p[3] > 128).count();
            assert!(
                ink(&b) > ink(&a),
                "{set}: running is solid, stopped is hollow"
            );
        }
    }

    #[test]
    fn sets_differ_from_each_other() {
        let [a, b, c] = [SETS[0], SETS[1], SETS[2]].map(|s| rgba(s, State::Running));
        assert!(a != b && b != c && a != c);
    }
}
