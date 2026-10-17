//! The fullscreen dashboard (`hostctl`, `hostctl tui`). `app` is the pure
//! state machine, `view` renders it, this module owns the terminal and the
//! event loop. See DESIGN.md ("TUI").

pub mod app;
pub mod view;

use std::fs::File;
use std::future::Future;
use std::io::{self, Stdout, Write as _};
use std::os::fd::AsRawFd as _;
use std::pin::Pin;
use std::process::ExitStatus;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event as TermEvent, EventStream, KeyCode,
    KeyEvent, KeyEventKind, KeyModifiers,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures_util::StreamExt as _;
use futures_util::stream::FuturesUnordered;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc::{self, UnboundedReceiver};

use self::app::{After, App, Command, Mode};
use crate::config::Host;
use crate::engine::{Engine, EventTx, Freshness};
use crate::live::{Sig, Signals};
use crate::model::{AppState, Event, HostId};
use crate::nix::FlakeInfo;
use crate::util;

#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Re-run the check this long after the previous one finished.
    pub refresh_interval: Option<Duration>,
    pub color: bool,
    /// `--offline`: refreshes never touch the network either.
    pub offline: bool,
}

/// At most one frame per this long.
const FRAME: Duration = Duration::from_millis(16);

/// Run the dashboard until the user quits; returns the final state's
/// [`AppState::exit_code`].
///
/// - the first check of `hosts` starts before terminal setup (so its
///   round trips overlap with metadata and probes);
/// - alternate screen + raw mode, restored on exit, error, panic (hook) and
///   SIGTERM/SIGHUP;
/// - `tokio::select!` over crossterm's `EventStream`, engine events (the
///   channel is drained before each draw; ≤ 1 frame / 16 ms) and a sleep
///   until [`app::App::next_wakeup`];
/// - commands from [`app::App::on_key`]: `Refresh` runs a new check over all
///   hosts with [`crate::engine::Freshness::Force`] semantics for
///   resolution (ignored with a flash while one runs); `Diff` runs
///   [`Engine::diff`] as a task; `Run` (a confirmed activation or build)
///   cancels a running check, suspends the TUI (leave the alternate screen,
///   disable raw mode, drop the event stream), gets the process from
///   [`Engine::prepare_interactive`], runs it with
///   [`crate::exec::run_interactive`], waits for Enter, restores the TUI and
///   then re-probes the host (activation) or diffs it (build); `Copy`
///   writes [`crate::util::osc52`];
/// - on quit the cache is flushed ([`Engine::save_cache`]).
///
/// Refreshes run on [`Engine::with_freshness`] (sharing the cache and the
/// limits): `r` with [`Freshness::Force`], the timer with
/// [`Freshness::Auto`] (which honors `nix.refresh`); both with
/// [`Freshness::Offline`] under `options.offline`. A check that `Run`
/// interrupted starts over once the command is done (which re-probes the
/// host too). A signal (SIGINT, SIGTERM, SIGHUP) ends the dashboard as
/// interrupted (exit 130).
pub async fn run(
    engine: Engine,
    state: AppState,
    hosts: Vec<(HostId, Host)>,
    known: Vec<FlakeInfo>,
    options: Options,
) -> anyhow::Result<u8> {
    let hosts = Rc::new(hosts);
    let (tx, rx) = mpsc::unbounded_channel();
    let mut first = start_check(&engine, &hosts, known, &tx);
    // Started now, its processes run while the terminal is set up.
    let check = futures_util::poll!(first.as_mut())
        .is_pending()
        .then_some(first);
    let mut ui = Ui {
        app: App::new(state, options.color),
        engine,
        hosts,
        tx,
        rx,
        check,
        jobs: FuturesUnordered::new(),
        events: None,
        refresh_interval: options.refresh_interval,
        refresh_at: None,
        offline: options.offline,
    };
    if ui.check.is_none() {
        ui.check_ended();
    }
    let mut signals = Signals::new()?;
    let result = ui.run(&mut signals).await;
    // Dropping the work in flight kills its processes.
    ui.check = None;
    ui.jobs.clear();
    crate::live::save_cache(&ui.engine).await;
    result?;
    Ok(ui.app.state.exit_code())
}

type Work = Pin<Box<dyn Future<Output = ()>>>;
type Tty = Terminal<CrosstermBackend<Stdout>>;

/// A check of `hosts` (already resolved flakes in `known`), as a future
/// owning everything it needs.
fn start_check(
    engine: &Engine,
    hosts: &Rc<Vec<(HostId, Host)>>,
    known: Vec<FlakeInfo>,
    tx: &EventTx,
) -> Work {
    let (engine, hosts, tx) = (engine.clone(), Rc::clone(hosts), tx.clone());
    Box::pin(async move { engine.check(&hosts, &known, &tx).await })
}

/// How a refresh resolves flakes: `r` asks for the branch's latest commit,
/// the timer respects `nix.refresh`; `--offline` never touches the network.
fn refresh_freshness(manual: bool, offline: bool) -> Freshness {
    if offline {
        Freshness::Offline
    } else if manual {
        Freshness::Force
    } else {
        Freshness::Auto
    }
}

/// What the event loop does next.
enum Flow {
    Continue,
    Quit,
}

struct Ui {
    app: App,
    /// Runs diffs, probes and interactive commands; checks run on it with
    /// their own [`Freshness`].
    engine: Engine,
    hosts: Rc<Vec<(HostId, Host)>>,
    tx: EventTx,
    rx: UnboundedReceiver<Event>,
    check: Option<Work>,
    /// Diffs and re-probes.
    jobs: FuturesUnordered<Work>,
    /// The terminal's input; `None` while suspended.
    events: Option<EventStream>,
    refresh_interval: Option<Duration>,
    /// When the timer starts the next check.
    refresh_at: Option<Instant>,
    offline: bool,
}

impl Ui {
    async fn run(&mut self, signals: &mut Signals) -> anyhow::Result<()> {
        let hook = PanicHook::install();
        let mut modes = Modes::default();
        modes.enter()?;
        let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        repaint(&mut terminal)?;
        let result = self.event_loop(&mut terminal, &mut modes, signals).await;
        // Restore in the reverse order, whatever happened.
        let shown = terminal.show_cursor();
        drop(terminal);
        let left = modes.leave();
        drop(hook);
        result?;
        shown?;
        left?;
        Ok(())
    }

    async fn event_loop(
        &mut self,
        terminal: &mut Tty,
        modes: &mut Modes,
        signals: &mut Signals,
    ) -> anyhow::Result<()> {
        self.events = Some(EventStream::new());
        let mut dirty = true;
        let mut last_frame: Option<Instant> = None;
        loop {
            let now = Instant::now();
            let next_frame = last_frame.map_or(now, |at| at + FRAME);
            if dirty && now >= next_frame {
                self.drain();
                terminal
                    .draw(|frame| view::draw(frame, &self.app, util::now_unix(), Instant::now()))?;
                last_frame = Some(now);
                dirty = false;
            }
            let wake = if dirty {
                next_frame
            } else {
                self.app
                    .next_wakeup(now, self.refresh_at)
                    .unwrap_or(now + Duration::from_secs(30))
            };
            tokio::select! {
                biased;
                _ = signals.recv() => {
                    self.interrupt();
                    return Ok(());
                }
                event = next_event(&mut self.events) => {
                    dirty = true;
                    match event {
                        Some(Ok(event)) => {
                            if let Flow::Quit = self.on_terminal(event, terminal, modes, signals).await? {
                                return Ok(());
                            }
                        }
                        Some(Err(e)) => return Err(anyhow::Error::new(e).context("reading the terminal")),
                        None => return Ok(()),
                    }
                }
                Some(event) = self.rx.recv() => {
                    self.app.state.apply(event, Instant::now());
                    dirty = true;
                }
                () = finished(&mut self.check) => {
                    self.check = None;
                    self.check_ended();
                    dirty = true;
                }
                Some(()) = self.jobs.next(), if !self.jobs.is_empty() => dirty = true,
                () = tokio::time::sleep_until(wake.into()) => {
                    // The frame-rate wait is over (`dirty`), or it's time for a
                    // spinner frame, a flash to go, new ages or a refresh.
                    if !dirty {
                        if self.app.state.is_busy() {
                            self.app.tick();
                        }
                        if self.check.is_none()
                            && self.refresh_at.is_some_and(|at| Instant::now() >= at)
                        {
                            self.refresh(false).await;
                        }
                        dirty = true;
                    }
                }
            }
        }
    }

    /// Apply the events waiting in the channel.
    fn drain(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            self.app.state.apply(event, Instant::now());
        }
    }

    fn check_ended(&mut self) {
        self.refresh_at = self.refresh_interval.map(|every| Instant::now() + every);
    }

    async fn on_terminal(
        &mut self,
        event: TermEvent,
        terminal: &mut Tty,
        modes: &mut Modes,
        signals: &mut Signals,
    ) -> anyhow::Result<Flow> {
        let now = Instant::now();
        let command = match event {
            TermEvent::Key(key) => self.app.on_key(key, now),
            // Bracketed paste: pasted text never triggers keys, except as
            // typing in the filter prompt.
            TermEvent::Paste(text) => {
                if self.app.mode == Mode::Filter {
                    for c in text.chars().filter(|c| !c.is_control()) {
                        let key = KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
                        self.app.on_key(key, now);
                    }
                }
                Command::None
            }
            // Redrawing (which resizes) is all a resize needs.
            _ => Command::None,
        };
        match command {
            Command::None => {}
            Command::Quit => return Ok(Flow::Quit),
            Command::Refresh => self.refresh(true).await,
            Command::Diff(id) => self.diff(id).await,
            Command::Copy(text) => {
                let mut out = io::stdout().lock();
                let _ = out
                    .write_all(util::osc52(&text).as_bytes())
                    .and_then(|()| out.flush());
            }
            Command::Run { id, command, after } => {
                return self
                    .run_command(id, &command, after, terminal, modes, signals)
                    .await;
            }
        }
        Ok(Flow::Continue)
    }

    /// A new check of every host (`manual`: `r`, else the timer); see
    /// [`run`] for how flakes are resolved again.
    async fn refresh(&mut self, manual: bool) {
        if self.check.is_some() {
            return;
        }
        self.refresh_at = None;
        let engine = self
            .engine
            .with_freshness(refresh_freshness(manual, self.offline));
        let mut check = start_check(&engine, &self.hosts, Vec::new(), &self.tx);
        // Poll once so that its first events are on the next frame.
        if futures_util::poll!(check.as_mut()).is_pending() {
            self.check = Some(check);
        } else {
            self.check_ended();
        }
    }

    /// Diff host `id` from its current to its desired generation.
    async fn diff(&mut self, id: HostId) {
        let Some(h) = self.app.state.hosts.get(id) else {
            return;
        };
        let (Some(current), Some(desired)) =
            (h.current().cloned(), h.desired().map(|d| d.path.clone()))
        else {
            let message = format!(
                "can't diff {}: its generations aren't known",
                util::sanitize_terminal(&h.host.name)
            );
            self.app.flash(message, Instant::now());
            return;
        };
        let (engine, host, tx) = (self.engine.clone(), h.host.clone(), self.tx.clone());
        self.push_job(Box::pin(async move {
            engine.diff(id, &host, &current, &desired, &tx).await;
        }))
        .await;
    }

    /// Probe host `id` again.
    async fn reprobe(&mut self, id: HostId) {
        let Some(h) = self.app.state.hosts.get(id) else {
            return;
        };
        let (engine, host, tx) = (self.engine.clone(), h.host.clone(), self.tx.clone());
        self.push_job(Box::pin(async move {
            engine.probe(id, &host, &tx).await;
        }))
        .await;
    }

    async fn push_job(&mut self, job: Work) {
        self.jobs.push(job);
        // Poll once so that it starts (and says so) before the next frame.
        let _ = futures_util::poll!(self.jobs.next());
    }

    /// A signal: everything in flight is interrupted, and so is the
    /// dashboard (exit 130).
    fn interrupt(&mut self) {
        self.check = None;
        self.jobs.clear();
        self.drain();
        self.app.state.apply(Event::Interrupted, Instant::now());
    }

    /// Stop the check and the jobs in flight (dropping them kills their
    /// processes), as before an interactive command. Whether a check was
    /// running.
    fn cancel_work(&mut self) -> bool {
        let checking = self.check.is_some();
        let busy = checking || !self.jobs.is_empty();
        self.check = None;
        self.jobs.clear();
        self.drain();
        if busy {
            cancelled(&mut self.app.state, Instant::now());
            self.check_ended();
        }
        checking
    }

    /// A confirmed activation or build: run `command` for host `id` on the
    /// terminal, with the dashboard suspended.
    async fn run_command(
        &mut self,
        id: HostId,
        command: &str,
        after: After,
        terminal: &mut Tty,
        modes: &mut Modes,
        signals: &mut Signals,
    ) -> anyhow::Result<Flow> {
        let Some(host) = self.app.state.hosts.get(id).map(|h| h.host.clone()) else {
            return Ok(Flow::Continue);
        };
        let interrupted_check = self.cancel_work();
        // Nothing may read the terminal while the command does.
        self.events = None;
        terminal.show_cursor()?;
        modes.leave()?;
        let outcome = suspended(&self.engine, &host, command, after, signals).await;
        if outcome.quit {
            self.interrupt();
            return Ok(Flow::Quit);
        }
        modes.enter()?;
        repaint(terminal)?;
        self.events = Some(EventStream::new());
        let now = Instant::now();
        self.app
            .flash(run_flash(&host.name, after, &outcome.ran), now);
        match (after, &outcome.ran) {
            // Whatever the activation did, look again (a new check does).
            (After::Reprobe, Ran::Exited(_)) if !interrupted_check => self.reprobe(id).await,
            (After::Rediff, Ran::Exited(status)) if status.success() => self.diff(id).await,
            _ => {}
        }
        // The check the command interrupted starts over.
        if interrupted_check {
            self.refresh(false).await;
        }
        Ok(Flow::Continue)
    }
}

/// Clear the screen and have the next frame drawn in full. Not
/// [`Terminal::clear`]: it asks the terminal for the cursor position, an
/// answer crossterm gives up on at once after an `EventStream` was dropped
/// (its wake-up is still pending).
fn repaint(terminal: &mut Tty) -> io::Result<()> {
    let size = terminal.size()?;
    terminal.resize(ratatui::layout::Rect::new(0, 0, size.width, size.height))
}

/// `check` until it finishes; pending without one.
async fn finished(check: &mut Option<Work>) {
    match check {
        Some(check) => check.await,
        None => std::future::pending().await,
    }
}

/// The next terminal event; pending while suspended.
async fn next_event(events: &mut Option<EventStream>) -> Option<io::Result<TermEvent>> {
    match events {
        Some(events) => events.next().await,
        None => std::future::pending().await,
    }
}

/// What dropping the work in flight leaves in `state`: probes, evaluations
/// and diffs read as interrupted, but the session itself goes on (the exit
/// code isn't 130 because of it).
fn cancelled(state: &mut AppState, now: Instant) {
    let interrupted = state.interrupted;
    state.apply(Event::Interrupted, now);
    state.interrupted = interrupted;
}

/// What happened to a command run with the dashboard suspended.
#[derive(Debug)]
enum Ran {
    Exited(ExitStatus),
    /// It never ran, and why (it couldn't be prepared or started, or
    /// Ctrl-C came first).
    NotRun(String),
}

struct Suspended {
    ran: Ran,
    /// SIGTERM or SIGHUP: hostctl has to go.
    quit: bool,
}

/// Run `command` for `host` on the plain terminal: say what runs where, get
/// the process ([`Engine::prepare_interactive`]), run it
/// ([`crate::exec::run_interactive`]), report how it ended and wait for
/// Enter. Ctrl-C before it runs cancels it; SIGTERM/SIGHUP quit.
async fn suspended(
    engine: &Engine,
    host: &Host,
    command: &str,
    after: After,
    signals: &mut Signals,
) -> Suspended {
    let name = util::sanitize_terminal(&host.name);
    let _ = write_stdout(&format!(
        "hostctl: {} {name} {}\n$ {}\n\n",
        match after {
            After::Reprobe => "activating",
            After::Rediff => "building the new generation of",
        },
        place(host),
        util::sanitize_terminal(command),
    ));
    let ran = tokio::select! {
        biased;
        sig = signals.recv() => match sig {
            Sig::Interrupt => Ran::NotRun("interrupted".into()),
            Sig::Terminate | Sig::Hangup => return Suspended { ran: Ran::NotRun("signal".into()), quit: true },
        },
        prepared = engine.prepare_interactive(host, command) => match prepared {
            Err(e) => {
                let _ = write_stdout(&format!("{}\n", e.detail));
                Ran::NotRun(e.summary)
            }
            Ok(spec) => {
                let mut running = std::pin::pin!(crate::exec::run_interactive(&spec));
                loop {
                    tokio::select! {
                        status = &mut running => break match status {
                            Ok(status) => Ran::Exited(status),
                            Err(e) => Ran::NotRun(format!(
                                "can't run {}: {e}",
                                util::sanitize_terminal(&spec.program)
                            )),
                        },
                        // SIGINT is the command's (one racing its start
                        // reached hostctl instead).
                        sig = signals.recv() => if sig != Sig::Interrupt {
                            return Suspended { ran: Ran::NotRun("signal".into()), quit: true };
                        },
                    }
                }
            }
        },
    };
    let _ = write_stdout(&format!("\nhostctl: {name}: {}.", ran_text(&ran)));
    // Without a way to read the terminal, back to the dashboard (which
    // reports the failure itself when it can't read it either).
    let quit = press_enter(signals).await.unwrap_or(false);
    Suspended { ran, quit }
}

/// `here`, or `on <host>` (`(<destination>)` when it says more).
fn place(host: &Host) -> String {
    let name = util::sanitize_terminal(&host.name);
    match host.ssh() {
        _ if crate::actions::runs_here(host) => "here".into(),
        Some(ssh) if ssh.destination != host.name => {
            format!("on {name} ({})", util::sanitize_terminal(&ssh.destination))
        }
        _ => format!("on {name}"),
    }
}

/// `done`, `failed (exit 1)`, or why it didn't run.
fn ran_text(ran: &Ran) -> String {
    match ran {
        Ran::Exited(status) if status.success() => "done".into(),
        Ran::Exited(status) => format!("failed ({})", status_text(status)),
        Ran::NotRun(why) => format!("not run: {}", util::sanitize_terminal(why)),
    }
}

fn status_text(status: &ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt as _;
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exit {code}"),
        (None, Some(signal)) => format!("signal {signal}"),
        (None, None) => "unknown status".into(),
    }
}

/// The dashboard's flash after a suspended command.
fn run_flash(name: &str, after: After, ran: &Ran) -> String {
    let name = util::sanitize_terminal(name);
    match (after, ran) {
        (After::Reprobe, Ran::Exited(status)) if status.success() => {
            format!("activated {name}: checking it again")
        }
        (After::Reprobe, Ran::Exited(status)) => format!(
            "activating {name} failed ({}): checking it again",
            status_text(status)
        ),
        (After::Rediff, Ran::Exited(status)) if status.success() => {
            format!("built the new generation of {name}: diffing again")
        }
        (After::Rediff, Ran::Exited(status)) => {
            format!("building for {name} failed ({})", status_text(status))
        }
        (After::Reprobe, Ran::NotRun(why)) => {
            format!("didn't activate {name}: {}", util::sanitize_terminal(why))
        }
        (After::Rediff, Ran::NotRun(why)) => {
            format!("didn't build for {name}: {}", util::sanitize_terminal(why))
        }
    }
}

/// Wait for Enter (or Esc, Ctrl-C, Ctrl-D) after ` Press Enter to return.`;
/// keys typed before the prompt are dropped. `Ok(true)` when SIGTERM or
/// SIGHUP came instead.
async fn press_enter(signals: &mut Signals) -> io::Result<bool> {
    enable_raw_mode()?;
    discard_typeahead();
    // Events crossterm read before the suspension.
    while crossterm::event::poll(Duration::ZERO)? {
        let _ = crossterm::event::read()?;
    }
    // Raw mode: the line needs its carriage return.
    let _ = write_stdout(" Press Enter to return.\r\n");
    let mut events = EventStream::new();
    loop {
        tokio::select! {
            biased;
            sig = signals.recv() => return Ok(sig != Sig::Interrupt),
            event = events.next() => match event {
                Some(Ok(TermEvent::Key(key))) if returns(&key) => return Ok(false),
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e),
                None => return Ok(false),
            },
        }
    }
}

/// The keys that leave the "Press Enter" prompt: Enter, Esc, Ctrl-C,
/// Ctrl-D — not letters, which may be typing meant for the command.
fn returns(key: &KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    key.kind != KeyEventKind::Release
        && (matches!(key.code, KeyCode::Enter | KeyCode::Esc)
            || (ctrl && matches!(key.code, KeyCode::Char('c' | 'd'))))
}

/// Drop what was typed on the terminal and not read yet.
fn discard_typeahead() {
    if let Ok(tty) = File::options().read(true).open("/dev/tty") {
        // SAFETY: a plain call on an open descriptor.
        unsafe { libc::tcflush(tty.as_raw_fd(), libc::TCIFLUSH) };
    }
}

fn write_stdout(text: &str) -> io::Result<()> {
    let mut out = io::stdout().lock();
    out.write_all(text.as_bytes())?;
    out.flush()
}

/// The dashboard's terminal modes: raw input, the alternate screen,
/// bracketed paste. Left again on drop.
#[derive(Default)]
struct Modes {
    active: bool,
}

impl Modes {
    fn enter(&mut self) -> io::Result<()> {
        enable_raw_mode()?;
        self.active = true;
        crossterm::execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)
    }

    fn leave(&mut self) -> io::Result<()> {
        if !std::mem::take(&mut self.active) {
            return Ok(());
        }
        let screen = crossterm::execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen);
        let raw = disable_raw_mode();
        screen.and(raw)
    }
}

impl Drop for Modes {
    fn drop(&mut self) {
        let _ = self.leave();
    }
}

type PanicFn = Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Sync + Send + 'static>;

/// While this lives, a panic first restores the terminal (so its message
/// is readable), then runs the previous hook. Dropping it puts that hook
/// back.
struct PanicHook {
    previous: Option<Arc<PanicFn>>,
}

impl PanicHook {
    fn install() -> PanicHook {
        let previous: Arc<PanicFn> = Arc::new(std::panic::take_hook());
        let chained = Arc::clone(&previous);
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            chained(info);
        }));
        PanicHook {
            previous: Some(previous),
        }
    }
}

impl Drop for PanicHook {
    fn drop(&mut self) {
        // Hooks can't be changed while panicking (and ours is needed then).
        if std::thread::panicking() {
            return;
        }
        drop(std::panic::take_hook());
        if let Some(previous) = self.previous.take().and_then(|p| Arc::try_unwrap(p).ok()) {
            std::panic::set_hook(previous);
        }
    }
}

/// Undo the dashboard's terminal modes, from the panic hook (no ratatui
/// there, so the cursor is shown directly).
fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = crossterm::execute!(
        io::stdout(),
        DisableBracketedPaste,
        LeaveAlternateScreen,
        crossterm::cursor::Show
    );
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt as _;

    use super::*;
    use crate::config::{ActivateOn, Kind};
    use crate::model::ProbeFailure;
    use crate::tui::app::testing::{local_host, pending, ssh_host};

    #[test]
    fn refreshes_force_resolution_unless_offline() {
        assert_eq!(refresh_freshness(true, false), Freshness::Force);
        assert_eq!(refresh_freshness(false, false), Freshness::Auto);
        assert_eq!(refresh_freshness(true, true), Freshness::Offline);
        assert_eq!(refresh_freshness(false, true), Freshness::Offline);
    }

    #[test]
    fn only_deliberate_keys_leave_the_prompt() {
        let key = |code, modifiers| KeyEvent::new(code, modifiers);
        assert!(returns(&key(KeyCode::Enter, KeyModifiers::NONE)));
        assert!(returns(&key(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(returns(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(returns(&key(KeyCode::Char('d'), KeyModifiers::CONTROL)));
        for c in ['q', 'y', 'a', ' ', 'c'] {
            assert!(
                !returns(&key(KeyCode::Char(c), KeyModifiers::NONE)),
                "{c:?}"
            );
        }
        let mut release = key(KeyCode::Enter, KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert!(!returns(&release));
    }

    #[test]
    fn cancelling_stops_the_work_but_not_the_session() {
        let mut s = pending();
        assert!(s.state.is_busy());
        let at = s.at(500);
        cancelled(&mut s.state, at);
        assert!(!s.state.is_busy());
        assert!(!s.state.interrupted, "the dashboard wasn't interrupted");
        // Probes in flight read as interrupted.
        assert!(matches!(
            s.state.hosts[0].probe.get(),
            Some(Err(e)) if e.kind == ProbeFailure::Interrupted
        ));
        assert_ne!(s.state.exit_code(), 130);

        // An interrupted dashboard stays interrupted.
        let mut s = pending();
        let (t10, t20) = (s.at(10), s.at(20));
        s.state.apply(Event::Interrupted, t10);
        cancelled(&mut s.state, t20);
        assert!(s.state.interrupted);
    }

    #[test]
    fn where_commands_run() {
        assert_eq!(place(&local_host("bigmem", Kind::HomeManager)), "here");
        assert_eq!(
            place(&ssh_host("al2", Kind::HomeManager, "devhost.example.com")),
            "on al2 (devhost.example.com)"
        );
        assert_eq!(place(&ssh_host("web1", Kind::Nixos, "web1")), "on web1");
        let mut pushed = ssh_host("web2", Kind::Nixos, "web2");
        pushed.activate_on = ActivateOn::Local;
        assert_eq!(place(&pushed), "here");
    }

    #[test]
    fn flashes_after_a_command() {
        let ok = Ran::Exited(ExitStatus::from_raw(0));
        let failed = Ran::Exited(ExitStatus::from_raw(1 << 8));
        let killed = Ran::Exited(ExitStatus::from_raw(libc::SIGKILL));
        assert_eq!(
            run_flash("web1", After::Reprobe, &ok),
            "activated web1: checking it again"
        );
        assert_eq!(
            run_flash("web1", After::Reprobe, &failed),
            "activating web1 failed (exit 1): checking it again"
        );
        assert_eq!(
            run_flash("web1", After::Rediff, &ok),
            "built the new generation of web1: diffing again"
        );
        assert_eq!(
            run_flash("web1", After::Rediff, &killed),
            "building for web1 failed (signal 9)"
        );
        assert_eq!(
            run_flash("web1", After::Reprobe, &Ran::NotRun("upload failed".into())),
            "didn't activate web1: upload failed"
        );
        assert_eq!(
            run_flash(
                "evil\x1b]52;c;x\x07",
                After::Rediff,
                &Ran::NotRun("x".into())
            ),
            "didn't build for evil\\u{1b}]52;c;x\\u{7}: x"
        );
        assert_eq!(ran_text(&ok), "done");
        assert_eq!(ran_text(&failed), "failed (exit 1)");
        assert_eq!(
            ran_text(&Ran::NotRun("interrupted".into())),
            "not run: interrupted"
        );
    }

    #[test]
    fn the_panic_hook_is_put_back() {
        // Nothing panics here: installing and dropping must leave a working
        // hook chain behind (the default one).
        let hook = PanicHook::install();
        drop(hook);
        let result = std::panic::catch_unwind(|| 1 + 1);
        assert_eq!(result.ok(), Some(2));
    }
}
