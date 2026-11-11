//! Colour tokens, typography and widget styling for the two themes.

use eframe::egui::{self, Color32, FontFamily, FontId, TextStyle};

use super::ThemeMode;

/// One token set. Dark is the default; light is built from the same names so a
/// widget never hard-codes a colour for only one theme.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub dark: bool,
    pub bg_app: Color32,
    pub bg_sidebar: Color32,
    pub bg_player: Color32,
    pub bg_row: Color32,
    pub bg_row_alt: Color32,
    pub bg_row_selected: Color32,
    pub bg_row_playing: Color32,
    pub border: Color32,
    pub text_primary: Color32,
    pub text_secondary: Color32,
    pub text_disabled: Color32,
    pub accent: Color32,
    pub accent_muted: Color32,
    pub danger: Color32,
    pub status_ok: Color32,
    pub status_warn: Color32,
}

impl Theme {
    pub fn for_mode(mode: ThemeMode) -> Self {
        match mode {
            ThemeMode::Dark => Self::dark(),
            ThemeMode::Light => Self::light(),
        }
    }

    pub fn dark() -> Self {
        Self {
            dark: true,
            bg_app: rgb(0x16, 0x18, 0x1C),
            bg_sidebar: rgb(0x1B, 0x1E, 0x23),
            bg_player: rgb(0x1F, 0x23, 0x2A),
            bg_row: rgb(0x16, 0x18, 0x1C),
            bg_row_alt: rgb(0x1A, 0x1D, 0x23),
            bg_row_selected: rgb(0x23, 0x28, 0x33),
            bg_row_playing: rgb(0x1E, 0x2A, 0x32),
            border: rgb(0x2A, 0x2F, 0x38),
            text_primary: rgb(0xE6, 0xE9, 0xEF),
            text_secondary: rgb(0x9A, 0xA3, 0xB2),
            text_disabled: rgb(0x5C, 0x64, 0x70),
            accent: rgb(0x3F, 0xB6, 0xA8),
            accent_muted: rgb(0x2A, 0x6F, 0x68),
            danger: rgb(0xE0, 0x6C, 0x75),
            status_ok: rgb(0x57, 0xC0, 0x8A),
            status_warn: rgb(0xD8, 0xA6, 0x57),
        }
    }

    pub fn light() -> Self {
        Self {
            dark: false,
            bg_app: rgb(0xF7, 0xF8, 0xFA),
            bg_sidebar: rgb(0xEE, 0xF0, 0xF4),
            bg_player: rgb(0xFF, 0xFF, 0xFF),
            bg_row: rgb(0xF7, 0xF8, 0xFA),
            bg_row_alt: rgb(0xF1, 0xF3, 0xF7),
            bg_row_selected: rgb(0xE3, 0xE8, 0xF0),
            bg_row_playing: rgb(0xE6, 0xF4, 0xF1),
            border: rgb(0xD8, 0xDD, 0xE5),
            text_primary: rgb(0x1B, 0x1F, 0x26),
            text_secondary: rgb(0x5C, 0x64, 0x70),
            text_disabled: rgb(0x8A, 0x92, 0x9E),
            accent: rgb(0x1F, 0x8A, 0x7C),
            accent_muted: rgb(0x9F, 0xD4, 0xCD),
            danger: rgb(0xC0, 0x39, 0x2B),
            status_ok: rgb(0x1F, 0x8A, 0x5C),
            status_warn: rgb(0xA2, 0x6B, 0x00),
        }
    }

    pub fn apply(&self, ctx: &egui::Context) {
        let mut visuals = if self.dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        };
        visuals.panel_fill = self.bg_app;
        visuals.window_fill = self.bg_player;
        visuals.extreme_bg_color = self.bg_sidebar;
        visuals.faint_bg_color = self.bg_row_alt;
        visuals.override_text_color = Some(self.text_primary);
        visuals.selection.bg_fill = self.accent_muted;
        visuals.selection.stroke = egui::Stroke::new(1.0, self.accent);
        visuals.widgets.noninteractive.bg_fill = self.bg_sidebar;
        visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, self.border);
        visuals.widgets.inactive.bg_fill = self.bg_row_alt;
        visuals.widgets.inactive.weak_bg_fill = self.bg_row_alt;
        visuals.widgets.hovered.bg_fill = self.bg_row_selected;
        visuals.widgets.hovered.weak_bg_fill = self.bg_row_selected;
        visuals.widgets.active.bg_fill = self.bg_row_selected;
        visuals.widgets.active.weak_bg_fill = self.bg_row_selected;
        ctx.set_visuals(visuals);

        let mut style = (*ctx.style()).clone();
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(8.0, 4.0);
        style.spacing.slider_width = 96.0;
        style.text_styles = [
            (
                TextStyle::Heading,
                FontId::new(16.0, FontFamily::Proportional),
            ),
            (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
            (
                TextStyle::Button,
                FontId::new(14.0, FontFamily::Proportional),
            ),
            (
                TextStyle::Small,
                FontId::new(12.0, FontFamily::Proportional),
            ),
            (
                TextStyle::Monospace,
                FontId::new(12.0, FontFamily::Monospace),
            ),
        ]
        .into();
        ctx.set_style(style);
    }
}

fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

/// Install the bundled OFL fonts. Falls back to egui's defaults only if the
/// embedded bytes cannot be parsed, which would be a build-time mistake.
pub fn install_fonts(ctx: &egui::Context, scripts: u32) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "inter".to_owned(),
        egui::FontData::from_static(include_bytes!("../../assets/fonts/Inter.ttf")),
    );
    fonts.font_data.insert(
        "jetbrains".to_owned(),
        egui::FontData::from_static(include_bytes!("../../assets/fonts/JetBrainsMono.ttf")),
    );
    fonts
        .families
        .get_mut(&FontFamily::Proportional)
        .expect("egui always defines proportional")
        .insert(0, "inter".to_owned());
    fonts
        .families
        .get_mut(&FontFamily::Monospace)
        .expect("egui always defines monospace")
        .insert(0, "jetbrains".to_owned());
    // Inter and JetBrains Mono have no CJK or other non-Latin glyphs; append
    // system faces so station names render instead of tofu.
    let fallbacks = crate::fonts::fallbacks(scripts);
    let mut added = 0;
    for (index, fallback) in fallbacks.into_iter().enumerate() {
        log::info!(
            "font fallback for {}: {} (face {})",
            fallback.script,
            fallback.path.display(),
            fallback.index
        );
        let key = format!("fallback-{index}");
        // epaint clones owned font bytes once per family. Leaking the loaded
        // face gives it a borrowed slice instead, so a 30 MB CJK collection is
        // not duplicated in memory. These fonts live as long as the process.
        let data: &'static [u8] = Box::leak(fallback.data.into_boxed_slice());
        fonts.font_data.insert(
            key.clone(),
            egui::FontData {
                font: std::borrow::Cow::Borrowed(data),
                index: fallback.index,
                tweak: Default::default(),
            },
        );
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            fonts
                .families
                .get_mut(&family)
                .expect("egui always defines both families")
                .push(key.clone());
        }
        added += 1;
    }
    if added == 0 {
        log::info!("no system script fonts found; non-Latin station names may show boxes");
    }
    ctx.set_fonts(fonts);
}
