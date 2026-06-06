/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::time::Duration;

use bz_event_observer::action_stats::ActionStats;
use bz_event_observer::fmt_duration;
use bz_event_observer::humanized::CommaSeparatedCount;
use bz_event_observer::pending_estimate::pending_estimate;
use bz_event_observer::progress::BuildProgressPhaseStats;
use bz_event_observer::progress::BuildProgressStats;
use superconsole::Component;
use superconsole::Dimensions;
use superconsole::DrawMode;
use superconsole::Line;
use superconsole::Lines;
use superconsole::Span;
use superconsole::style::Attribute;
use superconsole::style::Color;
use superconsole::style::ContentStyle;
use superconsole::style::StyledContent;

use crate::subscribers::superconsole::SuperConsoleState;
use crate::subscribers::superconsole::common::HeaderLineComponent;
use crate::subscribers::superconsole::common::StaticStringComponent;

const PROGRESS_LABEL_WIDTH: usize = "Validated".len();
const PROGRESS_ROW_INDENT: &str = " ";

pub(crate) struct TasksHeader<'s> {
    header: &'s str,
    state: &'s SuperConsoleState,
}

impl<'s> TasksHeader<'s> {
    pub fn new(header: &'s str, state: &'s SuperConsoleState) -> Self {
        Self { header, state }
    }
}

impl Component for TasksHeader<'_> {
    type Error = bz_error::Error;

    fn draw_unchecked(&self, dimensions: Dimensions, mode: DrawMode) -> bz_error::Result<Lines> {
        if self.state.config.expanded_progress {
            let mut phase_stats = self.state.extra().progress_state().phase_stats();
            if let DrawMode::Final = mode {
                phase_stats.loads.mark_all_finished();
                phase_stats.analyses.mark_all_finished();
                phase_stats.actions.mark_all_finished();
            }

            ProgressHeader {
                phase_stats: &phase_stats,
                progress_stats: self.state.extra().progress_state().progress_stats(),
                action_stats: self.state.simple_console.observer.action_stats(),
                time_elapsed: time_elapsed(self.state),
            }
            .draw(dimensions, mode)
        } else {
            SimpleHeader::new(self.header, self.state).draw(dimensions, mode)
        }
    }
}

struct HeaderData<'s> {
    header: &'s str,
    action_stats: &'s ActionStats,
    elapsed_str: String,
    finished: u64,
    remaining: u64,
}

impl<'s> HeaderData<'s> {
    fn from_state(header: &'s str, state: &'s SuperConsoleState) -> Self {
        let observer = state.simple_console.observer();
        let spans = observer.spans();
        let pending = pending_estimate(spans.roots(), observer.dice_state());
        let finished = spans.roots_completed() as u64;
        let remaining = spans.iter_roots().len() as u64 + pending;

        HeaderData {
            header,
            action_stats: state.simple_console.observer().action_stats(),
            elapsed_str: time_elapsed(state),
            finished,
            remaining,
        }
    }

    fn total(&self) -> u64 {
        self.finished + self.remaining
    }
}

struct SimpleHeader<'s> {
    data: HeaderData<'s>,
}

impl<'s> SimpleHeader<'s> {
    fn new(header: &'s str, state: &'s SuperConsoleState) -> Self {
        Self::new_for_data(HeaderData::from_state(header, state))
    }

    fn new_for_data(data: HeaderData<'s>) -> Self {
        Self { data }
    }
}

impl Component for SimpleHeader<'_> {
    type Error = bz_error::Error;

    fn draw_unchecked(&self, dimensions: Dimensions, mode: DrawMode) -> bz_error::Result<Lines> {
        match mode {
            DrawMode::Normal => HeaderLineComponent::new(
                StaticStringComponent {
                    header: self.data.header,
                },
                CountComponent { data: &self.data },
            )
            .draw(dimensions, mode),
            DrawMode::Final => CountComponent { data: &self.data }.draw(dimensions, mode),
        }
    }
}

fn format_count(count: u64) -> String {
    CommaSeparatedCount::new(count).to_string()
}

fn time_elapsed(state: &SuperConsoleState) -> String {
    fmt_duration::fmt_duration(state.timekeeper.duration_since_command_start())
}

/// This component is used to display summary counts about the number of jobs.
struct CountComponent<'s> {
    data: &'s HeaderData<'s>,
}

impl Component for CountComponent<'_> {
    type Error = bz_error::Error;

    fn draw_unchecked(
        &self,
        _dimensions: Dimensions,
        mode: DrawMode,
    ) -> bz_error::Result<Lines> {
        match mode {
            DrawMode::Normal => {
                let remaining = CommaSeparatedCount::new(self.data.remaining);
                let total = CommaSeparatedCount::new(self.data.total());

                let contents = if self.data.action_stats.log_stats() {
                    let mut actions_summary = format!(
                        "Remaining: {}/{}. Cache hits: {}%. ",
                        remaining,
                        total,
                        self.data.action_stats.total_cache_hit_percentage()
                    );
                    if self.data.action_stats.fallback_actions > 0 {
                        actions_summary += format!(
                            "Fallback: {}/{}. ",
                            CommaSeparatedCount::new(self.data.action_stats.fallback_actions),
                            CommaSeparatedCount::new(
                                self.data.action_stats.total_executed_actions()
                            )
                        )
                        .as_str();
                    }
                    actions_summary += format!("Time elapsed: {}", self.data.elapsed_str).as_str();
                    actions_summary
                } else {
                    format!(
                        "Remaining: {}/{}. Time elapsed: {}",
                        CommaSeparatedCount::new(self.data.remaining),
                        CommaSeparatedCount::new(self.data.total()),
                        self.data.elapsed_str
                    )
                };
                Ok(Lines(vec![Line::unstyled(&contents)?]))
            }
            DrawMode::Final => {
                let mut lines = vec![Line::unstyled(&format!(
                    "Jobs completed: {}.",
                    CommaSeparatedCount::new(self.data.finished),
                ))?];
                if self.data.action_stats.log_stats() {
                    lines.push(Line::unstyled(&self.data.action_stats.to_string())?);
                }
                Ok(Lines(lines))
            }
        }
    }
}

pub(crate) struct ProgressHeader<'s> {
    phase_stats: &'s BuildProgressPhaseStats,
    progress_stats: &'s BuildProgressStats,
    action_stats: &'s ActionStats,
    time_elapsed: String,
}

#[derive(Clone, Copy)]
enum Style {
    Normal(usize),
    Compact(usize),
    ExtraCompact,
}

impl Style {
    fn render(
        &self,
        mode: DrawMode,
        progress_label: &str,
        mut completed: u64,
        total: u64,
        running_str: &str,
        running_num: u64,
    ) -> String {
        if let DrawMode::Final = mode {
            completed = total;
        }
        let executed = progress_label == "Executed";
        let mut line = match self {
            Style::Normal(num_width) | Style::Compact(num_width) => {
                let completed = format_count(completed);
                let total = format_count(total);
                let count = if executed {
                    let tight_count = format!("[{completed} / {total}]");
                    format!("{tight_count:^width$}", width = num_width * 2 + 5)
                } else {
                    format!("{completed:>num_width$} / {total:<num_width$}")
                };
                format!(
                    "{PROGRESS_ROW_INDENT}{progress_label:<progress_label_width$} {count}",
                    progress_label = progress_label,
                    progress_label_width = PROGRESS_LABEL_WIDTH,
                )
            }
            Style::ExtraCompact => {
                let count = if executed {
                    format!("[{} / {}]", format_count(completed), format_count(total))
                } else {
                    format!("{} / {}", format_count(completed), format_count(total))
                };
                format!(
                    "{PROGRESS_ROW_INDENT}{progress_label:<progress_label_width$} {count}",
                    progress_label_width = PROGRESS_LABEL_WIDTH,
                )
            }
        };

        if let DrawMode::Normal = mode {
            line += &match self {
                Style::Normal(_) | Style::Compact(_) => {
                    format!(" (running: {running_str})",)
                }
                Style::ExtraCompact => {
                    format!(" ({})", format_count(running_num))
                }
            };
        }
        line
    }

    fn display_num(&self, num: u64) -> String {
        match self {
            Style::Normal(num_width) | Style::Compact(num_width) => {
                format!("{:>width$}", format_count(num), width = *num_width)
            }
            Style::ExtraCompact => format_count(num),
        }
    }
}

impl ProgressHeader<'_> {
    fn render_loads(&self, style: Style, mode: DrawMode) -> String {
        style.render(
            mode,
            "Loaded",
            self.phase_stats.loads.finished,
            self.phase_stats.loads.started,
            &style.display_num(self.phase_stats.loads.running),
            self.phase_stats.loads.running,
        )
    }

    fn render_loads_extra(&self) -> String {
        let mut msgs = Vec::new();
        if self.progress_stats.dirs_read > 0 {
            msgs.push(format!(
                "{} dirs read",
                CommaSeparatedCount::new(self.progress_stats.dirs_read)
            ));
        }
        if self.progress_stats.targets > 0 {
            msgs.push(format!(
                "{} targets declared",
                CommaSeparatedCount::new(self.progress_stats.targets)
            ));
        }
        msgs.join(", ")
    }

    fn render_analyses(&self, style: Style, mode: DrawMode) -> String {
        style.render(
            mode,
            "Analyzed",
            self.phase_stats.analyses.finished,
            self.phase_stats.analyses.started,
            &style.display_num(self.phase_stats.analyses.running),
            self.phase_stats.analyses.running,
        )
    }

    fn render_analyses_extra(&self) -> String {
        let mut msgs = Vec::new();
        if self.progress_stats.actions_declared > 0 {
            msgs.push(format!(
                "{} actions",
                CommaSeparatedCount::new(self.progress_stats.actions_declared)
            ));
        }
        if self.progress_stats.artifacts_declared > 0 {
            msgs.push(format!(
                "{} artifacts declared",
                CommaSeparatedCount::new(self.progress_stats.artifacts_declared)
            ));
        }
        msgs.join(", ")
    }

    fn render_actions(&self, style: Style, mode: DrawMode) -> String {
        let phase_stats = &self.phase_stats.actions;

        let mut running = Vec::new();
        if self.progress_stats.running_local > 0 || self.action_stats.local_actions > 0 {
            running.push(format!(
                "{} local",
                style.display_num(self.progress_stats.running_local),
            ));
        }
        if self.progress_stats.running_remote > 0 || self.action_stats.remote_actions > 0 {
            running.push(format!(
                "{} remote",
                style.display_num(self.progress_stats.running_remote),
            ));
        }

        let running_str = if running.is_empty() {
            style.display_num(0)
        } else {
            running.join(", ")
        };

        style.render(
            mode,
            "Executed",
            phase_stats.finished,
            phase_stats.started,
            &running_str,
            phase_stats.running,
        )
    }

    fn render_remote_cache_checks(&self, style: Style, mode: DrawMode) -> Option<String> {
        let running = self.progress_stats.running_remote_cache_checks;
        let started = self.progress_stats.remote_cache_checks_started;
        let finished = self.progress_stats.remote_cache_checks_finished;

        if started == 0 || (running == 0 && finished == 0) || matches!(mode, DrawMode::Final) {
            return None;
        }

        Some(style.render(
            mode,
            "Checked",
            finished,
            started,
            &style.display_num(running),
            running,
        ))
    }

    fn render_actions_extra(&self) -> String {
        let exec_time_ms = self.progress_stats.exec_time_ms;
        if exec_time_ms > 0 {
            format!(
                "{} exec time total",
                fmt_duration::fmt_duration(Duration::from_millis(exec_time_ms)),
            )
        } else {
            String::new()
        }
    }

    fn render_actions_stats(&self, style: Style) -> String {
        match style {
            Style::Normal(_) | Style::Compact(_) => {
                let compact = matches!(style, Style::Compact(_));

                let mut res_types = Vec::new();
                if self.action_stats.local_actions > 0 {
                    res_types.push(format!(
                        "{} local",
                        CommaSeparatedCount::new(self.action_stats.local_actions)
                    ));
                }
                if self.action_stats.remote_actions > 0 {
                    res_types.push(format!(
                        "{} remote",
                        CommaSeparatedCount::new(self.action_stats.remote_actions)
                    ));
                }
                let mut cache_types = Vec::new();
                if self.action_stats.local_cached_actions > 0 {
                    cache_types.push(format!(
                        "{} local cache",
                        CommaSeparatedCount::new(self.action_stats.local_cached_actions)
                    ));
                }
                if self.action_stats.total_remote_cached_actions() > 0 {
                    cache_types.push(format!(
                        "{} remote cache",
                        CommaSeparatedCount::new(self.action_stats.total_remote_cached_actions())
                    ));
                }
                if !cache_types.is_empty() {
                    res_types.push(format!(
                        "{} ({}%{})",
                        cache_types.join(", "),
                        self.action_stats.total_cache_hit_percentage(),
                        if compact { "" } else { " hit" }
                    ));
                }

                if res_types.is_empty() {
                    String::new()
                } else {
                    format!(
                        "{}{}",
                        if compact { "" } else { "Finished  " },
                        res_types.join(", ")
                    )
                }
            }

            Style::ExtraCompact => {
                if self.action_stats.total_cached_actions() > 0 {
                    format!(
                        "Cache hits {}%",
                        self.action_stats.total_cache_hit_percentage()
                    )
                } else {
                    String::new()
                }
            }
        }
    }

    fn render_actions_stats_extra(&self) -> String {
        let exec_time_ms = self.progress_stats.exec_time_ms;
        let cached_exec_time_ms = self.progress_stats.cached_exec_time_ms;

        if cached_exec_time_ms > 0 {
            format!(
                "{} exec time cached ({}%)",
                fmt_duration::fmt_duration(Duration::from_millis(cached_exec_time_ms)),
                cached_exec_time_ms * 100 / std::cmp::max(exec_time_ms, 1)
            )
        } else {
            String::new()
        }
    }

    fn render_validations(&self, style: Style, mode: DrawMode) -> String {
        style.render(
            mode,
            "Validated",
            self.phase_stats.validations.finished,
            self.phase_stats.validations.started,
            &style.display_num(self.phase_stats.validations.running),
            self.phase_stats.validations.running,
        )
    }
}

impl Component for ProgressHeader<'_> {
    type Error = bz_error::Error;

    fn draw_unchecked(&self, dimensions: Dimensions, mode: DrawMode) -> bz_error::Result<Lines> {
        fn count_len(v: u64) -> usize {
            format_count(v).len()
        }

        let loads = &self.phase_stats.loads;
        let analysis = &self.phase_stats.analyses;
        let actions = &self.phase_stats.actions;
        let validations = &self.phase_stats.validations;
        let remote_cache_checks_started = self.progress_stats.remote_cache_checks_started;

        let max_total = std::cmp::max(
            std::cmp::max(
                std::cmp::max(
                    std::cmp::max(loads.started, analysis.started),
                    remote_cache_checks_started,
                ),
                actions.started,
            ),
            validations.started,
        );

        let num_width = std::cmp::max(5, count_len(max_total));

        let header_width = PROGRESS_ROW_INDENT.len()
            + "Executed [_ / _] (running: _ local, _ remote)  ".len()
            + 4 * (num_width - 1);

        let elapsed = format!("Time elapsed: {}", &self.time_elapsed);
        let inline_elapsed = match mode {
            DrawMode::Normal => &elapsed,
            DrawMode::Final => "",
        };

        let long_middle_len = "111,222,333 actions, 111,222,333 artifacts declared  ".len();

        let style = if header_width + long_middle_len < dimensions.width {
            Style::Normal(num_width)
        } else if header_width < dimensions.width {
            Style::Compact(num_width)
        } else {
            Style::ExtraCompact
        };

        let mut main = Vec::new();
        let mut extra = Vec::new();

        if loads.started > 0 || matches!(mode, DrawMode::Normal) {
            main.push(self.render_loads(style, mode));
            if let Style::Normal(..) = style {
                extra.push(self.render_loads_extra());
            } else {
                extra.push(String::new());
            }
        }

        if analysis.started > 0 {
            main.push(self.render_analyses(style, mode));
            if let Style::Normal(..) = style {
                extra.push(self.render_analyses_extra());
            } else {
                extra.push(String::new());
            }
        }

        if actions.started > 0 {
            if let Some(line) = self.render_remote_cache_checks(style, mode) {
                main.push(line);
                extra.push(String::new());
            }

            main.push(self.render_actions(style, mode));
            if let Style::Normal(..) = style {
                extra.push(self.render_actions_extra());
            } else {
                extra.push(String::new());
            }

            // Show validation progress if validation has started (before the header/stats line)
            if validations.started > 0 {
                main.push(self.render_validations(style, mode));
                extra.push(String::new());
            }

            let actions_stats = self.render_actions_stats(if dimensions.width > 90 {
                Style::Normal(num_width)
            } else {
                style
            });
            if !actions_stats.is_empty() {
                main.push(format!("{PROGRESS_ROW_INDENT}{actions_stats}"));
                if let Style::Normal(..) = style {
                    extra.push(self.render_actions_stats_extra());
                } else {
                    extra.push(String::new());
                }
            }
        }

        if main.is_empty() && matches!(mode, DrawMode::Normal) {
            main.push(String::new());
            extra.push(String::new());
        }
        if main.is_empty() {
            return Ok(Lines::new());
        }

        assert!(!extra.is_empty());
        assert_eq!(main.len(), extra.len());

        // We now have the "main" column and the "extra" column and we want to lay them out. In normal mode, we also
        // insert the "Time elapsed: 12s" string at the end of the final line.
        //
        // The main column is printed on the left and then padded to align the extra column.
        // As long as there is less than `extra_preferred_width` space, the extra column will go immediately after the main column,
        // once it's wider than that we'll right align it.

        let main_width = main.iter().map(String::len).max().unwrap();

        let extra_preferred_width = long_middle_len + 20;
        let extra_width = extra.iter().map(String::len).max().unwrap();
        let extra_min_width = 2 + std::cmp::max(
            extra_width,
            extra.last().unwrap().len() + inline_elapsed.len() + 2,
        );
        let extra_max_width = dimensions.width.saturating_sub(main_width + 2);

        // If there's not actually enough space to draw them both, we'll prefer for the extra column to be truncated.
        let extra_final_width = std::cmp::min(
            std::cmp::max(extra_preferred_width, extra_min_width),
            extra_max_width,
        );

        let pad_to = std::cmp::max(
            main_width,
            dimensions.width.saturating_sub(extra_final_width),
        );

        let mut lines = Vec::new();
        for i in 0..main.len() {
            let mut line = format!("{:<pad_to$}{}", main[i], extra[i], pad_to = pad_to);

            if i == main.len() - 1 && !inline_elapsed.is_empty() {
                let wanted_len = dimensions.width.saturating_sub(inline_elapsed.len() + 2);
                if line.len() > wanted_len {
                    // If we're going to have to truncate the extra column for the elapsed time, just drop it in this row.
                    line = main[i].to_owned();
                }

                if line.len() < wanted_len {
                    line += &" ".repeat(wanted_len - line.len());
                } else {
                    line.truncate(wanted_len);
                }
                line += "  ";
                line += inline_elapsed;
            }

            lines.push(style_progress_header_line(&line));
        }

        Ok(Lines(lines))
    }
}

fn style_progress_header_line(line: &str) -> Line {
    let mut ranges = Vec::new();

    if let Some((count_start, count_end, is_executed)) = progress_count_range(line) {
        ranges.push(StyledRange {
            start: count_start,
            end: count_end,
            foreground_color: if is_executed {
                Some(Color::DarkGreen)
            } else {
                None
            },
            bold: true,
        });
    }

    add_progress_detail_ranges(line, &mut ranges);
    add_progress_extra_ranges(line, &mut ranges);
    add_action_stats_count_ranges(line, &mut ranges);

    if ranges.is_empty() {
        return Line::unstyled(line).expect("progress header line should be valid");
    }

    line_from_styled_ranges(line, ranges)
}

fn progress_count_range(line: &str) -> Option<(usize, usize, bool)> {
    let labels = ["Loaded", "Analyzed", "Executed", "Checked", "Validated"];
    let label = labels.iter().find(|label| line.contains(*label))?;
    let search_start = line.find(label)? + label.len();
    let count_start = line[search_start..]
        .char_indices()
        .find(|(_, ch)| ch.is_ascii_digit() || *ch == '[')?
        .0
        + search_start;
    if line.as_bytes()[count_start] == b'[' {
        let count_end = line[count_start..]
            .find(']')
            .map(|offset| count_start + offset + 1)?;
        return Some((count_start, count_end, *label == "Executed"));
    }
    let count_end = progress_count_end(line, count_start)?;
    Some((count_start, count_end, *label == "Executed"))
}

fn progress_count_end(line: &str, count_start: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut index = count_start;

    while bytes
        .get(index)
        .is_some_and(|ch| matches!(*ch, b'0'..=b'9' | b','))
    {
        index += 1;
    }
    while bytes.get(index).is_some_and(|ch| *ch == b' ') {
        index += 1;
    }
    if bytes.get(index) != Some(&b'/') {
        return None;
    }
    index += 1;
    while bytes.get(index).is_some_and(|ch| *ch == b' ') {
        index += 1;
    }
    while bytes
        .get(index)
        .is_some_and(|ch| matches!(*ch, b'0'..=b'9' | b','))
    {
        index += 1;
    }

    Some(index)
}

#[derive(Clone, Copy)]
struct StyledRange {
    start: usize,
    end: usize,
    foreground_color: Option<Color>,
    bold: bool,
}

fn add_progress_detail_ranges(line: &str, ranges: &mut Vec<StyledRange>) {
    let mut search_start = 0;
    while let Some(open_offset) = line[search_start..].find('(') {
        let open = search_start + open_offset;
        let Some(close_offset) = line[open..].find(')') else {
            break;
        };
        let close = open + close_offset + 1;
        let detail = &line[open + 1..close - 1];
        if detail.starts_with("running:") || detail.contains("% hit") || detail.ends_with('%') {
            ranges.push(StyledRange {
                start: open,
                end: close,
                foreground_color: Some(Color::Grey),
                bold: false,
            });
        }
        search_start = close;
    }
}

fn add_progress_extra_ranges(line: &str, ranges: &mut Vec<StyledRange>) {
    for label in ["exec time total", "exec time cached"] {
        let mut search_start = 0;
        while let Some(label_offset) = line[search_start..].find(label) {
            let label_start = search_start + label_offset;
            let label_end = label_start + label.len();
            ranges.push(StyledRange {
                start: label_start,
                end: label_end,
                foreground_color: Some(Color::Grey),
                bold: false,
            });
            search_start = label_end;
        }
    }

    for label in [
        "dirs read",
        "targets declared",
        "actions",
        "artifacts declared",
    ] {
        let mut search_start = 0;
        while let Some(label_offset) = line[search_start..].find(label) {
            let label_start = search_start + label_offset;
            let label_end = label_start + label.len();
            if count_range_before(line, label_start).is_some() {
                ranges.push(StyledRange {
                    start: label_start,
                    end: label_end,
                    foreground_color: Some(Color::Grey),
                    bold: false,
                });
                if line.as_bytes().get(label_end).is_some_and(|ch| *ch == b',') {
                    ranges.push(StyledRange {
                        start: label_end,
                        end: label_end + 1,
                        foreground_color: Some(Color::Grey),
                        bold: false,
                    });
                }
            }
            search_start = label_end;
        }
    }
}

fn count_range_before(line: &str, before: usize) -> Option<(usize, usize)> {
    let bytes = line.as_bytes();
    let mut end = before;
    while end > 0 && bytes[end - 1] == b' ' {
        end -= 1;
    }
    if end == 0 || !bytes[end - 1].is_ascii_digit() {
        return None;
    }

    let mut start = end;
    while start > 0 && matches!(bytes[start - 1], b'0'..=b'9' | b',') {
        start -= 1;
    }
    Some((start, end))
}

fn add_action_stats_count_ranges(line: &str, ranges: &mut Vec<StyledRange>) {
    let Some(finished) = line.find("Finished ") else {
        return;
    };
    let stats_start = finished + "Finished ".len();
    let stats_end = line.find("Time elapsed:").unwrap_or(line.len());
    let mut search_start = stats_start;

    while search_start < stats_end {
        let Some(number_offset) = line[search_start..stats_end]
            .char_indices()
            .find(|(_, ch)| ch.is_ascii_digit())
            .map(|(offset, _)| offset)
        else {
            break;
        };
        let start = search_start + number_offset;
        let end = line[start..stats_end]
            .char_indices()
            .find(|(_, ch)| !matches!(ch, '0'..='9' | ','))
            .map_or(stats_end, |(offset, _)| start + offset);
        let suffix = line[end..stats_end].trim_start();
        if suffix.starts_with("local") || suffix.starts_with("remote") {
            ranges.push(StyledRange {
                start,
                end,
                foreground_color: None,
                bold: true,
            });
        }
        search_start = end;
    }
}

fn line_from_styled_ranges(line: &str, mut ranges: Vec<StyledRange>) -> Line {
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut spans = Vec::new();
    let mut cursor = 0;

    for range in ranges {
        if range.start < cursor || range.start >= range.end {
            continue;
        }
        if cursor < range.start {
            spans.push(Span::new_unstyled_lossy(&line[cursor..range.start]));
        }
        spans.push(styled_span(
            &line[range.start..range.end],
            range.foreground_color,
            range.bold,
        ));
        cursor = range.end;
    }

    if cursor < line.len() {
        spans.push(Span::new_unstyled_lossy(&line[cursor..]));
    }

    Line::from_iter(spans)
}

fn styled_span(text: &str, foreground_color: Option<Color>, bold: bool) -> Span {
    Span::new_styled_lossy(StyledContent::new(
        ContentStyle {
            foreground_color,
            background_color: None,
            underline_color: None,
            attributes: if bold {
                Attribute::Bold.into()
            } else {
                Default::default()
            },
        },
        text.to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use std::fmt::Write;

    use bz_error::conversion::from_any_with_tag;
    use bz_event_observer::progress::BuildProgressPhaseStatsItem;
    use itertools::Itertools;

    use super::*;

    fn phase_stats() -> BuildProgressPhaseStats {
        BuildProgressPhaseStats {
            loads: BuildProgressPhaseStatsItem {
                started: 11111,
                finished: 111,
                running: 11,
            },
            analyses: BuildProgressPhaseStatsItem {
                started: 22222,
                finished: 222,
                running: 22,
            },
            actions: BuildProgressPhaseStatsItem {
                started: 33333,
                finished: 333,
                running: 100,
            },
            validations: BuildProgressPhaseStatsItem {
                started: 44444,
                finished: 444,
                running: 44,
            },
        }
    }

    fn progress_stats() -> BuildProgressStats {
        BuildProgressStats {
            dirs_read: 111,
            targets: 22222,
            actions_declared: 3333333,
            artifacts_declared: 4444444,
            remote_cache_checks_started: 0,
            remote_cache_checks_finished: 0,
            running_remote_cache_checks: 0,
            running_local: 55,
            running_remote: 66,
            exec_time_ms: 7777000,
            cached_exec_time_ms: 666000,
        }
    }

    fn action_stats() -> ActionStats {
        ActionStats {
            local_actions: 100,
            remote_actions: 122,
            cached_actions: 133,
            local_cached_actions: 0,
            fallback_actions: 0,
            remote_dep_file_cached_actions: 0,
            excess_cache_misses: 0,
        }
    }

    #[test]
    fn test_final_stats_split_local_and_remote_cache() -> bz_error::Result<()> {
        let phase_stats = &phase_stats();
        let progress_stats = &progress_stats();
        let action_stats = ActionStats {
            local_actions: 0,
            remote_actions: 0,
            cached_actions: 11,
            local_cached_actions: 7,
            fallback_actions: 0,
            remote_dep_file_cached_actions: 0,
            excess_cache_misses: 0,
        };
        let header = ProgressHeader {
            phase_stats,
            progress_stats,
            action_stats: &action_stats,
            time_elapsed: "1234s".to_owned(),
        };

        let output = header
            .draw(
                Dimensions {
                    width: 160,
                    height: 10,
                },
                DrawMode::Final,
            )?
            .fmt_for_test()
            .to_string();

        assert!(output.contains("7 local cache, 11 remote cache (100% hit)"));

        Ok(())
    }

    #[test]
    fn test_different_sizes_dont_fail() -> bz_error::Result<()> {
        let phase_stats = &phase_stats();
        let progress_stats = &progress_stats();
        let action_stats = &action_stats();
        for i in 0..120 {
            let header = ProgressHeader {
                phase_stats,
                progress_stats,
                action_stats,
                time_elapsed: "1234s".to_owned(),
            };

            header.draw(
                Dimensions {
                    width: i,
                    height: 10,
                },
                DrawMode::Normal,
            )?;
            header.draw(
                Dimensions {
                    width: i,
                    height: 10,
                },
                DrawMode::Final,
            )?;
        }
        Ok(())
    }

    #[test]
    fn test_rendering_golden() -> bz_error::Result<()> {
        let mut all_output = String::new();

        fn draw(
            width: usize,
            normal: bool,
            phase_stats: &BuildProgressPhaseStats,
        ) -> bz_error::Result<Lines> {
            ProgressHeader {
                phase_stats,
                progress_stats: &progress_stats(),
                action_stats: &action_stats(),
                time_elapsed: "1234s".to_owned(),
            }
            .draw(
                Dimensions { width, height: 10 },
                if normal {
                    DrawMode::Normal
                } else {
                    DrawMode::Final
                },
            )
        }

        // 129 looks out of place here, but it tests the case where we have an extra column but Time elapsed won't quite fit.
        for width in [30, 40, 60, 80, 100, 129, 130, 140, 160] {
            writeln!(
                &mut all_output,
                "{}",
                draw(width, true, &phase_stats())?.fmt_for_test()
            )
            .map_err(|e| from_any_with_tag(e, bz_error::ErrorTag::SuperConsole))?;
        }

        for width in [60, 140] {
            writeln!(
                &mut all_output,
                "{}",
                draw(width, false, &phase_stats())?.fmt_for_test()
            )
            .map_err(|e| from_any_with_tag(e, bz_error::ErrorTag::SuperConsole))?;
        }

        let expected = indoc::indoc!(
            r#"
                Loading targets.     Loaded
                Analyzing targets.   Analyzed
                Executing actions.   Executed
                Running validations. Validated
                header     Time elapsed: 1234s

                Loading targets.     Loaded    111/11111
                Analyzing targets.   Analyzed  222/22222
                Executing actions.   Executed  333/33333
                Running validations. Validated 444/44444
                header               Time elapsed: 1234s

                Loading targets.     Loaded    111/11111 (11)
                Analyzing targets.   Analyzed  222/22222 (22)
                Executing actions.   Executed  333/33333 (100)
                Running validations. Validated 444/44444 (44)
                header               Cache hits 37%      Time elapsed: 1234s

                Loading targets.     Loaded    111/11111 (11)
                Analyzing targets.   Analyzed  222/22222 (22)
                Executing actions.   Executed  333/33333 (100)
                Running validations. Validated 444/44444 (44)
                header               Cache hits 37%                          Time elapsed: 1234s

                Loading targets.     Loaded      111/11111 (running:    11)
                Analyzing targets.   Analyzed    222/22222 (running:    22)
                Executing actions.   Executed    333/33333 (running:    55 local,    66 remote)
                Running validations. Validated   444/44444 (running:    44)
                header               Finished 100 local, 122 remote, 133 remote cache (37% hit)         Time elapsed: 1234s

                Loading targets.     Loaded      111/11111 (running:    11)
                Analyzing targets.   Analyzed    222/22222 (running:    22)
                Executing actions.   Executed    333/33333 (running:    55 local,    66 remote)
                Running validations. Validated   444/44444 (running:    44)
                header               Finished 100 local, 122 remote, 133 remote cache (37% hit)                                      Time elapsed: 1234s

                Loading targets.     Loaded      111/11111 (running:    11)                      111 dirs read, 22222 targets declared
                Analyzing targets.   Analyzed    222/22222 (running:    22)                      3333333 actions, 4444444 artifacts declared
                Executing actions.   Executed    333/33333 (running:    55 local,    66 remote)  2:09:37.0s exec time total
                Running validations. Validated   444/44444 (running:    44)
                header               Finished 100 local, 122 remote, 133 remote cache (37% hit)                                       Time elapsed: 1234s

                Loading targets.     Loaded      111/11111 (running:    11)                      111 dirs read, 22222 targets declared
                Analyzing targets.   Analyzed    222/22222 (running:    22)                      3333333 actions, 4444444 artifacts declared
                Executing actions.   Executed    333/33333 (running:    55 local,    66 remote)  2:09:37.0s exec time total
                Running validations. Validated   444/44444 (running:    44)
                header               Finished 100 local, 122 remote, 133 remote cache (37% hit)         11:06.0s exec time cached (8%)          Time elapsed: 1234s

                Loading targets.     Loaded      111/11111 (running:    11)                                111 dirs read, 22222 targets declared
                Analyzing targets.   Analyzed    222/22222 (running:    22)                                3333333 actions, 4444444 artifacts declared
                Executing actions.   Executed    333/33333 (running:    55 local,    66 remote)            2:09:37.0s exec time total
                Running validations. Validated   444/44444 (running:    44)
                header               Finished 100 local, 122 remote, 133 remote cache (37% hit)                   11:06.0s exec time cached (8%)                    Time elapsed: 1234s

                Loading targets.     Loaded    11111/11111
                Analyzing targets.   Analyzed  22222/22222
                Executing actions.   Executed  33333/33333
                Running validations. Validated 44444/44444
                header               Cache hits 37%
                Time elapsed: 1234s

                Loading targets.     Loaded    11111/11111                                111 dirs read, 22222 targets declared
                Analyzing targets.   Analyzed  22222/22222                                3333333 actions, 4444444 artifacts declared
                Executing actions.   Executed  33333/33333                                2:09:37.0s exec time total
                Running validations. Validated 44444/44444
                header               Finished 100 local, 122 remote, 133 remote cache (37% hit)  11:06.0s exec time cached (8%)
                Time elapsed: 1234s

        "#
        );

        // copy-paste is easier if we don't need to worry about getting trailing spaces right
        let expected = expected.lines().map(str::trim_end).join("\n");
        let all_output = all_output.lines().map(str::trim_end).join("\n");

        // don't use pretty_assertions here because we mostly just want to copy-paste the golden
        assert!(
            all_output == expected,
            "GOLDEN:\n{all_output}\nEND_GOLDEN\nEXPECTED:\n{expected}\nEND_EXPECTED"
        );

        Ok(())
    }

    #[test]
    fn test_validation_line_not_shown_when_not_started() -> bz_error::Result<()> {
        let mut stats = phase_stats();
        stats.validations = BuildProgressPhaseStatsItem {
            started: 0,
            finished: 0,
            running: 0,
        };

        let output = ProgressHeader {
            phase_stats: &stats,
            progress_stats: &progress_stats(),
            action_stats: &action_stats(),
            time_elapsed: "1234s".to_owned(),
        }
        .draw(
            Dimensions {
                width: 140,
                height: 10,
            },
            DrawMode::Normal,
        )?;

        let output_str = output.fmt_for_test().to_string();
        assert!(
            !output_str.contains("Running validations."),
            "Validation line should not appear when validations haven't started:\n{output_str}"
        );

        Ok(())
    }

    #[test]
    fn test_final_header_empty_when_no_progress_started() -> bz_error::Result<()> {
        let output = ProgressHeader {
            phase_stats: &BuildProgressPhaseStats::default(),
            progress_stats: &BuildProgressStats::default(),
            action_stats: &ActionStats::default(),
            time_elapsed: "1234s".to_owned(),
        }
        .draw(
            Dimensions {
                width: 140,
                height: 10,
            },
            DrawMode::Final,
        )?;

        assert!(output.is_empty());

        Ok(())
    }

    #[test]
    fn test_progress_count_range_stops_before_extra_columns() {
        let executed =
            " Executed   [2,943 / 2,943]                                                       13:04.2s exec time total";
        let (start, end, is_executed) = progress_count_range(executed).unwrap();
        assert!(is_executed);
        assert_eq!(&executed[start..end], "[2,943 / 2,943]");

        let loaded =
            " Loaded       217 / 217                                                           478 dirs read, 22,335 targets declared";
        let (start, end, is_executed) = progress_count_range(loaded).unwrap();
        assert!(!is_executed);
        assert_eq!(&loaded[start..end], "217 / 217");
    }

    #[test]
    fn test_remote_cache_check_line() -> bz_error::Result<()> {
        let mut progress_stats = progress_stats();
        progress_stats.remote_cache_checks_started = 1234;
        progress_stats.remote_cache_checks_finished = 100;
        progress_stats.running_remote_cache_checks = 30;

        let output = ProgressHeader {
            phase_stats: &phase_stats(),
            progress_stats: &progress_stats,
            action_stats: &action_stats(),
            time_elapsed: "1234s".to_owned(),
        }
        .draw(
            Dimensions {
                width: 140,
                height: 10,
            },
            DrawMode::Normal,
        )?
        .fmt_for_test()
        .to_string();

        assert!(output.contains("Checking cache."));
        assert!(output.contains("Checked"));
        assert!(output.contains("100/1234"));
        assert!(output.contains("running:    30"));

        progress_stats.running_remote_cache_checks = 0;
        progress_stats.remote_cache_checks_finished = 1234;
        let completed_output = ProgressHeader {
            phase_stats: &phase_stats(),
            progress_stats: &progress_stats,
            action_stats: &action_stats(),
            time_elapsed: "1234s".to_owned(),
        }
        .draw(
            Dimensions {
                width: 140,
                height: 10,
            },
            DrawMode::Normal,
        )?
        .fmt_for_test()
        .to_string();

        assert!(completed_output.contains("Checking cache."));
        assert!(completed_output.contains("1234/1234"));
        assert!(completed_output.contains("running:     0"));

        let final_output = ProgressHeader {
            phase_stats: &phase_stats(),
            progress_stats: &progress_stats,
            action_stats: &action_stats(),
            time_elapsed: "1234s".to_owned(),
        }
        .draw(
            Dimensions {
                width: 140,
                height: 10,
            },
            DrawMode::Final,
        )?
        .fmt_for_test()
        .to_string();

        assert!(!final_output.contains("Checking cache."));

        Ok(())
    }

    #[test]
    fn test_remaining() -> bz_error::Result<()> {
        let action_stats = ActionStats {
            local_actions: 0,
            remote_actions: 0,
            cached_actions: 1,
            local_cached_actions: 0,
            fallback_actions: 0,
            remote_dep_file_cached_actions: 0,
            excess_cache_misses: 0,
        };
        let output = SimpleHeader::new_for_data(HeaderData {
            header: "test",
            action_stats: &action_stats,
            elapsed_str: "123s".to_owned(),
            finished: 0,
            remaining: 3,
        })
        .draw(
            Dimensions {
                width: 40,
                height: 10,
            },
            DrawMode::Normal,
        )?;
        let expected = "testRemaining: 3/3. Cache hits: 100%. Ti\n".to_owned();

        pretty_assertions::assert_eq!(output.fmt_for_test().to_string(), expected);

        Ok(())
    }

    #[test]
    fn test_remaining_with_pending() -> bz_error::Result<()> {
        let action_stats = ActionStats {
            local_actions: 0,
            remote_actions: 0,
            cached_actions: 0,
            local_cached_actions: 0,
            fallback_actions: 0,
            remote_dep_file_cached_actions: 0,
            excess_cache_misses: 0,
        };
        let output = SimpleHeader::new_for_data(HeaderData {
            header: "test",
            action_stats: &action_stats,
            elapsed_str: "0.0s".to_owned(),
            finished: 0,
            remaining: 2,
        })
        .draw(
            Dimensions {
                width: 60,
                height: 10,
            },
            DrawMode::Normal,
        )?;

        let expected = "test                      Remaining: 2/2. Time elapsed: 0.0s\n".to_owned();

        pretty_assertions::assert_eq!(output.fmt_for_test().to_string(), expected);

        Ok(())
    }

    #[test]
    fn test_children() -> bz_error::Result<()> {
        let action_stats = ActionStats {
            local_actions: 0,
            remote_actions: 0,
            cached_actions: 1,
            local_cached_actions: 0,
            fallback_actions: 0,
            remote_dep_file_cached_actions: 0,
            excess_cache_misses: 0,
        };
        let output = SimpleHeader::new_for_data(HeaderData {
            header: "test",
            action_stats: &action_stats,
            elapsed_str: "0.0s".to_owned(),
            finished: 0,
            remaining: 1,
        })
        .draw(
            Dimensions {
                width: 80,
                height: 10,
            },
            DrawMode::Normal,
        )?;
        let expected =
            "test                        Remaining: 1/1. Cache hits: 100%. Time elapsed: 0.0s\n"
                .to_owned();

        pretty_assertions::assert_eq!(output.fmt_for_test().to_string(), expected);

        Ok(())
    }

    #[test]
    fn test_simple_header_final() -> bz_error::Result<()> {
        let action_stats = ActionStats {
            local_actions: 0,
            remote_actions: 0,
            cached_actions: 1,
            local_cached_actions: 0,
            fallback_actions: 0,
            remote_dep_file_cached_actions: 0,
            excess_cache_misses: 0,
        };
        let output = SimpleHeader::new_for_data(HeaderData {
            header: "test",
            action_stats: &action_stats,
            elapsed_str: "0.0s".to_owned(),
            finished: 0,
            remaining: 1,
        })
        .draw(
            Dimensions {
                width: 80,
                height: 10,
            },
            DrawMode::Final,
        )?;
        let expected = indoc::indoc!(
            r#"
            Jobs completed: 0. Time elapsed: 0.0s.
            Cache hits: 100%. Commands: 1 (cached: 1, remote: 0, local: 0)
            "#
        );

        pretty_assertions::assert_eq!(output.fmt_for_test().to_string(), expected);

        Ok(())
    }
}
