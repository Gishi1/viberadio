//! Search, sortable header and the virtualised station table.

use std::collections::HashSet;

use eframe::egui::{self, Align2, FontId, Rect, Sense};
use uuid::Uuid;

use crate::models::Station;

use crate::i18n::{t, tf};

use super::theme::Theme;
use super::{paint_clipped_text, Action, SortKey, UiState, ROW_HEIGHT, TABLE_HEADER_HEIGHT};

pub const SEARCH_ID: &str = "viberadio-station-search";

pub fn search_id() -> egui::Id {
    egui::Id::new(SEARCH_ID)
}

pub fn render(
    ui: &mut egui::Ui,
    theme: &Theme,
    state: &mut UiState,
    stations: &[Station],
    indices: &[usize],
    selected: Option<Uuid>,
    active_station: Option<Uuid>,
    live_stations: &HashSet<Uuid>,
) -> Vec<Action> {
    let mut actions = Vec::new();

    // Search filters as you type; `/` focuses it.
    ui.horizontal(|ui| {
        let count_text = if indices.len() == 1 {
            tf("topbar.station_one", &[&indices.len().to_string()])
        } else {
            tf("topbar.station_many", &[&indices.len().to_string()])
        };
        // Measure the count so a large library cannot push the label under the
        // search field.
        let label_width = ui.fonts(|fonts| {
            fonts
                .layout_no_wrap(
                    count_text.clone(),
                    FontId::proportional(12.0),
                    theme.text_secondary,
                )
                .size()
                .x
        });
        let clear_width = if state.search.is_empty() { 0.0 } else { 28.0 };
        let spacing = ui.spacing().item_spacing.x;
        let gaps = (if state.search.is_empty() { 2.0 } else { 3.0 }) * spacing + 4.0;
        let field_width =
            (ui.available_width() - 16.0 - clear_width - label_width - gaps).max(120.0);
        // Draw the magnifier rather than relying on a glyph the bundled font may
        // not have.
        let (icon_rect, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), Sense::hover());
        let center = icon_rect.center() - egui::vec2(1.0, 1.0);
        let stroke = egui::Stroke::new(1.4, theme.text_secondary);
        ui.painter().circle_stroke(center, 5.0, stroke);
        ui.painter().line_segment(
            [center + egui::vec2(3.5, 3.5), center + egui::vec2(8.0, 8.0)],
            stroke,
        );
        let mut edit = egui::TextEdit::singleline(&mut state.search)
            .hint_text(t("topbar.search"))
            .id(search_id())
            .desired_width(field_width);
        edit = edit.text_color(theme.text_primary);
        ui.add_sized([field_width, 28.0], edit);
        if !state.search.is_empty() && ui.small_button("×").clicked() {
            state.search.clear();
            actions.push(Action::ClearSearch);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                egui::RichText::new(count_text)
                    .size(12.0)
                    .color(theme.text_secondary),
            );
        });
    });
    ui.add_space(4.0);

    let broken_count = stations.iter().filter(|station| station.broken).count();
    let missing = indices
        .iter()
        .filter(|index| stations[**index].bitrate.unwrap_or(0) == 0)
        .count();
    let probe_targets = if state.selected_stations.is_empty() {
        missing
    } else {
        indices
            .iter()
            .filter(|index| {
                let station = &stations[**index];
                state.selected_stations.contains(&station.id)
                    && station.bitrate.unwrap_or(0) == 0
            })
            .count()
    };
    if !state.selected_stations.is_empty()
        || broken_count > 0
        || probe_targets > 0
        || state.probe_progress.is_some()
    {
        ui.horizontal(|ui| {
            if let Some((done, total)) = state.probe_progress {
                if ui.button(t("table.stop_bitrates")).clicked() {
                    actions.push(Action::StopBitrateProbe);
                }
                ui.label(
                    egui::RichText::new(tf(
                        "table.bitrate_progress",
                        &[&done.to_string(), &total.to_string()],
                    ))
                    .size(12.0)
                    .color(theme.text_secondary),
                );
            } else if probe_targets > 0 {
                let label = tf("table.fetch_bitrates", &[&probe_targets.to_string()]);
                if ui.button(label).clicked() {
                    actions.push(Action::ProbeBitrates);
                }
            }
            if !state.selected_stations.is_empty() {
                let label = tf(
                    "table.remove_selected",
                    &[&state.selected_stations.len().to_string()],
                );
                if ui.button(label).clicked() {
                    actions.push(Action::RemoveStations(
                        state.selected_stations.iter().copied().collect(),
                    ));
                }
                if ui.button(t("table.clear_selection")).clicked() {
                    actions.push(Action::ClearSelection);
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.checkbox(&mut state.show_broken, t("table.show_broken"));
                if broken_count > 0 {
                    let label =
                        tf("table.remove_broken", &[&broken_count.to_string()]);
                    if ui.button(label).clicked() {
                        actions.push(Action::RemoveBroken);
                    }
                }
            });
        });
        ui.add_space(4.0);
    }

    if stations.is_empty() {
        return empty_library(ui, theme, &mut actions);
    }
    if indices.is_empty() {
        return empty_filter(ui, theme, state, &mut actions);
    }

    let view_ids: Vec<Uuid> = indices.iter().map(|&index| stations[index].id).collect();
    let width = ui.available_width();
    let (header_rect, _) =
        ui.allocate_exact_size(egui::vec2(width, TABLE_HEADER_HEIGHT), Sense::hover());
    paint_header(ui, theme, state, header_rect);

    let old_spacing = ui.spacing().item_spacing.y;
    ui.spacing_mut().item_spacing.y = 0.0;
    egui::ScrollArea::vertical()
        .id_salt("station-rows")
        .auto_shrink([false, false])
        .show_rows(ui, ROW_HEIGHT, indices.len(), |ui, range| {
            for position in range {
                let station = &stations[indices[position]];
                let playing = live_stations.contains(&station.id);
                let focused = selected == Some(station.id);
                let active = active_station == Some(station.id);
                render_row(
                    ui,
                    theme,
                    station,
                    position,
                    playing,
                    focused,
                    active,
                    &view_ids,
                    state,
                    &mut actions,
                );
            }
        });
    ui.spacing_mut().item_spacing.y = old_spacing;

    actions
}

fn paint_header(ui: &mut egui::Ui, theme: &Theme, state: &mut UiState, rect: Rect) {
    let columns = columns(rect);
    ui.painter().line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        egui::Stroke::new(1.0, theme.border),
    );
    let font = FontId::proportional(12.0);

    let star = Rect::from_center_size(columns.star.center(), egui::vec2(16.0, 16.0));
    ui.painter().text(
        star.center(),
        Align2::CENTER_CENTER,
        "★",
        font.clone(),
        theme.text_secondary,
    );

    sort_header(
        ui,
        theme,
        state,
        columns.name,
        "name",
        t("table.name"),
        SortKey::Name,
        Align2::LEFT_CENTER,
    );
    sort_header(
        ui,
        theme,
        state,
        columns.genre,
        "genre",
        t("table.genre"),
        SortKey::Genre,
        Align2::LEFT_CENTER,
    );
    sort_header(
        ui,
        theme,
        state,
        columns.bitrate,
        "bitrate",
        t("table.bitrate"),
        SortKey::Bitrate,
        Align2::RIGHT_CENTER,
    );
    sort_header(
        ui,
        theme,
        state,
        columns.country,
        "country",
        t("table.country"),
        SortKey::Country,
        Align2::LEFT_CENTER,
    );
}

fn sort_header(
    ui: &mut egui::Ui,
    theme: &Theme,
    state: &mut UiState,
    rect: Rect,
    id: &str,
    label: &str,
    key: SortKey,
    anchor: Align2,
) {
    let active = state.sort == key;
    let arrow = if active {
        if state.sort_ascending {
            "  ▲"
        } else {
            "  ▼"
        }
    } else {
        ""
    };
    let text = format!("{label}{arrow}");
    let pos = match anchor {
        Align2::RIGHT_CENTER => rect.right_center(),
        _ => rect.left_center(),
    };
    ui.painter().text(
        pos,
        anchor,
        text,
        FontId::proportional(12.0),
        if active {
            theme.accent
        } else {
            theme.text_secondary
        },
    );
    let response = ui.interact(rect, ui.make_persistent_id(("sort", id)), Sense::click());
    if response.clicked() {
        if state.sort == key {
            state.sort_ascending = !state.sort_ascending;
        } else {
            state.sort = key;
            state.sort_ascending = key == SortKey::Name || key == SortKey::Country;
        }
    }
}

fn render_row(
    ui: &mut egui::Ui,
    theme: &Theme,
    station: &Station,
    position: usize,
    playing: bool,
    focused: bool,
    active: bool,
    view_ids: &[Uuid],
    state: &mut UiState,
    actions: &mut Vec<Action>,
) {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, ROW_HEIGHT), Sense::click());
    let hovered = response.hovered();
    let selected = state.selected_stations.contains(&station.id) || focused;
    let fill = if playing {
        theme.bg_row_playing
    } else if selected || hovered {
        theme.bg_row_selected
    } else if position % 2 == 1 {
        theme.bg_row_alt
    } else {
        theme.bg_row
    };
    ui.painter().rect_filled(rect, 0.0, fill);
    if active {
        ui.painter().rect_filled(
            Rect::from_min_size(rect.min, egui::vec2(2.0, rect.height())),
            0.0,
            theme.accent,
        );
    }

    let columns = columns(rect);

    let star_rect = Rect::from_min_size(
        egui::pos2(rect.left() + 6.0, rect.top()),
        egui::vec2(22.0, ROW_HEIGHT),
    );
    let star = ui.interact(
        star_rect,
        ui.make_persistent_id(("favourite", station.id)),
        Sense::click(),
    );
    let star_color = if station.is_favorite {
        theme.accent
    } else {
        theme.text_disabled
    };
    ui.painter().text(
        star_rect.center(),
        Align2::CENTER_CENTER,
        if station.is_favorite { "★" } else { "☆" },
        FontId::proportional(14.0),
        star_color,
    );

    let name_color = if station.broken {
        theme.danger
    } else {
        theme.text_primary
    };
    paint_clipped_text(
        ui.painter(),
        columns.name,
        egui::pos2(columns.name.left(), columns.name.center().y),
        Align2::LEFT_CENTER,
        &station.name,
        FontId::proportional(14.0),
        name_color,
    );
    let genre = station
        .genre
        .as_deref()
        .filter(|value| !value.trim().is_empty());
    let bitrate = station.bitrate.filter(|value| *value > 0);
    let country = station
        .country
        .as_deref()
        .filter(|value| !value.trim().is_empty());
    paint_clipped_text(
        ui.painter(),
        columns.genre,
        egui::pos2(columns.genre.left(), columns.genre.center().y),
        Align2::LEFT_CENTER,
        genre.unwrap_or("—"),
        FontId::proportional(12.0),
        if genre.is_some() {
            theme.text_secondary
        } else {
            theme.text_disabled
        },
    );
    paint_clipped_text(
        ui.painter(),
        columns.bitrate,
        egui::pos2(columns.bitrate.right(), columns.bitrate.center().y),
        Align2::RIGHT_CENTER,
        &bitrate_label(bitrate),
        FontId::monospace(12.0),
        if bitrate.is_some() {
            theme.text_secondary
        } else {
            theme.text_disabled
        },
    );
    paint_clipped_text(
        ui.painter(),
        columns.country,
        egui::pos2(columns.country.left(), columns.country.center().y),
        Align2::LEFT_CENTER,
        country.unwrap_or("—"),
        FontId::proportional(12.0),
        if country.is_some() {
            theme.text_secondary
        } else {
            theme.text_disabled
        },
    );

    if star.clicked() {
        actions.push(Action::ToggleFavorite(station.id));
    } else if response.double_clicked() {
        actions.push(Action::Play(station.id));
    } else if response.clicked() {
        let modifiers = ui.input(|input| input.modifiers);
        if modifiers.shift {
            if let Some(anchor) = state.selection_anchor {
                let from = view_ids.iter().position(|id| *id == anchor);
                let to = view_ids.iter().position(|id| *id == station.id);
                if let (Some(from), Some(to)) = (from, to) {
                    let (low, high) = if from <= to { (from, to) } else { (to, from) };
                    state.selected_stations.clear();
                    state
                        .selected_stations
                        .extend(view_ids[low..=high].iter().copied());
                } else {
                    state.selected_stations.insert(station.id);
                }
            } else {
                state.selected_stations.insert(station.id);
            }
            state.selection_anchor = Some(station.id);
        } else if modifiers.command || modifiers.ctrl {
            if !state.selected_stations.remove(&station.id) {
                state.selected_stations.insert(station.id);
            }
            state.selection_anchor = Some(station.id);
        } else {
            state.selected_stations.clear();
            state.selected_stations.insert(station.id);
            state.selection_anchor = Some(station.id);
        }
        actions.push(Action::Select(station.id));
    }
    response.context_menu(|ui| {
        if ui.button(t("menu.play")).clicked() {
            actions.push(Action::Play(station.id));
            ui.close_menu();
        }
        if ui.button(t("menu.schedule")).clicked() {
            actions.push(Action::ScheduleStation(station.id));
            ui.close_menu();
        }
        if ui.button(t("menu.record_background")).clicked() {
            actions.push(Action::RecordBackground(station.id));
            ui.close_menu();
        }
        let favourite = if station.is_favorite {
            t("menu.remove_favourite")
        } else {
            t("menu.add_favourite")
        };
        if ui.button(favourite).clicked() {
            actions.push(Action::ToggleFavorite(station.id));
            ui.close_menu();
        }
        if ui.button(t("menu.remove_station")).clicked() {
            actions.push(Action::RemoveStations(vec![station.id]));
            ui.close_menu();
        }
        let watch = if station.watched {
            t("menu.unwatch_station")
        } else {
            t("menu.watch_station")
        };
        if ui.button(watch).clicked() {
            actions.push(Action::ToggleWatched(station.id));
            ui.close_menu();
        }
    });
    response.on_hover_text(&station.url);
}

fn empty_library(ui: &mut egui::Ui, theme: &Theme, actions: &mut Vec<Action>) -> Vec<Action> {
    ui.add_space((ui.available_height() * 0.2).max(40.0));
    ui.vertical_centered(|ui| {
        ui.label(
            egui::RichText::new(t("empty.title"))
                .size(20.0)
                .strong()
                .color(theme.text_primary),
        );
        ui.label(
            egui::RichText::new(t("empty.body"))
                .size(14.0)
                .color(theme.text_secondary),
        );
        ui.add_space(12.0);
        if ui
            .add(
                egui::Button::new(egui::RichText::new(t("empty.import")).color(theme.bg_app))
                    .fill(theme.accent),
            )
            .clicked()
        {
            actions.push(Action::ImportRadioNet);
        }
        if ui.button(t("empty.add")).clicked() {
            actions.push(Action::OpenAddStation);
        }
    });
    std::mem::take(actions)
}

fn empty_filter(
    ui: &mut egui::Ui,
    theme: &Theme,
    state: &mut UiState,
    actions: &mut Vec<Action>,
) -> Vec<Action> {
    ui.add_space((ui.available_height() * 0.2).max(40.0));
    ui.vertical_centered(|ui| {
        ui.label(
            egui::RichText::new(t("empty.no_match"))
                .size(20.0)
                .strong()
                .color(theme.text_primary),
        );
        let hint = if state.search.trim().is_empty() {
            t("empty.filter_empty")
        } else {
            t("empty.try_search")
        };
        ui.label(
            egui::RichText::new(hint)
                .size(14.0)
                .color(theme.text_secondary),
        );
        ui.add_space(12.0);
        if !state.search.is_empty() && ui.button(t("empty.clear_search")).clicked() {
            state.search.clear();
            actions.push(Action::ClearSearch);
        }
        if state.filter != super::LibraryFilter::All && ui.button(t("empty.show_all")).clicked() {
            state.filter = super::LibraryFilter::All;
        }
    });
    std::mem::take(actions)
}

fn bitrate_label(bitrate: Option<u32>) -> String {
    match bitrate {
        Some(value) => format!("{value} kbps"),
        None => "—".to_string(),
    }
}

struct Columns {
    star: Rect,
    name: Rect,
    genre: Rect,
    bitrate: Rect,
    country: Rect,
}

fn columns(rect: Rect) -> Columns {
    let pad = 8.0;
    let star_width = 28.0;
    let genre_width = 150.0;
    let bitrate_width = 80.0;
    let country_width = 140.0;
    let gap = 12.0;
    let name_left = rect.left() + star_width + pad;
    let country_left = rect.right() - country_width;
    let bitrate_right = country_left - gap;
    let bitrate_left = bitrate_right - bitrate_width;
    let genre_right = bitrate_left - gap;
    let genre_left = genre_right - genre_width;
    let name_right = genre_left - gap;
    Columns {
        star: Rect::from_min_max(
            egui::pos2(rect.left(), rect.top()),
            egui::pos2(rect.left() + star_width, rect.bottom()),
        ),
        name: Rect::from_min_max(
            egui::pos2(name_left, rect.top()),
            egui::pos2(name_right.max(name_left), rect.bottom()),
        ),
        genre: Rect::from_min_max(
            egui::pos2(genre_left, rect.top()),
            egui::pos2(genre_right, rect.bottom()),
        ),
        bitrate: Rect::from_min_max(
            egui::pos2(bitrate_left, rect.top()),
            egui::pos2(bitrate_right, rect.bottom()),
        ),
        country: Rect::from_min_max(
            egui::pos2(country_left, rect.top()),
            egui::pos2(rect.right() - pad, rect.bottom()),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_fit_the_minimum_window() {
        // 900 px window minus the 200 px sidebar and 12 px margins each side.
        let rect = Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(676.0, ROW_HEIGHT));
        let columns = columns(rect);
        assert!(columns.name.width() > 40.0, "name column collapsed");
        assert!(columns.name.right() <= columns.genre.left());
        assert!(columns.genre.right() <= columns.bitrate.left());
        assert!(columns.bitrate.right() <= columns.country.left());
        assert!(columns.country.right() <= rect.right());
    }

    #[test]
    fn empty_bitrate_renders_a_dash() {
        assert_eq!(bitrate_label(None), "—");
        assert_eq!(bitrate_label(Some(128)), "128 kbps");
    }
}
