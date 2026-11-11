//! Compact always-on-top player window.
//!
//! Rendered in its own immediate viewport so it can stay above other windows
//! while the main window is behind them. It reads the same engine snapshot as
//! the player bar and returns [`Action`]s, which the app applies after the frame.

use eframe::egui::{self, RichText};

use crate::i18n::t;
use crate::models::Station;
use crate::player::{EngineSnapshot, PlaybackStatus};

use super::theme::Theme;
use super::{status_color, status_label, Action, TrackDisplay};

pub fn render(
    ctx: &egui::Context,
    theme: &Theme,
    engine: &EngineSnapshot,
    active_station: Option<&Station>,
    track: Option<&TrackDisplay>,
    selected: Option<uuid::Uuid>,
    volume: f32,
) -> Vec<Action> {
    let mut actions = Vec::new();
    egui::CentralPanel::default()
        .frame(
            egui::Frame::none()
                .fill(theme.bg_app)
                .inner_margin(egui::Margin::same(12.0)),
        )
        .show(ctx, |ui| {
            let now_playing = engine
                .active
                .as_ref()
                .and_then(|stream| {
                    stream
                        .display_now_playing
                        .as_deref()
                        .or(stream.now_playing.as_deref())
                })
                .filter(|title| !title.trim().is_empty());
            let catalog_title = track
                .map(|track| track.title.as_str())
                .filter(|title| !title.trim().is_empty());
            let title = catalog_title
                .or(now_playing)
                .or_else(|| active_station.map(|station| station.name.as_str()))
                .unwrap_or(t("player.nothing"));
            ui.horizontal(|ui| {
                if let Some(texture) = track.and_then(|track| track.artwork.as_ref()) {
                    ui.add(egui::Image::new(egui::load::SizedTexture::new(
                        texture.id(),
                        egui::vec2(48.0, 48.0),
                    )));
                }
                ui.vertical(|ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(title)
                                .size(15.0)
                                .strong()
                                .color(theme.text_primary),
                        )
                        .truncate(),
                    );
                    if let Some(detail) = track.and_then(|track| track.detail.as_deref()) {
                        ui.add(
                            egui::Label::new(
                                RichText::new(detail)
                                    .size(11.0)
                                    .color(theme.text_secondary),
                            )
                            .truncate(),
                        );
                    }
                });
            });

            ui.horizontal(|ui| {
                let status = engine
                    .active
                    .as_ref()
                    .map(|stream| stream.status)
                    .unwrap_or(PlaybackStatus::Stopped);
                let (dot, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                ui.painter()
                    .circle_filled(dot.center(), 4.0, status_color(theme, status));
                ui.label(
                    RichText::new(status_label(status))
                        .size(11.0)
                        .color(theme.text_secondary),
                );
                if let Some(station) = active_station {
                    ui.label(
                        RichText::new(&station.name)
                            .size(11.0)
                            .color(theme.text_secondary),
                    );
                }
            });

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let playing = engine.active.is_some();
                if ui
                    .add_enabled(!playing && selected.is_some(), egui::Button::new("▶"))
                    .on_hover_text(t("player.play_selected"))
                    .clicked()
                {
                    if let Some(id) = selected {
                        actions.push(Action::Play(id));
                    }
                }
                if ui
                    .add_enabled(playing, egui::Button::new("■"))
                    .on_hover_text(t("player.stop_all_tooltip"))
                    .clicked()
                {
                    actions.push(Action::StopAll);
                }
                let mut volume = volume;
                if ui
                    .add_sized(
                        [96.0, 18.0],
                        egui::Slider::new(&mut volume, 0.0..=1.0).show_value(false),
                    )
                    .changed()
                {
                    actions.push(Action::SetVolume(volume));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(t("mini.open")).clicked() {
                        let root = egui::ViewportId::ROOT;
                        ctx.send_viewport_cmd_to(root, egui::ViewportCommand::Minimized(false));
                        ctx.send_viewport_cmd_to(root, egui::ViewportCommand::Visible(true));
                        ctx.send_viewport_cmd_to(root, egui::ViewportCommand::Focus);
                    }
                });
            });
        });
    actions
}
