// SPDX-License-Identifier: Apache-2.0
//! The `ui` object plugin windows draw with: egui's immediate mode, with values passed
//! in and returned (`name = ui:text_edit(name)`), since Lua can't hold references.
//! A `ui` only lives for the call that received it.

use eframe::egui;
use mlua::{AnyUserData, Function, Lua, Table, UserDataMethods};

use crate::ui::CONTROL_HEIGHT;

/// Call `f` with `ui` wrapped as a Lua value that expires when `f` returns.
pub fn with_ui<R>(
    lua: &Lua,
    ui: &mut egui::Ui,
    f: impl FnOnce(AnyUserData) -> mlua::Result<R>,
) -> mlua::Result<R> {
    lua.scope(|s| f(s.create_any_userdata_ref_mut(ui)?))
}

/// Run a Lua function with a child `ui` (horizontal, grid, …).
fn nested(lua: &Lua, ui: &mut egui::Ui, f: &Function) -> mlua::Result<()> {
    with_ui(lua, ui, |u| f.call::<()>(u))
}

fn text_edit_opts(o: &Option<Table>) -> mlua::Result<(String, Option<f32>, bool, bool)> {
    let Some(o) = o else {
        return Ok((String::new(), None, false, false));
    };
    Ok((
        o.get::<Option<String>>("hint")?.unwrap_or_default(),
        o.get("width")?,
        o.get::<Option<bool>>("multiline")?.unwrap_or(false),
        o.get::<Option<bool>>("password")?.unwrap_or(false),
    ))
}

pub fn register(lua: &Lua) -> mlua::Result<()> {
    lua.register_userdata_type::<egui::Ui>(|r| {
        // text
        r.add_method_mut("label", |_, ui, t: String| {
            ui.label(t);
            Ok(())
        });
        r.add_method_mut("heading", |_, ui, t: String| {
            ui.heading(t);
            Ok(())
        });
        r.add_method_mut("strong", |_, ui, t: String| {
            ui.strong(t);
            Ok(())
        });
        r.add_method_mut("weak", |_, ui, t: String| {
            ui.weak(t);
            Ok(())
        });
        r.add_method_mut("monospace", |_, ui, t: String| {
            ui.monospace(t);
            Ok(())
        });
        r.add_method_mut("small", |_, ui, t: String| {
            ui.small(t);
            Ok(())
        });
        r.add_method_mut("error", |_, ui, t: String| {
            ui.colored_label(ui.visuals().error_fg_color, t);
            Ok(())
        });
        r.add_method_mut("warn", |_, ui, t: String| {
            ui.colored_label(ui.visuals().warn_fg_color, t);
            Ok(())
        });
        r.add_method_mut("colored", |_, ui, (t, c): (String, String)| {
            let color = super::host::color_of(&c)
                .ok_or_else(|| mlua::Error::runtime(format!("color {c:?}: use \"#rrggbb\"")))?;
            ui.colored_label(color, t);
            Ok(())
        });
        r.add_method_mut("hyperlink", |_, ui, (t, url): (String, Option<String>)| {
            match url {
                Some(u) => ui.hyperlink_to(t, u),
                None => ui.hyperlink(t),
            };
            Ok(())
        });

        // layout
        r.add_method_mut("separator", |_, ui, ()| {
            ui.separator();
            Ok(())
        });
        r.add_method_mut("space", |_, ui, px: Option<f32>| {
            // an empty box rather than add_space, which egui doesn't allow in a grid
            // (a debug-build panic); in a grid this is an empty cell
            let px = px.unwrap_or(8.0).max(0.0);
            ui.allocate_exact_size(egui::Vec2::splat(px), egui::Sense::hover());
            Ok(())
        });
        r.add_method_mut("horizontal", |lua, ui, f: Function| {
            ui.horizontal(|ui| nested(lua, ui, &f)).inner
        });
        r.add_method_mut("vertical", |lua, ui, f: Function| {
            ui.vertical(|ui| nested(lua, ui, &f)).inner
        });
        r.add_method_mut("enabled", |lua, ui, (on, f): (bool, Function)| {
            ui.add_enabled_ui(on, |ui| nested(lua, ui, &f)).inner
        });
        r.add_method_mut("collapsing", |lua, ui, (title, f): (String, Function)| {
            ui.collapsing(title, |ui| nested(lua, ui, &f))
                .body_returned
                .unwrap_or(Ok(()))
        });
        r.add_method_mut(
            "grid",
            |lua, ui, (id, cols, f): (String, Option<usize>, Function)| {
                let mut g = egui::Grid::new(("plugin-grid", id))
                    .striped(true)
                    .spacing([10.0, 6.0]);
                if let Some(c) = cols {
                    g = g.num_columns(c);
                }
                g.show(ui, |ui| nested(lua, ui, &f)).inner
            },
        );
        r.add_method_mut("end_row", |_, ui, ()| {
            ui.end_row();
            Ok(())
        });
        r.add_method_mut("scroll", |lua, ui, (f, max_h): (Function, Option<f32>)| {
            let mut s = egui::ScrollArea::vertical().auto_shrink([false, true]);
            if let Some(h) = max_h {
                s = s.max_height(h);
            }
            s.show(ui, |ui| nested(lua, ui, &f)).inner
        });
        r.add_method("available_width", |_, ui, ()| Ok(ui.available_width()));

        // widgets: each returns the new value
        r.add_method_mut("button", |_, ui, (t, tip): (String, Option<String>)| {
            let mut resp = ui.button(t);
            if let Some(tip) = tip {
                resp = resp.on_hover_text(tip);
            }
            Ok(resp.clicked())
        });
        r.add_method_mut("small_button", |_, ui, t: String| {
            Ok(ui.small_button(t).clicked())
        });
        r.add_method_mut("checkbox", |_, ui, (mut v, t): (bool, Option<String>)| {
            ui.checkbox(&mut v, t.unwrap_or_default());
            Ok(v)
        });
        r.add_method_mut(
            "text_edit",
            |_, ui, (v, opts): (Option<String>, Option<Table>)| {
                let mut v = v.unwrap_or_default();
                let (hint, width, multiline, password) = text_edit_opts(&opts)?;
                let edit = if multiline {
                    egui::TextEdit::multiline(&mut v)
                } else {
                    egui::TextEdit::singleline(&mut v).vertical_align(egui::Align::Center)
                }
                .hint_text(hint)
                .password(password);
                match width {
                    // Grid ignores desired_width; add_sized works everywhere
                    Some(w) if !multiline => ui.add_sized([w, CONTROL_HEIGHT], edit),
                    Some(w) => ui.add(edit.desired_width(w)),
                    None => ui.add(edit),
                };
                Ok(v)
            },
        );
        r.add_method_mut(
            "slider",
            |_, ui, (mut v, min, max, text): (f64, f64, f64, Option<String>)| {
                let mut s = egui::Slider::new(&mut v, min..=max);
                if let Some(t) = text {
                    s = s.text(t);
                }
                ui.add(s);
                Ok(v)
            },
        );
        r.add_method_mut(
            "drag_value",
            |_, ui, (mut v, speed, min, max): (f64, Option<f64>, Option<f64>, Option<f64>)| {
                let mut d = egui::DragValue::new(&mut v).speed(speed.unwrap_or(1.0));
                if min.is_some() || max.is_some() {
                    d = d.range(min.unwrap_or(f64::MIN)..=max.unwrap_or(f64::MAX));
                }
                ui.add(d);
                Ok(v)
            },
        );
        r.add_method_mut(
            "combo",
            |_, ui, (id, selected, options): (String, Option<String>, Vec<String>)| {
                let mut sel = selected.unwrap_or_default();
                egui::ComboBox::from_id_salt(("plugin-combo", id))
                    .selected_text(sel.clone())
                    .show_ui(ui, |ui| {
                        for o in &options {
                            ui.selectable_value(&mut sel, o.clone(), o.as_str());
                        }
                    });
                Ok(sel)
            },
        );
        r.add_method_mut("progress", |_, ui, (f, text): (f32, Option<String>)| {
            let mut p = egui::ProgressBar::new(f.clamp(0.0, 1.0));
            p = match text {
                Some(t) => p.text(t),
                None => p.show_percentage(),
            };
            ui.add(p);
            Ok(())
        });
        r.add_method_mut("spinner", |_, ui, ()| {
            ui.spinner();
            Ok(())
        });

        // misc
        r.add_method("copy", |_, ui, t: String| {
            ui.ctx().copy_text(t);
            Ok(())
        });
        r.add_method("repaint_after", |_, ui, secs: f64| {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_secs_f64(secs.max(0.0)));
            Ok(())
        });
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every widget, in and out of a grid, in one frame. Tests are debug builds, so egui's
    /// layout assertions (e.g. add_space in a grid) run too.
    #[test]
    fn draws_every_widget() {
        let lua = Lua::new();
        register(&lua).unwrap();
        let draw: Function = lua
            .load(
                r##"
                return function(ui)
                  local function all(ui)
                    ui:label("l"); ui:heading("h"); ui:strong("s"); ui:weak("w"); ui:small("s")
                    ui:monospace("m"); ui:error("e"); ui:warn("w"); ui:colored("c", "#ff0000")
                    ui:hyperlink("https://example.com"); ui:hyperlink("x", "https://example.com")
                    ui:separator(); ui:space(); ui:space(4)
                    ui:button("b", "tip"); ui:small_button("b")
                    ui:checkbox(true, "c"); ui:text_edit("t", { hint = "h", width = 80 })
                    ui:text_edit("t", { multiline = true, width = 80 }); ui:text_edit(nil)
                    ui:slider(1, 0, 2, "s"); ui:drag_value(1, 0.1, 0, 5)
                    ui:combo("c", "a", { "a", "b" }); ui:progress(0.5); ui:progress(0.5, "p")
                    ui:spinner(); ui:available_width()
                  end
                  all(ui)
                  ui:horizontal(all); ui:vertical(all); ui:enabled(false, all)
                  ui:collapsing("c", all); ui:scroll(all, 100)
                  ui:grid("g", 2, function(ui) all(ui); ui:end_row(); all(ui) end)
                end
                "##,
            )
            .eval()
            .unwrap();
        let ctx = egui::Context::default();
        for _ in 0..2 {
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    with_ui(&lua, ui, |u| draw.call::<()>(u)).unwrap();
                });
            });
        }
    }
}
