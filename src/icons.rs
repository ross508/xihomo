//! Bundled menu bar icons, drawn in code as template images (black plus alpha). State is carried by
//! shape, not color: stopped is hollow, running is solid, error is hollow with a slash.
use crate::backend::Phase;
use std::f32::consts::{FRAC_1_SQRT_2, FRAC_PI_2, TAU};

pub const SETS: &[&str] = &[
    "circle",
    "square",
    "diamond",
    "hexagon",
    "plus",
    "shield",
    "bars",
    "leaf",
    "drop",
    "snowflake",
];
/// 16pt at 2x; AppKit sizes the status item, and its highlight, from the image.
const SIZE: u32 = 32;
pub const POINTS: f64 = (SIZE / 2) as f64;
/// Shapes reach 0.9 at most, so the canvas starts there instead of leaving an empty margin.
const EXTENT: f32 = 0.9;
const SAMPLES: u32 = 4;
const STROKE: f32 = 0.17;

pub type Polygon = Vec<(f32, f32)>;

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

/// Points around (0, `cy`) at radius `r`, `from`..=`to` radians clockwise from the top.
fn arc(cy: f32, r: f32, from: f32, to: f32, steps: usize) -> Polygon {
    (0..=steps)
        .map(|i| {
            let angle = from + (to - from) * i as f32 / steps as f32;
            (r * angle.sin(), cy - r * angle.cos())
        })
        .collect()
}

/// A regular polygon with a corner at the top.
fn regular(corners: usize, r: f32) -> Polygon {
    let mut polygon = arc(0.0, r, 0.0, TAU, corners);
    polygon.pop();
    polygon
}

fn rect(left: f32, top: f32, right: f32, bottom: f32) -> Polygon {
    vec![(left, top), (right, top), (right, bottom), (left, bottom)]
}

/// A bar of half `length` and half `width` through the center, turned `angle` radians.
fn bar(angle: f32, length: f32, width: f32) -> Polygon {
    let (sin, cos) = angle.sin_cos();
    [
        (-length, -width),
        (length, -width),
        (length, width),
        (-length, width),
    ]
    .map(|(u, v)| (u * cos - v * sin, u * sin + v * cos))
    .to_vec()
}

/// The set's shape as a union of polygons, sized to look alike within one square frame.
fn shape(set: &str) -> Vec<Polygon> {
    let one = |polygon| vec![polygon];
    match set {
        "square" => one(rect(-0.64, -0.64, 0.64, 0.64)),
        "diamond" => one(regular(4, 0.86)),
        "hexagon" => one(regular(6, 0.82)),
        "plus" => one(vec![
            (-0.26, -0.8),
            (0.26, -0.8),
            (0.26, -0.26),
            (0.8, -0.26),
            (0.8, 0.26),
            (0.26, 0.26),
            (0.26, 0.8),
            (-0.26, 0.8),
            (-0.26, 0.26),
            (-0.8, 0.26),
            (-0.8, -0.26),
            (-0.26, -0.26),
        ]),
        "shield" => one(vec![
            (-0.7, -0.74),
            (0.7, -0.74),
            (0.7, 0.04),
            (0.0, 0.84),
            (-0.7, 0.04),
        ]),
        // Rising signal bars filling the same square as the others.
        "bars" => vec![
            rect(-0.7, 0.24, -0.3, 0.7),
            rect(-0.2, -0.24, 0.2, 0.7),
            rect(0.3, -0.7, 0.7, 0.7),
        ],
        // A lens pointed at both ends, across the error slash so the slash stays visible.
        "leaf" => {
            let side = |sign: f32| {
                (0..=24).map(move |i| {
                    let t = i as f32 / 12.0 - 1.0;
                    let (u, v) = (0.92 * t, sign * 0.5 * (1.0 - t * t));
                    ((u - v) * FRAC_1_SQRT_2, (u + v) * FRAC_1_SQRT_2)
                })
            };
            one(side(1.0).chain(side(-1.0).rev().skip(1)).collect())
        }
        // A round bottom meeting the tip along its tangents.
        "drop" => {
            let (cy, r, tip) = (0.24f32, 0.6, -0.86);
            let tangent = (r / (cy - tip)).acos();
            let mut polygon = vec![(0.0, tip)];
            polygon.extend(arc(cy, r, tangent, TAU - tangent, 40));
            one(polygon)
        }
        "snowflake" => (0..3)
            .map(|i| bar(FRAC_PI_2 + i as f32 * TAU / 6.0, 0.86, 0.2))
            .collect(),
        _ => one(regular(96, 0.8)),
    }
}

/// The set's solid shape as polygons in a 0..1 square, y down, for drawing as vector paths.
pub fn outline(set: &str) -> Vec<Polygon> {
    let unit = |v: f32| (v / EXTENT + 1.0) / 2.0;
    shape(set)
        .into_iter()
        .map(|polygon| {
            polygon
                .into_iter()
                .map(|(x, y)| (unit(x), unit(y)))
                .collect()
        })
        .collect()
}

/// Signed distance from (x, y) to the polygon's edge, negative inside.
fn distance(polygon: &[(f32, f32)], x: f32, y: f32) -> f32 {
    let mut nearest = f32::MAX;
    let mut inside = false;
    for (i, &(ax, ay)) in polygon.iter().enumerate() {
        let (bx, by) = polygon[(i + 1) % polygon.len()];
        let (ex, ey) = (bx - ax, by - ay);
        let t = (((x - ax) * ex + (y - ay) * ey) / (ex * ex + ey * ey)).clamp(0.0, 1.0);
        nearest = nearest.min((x - ax - ex * t).hypot(y - ay - ey * t));
        if (ay > y) != (by > y) && x < ax + ex * (y - ay) / ey {
            inside = !inside;
        }
    }
    if inside { -nearest } else { nearest }
}

/// Is (x, y) inside the shape shrunk by `inset`? Hollow forms subtract an inset copy.
fn inside(shape: &[Polygon], x: f32, y: f32, inset: f32) -> bool {
    shape.iter().any(|polygon| distance(polygon, x, y) < -inset)
}

fn covered(shape: &[Polygon], stroke: f32, state: State, x: f32, y: f32) -> bool {
    let solid = inside(shape, x, y, 0.0);
    let body = match state {
        State::Running => solid,
        _ => solid && !inside(shape, x, y, stroke),
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
    let shape = shape(set);
    // Thin parts would close up with the common stroke.
    let stroke = if matches!(set, "bars" | "snowflake") {
        0.1
    } else {
        STROKE
    };
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
                    hits += covered(&shape, stroke, state, map(px, sx), map(py, sy)) as u32;
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
        let icons: Vec<_> = SETS.iter().map(|s| rgba(s, State::Running)).collect();
        for (i, a) in icons.iter().enumerate() {
            assert!(icons[i + 1..].iter().all(|b| a != b), "{}", SETS[i]);
        }
    }
}
