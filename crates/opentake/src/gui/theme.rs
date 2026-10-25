//! Visual theme: a dark, low-contrast palette in the spirit of Logic Pro.

use eframe::egui;
use egui::{Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle, vec2};

// Surfaces, from back to front.
pub const BG_DEEP: Color32 = Color32::from_rgb(22, 22, 24);
pub const BG_LANE: Color32 = Color32::from_rgb(30, 30, 33);
pub const BG_LANE_ALT: Color32 = Color32::from_rgb(34, 34, 37);
pub const BG_LANE_SELECTED: Color32 = Color32::from_rgb(42, 42, 47);
pub const BG_PANEL: Color32 = Color32::from_rgb(40, 40, 43);
pub const BG_HEADER: Color32 = Color32::from_rgb(46, 46, 50);
pub const BG_HEADER_SELECTED: Color32 = Color32::from_rgb(58, 58, 64);
pub const BG_CONTROL: Color32 = Color32::from_rgb(56, 56, 60);
pub const BG_CONTROL_HOVER: Color32 = Color32::from_rgb(68, 68, 73);
pub const LCD_BG: Color32 = Color32::from_rgb(18, 20, 22);
pub const LCD_TEXT: Color32 = Color32::from_rgb(232, 236, 240);
pub const LCD_DIM: Color32 = Color32::from_rgb(120, 128, 136);

pub const BORDER: Color32 = Color32::from_rgb(18, 18, 20);
pub const HAIRLINE: Color32 = Color32::from_rgb(62, 62, 67);
pub const GRID_BAR: Color32 = Color32::from_rgb(52, 52, 57);
pub const GRID_BEAT: Color32 = Color32::from_rgb(44, 44, 49);
pub const GRID_SUB: Color32 = Color32::from_rgb(37, 37, 41);

pub const TEXT: Color32 = Color32::from_rgb(218, 218, 222);
pub const TEXT_DIM: Color32 = Color32::from_rgb(140, 140, 148);
pub const TEXT_FAINT: Color32 = Color32::from_rgb(96, 96, 104);

pub const ACCENT: Color32 = Color32::from_rgb(64, 132, 255);
pub const PLAYHEAD: Color32 = Color32::from_rgb(245, 245, 245);

// Track header button colours (Logic: mute blue, solo yellow, record red, input orange).
pub const MUTE: Color32 = Color32::from_rgb(70, 150, 235);
pub const SOLO: Color32 = Color32::from_rgb(240, 196, 50);
/// Tempo markings (song tempo from an imported score).
pub const TEMPO: Color32 = Color32::from_rgb(255, 150, 70);
pub const RECORD: Color32 = Color32::from_rgb(232, 64, 58);
pub const INPUT: Color32 = Color32::from_rgb(240, 140, 50);
pub const PLAY: Color32 = Color32::from_rgb(80, 205, 120);

pub const METER_LOW: Color32 = Color32::from_rgb(70, 210, 110);
pub const METER_MID: Color32 = Color32::from_rgb(230, 205, 70);
pub const METER_HIGH: Color32 = Color32::from_rgb(240, 70, 60);

pub const TRACK_COLORS: [Color32; 8] = [
    Color32::from_rgb(73, 152, 238),  // blue
    Color32::from_rgb(78, 196, 140),  // green
    Color32::from_rgb(236, 168, 64),  // orange
    Color32::from_rgb(196, 110, 226), // purple
    Color32::from_rgb(232, 98, 110),  // red
    Color32::from_rgb(76, 200, 210),  // cyan
    Color32::from_rgb(222, 206, 80),  // yellow
    Color32::from_rgb(132, 136, 236), // indigo
];

pub fn track_color(i: usize) -> Color32 {
    TRACK_COLORS[i % TRACK_COLORS.len()]
}

/// Scale a colour's brightness (keeps alpha).
pub fn shade(c: Color32, f: f32) -> Color32 {
    let s = |v: u8| ((v as f32 * f).round().clamp(0.0, 255.0)) as u8;
    Color32::from_rgba_unmultiplied(s(c.r()), s(c.g()), s(c.b()), c.a())
}

pub fn with_alpha(c: Color32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    install_system_font(ctx);
    install_music_font(ctx);

    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Small, FontId::new(10.5, FontFamily::Proportional)),
            (TextStyle::Body, FontId::new(12.5, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(12.5, FontFamily::Proportional)),
            (TextStyle::Heading, FontId::new(15.0, FontFamily::Proportional)),
            (TextStyle::Monospace, FontId::new(12.0, FontFamily::Monospace)),
        ]
        .into();

        let s = &mut style.spacing;
        s.item_spacing = vec2(6.0, 5.0);
        s.button_padding = vec2(9.0, 4.0);
        s.interact_size = vec2(24.0, 22.0);
        s.slider_width = 120.0;
        s.combo_width = 140.0;
        s.window_margin = egui::Margin::same(12);
        s.menu_margin = egui::Margin::same(6);

        let v = &mut style.visuals;
        v.dark_mode = true;
        v.panel_fill = BG_PANEL;
        v.window_fill = Color32::from_rgb(44, 44, 48);
        v.extreme_bg_color = BG_DEEP;
        v.faint_bg_color = Color32::from_rgb(46, 46, 50);
        v.window_stroke = Stroke::new(1.0, HAIRLINE);
        v.window_corner_radius = CornerRadius::same(10);
        v.menu_corner_radius = CornerRadius::same(8);
        v.window_shadow = egui::Shadow { offset: [0, 8], blur: 28, spread: 0, color: Color32::from_black_alpha(110) };
        v.popup_shadow = egui::Shadow { offset: [0, 4], blur: 16, spread: 0, color: Color32::from_black_alpha(90) };
        v.selection.bg_fill = with_alpha(ACCENT, 170);
        v.selection.stroke = Stroke::new(1.0, Color32::WHITE);
        v.hyperlink_color = ACCENT;
        v.slider_trailing_fill = true;

        let radius = CornerRadius::same(5);
        let w = &mut v.widgets;
        w.noninteractive.bg_fill = BG_PANEL;
        w.noninteractive.weak_bg_fill = BG_PANEL;
        w.noninteractive.bg_stroke = Stroke::new(1.0, HAIRLINE);
        w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
        w.noninteractive.corner_radius = radius;

        w.inactive.bg_fill = BG_CONTROL;
        w.inactive.weak_bg_fill = BG_CONTROL;
        w.inactive.bg_stroke = Stroke::NONE;
        w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
        w.inactive.corner_radius = radius;

        w.hovered.bg_fill = BG_CONTROL_HOVER;
        w.hovered.weak_bg_fill = BG_CONTROL_HOVER;
        w.hovered.bg_stroke = Stroke::new(1.0, Color32::from_gray(90));
        w.hovered.fg_stroke = Stroke::new(1.0, Color32::WHITE);
        w.hovered.corner_radius = radius;
        w.hovered.expansion = 0.0;

        w.active.bg_fill = Color32::from_rgb(78, 78, 84);
        w.active.weak_bg_fill = Color32::from_rgb(78, 78, 84);
        w.active.bg_stroke = Stroke::new(1.0, Color32::from_gray(110));
        w.active.fg_stroke = Stroke::new(1.0, Color32::WHITE);
        w.active.corner_radius = radius;
        w.active.expansion = 0.0;

        w.open.bg_fill = BG_CONTROL_HOVER;
        w.open.weak_bg_fill = BG_CONTROL_HOVER;
        w.open.corner_radius = radius;
    });
}

/// Use the macOS system font (SF Pro) when available, like native apps do.
fn install_system_font(ctx: &egui::Context) {
    let candidates: &[(&str, FontFamily, &str)] = &[
        ("sf-pro", FontFamily::Proportional, "/System/Library/Fonts/SFNS.ttf"),
        ("sf-mono", FontFamily::Monospace, "/System/Library/Fonts/SFNSMono.ttf"),
    ];
    for (name, family, path) in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            ctx.add_font(egui::epaint::text::FontInsert::new(
                name,
                egui::FontData::from_owned(bytes),
                vec![egui::epaint::text::InsertFontFamily {
                    family: family.clone(),
                    priority: egui::epaint::text::FontPriority::Highest,
                }],
            ));
        }
    }
}

static MUSIC_FONT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A tempo marking: "♩ = 120" (or "120 BPM" without a music font).
pub fn tempo_label(bpm: f64) -> String {
    let n = if (bpm - bpm.round()).abs() < 0.05 { format!("{bpm:.0}") } else { format!("{bpm:.1}") };
    if has_music_font() { format!("\u{2669} = {n}") } else { format!("{n} BPM") }
}

/// Whether a font with music symbols (clefs) is installed as a fallback.
pub fn has_music_font() -> bool {
    MUSIC_FONT.load(std::sync::atomic::Ordering::Relaxed)
}

/// Apple Symbols has the clef glyphs (𝄞 𝄢); add it as the last fallback.
fn install_music_font(ctx: &egui::Context) {
    if let Ok(bytes) = std::fs::read("/System/Library/Fonts/Apple Symbols.ttf") {
        ctx.add_font(egui::epaint::text::FontInsert::new(
            "apple-symbols",
            egui::FontData::from_owned(bytes),
            vec![egui::epaint::text::InsertFontFamily { family: FontFamily::Proportional, priority: egui::epaint::text::FontPriority::Lowest }],
        ));
        MUSIC_FONT.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}
