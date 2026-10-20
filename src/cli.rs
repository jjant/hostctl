//! Command-line interface: argument definitions and dispatch.

use clap_complete::engine::{ArgValueCompleter, CompletionCandidate};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, BufRead as _, IsTerminal as _, Write as _};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, ExitStatus};
use std::time::Instant;

use clap::{Args, CommandFactory as _, FromArgMatches as _, Parser, Subcommand, ValueEnum};
use serde::Serialize;
use tokio::sync::mpsc;

use crate::actions::{self, Activation, RunsOn};
use crate::cache::Cache;
use crate::config::{Config, FlakeHosts, Host, Overrides, Resolved};
use crate::engine::{Engine, Freshness};
use crate::live::{self, Confirmed, Display, Received, Recheck, Sig, Signals};
use crate::local::LocalInfo;
use crate::model::{AppState, DiffOutcome, DiffResult, ErrorInfo, HostId, HostState, Status};
use crate::nix::FlakeInfo;
use crate::output::{self, Style};
use crate::remote::{self, CommandSpec};
use crate::storepath::StorePath;
use crate::util;

#[derive(Debug, Parser)]
#[command(
    name = "hostctl",
    version,
    about = "Check whether your Nix hosts run their flake's current configuration",
    after_help = "Without a command: `status` (its flags and patterns work here too).\n\
                  Exit codes: 0 up to date, 1 outdated or ahead, 2 trouble, 3 couldn't check, 130 interrupted."
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalArgs,
    /// `status`'s, for `hostctl` without a command.
    #[command(flatten)]
    pub status: StatusArgs,
    #[command(subcommand)]
    pub command: Option<Cmd>,
}

#[derive(Debug, Default, Args)]
pub struct StatusArgs {
    /// Only hosts matching these globs (or @local)
    #[arg(add = ArgValueCompleter::new(complete_hosts))]
    pub hosts: Vec<String>,
    /// Print JSON
    #[arg(long)]
    pub json: bool,
    /// Never redraw in place, print once at the end
    #[arg(long)]
    pub plain: bool,
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
    /// Check every host and print a table (the default)
    Status(StatusArgs),
    /// Show what activating would change on a host (closure diff)
    Diff {
        #[arg(add = ArgValueCompleter::new(complete_hosts))]
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
        #[arg(required_unless_present = "all", add = ArgValueCompleter::new(complete_hosts))]
        hosts: Vec<String>,
        /// All hosts
        #[arg(long, conflicts_with = "hosts")]
        all: bool,
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
        #[arg(add = ArgValueCompleter::new(complete_hosts))]
        hosts: Vec<String>,
    },
    /// Inspect the configuration
    Config {
        #[command(subcommand)]
        action: ConfigCmd,
    },
    /// Print the shell code that completes hostctl's commands, flags and
    /// host names (it calls hostctl to complete)
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
    // What the footer's duration counts from.
    let started = Instant::now();
    // `COMPLETE=<shell> hostctl -- <words>`: the shell asking for completions
    // (see `hostctl completions`).
    clap_complete::CompleteEnv::with_factory(Cli::command).complete();
    let (cli, given) = match parse(std::env::args_os()) {
        Ok(parsed) => parsed,
        Err(e) => {
            let _ = e.print();
            return ExitCode::from(clap_exit_code(&e));
        }
    };
    let cwd = std::env::current_dir().ok();
    let command = command_prefix(&cli.global, given, cwd.as_deref());
    let code = finish(run(cli, &command, started));
    // Every way out (a finished check, Ctrl-C) comes here: what was killed on the way gets its SIGKILL before
    // hostctl is gone.
    crate::exec::finish_kills();
    ExitCode::from(code)
}

/// The arguments, and which global flags were given on the command line
/// itself (rather than by their environment variables).
fn parse(
    args: impl IntoIterator<Item = std::ffi::OsString>,
) -> Result<(Cli, OnCommandLine), clap::Error> {
    let matches = Cli::command().try_get_matches_from(args)?;
    let cli = Cli::from_arg_matches(&matches)?;
    let given = |id| matches.value_source(id) == Some(clap::parser::ValueSource::CommandLine);
    let on_command_line = OnCommandLine {
        config: given("config"),
        flake: given("flake"),
        self_host: given("self_host"),
    };
    Ok((cli, on_command_line))
}

/// Which of the global flags that choose what hostctl looks at were given
/// on the command line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct OnCommandLine {
    config: bool,
    flake: bool,
    self_host: bool,
}

/// How to run hostctl again in this context, for the commands it suggests
/// and copies: `hostctl`, then `-c`, `-f` and `--self` as given on the
/// command line, shell-quoted, with a relative config file or flake path
/// made absolute (in `cwd`) so that the command works from anywhere. What
/// the environment or the config file say applies again by itself.
fn command_prefix(global: &GlobalArgs, given: OnCommandLine, cwd: Option<&Path>) -> String {
    let mut command = String::from("hostctl");
    let mut flag = |flag: &str, value: &str| {
        command.push_str(&format!(" {flag} {}", remote::sh_quote(value)));
    };
    // `.` components go (`Path::components` drops them), `..` stay.
    let absolute = |path: &Path| match cwd {
        Some(cwd) if path.is_relative() => cwd.join(path).components().collect(),
        _ => path.to_path_buf(),
    };
    if let Some(config) = global.config.as_deref().filter(|_| given.config)
        && !config.as_os_str().is_empty()
    {
        flag("-c", &absolute(config).to_string_lossy());
    }
    if let Some(flake) = global
        .flake
        .as_deref()
        .filter(|f| given.flake && !f.is_empty())
    {
        // A path flake; anything else (`github:…`, `nixpkgs`) means the same anywhere.
        let relative =
            flake == "." || flake == ".." || flake.starts_with("./") || flake.starts_with("../");
        if relative {
            flag("-f", &absolute(Path::new(flake)).to_string_lossy());
        } else {
            flag("-f", flake);
        }
    }
    if let Some(name) = global
        .self_host
        .as_deref()
        .filter(|s| given.self_host && !s.is_empty())
    {
        flag("--self", name);
    }
    command
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
    /// A signal (SIGINT, SIGQUIT, SIGTERM, SIGHUP).
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
    fn patterns(&self) -> &[String] {
        match self {
            Check::Status { hosts, .. }
            | Check::Activate { hosts, .. }
            | Check::Hosts { hosts } => hosts,
            Check::Diff { host, .. } => std::slice::from_ref(host),
        }
    }
}

/// Run the command; `hostctl` is how to run hostctl again in this context
/// ([`command_prefix`]).
fn run(cli: Cli, hostctl: &str, started: Instant) -> Result<u8, Exit> {
    let Cli {
        global,
        status,
        command,
    } = cli;
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
            let setup = Setup::load(&global, hostctl, started)?;
            print(&setup.config.to_toml())?;
            return Ok(0);
        }
        Some(Cmd::Status(StatusArgs { hosts, json, plain })) => {
            Check::Status { hosts, json, plain }
        }
        Some(Cmd::Diff { host, build, json }) => Check::Diff { host, build, json },
        // `--all` is no patterns: every host.
        Some(Cmd::Activate {
            hosts,
            all: _,
            yes,
            force,
        }) => Check::Activate { hosts, yes, force },
        Some(Cmd::Hosts { hosts }) => Check::Hosts { hosts },
        None => Check::Status {
            hosts: status.hosts,
            json: status.json,
            plain: status.plain,
        },
    };
    checking(&global, hostctl, started, check)
}

/// Run `check` in a fresh runtime, with hostctl's signal handling.
fn checking(
    global: &GlobalArgs,
    hostctl: &str,
    started: Instant,
    check: Check,
) -> Result<u8, Exit> {
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
    let setup = Setup::load(global, hostctl, started)?;
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
    /// How to run hostctl again in this context ([`AppState::command`]).
    command: String,
    /// The flake is the current directory's `./flake.nix`, because nothing
    /// else set one: messages say so.
    flake_here: bool,
    /// When hostctl started: a check's duration counts from there.
    started: Instant,
}

impl Setup {
    /// Load the configuration and apply the global flags (blocking: reads
    /// files, maybe the passwd database). `hostctl` is how to run hostctl
    /// again with the same flags ([`command_prefix`]); a flake found in the
    /// current directory is added to it.
    fn load(global: &GlobalArgs, hostctl: &str, started: Instant) -> Result<Setup, Exit> {
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
        let mut command = hostctl.to_string();
        let mut here = false;
        if config.flake.is_none() {
            config.flake = flake_here(std::env::current_dir().ok().as_deref());
            if let Some(flake) = &config.flake {
                command.push_str(&format!(" -f {}", remote::sh_quote(flake)));
                here = true;
            }
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
            command,
            flake_here: here,
            started,
        })
    }

    fn style(&self) -> Style {
        Style { color: self.color }
    }

    /// When the flake is the current directory's ([`Setup::flake_here`]):
    /// why (`no config file (looked in …) and no flake set, so ./flake.nix
    /// (<dir>) was used`) and how to choose another.
    fn flake_here_note(&self) -> Option<(String, String)> {
        let dir = util::one_line(self.config.flake.as_deref().filter(|_| self.flake_here)?);
        Some(match &self.path {
            None => {
                let looked: Vec<String> = Config::search_paths()
                    .iter()
                    .map(|p| util::one_line(&p.display().to_string()).into_owned())
                    .collect();
                (
                    format!(
                        "no config file (looked in {}) and no flake set, so ./flake.nix \
                         ({dir}) was used",
                        looked.join(", ")
                    ),
                    "pass -c FILE or --flake, or set HOSTCTL_CONFIG".into(),
                )
            }
            Some(file) => {
                let file = util::one_line(&file.display().to_string()).into_owned();
                (
                    format!("{file} sets no flake, so ./flake.nix ({dir}) was used"),
                    format!("set flake = \"…\" in {file}, or pass --flake"),
                )
            }
        })
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
/// directory, as an absolute path, when it has a `flake.nix`. Nix would
/// misread a path with `#` (a fragment) or `?` (a query) in it: that one
/// becomes the URL nix makes of a path, `git+file://<repository>` (with
/// `?dir=<subdirectory>`) when a `.git` is found above it the way nix
/// looks, else `path:<directory>`, percent-encoded.
fn flake_here(dir: Option<&Path>) -> Option<String> {
    let dir = dir?;
    if !dir.is_absolute() || !dir.join("flake.nix").is_file() {
        return None;
    }
    let path = dir.to_str()?;
    if !path.contains(['#', '?']) {
        return Some(path.to_owned());
    }
    // As nix's parsePathFlakeRefWithFragment: up to, not including, `/`.
    let repository = dir
        .ancestors()
        .take_while(|root| root.parent().is_some())
        .find(|root| root.join(".git").exists());
    Some(match repository {
        Some(root) => {
            let mut query = Vec::new();
            if let Ok(sub) = dir.strip_prefix(root)
                && !sub.as_os_str().is_empty()
            {
                query.push(format!("dir={}", percent_encode(&sub.to_string_lossy())));
            }
            if root.join(".git/shallow").exists() {
                query.push("shallow=1".into());
            }
            let mut url = format!("git+file://{}", percent_encode(&root.to_string_lossy()));
            if !query.is_empty() {
                url.push('?');
                url.push_str(&query.join("&"));
            }
            url
        }
        None => format!("path:{}", percent_encode(path)),
    })
}

/// `s` with every byte but `[A-Za-z0-9-._~/]` percent-encoded (nix decodes
/// the paths and query values of flake URLs).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
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
    let selection = match speculate(setup, engine, &check) {
        Some(selection) => selection,
        None => {
            let (resolved, known) = find_hosts(setup, engine, signals).await?;
            let Resolved { hosts, warnings } = resolved;
            let hosts = selected(setup, hosts, check.patterns())?;
            if let Check::Hosts { .. } = check {
                print(&output::hosts_table(&hosts, &warnings, setup.style()))?;
                return Ok(0);
            }
            for warning in &warnings {
                warn(warning);
            }
            Selection {
                hosts: hosts.into_iter().enumerate().collect(),
                known,
                speculative: false,
            }
        }
    };
    let patterns = check.patterns().to_vec();
    match check {
        Check::Status { json, plain, .. } => {
            status(setup, engine, &selection, &patterns, json, plain, signals).await
        }
        Check::Diff { host, build, json } => {
            diff(setup, engine, &selection, &host, build, json, signals).await
        }
        Check::Activate { yes, force, .. } => {
            activate(setup, engine, &selection, &patterns, yes, force, signals).await
        }
        Check::Hosts { .. } => Ok(0),
    }
}

/// The hosts to check from the cache — those the flake had the last time
/// discovery read them — so that checking starts at once, while
/// [`confirm_hosts`] reads them again beside it. `None` (they are read
/// first) without such a list, for `hosts` (which shows just them), and
/// when the command's patterns don't pick them as they should.
fn speculate(setup: &Setup, engine: &Engine, check: &Check) -> Option<Selection> {
    if let Check::Hosts { .. } = check {
        return None;
    }
    let flake = discovery_flake(&setup.config)?;
    let cached = engine.cached_hosts(flake)?;
    let resolved = setup
        .config
        .resolve_hosts(Some(&cached), &setup.local)
        .ok()?;
    let hosts = selected(setup, resolved.hosts, check.patterns()).ok()?;
    if let Check::Diff { .. } = check
        && hosts.len() != 1
    {
        return None;
    }
    for warning in &resolved.warnings {
        warn(warning);
    }
    Some(Selection {
        hosts: hosts.into_iter().enumerate().collect(),
        known: Vec::new(),
        speculative: true,
    })
}

/// Discovery beside a check of hosts from the cache ([`speculate`]): what
/// the hosts are now, found as [`dispatch`] finds them —
/// [`live::Confirmed::Same`], or a check to start over with — or why there
/// are none to check (a diff needs exactly one host). Warnings go to
/// `deferred`, to be printed once the check is done rather than into its
/// table.
async fn confirm_hosts(
    setup: &Setup,
    engine: &Engine,
    patterns: &[String],
    purpose: Purpose<'_>,
    speculative: &[(HostId, Host)],
    deferred: &std::cell::RefCell<Vec<String>>,
) -> Result<Confirmed, Exit> {
    let discovered = match discovery_flake(&setup.config) {
        Some(flake) => Some(engine.discover(flake).await),
        None => None,
    };
    let (resolved, known, warning) = hosts_of(setup, discovered)?;
    let Resolved { hosts, warnings } = resolved;
    let hosts = selected(setup, hosts, patterns)?;
    if hosts.iter().eq(speculative.iter().map(|(_, host)| host)) {
        return Ok(Confirmed::Same);
    }
    let selection = Selection {
        hosts: hosts.into_iter().enumerate().collect(),
        known,
        speculative: false,
    };
    if let Purpose::Diff(pattern) = purpose {
        one_host(&selection.hosts, pattern)?;
    }
    deferred
        .borrow_mut()
        .extend(warning.into_iter().chain(warnings));
    Ok(Confirmed::Changed(Box::new(Recheck {
        state: selection.state(setup, purpose),
        hosts: selection.hosts,
        known: selection.known,
    })))
}

/// What a check is for.
#[derive(Debug, Clone, Copy)]
enum Purpose<'a> {
    Status,
    /// A diff of the one host this pattern names.
    Diff(&'a str),
    /// Activations: the table doesn't suggest them again.
    Activate,
}

/// [`live::check`] of `selection` — with [`confirm_hosts`] beside it when
/// its hosts came from the cache, and then the warnings found on the way.
async fn check_selection(
    setup: &Setup,
    engine: &Engine,
    selection: &Selection,
    patterns: &[String],
    purpose: Purpose<'_>,
    display: Display,
    signals: &mut Signals,
) -> Result<AppState, Exit> {
    let trouble = |e: anyhow::Error| Exit::Trouble(format!("{e:#}"));
    let (hosts, known) = (&selection.hosts, &selection.known);
    let state = selection.state(setup, purpose);
    if !selection.speculative {
        let checked = live::check(engine, state, hosts, known, display, signals, setup.started);
        return checked.await.map_err(trouble);
    }
    let deferred = std::cell::RefCell::new(Vec::new());
    let confirmed = confirm_hosts(setup, engine, patterns, purpose, hosts, &deferred);
    let started = setup.started;
    let checked = live::check_confirming(
        engine, state, hosts, known, display, signals, started, confirmed,
    )
    .await
    .map_err(trouble)?;
    for warning in deferred.into_inner() {
        warn(&warning);
    }
    checked
}

/// The hosts to check: discovery and the flake's inventory (when a flake
/// is known and either is on), then the configuration. Also the flakes
/// discovery resolved.
async fn find_hosts(
    setup: &Setup,
    engine: &Engine,
    signals: &mut Signals,
) -> Result<(Resolved, Vec<FlakeInfo>), Exit> {
    let discovered = match discovery_flake(&setup.config) {
        Some(flake) => {
            let label = format!("reading the hosts of {flake}");
            let discovered = live::wait_for(&label, engine.discover(flake), signals).await;
            Some(discovered.ok_or(Exit::Interrupted)?)
        }
        None => None,
    };
    let (resolved, known, warning) = hosts_of(setup, discovered)?;
    if let Some(warning) = warning {
        warn(&warning);
    }
    Ok((resolved, known))
}

/// The flake whose hosts discovery reads: the default one, when discovery
/// or the inventory is on.
fn discovery_flake(config: &Config) -> Option<&str> {
    let reads = config.discover || !config.inventory.is_empty();
    config.flake.as_deref().filter(|_| reads)
}

/// The hosts, given what discovery of the [`discovery_flake`] came to
/// (`None`: there is none), with the configuration's on top; also the flakes
/// discovery resolved, and a warning when its hosts couldn't be read but
/// configured ones can be checked.
fn hosts_of(
    setup: &Setup,
    discovered: Option<Result<(FlakeInfo, FlakeHosts), ErrorInfo>>,
) -> Result<(Resolved, Vec<FlakeInfo>, Option<String>), Exit> {
    let config = &setup.config;
    let (mut known, mut flake_hosts, mut warning) = (Vec::new(), None, None);
    if let (Some(flake), Some(discovered)) = (discovery_flake(config), discovered) {
        let note = setup.flake_here_note();
        match discovered {
            Ok((info, hosts)) => {
                known.push(info);
                flake_hosts = Some(hosts);
            }
            Err(e) if !config.hosts.is_empty() => {
                warning = Some(format!(
                    "{}; checking the configured hosts only{}",
                    hosts_error(flake, &e.summary),
                    note.map(|(why, _)| format!(" ({why})")).unwrap_or_default()
                ));
            }
            Err(e) => {
                let mut message = hosts_error(flake, &describe(&e));
                if let Some((why, what)) = note {
                    message.push_str(&format!("\nhint: {why}; {what}"));
                }
                return Err(Exit::Usage(message));
            }
        }
    }
    let resolved = config
        .resolve_hosts(flake_hosts.as_ref(), &setup.local)
        .map_err(|e| Exit::Usage(format!("{e:#}")))?;
    Ok((resolved, known, warning))
}

/// The hosts `patterns` pick of `hosts` (every pattern has to match one),
/// or why there are none to check.
fn selected(setup: &Setup, hosts: Vec<Host>, patterns: &[String]) -> Result<Vec<Host>, Exit> {
    if hosts.is_empty() {
        return Err(Exit::Usage(no_hosts(
            &setup.config,
            setup.path.as_deref(),
            setup.flake_here_note(),
        )));
    }
    select(hosts, patterns).map_err(Exit::Usage)
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

/// What to say when there are no hosts at all; `here` is
/// [`Setup::flake_here_note`] (the flake is the current directory's).
fn no_hosts(config: &Config, path: Option<&Path>, here: Option<(String, String)>) -> String {
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
    let nor = if config.inventory.is_empty() {
        String::new()
    } else {
        format!(" (nor {}.hosts)", config.inventory)
    };
    match (config.flake.as_deref(), here) {
        (Some(_), Some((why, what))) if config.discover => format!(
            "no hosts{excluded}: {why}: it has no nixosConfigurations, darwinConfigurations or \
             homeConfigurations{nor}; {what}"
        ),
        (Some(flake), _) if config.discover => format!(
            "no hosts{excluded}: {flake} has no nixosConfigurations, darwinConfigurations or \
             homeConfigurations{nor}; {tables}, or point --flake at another flake"
        ),
        (Some(_), _) => format!("no hosts{excluded}: discover = false; {tables}"),
        (None, _) => format!(
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
    /// The hosts are the cache's ([`speculate`]): to be confirmed beside
    /// the check.
    speculative: bool,
}

impl Selection {
    fn state(&self, setup: &Setup, purpose: Purpose<'_>) -> AppState {
        let hosts = self.hosts.iter().map(|(_, host)| host.clone()).collect();
        let mut state = AppState::new(hosts).with_flakes(&self.known, Instant::now());
        state.config_path.clone_from(&setup.path);
        state.command.clone_from(&setup.command);
        state.suggest_activate = !matches!(purpose, Purpose::Activate);
        state
    }
}

async fn status(
    setup: &Setup,
    engine: &Engine,
    selection: &Selection,
    patterns: &[String],
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
    let checked = check_selection(
        setup,
        engine,
        selection,
        patterns,
        Purpose::Status,
        display,
        signals,
    );
    let state = checked.await?;
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
    let id = one_host(&selection.hosts, pattern)?;
    let patterns = [pattern.to_string()];
    let checked = check_selection(
        setup,
        engine,
        selection,
        &patterns,
        Purpose::Diff(pattern),
        Display::Progress,
        signals,
    );
    let mut state = checked.await?;
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
            desired_missing(host, &setup.command)
        })),
        Err(e) => Err(Exit::Trouble(format!(
            "can't diff {name}: {}",
            describe(&e)
        ))),
    }
}

/// Why `diff` has no desired closure to compare: it is in none of the
/// stores the diff looked in (this machine's, and the host's when it has
/// a shell), which doesn't mean it was never built; and how to fetch or
/// build it (`hostctl` is how to run hostctl again in this context).
fn desired_missing(host: &Host, hostctl: &str) -> String {
    let name = util::one_line(&host.name);
    let listed = host.ssh().is_some_and(|ssh| ssh.forced_command.is_none());
    let stores = if listed {
        format!("this machine's or {name}'s store")
    } else {
        "this machine's store".into()
    };
    format!(
        "the new generation of {name} isn't in {stores} yet (a binary cache may have it): \
         `{hostctl} diff --build {}` fetches or builds it {}",
        remote::sh_quote(&name),
        build_place(host)
    )
}

/// The host of a diff's selection, which needs exactly one, or which hosts
/// its `pattern` picks instead.
fn one_host(hosts: &[(HostId, Host)], pattern: &str) -> Result<HostId, Exit> {
    if let [(id, _)] = hosts {
        return Ok(*id);
    }
    let names: Vec<_> = hosts.iter().map(|(_, h)| util::one_line(&h.name)).collect();
    Err(Exit::Usage(format!(
        "diff needs exactly one host, but {pattern:?} matches {}",
        names.join(", ")
    )))
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
    let prepared = live::wait_for(&label, engine.prepare_interactive(host, &command), signals)
        .await
        .ok_or(Exit::Interrupted)?
        .map_err(|e| Exit::Trouble(format!("can't build for {name}: {}", describe(&e))))?;
    let spec = if json {
        stdout_to_stderr(prepared.spec)
    } else {
        prepared.spec
    };
    match run_prepared(engine, host, &spec, prepared.upload.as_deref(), signals).await? {
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

/// [`interactive`] for a command [`Engine::prepare_interactive`] prepared
/// for `host`. The script it `upload`ed removes itself once it runs; when
/// the run may not have got that far (it failed, couldn't connect, was cut
/// short), hostctl removes it.
async fn run_prepared(
    engine: &Engine,
    host: &Host,
    spec: &CommandSpec,
    upload: Option<&str>,
    signals: &mut Signals,
) -> Result<io::Result<ExitStatus>, Exit> {
    let ran = interactive(spec, signals).await;
    if let Some(path) = upload
        && !matches!(&ran, Ok(Ok(status)) if status.success())
    {
        engine.discard_upload(host, path).await;
    }
    ran
}

async fn activate(
    setup: &Setup,
    engine: &Engine,
    selection: &Selection,
    patterns: &[String],
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
    let checked = check_selection(
        setup,
        engine,
        selection,
        patterns,
        Purpose::Activate,
        display,
        signals,
    );
    let mut state = checked.await?;
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
        print(&format!("\n{}\n", activating(&host, activation, style)))?;
        let label = format!("preparing the activation of {name}");
        let prepared = live::wait_for(
            &label,
            engine.prepare_interactive(&host, &activation.command),
            signals,
        )
        .await
        .ok_or(Exit::Interrupted)?;
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(e) => {
                eprint(&format!(
                    "error: can't run the activation of {name}: {}\n",
                    describe(&e)
                ));
                continue;
            }
        };
        let upload = prepared.upload.as_deref();
        let ran = run_prepared(engine, &host, &prepared.spec, upload, signals).await?;
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
        print(&outcome_text(h, &ran, &activation.command, style))?;
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
    let mut activation = actions::activation(&h.host, flake, &desired.path)?;
    if let Some(back) = h.going_back() {
        activation.warning = Some(match activation.warning.take() {
            Some(warning) => format!("{warning}; {back}"),
            None => back,
        });
    }
    Ok(activation)
}

/// What to know before activating `host`, if anything: the command when
/// hostctl didn't write it (a template from the config or the flake's
/// inventory; continuation lines indented), that it runs here rather than
/// on the host, and any warning. hostctl's own commands are shown only if
/// they fail (and in `status --json`).
fn plan_text(host: &Host, activation: &Activation) -> String {
    let name = util::one_line(&host.name);
    let mut text = String::new();
    if host.activate.as_deref() != host.kind.default_activate() {
        text.push_str(&format!(
            "{name}: {}\n",
            command_text(&activation.command, "")
        ));
    }
    if runs_elsewhere(host, activation) {
        text.push_str(&format!("{name}: activated from here\n"));
    }
    if let Some(warning) = &activation.warning {
        text.push_str(&format!("{name}: warning: {}\n", util::one_line(warning)));
    }
    text
}

/// A remote host's activation runs on this machine (`activate-on =
/// "local"`).
fn runs_elsewhere(host: &Host, activation: &Activation) -> bool {
    activation.runs_on == RunsOn::Here && host.ssh().is_some()
}

/// `==> activating <name>` (` from here` when it runs here for a remote
/// host), in bold.
fn activating(host: &Host, activation: &Activation, style: Style) -> String {
    let from = if runs_elsewhere(host, activation) {
        " from here"
    } else {
        ""
    };
    output::bold(
        style,
        &format!("==> activating {}{from}", util::one_line(&host.name)),
    )
}

/// Why `activate` has nothing to do.
fn nothing_to_activate(state: &AppState) -> String {
    let unchecked: Vec<_> = state
        .hosts
        .iter()
        .filter(|h| {
            !matches!(
                h.status(),
                Status::UpToDate | Status::Offline | Status::Ahead
            )
        })
        .map(|h| format!("{} ({})", util::one_line(&h.host.name), h.status().label()))
        .collect();
    let ahead: Vec<_> = state
        .hosts
        .iter()
        .filter(|h| h.status() == Status::Ahead)
        .map(|h| util::one_line(&h.host.name).into_owned())
        .collect();
    if unchecked.is_empty() && !ahead.is_empty() {
        format!(
            "\nnothing to activate: {} ahead of the compared revision (--force goes back)\n",
            ahead.join(", ")
        )
    } else if unchecked.is_empty() {
        "\nnothing to activate: up to date (--force activates anyway)\n".into()
    } else {
        format!(
            "\nnothing to activate: {} couldn't be checked (--force tries anyway)\n",
            unchecked.join(", ")
        )
    }
}

/// The result of one activation of `command`, after its host was probed
/// again: its status, and why the activation failed (with the command it
/// ran) if it did.
fn outcome_text(
    h: &HostState,
    ran: &io::Result<ExitStatus>,
    command: &str,
    style: Style,
) -> String {
    let name = util::one_line(&h.host.name);
    let status = h.status();
    let icon = output::status_icon(style, status);
    let failed = match ran {
        Ok(exit) if exit.success() => None,
        Ok(exit) => Some(format!(
            "the activation failed ({})",
            exit_status_text(exit)
        )),
        Err(e) => Some(format!("can't run the activation: {e}")),
    };
    let mut text = match (status, h.probe.get(), &failed) {
        (Status::UpToDate, _, None) => format!("{icon} {name} is up to date\n"),
        (Status::UpToDate, _, Some(failed)) => {
            format!("{icon} {name} is up to date, but {failed}\n")
        }
        (_, Some(Err(failure)), _) => {
            let probe = format!(
                "{icon} {name}: {}: {}\n",
                status.label(),
                util::one_line(&failure.error.summary)
            );
            match &failed {
                Some(failed) => format!("{name}: {failed}\n{probe}"),
                None => probe,
            }
        }
        (_, _, Some(failed)) => format!("{icon} {name}: {failed}\n"),
        (_, _, None) => format!("{icon} {name} is still {}\n", status.label()),
    };
    if matches!(ran, Ok(exit) if !exit.success()) {
        text.push_str(&output::dim(
            style,
            &format!("  it ran: {}", command_text(command, "  ")),
        ));
        text.push('\n');
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
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    // Reading a terminal blocks: a thread of its own, which the exit
    // doesn't wait for if the answer never comes.
    let asked = question.clone();
    std::thread::Builder::new()
        .name("hostctl-confirm".into())
        .spawn(move || {
            let _ = tx.send(ask(&asked));
        })
        .map_err(|e| Exit::Trouble(format!("can't ask for confirmation: {e}")))?;
    loop {
        tokio::select! {
            answer = &mut rx => return match answer {
                Ok(Ok(yes)) => Ok(yes),
                Ok(Err(e)) => Err(Exit::Usage(format!(
                    "can't ask for confirmation ({e}): pass --yes to activate without asking"
                ))),
                Err(_) => Err(Exit::Trouble("the confirmation prompt went away".into())),
            },
            received = signals.recv_all() => match received {
                // Ctrl-Z: the shell has had the terminal since; ask again.
                Received::Suspend => {
                    live::stop();
                    if let Ok(tty) = File::options().write(true).open("/dev/tty") {
                        let _ = (&tty).write_all(prompt(&question).as_bytes());
                    }
                }
                Received::Sig(_) => {
                    eprint("\n");
                    return Err(Exit::Interrupted);
                }
            },
        }
    }
}

/// `question [y/N] `.
fn prompt(question: &str) -> String {
    format!("{question} [y/N] ")
}

/// [`prompt`] on `/dev/tty`, and the answer read from it.
fn ask(question: &str) -> io::Result<bool> {
    let tty = File::options().read(true).write(true).open("/dev/tty")?;
    (&tty).write_all(prompt(question).as_bytes())?;
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

/// What `COMPLETE=<shell> hostctl` prints: shell code that asks hostctl
/// (whichever is on `PATH`, so it always matches) for completions.
fn completion_script(shell: clap_complete::Shell) -> String {
    let mut script = Vec::new();
    let shells = clap_complete::env::Shells::builtins();
    if let Some(completer) = shells.completer(&shell.to_string()) {
        // Writing into a Vec can't fail.
        let _ =
            completer.write_registration("COMPLETE", "hostctl", "hostctl", "hostctl", &mut script);
    }
    let mut script = String::from_utf8_lossy(&script).into_owned();
    if shell == clap_complete::Shell::Zsh {
        // Autoloaded from $fpath as `_hostctl`, the file is the completion
        // function: complete right away, not only from the next TAB on.
        script.push_str(
            "[[ ${funcstack[1]-} == _hostctl ]] && _clap_dynamic_completer_hostctl \"$@\"\n",
        );
    }
    script
}

/// Host names (and `@local`) starting with `current`, for the shell.
fn complete_hosts(current: &std::ffi::OsStr) -> Vec<CompletionCandidate> {
    let prefix = current.to_string_lossy();
    let mut names = completion_hosts();
    names.push("@local".into());
    names
        .into_iter()
        .filter(|name| name.starts_with(&*prefix))
        .map(CompletionCandidate::new)
        .collect()
}

/// The hosts `hostctl hosts --offline` would list, from the configuration
/// and the cache only: completing must be quick, and never wait on the
/// network. Nothing when they can't be found.
fn completion_hosts() -> Vec<String> {
    let Ok(cli) = Cli::try_parse_from(["hostctl"]) else {
        return Vec::new();
    };
    let Ok(setup) = Setup::load(&cli.global, "hostctl", Instant::now()) else {
        return Vec::new();
    };
    let config = &setup.config;
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return Vec::new();
    };
    let discovered = runtime.block_on(async {
        let engine = Engine::new(config, open_cache(config), Freshness::Offline);
        let flake = discovery_flake(config)?;
        engine.discover(flake).await.ok().map(|(_, hosts)| hosts)
    });
    config
        .resolve_hosts(discovered.as_ref(), &setup.local)
        .map(|resolved| resolved.hosts.into_iter().map(|host| host.name).collect())
        .unwrap_or_default()
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
        let Some(Cmd::Status(StatusArgs { hosts, json, plain })) = cli.command else {
            panic!("{cli:?}")
        };
        assert_eq!(hosts, ["al2-*", "@local"]);
        assert!(json && !plain);
        // Without a command, the same is `status`'s.
        let cli = parse(&["al2-*", "--json"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.status.hosts, ["al2-*"]);
        assert!(cli.status.json);

        let cli = parse(&["diff", "bigmem", "--build"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Cmd::Diff { ref host, build: true, json: false }) if host == "bigmem"
        ));

        let cli = parse(&["activate", "-y", "a", "b", "--force"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Cmd::Activate { ref hosts, all: false, yes: true, force: true }) if hosts.len() == 2
        ));
        let cli = parse(&["activate", "--all"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Cmd::Activate { ref hosts, all: true, .. }) if hosts.is_empty()
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
    fn suggested_commands_keep_the_flags_that_choose_the_hosts() {
        let cwd = Path::new("/home/jj/src");
        let prefix = |args: &[&str]| {
            let args = std::iter::once("hostctl").chain(args.iter().copied());
            let (cli, given) = super::parse(args.map(std::ffi::OsString::from)).unwrap();
            command_prefix(&cli.global, given, Some(cwd))
        };
        assert_eq!(prefix(&["status"]), "hostctl");
        assert_eq!(
            prefix(&["-c", "hosts.toml", "status"]),
            "hostctl -c /home/jj/src/hosts.toml"
        );
        // Wherever the flags go: relative paths made absolute, values quoted.
        assert_eq!(
            prefix(&[
                "status",
                "--self",
                "my desk",
                "-f",
                ".",
                "--config",
                "./a b.toml"
            ]),
            "hostctl -c '/home/jj/src/a b.toml' -f /home/jj/src --self 'my desk'"
        );
        assert_eq!(
            prefix(&["-f", "../nix", "-c", "/etc/hostctl/config.toml", "status"]),
            "hostctl -c /etc/hostctl/config.toml -f /home/jj/src/../nix"
        );
        // Flake references other than paths mean the same anywhere; the other
        // flags don't change which hosts a command is about.
        assert_eq!(
            prefix(&[
                "-f",
                "github:jj/nix-config",
                "-j",
                "3",
                "--refresh",
                "--no-cache"
            ]),
            "hostctl -f github:jj/nix-config"
        );
        assert_eq!(prefix(&["-f", "", "--self", ""]), "hostctl");
    }

    #[test]
    fn usage_errors_exit_3_and_help_exits_0() {
        for bad in [
            &["--bogus"][..],
            &["activate"],
            &["activate", "--all", "a"],
            &["diff"],
            &["status", "--refresh", "--offline"],
            &["--color", "sometimes"],
            &["--jobs", "many"],
            &["completions", "tcsh"],
            &["config"],
        ] {
            let e = parse(bad).unwrap_err();
            assert_eq!(clap_exit_code(&e), 3, "{bad:?}: {e}");
        }
        // Not a command: a pattern for `status` (which fails if it matches
        // no host).
        assert_eq!(parse(&["frobnicate"]).unwrap().status.hosts, ["frobnicate"]);
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
    fn checks_know_their_patterns() {
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
        let none = no_hosts(&config, Some(file), None);
        assert!(
            none.starts_with("no hosts to check: point --flake"),
            "{none}"
        );
        assert!(none.contains("[hosts.<name>] tables to /home/u/.config/hostctl/config.toml"));

        config.flake = Some("github:o/r".into());
        let empty = no_hosts(&config, Some(file), None);
        assert!(
            empty.starts_with("no hosts: github:o/r has no nixosConfigurations"),
            "{empty}"
        );
        assert!(empty.contains("(nor lib.hostctl.hosts)"), "{empty}");

        config.discover = false;
        config.exclude = vec!["*-iso".into()];
        assert_eq!(
            no_hosts(&config, Some(file), None),
            "no hosts (exclude drops *-iso): discover = false; \
             add [hosts.<name>] tables to /home/u/.config/hostctl/config.toml"
        );
    }

    #[test]
    fn a_flake_found_in_the_current_directory_is_explained() {
        let mut setup = Setup {
            config: Config {
                flake: Some("/home/u/src/proj".into()),
                ..Config::default()
            },
            path: Some(PathBuf::from("/home/u/.config/hostctl/config.toml")),
            local: LocalInfo {
                hostname: "b".into(),
                user: "u".into(),
                os: crate::local::Os::Linux,
                forced: None,
            },
            freshness: Freshness::Auto,
            color: false,
            command: "hostctl -f /home/u/src/proj".into(),
            flake_here: true,
            started: Instant::now(),
        };
        let note = setup.flake_here_note().unwrap();
        assert_eq!(
            note,
            (
                "/home/u/.config/hostctl/config.toml sets no flake, so ./flake.nix \
                 (/home/u/src/proj) was used"
                    .to_string(),
                "set flake = \"…\" in /home/u/.config/hostctl/config.toml, or pass --flake"
                    .to_string()
            )
        );
        assert_eq!(
            no_hosts(&setup.config, setup.path.as_deref(), Some(note)),
            "no hosts: /home/u/.config/hostctl/config.toml sets no flake, so ./flake.nix \
             (/home/u/src/proj) was used: it has no nixosConfigurations, darwinConfigurations or \
             homeConfigurations (nor lib.hostctl.hosts); set flake = \"…\" in \
             /home/u/.config/hostctl/config.toml, or pass --flake"
        );
        setup.path = None;
        let (why, what) = setup.flake_here_note().unwrap();
        assert!(why.starts_with("no config file (looked in "), "{why}");
        assert!(
            why.ends_with(") and no flake set, so ./flake.nix (/home/u/src/proj) was used"),
            "{why}"
        );
        assert_eq!(what, "pass -c FILE or --flake, or set HOSTCTL_CONFIG");
        // A flake that was set needs no explaining.
        setup.flake_here = false;
        assert_eq!(setup.flake_here_note(), None);
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
    fn a_flake_path_nix_would_misread_becomes_a_url() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap().to_string();
        let flake = |path: &Path| {
            std::fs::create_dir_all(path).unwrap();
            std::fs::write(path.join("flake.nix"), "{ outputs = _: { }; }").unwrap();
            flake_here(Some(path)).unwrap()
        };
        let encoded = percent_encode(&base);
        assert_eq!(
            flake(&tmp.path().join("c#-100% ok?")),
            format!("path:{encoded}/c%23-100%25%20ok%3F")
        );
        // Spaces alone don't need it.
        assert_eq!(
            flake(&tmp.path().join("my flake")),
            format!("{base}/my flake")
        );
        // In a git work tree, what nix makes of the path: the repository and
        // the flake's directory in it.
        let repo = tmp.path().join("nix#config");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        assert_eq!(flake(&repo), format!("git+file://{encoded}/nix%23config"));
        assert_eq!(
            flake(&repo.join("hosts/a b")),
            format!("git+file://{encoded}/nix%23config?dir=hosts/a%20b")
        );
        std::fs::write(repo.join(".git/shallow"), "").unwrap();
        assert_eq!(
            flake(&repo.join("hosts")),
            format!("git+file://{encoded}/nix%23config?dir=hosts&shallow=1")
        );
        assert_eq!(percent_encode("a-z_0.9~/x"), "a-z_0.9~/x");
        assert_eq!(percent_encode("é&="), "%C3%A9%26%3D");
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
            assert!(script.contains("COMPLETE"), "{shell}: asks hostctl");
            assert!(script.contains("hostctl"), "{shell}");
        }
    }

    #[test]
    fn the_state_starts_with_the_known_flakes() {
        let selection = Selection {
            hosts: vec![(0, host("a")), (1, local("b", false))],
            known: vec![info()],
            speculative: false,
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
            command: "hostctl -c /etc/hostctl/config.toml".into(),
            flake_here: false,
            started: Instant::now(),
        };
        let state = selection.state(&setup, Purpose::Status);
        assert!(state.suggest_activate);
        assert!(!selection.state(&setup, Purpose::Activate).suggest_activate);
        assert_eq!(state.hosts.len(), 2);
        assert_eq!(
            state.config_path.as_deref(),
            Some(Path::new("/etc/hostctl/config.toml"))
        );
        assert_eq!(state.command, "hostctl -c /etc/hostctl/config.toml");
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
                looking_up: false,
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
                    last_modified: None,
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
    fn a_missing_new_generation_may_just_not_be_fetched_yet() {
        assert_eq!(
            desired_missing(&host("web1"), "hostctl -c /etc/hosts.toml"),
            "the new generation of web1 isn't in this machine's or web1's store yet (a binary \
             cache may have it): `hostctl -c /etc/hosts.toml diff --build web1` fetches or \
             builds it on web1"
        );
        assert_eq!(
            desired_missing(&local("bigmem", false), "hostctl"),
            "the new generation of bigmem isn't in this machine's store yet (a binary cache may \
             have it): `hostctl diff --build bigmem` fetches or builds it here"
        );
        let mut pushed = host("web2");
        pushed.activate_on = ActivateOn::Local;
        assert!(
            desired_missing(&pushed, "hostctl").ends_with(
                "in this machine's or web2's store yet (a binary cache may have it): \
                 `hostctl diff --build web2` fetches or builds it here"
            ),
            "{}",
            desired_missing(&pushed, "hostctl")
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
        // hostctl's own command needn't be shown before asking.
        assert_eq!(plan_text(&state.hosts[0].host, &activation), "");
        let mut custom = state.hosts[0].host.clone();
        custom.activate = Some("nix run {flake}#activate".into());
        custom.activate_on = ActivateOn::Local;
        let local = Activation {
            runs_on: RunsOn::Here,
            warning: Some("the flake is a dirty tree".into()),
            ..activation.clone()
        };
        assert_eq!(
            plan_text(&custom, &local),
            format!(
                "a: {}\na: activated from here\na: warning: the flake is a dirty tree\n",
                activation.command
            )
        );
        assert_eq!(
            activating(&custom, &local, Style { color: false }),
            "==> activating a from here"
        );
        assert_eq!(
            activating(&state.hosts[0].host, &activation, Style { color: true }),
            "\x1b[1m==> activating a\x1b[0m"
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
    fn where_builds_run() {
        let mut desk = host("desk");
        if let Target::Ssh(ssh) = &mut desk.target {
            ssh.destination = "jj@desk.example.com".into();
        }
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
        fn outcome(h: &HostState, ran: &io::Result<ExitStatus>) -> String {
            outcome_text(h, ran, "switch\nto it", Style { color: false })
        }
        let ok = ExitStatus::from_raw(0);
        assert_eq!(outcome(&state.hosts[0], &Ok(ok)), "✓ a is up to date\n");
        let failed = ExitStatus::from_raw(1 << 8);
        assert_eq!(
            outcome(&state.hosts[0], &Ok(failed)),
            "✓ a is up to date, but the activation failed (exit 1)\n\
             \x20 it ran: switch\n      to it\n"
        );
        assert_eq!(
            outcome_text(&state.hosts[0], &Ok(ok), "", Style { color: true }),
            "\x1b[32m✓\x1b[0m a is up to date\n"
        );

        probed(&mut state, 1, Ok(at('a')));
        evaluated(&mut state, 1, Ok(store('b')));
        assert_eq!(
            outcome(&state.hosts[1], &Ok(failed)),
            "✗ b: the activation failed (exit 1)\n  it ran: switch\n      to it\n"
        );
        assert_eq!(outcome(&state.hosts[1], &Ok(ok)), "✗ b is still outdated\n");

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
        let text = outcome(&state.hosts[2], &missing);
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
                 {flat}: echo one\n    echo two\n    web3: rm -rf /\n"
            )
        );
        assert_eq!(
            question(&state, &plan.activations),
            format!("Activate {flat}?")
        );
        let plain = Style { color: false };
        let (_, activation) = &plan.activations[0];
        assert_eq!(
            activating(&state.hosts[0].host, activation, plain),
            format!("==> activating {flat}")
        );
        let failed = Ok(ExitStatus::from_raw(1 << 8));
        let outcome = outcome_text(&state.hosts[0], &failed, &activation.command, plain);
        assert!(
            lines_not_starting_with(&outcome, &["✗ evil web2: ", "  it ran: ", "      "])
                .is_empty(),
            "{outcome:?}"
        );
        let outcome = outcome_text(&state.hosts[1], &Ok(ExitStatus::from_raw(0)), "", plain);
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
