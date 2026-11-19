#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Size {
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

pub fn bottom_center(area: Rect, size: Size, bottom_margin: f64) -> Point {
    Point { x: (area.x + (area.width - size.width) / 2.0).floor(), y: area.y + bottom_margin }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OVERLAY: Size = Size { width: 380.0, height: 90.0 };

    #[test]
    fn the_overlay_sits_centered_above_the_bottom_margin() {
        let screen = Rect { x: 0.0, y: 0.0, width: 1440.0, height: 900.0 };
        assert_eq!(bottom_center(screen, OVERLAY, 50.0), Point { x: 530.0, y: 50.0 });
    }

    #[test]
    fn the_margin_is_measured_from_the_area_left_free_by_the_dock() {
        let above_dock = Rect { x: 0.0, y: 70.0, width: 1440.0, height: 805.0 };
        assert_eq!(bottom_center(above_dock, OVERLAY, 50.0), Point { x: 530.0, y: 120.0 });
    }

    #[test]
    fn a_secondary_screen_keeps_its_own_origin() {
        let left_of_main = Rect { x: -1920.0, y: 180.0, width: 1920.0, height: 1080.0 };
        assert_eq!(bottom_center(left_of_main, OVERLAY, 50.0), Point { x: -1150.0, y: 230.0 });
    }

    #[test]
    fn an_odd_width_lands_on_a_whole_point() {
        let odd = Rect { x: 0.0, y: 0.0, width: 1367.0, height: 768.0 };
        assert_eq!(bottom_center(odd, OVERLAY, 50.0).x, 493.0);
    }
}
