//! `hostctl status` on a terminal: run the check while redrawing the plain
//! status table in place, so the final screen is exactly `--plain`'s output.
//!
//! Also the small terminal helpers the other front-ends share: the signals
//! hostctl handles, a spinner for waits, the terminal size.

use std::fmt::Write as _;
use std::fs::File;
use std::future::Future;
use std::io::{self, Write as _};
use std::os::fd::{AsRawFd as _, RawFd};
use std::time::{Duration, Instant};

use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio::time::MissedTickBehavior;
use unicode_width::UnicodeWidthChar as _;

use crate::config::Host;
use crate::engine::Engine;
use crate::model::{AppState, Event, HostId, Status};
use crate::nix::FlakeInfo;
use crate::output::{self, Style};
use crate::tui::view::SPINNER;
use crate::util;

/// How often the screen is brought up to date while a check runs (the
/// spinner's frame rate; the table's timers tick by the second).
const TICK: Duration = Duration::from_millis(80);

/// A wait shorter than this shows no spinner.
const SPINNER_DELAY: Duration = Duration::from_millis(250);

/// Run a check of `hosts`, redrawing [`crate::output::status_table`] in
/// place on stdout (move up, rewrite each row, clear what is left) on each
/// burst of events and on an 80 ms spinner tick, while the table fits the
/// terminal; when it doesn't, draw a one-line progress on stderr instead
/// (`⠋ 3/5 · 2 evaluating · 2.1s`) and print the table once at the end.
/// Ctrl-C applies [`crate::model::Event::Interrupted`], flushes the cache
/// and returns the partial state. The cursor is hidden while drawing and
/// always restored.
pub async fn status(
    engine: &Engine,
    state: AppState,
    hosts: &[(HostId, Host)],
    known: &[FlakeInfo],
    style: Style,
) -> anyhow::Result<AppState> {
    let mut signals = Signals::new()?;
    check(
        engine,
        state,
        hosts,
        known,
        Display::Table(style),
        &mut signals,
        Instant::now(),
    )
    .await
}

/// How [`check`] shows a check in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Display {
    /// The status table redrawn in place on stdout, ending with the final
    /// table (what [`status`] does).
    Table(Style),
    /// Only the one-line progress on stderr, when it is a terminal; the
    /// caller prints the outcome.
    Progress,
}

/// Run a check of `hosts` shown with `display` until it completes or one
/// of `signals` arrives; then [`Event::Interrupted`] is applied, the cache
/// flushed and the partial state returned (its exit code is 130). The run's
/// duration counts from `started` (when hostctl started, say).
pub(crate) async fn check(
    engine: &Engine,
    state: AppState,
    hosts: &[(HostId, Host)],
    known: &[FlakeInfo],
    display: Display,
    signals: &mut Signals,
    started: Instant,
) -> anyhow::Result<AppState> {
    let same = std::future::ready(Ok::<_, std::convert::Infallible>(Confirmed::Same));
    let checked = check_confirming(engine, state, hosts, known, display, signals, started, same);
    match checked.await? {
        Ok(state) => Ok(state),
        Err(never) => match never {},
    }
}

/// What finding the hosts again ([`check_confirming`]) says of those that
/// a check started with.
pub(crate) enum Confirmed {
    /// They are the hosts.
    Same,
    /// These are, and the check starts over with them.
    Changed(Box<Recheck>),
}

/// A check to start over with ([`Confirmed::Changed`]).
pub(crate) struct Recheck {
    pub(crate) state: AppState,
    pub(crate) hosts: Vec<(HostId, Host)>,
    pub(crate) known: Vec<FlakeInfo>,
}

/// [`check`] of `hosts` that may not be the hosts any more (the cache's,
/// as of the last time the flake's were read), while `confirmed` finds out:
/// the same ones, and the check goes on; others, and it starts over with
/// those, its table redrawn in place; or an error, and the table goes
/// (`Ok(Err(…))`). The outcome waits for `confirmed`, also when the check
/// is done first.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn check_confirming<E>(
    engine: &Engine,
    mut state: AppState,
    hosts: &[(HostId, Host)],
    known: &[FlakeInfo],
    display: Display,
    signals: &mut Signals,
    started: Instant,
    confirmed: impl Future<Output = Result<Confirmed, E>>,
) -> anyhow::Result<Result<AppState, E>> {
    let (mut hosts, mut known) = (hosts.to_vec(), known.to_vec());
    let mut confirmed = Some(Box::pin(confirmed));
    let mut term = Stdio::detect();
    let table = matches!(display, Display::Table(_)) && term.out_size().is_some();
    let progress = !table && term.err_size().is_some();
    if table || progress {
        term.quiet(!table);
    }
    let mut screen = Screen::new(display, started);
    // The engine saves the cache when a check completes.
    let mut completed = false;
    let outcome: io::Result<Result<(), E>> = loop {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let work = checked(engine.check(&hosts, &known, &tx), confirmed.take());
        let ended = drive(&mut term, &mut screen, work, &mut rx, signals, &mut state).await;
        match ended {
            Ok(Some(Checked::Changed(recheck))) => {
                Recheck {
                    state,
                    hosts,
                    known,
                } = *recheck;
            }
            Ok(Some(Checked::Done)) => {
                completed = true;
                break screen.finish(&mut term, &state).map(Ok);
            }
            Ok(Some(Checked::Failed(e))) => break screen.abandon(&mut term).map(|()| Err(e)),
            Ok(None) => {
                // The terminal may be gone (SIGHUP): the state is what matters.
                let _ = screen.finish(&mut term, &state);
                break Ok(Ok(()));
            }
            Err(e) => break Err(e),
        }
    };
    drop(term);
    if !completed {
        save_cache(engine).await;
    }
    Ok(outcome?.map(|()| state))
}

/// What a check came to ([`checked`]).
enum Checked<E> {
    /// It is done, and its hosts were the hosts.
    Done,
    Changed(Box<Recheck>),
    Failed(E),
}

/// `check`, and what `confirmed` says of its hosts if that is still to
/// come: done once both are, unless `confirmed` says other hosts (at once,
/// and then the check is dropped) or fails.
async fn checked<E>(
    check: impl Future<Output = ()>,
    confirmed: Option<std::pin::Pin<Box<impl Future<Output = Result<Confirmed, E>>>>>,
) -> Checked<E> {
    let mut check = std::pin::pin!(check);
    let Some(mut confirmed) = confirmed else {
        check.await;
        return Checked::Done;
    };
    let mut done = false;
    loop {
        tokio::select! {
            () = &mut check, if !done => done = true,
            confirmed = &mut confirmed => return match confirmed {
                Ok(Confirmed::Same) => {
                    if !done {
                        check.await;
                    }
                    Checked::Done
                }
                Ok(Confirmed::Changed(recheck)) => Checked::Changed(recheck),
                Err(e) => Checked::Failed(e),
            },
        }
    }
}

/// [`Engine::save_cache`] off the async thread (it takes a file lock).
pub(crate) async fn save_cache(engine: &Engine) {
    let engine = engine.clone();
    let _ = tokio::task::spawn_blocking(move || engine.save_cache()).await;
}

/// SIGINT, SIGQUIT, SIGTERM, SIGHUP and SIGTSTP as one stream. Creating it
/// installs hostctl's handlers (for the rest of the process' life), so the
/// first one must exist before any [`crate::exec::run_interactive`].
pub(crate) struct Signals {
    interrupt: Signal,
    quit: Signal,
    terminate: Signal,
    hangup: Signal,
    suspend: Signal,
}

impl Signals {
    /// Needs a tokio runtime (with I/O enabled) to be entered.
    pub(crate) fn new() -> io::Result<Signals> {
        Ok(Signals {
            interrupt: signal(SignalKind::interrupt())?,
            quit: signal(SignalKind::quit())?,
            terminate: signal(SignalKind::terminate())?,
            hangup: signal(SignalKind::hangup())?,
            suspend: signal(SignalKind::from_raw(libc::SIGTSTP))?,
        })
    }

    /// The next of them but SIGTSTP (pending forever once the runtime shuts
    /// down), which [`stop`]s hostctl as its default action would: for
    /// waits with nothing of hostctl's on the terminal to give back.
    /// SIGQUIT (Ctrl-\) is an interrupt like SIGINT: its default action
    /// would dump core, with the terminal as hostctl left it and the
    /// children still running.
    pub(crate) async fn recv(&mut self) -> Sig {
        loop {
            match self.recv_all().await {
                Received::Suspend => stop(),
                Received::Sig(sig) => return sig,
            }
        }
    }

    /// The next of them, SIGTSTP ([`Received::Suspend`]) included: whoever
    /// gets that puts the terminal back as it was, [`stop`]s, and takes it
    /// again.
    pub(crate) async fn recv_all(&mut self) -> Received {
        tokio::select! {
            Some(()) = self.interrupt.recv() => Received::Sig(Sig::Interrupt),
            Some(()) = self.quit.recv() => Received::Sig(Sig::Interrupt),
            Some(()) = self.terminate.recv() => Received::Sig(Sig::Terminate),
            Some(()) = self.hangup.recv() => Received::Sig(Sig::Hangup),
            Some(()) = self.suspend.recv() => Received::Suspend,
            else => std::future::pending().await,
        }
    }
}

/// Which of [`Signals`] arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sig {
    /// SIGINT (Ctrl-C) or SIGQUIT (Ctrl-\).
    Interrupt,
    /// SIGTERM.
    Terminate,
    /// SIGHUP (the terminal went away).
    Hangup,
}

/// What [`Signals::recv_all`] got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Received {
    Sig(Sig),
    /// SIGTSTP (Ctrl-Z): stop until continued (`fg`).
    Suspend,
}

/// Stop hostctl the way SIGTSTP's default action does (so the shell reports
/// a suspended job; nothing happens in an orphaned process group), until
/// it is continued. hostctl's own handler is back afterwards.
pub(crate) fn stop() {
    // SAFETY: all-zero is a valid sigaction (SIG_DFL, empty mask, no flags);
    // sigaction and raise are plain calls with valid arguments.
    unsafe {
        let default: libc::sigaction = std::mem::zeroed();
        let mut handler: libc::sigaction = std::mem::zeroed();
        if libc::sigaction(libc::SIGTSTP, &default, &mut handler) != 0 {
            return;
        }
        libc::raise(libc::SIGTSTP);
        libc::sigaction(libc::SIGTSTP, &handler, std::ptr::null_mut());
    }
}

/// Await `work`, showing `label` and a spinner on stderr if it takes more
/// than a moment (and stderr is a terminal). `None` when one of `signals`
/// arrived first; `work` is then dropped, which cancels it. Ctrl-Z takes
/// the spinner off the line while hostctl is stopped.
pub(crate) async fn wait_for<F: Future>(
    label: &str,
    work: F,
    signals: &mut Signals,
) -> Option<F::Output> {
    let spin = drawable(&io::stderr());
    let interrupt = async {
        loop {
            match signals.recv_all().await {
                Received::Suspend => {
                    if spin {
                        let _ = write_err("\r\x1b[K");
                    }
                    stop();
                }
                _ => return,
            }
        }
    };
    wait_until(label, work, interrupt, spin).await
}

/// [`wait_for`] with any `interrupt`; the spinner only with `spin`.
async fn wait_until<F: Future>(
    label: &str,
    work: F,
    interrupt: impl Future<Output = ()>,
    spin: bool,
) -> Option<F::Output> {
    let mut work = std::pin::pin!(work);
    let mut interrupt = std::pin::pin!(interrupt);
    let start = Instant::now();
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + SPINNER_DELAY, TICK);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut frame = 0;
    let mut shown = false;
    let output = loop {
        tokio::select! {
            biased;
            () = &mut interrupt => break None,
            output = &mut work => break Some(output),
            _ = ticker.tick(), if spin => {
                let cols = terminal_size(libc::STDERR_FILENO).map_or(80, |(cols, _)| cols);
                let _ = write_err(&spinner_line(label, frame, start.elapsed(), cols));
                shown = true;
                frame += 1;
            }
        }
    };
    if shown {
        let _ = write_err("\r\x1b[K");
    }
    output
}

/// What [`wait_until`] writes for spinner `frame` after `elapsed`:
/// `⠋ <label> 2.1s` over the current line, on one line of `cols` columns
/// (the label may come from a flake: it is sanitized and kept on one line).
fn spinner_line(label: &str, frame: usize, elapsed: Duration, cols: usize) -> String {
    let line = format!(
        "{} {} {}",
        SPINNER[frame % SPINNER.len()],
        util::one_line(label),
        seconds(elapsed)
    );
    format!("\r\x1b[K{}", util::truncate(&line, cols.saturating_sub(1)))
}

/// The size (columns, rows) of the terminal `fd` refers to, if it is one.
pub(crate) fn terminal_size(fd: RawFd) -> Option<(usize, usize)> {
    // SAFETY: all-zero is a valid winsize, which TIOCGWINSZ only writes to.
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: a valid request with a pointer to a live winsize.
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) };
    (rc == 0 && size.ws_col > 0 && size.ws_row > 0)
        .then(|| (usize::from(size.ws_col), usize::from(size.ws_row)))
}

/// `2.1s` (tenths keep a short wait visibly moving), `1m05s` past a minute.
pub(crate) fn seconds(d: Duration) -> String {
    if d.as_secs() < 60 {
        format!("{:.1}s", d.as_secs_f64())
    } else {
        util::format_elapsed(d)
    }
}

fn write_err(text: &str) -> io::Result<()> {
    let mut err = io::stderr().lock();
    err.write_all(text.as_bytes())?;
    err.flush()
}

fn write_out(text: &str) -> io::Result<()> {
    let mut out = io::stdout().lock();
    out.write_all(text.as_bytes())?;
    out.flush()
}

/// What [`drive`] draws on: the process' stdout and stderr, or a fake.
trait Term {
    /// stdout's terminal size (columns, rows); `None` unless it is a
    /// terminal.
    fn out_size(&self) -> Option<(usize, usize)>;
    /// stderr's terminal size; `None` unless it is a terminal (and then no
    /// progress line is shown).
    fn err_size(&self) -> Option<(usize, usize)>;
    fn out(&mut self, text: &str) -> io::Result<()>;
    fn err(&mut self, text: &str) -> io::Result<()>;
    /// Ctrl-Z: give the terminal back as it was (echo, cursor), [`stop`],
    /// and once continued take it again.
    fn suspend(&mut self);
}

/// Where [`drive`] gets its signals: [`Signals`], or a script.
trait SignalSource {
    /// The next one, SIGTSTP included ([`Signals::recv_all`]).
    async fn recv_all(&mut self) -> Received;
}

impl SignalSource for Signals {
    async fn recv_all(&mut self) -> Received {
        Signals::recv_all(self).await
    }
}

/// The real stdout and stderr.
struct Stdio {
    out_tty: bool,
    err_tty: bool,
    /// While drawing: echo off, and the cursor hidden on stdout (or on
    /// stderr, `true`).
    quiet: Option<(Option<EchoOff>, CursorHidden)>,
}

impl Stdio {
    fn detect() -> Stdio {
        Stdio {
            out_tty: drawable(&io::stdout()),
            err_tty: drawable(&io::stderr()),
            quiet: None,
        }
    }

    /// Echo off (typed keys would move the cursor under lines redrawn in
    /// place) and the cursor hidden on stdout, or with `stderr` on stderr,
    /// until this is dropped.
    fn quiet(&mut self, stderr: bool) {
        self.quiet = Some((EchoOff::new(), CursorHidden::new(stderr)));
    }
}

/// Whether lines can be redrawn on `stream`: a terminal that isn't
/// `TERM=dumb` (Emacs' shell, for one, can't move the cursor).
pub(crate) fn drawable(stream: &impl io::IsTerminal) -> bool {
    drawable_with(stream.is_terminal(), std::env::var_os("TERM").as_deref())
}

fn drawable_with(terminal: bool, term: Option<&std::ffi::OsStr>) -> bool {
    terminal && term.is_none_or(|term| term != "dumb")
}

impl Term for Stdio {
    fn out_size(&self) -> Option<(usize, usize)> {
        self.out_tty
            .then(|| terminal_size(libc::STDOUT_FILENO))
            .flatten()
    }

    fn err_size(&self) -> Option<(usize, usize)> {
        self.err_tty
            .then(|| terminal_size(libc::STDERR_FILENO))
            .flatten()
    }

    fn out(&mut self, text: &str) -> io::Result<()> {
        write_out(text)
    }

    fn err(&mut self, text: &str) -> io::Result<()> {
        write_err(text)
    }

    fn suspend(&mut self) {
        // Dropping them puts back echo and the cursor.
        let stderr = self.quiet.take().map(|(_, cursor)| cursor.stderr);
        stop();
        // Continued: the settings the terminal has now are the ones to keep.
        if let Some(stderr) = stderr {
            self.quiet(stderr);
        }
    }
}

/// Run `work` (a check, whose events arrive on `rx`) to completion — `Some`
/// of what it came to — or until one of `signals` — `None`, once the
/// events already sent and [`Event::Interrupted`] are applied — keeping
/// `term` up to date with `state` on `screen` (whose
/// [`Screen::finish`] is the caller's). A check's start counts from
/// `screen`'s. Ctrl-Z ([`Received::Suspend`]) takes the progress line off,
/// gives the terminal back and stops; once continued, the next frame is
/// drawn afresh where the cursor is (the shell wrote below the last one).
async fn drive<T: Term, W: Future>(
    term: &mut T,
    screen: &mut Screen,
    work: W,
    rx: &mut UnboundedReceiver<Event>,
    signals: &mut impl SignalSource,
    state: &mut AppState,
) -> io::Result<Option<W::Output>> {
    let mut work = std::pin::pin!(work);
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut outcome = None;
    let ended = loop {
        tokio::select! {
            biased;
            sig = signals.recv_all() => match sig {
                Received::Suspend => {
                    screen.suspend(term);
                    term.suspend();
                }
                _ => break None,
            },
            done = &mut work, if outcome.is_none() => outcome = Some(done),
            Some(event) = rx.recv() => apply(state, event, screen.start),
            _ = ticker.tick() => screen.frame += 1,
        }
        // One redraw per burst of events.
        drain(rx, state, screen.start);
        if let Some(done) = outcome.take() {
            break Some(done);
        }
        screen.update(term, state)?;
    };
    drain(rx, state, screen.start);
    if ended.is_none() {
        state.apply(Event::Interrupted, Instant::now());
    }
    Ok(ended)
}

/// Apply `event` to `state` now; a check's start counts from `started`
/// (when hostctl did), so that its duration covers what came before it.
fn apply(state: &mut AppState, event: Event, started: Instant) {
    let now = Instant::now();
    match event {
        Event::RunStarted => state.apply(Event::RunStarted, started.min(now)),
        event => state.apply(event, now),
    }
}

fn drain(rx: &mut UnboundedReceiver<Event>, state: &mut AppState, started: Instant) {
    while let Ok(event) = rx.try_recv() {
        apply(state, event, started);
    }
}

/// What is on the terminal.
struct Screen {
    display: Display,
    /// When it all started (the progress line's clock, and the check's).
    start: Instant,
    /// Spinner frame.
    frame: usize,
    /// The table drawn in place on stdout (the cursor is right below it)
    /// and the stdout size it was drawn at.
    drawn: Option<(String, (usize, usize))>,
    /// The progress line on stderr.
    progress: Option<String>,
}

impl Screen {
    fn new(display: Display, start: Instant) -> Screen {
        Screen {
            display,
            start,
            frame: 0,
            drawn: None,
            progress: None,
        }
    }

    /// Take everything off the screen: a check that won't finish.
    fn abandon(&mut self, term: &mut impl Term) -> io::Result<()> {
        self.clear_progress(term);
        if let Some((cols, _)) = term.out_size() {
            self.erase(term, cols)?;
        }
        self.drawn = None;
        Ok(())
    }

    fn update(&mut self, term: &mut impl Term, state: &AppState) -> io::Result<()> {
        if let (Display::Table(style), Some(size)) = (self.display, term.out_size()) {
            let table = output::status_table(state, style, Some(size.0), util::now_unix());
            if self
                .drawn
                .as_ref()
                .is_some_and(|(drawn, at)| *drawn == table && *at == size)
            {
                return Ok(());
            }
            if let Some(bytes) = redraw(self.drawn_rows(size.0), &table, size) {
                self.clear_progress(term);
                term.out(&bytes)?;
                self.drawn = Some((table, size));
                return Ok(());
            }
            // Too tall to come back to: show the progress line instead.
            self.erase(term, size.0)?;
        }
        self.show_progress(term, state);
        Ok(())
    }

    /// The final screen: the table once more ([`Display::Table`]), exactly
    /// as `--plain` prints it — not cut to the terminal's width like the
    /// frames before it (its long lines wrap) — and the progress line gone.
    fn finish(&mut self, term: &mut impl Term, state: &AppState) -> io::Result<()> {
        self.clear_progress(term);
        let Display::Table(style) = self.display else {
            return Ok(());
        };
        let size = term.out_size();
        let table = output::status_table(state, style, None, util::now_unix());
        let unchanged = self
            .drawn
            .as_ref()
            .is_some_and(|(drawn, at)| *drawn == table && Some(*at) == size);
        if !unchanged {
            if let Some((cols, _)) = size {
                self.erase(term, cols)?;
            }
            term.out(&table)?;
        }
        self.drawn = None;
        Ok(())
    }

    /// Before stopping (Ctrl-Z): the progress line goes; the table stays
    /// as it is, but isn't redrawn over any more, since the shell writes
    /// below it: the next frame starts afresh where the cursor is then.
    fn suspend(&mut self, term: &mut impl Term) {
        self.clear_progress(term);
        self.drawn = None;
    }

    /// Rows the drawn table takes at `cols` columns (it may have been
    /// drawn at another width).
    fn drawn_rows(&self, cols: usize) -> usize {
        self.drawn
            .as_ref()
            .map_or(0, |(table, _)| rows_of(table, cols))
    }

    /// Remove the drawn table, leaving the cursor where it started.
    fn erase(&mut self, term: &mut impl Term, cols: usize) -> io::Result<()> {
        if self.drawn.is_some() {
            term.out(&erase(self.drawn_rows(cols)))?;
            self.drawn = None;
        }
        Ok(())
    }

    fn show_progress(&mut self, term: &mut impl Term, state: &AppState) {
        let Some((cols, _)) = term.err_size() else {
            return;
        };
        let line = progress_line(state, self.frame, self.start.elapsed());
        let line = util::truncate(&line, cols.saturating_sub(1)).into_owned();
        if self.progress.as_deref() == Some(line.as_str()) {
            return;
        }
        // A broken stderr only costs the progress line.
        let _ = term.err(&format!("\r\x1b[K{line}"));
        self.progress = Some(line);
    }

    fn clear_progress(&mut self, term: &mut impl Term) {
        if self.progress.take().is_some() {
            let _ = term.err("\r\x1b[K");
        }
    }
}

/// The bytes that replace a table drawn on the `prev_rows` rows right
/// above the cursor (0: nothing drawn yet) with `table`, on a terminal of
/// `size` (columns, rows): back to the first column, up `prev_rows`, each
/// line of `table` over its cleared row (`CSI K`; frames are cut to the
/// width, so a line is a row), then the rows of the previous table left
/// over cleared one by one. `None` when `table` doesn't fit, i.e. needs as
/// many rows as the terminal has: the cursor (on the row below it) could
/// no longer get back to its top.
///
/// Never "erase below" (`CSI J`): in tmux (`scroll-on-clear`, on by
/// default) that moves the whole screen into the scrollback when the
/// cursor is at the top-left corner, where the table starts when hostctl
/// runs in a pane of its own or when it fills the screen.
fn redraw(prev_rows: usize, table: &str, (cols, rows): (usize, usize)) -> Option<String> {
    let new_rows = rows_of(table, cols);
    if new_rows >= rows {
        return None;
    }
    let mut out = String::from("\r");
    up(&mut out, prev_rows);
    for line in table.lines() {
        out.push_str("\x1b[K");
        out.push_str(line);
        out.push('\n');
    }
    clear_rows(&mut out, prev_rows.saturating_sub(new_rows));
    Some(out)
}

/// Clear the `rows` rows above the cursor, leaving it at the start of the
/// first of them (without `CSI J`: see [`redraw`]).
fn erase(rows: usize) -> String {
    let mut out = String::from("\r");
    up(&mut out, rows);
    clear_rows(&mut out, rows);
    out
}

/// Clear `rows` rows from the cursor's on down, one by one (`CSI K`, then
/// `CSI B`, which never scrolls), and come back up.
fn clear_rows(out: &mut String, rows: usize) {
    for _ in 0..rows {
        out.push_str("\x1b[K\x1b[B");
    }
    up(out, rows);
}

/// Move up `rows` rows (`CSI 0 A` moves up one on most terminals: never
/// emitted).
fn up(out: &mut String, rows: usize) {
    if rows > 0 {
        let _ = write!(out, "\x1b[{rows}A");
    }
}

/// Terminal rows `text` takes at `cols` columns: one per line, more for a
/// line wider than the terminal (wrapped, as terminals that reflow on
/// resize show it).
fn rows_of(text: &str, cols: usize) -> usize {
    let cols = cols.max(1);
    text.lines()
        .map(|line| visible_width(line).div_ceil(cols).max(1))
        .sum()
}

/// Display width of `line` without its escape sequences (the tables'
/// colors).
fn visible_width(line: &str) -> usize {
    let mut width = 0;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.next() == Some('[') {
                // Parameters up to the final byte (`@` to `~`).
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
        } else {
            width += c.width().unwrap_or(0);
        }
    }
    width
}

/// `⠋ 3/5 · 1 resolving · 2 evaluating · 2.1s`: hosts done of all, what
/// the others are doing (flakes resolving, hosts probing, evaluating or
/// waiting for either), time since the check started.
fn progress_line(state: &AppState, frame: usize, elapsed: Duration) -> String {
    let done = state
        .hosts
        .iter()
        .filter(|h| !h.busy() && h.status() != Status::Pending)
        .count();
    let mut line = format!(
        "{} {done}/{}",
        SPINNER[frame % SPINNER.len()],
        state.hosts.len()
    );
    let resolving = state.flakes.iter().filter(|f| f.info.is_running()).count();
    let mut counts = [
        ("resolving", resolving),
        ("probing", 0),
        ("evaluating", 0),
        ("waiting", 0),
    ];
    for h in &state.hosts {
        if let Some(activity) = h.activity()
            && let Some((_, n)) = counts[1..].iter_mut().find(|(label, _)| *label == activity)
        {
            *n += 1;
        }
    }
    for (label, n) in counts {
        if n > 0 {
            let _ = write!(line, " · {n} {label}");
        }
    }
    let _ = write!(line, " · {}", seconds(elapsed));
    line
}

/// The terminal's echo off while this lives, so keys typed during a redraw
/// in place don't move the cursor. Only in the foreground (a background job
/// changing terminal modes would be stopped by SIGTTOU); the previous
/// settings come back on drop.
struct EchoOff {
    tty: File,
    saved: libc::termios,
}

impl EchoOff {
    fn new() -> Option<EchoOff> {
        let tty = File::options()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .ok()?;
        let fd = tty.as_raw_fd();
        // SAFETY: plain calls on an open descriptor.
        let foreground = unsafe { libc::tcgetpgrp(fd) == libc::getpgrp() };
        if !foreground {
            return None;
        }
        // SAFETY: all-zero is a valid termios, which tcgetattr fills in.
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: a valid descriptor and a pointer to a live termios.
        if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
            return None;
        }
        let mut quiet = saved;
        quiet.c_lflag &= !(libc::ECHO | libc::ECHONL);
        // SAFETY: as above; `quiet` is `saved` with two flags cleared.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &quiet) } != 0 {
            return None;
        }
        Some(EchoOff { tty, saved })
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        // SAFETY: the settings read in `new`, on the same descriptor.
        unsafe { libc::tcsetattr(self.tty.as_raw_fd(), libc::TCSANOW, &self.saved) };
    }
}

/// The cursor hidden (on stdout, or stderr) while this lives.
struct CursorHidden {
    stderr: bool,
}

impl CursorHidden {
    fn new(stderr: bool) -> CursorHidden {
        let guard = CursorHidden { stderr };
        guard.write("\x1b[?25l");
        guard
    }

    fn write(&self, code: &str) {
        let _ = if self.stderr {
            write_err(code)
        } else {
            write_out(code)
        };
    }
}

impl Drop for CursorHidden {
    fn drop(&mut self) {
        self.write("\x1b[?25h");
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;
    use crate::config::{ActivateOn, Kind, Sources, SshTarget, Target};
    use crate::model::{Desired, ErrorInfo, ProbeError, ProbeFailure};
    use crate::probe::ProbeReport;
    use crate::storepath::StorePath;

    const FLAKE: &str = "github:jjant/nix-config";

    fn host(name: &str, local: bool) -> Host {
        Host {
            name: name.into(),
            kind: Kind::HomeManager,
            flake: FLAKE.into(),
            attr: format!("homeConfigurations.{name}.activationPackage"),
            target: if local {
                Target::Local { assumed: false }
            } else {
                Target::Ssh(SshTarget {
                    destination: name.into(),
                    user: None,
                    host: name.into(),
                    port: None,
                    options: vec![],
                    forced_command: None,
                })
            },
            profile: None,
            activate: Kind::HomeManager.default_activate().map(str::to_string),
            activate_on: ActivateOn::Target,
            optional: false,
            note: None,
            sources: Sources::default(),
        }
    }

    fn path(c: char) -> StorePath {
        StorePath::parse(&format!(
            "/nix/store/{}-home-manager-generation",
            c.to_string().repeat(32)
        ))
        .unwrap()
    }

    fn info() -> FlakeInfo {
        FlakeInfo {
            flake: FLAKE.into(),
            locked: format!("github:jjant/nix-config/{}", "4".repeat(40)),
            kind: Some("github".into()),
            locked_url: None,
            rev: Some("4".repeat(40)),
            dirty_rev: None,
            last_modified: Some(util::now_unix() - 3 * 3600 - 600),
            rev_count: None,
            fingerprint: None,
            nar_hash: None,
            description: None,
            stale: None,
            lock_outdated: false,
        }
    }

    fn report(c: char) -> ProbeReport {
        ProbeReport {
            current: Some(path(c)),
            generation: Some(7),
            activated: Some(util::now_unix() - 5 * 3600 - 600),
            ..ProbeReport::default()
        }
    }

    fn desired(c: char) -> Desired {
        Desired {
            path: path(c),
            rev: Some("4".repeat(40)),
            last_modified: None,
            cached: false,
        }
    }

    /// A check of `hosts`' worth of events: (delay before it in ms, event).
    fn complete_check(names: &[&str]) -> Vec<(u64, Event)> {
        let mut events = vec![
            (0, Event::RunStarted),
            (0, Event::FlakeStarted { url: FLAKE.into() }),
        ];
        events.extend((0..names.len()).map(|id| (0, Event::ProbeStarted(id))));
        events.push((
            300,
            Event::FlakeFinished {
                url: FLAKE.into(),
                result: Ok(info()),
            },
        ));
        for id in 0..names.len() {
            events.push((40, Event::EvalStarted(id)));
            events.push((
                100,
                Event::ProbeFinished {
                    id,
                    result: Ok(report(if id % 2 == 0 { 'a' } else { 'b' })),
                    deployed: None,
                },
            ));
        }
        for id in 0..names.len() {
            events.push((
                1500,
                Event::EvalFinished {
                    id,
                    result: Ok(desired('a')),
                },
            ));
        }
        events.push((0, Event::RunFinished));
        events
    }

    async fn play(tx: mpsc::UnboundedSender<Event>, events: Vec<(u64, Event)>) {
        for (ms, event) in events {
            if ms > 0 {
                tokio::time::sleep(Duration::from_millis(ms)).await;
            }
            let _ = tx.send(event);
        }
    }

    fn state(names: &[&str]) -> AppState {
        AppState::new(
            names
                .iter()
                .enumerate()
                .map(|(i, name)| host(name, i == 0))
                .collect(),
        )
    }

    /// A terminal whose output is recorded; sizes can change mid-run.
    struct Fake {
        out: String,
        err: String,
        size: Rc<Cell<(usize, usize)>>,
        out_tty: bool,
        err_tty: bool,
        /// What the shell prints while hostctl is stopped (on stdout, the
        /// terminal), and how often that happened.
        shell: String,
        suspended: usize,
    }

    impl Fake {
        fn new(cols: usize, rows: usize) -> Fake {
            Fake {
                out: String::new(),
                err: String::new(),
                size: Rc::new(Cell::new((cols, rows))),
                out_tty: true,
                err_tty: true,
                shell: String::new(),
                suspended: 0,
            }
        }
    }

    impl Term for Fake {
        fn out_size(&self) -> Option<(usize, usize)> {
            self.out_tty.then(|| self.size.get())
        }

        fn err_size(&self) -> Option<(usize, usize)> {
            self.err_tty.then(|| self.size.get())
        }

        fn out(&mut self, text: &str) -> io::Result<()> {
            self.out.push_str(text);
            Ok(())
        }

        fn err(&mut self, text: &str) -> io::Result<()> {
            self.err.push_str(text);
            Ok(())
        }

        fn suspend(&mut self) {
            self.suspended += 1;
            self.out.push_str(&self.shell);
        }
    }

    /// Signals at fixed times (ms after it was made), then none.
    struct Script {
        start: tokio::time::Instant,
        signals: std::collections::VecDeque<(u64, Received)>,
    }

    impl Script {
        fn new(signals: &[(u64, Received)]) -> Script {
            Script {
                start: tokio::time::Instant::now(),
                signals: signals.iter().copied().collect(),
            }
        }
    }

    impl SignalSource for Script {
        async fn recv_all(&mut self) -> Received {
            let Some(&(at, sig)) = self.signals.front() else {
                return std::future::pending().await;
            };
            if at > 0 {
                tokio::time::sleep_until(self.start + Duration::from_millis(at)).await;
            }
            // Only once it is delivered: a wait cut short by another
            // branch of the loop starts over.
            self.signals.pop_front();
            sig
        }
    }

    /// Just enough of a VT100 to check what a user would see: printing with
    /// deferred autowrap, `\r`, `\n` (as the tty's ONLCR makes it), scrolling
    /// into a scrollback, `CSI n A`, `CSI n B`, `CSI J`, `CSI K`, and ignored
    /// SGR / cursor visibility. Optionally tmux's `scroll-on-clear` (on by
    /// default there): `CSI J` at the top-left corner moves the screen, up
    /// to its last used row, into the scrollback.
    struct Vt {
        cols: usize,
        grid: Vec<Vec<char>>,
        scrollback: Vec<String>,
        row: usize,
        col: usize,
        wrap_next: bool,
        scroll_on_clear: bool,
    }

    impl Vt {
        fn new(cols: usize, rows: usize) -> Vt {
            Vt {
                cols,
                grid: vec![vec![' '; cols]; rows],
                scrollback: Vec::new(),
                row: 0,
                col: 0,
                wrap_next: false,
                scroll_on_clear: false,
            }
        }

        /// A tmux pane.
        fn tmux(cols: usize, rows: usize) -> Vt {
            Vt {
                scroll_on_clear: true,
                ..Vt::new(cols, rows)
            }
        }

        fn line_feed(&mut self) {
            self.col = 0;
            self.wrap_next = false;
            if self.row + 1 == self.grid.len() {
                let top = self.grid.remove(0);
                self.scrollback
                    .push(top.iter().collect::<String>().trim_end().to_string());
                self.grid.push(vec![' '; self.cols]);
            } else {
                self.row += 1;
            }
        }

        fn feed(&mut self, text: &str) {
            let mut chars = text.chars().peekable();
            while let Some(c) = chars.next() {
                match c {
                    '\r' => {
                        self.col = 0;
                        self.wrap_next = false;
                    }
                    '\n' => self.line_feed(),
                    '\x1b' => {
                        assert_eq!(chars.next(), Some('['), "only CSI sequences");
                        let mut params = String::new();
                        let end = loop {
                            let c = chars.next().expect("unterminated CSI");
                            if ('@'..='~').contains(&c) {
                                break c;
                            }
                            params.push(c);
                        };
                        let n = params.parse::<usize>().unwrap_or(1).max(1);
                        match end {
                            'A' => {
                                self.row = self.row.saturating_sub(n);
                                self.wrap_next = false;
                            }
                            'B' => {
                                self.row = (self.row + n).min(self.grid.len() - 1);
                                self.wrap_next = false;
                            }
                            'J' => {
                                if self.scroll_on_clear && (self.row, self.col) == (0, 0) {
                                    let used = self
                                        .grid
                                        .iter()
                                        .rposition(|row| row.iter().any(|&c| c != ' '));
                                    for row in &self.grid[..used.map_or(0, |last| last + 1)] {
                                        self.scrollback.push(
                                            row.iter().collect::<String>().trim_end().to_string(),
                                        );
                                    }
                                }
                                for col in self.col..self.cols {
                                    self.grid[self.row][col] = ' ';
                                }
                                for row in self.row + 1..self.grid.len() {
                                    self.grid[row] = vec![' '; self.cols];
                                }
                            }
                            'K' => {
                                for col in self.col..self.cols {
                                    self.grid[self.row][col] = ' ';
                                }
                            }
                            'm' | 'h' | 'l' => {}
                            other => panic!("unexpected CSI {params}{other}"),
                        }
                    }
                    c => {
                        assert!(c.width() == Some(1), "test text is narrow: {c:?}");
                        if self.wrap_next {
                            self.line_feed();
                        }
                        self.grid[self.row][self.col] = c;
                        if self.col + 1 == self.cols {
                            self.wrap_next = true;
                        } else {
                            self.col += 1;
                        }
                    }
                }
            }
        }

        /// The visible rows, without trailing blanks (and trailing empty
        /// rows dropped).
        fn screen(&self) -> Vec<String> {
            let mut rows: Vec<String> = self
                .grid
                .iter()
                .map(|row| row.iter().collect::<String>().trim_end().to_string())
                .collect();
            while rows.last().is_some_and(String::is_empty) {
                rows.pop();
            }
            rows
        }
    }

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_string).collect()
    }

    /// The rows `text` (narrow characters only) takes on a terminal `cols`
    /// wide: long lines wrap.
    fn wrapped(text: &str, cols: usize) -> Vec<String> {
        text.lines()
            .flat_map(|line| {
                let chars: Vec<char> = line.chars().collect();
                if chars.is_empty() {
                    return vec![String::new()];
                }
                chars.chunks(cols).map(|row| row.iter().collect()).collect()
            })
            .collect()
    }

    async fn run(
        term: &mut Fake,
        display: Display,
        state: &mut AppState,
        events: Vec<(u64, Event)>,
        interrupt_after: Option<u64>,
    ) -> Ended {
        let signals: Vec<(u64, Received)> = interrupt_after
            .map(|ms| (ms, Received::Sig(Sig::Interrupt)))
            .into_iter()
            .collect();
        run_with(term, display, state, events, &signals).await
    }

    /// [`run`] with these signals.
    async fn run_with(
        term: &mut Fake,
        display: Display,
        state: &mut AppState,
        events: Vec<(u64, Event)>,
        signals: &[(u64, Received)],
    ) -> Ended {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut signals = Script::new(signals);
        drive_to_end(
            term,
            display,
            play(tx, events),
            &mut rx,
            &mut signals,
            state,
        )
        .await
    }

    /// How [`drive_to_end`] ended.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Ended {
        Finished,
        Interrupted,
    }

    /// [`drive`] `check`, then the final screen, as [`check_confirming`]
    /// does.
    async fn drive_to_end(
        term: &mut Fake,
        display: Display,
        check: impl Future<Output = ()>,
        rx: &mut UnboundedReceiver<Event>,
        signals: &mut Script,
        state: &mut AppState,
    ) -> Ended {
        let mut screen = Screen::new(display, Instant::now());
        let ended = drive(term, &mut screen, check, rx, signals, state)
            .await
            .unwrap();
        screen.finish(term, state).unwrap();
        match ended {
            Some(()) => Ended::Finished,
            None => Ended::Interrupted,
        }
    }

    const PLAIN: Style = Style { color: false };

    #[test]
    fn visible_width_skips_escapes() {
        assert_eq!(visible_width("plain"), 5);
        assert_eq!(visible_width("\x1b[32m✓ up to date\x1b[0m"), 12);
        assert_eq!(visible_width("\x1b[1mgithub:x\x1b[0m @ 42"), 13);
        assert_eq!(visible_width("日本"), 4);
        assert_eq!(visible_width(""), 0);
    }

    #[test]
    fn rows_count_wrapped_lines() {
        assert_eq!(rows_of("", 80), 0);
        assert_eq!(rows_of("a\n\nb\n", 80), 3);
        assert_eq!(rows_of(&format!("{}\n", "x".repeat(80)), 80), 1);
        assert_eq!(rows_of(&format!("{}\n", "x".repeat(81)), 80), 2);
        assert_eq!(rows_of(&format!("{}\nshort\n", "x".repeat(100)), 40), 4);
        // Colors don't take room.
        assert_eq!(rows_of("\x1b[2mxxxx\x1b[0m\n", 4), 1);
        assert_eq!(rows_of("x\n", 0), 1);
    }

    #[test]
    fn redraw_moves_up_over_the_previous_table() {
        assert_eq!(
            redraw(0, "a\nb\n", (80, 24)).unwrap(),
            "\r\x1b[Ka\n\x1b[Kb\n"
        );
        assert_eq!(
            redraw(2, "a\nb\nc\n", (80, 24)).unwrap(),
            "\r\x1b[2A\x1b[Ka\n\x1b[Kb\n\x1b[Kc\n"
        );
        // Shrinking: the rows left over are cleared one by one.
        assert_eq!(
            redraw(3, "a\n", (80, 24)).unwrap(),
            "\r\x1b[3A\x1b[Ka\n\x1b[K\x1b[B\x1b[K\x1b[B\x1b[2A"
        );
        assert_eq!(erase(0), "\r");
        assert_eq!(erase(2), "\r\x1b[2A\x1b[K\x1b[B\x1b[K\x1b[B\x1b[2A");
        // Never "erase below": tmux's scroll-on-clear.
        for out in [redraw(3, "a\n", (80, 24)).unwrap(), erase(5)] {
            assert!(
                !out.contains("\x1b[J") && !out.contains("\x1b[0"),
                "{out:?}"
            );
        }
    }

    #[test]
    fn redraw_needs_a_row_below_the_table() {
        let table = "x\n".repeat(23);
        assert!(redraw(0, &table, (80, 24)).is_some());
        assert!(redraw(0, &"x\n".repeat(24), (80, 24)).is_none());
        // Lines that wrap count double.
        let wide = format!("{}\n", "x".repeat(81)).repeat(12);
        assert!(redraw(0, &wide, (80, 24)).is_none());
        assert!(redraw(0, &wide, (81, 24)).is_some());
    }

    #[test]
    fn progress_line_counts_activities() {
        let mut s = state(&["bigmem", "web1", "web2", "web3"]);
        let now = Instant::now();
        s.apply(Event::RunStarted, now);
        s.apply(Event::FlakeStarted { url: FLAKE.into() }, now);
        for id in 0..4 {
            s.apply(Event::ProbeStarted(id), now);
        }
        assert_eq!(
            progress_line(&s, 0, Duration::from_millis(300)),
            "⠋ 0/4 · 1 resolving · 4 probing · 0.3s"
        );
        s.apply(
            Event::FlakeFinished {
                url: FLAKE.into(),
                result: Ok(info()),
            },
            now,
        );
        s.apply(
            Event::ProbeFinished {
                id: 0,
                result: Ok(report('a')),
                deployed: None,
            },
            now,
        );
        s.apply(
            Event::ProbeFinished {
                id: 1,
                result: Err(ProbeError {
                    kind: ProbeFailure::Unreachable,
                    error: ErrorInfo::msg("ssh: connect to host web1: Connection refused"),
                }),
                deployed: None,
            },
            now,
        );
        s.apply(Event::EvalStarted(2), now);
        s.apply(Event::EvalStarted(3), now);
        s.apply(
            Event::ProbeFinished {
                id: 3,
                result: Ok(report('a')),
                deployed: None,
            },
            now,
        );
        // web1 is done (unreachable), bigmem waits for its evaluation.
        assert_eq!(
            progress_line(&s, 1, Duration::from_millis(2140)),
            "⠙ 1/4 · 2 evaluating · 1 waiting · 2.1s"
        );
        s.apply(
            Event::EvalFinished {
                id: 0,
                result: Ok(desired('a')),
            },
            now,
        );
        assert_eq!(
            progress_line(&s, 12, Duration::from_secs(65)),
            "⠹ 2/4 · 2 evaluating · 1m05s"
        );
    }

    /// A check that takes `ms` and says when it is done (or dropped).
    fn timed_check(ms: u64, done: &Rc<Cell<Option<bool>>>) -> impl Future<Output = ()> {
        struct Dropped(Rc<Cell<Option<bool>>>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                if self.0.get().is_none() {
                    self.0.set(Some(false));
                }
            }
        }
        let flag = Dropped(Rc::clone(done));
        async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            flag.0.set(Some(true));
        }
    }

    async fn confirmed_after(
        ms: u64,
        confirmed: Result<Confirmed, &str>,
    ) -> Result<Confirmed, &str> {
        tokio::time::sleep(Duration::from_millis(ms)).await;
        confirmed
    }

    #[tokio::test(start_paused = true)]
    async fn a_check_of_speculative_hosts_waits_for_their_confirmation() {
        // Confirmed first: the check goes on to the end.
        let done = Rc::new(Cell::new(None));
        let start = tokio::time::Instant::now();
        let confirmed = Box::pin(confirmed_after(100, Ok(Confirmed::Same)));
        let outcome = checked(timed_check(500, &done), Some(confirmed)).await;
        assert!(matches!(outcome, Checked::Done));
        assert_eq!(
            (done.get(), start.elapsed()),
            (Some(true), Duration::from_millis(500))
        );
        // Done first: it waits for the confirmation.
        let done = Rc::new(Cell::new(None));
        let start = tokio::time::Instant::now();
        let confirmed = Box::pin(confirmed_after(800, Ok(Confirmed::Same)));
        let outcome = checked(timed_check(500, &done), Some(confirmed)).await;
        assert!(matches!(outcome, Checked::Done));
        assert_eq!(start.elapsed(), Duration::from_millis(800));
        // Other hosts: the check stops at once.
        let done = Rc::new(Cell::new(None));
        let recheck = Recheck {
            state: state(&["web3"]),
            hosts: vec![(0, host("web3", false))],
            known: Vec::new(),
        };
        let confirmed = Box::pin(confirmed_after(
            100,
            Ok(Confirmed::Changed(Box::new(recheck))),
        ));
        let outcome = checked(timed_check(500, &done), Some(confirmed)).await;
        let Checked::Changed(recheck) = outcome else {
            panic!("expected a new check")
        };
        assert_eq!(recheck.hosts[0].1.name, "web3");
        assert_eq!(done.get(), Some(false), "dropped");
        // Or an error.
        let done = Rc::new(Cell::new(None));
        let confirmed = Box::pin(confirmed_after(100, Err("no hosts")));
        let outcome = checked(timed_check(500, &done), Some(confirmed)).await;
        assert!(matches!(outcome, Checked::Failed("no hosts")));
        // Nothing to confirm: just the check.
        let done = Rc::new(Cell::new(None));
        let mut confirmed = Some(Box::pin(confirmed_after(0, Ok(Confirmed::Same))));
        confirmed.take();
        assert!(matches!(
            checked(timed_check(5, &done), confirmed).await,
            Checked::Done
        ));
        assert_eq!(done.get(), Some(true));
    }

    #[tokio::test(start_paused = true)]
    async fn waits_end_with_the_work_or_the_interrupt() {
        let work = async {
            tokio::time::sleep(Duration::from_secs(3)).await;
            7
        };
        let never = std::future::pending();
        assert_eq!(wait_until("x", work, never, false).await, Some(7));

        // The interrupt wins, and the work is dropped (cancelled).
        let dropped = Rc::new(Cell::new(false));
        struct Flag(Rc<Cell<bool>>);
        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }
        let flag = Flag(dropped.clone());
        let work = async move {
            let _flag = flag;
            std::future::pending::<()>().await;
        };
        let interrupt = tokio::time::sleep(Duration::from_secs(1));
        assert_eq!(wait_until("x", work, interrupt, false).await, None);
        assert!(dropped.get());
    }

    #[test]
    fn the_spinner_label_stays_on_its_line() {
        let line = spinner_line(
            "reading the hosts of evil\nflake\u{1b}[2J",
            1,
            Duration::from_millis(2140),
            80,
        );
        assert_eq!(
            line,
            "\r\x1b[K⠙ reading the hosts of evil flake\\u{1b}[2J 2.1s"
        );
        assert_eq!(
            spinner_line("comparing the closures of web1", 0, Duration::ZERO, 20),
            "\r\x1b[K⠋ comparing the cl…"
        );
    }

    #[test]
    fn dumb_terminals_get_no_redraws() {
        use std::ffi::OsStr;
        assert!(drawable_with(true, Some(OsStr::new("xterm-256color"))));
        assert!(drawable_with(true, None));
        assert!(!drawable_with(true, Some(OsStr::new("dumb"))));
        assert!(!drawable_with(false, Some(OsStr::new("xterm"))));
    }

    #[test]
    fn seconds_have_tenths_under_a_minute() {
        assert_eq!(seconds(Duration::ZERO), "0.0s");
        assert_eq!(seconds(Duration::from_millis(2140)), "2.1s");
        assert_eq!(seconds(Duration::from_secs(59)), "59.0s");
        assert_eq!(seconds(Duration::from_secs(61)), "1m01s");
    }

    #[tokio::test(start_paused = true)]
    async fn final_screen_is_the_plain_table() {
        let names = ["bigmem", "web1", "web2"];
        let mut s = state(&names);
        let mut term = Fake::new(80, 24);
        let ended = run(
            &mut term,
            Display::Table(PLAIN),
            &mut s,
            complete_check(&names),
            None,
        )
        .await;
        assert_eq!(ended, Ended::Finished);
        assert!(!s.is_busy());
        assert!(term.err.is_empty(), "no progress line: {:?}", term.err);

        let mut vt = Vt::new(80, 24);
        vt.feed("$ hostctl status\n");
        vt.feed(&term.out);
        let table = output::status_table(&s, PLAIN, None, util::now_unix());
        let mut expected = vec!["$ hostctl status".to_string()];
        expected.extend(lines(&table));
        assert_eq!(vt.screen(), expected);
        assert!(vt.scrollback.is_empty(), "{:?}", vt.scrollback);
        // It was redrawn along the way, not printed once.
        assert!(term.out.matches("A\x1b[K").count() > 3, "{:?}", term.out);
        assert!(table.contains("2 up to date"), "{table}");
    }

    #[tokio::test(start_paused = true)]
    async fn the_final_table_is_not_cut_to_the_terminal() {
        let names = ["bigmem", "web1-with-a-long-name", "web2-with-a-long-name"];
        let mut s = state(&names);
        s.hosts[1].host.note = Some("a note that is longer than the terminal is wide".into());
        // Every host outdated: the update line names all of them.
        let mut events = complete_check(&names);
        for (_, event) in &mut events {
            if let Event::EvalFinished { result, .. } = event {
                *result = Ok(desired('c'));
            }
        }
        let mut term = Fake::new(40, 24);
        run(&mut term, Display::Table(PLAIN), &mut s, events, None).await;
        let plain = output::status_table(&s, PLAIN, None, util::now_unix());
        assert!(
            plain.contains(
                "update: hostctl activate bigmem web1-with-a-long-name web2-with-a-long-name\n"
            ),
            "{plain}"
        );
        assert!(plain.lines().any(|l| visible_width(l) > 40), "{plain}");
        // What stays on the screen is what --plain prints (wrapped), not the
        // frames cut to 40 columns.
        assert!(term.out.ends_with(&plain), "{:?}", term.out);
        let mut vt = Vt::new(40, 24);
        vt.feed(&term.out);
        assert_eq!(vt.screen(), wrapped(&plain, 40));
        // While it ran, frames were cut to fit.
        assert!(term.out.contains("…"), "{:?}", term.out);
    }

    #[tokio::test(start_paused = true)]
    async fn redraws_at_the_top_left_leave_tmux_scrollback_alone() {
        // A pane running hostctl directly: the table starts at the top-left.
        let names = ["bigmem", "web1", "web2"];
        let mut s = state(&names);
        let mut term = Fake::new(80, 24);
        run(
            &mut term,
            Display::Table(PLAIN),
            &mut s,
            complete_check(&names),
            None,
        )
        .await;
        let mut vt = Vt::tmux(80, 24);
        vt.feed(&term.out);
        let table = output::status_table(&s, PLAIN, None, util::now_unix());
        assert_eq!(vt.screen(), lines(&table));
        assert_eq!(
            vt.scrollback,
            Vec::<String>::new(),
            "frames went to the scrollback"
        );

        // A table that fills the screen ends up starting at its top too.
        let names = ["bigmem", "web1", "web2", "web3", "web4"];
        let mut s = state(&names);
        let mut term = Fake::new(80, 11);
        run(
            &mut term,
            Display::Table(PLAIN),
            &mut s,
            complete_check(&names),
            None,
        )
        .await;
        let mut vt = Vt::tmux(80, 11);
        vt.feed("$ hostctl status\n");
        vt.feed(&term.out);
        let table = output::status_table(&s, PLAIN, None, util::now_unix());
        assert_eq!(table.lines().count(), 10, "{table}");
        assert_eq!(vt.screen(), lines(&table));
        assert_eq!(vt.scrollback, ["$ hostctl status"]);
    }

    /// What bash prints when a job is stopped with Ctrl-Z and `fg`ed again.
    const BASH_STOP_AND_FG: &str =
        "^Z\n[1]+  Stopped                 hostctl status\n$ fg\nhostctl status\n";

    #[tokio::test(start_paused = true)]
    async fn ctrl_z_gives_the_terminal_back_and_draws_afresh() {
        let names = ["bigmem", "web1", "web2"];
        let mut s = state(&names);
        let mut term = Fake::new(80, 30);
        term.shell = BASH_STOP_AND_FG.into();
        let ended = run_with(
            &mut term,
            Display::Table(PLAIN),
            &mut s,
            complete_check(&names),
            &[(700, Received::Suspend)],
        )
        .await;
        assert_eq!(ended, Ended::Finished);
        assert_eq!(term.suspended, 1);
        let mut vt = Vt::new(80, 30);
        vt.feed("$ hostctl status\n");
        vt.feed(&term.out);
        let screen = vt.screen();
        // The frame on the screen when it stopped, what the shell said, then
        // the whole table again, below.
        let shell = lines(BASH_STOP_AND_FG);
        let at = screen
            .iter()
            .position(|line| *line == shell[0])
            .unwrap_or_else(|| panic!("the shell's lines were drawn over: {screen:#?}"));
        assert_eq!(screen[at..at + shell.len()], shell[..], "{screen:#?}");
        assert!(
            screen[1].starts_with("github:jjant/nix-config"),
            "{screen:#?}"
        );
        let table = output::status_table(&s, PLAIN, None, util::now_unix());
        assert_eq!(screen[at + shell.len()..], lines(&table)[..], "{screen:#?}");
    }

    #[tokio::test(start_paused = true)]
    async fn ctrl_z_takes_the_progress_line_off_first() {
        let names = ["bigmem", "web1"];
        let mut s = state(&names);
        let mut term = Fake::new(80, 24);
        run_with(
            &mut term,
            Display::Progress,
            &mut s,
            complete_check(&names),
            &[(400, Received::Suspend), (900, Received::Suspend)],
        )
        .await;
        assert_eq!(term.suspended, 2);
        // Each stop came after a cleared line, and the line came back.
        let frames: Vec<&str> = term.err.split("\r\x1b[K").collect();
        assert!(
            frames.iter().filter(|f| f.is_empty()).count() >= 3,
            "{:?}",
            term.err
        );
        assert!(term.err.ends_with("\r\x1b[K"), "{:?}", term.err);
        assert!(term.out.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn typed_lines_above_stay_put() {
        // Output that was on the screen before (warnings, the prompt) is
        // never touched by the redraws.
        let names = ["bigmem", "web1"];
        let mut s = state(&names);
        let mut term = Fake::new(60, 12);
        run(
            &mut term,
            Display::Table(PLAIN),
            &mut s,
            complete_check(&names),
            None,
        )
        .await;
        let mut vt = Vt::new(60, 12);
        vt.feed("line 1\nline 2\nwarning: something\n");
        vt.feed(&term.out);
        let screen = vt.screen();
        assert_eq!(screen[..3], ["line 1", "line 2", "warning: something"]);
        assert_eq!(
            screen[3..].to_vec(),
            wrapped(&output::status_table(&s, PLAIN, None, util::now_unix()), 60)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_table_taller_than_the_terminal_is_printed_once() {
        let names = ["bigmem", "web1", "web2", "web3", "web4", "web5"];
        let mut s = state(&names);
        // 6 hosts + header + footer + blank lines don't fit 8 rows.
        let mut term = Fake::new(80, 8);
        run(
            &mut term,
            Display::Table(PLAIN),
            &mut s,
            complete_check(&names),
            None,
        )
        .await;
        let table = output::status_table(&s, PLAIN, None, util::now_unix());
        assert_eq!(term.out, table, "stdout is exactly the table");
        assert!(term.err.starts_with("\r\x1b[K⠋ 0/6"), "{:?}", term.err);
        assert!(term.err.ends_with("\r\x1b[K"), "progress cleared");
    }

    #[tokio::test(start_paused = true)]
    async fn growing_too_tall_mid_run_erases_the_table() {
        let names = ["bigmem", "web1", "web2"];
        let mut s = state(&names);
        let mut term = Fake::new(80, 24);
        let size = term.size.clone();
        let mut events = complete_check(&names);
        // Shrink the terminal halfway through.
        events.insert(8, (0, Event::Error(ErrorInfo::msg("ssh can't be run"))));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let check = async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            size.set((80, 5));
            play(tx, events).await;
        };
        drive_to_end(
            &mut term,
            Display::Table(PLAIN),
            check,
            &mut rx,
            &mut Script::new(&[]),
            &mut s,
        )
        .await;
        let table = output::status_table(&s, PLAIN, None, util::now_unix());
        assert!(term.out.ends_with(&table));
        let mut vt = Vt::new(80, 24);
        vt.feed(&term.out);
        // The erased early table left nothing behind: only the final one.
        assert_eq!(vt.screen(), lines(&table));
        assert!(term.err.ends_with("\r\x1b[K"));
    }

    #[test]
    fn narrowing_counts_the_reflowed_rows() {
        let mut s = state(&["bigmem", "web1"]);
        s.hosts[1].host.note = Some("a note long enough to wrap at thirty columns".into());
        let mut screen = Screen::new(Display::Table(PLAIN), Instant::now());
        let mut term = Fake::new(100, 30);
        screen.update(&mut term, &s).unwrap();
        let wide = term
            .out
            .strip_prefix("\r")
            .expect("first draw")
            .replace("\x1b[K", "");
        assert!(wide.lines().any(|l| visible_width(l) > 30), "{wide}");

        // Nothing changed: nothing is written.
        term.out.clear();
        screen.update(&mut term, &s).unwrap();
        assert_eq!(term.out, "");

        // The terminal reflowed the old lines; the cursor goes up past all.
        term.size.set((30, 30));
        screen.update(&mut term, &s).unwrap();
        let up = rows_of(&wide, 30);
        assert!(up > wide.lines().count());
        let narrow = output::status_table(&s, PLAIN, Some(30), util::now_unix());
        assert_eq!(term.out, redraw(up, &narrow, (30, 30)).unwrap());
        assert!(term.out.starts_with(&format!("\r\x1b[{up}A\x1b[K")));
        for line in narrow.lines() {
            assert!(visible_width(line) <= 30, "{line:?}");
        }
        // What the old lines left below the new ones is cleared.
        let mut reflowed = Vt::new(30, 30);
        reflowed.feed(&wide);
        reflowed.feed(&term.out);
        assert_eq!(reflowed.screen(), lines(&narrow));
    }

    #[tokio::test(start_paused = true)]
    async fn interrupting_shows_the_partial_state() {
        let names = ["bigmem", "web1"];
        let mut s = state(&names);
        let mut term = Fake::new(80, 24);
        let ended = run(
            &mut term,
            Display::Table(PLAIN),
            &mut s,
            complete_check(&names),
            Some(700),
        )
        .await;
        assert_eq!(ended, Ended::Interrupted);
        assert!(s.interrupted);
        assert_eq!(s.exit_code(), 130);
        assert!(!s.is_busy());
        let table = output::status_table(&s, PLAIN, None, util::now_unix());
        assert!(table.contains("interrupted"), "{table}");
        let mut vt = Vt::new(80, 24);
        vt.feed(&term.out);
        assert_eq!(vt.screen(), lines(&table));
    }

    #[tokio::test(start_paused = true)]
    async fn events_sent_before_the_interrupt_count() {
        let names = ["bigmem"];
        let mut s = state(&names);
        let mut term = Fake::new(80, 24);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _ = tx.send(Event::RunStarted);
        let _ = tx.send(Event::ProbeStarted(0));
        let _ = tx.send(Event::ProbeFinished {
            id: 0,
            result: Ok(report('a')),
            deployed: None,
        });
        // The interrupt is ready at once, before any event is read.
        let ended = drive_to_end(
            &mut term,
            Display::Progress,
            std::future::pending(),
            &mut rx,
            &mut Script::new(&[(0, Received::Sig(Sig::Interrupt))]),
            &mut s,
        )
        .await;
        assert_eq!(ended, Ended::Interrupted);
        assert!(s.hosts[0].report().is_some());
        assert!(term.out.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn progress_mode_only_writes_stderr() {
        let names = ["bigmem", "web1"];
        let mut s = state(&names);
        let mut term = Fake::new(80, 24);
        run(
            &mut term,
            Display::Progress,
            &mut s,
            complete_check(&names),
            None,
        )
        .await;
        assert!(term.out.is_empty());
        assert!(term.err.starts_with("\r\x1b[K⠋ 0/2"), "{:?}", term.err);
        assert!(term.err.contains(" · 2 probing · "), "{:?}", term.err);
        assert!(term.err.ends_with("\r\x1b[K"));
        // Every frame fits on one row.
        for frame in term.err.split("\r\x1b[K") {
            assert!(visible_width(frame) < 80, "{frame:?}");
        }

        // Without a terminal on stderr, nothing at all.
        let mut s = state(&names);
        let mut term = Fake::new(80, 24);
        term.err_tty = false;
        run(
            &mut term,
            Display::Progress,
            &mut s,
            complete_check(&names),
            None,
        )
        .await;
        assert_eq!((term.out.as_str(), term.err.as_str()), ("", ""));
        assert!(!s.is_busy());
    }

    #[tokio::test(start_paused = true)]
    async fn table_mode_without_a_terminal_prints_the_full_table() {
        let names = ["bigmem", "web1"];
        let mut s = state(&names);
        let mut term = Fake::new(20, 24);
        term.out_tty = false;
        term.err_tty = false;
        run(
            &mut term,
            Display::Table(PLAIN),
            &mut s,
            complete_check(&names),
            None,
        )
        .await;
        assert_eq!(
            term.out,
            output::status_table(&s, PLAIN, None, util::now_unix()),
            "not cut to any width"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn colors_survive_the_redraw() {
        let names = ["bigmem", "web1"];
        let style = Style { color: true };
        let mut s = state(&names);
        let mut term = Fake::new(80, 24);
        run(
            &mut term,
            Display::Table(style),
            &mut s,
            complete_check(&names),
            None,
        )
        .await;
        let table = output::status_table(&s, style, None, util::now_unix());
        assert!(table.contains("\x1b[32m"), "{table:?}");
        assert!(term.out.ends_with(&table));
        let mut vt = Vt::new(80, 24);
        vt.feed(&term.out);
        let plain = output::status_table(&s, PLAIN, None, util::now_unix());
        assert_eq!(vt.screen(), lines(&plain));
    }
}
