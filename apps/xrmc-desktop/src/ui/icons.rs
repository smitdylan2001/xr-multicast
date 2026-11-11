//! Vector icons (the Electron app's `icons.js` set), drawn with the painter on a 24×24 grid.

use eframe::egui::{Color32, Painter, Pos2, Rect, Shape, Stroke, pos2};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Play,
    Stop,
    Radar,
    Plus,
    Grid,
    Help,
    Gear,
    Usb,
    Wifi,
    Check,
    Shield,
    Alert,
    More,
    Expand,
    Shrink,
    Sliders,
    Camera,
    X,
    Battery(u32),
    Bolt,
    Rotate,
    Refresh,
    Headset,
    Menu,
    Eye,
    Info,
}

struct Pen<'a> {
    p: &'a Painter,
    r: Rect,
    s: f32,
    color: Color32,
    stroke: Stroke,
}

impl Pen<'_> {
    fn pt(&self, x: f32, y: f32) -> Pos2 {
        pos2(self.r.min.x + x * self.s, self.r.min.y + y * self.s)
    }
    fn line(&self, pts: &[(f32, f32)]) {
        let v: Vec<Pos2> = pts.iter().map(|&(x, y)| self.pt(x, y)).collect();
        self.p.add(Shape::line(v, self.stroke));
        // Round caps / joins.
        for &(x, y) in [pts[0], pts[pts.len() - 1]].iter() {
            self.p
                .circle_filled(self.pt(x, y), self.stroke.width / 2.0, self.color);
        }
    }
    fn closed(&self, pts: &[(f32, f32)]) {
        let v: Vec<Pos2> = pts.iter().map(|&(x, y)| self.pt(x, y)).collect();
        self.p.add(Shape::closed_line(v, self.stroke));
    }
    fn fill(&self, pts: &[(f32, f32)]) {
        let v: Vec<Pos2> = pts.iter().map(|&(x, y)| self.pt(x, y)).collect();
        self.p
            .add(Shape::convex_polygon(v, self.color, Stroke::NONE));
    }
    fn circle(&self, x: f32, y: f32, r: f32) {
        self.p.circle_stroke(self.pt(x, y), r * self.s, self.stroke);
    }
    fn dot(&self, x: f32, y: f32, r: f32) {
        self.p.circle_filled(self.pt(x, y), r * self.s, self.color);
    }
    fn arc_pts(&self, cx: f32, cy: f32, r: f32, a0: f32, a1: f32) -> Vec<(f32, f32)> {
        let n = (((a1 - a0).abs() / 12.0).ceil() as usize).max(3);
        (0..=n)
            .map(|i| {
                let a = (a0 + (a1 - a0) * i as f32 / n as f32).to_radians();
                (cx + r * a.cos(), cy + r * a.sin())
            })
            .collect()
    }
    /// Arc in SVG orientation (y down, angles clockwise from +x), degrees.
    fn arc(&self, cx: f32, cy: f32, r: f32, a0: f32, a1: f32) {
        self.line(&self.arc_pts(cx, cy, r, a0, a1));
    }
    fn rrect(&self, x: f32, y: f32, w: f32, h: f32, r: f32) {
        let rect = Rect::from_min_max(self.pt(x, y), self.pt(x + w, y + h));
        self.p.rect_stroke(
            rect,
            r * self.s,
            self.stroke,
            eframe::egui::StrokeKind::Middle,
        );
    }
    fn rrect_fill(&self, x: f32, y: f32, w: f32, h: f32, r: f32) {
        let rect = Rect::from_min_max(self.pt(x, y), self.pt(x + w, y + h));
        self.p.rect_filled(rect, r * self.s, self.color);
    }
}

/// Draw `icon` into the square `rect` (centred if not square).
pub fn paint(p: &Painter, rect: Rect, icon: Icon, color: Color32) {
    let side = rect.width().min(rect.height());
    let r = Rect::from_center_size(rect.center(), eframe::egui::vec2(side, side));
    let s = side / 24.0;
    let pen = Pen {
        p,
        r,
        s,
        color,
        stroke: Stroke::new((2.0 * s).max(1.2), color),
    };
    match icon {
        Icon::Play => pen.fill(&[(6.0, 4.0), (20.0, 12.0), (6.0, 20.0)]),
        Icon::Stop => pen.rrect_fill(6.0, 6.0, 12.0, 12.0, 1.5),
        Icon::Radar => {
            pen.line(&[(12.0, 12.0), (19.0, 5.0)]);
            pen.arc(12.0, 12.0, 8.5, -17.0, 287.0);
            pen.arc(12.0, 12.0, 4.0, -11.0, 281.0);
        }
        Icon::Plus => {
            pen.line(&[(12.0, 5.0), (12.0, 19.0)]);
            pen.line(&[(5.0, 12.0), (19.0, 12.0)]);
        }
        Icon::Grid => {
            for (x, y) in [(3.0, 3.0), (14.0, 3.0), (3.0, 14.0), (14.0, 14.0)] {
                pen.rrect(x, y, 7.0, 7.0, 1.0);
            }
        }
        Icon::Help => {
            pen.circle(12.0, 12.0, 9.5);
            pen.line(&[
                (9.3, 9.4),
                (9.7, 7.9),
                (10.8, 7.0),
                (12.2, 6.8),
                (13.6, 7.2),
                (14.6, 8.3),
                (14.6, 9.7),
                (13.8, 10.8),
                (12.6, 11.6),
                (12.0, 12.6),
                (12.0, 13.8),
            ]);
            pen.dot(12.0, 17.2, 1.2);
        }
        Icon::Info => {
            pen.circle(12.0, 12.0, 9.5);
            pen.line(&[(12.0, 11.0), (12.0, 16.5)]);
            pen.dot(12.0, 7.8, 1.2);
        }
        Icon::Gear => {
            pen.circle(12.0, 12.0, 3.0);
            pen.circle(12.0, 12.0, 7.0);
            for i in 0..8 {
                let a = (i as f32 * 45.0 + 22.5).to_radians();
                let (c, sn) = (a.cos(), a.sin());
                pen.line(&[
                    (12.0 + 7.0 * c, 12.0 + 7.0 * sn),
                    (12.0 + 9.6 * c, 12.0 + 9.6 * sn),
                ]);
            }
        }
        Icon::Usb => {
            pen.line(&[(12.0, 3.0), (12.0, 16.5)]);
            pen.line(&[(9.0, 5.5), (12.0, 2.5), (15.0, 5.5)]);
            pen.circle(12.0, 19.0, 2.5);
            pen.line(&[(12.0, 12.0), (7.0, 9.5), (7.0, 7.5)]);
            pen.line(&[(12.0, 14.0), (17.0, 11.5), (17.0, 9.3)]);
            pen.rrect_fill(5.6, 5.0, 2.8, 2.4, 0.0);
            pen.dot(17.0, 8.0, 1.5);
        }
        Icon::Wifi => {
            pen.arc(12.0, 20.0, 14.0, -133.0, -47.0);
            pen.arc(12.0, 20.0, 9.5, -133.0, -47.0);
            pen.arc(12.0, 20.0, 5.0, -133.0, -47.0);
            pen.dot(12.0, 19.6, 1.3);
        }
        Icon::Check => pen.line(&[(20.0, 6.0), (9.0, 17.0), (4.0, 12.0)]),
        Icon::Shield => {
            pen.closed(&[
                (12.0, 22.0),
                (16.5, 19.2),
                (19.3, 15.6),
                (20.0, 12.0),
                (20.0, 5.0),
                (12.0, 2.0),
                (4.0, 5.0),
                (4.0, 12.0),
                (4.7, 15.6),
                (7.5, 19.2),
            ]);
            pen.line(&[(9.0, 12.0), (11.0, 14.0), (15.0, 10.0)]);
        }
        Icon::Alert => {
            pen.closed(&[(12.0, 3.2), (22.0, 20.5), (2.0, 20.5)]);
            pen.line(&[(12.0, 9.5), (12.0, 13.5)]);
            pen.dot(12.0, 17.0, 1.2);
        }
        Icon::More => {
            for x in [5.0, 12.0, 19.0] {
                pen.dot(x, 12.0, 1.9);
            }
        }
        Icon::Expand => {
            pen.line(&[(15.0, 3.0), (21.0, 3.0), (21.0, 9.0)]);
            pen.line(&[(9.0, 21.0), (3.0, 21.0), (3.0, 15.0)]);
            pen.line(&[(21.0, 3.0), (14.0, 10.0)]);
            pen.line(&[(3.0, 21.0), (10.0, 14.0)]);
        }
        Icon::Shrink => {
            pen.line(&[(14.0, 4.0), (14.0, 10.0), (20.0, 10.0)]);
            pen.line(&[(10.0, 20.0), (10.0, 14.0), (4.0, 14.0)]);
            pen.line(&[(14.0, 10.0), (21.0, 3.0)]);
            pen.line(&[(10.0, 14.0), (3.0, 21.0)]);
        }
        Icon::Sliders => {
            for (x, a, b, c, d) in [
                (4.0, 21.0, 14.0, 10.0, 3.0),
                (12.0, 21.0, 12.0, 8.0, 3.0),
                (20.0, 21.0, 16.0, 12.0, 3.0),
            ] {
                pen.line(&[(x, a), (x, b)]);
                pen.line(&[(x, c), (x, d)]);
            }
            pen.line(&[(1.0, 14.0), (7.0, 14.0)]);
            pen.line(&[(9.0, 8.0), (15.0, 8.0)]);
            pen.line(&[(17.0, 16.0), (23.0, 16.0)]);
        }
        Icon::Camera => {
            pen.closed(&[
                (3.0, 6.5),
                (7.0, 6.5),
                (9.0, 3.5),
                (15.0, 3.5),
                (17.0, 6.5),
                (21.0, 6.5),
                (22.5, 8.0),
                (22.5, 19.5),
                (21.0, 21.0),
                (3.0, 21.0),
                (1.5, 19.5),
                (1.5, 8.0),
            ]);
            pen.circle(12.0, 13.0, 4.0);
        }
        Icon::X => {
            pen.line(&[(18.0, 6.0), (6.0, 18.0)]);
            pen.line(&[(6.0, 6.0), (18.0, 18.0)]);
        }
        Icon::Battery(level) => {
            pen.rrect(2.0, 7.0, 17.0, 10.0, 2.0);
            pen.line(&[(22.0, 11.0), (22.0, 13.0)]);
            let w = 13.0 * (level.min(100) as f32 / 100.0);
            if w > 0.5 {
                pen.rrect_fill(4.0, 9.0, w, 6.0, 0.8);
            }
        }
        Icon::Bolt => {
            pen.fill(&[(13.0, 2.0), (4.0, 14.0), (11.0, 14.0), (12.0, 10.0)]);
            pen.fill(&[(12.0, 10.0), (11.0, 14.0), (10.0, 22.0), (19.0, 10.0)]);
        }
        Icon::Rotate => {
            pen.rrect(2.0, 10.0, 11.0, 11.0, 1.5);
            let mut pts = vec![(8.0, 4.0), (12.0, 4.0)];
            pts.extend(pen.arc_pts(12.0, 11.0, 7.0, -90.0, 0.0));
            pts.push((19.0, 13.5));
            pen.line(&pts);
            pen.line(&[(16.0, 11.0), (19.0, 14.0), (22.0, 11.0)]);
        }
        Icon::Refresh => {
            pen.arc(12.0, 12.0, 9.0, 0.0, 315.0);
            pen.line(&[(21.0, 3.0), (21.0, 9.0), (15.0, 9.0)]);
        }
        Icon::Headset => {
            pen.rrect(2.0, 7.0, 20.0, 11.0, 4.0);
            pen.circle(8.0, 12.5, 2.0);
            pen.circle(16.0, 12.5, 2.0);
        }
        Icon::Menu => {
            for y in [6.0, 12.0, 18.0] {
                pen.line(&[(3.0, y), (21.0, y)]);
            }
        }
        Icon::Eye => {
            let mut top = Vec::new();
            let mut bottom = Vec::new();
            for i in 0..=16 {
                let t = i as f32 / 16.0;
                let x = 1.0 + 22.0 * t;
                let dy = 7.0 * (1.0 - (2.0 * t - 1.0).powi(2));
                top.push((x, 12.0 - dy));
                bottom.push((x, 12.0 + dy));
            }
            pen.line(&top);
            pen.line(&bottom);
            pen.circle(12.0, 12.0, 3.0);
        }
    }
}

/// The XR MultiCast logo (headset outline with two lenses) in `rect`.
pub fn logo(p: &Painter, rect: Rect, color: Color32) {
    let side = rect.width().min(rect.height());
    let r = Rect::from_center_size(rect.center(), eframe::egui::vec2(side, side));
    let s = side / 32.0;
    let pt = |x: f32, y: f32| pos2(r.min.x + x * s, r.min.y + y * s);
    p.rect_stroke(
        Rect::from_min_max(pt(2.0, 9.0), pt(30.0, 24.0)),
        6.0 * s,
        Stroke::new(2.4 * s, color),
        eframe::egui::StrokeKind::Middle,
    );
    p.circle_filled(pt(10.5, 16.5), 3.2 * s, color);
    p.circle_filled(pt(21.5, 16.5), 3.2 * s, color);
}
