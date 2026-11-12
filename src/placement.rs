//! The window's last size and position, kept per display so replugging a monitor brings it back.
use crate::settings::write_private;
use anyhow::Result;
use gpui::{Bounds, Pixels, Size, point, px, size};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Placement {
    /// The display's UUID, which survives replugging; its numeric ID may not.
    pub display: String,
    /// Logical points from the display's top left corner.
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// A missing or unreadable file means no placement: the window opens at its default.
pub fn load(path: &Path) -> Option<Placement> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

pub fn save(path: &Path, placement: &Placement) -> Result<()> {
    write_private(path, &serde_json::to_vec_pretty(placement)?)
}

/// `size`, at least `min`, wholly on a display of size `screen`: at `origin` moved just enough to
/// fit, or centered when there is none. A display smaller than `min` wins over `min`.
pub fn fit(
    origin: Option<(f32, f32)>,
    (width, height): (f32, f32),
    (min_width, min_height): (f32, f32),
    screen: Size<Pixels>,
) -> Bounds<Pixels> {
    let (screen_width, screen_height) = (f32::from(screen.width), f32::from(screen.height));
    let width = width.max(min_width).min(screen_width);
    let height = height.max(min_height).min(screen_height);
    let (x, y) = match origin {
        Some((x, y)) => (
            x.clamp(0.0, screen_width - width),
            y.clamp(0.0, screen_height - height),
        ),
        None => ((screen_width - width) / 2.0, (screen_height - height) / 2.0),
    };
    Bounds::new(point(px(x), px(y)), size(px(width), px(height)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: (f32, f32) = (480.0, 420.0);

    fn screen(width: f32, height: f32) -> Size<Pixels> {
        size(px(width), px(height))
    }

    fn parts(b: Bounds<Pixels>) -> [f32; 4] {
        [b.origin.x, b.origin.y, b.size.width, b.size.height].map(f32::from)
    }

    #[test]
    fn a_window_on_screen_stays_where_it_was() {
        let b = fit(
            Some((100.0, 50.0)),
            (640.0, 680.0),
            MIN,
            screen(1512.0, 982.0),
        );
        assert_eq!(parts(b), [100.0, 50.0, 640.0, 680.0]);
    }

    #[test]
    fn a_window_partly_off_screen_moves_just_inside() {
        let b = fit(
            Some((1300.0, -40.0)),
            (640.0, 680.0),
            MIN,
            screen(1512.0, 982.0),
        );
        assert_eq!(parts(b), [872.0, 0.0, 640.0, 680.0]);
    }

    #[test]
    fn a_window_from_a_larger_display_shrinks_to_fit() {
        let b = fit(
            Some((900.0, 300.0)),
            (1280.0, 1400.0),
            MIN,
            screen(1512.0, 982.0),
        );
        assert_eq!(parts(b), [232.0, 0.0, 1280.0, 982.0]);
    }

    #[test]
    fn without_an_origin_the_window_is_centered_and_at_least_the_minimum() {
        let b = fit(None, (300.0, 200.0), MIN, screen(1512.0, 982.0));
        assert_eq!(parts(b), [516.0, 281.0, 480.0, 420.0]);
    }

    #[test]
    fn placements_round_trip_and_a_bad_file_reads_as_none() {
        let dir = std::env::temp_dir().join(format!("xihomo-placement-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("window.json");
        assert_eq!(load(&path), None);
        let placement = Placement {
            display: "display-a".into(),
            x: 10.0,
            y: 20.0,
            width: 640.0,
            height: 680.0,
        };
        save(&path, &placement).unwrap();
        assert_eq!(load(&path), Some(placement));
        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(load(&path), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
