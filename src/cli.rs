//! Command-line interface: argument definitions and dispatch.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, BufRead as _, IsTerminal as _, Write as _};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, ExitStatus};
use std::time::Instant;

use clap::{Args, CommandFactory as _, Parser, Subcommand, ValueEnum};
use serde::Serialize;
use tokio::sync::mpsc;

use crate::actions::{self, Activation, RunsOn};
use crate::cache::Cache;
use crate::config::{Config, Host, Overrides, Resolved};
use crate::engine::{Engine, Freshness};
use crate::live::{self, Display, Sig, Signals};
use crate::local::LocalInfo;
use crate::model::{AppState, DiffOutcome, DiffResult, ErrorInfo, HostId, HostState, Status};
use crate::nix::FlakeInfo;
use crate::output::{self, Style};
use crate::remote::CommandSpec;
use crate::storepath::StorePath;
use crate::util;

#[derive(Debug, Parser)]
#[command(
    name = "hostctl",
    version,
    about = "Are your Nix hosts running the configuration on your flake's main branch?",
    after_help = "Without a command: the dashboard on a terminal, `status` otherwise.\n\
                  Exit codes: 0 up to date, 1 outdated, 2 trouble, 3 couldn't check, 130 interrupted."
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalArgs,
    #[command(subcommand)]
    pub command: Option<Cmd>,
}

#[derive(Debug, Args)]
pub struct GlobalArgs {
    /// Config file [default: $XDG_CONFIG_HOME/hostctl/config.toml, /etc/hostctl/config.toml]
    #[arg(
        short,
        long,
        global = true,
        env = "HOSTCTL_CONFIG",
        value_name = "FILE"
    )]
    pub config: Option<PathBuf>,
    /// Flake to compare against (overrides the config's `flake`)
    #[arg(
        short,
        long,
        global = true,
        env = "HOSTCTL_FLAKE",
        value_name = "FLAKE"
    )]
    pub flake: Option<String>,
    /// Concurrent evaluations [default: min(CPUs, available GiB)]
    #[arg(short, long, global = true, value_name = "N")]
    pub jobs: Option<usize>,
    /// Treat this host as the local machine
    #[arg(
        long = "self",
        global = true,
        env = "HOSTCTL_SELF",
        value_name = "HOST"
    )]
    pub self_host: Option<String>,
    /// Don't read or write the result cache
    #[arg(long, global = true)]
    pub no_cache: bool,
    /// Re-resolve flakes now (ignore `nix.refresh`)
    #[arg(long, global = true, conflicts_with = "offline")]
    pub refresh: bool,
    /// Don't touch the network: cached resolutions and results only
    #[arg(long, global = true)]
    pub offline: bool,
    /// When to use colors (NO_COLOR is honored for `auto`)
    #[arg(long, global = true, value_enum, default_value_t = ColorChoice::Auto, value_name = "WHEN")]
    pub color: ColorChoice,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Interactive dashboard (the default on a terminal)
    Tui {
        /// Only hosts matching these globs (or @local)
        hosts: Vec<String>,
    },
    /// Check every host and print a table
    Status {
        /// Only hosts matching these globs (or @local)
        hosts: Vec<String>,
        /// Print JSON
        #[arg(long)]
        json: bool,
        /// Never redraw in place, print once at the end
        #[arg(long)]
        plain: bool,
    },
    /// Show what activating would change on a host (closure diff)
    Diff {
        host: String,
        /// Build/fetch the new generation on the host if it isn't anywhere yet
        #[arg(long)]
        build: bool,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Activate the flake's configuration on hosts (interactive)
    Activate {
        /// Hosts (globs or @local) to activate
        #[arg(required = true)]
        hosts: Vec<String>,
        /// Don't ask for confirmation
        #[arg(short, long)]
        yes: bool,
        /// Activate even if already up to date
        #[arg(long)]
        force: bool,
    },
    /// List the hosts, where they come from and how they're reached
    Hosts {
        /// Only hosts matching these globs (or @local)
        hosts: Vec<String>,
    },
    /// Inspect the configuration
    Config {
        #[command(subcommand)]
        action: ConfigCmd,
    },
    /// Print shell completions
    Completions { shell: clap_complete::Shell },
}

#[derive(Debug, Subcommand)]
pub enum ConfigCmd {
    /// Print the effective configuration as TOML
    Show,
    /// Print the config file in use and where hostctl looks
    Path,
    /// Validate a config file (default: the one in use); reads nothing else
    Check { file: Option<PathBuf> },
}

/// Parse arguments, run the command, map the outcome to an exit code
/// (DESIGN.md: 0/1/2 from the state, 3 when the check couldn't run — usage
/// and config errors, no host selected, nothing to compare against — and
/// 130 when interrupted).
pub fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            return ExitCode::from(clap_exit_code(&e));
        }
    };
    let code = finish(run(cli));
    // Every way out (a finished check, Ctrl-C, quitting the dashboard)
    // comes here: what was killed on the way gets its SIGKILL before
    // hostctl is gone.
    crate::exec::finish_kills();
    ExitCode::from(code)
}

/// `--help` and `--version` succeed; whatever else clap rejects is a usage
/// error.
fn clap_exit_code(e: &clap::Error) -> u8 {
    if e.use_stderr() { 3 } else { 0 }
}

/// Why a command stopped short of an exit code from the state.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Exit {
    /// The check couldn't run: usage or config errors, no host selected,
    /// nothing to compare against.
    Usage(String),
    /// Something failed once checking had started.
    Trouble(String),
    /// A signal (SIGINT, SIGTERM, SIGHUP).
    Interrupted,
}

impl Exit {
    fn code(&self) -> u8 {
        match self {
            Exit::Usage(_) => 3,
            Exit::Trouble(_) => 2,
            Exit::Interrupted => 130,
        }
    }
}

/// The exit code of an outcome, after printing why it failed.
fn finish(result: Result<u8, Exit>) -> u8 {
    match result {
        Ok(code) => code,
        Err(exit) => {
            if let Some(message) = exit_message(&exit) {
                eprint(&message);
            }
            exit.code()
        }
    }
}

/// `error: <why>` for an exit that says why. Sanitized: messages quote
/// config files, paths and what hosts and flakes said.
fn exit_message(exit: &Exit) -> Option<String> {
    match exit {
        Exit::Usage(message) | Exit::Trouble(message) => {
            Some(format!("error: {}\n", util::sanitize_terminal(message)))
        }
        Exit::Interrupted => None,
    }
}

/// The commands that check hosts.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Check {
    Tui {
        hosts: Vec<String>,
    },
    Status {
        hosts: Vec<String>,
        json: bool,
        plain: bool,
    },
    Diff {
        host: String,
        build: bool,
        json: bool,
    },
    Activate {
        hosts: Vec<String>,
        yes: bool,
        force: bool,
    },
    Hosts {
        hosts: Vec<String>,
    },
}

impl Check {
    /// What the command runs when none is given.
    fn default_for(stdout_tty: bool) -> Check {
        if stdout_tty {
            Check::Tui { hosts: Vec::new() }
        } else {
            Check::Status {
                hosts: Vec::new(),
                json: false,
                plain: false,
            }
        }
    }

    fn patterns(&self) -> &[String] {
        match self {
            Check::Tui { hosts }
            | Check::Status { hosts, .. }
            | Check::Activate { hosts, .. }
            | Check::Hosts { hosts } => hosts,
            Check::Diff { host, .. } => std::slice::from_ref(host),
        }
    }
}

fn run(cli: Cli) -> Result<u8, Exit> {
    let Cli { global, command } = cli;
    let check = match command {
        // These read neither the configuration (beyond its location) nor
        // anything else, so they work whatever state it is in.
        Some(Cmd::Completions { shell }) => return completions(shell),
        Some(Cmd::Config {
            action: ConfigCmd::Path,
        }) => return config_path(&global),
        Some(Cmd::Config {
            action: ConfigCmd::Check { file },
        }) => return config_check(file.as_deref(), &global),
        Some(Cmd::Config {
            action: ConfigCmd::Show,
        }) => {
            let setup = Setup::load(&global)?;
            print(&setup.config.to_toml())?;
            return Ok(0);
        }
        Some(Cmd::Tui { hosts }) => {
            if !io::stdout().is_terminal() {
                return Err(Exit::Usage(
                    "the dashboard needs a terminal (stdout isn't one): use `hostctl status`"
                        .into(),
                ));
            }
            Check::Tui { hosts }
        }
        Some(Cmd::Status { hosts, json, plain }) => Check::Status { hosts, json, plain },
        Some(Cmd::Diff { host, build, json }) => Check::Diff { host, build, json },
        Some(Cmd::Activate { hosts, yes, force }) => Check::Activate { hosts, yes, force },
        Some(Cmd::Hosts { hosts }) => Check::Hosts { hosts },
        None => Check::default_for(live::drawable(&io::stdout())),
    };
    checking(&global, check)
}

/// Run `check` in a fresh runtime, with hostctl's signal handling.
fn checking(global: &GlobalArgs, check: Check) -> Result<u8, Exit> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Exit::Usage(format!("can't start the async runtime: {e}")))?;
    // First of all: `exec::run_interactive` puts back the dispositions it
    // finds, so hostctl's handlers must already be there.
    let mut signals = {
        let _entered = runtime.enter();
        Signals::new().map_err(|e| Exit::Usage(format!("can't handle signals: {e}")))?
    };
    // Blocking work (files, the passwd database) before going async.
    let setup = Setup::load(global)?;
    let cache = open_cache(&setup.config);
    let result = runtime.block_on(async {
        let engine = Engine::new(&setup.config, cache, setup.freshness);
        let result = dispatch(check, &setup, &engine, &mut signals).await;
        // Whatever the outcome (a no-op when nothing changed).
        live::save_cache(&engine).await;
        result
    });
    // Nothing may hold up the exit (e.g. a confirmation prompt's reader).
    runtime.shutdown_background();
    result
}

/// The configuration and what follows from the global flags.
struct Setup {
    config: Config,
    /// The config file in use.
    path: Option<PathBuf>,
    local: LocalInfo,
    freshness: Freshness,
    /// Colors on stdout.
    color: bool,
}

impl Setup {
    /// Load the configuration and apply the global flags (blocking: reads
    /// files, maybe the passwd database).
    fn load(global: &GlobalArgs) -> Result<Setup, Exit> {
        let (mut config, path) =
            Config::load(global.config.as_deref()).map_err(|e| Exit::Usage(format!("{e:#}")))?;
        config
            .apply(&Overrides {
                flake: global.flake.clone(),
                jobs: global.jobs,
                self_host: global.self_host.clone(),
                no_cache: global.no_cache,
            })
            .map_err(|e| Exit::Usage(format!("{e:#}")))?;
        if config.flake.is_none() {
            config.flake = flake_here(std::env::current_dir().ok().as_deref());
        }
        let local = LocalInfo::detect(config.self_host.clone());
        let color = use_color(
            global.color,
            io::stdout().is_terminal(),
            std::env::var_os("NO_COLOR").as_deref(),
            std::env::var_os("TERM").as_deref(),
        );
        Ok(Setup {
            config,
            path,
            local,
            freshness: freshness(global.refresh, global.offline),
            color,
        })
    }

    fn style(&self) -> Style {
        Style { color: self.color }
    }
}

/// `--refresh` forces resolutions, `--offline` forbids them.
fn freshness(refresh: bool, offline: bool) -> Freshness {
    if refresh {
        Freshness::Force
    } else if offline {
        Freshness::Offline
    } else {
        Freshness::Auto
    }
}

/// The last fallback for `flake` (DESIGN.md "Configuration"): the current
/// directory, as an absolute path, when it has a `flake.nix`.
fn flake_here(dir: Option<&Path>) -> Option<String> {
    let dir = dir?;
    if !dir.is_absolute() || !dir.join("flake.nix").is_file() {
        return None;
    }
    dir.to_str().map(str::to_owned)
}

/// Whether to color stdout: `auto` means it is a terminal, `NO_COLOR` is
/// unset or empty, and `TERM` isn't `dumb`.
fn use_color(
    choice: ColorChoice,
    stdout_tty: bool,
    no_color: Option<&OsStr>,
    term: Option<&OsStr>,
) -> bool {
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => {
            stdout_tty && no_color.is_none_or(OsStr::is_empty) && term != Some(OsStr::new("dumb"))
        }
    }
}

/// The result cache `config` asks for: the on-disk one, or a memory-only
/// one with `cache = false` / `--no-cache` (or no cache directory).
/// Blocking: reads the file.
fn open_cache(config: &Config) -> Cache {
    match Cache::default_path() {
        Some(path) if config.cache => Cache::open(path),
        _ => Cache::in_memory(),
    }
}

async fn dispatch(
    check: Check,
    setup: &Setup,
    engine: &Engine,
    signals: &mut Signals,
) -> Result<u8, Exit> {
    let (resolved, known) = find_hosts(setup, engine, signals).await?;
    let Resolved { hosts, warnings } = resolved;
    if hosts.is_empty() {
        return Err(Exit::Usage(no_hosts(&setup.config, setup.path.as_deref())));
    }
    let hosts = select(hosts, check.patterns()).map_err(Exit::Usage)?;
    if let Check::Hosts { .. } = check {
        print(&output::hosts_table(&hosts, &warnings, setup.style()))?;
        return Ok(0);
    }
    for warning in &warnings {
        warn(warning);
    }
    let selection = Selection {
        hosts: hosts.into_iter().enumerate().collect(),
        known,
    };
    match check {
        Check::Tui { .. } => dashboard(setup, engine, selection).await,
        Check::Status { json, plain, .. } => {
            status(setup, engine, &selection, json, plain, signals).await
        }
        Check::Diff { host, build, json } => {
            diff(setup, engine, &selection, &host, build, json, signals).await
        }
        Check::Activate { yes, force, .. } => {
            activate(setup, engine, &selection, yes, force, signals).await
        }
        Check::Hosts { .. } => Ok(0),
    }
}

/// The hosts to check: discovery and the flake's inventory (when a flake
/// is known and either is on), then the configuration. Also the flakes
/// discovery resolved.
async fn find_hosts(
    setup: &Setup,
    engine: &Engine,
    signals: &mut Signals,
) -> Result<(Resolved, Vec<FlakeInfo>), Exit> {
    let config = &setup.config;
    let mut known = Vec::new();
    let mut flake_hosts = None;
    if let Some(flake) = config.flake.as_deref()
        && (config.discover || !config.inventory.is_empty())
    {
        let label = format!("reading the hosts of {flake}");
        let discovered = live::wait_for(&label, engine.discover(flake), signals)
            .await
            .ok_or(Exit::Interrupted)?;
        match discovered {
            Ok((info, hosts)) => {
                known.push(info);
                flake_hosts = Some(hosts);
            }
            Err(e) if !config.hosts.is_empty() => warn(&format!(
                "{}; checking the configured hosts only",
                hosts_error(flake, &e.summary)
            )),
            Err(e) => return Err(Exit::Usage(hosts_error(flake, &describe(&e)))),
        }
    }
    let resolved = config
        .resolve_hosts(flake_hosts.as_ref(), &setup.local)
        .map_err(|e| Exit::Usage(format!("{e:#}")))?;
    Ok((resolved, known))
}

/// `can't read the hosts of <flake>: <why>`, said once: the errors of
/// [`Engine::discover`]'s own evaluation already start like that.
fn hosts_error(flake: &str, why: &str) -> String {
    let prefix = format!("can't read the hosts of {flake}: ");
    if why.starts_with(&prefix) {
        why.to_string()
    } else {
        prefix + why
    }
}

/// What to say when there are no hosts at all.
fn no_hosts(config: &Config, path: Option<&Path>) -> String {
    let file = match path {
        Some(path) => path.display().to_string(),
        None => Config::search_paths()
            .first()
            .map_or_else(|| "the config".to_string(), |p| p.display().to_string()),
    };
    let excluded = if config.exclude.is_empty() {
        String::new()
    } else {
        format!(" (exclude drops {})", config.exclude.join(", "))
    };
    let tables = format!("add [hosts.<name>] tables to {file}");
    match config.flake.as_deref() {
        Some(flake) if config.discover => format!(
            "no hosts{excluded}: {flake} has no nixosConfigurations, darwinConfigurations or \
             homeConfigurations{}; {tables}, or point --flake at another flake",
            if config.inventory.is_empty() {
                String::new()
            } else {
                format!(" (nor {}.hosts)", config.inventory)
            }
        ),
        Some(_) => format!("no hosts{excluded}: discover = false; {tables}"),
        None => format!(
            "no hosts to check{excluded}: point --flake (or flake = \"…\" in the config) at a \
             flake with nixosConfigurations, darwinConfigurations or homeConfigurations, or \
             {tables}"
        ),
    }
}

/// The hosts matching `patterns` — globs over names, or `@local` for the
/// hosts that are this machine — in their order; all of them without
/// patterns. Every pattern has to match a host.
fn select(hosts: Vec<Host>, patterns: &[String]) -> Result<Vec<Host>, String> {
    if patterns.is_empty() {
        return Ok(hosts);
    }
    let matches = |pattern: &str, host: &Host| {
        if pattern == "@local" {
            host.is_local()
        } else {
            util::glob_match(pattern, &host.name)
        }
    };
    if let Some(unmatched) = patterns
        .iter()
        .find(|pattern| !hosts.iter().any(|host| matches(pattern, host)))
    {
        let names: Vec<_> = hosts.iter().map(|h| util::one_line(&h.name)).collect();
        let hint = if unmatched == "@local" {
            "; none is this machine (set self, or pass --self HOST)"
        } else {
            ""
        };
        return Err(format!(
            "no host matches {unmatched:?} (hosts: {}){hint}",
            names.join(", ")
        ));
    }
    Ok(hosts
        .into_iter()
        .filter(|host| patterns.iter().any(|pattern| matches(pattern, host)))
        .collect())
}

/// The selected hosts, numbered for a check, and the flakes already
/// resolved.
struct Selection {
    hosts: Vec<(HostId, Host)>,
    known: Vec<FlakeInfo>,
}

impl Selection {
    fn state(&self, setup: &Setup) -> AppState {
        let hosts = self.hosts.iter().map(|(_, host)| host.clone()).collect();
        let mut state = AppState::new(hosts).with_flakes(&self.known, Instant::now());
        state.config_path.clone_from(&setup.path);
        state
    }
}

async fn dashboard(setup: &Setup, engine: &Engine, selection: Selection) -> Result<u8, Exit> {
    let options = crate::tui::Options {
        refresh_interval: setup.config.ui.refresh_interval.non_zero(),
        color: setup.color,
        offline: setup.freshness == Freshness::Offline,
    };
    let state = selection.state(setup);
    crate::tui::run(
        engine.clone(),
        state,
        selection.hosts,
        selection.known,
        options,
    )
    .await
    .map_err(|e| Exit::Trouble(format!("{e:#}")))
}

async fn status(
    setup: &Setup,
    engine: &Engine,
    selection: &Selection,
    json: bool,
    plain: bool,
    signals: &mut Signals,
) -> Result<u8, Exit> {
    let style = setup.style();
    let redraw = !json && !plain && live::drawable(&io::stdout());
    let display = if redraw {
        Display::Table(style)
    } else {
        Display::Progress
    };
    let state = live::check(
        engine,
        selection.state(setup),
        &selection.hosts,
        &selection.known,
        display,
        signals,
    )
    .await
    .map_err(|e| Exit::Trouble(format!("{e:#}")))?;
    if json {
        print_json(&output::status_report(&state))?;
    } else if !redraw {
        print(&output::status_table(&state, style, None, util::now_unix()))?;
    }
    Ok(state.exit_code())
}

async fn diff(
    setup: &Setup,
    engine: &Engine,
    selection: &Selection,
    pattern: &str,
    build: bool,
    json: bool,
    signals: &mut Signals,
) -> Result<u8, Exit> {
    let [(id, _)] = selection.hosts.as_slice() else {
        let names: Vec<_> = selection
            .hosts
            .iter()
            .map(|(_, h)| util::one_line(&h.name))
            .collect();
        return Err(Exit::Usage(format!(
            "diff needs exactly one host, but {pattern:?} matches {}",
            names.join(", ")
        )));
    };
    let id = *id;
    let mut state = live::check(
        engine,
        selection.state(setup),
        &selection.hosts,
        &selection.known,
        Display::Progress,
        signals,
    )
    .await
    .map_err(|e| Exit::Trouble(format!("{e:#}")))?;
    if state.interrupted {
        return Err(Exit::Interrupted);
    }
    let (current, desired) = diff_inputs(&state.hosts[id]).map_err(Exit::Trouble)?;
    let mut outcome = run_diff(engine, &mut state, id, &current, &desired, signals).await?;
    if build && matches!(outcome, Ok(DiffOutcome::DesiredMissing)) {
        build_desired(engine, &state, id, json, signals).await?;
        outcome = run_diff(engine, &mut state, id, &current, &desired, signals).await?;
    }
    let host = &state.hosts[id].host;
    let name = util::one_line(&host.name);
    match outcome {
        Ok(DiffOutcome::Diff { diff }) => {
            if json {
                print_json(&output::DiffReport {
                    version: 1,
                    host: &util::sanitize_terminal(&host.name),
                    diff: &diff,
                })?;
            } else {
                print(&output::diff_text(&host.name, &diff, setup.style()))?;
            }
            Ok(0)
        }
        Ok(DiffOutcome::DesiredMissing) => Err(Exit::Trouble(if build {
            format!("the new generation of {name} is still missing after the build")
        } else {
            format!(
                "the new generation of {name} isn't built anywhere yet: \
                 `hostctl diff --build {}` builds it {}",
                crate::remote::sh_quote(&name),
                build_place(host)
            )
        })),
        Err(e) => Err(Exit::Trouble(format!(
            "can't diff {name}: {}",
            describe(&e)
        ))),
    }
}

/// The current and desired generations of a checked host, or why they
/// aren't both known.
fn diff_inputs(h: &HostState) -> Result<(StorePath, StorePath), String> {
    let name = util::one_line(&h.host.name);
    let current = match (h.current(), h.probe.get()) {
        (Some(current), _) => current.clone(),
        (None, Some(Ok(report))) => {
            return Err(format!(
                "{name} has no generation to compare: {}",
                util::one_line(
                    report
                        .none_reason
                        .as_deref()
                        .unwrap_or("it was never activated")
                )
            ));
        }
        (None, Some(Err(e))) => {
            return Err(format!(
                "{name}'s current generation is unknown ({}): {}",
                h.status().label(),
                describe(&e.error)
            ));
        }
        (None, None) => return Err(format!("{name} couldn't be probed")),
    };
    let desired = match h.eval.get() {
        Some(Ok(desired)) => desired.path.clone(),
        Some(Err(e)) => {
            return Err(format!(
                "{name}'s configuration doesn't evaluate: {}",
                describe(e)
            ));
        }
        None => return Err(format!("{name}'s desired generation is unknown")),
    };
    Ok((current, desired))
}

/// Diff host `id` from `current` to `desired` and fold the events into
/// `state`.
async fn run_diff(
    engine: &Engine,
    state: &mut AppState,
    id: HostId,
    current: &StorePath,
    desired: &StorePath,
    signals: &mut Signals,
) -> Result<DiffResult, Exit> {
    let host = state.hosts[id].host.clone();
    let name = util::one_line(&host.name);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let label = format!("comparing the closures of {name}");
    live::wait_for(
        &label,
        engine.diff(id, &host, current, desired, &tx),
        signals,
    )
    .await
    .ok_or(Exit::Interrupted)?;
    while let Ok(event) = rx.try_recv() {
        state.apply(event, Instant::now());
    }
    state.hosts[id]
        .diff
        .get()
        .cloned()
        .ok_or_else(|| Exit::Trouble(format!("the diff of {name} produced no result")))
}

/// Build host `id`'s desired generation where its activation would run,
/// interactively (with `json`, the build's stdout goes to stderr).
async fn build_desired(
    engine: &Engine,
    state: &AppState,
    id: HostId,
    json: bool,
    signals: &mut Signals,
) -> Result<(), Exit> {
    let host = &state.hosts[id].host;
    let name = util::one_line(&host.name);
    let flake = state.flake_info(&host.flake).ok_or_else(|| {
        Exit::Trouble(format!("can't build for {name}: its flake isn't resolved"))
    })?;
    let command = actions::build_command(host, flake).map_err(|why| {
        Exit::Trouble(format!("can't build for {name}: {}", util::one_line(&why)))
    })?;
    eprint(&format!(
        "building the new generation of {name} {}:\n  {}\n",
        build_place(host),
        command_text(&command, "  ")
    ));
    let label = format!("preparing the build on {name}");
    let spec = live::wait_for(&label, engine.prepare_interactive(host, &command), signals)
        .await
        .ok_or(Exit::Interrupted)?
        .map_err(|e| Exit::Trouble(format!("can't build for {name}: {}", describe(&e))))?;
    let spec = if json { stdout_to_stderr(spec) } else { spec };
    match interactive(&spec, signals).await? {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(Exit::Trouble(format!(
            "the build for {name} failed ({})",
            exit_status_text(&status)
        ))),
        Err(e) => Err(Exit::Trouble(format!(
            "can't run {}: {e}",
            util::one_line(&spec.program)
        ))),
    }
}

/// `here` or `on <host>`: where builds and activations of `host` run.
fn build_place(host: &Host) -> String {
    if actions::runs_here(host) {
        "here".into()
    } else {
        format!("on {}", util::one_line(&host.name))
    }
}

/// Command text for a message, sanitized but kept on its lines (it may
/// span several): every line after the first starts with `indent` and
/// four more spaces, so none can pass for a line of hostctl's own.
fn command_text(command: &str, indent: &str) -> String {
    util::sanitize_terminal(command).replace('\n', &format!("\n{indent}    "))
}

/// `spec` with its stdout sent to stderr, so `--json` output stays clean.
fn stdout_to_stderr(spec: CommandSpec) -> CommandSpec {
    let CommandSpec {
        program, args, env, ..
    } = spec;
    let mut wrapped = CommandSpec::new("sh")
        .args(["-c", "exec \"$@\" >&2", "sh"])
        .arg(program)
        .args(args);
    wrapped.env = env;
    wrapped
}

/// Run `spec` attached to the terminal. SIGINT goes to it alone (see
/// [`crate::exec::run_interactive`]; one racing its start that reached
/// hostctl is ignored too); SIGTERM or SIGHUP abandon it.
async fn interactive(
    spec: &CommandSpec,
    signals: &mut Signals,
) -> Result<io::Result<ExitStatus>, Exit> {
    let mut running = std::pin::pin!(crate::exec::run_interactive(spec));
    loop {
        tokio::select! {
            status = &mut running => return Ok(status),
            sig = signals.recv() => if sig != Sig::Interrupt {
                return Err(Exit::Interrupted);
            },
        }
    }
}

async fn activate(
    setup: &Setup,
    engine: &Engine,
    selection: &Selection,
    yes: bool,
    force: bool,
    signals: &mut Signals,
) -> Result<u8, Exit> {
    let style = setup.style();
    let redraw = live::drawable(&io::stdout());
    let display = if redraw {
        Display::Table(style)
    } else {
        Display::Progress
    };
    let mut state = live::check(
        engine,
        selection.state(setup),
        &selection.hosts,
        &selection.known,
        display,
        signals,
    )
    .await
    .map_err(|e| Exit::Trouble(format!("{e:#}")))?;
    if !redraw {
        print(&output::status_table(&state, style, None, util::now_unix()))?;
    }
    if state.interrupted {
        return Ok(state.exit_code());
    }
    let targets: Vec<HostId> = (0..state.hosts.len())
        .filter(|&id| force || state.hosts[id].status().needs_activation())
        .collect();
    if targets.is_empty() {
        print(&nothing_to_activate(&state))?;
        return Ok(state.exit_code());
    }

    let Plan {
        listing,
        activations: plan,
        refused,
    } = plan_activations(&state, &targets, force);
    print(&listing)?;
    if plan.is_empty() {
        return Ok(2);
    }
    if !yes && !confirm(question(&state, &plan), signals).await? {
        print("not activating\n")?;
        return Ok(state.exit_code());
    }

    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut up_to_date = 0;
    let mut attempted = 0;
    for (n, (id, activation)) in plan.iter().enumerate() {
        let id = *id;
        let host = state.hosts[id].host.clone();
        let name = util::one_line(&host.name).into_owned();
        attempted += 1;
        print(&format!(
            "\n==> activating {name} {}\n",
            runs_on(&host, activation.runs_on)
        ))?;
        let label = format!("preparing the activation of {name}");
        let prepared = live::wait_for(
            &label,
            engine.prepare_interactive(&host, &activation.command),
            signals,
        )
        .await
        .ok_or(Exit::Interrupted)?;
        let spec = match prepared {
            Ok(spec) => spec,
            Err(e) => {
                eprint(&format!(
                    "error: can't run the activation of {name}: {}\n",
                    describe(&e)
                ));
                continue;
            }
        };
        let ran = interactive(&spec, signals).await?;
        let label = format!("checking {name} again");
        live::wait_for(&label, engine.probe(id, &host, &tx), signals)
            .await
            .ok_or(Exit::Interrupted)?;
        while let Ok(event) = rx.try_recv() {
            state.apply(event, Instant::now());
        }
        let h = &state.hosts[id];
        if h.status() == Status::UpToDate {
            up_to_date += 1;
        }
        print(&outcome_text(h, &ran))?;
        if ran.as_ref().is_ok_and(was_interrupted) {
            let rest: Vec<_> = plan[n + 1..]
                .iter()
                .map(|(id, _)| util::one_line(&state.hosts[*id].host.name))
                .collect();
            if !rest.is_empty() {
                print(&format!(
                    "interrupted: not activating {}\n",
                    rest.join(", ")
                ))?;
            }
            return Err(Exit::Interrupted);
        }
    }
    if attempted > 1 {
        print(&format!("\n{up_to_date} of {attempted} hosts up to date\n"))?;
    }
    // A host that needed activation but couldn't have one isn't done either.
    Ok(if up_to_date == attempted && refused == 0 {
        0
    } else {
        2
    })
}

/// What `activate` is about to do.
struct Plan {
    /// Printed before asking: hosts skipped, each activation, refusals.
    listing: String,
    activations: Vec<(HostId, Activation)>,
    /// Targets that can't be activated.
    refused: usize,
}

/// The plan for activating `targets` of a checked `state`; hosts that
/// aren't targets are listed as skipped when they couldn't be checked
/// (unless `force`).
fn plan_activations(state: &AppState, targets: &[HostId], force: bool) -> Plan {
    let mut plan = Plan {
        listing: String::from("\n"),
        activations: Vec::new(),
        refused: 0,
    };
    for h in &state.hosts {
        let status = h.status();
        if !force && !status.needs_activation() && status != Status::UpToDate {
            plan.listing.push_str(&format!(
                "{}: skipped ({})\n",
                util::one_line(&h.host.name),
                status.label()
            ));
        }
    }
    for &id in targets {
        let h = &state.hosts[id];
        match activation_of(state, id) {
            Ok(activation) => {
                plan.listing.push_str(&plan_text(&h.host, &activation));
                plan.activations.push((id, activation));
            }
            Err(why) => {
                plan.refused += 1;
                plan.listing.push_str(&format!(
                    "{}: can't activate: {}\n",
                    util::one_line(&h.host.name),
                    util::one_line(&why)
                ));
            }
        }
    }
    plan
}

/// The `[y/N]` question before running `activations`.
fn question(state: &AppState, activations: &[(HostId, Activation)]) -> String {
    match activations {
        [(id, _)] => format!("Activate {}?", util::one_line(&state.hosts[*id].host.name)),
        many => format!("Activate these {} hosts?", many.len()),
    }
}

/// The activation of a checked host, or why there is none.
fn activation_of(state: &AppState, id: HostId) -> Result<Activation, String> {
    let h = &state.hosts[id];
    let desired = match h.eval.get() {
        Some(Ok(desired)) => desired,
        Some(Err(e)) => return Err(format!("its configuration doesn't evaluate: {}", e.summary)),
        None => return Err("its desired generation is unknown".into()),
    };
    let flake = state
        .flake_info(&h.host.flake)
        .ok_or_else(|| "its flake couldn't be resolved".to_string())?;
    actions::activation(&h.host, flake, &desired.path)
}

/// `<name>: <command>` (continuation lines indented), then where it runs
/// and any warning.
fn plan_text(host: &Host, activation: &Activation) -> String {
    let mut text = format!(
        "{}: {}\n  runs {}\n",
        util::one_line(&host.name),
        command_text(&activation.command, ""),
        runs_on(host, activation.runs_on)
    );
    if let Some(warning) = &activation.warning {
        text.push_str(&format!("  warning: {}\n", util::one_line(warning)));
    }
    text
}

/// `here`, or `on <host> (<destination>)` (the destination only when it
/// says more than the name).
fn runs_on(host: &Host, runs_on: RunsOn) -> String {
    let name = util::one_line(&host.name);
    match (runs_on, host.ssh()) {
        (RunsOn::Here, _) => "here".into(),
        (RunsOn::Remote, Some(ssh)) if ssh.destination != host.name => {
            format!("on {name} ({})", util::one_line(&ssh.destination))
        }
        (RunsOn::Remote, _) => format!("on {name}"),
    }
}

/// Why `activate` has nothing to do.
fn nothing_to_activate(state: &AppState) -> String {
    let unchecked: Vec<_> = state
        .hosts
        .iter()
        .filter(|h| !matches!(h.status(), Status::UpToDate | Status::Offline))
        .map(|h| format!("{} ({})", util::one_line(&h.host.name), h.status().label()))
        .collect();
    if unchecked.is_empty() {
        "\nnothing to activate: up to date (--force activates anyway)\n".into()
    } else {
        format!(
            "\nnothing to activate: {} couldn't be checked (--force tries anyway)\n",
            unchecked.join(", ")
        )
    }
}

/// The result of one activation, after its host was probed again.
fn outcome_text(h: &HostState, ran: &io::Result<ExitStatus>) -> String {
    let name = util::one_line(&h.host.name);
    let mut text = match ran {
        Ok(status) if status.success() => String::new(),
        Ok(status) => format!(
            "{name}: the activation failed ({})\n",
            exit_status_text(status)
        ),
        Err(e) => format!("{name}: can't run the activation: {e}\n"),
    };
    let status = h.status();
    match (status, h.probe.get()) {
        (Status::UpToDate, _) => text.push_str(&format!("✓ {name} is up to date\n")),
        (_, Some(Err(failure))) => text.push_str(&format!(
            "{} {name}: {}: {}\n",
            status.icon(),
            status.label(),
            util::one_line(&failure.error.summary)
        )),
        _ => text.push_str(&format!("{} {name} is {}\n", status.icon(), status.label())),
    }
    text
}

/// Whether a command ended because of Ctrl-C (it got SIGINT, or a shell
/// or ssh reported it as 130).
fn was_interrupted(status: &ExitStatus) -> bool {
    status.signal() == Some(libc::SIGINT) || status.code() == Some(130)
}

/// `exit 1`, `killed by signal 9`.
fn exit_status_text(status: &ExitStatus) -> String {
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exit {code}"),
        (None, Some(signal)) => format!("killed by signal {signal}"),
        (None, None) => "unknown exit status".into(),
    }
}

/// Ask `question` on the terminal (whatever stdin and stdout are); a
/// signal while waiting interrupts.
async fn confirm(question: String, signals: &mut Signals) -> Result<bool, Exit> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    // Reading a terminal blocks: a thread of its own, which the exit
    // doesn't wait for if the answer never comes.
    std::thread::Builder::new()
        .name("hostctl-confirm".into())
        .spawn(move || {
            let _ = tx.send(ask(&question));
        })
        .map_err(|e| Exit::Trouble(format!("can't ask for confirmation: {e}")))?;
    tokio::select! {
        answer = rx => match answer {
            Ok(Ok(yes)) => Ok(yes),
            Ok(Err(e)) => Err(Exit::Usage(format!(
                "can't ask for confirmation ({e}): pass --yes to activate without asking"
            ))),
            Err(_) => Err(Exit::Trouble("the confirmation prompt went away".into())),
        },
        _ = signals.recv() => {
            eprint("\n");
            Err(Exit::Interrupted)
        }
    }
}

/// `question [y/N] ` on `/dev/tty`, and the answer read from it.
fn ask(question: &str) -> io::Result<bool> {
    let tty = File::options().read(true).write(true).open("/dev/tty")?;
    (&tty).write_all(format!("{question} [y/N] ").as_bytes())?;
    let mut answer = String::new();
    io::BufReader::new(&tty).read_line(&mut answer)?;
    if answer.is_empty() {
        // End of input (Ctrl-D): end the prompt's line.
        (&tty).write_all(b"\n")?;
    }
    Ok(parse_yes(&answer))
}

/// A `[y/N]` answer: `y` or `yes`, in any case; anything else is no.
fn parse_yes(answer: &str) -> bool {
    let answer = answer.trim();
    answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes")
}

/// An error's summary, then its detail (indented) when it says more.
fn describe(error: &ErrorInfo) -> String {
    let detail = error.detail.trim();
    if detail.is_empty() || detail == error.summary {
        return error.summary.clone();
    }
    let mut text = error.summary.clone();
    for line in detail.lines() {
        text.push('\n');
        if !line.trim().is_empty() {
            text.push_str("  ");
            text.push_str(line);
        }
    }
    text
}

fn completions(shell: clap_complete::Shell) -> Result<u8, Exit> {
    print(&completion_script(shell))?;
    Ok(0)
}

fn completion_script(shell: clap_complete::Shell) -> String {
    let mut script = Vec::new();
    clap_complete::generate(shell, &mut Cli::command(), "hostctl", &mut script);
    String::from_utf8_lossy(&script).into_owned()
}

/// The config file hostctl reads: `explicit` (`--config`, which clap fills
/// from `$HOSTCTL_CONFIG`), else the first of `search` that exists.
fn config_in_use(explicit: Option<&Path>, search: &[PathBuf]) -> Option<PathBuf> {
    explicit
        .filter(|path| !path.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .or_else(|| search.iter().find(|path| path.exists()).cloned())
}

/// The file in use (or `none`), then where hostctl looks.
fn config_path_text(in_use: Option<&Path>, search: &[PathBuf]) -> String {
    let mut text = match in_use {
        Some(path) => format!("{}\n", path.display()),
        None => "none\n".into(),
    };
    text.push_str("search order:\n  --config FILE\n  $HOSTCTL_CONFIG\n");
    for path in search {
        text.push_str(&format!("  {}\n", path.display()));
    }
    text
}

fn config_path(global: &GlobalArgs) -> Result<u8, Exit> {
    let search = Config::search_paths();
    let in_use = config_in_use(global.config.as_deref(), &search);
    print(&config_path_text(in_use.as_deref(), &search))?;
    Ok(0)
}

/// Validate `file`, or the config in use; reads nothing but that file.
fn config_check(file: Option<&Path>, global: &GlobalArgs) -> Result<u8, Exit> {
    let path = match file {
        Some(file) => file.to_path_buf(),
        None => match config_in_use(global.config.as_deref(), &Config::search_paths()) {
            Some(path) => path,
            None => {
                print("no config file in use: hostctl runs with the defaults\n")?;
                return Ok(0);
            }
        },
    };
    print(&check_file(&path)?)?;
    Ok(0)
}

/// `ok: <path>` if `path` is a valid config, else why not.
fn check_file(path: &Path) -> Result<String, Exit> {
    Config::from_file(path).map_err(|e| Exit::Usage(format!("{e:#}")))?;
    Ok(format!("ok: {}\n", path.display()))
}

/// Write `text` to stdout. A closed pipe (`hostctl status | head -1`)
/// isn't an error: nobody is reading any more.
fn print(text: &str) -> Result<(), Exit> {
    let mut out = io::stdout().lock();
    match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(Exit::Trouble(format!("can't write to stdout: {e}"))),
    }
}

fn print_json(value: &impl Serialize) -> Result<(), Exit> {
    let mut json = serde_json::to_string_pretty(value)
        .map_err(|e| Exit::Trouble(format!("can't write JSON: {e}")))?;
    json.push('\n');
    print(&json)
}

/// Write `text` to stderr (best effort: there is nowhere else to complain).
fn eprint(text: &str) {
    let mut err = io::stderr().lock();
    let _ = err.write_all(text.as_bytes()).and_then(|()| err.flush());
}

fn warn(message: &str) {
    eprint(&format!("warning: {}\n", util::one_line(message)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ActivateOn, Kind, Sources, SshTarget, Target};
    use crate::model::{Desired, Event, ProbeError, ProbeFailure};
    use crate::probe::ProbeReport;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("hostctl").chain(args.iter().copied()))
    }

    fn host(name: &str) -> Host {
        Host {
            name: name.into(),
            kind: Kind::HomeManager,
            flake: "github:jjant/nix-config".into(),
            attr: format!("homeConfigurations.{name}.activationPackage"),
            target: Target::Ssh(SshTarget {
                destination: name.into(),
                user: None,
                host: name.into(),
                port: None,
                options: vec![],
                forced_command: None,
            }),
            profile: None,
            activate: Kind::HomeManager.default_activate().map(str::to_string),
            activate_on: ActivateOn::Target,
            optional: false,
            note: None,
            sources: Sources::default(),
        }
    }

    fn local(name: &str, assumed: bool) -> Host {
        Host {
            target: Target::Local { assumed },
            ..host(name)
        }
    }

    fn names(hosts: &[Host]) -> Vec<&str> {
        hosts.iter().map(|h| h.name.as_str()).collect()
    }

    fn patterns(p: &[&str]) -> Vec<String> {
        p.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_command_line_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_commands() {
        let cli = parse(&[]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.global.color, ColorChoice::Auto);
        assert!(!cli.global.refresh && !cli.global.offline && !cli.global.no_cache);

        let cli = parse(&["status", "al2-*", "@local", "--json"]).unwrap();
        let Some(Cmd::Status { hosts, json, plain }) = cli.command else {
            panic!("{cli:?}")
        };
        assert_eq!(hosts, ["al2-*", "@local"]);
        assert!(json && !plain);

        let cli = parse(&["diff", "bigmem", "--build"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Cmd::Diff { ref host, build: true, json: false }) if host == "bigmem"
        ));

        let cli = parse(&["activate", "-y", "a", "b", "--force"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Cmd::Activate { ref hosts, yes: true, force: true }) if hosts.len() == 2
        ));

        let cli = parse(&["config", "check", "/etc/hostctl/config.toml"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Cmd::Config { action: ConfigCmd::Check { file: Some(ref f) } })
                if f == Path::new("/etc/hostctl/config.toml")
        ));
        assert!(matches!(
            parse(&["config", "check"]).unwrap().command,
            Some(Cmd::Config {
                action: ConfigCmd::Check { file: None }
            })
        ));

        let cli = parse(&["completions", "zsh"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Cmd::Completions {
                shell: clap_complete::Shell::Zsh
            })
        ));
    }

    #[test]
    fn global_flags_go_anywhere() {
        let cli = parse(&[
            "status",
            "-f",
            "github:o/r",
            "--jobs",
            "3",
            "--self",
            "bigmem",
            "--color",
            "never",
            "--no-cache",
            "--refresh",
        ])
        .unwrap();
        assert_eq!(cli.global.flake.as_deref(), Some("github:o/r"));
        assert_eq!(cli.global.jobs, Some(3));
        assert_eq!(cli.global.self_host.as_deref(), Some("bigmem"));
        assert_eq!(cli.global.color, ColorChoice::Never);
        assert!(cli.global.no_cache && cli.global.refresh);
        let cli = parse(&["-c", "/tmp/x.toml", "--offline", "hosts"]).unwrap();
        assert_eq!(cli.global.config.as_deref(), Some(Path::new("/tmp/x.toml")));
        assert!(cli.global.offline);
    }

    #[test]
    fn usage_errors_exit_3_and_help_exits_0() {
        for bad in [
            &["--bogus"][..],
            &["activate"],
            &["diff"],
            &["status", "--refresh", "--offline"],
            &["--color", "sometimes"],
            &["--jobs", "many"],
            &["completions", "tcsh"],
            &["config"],
            &["frobnicate"],
        ] {
            let e = parse(bad).unwrap_err();
            assert_eq!(clap_exit_code(&e), 3, "{bad:?}: {e}");
        }
        for ok in [
            &["--help"][..],
            &["-h"],
            &["--version"],
            &["status", "--help"],
        ] {
            let e = parse(ok).unwrap_err();
            assert_eq!(clap_exit_code(&e), 0, "{ok:?}");
        }
    }

    #[test]
    fn exit_codes() {
        // From the state: passed through.
        for code in [0, 1, 2, 130] {
            assert_eq!(finish(Ok(code)), code);
        }
        assert_eq!(Exit::Usage("bad config".into()).code(), 3);
        assert_eq!(Exit::Trouble("the build failed".into()).code(), 2);
        assert_eq!(finish(Err(Exit::Interrupted)), 130);
    }

    #[test]
    fn exit_messages_are_sanitized() {
        let usage = Exit::Usage("TOML parse error\n2 | \x1b]0;pwned\x07bogus = 1".into());
        assert_eq!(
            exit_message(&usage).unwrap(),
            "error: TOML parse error\n2 | \\u{1b}]0;pwned\\u{7}bogus = 1\n"
        );
        assert_eq!(
            exit_message(&Exit::Trouble("can't diff x\u{9b}2J".into())).unwrap(),
            "error: can't diff x\\u{9b}2J\n"
        );
        assert_eq!(exit_message(&Exit::Interrupted), None);
    }

    #[test]
    fn default_command_depends_on_the_terminal() {
        assert_eq!(Check::default_for(true), Check::Tui { hosts: vec![] });
        assert_eq!(
            Check::default_for(false),
            Check::Status {
                hosts: vec![],
                json: false,
                plain: false
            }
        );
        let diff = Check::Diff {
            host: "web1".into(),
            build: false,
            json: false,
        };
        assert_eq!(diff.patterns(), ["web1"]);
    }

    #[test]
    fn select_by_glob_and_local() {
        let hosts = || {
            vec![
                host("al2-aarch64"),
                host("al2-x86_64"),
                host("al2023-x86_64"),
                local("bigmem", false),
                local("mac-m1", true),
            ]
        };
        assert_eq!(select(hosts(), &[]).unwrap().len(), 5);
        assert_eq!(
            names(&select(hosts(), &patterns(&["al2-*"])).unwrap()),
            ["al2-aarch64", "al2-x86_64"]
        );
        assert_eq!(
            names(&select(hosts(), &patterns(&["@local"])).unwrap()),
            ["bigmem", "mac-m1"]
        );
        // Overlapping patterns: each host once, in host order.
        assert_eq!(
            names(&select(hosts(), &patterns(&["*x86*", "al2-*", "bigmem"])).unwrap()),
            ["al2-aarch64", "al2-x86_64", "al2023-x86_64", "bigmem"]
        );
        assert_eq!(
            names(&select(hosts(), &patterns(&["bigmem"])).unwrap()),
            ["bigmem"]
        );
    }

    #[test]
    fn a_pattern_matching_nothing_is_an_error() {
        let hosts = vec![host("a"), host("b"), host("c")];
        assert_eq!(
            select(hosts.clone(), &patterns(&["a", "x*"])).unwrap_err(),
            r#"no host matches "x*" (hosts: a, b, c)"#
        );
        assert_eq!(
            select(hosts, &patterns(&["@local"])).unwrap_err(),
            r#"no host matches "@local" (hosts: a, b, c); none is this machine (set self, or pass --self HOST)"#
        );
        assert_eq!(
            select(vec![host("a")], &patterns(&[""])).unwrap_err(),
            r#"no host matches "" (hosts: a)"#
        );
        // Names from a flake can't mess with the terminal.
        let err = select(vec![host("evil\x1b[2J")], &patterns(&["x"])).unwrap_err();
        assert!(!err.contains('\x1b'), "{err:?}");
    }

    #[test]
    fn no_hosts_explains_where_they_come_from() {
        let mut config = Config::default();
        let file = Path::new("/home/u/.config/hostctl/config.toml");
        let none = no_hosts(&config, Some(file));
        assert!(
            none.starts_with("no hosts to check: point --flake"),
            "{none}"
        );
        assert!(none.contains("[hosts.<name>] tables to /home/u/.config/hostctl/config.toml"));

        config.flake = Some("github:o/r".into());
        let empty = no_hosts(&config, Some(file));
        assert!(
            empty.starts_with("no hosts: github:o/r has no nixosConfigurations"),
            "{empty}"
        );
        assert!(empty.contains("(nor lib.hostctl.hosts)"), "{empty}");

        config.discover = false;
        config.exclude = vec!["*-iso".into()];
        assert_eq!(
            no_hosts(&config, Some(file)),
            "no hosts (exclude drops *-iso): discover = false; \
             add [hosts.<name>] tables to /home/u/.config/hostctl/config.toml"
        );
    }

    #[test]
    fn failing_to_read_the_hosts_names_the_flake_once() {
        assert_eq!(
            hosts_error("github:o/r", "unable to download"),
            "can't read the hosts of github:o/r: unable to download"
        );
        let from_discovery = "can't read the hosts of github:o/r: attribute 'hostctl' missing";
        assert_eq!(hosts_error("github:o/r", from_discovery), from_discovery);
    }

    #[test]
    fn colors_follow_the_terminal_and_no_color() {
        let auto = |tty, no_color: Option<&str>, term: Option<&str>| {
            use_color(
                ColorChoice::Auto,
                tty,
                no_color.map(OsStr::new),
                term.map(OsStr::new),
            )
        };
        assert!(auto(true, None, Some("xterm-256color")));
        assert!(auto(true, None, None));
        assert!(
            auto(true, Some(""), Some("xterm")),
            "empty NO_COLOR is unset"
        );
        assert!(!auto(false, None, Some("xterm")));
        assert!(!auto(true, Some("1"), Some("xterm")));
        assert!(!auto(true, None, Some("dumb")));
        assert!(use_color(
            ColorChoice::Always,
            false,
            Some(OsStr::new("1")),
            None
        ));
        assert!(!use_color(
            ColorChoice::Never,
            true,
            None,
            Some(OsStr::new("xterm"))
        ));
    }

    #[test]
    fn freshness_from_flags() {
        assert_eq!(freshness(false, false), Freshness::Auto);
        assert_eq!(freshness(true, false), Freshness::Force);
        assert_eq!(freshness(false, true), Freshness::Offline);
    }

    #[test]
    fn the_flake_falls_back_to_the_current_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(flake_here(Some(dir.path())), None);
        std::fs::write(dir.path().join("flake.nix"), "{ outputs = _: { }; }").unwrap();
        let here = flake_here(Some(dir.path())).unwrap();
        assert_eq!(Path::new(&here), dir.path());
        assert!(Path::new(&here).is_absolute());
        assert_eq!(flake_here(None), None);
        assert_eq!(flake_here(Some(Path::new("relative"))), None);
        // A directory named flake.nix isn't a flake.
        let other = tempfile::tempdir().unwrap();
        std::fs::create_dir(other.path().join("flake.nix")).unwrap();
        assert_eq!(flake_here(Some(other.path())), None);
    }

    #[test]
    fn config_check_validates_one_file() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good.toml");
        std::fs::write(
            &good,
            "flake = \"github:o/r\"\n[hosts.web1]\nkind = \"nixos\"\n",
        )
        .unwrap();
        assert_eq!(
            check_file(&good).unwrap(),
            format!("ok: {}\n", good.display())
        );

        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, "flake = \"github:o/r\"\nflakes = 1\n").unwrap();
        let Err(Exit::Usage(message)) = check_file(&bad) else {
            panic!("an unknown key is an error")
        };
        assert!(message.contains(&bad.display().to_string()), "{message}");
        assert!(message.contains("flakes"), "{message}");

        let missing = dir.path().join("missing.toml");
        assert_eq!(check_file(&missing).map_err(|e| e.code()), Err(3));
    }

    #[test]
    fn completions_for_every_shell() {
        use clap::ValueEnum as _;
        for shell in clap_complete::Shell::value_variants() {
            let script = completion_script(*shell);
            assert!(script.contains("hostctl"), "{shell}");
            assert!(script.contains("activate"), "{shell}: subcommands complete");
        }
    }

    #[test]
    fn the_state_starts_with_the_known_flakes() {
        let selection = Selection {
            hosts: vec![(0, host("a")), (1, local("b", false))],
            known: vec![info()],
        };
        let setup = Setup {
            config: Config::default(),
            path: Some(PathBuf::from("/etc/hostctl/config.toml")),
            local: LocalInfo {
                hostname: "b".into(),
                user: "u".into(),
                os: crate::local::Os::Linux,
                forced: None,
            },
            freshness: Freshness::Auto,
            color: false,
        };
        let state = selection.state(&setup);
        assert_eq!(state.hosts.len(), 2);
        assert_eq!(
            state.config_path.as_deref(),
            Some(Path::new("/etc/hostctl/config.toml"))
        );
        // Discovery resolved the flake: no "resolving…" while checking.
        assert!(state.flake_info("github:jjant/nix-config").is_some());
        assert!(!state.is_busy());
    }

    #[test]
    fn yes_or_no() {
        for yes in ["y", "Y", "yes", "YES", " yes\n", "y\r\n"] {
            assert!(parse_yes(yes), "{yes:?}");
        }
        for no in ["", "\n", "n", "no", "N", "yep", "sure", "y y", "ye"] {
            assert!(!parse_yes(no), "{no:?}");
        }
    }

    #[test]
    fn errors_show_their_detail_when_it_says_more() {
        assert_eq!(describe(&ErrorInfo::msg("nix not found")), "nix not found");
        assert_eq!(
            describe(&ErrorInfo::new(
                "attribute 'x' missing",
                "nix eval …\n\nerror: attribute 'x' missing"
            )),
            "attribute 'x' missing\n  nix eval …\n\n  error: attribute 'x' missing"
        );
    }

    #[test]
    fn json_builds_keep_stdout_clean() {
        let spec = CommandSpec::new("ssh")
            .args(["-t", "--", "web1", "sh", "/tmp/hostctl.x"])
            .env("A", "b");
        let wrapped = stdout_to_stderr(spec);
        assert_eq!(wrapped.program, "sh");
        assert_eq!(
            wrapped.args,
            [
                "-c",
                "exec \"$@\" >&2",
                "sh",
                "ssh",
                "-t",
                "--",
                "web1",
                "sh",
                "/tmp/hostctl.x"
            ]
        );
        assert_eq!(wrapped.env, [("A".to_string(), "b".to_string())]);
    }

    #[tokio::test]
    async fn the_stdout_wrapper_really_redirects() {
        let spec = stdout_to_stderr(CommandSpec::new("sh").args(["-c", "echo out; echo err >&2"]));
        let out = crate::exec::run(&spec, Some(std::time::Duration::from_secs(10)))
            .await
            .unwrap();
        assert_eq!(out.stdout, "");
        assert_eq!(out.stderr, "out\nerr\n");
    }

    #[test]
    fn config_path_lists_the_search_order() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user.toml");
        let system = dir.path().join("system.toml");
        let search = vec![user.clone(), system.clone()];
        assert_eq!(config_in_use(None, &search), None);
        std::fs::write(&system, "").unwrap();
        assert_eq!(config_in_use(None, &search), Some(system.clone()));
        std::fs::write(&user, "").unwrap();
        assert_eq!(config_in_use(None, &search), Some(user.clone()));
        let explicit = dir.path().join("missing.toml");
        assert_eq!(
            config_in_use(Some(&explicit), &search),
            Some(explicit.clone()),
            "an explicit file is in use even if missing (loading it fails)"
        );
        assert_eq!(
            config_in_use(Some(Path::new("")), &search),
            Some(user.clone())
        );

        let text = config_path_text(Some(&user), &search);
        assert_eq!(
            text,
            format!(
                "{}\nsearch order:\n  --config FILE\n  $HOSTCTL_CONFIG\n  {}\n  {}\n",
                user.display(),
                user.display(),
                system.display()
            )
        );
        assert!(config_path_text(None, &search).starts_with("none\n"));
    }

    fn info() -> FlakeInfo {
        FlakeInfo {
            flake: "github:jjant/nix-config".into(),
            locked: format!("github:jjant/nix-config/{}", "4".repeat(40)),
            kind: Some("github".into()),
            locked_url: None,
            rev: Some("4".repeat(40)),
            dirty_rev: None,
            last_modified: Some(1),
            rev_count: None,
            fingerprint: None,
            nar_hash: None,
            description: None,
            stale: None,
            lock_outdated: false,
        }
    }

    /// `hosts` with their flake resolved.
    fn checked(hosts: Vec<Host>) -> AppState {
        let mut state = AppState::new(hosts);
        state.apply(
            Event::FlakeFinished {
                url: info().flake,
                result: Ok(info()),
            },
            Instant::now(),
        );
        state
    }

    fn store(c: char) -> StorePath {
        StorePath::parse(&format!(
            "/nix/store/{}-home-manager-generation",
            c.to_string().repeat(32)
        ))
        .unwrap()
    }

    fn probed(state: &mut AppState, id: HostId, result: Result<ProbeReport, ProbeError>) {
        state.apply(
            Event::ProbeFinished {
                id,
                result,
                deployed: None,
            },
            Instant::now(),
        );
    }

    fn evaluated(state: &mut AppState, id: HostId, result: Result<StorePath, ErrorInfo>) {
        state.apply(
            Event::EvalFinished {
                id,
                result: result.map(|path| Desired {
                    path,
                    rev: Some("4".repeat(40)),
                    cached: false,
                }),
            },
            Instant::now(),
        );
    }

    fn at(c: char) -> ProbeReport {
        ProbeReport {
            current: Some(store(c)),
            ..ProbeReport::default()
        }
    }

    #[test]
    fn diff_needs_both_generations() {
        let mut state = checked(vec![host("a"), host("b"), host("c"), host("d")]);
        probed(&mut state, 0, Ok(at('a')));
        evaluated(&mut state, 0, Ok(store('b')));
        assert_eq!(
            diff_inputs(&state.hosts[0]).unwrap(),
            (store('a'), store('b'))
        );

        probed(
            &mut state,
            1,
            Ok(ProbeReport {
                none_reason: Some("no home-manager profile".into()),
                ..ProbeReport::default()
            }),
        );
        evaluated(&mut state, 1, Ok(store('b')));
        assert_eq!(
            diff_inputs(&state.hosts[1]).unwrap_err(),
            "b has no generation to compare: no home-manager profile"
        );

        probed(
            &mut state,
            2,
            Err(ProbeError {
                kind: ProbeFailure::Unreachable,
                error: ErrorInfo::msg("ssh: Could not resolve hostname c"),
            }),
        );
        assert_eq!(
            diff_inputs(&state.hosts[2]).unwrap_err(),
            "c's current generation is unknown (unreachable): ssh: Could not resolve hostname c"
        );

        probed(&mut state, 3, Ok(at('a')));
        evaluated(&mut state, 3, Err(ErrorInfo::msg("attribute 'd' missing")));
        assert_eq!(
            diff_inputs(&state.hosts[3]).unwrap_err(),
            "d's configuration doesn't evaluate: attribute 'd' missing"
        );
    }

    #[test]
    fn activations_need_a_desired_generation() {
        let mut state = checked(vec![host("a"), host("b"), local("c", false)]);
        probed(&mut state, 0, Ok(at('a')));
        evaluated(&mut state, 0, Ok(store('b')));
        let activation = activation_of(&state, 0).unwrap();
        assert_eq!(activation.runs_on, RunsOn::Remote);
        assert!(activation.command.contains(store('b').as_str()));
        assert_eq!(
            plan_text(&state.hosts[0].host, &activation),
            format!("a: {}\n  runs on a\n", activation.command)
        );

        evaluated(&mut state, 1, Err(ErrorInfo::msg("attribute 'b' missing")));
        assert_eq!(
            activation_of(&state, 1).unwrap_err(),
            "its configuration doesn't evaluate: attribute 'b' missing"
        );
        assert_eq!(
            activation_of(&state, 2).unwrap_err(),
            "its desired generation is unknown"
        );
        evaluated(&mut state, 2, Ok(store('c')));
        assert_eq!(activation_of(&state, 2).unwrap().runs_on, RunsOn::Here);
    }

    #[test]
    fn where_activations_run() {
        let mut desk = host("desk");
        if let Target::Ssh(ssh) = &mut desk.target {
            ssh.destination = "jj@desk.example.com".into();
        }
        assert_eq!(
            runs_on(&desk, RunsOn::Remote),
            "on desk (jj@desk.example.com)"
        );
        assert_eq!(runs_on(&host("web1"), RunsOn::Remote), "on web1");
        assert_eq!(runs_on(&desk, RunsOn::Here), "here");
        assert_eq!(build_place(&local("bigmem", false)), "here");
        assert_eq!(build_place(&desk), "on desk");
        let mut pushed = host("web2");
        pushed.activate_on = ActivateOn::Local;
        assert_eq!(build_place(&pushed), "here");
    }

    #[test]
    fn reports_after_activating() {
        let mut state = checked(vec![host("a"), host("b"), host("c")]);
        probed(&mut state, 0, Ok(at('b')));
        evaluated(&mut state, 0, Ok(store('b')));
        let ok = ExitStatus::from_raw(0);
        assert_eq!(
            outcome_text(&state.hosts[0], &Ok(ok)),
            "✓ a is up to date\n"
        );

        probed(&mut state, 1, Ok(at('a')));
        evaluated(&mut state, 1, Ok(store('b')));
        let failed = ExitStatus::from_raw(1 << 8);
        assert_eq!(
            outcome_text(&state.hosts[1], &Ok(failed)),
            "b: the activation failed (exit 1)\n✗ b is outdated\n"
        );

        probed(
            &mut state,
            2,
            Err(ProbeError {
                kind: ProbeFailure::Unreachable,
                error: ErrorInfo::msg("ssh: connect to host c port 22: Connection refused"),
            }),
        );
        evaluated(&mut state, 2, Ok(store('b')));
        let missing = Err(io::Error::from(io::ErrorKind::NotFound));
        let text = outcome_text(&state.hosts[2], &missing);
        assert!(text.starts_with("c: can't run the activation: "), "{text}");
        assert!(
            text.ends_with(
                "? c: unreachable: ssh: connect to host c port 22: Connection refused\n"
            ),
            "{text}"
        );
    }

    /// Every line of `text` that doesn't start with one of `starts`.
    fn lines_not_starting_with<'a>(text: &'a str, starts: &[&str]) -> Vec<&'a str> {
        text.lines()
            .filter(|line| !line.is_empty() && !starts.iter().any(|s| line.starts_with(s)))
            .collect()
    }

    #[test]
    fn names_from_a_flake_cannot_forge_lines() {
        let evil = "evil\nweb2: rm -rf ~\n==> activating web2";
        let mut forged = host(evil);
        if let Target::Ssh(ssh) = &mut forged.target {
            ssh.destination = "jj@desk\n✓ desk is up to date".into();
        }
        forged.activate = Some("echo one\necho two\nweb3: rm -rf /".into());
        let mut state = checked(vec![
            forged,
            host("unreachable\nweb4: skipped (up to date)"),
        ]);
        probed(&mut state, 0, Ok(at('a')));
        evaluated(&mut state, 0, Ok(store('b')));
        probed(
            &mut state,
            1,
            Err(ProbeError {
                kind: ProbeFailure::Unreachable,
                error: ErrorInfo::msg("ssh: connect to\nweb5: forged"),
            }),
        );
        evaluated(&mut state, 1, Ok(store('b')));

        let plan = plan_activations(&state, &[0], false);
        let flat = "evil web2: rm -rf ~ ==> activating web2";
        assert_eq!(
            plan.listing,
            format!(
                "\nunreachable web4: skipped (up to date): skipped (unreachable)\n\
                 {flat}: echo one\n    echo two\n    web3: rm -rf /\n  \
                 runs on {flat} (jj@desk ✓ desk is up to date)\n"
            )
        );
        assert_eq!(
            question(&state, &plan.activations),
            format!("Activate {flat}?")
        );
        let outcome = outcome_text(&state.hosts[1], &Ok(ExitStatus::from_raw(0)));
        assert_eq!(
            outcome,
            "? unreachable web4: skipped (up to date): unreachable: ssh: connect to web5: forged\n"
        );
        let nothing = nothing_to_activate(&state);
        assert!(
            lines_not_starting_with(&nothing, &["nothing to activate: "]).is_empty(),
            "{nothing:?}"
        );
        let unmatched = select(vec![host(evil)], &patterns(&["x"])).unwrap_err();
        assert!(!unmatched.contains('\n'), "{unmatched:?}");
        probed(
            &mut state,
            0,
            Ok(ProbeReport {
                none_reason: Some("no profile\nerror: forged".into()),
                ..ProbeReport::default()
            }),
        );
        let why = diff_inputs(&state.hosts[0]).unwrap_err();
        assert_eq!(
            why,
            format!("{flat} has no generation to compare: no profile error: forged")
        );
        assert_eq!(build_place(&state.hosts[0].host), format!("on {flat}"));
    }

    #[test]
    fn nothing_to_activate_says_why() {
        let mut state = checked(vec![host("a"), host("b")]);
        for id in 0..2 {
            probed(&mut state, id, Ok(at('a')));
            evaluated(&mut state, id, Ok(store('a')));
        }
        assert_eq!(
            nothing_to_activate(&state),
            "\nnothing to activate: up to date (--force activates anyway)\n"
        );
        evaluated(&mut state, 1, Err(ErrorInfo::msg("boom")));
        assert_eq!(
            nothing_to_activate(&state),
            "\nnothing to activate: b (eval failed) couldn't be checked (--force tries anyway)\n"
        );
    }

    #[test]
    fn exit_statuses() {
        assert_eq!(exit_status_text(&ExitStatus::from_raw(0)), "exit 0");
        assert_eq!(exit_status_text(&ExitStatus::from_raw(4 << 8)), "exit 4");
        assert_eq!(
            exit_status_text(&ExitStatus::from_raw(9)),
            "killed by signal 9"
        );
        assert!(was_interrupted(&ExitStatus::from_raw(libc::SIGINT)));
        assert!(was_interrupted(&ExitStatus::from_raw(130 << 8)));
        assert!(!was_interrupted(&ExitStatus::from_raw(1 << 8)));
        assert!(!was_interrupted(&ExitStatus::from_raw(libc::SIGTERM)));
    }
}
