//! Bundled menu bar icons, drawn in code as template images (black plus alpha). State is carried by
//! shape, not color: stopped is hollow, running is solid, error is hollow with a slash. Each set also
//! has an app icon for the Dock, and the bundle has a fixed "X".
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
/// Every app icon's glyph, the sets' and the bundle's "X", on white.
const INK: u32 = 0x212529;
const DEV_BAND: u32 = 0xFFD43B;
/// The "X" icon's rim, so its white body shows on white, as in Finder.
const RIM: u32 = 0xD0D4D9;
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

/// The letter "X": two bars cut level at the top and bottom, a little taller than wide.
fn x_distance(x: f32, y: f32) -> f32 {
    let angle = 63f32.to_radians();
    let bars = [angle, -angle].map(|a| distance(&bar(a, 1.2, 0.1), x, y));
    bars[0].min(bars[1]).max(y.abs() - 0.8)
}

/// An app icon on Apple's macOS grid, RGBA with straight alpha: in a 1024 canvas an 824 rounded
/// square with a transparent 100 margin, so macOS shows it as drawn: a graphite glyph on white. `None` is the "X" bundle icon;
/// `dev` adds a band across the lower-right corner.
pub fn app_icon(set: Option<&str>, dev: bool, size: u32) -> Vec<u8> {
    let glyph = set.map(shape);
    let rgb = |c: u32| [(c >> 16) as f32, (c >> 8 & 0xFF) as f32, (c & 0xFF) as f32];
    let (ink_color, band, rim) = (rgb(INK), rgb(DEV_BAND), rgb(RIM));
    // Canvas units per pixel, and shape units per canvas unit: the glyph is about 44% of the body.
    let pixel = 1024.0 / size as f32;
    let scale = 0.44 * 824.0 / (2.0 * EXTENT);
    let coverage = |d: f32| (0.5 - d / pixel).clamp(0.0, 1.0);
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let (u, v) = ((px as f32 + 0.5) * pixel, (py as f32 + 0.5) * pixel);
            // Signed distance to the rounded square: half side 412, corner radius 185.
            let (qx, qy) = ((u - 512.0).abs() - 227.0, (v - 512.0).abs() - 227.0);
            let edge = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - 185.0;
            let mut c = [255.0; 3];
            if dev {
                // Between two lines across the corner, at 45 degrees.
                let d = (1450.0 - (u + v)).max(u + v - 1590.0) / std::f32::consts::SQRT_2;
                let a = coverage(d);
                c = [0, 1, 2].map(|i| c[i] + (band[i] - c[i]) * a);
            }
            if set.is_none() {
                // A band 12 canvas units wide inside the body's edge.
                let a = coverage(edge) - coverage(edge + 12.0);
                c = [0, 1, 2].map(|i| c[i] + (rim[i] - c[i]) * a);
            }
            let (x, y) = ((u - 512.0) / scale, (v - 512.0) / scale);
            // Outside the glyph's frame nothing is near enough to cover a pixel.
            let ink = if x.abs() < 1.0 && y.abs() < 1.0 {
                let d = match &glyph {
                    Some(polygons) => polygons
                        .iter()
                        .map(|p| distance(p, x, y))
                        .fold(f32::MAX, f32::min),
                    None => x_distance(x, y),
                };
                coverage(d * scale)
            } else {
                0.0
            };
            c = [0, 1, 2].map(|i| c[i] + (ink_color[i] - c[i]) * ink);
            out.extend(c.map(|c| c.round() as u8));
            out.push((coverage(edge) * 255.0).round() as u8);
        }
    }
    out
}

/// A square RGBA image as PNG.
pub fn png(rgba: &[u8], size: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, size, size);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("writing to memory");
    writer
        .write_image_data(rgba)
        .expect("the buffer matches the size");
    writer.finish().expect("writing to memory");
    out
}

/// The "X" bundle icon for this build's channel, as the files `iconutil -c icns` expects, each drawn
/// at its own pixel size.
pub fn write_iconset(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    for points in [16, 32, 128, 256, 512] {
        for (scale, suffix) in [(1, ""), (2, "@2x")] {
            let size = points * scale;
            let png = png(&app_icon(None, cfg!(debug_assertions), size), size);
            std::fs::write(dir.join(format!("icon_{points}x{points}{suffix}.png")), png)?;
        }
    }
    Ok(())
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
    fn app_icons_are_distinct_shaped_and_white_in_the_middle() {
        const N: u32 = 64;
        let mut icons = vec![app_icon(None, false, N)];
        for set in SETS {
            icons.push(app_icon(Some(set), false, N));
        }
        icons.push(app_icon(None, true, N));
        icons.push(app_icon(Some("circle"), true, N));
        for (i, a) in icons.iter().enumerate() {
            assert!(icons[i + 1..].iter().all(|b| a != b), "icon {i} repeats");
            assert_eq!(a.len(), (N * N * 4) as usize);
            let at = |x: u32, y: u32| &a[((y * N + x) * 4) as usize..][..4];
            // Corners and the margin are transparent, the body white to its edge, the glyph dark.
            for (x, y) in [
                (0, 0),
                (N - 1, N - 1),
                (N - 4, 4),
                (2, N / 2),
                (N / 2, N - 3),
            ] {
                assert_eq!(at(x, y)[3], 0, "icon {i} at {x},{y}");
            }
            assert_eq!(at(N / 10 + 2, N / 2), [255, 255, 255, 255], "icon {i}");
            let center = at(N / 2, N / 2);
            assert!(
                center[3] == 255 && center[..3] != [255, 255, 255],
                "icon {i}"
            );
        }
    }

    #[test]
    fn png_round_trips() {
        let rgba = app_icon(Some("drop"), true, 48);
        let png = png(&rgba, 48);
        let mut reader = png::Decoder::new(std::io::Cursor::new(png))
            .read_info()
            .unwrap();
        let mut decoded = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut decoded).unwrap();
        assert_eq!((info.width, info.height), (48, 48));
        assert_eq!(decoded, rgba);
    }

    #[test]
    fn the_iconset_has_every_size_iconutil_expects() {
        let dir = std::env::temp_dir().join(format!("xihomo-test-{}-iconset", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_iconset(&dir).unwrap();
        let mut names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names.len(), 10, "{names:?}");
        for name in names {
            let (points, scale) = name
                .strip_prefix("icon_")
                .and_then(|n| n.split_once('x'))
                .map(|(p, rest)| (p.parse::<u32>().unwrap(), 1 + rest.contains("@2x") as u32))
                .unwrap();
            let file = std::fs::File::open(dir.join(&name)).unwrap();
            let info = png::Decoder::new(std::io::BufReader::new(file))
                .read_info()
                .unwrap();
            let size = (info.info().width, info.info().height);
            assert_eq!(size, (points * scale, points * scale), "{name}");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn sets_differ_from_each_other() {
        let icons: Vec<_> = SETS.iter().map(|s| rgba(s, State::Running)).collect();
        for (i, a) in icons.iter().enumerate() {
            assert!(icons[i + 1..].iter().all(|b| a != b), "{}", SETS[i]);
        }
    }
}
