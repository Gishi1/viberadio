//! Player bar and the per-stream list.

use eframe::egui::{self, Align2, Color32, FontId, Sense};

use crate::i18n::{t, tf};
use crate::models::Station;
use crate::player::{EngineSnapshot, PlaybackStatus};

use super::theme::Theme;
use super::{
    ghost_button, paint_clipped_text, space, status_color, text, Action, StreamRow, TrackDisplay,
    UiState,
};

/// Height of the per-stream panel above the player bar.
pub fn streams_height(rows: usize) -> f32 {
    40.0 + (rows.max(1) as f32) * 30.0
}

pub fn render_bar(
    ui: &mut egui::Ui,
    theme: &Theme,
    state: &mut UiState,
    engine: &EngineSnapshot,
    active_station: Option<&Station>,
    track: Option<&TrackDisplay>,
    selected: Option<uuid::Uuid>,
    volume: f32,
    sleep_remaining: Option<std::time::Duration>,
    next_schedule: Option<(&str, &str)>,
) -> Vec<Action> {
    let mut actions = Vec::new();
    let top_left = ui.min_rect().left_top();
    ui.painter().line_segment(
        [top_left, egui::pos2(ui.min_rect().right(), top_left.y)],
        egui::Stroke::new(1.0, theme.border),
    );
    ui.add_space(8.0);
    ui.set_min_height(super::PLAYER_HEIGHT - 16.0);

    ui.horizontal(|ui| {
        // Station icon block.
        let (icon_rect, _) = ui.allocate_exact_size(egui::vec2(48.0, 48.0), Sense::hover());
        let fill = if active_station.is_some() {
            theme.accent_muted
        } else {
            theme.bg_row_alt
        };
        ui.painter().rect_filled(icon_rect, 6.0, fill);
        if let Some(texture) = track.and_then(|track| track.artwork.as_ref()) {
            ui.painter().image(
                texture.id(),
                icon_rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        } else {
            let initials = active_station
                .map(|station| initials(&station.name))
                .unwrap_or_else(|| "VR".to_string());
            ui.painter().text(
                icon_rect.center(),
                Align2::CENTER_CENTER,
                initials,
                FontId::proportional(18.0),
                theme.text_primary,
            );
        }

        ui.add_space(4.0);
        ui.vertical(|ui| {
            // Leave room for the status pill, the bitrate pill and the
            // transport group so they cannot overlap the title.
            let reserve = 780.0;
            ui.set_max_width((ui.available_width() - reserve).max(80.0));
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
            ui.add(
                egui::Label::new(
                    egui::RichText::new(title)
                        .size(text::HEADING)
                        .strong()
                        .color(if active_station.is_some() {
                            theme.text_primary
                        } else {
                            theme.text_secondary
                        }),
                )
                .truncate(),
            );
            let detail = if let Some(track) = track {
                let mut parts = Vec::new();
                if let Some(detail) = track
                    .detail
                    .as_deref()
                    .filter(|detail| !detail.trim().is_empty())
                {
                    parts.push(detail.to_string());
                }
                if let Some(station) = active_station {
                    parts.push(station.name.clone());
                }
                if parts.is_empty() {
                    t("player.internet_radio").to_string()
                } else {
                    parts.join(" · ")
                }
            } else {
                active_station
                    .map(|station| {
                        let mut parts = Vec::new();
                        if now_playing.is_some() {
                            parts.push(station.name.clone());
                        }
                        if let Some(genre) = &station.genre {
                            parts.push(genre.clone());
                        }
                        if let Some(country) = &station.country {
                            parts.push(country.clone());
                        }
                        if parts.is_empty() {
                            t("player.internet_radio").to_string()
                        } else {
                            parts.join(" · ")
                        }
                    })
                    .unwrap_or_else(|| t("player.select_station").to_string())
            };
            ui.add(
                egui::Label::new(
                    egui::RichText::new(detail)
                        .size(text::CAPTION)
                        .color(theme.text_secondary),
                )
                .truncate(),
            );
        });

        ui.add_space(16.0);
        let status = engine
            .active
            .as_ref()
            .map(|stream| stream.status)
            .unwrap_or(PlaybackStatus::Stopped);
        let status_text = engine
            .active
            .as_ref()
            .map(|stream| match stream.status {
                PlaybackStatus::Error => truncate_chars(
                    stream
                        .error
                        .as_deref()
                        .unwrap_or_else(|| t("player.error")),
                    48,
                ),
                status => super::status_label(status).to_string(),
            })
            .unwrap_or_else(|| t("player.idle").to_string());
        egui::Frame::none()
            .fill(theme.bg_row_alt)
            .rounding(10.0)
            .inner_margin(egui::Margin::symmetric(8.0, 3.0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let (dot_rect, _) =
                        ui.allocate_exact_size(egui::vec2(10.0, 10.0), Sense::hover());
                    paint_status_dot(ui.painter(), dot_rect.center(), theme, status);
                    ui.label(
                        egui::RichText::new(&status_text)
                            .size(text::SECTION)
                            .color(status_color(theme, status)),
                    );
                });
            });

        if let Some(bitrate) = active_station.and_then(|station| station.bitrate) {
            ui.add_space(4.0);
            egui::Frame::none()
                .fill(theme.bg_row_alt)
                .rounding(10.0)
                .inner_margin(egui::Margin::symmetric(8.0, 3.0))
                .show(ui, |ui| {
                    ui.label(
                        egui::RichText::new(format!("{bitrate} kbps"))
                            .font(FontId::monospace(text::SECTION))
                            .color(theme.text_secondary),
                    );
                });
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // Volume: painted speaker + slider.
            let mut volume = volume;
            let slider = egui::Slider::new(&mut volume, 0.0..=1.0)
                .show_value(false)
                .trailing_fill(true);
            if ui.add_sized([96.0, 20.0], slider).changed() {
                actions.push(Action::SetVolume(volume));
            }
            let (speaker_rect, _) =
                ui.allocate_exact_size(egui::vec2(18.0, 18.0), Sense::hover());
            paint_speaker(ui.painter(), speaker_rect, theme.text_secondary);

            ui.add_space(space::S);
            ui.separator();

            let sleep_label = sleep_remaining
                .map(|remaining| {
                    tf(
                        "player.sleep_minutes",
                        &[&remaining.as_secs().div_ceil(60).max(1).to_string()],
                    )
                })
                .unwrap_or_else(|| t("player.sleep").to_string());
            if ui
                .add(ghost_button(sleep_label, theme))
                .on_hover_text(t("player.sleep_tooltip"))
                .clicked()
            {
                actions.push(Action::CycleSleep);
            }

            if let Some((label, tooltip)) = next_schedule {
                if ui
                    .add(ghost_button(label, theme))
                    .on_hover_text(tooltip)
                    .clicked()
                {
                    actions.push(Action::OpenSchedules);
                }
            }

            let streams_label = tf(
                "player.streams_count",
                &[t("player.streams"), &engine.streams.len().to_string()],
            );
            if ui
                .selectable_label(state.show_streams, streams_label)
                .on_hover_text(t("player.show_streams"))
                .clicked()
            {
                state.show_streams = !state.show_streams;
            }
            if ui
                .add(ghost_button("⧉", theme))
                .on_hover_text(t("player.mini"))
                .clicked()
            {
                actions.push(Action::ToggleMini);
            }
            if ui
                .add(ghost_button("⛶", theme))
                .on_hover_text(t("player.display"))
                .clicked()
            {
                actions.push(Action::ToggleDisplayMode);
            }

            ui.add_space(space::S);
            ui.separator();

            // Transport: restart, play (primary), stop, record.
            let refresh = ui
                .add_enabled(
                    selected.is_some(),
                    ghost_button("↻", theme).min_size(egui::vec2(36.0, 36.0)),
                )
                .on_hover_text(t("player.restart_selected"));
            if refresh.clicked() {
                if let Some(id) = selected {
                    actions.push(Action::Restart(id));
                }
            }

            let play = ui
                .add_enabled(
                    selected.is_some(),
                    egui::Button::new(
                        egui::RichText::new("▶").size(18.0).color(theme.bg_app),
                    )
                    .fill(theme.accent)
                    .rounding(egui::Rounding::same(20.0))
                    .min_size(egui::vec2(40.0, 40.0)),
                )
                .on_hover_text(t("player.play_selected"));
            if play.clicked() {
                if let Some(id) = selected {
                    actions.push(Action::Play(id));
                }
            }

            let stop = ui
                .add_enabled(
                    engine.active.is_some(),
                    ghost_button("■", theme).min_size(egui::vec2(36.0, 36.0)),
                )
                .on_hover_text(t("player.stop_all_tooltip"));
            if stop.clicked() {
                actions.push(Action::StopAll);
            }

            let recording = engine
                .active
                .as_ref()
                .and_then(|stream| stream.station_id)
                .map(|station_id| {
                    engine
                        .streams
                        .iter()
                        .any(|stream| stream.station_id == Some(station_id) && stream.recording)
                })
                .unwrap_or(false);
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
            .rounding(10.0)
            .min_size(egui::vec2(58.0, 26.0));
            let record = ui
                .add_enabled(engine.active.is_some(), record_button)
                .on_hover_text(if recording {
                    t("player.record_stop_tooltip")
                } else {
                    t("player.record_tooltip")
                });
            if record.clicked() {
                actions.push(Action::ToggleRecord);
            }
        });
    });

    actions
}

pub fn render_streams(ui: &mut egui::Ui, theme: &Theme, rows: &[StreamRow]) -> Vec<Action> {
    let mut actions = Vec::new();
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(t("player.active_streams"))
                .size(12.0)
                .color(theme.text_secondary),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if !rows.is_empty() && ui.small_button(t("player.stop_all")).clicked() {
                actions.push(Action::StopAll);
            }
        });
    });

    if rows.is_empty() {
        ui.label(
            egui::RichText::new(t("player.no_streams"))
                .size(12.0)
                .color(theme.text_disabled),
        );
        return actions;
    }

    egui::ScrollArea::vertical()
        .id_salt("stream-list-rows")
        .max_height(ui.available_height())
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for row in rows {
                ui.horizontal(|ui| {
                    let (dot, _) = ui.allocate_exact_size(egui::vec2(16.0, 18.0), Sense::hover());
                    ui.painter()
                        .circle_filled(dot.center(), 4.0, status_color(theme, row.status));
                    let (name_rect, name) =
                        ui.allocate_exact_size(egui::vec2(170.0, 18.0), Sense::click());
                    if row.active {
                        ui.painter().rect_filled(name_rect, 4.0, theme.accent_muted);
                    } else if name.hovered() {
                        ui.painter()
                            .rect_filled(name_rect, 4.0, theme.bg_row_selected);
                    }
                    paint_clipped_text(
                        ui.painter(),
                        name_rect.shrink2(egui::vec2(6.0, 0.0)),
                        egui::pos2(name_rect.left() + 6.0, name_rect.center().y),
                        Align2::LEFT_CENTER,
                        &row.name,
                        FontId::proportional(12.5),
                        if row.active {
                            theme.accent
                        } else {
                            theme.text_primary
                        },
                    );
                    if !row.record_only
                        && name.on_hover_text(t("player.activate_stream")).clicked()
                    {
                        actions.push(Action::ActivateStream(row.id));
                    }
                    if row.recording {
                        ui.label(egui::RichText::new("REC").size(10.0).color(theme.accent));
                    }
                    ui.allocate_ui_with_layout(
                        egui::vec2(300.0, 18.0),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(&row.status_text)
                                        .size(12.0)
                                        .color(theme.text_secondary),
                                )
                                .truncate(),
                            );
                        },
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if row.watching {
                            // The app recreates a watched station's background
                            // stream, so a stop button here would do nothing.
                            ui.label(
                                egui::RichText::new(t("player.watching"))
                                    .size(11.0)
                                    .color(theme.text_disabled),
                            );
                        } else if ui
                            .small_button("■")
                            .on_hover_text(t("player.stop_stream"))
                            .clicked()
                        {
                            actions.push(Action::StopStream(row.id));
                        }
                        if !row.record_only {
                            let mut volume = row.volume;
                            if ui
                                .add_sized(
                                    [72.0, 16.0],
                                    egui::Slider::new(&mut volume, 0.0..=1.0).show_value(false),
                                )
                                .changed()
                            {
                                actions.push(Action::SetStreamVolume(row.id, volume));
                            }
                        }
                    });
                });
            }
        });
    actions
}

/// One status pill dot whose shape also changes, so colour is not the only cue.
fn paint_status_dot(
    painter: &egui::Painter,
    center: egui::Pos2,
    theme: &Theme,
    status: PlaybackStatus,
) {
    let color = status_color(theme, status);
    match status {
        PlaybackStatus::Live => {
            painter.circle_filled(center, 3.5, color);
        }
        PlaybackStatus::Connecting | PlaybackStatus::Buffering => {
            painter.circle_stroke(center, 3.0, egui::Stroke::new(1.5, color));
        }
        PlaybackStatus::Stalled => {
            let rect = egui::Rect::from_center_size(center, egui::vec2(6.0, 6.0));
            painter.rect_filled(rect, 1.0, color);
        }
        PlaybackStatus::Error => {
            let points = vec![
                center + egui::vec2(0.0, -4.0),
                center + egui::vec2(4.0, 3.0),
                center + egui::vec2(-4.0, 3.0),
            ];
            painter.add(egui::Shape::convex_polygon(
                points,
                color,
                egui::Stroke::NONE,
            ));
        }
        PlaybackStatus::Stopped => {
            painter.circle_stroke(center, 3.0, egui::Stroke::new(1.0, color));
        }
    }
}

/// A painted speaker so the volume control does not depend on a font glyph.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn paint_speaker(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let center = rect.center();
    let body = egui::Rect::from_center_size(
        center + egui::vec2(-3.0, 0.0),
        egui::vec2(6.0, 7.0),
    );
    painter.rect_filled(body, egui::Rounding::same(1.0), color);
    let cone = vec![
        center + egui::vec2(1.0, -3.0),
        center + egui::vec2(6.0, -6.0),
        center + egui::vec2(6.0, 6.0),
        center + egui::vec2(1.0, 3.0),
    ];
    painter.add(egui::Shape::convex_polygon(
        cone,
        color,
        egui::Stroke::NONE,
    ));
    for radius in [7.0_f32, 10.0] {
        let points: Vec<egui::Pos2> = (-60..=60)
            .step_by(10)
            .map(|deg| {
                let rad = (deg as f32).to_radians();
                center + egui::vec2(2.0 + radius * rad.cos(), radius * rad.sin())
            })
            .collect();
        painter.add(egui::Shape::line(
            points,
            egui::Stroke::new(1.2, color),
        ));
    }
}

fn initials(name: &str) -> String {
    name.split_whitespace()
        .filter_map(|word| word.chars().next())
        .take(2)
        .collect::<String>()
        .to_uppercase()
}

#[allow(dead_code)]
fn _keep_color(_: Color32) {}
