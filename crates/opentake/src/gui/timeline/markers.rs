//! The marker track (Logic's global Marker track), between the ruler and the tempo track:
//! coloured flags with names. Click one to move the playhead there, drag it to move it
//! (snapped), double-click to rename it in place (on empty space: a new marker), right-click
//! for colour, delete and "Set Cycle to Marker". Markers also show as faint lines through
//! the arrangement.

use opentake_engine::MarkerUpdate;
use opentake_engine::project::{MARKER_COLORS, Marker};

use super::*;

/// Height of the marker track.
pub(super) const MARKER_ROW_H: f32 = 18.0;
/// Colour of a marker without one of its own.
const MARKER_DEFAULT: Color32 = Color32::from_rgb(132, 140, 160);

/// A marker being dragged: which, and its time when grabbed.
#[derive(Clone, Copy)]
pub(super) struct MarkerDrag {
    id: u32,
    from: f64,
}

/// A marker being renamed in place.
pub(super) struct MarkerEdit {
    id: u32,
    text: String,
    focused: bool,
}

pub(super) fn color_of(m: &Marker) -> Color32 {
    m.color.map_or(MARKER_DEFAULT, |[r, g, b]| Color32::from_rgb(r, g, b))
}

/// Faint marker lines through an arrangement row.
pub(super) fn draw_marker_lines(painter: &Painter, rect: Rect, view: &View, project: &Project) {
    for m in &project.markers {
        let x = view.x_of(m.time_secs);
        if rect.x_range().contains(x) {
            painter.vline(x, rect.y_range(), Stroke::new(1.0, theme::with_alpha(color_of(m), 34)));
        }
    }
}

/// Header of the marker track: its name and a "+" that creates a marker at the playhead.
pub(super) fn marker_header(app: &mut DawApp, ui: &mut egui::Ui, row: Rect, snap: &Snapshot) {
    ui.painter().text(pos2(row.right() - 10.0, row.center().y), Align2::RIGHT_CENTER, "Marker", FontId::proportional(10.0), theme::TEXT_FAINT);
    let at = Rect::from_min_size(pos2(row.right() - 64.0, row.top() + 2.0), vec2(16.0, row.height() - 4.0));
    let mut child = ui.new_child(UiBuilder::new().id_salt("create_marker").max_rect(at));
    if widgets::icon_button(&mut child, Icon::Plus, false, theme::TEXT, "Create Marker (M)", at.size()).clicked() {
        app.run_marker_command(super::super::menu::MenuCommand::CreateMarker, snap);
    }
}

pub(super) fn marker_track(app: &mut DawApp, ui: &mut egui::Ui, painter: &Painter, rect: Rect, project: &Project, playhead: f64) {
    let p = painter.with_clip_rect(rect);
    let bg = theme::shade(theme::BG_HEADER, 0.95);
    p.rect_filled(rect, 0.0, bg);
    p.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, theme::BORDER));
    let markers = &project.markers;
    // A press anywhere else deselects the marker (so ⌫ goes back to regions and tracks).
    if ui.input(|i| i.pointer.any_pressed() && i.pointer.press_origin().is_some_and(|o| !rect.contains(o))) {
        app.view.marker_sel = None;
    }

    // Flags: labels placed like the tempo track's (sticky at the left, never overlapping;
    // the one playing wins).
    let font = FontId::proportional(10.5);
    let galleys: Vec<_> = markers.iter().map(|m| p.layout_no_wrap(m.name.clone(), font.clone(), theme::TEXT)).collect();
    let xs: Vec<f32> = markers.iter().map(|m| app.view.x_of(m.time_secs)).collect();
    let widths: Vec<(f32, f32)> = xs.iter().zip(&galleys).map(|(&x, g)| (x, g.size().x + 8.0)).collect();
    let current = project.marker_at(playhead).and_then(|m| project.marker_index(m.id));
    let placed = place_tempo_labels(rect.x_range(), &widths, current);
    let flag = |i: usize, left: f32| Rect::from_min_size(pos2(left - 3.0, rect.top() + 2.0), vec2(widths[i].1, rect.height() - 4.0));
    for (i, m) in markers.iter().enumerate() {
        if rect.x_range().contains(xs[i]) {
            p.vline(xs[i], rect.y_range(), Stroke::new(1.0, color_of(m)));
        }
    }
    for &(i, left) in &placed {
        let m = &markers[i];
        let selected = app.view.marker_sel == Some(m.id);
        let r = flag(i, left);
        let color = color_of(m);
        p.rect_filled(r, 2.0, theme::shade(color, if selected { 0.8 } else { 0.5 }));
        p.rect_filled(Rect::from_min_size(r.min, vec2(2.0, r.height())), 0.0, color);
        if selected {
            p.rect_stroke(r, 2.0, Stroke::new(1.0, theme::TEXT), StrokeKind::Inside);
        }
        p.galley(pos2(r.left() + 5.0, r.center().y - galleys[i].size().y / 2.0), galleys[i].clone(), theme::TEXT);
    }

    // The marker under screen x: its flag, else its line.
    let hit = |x: f32| -> Option<usize> {
        placed.iter().rev().find(|&&(i, left)| flag(i, left).x_range().contains(x)).map(|&(i, _)| i).or_else(|| {
            let near = (0..markers.len()).filter(|&i| (xs[i] - x).abs() <= 5.0);
            near.min_by(|&a, &b| (xs[a] - x).abs().total_cmp(&(xs[b] - x).abs()))
        })
    };

    let resp = ui.interact(rect, ui.make_persistent_id("marker_track"), Sense::click_and_drag());
    widgets::label(&resp, egui::Role::Group, "Marker Track");
    if let Some(pos) = resp.hover_pos() {
        if app.view.marker_drag.is_some() {
            ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
        } else if hit(pos.x).is_some() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
    }
    let resp = resp.on_hover_text("Markers: click one to go there, drag to move it, double-click to rename (or to add one), right-click for more");

    if resp.drag_started() {
        if let Some(i) = ui.input(|i| i.pointer.press_origin()).and_then(|o| hit(o.x)) {
            app.view.marker_drag = Some(MarkerDrag { id: markers[i].id, from: markers[i].time_secs });
            app.view.marker_sel = Some(markers[i].id);
        }
    }
    if resp.dragged() {
        if let Some(d) = app.view.marker_drag {
            let dx = ui.input(|i| i.pointer.press_origin().zip(i.pointer.latest_pos()).map(|(a, b)| b.x - a.x)).unwrap_or(0.0);
            let t = snap_time(app, ui, d.from + (dx / app.view.px_per_sec) as f64, project).max(0.0);
            let now = project.marker_index(d.id).map(|i| markers[i].time_secs);
            if now.is_some_and(|now| (now - t).abs() > 1e-9) {
                let update = MarkerUpdate { time_secs: Some(t), ..Default::default() };
                app.act(Action::UpdateMarker { id: d.id, update, merge: Some(format!("marker-move:{}", d.id)) });
            }
        }
    }
    if resp.drag_stopped() {
        app.view.marker_drag = None;
    }
    if resp.clicked() {
        let i = resp.interact_pointer_pos().and_then(|pos| hit(pos.x));
        app.view.marker_sel = i.map(|i| markers[i].id);
        if let Some(i) = i {
            app.act(Action::Seek(markers[i].time_secs));
        }
    }
    if resp.double_clicked() {
        if let Some(pos) = resp.interact_pointer_pos() {
            match hit(pos.x) {
                Some(i) => app.view.marker_edit = Some(MarkerEdit { id: markers[i].id, text: markers[i].name.clone(), focused: false }),
                None => {
                    if let Some(t) = app.view.time_at(pos.x) {
                        app.act(Action::AddMarker { time: snap_time(app, ui, t, project), name: None });
                    }
                }
            }
        }
    }
    if resp.secondary_clicked() {
        if let Some(pos) = resp.interact_pointer_pos() {
            app.view.marker_context = (app.view.time_at(pos.x).unwrap_or(0.0), hit(pos.x).map(|i| markers[i].id));
            app.view.marker_sel = app.view.marker_context.1;
        }
    }
    resp.context_menu(|ui| marker_menu(app, ui, project));

    // Renaming in place: a text field over the flag; Enter (or clicking away) keeps it,
    // Escape cancels.
    let Some(edit) = app.view.marker_edit.as_mut() else { return };
    let Some(i) = project.marker_index(edit.id) else {
        app.view.marker_edit = None;
        return;
    };
    let left = placed.iter().find(|p| p.0 == i).map_or(xs[i] + 3.0, |p| p.1);
    let field = Rect::from_min_size(pos2(left - 3.0, rect.top() + 1.0), vec2(150.0, rect.height() - 2.0));
    let mut child = ui.new_child(UiBuilder::new().id_salt("marker_rename").max_rect(field));
    let te = egui::TextEdit::singleline(&mut edit.text).font(FontId::proportional(10.5)).desired_width(field.width() - 8.0).margin(vec2(4.0, 1.0));
    let r = child.add(te);
    widgets::label(&r, egui::Role::TextInput, "Marker Name");
    if !edit.focused {
        // Focused with the name selected, so typing replaces it.
        r.request_focus();
        if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), r.id) {
            let all = egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(edit.text.chars().count()));
            state.cursor.set_char_range(Some(all));
            state.store(ui.ctx(), r.id);
        }
        edit.focused = true;
    } else if r.lost_focus() {
        let (id, text) = (edit.id, edit.text.trim().to_string());
        app.view.marker_edit = None;
        if !ui.input(|i| i.key_pressed(egui::Key::Escape)) && !text.is_empty() && text != markers[i].name {
            app.act(Action::UpdateMarker { id, update: MarkerUpdate { name: Some(text), ..Default::default() }, merge: None });
        }
    }
}

/// Right-click menu: for the marker clicked (rename, colour, delete, cycle its section), or
/// on empty space, a new marker there.
fn marker_menu(app: &mut DawApp, ui: &mut egui::Ui, project: &Project) {
    let (t, id) = app.view.marker_context;
    let Some(m) = id.and_then(|id| project.marker_index(id)).map(|i| project.markers[i].clone()) else {
        if ui.button("Create Marker Here").clicked() {
            app.act(Action::AddMarker { time: snap_time(app, ui, t, project), name: None });
            ui.close();
        }
        return;
    };
    widgets::section(ui, &m.name);
    if ui.button("Rename…").clicked() {
        app.view.marker_edit = Some(MarkerEdit { id: m.id, text: m.name.clone(), focused: false });
        ui.close();
    }
    ui.menu_button("Color", |ui| {
        let swatch = |c: Color32, name: &str| RichText::new(format!("●  {name}")).color(c);
        let choose = |color: Option<[u8; 3]>, app: &mut DawApp| {
            app.act(Action::UpdateMarker { id: m.id, update: MarkerUpdate { color: Some(color), ..Default::default() }, merge: None });
        };
        if ui.add(egui::Button::new(swatch(MARKER_DEFAULT, "Default")).selected(m.color.is_none())).clicked() {
            choose(None, app);
            ui.close();
        }
        for (name, rgb) in MARKER_COLORS {
            let [r, g, b] = rgb;
            let mut label = name.to_string();
            label[..1].make_ascii_uppercase();
            if ui.add(egui::Button::new(swatch(Color32::from_rgb(r, g, b), &label)).selected(m.color == Some(rgb))).clicked() {
                choose(Some(rgb), app);
                ui.close();
            }
        }
    });
    if ui.button("Set Cycle to Marker").on_hover_text("Cycle from this marker to the next one").clicked() {
        app.act(Action::CycleMarker(m.id));
        ui.close();
    }
    ui.separator();
    if ui.button("Delete Marker").clicked() {
        app.act(Action::RemoveMarker(m.id));
        ui.close();
    }
}
