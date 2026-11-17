//! Fullscreen media-centre view: large artwork, track and transport.

use eframe::egui::{self, Align2, Color32, FontId};

use crate::i18n::t;
use crate::models::Station;
use crate::player::{EngineSnapshot, PlaybackStatus};
use crate::station_playlist::StationPlaylist;

use super::theme::Theme;
use super::{ghost_button, primary_button, space, status_color, status_label, text, Action, TrackDisplay};

pub fn render(
    ctx: &egui::Context,
    theme: &Theme,
    engine: &EngineSnapshot,
    active_station: Option<&Station>,
    track: Option<&TrackDisplay>,
    playlist: Option<&StationPlaylist>,
    selected: Option<uuid::Uuid>,
    volume: f32,
    actions: &mut Vec<Action>,
) {
    egui::CentralPanel::default()
        .frame(
            egui::Frame::none()
                .fill(theme.bg_app)
                .inner_margin(egui::Margin::same(32.0)),
        )
        .show(ctx, |ui| {
            let active = engine.active.as_ref();
            let status = active.map(|stream| stream.status).unwrap_or(PlaybackStatus::Stopped);
            let now_playing = active
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
            let detail = track
                .and_then(|track| track.detail.as_deref())
                .filter(|detail| !detail.trim().is_empty());
            let station_name = active_station
                .map(|station| station.name.as_str())
                .unwrap_or(t("player.select_station"));

            // Top bar: station and the two actions that leave or explain it.
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(station_name)
                        .size(text::BODY)
                        .color(theme.text_secondary),
                );
                if let Some(station) = active_station {
                    if ui
                        .add(ghost_button(t("panel.info"), theme))
                        .clicked()
                    {
                        actions.push(Action::OpenStationInfo(station.id));
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(ghost_button(t("display.exit"), theme))
                        .clicked()
                    {
                        actions.push(Action::ToggleDisplayMode);
                    }
                });
            });

            // Scale with the screen so a TV or media-centre display fills it
            // instead of showing a small card in the corner.
            let scale = (ui.available_height() / 900.0).clamp(0.9, 1.5);
            let art = (300.0 * scale).min(ui.available_width() * 0.35);
            let recent_height = if playlist.is_some() { 230.0 } else { 0.0 };
            let top_space =
                ((ui.available_height() - recent_height - (art + 240.0)) * 0.35).max(space::L);
            ui.add_space(top_space);
            ui.vertical_centered(|ui| {
                if let Some(texture) = track.and_then(|track| track.artwork.as_ref()) {
                    ui.add(egui::Image::new(egui::load::SizedTexture::new(
                        texture.id(),
                        egui::vec2(art, art),
                    )));
                } else {
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(art, art), egui::Sense::hover());
                    ui.painter().rect_filled(rect, 18.0, theme.bg_sidebar);
                    let initials = active_station
                        .map(|station| initials(&station.name))
                        .unwrap_or_else(|| "VR".to_string());
                    ui.painter().text(
                        rect.center(),
                        Align2::CENTER_CENTER,
                        initials,
                        FontId::proportional(72.0 * scale),
                        theme.accent,
                    );
                }

                ui.add_space(space::L);
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(title)
                            .size((34.0 * scale).clamp(30.0, 52.0))
                            .strong()
                            .color(theme.text_primary),
                    )
                    .truncate(),
                );
                if let Some(detail) = detail {
                    ui.label(
                        egui::RichText::new(detail)
                            .size((18.0 * scale).clamp(16.0, 26.0))
                            .color(theme.text_secondary),
                    );
                }
                ui.add_space(space::XS);
                ui.horizontal(|ui| {
                    ui.add_space((ui.available_width() - 200.0 * scale).max(0.0) / 2.0);
                    status_pill(ui, theme, status);
                    if let Some(bitrate) = active_station.and_then(|station| station.bitrate) {
                        ui.label(
                            egui::RichText::new(format!("{bitrate} kbps"))
                                .size(text::CAPTION)
                                .color(theme.text_secondary),
                        );
                    }
                    if let Some(codec) = active_station.and_then(|station| station.codec.as_deref()) {
                        ui.label(
                            egui::RichText::new(codec)
                                .size(text::CAPTION)
                                .color(theme.text_secondary),
                        );
                    }
                });
                ui.add_space(space::L);

                let playing = active.is_some();
                let recording = active_station
                    .map(|station| station.id)
                    .map(|station_id| {
                        engine.streams.iter().any(|stream| {
                            stream.station_id == Some(station_id) && stream.recording
                        })
                    })
                    .unwrap_or(false);
                ui.horizontal(|ui| {
                    ui.add_space((ui.available_width() - 330.0 * scale).max(0.0) / 2.0);
                    if ui
                        .add_enabled(
                            selected.is_some(),
                            ghost_button("↻", theme).min_size(egui::vec2(44.0 * scale, 44.0 * scale)),
                        )
                        .on_hover_text(t("player.restart_selected"))
                        .clicked()
                    {
                        if let Some(id) = selected {
                            actions.push(Action::Restart(id));
                        }
                    }
                    if playing {
                        if ui
                            .add(
                                primary_button("■", theme)
                                    .min_size(egui::vec2(56.0 * scale, 56.0 * scale))
                                    .rounding(egui::Rounding::same(28.0 * scale)),
                            )
                            .on_hover_text(t("player.stop_all_tooltip"))
                            .clicked()
                        {
                            actions.push(Action::StopAll);
                        }
                    } else if ui
                        .add_enabled(
                            selected.is_some(),
                            primary_button("▶", theme)
                                .min_size(egui::vec2(56.0 * scale, 56.0 * scale))
                                .rounding(egui::Rounding::same(28.0 * scale)),
                        )
                        .on_hover_text(t("player.play_selected"))
                        .clicked()
                    {
                        if let Some(id) = selected {
                            actions.push(Action::Play(id));
                        }
                    }

                    let record_label = if recording { "● REC" } else { "REC" };
                    let record_button = egui::Button::new(
                        egui::RichText::new(record_label)
                            .size(text::SECTION)
                            .color(if recording {
                                Color32::WHITE
                            } else {
                                theme.text_secondary
                            }),
                    )
                    .fill(if recording {
                        theme.danger
                    } else {
                        Color32::TRANSPARENT
                    })
                    .stroke(egui::Stroke::new(
                        1.0,
                        if recording { theme.danger } else { theme.border },
                    ))
                    .rounding(12.0 * scale)
                    .min_size(egui::vec2(68.0 * scale, 32.0 * scale));
                    if ui
                        .add_enabled(playing, record_button)
                        .on_hover_text(if recording {
                            t("player.record_stop_tooltip")
                        } else {
                            t("player.record_tooltip")
                        })
                        .clicked()
                    {
                        actions.push(Action::ToggleRecord);
                    }

                    let mut volume = volume;
                    if ui
                        .add_sized(
                            [120.0 * scale, 24.0 * scale],
                            egui::Slider::new(&mut volume, 0.0..=1.0).show_value(false),
                        )
                        .changed()
                    {
                        actions.push(Action::SetVolume(volume));
                    }
                });
            });

            if let Some(playlist) = playlist {
                ui.add_space(space::L);
                ui.separator();
                ui.add_space(space::S);
                ui.label(
                    egui::RichText::new(t("display.recent"))
                        .size(text::SECTION)
                        .color(theme.text_secondary),
                );
                ui.add_space(space::XS);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .max_height(160.0)
                    .show(ui, |ui| {
                        for entry in playlist.entries.iter().take(12) {
                            ui.horizontal(|ui| {
                                if let Some(at) = &entry.at {
                                    ui.label(
                                        egui::RichText::new(display_time(at))
                                            .font(FontId::monospace(text::CAPTION))
                                            .color(theme.text_disabled),
                                    );
                                }
                                ui.label(
                                    egui::RichText::new(entry_text(entry))
                                        .size(text::CAPTION)
                                        .color(theme.text_primary),
                                );
                            });
                        }
                    });
            }
        });
}

fn status_pill(ui: &mut egui::Ui, theme: &Theme, status: PlaybackStatus) {
    egui::Frame::none()
        .fill(theme.bg_row_alt)
        .rounding(12.0)
        .inner_margin(egui::Margin::symmetric(10.0, 4.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (dot, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                ui.painter()
                    .circle_filled(dot.center(), 3.5, status_color(theme, status));
                ui.label(
                    egui::RichText::new(status_label(status))
                        .size(text::SECTION)
                        .color(status_color(theme, status)),
                );
            });
        });
}

fn initials(name: &str) -> String {
    name.split_whitespace()
        .filter_map(|word| word.chars().next())
        .take(2)
        .collect::<String>()
        .to_uppercase()
}

fn entry_text(entry: &crate::station_playlist::PlaylistEntry) -> String {
    if entry.artist.is_empty() {
        entry.title.clone()
    } else {
        format!("{} – {}", entry.artist, entry.title)
    }
}

fn display_time(at: &str) -> String {
    if let Some((_, time)) = at.split_once('T') {
        return time.chars().take(5).collect();
    }
    let parts: Vec<&str> = at.split_whitespace().collect();
    if parts.len() >= 2 && parts[1].contains(':') {
        return parts[1].chars().take(5).collect();
    }
    at.to_string()
}
