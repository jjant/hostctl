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
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Poll;
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event as TermEvent, EventStream, KeyCode,
    KeyEvent, KeyEventKind, KeyModifiers,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures_util::StreamExt as _;
use futures_util::future::LocalBoxFuture;
use futures_util::stream::FuturesUnordered;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc::{self, UnboundedReceiver};

use self::app::{After, App, Command, Mode};
use crate::config::Host;
use crate::engine::{Engine, EventTx, Freshness};
use crate::live::{Received, Sig, Signals};
use crate::model::{AppState, Event, HostId};
use crate::nix::FlakeInfo;
use crate::remote::CommandSpec;
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

/// The hosts a refresh found again ([`Rediscover`]).
pub struct Found {
    pub hosts: Vec<Host>,
    /// Flakes resolved on the way: the check doesn't resolve them again.
    pub known: Vec<FlakeInfo>,
}

/// How a refresh finds the hosts again, on the engine it checks with: the
/// flake's discovery and inventory (cached, and sharing the check's
/// resolution of the flake), then the config and the patterns given.
/// `Err` says why not; the hosts then stay as they were.
pub type Rediscover = Box<dyn Fn(Engine) -> LocalBoxFuture<'static, Result<Found, String>>>;

/// Run the dashboard until the user quits; returns the final state's
/// [`AppState::exit_code`].
///
/// - the first check of `hosts` starts before terminal setup (so its
///   round trips overlap with metadata and probes);
/// - alternate screen + raw mode, restored on exit, error, panic (hook) and
///   SIGTERM/SIGHUP, and while suspended (Ctrl-Z, SIGTSTP);
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
/// [`Freshness::Offline`] under `options.offline`. Beside a refresh's
/// check, `rediscover` finds the hosts again; when they changed (the flake
/// gained, lost or redefined some), that check makes way for one of the
/// new list, which keeps what is known about the hosts that stayed the
/// same and the selection by name. With `speculative` hosts (the cache's,
/// as of the last time the flake's were read), the first check gets the
/// same. A check that `Run` interrupted starts over once the command is
/// done (which re-probes the host too). A signal (SIGINT, SIGQUIT,
/// SIGTERM, SIGHUP) ends the dashboard as interrupted (exit 130).
#[allow(clippy::too_many_arguments)]
pub async fn run(
    engine: Engine,
    state: AppState,
    hosts: Vec<(HostId, Host)>,
    known: Vec<FlakeInfo>,
    options: Options,
    rediscover: Option<Rediscover>,
    speculative: bool,
) -> anyhow::Result<u8> {
    let hosts = Rc::new(hosts);
    let (tx, rx) = mpsc::unbounded_channel();
    let mut first = start_check(&engine, &hosts, known, &tx);
    // Started now, its processes run while the terminal is set up.
    let check = futures_util::poll!(first.as_mut())
        .is_pending()
        .then_some(first);
    // And beside it, the hosts it checks read again.
    let mut discovery = rediscover
        .as_ref()
        .filter(|_| speculative)
        .map(|rediscover| rediscover(engine.clone()));
    let found = match &mut discovery {
        Some(discovery) => match futures_util::poll!(discovery.as_mut()) {
            Poll::Ready(found) => Some(found),
            Poll::Pending => None,
        },
        None => None,
    };
    if found.is_some() {
        discovery = None;
    }
    let mut ui = Ui {
        app: App::new(state, options.color),
        engine,
        hosts,
        tx,
        rx,
        check,
        rediscover,
        discovery,
        jobs: FuturesUnordered::new(),
        events: None,
        refresh_interval: options.refresh_interval,
        refresh_at: None,
        offline: options.offline,
    };
    if ui.check.is_none() {
        ui.check_ended();
    }
    if let Some(found) = found {
        ui.rediscovered(found).await;
    }
    let mut signals = Signals::new()?;
    let result = ui.run(&mut signals).await;
    // Dropping the work in flight kills its processes.
    ui.check = None;
    ui.discovery = None;
    ui.jobs.clear();
    crate::live::save_cache(&ui.engine).await;
    result?;
    Ok(ui.app.state.exit_code())
}

type Work = Pin<Box<dyn Future<Output = ()>>>;
type Discovery = LocalBoxFuture<'static, Result<Found, String>>;
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
    rediscover: Option<Rediscover>,
    /// A refresh's search for the hosts, beside its check.
    discovery: Option<Discovery>,
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
        let mut modes = Modes::new(Arc::clone(&hook.restored));
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
                received = signals.recv_all() => match received {
                    Received::Suspend => {
                        self.suspend(terminal, modes)?;
                        dirty = true;
                    }
                    Received::Sig(_) => {
                        self.interrupt();
                        return Ok(());
                    }
                },
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
                found = finished(&mut self.discovery) => {
                    self.discovery = None;
                    self.rediscovered(found).await;
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
                            && self.discovery.is_none()
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

    /// Ctrl-Z (or SIGTSTP): give the terminal back as on quitting, stop
    /// until continued (`fg`), then take it again and draw everything anew.
    /// Nothing reads the terminal meanwhile; work in flight goes on once
    /// hostctl does.
    fn suspend(&mut self, terminal: &mut Tty, modes: &mut Modes) -> anyhow::Result<()> {
        self.events = None;
        terminal.show_cursor()?;
        modes.leave()?;
        crate::live::stop();
        modes.enter()?;
        repaint(terminal)?;
        self.events = Some(EventStream::new());
        Ok(())
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
            // Raw mode makes Ctrl-Z a key, not SIGTSTP.
            TermEvent::Key(key) if suspends(&key) => {
                self.suspend(terminal, modes)?;
                Command::None
            }
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
        if self.discovery.is_some() {
            if manual {
                self.app
                    .flash("still reading the flake's hosts", Instant::now());
            }
            return;
        }
        self.refresh_at = None;
        let engine = self
            .engine
            .with_freshness(refresh_freshness(manual, self.offline));
        self.check_hosts(&engine, Vec::new()).await;
        // The hosts again, beside the check (whose resolution of the flake
        // it shares): the flake may have gained or lost some.
        if let Some(rediscover) = &self.rediscover {
            let mut discovery = rediscover(engine);
            match futures_util::poll!(discovery.as_mut()) {
                Poll::Ready(found) => self.rediscovered(found).await,
                Poll::Pending => self.discovery = Some(discovery),
            }
        }
    }

    /// Start a check of the hosts (`known`: flakes already resolved) on
    /// `engine`.
    async fn check_hosts(&mut self, engine: &Engine, known: Vec<FlakeInfo>) {
        let mut check = start_check(engine, &self.hosts, known, &self.tx);
        // Poll once so that its first events are on the next frame.
        if futures_util::poll!(check.as_mut()).is_pending() {
            self.check = Some(check);
        } else {
            self.check_ended();
        }
    }

    /// What a refresh's search for the hosts found. The same hosts: nothing
    /// to do. Others: whatever runs for the old list stops, the state is
    /// rebuilt for the new one ([`rebuilt`], [`App::replace_state`]) and
    /// checked. Failing that, a flash says why, and the hosts stay.
    async fn rediscovered(&mut self, found: Result<Found, String>) {
        let now = Instant::now();
        let Found { hosts, known } = match found {
            Ok(found) => found,
            Err(why) => {
                let why = util::one_line(&why);
                self.app.flash(format!("{why} (hosts unchanged)"), now);
                return;
            }
        };
        if hosts.iter().eq(self.hosts.iter().map(|(_, host)| host)) {
            return;
        }
        // Dropping them kills their processes; what they sent is about the
        // old list, so it goes there (and over with the hosts that stay).
        self.check = None;
        self.jobs.clear();
        self.drain();
        let state = rebuilt(&self.app.state, &hosts, &known, now);
        let mut message = hosts_changed(&self.app.state, &state);
        if self.app.replace_state(state) {
            message.push_str(": confirm again");
        }
        self.app.flash(message, now);
        self.hosts = Rc::new(hosts.into_iter().enumerate().collect());
        let engine = self
            .engine
            .with_freshness(refresh_freshness(false, self.offline));
        self.check_hosts(&engine, known).await;
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
        self.discovery = None;
        self.jobs.clear();
        self.drain();
        self.app.state.apply(Event::Interrupted, Instant::now());
    }

    /// Stop the check and the jobs in flight (dropping them kills their
    /// processes), as before an interactive command. Whether a check was
    /// running.
    fn cancel_work(&mut self) -> bool {
        let checking = self.check.is_some() || self.discovery.is_some();
        let busy = checking || !self.jobs.is_empty();
        self.check = None;
        self.discovery = None;
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

/// `work` until it finishes; pending without any.
async fn finished<T>(work: &mut Option<LocalBoxFuture<'static, T>>) -> T {
    match work {
        Some(work) => work.await,
        None => std::future::pending().await,
    }
}

/// The state for `hosts`, found again: the hosts `old` had already (the
/// same in every respect) keep what is known about them — nothing of
/// theirs is in flight any more — new and redefined ones start afresh; the
/// flakes `known` are resolved, the others keep what `old` knew of them.
fn rebuilt(old: &AppState, hosts: &[Host], known: &[FlakeInfo], now: Instant) -> AppState {
    let mut state = AppState::new(hosts.to_vec()).with_flakes(known, now);
    for h in &mut state.hosts {
        if let Some(same) = old.hosts.iter().find(|o| o.host == h.host) {
            *h = same.clone();
            h.probe.stop();
            h.eval.stop();
            h.diff.stop();
        }
    }
    for flake in &mut state.flakes {
        if flake.info.get().is_none()
            && let Some(before) = old.flake(&flake.url)
        {
            flake.info = before.info.clone();
            flake.info.stop();
        }
    }
    state.run = old.run.clone();
    state.config_path.clone_from(&old.config_path);
    state.command.clone_from(&old.command);
    state.errors.clone_from(&old.errors);
    state.interrupted = old.interrupted;
    state
}

/// The flash for a host list that changed: which hosts are new, which gone.
fn hosts_changed(old: &AppState, new: &AppState) -> String {
    let names = |state: &AppState| -> std::collections::BTreeSet<String> {
        state
            .hosts
            .iter()
            .map(|h| util::one_line(&h.host.name).into_owned())
            .collect()
    };
    let (before, after) = (names(old), names(new));
    let mut changes = Vec::new();
    let added: Vec<&str> = after.difference(&before).map(String::as_str).collect();
    if !added.is_empty() {
        changes.push(format!("new: {}", added.join(", ")));
    }
    let gone: Vec<&str> = before.difference(&after).map(String::as_str).collect();
    if !gone.is_empty() {
        changes.push(format!("gone: {}", gone.join(", ")));
    }
    if changes.is_empty() {
        "the flake's hosts changed".into()
    } else {
        format!("the flake's hosts changed ({})", changes.join("; "))
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
    let name = util::one_line(&host.name);
    let _ = write_stdout(&announcement(host, command, after));
    let ran = tokio::select! {
        biased;
        sig = signals.recv() => match sig {
            Sig::Interrupt => Ran::NotRun("interrupted".into()),
            Sig::Terminate | Sig::Hangup => return Suspended { ran: Ran::NotRun("signal".into()), quit: true },
        },
        prepared = engine.prepare_interactive(host, command) => match prepared {
            Err(e) => {
                let _ = write_stdout(&format!("  {}\n", e.detail.replace('\n', "\n  ")));
                Ran::NotRun(e.summary)
            }
            Ok(prepared) => {
                let ran = attached(&prepared.spec, signals).await;
                // The uploaded script removes itself once it runs: unless
                // it surely did, remove it.
                if let Some(path) = &prepared.upload
                    && !matches!(&ran, Some(Ran::Exited(status)) if status.success())
                {
                    engine.discard_upload(host, path).await;
                }
                match ran {
                    Some(ran) => ran,
                    None => return Suspended { ran: Ran::NotRun("signal".into()), quit: true },
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

/// Run `spec` attached to the terminal; `None` when SIGTERM or SIGHUP came
/// first (SIGINT is the command's: one racing its start that reached
/// hostctl is ignored).
async fn attached(spec: &CommandSpec, signals: &mut Signals) -> Option<Ran> {
    let mut running = std::pin::pin!(crate::exec::run_interactive(spec));
    loop {
        tokio::select! {
            status = &mut running => return Some(match status {
                Ok(status) => Ran::Exited(status),
                Err(e) => Ran::NotRun(format!(
                    "can't run {}: {e}",
                    util::sanitize_terminal(&spec.program)
                )),
            }),
            sig = signals.recv() => if sig != Sig::Interrupt {
                return None;
            },
        }
    }
}

/// What runs where, before a suspended command: `hostctl: activating
/// <host> on <host>`, then `$ <command>` (its continuation lines
/// indented, so none passes for a line of hostctl's own).
fn announcement(host: &Host, command: &str, after: After) -> String {
    format!(
        "hostctl: {} {} {}\n$ {}\n\n",
        match after {
            After::Reprobe => "activating",
            After::Rediff => "building the new generation of",
        },
        util::one_line(&host.name),
        place(host),
        util::sanitize_terminal(command).replace('\n', "\n    "),
    )
}

/// `here`, or `on <host>` (`(<destination>)` when it says more).
fn place(host: &Host) -> String {
    let name = util::one_line(&host.name);
    match host.ssh() {
        _ if crate::actions::runs_here(host) => "here".into(),
        Some(ssh) if ssh.destination != host.name => {
            format!("on {name} ({})", util::one_line(&ssh.destination))
        }
        _ => format!("on {name}"),
    }
}

/// `done`, `failed (exit 1)`, or why it didn't run.
fn ran_text(ran: &Ran) -> String {
    match ran {
        Ran::Exited(status) if status.success() => "done".into(),
        Ran::Exited(status) => format!("failed ({})", status_text(status)),
        Ran::NotRun(why) => format!("not run: {}", util::one_line(why)),
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
    // However this ends: a SIGTERM here quits hostctl, and the dashboard
    // turns raw mode on again itself when it comes back.
    let _raw = RawMode;
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

/// Raw mode, which was turned on, turned off again on drop.
struct RawMode;

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

/// Ctrl-Z, which suspends the dashboard.
fn suspends(key: &KeyEvent) -> bool {
    key.kind != KeyEventKind::Release
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('z' | 'Z'))
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
/// bracketed paste. Left again on drop — unless the [`PanicHook`] has
/// already put the terminal back: leaving the alternate screen a second
/// time would move the cursor back up over the panic's message.
#[derive(Default)]
struct Modes {
    active: bool,
    /// Set by the panic hook once it restored the terminal.
    restored: Arc<AtomicBool>,
}

impl Modes {
    /// Modes the panic hook that sets `restored` undoes.
    fn new(restored: Arc<AtomicBool>) -> Modes {
        Modes {
            active: false,
            restored,
        }
    }

    fn enter(&mut self) -> io::Result<()> {
        enable_raw_mode()?;
        self.active = true;
        self.restored.store(false, Ordering::SeqCst);
        crossterm::execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)
    }

    fn leave(&mut self) -> io::Result<()> {
        if !self.leaving() {
            return Ok(());
        }
        let screen = crossterm::execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen);
        let raw = disable_raw_mode();
        screen.and(raw)
    }

    /// Whether [`Modes::leave`] has anything to undo (once).
    fn leaving(&mut self) -> bool {
        std::mem::take(&mut self.active) && !self.restored.load(Ordering::SeqCst)
    }
}

impl Drop for Modes {
    fn drop(&mut self) {
        let _ = self.leave();
    }
}

type PanicFn = Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Sync + Send + 'static>;

/// While this lives, a panic first restores the terminal (so its message
/// is readable) and says so in [`PanicHook::restored`], then runs the
/// previous hook. Dropping it puts that hook back.
struct PanicHook {
    previous: Option<Arc<PanicFn>>,
    /// Set once the hook restored the terminal ([`Modes`] then leaves it be).
    restored: Arc<AtomicBool>,
}

impl PanicHook {
    fn install() -> PanicHook {
        let previous: Arc<PanicFn> = Arc::new(std::panic::take_hook());
        let chained = Arc::clone(&previous);
        let restored = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&restored);
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            flag.store(true, Ordering::SeqCst);
            chained(info);
        }));
        PanicHook {
            previous: Some(previous),
            restored,
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
    fn ctrl_z_suspends() {
        let key = |code, modifiers| KeyEvent::new(code, modifiers);
        assert!(suspends(&key(KeyCode::Char('z'), KeyModifiers::CONTROL)));
        assert!(suspends(&key(
            KeyCode::Char('Z'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        )));
        assert!(!suspends(&key(KeyCode::Char('z'), KeyModifiers::NONE)));
        assert!(!suspends(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        let mut release = key(KeyCode::Char('z'), KeyModifiers::CONTROL);
        release.kind = KeyEventKind::Release;
        assert!(!suspends(&release));
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
    fn hosts_found_again_keep_what_is_known_about_them() {
        let s = pending();
        let mut old = s.state.clone();
        let at = s.at(100);
        old.command = "hostctl -c /x.toml".into();
        old.hosts[0].probe.finish(
            Err(crate::model::ProbeError {
                kind: ProbeFailure::Unreachable,
                error: crate::model::ErrorInfo::msg("no route"),
            }),
            at,
        );
        old.hosts[0].probe.start(at);
        let kept = old.hosts[0].host.clone();
        let mut redefined = old.hosts[1].host.clone();
        redefined.note = Some("moved".into());
        let new = ssh_host("web9", Kind::Nixos, "web9");
        let hosts = vec![new.clone(), redefined.clone(), kept.clone()];
        let state = rebuilt(&old, &hosts, &[], at);
        assert_eq!(
            state.hosts.iter().map(|h| &h.host).collect::<Vec<_>>(),
            [&new, &redefined, &kept]
        );
        // The same host: its last probe, nothing in flight.
        assert!(state.hosts[2].probe.get().is_some());
        assert!(!state.hosts[2].busy());
        // A redefined one is another host.
        assert!(state.hosts[1].probe.get().is_none());
        assert_eq!(state.command, "hostctl -c /x.toml");
        assert_eq!(
            hosts_changed(&old, &state),
            format!(
                "the flake's hosts changed (new: web9; gone: {})",
                old.hosts
                    .iter()
                    .map(|h| h.host.name.as_str())
                    .filter(|name| ![kept.name.as_str(), redefined.name.as_str()].contains(name))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        );
        // Only redefined: the same names.
        let before = rebuilt(&old, std::slice::from_ref(&redefined), &[], at);
        redefined.note = None;
        let after = rebuilt(&before, &[redefined], &[], at);
        assert_eq!(hosts_changed(&before, &after), "the flake's hosts changed");
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
    fn announcements_keep_names_on_their_line() {
        let host = ssh_host(
            "evil\nhostctl: web2: done.",
            Kind::HomeManager,
            "web1\n$ rm -rf ~",
        );
        assert_eq!(
            announcement(&host, "echo one\nhostctl: web3: done.", After::Reprobe),
            "hostctl: activating evil hostctl: web2: done. on evil hostctl: web2: done. \
             (web1 $ rm -rf ~)\n$ echo one\n    hostctl: web3: done.\n\n"
        );
        assert_eq!(
            ran_text(&Ran::NotRun("upload failed\nhostctl: web1: done".into())),
            "not run: upload failed hostctl: web1: done"
        );
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
    fn modes_the_panic_hook_restored_are_left_alone() {
        let restored = Arc::new(AtomicBool::new(false));
        let mut modes = Modes::new(Arc::clone(&restored));
        assert!(!modes.leaving(), "never entered");
        modes.active = true;
        assert!(modes.leaving());
        assert!(!modes.leaving(), "left once");
        // A panic put the terminal back: leaving again (the alternate
        // screen, mostly) would move the cursor over the panic's message.
        modes.active = true;
        restored.store(true, Ordering::SeqCst);
        assert!(!modes.leaving());
        assert!(!modes.active);
        // The hook hands out its flag.
        let hook = PanicHook::install();
        assert!(!hook.restored.load(Ordering::SeqCst));
        drop(hook);
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
