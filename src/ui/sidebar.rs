//! Filter sidebar: library filters, genre counts and the library actions.

use eframe::egui::{self, Align2, Color32, FontId, Rect, Sense};

use crate::i18n::t;

use super::theme::Theme;
use super::{Action, LibraryFilter, LibraryStats, UiState, SIDEBAR_ITEM_HEIGHT};

pub fn render(
    ui: &mut egui::Ui,
    theme: &Theme,
    state: &mut UiState,
    stats: &LibraryStats,
) -> Vec<Action> {
    let mut actions = Vec::new();

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.add_space(16.0);
        ui.label(
            egui::RichText::new(t("app.name"))
                .size(16.0)
                .strong()
                .color(theme.text_primary),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let label = if theme.dark {
                t("sidebar.theme_light")
            } else {
                t("sidebar.theme_dark")
            };
            if ui
                .small_button(label)
                .on_hover_text(t("sidebar.theme_toggle"))
                .clicked()
            {
                state.theme = state.theme.toggled();
            }
        });
    });
    ui.add_space(10.0);

    // Leave room for the action footer, and scroll if the genre list is long.
    // The footer is drawn after the scroll area, so this must cover its natural
    // height: separator, spacing, one 20 px row per action, and a little slack.
    const FOOTER_ACTIONS: usize = 6;
    const FOOTER_ITEM_HEIGHT: f32 = 20.0;
    let footer_height = 52.0 + FOOTER_ACTIONS as f32 * (FOOTER_ITEM_HEIGHT + 1.0);
    let scroll_height = (ui.available_height() - footer_height).max(120.0);
    egui::ScrollArea::vertical()
        .id_salt("sidebar-scroll")
        .auto_shrink([false, false])
        .max_height(scroll_height)
        .show(ui, |ui| {
            let old_spacing = ui.spacing().item_spacing.y;
            ui.spacing_mut().item_spacing.y = 0.0;
            section_label(ui, theme, t("sidebar.library"));
            if item(
                ui,
                theme,
                t("sidebar.all"),
                Some(stats.total),
                state.filter == LibraryFilter::All,
            )
            .clicked()
            {
                state.filter = LibraryFilter::All;
            }
            if item(
                ui,
                theme,
                t("sidebar.favourites"),
                Some(stats.favorites),
                state.filter == LibraryFilter::Favorites,
            )
            .clicked()
            {
                state.filter = LibraryFilter::Favorites;
            }
            if item(
                ui,
                theme,
                t("sidebar.recently_played"),
                Some(stats.recently_played),
                state.filter == LibraryFilter::RecentlyPlayed,
            )
            .clicked()
            {
                state.filter = LibraryFilter::RecentlyPlayed;
            }
            if item(
                ui,
                theme,
                t("sidebar.watched"),
                Some(stats.watched),
                state.filter == LibraryFilter::Watched,
            )
            .clicked()
            {
                state.filter = LibraryFilter::Watched;
            }
            if item(ui, theme, t("sidebar.sessions"), None, false).clicked() {
                actions.push(Action::OpenSessions);
            }
            if item(ui, theme, t("sidebar.schedules"), None, false).clicked() {
                actions.push(Action::OpenSchedules);
            }
            if item(ui, theme, t("sidebar.wishlist"), None, false).clicked() {
                actions.push(Action::OpenWishlist);
            }
            if item(ui, theme, t("sidebar.watch_history"), None, false).clicked() {
                actions.push(Action::OpenWatchHistory);
            }
            if item(ui, theme, t("sidebar.settings"), None, false).clicked() {
                actions.push(Action::OpenSettings);
            }
            if item(ui, theme, t("sidebar.about"), None, false).clicked() {
                actions.push(Action::OpenAbout);
            }
            if item(ui, theme, t("sidebar.help"), None, false).clicked() {
                actions.push(Action::OpenHelp);
            }

            ui.add_space(8.0);
            section_label(ui, theme, t("sidebar.genres"));
            if stats.genres.is_empty() {
                ui.horizontal(|ui| {
                    ui.add_space(16.0);
                    ui.label(
                        egui::RichText::new(t("sidebar.no_genres"))
                            .size(12.0)
                            .color(theme.text_disabled),
                    );
                });
            }
            for (genre, count) in &stats.genres {
                let active = matches!(&state.filter, LibraryFilter::Genre(name) if name == genre);
                if item(ui, theme, genre, Some(*count), active).clicked() {
                    state.filter = LibraryFilter::Genre(genre.clone());
                }
            }
            ui.add_space(8.0);
            ui.spacing_mut().item_spacing.y = old_spacing;
        });

    ui.add_space(4.0);
    ui.separator();
    ui.add_space(4.0);
    let old_spacing = ui.spacing().item_spacing.y;
    ui.spacing_mut().item_spacing.y = 1.0;

    if footer_item(ui, theme, t("sidebar.add_station")).clicked() {
        actions.push(Action::OpenAddStation);
    }
    if footer_item(ui, theme, t("sidebar.import_radionet")).clicked() {
        actions.push(Action::ImportRadioNet);
    }
    if footer_item(ui, theme, t("sidebar.import_radiobrowser")).clicked() {
        actions.push(Action::ImportRadioBrowser);
    }
    if footer_item(ui, theme, t("sidebar.export_m3u")).clicked() {
        actions.push(Action::ExportPlaylist);
    }
    if footer_item(ui, theme, t("sidebar.import_m3u")).clicked() {
        actions.push(Action::ImportPlaylist);
    }
    if footer_item(ui, theme, t("sidebar.export_history")).clicked() {
        actions.push(Action::ExportHistory);
    }
    ui.spacing_mut().item_spacing.y = old_spacing;

    actions
}

fn section_label(ui: &mut egui::Ui, theme: &Theme, label: &str) {
    ui.horizontal(|ui| {
        ui.add_space(16.0);
        ui.label(
            egui::RichText::new(label)
                .size(12.0)
                .color(theme.text_secondary),
        );
    });
    ui.add_space(2.0);
}

fn item(
    ui: &mut egui::Ui,
    theme: &Theme,
    label: &str,
    count: Option<usize>,
    active: bool,
) -> egui::Response {
    let width = ui.available_width();
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, SIDEBAR_ITEM_HEIGHT), Sense::click());
    let hovered = response.hovered();
    let fill = if active {
        theme.bg_row_selected
    } else if hovered {
        theme.bg_row_alt
    } else {
        Color32::TRANSPARENT
    };
    if fill != Color32::TRANSPARENT {
        ui.painter().rect_filled(rect, 0.0, fill);
    }
    if active {
        ui.painter().rect_filled(
            Rect::from_min_size(rect.min, egui::vec2(2.0, rect.height())),
            0.0,
            theme.accent,
        );
    }
    let text_color = if active {
        theme.accent
    } else {
        theme.text_primary
    };
    ui.painter().text(
        egui::pos2(rect.left() + 16.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(14.0),
        text_color,
    );
    if let Some(count) = count {
        ui.painter().text(
            egui::pos2(rect.right() - 16.0, rect.center().y),
            Align2::RIGHT_CENTER,
            count.to_string(),
            FontId::monospace(12.0),
            theme.text_secondary,
        );
    }
    response
}

fn footer_item(ui: &mut egui::Ui, theme: &Theme, label: &str) -> egui::Response {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 20.0), Sense::click());
    if response.hovered() {
        ui.painter().rect_filled(rect, 0.0, theme.bg_row_alt);
    }
    ui.painter().text(
        egui::pos2(rect.left() + 16.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(13.0),
        theme.text_secondary,
    );
    response
}
