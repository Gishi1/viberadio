//! Right-hand now-playing and recently-played panel.

use eframe::egui;

use crate::i18n::{t, tf};
use crate::models::Station;
use crate::station_playlist::StationPlaylist;

use super::theme::Theme;
use super::{ghost_button, paint_clipped_text, section_header, space, text, Action};

pub fn render(
    ui: &mut egui::Ui,
    theme: &Theme,
    station: Option<&Station>,
    now_playing: Option<&str>,
    playlist: Option<&StationPlaylist>,
    error: Option<&str>,
    loading: bool,
    actions: &mut Vec<Action>,
) {
    // Now playing: header, title, station, action.
    section_header(ui, theme, t("panel.now_playing"));
    ui.add_space(space::XS);
    ui.label(
        egui::RichText::new(now_playing.unwrap_or("—"))
            .size(text::HEADING)
            .strong()
            .color(if now_playing.is_some() {
                theme.text_primary
            } else {
                theme.text_disabled
            }),
    );
    if let Some(station) = station {
        ui.label(
            egui::RichText::new(&station.name)
                .size(text::CAPTION)
                .color(theme.text_secondary),
        );
    }
    ui.add_space(space::S);
    ui.horizontal(|ui| {
        if now_playing.is_some()
            && ui.add(ghost_button(t("panel.want_this"), theme)).clicked()
        {
            actions.push(Action::WantCurrent);
        }
        if let Some(station) = station {
            if ui.add(ghost_button(t("panel.info"), theme)).clicked() {
                actions.push(Action::OpenStationInfo(station.id));
            }
        }
    });
    ui.add_space(space::M);
    ui.separator();
    ui.add_space(space::S);

    if let Some(next) = playlist.and_then(|playlist| playlist.next.as_ref()) {
        section_header(ui, theme, t("panel.up_next"));
        ui.add_space(space::XS);
        ui.horizontal_wrapped(|ui| {
            ui.label(
                egui::RichText::new(entry_text(next))
                    .size(text::CAPTION)
                    .color(theme.text_primary),
            );
            if ui
                .add(ghost_button("+", theme))
                .on_hover_text(t("panel.want_this"))
                .clicked()
            {
                actions.push(Action::WantSong {
                    artist: (!next.artist.trim().is_empty()).then(|| next.artist.clone()),
                    title: next.title.clone(),
                });
            }
        });
        ui.add_space(space::S);
    }

    ui.horizontal(|ui| {
        section_header(ui, theme, t("panel.recently_played"));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add(ghost_button("↻", theme))
                .on_hover_text(t("panel.refresh"))
                .clicked()
            {
                actions.push(Action::RefreshPlaylist);
            }
        });
    });
    ui.add_space(space::XS);

    if let Some(error) = error {
        empty_state(ui, theme, t("panel.no_track_history"));
        ui.label(
            egui::RichText::new(tf("panel.playlist_failed", &[error]))
                .size(text::CAPTION)
                .color(theme.danger),
        );
        return;
    }
    if loading {
        ui.label(
            egui::RichText::new(t("panel.playlist_loading"))
                .size(text::CAPTION)
                .color(theme.text_secondary),
        );
        return;
    }
    let Some(playlist) = playlist else {
        let (title, body) = if station.is_some() {
            (t("panel.no_track_history"), t("panel.playlist_empty"))
        } else {
            (t("panel.nothing_selected"), t("panel.select_station"))
        };
        empty_state(ui, theme, title);
        ui.label(
            egui::RichText::new(body)
                .size(text::CAPTION)
                .color(theme.text_secondary),
        );
        return;
    };
    if playlist.entries.is_empty() {
        empty_state(ui, theme, t("panel.no_track_history"));
        ui.label(
            egui::RichText::new(t("panel.playlist_empty"))
                .size(text::CAPTION)
                .color(theme.text_secondary),
        );
        return;
    }

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for (index, entry) in playlist.entries.iter().enumerate() {
                let width = ui.available_width();
                let (rect, response) =
                    ui.allocate_exact_size(egui::vec2(width, 28.0), egui::Sense::hover());
                if response.hovered() {
                    ui.painter().rect_filled(rect, 4.0, theme.bg_hover);
                }
                let mut left = rect.left() + space::S;
                if let Some(at) = &entry.at {
                    let time = display_time(at);
                    let galley = ui.painter().layout_no_wrap(
                        time,
                        egui::FontId::monospace(text::CAPTION),
                        theme.text_disabled,
                    );
                    left += galley.size().x + space::S;
                    ui.painter().galley(
                        egui::pos2(
                            rect.left() + space::S,
                            rect.center().y - galley.size().y / 2.0,
                        ),
                        galley,
                        theme.text_disabled,
                    );
                }
                let plus_width = 24.0;
                paint_clipped_text(
                    ui.painter(),
                    egui::Rect::from_min_max(
                        egui::pos2(left, rect.top()),
                        egui::pos2(rect.right() - plus_width, rect.bottom()),
                    ),
                    egui::pos2(left, rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    &entry_text(entry),
                    egui::FontId::proportional(text::CAPTION),
                    theme.text_primary,
                );
                if response.hovered() {
                    let plus_rect = egui::Rect::from_center_size(
                        egui::pos2(rect.right() - plus_width / 2.0, rect.center().y),
                        egui::vec2(20.0, 20.0),
                    );
                    let plus = ui.interact(
                        plus_rect,
                        ui.make_persistent_id(("want-entry", index)),
                        egui::Sense::click(),
                    );
                    ui.painter().text(
                        plus_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "+",
                        egui::FontId::proportional(text::BODY),
                        theme.accent,
                    );
                    if plus.clicked() {
                        actions.push(Action::WantSong {
                            artist: (!entry.artist.trim().is_empty())
                                .then(|| entry.artist.clone()),
                            title: entry.title.clone(),
                        });
                    }
                }
            }
        });

    if let Some(url) = playlist
        .page_url
        .as_deref()
        .or(playlist.homepage.as_deref())
    {
        ui.add_space(space::S);
        let label = match playlist.provider {
            Some(provider) => format!("{} ({provider})", t("panel.playlist_source")),
            None => t("panel.playlist_source").to_string(),
        };
        ui.hyperlink_to(label, url);
    }
}

fn empty_state(ui: &mut egui::Ui, theme: &Theme, title: &str) {
    ui.label(
        egui::RichText::new(title)
            .size(text::HEADING)
            .strong()
            .color(theme.text_primary),
    );
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
    // Provider timestamps look like `2026-11-10 06:52:13 +0100`.
    let parts: Vec<&str> = at.split_whitespace().collect();
    if parts.len() >= 2 && parts[1].contains(':') {
        return parts[1].chars().take(5).collect();
    }
    at.to_string()
}
