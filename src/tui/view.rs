//! Rendering. Everything here is a function of the state (no I/O; it only
//! records the frame's geometry for the keys), snapshot tested with
//! ratatui's `TestBackend`.

use std::borrow::Cow;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::layout::{Constraint, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Cell, Clear, HighlightSpacing, Padding, Paragraph, Row, Table, TableState,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::actions::{self, RunsOn};
use crate::closure::{self, ChangeKind, ClosureDiff, SIZE_THRESHOLD};
use crate::config::Target;
use crate::model::{
    DiffOutcome, Drift, ErrorInfo, FlakeState, HostId, HostState, ProbeFailure, Status,
};
use crate::storepath::StorePath;
use crate::tui::app::{self, App, Confirm, Mode, SortKey};
use crate::util;

/// Braille spinner frames.
pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Space between table columns.
const GAP: usize = 2;
/// The details pane's key column (`activate `).
const KEY_W: usize = 9;
/// The narrowest DETAIL column worth keeping optional columns out for.
const MIN_DETAIL: usize = 16;
/// The STATUS column is never narrower (`⠋ evaluating 12s`), so it doesn't
/// jitter while a check runs.
const STATUS_W: usize = 16;
/// The smallest details pane: three lines of content.
const DETAILS_MIN: u16 = 5;
/// Wrapped lines kept of one error's detail (bounds the work per frame).
const MAX_ERROR_LINES: usize = 400;

/// The dashboard (DESIGN.md "TUI"):
/// - header: primary flake `@ rev · age` (stale / lock warnings), counts,
///   and while a check runs its progress (`⠋ checking 3/12`);
/// - hosts table: HOST (• local, •? assumed) · STATUS (icon + label, or
///   spinner + activity + elapsed) · GEN · ACTIVATED · DETAIL (drift,
///   reason, note, badges), + KIND when wide; shrink order: DETAIL
///   truncates, then KIND, GEN, ACTIVATED disappear; HOST and STATUS stay;
/// - details of the selected host: title `name · kind · platform · reached
///   via`; current/desired (short store paths, generation, activated, rev,
///   cached), drift and compare URL, timings, the activation labelled
///   `here:` / `on <host>:`, errors (full detail, scrollable);
/// - footer: key hints, or the flash, or the filter prompt;
/// - overlays: help, confirm (title, exact command, where it runs, warning);
/// - [`crate::tui::app::Mode::Diff`]: a full-screen scrollable diff (summary
///   line + one row per change, colored by kind), its spinner, or the
///   "desired generation missing — b to build on `<host>`" message.
///
/// Colors only when `app.color` (else bold/reverse/dim only). Works down to
/// 40×10.
///
/// The frame's geometry (visible rows, scroll limits, the table's scroll
/// offset) is recorded in `app` for the keys, so a host list longer than
/// the screen scrolls only when the selection leaves it.
pub fn draw(frame: &mut Frame, app: &App, now_unix: i64, now: Instant) {
    let area = frame.area();
    if area.is_empty() {
        return;
    }
    let ctx = Ctx {
        app,
        t: Theme { color: app.color },
        now,
        now_unix,
    };
    let diff_of = match &app.mode {
        Mode::Diff { id, .. } => Some(*id),
        Mode::Confirm(confirm) if confirm.from_diff => Some(confirm.id),
        _ => None,
    };
    match diff_of.filter(|&id| id < app.state.hosts.len()) {
        Some(id) => draw_diff_view(frame, area, &ctx, id),
        None => draw_dashboard(frame, area, &ctx),
    }
    match &app.mode {
        Mode::Help => draw_help(frame, area, &ctx),
        Mode::Confirm(confirm) => draw_confirm(frame, area, &ctx, confirm),
        _ => {}
    }
}

/// What every function here needs.
struct Ctx<'a> {
    app: &'a App,
    t: Theme,
    now: Instant,
    now_unix: i64,
}

impl Ctx<'_> {
    fn spinner(&self) -> &'static str {
        SPINNER[self.app.spinner % SPINNER.len()]
    }

    fn host(&self, id: HostId) -> &HostState {
        &self.app.state.hosts[id]
    }
}

/// Styles, with and without colors: the status colors (green ✓, yellow ✗
/// ○, red !, magenta ?), cyan for activity and the selection's `▶`, gray
/// borders, dim secondary text, the selected host reversed.
#[derive(Debug, Clone, Copy)]
struct Theme {
    color: bool,
}

impl Theme {
    fn fg(self, color: Color) -> Style {
        if self.color {
            Style::new().fg(color)
        } else {
            Style::new()
        }
    }

    /// `color`, or bold without colors (for what must still stand out).
    fn loud(self, color: Color) -> Style {
        if self.color {
            Style::new().fg(color)
        } else {
            self.bold()
        }
    }

    fn dim(self) -> Style {
        Style::new().add_modifier(Modifier::DIM)
    }

    fn bold(self) -> Style {
        Style::new().add_modifier(Modifier::BOLD)
    }

    fn border(self) -> Style {
        if self.color {
            Style::new().fg(Color::DarkGray)
        } else {
            self.dim()
        }
    }

    fn accent(self) -> Style {
        self.fg(Color::Cyan)
    }

    fn error(self) -> Style {
        self.loud(Color::Red)
    }

    fn warn(self) -> Style {
        self.loud(Color::Yellow)
    }

    fn status(self, status: Status) -> Style {
        match status {
            Status::UpToDate => self.fg(Color::Green),
            Status::Outdated | Status::NotActivated => self.fg(Color::Yellow),
            Status::EvalFailed | Status::ProbeFailed => self.error(),
            Status::Unreachable => self.loud(Color::Magenta),
            Status::Offline | Status::Pending => self.dim(),
        }
    }

    fn change(self, kind: ChangeKind) -> Style {
        match kind {
            ChangeKind::Added | ChangeKind::Upgraded => self.fg(Color::Green),
            ChangeKind::Removed | ChangeKind::Downgraded => self.fg(Color::Red),
            ChangeKind::Changed => self.fg(Color::Yellow),
            ChangeKind::Resized => self.dim(),
        }
    }

    /// The selected row's HOST cell (after its `▶`): reversed, with or
    /// without colors. Nothing else of the row gets a background, so status
    /// colors stay on the terminal's own (a gray band under red or magenta
    /// is unreadable in common palettes).
    fn cursor(self) -> Style {
        Style::new().add_modifier(Modifier::REVERSED)
    }
}

// ─── dashboard ──────────────────────────────────────────────────────────────

fn draw_dashboard(frame: &mut Frame, area: Rect, ctx: &Ctx) {
    let app = ctx.app;
    let visible = app.visible();
    let selected = app.selected_in(&visible);
    let hints: &[Hint] = match app.mode {
        Mode::Confirm(_) | Mode::Help => &OVERLAY_HINTS,
        _ if visible.is_empty() => &EMPTY_HINTS,
        _ => &NORMAL_HINTS,
    };
    let body = draw_chrome(frame, area, ctx, hints);
    let (table, details) = split_body(body, visible.len(), selected.is_some());
    draw_hosts(frame, table, ctx, &visible, selected);
    match (details, selected) {
        (Some(area), Some(id)) => draw_details(frame, area, ctx, id),
        _ => app.geometry().detail_max.store(0, Relaxed),
    }
}

/// The header (and the global error line) at the top, the footer at the
/// bottom; returns what is left in between.
fn draw_chrome(frame: &mut Frame, area: Rect, ctx: &Ctx, hints: &[Hint]) -> Rect {
    let mut body = area;
    let header = take_top(&mut body, 1);
    let footer = take_bottom(&mut body, 1);
    frame.render_widget(
        Paragraph::new(header_line(ctx, usize::from(header.width))),
        header,
    );
    if !ctx.app.state.errors.is_empty() && body.height > 2 {
        let errors = take_top(&mut body, 1);
        frame.render_widget(
            Paragraph::new(errors_line(ctx, usize::from(errors.width))),
            errors,
        );
    }
    draw_footer(frame, footer, ctx, hints);
    body
}

/// Splits the body into the hosts table (as tall as its rows, or most of
/// the screen when they don't fit) and the details pane (the rest), or
/// drops the details when the table would get too few rows.
fn split_body(body: Rect, rows: usize, details: bool) -> (Rect, Option<Rect>) {
    if !details {
        return (body, None);
    }
    let rows = u16::try_from(rows.max(1)).unwrap_or(u16::MAX);
    let need = rows.saturating_add(3);
    let spare = body.height.saturating_sub(need);
    let details_h = if spare >= DETAILS_MIN {
        spare
    } else {
        ((u32::from(body.height) * 2 / 5) as u16).max(DETAILS_MIN)
    };
    let table_h = body.height.saturating_sub(details_h);
    if table_h < 3 + rows.min(3) {
        return (body, None);
    }
    let mut rest = body;
    let table = take_top(&mut rest, table_h);
    (table, Some(rest))
}

fn header_line(ctx: &Ctx, width: usize) -> Line<'static> {
    let right = header_right(ctx, width);
    let right_w = spans_width(&right);
    // " left  …  right ": at least two spaces before the right side.
    let room = width.saturating_sub(if right_w > 0 { right_w + 4 } else { 2 });
    let left = header_left(ctx, room);
    let pad = width.saturating_sub(spans_width(&left) + right_w + 2);
    let mut spans = Vec::with_capacity(left.len() + right.len() + 2);
    spans.push(Span::raw(" "));
    spans.extend(left);
    if right_w > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
        spans.extend(right);
    }
    Line::from(spans)
}

/// The header's right side: the status counts (compact when they'd take
/// more than half the line), after the check's progress while one runs —
/// `⠋ checking 3/12` when the left side keeps the flake's name beside it,
/// else `⠋ 3/12`. When the revision wouldn't fit on the left, the counts
/// go first (they are in flux while a check runs), then the progress.
fn header_right(ctx: &Ctx, width: usize) -> Vec<Span<'static>> {
    let mut compact = false;
    let mut counts = status_counts(ctx, false);
    if spans_width(&counts) * 2 > width {
        compact = true;
        counts = status_counts(ctx, true);
    }
    let with_counts = |mut spans: Vec<Span<'static>>| {
        if !spans.is_empty() && !counts.is_empty() {
            spans.push(Span::raw(if compact { "  " } else { "   " }));
        }
        spans.extend(counts.iter().cloned());
        spans
    };
    let long = with_counts(check_progress(ctx, false));
    let long_w = spans_width(&long);
    let mut right =
        if long_w * 2 <= width && width.saturating_sub(long_w + 4) >= header_left_min(ctx) {
            long
        } else {
            with_counts(check_progress(ctx, true))
        };
    if spans_width(&right) + 12 > width {
        right = check_progress(ctx, true);
    }
    if spans_width(&right) + 12 > width {
        right.clear();
    }
    right
}

/// `⠋ checking 3/12` (`⠋ 3/12` when `compact`) while a check runs: the
/// hosts it is done with, of all.
fn check_progress(ctx: &Ctx, compact: bool) -> Vec<Span<'static>> {
    let Some((done, total)) = ctx.app.state.check_progress() else {
        return Vec::new();
    };
    let mut spans = vec![Span::styled(ctx.spinner(), ctx.t.accent())];
    if !compact {
        spans.push(Span::styled(" checking", ctx.t.dim()));
    }
    if total > 0 {
        spans.push(Span::raw(format!(" {done}/{total}")));
    }
    spans
}

/// `hostctl  <flake> @ <rev> · <age>` and warnings, shortened to `room`:
/// first the name goes, then the age, then the flake is named by its last
/// path component, then not at all (never the revision or the warnings).
fn header_left(ctx: &Ctx, room: usize) -> Vec<Span<'static>> {
    let brand = Span::styled("hostctl", ctx.t.bold());
    let Some(flake) = ctx.app.state.primary_flake() else {
        return fit(vec![brand], room);
    };
    let url = flat(&flake.url);
    let full = flake_spans(ctx, flake, &url, true);
    if spans_width(&full) + 9 <= room {
        let mut branded = vec![brand, Span::raw("  ")];
        branded.extend(full);
        return branded;
    }
    let candidates = [
        full,
        flake_spans(ctx, flake, &url, false),
        flake_spans(ctx, flake, short_flake(&url), false),
        flake_spans(ctx, flake, "", false),
    ];
    let mut last = Vec::new();
    for candidate in candidates {
        if spans_width(&candidate) <= room {
            return candidate;
        }
        last = candidate;
    }
    fit(last, room)
}

/// The room [`header_left`] needs to still name the flake (by its last
/// path component).
fn header_left_min(ctx: &Ctx) -> usize {
    match ctx.app.state.primary_flake() {
        Some(flake) => spans_width(&flake_spans(
            ctx,
            flake,
            short_flake(&flat(&flake.url)),
            false,
        )),
        None => "hostctl".len(),
    }
}

/// `nix-config` for `github:jjant/nix-config`: the last path component,
/// without query or fragment.
fn short_flake(url: &str) -> &str {
    let url = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .trim_end_matches('/');
    match url.rfind(['/', ':']) {
        Some(i) if i + 1 < url.len() => &url[i + 1..],
        _ => url,
    }
}

/// The flake labelled `label` (none if empty) and what is known about it.
fn flake_spans(ctx: &Ctx, flake: &FlakeState, label: &str, with_age: bool) -> Vec<Span<'static>> {
    let t = ctx.t;
    let mut spans = Vec::new();
    if !label.is_empty() {
        spans.push(Span::raw(label.to_string()));
    }
    match flake.info.get() {
        Some(Ok(info)) => {
            if !label.is_empty() {
                spans.push(Span::styled(" @ ", t.dim()));
            }
            spans.push(Span::styled(flat(&info.short_rev()).into_owned(), t.bold()));
            if let (true, Some(at)) = (with_age, info.last_modified) {
                let age = format!(" · {}", util::format_ago(at, ctx.now_unix));
                spans.push(Span::styled(age, t.dim()));
            }
            if let Some(why) = &info.stale {
                spans.push(Span::styled(format!(" · stale: {}", flat(why)), t.warn()));
            }
            if info.lock_outdated {
                spans.push(Span::styled(" · flake.lock out of date", t.warn()));
            }
        }
        Some(Err(e)) => {
            let sep = if label.is_empty() { "" } else { ": " };
            spans.push(Span::styled(
                format!("{sep}{}", flat(&e.summary)),
                t.error(),
            ));
        }
        None => {}
    }
    if flake.info.is_running() {
        let sep = if spans.is_empty() { "" } else { " " };
        spans.push(Span::styled(format!("{sep}{}", ctx.spinner()), t.accent()));
        if flake.info.get().is_none() {
            spans.push(Span::styled(" resolving", t.dim()));
        }
    }
    let others = ctx.app.state.flakes.len().saturating_sub(1);
    if others > 0 {
        let plural = if others == 1 { "" } else { "s" };
        spans.push(Span::styled(format!(" +{others} flake{plural}"), t.dim()));
    }
    spans
}

/// `3 ✓  1 ✗  1 –`: how many hosts have each status (failures share `!`;
/// what spins while a check runs is its progress, next to these).
fn status_counts(ctx: &Ctx, compact: bool) -> Vec<Span<'static>> {
    const ORDER: [Status; 7] = [
        Status::UpToDate,
        Status::Outdated,
        Status::NotActivated,
        Status::EvalFailed,
        Status::Unreachable,
        Status::Offline,
        Status::Pending,
    ];
    let state = &ctx.app.state;
    let mut counts = [0usize; ORDER.len()];
    for h in &state.hosts {
        let status = match h.status() {
            Status::ProbeFailed => Status::EvalFailed,
            other => other,
        };
        if let Some(i) = ORDER.iter().position(|&s| s == status) {
            counts[i] += 1;
        }
    }
    let mut spans = Vec::new();
    for (status, n) in ORDER.into_iter().zip(counts) {
        if n == 0 {
            continue;
        }
        if !spans.is_empty() {
            spans.push(Span::raw(if compact { " " } else { "  " }));
        }
        spans.push(Span::raw(if compact {
            n.to_string()
        } else {
            format!("{n} ")
        }));
        spans.push(Span::styled(status.icon(), ctx.t.status(status)));
    }
    spans
}

fn errors_line(ctx: &Ctx, width: usize) -> Line<'static> {
    let errors = &ctx.app.state.errors;
    let mut spans = vec![Span::raw(" ")];
    if let Some(last) = errors.last() {
        spans.push(Span::styled(
            format!("! {}", flat(&last.summary)),
            ctx.t.error(),
        ));
        if errors.len() > 1 {
            spans.push(Span::styled(
                format!(" (+{} more)", errors.len() - 1),
                ctx.t.dim(),
            ));
        }
    }
    Line::from(fit(spans, width))
}

// ─── hosts table ────────────────────────────────────────────────────────────

/// One host's cells, before fitting them to the columns.
struct HostRow {
    name: Vec<Span<'static>>,
    kind: &'static str,
    status: Vec<Span<'static>>,
    generation: String,
    activated: String,
    detail: Vec<Span<'static>>,
}

fn host_row(ctx: &Ctx, h: &HostState) -> HostRow {
    let mut name = vec![Span::raw(flat(&h.host.name).into_owned())];
    if let Target::Local { assumed } = h.host.target {
        name.push(Span::styled(
            if assumed { " •?" } else { " •" },
            ctx.t.accent(),
        ));
    }
    let report = h.report();
    HostRow {
        name,
        kind: h.host.kind.as_str(),
        status: status_cell(ctx, h),
        generation: report
            .and_then(|r| r.generation)
            .map(|g| g.to_string())
            .unwrap_or_default(),
        activated: report
            .and_then(|r| r.activated)
            .map(|at| util::format_ago(at, ctx.now_unix))
            .unwrap_or_default(),
        detail: detail_cell(ctx, h),
    }
}

/// `✓ up to date`, or `⠋ evaluating 3s` while the host is being checked.
fn status_cell(ctx: &Ctx, h: &HostState) -> Vec<Span<'static>> {
    let t = ctx.t;
    let status = h.status();
    let checking = h.busy() || (status == Status::Pending && ctx.app.state.run.is_running());
    match h.activity() {
        Some(activity) if checking => {
            let mut spans = vec![
                Span::styled(ctx.spinner(), t.accent()),
                Span::raw(format!(" {activity}")),
            ];
            if let Some(elapsed) = running_for(h, ctx.now).and_then(seconds) {
                spans.push(Span::styled(format!(" {elapsed}"), t.dim()));
            }
            spans
        }
        _ => vec![Span::styled(
            format!("{} {}", status.icon(), status.label()),
            t.status(status),
        )],
    }
}

/// Why the status is what it is, in a few words: drift, the reason there
/// is no generation, the error, badges and the note.
fn detail_cell(ctx: &Ctx, h: &HostState) -> Vec<Span<'static>> {
    let t = ctx.t;
    let mut parts: Vec<Span<'static>> = Vec::new();
    let mut note_shown = false;
    match h.status() {
        Status::Outdated => {
            if let Some(drift) = h.drift() {
                parts.push(Span::styled(drift_short(&drift, ctx.now_unix), t.dim()));
            }
        }
        Status::NotActivated => {
            let reason = h
                .report()
                .and_then(|r| r.none_reason.as_deref())
                .unwrap_or("never activated");
            parts.push(Span::styled(flat(reason).into_owned(), t.dim()));
        }
        Status::EvalFailed => {
            if let Some(Err(e)) = h.eval.get() {
                parts.push(Span::raw(summary(&e.summary)));
            }
        }
        Status::ProbeFailed | Status::Unreachable => {
            if let Some(Err(e)) = h.probe.get() {
                parts.push(Span::raw(summary(&e.error.summary)));
            }
        }
        Status::Offline => match (&h.host.note, h.probe.get()) {
            (Some(note), _) => {
                parts.push(Span::styled(flat(note).into_owned(), t.dim()));
                note_shown = true;
            }
            (None, Some(Err(e))) => {
                parts.push(Span::styled(summary(&e.error.summary), t.dim()));
            }
            _ => {}
        },
        Status::Pending => {
            if let Some(Err(e)) = h.probe.get() {
                parts.push(Span::styled(summary(&e.error.summary), t.dim()));
            }
        }
        Status::UpToDate => {}
    }
    if let Some(report) = h.report() {
        if report.reboot_required {
            parts.push(Span::styled("⟳ reboot", t.warn()));
        }
        if let Some(name) = &report.specialisation {
            parts.push(Span::styled(
                format!("specialisation {}", flat(name)),
                t.dim(),
            ));
        }
    }
    if let (false, Some(note)) = (note_shown, &h.host.note) {
        parts.push(Span::styled(flat(note).into_owned(), t.dim()));
    }
    let mut spans = Vec::with_capacity(parts.len() * 2);
    for (i, part) in parts.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", t.dim()));
        }
        spans.push(part);
    }
    spans
}

/// Natural (wanted) column widths.
#[derive(Debug, Clone, Copy)]
struct Natural {
    host: usize,
    kind: usize,
    status: usize,
    generation: usize,
    activated: usize,
    detail: usize,
}

/// The columns shown and their widths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Columns {
    host: usize,
    kind: Option<usize>,
    status: usize,
    generation: Option<usize>,
    activated: Option<usize>,
    detail: Option<usize>,
}

impl Columns {
    /// Fit the columns into `avail`: DETAIL truncates down to
    /// [`MIN_DETAIL`], then KIND, GEN and ACTIVATED disappear in that order
    /// (DETAIL takes their room); HOST and STATUS always stay, and only
    /// shrink when they alone don't fit.
    fn fit(avail: usize, n: Natural) -> Columns {
        let floor = n.detail.min(MIN_DETAIL);
        let mut cols = Columns {
            host: n.host,
            kind: Some(n.kind),
            status: n.status,
            generation: Some(n.generation),
            activated: Some(n.activated),
            detail: None,
        };
        for drop in 0..=3 {
            match drop {
                1 => cols.kind = None,
                2 => cols.generation = None,
                3 => cols.activated = None,
                _ => {}
            }
            let used = cols.fixed_width();
            if used + GAP + floor <= avail || drop == 3 {
                let space = avail.saturating_sub(used + GAP);
                cols.detail =
                    (n.detail > 0 && space >= floor.clamp(1, 6)).then(|| n.detail.min(space));
                break;
            }
        }
        if cols.host + GAP + cols.status > avail {
            let host = n
                .host
                .min(avail.saturating_sub(GAP + n.status).max(avail * 2 / 5));
            cols.host = host;
            cols.status = avail.saturating_sub(host + GAP);
        }
        cols
    }

    /// Everything but DETAIL, with the gaps between.
    fn fixed_width(&self) -> usize {
        self.host
            + GAP
            + self.status
            + [self.kind, self.generation, self.activated]
                .into_iter()
                .flatten()
                .map(|w| w + GAP)
                .sum::<usize>()
    }

    fn constraints(&self) -> Vec<Constraint> {
        [
            Some(self.host),
            self.kind,
            Some(self.status),
            self.generation,
            self.activated,
            self.detail,
        ]
        .into_iter()
        .flatten()
        .map(|w| Constraint::Length(u16::try_from(w).unwrap_or(u16::MAX)))
        .collect()
    }

    fn header(&self, t: Theme) -> Row<'static> {
        let mut cells = vec![Cell::from("HOST")];
        if self.kind.is_some() {
            cells.push(Cell::from("KIND"));
        }
        cells.push(Cell::from("STATUS"));
        if self.generation.is_some() {
            cells.push(Cell::from(Line::from("GEN").right_aligned()));
        }
        if self.activated.is_some() {
            cells.push(Cell::from("ACTIVATED"));
        }
        if self.detail.is_some() {
            cells.push(Cell::from("DETAIL"));
        }
        Row::new(cells).style(t.dim())
    }

    /// `r`'s cells; the selected row's HOST cell is [`Theme::cursor`], its
    /// text unstyled so that it reverses evenly.
    fn row(&self, r: HostRow, t: Theme, selected: bool) -> Row<'static> {
        let name = if selected {
            let plain = r.name.into_iter().map(|s| Span::raw(s.content));
            Cell::from(Line::from(fit(plain.collect(), self.host))).style(t.cursor())
        } else {
            Cell::from(Line::from(fit(r.name, self.host)))
        };
        let mut cells = vec![name];
        if let Some(w) = self.kind {
            cells.push(Cell::from(Span::styled(
                util::truncate(r.kind, w).into_owned(),
                t.dim(),
            )));
        }
        cells.push(Cell::from(Line::from(fit(r.status, self.status))));
        if self.generation.is_some() {
            cells.push(Cell::from(
                Line::from(Span::styled(r.generation, t.dim())).right_aligned(),
            ));
        }
        if self.activated.is_some() {
            cells.push(Cell::from(Span::styled(r.activated, t.dim())));
        }
        if let Some(w) = self.detail {
            cells.push(Cell::from(Line::from(fit(r.detail, w))));
        }
        Row::new(cells)
    }
}

fn draw_hosts(
    frame: &mut Frame,
    area: Rect,
    ctx: &Ctx,
    visible: &[HostId],
    selected: Option<HostId>,
) {
    let (app, t) = (ctx.app, ctx.t);
    let geometry = app.geometry();
    let mut block = panel(t);
    let info = table_info(ctx, visible.len());
    if !info.is_empty() {
        let info = fit(info, usize::from(area.width.saturating_sub(4)));
        block = block.title_top(Line::from(info).right_aligned());
    }
    let inner = block.inner(area);
    if visible.is_empty() {
        geometry.table_rows.store(inner.height, Relaxed);
        geometry.table_offset.store(0, Relaxed);
        let message = empty_table(ctx, usize::from(inner.width));
        frame.render_widget(Paragraph::new(message).block(block), area);
        return;
    }
    let rows: Vec<HostRow> = visible
        .iter()
        .map(|&id| host_row(ctx, ctx.host(id)))
        .collect();
    let body_h = usize::from(inner.height.saturating_sub(1));
    let at = selected.and_then(|id| visible.iter().position(|&v| v == id));
    let offset = scroll_offset(geometry.table_offset.load(Relaxed), at, rows.len(), body_h);
    let end = (offset + body_h).min(rows.len());
    geometry
        .table_rows
        .store(u16::try_from(body_h).unwrap_or(u16::MAX), Relaxed);
    geometry.table_offset.store(offset, Relaxed);
    if rows.len() > body_h && body_h > 0 {
        let position = format!(" {}–{} of {} ", offset + 1, end, rows.len());
        block = block.title_bottom(Line::from(Span::styled(position, t.dim())).right_aligned());
    }
    let natural = Natural {
        host: rows
            .iter()
            .map(|r| spans_width(&r.name))
            .max()
            .unwrap_or(0)
            .max(4),
        kind: rows
            .iter()
            .map(|r| r.kind.width())
            .max()
            .unwrap_or(0)
            .max(4),
        status: rows
            .iter()
            .map(|r| spans_width(&r.status))
            .max()
            .unwrap_or(0)
            .max(STATUS_W),
        generation: rows
            .iter()
            .map(|r| r.generation.width())
            .max()
            .unwrap_or(0)
            .max(3),
        activated: rows
            .iter()
            .map(|r| r.activated.width())
            .max()
            .unwrap_or(0)
            .max(9),
        detail: rows
            .iter()
            .map(|r| spans_width(&r.detail))
            .max()
            .unwrap_or(0),
    };
    // The selection marker takes two columns.
    let cols = Columns::fit(usize::from(inner.width.saturating_sub(2)), natural);
    let selected_row = at.filter(|i| (offset..end).contains(i)).map(|i| i - offset);
    let body: Vec<Row> = rows
        .into_iter()
        .skip(offset)
        .take(end - offset)
        .enumerate()
        .map(|(i, r)| cols.row(r, t, selected_row == Some(i)))
        .collect();
    let table = Table::new(body, cols.constraints())
        .header(cols.header(t))
        .column_spacing(GAP as u16)
        .highlight_symbol(Line::from(vec![
            Span::styled("▶", t.accent()),
            Span::raw(" "),
        ]))
        .highlight_spacing(HighlightSpacing::Always)
        .block(block);
    let mut state = TableState::default().with_selected(selected_row);
    frame.render_stateful_widget(table, area, &mut state);
}

/// ` /web · 4 of 12 · by status `: the filter and sort, when not default.
fn table_info(ctx: &Ctx, shown: usize) -> Vec<Span<'static>> {
    let (app, t) = (ctx.app, ctx.t);
    let mut spans = Vec::new();
    if !app.filter.is_empty() {
        spans.push(Span::styled(" /", t.dim()));
        spans.push(Span::raw(flat(&app.filter).into_owned()));
        spans.push(Span::styled(
            format!(" · {shown} of {}", app.state.hosts.len()),
            t.dim(),
        ));
    }
    if app.sort == SortKey::Status {
        spans.push(Span::styled(
            if spans.is_empty() {
                " by status"
            } else {
                " · by status"
            },
            t.dim(),
        ));
    }
    if !spans.is_empty() {
        spans.push(Span::raw(" "));
    }
    spans
}

/// What the table says when it has no rows: no hosts at all, or none
/// matching the filter.
fn empty_table(ctx: &Ctx, width: usize) -> Vec<Line<'static>> {
    let (app, t) = (ctx.app, ctx.t);
    let mut lines = vec![Line::raw("")];
    if app.state.hosts.is_empty() {
        let config = match &app.state.config_path {
            Some(path) => flat(&path.display().to_string()).into_owned(),
            None => "~/.config/hostctl/config.toml".into(),
        };
        let mut wrapped = Vec::new();
        for (text, style) in [
            ("No hosts to check.".to_string(), t.bold()),
            (String::new(), Style::new()),
            ("Add [hosts.<name>] tables to".to_string(), t.dim()),
            (config, Style::new()),
            (
                "or set flake = \"…\" to check its configurations.".to_string(),
                t.dim(),
            ),
        ] {
            wrap(vec![Span::styled(text, style)], width, 0, &mut wrapped);
        }
        lines.extend(wrapped.into_iter().map(Line::centered));
    } else {
        lines.push(
            Line::from(vec![
                Span::raw("No host matches "),
                Span::styled(flat(&app.filter).into_owned(), t.bold()),
            ])
            .centered(),
        );
        lines.push(Line::from(Span::styled("esc clears the filter", t.dim())).centered());
    }
    lines
}

/// The first row to show so that `selected` is visible, moving the
/// previous offset as little as possible.
fn scroll_offset(previous: usize, selected: Option<usize>, len: usize, rows: usize) -> usize {
    if rows == 0 {
        return 0;
    }
    let mut offset = previous.min(len.saturating_sub(rows));
    if let Some(at) = selected {
        if at < offset {
            offset = at;
        } else if at >= offset + rows {
            offset = at + 1 - rows;
        }
    }
    offset
}

// ─── details pane ───────────────────────────────────────────────────────────

fn draw_details(frame: &mut Frame, area: Rect, ctx: &Ctx, id: HostId) {
    let (h, t) = (ctx.host(id), ctx.t);
    let title = fit(
        details_title(ctx, h),
        usize::from(area.width.saturating_sub(4)),
    );
    let mut block = panel(t).title_top(Line::from(title));
    let inner = block.inner(area);
    let lines = details_lines(ctx, id, usize::from(inner.width));
    let height = usize::from(inner.height);
    let max = lines.len().saturating_sub(height);
    ctx.app
        .geometry()
        .detail_max
        .store(u16::try_from(max).unwrap_or(u16::MAX), Relaxed);
    let scroll = usize::from(ctx.app.detail_scroll).min(max);
    if max > 0 {
        let last = (scroll + height).min(lines.len());
        let position = format!(" {}–{last} of {} · J/K ", scroll + 1, lines.len());
        block = block.title_bottom(Line::from(Span::styled(position, t.dim())).right_aligned());
    }
    let shown: Vec<Line> = lines.into_iter().skip(scroll).take(height).collect();
    frame.render_widget(Paragraph::new(shown).block(block), area);
}

/// ` bigmem · home-manager · x86_64-linux · local `
fn details_title(ctx: &Ctx, h: &HostState) -> Vec<Span<'static>> {
    let t = ctx.t;
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(flat(&h.host.name).into_owned(), t.bold()),
    ];
    let mut add = |text: String| {
        spans.push(Span::styled(" · ", t.dim()));
        spans.push(Span::styled(text, t.dim()));
    };
    add(h.host.kind.as_str().into());
    if let Some(platform) = h.report().and_then(|r| r.platform.as_deref()) {
        add(flat(platform).into_owned());
    }
    let via = h.host.reached_via();
    add(if via == h.host.name {
        "ssh".into()
    } else {
        flat(via).into_owned()
    });
    if h.host.ssh().is_some_and(|ssh| ssh.forced_command.is_some()) {
        add("probe-only route".into());
    }
    spans.push(Span::raw(" "));
    spans
}

fn details_lines(ctx: &Ctx, id: HostId, width: usize) -> Vec<Line<'static>> {
    let (app, t, h) = (ctx.app, ctx.t, ctx.host(id));
    let status = h.status();
    let mut out = Vec::new();
    let kv = |key: &str, value: Vec<Span<'static>>, out: &mut Vec<Line<'static>>| {
        let mut spans = Vec::with_capacity(value.len() + 1);
        spans.push(Span::styled(format!("{key:<w$}", w = KEY_W), t.dim()));
        spans.extend(value);
        wrap(spans, width, KEY_W, out);
    };

    kv("current", current_value(ctx, h), &mut out);
    if let Some(Err(e)) = h.probe.get() {
        error_detail(t, &e.error, width, &mut out);
    }
    kv("desired", desired_value(ctx, h), &mut out);
    if let Some(Err(e)) = h.eval.get() {
        error_detail(t, e, width, &mut out);
    }
    let flake = app.state.flake_info(&h.host.flake);
    if app
        .state
        .primary_flake()
        .is_some_and(|p| p.url != h.host.flake)
    {
        let mut value = vec![Span::raw(flat(&h.host.flake).into_owned())];
        if let Some(info) = flake {
            value.push(Span::styled(
                format!(" @ {}", flat(&info.short_rev())),
                t.dim(),
            ));
        }
        kv("flake", value, &mut out);
    }
    let drift = h.drift();
    if let Some(drift) = &drift {
        kv(
            "drift",
            vec![Span::raw(drift_long(drift, ctx.now_unix))],
            &mut out,
        );
        if let (Drift::Revision { rev, .. }, Some(info)) = (drift, flake)
            && let Some(url) = info.compare_url(rev)
        {
            kv(
                "compare",
                vec![Span::raw(flat(&url).into_owned())],
                &mut out,
            );
        }
    }
    if let Some(report) = h.report() {
        if let Some(profile) = &report.profile {
            let active = report.current.as_ref() == Some(profile)
                || report.toplevel.as_ref() == Some(profile);
            if !active && !matches!(drift, Some(Drift::Staged)) {
                let mut value = path_spans(t, profile);
                value.push(Span::styled(" (not active)", t.dim()));
                kv("profile", value, &mut out);
            }
        }
        if report.reboot_required {
            let mut value = vec![Span::styled(
                "booted kernel, initrd or modules differ",
                t.warn(),
            )];
            if let Some(booted) = &report.booted {
                value.push(Span::styled(" · booted ", t.dim()));
                value.extend(path_spans(t, booted));
            }
            kv("reboot", value, &mut out);
        }
    }
    if let Some(note) = &h.host.note {
        kv("note", vec![Span::raw(flat(note).into_owned())], &mut out);
    }
    if status != Status::UpToDate {
        match app.activation(id) {
            Ok(activation) => {
                let label = match activation.runs_on {
                    RunsOn::Here => "here: ".to_string(),
                    RunsOn::Remote => format!("on {}: ", flat(&h.host.name)),
                };
                let value = vec![
                    Span::styled(label, t.dim()),
                    Span::raw(flat(&activation.command).into_owned()),
                ];
                kv("activate", value, &mut out);
                if let Some(warning) = &activation.warning {
                    kv(
                        "",
                        vec![Span::styled(flat(warning).into_owned(), t.warn())],
                        &mut out,
                    );
                }
            }
            Err(why) if status.needs_activation() => {
                kv(
                    "activate",
                    vec![Span::styled(
                        format!("unavailable: {}", flat(&why)),
                        t.dim(),
                    )],
                    &mut out,
                );
            }
            Err(_) => {}
        }
    }
    let timings = timings(h);
    if !timings.is_empty() {
        kv("timings", vec![Span::styled(timings, t.dim())], &mut out);
    }
    out
}

/// The deployed generation: path, specialisation, generation, activation
/// age; or why there is none; with a spinner while a probe runs.
fn current_value(ctx: &Ctx, h: &HostState) -> Vec<Span<'static>> {
    let t = ctx.t;
    let mut value = match h.probe.get() {
        Some(Ok(report)) => match report.deployed() {
            Some(path) => {
                let mut value = path_spans(t, path);
                if let Some(name) = &report.specialisation {
                    value.push(Span::styled(
                        format!(" · specialisation {}", flat(name)),
                        t.dim(),
                    ));
                }
                if let Some(generation) = report.generation {
                    value.push(Span::styled(format!(" · gen {generation}"), t.dim()));
                }
                if let Some(at) = report.activated {
                    let ago = util::format_ago(at, ctx.now_unix);
                    value.push(Span::styled(format!(" · activated {ago}"), t.dim()));
                }
                value
            }
            None => {
                let reason = match &report.none_reason {
                    Some(why) => format!("none: {}", flat(why)),
                    None => "none: never activated".into(),
                };
                vec![Span::styled(reason, t.warn())]
            }
        },
        Some(Err(e)) => {
            let (what, style) = match e.kind {
                ProbeFailure::Unreachable if h.host.optional => ("offline", t.dim()),
                ProbeFailure::Unreachable => ("unreachable", t.status(Status::Unreachable)),
                ProbeFailure::Failed => ("probe failed", t.error()),
                ProbeFailure::Interrupted => ("interrupted", t.dim()),
            };
            vec![
                Span::styled(format!("{what}: "), style),
                Span::raw(flat(&e.error.summary).into_owned()),
            ]
        }
        None if h.probe.is_running() => Vec::new(),
        None => vec![Span::styled("not probed yet", t.dim())],
    };
    if h.probe.is_running() {
        busy_suffix(ctx, &mut value, "probing", h.probe.running_for(ctx.now));
    }
    value
}

/// The evaluated generation: path, revision, cached; with a spinner while
/// an evaluation runs.
fn desired_value(ctx: &Ctx, h: &HostState) -> Vec<Span<'static>> {
    let t = ctx.t;
    let mut value = match h.eval.get() {
        Some(Ok(desired)) => {
            let mut value = path_spans(t, &desired.path);
            if let Some(rev) = &desired.rev {
                value.push(Span::styled(format!(" · {}", short_rev(rev)), t.dim()));
            }
            if desired.cached {
                value.push(Span::styled(" · cached", t.dim()));
            }
            value
        }
        Some(Err(e)) => vec![
            Span::styled("eval failed: ", t.error()),
            Span::raw(flat(&e.summary).into_owned()),
        ],
        None if h.eval.is_running() => Vec::new(),
        None => {
            let resolving = ctx
                .app
                .state
                .flake(&h.host.flake)
                .is_some_and(|f| f.info.is_running());
            let why = if resolving {
                "waiting for the flake"
            } else {
                "not evaluated yet"
            };
            vec![Span::styled(why, t.dim())]
        }
    };
    if h.eval.is_running() {
        busy_suffix(ctx, &mut value, "evaluating", h.eval.running_for(ctx.now));
    }
    value
}

/// ` ⠋ probing 2s` after a (stale) value.
fn busy_suffix(
    ctx: &Ctx,
    value: &mut Vec<Span<'static>>,
    activity: &str,
    elapsed: Option<Duration>,
) {
    let lead = if value.is_empty() { "" } else { "  " };
    value.push(Span::styled(
        format!("{lead}{}", ctx.spinner()),
        ctx.t.accent(),
    ));
    value.push(Span::raw(format!(" {activity}")));
    if let Some(elapsed) = elapsed.and_then(seconds) {
        value.push(Span::styled(format!(" {elapsed}"), ctx.t.dim()));
    }
}

/// `probe 12ms · eval 3.4s` (or `eval cached`) `· diff 1.2s`.
fn timings(h: &HostState) -> String {
    let mut parts = Vec::new();
    if let Some(took) = h.probe.took() {
        parts.push(format!("probe {}", util::format_elapsed(took)));
    }
    match (h.eval.took(), h.eval.get()) {
        (Some(took), Some(_)) => parts.push(format!("eval {}", util::format_elapsed(took))),
        (None, Some(Ok(desired))) if desired.cached => parts.push("eval cached".into()),
        _ => {}
    }
    if let Some(took) = h.diff.took() {
        parts.push(format!("diff {}", util::format_elapsed(took)));
    }
    parts.join(" · ")
}

/// An error's full detail (cleaned stderr, the command, hints) under the
/// line showing its summary, dim except the lines that say what's wrong.
fn error_detail(t: Theme, e: &ErrorInfo, width: usize, out: &mut Vec<Line<'static>>) {
    let detail = e.detail.trim_end();
    if detail.trim().is_empty() || detail.trim() == e.summary.trim() {
        return;
    }
    let start = out.len();
    let indent = " ".repeat(KEY_W.min(width / 4));
    for line in detail.lines() {
        let line = clean(line);
        let text = line.trim_start();
        let style = if text.starts_with("error") || text.starts_with("hint") {
            Style::new()
        } else {
            t.dim()
        };
        // Wrapped parts of a line align with its own indentation.
        let lead = line[..line.len() - text.len()].width();
        let hang = (indent.len() + lead).min(width / 2);
        wrap(
            vec![
                Span::raw(indent.clone()),
                Span::styled(line.into_owned(), style),
            ],
            width,
            hang,
            out,
        );
        if out.len() - start >= MAX_ERROR_LINES {
            out.truncate(start + MAX_ERROR_LINES);
            out.push(Line::from(Span::styled(format!("{indent}…"), t.dim())));
            break;
        }
    }
}

/// The DETAIL column's drift: `on 1a2b3c4 (4mo old)`, `staged: …` (same
/// wording as the plain table).
fn drift_short(drift: &Drift, now_unix: i64) -> String {
    match drift {
        Drift::Revision {
            rev, last_modified, ..
        } => match last_modified {
            Some(at) => format!("on {} ({} old)", short_rev(rev), age(*at, now_unix)),
            None => format!("on {}", short_rev(rev)),
        },
        Drift::SameRevision => "same rev, different build".into(),
        Drift::Staged => "staged: switch/reboot pending".into(),
        Drift::Unknown => "unknown generation".into(),
    }
}

/// The details pane's drift, with what it means.
fn drift_long(drift: &Drift, now_unix: i64) -> String {
    match drift {
        Drift::Revision { from_generation, .. } => format!(
            "{}, {}",
            drift_short(drift, now_unix),
            if *from_generation {
                "recorded by the generation"
            } else {
                "per hostctl's cache"
            }
        ),
        Drift::SameRevision => {
            "same rev, different build: activated from a reference without a rev, or with overridden inputs"
                .into()
        }
        Drift::Staged => {
            "staged: the profile already points at the desired generation; a switch or reboot activates it"
                .into()
        }
        Drift::Unknown => "unknown generation: not one hostctl has evaluated".into(),
    }
}

// ─── diff view ──────────────────────────────────────────────────────────────

fn draw_diff_view(frame: &mut Frame, area: Rect, ctx: &Ctx, id: HostId) {
    let (app, t, h) = (ctx.app, ctx.t, ctx.host(id));
    let missing =
        !h.diff.is_running() && matches!(h.diff.get(), Some(Ok(DiffOutcome::DesiredMissing)));
    let hints: &[Hint] = match app.mode {
        Mode::Confirm(_) => &OVERLAY_HINTS,
        _ if missing => &DIFF_MISSING_HINTS,
        _ => &DIFF_HINTS,
    };
    let body = draw_chrome(frame, area, ctx, hints);
    let title = fit(
        diff_title(ctx, h),
        usize::from(body.width.saturating_sub(4)),
    );
    let block = panel(t).title_top(Line::from(title));
    if let (false, Some(Ok(DiffOutcome::Diff { diff }))) = (h.diff.is_running(), h.diff.get()) {
        draw_changes(frame, body, block, ctx, diff);
        return;
    }
    let inner = block.inner(body);
    let geometry = app.geometry();
    geometry.diff_rows.store(inner.height, Relaxed);
    geometry.diff_max.store(0, Relaxed);
    let lines = diff_message(ctx, h, usize::from(inner.width));
    frame.render_widget(Paragraph::new(lines).block(block), body);
}

/// ` bigmem  494amg2b-home-manager-generation → ykn4vz9v-home-manager-generation `
fn diff_title(ctx: &Ctx, h: &HostState) -> Vec<Span<'static>> {
    let t = ctx.t;
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(flat(&h.host.name).into_owned(), t.bold()),
    ];
    let inputs = match &h.diff_inputs {
        Some((current, desired)) => Some((current, desired)),
        None => h.current().zip(h.desired().map(|d| &d.path)),
    };
    if let Some((current, desired)) = inputs {
        spans.push(Span::raw("  "));
        if current.name() == desired.name() {
            // One generation of the same thing: the hashes say which.
            spans.push(Span::styled(current.hash()[..8].to_string(), t.dim()));
        } else {
            spans.extend(path_spans(t, current));
        }
        spans.push(Span::styled(" → ", t.dim()));
        spans.extend(path_spans(t, desired));
    }
    spans.push(Span::raw(" "));
    spans
}

/// The diff view's text when there is no diff to list.
fn diff_message(ctx: &Ctx, h: &HostState, width: usize) -> Vec<Line<'static>> {
    let t = ctx.t;
    let name = flat(&h.host.name).into_owned();
    let mut out = Vec::new();
    if h.diff.is_running() {
        let mut spans = vec![
            Span::styled(ctx.spinner(), t.accent()),
            Span::raw(" comparing closures"),
        ];
        if let Some(elapsed) = h.diff.running_for(ctx.now).and_then(seconds) {
            spans.push(Span::styled(format!(" {elapsed}"), t.dim()));
        }
        out.push(Line::from(spans));
        let where_ = if h.host.is_local() {
            "in the local store".to_string()
        } else {
            format!("on {name} and in the local store")
        };
        wrap(
            vec![Span::styled(
                format!("listing both generations' closures {where_}"),
                t.dim(),
            )],
            width,
            0,
            &mut out,
        );
        return out;
    }
    match h.diff.get() {
        Some(Ok(DiffOutcome::DesiredMissing)) => {
            let target = if actions::runs_here(&h.host) {
                "here".to_string()
            } else {
                format!("on {name}")
            };
            wrap(
                vec![
                    Span::styled("desired generation missing", t.warn()),
                    Span::styled(" — ", t.dim()),
                    Span::styled("b", t.bold()),
                    Span::raw(format!(" to build {target}")),
                ],
                width,
                0,
                &mut out,
            );
            out.push(Line::raw(""));
            let desired = h
                .desired()
                .map_or_else(|| "it".to_string(), |d| d.path.short());
            let place = if h.host.is_local() {
                "isn't in the local store yet".to_string()
            } else {
                format!("is neither on {name} nor in the local store yet")
            };
            let text = format!(
                "{desired} {place}. Building it activates nothing; the diff runs again afterwards."
            );
            wrap(vec![Span::styled(text, t.dim())], width, 0, &mut out);
        }
        Some(Err(e)) => {
            wrap(
                vec![
                    Span::styled("diff failed: ", t.error()),
                    Span::raw(flat(&e.summary).into_owned()),
                ],
                width,
                0,
                &mut out,
            );
            let detail = e.detail.trim();
            if !detail.is_empty() && detail != e.summary.trim() {
                out.push(Line::raw(""));
                for line in detail.lines().take(MAX_ERROR_LINES) {
                    wrap(
                        vec![Span::styled(clean(line).into_owned(), t.dim())],
                        width,
                        0,
                        &mut out,
                    );
                }
            }
            out.push(Line::raw(""));
            out.push(Line::from(vec![
                Span::styled("r", t.bold()),
                Span::styled(" tries again", t.dim()),
            ]));
        }
        _ => {
            let why = app::diff_problem(h).unwrap_or_else(|| "no diff yet: r computes it".into());
            wrap(
                vec![Span::styled(flat(&why).into_owned(), t.dim())],
                width,
                0,
                &mut out,
            );
        }
    }
    out
}

/// A change's marker, colored by kind.
fn change_marker(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added => "+",
        ChangeKind::Removed => "-",
        ChangeKind::Upgraded => "↑",
        ChangeKind::Downgraded => "↓",
        ChangeKind::Changed => "~",
        ChangeKind::Resized => "±",
    }
}

fn change_label(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added => "added",
        ChangeKind::Removed => "removed",
        ChangeKind::Upgraded => "upgraded",
        ChangeKind::Downgraded => "downgraded",
        ChangeKind::Changed => "changed",
        ChangeKind::Resized => "resized",
    }
}

const CHANGE_KINDS: [ChangeKind; 6] = [
    ChangeKind::Added,
    ChangeKind::Removed,
    ChangeKind::Upgraded,
    ChangeKind::Downgraded,
    ChangeKind::Changed,
    ChangeKind::Resized,
];

fn draw_changes(
    frame: &mut Frame,
    area: Rect,
    block: Block<'static>,
    ctx: &Ctx,
    diff: &ClosureDiff,
) {
    let (app, t) = (ctx.app, ctx.t);
    let geometry = app.geometry();
    let inner = block.inner(area);
    let summary = vec![
        Line::from(vec![
            Span::raw(format!(
                "{} → {} paths",
                diff.paths_before, diff.paths_after
            )),
            Span::styled(" · ", t.dim()),
            Span::raw(format!(
                "{} → {}",
                util::format_size(diff.size_before),
                util::format_size(diff.size_after)
            )),
            Span::styled(
                format!(" ({})", util::format_size_delta(diff.size_delta())),
                t.dim(),
            ),
        ]),
        change_counts(t, diff),
    ];
    let header_h: u16 = if inner.height >= 6 {
        3
    } else {
        inner.height.min(2)
    };
    let mut rest = inner;
    let header = take_top(&mut rest, header_h);
    let rows_h = usize::from(rest.height);
    let len = diff.changes.len();
    let max = len.saturating_sub(rows_h);
    let scroll = usize::from(app.diff_scroll()).min(max);
    geometry.diff_rows.store(rest.height, Relaxed);
    geometry
        .diff_max
        .store(u16::try_from(max).unwrap_or(u16::MAX), Relaxed);
    let block = if len > rows_h && rows_h > 0 {
        let last = (scroll + rows_h).min(len);
        let position = format!(" {}–{last} of {len} ", scroll + 1);
        block.title_bottom(Line::from(Span::styled(position, t.dim())).right_aligned())
    } else {
        block
    };
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(summary), header);
    if diff.is_empty() {
        let message = Line::from(Span::styled("no package changes", t.dim()));
        frame.render_widget(Paragraph::new(message), rest);
        return;
    }
    let shown = &diff.changes[scroll..(scroll + rows_h).min(len)];
    let versions = |change: &closure::Change| -> Vec<Span<'static>> {
        if change.kind() == ChangeKind::Resized {
            return Vec::new();
        }
        let side = |versions: &[String], style: Style| {
            let style = if versions.is_empty() { t.dim() } else { style };
            Span::styled(closure::format_versions(versions), style)
        };
        vec![
            side(&change.removed, t.change(ChangeKind::Removed)),
            Span::styled(" → ", t.dim()),
            side(&change.added, t.change(ChangeKind::Added)),
        ]
    };
    let delta = |change: &closure::Change| {
        let delta = change.size_delta();
        if delta.unsigned_abs() >= SIZE_THRESHOLD {
            util::format_size_delta(delta)
        } else {
            String::new()
        }
    };
    // Widths over the whole diff (one pass), so columns stay put while
    // scrolling.
    let (mut name_w, mut versions_w, mut delta_w, mut kind_w) = (0, 0, 0, 0);
    for change in &diff.changes {
        let kind = change.kind();
        name_w = name_w.max(change.name.width());
        kind_w = kind_w.max(change_label(kind).width());
        if kind != ChangeKind::Resized {
            let w = versions_width(&change.removed) + 3 + versions_width(&change.added);
            versions_w = versions_w.max(w);
        }
        if change.size_delta().unsigned_abs() >= SIZE_THRESHOLD {
            delta_w = delta_w.max(delta(change).width());
        }
    }
    let cols = DiffColumns::fit(usize::from(rest.width), name_w, versions_w, delta_w, kind_w);
    let rows: Vec<Row> = shown
        .iter()
        .map(|change| {
            let kind = change.kind();
            let style = t.change(kind);
            let mut cells = vec![
                Cell::from(Span::styled(change_marker(kind), style)),
                Cell::from(util::truncate(&change.name, cols.name).into_owned()),
                Cell::from(Line::from(fit(versions(change), cols.versions))),
            ];
            if cols.delta > 0 {
                cells.push(Cell::from(Line::from(delta(change)).right_aligned()));
            }
            if cols.kind.is_some() {
                cells.push(Cell::from(Span::styled(change_label(kind), style)));
            }
            Row::new(cells)
        })
        .collect();
    let table = Table::new(rows, cols.constraints()).column_spacing(GAP as u16);
    frame.render_widget(table, rest);
}

/// The width of [`closure::format_versions`]'s output, without building it.
fn versions_width(versions: &[String]) -> usize {
    if versions.is_empty() {
        return 1;
    }
    let text: usize = versions
        .iter()
        .map(|v| if v.is_empty() { 1 } else { v.width() })
        .sum();
    text + 2 * (versions.len() - 1)
}

/// `4 added · 1 changed · 1 resized`
fn change_counts(t: Theme, diff: &ClosureDiff) -> Line<'static> {
    let counts = diff.counts();
    let mut spans = Vec::new();
    for (kind, n) in CHANGE_KINDS.into_iter().zip(counts) {
        if n == 0 {
            continue;
        }
        if !spans.is_empty() {
            spans.push(Span::styled(" · ", t.dim()));
        }
        spans.push(Span::styled(
            format!("{n} {}", change_label(kind)),
            t.change(kind),
        ));
    }
    Line::from(spans)
}

/// The diff table's columns: marker, name, versions, size delta, kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DiffColumns {
    name: usize,
    versions: usize,
    /// 0 when no change has a size delta worth showing.
    delta: usize,
    kind: Option<usize>,
}

impl DiffColumns {
    /// Versions shrink first (to 12 columns), then the kind label goes
    /// (the marker's color still says it), then names are cut.
    fn fit(avail: usize, name: usize, versions: usize, delta: usize, kind: usize) -> DiffColumns {
        let delta_part = if delta > 0 { GAP + delta } else { 0 };
        let base = 1 + GAP + name + GAP + delta_part;
        let room = |used: usize| avail.saturating_sub(used);
        if base + GAP + kind + versions.min(12) <= avail {
            return DiffColumns {
                name,
                versions: versions.min(room(base + GAP + kind)),
                delta,
                kind: Some(kind),
            };
        }
        if base + versions.min(12) <= avail {
            return DiffColumns {
                name,
                versions: versions.min(room(base)),
                delta,
                kind: None,
            };
        }
        let name = name.min(room(1 + GAP + GAP + delta_part + versions.min(12)).max(avail / 3));
        DiffColumns {
            name,
            versions: versions.min(room(1 + GAP + name + GAP + delta_part)),
            delta,
            kind: None,
        }
    }

    fn constraints(&self) -> Vec<Constraint> {
        let len = |w: usize| Constraint::Length(u16::try_from(w).unwrap_or(u16::MAX));
        let mut widths = vec![len(1), len(self.name), len(self.versions)];
        if self.delta > 0 {
            widths.push(len(self.delta));
        }
        if let Some(kind) = self.kind {
            widths.push(len(kind));
        }
        widths
    }
}

// ─── overlays ───────────────────────────────────────────────────────────────

fn draw_confirm(frame: &mut Frame, area: Rect, ctx: &Ctx, confirm: &Confirm) {
    let t = ctx.t;
    let width = area.width.saturating_sub(4).clamp(area.width.min(30), 100);
    let inner_w = usize::from(width.saturating_sub(4));
    let mut command = Vec::new();
    wrap(
        vec![Span::raw(clean(&confirm.command).into_owned())],
        inner_w,
        0,
        &mut command,
    );
    let mut tail = Vec::new();
    wrap(
        vec![
            Span::styled("runs ", t.dim()),
            Span::styled(flat(&confirm.runs_on).into_owned(), t.bold()),
        ],
        inner_w,
        0,
        &mut tail,
    );
    if let Some(warning) = &confirm.warning {
        wrap(
            vec![Span::styled(format!("! {}", flat(warning)), t.warn())],
            inner_w,
            2,
            &mut tail,
        );
    }
    // Blank lines go first when space is short, then the command's end.
    let room = usize::from(area.height.saturating_sub(2));
    let blanks = room >= command.len() + tail.len() + 3;
    let gap = room > command.len() + tail.len();
    let fixed = tail.len() + usize::from(gap) + 2 * usize::from(blanks);
    if command.len() + fixed > room {
        command.truncate(room.saturating_sub(fixed).max(1));
        if let Some(last) = command.last_mut() {
            let spans = std::mem::take(&mut last.spans);
            last.spans = if spans_width(&spans) + 2 <= inner_w {
                let mut spans = spans;
                spans.push(Span::styled(" …", t.dim()));
                spans
            } else {
                fit(spans, inner_w.saturating_sub(1))
            };
        }
    }
    let mut lines = Vec::with_capacity(command.len() + tail.len() + 3);
    if blanks {
        lines.push(Line::raw(""));
    }
    lines.extend(command);
    if gap {
        lines.push(Line::raw(""));
    }
    lines.extend(tail);
    if blanks {
        lines.push(Line::raw(""));
    }
    let height = u16::try_from(lines.len() + 2).unwrap_or(u16::MAX);
    let rect = centered(area, width, height);
    let title = fit(
        vec![Span::styled(
            format!(" {} ", flat(&confirm.title)),
            t.bold(),
        )],
        usize::from(width.saturating_sub(2)),
    );
    let keys = Line::from(vec![
        Span::raw(" "),
        Span::styled("y", t.bold()),
        Span::styled(" run · ", t.dim()),
        Span::styled("n", t.bold()),
        Span::styled(" cancel ", t.dim()),
    ])
    .right_aligned();
    let block = overlay(t).title_top(Line::from(title)).title_bottom(keys);
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(lines).block(block), rect);
}

const HELP_HOSTS: [(&str, &str); 12] = [
    ("↑↓ j k", "move"),
    ("g G", "first / last"),
    ("PgUp PgDn", "page"),
    ("enter d", "closure diff"),
    ("a", "activate"),
    ("y", "copy command"),
    ("r", "check again"),
    ("/", "filter"),
    ("s", "sort"),
    ("J K", "scroll details"),
    ("esc", "clear filter"),
    ("q", "quit"),
];

const HELP_DIFF: [(&str, &str); 5] = [
    ("↑↓ j k", "scroll"),
    ("b", "build it"),
    ("a", "activate"),
    ("r", "diff again"),
    ("q esc", "back"),
];

/// Key column of the help.
const HELP_KEY_W: usize = 10;
const HELP_COLUMN_W: usize = 26;

fn draw_help(frame: &mut Frame, area: Rect, ctx: &Ctx) {
    let t = ctx.t;
    let section = |title: &str| Line::from(Span::styled(title.to_string(), t.bold()));
    let entry = |key: &str, what: &str| {
        Line::from(vec![
            Span::styled(format!("{key:<w$}", w = HELP_KEY_W), t.accent()),
            Span::raw(what.to_string()),
        ])
    };
    let mut hosts = vec![section("Hosts")];
    hosts.extend(HELP_HOSTS.iter().map(|(k, w)| entry(k, w)));
    let mut more = vec![section("Diff")];
    more.extend(HELP_DIFF.iter().map(|(k, w)| entry(k, w)));
    more.push(Line::raw(""));
    more.push(section("Status"));
    for status in [
        Status::UpToDate,
        Status::Outdated,
        Status::NotActivated,
        Status::EvalFailed,
        Status::Unreachable,
        Status::Offline,
    ] {
        let label = match status {
            Status::EvalFailed => "failed",
            other => other.label(),
        };
        more.push(Line::from(vec![
            Span::styled(format!("{:<w$}", status.icon(), w = 2), t.status(status)),
            Span::raw(label),
        ]));
    }
    more.push(Line::from(vec![
        Span::styled(format!("{:<2}", SPINNER[0]), t.accent()),
        Span::raw("checking"),
    ]));
    more.push(Line::from(vec![
        Span::styled("• ", t.accent()),
        Span::raw("this machine"),
    ]));

    let two_columns = usize::from(area.width) >= 2 * HELP_COLUMN_W + 4 + 4;
    let lines: Vec<Line> = if two_columns {
        let rows = hosts.len().max(more.len());
        let mut left = hosts.into_iter();
        let mut right = more.into_iter();
        (0..rows)
            .map(|_| {
                let l = left.next().unwrap_or_default();
                let pad = (HELP_COLUMN_W + 4).saturating_sub(l.width());
                let mut spans = l.spans;
                spans.push(Span::raw(" ".repeat(pad)));
                spans.extend(right.next().unwrap_or_default().spans);
                Line::from(spans)
            })
            .collect()
    } else {
        hosts.push(Line::raw(""));
        hosts.extend(more);
        hosts
    };
    let content_w = lines.iter().map(Line::width).max().unwrap_or(0);
    let width = u16::try_from(content_w + 4)
        .unwrap_or(u16::MAX)
        .min(area.width);
    let room = usize::from(area.height.saturating_sub(2));
    let mut lines = lines;
    if lines.len() > room {
        lines.truncate(room.saturating_sub(1));
        lines.push(Line::from(Span::styled("…", t.dim())));
    }
    let height = u16::try_from(lines.len() + 2).unwrap_or(u16::MAX);
    let rect = centered(area, width, height);
    let block = overlay(t)
        .title_top(Line::from(Span::styled(" hostctl keys ", t.bold())))
        .title_bottom(Line::from(Span::styled(" any key closes ", t.dim())).right_aligned());
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(lines).block(block), rect);
}

// ─── footer ─────────────────────────────────────────────────────────────────

/// A key hint; when space runs out, lower `keep` goes first.
#[derive(Debug, Clone, Copy)]
struct Hint {
    key: &'static str,
    label: &'static str,
    keep: u8,
}

const fn hint(key: &'static str, label: &'static str, keep: u8) -> Hint {
    Hint { key, label, keep }
}

const NORMAL_HINTS: [Hint; 9] = [
    hint("↑↓", "select", 1),
    hint("enter", "diff", 7),
    hint("a", "activate", 8),
    hint("y", "copy", 3),
    hint("r", "refresh", 5),
    hint("/", "filter", 4),
    hint("s", "sort", 2),
    hint("?", "help", 9),
    hint("q", "quit", 6),
];

const DIFF_HINTS: [Hint; 5] = [
    hint("↑↓", "scroll", 5),
    hint("a", "activate", 6),
    hint("r", "diff again", 3),
    hint("y", "copy", 2),
    hint("q", "back", 9),
];

const DIFF_MISSING_HINTS: [Hint; 4] = [
    hint("b", "build", 8),
    hint("a", "activate", 6),
    hint("r", "diff again", 4),
    hint("q", "back", 9),
];

/// While a box is open it shows its own keys; the footer stays quiet.
const OVERLAY_HINTS: [Hint; 0] = [];

/// With no host shown, only what still makes sense.
const EMPTY_HINTS: [Hint; 4] = [
    hint("r", "refresh", 5),
    hint("/", "filter", 4),
    hint("?", "help", 9),
    hint("q", "quit", 6),
];

fn draw_footer(frame: &mut Frame, area: Rect, ctx: &Ctx, hints: &[Hint]) {
    if area.is_empty() {
        return;
    }
    let (app, t) = (ctx.app, ctx.t);
    let width = usize::from(area.width);
    let line = if app.mode == Mode::Filter {
        let prompt = Span::styled(" /", t.accent().add_modifier(Modifier::BOLD));
        let filter = flat(&app.filter);
        let text = tail(&filter, width.saturating_sub(3)).into_owned();
        let used = 2 + text.width();
        let mut spans = vec![prompt, Span::raw(text)];
        let help = "enter keep · esc clear ";
        if used + help.width() + 3 <= width {
            spans.push(Span::raw(" ".repeat(width - used - help.width())));
            spans.push(Span::styled(help, t.dim()));
        }
        let x = area
            .x
            .saturating_add(u16::try_from(used.min(width - 1)).unwrap_or(0));
        frame.set_cursor_position(Position::new(x, area.y));
        Line::from(spans)
    } else if let Some(message) = app.current_flash(ctx.now) {
        Line::from(fit(
            vec![
                Span::raw(" "),
                Span::styled(flat(message).into_owned(), t.bold()),
            ],
            width,
        ))
    } else {
        hints_line(hints, width, t)
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn hints_line(hints: &[Hint], width: usize, t: Theme) -> Line<'static> {
    let cost = |h: &Hint| 2 + h.key.width() + 1 + h.label.width();
    let mut shown = hints.to_vec();
    // The first hint is preceded by one space, not two.
    while !shown.is_empty() && shown.iter().map(cost).sum::<usize>() - 1 > width {
        let lowest = shown
            .iter()
            .enumerate()
            .min_by_key(|(_, h)| h.keep)
            .map_or(0, |(i, _)| i);
        shown.remove(lowest);
    }
    let mut spans = Vec::with_capacity(shown.len() * 3);
    for (i, h) in shown.iter().enumerate() {
        spans.push(Span::raw(if i == 0 { " " } else { "  " }));
        spans.push(Span::styled(h.key, t.bold()));
        spans.push(Span::styled(format!(" {}", h.label), t.dim()));
    }
    Line::from(spans)
}

// ─── small helpers ──────────────────────────────────────────────────────────

/// A bordered pane of the dashboard.
fn panel(t: Theme) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(t.border())
        // Titles sit on the border; don't let them inherit its gray.
        .title_style(Style::reset())
        .padding(Padding::horizontal(1))
}

/// A box drawn over the dashboard: same shape, brighter border.
fn overlay(t: Theme) -> Block<'static> {
    let border = if t.color {
        Style::new().fg(Color::Cyan)
    } else {
        Style::new()
    };
    panel(t).border_style(border)
}

/// `ykn4vz9v-home-manager-generation`, the hash dim.
fn path_spans(t: Theme, path: &StorePath) -> Vec<Span<'static>> {
    let hash = path.hash();
    vec![
        Span::styled(format!("{}-", &hash[..8.min(hash.len())]), t.dim()),
        Span::raw(path.name().to_string()),
    ]
}

/// First 7 characters of a revision, keeping a `-dirty` suffix.
fn short_rev(rev: &str) -> String {
    let flat = flat(rev);
    match flat.strip_suffix("-dirty") {
        Some(base) => format!("{}-dirty", base.chars().take(7).collect::<String>()),
        None => flat.chars().take(7).collect(),
    }
}

/// `4mo`: [`util::format_ago`] without the ` ago`.
fn age(then: i64, now: i64) -> String {
    let ago = util::format_ago(then, now);
    match ago.strip_suffix(" ago") {
        Some(age) => age.to_string(),
        None => ago,
    }
}

/// How long the host's current activity has been running (the diff, else
/// the evaluation, else the probe — like [`HostState::activity`]).
fn running_for(h: &HostState, now: Instant) -> Option<Duration> {
    h.diff
        .running_for(now)
        .or_else(|| h.eval.running_for(now))
        .or_else(|| h.probe.running_for(now))
}

/// Whole seconds for busy labels (`3s`, `1m05s`); nothing under a second,
/// so fast work doesn't flicker.
fn seconds(d: Duration) -> Option<String> {
    let s = d.as_secs();
    match s {
        0 => None,
        1..60 => Some(format!("{s}s")),
        60..3600 => Some(format!("{}m{:02}s", s / 60, s % 60)),
        _ => Some(format!("{}h{:02}m", s / 3600, s % 3600 / 60)),
    }
}

/// An error summary for the table: one line, store paths shortened like
/// everywhere else here (`path 'wdfh0jh5-gen.drv' is not valid`).
fn summary(text: &str) -> String {
    const STORE: &str = "/nix/store/";
    const BASE32: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";
    let text = flat(text);
    let mut out = String::with_capacity(text.len());
    let mut rest: &str = &text;
    while let Some(i) = rest.find(STORE) {
        let after = &rest[i + STORE.len()..];
        let bytes = after.as_bytes();
        let hashed =
            bytes.len() > 33 && bytes[..32].iter().all(|b| BASE32.contains(b)) && bytes[32] == b'-';
        out.push_str(&rest[..i]);
        if hashed {
            out.push_str(&after[..8]);
            rest = &after[32..];
        } else {
            out.push_str(STORE);
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// Untrusted text for one line: [`util::sanitize_terminal`], with newlines
/// and tabs as spaces.
fn flat(s: &str) -> Cow<'_, str> {
    let clean = util::sanitize_terminal(s);
    if clean.contains(['\n', '\t']) {
        Cow::Owned(clean.replace(['\n', '\t'], " "))
    } else {
        clean
    }
}

/// Untrusted text that may span lines (for [`wrap`]): sanitized, tabs as
/// spaces (terminals would drop them).
fn clean(s: &str) -> Cow<'_, str> {
    let clean = util::sanitize_terminal(s);
    if clean.contains('\t') {
        Cow::Owned(clean.replace('\t', "    "))
    } else {
        clean
    }
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(Span::width).sum()
}

/// `spans` cut to `max` columns, ending in `…` when cut.
fn fit(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
    if spans_width(&spans) <= max {
        return spans;
    }
    let mut out = Vec::with_capacity(spans.len());
    let mut room = max;
    for span in spans {
        let w = span.width();
        // Something follows, so a span needs room for the `…` after it.
        if w < room {
            room -= w;
            out.push(span);
            continue;
        }
        if room > 0 {
            out.push(Span::styled(ellipsize(&span.content, room), span.style));
        }
        break;
    }
    out
}

/// The start of `s` in at most `max - 1` columns, then `…`.
fn ellipsize(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max * 4) + 3);
    let mut width = 0;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if width + w + 1 > max {
            break;
        }
        width += w;
        out.push(c);
    }
    out.push('…');
    out
}

/// The end of `s` within `max` columns, starting with `…` when cut.
fn tail(s: &str, max: usize) -> Cow<'_, str> {
    if s.width() <= max {
        return Cow::Borrowed(s);
    }
    if max == 0 {
        return Cow::Borrowed("");
    }
    let mut width = 1;
    let mut start = s.len();
    for (i, c) in s.char_indices().rev() {
        let w = c.width().unwrap_or(0);
        if width + w > max {
            break;
        }
        width += w;
        start = i;
    }
    Cow::Owned(format!("…{}", &s[start..]))
}

/// Word-wrap styled text to `width` columns into `out`: breaks after
/// spaces where possible and inside words longer than a line; `\n` starts
/// a new line; continuation lines are indented by `hang` columns (none on
/// panes too narrow for it).
fn wrap(spans: Vec<Span<'static>>, width: usize, hang: usize, out: &mut Vec<Line<'static>>) {
    let width = width.max(1);
    let hang = if hang * 2 > width { 0 } else { hang };
    let mut w = Wrapper {
        out,
        width,
        hang,
        line: Vec::new(),
        used: 0,
        start: 0,
    };
    for span in spans {
        for (i, part) in span.content.split('\n').enumerate() {
            if i > 0 {
                w.newline();
            }
            w.text(part, span.style);
        }
    }
    w.finish();
}

struct Wrapper<'o> {
    out: &'o mut Vec<Line<'static>>,
    width: usize,
    hang: usize,
    line: Vec<Span<'static>>,
    /// Columns used on the current line.
    used: usize,
    /// Columns of the current line's indentation.
    start: usize,
}

impl Wrapper<'_> {
    fn text(&mut self, text: &str, style: Style) {
        let mut rest = text;
        while !rest.is_empty() {
            // A word and the spaces after it.
            let word_end = rest.find(' ').unwrap_or(rest.len());
            let token_end = rest[word_end..]
                .find(|c| c != ' ')
                .map_or(rest.len(), |i| word_end + i);
            let (token, next) = rest.split_at(token_end);
            let word_w = rest[..word_end].width();
            if word_w > self.width - self.hang.min(self.width) {
                // Longer than any line: break it where we are.
                self.hard(token, style);
            } else {
                if self.used + word_w > self.width && self.used > self.start {
                    self.newline();
                }
                self.push(token, style);
            }
            rest = next;
        }
    }

    /// Character by character, for a word longer than a line.
    fn hard(&mut self, token: &str, style: Style) {
        let mut piece = String::new();
        let mut piece_w = 0;
        for c in token.chars() {
            let w = c.width().unwrap_or(0);
            if self.used + piece_w + w > self.width && self.used + piece_w > self.start {
                self.push(&std::mem::take(&mut piece), style);
                piece_w = 0;
                self.newline();
            }
            piece.push(c);
            piece_w += w;
        }
        self.push(&piece, style);
    }

    fn push(&mut self, text: &str, style: Style) {
        if text.is_empty() {
            return;
        }
        self.used += text.width();
        match self.line.last_mut() {
            Some(last) if last.style == style => last.content.to_mut().push_str(text),
            _ => self.line.push(Span::styled(text.to_string(), style)),
        }
    }

    fn newline(&mut self) {
        let line = std::mem::take(&mut self.line);
        self.out.push(Line::from(line));
        self.start = self.hang;
        self.used = self.hang;
        if self.hang > 0 {
            self.line.push(Span::raw(" ".repeat(self.hang)));
        }
    }

    fn finish(self) {
        self.out.push(Line::from(self.line));
    }
}

/// Cut `h` rows off the top of `area`.
fn take_top(area: &mut Rect, h: u16) -> Rect {
    let h = h.min(area.height);
    let top = Rect { height: h, ..*area };
    area.y += h;
    area.height -= h;
    top
}

/// Cut `h` rows off the bottom of `area`.
fn take_bottom(area: &mut Rect, h: u16) -> Rect {
    let h = h.min(area.height);
    area.height -= h;
    Rect {
        y: area.y + area.height,
        height: h,
        ..*area
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let (width, height) = (width.min(area.width), height.min(area.height));
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    use super::*;
    use crate::config::Kind;
    use crate::model::{AppState, Desired, ErrorInfo, Event};
    use crate::tui::app::testing::*;

    const SIZES: [(u16, u16); 4] = [(120, 32), (80, 24), (60, 20), (40, 10)];

    fn render(app: &App, (width, height): (u16, u16), now: Instant) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, app, NOW_UNIX, now))
            .expect("draw");
        terminal
    }

    /// One snapshot per size, each from a fresh app (frames remember
    /// scroll offsets).
    fn snapshots(name: &str, make: impl Fn() -> (App, Instant)) {
        for size in SIZES {
            let (app, now) = make();
            let terminal = render(&app, size, now);
            insta::assert_snapshot!(format!("{name}_{}x{}", size.0, size.1), terminal.backend());
        }
    }

    fn id_of(app: &App, name: &str) -> HostId {
        app.state
            .hosts
            .iter()
            .position(|h| h.host.name == name)
            .expect("host exists")
    }

    fn key(c: char) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Char(c))
    }

    /// The mixed state 12 s into the check (host 2 still evaluating).
    fn mixed_app() -> (App, Instant) {
        let s = mixed();
        let now = s.at(12_000);
        let mut app = App::new(s.state, false);
        app.spinner = 4;
        (app, now)
    }

    /// The mixed state once its check is over, then 3 s into a second
    /// check: every host has its answers again (the evaluations from the
    /// cache) except web4, the last one, still evaluating.
    fn refreshing_app() -> (App, Instant) {
        let mut s = mixed();
        let al2023 = desired(path('d', "home-manager-generation"), false);
        s.apply(
            11_000,
            Event::EvalFinished {
                id: 2,
                result: Ok(al2023),
            },
        );
        s.apply(11_000, Event::RunFinished);
        s.apply(20_000, Event::RunStarted);
        for id in 0..12 {
            s.apply(20_000, Event::ProbeStarted(id));
        }
        for id in 0..12 {
            let h = &s.state.hosts[id];
            let (result, deployed) = (h.probe.get().cloned().unwrap(), h.deployed.clone());
            let eval = h.eval.get().cloned().unwrap();
            s.apply(
                20_050,
                Event::ProbeFinished {
                    id,
                    result,
                    deployed,
                },
            );
            if id == 11 {
                s.apply(20_400, Event::EvalStarted(id));
            } else {
                let result = eval.map(|d| Desired { cached: true, ..d });
                s.apply(20_400, Event::EvalFinished { id, result });
            }
        }
        let now = s.at(23_000);
        let mut app = App::new(s.state, false);
        app.spinner = 4;
        (app, now)
    }

    fn text(buffer: &Buffer) -> Vec<String> {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    /// Where `needle` starts in the buffer (cells hold one char each here).
    fn find(buffer: &Buffer, needle: &str) -> Option<(u16, u16)> {
        let first = needle.chars().next()?;
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                let matches = needle.chars().enumerate().all(|(i, c)| {
                    let x = x as usize + i;
                    x < buffer.area.width as usize
                        && buffer[(x as u16, y)].symbol() == c.to_string()
                });
                if matches && buffer[(x, y)].symbol() == first.to_string() {
                    return Some((x, y));
                }
            }
        }
        None
    }

    fn ctx(app: &App, now: Instant) -> Ctx<'_> {
        Ctx {
            app,
            t: Theme { color: app.color },
            now,
            now_unix: NOW_UNIX,
        }
    }

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn details_text(app: &App, name: &str, width: usize, now: Instant) -> Vec<String> {
        let id = id_of(app, name);
        details_lines(&ctx(app, now), id, width)
            .iter()
            .map(line_text)
            .collect()
    }

    fn row_of(buffer: &Buffer, needle: &str) -> u16 {
        find(buffer, needle)
            .unwrap_or_else(|| panic!("{needle:?} not on screen:\n{}", text(buffer).join("\n")))
            .1
    }

    #[test]
    fn colors_mark_statuses_activity_and_the_selection() {
        let (mut app, now) = mixed_app();
        app.color = true;
        let terminal = render(&app, (120, 32), now);
        let buffer = terminal.backend().buffer();
        let cell = |needle: &str| {
            let (x, y) = find(buffer, needle).unwrap_or_else(|| panic!("{needle:?} not found"));
            buffer[(x, y)].clone()
        };
        assert_eq!(cell("✓ up to date").fg, Color::Green);
        assert_eq!(cell("✗ outdated").fg, Color::Yellow);
        assert_eq!(cell("○ not activated").fg, Color::Yellow);
        assert_eq!(cell("! eval failed").fg, Color::Red);
        assert_eq!(cell("! probe failed").fg, Color::Red);
        assert_eq!(cell("? unreachable").fg, Color::Magenta);
        let offline = cell("– offline");
        assert_eq!(offline.fg, Color::Reset);
        assert!(offline.modifier.contains(Modifier::DIM));
        assert_eq!(cell("⠼ evaluating").fg, Color::Cyan, "spinner");
        assert_eq!(cell("•").fg, Color::Cyan, "this machine");
        // Secondary text is dim, an error summary isn't.
        assert!(cell("unknown generation").modifier.contains(Modifier::DIM));
        assert!(!cell("probe output ended").modifier.contains(Modifier::DIM));
        assert_eq!(cell("⟳ reboot").fg, Color::Yellow);
        // The selection: ▶ and the HOST cell reversed, no colored band.
        let selected = row_of(buffer, "▶ al2-aarch64");
        assert_eq!(cell("▶").fg, Color::Cyan);
        for x in 4..17 {
            let c = &buffer[(x, selected)];
            assert!(c.modifier.contains(Modifier::REVERSED), "x={x}");
            assert_eq!((c.fg, c.bg), (Color::Reset, Color::Reset), "x={x}");
        }
        for x in 0..120 {
            assert_eq!(buffer[(x, selected)].bg, Color::Reset, "x={x}");
        }
        let other = row_of(buffer, "al2-x86_64");
        assert!(!buffer[(5, other)].modifier.contains(Modifier::REVERSED));
        // Gray borders, titles in the normal color, the revision bold.
        assert_eq!(buffer[(0, 1)].symbol(), "╭");
        assert_eq!(buffer[(0, 1)].fg, Color::DarkGray);
        let title = cell("al2-aarch64 · home-manager");
        assert_eq!(title.fg, Color::Reset);
        assert!(title.modifier.contains(Modifier::BOLD));
        assert!(cell("42ea902").modifier.contains(Modifier::BOLD));
        // The header's counts: icons colored; the check's progress spins.
        let counts = row_of(buffer, "2 ✓");
        assert_eq!(counts, 0);
        assert_eq!(cell("✓  4").fg, Color::Green);
        assert_eq!(row_of(buffer, "⠼ checking 11/12"), 0);
        assert_eq!(cell("⠼ checking").fg, Color::Cyan);
    }

    #[test]
    fn a_selected_row_keeps_its_status_colors_on_the_default_background() {
        // Red or magenta on a gray band is unreadable in common palettes
        // (Alacritty's default red on its gray: 1.09:1).
        for (name, status, color) in [
            ("desk", "! eval failed", Color::Red),
            ("pi", "? unreachable", Color::Magenta),
            ("bigmem", "✗ outdated", Color::Yellow),
        ] {
            let (mut app, now) = mixed_app();
            app.color = true;
            app.select(id_of(&app, name));
            let terminal = render(&app, (120, 32), now);
            let buffer = terminal.backend().buffer();
            let y = row_of(buffer, &format!("▶ {name}"));
            let (x, _) = find(buffer, status).expect("status shown");
            for x in x..x + status.chars().count() as u16 {
                let c = &buffer[(x, y)];
                assert_eq!((c.fg, c.bg), (color, Color::Reset), "{name} x={x}");
                assert!(!c.modifier.contains(Modifier::REVERSED), "{name} x={x}");
            }
            for x in 0..120 {
                assert_eq!(buffer[(x, y)].bg, Color::Reset, "{name} x={x}");
            }
            // The local marker is reversed with the name, not a cyan block.
            if name == "bigmem" {
                let (x, _) = find(buffer, "•").expect("marker shown");
                assert_eq!(buffer[(x, y)].fg, Color::Reset);
                assert!(buffer[(x, y)].modifier.contains(Modifier::REVERSED));
            }
        }
    }

    #[test]
    fn diff_colors_follow_the_change_kind() {
        let mut s = mixed();
        s.diffed(
            3,
            1_200,
            Ok(DiffOutcome::Diff {
                diff: fixture_diff(),
            }),
        );
        let now = s.at(12_000);
        let mut app = App::new(s.state, true);
        app.mode = Mode::Diff { id: 3, scroll: 0 };
        let terminal = render(&app, (120, 32), now);
        let buffer = terminal.backend().buffer();
        let at = |needle: &str| {
            let (x, y) = find(buffer, needle).unwrap_or_else(|| panic!("{needle:?} not found"));
            buffer[(x, y)].clone()
        };
        assert_eq!(at("+  zenoh ").fg, Color::Green);
        assert_eq!(at("~  atuin").fg, Color::Yellow);
        let resized = at("±  home-manager");
        assert!(resized.modifier.contains(Modifier::DIM));
        assert_eq!(at("1.10.0, 1.10.0-fish").fg, Color::Green);
        assert!(
            at("∅ → 1.10.0").modifier.contains(Modifier::DIM),
            "nothing removed"
        );
        assert_eq!(at("4 added").fg, Color::Green);
    }

    #[test]
    fn without_colors_only_modifiers_are_used() {
        for (name, make) in [
            ("mixed", mixed_app as fn() -> (App, Instant)),
            ("diff", || {
                let mut s = mixed();
                s.diffed(
                    3,
                    1_200,
                    Ok(DiffOutcome::Diff {
                        diff: fixture_diff(),
                    }),
                );
                let now = s.at(12_000);
                let mut app = App::new(s.state, false);
                app.mode = Mode::Diff { id: 3, scroll: 0 };
                (app, now)
            }),
            ("confirm", || {
                let (mut app, now) = mixed_app();
                app.on_key(key('a'), now);
                (app, now)
            }),
        ] {
            let (app, now) = make();
            let terminal = render(&app, (120, 32), now);
            let buffer = terminal.backend().buffer();
            for c in &buffer.content {
                assert_eq!((c.fg, c.bg), (Color::Reset, Color::Reset), "{name}: {c:?}");
            }
            if name == "mixed" {
                // The selected host is reversed, the rest of its row isn't.
                let y = row_of(buffer, "▶ al2-aarch64");
                assert!(buffer[(10, y)].modifier.contains(Modifier::REVERSED));
                let (status, _) = find(buffer, "✗ outdated").expect("status shown");
                assert!(!buffer[(status, y)].modifier.contains(Modifier::REVERSED));
            }
        }
    }

    #[test]
    fn filter_prompt_puts_the_cursor_after_the_text() {
        let (mut app, now) = mixed_app();
        for c in "/web".chars() {
            app.on_key(key(c), now);
        }
        let mut terminal = render(&app, (80, 24), now);
        let lines = text(terminal.backend().buffer());
        assert!(lines[23].starts_with(" /web "), "{:?}", lines[23]);
        assert!(lines[23].ends_with("enter keep · esc clear "));
        assert!(lines[1].contains(" /web · 4 of 12 "), "{:?}", lines[1]);
        assert!(terminal.backend().cursor_visible());
        terminal.backend_mut().assert_cursor_position((5, 23));
        // A filter longer than the footer shows its end.
        app.filter = "x".repeat(60);
        let mut terminal = render(&app, (40, 10), now);
        let footer = text(terminal.backend().buffer())[9].clone();
        assert!(footer.starts_with(" /…xxx"), "{footer:?}");
        terminal.backend_mut().assert_cursor_position((39, 9));
        // Outside the prompt the cursor is hidden.
        app.mode = Mode::Normal;
        let terminal = render(&app, (80, 24), now);
        assert!(!terminal.backend().cursor_visible());
    }

    #[test]
    fn untrusted_text_cannot_reach_the_terminal() {
        let mut evil = ssh_host("evil\u{1b}]52;c;Zm9v\u{7}", Kind::Nixos, "evil");
        evil.note = Some("\u{202e}gnp.exe\ttab".into());
        let mut s = Script::new(vec![evil]);
        s.flake(10);
        s.probed(
            0,
            10,
            crate::probe::ProbeReport {
                none_reason: Some("no profile \u{1b}[2J here".into()),
                platform: Some("x86_64-linux\u{9b}31m".into()),
                ..Default::default()
            },
        );
        s.eval_failed(
            0,
            20,
            ErrorInfo::new("bad\u{1b}[31m", "line one\n\u{1b}]0;title\u{7}"),
        );
        let now = s.at(100);
        let app = App::new(s.state, true);
        for mode in [Mode::Normal, Mode::Help] {
            let mut app = App::new(app.state.clone(), true);
            app.mode = mode;
            let terminal = render(&app, (120, 32), now);
            let buffer = terminal.backend().buffer();
            for c in &buffer.content {
                assert!(
                    !c.symbol()
                        .chars()
                        .any(|ch| ch.is_control() || ('\u{202a}'..='\u{202e}').contains(&ch)),
                    "{:?}",
                    c.symbol()
                );
            }
            if app.mode == Mode::Normal {
                let screen = text(buffer).join("\n");
                assert!(screen.contains("evil\\u{1b}]52;c;Zm9v\\u{7}"), "{screen}");
                assert!(screen.contains("\\u{202e}gnp.exe tab"), "{screen}");
            }
        }
    }

    #[test]
    fn renders_at_any_size_without_panicking() {
        let scenarios: Vec<fn() -> (App, Instant)> = vec![
            mixed_app,
            || {
                let s = pending();
                let now = s.at(2_500);
                (App::new(s.state, true), now)
            },
            || {
                let (mut app, now) = mixed_app();
                app.on_key(key('?'), now);
                (app, now)
            },
            || {
                let (mut app, now) = mixed_app();
                app.on_key(key('a'), now);
                (app, now)
            },
            || {
                let (mut app, now) = mixed_app();
                for c in "/zz".chars() {
                    app.on_key(key(c), now);
                }
                (app, now)
            },
            || {
                let mut s = mixed();
                s.diffed(
                    3,
                    10,
                    Ok(DiffOutcome::Diff {
                        diff: fixture_diff(),
                    }),
                );
                let now = s.at(12_000);
                let mut app = App::new(s.state, true);
                app.mode = Mode::Diff { id: 3, scroll: 400 };
                (app, now)
            },
            || {
                let mut s = mixed();
                s.diffed(0, 10, Ok(DiffOutcome::DesiredMissing));
                let now = s.at(12_000);
                let mut app = App::new(s.state, true);
                app.mode = Mode::Diff { id: 0, scroll: 0 };
                app.on_key(key('b'), now);
                (app, now)
            },
            || (App::new(AppState::default(), true), Instant::now()),
        ];
        let sizes = [
            (1, 1),
            (2, 1),
            (1, 3),
            (5, 3),
            (10, 4),
            (20, 6),
            (30, 8),
            (39, 9),
            (41, 11),
            (100, 7),
            (7, 100),
            (250, 70),
        ];
        for make in &scenarios {
            for size in sizes {
                let (mut app, now) = make();
                app.detail_scroll = u16::MAX;
                render(&app, size, now);
            }
        }
    }

    #[test]
    fn the_revision_survives_every_width() {
        let (app, now) = mixed_app();
        let pending = pending();
        let resolving = App::new(pending.state, false);
        for width in 20..=140 {
            let header = line_text(&header_line(&ctx(&app, now), width));
            assert!(header.contains("42ea902"), "{width}: {header:?}");
            assert!(header.width() <= width, "{width}: {header:?}");
            let header = line_text(&header_line(&ctx(&resolving, now), width));
            assert!(
                header.contains("resolving") || width < 40,
                "{width}: {header:?}"
            );
            assert!(header.width() <= width, "{width}: {header:?}");
        }
    }

    #[test]
    fn the_header_says_a_check_runs_even_when_no_busy_host_is_shown() {
        let (app, now) = refreshing_app();
        let terminal = render(&app, (40, 10), now);
        let lines = text(terminal.backend().buffer());
        // The rows shown keep their statuses: nothing there spins.
        assert!(
            lines[1..].iter().all(|l| !l.contains('⠼')),
            "{}",
            lines.join("\n")
        );
        assert_eq!(lines[0], " 42ea902     ⠼ 11/12  3✓ 4✗ 1○ 2! 1? 1– ");
        // (left, right) of the header at `width`, which it fills but for
        // the last column.
        let header = |app: &App, width| {
            let line = line_text(&header_line(&ctx(app, now), width));
            assert_eq!(line.width(), width - 1, "{line:?}");
            let (left, right) = line.split_once("     ").expect("a gap");
            (left.to_string(), right.trim_start().to_string())
        };
        let counts = "3 ✓  4 ✗  1 ○  2 !  1 ?  1 –";
        assert_eq!(
            header(&app, 120),
            (
                " hostctl  github:jjant/nix-config @ 42ea902 · 2h ago".into(),
                format!("⠼ checking 11/12   {counts}")
            )
        );
        assert_eq!(
            header(&app, 80),
            (
                " github:jjant/nix-config @ 42ea902".into(),
                format!("⠼ 11/12   {counts}")
            )
        );
        // Once it's over, only the counts.
        let mut app = app;
        app.state.apply(Event::RunFinished, now);
        assert_eq!(
            header(&app, 80),
            (
                " github:jjant/nix-config @ 42ea902 · 2h ago".into(),
                counts.into()
            )
        );
    }

    #[test]
    fn header_shows_resolution_problems() {
        let mut s = mixed();
        let mut info = flake_info();
        info.stale = Some("rate limited by GitHub".into());
        info.lock_outdated = true;
        s.apply(
            400,
            Event::FlakeFinished {
                url: FLAKE.into(),
                result: Ok(info),
            },
        )
        .apply(11_000, Event::RunFinished);
        let now = s.at(12_000);
        let app = App::new(s.state.clone(), false);
        let header = line_text(&header_line(&ctx(&app, now), 140));
        assert!(
            header.contains("github:jjant/nix-config @ 42ea902 · 2h ago · stale: rate limited by GitHub · flake.lock out of date"),
            "{header}"
        );
        // A second flake is mentioned; a failed resolution is shown in red.
        let mut other = ssh_host("lab", Kind::Nixos, "lab");
        other.flake = "git+ssh://git@example.com/lab".into();
        let mut hosts: Vec<_> = s.state.hosts.iter().map(|h| h.host.clone()).collect();
        hosts.push(other);
        let mut state = AppState::new(hosts);
        state.apply(
            Event::FlakeFinished {
                url: FLAKE.into(),
                result: Err(ErrorInfo::msg(
                    "unable to download 'https://api.github.com/…': HTTP error 403",
                )),
            },
            now,
        );
        let app = App::new(state, false);
        let header = line_text(&header_line(&ctx(&app, now), 140));
        assert!(
            header.contains("github:jjant/nix-config: unable to download 'https://api.github.com/…': HTTP error 403 +1 flake"),
            "{header}"
        );
        let lab = id_of(&app, "lab");
        let flake_line = details_lines(&ctx(&app, now), lab, 100)
            .iter()
            .map(line_text)
            .find(|l| l.starts_with("flake"));
        assert_eq!(
            flake_line.as_deref(),
            Some("flake    git+ssh://git@example.com/lab")
        );
    }

    #[test]
    fn global_errors_get_a_line_under_the_header() {
        let (mut app, now) = mixed_app();
        app.state.apply(
            Event::Error(ErrorInfo::msg("nix.command \"nix\" can't be run")),
            now,
        );
        app.state.apply(
            Event::Error(ErrorInfo::msg("ssh.command \"ssh\" can't be run")),
            now,
        );
        let terminal = render(&app, (80, 24), now);
        let lines = text(terminal.backend().buffer());
        assert_eq!(
            lines[1].trim_end(),
            " ! ssh.command \"ssh\" can't be run (+1 more)"
        );
        assert!(lines[2].starts_with("╭"));
    }

    #[test]
    fn details_explain_each_status() {
        let (app, now) = mixed_app();
        let details = |name| details_text(&app, name, 100, now);
        let web1 = details("web1");
        assert_eq!(
            web1[0],
            "current  jjjjjjjj-nixos-system-web1-25.11 · specialisation gaming · gen 230 · activated 2h ago"
        );
        assert!(web1.iter().any(|l| l.starts_with(
            "reboot   booted kernel, initrd or modules differ · booted llllllll-nixos-system-web1-25.11"
        )));
        assert!(
            !web1.iter().any(|l| l.starts_with("activate")),
            "up to date: no activation"
        );
        assert_eq!(
            web1.last().map(String::as_str),
            Some("timings  probe 20ms · eval 4.1s")
        );

        let web4 = details("web4");
        assert_eq!(
            web4[1],
            "desired  rrrrrrrr-nixos-system-web4-25.11 · 42ea902 · cached"
        );
        assert_eq!(
            web4.last().map(String::as_str),
            Some("timings  probe 18ms · eval cached")
        );

        let x86 = details("al2-x86_64");
        assert_eq!(x86[0], "current  none: no home-manager profile");
        assert!(
            x86.iter()
                .any(|l| l.starts_with("activate on al2-x86_64: out=$(nix"))
        );

        let pi = details("pi");
        assert_eq!(
            pi[0],
            "current  unreachable: ssh: Could not resolve hostname pi: Name or service not known"
        );
        assert_eq!(pi[1], "         ssh -T -o BatchMode=yes … pi sh -s");
        assert_eq!(pi[3], "         hint: is `pi` in your ssh config or DNS?");
        assert!(pi[4].starts_with("desired  iiiiiiii-nixos-system-pi-25.11"));

        let mac = details("mac-m1");
        assert_eq!(
            mac[0],
            "current  offline: ssh: connect to host localhost port 2022: Connection refused"
        );
        assert!(
            mac.iter()
                .any(|l| l == "note     reachable only from a desk you ssh into from the Mac")
        );
        assert!(
            !mac.iter().any(|l| l.starts_with("activate")),
            "offline needs nothing"
        );

        let web2 = details("web2");
        assert!(
            web2.iter()
                .any(|l| l.starts_with("drift    staged: the profile already points"))
        );
        assert!(
            !web2.iter().any(|l| l.starts_with("profile")),
            "staged explains the profile"
        );
        let web3 = details("web3");
        assert!(
            web3.iter()
                .any(|l| l.starts_with("drift    same rev, different build: "))
        );
        let bigmem = details("bigmem");
        assert!(
            bigmem
                .iter()
                .any(|l| l == "drift    unknown generation: not one hostctl has evaluated")
        );
        assert!(
            bigmem
                .iter()
                .any(|l| l.starts_with("activate here: out=$(nix"))
        );

        let busy = details("al2023-x86_64");
        assert_eq!(busy[1], "desired  ⠼ evaluating 11s");
        let pending = pending();
        let app = App::new(pending.state, false);
        let now = pending.t0 + Duration::from_millis(2_500);
        let al2 = details_text(&app, "al2-aarch64", 100, now);
        assert_eq!(
            al2[..2],
            ["current  ⠋ probing 2s", "desired  waiting for the flake"]
        );
        let bigmem = details_text(&app, "bigmem", 100, now);
        assert!(
            bigmem[0].ends_with("· gen 9 · activated 12m ago"),
            "{:?}",
            bigmem[0]
        );
    }

    #[test]
    fn details_scroll_is_clamped_and_shown() {
        let (mut app, now) = mixed_app();
        let desk = id_of(&app, "desk");
        app.select(desk);
        let terminal = render(&app, (80, 24), now);
        let lines = text(terminal.backend().buffer());
        let max = app.geometry().detail_max.load(Relaxed);
        assert!(max > 10, "the IFD trace doesn't fit: {max}");
        assert!(
            lines[22].contains(&format!("1–5 of {} · J/K", max + 5)),
            "{:?}",
            lines[22]
        );
        app.detail_scroll = u16::MAX;
        let terminal = render(&app, (80, 24), now);
        let lines = text(terminal.backend().buffer());
        assert!(
            lines[22].contains(&format!("{}–{} of {}", max + 1, max + 5, max + 5)),
            "{:?}",
            lines[22]
        );
        assert!(
            lines[21].contains("timings  probe 30ms · eval 2.5s"),
            "{:?}",
            lines[21]
        );
    }

    #[test]
    fn the_table_scrolls_only_when_the_selection_leaves_it() {
        let (mut app, now) = mixed_app();
        let offset = |app: &App| app.geometry().table_offset.load(Relaxed);
        render(&app, (40, 10), now);
        assert_eq!(app.geometry().table_rows.load(Relaxed), 5);
        assert_eq!(offset(&app), 0);
        app.on_key(key('G'), now);
        let terminal = render(&app, (40, 10), now);
        assert_eq!(offset(&app), 7);
        assert!(text(terminal.backend().buffer())[8].contains("8–12 of 12"));
        app.on_key(key('k'), now);
        render(&app, (40, 10), now);
        assert_eq!(offset(&app), 7, "still visible: no scrolling");
        for _ in 0..4 {
            app.on_key(key('k'), now);
        }
        render(&app, (40, 10), now);
        assert_eq!(offset(&app), 6);
        // A taller screen shows everything from the top.
        render(&app, (40, 40), now);
        assert_eq!(offset(&app), 0);
    }

    fn many_changes(n: usize) -> crate::closure::ClosureDiff {
        let changes = (0..n)
            .map(|i| crate::closure::Change {
                name: format!("pkg-{i:03}"),
                removed: vec![format!("1.{i}")],
                added: vec![format!("2.{i}")],
                in_before: true,
                in_after: true,
                size_before: 1 << 20,
                size_after: (1 << 20) + (i as u64) * 4096,
            })
            .collect();
        crate::closure::ClosureDiff {
            changes,
            paths_before: 900,
            paths_after: 900,
            size_before: 1 << 30,
            size_after: (1 << 30) + 12_345_678,
        }
    }

    #[test]
    fn diff_scroll_is_clamped_and_shown() {
        let mut s = mixed();
        s.diffed(
            3,
            10,
            Ok(DiffOutcome::Diff {
                diff: many_changes(50),
            }),
        );
        let now = s.at(12_000);
        let mut app = App::new(s.state, false);
        app.mode = Mode::Diff { id: 3, scroll: 0 };
        let terminal = render(&app, (80, 24), now);
        let lines = text(terminal.backend().buffer());
        let rows = app.geometry().diff_rows.load(Relaxed);
        assert_eq!(
            rows,
            22 - 2 - 3,
            "screen minus header, footer, borders, summary"
        );
        assert_eq!(app.geometry().diff_max.load(Relaxed), 50 - rows);
        assert!(
            lines[22].contains(&format!("1–{rows} of 50")),
            "{:?}",
            lines[22]
        );
        assert!(
            lines[5].starts_with("│ ↑  pkg-000  1.0 → 2.0"),
            "{:?}",
            lines[5]
        );
        // Too far: the last page.
        app.mode = Mode::Diff { id: 3, scroll: 999 };
        let terminal = render(&app, (80, 24), now);
        let lines = text(terminal.backend().buffer());
        assert!(
            lines[22].contains(&format!("{}–50 of 50", 50 - rows + 1)),
            "{:?}",
            lines[22]
        );
        assert!(lines[21].contains("pkg-049"), "{:?}", lines[21]);
        // The keys use what the frame measured.
        app.on_key(
            crossterm::event::KeyEvent::from(crossterm::event::KeyCode::End),
            now,
        );
        assert_eq!(
            app.mode,
            Mode::Diff {
                id: 3,
                scroll: 50 - rows
            }
        );
        app.on_key(
            crossterm::event::KeyEvent::from(crossterm::event::KeyCode::PageUp),
            now,
        );
        assert_eq!(
            app.mode,
            Mode::Diff {
                id: 3,
                scroll: 50 - 2 * rows
            }
        );
    }

    #[test]
    fn diff_view_states() {
        let mut s = mixed();
        let current = store(HM_1);
        let desired = store(HM_9);
        s.apply(
            100,
            Event::DiffStarted {
                id: 3,
                current: current.clone(),
                desired: desired.clone(),
            },
        );
        let now = s.at(3_100);
        let mut app = App::new(s.state.clone(), false);
        app.mode = Mode::Diff { id: 3, scroll: 0 };
        let message = |app: &App| -> Vec<String> {
            diff_message(&ctx(app, now), &app.state.hosts[3], 76)
                .iter()
                .map(line_text)
                .collect()
        };
        assert_eq!(
            message(&app),
            [
                "⠋ comparing closures 3s",
                "listing both generations' closures in the local store"
            ]
        );
        app.state.apply(
            Event::DiffFinished {
                id: 3,
                current,
                desired,
                result: Err(ErrorInfo::new(
                    "nix-store: command not found",
                    "sh: line 1: nix-store: command not found\nexit 127",
                )),
            },
            now,
        );
        assert_eq!(
            message(&app),
            [
                "diff failed: nix-store: command not found",
                "",
                "sh: line 1: nix-store: command not found",
                "exit 127",
                "",
                "r tries again",
            ]
        );
        assert_eq!(
            hints_for(&app),
            "↑↓ scroll  a activate  r diff again  y copy  q back"
        );
        // No diff (yet, or any more after the host changed): say why.
        let app = App::new(mixed().state, false);
        assert_eq!(message(&app), ["no diff yet: r computes it"]);
        let web1 = &app.state.hosts[8];
        assert_eq!(
            diff_message(&ctx(&app, now), web1, 76)
                .iter()
                .map(line_text)
                .collect::<Vec<_>>(),
            ["web1 is up to date: nothing to diff"]
        );
    }

    fn hints_for(app: &App) -> String {
        let terminal = render(app, (120, 32), Instant::now());
        text(terminal.backend().buffer())[31].trim().to_string()
    }

    #[test]
    fn footer_hints_drop_the_least_useful_first() {
        let t = Theme { color: false };
        let hints = |width| line_text(&hints_line(&NORMAL_HINTS, width, t));
        assert_eq!(
            hints(120),
            " ↑↓ select  enter diff  a activate  y copy  r refresh  / filter  s sort  ? help  q quit"
        );
        assert_eq!(hints(87), hints(120), "exactly fits");
        assert_eq!(
            hints(86),
            " enter diff  a activate  y copy  r refresh  / filter  s sort  ? help  q quit"
        );
        assert_eq!(hints(40), " enter diff  a activate  ? help  q quit");
        assert_eq!(hints(12), " ? help");
        assert_eq!(hints(3), "");
        for width in 0..100 {
            assert!(hints(width).width() <= width, "{width}");
        }
        // Overlays show their own keys.
        let (mut app, now) = mixed_app();
        app.on_key(key('a'), now);
        let terminal = render(&app, (80, 24), now);
        let lines = text(terminal.backend().buffer());
        assert_eq!(lines[23].trim(), "");
        assert!(lines.iter().any(|l| l.contains("y run · n cancel")));
        // A flash replaces the hints.
        let (mut app, now) = mixed_app();
        app.on_key(key('s'), now);
        let terminal = render(&app, (80, 24), now);
        assert_eq!(
            text(terminal.backend().buffer())[23].trim(),
            "sorted by status"
        );
        assert!(text(terminal.backend().buffer())[1].ends_with(" by status ╮"));
    }

    #[test]
    fn wraps_words_long_words_and_newlines() {
        let wrapped = |text: &str, width, hang| {
            let mut out = Vec::new();
            wrap(vec![Span::raw(text.to_string())], width, hang, &mut out);
            out.iter().map(line_text).collect::<Vec<_>>()
        };
        assert_eq!(
            wrapped("the quick brown fox", 10, 0),
            ["the quick ", "brown fox"]
        );
        assert_eq!(
            wrapped("the quick brown fox", 10, 2),
            ["the quick ", "  brown ", "  fox"]
        );
        assert_eq!(
            wrapped("abcdefghijkl xy", 5, 0),
            ["abcde", "fghij", "kl xy"]
        );
        assert_eq!(
            wrapped("ab abcdefghijkl", 5, 0),
            ["ab ab", "cdefg", "hijkl"]
        );
        assert_eq!(wrapped("one\ntwo", 10, 0), ["one", "two"]);
        assert_eq!(wrapped("", 10, 0), [""]);
        assert_eq!(wrapped("   indented", 20, 0), ["   indented"]);
        assert_eq!(
            wrapped("日本語のテキスト", 6, 0),
            ["日本語", "のテキ", "スト"]
        );
        assert_eq!(wrapped("abc", 0, 0), ["a", "b", "c"], "width 0 acts as 1");
        // The hang is dropped when it would leave no room.
        assert_eq!(wrapped("aaaa bbbb", 6, 4), ["aaaa ", "bbbb"]);
        // Styles are kept per piece, and equal neighbours merge.
        let mut out = Vec::new();
        let bold = Style::new().add_modifier(Modifier::BOLD);
        wrap(
            vec![
                Span::raw("key "),
                Span::raw("value "),
                Span::styled("loud words", bold),
            ],
            16,
            0,
            &mut out,
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].spans.len(), 2, "{:?}", out[0]);
        assert_eq!(out[0].spans[0].content, "key value ");
        assert_eq!(out[0].spans[1].content, "loud ");
        assert_eq!(out[0].spans[1].style, bold);
        for width in 1..30 {
            for line in wrapped("a bb ccc dddd eeeee ffffff ggggggg", width, 3) {
                assert!(line.trim_end().width() <= width, "{width}: {line:?}");
            }
        }
    }

    #[test]
    fn fits_and_tails_by_display_width() {
        let spans = || vec![Span::raw("abcd"), Span::raw("efgh")];
        let text = |spans: Vec<Span>| {
            spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        };
        assert_eq!(text(fit(spans(), 8)), "abcdefgh");
        assert_eq!(text(fit(spans(), 7)), "abcdef…");
        assert_eq!(text(fit(spans(), 5)), "abcd…");
        assert_eq!(
            text(fit(spans(), 4)),
            "abc…",
            "a span filling the room still needs the …"
        );
        assert_eq!(text(fit(spans(), 1)), "…");
        assert_eq!(text(fit(spans(), 0)), "");
        assert_eq!(text(fit(vec![Span::raw("日本語")], 5)), "日本…");
        assert_eq!(tail("abcdef", 6), "abcdef");
        assert_eq!(tail("abcdef", 4), "…def");
        assert_eq!(tail("abcdef", 1), "…");
        assert_eq!(tail("abcdef", 0), "");
        assert_eq!(ellipsize("abc", 3), "ab…");
    }

    #[test]
    fn columns_shrink_in_the_documented_order() {
        let natural = Natural {
            host: 13,
            kind: 12,
            status: 16,
            generation: 3,
            activated: 9,
            detail: 50,
        };
        let fit = |avail| Columns::fit(avail, natural);
        // Everything, DETAIL at its natural width.
        let wide = fit(200);
        assert_eq!(
            (wide.kind, wide.generation, wide.activated, wide.detail),
            (Some(12), Some(3), Some(9), Some(50))
        );
        // DETAIL truncates first…
        let c = fit(80);
        assert_eq!(c.fixed_width(), 61);
        assert_eq!((c.kind, c.detail), (Some(12), Some(80 - 61 - 2)));
        assert_eq!(c.fixed_width() + GAP + c.detail.unwrap(), 80);
        // …then KIND goes, then GEN, then ACTIVATED.
        let c = fit(78);
        assert_eq!(
            (c.kind, c.generation, c.activated),
            (None, Some(3), Some(9))
        );
        let c = fit(64);
        assert_eq!((c.kind, c.generation, c.activated), (None, None, Some(9)));
        assert!(c.detail.unwrap() >= MIN_DETAIL);
        let c = fit(54);
        assert_eq!(
            (c.kind, c.generation, c.activated, c.detail),
            (None, None, None, Some(21))
        );
        // HOST and STATUS stay; DETAIL goes when there's no room for it.
        let c = fit(34);
        assert_eq!((c.host, c.status, c.detail), (13, 16, None));
        // Even they shrink when they alone don't fit.
        let c = fit(20);
        assert_eq!(c.host + GAP + c.status, 20);
        assert!(c.host >= 8);
        // Short details don't push columns out.
        let c = Columns::fit(
            70,
            Natural {
                detail: 18,
                ..natural
            },
        );
        assert_eq!((c.kind, c.detail), (None, Some(18)));
        let c = Columns::fit(
            83,
            Natural {
                detail: 18,
                ..natural
            },
        );
        assert_eq!((c.kind, c.detail), (Some(12), Some(18)));
        // No details at all: no DETAIL column, nothing dropped needlessly.
        let c = Columns::fit(
            66,
            Natural {
                detail: 0,
                ..natural
            },
        );
        assert_eq!((c.kind, c.detail), (Some(12), None));
    }

    #[test]
    fn diff_columns_shrink_versions_then_drop_the_kind() {
        let c = DiffColumns::fit(76, 23, 23, 10, 8);
        assert_eq!(
            c,
            DiffColumns {
                name: 23,
                versions: 23,
                delta: 10,
                kind: Some(8)
            }
        );
        let c = DiffColumns::fit(60, 23, 23, 10, 8);
        assert_eq!(c.kind, None);
        assert_eq!(c.versions, 60 - (1 + 2 + 23 + 2 + 2 + 10));
        let c = DiffColumns::fit(36, 23, 23, 0, 8);
        assert_eq!(c.kind, None);
        assert_eq!(c.delta, 0);
        assert!(c.name < 23 && c.versions >= 8, "{c:?}");
        assert!(1 + 2 + c.name + 2 + c.versions <= 36);
        assert_eq!(c.constraints().len(), 3);
    }

    #[test]
    fn scroll_offsets_stick_until_the_selection_leaves() {
        assert_eq!(scroll_offset(0, Some(3), 10, 5), 0);
        assert_eq!(scroll_offset(0, Some(7), 10, 5), 3);
        assert_eq!(scroll_offset(3, Some(4), 10, 5), 3, "visible: stays");
        assert_eq!(scroll_offset(3, Some(2), 10, 5), 2);
        assert_eq!(scroll_offset(8, Some(9), 10, 5), 5, "never past the end");
        assert_eq!(scroll_offset(4, None, 3, 5), 0);
        assert_eq!(scroll_offset(4, Some(1), 10, 0), 0);
    }

    #[test]
    fn the_body_split_keeps_the_table_useful() {
        let body = |height| Rect::new(0, 0, 80, height);
        let (table, details) = split_body(body(22), 5, true);
        assert_eq!((table.height, details.map(|d| d.height)), (8, Some(14)));
        let (table, details) = split_body(body(22), 30, true);
        assert_eq!(
            (table.height, details.map(|d| (d.y, d.height))),
            (14, Some((14, 8)))
        );
        let (table, details) = split_body(body(8), 12, true);
        assert_eq!((table.height, details), (8, None), "too short for both");
        let (table, details) = split_body(body(22), 5, false);
        assert_eq!((table.height, details), (22, None));
        let (table, details) = split_body(body(9), 1, true);
        assert_eq!((table.height, details.map(|d| d.height)), (4, Some(5)));
    }

    #[test]
    fn versions_width_matches_nix_rendering() {
        let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        for versions in [
            v(&[]),
            v(&[""]),
            v(&["1.10.0", "1.10.0-fish"]),
            v(&["", "2"]),
            v(&["日本"]),
        ] {
            assert_eq!(
                versions_width(&versions),
                closure::format_versions(&versions).width(),
                "{versions:?}"
            );
        }
    }

    #[test]
    fn drift_says_where_the_revision_comes_from() {
        let revision = |from_generation| Drift::Revision {
            rev: "1a2b3c4d5e6f".into(),
            last_modified: Some(NOW_UNIX - 3 * 86_400),
            from_generation,
        };
        assert_eq!(
            drift_long(&revision(true), NOW_UNIX),
            "on 1a2b3c4 (3d old), recorded by the generation"
        );
        assert_eq!(
            drift_long(&revision(false), NOW_UNIX),
            "on 1a2b3c4 (3d old), per hostctl's cache"
        );
        assert_eq!(
            drift_short(&Drift::Staged, NOW_UNIX),
            "staged: switch/reboot pending"
        );
    }

    #[test]
    fn marks_this_machine_and_assumed_ones() {
        let mut mac = local_host("mac-studio", Kind::Darwin);
        mac.target = Target::Local { assumed: true };
        let s = Script::new(vec![local_host("bigmem", Kind::HomeManager), mac]);
        let app = App::new(s.state, false);
        let terminal = render(&app, (80, 24), Instant::now());
        let screen = text(terminal.backend().buffer()).join("\n");
        assert!(screen.contains("▶ bigmem •  "), "{screen}");
        assert!(screen.contains("  mac-studio •?  "), "{screen}");
        assert!(
            screen.contains("╭ bigmem · home-manager · local ─"),
            "{screen}"
        );
    }

    #[test]
    fn diff_title_names_both_generations_when_they_differ() {
        let mut s = mixed();
        s.diffed(
            10,
            10,
            Ok(DiffOutcome::Diff {
                diff: fixture_diff(),
            }),
        );
        let app = App::new(s.state, false);
        let title: String = diff_title(&ctx(&app, Instant::now()), &app.state.hosts[3])
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(
            title,
            " bigmem  494amg2b → ykn4vz9v-home-manager-generation "
        );
        let mut web3 = app.state.hosts[10].clone();
        let older = path('s', "nixos-system-web3-25.05");
        web3.diff_inputs = Some((older, path('q', "nixos-system-web3-25.11")));
        let title: String = diff_title(&ctx(&app, Instant::now()), &web3)
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(
            title,
            " web3  ssssssss-nixos-system-web3-25.05 → qqqqqqqq-nixos-system-web3-25.11 "
        );
    }

    #[test]
    fn small_formatters() {
        assert_eq!(short_flake("github:jjant/nix-config"), "nix-config");
        assert_eq!(
            short_flake("git+https://example.com/x/y.git?ref=main"),
            "y.git"
        );
        assert_eq!(short_flake("path:/home/jj/nix/"), "nix");
        assert_eq!(short_flake("."), ".");
        assert_eq!(short_flake("nixpkgs"), "nixpkgs");
        assert_eq!(
            summary("path '/nix/store/wdfh0jh598v4qshmcs296ri45p0win4b-gen.drv' is not valid"),
            "path 'wdfh0jh5-gen.drv' is not valid"
        );
        assert_eq!(
            summary("in /nix/store/short-x and\nnext"),
            "in /nix/store/short-x and next"
        );
        assert_eq!(seconds(Duration::from_millis(999)), None);
        assert_eq!(seconds(Duration::from_secs(3)), Some("3s".into()));
        assert_eq!(seconds(Duration::from_secs(65)), Some("1m05s".into()));
        assert_eq!(seconds(Duration::from_secs(3720)), Some("1h02m".into()));
        assert_eq!(age(NOW_UNIX - 4 * 30 * 86_400, NOW_UNIX), "4mo");
        assert_eq!(age(NOW_UNIX, NOW_UNIX), "just now");
        assert_eq!(short_rev("42ea902131b93c04"), "42ea902");
        assert_eq!(short_rev("a8f62b6f2c1170af-dirty"), "a8f62b6-dirty");
        assert_eq!(short_rev("abc"), "abc");
        assert_eq!(flat("a\nb\tc\u{1b}"), "a b c\\u{1b}");
        assert_eq!(clean("a\nb\tc"), "a\nb    c");
        assert!(matches!(flat("plain"), Cow::Borrowed(_)));
        assert_eq!(
            drift_short(
                &Drift::Revision {
                    rev: "1a2b3c4d".into(),
                    last_modified: None,
                    from_generation: true
                },
                NOW_UNIX
            ),
            "on 1a2b3c4"
        );
        let centered_rect = centered(Rect::new(10, 5, 40, 10), 20, 4);
        assert_eq!(centered_rect, Rect::new(20, 8, 20, 4));
        assert_eq!(
            centered(Rect::new(0, 0, 10, 3), 20, 4),
            Rect::new(0, 0, 10, 3)
        );
    }

    #[test]
    fn snapshot_pending() {
        snapshots("pending", || {
            let s = pending();
            let now = s.at(2_500);
            let mut app = App::new(s.state, false);
            app.spinner = 2;
            (app, now)
        });
    }

    #[test]
    fn snapshot_mixed() {
        snapshots("mixed", mixed_app);
    }

    #[test]
    fn snapshot_failed_host_details() {
        snapshots("failed_details", || {
            let (mut app, now) = mixed_app();
            app.sort = SortKey::Status;
            let desk = id_of(&app, "desk");
            app.select(desk);
            app.state.apply(
                Event::Error(ErrorInfo::msg(
                    "ssh.command \"ssh\" can't be run: No such file or directory",
                )),
                now,
            );
            (app, now)
        });
    }

    #[test]
    fn snapshot_confirm() {
        snapshots("confirm", || {
            let (mut app, now) = mixed_app();
            app.select(id_of(&app, "al2-aarch64"));
            app.on_key(key('a'), now);
            assert!(matches!(app.mode, Mode::Confirm(_)));
            (app, now)
        });
    }

    #[test]
    fn snapshot_help() {
        snapshots("help", || {
            let (mut app, now) = mixed_app();
            app.on_key(key('?'), now);
            (app, now)
        });
    }

    #[test]
    fn snapshot_filter_without_matches() {
        snapshots("filter_no_match", || {
            let (mut app, now) = mixed_app();
            for c in "/zzz".chars() {
                app.on_key(key(c), now);
            }
            assert_eq!(app.mode, Mode::Filter);
            (app, now)
        });
    }

    #[test]
    fn snapshot_diff() {
        snapshots("diff", || {
            let mut s = mixed();
            s.diffed(
                3,
                1_200,
                Ok(DiffOutcome::Diff {
                    diff: fixture_diff(),
                }),
            );
            let now = s.at(12_000);
            let mut app = App::new(s.state, false);
            app.select(3);
            assert_eq!(
                app.on_key(key('d'), now),
                app::Command::None,
                "already diffed"
            );
            (app, now)
        });
    }

    #[test]
    fn snapshot_diff_desired_missing() {
        snapshots("diff_missing", || {
            let mut s = mixed();
            s.diffed(0, 800, Ok(DiffOutcome::DesiredMissing));
            let now = s.at(12_000);
            let mut app = App::new(s.state, false);
            app.mode = Mode::Diff { id: 0, scroll: 0 };
            (app, now)
        });
    }

    #[test]
    fn snapshot_no_hosts() {
        snapshots("no_hosts", || {
            let state = AppState {
                config_path: Some("/home/jj/.config/hostctl/config.toml".into()),
                ..AppState::default()
            };
            (App::new(state, false), Instant::now())
        });
    }
}
