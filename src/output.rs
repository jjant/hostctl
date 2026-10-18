//! Non-interactive output: the plain status table, the JSON reports, closure
//! diffs and the hosts listing. The JSON shapes below are the public,
//! versioned schema (absent values are `null`, never omitted).

use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::HashMap;
use std::fmt::{self, Write as _};
use std::time::{Duration, Instant};

use serde::Serialize;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::actions::{self, Activation, RunsOn};
use crate::closure::{Change, ChangeKind, ClosureDiff, SIZE_THRESHOLD, format_versions};
use crate::config::{Host, Target};
use crate::model::{
    AppState, Drift, ErrorInfo, FlakeState, HostState, ProbeFailure, Status, Summary,
};
use crate::remote::sh_quote;
use crate::util::{format_ago, format_elapsed, format_size, format_size_delta, sanitize_terminal};

/// How to render text output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    /// ANSI colors (status colors, dim secondary text).
    pub color: bool,
}

impl Style {
    /// Append `text`, wrapped in the SGR sequence `code` when colors are on.
    fn paint(self, out: &mut String, code: &str, text: &str) {
        if self.color && !code.is_empty() && !text.is_empty() {
            let _ = write!(out, "\x1b[{code}m{text}\x1b[0m");
        } else {
            out.push_str(text);
        }
    }
}

// SGR parameters for `Style::paint`; `PLAIN` is unstyled.
const PLAIN: &str = "";
const BOLD: &str = "1";
const DIM: &str = "2";
const RED: &str = "31";
const GREEN: &str = "32";
const YELLOW: &str = "33";
const MAGENTA: &str = "35";
const BLUE: &str = "34";
const CYAN: &str = "36";

/// The `hostctl status` table, for the terminal and for `--plain`:
///
/// ```text
/// github:jjant/nix-config @ 42ea902 (2h ago)
///
///   ✗ al2-aarch64         outdated       on 1a2b3c4 (4mo old)
///   ○ al2-x86_64          not activated  no home-manager profile
///   ✓ bigmem (this host)  up to date
///   – mac-m1              offline        reachable only from a desk you ssh into from the Mac
///
/// update: hostctl activate al2-aarch64 al2-x86_64
/// ```
///
/// - header: one line per flake in use: `<flake> @ <short rev> (<age of
///   lastModified>)`, plus ` · stale: <reason>` / ` · flake.lock out of date`,
///   or `<flake>: <error summary>` (` · resolving…` while it resolves);
/// - one row per host in state order: the status's icon, name (+ ` (this
///   host)` when local, ` (this host?)` when assumed local), label (pending rows show their
///   activity and elapsed time, e.g. `evaluating 3s`, or `interrupted`),
///   detail:
///   up-to-date/outdated/ahead → the drift (`on <rev7> (<age> old)`, `on
///   <branch> (<rev7>, <age> newer)`, `same rev, different build`, `staged:
///   switch/reboot pending`; nothing when the revision is unknown) and
///   badges (`⟳ reboot`, `specialisation X`);
///   not activated → the reason; failures → the error summary (offline:
///   only without a note); then the host's `note`, if any;
/// - columns aligned by display width; with `width`, every line is cut with
///   `…` to fit (in rows that is the detail column, unless name and status
///   alone don't fit); without it nothing is truncated;
/// - footer: `update: hostctl activate <names>` for hosts that need
///   activation (not ahead ones) and have an activation (`hostctl` as
///   [`AppState::command`] says, with the flags this run was given); then
///   `error: <summary>` per global error;
/// - with `style.color`: icon and label green ✓, blue ↑, yellow ✗ and ○,
///   red !, magenta ?, cyan pending, dim offline; ` (this host)` cyan;
///   revisions yellow; the rest of the detail and `update:` dim.
///
/// Untrusted text (names, notes, probe answers) is sanitized and kept on
/// one line.
pub fn status_table(state: &AppState, style: Style, width: Option<usize>, now_unix: i64) -> String {
    render_status(state, style, width, now_unix, Instant::now())
}

/// [`status_table`] at the monotonic time `now` (which pending timers are
/// measured against).
fn render_status(
    state: &AppState,
    style: Style,
    width: Option<usize>,
    now_unix: i64,
    now: Instant,
) -> String {
    let rows: Vec<Row<'_>> = state
        .hosts
        .iter()
        .map(|h| Row::new(h, now_unix, now, state.interrupted))
        .collect();
    let mut out = String::with_capacity(96 * (rows.len() + state.flakes.len() + 4));
    for flake in &state.flakes {
        flake_line(state, flake, now_unix).write(&mut out, style, width);
    }
    if !rows.is_empty() {
        section_break(&mut out);
        let name_w = rows.iter().map(|r| r.name.width()).max().unwrap_or(0);
        // Rows without detail don't widen the status column, so a ticking
        // "… evaluating 12s" doesn't shift every other row's detail.
        let label_w = rows
            .iter()
            .filter(|r| !r.detail.is_empty())
            .map(|r| r.label.width())
            .max()
            .unwrap_or(0);
        for row in &rows {
            let color = status_color(row.status);
            let mut line = Line::default();
            line.push("  ", PLAIN)
                .push(row.status.icon(), color)
                .push(" ", PLAIN)
                .name(&row.name, name_w)
                .push("  ", PLAIN);
            if row.detail.is_empty() {
                line.push(&row.label, color);
            } else {
                line.padded(&row.label, color, label_w).push("  ", PLAIN);
                for (text, code) in &row.detail.pieces {
                    line.push(text.as_str(), code);
                }
            }
            line.write(&mut out, style, width);
        }
    }
    let footer = footer(state, &rows);
    if !footer.is_empty() {
        section_break(&mut out);
        for line in &footer {
            line.write(&mut out, style, width);
        }
    }
    out
}

fn status_color(status: Status) -> &'static str {
    match status {
        Status::EvalFailed | Status::ProbeFailed => RED,
        Status::Unreachable => MAGENTA,
        Status::Outdated | Status::NotActivated => YELLOW,
        Status::Pending => CYAN,
        Status::Offline => DIM,
        Status::Ahead => BLUE,
        Status::UpToDate => GREEN,
    }
}

/// One host's cells in the status table.
struct Row<'a> {
    status: Status,
    name: Name<'a>,
    label: String,
    detail: Detail,
}

impl<'a> Row<'a> {
    fn new(h: &'a HostState, now_unix: i64, now: Instant, interrupted: bool) -> Row<'a> {
        let status = h.status();
        Row {
            status,
            name: Name::new(&h.host),
            label: status_label(h, status, now, interrupted),
            detail: detail(h, status, now_unix),
        }
    }
}

/// A host's name for tables, marked ` (this host)` when it is this machine
/// and ` (this host?)` when that is only assumed.
struct Name<'a> {
    name: Cow<'a, str>,
    this_host: Option<&'static str>,
}

impl<'a> Name<'a> {
    fn new(host: &'a Host) -> Name<'a> {
        Name {
            name: one_line(&host.name),
            this_host: match host.target {
                Target::Local { assumed: false } => Some(" (this host)"),
                Target::Local { assumed: true } => Some(" (this host?)"),
                Target::Ssh(_) => None,
            },
        }
    }

    fn width(&self) -> usize {
        self.name.width() + self.this_host.map_or(0, UnicodeWidthStr::width)
    }
}

/// The status's label; a pending host says what it waits for, and for how
/// long (or that the check was interrupted).
fn status_label(h: &HostState, status: Status, now: Instant, interrupted: bool) -> String {
    if status != Status::Pending {
        return status.label().into();
    }
    let interrupted = !h.busy()
        && (interrupted
            || matches!(h.probe.get(), Some(Err(e)) if e.kind == ProbeFailure::Interrupted));
    let activity = if interrupted {
        "interrupted"
    } else {
        h.activity().unwrap_or("waiting")
    };
    // The running slots in `HostState::activity`'s precedence.
    let running = [
        h.diff.running_for(now),
        h.eval.running_for(now),
        h.probe.running_for(now),
    ];
    match running.into_iter().flatten().next().and_then(ticking) {
        Some(elapsed) => format!("{activity} {elapsed}"),
        None => activity.into(),
    }
}

/// Whole seconds for a timer that is redrawn while it runs (`12s`,
/// `1m05s`); nothing under a second.
fn ticking(d: Duration) -> Option<String> {
    match d.as_secs() {
        0 => None,
        secs @ 1..60 => Some(format!("{secs}s")),
        _ => Some(format_elapsed(d)),
    }
}

/// The detail cell: ` · `-separated items, dim but for revisions (yellow).
#[derive(Default)]
struct Detail {
    pieces: Vec<(String, &'static str)>,
}

impl Detail {
    /// Append a dim item; empty items are skipped.
    fn item(&mut self, text: impl fmt::Display) {
        self.styled([(text.to_string(), DIM)]);
    }

    /// Append an item of styled pieces; empty pieces are skipped.
    fn styled(&mut self, pieces: impl IntoIterator<Item = (String, &'static str)>) {
        let start = self.pieces.len();
        if start > 0 {
            self.pieces.push((" · ".into(), DIM));
        }
        let separator = self.pieces.len();
        self.pieces
            .extend(pieces.into_iter().filter(|(text, _)| !text.is_empty()));
        if self.pieces.len() == separator {
            self.pieces.truncate(start);
        }
    }

    fn is_empty(&self) -> bool {
        self.pieces.is_empty()
    }
}

/// The detail cell (see [`status_table`]).
fn detail(h: &HostState, status: Status, now_unix: i64) -> Detail {
    let mut out = Detail::default();
    let report = h.report();
    match status {
        Status::UpToDate | Status::Outdated | Status::Ahead => {
            if let Some(drift) = h.drift() {
                out.styled(drift_text(&drift, now_unix));
            }
            if let Some(r) = report {
                if r.reboot_required {
                    out.item("⟳ reboot");
                }
                if let Some(name) = &r.specialisation {
                    out.item(format_args!("specialisation {}", one_line(name)));
                }
            }
        }
        Status::NotActivated => {
            let reason = report
                .and_then(|r| r.none_reason.as_deref())
                .filter(|reason| !reason.trim().is_empty())
                .unwrap_or("no active generation");
            out.item(one_line(reason));
        }
        Status::EvalFailed => {
            if let Some(Err(error)) = h.eval.get() {
                out.item(one_line(&error.summary));
            }
        }
        Status::ProbeFailed | Status::Unreachable => {
            if let Some(Err(failure)) = h.probe.get() {
                out.item(one_line(&failure.error.summary));
            }
        }
        // Being unreachable is expected; a note explains it better than ssh.
        Status::Offline => {
            if let (None, Some(Err(failure))) = (&h.host.note, h.probe.get()) {
                out.item(one_line(&failure.error.summary));
            }
        }
        Status::Pending => {}
    }
    if let Some(note) = &h.host.note {
        out.item(one_line(note));
    }
    out
}

/// What the detail cell says about `drift`, the revision in yellow: nothing
/// when the revision a host runs is unknown.
fn drift_text(drift: &Drift, now_unix: i64) -> Vec<(String, &'static str)> {
    let text = |text: &str| (text.to_string(), DIM);
    match drift {
        Drift::Staged => vec![text("staged: switch/reboot pending")],
        Drift::SameRevision => vec![text("same rev, different build")],
        Drift::Unknown => vec![],
        Drift::Revision {
            rev,
            last_modified,
            newer_by,
            branches,
            ..
        } => {
            let rev = (short_rev(rev), YELLOW);
            let age = match (newer_by, last_modified) {
                (Some(by), _) => Some(format!("{} newer", age(0, *by))),
                (None, Some(at)) => Some(format!("{} old", age(*at, now_unix))),
                (None, None) => None,
            };
            match (branches.first(), age) {
                (Some(branch), Some(age)) => vec![
                    text(&format!("on {} (", one_line(branch))),
                    rev,
                    text(&format!(", {age})")),
                ],
                (Some(branch), None) => {
                    vec![text(&format!("on {} (", one_line(branch))), rev, text(")")]
                }
                (None, Some(age)) => vec![text("on "), rev, text(&format!(" ({age})"))],
                (None, None) => vec![text("on "), rev],
            }
        }
    }
}

/// A revision cut to 7 characters, keeping a `-dirty` suffix.
fn short_rev(rev: &str) -> String {
    let (base, suffix) = match rev.strip_suffix("-dirty") {
        Some(base) => (base, "-dirty"),
        None => (rev, ""),
    };
    let end = base.char_indices().nth(7).map_or(base.len(), |(i, _)| i);
    format!("{}{suffix}", one_line(&base[..end]))
}

/// How old `then` is at `now` (unix seconds): `4mo`, `3h`, `<1m`.
fn age(then: i64, now: i64) -> String {
    let mut ago = format_ago(then, now);
    match ago.strip_suffix(" ago").map(str::len) {
        Some(len) => {
            ago.truncate(len);
            ago
        }
        None => "<1m".into(),
    }
}

/// `<flake> @ <rev> (<age>)` plus warnings, or `<flake>: <error>`.
fn flake_line<'a>(state: &AppState, flake: &'a FlakeState, now_unix: i64) -> Line<'a> {
    let mut line = Line::default();
    line.push(one_line(&flake.url), BOLD);
    match flake.info.get() {
        Some(Ok(info)) => {
            if info.rev.is_some() || info.dirty_rev.is_some() {
                line.push(" @ ", PLAIN)
                    .push(one_line(&info.short_rev()).into_owned(), YELLOW);
            }
            if let Some(at) = info.last_modified {
                line.push(format!(" ({})", format_ago(at, now_unix)), DIM);
            }
            if let Some(reason) = &info.stale {
                line.push(" · ", PLAIN)
                    .push(format!("stale: {}", one_line(reason)), YELLOW);
            }
            if info.lock_outdated {
                line.push(" · ", PLAIN)
                    .push("flake.lock out of date", YELLOW);
            }
        }
        Some(Err(error)) => {
            line.push(": ", PLAIN).push(one_line(&error.summary), RED);
        }
        // Interrupting leaves resolutions "running" forever.
        None if flake.info.is_running() && !state.interrupted => {
            line.push(" · resolving…", DIM);
        }
        None => {}
    }
    line
}

/// The hosts to update; global errors.
fn footer<'a>(state: &'a AppState, rows: &[Row<'_>]) -> Vec<Line<'a>> {
    let mut lines = Vec::new();

    let mut names = String::new();
    let mut option_like = false;
    for (h, row) in state.hosts.iter().zip(rows) {
        // Not an ahead host: it would go back.
        if row.status.needs_activation() && activation(state, h).is_some() {
            let name = one_line(&h.host.name);
            option_like |= name.starts_with('-');
            names.push(' ');
            names.push_str(&sh_quote(&name));
        }
    }
    // A name that looks like an option (`-x`) is still a name.
    if option_like {
        names.insert_str(0, " --");
    }
    if !names.is_empty() {
        let mut line = Line::default();
        line.push("update:", DIM)
            .push(format!(" {} activate", one_line(&state.command)), PLAIN)
            .push(names, PLAIN);
        lines.push(line);
    }

    for error in &state.errors {
        let mut line = Line::default();
        line.push("error:", RED)
            .push(" ", PLAIN)
            .push(one_line(&error.summary), PLAIN);
        lines.push(line);
    }
    lines
}

/// The activation of `h`, once its desired generation and flake are known
/// and [`actions::activation`] offers one.
fn activation(state: &AppState, h: &HostState) -> Option<Activation> {
    let desired = h.desired()?;
    let flake = state.flake_info(&h.host.flake)?;
    actions::activation(&h.host, flake, &desired.path).ok()
}

/// `hostctl status --json`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusReport {
    /// Schema version (1).
    pub version: u32,
    pub summary: Summary,
    pub flakes: Vec<FlakeReport>,
    pub hosts: Vec<HostReport>,
    pub errors: Vec<ErrorInfo>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FlakeReport {
    /// As configured.
    pub flake: String,
    pub locked: Option<String>,
    /// The revision (`<rev>-dirty` for a dirty tree).
    pub rev: Option<String>,
    pub last_modified: Option<i64>,
    pub stale: Option<String>,
    pub lock_outdated: bool,
    pub error: Option<ErrorInfo>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostReport {
    pub name: String,
    pub kind: String,
    /// `"local"`, `"assumed"` or `null` (remote).
    pub local: Option<String>,
    pub destination: Option<String>,
    pub flake: String,
    pub attr: String,
    pub status: Status,
    /// `{"type": "staged" | "same-revision" | "unknown"}` or `{"type":
    /// "revision", "rev", "lastModified", "fromGeneration"}`.
    #[serde(serialize_with = "drift_json")]
    pub drift: Option<Drift>,
    /// The deployed generation compared with `desired` (the parent toplevel
    /// when a specialisation is active).
    pub current: Option<String>,
    pub desired: Option<String>,
    pub cached: Option<bool>,
    pub generation: Option<u64>,
    pub activated: Option<i64>,
    pub platform: Option<String>,
    pub revision: Option<String>,
    pub specialisation: Option<String>,
    pub booted: Option<String>,
    pub reboot_required: bool,
    pub note: Option<String>,
    /// The error that determines the status (eval error, else probe error).
    pub error: Option<ErrorInfo>,
    pub activate: Option<ActivateReport>,
    pub timings: Timings,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivateReport {
    pub command: String,
    /// `"here"` or `"remote"`.
    pub runs_on: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Timings {
    pub probe_ms: Option<u64>,
    pub eval_ms: Option<u64>,
}

/// The JSON report of `state`. Free text is sanitized like the table's
/// (`serde_json` only escapes C0 controls).
pub fn status_report(state: &AppState) -> StatusReport {
    StatusReport {
        version: 1,
        summary: state.summary(),
        flakes: state.flakes.iter().map(flake_report).collect(),
        hosts: state.hosts.iter().map(|h| host_report(state, h)).collect(),
        errors: state.errors.clone(),
    }
}

fn flake_report(flake: &FlakeState) -> FlakeReport {
    let (info, error) = match flake.info.get() {
        Some(Ok(info)) => (Some(info), None),
        Some(Err(error)) => (None, Some(error.clone())),
        None => (None, None),
    };
    FlakeReport {
        flake: clean(&flake.url),
        locked: info.map(|i| clean(&i.locked)),
        rev: info
            .and_then(|i| i.rev.as_deref().or(i.dirty_rev.as_deref()))
            .map(clean),
        last_modified: info.and_then(|i| i.last_modified),
        stale: info.and_then(|i| i.stale.as_deref()).map(clean),
        lock_outdated: info.is_some_and(|i| i.lock_outdated),
        error,
    }
}

fn host_report(state: &AppState, h: &HostState) -> HostReport {
    let host = &h.host;
    let report = h.report();
    let desired = h.desired();
    HostReport {
        name: clean(&host.name),
        kind: host.kind.as_str().into(),
        local: match host.target {
            Target::Local { assumed: false } => Some("local".into()),
            Target::Local { assumed: true } => Some("assumed".into()),
            Target::Ssh(_) => None,
        },
        destination: host.ssh().map(|t| clean(&t.destination)),
        flake: clean(&host.flake),
        attr: clean(&host.attr),
        status: h.status(),
        drift: h.drift().map(clean_drift),
        current: h.current().map(|p| p.as_str().into()),
        desired: desired.map(|d| d.path.as_str().into()),
        cached: desired.map(|d| d.cached),
        generation: report.and_then(|r| r.generation),
        activated: report.and_then(|r| r.activated),
        platform: report.and_then(|r| r.platform.as_deref()).map(clean),
        revision: report.and_then(|r| r.revision.as_deref()).map(clean),
        specialisation: report.and_then(|r| r.specialisation.as_deref()).map(clean),
        booted: report
            .and_then(|r| r.booted.as_ref())
            .map(|p| p.as_str().into()),
        reboot_required: report.is_some_and(|r| r.reboot_required),
        note: host.note.as_deref().map(clean),
        error: match (h.eval.get(), h.probe.get()) {
            (Some(Err(error)), _) => Some(error.clone()),
            (_, Some(Err(failure))) => Some(failure.error.clone()),
            _ => None,
        },
        activate: activation(state, h).map(|a| ActivateReport {
            command: clean(&a.command),
            runs_on: match a.runs_on {
                RunsOn::Here => "here",
                RunsOn::Remote => "remote",
            }
            .into(),
        }),
        timings: Timings {
            probe_ms: h.probe.took().map(millis),
            eval_ms: h.eval.took().map(millis),
        },
    }
}

/// [`Drift`] with camelCase fields like the rest of the report
/// (`model::Drift` itself derives `snake_case` ones).
fn drift_json<S: serde::Serializer>(
    drift: &Option<Drift>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    #[derive(Serialize)]
    #[serde(
        tag = "type",
        rename_all = "kebab-case",
        rename_all_fields = "camelCase"
    )]
    enum Json<'a> {
        Staged,
        SameRevision,
        Revision {
            rev: &'a str,
            last_modified: Option<i64>,
            from_generation: bool,
            newer: bool,
            branches: &'a [String],
        },
        Unknown,
    }
    let json = drift.as_ref().map(|drift| match drift {
        Drift::Staged => Json::Staged,
        Drift::SameRevision => Json::SameRevision,
        Drift::Revision {
            rev,
            last_modified,
            from_generation,
            newer_by,
            branches,
        } => Json::Revision {
            rev,
            last_modified: *last_modified,
            from_generation: *from_generation,
            newer: newer_by.is_some(),
            branches,
        },
        Drift::Unknown => Json::Unknown,
    });
    json.serialize(serializer)
}

/// `drift` with its (probe-reported) revision and (git-reported) branches
/// sanitized.
fn clean_drift(drift: Drift) -> Drift {
    match drift {
        Drift::Revision {
            rev,
            last_modified,
            from_generation,
            newer_by,
            branches,
        } => Drift::Revision {
            rev: clean(&rev),
            last_modified,
            from_generation,
            newer_by,
            branches: branches.iter().map(|b| clean(b)).collect(),
        },
        other => other,
    }
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// `hostctl diff <host>`:
///
/// ```text
/// bigmem: 702 → 705 paths · 3.1 GiB → 3.2 GiB (+24.3 MiB)
///   firefox       131.0 → 132.0    +2.1 MiB  upgraded
///   home-manager                  +10.8 KiB  resized
///   old-tool      0.3 → ∅          -1.2 MiB  removed
///   zenoh         ∅ → 1.10.0     +331.9 MiB  added
/// ```
///
/// One aligned line per change: name, `removed → added` (blank for
/// resized, whose versions didn't change; [`crate::closure::format_versions`]),
/// size delta (right-aligned, blank below the threshold), kind. A name or
/// version cell wider than 40 columns doesn't widen its column (it pushes
/// the rest of its own line instead). Colors: added/upgraded green,
/// removed/downgraded red, growth red / shrink green. An empty diff prints
/// `<host>: no package changes (N paths)`.
pub fn diff_text(host: &str, diff: &ClosureDiff, style: Style) -> String {
    let mut out = String::with_capacity(64 * (diff.changes.len() + 1));
    let mut header = Line::default();
    header.push(one_line(host), BOLD);
    if diff.is_empty() {
        let paths = if diff.paths_before == diff.paths_after {
            format!("{} paths", diff.paths_after)
        } else {
            format!("{} → {} paths", diff.paths_before, diff.paths_after)
        };
        header.push(format!(": no package changes ({paths})"), PLAIN);
        header.write(&mut out, style, None);
        return out;
    }
    let delta = diff.size_delta();
    header
        .push(
            format!(
                ": {} → {} paths · {} → {} ",
                diff.paths_before,
                diff.paths_after,
                format_size(diff.size_before),
                format_size(diff.size_after)
            ),
            PLAIN,
        )
        .push(format!("({})", format_size_delta(delta)), size_color(delta));
    header.write(&mut out, style, None);

    let rows: Vec<DiffRow<'_>> = diff.changes.iter().map(DiffRow::new).collect();
    let name_w = aligned_width(rows.iter().map(|r| r.name.width()));
    let versions_w = aligned_width(rows.iter().map(|r| r.versions.width()));
    let has_versions = rows.iter().any(|r| !r.versions.is_empty());
    let delta_w = rows.iter().map(|r| r.delta.width()).max().unwrap_or(0);
    for row in &rows {
        let mut line = Line::default();
        line.push("  ", PLAIN)
            .padded(&*row.name, PLAIN, name_w)
            .push("  ", PLAIN);
        if has_versions {
            line.padded(&row.versions, PLAIN, versions_w)
                .push("  ", PLAIN);
        }
        if delta_w > 0 {
            line.push(spaces(delta_w.saturating_sub(row.delta.width())), PLAIN)
                .push(&row.delta, size_color(row.size_delta))
                .push("  ", PLAIN);
        }
        let (label, color) = change_label(row.kind);
        line.push(label, color);
        line.write(&mut out, style, None);
    }
    out
}

/// Cells wider than this don't widen their column: one long name pushes
/// the rest of its own line instead of spreading out every line.
const MAX_ALIGNED: usize = 40;

/// The width of a column of cells `widths` wide (see [`MAX_ALIGNED`]).
fn aligned_width(widths: impl Iterator<Item = usize>) -> usize {
    widths.filter(|&w| w <= MAX_ALIGNED).max().unwrap_or(0)
}

/// One change's cells in a diff.
struct DiffRow<'a> {
    name: Cow<'a, str>,
    versions: String,
    /// Blank below [`SIZE_THRESHOLD`].
    delta: String,
    size_delta: i64,
    kind: ChangeKind,
}

impl<'a> DiffRow<'a> {
    fn new(change: &'a Change) -> DiffRow<'a> {
        let kind = change.kind();
        let versions = if kind == ChangeKind::Resized {
            String::new()
        } else {
            let versions = format!(
                "{} → {}",
                format_versions(&change.removed),
                format_versions(&change.added)
            );
            one_line(&versions).into_owned()
        };
        let size_delta = change.size_delta();
        DiffRow {
            name: one_line(&change.name),
            versions,
            delta: if size_delta.unsigned_abs() < SIZE_THRESHOLD {
                String::new()
            } else {
                format_size_delta(size_delta)
            },
            size_delta,
            kind,
        }
    }
}

/// Growth red, shrink green; changes below the noise threshold unstyled.
fn size_color(delta: i64) -> &'static str {
    if delta.unsigned_abs() < SIZE_THRESHOLD {
        PLAIN
    } else if delta > 0 {
        RED
    } else {
        GREEN
    }
}

fn change_label(kind: ChangeKind) -> (&'static str, &'static str) {
    match kind {
        ChangeKind::Added => ("added", GREEN),
        ChangeKind::Removed => ("removed", RED),
        ChangeKind::Upgraded => ("upgraded", GREEN),
        ChangeKind::Downgraded => ("downgraded", RED),
        ChangeKind::Changed => ("changed", PLAIN),
        ChangeKind::Resized => ("resized", DIM),
    }
}

/// `hostctl diff --json`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffReport<'a> {
    /// Schema version (1).
    pub version: u32,
    pub host: &'a str,
    pub diff: &'a ClosureDiff,
}

/// `hostctl hosts`: one aligned row per host — name, kind,
/// how it's reached (`local`, destination, ` (probe: <cmd>)` for
/// forced-command routes), where it's defined (`discovered`, `inventory`,
/// `config`, comma-joined), and the attr, as `<flake>#<attr>` when the
/// flake differs from the most common one. `warnings` are printed after
/// the table.
pub fn hosts_table(hosts: &[Host], warnings: &[String], style: Style) -> String {
    let common = most_common_flake(hosts);
    let rows: Vec<HostRow<'_>> = hosts.iter().map(|h| HostRow::new(h, common)).collect();
    let width = |cell: fn(&HostRow<'_>) -> usize| rows.iter().map(cell).max().unwrap_or(0);
    let name_w = width(|r| r.name.width());
    let kind_w = width(|r| r.kind.width());
    let via_w = width(|r| r.via.width() + r.probe.width());
    let sources_w = width(|r| r.sources.width());

    let mut out = String::with_capacity(128 * (rows.len() + warnings.len()));
    for row in &rows {
        let mut line = Line::default();
        line.padded(&*row.name, PLAIN, name_w)
            .push("  ", PLAIN)
            .padded(row.kind, PLAIN, kind_w)
            .push("  ", PLAIN);
        let via_start = line.width;
        line.push(&*row.via, PLAIN)
            .push(&row.probe, DIM)
            .pad_to(via_start + via_w)
            .push("  ", PLAIN)
            .padded(&row.sources, DIM, sources_w)
            .push("  ", PLAIN);
        if let Some(flake) = &row.flake {
            line.push(&**flake, PLAIN).push("#", DIM);
        }
        line.push(&*row.attr, DIM);
        line.write(&mut out, style, None);
    }
    if !warnings.is_empty() {
        section_break(&mut out);
        for warning in warnings {
            let mut line = Line::default();
            line.push("warning:", YELLOW)
                .push(" ", PLAIN)
                .push(one_line(warning), PLAIN);
            line.write(&mut out, style, None);
        }
    }
    out
}

/// One host's cells in the hosts listing.
struct HostRow<'a> {
    name: Cow<'a, str>,
    kind: &'static str,
    via: Cow<'a, str>,
    /// ` (probe: <command>)` for forced-command routes.
    probe: String,
    sources: String,
    /// Only when it isn't the most common flake (shown before the attr).
    flake: Option<Cow<'a, str>>,
    attr: Cow<'a, str>,
}

impl<'a> HostRow<'a> {
    fn new(host: &'a Host, common: Option<&str>) -> HostRow<'a> {
        let probe = match host.ssh().and_then(|t| t.forced_command.as_deref()) {
            Some(command) => format!(" (probe: {})", one_line(command)),
            None => String::new(),
        };
        let mut sources = String::new();
        for (defined, source) in [
            (host.sources.discovered, "discovered"),
            (host.sources.inventory, "inventory"),
            (host.sources.configured, "config"),
        ] {
            if defined {
                if !sources.is_empty() {
                    sources.push_str(", ");
                }
                sources.push_str(source);
            }
        }
        HostRow {
            name: one_line(&host.name),
            kind: host.kind.as_str(),
            via: one_line(host.reached_via()),
            probe,
            sources,
            flake: (Some(host.flake.as_str()) != common).then(|| one_line(&host.flake)),
            attr: one_line(&host.attr),
        }
    }
}

/// The flake most hosts use (the first one on ties).
fn most_common_flake(hosts: &[Host]) -> Option<&str> {
    let mut counts: HashMap<&str, (usize, Reverse<usize>)> = HashMap::new();
    for (i, host) in hosts.iter().enumerate() {
        counts.entry(&host.flake).or_insert((0, Reverse(i))).0 += 1;
    }
    counts
        .into_iter()
        .max_by_key(|&(_, rank)| rank)
        .map(|(flake, _)| flake)
}

/// A line of output as styled segments, so it can be cut to a width
/// without splitting escape sequences.
#[derive(Default)]
struct Line<'a> {
    segments: Vec<(Cow<'a, str>, &'static str)>,
    /// Display width of all segments.
    width: usize,
}

impl<'a> Line<'a> {
    fn push(&mut self, text: impl Into<Cow<'a, str>>, code: &'static str) -> &mut Self {
        let text = text.into();
        self.width += text.width();
        self.segments.push((text, code));
        self
    }

    /// `text`, then spaces up to `cols` columns.
    fn padded(
        &mut self,
        text: impl Into<Cow<'a, str>>,
        code: &'static str,
        cols: usize,
    ) -> &mut Self {
        let start = self.width;
        self.push(text, code).pad_to(start + cols)
    }

    /// A host's name, its ` (this host)` mark in cyan, then spaces up to
    /// `cols` columns.
    fn name(&mut self, name: &'a Name<'_>, cols: usize) -> &mut Self {
        let start = self.width;
        self.push(&*name.name, PLAIN);
        if let Some(mark) = name.this_host {
            self.push(mark, CYAN);
        }
        self.pad_to(start + cols)
    }

    /// Spaces up to column `col` (none if the line is already that wide).
    fn pad_to(&mut self, col: usize) -> &mut Self {
        if col > self.width {
            self.push(spaces(col - self.width), PLAIN);
        }
        self
    }

    /// Append the line and a newline to `out`, without trailing blanks, cut
    /// with `…` to `max` columns.
    fn write(&self, out: &mut String, style: Style, max: Option<usize>) {
        let end = self
            .segments
            .iter()
            .rposition(|(text, _)| !text.trim_end().is_empty())
            .map_or(0, |last| last + 1);
        let segments = self.segments[..end]
            .iter()
            .enumerate()
            .map(|(i, (text, code))| {
                let text: &str = if i + 1 == end { text.trim_end() } else { text };
                (text, text.width(), *code)
            });
        let total: usize = segments.clone().map(|(_, width, _)| width).sum();
        // Columns left before the `…` once the line has to be cut.
        let mut left = match max {
            Some(0) => {
                out.push('\n');
                return;
            }
            Some(max) if total > max => Some(max - 1),
            _ => None,
        };
        for (text, width, code) in segments {
            if let Some(cols) = left {
                if width > cols {
                    style.paint(out, code, &format!("{}…", prefix(text, cols)));
                    break;
                }
                left = Some(cols - width);
            }
            style.paint(out, code, text);
        }
        out.push('\n');
    }
}

/// The longest prefix of `s` at most `cols` columns wide.
fn prefix(s: &str, cols: usize) -> &str {
    let mut width = 0;
    for (i, c) in s.char_indices() {
        width += c.width().unwrap_or(0);
        if width > cols {
            return &s[..i];
        }
    }
    s
}

/// `n` spaces (borrowed for the usual widths).
fn spaces(n: usize) -> Cow<'static, str> {
    const SPACES: &str = "                                                                ";
    match SPACES.get(..n) {
        Some(spaces) => Cow::Borrowed(spaces),
        None => Cow::Owned(" ".repeat(n)),
    }
}

/// A blank line between sections (none before the first).
fn section_break(out: &mut String) {
    if !out.is_empty() {
        out.push('\n');
    }
}

/// Untrusted text for a table: sanitized, trimmed, and on one line (line
/// breaks and tabs become spaces).
fn one_line(s: &str) -> Cow<'_, str> {
    let s = sanitize_terminal(s.trim());
    if !s.contains(['\n', '\t']) {
        return s;
    }
    let mut flat = String::with_capacity(s.len());
    for part in s
        .split(['\n', '\t'])
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        if !flat.is_empty() {
            flat.push(' ');
        }
        flat.push_str(part);
    }
    Cow::Owned(flat)
}

/// Untrusted text for the JSON report.
fn clean(s: &str) -> String {
    sanitize_terminal(s).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::DeployedRev;
    use crate::config::{ActivateOn, Kind, Sources, SshTarget};
    use crate::model::{Desired, EvalResult, Event, HostId, ProbeError, ProbeResult};
    use crate::nix::FlakeInfo;
    use crate::probe::ProbeReport;
    use crate::storepath::StorePath;

    /// Every status, most urgent first.
    const STATUSES: [Status; 9] = [
        Status::EvalFailed,
        Status::ProbeFailed,
        Status::Unreachable,
        Status::Outdated,
        Status::NotActivated,
        Status::Pending,
        Status::Offline,
        Status::Ahead,
        Status::UpToDate,
    ];

    const NO_COLOR: Style = Style { color: false };
    const COLOR: Style = Style { color: true };

    const MIN: i64 = 60;
    const HOUR: i64 = 60 * MIN;
    const DAY: i64 = 24 * HOUR;
    const MONTH: i64 = 30 * DAY;
    /// "Now" in every test: 2h46m after the github fixture's `lastModified`.
    const NOW: i64 = 1_792_250_000;

    // tests/fixtures/metadata-github.json
    const GITHUB: &str = "github:jjant/nix-config";
    const REV: &str = "42ea902131b93c04d098d3bcd02f29305b4bad7c";
    const OLD_REV: &str = "1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b";

    // tests/fixtures/closure-hm-*.root: real home-manager generations.
    const HM_1: &str = "/nix/store/494amg2bizplp1s8485gr6b2k6pl4mxh-home-manager-generation";
    const HM_5: &str = "/nix/store/1nvwx1ddkfhxws6vwff5rnf5hrhivqmp-home-manager-generation";
    const HM_8: &str = "/nix/store/afvwn2ldnj8z50fisjnj40xpdq7602y4-home-manager-generation";
    const HM_9: &str = "/nix/store/ykn4vz9vpqccp1n36p50irs3dwrjfkk6-home-manager-generation";

    fn sp(path: &str) -> StorePath {
        StorePath::parse(path).unwrap()
    }

    /// A store path whose hash is `c` repeated.
    fn nix_path(c: char, name: &str) -> String {
        format!("/nix/store/{}-{name}", c.to_string().repeat(32))
    }

    /// tests/fixtures/metadata-github.json, as `nix::parse_metadata` reads it.
    fn github() -> FlakeInfo {
        FlakeInfo {
            flake: GITHUB.into(),
            locked: format!(
                "{GITHUB}/{REV}?narHash=sha256-rTwky4CPWade4oeZ81h5PcDt8%2Be8kXznGgaiDqYFhFk%3D"
            ),
            kind: Some("github".into()),
            locked_url: None,
            rev: Some(REV.into()),
            dirty_rev: None,
            last_modified: Some(1_792_240_011),
            rev_count: None,
            fingerprint: Some(
                "773b1c0a8f4cbe4ff5c09fd6c963dc4df2680450367f592cf85c58dd280bfc74".into(),
            ),
            nar_hash: Some("sha256-rTwky4CPWade4oeZ81h5PcDt8+e8kXznGgaiDqYFhFk=".into()),
            description: Some("jjant's nix config".into()),
            stale: None,
            lock_outdated: false,
        }
    }

    /// tests/fixtures/metadata-git-dirty.json.
    fn dirty_git() -> FlakeInfo {
        FlakeInfo {
            flake: "git+file:///tmp/hostctl-ifd".into(),
            locked: "git+file:///tmp/hostctl-ifd".into(),
            kind: Some("git".into()),
            locked_url: Some("file:///tmp/hostctl-ifd".into()),
            rev: None,
            dirty_rev: Some("a8f62b6f2c1170af6c40178c9080c87fc51fe5de-dirty".into()),
            last_modified: Some(1_792_240_480),
            rev_count: None,
            fingerprint: Some(
                "fe131a44ab7cc53bb94635dfa53e16cc208cb97eeb302f0d87aa6ebeb822ede3".into(),
            ),
            nar_hash: Some("sha256-BJnVjXIcwDyH0iDJ5g8NI9EZ6E6HeKY9+fb9ic1WNPY=".into()),
            description: None,
            stale: None,
            lock_outdated: false,
        }
    }

    fn ssh(destination: &str) -> Target {
        let d = crate::remote::parse_destination(destination).unwrap();
        Target::Ssh(SshTarget {
            destination: destination.into(),
            user: d.user,
            host: d.host,
            port: d.port,
            options: vec![],
            forced_command: None,
        })
    }

    /// A discovered host of `kind`, reached over ssh by its name, with the
    /// kind's default attribute and activation.
    fn host(name: &str, kind: Kind) -> Host {
        Host {
            name: name.into(),
            kind,
            flake: GITHUB.into(),
            attr: kind
                .default_attr(name)
                .unwrap_or_else(|| format!("packages.x86_64-linux.{name}")),
            // Not parsed: odd names must reach the renderers.
            target: Target::Ssh(SshTarget {
                destination: name.into(),
                user: None,
                host: name.into(),
                port: None,
                options: vec![],
                forced_command: None,
            }),
            profile: None,
            activate: kind.default_activate().map(str::to_string),
            activate_on: ActivateOn::Target,
            optional: false,
            note: None,
            sources: Sources {
                discovered: true,
                ..Sources::default()
            },
        }
    }

    fn hm(name: &str) -> Host {
        host(name, Kind::HomeManager)
    }

    fn local(mut host: Host, assumed: bool) -> Host {
        host.target = Target::Local { assumed };
        host
    }

    /// The Mac behind a desk's reverse tunnel (DESIGN.md "Configuration").
    fn mac() -> Host {
        let mut mac = host("mac-m1", Kind::Darwin);
        mac.target = ssh("ssh://devuser1@localhost:2022");
        if let Target::Ssh(t) = &mut mac.target {
            t.forced_command = Some("current-system".into());
        }
        mac.optional = true;
        mac.note = Some("reachable only from a desk you ssh into from the Mac".into());
        mac.sources.configured = true;
        mac
    }

    /// Drives an [`AppState`] through [`AppState::apply`] like the engine:
    /// probes take 250 ms, evaluations 3 s, a whole check 3.4 s; tables are
    /// rendered 20 s after the start.
    struct Sim {
        state: AppState,
        t0: Instant,
    }

    impl Sim {
        fn new(hosts: Vec<Host>) -> Sim {
            Sim {
                state: AppState::new(hosts),
                t0: Instant::now(),
            }
        }

        fn apply(&mut self, at_ms: u64, event: Event) {
            let at = self.t0 + Duration::from_millis(at_ms);
            self.state.apply(event, at);
        }

        fn started(&mut self) {
            self.apply(0, Event::RunStarted);
        }

        fn finished(&mut self) {
            self.apply(0, Event::RunStarted);
            self.apply(3_400, Event::RunFinished);
        }

        fn resolved(&mut self, info: FlakeInfo) {
            let url = info.flake.clone();
            self.apply(0, Event::FlakeStarted { url: url.clone() });
            self.apply(
                300,
                Event::FlakeFinished {
                    url,
                    result: Ok(info),
                },
            );
        }

        fn unresolved(&mut self, url: &str, error: ErrorInfo) {
            self.apply(0, Event::FlakeStarted { url: url.into() });
            self.apply(
                1_300,
                Event::FlakeFinished {
                    url: url.into(),
                    result: Err(error),
                },
            );
        }

        fn probed(&mut self, id: HostId, result: ProbeResult) {
            self.probed_deployed(id, result, None);
        }

        fn probed_deployed(
            &mut self,
            id: HostId,
            result: ProbeResult,
            deployed: Option<DeployedRev>,
        ) {
            self.apply(0, Event::ProbeStarted(id));
            self.apply(
                250,
                Event::ProbeFinished {
                    id,
                    result,
                    deployed,
                },
            );
        }

        fn evaluated(&mut self, id: HostId, result: EvalResult) {
            self.apply(300, Event::EvalStarted(id));
            self.apply(3_300, Event::EvalFinished { id, result });
        }

        /// A cache hit: no `EvalStarted`, no duration.
        fn cached(&mut self, id: HostId, path: &str) {
            let result = Ok(Desired {
                path: sp(path),
                rev: Some(REV.into()),
                last_modified: github().last_modified,
                cached: true,
            });
            self.apply(300, Event::EvalFinished { id, result });
        }

        fn table(&self, style: Style, width: Option<usize>) -> String {
            let now = self.t0 + Duration::from_secs(20);
            let table = render_status(&self.state, style, width, NOW, now);
            check(&strip_sgr(&table), width);
            table
        }

        fn json(&self) -> serde_json::Value {
            serde_json::to_value(status_report(&self.state)).unwrap()
        }
    }

    /// Generation `generation` of `current`, activated `ago` seconds
    /// before [`NOW`], on x86_64-linux.
    fn active(current: &str, generation: u64, ago: i64) -> ProbeResult {
        Ok(ProbeReport {
            current: Some(sp(current)),
            platform: Some("x86_64-linux".into()),
            profile: Some(sp(current)),
            generation: Some(generation),
            activated: Some(NOW - ago),
            ..ProbeReport::default()
        })
    }

    fn with(result: ProbeResult, edit: impl FnOnce(&mut ProbeReport)) -> ProbeResult {
        result.map(|mut report| {
            edit(&mut report);
            report
        })
    }

    fn never_activated(reason: &str) -> ProbeResult {
        Ok(ProbeReport {
            none_reason: Some(reason.into()),
            platform: Some("x86_64-linux".into()),
            ..ProbeReport::default()
        })
    }

    fn probe_error(kind: ProbeFailure, summary: &str) -> ProbeResult {
        Err(ProbeError {
            kind,
            error: ErrorInfo::new(summary, format!("ssh -T … sh -s\n{summary}")),
        })
    }

    fn wants(path: &str) -> EvalResult {
        Ok(Desired {
            path: sp(path),
            rev: Some(REV.into()),
            last_modified: github().last_modified,
            cached: false,
        })
    }

    fn cached_rev(rev: &str, last_modified: i64) -> Option<DeployedRev> {
        Some(DeployedRev {
            flake: GITHUB.into(),
            rev: Some(rev.into()),
            last_modified: Some(last_modified),
        })
    }

    /// `s` without its SGR sequences (`ESC [ … m`); panics on any other
    /// escape sequence.
    fn strip_sgr(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut rest = s;
        while let Some(start) = rest.find('\x1b') {
            out.push_str(&rest[..start]);
            let sequence = &rest[start..];
            let end = sequence.find('m').expect("unterminated escape sequence");
            let params = &sequence[..end];
            assert!(
                params.starts_with("\x1b[") && params[2..].bytes().all(|b| b.is_ascii_digit()),
                "not an SGR sequence: {sequence:?}"
            );
            rest = &sequence[end + 1..];
        }
        out.push_str(rest);
        out
    }

    /// Invariants of every text output (without colors): newline-terminated
    /// lines without trailing blanks or control characters, within `width`.
    fn check(text: &str, width: Option<usize>) {
        assert!(text.is_empty() || text.ends_with('\n'), "{text:?}");
        for line in text.split_terminator('\n') {
            assert_eq!(line, line.trim_end(), "trailing blank: {line:?}");
            assert!(
                !line.chars().any(char::is_control),
                "control character in {line:?}"
            );
            if let Some(width) = width {
                assert!(line.width() <= width, "wider than {width}: {line:?}");
            }
        }
    }

    /// Readable color snapshots: ESC as `␛`.
    fn visible_escapes(s: &str) -> String {
        s.replace('\x1b', "␛")
    }

    // Host ids of `doc_fleet`.
    const AARCH64: HostId = 0;
    const X86_64: HostId = 1;
    const BIGMEM: HostId = 2;
    const MAC: HostId = 3;

    const DARWIN_SYSTEM: &str = "darwin-system-26.05.42ea902";

    /// The fleet of the [`status_table`] doc example, after a check.
    fn doc_fleet() -> Sim {
        let mut sim = Sim::new(vec![
            hm("al2-aarch64"),
            hm("al2-x86_64"),
            local(hm("bigmem"), false),
            mac(),
        ]);
        sim.finished();
        sim.resolved(github());
        sim.probed_deployed(
            AARCH64,
            active(HM_1, 93, 4 * MONTH + 2 * DAY),
            cached_rev(OLD_REV, NOW - 4 * MONTH - 9 * DAY),
        );
        sim.evaluated(AARCH64, wants(HM_9));
        sim.probed(X86_64, never_activated("no home-manager profile"));
        sim.evaluated(X86_64, wants(HM_9));
        sim.probed(BIGMEM, active(HM_9, 9, 12 * MIN));
        sim.cached(BIGMEM, HM_9);
        sim.probed(
            MAC,
            probe_error(
                ProbeFailure::Unreachable,
                "ssh: connect to host localhost port 2022: Connection refused",
            ),
        );
        sim.cached(MAC, &nix_path('d', DARWIN_SYSTEM));
        sim
    }

    #[test]
    fn reproduces_the_doc_example() {
        let doc = "\
github:jjant/nix-config @ 42ea902 (2h ago)

  ✗ al2-aarch64         outdated       on 1a2b3c4 (4mo old)
  ○ al2-x86_64          not activated  no home-manager profile
  ✓ bigmem (this host)  up to date
  – mac-m1              offline        reachable only from a desk you ssh into from the Mac

update: hostctl activate al2-aarch64 al2-x86_64
";
        let sim = doc_fleet();
        assert_eq!(sim.table(NO_COLOR, None), doc);
        // Nothing is pending, so the clock doesn't matter.
        assert_eq!(status_table(&sim.state, NO_COLOR, None, NOW), doc);
    }

    // Host ids of `every_status`.
    const E_AARCH64: HostId = 0;
    const E_X86_64: HostId = 1;
    const E_AL2023: HostId = 2;
    const E_BIGMEM: HostId = 3;
    const E_CI: HostId = 4;
    const E_MAC: HostId = 5;
    const E_NEW: HostId = 6;
    const E_WEB1: HostId = 7;
    const E_WEB2: HostId = 8;
    const E_TRYOUT: HostId = 9;

    /// One host per status, halfway through a check.
    fn every_status() -> Sim {
        let mut sim = Sim::new(vec![
            hm("al2-aarch64"),
            hm("al2-x86_64"),
            hm("al2023-x86_64"),
            local(hm("bigmem"), false),
            hm("ci"),
            mac(),
            hm("new-box"),
            host("web1", Kind::Nixos),
            host("web2", Kind::Nixos),
            host("tryout", Kind::Nixos),
        ]);
        sim.started();
        sim.resolved(github());
        // A branch commit, 3 h after the compared one.
        let branch = "9b8a7c6d5e4f30211223344556677889900aabbc";
        sim.probed_deployed(
            E_TRYOUT,
            with(active(HM_8, 12, 20 * MIN), |r| {
                r.revision = Some(branch.into())
            }),
            Some(DeployedRev {
                flake: GITHUB.into(),
                rev: Some(branch.into()),
                last_modified: github().last_modified.map(|t| t + 3 * 3600),
            }),
        );
        sim.evaluated(E_TRYOUT, wants(HM_9));
        sim.probed_deployed(
            E_AARCH64,
            active(HM_1, 93, 4 * MONTH + 2 * DAY),
            cached_rev(OLD_REV, NOW - 4 * MONTH - 9 * DAY),
        );
        sim.evaluated(E_AARCH64, wants(HM_9));
        sim.probed(E_X86_64, never_activated("no home-manager profile"));
        sim.evaluated(E_X86_64, wants(HM_9));
        sim.probed(E_AL2023, active(HM_5, 41, 3 * DAY));
        sim.apply(8_000, Event::EvalStarted(E_AL2023));
        sim.probed(E_BIGMEM, active(HM_9, 9, 12 * MIN));
        sim.cached(E_BIGMEM, HM_9);
        sim.probed(E_CI, active(HM_8, 3, 2 * DAY));
        sim.evaluated(
            E_CI,
            Err(ErrorInfo::new(
                "attribute 'ci' missing",
                "error: attribute 'ci' missing\n       at «github:jjant/nix-config/42ea902…»/flake.nix:12:5:",
            )),
        );
        sim.probed(
            E_MAC,
            probe_error(
                ProbeFailure::Unreachable,
                "ssh: connect to host localhost port 2022: Connection refused",
            ),
        );
        sim.cached(E_MAC, &nix_path('d', DARWIN_SYSTEM));
        sim.probed(E_NEW, active(HM_1, 1, 5 * MONTH));
        sim.probed(
            E_WEB1,
            probe_error(
                ProbeFailure::Unreachable,
                "ssh: Could not resolve hostname web1: Name or service not known",
            ),
        );
        sim.probed(
            E_WEB2,
            probe_error(
                ProbeFailure::Failed,
                "connection dropped / timed out after connecting",
            ),
        );
        sim
    }

    #[test]
    fn table_shows_every_status() {
        let sim = every_status();
        let statuses: Vec<Status> = sim.state.hosts.iter().map(HostState::status).collect();
        for status in STATUSES {
            assert!(statuses.contains(&status), "no {status:?} host");
        }
        insta::assert_snapshot!("status_every_status", sim.table(NO_COLOR, None));
    }

    #[test]
    fn table_with_colors() {
        let sim = every_status();
        let colored = sim.table(COLOR, None);
        assert_eq!(strip_sgr(&colored), sim.table(NO_COLOR, None));
        for painted in [
            "\x1b[32m✓\x1b[0m bigmem\x1b[36m (this host)\x1b[0m  \x1b[32mup to date\x1b[0m",
            "\x1b[33m✗\x1b[0m al2-aarch64         \x1b[33moutdated\x1b[0m",
            "\x1b[33m○\x1b[0m al2-x86_64          \x1b[33mnot activated\x1b[0m",
            "\x1b[31m!\x1b[0m ci                  \x1b[31meval failed\x1b[0m",
            "\x1b[31m!\x1b[0m web2                \x1b[31mprobe failed\x1b[0m",
            "\x1b[35m?\x1b[0m web1                \x1b[35munreachable\x1b[0m",
            "\x1b[2m–\x1b[0m mac-m1              \x1b[2moffline\x1b[0m",
            "\x1b[36m…\x1b[0m al2023-x86_64       \x1b[36mevaluating 12s\x1b[0m",
            "\x1b[34m↑\x1b[0m tryout              \x1b[34mahead\x1b[0m",
            "\x1b[1mgithub:jjant/nix-config\x1b[0m @ \x1b[33m42ea902\x1b[0m",
            "\x1b[2mon \x1b[0m\x1b[33m1a2b3c4\x1b[0m\x1b[2m (4mo old)\x1b[0m",
        ] {
            assert!(
                colored.contains(painted),
                "{painted:?} missing in\n{colored}"
            );
        }
        insta::assert_snapshot!("status_color", visible_escapes(&colored));
    }

    #[test]
    fn no_escape_codes_without_color() {
        for sim in [doc_fleet(), every_status(), drift_fleet(), hostile_fleet()] {
            for width in [None, Some(40), Some(80)] {
                assert!(!sim.table(NO_COLOR, width).contains('\x1b'));
            }
        }
    }

    #[test]
    fn pending_rows_show_activity_and_elapsed_time() {
        let mut sim = Sim::new(vec![hm("a"), hm("b"), hm("c"), hm("d"), hm("e")]);
        sim.started();
        sim.apply(0, Event::FlakeStarted { url: GITHUB.into() });
        sim.apply(0, Event::ProbeStarted(0));
        sim.apply(19_400, Event::ProbeStarted(1));
        sim.probed(2, active(HM_1, 1, HOUR));
        sim.apply(1_000, Event::EvalStarted(2));
        sim.probed(3, active(HM_1, 1, HOUR));
        // A diff in flight doesn't make an outdated host pending.
        sim.probed(4, active(HM_1, 1, HOUR));
        sim.evaluated(4, wants(HM_9));
        sim.apply(
            5_000,
            Event::DiffStarted {
                id: 4,
                current: sp(HM_1),
                desired: sp(HM_9),
            },
        );
        let table = sim.table(NO_COLOR, None);
        insta::assert_snapshot!("status_pending", table);
        assert!(table.contains("… a  probing 20s"), "{table}");
        assert!(
            table.contains("… b  probing\n"),
            "under a second: no timer\n{table}"
        );
        assert!(table.contains("… c  evaluating 19s"), "{table}");
        assert!(table.contains("… d  waiting"), "{table}");
        assert!(table.contains("· resolving…"), "{table}");
    }

    #[test]
    fn ticking_timers_count_whole_seconds() {
        assert_eq!(ticking(Duration::from_millis(999)), None);
        assert_eq!(ticking(Duration::from_millis(1_000)).as_deref(), Some("1s"));
        assert_eq!(
            ticking(Duration::from_millis(12_900)).as_deref(),
            Some("12s")
        );
        assert_eq!(ticking(Duration::from_secs(59)).as_deref(), Some("59s"));
        assert_eq!(ticking(Duration::from_secs(65)).as_deref(), Some("1m05s"));
        assert_eq!(
            ticking(Duration::from_secs(3_720)).as_deref(),
            Some("1h02m")
        );
    }

    #[test]
    fn interrupted_checks_say_so() {
        let mut sim = Sim::new(vec![hm("a"), hm("b"), hm("c")]);
        sim.apply(0, Event::RunStarted);
        sim.apply(0, Event::FlakeStarted { url: GITHUB.into() });
        sim.apply(0, Event::ProbeStarted(0));
        sim.probed(1, active(HM_1, 1, HOUR));
        sim.apply(300, Event::EvalStarted(1));
        sim.apply(2_000, Event::Interrupted);
        let table = sim.table(NO_COLOR, None);
        insta::assert_snapshot!("status_interrupted", table);
        assert!(!table.contains("resolving"), "{table}");
        assert!(table.contains("… a  interrupted"), "{table}");
    }

    // Host ids of `drift_fleet`.
    const D_SPECIALISATION: HostId = 6;

    /// Every drift, and the badges.
    fn drift_fleet() -> Sim {
        let nixos_old = nix_path('a', "nixos-system-web1-25.11.20250601.1a2b3c4");
        let nixos_new = nix_path('b', "nixos-system-web1-25.11.20251215.42ea902");
        let gaming = nix_path('c', "nixos-system-web1-25.11.20251215.42ea902");
        let darwin_old = nix_path('f', "darwin-system-26.05.1a2b3c4");
        let darwin_new = nix_path('g', DARWIN_SYSTEM);
        let mut sim = Sim::new(vec![
            host("darwin-gen", Kind::Darwin),
            host("dirty-mac", Kind::Darwin),
            hm("hm-cache"),
            hm("hm-same"),
            hm("hm-staged"),
            hm("hm-unknown"),
            host("nixos-gaming", Kind::Nixos),
            host("nixos-reboot", Kind::Nixos),
            host("nixos-uptodate", Kind::Nixos),
            host("darwin-dated", Kind::Darwin),
            host("darwin-ahead", Kind::Darwin),
        ]);
        sim.finished();
        sim.resolved(github());
        let revision = |rev: &str| {
            let rev = rev.to_string();
            move |r: &mut ProbeReport| r.revision = Some(rev)
        };
        sim.probed(
            0,
            with(active(&darwin_old, 41, 3 * 7 * DAY), revision(OLD_REV)),
        );
        sim.cached(0, &darwin_new);
        sim.probed(
            1,
            with(
                active(&darwin_old, 42, 2 * DAY),
                revision("a8f62b6f2c1170af6c40178c9080c87fc51fe5de-dirty"),
            ),
        );
        sim.cached(1, &darwin_new);
        sim.probed_deployed(
            2,
            active(HM_1, 93, 4 * MONTH),
            cached_rev(OLD_REV, NOW - 4 * MONTH - 9 * DAY),
        );
        sim.cached(2, HM_9);
        sim.probed(3, with(active(HM_8, 12, 3 * HOUR), revision(REV)));
        sim.cached(3, HM_9);
        sim.probed(
            4,
            with(active(HM_5, 60, 2 * DAY), |r| r.profile = Some(sp(HM_9))),
        );
        sim.cached(4, HM_9);
        sim.probed(5, active(HM_1, 9, 12 * MIN));
        sim.cached(5, HM_9);
        sim.probed(
            D_SPECIALISATION,
            with(active(&gaming, 77, 6 * HOUR), |r| {
                r.toplevel = Some(sp(&nixos_new));
                r.specialisation = Some("gaming".into());
            }),
        );
        sim.cached(D_SPECIALISATION, &nixos_new);
        sim.probed(
            7,
            with(active(&nixos_old, 120, 5 * DAY), |r| {
                r.revision = Some(OLD_REV.into());
                r.reboot_required = true;
            }),
        );
        sim.cached(7, &nixos_new);
        sim.probed(
            8,
            with(active(&nixos_new, 121, DAY), |r| r.reboot_required = true),
        );
        sim.cached(8, &nixos_new);
        // Recorded revisions that were dated: an older one, a newer one.
        let dated = |rev: &str, at: Option<i64>| {
            Some(DeployedRev {
                flake: GITHUB.into(),
                rev: Some(rev.into()),
                last_modified: at,
            })
        };
        sim.probed_deployed(
            9,
            with(active(&darwin_old, 40, 3 * 7 * DAY), revision(OLD_REV)),
            dated(OLD_REV, Some(NOW - 3 * 7 * DAY - DAY)),
        );
        sim.cached(9, &darwin_new);
        let branch = "9b8a7c6d5e4f30211223344556677889900aabbc";
        sim.probed_deployed(
            10,
            with(active(&darwin_old, 43, 20 * MIN), revision(branch)),
            dated(branch, github().last_modified.map(|t| t + 2 * HOUR)),
        );
        sim.cached(10, &darwin_new);
        sim
    }

    #[test]
    fn table_explains_every_drift() {
        let sim = drift_fleet();
        let drifts: Vec<Option<Drift>> = sim.state.hosts.iter().map(HostState::drift).collect();
        assert!(drifts.contains(&Some(Drift::Staged)));
        assert!(drifts.contains(&Some(Drift::SameRevision)));
        assert!(drifts.contains(&Some(Drift::Unknown)));
        assert!(drifts.iter().flatten().any(|d| matches!(
            d,
            Drift::Revision {
                from_generation: true,
                last_modified: None,
                ..
            }
        )));
        assert!(drifts.iter().flatten().any(|d| matches!(
            d,
            Drift::Revision {
                from_generation: false,
                last_modified: Some(_),
                ..
            }
        )));
        assert!(drifts.iter().flatten().any(|d| matches!(
            d,
            Drift::Revision {
                from_generation: true,
                last_modified: Some(_),
                newer_by: None,
                ..
            }
        )));
        assert!(drifts.iter().flatten().any(Drift::is_newer));
        assert_eq!(sim.state.hosts[10].status(), Status::Ahead);
        assert_eq!(
            sim.state.hosts[D_SPECIALISATION].status(),
            Status::UpToDate,
            "a specialisation compares its parent"
        );
        insta::assert_snapshot!("status_drift", sim.table(NO_COLOR, None));
    }

    #[test]
    fn header_warns_about_stale_and_unlocked_flakes() {
        let mut sim = Sim::new(vec![hm("bigmem")]);
        sim.finished();
        sim.resolved(FlakeInfo {
            stale: Some(
                "unable to download 'https://api.github.com/repos/jjant/nix-config/commits/HEAD': HTTP error 403"
                    .into(),
            ),
            lock_outdated: true,
            ..github()
        });
        sim.probed(0, active(HM_9, 9, 12 * MIN));
        sim.cached(0, HM_9);
        assert_eq!(sim.state.exit_code(), 2, "stale flakes are trouble");
        insta::assert_snapshot!("status_stale_flake", sim.table(NO_COLOR, None));
        let colored = sim.table(COLOR, None);
        assert!(
            colored.contains("\x1b[33mflake.lock out of date\x1b[0m"),
            "{colored}"
        );
        assert!(
            colored.contains("\x1b[33mstale: unable to download"),
            "{colored}"
        );
    }

    #[test]
    fn header_lists_every_flake() {
        let other = "github:jjant/work-config";
        let pending = "sourcehut:~jj/lab";
        let mut dirty = hm("scratch");
        dirty.flake = dirty_git().flake;
        let mut work = hm("work-desk");
        work.flake = other.into();
        let mut lab = hm("lab");
        lab.flake = pending.into();
        let mut sim = Sim::new(vec![hm("bigmem"), dirty, lab, work]);
        sim.started();
        sim.resolved(github());
        sim.resolved(dirty_git());
        sim.unresolved(
            other,
            ErrorInfo::new(
                "unable to download 'https://api.github.com/repos/jjant/work-config/commits/HEAD': HTTP error 404",
                "…",
            ),
        );
        sim.apply(
            0,
            Event::FlakeStarted {
                url: pending.into(),
            },
        );
        sim.probed(0, active(HM_9, 9, 12 * MIN));
        sim.cached(0, HM_9);
        sim.probed(1, active(HM_8, 2, 5 * MIN));
        sim.evaluated(1, wants(HM_9));
        sim.probed(3, active(HM_5, 5, DAY));
        sim.evaluated(
            3,
            Err(ErrorInfo::msg(
                "unable to download 'https://api.github.com/repos/jjant/work-config/commits/HEAD': HTTP error 404",
            )),
        );
        let table = sim.table(NO_COLOR, None);
        insta::assert_snapshot!("status_two_flakes", table);
        // A dirty local tree can't be activated remotely: no update hint.
        assert!(!table.contains("update:"), "{table}");
    }

    #[test]
    fn local_and_assumed_hosts_are_marked() {
        let mut sim = Sim::new(vec![
            local(hm("bigmem"), false),
            local(host("mac-m1", Kind::Darwin), true),
            hm("web1"),
        ]);
        sim.finished();
        sim.resolved(github());
        for id in 0..3 {
            sim.probed(id, active(HM_1, 4, HOUR));
            sim.cached(id, HM_1);
        }
        let table = sim.table(NO_COLOR, None);
        insta::assert_snapshot!("status_local_markers", table);
        let json = sim.json();
        let local: Vec<serde_json::Value> = json["hosts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["local"].clone())
            .collect();
        assert_eq!(
            local,
            [
                serde_json::json!("local"),
                "assumed".into(),
                serde_json::Value::Null
            ]
        );
    }

    #[test]
    fn notes_follow_the_detail() {
        let mut up = local(hm("bigmem"), false);
        up.note = Some("the build box".into());
        let mut multi = hm("lab");
        multi.note = Some("  rack 3,\n\tshelf 2\n".into());
        let mut quiet = mac();
        quiet.note = None;
        let mut sim = Sim::new(vec![up, multi, quiet, mac()]);
        sim.finished();
        sim.resolved(github());
        sim.probed(0, active(HM_9, 9, 12 * MIN));
        sim.cached(0, HM_9);
        sim.probed(1, active(HM_1, 3, DAY));
        sim.cached(1, HM_9);
        for id in [2, 3] {
            sim.probed(
                id,
                probe_error(
                    ProbeFailure::Unreachable,
                    "ssh: connect to host localhost port 2022: Connection refused",
                ),
            );
        }
        let table = sim.table(NO_COLOR, None);
        insta::assert_snapshot!("status_notes", table);
        assert!(
            table.contains("rack 3, shelf 2"),
            "notes stay on one line\n{table}"
        );
    }

    /// Long eval errors (real ones, from the fixtures), a long ssh error and
    /// a long global error.
    fn long_errors() -> Sim {
        let mut sim = Sim::new(vec![
            hm("al2023-x86_64"),
            local(hm("bigmem"), false),
            host("web1", Kind::Nixos),
        ]);
        sim.finished();
        sim.resolved(github());
        sim.probed(0, active(HM_1, 1, DAY));
        // tests/fixtures/eval-missing-attr.stderr
        sim.evaluated(
            0,
            Err(ErrorInfo::msg(
                "flake 'path:/tmp/hostctl-ifd' does not provide attribute 'packages.x86_64-linux.nope.outPath', 'legacyPackages.x86_64-linux.nope.outPath' or 'nope.outPath'",
            )),
        );
        sim.probed(1, active(HM_8, 8, HOUR));
        // tests/fixtures/eval-ifd-read-only.stderr
        sim.evaluated(
            1,
            Err(ErrorInfo::msg(
                "path '/nix/store/wdfh0jh598v4qshmcs296ri45p0win4b-gen.drv' is not valid",
            )),
        );
        sim.probed(
            2,
            probe_error(
                ProbeFailure::Unreachable,
                "ssh: Could not resolve hostname web1.internal.example.com: Name or service not known (set `destination` for web1)",
            ),
        );
        sim.apply(
            0,
            Event::Error(ErrorInfo::msg(
                "nix.command = \"nix\" could not be run: No such file or directory (os error 2); is Nix installed and on PATH?",
            )),
        );
        sim
    }

    #[test]
    fn long_errors_print_in_full_without_a_width() {
        let table = long_errors().table(NO_COLOR, None);
        assert!(table.contains("or 'nope.outPath'\n"), "{table}");
        insta::assert_snapshot!("status_long_errors", table);
    }

    #[test]
    fn long_errors_are_cut_to_the_width() {
        let sim = long_errors();
        insta::assert_snapshot!("status_long_errors_80", sim.table(NO_COLOR, Some(80)));
        insta::assert_snapshot!("status_long_errors_50", sim.table(NO_COLOR, Some(50)));
        insta::assert_snapshot!("status_long_errors_24", sim.table(NO_COLOR, Some(24)));
    }

    #[test]
    fn every_line_fits_every_width() {
        for sim in [
            doc_fleet(),
            every_status(),
            drift_fleet(),
            long_errors(),
            unicode_fleet(),
        ] {
            let full = sim.table(NO_COLOR, None);
            let widest = full.lines().map(UnicodeWidthStr::width).max().unwrap();
            for width in 0..=widest + 2 {
                // `Sim::table` checks the width of every line.
                let table = sim.table(NO_COLOR, Some(width));
                assert_eq!(table.lines().count(), full.lines().count());
                assert_eq!(strip_sgr(&sim.table(COLOR, Some(width))), table);
            }
            assert_eq!(sim.table(NO_COLOR, Some(widest)), full, "fits: nothing cut");
            assert_ne!(sim.table(NO_COLOR, Some(widest - 1)), full);
        }
    }

    fn unicode_fleet() -> Sim {
        let mut tokyo = hm("東京-desk");
        tokyo.note = Some("会議室の横の机、再起動は金曜日だけ".into());
        let mut sim = Sim::new(vec![
            hm("mäc-mini"),
            hm("zürich"),
            tokyo,
            local(hm("ñandú"), false),
            hm("🍎-air"),
        ]);
        sim.finished();
        sim.resolved(github());
        for id in 0..5 {
            sim.probed(id, active(HM_5, 5 + id as u64, (id as i64 + 1) * DAY));
            sim.cached(id, if id % 2 == 0 { HM_9 } else { HM_5 });
        }
        sim
    }

    #[test]
    fn unicode_names_align_by_display_width() {
        let sim = unicode_fleet();
        let table = sim.table(NO_COLOR, None);
        insta::assert_snapshot!("status_unicode", table);
        // Every status cell starts at the same column.
        let columns: Vec<usize> = table
            .lines()
            .filter(|l| l.starts_with("  "))
            .map(|l| {
                let at = l.find(['✓', '✗']).unwrap();
                l[..at].width()
            })
            .collect();
        assert!(
            columns.windows(2).all(|w| w[0] == w[1]),
            "{columns:?}\n{table}"
        );
        insta::assert_snapshot!("status_unicode_40", sim.table(NO_COLOR, Some(40)));
    }

    /// Escape sequences everywhere a remote host or a flake can put text.
    fn hostile_fleet() -> Sim {
        let mut evil = hm("evil\u{1b}]52;c;Y3VybA==\u{7}");
        evil.note = Some("\u{1b}[2Jgotcha".into());
        let mut bidi = hm("bidi\u{202e}txt");
        bidi.flake = "github:evil/\u{9b}31mflake".into();
        let mut sim = Sim::new(vec![evil, bidi]);
        sim.finished();
        sim.resolved(FlakeInfo {
            stale: Some("cached \u{1b}[5mversion".into()),
            ..github()
        });
        sim.resolved(FlakeInfo {
            flake: "github:evil/\u{9b}31mflake".into(),
            rev: Some("\u{1b}[31mdeadbeef".into()),
            ..github()
        });
        sim.probed(
            0,
            with(active(HM_1, 1, DAY), |r| {
                r.platform = Some("x86_64-linux\u{1b}[8m".into());
                r.revision = Some("\u{1b}[1A\u{1b}[2K".into());
                r.specialisation = Some("spec\u{7}".into());
            }),
        );
        sim.cached(0, HM_9);
        sim.probed(1, never_activated("no \u{1b}[31mprofile\u{9b}"));
        sim.cached(1, HM_9);
        sim.apply(0, Event::Error(ErrorInfo::msg("global \u{1b}[H error")));
        sim
    }

    #[test]
    fn untrusted_text_is_sanitized() {
        let sim = hostile_fleet();
        // `Sim::table` asserts that no control character is left.
        let table = sim.table(NO_COLOR, None);
        insta::assert_snapshot!("status_hostile", table);
        assert!(table.contains("evil\\u{1b}]52;c;Y3VybA==\\u{7}"), "{table}");
        assert!(table.contains("bidi\\u{202e}txt"), "{table}");
        assert!(!table.contains('\u{202e}') && !table.contains('\u{9b}'));
        // With colors, the only escapes are hostctl's own SGR sequences.
        let colored = sim.table(COLOR, None);
        assert_eq!(strip_sgr(&colored), table);

        let json = serde_json::to_string(&status_report(&sim.state)).unwrap();
        for raw in ['\u{1b}', '\u{7}', '\u{9b}', '\u{202e}'] {
            assert!(!json.contains(raw), "{raw:?} in {json}");
        }
        assert!(
            !json.contains("\\u001b"),
            "escapes are sanitized, not JSON-escaped"
        );
        let json = sim.json();
        let evil = &json["hosts"][0];
        assert_eq!(evil["name"], "evil\\u{1b}]52;c;Y3VybA==\\u{7}");
        assert_eq!(evil["note"], "\\u{1b}[2Jgotcha");
        assert_eq!(evil["platform"], "x86_64-linux\\u{1b}[8m");
        assert_eq!(evil["revision"], "\\u{1b}[1A\\u{1b}[2K");
        assert_eq!(evil["drift"]["rev"], "\\u{1b}[1A\\u{1b}[2K");
        assert_eq!(evil["specialisation"], "spec\\u{7}");
        assert_eq!(json["flakes"][0]["stale"], "cached \\u{1b}[5mversion");
        assert_eq!(json["flakes"][1]["flake"], "github:evil/\\u{9b}31mflake");
        assert_eq!(json["flakes"][1]["rev"], "\\u{1b}[31mdeadbeef");
    }

    #[test]
    fn update_lists_hosts_with_an_activation() {
        let mut no_template = hm("custom-box");
        no_template.activate = None;
        let mut weird = hm("it's a box");
        weird.target = Target::Local { assumed: false };
        let mut sim = Sim::new(vec![
            hm("al2-aarch64"),
            no_template,
            weird,
            mac(),
            hm("pending"),
            hm("up"),
        ]);
        sim.finished();
        sim.resolved(github());
        for id in 0..4 {
            sim.probed(id, active(HM_1, 1, DAY));
            sim.cached(id, HM_9);
        }
        // The Mac answers through its forced command: no remote activation.
        sim.probed(
            3,
            Ok(ProbeReport {
                current: Some(sp(HM_1)),
                ..ProbeReport::default()
            }),
        );
        sim.probed(4, never_activated("no home-manager profile"));
        sim.probed(5, active(HM_9, 1, DAY));
        sim.cached(5, HM_9);
        let table = sim.table(NO_COLOR, None);
        let update = table.lines().find(|l| l.starts_with("update:")).unwrap();
        assert_eq!(
            update,
            "update: hostctl activate al2-aarch64 'it'\\''s a box'"
        );
        // Started with flags: the suggestion has them too.
        sim.state.command = "hostctl -c '/home/jj/my hosts.toml' -f github:jj/nix".into();
        let table = sim.table(NO_COLOR, None);
        let update = table.lines().find(|l| l.starts_with("update:")).unwrap();
        assert_eq!(
            update,
            "update: hostctl -c '/home/jj/my hosts.toml' -f github:jj/nix activate al2-aarch64 \
             'it'\\''s a box'"
        );
        // A name that looks like an option doesn't pass for one.
        sim.state.command = "hostctl".into();
        sim.state.hosts[0].host.name = "-x".into();
        let table = sim.table(NO_COLOR, None);
        let update = table.lines().find(|l| l.starts_with("update:")).unwrap();
        assert_eq!(update, "update: hostctl activate -- -x 'it'\\''s a box'");
    }

    #[test]
    fn global_errors_end_the_footer() {
        let mut sim = Sim::new(vec![]);
        sim.apply(0, Event::Error(ErrorInfo::msg("nix not found")));
        sim.apply(
            0,
            Event::Error(ErrorInfo::new("ssh not found\nsecond line", "…")),
        );
        assert_eq!(
            sim.table(NO_COLOR, None),
            "error: nix not found\nerror: ssh not found second line\n"
        );
        assert_eq!(
            sim.table(COLOR, None),
            "\x1b[31merror:\x1b[0m nix not found\n\x1b[31merror:\x1b[0m ssh not found second line\n"
        );
        assert_eq!(Sim::new(vec![]).table(NO_COLOR, None), "");
    }

    #[test]
    fn statuses_are_listed_most_urgent_first() {
        assert!(STATUSES.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(STATUSES[0], Status::EvalFailed);
        assert_eq!(STATUSES[8], Status::UpToDate);
    }

    #[test]
    fn lines_are_cut_across_segments() {
        let cut = |max: Option<usize>, style: Style| {
            let mut line = Line::default();
            line.push("abc", RED).push("def", PLAIN).push("   ", PLAIN);
            let mut out = String::new();
            line.write(&mut out, style, max);
            out
        };
        assert_eq!(cut(None, NO_COLOR), "abcdef\n");
        assert_eq!(
            cut(Some(6), NO_COLOR),
            "abcdef\n",
            "trailing blanks don't count"
        );
        assert_eq!(cut(Some(5), NO_COLOR), "abcd…\n");
        assert_eq!(cut(Some(4), NO_COLOR), "abc…\n");
        assert_eq!(cut(Some(3), NO_COLOR), "ab…\n");
        assert_eq!(cut(Some(1), NO_COLOR), "…\n");
        assert_eq!(cut(Some(0), NO_COLOR), "\n");
        assert_eq!(cut(Some(4), COLOR), "\x1b[31mabc\x1b[0m…\n");
        assert_eq!(cut(Some(3), COLOR), "\x1b[31mab…\x1b[0m\n");

        let mut wide = Line::default();
        wide.push("日本語", PLAIN);
        for (max, want) in [(6, "日本語\n"), (5, "日本…\n"), (4, "日…\n"), (2, "…\n")] {
            let mut out = String::new();
            wide.write(&mut out, NO_COLOR, Some(max));
            assert_eq!(out, want, "{max}");
        }
        let mut blank = Line::default();
        blank.push("   ", PLAIN);
        let mut out = String::new();
        blank.write(&mut out, COLOR, Some(2));
        assert_eq!(out, "\n");
    }

    #[test]
    fn prefixes_respect_display_width() {
        assert_eq!(prefix("hello", 3), "hel");
        assert_eq!(prefix("hello", 9), "hello");
        assert_eq!(prefix("日本語", 3), "日");
        assert_eq!(
            prefix("e\u{301}x", 1),
            "e\u{301}",
            "combining marks stay attached"
        );
        assert_eq!(prefix("", 0), "");
        assert_eq!(spaces(3), "   ");
        assert_eq!(spaces(100).len(), 100);
    }

    #[test]
    fn text_helpers() {
        assert!(matches!(one_line("plain"), Cow::Borrowed("plain")));
        assert_eq!(one_line("  a\nb\t\tc \n "), "a b c");
        assert_eq!(one_line("x\u{1b}[2Jy"), "x\\u{1b}[2Jy");
        assert_eq!(short_rev(REV), "42ea902");
        assert_eq!(
            short_rev("a8f62b6f2c1170af6c40178c9080c87fc51fe5de-dirty"),
            "a8f62b6-dirty"
        );
        assert_eq!(short_rev("abc"), "abc");
        assert_eq!(short_rev("ünïcödé-rev"), "ünïcödé");
        assert_eq!(age(NOW - 4 * MONTH - DAY, NOW), "4mo");
        assert_eq!(age(NOW - 3 * HOUR, NOW), "3h");
        assert_eq!(age(NOW - 10, NOW), "<1m");
        assert_eq!(age(NOW + 100, NOW), "<1m");
        let mut detail = Detail::default();
        detail.item("a");
        detail.item("");
        detail.styled([(String::new(), YELLOW)]);
        detail.styled([("2".into(), YELLOW), ("!".into(), DIM)]);
        let pieces: Vec<_> = detail
            .pieces
            .iter()
            .map(|(t, c)| (t.as_str(), *c))
            .collect();
        assert_eq!(
            pieces,
            [("a", DIM), (" · ", DIM), ("2", YELLOW), ("!", DIM)]
        );
    }

    #[test]
    fn report_json_snapshot() {
        let sim = every_status();
        let json = serde_json::to_string_pretty(&sim.json()).unwrap();
        insta::assert_snapshot!("report_every_status", json);
    }

    #[test]
    fn report_absent_values_are_null_not_omitted() {
        let mut sim = Sim::new(vec![hm("new-box"), hm("other")]);
        sim.started();
        sim.unresolved(GITHUB, ErrorInfo::msg("HTTP error 403"));
        let json = sim.json();
        let pending = json["hosts"][0].as_object().unwrap();
        for key in [
            "local",
            "drift",
            "current",
            "desired",
            "cached",
            "generation",
            "activated",
            "platform",
            "revision",
            "specialisation",
            "booted",
            "note",
            "error",
            "activate",
        ] {
            assert_eq!(pending.get(key), Some(&serde_json::Value::Null), "{key}");
        }
        assert_eq!(pending["timings"]["probeMs"], serde_json::Value::Null);
        assert_eq!(pending["timings"]["evalMs"], serde_json::Value::Null);
        assert_eq!(pending["rebootRequired"], false);
        assert_eq!(pending["status"], "pending");
        let flake = json["flakes"][0].as_object().unwrap();
        for key in ["locked", "rev", "lastModified", "stale"] {
            assert_eq!(flake.get(key), Some(&serde_json::Value::Null), "{key}");
        }
        assert_eq!(flake["error"]["summary"], "HTTP error 403");
        // Every field of every host is present, whatever its state.
        let keys = |v: &serde_json::Value| -> Vec<String> {
            v.as_object().unwrap().keys().cloned().collect()
        };
        let every = every_status().json();
        for host in every["hosts"].as_array().unwrap() {
            assert_eq!(keys(host), keys(&json["hosts"][0]));
            assert_eq!(keys(&host["timings"]), ["evalMs", "probeMs"]);
        }
        assert_eq!(
            keys(&every),
            ["errors", "flakes", "hosts", "summary", "version"]
        );
    }

    #[test]
    fn report_fields() {
        let sim = every_status();
        let json = sim.json();
        assert_eq!(json["version"], 1);
        assert_eq!(
            json["summary"],
            serde_json::json!({
                "total": 10, "upToDate": 1, "outdated": 2, "ahead": 1, "trouble": 3, "pending": 2, "offline": 1
            })
        );
        let hosts = &json["hosts"];
        let aarch64 = &hosts[E_AARCH64];
        assert_eq!(aarch64["status"], "outdated");
        assert_eq!(
            aarch64["drift"],
            serde_json::json!({
                "type": "revision", "rev": OLD_REV, "lastModified": NOW - 4 * MONTH - 9 * DAY,
                "fromGeneration": false, "newer": false, "branches": []
            })
        );
        assert_eq!(aarch64["current"], HM_1);
        assert_eq!(aarch64["desired"], HM_9);
        assert_eq!(aarch64["cached"], false);
        assert_eq!(
            aarch64["timings"],
            serde_json::json!({ "probeMs": 250, "evalMs": 3000 })
        );
        assert_eq!(aarch64["activate"]["runsOn"], "remote");
        assert_eq!(
            aarch64["activate"]["command"],
            format!(
                "out=$(nix --extra-experimental-features 'nix-command flakes' build --no-link \
                 --print-out-paths '{}'#.homeConfigurations.al2-aarch64.activationPackage) && \
                 [ \"$out\" = {HM_9} ] && \"$out/activate\"",
                github().locked
            )
        );
        let bigmem = &hosts[E_BIGMEM];
        assert_eq!(bigmem["local"], "local");
        assert_eq!(bigmem["destination"], serde_json::Value::Null);
        assert_eq!(bigmem["cached"], true);
        assert_eq!(
            bigmem["timings"]["evalMs"],
            serde_json::Value::Null,
            "cache hit"
        );
        assert_eq!(bigmem["activate"]["runsOn"], "here");
        let mac = &hosts[E_MAC];
        assert_eq!(mac["status"], "offline");
        assert_eq!(mac["destination"], "ssh://devuser1@localhost:2022");
        assert_eq!(
            mac["activate"],
            serde_json::Value::Null,
            "forced-command route"
        );
        assert_eq!(
            mac["error"]["summary"],
            "ssh: connect to host localhost port 2022: Connection refused"
        );
        // The eval error wins over a successful probe.
        assert_eq!(hosts[E_CI]["error"]["summary"], "attribute 'ci' missing");
        assert_eq!(hosts[E_CI]["generation"], 3);
        assert_eq!(hosts[E_WEB2]["status"], "probe-failed");
        assert_eq!(hosts[E_AL2023]["status"], "pending");
        assert_eq!(
            hosts[E_AL2023]["timings"]["evalMs"],
            serde_json::Value::Null
        );
        assert_eq!(
            hosts[E_NEW]["activate"],
            serde_json::Value::Null,
            "nothing to compare yet"
        );
        assert_eq!(hosts[E_X86_64]["status"], "not-activated");
        assert_eq!(hosts[E_WEB1]["kind"], "nixos");
    }

    #[test]
    fn report_specialisations_and_reboots() {
        let sim = drift_fleet();
        let json = sim.json();
        let gaming = &json["hosts"][D_SPECIALISATION];
        assert_eq!(gaming["status"], "up-to-date");
        assert_eq!(gaming["specialisation"], "gaming");
        assert_eq!(
            gaming["current"], gaming["desired"],
            "the deployed parent toplevel"
        );
        let reboot = &json["hosts"][7];
        assert_eq!(reboot["rebootRequired"], true);
        assert_eq!(
            reboot["drift"],
            serde_json::json!({
                "type": "revision", "rev": OLD_REV, "lastModified": null, "fromGeneration": true,
                "newer": false, "branches": []
            })
        );
        assert_eq!(
            json["hosts"][4]["drift"],
            serde_json::json!({ "type": "staged" })
        );
        assert_eq!(
            json["hosts"][3]["drift"],
            serde_json::json!({ "type": "same-revision" })
        );
        assert_eq!(
            json["hosts"][5]["drift"],
            serde_json::json!({ "type": "unknown" })
        );
        let flake = &json["flakes"][0];
        assert_eq!(flake["rev"], REV);
        assert_eq!(flake["lastModified"], 1_792_240_011);
        assert_eq!(flake["lockOutdated"], false);
        assert_eq!(flake["error"], serde_json::Value::Null);
    }

    #[test]
    fn report_rev_of_a_dirty_tree() {
        let mut h = hm("scratch");
        h.flake = dirty_git().flake;
        let mut sim = Sim::new(vec![h]);
        sim.resolved(dirty_git());
        let flake = &sim.json()["flakes"][0];
        assert_eq!(
            flake["rev"],
            "a8f62b6f2c1170af6c40178c9080c87fc51fe5de-dirty"
        );
        assert_eq!(flake["locked"], "git+file:///tmp/hostctl-ifd");
    }

    fn change(
        name: &str,
        removed: &[&str],
        added: &[&str],
        sizes: (Option<u64>, Option<u64>),
    ) -> Change {
        let strings = |v: &[&str]| v.iter().map(|s| s.to_string()).collect();
        Change {
            name: name.into(),
            removed: strings(removed),
            added: strings(added),
            in_before: sizes.0.is_some(),
            in_after: sizes.1.is_some(),
            size_before: sizes.0.unwrap_or(0),
            size_after: sizes.1.unwrap_or(0),
        }
    }

    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;

    fn closure_diff(changes: Vec<Change>, paths: (usize, usize), sizes: (u64, u64)) -> ClosureDiff {
        ClosureDiff {
            changes,
            paths_before: paths.0,
            paths_after: paths.1,
            size_before: sizes.0,
            size_after: sizes.1,
        }
    }

    #[test]
    fn diff_reproduces_the_doc_example() {
        let before = 3 * GIB + 150 * MIB;
        let diff = closure_diff(
            vec![
                change(
                    "firefox",
                    &["131.0"],
                    &["132.0"],
                    (Some(250 * MIB), Some(250 * MIB + 2_202_010)),
                ),
                change(
                    "home-manager",
                    &[],
                    &[],
                    (Some(10 * KIB), Some(10 * KIB + 11_060)),
                ),
                change("old-tool", &["0.3"], &[], (Some(1_258_292), None)),
                change("zenoh", &[], &["1.10.0"], (None, Some(348_055_896))),
            ],
            (702, 705),
            (before, before + 25_480_397),
        );
        let doc = "\
bigmem: 702 → 705 paths · 3.1 GiB → 3.2 GiB (+24.3 MiB)
  firefox       131.0 → 132.0    +2.1 MiB  upgraded
  home-manager                  +10.8 KiB  resized
  old-tool      0.3 → ∅          -1.2 MiB  removed
  zenoh         ∅ → 1.10.0     +331.9 MiB  added
";
        assert_eq!(diff_text("bigmem", &diff, NO_COLOR), doc);
    }

    #[test]
    fn diff_of_nothing() {
        let same = closure_diff(vec![], (702, 702), (7 * GIB, 7 * GIB));
        assert_eq!(
            diff_text("bigmem", &same, NO_COLOR),
            "bigmem: no package changes (702 paths)\n"
        );
        let rehashed = closure_diff(vec![], (701, 702), (7 * GIB, 7 * GIB + 100));
        assert_eq!(
            diff_text("bigmem", &rehashed, NO_COLOR),
            "bigmem: no package changes (701 → 702 paths)\n"
        );
        assert_eq!(
            diff_text("bigmem", &same, COLOR),
            "\x1b[1mbigmem\x1b[0m: no package changes (702 paths)\n"
        );
        insta::assert_snapshot!("diff_empty", diff_text("al2-x86_64", &same, NO_COLOR));
    }

    fn every_kind() -> ClosureDiff {
        closure_diff(
            vec![
                change("atuin", &[], &[""], (Some(74_175_600), Some(74_175_776))),
                change(
                    "bash",
                    &["5.3p3"],
                    &["5.2p37"],
                    (Some(9 * MIB), Some(8 * MIB)),
                ),
                change(
                    "firefox",
                    &["131.0"],
                    &["132.0"],
                    (Some(250 * MIB), Some(252 * MIB)),
                ),
                change("hm_tmuxwindownaming", &[], &[""], (None, Some(1_848))),
                change("home-manager", &[], &[], (Some(4_289_336), Some(4_300_392))),
                change("hosts", &["0.0.0", "0.0.0-fish"], &[], (Some(7_872), None)),
                change(
                    "python3",
                    &["3.12.8", "3.13.1"],
                    &["3.13.2"],
                    (Some(180 * MIB), Some(110 * MIB)),
                ),
                change(
                    "zenoh",
                    &[],
                    &["1.10.0", "1.10.0-fish"],
                    (None, Some(348_055_896)),
                ),
            ],
            (695, 702),
            (7_474_800_208, 7_856_012_864),
        )
    }

    #[test]
    fn diff_every_kind() {
        let diff = every_kind();
        let kinds: Vec<ChangeKind> = diff.changes.iter().map(Change::kind).collect();
        for kind in [
            ChangeKind::Added,
            ChangeKind::Removed,
            ChangeKind::Upgraded,
            ChangeKind::Downgraded,
            ChangeKind::Changed,
            ChangeKind::Resized,
        ] {
            assert!(kinds.contains(&kind), "no {kind:?} change");
        }
        let text = diff_text("bigmem", &diff, NO_COLOR);
        check(&text, None);
        insta::assert_snapshot!("diff_every_kind", text);
    }

    #[test]
    fn diff_colors() {
        let diff = every_kind();
        let colored = diff_text("bigmem", &diff, COLOR);
        assert_eq!(strip_sgr(&colored), diff_text("bigmem", &diff, NO_COLOR));
        for painted in [
            "\x1b[1mbigmem\x1b[0m",
            "\x1b[31m(+363.6 MiB)\x1b[0m",
            "\x1b[32madded\x1b[0m",
            "\x1b[32mupgraded\x1b[0m",
            "\x1b[31mremoved\x1b[0m",
            "\x1b[31mdowngraded\x1b[0m",
            "\x1b[2mresized\x1b[0m",
            "\x1b[31m+331.9 MiB\x1b[0m",
            "\x1b[32m-70.0 MiB\x1b[0m",
        ] {
            assert!(
                colored.contains(painted),
                "{painted:?} missing in\n{colored}"
            );
        }
        assert!(
            colored.contains("  changed\n"),
            "changes are unstyled\n{colored}"
        );
        insta::assert_snapshot!("diff_color", visible_escapes(&colored));
    }

    #[test]
    fn diff_long_names_dont_spread_every_line() {
        let long = "python3.13-azure-mgmt-recoveryservicesbackup-passivestamp-extension";
        let diff = closure_diff(
            vec![
                change(
                    "firefox",
                    &["131.0"],
                    &["132.0"],
                    (Some(250 * MIB), Some(252 * MIB)),
                ),
                change(
                    "nixos-system-web1",
                    &["25.11.20251201.1a2b3c4"],
                    &["25.11.20251215.42ea902"],
                    (Some(KIB), Some(KIB)),
                ),
                change(long, &["1.0.0b1"], &["1.0.0b2"], (Some(MIB), Some(2 * MIB))),
                change(
                    "unit-script-nixos-activation-start",
                    &[],
                    &[""],
                    (None, Some(KIB)),
                ),
                change("zenoh", &[], &["1.10.0"], (None, Some(348_055_896))),
            ],
            (2_041, 2_043),
            (9 * GIB, 9 * GIB + 350 * MIB),
        );
        let text = diff_text("web1", &diff, NO_COLOR);
        check(&text, None);
        insta::assert_snapshot!("diff_long_names", text);
        let kind_column = |name: &str| {
            let line = text.lines().find(|l| l.contains(name)).unwrap();
            line.rfind("  ").map(|i| line[..i].width())
        };
        assert_eq!(kind_column("firefox"), kind_column("zenoh"), "{text}");
        assert_eq!(kind_column("firefox"), kind_column("unit-script"), "{text}");
        assert!(text.contains(long), "long names are never cut\n{text}");
    }

    /// The real home-manager closures of tests/fixtures/closure-hm-1.tsv and
    /// closure-hm-9.tsv (sizes summed per name), in both directions.
    fn fixture_diff(forward: bool) -> ClosureDiff {
        let mut changes = vec![
            change("atuin", &[], &[""], (Some(74_175_600), Some(74_175_776))),
            change("atuin-server.service", &[], &[""], (None, Some(920))),
            change("hm_tmuxwindownaming", &[], &[""], (None, Some(1_848))),
            change("home-manager", &[], &[], (Some(4_289_336), Some(4_300_392))),
            change(
                "zenoh",
                &[],
                &["1.10.0", "1.10.0-fish"],
                (None, Some(348_055_896)),
            ),
            change(
                "zenoh-bridge-remote-api",
                &[],
                &["1.10.1", "1.10.1-fish"],
                (None, Some(33_140_288)),
            ),
        ];
        let (mut paths, mut sizes) = ((695, 702), (7_474_800_208, 7_856_012_864));
        if !forward {
            for c in &mut changes {
                std::mem::swap(&mut c.removed, &mut c.added);
                std::mem::swap(&mut c.in_before, &mut c.in_after);
                std::mem::swap(&mut c.size_before, &mut c.size_after);
            }
            paths = (paths.1, paths.0);
            sizes = (sizes.1, sizes.0);
        }
        closure_diff(changes, paths, sizes)
    }

    fn fixture(name: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    /// Every line of `nix store diff-closures` (a fixture) is in our text:
    /// same versions and, when Nix shows one, the same size change.
    fn assert_matches_nix(text: &str, golden: &str) {
        let nix = fixture(golden);
        assert_eq!(text.lines().count(), nix.lines().count() + 1, "{text}");
        for line in nix.lines() {
            let (name, rest) = line.split_once(": ").unwrap();
            let is_size = |s: &str| s.ends_with("iB") || s.ends_with(" B");
            let (versions, size) = match rest.rsplit_once(", ") {
                Some((versions, size)) if is_size(size) => (versions, Some(size)),
                _ if is_size(rest) => ("", Some(rest)),
                _ => (rest, None),
            };
            let ours = text
                .lines()
                .skip(1)
                .find(|l| l.split("  ").map(str::trim).find(|c| !c.is_empty()) == Some(name))
                .unwrap_or_else(|| panic!("{name} missing in\n{text}"));
            let cells: Vec<&str> = ours
                .split("  ")
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .collect();
            if !versions.is_empty() {
                assert_eq!(cells[1], versions, "{ours}");
            }
            match size {
                Some(size) => {
                    let signed = if size.starts_with('-') {
                        size.to_string()
                    } else {
                        format!("+{size}")
                    };
                    assert!(cells.contains(&signed.as_str()), "{signed} in {ours:?}");
                }
                None => assert_eq!(cells.len(), 3, "no size: {ours:?}"),
            }
        }
    }

    #[test]
    fn diff_of_real_generations_matches_nix() {
        let forward = diff_text("bigmem", &fixture_diff(true), NO_COLOR);
        assert_matches_nix(&forward, "diff-closures-1-9.txt");
        insta::assert_snapshot!("diff_fixture_1_9", forward);
        let backward = diff_text("bigmem", &fixture_diff(false), NO_COLOR);
        assert_matches_nix(&backward, "diff-closures-9-1.txt");
        insta::assert_snapshot!("diff_fixture_9_1", backward);
    }

    /// End to end: real closures through `closure::diff`, against what
    /// `nix store diff-closures` printed for every pair of generations.
    #[test]
    fn diff_text_agrees_with_nix_on_every_real_pair() {
        let closure = |generation: u32| -> Vec<crate::closure::ClosureEntry> {
            fixture(&format!("closure-hm-{generation}.tsv"))
                .lines()
                .map(|line| {
                    let (path, size) = line.split_once('\t').unwrap();
                    crate::closure::ClosureEntry {
                        path: sp(path),
                        size: size.parse().unwrap(),
                    }
                })
                .collect()
        };
        for (old, new) in [(1, 9), (5, 1), (5, 9), (8, 1), (8, 9), (9, 1)] {
            let diff = crate::closure::diff(&closure(old), &closure(new));
            let text = diff_text("bigmem", &diff, NO_COLOR);
            check(&text, None);
            assert_matches_nix(&text, &format!("diff-closures-{old}-{new}.txt"));
        }
        let real = crate::closure::diff(&closure(1), &closure(9));
        assert_eq!(real, fixture_diff(true), "the snapshots' data is real");
    }

    #[test]
    fn diff_report_json() {
        let diff = closure_diff(
            vec![change("zenoh", &[], &["1.10.0"], (None, Some(348_055_896)))],
            (695, 696),
            (7_474_800_208, 7_822_856_104),
        );
        let report = DiffReport {
            version: 1,
            host: "bigmem",
            diff: &diff,
        };
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            serde_json::json!({
                "version": 1,
                "host": "bigmem",
                "diff": {
                    "changes": [{
                        "name": "zenoh", "removed": [], "added": ["1.10.0"],
                        "inBefore": false, "inAfter": true, "sizeBefore": 0, "sizeAfter": 348_055_896
                    }],
                    "pathsBefore": 695, "pathsAfter": 696,
                    "sizeBefore": 7_474_800_208u64, "sizeAfter": 7_822_856_104u64
                }
            })
        );
    }

    #[test]
    fn diff_sanitizes_names() {
        let diff = closure_diff(
            vec![change(
                "x\u{1b}[2J",
                &["1\u{7}"],
                &["2"],
                (Some(MIB), Some(2 * MIB)),
            )],
            (1, 1),
            (MIB, 2 * MIB),
        );
        let text = diff_text("evil\u{1b}]0;title\u{7}", &diff, NO_COLOR);
        check(&text, None);
        assert!(text.starts_with("evil\\u{1b}]0;title\\u{7}: "), "{text}");
        assert!(text.contains("x\\u{1b}[2J  1\\u{7} → 2"), "{text}");
        check(&strip_sgr(&diff_text("evil\u{1b}", &diff, COLOR)), None);
    }

    fn fleet() -> (Vec<Host>, Vec<String>) {
        let mut desk = hm("al2-x86_64");
        desk.target = ssh("devhost-devuser1-1a-8d41e07a.dc-west1.example.com");
        desk.sources = Sources {
            discovered: true,
            inventory: false,
            configured: true,
        };
        let mut inventory = host("web1", Kind::Nixos);
        inventory.target = ssh("root@web1.example.com");
        inventory.sources = Sources {
            discovered: true,
            inventory: true,
            configured: false,
        };
        let mut work = hm("work-desk");
        work.flake = "github:jjant/work-config".into();
        work.sources = Sources {
            discovered: false,
            inventory: false,
            configured: true,
        };
        let mut custom = host("printer", Kind::Custom);
        custom.attr = "packages.aarch64-linux.printer-image".into();
        custom.target = ssh("ssh://pi@[fe80::1]:2222");
        custom.sources = Sources {
            discovered: false,
            inventory: true,
            configured: false,
        };
        let mut dirty = hm("scratch");
        dirty.flake = "path:/home/jj/scratch".into();
        let hosts = vec![
            hm("al2-aarch64"),
            desk,
            local(hm("bigmem"), false),
            mac(),
            local(host("mac-mini", Kind::Darwin), true),
            custom,
            dirty,
            inventory,
            work,
        ];
        let warnings = vec![
            "\"bigmem\" is both a nixosConfigurations and a homeConfigurations entry; using nixos"
                .into(),
            "exclude pattern \"*-iso\" matches no host".into(),
        ];
        (hosts, warnings)
    }

    #[test]
    fn hosts_table_snapshot() {
        let (hosts, warnings) = fleet();
        let text = hosts_table(&hosts, &warnings, NO_COLOR);
        check(&text, None);
        insta::assert_snapshot!("hosts", text);
        let colored = hosts_table(&hosts, &warnings, COLOR);
        assert_eq!(strip_sgr(&colored), text);
        assert!(
            colored.contains("\x1b[33mwarning:\x1b[0m exclude pattern"),
            "{colored}"
        );
        assert!(
            colored.contains("\x1b[2m (probe: current-system)\x1b[0m"),
            "{colored}"
        );
    }

    #[test]
    fn hosts_table_shows_the_flake_only_when_it_differs() {
        let hosts = vec![hm("a"), local(hm("bigmem"), false)];
        let text = hosts_table(&hosts, &[], NO_COLOR);
        check(&text, None);
        assert_eq!(
            text,
            "\
a       home-manager  a      discovered  homeConfigurations.a.activationPackage
bigmem  home-manager  local  discovered  homeConfigurations.bigmem.activationPackage
"
        );
        // Ties go to the first flake.
        let mut other = hm("b");
        other.flake = "github:x/y".into();
        let text = hosts_table(&[hm("a"), other], &[], NO_COLOR);
        assert!(
            text.lines().nth(1).unwrap().contains("github:x/y"),
            "{text}"
        );
        assert!(!text.lines().next().unwrap().contains(GITHUB), "{text}");
        assert_eq!(most_common_flake(&[]), None);
    }

    #[test]
    fn hosts_table_without_hosts() {
        assert_eq!(hosts_table(&[], &[], NO_COLOR), "");
        assert_eq!(
            hosts_table(&[], &["no hosts\nat all".into()], NO_COLOR),
            "warning: no hosts at all\n"
        );
        let mut nowhere = hm("ghost");
        nowhere.sources = Sources::default();
        nowhere.name = "gh\u{1b}[8most".into();
        let text = hosts_table(&[nowhere], &[], NO_COLOR);
        check(&text, None);
        assert_eq!(
            text,
            "gh\\u{1b}[8most  home-manager  ghost    homeConfigurations.ghost.activationPackage\n"
        );
    }
}
