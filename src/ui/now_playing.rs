//! Right-hand now-playing and recently-played panel.

use eframe::egui;

use crate::i18n::{t, tf};
use crate::models::Station;
use crate::station_playlist::StationPlaylist;

use super::theme::Theme;
use super::Action;

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
    if let Some(station) = station {
        ui.label(
            egui::RichText::new(&station.name)
                .size(14.0)
                .color(theme.text_primary),
        );
        ui.add_space(6.0);
    }

    ui.label(
        egui::RichText::new(t("panel.now_playing"))
            .size(11.0)
            .color(theme.text_secondary),
    );
    ui.add_space(2.0);
    ui.label(
        egui::RichText::new(now_playing.unwrap_or("—"))
            .size(14.0)
            .color(theme.text_primary),
    );
    if now_playing.is_some() {
        ui.add_space(4.0);
        if ui.small_button(t("panel.want_this")).clicked() {
            actions.push(Action::WantCurrent);
        }
    }
    ui.add_space(10.0);
    ui.separator();
    ui.add_space(6.0);

    if let Some(next) = playlist.and_then(|playlist| playlist.next.as_ref()) {
        ui.label(
            egui::RichText::new(t("panel.up_next"))
                .size(11.0)
                .color(theme.text_secondary),
        );
        ui.add_space(2.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(
                egui::RichText::new(entry_text(next))
                    .size(13.0)
                    .color(theme.text_primary),
            );
            if ui
                .small_button("+")
                .on_hover_text(t("panel.want_this"))
                .clicked()
            {
                actions.push(Action::WantSong {
                    artist: (!next.artist.trim().is_empty()).then(|| next.artist.clone()),
                    title: next.title.clone(),
                });
            }
        });
        ui.add_space(8.0);
    }

    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(t("panel.recently_played"))
                .size(11.0)
                .color(theme.text_secondary),
        );
        if ui.small_button(t("panel.refresh")).clicked() {
            actions.push(Action::RefreshPlaylist);
        }
    });
    ui.add_space(4.0);

    if let Some(error) = error {
        ui.label(
            egui::RichText::new(tf("panel.playlist_failed", &[error]))
                .size(12.0)
                .color(theme.text_secondary),
        );
        return;
    }
    if loading {
        ui.label(
            egui::RichText::new(t("panel.playlist_loading"))
                .size(12.0)
                .color(theme.text_secondary),
        );
        return;
    }
    let Some(playlist) = playlist else {
        let key = if station.is_some() {
            "panel.playlist_empty"
        } else {
            "panel.select_station"
        };
        ui.label(
            egui::RichText::new(t(key))
                .size(12.0)
                .color(theme.text_secondary),
        );
        return;
    };
    if playlist.entries.is_empty() {
        ui.label(
            egui::RichText::new(t("panel.playlist_empty"))
                .size(12.0)
                .color(theme.text_secondary),
        );
        return;
    }

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for entry in &playlist.entries {
                ui.horizontal_wrapped(|ui| {
                    if let Some(at) = &entry.at {
                        ui.label(
                            egui::RichText::new(display_time(at))
                                .size(11.0)
                                .color(theme.text_secondary),
                        );
                    }
                    ui.label(
                        egui::RichText::new(entry_text(entry))
                            .size(12.0)
                            .color(theme.text_primary),
                    );
                    if ui
                        .small_button("+")
                        .on_hover_text(t("panel.want_this"))
                        .clicked()
                    {
                        actions.push(Action::WantSong {
                            artist: (!entry.artist.trim().is_empty()).then(|| entry.artist.clone()),
                            title: entry.title.clone(),
                        });
                    }
                });
                ui.add_space(2.0);
            }
        });

    if let Some(url) = playlist
        .page_url
        .as_deref()
        .or(playlist.homepage.as_deref())
    {
        ui.add_space(6.0);
        let label = match playlist.provider {
            Some(provider) => format!("{} ({provider})", t("panel.playlist_source")),
            None => t("panel.playlist_source").to_string(),
        };
        ui.hyperlink_to(label, url);
    }
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
