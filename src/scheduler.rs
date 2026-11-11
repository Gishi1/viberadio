//! Per-station schedules: recurrence, due evaluation and finish actions.
//!
//! The evaluator is pure: it takes a schedule, the current UTC instant and a
//! local-to-UTC resolver. The app owns persistence and applies the returned
//! effects, so the same logic runs under a fixed clock in tests (FR-S3) without
//! depending on the machine's time zone or a real audio device.
//!
//! `start_at` / `end_at` are *naive local* times: a user means "07:00", not an
//! instant. Each occurrence is resolved per local date and then converted to
//! UTC, which keeps a daily schedule at 07:00 across a DST change instead of
//! drifting by an hour.

use std::collections::HashSet;
use std::io;
use std::process::Command;

use chrono::{
    DateTime, Datelike, Duration, FixedOffset, Local, LocalResult, NaiveDate, NaiveDateTime,
    NaiveTime, TimeZone, Timelike, Utc, Weekday,
};
use uuid::Uuid;

use crate::player::PlaybackId;

/// How far back a missed occurrence is considered. A window is at most 24 h, so
/// eight days is enough to cover a closed laptop plus a weekend.
pub const LOOKBACK_DAYS: i64 = 8;

/// A schedule without an end time is a start-only event: it fires if the app
/// notices within this grace period, otherwise it is skipped.
pub const START_GRACE_MINUTES: i64 = 5;

/// A finish action that is noticed long after its window ended (for example
/// after a suspend) would be surprising: the stream is still stopped, but the
/// close/command is skipped beyond this grace period.
pub const FINISH_GRACE_MINUTES: i64 = 5;

// ---------------------------------------------------------------------------
// Actions and finish actions
// ---------------------------------------------------------------------------

/// What a schedule does when it fires (F37, FR-S1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleAction {
    Play,
    PlayRecordSplit,
    PlayRecordContinuous,
    RecordSplit,
    RecordContinuous,
}

impl ScheduleAction {
    pub const ALL: [ScheduleAction; 5] = [
        ScheduleAction::Play,
        ScheduleAction::PlayRecordSplit,
        ScheduleAction::PlayRecordContinuous,
        ScheduleAction::RecordSplit,
        ScheduleAction::RecordContinuous,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ScheduleAction::Play => "play",
            ScheduleAction::PlayRecordSplit => "play_record_split",
            ScheduleAction::PlayRecordContinuous => "play_record_continuous",
            ScheduleAction::RecordSplit => "record_split",
            ScheduleAction::RecordContinuous => "record_continuous",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "play" => Some(ScheduleAction::Play),
            "play_record_split" => Some(ScheduleAction::PlayRecordSplit),
            "play_record_continuous" => Some(ScheduleAction::PlayRecordContinuous),
            "record_split" => Some(ScheduleAction::RecordSplit),
            "record_continuous" => Some(ScheduleAction::RecordContinuous),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ScheduleAction::Play => crate::i18n::t("schedule.action.play"),
            ScheduleAction::PlayRecordSplit => {
                crate::i18n::t("schedule.action.play_record_split")
            }
            ScheduleAction::PlayRecordContinuous => {
                crate::i18n::t("schedule.action.play_record_continuous")
            }
            ScheduleAction::RecordSplit => crate::i18n::t("schedule.action.record_split"),
            ScheduleAction::RecordContinuous => {
                crate::i18n::t("schedule.action.record_continuous")
            }
        }
    }

    /// Whether the stream is audible. Record-only actions start it muted.
    pub fn plays(self) -> bool {
        matches!(
            self,
            ScheduleAction::Play
                | ScheduleAction::PlayRecordSplit
                | ScheduleAction::PlayRecordContinuous
        )
    }

    pub fn records(self) -> bool {
        !matches!(self, ScheduleAction::Play)
    }

    /// Split-per-track for the split variants, continuous for the others.
    pub fn split(self) -> bool {
        matches!(
            self,
            ScheduleAction::PlayRecordSplit | ScheduleAction::RecordSplit
        )
    }
}

/// What happens when the schedule's window ends (F38, FR-S2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishAction {
    None,
    CloseApp,
    RunCommand(String),
}

impl FinishAction {
    pub fn as_str(&self) -> String {
        match self {
            FinishAction::None => "none".to_string(),
            FinishAction::CloseApp => "close_app".to_string(),
            FinishAction::RunCommand(command) => format!("run:{command}"),
        }
    }

    pub fn parse(value: &str) -> Self {
        if value == "close_app" {
            FinishAction::CloseApp
        } else if let Some(command) = value.strip_prefix("run:") {
            if command.trim().is_empty() {
                FinishAction::None
            } else {
                FinishAction::RunCommand(command.to_string())
            }
        } else {
            FinishAction::None
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            FinishAction::None => crate::i18n::t("schedule.finish.none"),
            FinishAction::CloseApp => crate::i18n::t("schedule.finish.close"),
            FinishAction::RunCommand(_) => crate::i18n::t("schedule.finish.command"),
        }
    }

    /// A finish action that closes or runs something needs an end time, so it
    /// cannot fire at an arbitrary moment.
    pub fn requires_end(&self) -> bool {
        !matches!(self, FinishAction::None)
    }
}

// ---------------------------------------------------------------------------
// Recurrence
// ---------------------------------------------------------------------------

/// When a schedule repeats (F37). The start date is the not-before anchor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recurrence {
    Once,
    Daily,
    Weekly(Vec<Weekday>),
}

impl Recurrence {
    pub fn as_str(&self) -> String {
        match self {
            Recurrence::Once => "once".to_string(),
            Recurrence::Daily => "daily".to_string(),
            Recurrence::Weekly(days) => {
                let names: Vec<&str> = days.iter().map(|day| weekday_short(*day)).collect();
                format!("weekly:{}", names.join(","))
            }
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        if value == "once" {
            return Some(Recurrence::Once);
        }
        if value == "daily" {
            return Some(Recurrence::Daily);
        }
        let list = value.strip_prefix("weekly:")?;
        let mut days = Vec::new();
        for token in list.split(',') {
            let day = parse_weekday(token.trim())?;
            if !days.contains(&day) {
                days.push(day);
            }
        }
        if days.is_empty() {
            return None;
        }
        days.sort_by_key(|day| day.num_days_from_monday());
        Some(Recurrence::Weekly(days))
    }

    pub fn label(&self) -> String {
        match self {
            Recurrence::Once => crate::i18n::t("schedule.recurrence.once").to_string(),
            Recurrence::Daily => crate::i18n::t("schedule.recurrence.daily").to_string(),
            Recurrence::Weekly(days) => {
                let names: Vec<&str> = days.iter().map(|day| weekday_short(*day)).collect();
                crate::i18n::tf("schedule.recurrence.weekly", &[&names.join(", ")])
            }
        }
    }

    fn matches(&self, date: NaiveDate, anchor: NaiveDate) -> bool {
        match self {
            Recurrence::Once => date == anchor,
            Recurrence::Daily => date >= anchor,
            Recurrence::Weekly(days) => date >= anchor && days.contains(&date.weekday()),
        }
    }
}

pub fn weekday_short(day: Weekday) -> &'static str {
    match day {
        Weekday::Mon => crate::i18n::t("weekday.mon"),
        Weekday::Tue => crate::i18n::t("weekday.tue"),
        Weekday::Wed => crate::i18n::t("weekday.wed"),
        Weekday::Thu => crate::i18n::t("weekday.thu"),
        Weekday::Fri => crate::i18n::t("weekday.fri"),
        Weekday::Sat => crate::i18n::t("weekday.sat"),
        Weekday::Sun => crate::i18n::t("weekday.sun"),
    }
}

fn parse_weekday(value: &str) -> Option<Weekday> {
    match value.to_ascii_lowercase().as_str() {
        "mon" | "monday" => Some(Weekday::Mon),
        "tue" | "tues" | "tuesday" => Some(Weekday::Tue),
        "wed" | "wednesday" => Some(Weekday::Wed),
        "thu" | "thur" | "thurs" | "thursday" => Some(Weekday::Thu),
        "fri" | "friday" => Some(Weekday::Fri),
        "sat" | "saturday" => Some(Weekday::Sat),
        "sun" | "sunday" => Some(Weekday::Sun),
        _ => None,
    }
}

pub const WEEKDAYS: [Weekday; 7] = [
    Weekday::Mon,
    Weekday::Tue,
    Weekday::Wed,
    Weekday::Thu,
    Weekday::Fri,
    Weekday::Sat,
    Weekday::Sun,
];

// ---------------------------------------------------------------------------
// Schedule and occurrences
// ---------------------------------------------------------------------------

/// One persisted schedule. `start_at`/`end_at` are naive local times; only the
/// time of day of `end_at` matters, so a window may cross midnight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    pub id: Uuid,
    pub station_id: Uuid,
    pub action: ScheduleAction,
    pub recurrence: Recurrence,
    pub start_at: NaiveDateTime,
    pub end_at: Option<NaiveDateTime>,
    pub finish_action: FinishAction,
    pub enabled: bool,
    /// UTC start of the occurrence that last fired (or was deliberately
    /// skipped), so an occurrence can never fire twice.
    pub last_fired_at: Option<DateTime<Utc>>,
}

impl Schedule {
    pub fn new(station_id: Uuid, now_local: NaiveDateTime) -> Self {
        let start = round_up_to_minute(now_local);
        Self {
            id: Uuid::new_v4(),
            station_id,
            action: ScheduleAction::Play,
            recurrence: Recurrence::Once,
            start_at: start,
            end_at: Some(start + Duration::hours(1)),
            finish_action: FinishAction::None,
            enabled: true,
            last_fired_at: None,
        }
    }

    /// Length of the active window. Equal start and end times mean 24 hours;
    /// an end earlier than the start means the window crosses midnight.
    pub fn window(&self) -> Option<Duration> {
        self.end_at.map(|end| {
            let start = self.start_at.time().num_seconds_from_midnight() as i64;
            let finish = end.time().num_seconds_from_midnight() as i64;
            let mut seconds = finish - start;
            if seconds <= 0 {
                seconds += 24 * 60 * 60;
            }
            Duration::seconds(seconds)
        })
    }
}

/// One concrete occurrence of a schedule, in UTC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Occurrence {
    pub start: DateTime<Utc>,
    pub end: Option<DateTime<Utc>>,
}

/// The result of asking whether a schedule is due.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Due {
    /// The occurrence is still inside its window; join it late.
    Fire(Occurrence),
    /// The occurrence has passed. Advance `last_fired_at` so it cannot fire
    /// later, but run no effects.
    Skip(DateTime<Utc>),
    Nothing,
}

/// Evaluate one schedule against an injected clock.
///
/// `resolve` maps a naive local time to UTC. Production passes
/// [`resolve_local`]; tests pass a fixed offset or a hand-built DST resolver.
pub fn evaluate(
    schedule: &Schedule,
    now: DateTime<Utc>,
    resolve: &dyn Fn(NaiveDateTime) -> DateTime<Utc>,
) -> Due {
    if !schedule.enabled {
        return Due::Nothing;
    }
    let Some(occurrence) = latest_due_occurrence(schedule, now, resolve) else {
        return Due::Nothing;
    };
    match occurrence.end {
        Some(end) if now <= end => Due::Fire(occurrence),
        Some(_) => Due::Skip(occurrence.start),
        None if now - occurrence.start <= Duration::minutes(START_GRACE_MINUTES) => {
            Due::Fire(occurrence)
        }
        None => Due::Skip(occurrence.start),
    }
}

/// The earliest occurrence strictly after `now`, for the countdown display.
pub fn next_occurrence(
    schedule: &Schedule,
    now: DateTime<Utc>,
    resolve: &dyn Fn(NaiveDateTime) -> DateTime<Utc>,
) -> Option<Occurrence> {
    if !schedule.enabled {
        return None;
    }
    let anchor = schedule.start_at.date();
    let mut best: Option<Occurrence> = None;
    for date in candidate_dates(
        &schedule.recurrence,
        anchor,
        now.date_naive(),
        1,
        LOOKBACK_DAYS + 2,
    ) {
        if !schedule.recurrence.matches(date, anchor) {
            continue;
        }
        let occurrence = occurrence_bounds(schedule, date, resolve);
        if occurrence.start <= now {
            continue;
        }
        if best
            .as_ref()
            .map(|best| occurrence.start < best.start)
            .unwrap_or(true)
        {
            best = Some(occurrence);
        }
    }
    best
}

/// The soonest upcoming occurrence across every enabled schedule.
pub fn next_schedule<'a>(
    schedules: &'a [Schedule],
    now: DateTime<Utc>,
    resolve: &dyn Fn(NaiveDateTime) -> DateTime<Utc>,
) -> Option<(&'a Schedule, Occurrence)> {
    schedules
        .iter()
        .filter_map(|schedule| next_occurrence(schedule, now, resolve).map(|occurrence| (schedule, occurrence)))
        .min_by_key(|(_, occurrence)| occurrence.start)
}

/// The occurrence whose start is exactly `last_fired_at`, if it can still be
/// located in the current time zone. Used to resume a window after a restart.
pub fn last_fired_occurrence(
    schedule: &Schedule,
    resolve: &dyn Fn(NaiveDateTime) -> DateTime<Utc>,
) -> Option<Occurrence> {
    let last = schedule.last_fired_at?;
    let anchor = schedule.start_at.date();
    let utc_date = last.date_naive();
    for offset in [-1, 0, 1] {
        let date = utc_date + Duration::days(offset);
        if !schedule.recurrence.matches(date, anchor) {
            continue;
        }
        let occurrence = occurrence_bounds(schedule, date, resolve);
        if occurrence.start == last {
            return Some(occurrence);
        }
    }
    None
}

fn latest_due_occurrence(
    schedule: &Schedule,
    now: DateTime<Utc>,
    resolve: &dyn Fn(NaiveDateTime) -> DateTime<Utc>,
) -> Option<Occurrence> {
    let anchor = schedule.start_at.date();
    let mut best: Option<Occurrence> = None;
    for date in candidate_dates(
        &schedule.recurrence,
        anchor,
        now.date_naive(),
        LOOKBACK_DAYS + 1,
        1,
    ) {
        if !schedule.recurrence.matches(date, anchor) {
            continue;
        }
        let occurrence = occurrence_bounds(schedule, date, resolve);
        if occurrence.start > now {
            continue;
        }
        if let Some(last) = schedule.last_fired_at {
            if occurrence.start <= last {
                continue;
            }
        }
        if best
            .as_ref()
            .map(|best| occurrence.start > best.start)
            .unwrap_or(true)
        {
            best = Some(occurrence);
        }
    }
    best
}

/// Resolve one local occurrence to UTC.
///
/// The end is resolved as a local time, not as `start + duration`: on a DST day
/// the elapsed time between 01:00 and 03:00 is one or three hours, but the user
/// still means 03:00. An end at or before the start crosses midnight, so the end
/// is resolved on the next local date.
fn occurrence_bounds(
    schedule: &Schedule,
    date: NaiveDate,
    resolve: &dyn Fn(NaiveDateTime) -> DateTime<Utc>,
) -> Occurrence {
    let local_start = NaiveDateTime::new(date, schedule.start_at.time());
    let start = resolve(local_start);
    let end = schedule.end_at.map(|end| {
        let end_date = if end.time() <= schedule.start_at.time() {
            date + Duration::days(1)
        } else {
            date
        };
        let local_end = NaiveDateTime::new(end_date, end.time());
        let resolved_end = resolve(local_end);
        // A short window entirely inside a spring-forward gap collapses to a
        // zero-length instant. Keep the user's local length instead.
        if resolved_end <= start && local_end > local_start {
            start + (local_end - local_start)
        } else {
            resolved_end
        }
    });
    Occurrence { start, end }
}

fn candidate_dates(
    recurrence: &Recurrence,
    anchor: NaiveDate,
    center: NaiveDate,
    back_days: i64,
    forward_days: i64,
) -> Vec<NaiveDate> {
    if matches!(recurrence, Recurrence::Once) {
        return vec![anchor];
    }
    let start = (center - Duration::days(back_days)).max(anchor);
    let end = center + Duration::days(forward_days);
    if end < start {
        return Vec::new();
    }
    let mut dates = Vec::new();
    let mut date = start;
    while date <= end {
        dates.push(date);
        date += Duration::days(1);
    }
    dates
}

// ---------------------------------------------------------------------------
// Local time resolution
// ---------------------------------------------------------------------------

/// Resolve a naive local time to UTC using the system zone.
///
/// A time that does not exist (spring forward) fires at the first valid instant
/// after the gap; an ambiguous time (fall back) uses the earlier instant.
pub fn resolve_local(time: NaiveDateTime) -> DateTime<Utc> {
    resolve_with(time, |probe| {
        Local
            .from_local_datetime(&probe)
            .map(|value| value.fixed_offset())
    })
}

fn resolve_with<F>(time: NaiveDateTime, lookup: F) -> DateTime<Utc>
where
    F: Fn(NaiveDateTime) -> LocalResult<DateTime<FixedOffset>>,
{
    match lookup(time) {
        LocalResult::Single(value) => value.with_timezone(&Utc),
        LocalResult::Ambiguous(earlier, _later) => earlier.with_timezone(&Utc),
        LocalResult::None => {
            // Walk forward to the first minute that exists. DST gaps are at most
            // a couple of hours; six is a generous safety bound.
            let mut probe = time;
            for _ in 0..(6 * 60) {
                probe += Duration::minutes(1);
                match lookup(probe) {
                    LocalResult::Single(value) => return value.with_timezone(&Utc),
                    LocalResult::Ambiguous(earlier, _) => return earlier.with_timezone(&Utc),
                    LocalResult::None => {}
                }
            }
            Utc.from_utc_datetime(&time)
        }
    }
}

fn round_up_to_minute(time: NaiveDateTime) -> NaiveDateTime {
    let time = time + Duration::minutes(1);
    time.with_second(0).unwrap_or(time).with_nanosecond(0).unwrap_or(time)
}

// ---------------------------------------------------------------------------
// Runtime: active windows and finish actions
// ---------------------------------------------------------------------------

/// A schedule that has fired and whose window is still open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveWindow {
    pub schedule_id: Uuid,
    pub station_id: Uuid,
    pub ends_at: DateTime<Utc>,
    pub finish: FinishAction,
    /// Set once the app has started (or adopted) the stream.
    pub playback_id: Option<PlaybackId>,
    /// True only when this schedule opened the stream. A schedule that found
    /// the station already playing must not stop the user's stream at the end.
    pub owns_stream: bool,
    /// True only when this schedule started the recording tap.
    pub owns_recording: bool,
}

/// Something the app must do after a tick. Effects are ordered: a `Stop` that
/// ends a window comes before its `Finish`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchedulerEffect {
    Start {
        schedule_id: Uuid,
        station_id: Uuid,
        action: ScheduleAction,
    },
    Stop {
        schedule_id: Uuid,
        station_id: Uuid,
        playback_id: Option<PlaybackId>,
        owns_stream: bool,
        owns_recording: bool,
    },
    Finish {
        schedule_id: Uuid,
        action: FinishAction,
    },
}

#[derive(Debug, Clone, Default)]
pub struct TickResult {
    pub effects: Vec<SchedulerEffect>,
    /// Occurrences the app must persist as `last_fired_at`, including skipped
    /// ones, so nothing fires twice.
    pub marks: Vec<(Uuid, DateTime<Utc>)>,
}

pub type CommandRunner = Box<dyn FnMut(&str) -> io::Result<()>>;

/// Owns the in-memory active windows and the side effects of finish actions.
pub struct SchedulerRuntime {
    active: Vec<ActiveWindow>,
    pub pending_close: bool,
    pub message: Option<String>,
    runner: CommandRunner,
}

impl SchedulerRuntime {
    pub fn new() -> Self {
        Self::with_runner(Box::new(run_command))
    }

    pub fn with_runner(runner: CommandRunner) -> Self {
        Self {
            active: Vec::new(),
            pending_close: false,
            message: None,
            runner,
        }
    }

    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    /// Record which stream a `Start` effect actually opened, and whether the
    /// schedule owns it (as opposed to adopting an already-playing station).
    pub fn attach(
        &mut self,
        schedule_id: Uuid,
        playback_id: Option<PlaybackId>,
        owns_stream: bool,
        owns_recording: bool,
    ) {
        if let Some(window) = self
            .active
            .iter_mut()
            .find(|window| window.schedule_id == schedule_id)
        {
            window.playback_id = playback_id;
            window.owns_stream = owns_stream;
            window.owns_recording = owns_recording;
        }
    }

    /// Remove an active window because its schedule was disabled or deleted.
    /// The returned `Stop` lets the app end the stream/recording it opened; the
    /// finish action is deliberately not run.
    pub fn cancel(&mut self, schedule_id: Uuid) -> Option<SchedulerEffect> {
        let position = self
            .active
            .iter()
            .position(|window| window.schedule_id == schedule_id)?;
        let window = self.active.remove(position);
        let mut owns_stream = window.owns_stream;
        let mut owns_recording = window.owns_recording;
        if let Some(id) = window.playback_id {
            if let Some(other) = self
                .active
                .iter_mut()
                .find(|other| other.playback_id == Some(id))
            {
                other.owns_stream |= owns_stream;
                other.owns_recording |= owns_recording;
                owns_stream = false;
                owns_recording = false;
            }
        }
        Some(SchedulerEffect::Stop {
            schedule_id: window.schedule_id,
            station_id: window.station_id,
            playback_id: window.playback_id,
            owns_stream,
            owns_recording,
        })
    }

    /// Evaluate every schedule and close windows that have ended.
    pub fn tick(
        &mut self,
        schedules: &[Schedule],
        known_stations: &HashSet<Uuid>,
        now: DateTime<Utc>,
        resolve: &dyn Fn(NaiveDateTime) -> DateTime<Utc>,
    ) -> TickResult {
        let mut result = TickResult::default();

        let mut windows: Vec<ActiveWindow> = self.active.drain(..).collect();
        let mut still_active = Vec::new();
        let mut ending = Vec::new();
        for window in windows.drain(..) {
            if window.ends_at <= now {
                ending.push(window);
            } else {
                still_active.push(window);
            }
        }
        // Two schedules can share one stream (B adopted A's stream). When A
        // ends, hand its ownership to B instead of stopping the stream B uses.
        for window in &ending {
            if let Some(id) = window.playback_id {
                if let Some(other) = still_active
                    .iter_mut()
                    .find(|other| other.playback_id == Some(id))
                {
                    other.owns_stream |= window.owns_stream;
                    other.owns_recording |= window.owns_recording;
                }
            }
        }
        for window in ending {
            let transferred = window.playback_id.is_some_and(|id| {
                still_active
                    .iter()
                    .any(|other| other.playback_id == Some(id))
            });
            result.effects.push(SchedulerEffect::Stop {
                schedule_id: window.schedule_id,
                station_id: window.station_id,
                playback_id: window.playback_id,
                owns_stream: window.owns_stream && !transferred,
                owns_recording: window.owns_recording && !transferred,
            });
            if now - window.ends_at > Duration::minutes(FINISH_GRACE_MINUTES) {
                self.message = Some(crate::i18n::t("msg.schedule_finish_skipped").to_string());
            } else {
                result.effects.push(SchedulerEffect::Finish {
                    schedule_id: window.schedule_id,
                    action: window.finish,
                });
            }
        }
        self.active = still_active;

        for schedule in schedules {
            if !schedule.enabled {
                continue;
            }
            if self
                .active
                .iter()
                .any(|window| window.schedule_id == schedule.id)
            {
                continue;
            }
            match evaluate(schedule, now, resolve) {
                Due::Fire(occurrence) => {
                    result.marks.push((schedule.id, occurrence.start));
                    if !known_stations.contains(&schedule.station_id) {
                        self.message =
                            Some("Scheduled station is no longer in the library".to_string());
                        continue;
                    }
                    result.effects.push(SchedulerEffect::Start {
                        schedule_id: schedule.id,
                        station_id: schedule.station_id,
                        action: schedule.action,
                    });
                    if let Some(end) = occurrence.end {
                        self.active.push(ActiveWindow {
                            schedule_id: schedule.id,
                            station_id: schedule.station_id,
                            ends_at: end,
                            finish: schedule.finish_action.clone(),
                            playback_id: None,
                            owns_stream: false,
                            owns_recording: false,
                        });
                    }
                }
                Due::Skip(start) => result.marks.push((schedule.id, start)),
                Due::Nothing => {
                    // The app may have been restarted inside a window that
                    // already fired: the marker suppresses a second fire, but the
                    // stream should still be resumed until the window ends.
                    let Some(occurrence) = last_fired_occurrence(schedule, resolve) else {
                        continue;
                    };
                    let Some(end) = occurrence.end else {
                        continue;
                    };
                    if end <= now {
                        continue;
                    }
                    if !known_stations.contains(&schedule.station_id) {
                        // The initial Fire path already reported this once; do
                        // not repeat it every tick while the window is open.
                        continue;
                    }
                    result.effects.push(SchedulerEffect::Start {
                        schedule_id: schedule.id,
                        station_id: schedule.station_id,
                        action: schedule.action,
                    });
                    self.active.push(ActiveWindow {
                        schedule_id: schedule.id,
                        station_id: schedule.station_id,
                        ends_at: end,
                        finish: schedule.finish_action.clone(),
                        playback_id: None,
                        owns_stream: false,
                        owns_recording: false,
                    });
                }
            }
        }

        result
    }

    /// Run a finish action. `CloseApp` sets the flag the app turns into a
    /// viewport close after streams have stopped.
    pub fn apply_finish(&mut self, action: &FinishAction) {
        match action {
            FinishAction::None => {}
            FinishAction::CloseApp => self.pending_close = true,
            FinishAction::RunCommand(command) => {
                if command.trim().is_empty() {
                    self.message = Some(crate::i18n::t("msg.schedule_command_empty").to_string());
                    return;
                }
                match (self.runner)(command) {
                    Ok(()) => {
                        self.message = Some(crate::i18n::tf(
                            "msg.schedule_command_ran",
                            &[command],
                        ))
                    }
                    Err(e) => {
                        self.message = Some(crate::i18n::tf(
                            "msg.schedule_command_failed",
                            &[&e.to_string()],
                        ))
                    }
                }
            }
        }
    }
}

impl Default for SchedulerRuntime {
    fn default() -> Self {
        Self::new()
    }
}

fn run_command(command: &str) -> io::Result<()> {
    #[cfg(windows)]
    let mut child = {
        let mut command_line = Command::new("cmd");
        command_line.args(["/C", command]);
        command_line
    };
    #[cfg(not(windows))]
    let mut child = {
        let mut command_line = Command::new("sh");
        command_line.args(["-c", command]);
        command_line
    };
    let mut child = child.spawn()?;
    // Reap the child so a long-running command does not leave a zombie.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// Editor draft
// ---------------------------------------------------------------------------

/// String-backed schedule form. Parsing and validation live here so the window
/// code stays a thin renderer and the rules are testable without egui.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleDraft {
    pub station_id: Option<Uuid>,
    pub action: ScheduleAction,
    pub recurrence: Recurrence,
    pub start_date: String,
    pub start_time: String,
    /// Empty means start-only (no window).
    pub end_time: String,
    pub finish: FinishAction,
    pub enabled: bool,
}

impl ScheduleDraft {
    pub fn new(station_id: Option<Uuid>, now_local: NaiveDateTime) -> Self {
        let schedule = Schedule::new(station_id.unwrap_or_else(Uuid::nil), now_local);
        Self {
            station_id,
            action: schedule.action,
            recurrence: schedule.recurrence,
            start_date: schedule.start_at.format("%Y-%m-%d").to_string(),
            start_time: schedule.start_at.format("%H:%M").to_string(),
            end_time: schedule
                .end_at
                .map(|end| end.format("%H:%M").to_string())
                .unwrap_or_default(),
            finish: schedule.finish_action,
            enabled: schedule.enabled,
        }
    }

    pub fn from_schedule(schedule: &Schedule) -> Self {
        Self {
            station_id: Some(schedule.station_id),
            action: schedule.action,
            recurrence: schedule.recurrence.clone(),
            start_date: schedule.start_at.format("%Y-%m-%d").to_string(),
            start_time: schedule.start_at.format("%H:%M").to_string(),
            end_time: schedule
                .end_at
                .map(|end| end.format("%H:%M").to_string())
                .unwrap_or_default(),
            finish: schedule.finish_action.clone(),
            enabled: schedule.enabled,
        }
    }

    /// Validate the form and build the persisted shape. `now_local` is used only
    /// to give a clear error for a one-time window that has already closed.
    pub fn to_schedule(&self, id: Uuid, now_local: NaiveDateTime) -> Result<Schedule, String> {
        let station_id = self.station_id
            .ok_or_else(|| crate::i18n::t("schedule.error.station").to_string())?;
        let date = NaiveDate::parse_from_str(self.start_date.trim(), "%Y-%m-%d")
            .map_err(|_| crate::i18n::t("schedule.error.start_date").to_string())?;
        let start_time = NaiveTime::parse_from_str(self.start_time.trim(), "%H:%M")
            .map_err(|_| crate::i18n::t("schedule.error.start_time").to_string())?;
        let start_at = NaiveDateTime::new(date, start_time);
        let end_at = if self.end_time.trim().is_empty() {
            None
        } else {
            let end_time = NaiveTime::parse_from_str(self.end_time.trim(), "%H:%M")
                .map_err(|_| crate::i18n::t("schedule.error.end_time").to_string())?;
            Some(NaiveDateTime::new(date, end_time))
        };
        if self.finish.requires_end() && end_at.is_none() {
            return Err(crate::i18n::t("schedule.error.finish_needs_end").to_string());
        }
        if let Recurrence::Weekly(days) = &self.recurrence {
            if days.is_empty() {
                return Err(crate::i18n::t("schedule.error.weekday").to_string());
            }
        }
        let schedule = Schedule {
            id,
            station_id,
            action: self.action,
            recurrence: self.recurrence.clone(),
            start_at,
            end_at,
            finish_action: self.finish.clone(),
            enabled: self.enabled,
            last_fired_at: None,
        };
        if matches!(self.recurrence, Recurrence::Once) {
            let grace = schedule
                .window()
                .unwrap_or_else(|| Duration::minutes(START_GRACE_MINUTES));
            if start_at + grace < now_local {
                return Err(crate::i18n::t("schedule.error.passed").to_string());
            }
        }
        Ok(schedule)
    }
}

/// Human-readable countdown, e.g. `1h 12m`, `3d 4h`, `2m`.
pub fn format_countdown(remaining: Duration) -> String {
    let seconds = remaining.num_seconds().max(0);
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;
    if days > 0 {
        crate::i18n::tf(
            "schedule.countdown.days",
            &[&days.to_string(), &hours.to_string()],
        )
    } else if hours > 0 {
        crate::i18n::tf(
            "schedule.countdown.hours",
            &[&hours.to_string(), &minutes.to_string()],
        )
    } else {
        crate::i18n::tf(
            "schedule.countdown.minutes",
            &[&minutes.max(1).to_string()],
        )
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .unwrap()
    }

    fn naive(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(year, month, day)
            .unwrap()
            .and_hms_opt(hour, minute, 0)
            .unwrap()
    }

    fn resolve_utc(time: NaiveDateTime) -> DateTime<Utc> {
        Utc.from_utc_datetime(&time)
    }

    fn schedule(
        recurrence: Recurrence,
        start: NaiveDateTime,
        end: Option<NaiveDateTime>,
    ) -> Schedule {
        Schedule {
            id: Uuid::new_v4(),
            station_id: Uuid::new_v4(),
            action: ScheduleAction::Play,
            recurrence,
            start_at: start,
            end_at: end,
            finish_action: FinishAction::None,
            enabled: true,
            last_fired_at: None,
        }
    }

    #[test]
    fn once_fires_then_never_again() {
        let mut value = schedule(
            Recurrence::Once,
            naive(2026, 11, 8, 7, 0),
            Some(naive(2026, 11, 8, 8, 0)),
        );
        let now = utc(2026, 11, 8, 7, 30);
        match evaluate(&value, now, &resolve_utc) {
            Due::Fire(occurrence) => {
                assert_eq!(occurrence.start, utc(2026, 11, 8, 7, 0));
                assert_eq!(occurrence.end, Some(utc(2026, 11, 8, 8, 0)));
                value.last_fired_at = Some(occurrence.start);
            }
            other => panic!("expected fire, got {other:?}"),
        }
        assert_eq!(evaluate(&value, now, &resolve_utc), Due::Nothing);
    }

    #[test]
    fn daily_next_occurrence_is_today() {
        let value = schedule(
            Recurrence::Daily,
            naive(2024, 1, 1, 7, 0),
            Some(naive(2024, 1, 1, 8, 0)),
        );
        let next = next_occurrence(&value, utc(2026, 11, 8, 6, 0), &resolve_utc).unwrap();
        assert_eq!(next.start, utc(2026, 11, 8, 7, 0));
    }

    #[test]
    fn weekly_filters_to_selected_weekdays() {
        // 2024-01-01 was a Monday.
        let value = schedule(
            Recurrence::Weekly(vec![Weekday::Mon, Weekday::Wed]),
            naive(2024, 1, 1, 7, 0),
            Some(naive(2024, 1, 1, 8, 0)),
        );
        let next = next_occurrence(&value, utc(2024, 1, 2, 12, 0), &resolve_utc).unwrap();
        assert_eq!(next.start, utc(2024, 1, 3, 7, 0));
        assert_eq!(next.start.weekday(), Weekday::Wed);
    }

    #[test]
    fn window_crosses_midnight() {
        let value = schedule(
            Recurrence::Once,
            naive(2024, 1, 1, 23, 0),
            Some(naive(2024, 1, 1, 1, 0)),
        );
        assert_eq!(value.window(), Some(Duration::hours(2)));
        match evaluate(&value, utc(2024, 1, 2, 0, 30), &resolve_utc) {
            Due::Fire(occurrence) => {
                assert_eq!(occurrence.start, utc(2024, 1, 1, 23, 0));
                assert_eq!(occurrence.end, Some(utc(2024, 1, 2, 1, 0)));
            }
            other => panic!("expected fire, got {other:?}"),
        }
    }

    #[test]
    fn weekly_window_crosses_midnight() {
        // Monday 23:00-01:00. At Tuesday 00:30 the Monday window is still open.
        let value = schedule(
            Recurrence::Weekly(vec![Weekday::Mon]),
            naive(2024, 1, 1, 23, 0),
            Some(naive(2024, 1, 1, 1, 0)),
        );
        match evaluate(&value, utc(2024, 1, 2, 0, 30), &resolve_utc) {
            Due::Fire(occurrence) => {
                assert_eq!(occurrence.start, utc(2024, 1, 1, 23, 0));
                assert_eq!(occurrence.end, Some(utc(2024, 1, 2, 1, 0)));
            }
            other => panic!("expected fire, got {other:?}"),
        }
        let next = next_occurrence(&value, utc(2024, 1, 2, 2, 0), &resolve_utc).unwrap();
        assert_eq!(next.start, utc(2024, 1, 8, 23, 0));
    }

    #[test]
    fn dst_gap_resolves_to_first_valid_instant() {
        let value = schedule(
            Recurrence::Once,
            naive(2024, 3, 10, 2, 30),
            Some(naive(2024, 3, 10, 4, 30)),
        );
        let resolve = |time: NaiveDateTime| {
            let shifted = if time.time() >= NaiveTime::from_hms_opt(2, 0, 0).unwrap()
                && time.time() < NaiveTime::from_hms_opt(3, 0, 0).unwrap()
            {
                time + Duration::hours(1)
            } else {
                time
            };
            Utc.from_utc_datetime(&shifted)
        };
        match evaluate(&value, utc(2024, 3, 10, 3, 45), &resolve) {
            Due::Fire(occurrence) => {
                assert_eq!(occurrence.start, utc(2024, 3, 10, 3, 30));
                assert_eq!(occurrence.end, Some(utc(2024, 3, 10, 4, 30)));
            }
            other => panic!("expected fire, got {other:?}"),
        }
    }

    #[test]
    fn window_end_is_resolved_locally_across_dst() {
        // Spring forward: local 01:00 is UTC-5 (06:00Z), local 03:00 is UTC-4
        // (07:00Z). Only one hour elapses, but the user's 01:00-03:00 window
        // must end at 03:00 local, not at 04:00.
        let spring = schedule(
            Recurrence::Once,
            naive(2024, 3, 10, 1, 0),
            Some(naive(2024, 3, 10, 3, 0)),
        );
        let resolve_spring = |time: NaiveDateTime| {
            let offset = if time.time() < NaiveTime::from_hms_opt(2, 0, 0).unwrap() {
                -5
            } else {
                -4
            };
            Utc.from_utc_datetime(&(time - Duration::hours(offset)))
        };
        match evaluate(&spring, utc(2024, 3, 10, 6, 30), &resolve_spring) {
            Due::Fire(occurrence) => {
                assert_eq!(occurrence.start, utc(2024, 3, 10, 6, 0));
                assert_eq!(occurrence.end, Some(utc(2024, 3, 10, 7, 0)));
            }
            other => panic!("expected fire, got {other:?}"),
        }

        // Fall back: local 00:30 is UTC-4 (04:30Z), local 02:30 is UTC-5
        // (07:30Z). Three hours elapse, but the window still ends at 02:30.
        let fall = schedule(
            Recurrence::Once,
            naive(2024, 11, 3, 0, 30),
            Some(naive(2024, 11, 3, 2, 30)),
        );
        let resolve_fall = |time: NaiveDateTime| {
            let offset = if time.time() < NaiveTime::from_hms_opt(1, 0, 0).unwrap() {
                -4
            } else {
                -5
            };
            Utc.from_utc_datetime(&(time - Duration::hours(offset)))
        };
        match evaluate(&fall, utc(2024, 11, 3, 5, 0), &resolve_fall) {
            Due::Fire(occurrence) => {
                assert_eq!(occurrence.start, utc(2024, 11, 3, 4, 30));
                assert_eq!(occurrence.end, Some(utc(2024, 11, 3, 7, 30)));
            }
            other => panic!("expected fire, got {other:?}"),
        }
    }

    #[test]
    fn short_window_inside_a_dst_gap_keeps_its_local_length() {
        let value = schedule(
            Recurrence::Once,
            naive(2024, 3, 10, 2, 30),
            Some(naive(2024, 3, 10, 2, 45)),
        );
        let resolve = |time: NaiveDateTime| {
            // Both 02:30 and 02:45 do not exist; the first valid instant is
            // 03:00, exactly as `resolve_with` walks the gap.
            let shifted = if time.time() >= NaiveTime::from_hms_opt(2, 0, 0).unwrap()
                && time.time() < NaiveTime::from_hms_opt(3, 0, 0).unwrap()
            {
                NaiveDateTime::new(
                    time.date(),
                    NaiveTime::from_hms_opt(3, 0, 0).unwrap(),
                )
            } else {
                time
            };
            Utc.from_utc_datetime(&shifted)
        };
        match evaluate(&value, utc(2024, 3, 10, 3, 5), &resolve) {
            Due::Fire(occurrence) => {
                assert_eq!(occurrence.start, utc(2024, 3, 10, 3, 0));
                assert_eq!(occurrence.end, Some(utc(2024, 3, 10, 3, 15)));
            }
            other => panic!("expected fire, got {other:?}"),
        }
    }

    #[test]
    fn resolve_with_skips_the_gap_and_prefers_the_earlier_fold() {
        let zero = FixedOffset::east_opt(0).unwrap();
        let gap = |time: NaiveDateTime| {
            if time.time() >= NaiveTime::from_hms_opt(2, 0, 0).unwrap()
                && time.time() < NaiveTime::from_hms_opt(3, 0, 0).unwrap()
            {
                LocalResult::None
            } else {
                LocalResult::Single(zero.from_local_datetime(&time).unwrap())
            }
        };
        assert_eq!(
            resolve_with(naive(2024, 3, 10, 2, 30), gap),
            utc(2024, 3, 10, 3, 0)
        );

        // On the fall-back day the same wall time occurs twice. The earlier
        // instant is the one with the still-active DST offset (+01:00), i.e.
        // 00:30 UTC, not 01:30 UTC.
        let earlier = FixedOffset::east_opt(3600)
            .unwrap()
            .with_ymd_and_hms(2024, 11, 3, 1, 30, 0)
            .unwrap();
        let later = FixedOffset::east_opt(0)
            .unwrap()
            .with_ymd_and_hms(2024, 11, 3, 1, 30, 0)
            .unwrap();
        let fold = move |_: NaiveDateTime| LocalResult::Ambiguous(earlier, later);
        assert_eq!(
            resolve_with(naive(2024, 11, 3, 1, 30), fold),
            earlier.with_timezone(&Utc)
        );
        assert_eq!(earlier.with_timezone(&Utc), utc(2024, 11, 3, 0, 30));
        assert_eq!(later.with_timezone(&Utc), utc(2024, 11, 3, 1, 30));
    }

    #[test]
    fn missed_occurrence_inside_its_window_joins_late() {
        let value = schedule(
            Recurrence::Daily,
            naive(2024, 1, 1, 7, 0),
            Some(naive(2024, 1, 1, 8, 0)),
        );
        assert!(matches!(
            evaluate(&value, utc(2026, 11, 8, 7, 45), &resolve_utc),
            Due::Fire(_)
        ));
    }

    #[test]
    fn missed_occurrence_outside_its_window_is_skipped_and_marked() {
        let value = schedule(
            Recurrence::Daily,
            naive(2024, 1, 1, 7, 0),
            Some(naive(2024, 1, 1, 8, 0)),
        );
        assert_eq!(
            evaluate(&value, utc(2026, 11, 8, 9, 0), &resolve_utc),
            Due::Skip(utc(2026, 11, 8, 7, 0))
        );
    }

    #[test]
    fn start_only_schedule_uses_the_grace_period() {
        let value = schedule(Recurrence::Daily, naive(2024, 1, 1, 7, 0), None);
        assert!(matches!(
            evaluate(&value, utc(2026, 11, 8, 7, 4), &resolve_utc),
            Due::Fire(_)
        ));
        assert_eq!(
            evaluate(&value, utc(2026, 11, 8, 7, 6), &resolve_utc),
            Due::Skip(utc(2026, 11, 8, 7, 0))
        );
    }

    #[test]
    fn a_clock_that_steps_backwards_cannot_refire() {
        let mut value = schedule(
            Recurrence::Daily,
            naive(2024, 1, 1, 7, 0),
            Some(naive(2024, 1, 1, 8, 0)),
        );
        value.last_fired_at = Some(utc(2026, 11, 8, 7, 0));
        assert_eq!(
            evaluate(&value, utc(2026, 11, 8, 6, 30), &resolve_utc),
            Due::Nothing
        );
    }

    #[test]
    fn disabled_schedules_do_not_fire() {
        let mut value = schedule(
            Recurrence::Daily,
            naive(2024, 1, 1, 7, 0),
            Some(naive(2024, 1, 1, 8, 0)),
        );
        value.enabled = false;
        assert_eq!(
            evaluate(&value, utc(2026, 11, 8, 7, 30), &resolve_utc),
            Due::Nothing
        );
    }

    #[test]
    fn runtime_marks_and_starts_then_stops_and_finishes() {
        let station_id = Uuid::new_v4();
        let mut value = schedule(
            Recurrence::Once,
            naive(2026, 11, 8, 7, 0),
            Some(naive(2026, 11, 8, 8, 0)),
        );
        value.station_id = station_id;
        value.finish_action = FinishAction::None;
        let known = HashSet::from([station_id]);
        let mut runtime = SchedulerRuntime::new();

        let first = runtime.tick(&[value.clone()], &known, utc(2026, 11, 8, 7, 30), &resolve_utc);
        assert_eq!(first.marks, vec![(value.id, utc(2026, 11, 8, 7, 0))]);
        assert!(matches!(
            first.effects.as_slice(),
            [SchedulerEffect::Start { station_id: id, action: ScheduleAction::Play, .. }] if *id == station_id
        ));
        assert_eq!(runtime.active_count(), 1);
        runtime.attach(value.id, Some(42), true, false);

        value.last_fired_at = Some(utc(2026, 11, 8, 7, 0));
        let second = runtime.tick(&[value], &known, utc(2026, 11, 8, 8, 0), &resolve_utc);
        assert!(matches!(
            second.effects.as_slice(),
            [
                SchedulerEffect::Stop {
                    station_id: stopped,
                    playback_id: Some(42),
                    owns_stream: true,
                    owns_recording: false,
                    ..
                },
                SchedulerEffect::Finish { action: FinishAction::None, .. }
            ] if *stopped == station_id
        ));
        assert_eq!(runtime.active_count(), 0);
    }

    #[test]
    fn runtime_resumes_an_open_window_after_restart() {
        let station_id = Uuid::new_v4();
        let mut value = schedule(
            Recurrence::Once,
            naive(2026, 11, 8, 7, 0),
            Some(naive(2026, 11, 8, 8, 0)),
        );
        value.station_id = station_id;
        value.last_fired_at = Some(utc(2026, 11, 8, 7, 0));
        let known = HashSet::from([station_id]);

        let mut runtime = SchedulerRuntime::new();
        let result = runtime.tick(&[value.clone()], &known, utc(2026, 11, 8, 7, 30), &resolve_utc);
        assert!(result.marks.is_empty(), "a resumed window is not marked again");
        assert!(matches!(
            result.effects.as_slice(),
            [SchedulerEffect::Start { .. }]
        ));
        assert_eq!(runtime.active_count(), 1);

        runtime.attach(value.id, Some(7), true, false);
        let end = runtime.tick(&[], &known, utc(2026, 11, 8, 8, 0), &resolve_utc);
        assert!(matches!(
            end.effects.as_slice(),
            [SchedulerEffect::Stop { playback_id: Some(7), .. }, SchedulerEffect::Finish { .. }]
        ));
    }

    #[test]
    fn runtime_does_not_resume_a_closed_or_start_only_window() {
        let station_id = Uuid::new_v4();
        let known = HashSet::from([station_id]);

        let mut ended = schedule(
            Recurrence::Once,
            naive(2026, 11, 8, 7, 0),
            Some(naive(2026, 11, 8, 8, 0)),
        );
        ended.station_id = station_id;
        ended.last_fired_at = Some(utc(2026, 11, 8, 7, 0));
        let mut runtime = SchedulerRuntime::new();
        let result = runtime.tick(&[ended], &known, utc(2026, 11, 8, 9, 0), &resolve_utc);
        assert!(result.effects.is_empty());
        assert_eq!(runtime.active_count(), 0);

        let mut start_only = schedule(Recurrence::Once, naive(2026, 11, 8, 7, 0), None);
        start_only.station_id = station_id;
        start_only.last_fired_at = Some(utc(2026, 11, 8, 7, 0));
        let result = runtime.tick(&[start_only], &known, utc(2026, 11, 8, 7, 2), &resolve_utc);
        assert!(result.effects.is_empty());
    }

    #[test]
    fn runtime_cancel_stops_only_what_the_schedule_owns() {
        let station_id = Uuid::new_v4();
        let mut value = schedule(
            Recurrence::Once,
            naive(2026, 11, 8, 7, 0),
            Some(naive(2026, 11, 8, 8, 0)),
        );
        value.station_id = station_id;
        value.action = ScheduleAction::PlayRecordContinuous;
        let known = HashSet::from([station_id]);
        let mut runtime = SchedulerRuntime::new();
        runtime.tick(&[value.clone()], &known, utc(2026, 11, 8, 7, 30), &resolve_utc);
        runtime.attach(value.id, Some(9), false, true);

        let stop = runtime.cancel(value.id).expect("active window");
        assert!(matches!(
            stop,
            SchedulerEffect::Stop {
                playback_id: Some(9),
                owns_stream: false,
                owns_recording: true,
                ..
            }
        ));
        assert_eq!(runtime.active_count(), 0);
        assert!(runtime.cancel(value.id).is_none());
    }

    #[test]
    fn runtime_hands_stream_ownership_to_an_overlapping_window() {
        let station_id = Uuid::new_v4();
        let mut first = schedule(
            Recurrence::Once,
            naive(2026, 11, 8, 7, 0),
            Some(naive(2026, 11, 8, 8, 0)),
        );
        first.station_id = station_id;
        let mut second = schedule(
            Recurrence::Once,
            naive(2026, 11, 8, 7, 0),
            Some(naive(2026, 11, 8, 9, 0)),
        );
        second.station_id = station_id;
        let known = HashSet::from([station_id]);
        let mut runtime = SchedulerRuntime::new();

        let start = runtime.tick(
            &[first.clone(), second.clone()],
            &known,
            utc(2026, 11, 8, 7, 30),
            &resolve_utc,
        );
        assert_eq!(start.effects.len(), 2);
        runtime.attach(first.id, Some(5), true, false);
        runtime.attach(second.id, Some(5), false, false);

        // First ends; second still needs the stream, so ownership moves.
        let first_end = runtime.tick(&[], &known, utc(2026, 11, 8, 8, 0), &resolve_utc);
        assert!(matches!(
            first_end.effects.as_slice(),
            [SchedulerEffect::Stop { owns_stream: false, .. }, SchedulerEffect::Finish { .. }]
        ));
        assert_eq!(runtime.active_count(), 1);

        let second_end = runtime.tick(&[], &known, utc(2026, 11, 8, 9, 0), &resolve_utc);
        assert!(matches!(
            second_end.effects.as_slice(),
            [
                SchedulerEffect::Stop {
                    playback_id: Some(5),
                    owns_stream: true,
                    ..
                },
                SchedulerEffect::Finish { .. }
            ]
        ));
    }

    #[test]
    fn runtime_cancel_hands_ownership_to_an_overlapping_window() {
        let station_id = Uuid::new_v4();
        let mut first = schedule(
            Recurrence::Once,
            naive(2026, 11, 8, 7, 0),
            Some(naive(2026, 11, 8, 8, 0)),
        );
        first.station_id = station_id;
        let mut second = schedule(
            Recurrence::Once,
            naive(2026, 11, 8, 7, 0),
            Some(naive(2026, 11, 8, 9, 0)),
        );
        second.station_id = station_id;
        let known = HashSet::from([station_id]);
        let mut runtime = SchedulerRuntime::new();
        runtime.tick(
            &[first.clone(), second.clone()],
            &known,
            utc(2026, 11, 8, 7, 30),
            &resolve_utc,
        );
        runtime.attach(first.id, Some(5), true, false);
        runtime.attach(second.id, Some(5), false, false);

        assert!(matches!(
            runtime.cancel(first.id),
            Some(SchedulerEffect::Stop { owns_stream: false, .. })
        ));
        assert!(matches!(
            runtime.cancel(second.id),
            Some(SchedulerEffect::Stop { owns_stream: true, .. })
        ));
    }

    #[test]
    fn runtime_skips_a_finish_action_that_is_hours_late() {
        let station_id = Uuid::new_v4();
        let mut value = schedule(
            Recurrence::Once,
            naive(2026, 11, 8, 7, 0),
            Some(naive(2026, 11, 8, 8, 0)),
        );
        value.station_id = station_id;
        value.finish_action = FinishAction::CloseApp;
        let known = HashSet::from([station_id]);
        let mut runtime = SchedulerRuntime::new();
        runtime.tick(&[value.clone()], &known, utc(2026, 11, 8, 7, 30), &resolve_utc);
        runtime.attach(value.id, Some(3), true, false);

        // Resumed two hours after the window closed: stop, but do not close.
        let result = runtime.tick(&[], &known, utc(2026, 11, 8, 10, 0), &resolve_utc);
        assert!(matches!(
            result.effects.as_slice(),
            [SchedulerEffect::Stop { .. }]
        ));
        assert!(!runtime.pending_close);
        assert!(runtime.message.as_deref().unwrap().contains("finish skipped"));
    }

    #[test]
    fn runtime_skips_a_missing_station_but_marks_it() {
        let value = schedule(
            Recurrence::Once,
            naive(2026, 11, 8, 7, 0),
            Some(naive(2026, 11, 8, 8, 0)),
        );
        let mut runtime = SchedulerRuntime::new();
        let result = runtime.tick(
            &[value.clone()],
            &HashSet::new(),
            utc(2026, 11, 8, 7, 30),
            &resolve_utc,
        );
        assert_eq!(result.marks, vec![(value.id, utc(2026, 11, 8, 7, 0))]);
        assert!(result.effects.is_empty());
        assert!(runtime.message.as_deref().unwrap().contains("no longer"));
    }

    #[test]
    fn finish_actions_close_or_run_a_recorded_command() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let recorder = std::sync::Arc::clone(&seen);
        let mut runtime = SchedulerRuntime::with_runner(Box::new(move |command| {
            recorder.lock().unwrap().push(command.to_string());
            Ok(())
        }));

        runtime.apply_finish(&FinishAction::CloseApp);
        assert!(runtime.pending_close);

        runtime.apply_finish(&FinishAction::RunCommand("echo scheduled".to_string()));
        assert_eq!(seen.lock().unwrap().as_slice(), ["echo scheduled"]);
        assert!(runtime.message.as_deref().unwrap().contains("Ran scheduled"));
    }

    #[test]
    fn draft_validates_and_round_trips() {
        let station_id = Uuid::new_v4();
        let mut draft = ScheduleDraft::new(Some(station_id), naive(2026, 11, 8, 6, 0));
        draft.start_time = "07:00".to_string();
        draft.finish = FinishAction::CloseApp;
        draft.end_time.clear();
        assert!(draft
            .to_schedule(Uuid::new_v4(), naive(2026, 11, 8, 6, 0))
            .is_err());

        draft.end_time = "07:30".to_string();
        draft.recurrence = Recurrence::Weekly(vec![Weekday::Mon, Weekday::Wed]);
        let value = draft
            .to_schedule(Uuid::new_v4(), naive(2026, 11, 8, 6, 0))
            .unwrap();
        assert_eq!(value.start_at, naive(2026, 11, 8, 7, 0));
        assert_eq!(value.end_at, Some(naive(2026, 11, 8, 7, 30)));

        let round_trip = ScheduleDraft::from_schedule(&value);
        assert_eq!(round_trip.start_date, "2026-11-08");
        assert_eq!(round_trip.start_time, "07:00");
        assert_eq!(round_trip.end_time, "07:30");
        assert_eq!(round_trip.recurrence, value.recurrence);
        assert_eq!(round_trip.finish, value.finish_action);
    }

    #[test]
    fn draft_requires_a_station_and_a_weekday() {
        let mut draft = ScheduleDraft::new(None, naive(2026, 11, 8, 6, 0));
        assert!(draft
            .to_schedule(Uuid::new_v4(), naive(2026, 11, 8, 6, 0))
            .is_err());
        draft.station_id = Some(Uuid::new_v4());
        draft.recurrence = Recurrence::Weekly(Vec::new());
        assert!(draft
            .to_schedule(Uuid::new_v4(), naive(2026, 11, 8, 6, 0))
            .is_err());
    }

    #[test]
    fn draft_rejects_a_one_time_schedule_in_the_past() {
        let mut draft = ScheduleDraft::new(Some(Uuid::new_v4()), naive(2026, 11, 8, 6, 0));
        draft.start_date = "2026-11-07".to_string();
        draft.start_time = "07:00".to_string();
        draft.end_time.clear();
        assert!(draft
            .to_schedule(Uuid::new_v4(), naive(2026, 11, 8, 6, 0))
            .is_err());
    }

    #[test]
    fn recurrence_round_trips_through_text() {
        let weekly = Recurrence::Weekly(vec![Weekday::Mon, Weekday::Fri]);
        assert_eq!(weekly.as_str(), "weekly:Mon,Fri");
        assert_eq!(Recurrence::parse("weekly:Fri,Mon"), Some(weekly));
        assert_eq!(Recurrence::parse("daily"), Some(Recurrence::Daily));
        assert_eq!(Recurrence::parse("once"), Some(Recurrence::Once));
        assert_eq!(Recurrence::parse("weekly:"), None);
        assert_eq!(Recurrence::parse("hourly"), None);
    }

    #[test]
    fn finish_action_round_trips_through_text() {
        assert_eq!(FinishAction::parse("none"), FinishAction::None);
        assert_eq!(FinishAction::parse("close_app"), FinishAction::CloseApp);
        assert_eq!(
            FinishAction::parse("run:notify-send done"),
            FinishAction::RunCommand("notify-send done".to_string())
        );
        assert_eq!(
            FinishAction::RunCommand("notify-send done".to_string()).as_str(),
            "run:notify-send done"
        );
        assert_eq!(FinishAction::parse("run:"), FinishAction::None);
    }

    #[test]
    fn countdown_formats_days_hours_and_minutes() {
        assert_eq!(format_countdown(Duration::seconds(30)), "1m");
        assert_eq!(format_countdown(Duration::minutes(12)), "12m");
        assert_eq!(format_countdown(Duration::minutes(72)), "1h 12m");
        assert_eq!(format_countdown(Duration::hours(76)), "3d 4h");
    }
}
