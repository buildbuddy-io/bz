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

use bz_event_observer::display;
use bz_event_observer::display::TargetDisplayOptions;
use bz_event_observer::fmt_duration;
use bz_event_observer::span_tracker::BuckEventSpanInfo;
use derive_more::From;
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
use superconsole::style::Stylize;
use superconsole::style::style;

use crate::subscribers::superconsole::timed_list::Cutoffs;
use crate::subscribers::superconsole::timekeeper::Timekeeper;

const TIMED_ROW_MARKER: &str = " ↳ ";

#[derive(Debug, Clone, From)]
pub(crate) enum Row {
    Timed(TimedRow),
    Line(Line),
}

#[derive(Debug)]
pub(crate) struct Table {
    pub(crate) rows: Vec<Row>,
}

impl Table {
    pub(crate) fn new() -> Self {
        Self { rows: Vec::new() }
    }

    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }
}

impl Component for Table {
    type Error = bz_error::Error;

    /// Zips together each time and label lines, but gives the times preferential treatment.
    fn draw_unchecked(
        &self,
        Dimensions { width, .. }: Dimensions,
        _mode: DrawMode,
    ) -> bz_error::Result<Lines> {
        let combined = self
            .rows
            .iter()
            .cloned()
            .map(|row| {
                match row {
                    Row::Timed(row) => {
                        let mut label = row.event;
                        let time = row.time;
                        let time_len = time.len();
                        let padding = 1;
                        let maximum_label_width = width.saturating_sub(time_len + padding);
                        let original_label_len = label.len();
                        let will_be_truncated = original_label_len > maximum_label_width;
                        if will_be_truncated {
                            // make space for ellipses
                            let style = label.iter().last().unwrap().style;
                            label.truncate_line(maximum_label_width.saturating_sub(3));
                            label.push(Span::new_styled_lossy(StyledContent::new(
                                style,
                                "...".to_owned(),
                            )));
                        }

                        // add extra padding to compensate for missing spaces between label and time
                        label.pad_right(width.saturating_sub(time_len + label.len()));
                        let mut combined = label;
                        combined.extend(time);
                        combined
                    }
                    Row::Line(line) => line,
                }
            })
            .collect();

        Ok(combined)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TimedRow {
    event: Line,
    time: Line,
}

impl TimedRow {
    pub(crate) fn span(
        padding: usize,
        span: &BuckEventSpanInfo,
        timekeeper: &Timekeeper,
        cutoffs: &Cutoffs,
        display_platform: bool,
    ) -> bz_error::Result<Self> {
        let event = display::display_event(
            &span.event,
            TargetDisplayOptions::for_console(display_platform),
        )?;
        let elapsed = timekeeper.duration_since(span.start);
        let time = fmt_duration::fmt_duration(elapsed);
        Self::text(padding, event, time, elapsed, cutoffs)
    }

    pub(crate) fn text(
        padding: usize,
        event: String,
        time: String,
        age: Duration,
        cutoffs: &Cutoffs,
    ) -> bz_error::Result<Self> {
        let mut event = semantic_event_line(&event).unwrap_or_else(|| {
            Line::from_iter([Span::new_styled_lossy(styled_for_delay(
                style(event),
                age,
                cutoffs,
            ))])
        });
        add_timed_row_marker(&mut event, padding);

        let time = Line::from_iter([Span::new_styled(styled_for_delay(
            style(time),
            age,
            cutoffs,
        ))?]);
        Ok(Self { event, time })
    }
}

fn add_timed_row_marker(event: &mut Line, padding: usize) {
    if padding > 0 {
        event.pad_left(padding);
    }
    event.push_front(styled_span(TIMED_ROW_MARKER, Some(Color::DarkGrey), false));
}

fn semantic_event_line(event: &str) -> Option<Line> {
    let (target, status) = event.split_once(" -- ")?;
    let mut spans = Vec::new();
    push_target_spans(&mut spans, target);
    spans.push(styled_span(" → ", Some(Color::Grey), false));
    push_status_spans(&mut spans, status);
    Some(Line::from_iter(spans))
}

fn push_target_spans(spans: &mut Vec<Span>, target: &str) {
    let (label, suffix) = target.split_once(' ').unwrap_or((target, ""));
    if let Some(repo_end) = label.find("//").map(|idx| idx + "//".len()) {
        spans.push(styled_span(&label[..repo_end], Some(Color::Grey), false));
        push_package_and_name_spans(spans, &label[repo_end..]);
    } else {
        push_package_and_name_spans(spans, label);
    }

    if !suffix.is_empty() {
        spans.push(styled_span(" ", Some(Color::Grey), false));
        spans.push(styled_span(suffix, Some(Color::Grey), false));
    }
}

fn push_package_and_name_spans(spans: &mut Vec<Span>, label: &str) {
    if let Some(colon) = label.rfind(':') {
        if colon > 0 {
            spans.push(styled_span(&label[..colon], Some(Color::White), false));
        }
        spans.push(styled_span(&label[colon..], Some(Color::White), true));
    } else {
        spans.push(styled_span(label, Some(Color::White), false));
    }
}

fn push_status_spans(spans: &mut Vec<Span>, status: &str) {
    if let Some(stage_start) = status.rfind(" [")
        && status.ends_with(']')
    {
        let main = &status[..stage_start];
        if !main.is_empty() {
            spans.push(styled_span(main, Some(Color::Grey), false));
        }
        spans.push(styled_span(" [", Some(Color::Grey), false));
        spans.push(styled_span(
            &status[stage_start + 2..status.len() - 1],
            Some(Color::White),
            false,
        ));
        spans.push(styled_span("]", Some(Color::Grey), false));
        return;
    }

    if let Some(detail_start) = status.rfind(" (")
        && status.ends_with(')')
    {
        let main = &status[..detail_start];
        if !main.is_empty() {
            spans.push(styled_span(main, Some(Color::Grey), false));
        }
        spans.push(styled_span(&status[detail_start..], Some(Color::Grey), false));
        return;
    }

    spans.push(styled_span(status, Some(Color::Grey), false));
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

/// This component echoes the `Lines` that have been stored in it.
#[derive(Debug)]
#[allow(dead_code)]
struct LinesComponent(Lines);

impl Component for LinesComponent {
    type Error = bz_error::Error;

    fn draw_unchecked(
        &self,
        _dimensions: Dimensions,
        _mode: DrawMode,
    ) -> bz_error::Result<Lines> {
        Ok(self.0.clone())
    }
}

/// Colorize based on time.
fn styled_for_delay(
    content: StyledContent<String>,
    elapsed: Duration,
    cutoffs: &Cutoffs,
) -> StyledContent<String> {
    if elapsed < cutoffs.inform {
        content
    } else if elapsed < cutoffs.warn {
        content.dark_yellow()
    } else {
        content.dark_red()
    }
}
