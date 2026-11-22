// SPDX-License-Identifier: Apache-2.0
//! The trim timeline: a track with the selected range, a playhead, and two handles
//! hanging below the track that you drag to set the start and end.

use eframe::egui::{self, Color32, CornerRadius, Pos2, Rect, Sense, Stroke, Vec2};

pub const MIN_LEN: f64 = 0.5; // seconds

const PAD: f32 = 14.0;
const TRACK_TOP: f32 = 12.0;
const TRACK_H: f32 = 26.0;
const HANDLE_W: f32 = 18.0;
const HANDLE_H: f32 = 26.0;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Drag {
    Start,
    End,
    Seek,
}

/// What the user did this frame.
#[derive(Default)]
pub struct TimelineEvent {
    /// A handle moved (Drag::Start / Drag::End) or the playhead was dragged (Drag::Seek).
    pub changed: Option<Drag>,
    pub seek_to: Option<f64>,
}

pub struct Timeline {
    pub duration: f64,
    pub start: f64,
    pub end: f64,
    drag: Option<Drag>,
}

impl Timeline {
    pub fn new(duration: f64) -> Self {
        Self {
            duration: duration.max(0.001),
            start: 0.0,
            end: duration,
            drag: None,
        }
    }

    pub fn height() -> f32 {
        TRACK_TOP + TRACK_H + HANDLE_H + 6.0
    }

    fn x(&self, rect: Rect, t: f64) -> f32 {
        rect.left() + PAD + (rect.width() - 2.0 * PAD) * (t / self.duration) as f32
    }

    fn t(&self, rect: Rect, x: f32) -> f64 {
        let f = ((x - rect.left() - PAD) / (rect.width() - 2.0 * PAD)).clamp(0.0, 1.0);
        f as f64 * self.duration
    }

    fn handle(&self, rect: Rect, t: f64) -> Rect {
        let x = self.x(rect, t);
        Rect::from_min_size(
            Pos2::new(x - HANDLE_W / 2.0, rect.top() + TRACK_TOP + TRACK_H),
            Vec2::new(HANDLE_W, HANDLE_H),
        )
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, playhead: f64) -> TimelineEvent {
        let mut ev = TimelineEvent::default();
        let (rect, resp) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), Self::height()),
            Sense::click_and_drag(),
        );
        let (hs, he) = (self.handle(rect, self.start), self.handle(rect, self.end));

        if let Some(pos) = resp.hover_pos()
            && (hs.expand(4.0).contains(pos) || he.expand(4.0).contains(pos)) {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
            }
        if (resp.drag_started() || resp.clicked())
            && let Some(pos) = resp.interact_pointer_pos() {
                let (in_s, in_e) = (hs.expand(6.0).contains(pos), he.expand(6.0).contains(pos));
                self.drag = Some(match (in_s, in_e) {
                    (true, true) => {
                        if pos.x < hs.center().x {
                            Drag::Start
                        } else {
                            Drag::End
                        }
                    }
                    (true, false) => Drag::Start,
                    (false, true) => Drag::End,
                    _ => Drag::Seek,
                });
            }
        if let (Some(drag), Some(pos)) = (self.drag, resp.interact_pointer_pos())
            && (resp.dragged() || resp.drag_started() || resp.clicked()) {
                let t = self.t(rect, pos.x);
                match drag {
                    Drag::Start => self.start = t.min(self.end - MIN_LEN).max(0.0),
                    Drag::End => self.end = t.max(self.start + MIN_LEN).min(self.duration),
                    Drag::Seek => ev.seek_to = Some(t),
                }
                ev.changed = Some(drag);
            }
        if resp.drag_stopped() || (resp.clicked() && !resp.dragged()) {
            self.drag = None;
        }

        // paint
        let p = ui.painter_at(rect);
        let visuals = ui.visuals();
        let accent = visuals.selection.bg_fill;
        let track = Rect::from_min_size(
            Pos2::new(rect.left() + PAD, rect.top() + TRACK_TOP),
            Vec2::new(rect.width() - 2.0 * PAD, TRACK_H),
        );
        p.rect_filled(
            track,
            CornerRadius::same(4),
            visuals.widgets.inactive.bg_fill,
        );
        let sel = Rect::from_x_y_ranges(
            self.x(rect, self.start)..=self.x(rect, self.end),
            track.y_range(),
        );
        p.rect_filled(sel, CornerRadius::ZERO, accent.gamma_multiply(0.75));
        p.rect_stroke(
            sel,
            CornerRadius::ZERO,
            Stroke::new(1.0, accent),
            egui::StrokeKind::Inside,
        );

        for t in [self.start, self.end] {
            let h = self.handle(rect, t);
            let cx = h.center().x;
            p.rect_filled(
                Rect::from_x_y_ranges(cx - 1.5..=cx + 1.5, track.y_range()),
                CornerRadius::ZERO,
                accent,
            );
            let shape = vec![
                Pos2::new(cx, h.top()),
                Pos2::new(h.right(), h.top() + 6.0),
                Pos2::new(h.right(), h.bottom()),
                Pos2::new(h.left(), h.bottom()),
                Pos2::new(h.left(), h.top() + 6.0),
            ];
            p.add(egui::Shape::convex_polygon(shape, accent, Stroke::NONE));
            let grip = visuals.selection.stroke.color;
            for dx in [-3.0, 0.0, 3.0] {
                p.line_segment(
                    [
                        Pos2::new(cx + dx, h.top() + 10.0),
                        Pos2::new(cx + dx, h.bottom() - 4.0),
                    ],
                    Stroke::new(1.0, grip),
                );
            }
        }

        let x = self.x(rect, playhead);
        p.rect_filled(
            Rect::from_x_y_ranges(x - 1.0..=x + 1.0, track.top() - 6.0..=track.bottom() + 6.0),
            CornerRadius::ZERO,
            Color32::WHITE,
        );
        p.circle_filled(Pos2::new(x, track.top() - 6.0), 5.0, Color32::WHITE);
        ev
    }
}
