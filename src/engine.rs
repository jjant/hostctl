//! Orchestration: probes, flake resolutions and evaluations run in parallel
//! and report [`Event`]s as results arrive. See DESIGN.md ("Architecture",
//! "SSH", "Flake resolution and evaluation", "Cache").
//!
//! Everything a check does happens inside its future (no spawned tasks), so
//! dropping the future cancels the work, which kills the children. The cache
//! is only ever locked briefly, never across an `.await`.

use std::borrow::Cow;
use std::cmp::{Ordering as CmpOrdering, Reverse};
use std::collections::{BinaryHeap, HashMap};
use std::fs::{DirBuilder, Permissions};
use std::io;
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use futures_util::StreamExt as _;
use futures_util::future::{BoxFuture, FutureExt as _, Shared, WeakShared};
use futures_util::stream::FuturesUnordered;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::{OnceCell, Semaphore, SemaphorePermit};
use tokio::time::Instant;

use crate::actions;
use crate::cache::{Cache, DeployedRev, Hint};
use crate::closure::{self, ClosureEntry};
use crate::config::{Config, FlakeHosts, Host, SshConfig, SshTarget, Target};
use crate::exec::Output;
use crate::model::{
    Desired, DiffOutcome, DiffResult, ErrorInfo, EvalResult, Event, HostId, ProbeError,
    ProbeFailure, ProbeResult,
};
use crate::nix::{self, FlakeInfo};
use crate::probe::{self, Mode, Parsed};
use crate::remote::{self, CommandSpec, Mux};
use crate::storepath::StorePath;
use crate::util;

pub type EventTx = UnboundedSender<Event>;

/// How every process is run: [`crate::exec::run`] in production, a scripted
/// fake in tests.
pub type Runner = Arc<
    dyn Fn(CommandSpec, Option<Duration>) -> BoxFuture<'static, std::io::Result<Output>>
        + Send
        + Sync,
>;

/// At most this many ssh connections without a live control socket are
/// opened at once (sshd's default `MaxStartups` starts dropping at 10).
pub const MAX_NEW_CONNECTIONS: usize = 10;

/// `nix --version` gets this long.
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

/// How long looking up a revision's commit time may take (it may fetch the
/// revision; [`Engine::date_revisions`]).
const DATING_TIMEOUT: Duration = Duration::from_secs(20);

/// How much longer than its probes and evaluations a check may take to date
/// revisions.
const DATING_BUDGET: Duration = Duration::from_secs(30);

/// Revision lookups at once (each may download a tarball).
const DATING_JOBS: usize = 4;

/// [`Engine::discard_upload`] gets at most this long: it is only tidying up.
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

/// A closure listing gets at least this long (or the probe timeout, if
/// longer): it reads whole closures, possibly over a slow link.
const MIN_LISTING_TIMEOUT: Duration = Duration::from_secs(60);

/// `ssh -O exit` gets this long: a master answers it at once, even one
/// whose connection is dead.
const EXIT_MASTER_TIMEOUT: Duration = Duration::from_secs(2);

/// A transient ssh failure is retried after this many milliseconds,
/// jittered so hosts that sshd's MaxStartups dropped together don't come
/// back together.
const RETRY_DELAY_MS: (u64, u64) = (100, 500);

/// How an ssh older than OpenSSH 7.6 rejects hostctl's
/// `StrictHostKeyChecking=accept-new`.
const ACCEPT_NEW_UNSUPPORTED: &str = r#"unsupported option "accept-new""#;

/// Passed instead, once an ssh rejected `accept-new`: user options come
/// first, so hostctl's default is dropped (see `remote::ssh_argv`).
const STRICT_HOST_KEYS: [&str; 2] = ["-o", "StrictHostKeyChecking=yes"];

/// For GitHub API rate limits (60 unauthenticated requests per hour).
const TOKEN_HINT: &str = "configure access-tokens = github.com=<token> in nix.conf";

/// What a probe that started answering but never finished means.
const INCOMPLETE: &str = "connection dropped or timed out after the host answered";

/// Bytes of a failed command's (cleaned) stderr kept in an error's detail;
/// the end, where the final error is.
const MAX_DETAIL_STDERR: usize = 16 << 10;

/// How much to trust cached flake resolutions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Freshness {
    /// Reuse a resolution younger than `nix.refresh`, else `--refresh`.
    #[default]
    Auto,
    /// Always `--refresh` (`--refresh`, the TUI's `r`).
    Force,
    /// Never touch the network (`--offline`): no `nix flake metadata`, no
    /// evaluation; cached resolutions, results and host lists only. A flake
    /// without a cached resolution fails, and so does whatever its results
    /// lack ([`NOT_CACHED`]).
    Offline,
}

/// The summary of an evaluation (or discovery) [`Freshness::Offline`] can't
/// do because its result isn't cached.
pub const NOT_CACHED: &str = "not cached; run hostctl once without --offline";

/// The detail of [`NOT_CACHED`].
const NOT_CACHED_DETAIL: &str = "--offline evaluates nothing: it only uses what earlier runs \
                                 cached (and nothing is cached for dirty trees, outdated lock \
                                 files, impure nix.options or with the cache off)";

/// Runs checks. Cheap to clone; clones share the cache and the limits.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
    /// Per handle, so that [`Engine::with_freshness`] can force a refresh
    /// while sharing everything else.
    freshness: Freshness,
}

struct Inner {
    config: Config,
    runner: Runner,
    cache: Mutex<Cache>,
    /// Evaluation slots (`config.jobs()`).
    evals: Semaphore,
    /// New-connection slots ([`MAX_NEW_CONNECTIONS`]).
    connections: Semaphore,
    /// hostctl's private control socket directory, or `None` when
    /// multiplexing is off/unavailable.
    control_dir: Option<PathBuf>,
    /// Whether `control_dir` exists with mode 0700: made on first remote use.
    control_dir_ready: OnceLock<bool>,
    /// First line of `nix --version`, once nix could be run (`None` if that
    /// printed no version: no caching then). A spawn failure isn't kept,
    /// so the next check tries again.
    nix_version: OnceCell<Option<String>>,
    /// An ssh rejected `StrictHostKeyChecking=accept-new` (OpenSSH < 7.6):
    /// every later ssh run passes [`STRICT_HOST_KEYS`].
    strict_host_keys: AtomicBool,
    /// Resolutions in flight, by flake and [`Freshness`]: a check and the
    /// discovery running beside it share one `nix flake metadata`. Weak:
    /// once nobody awaits one, it is gone (and its process killed).
    resolving: Mutex<HashMap<(String, Freshness), WeakShared<ResolveFuture>>>,
}

type ResolveResult = Result<FlakeInfo, ResolveError>;
type ResolveFuture = BoxFuture<'static, ResolveResult>;

impl Inner {
    fn save_cache(&self) {
        let _ = self
            .cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .save();
    }
}

impl Engine {
    /// An engine running real processes ([`crate::exec::run`]).
    pub fn new(config: &Config, cache: Cache, freshness: Freshness) -> Engine {
        let runner: Runner = Arc::new(|spec: CommandSpec, timeout| {
            Box::pin(async move { crate::exec::run(&spec, timeout).await })
        });
        Engine::with_runner(config, cache, freshness, runner)
    }

    /// An engine running every process through `runner`. Multiplexing
    /// uses [`remote::control_dir`] (created on first use) unless
    /// `ssh.persist` is 0.
    pub fn with_runner(
        config: &Config,
        cache: Cache,
        freshness: Freshness,
        runner: Runner,
    ) -> Engine {
        let control_dir = config
            .ssh
            .persist
            .non_zero()
            .and_then(|_| remote::control_dir());
        Engine::assemble(config, cache, freshness, runner, control_dir)
    }

    fn assemble(
        config: &Config,
        cache: Cache,
        freshness: Freshness,
        runner: Runner,
        control_dir: Option<PathBuf>,
    ) -> Engine {
        Engine {
            inner: Arc::new(Inner {
                config: config.clone(),
                runner,
                cache: Mutex::new(cache),
                evals: Semaphore::new(config.jobs()),
                connections: Semaphore::new(MAX_NEW_CONNECTIONS),
                control_dir,
                control_dir_ready: OnceLock::new(),
                nix_version: OnceCell::new(),
                strict_host_keys: AtomicBool::new(false),
                resolving: Mutex::new(HashMap::new()),
            }),
            freshness,
        }
    }

    /// This engine with another [`Freshness`], sharing the cache, the
    /// limits and what was learned so far (e.g. the TUI's refresh, which
    /// forces re-resolution).
    pub fn with_freshness(&self, freshness: Freshness) -> Engine {
        Engine {
            inner: Arc::clone(&self.inner),
            freshness,
        }
    }

    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    /// Resolve `flake` (as configured): a cached resolution younger than
    /// `nix.refresh` is reused without running nix (unless
    /// [`Freshness::Force`]); otherwise `nix flake metadata` (with
    /// `--refresh` unless [`Freshness::Offline`] — which never runs it).
    /// Success is recorded as the flake's last good resolution. On failure,
    /// the last good resolution is returned with `stale` set to the reason;
    /// with none, the error (summary from [`crate::nix::error_summary`],
    /// hint about `access-tokens` on GitHub rate limits).
    ///
    /// Only flakes another machine fetches ([`FlakeInfo::fetchable_remotely`])
    /// reuse a young resolution: a local one is cheap to resolve and changes
    /// with every edit (a reused `path:` resolution would pin an old
    /// `narHash`).
    pub async fn resolve(&self, flake: &str) -> Result<FlakeInfo, ErrorInfo> {
        self.resolve_flake(flake).await.map_err(|e| match e {
            ResolveError::Spawn(error) => ErrorInfo::msg(self.nix_spawn_message(&error)),
            ResolveError::Failed(error) => error,
        })
    }

    /// Resolve `flake` and read the hosts it describes ([`FlakeHosts`]:
    /// discovered configurations + inventory) — from the cache when the
    /// flake's key has them, else one pure evaluation (recorded); under
    /// [`Freshness::Offline`], a cache miss is an error ([`NOT_CACHED`]).
    ///
    /// The key also covers `inventory`. The cache is saved by the caller
    /// ([`Self::save_cache`]) or the next check.
    pub async fn discover(&self, flake: &str) -> Result<(FlakeInfo, FlakeHosts), ErrorInfo> {
        let config = &self.inner.config;
        let (info, version) = tokio::join!(self.resolve(flake), async {
            if config.cache {
                self.nix_version().await.ok().flatten()
            } else {
                None
            }
        });
        let info = info?;
        let key = version
            .as_deref()
            .and_then(|version| self.cache_key(version, &info))
            .map(|key| format!("{key}{}", self.hosts_key_suffix()));
        if let Some(key) = &key
            && let Some(hosts) = self.with_cache(|cache| cache.hosts(key, util::now_unix()))
        {
            return Ok((info, hosts));
        }
        if self.freshness == Freshness::Offline {
            return Err(ErrorInfo::new(
                format!("can't read the hosts of {flake}: {NOT_CACHED}"),
                NOT_CACHED_DETAIL,
            ));
        }
        let spec = config.nix.hosts(&info.locked, &config.inventory);
        let timeout = config.eval_timeout.non_zero();
        let out = self
            .run(spec.clone(), timeout)
            .await
            .map_err(|error| ErrorInfo::msg(self.nix_spawn_message(&error)))?;
        let failed = |summary: String| {
            ErrorInfo::new(
                format!("can't read the hosts of {flake}: {summary}"),
                nix_detail(&spec, &out.stderr),
            )
        };
        if !out.success() {
            return Err(failed(nix_summary(&out, "nix eval", timeout)));
        }
        let hosts = nix::parse_hosts(&out.stdout).map_err(failed)?;
        if let Some(key) = &key {
            self.with_cache(|cache| cache.record_hosts(key, &info, &hosts, util::now_unix()));
        }
        Ok((info, hosts))
    }

    /// The hosts `flake` (as configured) described the last time
    /// [`Self::discover`] read them — at whatever revision that was — from
    /// the cache, if it has them: what a check can start with while
    /// discovery finds out whether they are still those. Runs nothing.
    pub fn cached_hosts(&self, flake: &str) -> Option<FlakeHosts> {
        if !self.inner.config.cache {
            return None;
        }
        let suffix = self.hosts_key_suffix();
        self.with_cache(|cache| {
            let entries = cache.data().flakes.iter();
            entries
                .filter(|(key, entry)| entry.flake == flake && key.ends_with(&suffix))
                .filter_map(|(_, entry)| Some((entry.used, entry.hosts.as_ref()?)))
                .max_by_key(|(used, _)| *used)
                .map(|(_, hosts)| hosts.clone())
        })
    }

    /// What tells the cache entries of host lists apart from those of out
    /// paths, and one `inventory` attribute's from another's.
    fn hosts_key_suffix(&self) -> String {
        let inventory = util::fnv1a64(self.inner.config.inventory.as_bytes());
        format!("+{inventory:016x}")
    }

    /// One complete check, from [`Event::RunStarted`] to
    /// [`Event::RunFinished`]. All work happens inside this future (no
    /// detached tasks), so dropping it cancels everything and kills the
    /// children.
    ///
    /// - every host's probe starts immediately (≤ [`MAX_NEW_CONNECTIONS`]
    ///   new ssh connections at once), independent of flake resolution;
    ///   remote probes use the private ControlMaster ([`crate::remote::mux_for`]);
    /// - each distinct flake not in `known` is resolved once ([`Self::resolve`]),
    ///   in parallel, emitting `FlakeStarted`/`FlakeFinished`;
    /// - desired paths: per `(flake, attr)` one evaluation shared by all
    ///   hosts with that attr; from the cache when [`Cache::key`] has the
    ///   attr (no `EvalStarted`); otherwise, under [`Freshness::Offline`],
    ///   an error ([`NOT_CACHED`], no `EvalStarted`); else `nix eval`
    ///   (≤ `jobs` at once, the longest-running first per the cache's
    ///   hints), `--read-only` first when enabled unless the hint says it
    ///   fails, retried without it on
    ///   [`crate::nix::is_read_only_failure`] (keeping the slot); results
    ///   and hints recorded in the cache; `eval-timeout` applies;
    /// - a flake that can't be resolved fails the evaluation of each of its
    ///   hosts with that error;
    /// - probe classification per DESIGN.md ("Probing"): complete output →
    ///   result whatever the exit code; incomplete → `Failed`; no output and
    ///   spawn error / exit 255 / timeout → `Unreachable`; else `Failed`.
    ///   Exit 255 over a private master with a stderr that says nothing
    ///   (empty, or only a proxy's `Connection closed by UNKNOWN port
    ///   65535`) is retried once with [`crate::remote::Mux::Off`] to
    ///   capture the reason; so is a timeout without output over a private
    ///   master that was already up, after that master is told to exit
    ///   (`ssh -O exit`: its connection may have died without a reset);
    ///   transient failures ([`crate::remote::is_transient_ssh_failure`])
    ///   are retried once after a short jittered delay. Error summaries
    ///   are the line of stderr that says why (before a proxy's closing
    ///   line); details name the command ([`CommandSpec::display`]), its
    ///   cleaned stderr and [`crate::remote::ssh_hint`];
    /// - `ProbeFinished.deployed` comes from [`Cache::deployed_rev`], or,
    ///   for a generation that records its revision, the time the cache
    ///   has for it ([`Cache::revision_time`]); one it has none for is
    ///   looked up after the probes and evaluations
    ///   (`date_revisions`);
    /// - `nix` or `ssh` that can't be spawned at all is reported once as an
    ///   [`Event::Unrunnable`] naming the config key
    ///   (`nix.command`/`ssh.command`), not per host;
    /// - the cache is saved when the check completes.
    ///
    /// Events: `RunStarted`, every `ProbeStarted` (in `hosts` order), every
    /// `FlakeStarted`, then results as they arrive, any `RevisionDated`,
    /// `RunFinished` last.
    pub async fn check(&self, hosts: &[(HostId, Host)], known: &[FlakeInfo], tx: &EventTx) {
        let run = Run::new(tx);
        run.send(Event::RunStarted);
        for (id, _) in hosts {
            run.send(Event::ProbeStarted(*id));
        }
        let flakes = group_by_flake(hosts, known);
        for flake in &flakes {
            match flake.known {
                Some(info) => run.resolved(info),
                None => run.send(Event::FlakeStarted {
                    url: flake.url.to_string(),
                }),
            }
        }
        tokio::join!(
            self.probe_hosts(&run, hosts),
            self.evaluate_flakes(&run, &flakes)
        );
        self.date_revisions(&run).await;
        self.save_cache_off_thread().await;
        run.send(Event::RunFinished);
    }

    /// Probe one host again: `ProbeStarted`, `ProbeFinished`.
    pub async fn probe(&self, id: HostId, host: &Host, tx: &EventTx) {
        let run = Run::new(tx);
        run.send(Event::ProbeStarted(id));
        let result = self.probe_host(&run, host).await;
        self.probe_finished(&run, id, host, result);
    }

    /// Closure diff from `current` to `desired` (`DiffStarted` …
    /// `DiffFinished`): one [`crate::actions::closure_script`] run on the
    /// target for both paths (skipped for forced-command routes) and, when
    /// the target is remote, concurrently a local run for `desired`; the
    /// desired closure is taken from wherever it exists; `DesiredMissing`
    /// if nowhere.
    ///
    /// A forced-command route has no shell, so both closures must be in
    /// this machine's store. `current` missing from the store listed is an
    /// error.
    pub async fn diff(
        &self,
        id: HostId,
        host: &Host,
        current: &StorePath,
        desired: &StorePath,
        tx: &EventTx,
    ) {
        let _ = tx.send(Event::DiffStarted {
            id,
            current: current.clone(),
            desired: desired.clone(),
        });
        let result = self.closure_diff(host, current, desired).await;
        let _ = tx.send(Event::DiffFinished {
            id,
            current: current.clone(),
            desired: desired.clone(),
            result,
        });
    }

    /// The process that runs `command` interactively for `host`: locally
    /// (local hosts, `activate-on = "local"`) via
    /// [`crate::remote::interactive_command`]; remotely by first uploading
    /// [`crate::remote::interactive_script`] with
    /// [`crate::remote::upload_command`] (checked with
    /// [`crate::remote::is_safe_remote_path`]) and returning the `ssh -t …
    /// sh <path>` command, and the path (for [`Engine::discard_upload`]).
    ///
    /// The upload's path is the last line it printed, trimmed (login-shell
    /// noise may precede it). Forced-command routes have no shell: an error.
    pub async fn prepare_interactive(
        &self,
        host: &Host,
        command: &str,
    ) -> Result<Prepared, ErrorInfo> {
        let target = match &host.target {
            Target::Ssh(target) if !actions::runs_here(host) => target,
            here => {
                let ssh = self.ssh_config();
                return Ok(Prepared {
                    spec: remote::interactive_command(here, &ssh, command, None),
                    upload: None,
                });
            }
        };
        if target.forced_command.is_some() {
            return Err(ErrorInfo::msg(format!(
                "{} only answers its probe command: nothing else can run there",
                host.name
            )));
        }
        let script = remote::interactive_script(command, true);
        let timeout = self.inner.config.timeout.non_zero();
        let attempt = self
            .run_on(&host.target, timeout, |ssh, mux| {
                (remote::upload_command(target, ssh, mux, &script), ())
            })
            .await;
        let out = attempt
            .result
            .map_err(|error| ErrorInfo::msg(self.ssh_spawn_message(&error)))?;
        let stderr = clean_ssh_stderr(&out.stderr);
        let failed = |summary: String| {
            let hint = self.ssh_hint_for(&host.target, &out.stderr);
            ErrorInfo::new(summary, detail(&attempt.spec, &stderr, hint.as_deref()))
        };
        if !out.success() {
            let why = failure_reason(&out, &stderr, timeout, &host.target);
            return Err(failed(format!(
                "couldn't upload the command to {}: {why}",
                host.name
            )));
        }
        let path = last_line(&out.stdout).unwrap_or_default();
        if !remote::is_safe_remote_path(path) {
            return Err(failed(format!(
                "uploading the command to {} printed {:?}, not the path of a temporary file",
                host.name,
                util::truncate(path, 80)
            )));
        }
        let ssh = self.ssh_config();
        Ok(Prepared {
            spec: remote::interactive_command(&host.target, &ssh, command, Some(path)),
            upload: Some(path.to_string()),
        })
    }

    /// Remove the script [`Engine::prepare_interactive`] uploaded to `host`
    /// at `path`, best effort, for when the interactive run that removes it
    /// first thing may not have started (it failed to connect, or was cut
    /// short): `rm -f -- <path>` over the unattended channel, given at most
    /// 5 s (or the probe timeout, if shorter).
    pub async fn discard_upload(&self, host: &Host, path: &str) {
        if !remote::is_safe_remote_path(path) || host.ssh().is_none() {
            return;
        }
        let script = format!("rm -f -- {}\n", remote::sh_quote(path));
        let timeout = self.inner.config.timeout.0.min(CLEANUP_TIMEOUT);
        let _ = self
            .run_on(&host.target, Some(timeout), |ssh, mux| {
                (remote::script_command(&host.target, ssh, mux, &script), ())
            })
            .await;
    }

    /// Write the cache now (e.g. on quit or interrupt); errors are ignored.
    pub fn save_cache(&self) {
        self.inner.save_cache();
    }

    // ─── resolution ─────────────────────────────────────────────────────

    /// [`Self::resolve_now`], shared: a caller resolving `flake` (with this
    /// handle's [`Freshness`]) while another one is at it awaits that one's
    /// result, so a check and the discovery beside it resolve once.
    async fn resolve_flake(&self, flake: &str) -> ResolveResult {
        let key = (flake.to_owned(), self.freshness);
        let resolving: Resolving = {
            let mut in_flight = self
                .inner
                .resolving
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            in_flight.retain(|_, weak| weak.upgrade().is_some());
            match in_flight.get(&key).and_then(WeakShared::upgrade) {
                Some(resolving) => resolving,
                None => {
                    let (engine, url) = (self.clone(), flake.to_owned());
                    let resolving = async move { engine.resolve_now(&url).await }
                        .boxed()
                        .shared();
                    if let Some(weak) = resolving.downgrade() {
                        in_flight.insert(key, weak);
                    }
                    resolving
                }
            }
        };
        resolving.await
    }

    async fn resolve_now(&self, flake: &str) -> ResolveResult {
        let config = &self.inner.config;
        let cached = self.with_cache(|cache| cache.resolution(flake).cloned());
        match self.freshness {
            Freshness::Offline => {
                return cached.map(|r| r.info).ok_or_else(|| {
                    ResolveError::Failed(ErrorInfo::msg(format!(
                        "{flake} was never resolved, and --offline only uses cached resolutions"
                    )))
                });
            }
            Freshness::Auto => {
                if let Some(resolution) = &cached
                    && resolution.info.fetchable_remotely()
                    && is_fresh(
                        resolution.resolved_at,
                        util::now_unix(),
                        config.nix.refresh.non_zero(),
                    )
                {
                    return Ok(resolution.info.clone());
                }
            }
            Freshness::Force => {}
        }
        let spec = config.nix.metadata(flake, true);
        let timeout = config.eval_timeout.non_zero();
        let out = self
            .run(spec.clone(), timeout)
            .await
            .map_err(|error| ResolveError::Spawn(Arc::new(error)))?;
        let rate_limited =
            out.stderr.contains("HTTP error 403") || out.stderr.contains("rate limit");
        let parsed = if out.success() {
            nix::parse_metadata(flake, &out.stdout, &out.stderr)
        } else {
            Err(nix_summary(&out, "nix flake metadata", timeout))
        };
        match parsed {
            Ok(mut info) => {
                if rate_limited && let Some(stale) = &mut info.stale {
                    *stale = with_token_hint(stale);
                }
                self.with_cache(|cache| cache.record_resolution(&info, util::now_unix()));
                Ok(info)
            }
            Err(summary) => {
                let summary = if rate_limited {
                    with_token_hint(&summary)
                } else {
                    summary
                };
                match cached {
                    Some(resolution) => Ok(FlakeInfo {
                        stale: Some(summary),
                        ..resolution.info
                    }),
                    None => {
                        let hint = rate_limited.then_some(TOKEN_HINT);
                        let stderr = nix::clean_stderr(&out.stderr);
                        Err(ResolveError::Failed(ErrorInfo::new(
                            summary,
                            detail(&spec, &stderr, hint),
                        )))
                    }
                }
            }
        }
    }

    /// [`Self::resolve_flake`] within a check: reports `FlakeFinished`, and
    /// a nix that can't be run as the check's one [`Event::Unrunnable`].
    async fn resolve_for_check(&self, run: &Run<'_>, url: &str) -> Result<FlakeInfo, ErrorInfo> {
        let result = self.resolve_flake(url).await.map_err(|e| match e {
            ResolveError::Spawn(error) => self.nix_unrunnable(run, &error),
            ResolveError::Failed(error) => error,
        });
        if let Ok(info) = &result {
            run.resolved(info);
        }
        run.send(Event::FlakeFinished {
            url: url.to_string(),
            result: result.clone(),
        });
        result
    }

    /// The first line of `nix --version`, run once per engine (successfully
    /// spawned, that is).
    async fn nix_version(&self) -> io::Result<Option<String>> {
        let version = self
            .inner
            .nix_version
            .get_or_try_init(|| async {
                let out = self
                    .run(self.inner.config.nix.version(), Some(VERSION_TIMEOUT))
                    .await?;
                Ok::<_, io::Error>(if out.success() {
                    nix::parse_version(&out.stdout)
                } else {
                    None
                })
            })
            .await?;
        Ok(version.clone())
    }

    /// [`Self::nix_version`] within a check; a nix that can't be run is
    /// reported.
    async fn version_for_check(&self, run: &Run<'_>) -> Option<String> {
        match self.nix_version().await {
            Ok(version) => version,
            Err(error) => {
                self.nix_unrunnable(run, &error);
                None
            }
        }
    }

    /// The cache key of `info`'s results, if they may be cached at all.
    fn cache_key(&self, nix_version: &str, info: &FlakeInfo) -> Option<String> {
        let config = &self.inner.config;
        if !config.cache {
            return None;
        }
        Cache::key(nix_version, &config.nix.options, info)
    }

    // ─── evaluation ─────────────────────────────────────────────────────

    /// Resolve the flakes that need it, then evaluate (or look up) every
    /// `(flake, attr)`: as soon as a flake is resolved (and the nix version
    /// known, for cache keys), its cache hits are reported and its misses
    /// queued; a free slot always takes the longest queued evaluation.
    async fn evaluate_flakes(&self, run: &Run<'_>, flakes: &[FlakeWork<'_>]) {
        if flakes.is_empty() {
            return;
        }
        let mut resolving: FuturesUnordered<_> = flakes
            .iter()
            .enumerate()
            .filter(|(_, flake)| flake.known.is_none())
            .map(|(i, flake)| async move { (i, self.resolve_for_check(run, flake.url).await) })
            .collect();
        let mut resolved: Vec<(usize, FlakeInfo)> = flakes
            .iter()
            .enumerate()
            .filter_map(|(i, flake)| Some((i, flake.known?.clone())))
            .collect();
        // Without caching, no version is needed (and none is asked for).
        let mut version: Option<Option<String>> = (!self.inner.config.cache).then_some(None);
        let mut version_lookup = pin!(self.version_for_check(run));
        let mut queue = EvalQueue::new();
        let mut running = FuturesUnordered::new();
        loop {
            if let Some(version) = &version {
                for (i, info) in resolved.drain(..) {
                    self.plan(run, &flakes[i], info, version.as_deref(), &mut queue);
                }
            }
            tokio::select! {
                biased;
                found = &mut version_lookup, if version.is_none() => version = Some(found),
                Some((i, result)) = resolving.next() => match result {
                    Ok(info) => resolved.push((i, info)),
                    Err(error) => {
                        for &id in flakes[i].attrs.iter().flat_map(|(_, ids)| ids) {
                            run.send(Event::EvalFinished { id, result: Err(error.clone()) });
                        }
                    }
                },
                Some(()) = running.next() => {}
                permit = self.inner.evals.acquire(), if !queue.is_empty() => {
                    // The semaphore is never closed.
                    if let (Ok(permit), Some(job)) = (permit, queue.pop()) {
                        for &id in job.hosts {
                            run.send(Event::EvalStarted(id));
                        }
                        running.push(self.eval(run, job, permit));
                    }
                }
                else => break,
            }
        }
    }

    /// Report the cache hits of a resolved flake and queue its misses (fail
    /// them, under [`Freshness::Offline`]).
    fn plan<'f>(
        &self,
        run: &Run<'_>,
        flake: &'f FlakeWork<'_>,
        info: FlakeInfo,
        version: Option<&str>,
        queue: &mut EvalQueue<'f>,
    ) {
        let info = Arc::new(info);
        let key = version.and_then(|version| self.cache_key(version, &info));
        let now = util::now_unix();
        for (attr, hosts) in &flake.attrs {
            let (cached, hint) = self.with_cache(|cache| {
                let cached = key
                    .as_deref()
                    .and_then(|key| cache.out_path(key, attr, now));
                (cached, cache.hint(&info.flake, attr))
            });
            match cached {
                Some(path) => {
                    let desired = Desired {
                        path,
                        rev: info.rev.clone(),
                        last_modified: info.last_modified,
                        cached: true,
                    };
                    for &id in hosts {
                        run.send(Event::EvalFinished {
                            id,
                            result: Ok(desired.clone()),
                        });
                    }
                }
                None if self.freshness == Freshness::Offline => {
                    let error = ErrorInfo::new(NOT_CACHED, NOT_CACHED_DETAIL);
                    for &id in hosts {
                        run.send(Event::EvalFinished {
                            id,
                            result: Err(error.clone()),
                        });
                    }
                }
                None => queue.push(Job {
                    info: Arc::clone(&info),
                    attr,
                    hosts,
                    key: key.clone(),
                    hint,
                }),
            }
        }
    }

    /// Run one queued evaluation in its slot and report it to its hosts.
    async fn eval(&self, run: &Run<'_>, job: Job<'_>, permit: SemaphorePermit<'_>) {
        let result = self.evaluate(run, &job).await;
        drop(permit);
        for &id in job.hosts {
            run.send(Event::EvalFinished {
                id,
                result: result.clone(),
            });
        }
    }

    /// `nix eval` of the job's attribute: `--read-only` first (when enabled
    /// and not known to fail), then again without it on a read-only
    /// failure; the result and the hint are recorded.
    async fn evaluate(&self, run: &Run<'_>, job: &Job<'_>) -> EvalResult {
        let config = &self.inner.config;
        let timeout = config.eval_timeout.non_zero();
        let mut read_only_fails = job.hint.is_some_and(|hint| hint.read_only_fails);
        let mut read_only = config.nix.read_only && !read_only_fails;
        let started = Instant::now();
        let (spec, out) = loop {
            let spec = config
                .nix
                .eval_out_path(&job.info.locked, job.attr, read_only);
            let out = match self.run(spec.clone(), timeout).await {
                Ok(out) => out,
                Err(error) => return Err(self.nix_unrunnable(run, &error)),
            };
            if read_only
                && !out.success()
                && !out.timed_out
                && nix::is_read_only_failure(&out.stderr)
            {
                read_only = false;
                read_only_fails = true;
                continue;
            }
            break (spec, out);
        };
        let eval_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let result = if out.success() {
            nix::parse_out_path(&out.stdout)
        } else {
            Err(nix_summary(&out, "nix eval", timeout))
        }
        .map_err(|summary| ErrorInfo::new(summary, nix_detail(&spec, &out.stderr)));
        let now = util::now_unix();
        self.with_cache(|cache| {
            let hint = Hint {
                eval_ms,
                read_only_fails,
            };
            cache.record_hint(&job.info.flake, job.attr, hint);
            if let (Ok(path), Some(key)) = (&result, &job.key) {
                cache.record_out_path(key, &job.info, job.attr, path, now);
            }
        });
        result.map(|path| Desired {
            path,
            rev: job.info.rev.clone(),
            last_modified: job.info.last_modified,
            cached: false,
        })
    }

    // ─── probes ─────────────────────────────────────────────────────────

    async fn probe_hosts(&self, run: &Run<'_>, hosts: &[(HostId, Host)]) {
        let mut probes: FuturesUnordered<_> = hosts
            .iter()
            .map(|(id, host)| async move {
                let result = self.probe_host(run, host).await;
                self.probe_finished(run, *id, host, result);
            })
            .collect();
        while probes.next().await.is_some() {}
    }

    async fn probe_host(&self, run: &Run<'_>, host: &Host) -> ProbeResult {
        let timeout = self.inner.config.timeout.non_zero();
        let attempt = self
            .run_on(&host.target, timeout, |ssh, mux| {
                probe::command(host, ssh, mux)
            })
            .await;
        self.classify_probe(run, host, attempt, timeout)
    }

    /// `ProbeFinished`, with what the cache knows about the generation: the
    /// revision it was evaluated at, or the time of the revision it records.
    /// A recorded revision the cache can't date is looked up later.
    fn probe_finished(&self, run: &Run<'_>, id: HostId, host: &Host, result: ProbeResult) {
        let report = result.as_ref().ok();
        let mut deployed = report
            .and_then(|report| report.deployed())
            .and_then(|path| self.with_cache(|cache| cache.deployed_rev(path)));
        if let Some(rev) = report.and_then(|report| report.revision.as_deref()) {
            let dated = deployed
                .as_ref()
                .is_some_and(|d| d.rev.as_deref() == Some(rev) && d.last_modified.is_some());
            if !dated {
                match self.with_cache(|cache| cache.revision_time(&host.flake, rev)) {
                    Some(at) => {
                        deployed = Some(DeployedRev {
                            flake: host.flake.clone(),
                            rev: Some(rev.to_owned()),
                            last_modified: Some(at),
                        });
                    }
                    None if nix::is_commit_hash(rev) && !self.undatable(&host.flake, rev) => {
                        run.undated(&host.flake, rev);
                    }
                    None => {}
                }
            }
        }
        run.send(Event::ProbeFinished {
            id,
            result,
            deployed,
        });
    }

    /// A lookup of when `rev` was committed failed lately (only `--refresh`
    /// tries again sooner than [`crate::cache::UNDATABLE_FOR`]).
    fn undatable(&self, flake: &str, rev: &str) -> bool {
        self.freshness != Freshness::Force
            && self.with_cache(|cache| cache.undatable(flake, rev, util::now_unix()))
    }

    /// When the revisions in [`Run::undated`] were committed:
    /// [`nix::NixConfig::commit_time`] of each one's repository at that
    /// commit ([`FlakeInfo::at_rev`]), [`DATING_JOBS`] at a time, each
    /// within [`DATING_TIMEOUT`] and all within [`DATING_BUDGET`]; recorded
    /// in the cache (failures too: [`Self::undatable`]) and sent as
    /// `RevisionDated`. Never under [`Freshness::Offline`] (it may fetch),
    /// nor for a flake whose resolution is stale (the network just failed).
    /// A failed lookup only leaves the revision undated.
    async fn date_revisions(&self, run: &Run<'_>) {
        if self.freshness == Freshness::Offline {
            return;
        }
        let undated =
            std::mem::take(&mut *run.undated.lock().unwrap_or_else(PoisonError::into_inner));
        let resolved = run
            .resolved
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let timeout = self
            .inner
            .config
            .eval_timeout
            .non_zero()
            .map_or(DATING_TIMEOUT, |t| t.min(DATING_TIMEOUT));
        let lookups = undated.into_iter().filter_map(|(flake, rev)| {
            let info = resolved.get(&flake)?;
            // The compared revision dates itself.
            if info.stale.is_some() || info.rev.as_deref() == Some(rev.as_str()) {
                return None;
            }
            let url = info.at_rev(&rev)?;
            Some(async move {
                let spec = self.inner.config.nix.commit_time(&url);
                let at = match self.run(spec, Some(timeout)).await {
                    Ok(out) if out.success() => out.stdout.trim().parse::<i64>().ok(),
                    _ => None,
                };
                (flake, rev, at)
            })
        });
        let mut lookups = futures_util::stream::iter(lookups).buffer_unordered(DATING_JOBS);
        let dating = async {
            while let Some((flake, rev, at)) = lookups.next().await {
                let now = util::now_unix();
                let Some(last_modified) = at else {
                    self.with_cache(|cache| cache.record_undatable(&flake, &rev, now));
                    continue;
                };
                self.with_cache(|cache| cache.record_revision(&flake, &rev, last_modified));
                run.send(Event::RevisionDated {
                    flake,
                    rev,
                    last_modified,
                });
            }
        };
        // What isn't dated by then is tried again next time.
        let _ = tokio::time::timeout(DATING_BUDGET, dating).await;
    }

    /// [`remote::ssh_hint`] for a run on `target`: none for this machine,
    /// and none that sends the user to an ssh config this ssh doesn't read.
    fn ssh_hint_for(&self, target: &Target, stderr: &str) -> Option<String> {
        match target {
            Target::Ssh(t) => {
                let args = t.options.iter().chain(&self.inner.config.ssh.options);
                remote::ssh_hint(stderr, remote::reads_ssh_config(args))
            }
            Target::Local { .. } => None,
        }
    }

    /// DESIGN.md ("Probing"): a complete answer counts whatever the exit
    /// code; an answer cut short is `Failed`; no output at all after a
    /// spawn error, ssh's exit 255 or a timeout is `Unreachable`; anything
    /// else is `Failed`.
    fn classify_probe(
        &self,
        run: &Run<'_>,
        host: &Host,
        attempt: Attempt<Mode>,
        timeout: Option<Duration>,
    ) -> ProbeResult {
        let Attempt {
            spec,
            extra: mode,
            result,
        } = attempt;
        let out = match result {
            Ok(out) => out,
            Err(error) => {
                let error = match &host.target {
                    Target::Ssh(_) => self.ssh_unrunnable(run, &error),
                    Target::Local { .. } => ErrorInfo::new(
                        format!("couldn't run {}: {error}", spec.program),
                        detail(&spec, "", None),
                    ),
                };
                return Err(ProbeError {
                    kind: ProbeFailure::Unreachable,
                    error,
                });
            }
        };
        let stderr = clean_ssh_stderr(&out.stderr);
        let fail = |kind, summary: String| {
            let hint = self.ssh_hint_for(&host.target, &out.stderr);
            Err(ProbeError {
                kind,
                error: ErrorInfo::new(summary, detail(&spec, &stderr, hint.as_deref())),
            })
        };
        let silent = out.stdout.trim().is_empty();
        match probe::parse(&out.stdout, &mode) {
            Parsed::Report(report) => Ok(*report),
            Parsed::Incomplete => fail(ProbeFailure::Failed, INCOMPLETE.into()),
            Parsed::Invalid(_) if silent && (out.timed_out || out.code == Some(255)) => fail(
                ProbeFailure::Unreachable,
                failure_reason(&out, &stderr, timeout, &host.target),
            ),
            Parsed::Invalid(_) if silent && stderr.is_empty() => match out.code {
                Some(code) => fail(
                    ProbeFailure::Failed,
                    format!("printed nothing (exit {code})"),
                ),
                None => fail(ProbeFailure::Failed, "killed by a signal".into()),
            },
            Parsed::Invalid(_) if silent => fail(
                ProbeFailure::Failed,
                failure_reason(&out, &stderr, timeout, &host.target),
            ),
            Parsed::Invalid(reason) if out.timed_out => fail(
                ProbeFailure::Failed,
                format!("{reason} ({})", timed_out(timeout)),
            ),
            Parsed::Invalid(reason) => fail(ProbeFailure::Failed, reason),
        }
    }

    // ─── closure diffs ──────────────────────────────────────────────────

    async fn closure_diff(
        &self,
        host: &Host,
        current: &StorePath,
        desired: &StorePath,
    ) -> DiffResult {
        let name = &host.name;
        let remote = host.ssh().filter(|t| t.forced_command.is_none()).is_some();
        let ([before, after], here) = if remote {
            let (target, here) = tokio::join!(
                self.closures(Some(host), [current, desired]),
                self.closures(None, [desired]),
            );
            (target?, Some(here))
        } else if host.is_local() {
            (self.closures(Some(host), [current, desired]).await?, None)
        } else {
            // A forced-command route has no shell: this machine's store only.
            (self.closures(None, [current, desired]).await?, None)
        };
        let Some(before) = before else {
            return Err(ErrorInfo::msg(if remote || host.is_local() {
                format!("the current generation isn't valid in {name}'s store: {current}")
            } else {
                format!(
                    "the current generation of {name} isn't in this machine's store, and \
                     {name}'s ssh route can't list closures: {current}"
                )
            }));
        };
        let after = match (after, here) {
            (Some(after), _) => after,
            (None, Some(Ok([Some(after)]))) => after,
            (None, Some(Err(error))) => return Err(error),
            (None, _) => return Ok(DiffOutcome::DesiredMissing),
        };
        Ok(DiffOutcome::Diff {
            diff: closure::diff(&before, &after),
        })
    }

    /// The closures of `paths` (`None`: not valid in that store) on `on`'s
    /// target, or here.
    async fn closures<const N: usize>(
        &self,
        on: Option<&Host>,
        paths: [&StorePath; N],
    ) -> Result<[Option<Vec<ClosureEntry>>; N], ErrorInfo> {
        let here = Target::Local { assumed: false };
        let (target, place) = match on {
            Some(host) => (&host.target, host.name.as_str()),
            None => (&here, "this machine"),
        };
        let script = actions::closure_script(&paths);
        let timeout = self.inner.config.timeout.0.max(MIN_LISTING_TIMEOUT);
        let attempt = self
            .run_on(target, Some(timeout), |ssh, mux| {
                let spec = remote::script_command(target, ssh, mux, &script)
                    .max_output(actions::MAX_CLOSURE_OUTPUT);
                (spec, ())
            })
            .await;
        let spec = &attempt.spec;
        let out = match attempt.result {
            Ok(out) => out,
            Err(error) => {
                return Err(match target {
                    Target::Ssh(_) => ErrorInfo::msg(self.ssh_spawn_message(&error)),
                    Target::Local { .. } => ErrorInfo::new(
                        format!("couldn't run {}: {error}", spec.program),
                        detail(spec, "", None),
                    ),
                });
            }
        };
        let stderr = clean_ssh_stderr(&out.stderr);
        let failed = |summary: String| {
            let hint = self.ssh_hint_for(target, &out.stderr);
            ErrorInfo::new(summary, detail(spec, &stderr, hint.as_deref()))
        };
        if out.code == Some(127) && !out.timed_out {
            return Err(failed(format!("nix-store isn't installed on {place}")));
        }
        if !out.success() {
            let why = failure_reason(&out, &stderr, Some(timeout), target);
            return Err(failed(format!("couldn't list closures on {place}: {why}")));
        }
        let listing = actions::parse_closures(&out.stdout)
            .map_err(|e| failed(format!("couldn't read the closures listed on {place}: {e}")))?;
        let mismatch = || failed(format!("{place} listed other closures than requested"));
        let requested = listing.len() == N
            && listing
                .iter()
                .zip(paths)
                .all(|((path, _), want)| path == want);
        if !requested {
            return Err(mismatch());
        }
        let closures: Vec<_> = listing.into_iter().map(|(_, closure)| closure).collect();
        closures.try_into().map_err(|_| mismatch())
    }

    // ─── processes and ssh ──────────────────────────────────────────────

    fn run(
        &self,
        spec: CommandSpec,
        timeout: Option<Duration>,
    ) -> BoxFuture<'static, io::Result<Output>> {
        (self.inner.runner)(spec, timeout)
    }

    /// Run what `command` builds for these ssh settings and mux on
    /// `target`. Over ssh: a slot of [`MAX_NEW_CONNECTIONS`] is held while
    /// the run opens a new connection, and a run with no output is retried
    /// (each at most once) when it timed out over a private master that
    /// was already up (unmultiplexed, after telling that master to exit:
    /// its connection may have died without a reset, see
    /// [`Self::exit_master`]), when ssh rejected `accept-new` (with
    /// [`STRICT_HOST_KEYS`] from then on, for every later run), when it
    /// exited 255 over the private master without saying why
    /// ([`says_nothing`]; unmultiplexed, to capture the reason
    /// ControlPersist swallowed), or after a transient failure
    /// ([`remote::is_transient_ssh_failure`]).
    async fn run_on<T>(
        &self,
        target: &Target,
        timeout: Option<Duration>,
        mut command: impl FnMut(&SshConfig, &Mux) -> (CommandSpec, T),
    ) -> Attempt<T> {
        let Target::Ssh(ssh_target) = target else {
            let (spec, extra) = command(&self.inner.config.ssh, &Mux::Off);
            let result = self.run(spec.clone(), timeout).await;
            return Attempt {
                spec,
                extra,
                result,
            };
        };
        let mut unmultiplexed = false;
        let (mut strict_retry, mut silent_retry, mut transient_retry) = (true, true, true);
        let mut wedged_retry = true;
        loop {
            let ssh = self.ssh_config();
            let mux = if unmultiplexed {
                Mux::Off
            } else {
                self.mux(ssh_target, &ssh)
            };
            let (spec, extra) = command(&ssh, &mux);
            let opens = opens_connection(&mux);
            let result = {
                let _slot = if opens {
                    self.inner.connections.acquire().await.ok()
                } else {
                    None
                };
                self.run(spec.clone(), timeout).await
            };
            if let Ok(out) = &result
                && out.timed_out
                && out.stdout.trim().is_empty()
                && wedged_retry
                && !opens
                && let Mux::Private { socket, .. } = &mux
            {
                wedged_retry = false;
                self.exit_master(ssh_target, &ssh, socket).await;
                unmultiplexed = true;
                continue;
            }
            if let Ok(out) = &result
                && !out.timed_out
                && out.stdout.trim().is_empty()
            {
                if strict_retry && out.stderr.contains(ACCEPT_NEW_UNSUPPORTED) {
                    strict_retry = false;
                    self.inner.strict_host_keys.store(true, Ordering::Relaxed);
                    continue;
                }
                if silent_retry
                    && matches!(mux, Mux::Private { .. })
                    && out.code == Some(255)
                    && says_nothing(&out.stderr)
                {
                    silent_retry = false;
                    unmultiplexed = true;
                    continue;
                }
                if transient_retry && remote::is_transient_ssh_failure(&out.stderr) {
                    transient_retry = false;
                    tokio::time::sleep(retry_delay()).await;
                    continue;
                }
            }
            return Attempt {
                spec,
                extra,
                result,
            };
        }
    }

    /// `ssh.*` as configured, plus [`STRICT_HOST_KEYS`] once an ssh
    /// rejected `accept-new`.
    fn ssh_config(&self) -> Cow<'_, SshConfig> {
        let ssh = &self.inner.config.ssh;
        if self.inner.strict_host_keys.load(Ordering::Relaxed) {
            let mut ssh = ssh.clone();
            ssh.options.extend(STRICT_HOST_KEYS.map(String::from));
            Cow::Owned(ssh)
        } else {
            Cow::Borrowed(ssh)
        }
    }

    fn mux(&self, target: &SshTarget, ssh: &SshConfig) -> Mux {
        remote::mux_for(target, ssh, self.control_dir())
    }

    /// Tell the private master at `socket` to exit (`ssh -O exit`; best
    /// effort, for at most [`EXIT_MASTER_TIMEOUT`]) after a run over it
    /// timed out without a word: its connection may have died without a
    /// reset (a sleep, a VPN change, a NAT drop), which a master without
    /// keepalives only notices when TCP gives up, some 15 minutes later.
    /// Until then every run over it would time out the same way.
    async fn exit_master(&self, target: &SshTarget, ssh: &SshConfig, socket: &Path) {
        if let Some(spec) = remote::exit_master_command(target, ssh, socket) {
            let _ = self.run(spec, Some(EXIT_MASTER_TIMEOUT)).await;
        }
    }

    /// The control socket directory, created (mode 0700) on first use;
    /// `None` if there is none or it can't be made.
    fn control_dir(&self) -> Option<&Path> {
        let dir = self.inner.control_dir.as_deref()?;
        let ready = *self
            .inner
            .control_dir_ready
            .get_or_init(|| make_private_dir(dir));
        ready.then_some(dir)
    }

    // ─── errors and the cache ───────────────────────────────────────────

    fn nix_spawn_message(&self, error: &io::Error) -> String {
        format!(
            "nix.command = {:?} could not be run: {error}; is Nix installed and on PATH?",
            self.inner.config.nix.command
        )
    }

    fn ssh_spawn_message(&self, error: &io::Error) -> String {
        format!(
            "ssh.command = {:?} could not be run: {error}; is OpenSSH installed and on PATH?",
            self.inner.config.ssh.command
        )
    }

    /// A nix that can't be spawned: reported once per check, and this
    /// short error for each evaluation (or flake) it fails.
    fn nix_unrunnable(&self, run: &Run<'_>, error: &io::Error) -> ErrorInfo {
        let message = self.nix_spawn_message(error);
        run.report_once(&run.nix_reported, &message);
        ErrorInfo::new("nix couldn't be run", message)
    }

    /// Like [`Self::nix_unrunnable`], for ssh.
    fn ssh_unrunnable(&self, run: &Run<'_>, error: &io::Error) -> ErrorInfo {
        let message = self.ssh_spawn_message(error);
        run.report_once(&run.ssh_reported, &message);
        ErrorInfo::new("ssh couldn't be run", message)
    }

    /// Use the cache; the lock can't be held across an `.await`.
    fn with_cache<R>(&self, f: impl FnOnce(&mut Cache) -> R) -> R {
        f(&mut self
            .inner
            .cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner))
    }

    /// [`Cache::save`] may wait for another process's lock: on a blocking
    /// thread, so the runtime (and a dashboard on it) keeps going.
    async fn save_cache_off_thread(&self) {
        let inner = Arc::clone(&self.inner);
        let _ = tokio::task::spawn_blocking(move || inner.save_cache()).await;
    }
}

/// An interactive command, ready to run ([`Engine::prepare_interactive`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    pub spec: CommandSpec,
    /// Where the script was uploaded on the host, when it was: it removes
    /// itself once it runs ([`Engine::discard_upload`] if that may not
    /// have happened).
    pub upload: Option<String>,
}

/// One check's (or other operation's) reporting: events, and whether nix /
/// ssh failing to spawn was already reported.
struct Run<'a> {
    tx: &'a EventTx,
    nix_reported: AtomicBool,
    ssh_reported: AtomicBool,
    /// `(flake, rev)`: revisions hosts' generations record that the cache
    /// can't date ([`Engine::date_revisions`]).
    undated: Mutex<Vec<(String, String)>>,
    /// The resolutions this check compares against, by flake as configured.
    resolved: Mutex<HashMap<String, FlakeInfo>>,
}

impl<'a> Run<'a> {
    fn new(tx: &'a EventTx) -> Self {
        Run {
            tx,
            nix_reported: AtomicBool::new(false),
            ssh_reported: AtomicBool::new(false),
            undated: Mutex::new(Vec::new()),
            resolved: Mutex::new(HashMap::new()),
        }
    }

    fn undated(&self, flake: &str, rev: &str) {
        let mut undated = self.undated.lock().unwrap_or_else(PoisonError::into_inner);
        if !undated.iter().any(|(f, r)| f == flake && r == rev) {
            undated.push((flake.to_owned(), rev.to_owned()));
        }
    }

    fn resolved(&self, info: &FlakeInfo) {
        self.resolved
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(info.flake.clone(), info.clone());
    }

    /// Nobody listening is fine: the front-end is gone.
    fn send(&self, event: Event) {
        let _ = self.tx.send(event);
    }

    /// `message` as the [`Event::Unrunnable`] of a program that couldn't
    /// be spawned, unless `reported` says it was already sent.
    fn report_once(&self, reported: &AtomicBool, message: &str) {
        if !reported.swap(true, Ordering::Relaxed) {
            self.send(Event::Unrunnable(ErrorInfo::msg(message)));
        }
    }
}

#[derive(Debug, Clone)]
enum ResolveError {
    /// nix couldn't be spawned.
    Spawn(Arc<io::Error>),
    Failed(ErrorInfo),
}

/// A resolution in flight, which every caller resolving the same flake
/// meanwhile awaits too ([`Engine::resolve_flake`]).
type Resolving = Shared<ResolveFuture>;

/// A run's last attempt: what ran, what built it, and what came of it.
struct Attempt<T> {
    spec: CommandSpec,
    extra: T,
    result: io::Result<Output>,
}

/// The hosts of one flake in a check, grouped by attribute (in order of
/// appearance).
struct FlakeWork<'h> {
    url: &'h str,
    /// Resolved before the check: not resolved again.
    known: Option<&'h FlakeInfo>,
    attrs: Vec<(&'h str, Vec<HostId>)>,
}

fn group_by_flake<'h>(hosts: &'h [(HostId, Host)], known: &'h [FlakeInfo]) -> Vec<FlakeWork<'h>> {
    let mut flakes: Vec<FlakeWork<'h>> = Vec::new();
    let mut flake_index: HashMap<&'h str, usize> = HashMap::new();
    let mut attr_index: HashMap<(usize, &'h str), usize> = HashMap::new();
    for (id, host) in hosts {
        let f = *flake_index.entry(&host.flake).or_insert_with(|| {
            flakes.push(FlakeWork {
                url: &host.flake,
                known: known.iter().find(|info| info.flake == host.flake),
                attrs: Vec::new(),
            });
            flakes.len() - 1
        });
        let attrs = &mut flakes[f].attrs;
        let a = *attr_index.entry((f, &host.attr)).or_insert_with(|| {
            attrs.push((&host.attr, Vec::new()));
            attrs.len() - 1
        });
        attrs[a].1.push(*id);
    }
    flakes
}

/// One `(flake, attr)` to evaluate, for these hosts.
struct Job<'f> {
    info: Arc<FlakeInfo>,
    attr: &'f str,
    hosts: &'f [HostId],
    /// Where to record the result; `None`: not cacheable.
    key: Option<String>,
    hint: Option<Hint>,
}

/// Evaluations waiting for a slot: the longest (per the hints) first,
/// then in order of appearance.
struct EvalQueue<'f> {
    heap: BinaryHeap<Queued<'f>>,
    pushed: usize,
}

struct Queued<'f> {
    eval_ms: u64,
    seq: usize,
    job: Job<'f>,
}

impl Queued<'_> {
    fn rank(&self) -> (u64, Reverse<usize>) {
        (self.eval_ms, Reverse(self.seq))
    }
}

impl PartialEq for Queued<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.rank() == other.rank()
    }
}

impl Eq for Queued<'_> {}

impl PartialOrd for Queued<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

impl Ord for Queued<'_> {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        self.rank().cmp(&other.rank())
    }
}

impl<'f> EvalQueue<'f> {
    fn new() -> Self {
        EvalQueue {
            heap: BinaryHeap::new(),
            pushed: 0,
        }
    }

    fn push(&mut self, job: Job<'f>) {
        let eval_ms = job.hint.map_or(0, |hint| hint.eval_ms);
        self.heap.push(Queued {
            eval_ms,
            seq: self.pushed,
            job,
        });
        self.pushed += 1;
    }

    fn pop(&mut self) -> Option<Job<'f>> {
        self.heap.pop().map(|queued| queued.job)
    }

    fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }
}

/// Whether a resolution made at `resolved_at` is younger than `ttl` at
/// `now` (unix seconds). One from the future (a clock that went back) isn't.
fn is_fresh(resolved_at: i64, now: i64, ttl: Option<Duration>) -> bool {
    let age = now.saturating_sub(resolved_at);
    ttl.is_some_and(|ttl| u64::try_from(age).is_ok_and(|age| Duration::from_secs(age) < ttl))
}

fn with_token_hint(reason: &str) -> String {
    if reason.contains("access-tokens") {
        reason.to_string()
    } else {
        format!("{reason} ({TOKEN_HINT})")
    }
}

/// Whether a run over `mux` opens a new connection: always unmultiplexed;
/// with the private master, unless its socket is already there.
fn opens_connection(mux: &Mux) -> bool {
    match mux {
        Mux::Off => true,
        Mux::Private { socket, .. } => !socket.exists(),
    }
}

/// `dir` (and missing parents) with mode 0700; whether it worked.
fn make_private_dir(dir: &Path) -> bool {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .is_ok()
        && std::fs::set_permissions(dir, Permissions::from_mode(0o700)).is_ok()
}

fn retry_delay() -> Duration {
    let (min, max) = RETRY_DELAY_MS;
    let jitter = util::fnv1a64(probe::nonce().as_bytes()) % (max - min + 1);
    Duration::from_millis(min + jitter)
}

/// ssh's (or a remote command's) stderr without blank lines, `\r`, and
/// notices that aren't failures (`Warning: Permanently added …`, OpenSSH
/// 10's `** WARNING: connection is not using a post-quantum key exchange`
/// block).
fn clean_ssh_stderr(stderr: &str) -> String {
    let mut kept = String::with_capacity(stderr.len().min(MAX_DETAIL_STDERR));
    for line in stderr.lines().map(str::trim_end) {
        if line.trim().is_empty()
            || line.starts_with("** ")
            || line.starts_with("Warning: Permanently added ")
        {
            continue;
        }
        if !kept.is_empty() {
            kept.push('\n');
        }
        kept.push_str(line);
    }
    kept
}

/// Whether ssh failed without saying why: nothing on stderr (once cleaned)
/// or only that a proxy's connection closed ([`remote::is_proxy_closed`]),
/// which is all ControlPersist leaves of a failing ProxyCommand's or
/// ProxyJump's own error.
fn says_nothing(stderr: &str) -> bool {
    clean_ssh_stderr(stderr)
        .lines()
        .all(remote::is_proxy_closed)
}

/// The last non-blank line, trimmed.
fn last_line(text: &str) -> Option<&str> {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
}

/// The line of a failed run's stderr that says why: the last one, or the
/// one before it when that is just a proxy's connection closing (which
/// follows the proxy's own error).
fn reason_line(stderr: &str) -> Option<&str> {
    let mut lines = stderr
        .lines()
        .rev()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let last = lines.next()?;
    match lines.next() {
        Some(before) if remote::is_proxy_closed(last) => Some(before),
        _ => Some(last),
    }
}

fn timed_out(timeout: Option<Duration>) -> String {
    match timeout {
        Some(timeout) => format!("timed out after {}", util::format_duration(timeout)),
        None => "timed out".into(),
    }
}

/// Why a run without a usable answer failed: the timeout, else the line of
/// its (cleaned) stderr that says why ([`reason_line`]), else its exit
/// status (255 over ssh: ssh's own failure).
fn failure_reason(
    out: &Output,
    stderr: &str,
    timeout: Option<Duration>,
    target: &Target,
) -> String {
    if out.timed_out {
        return timed_out(timeout);
    }
    if let Some(line) = reason_line(stderr) {
        return line.to_string();
    }
    match (out.code, target) {
        (Some(255), Target::Ssh(_)) => "ssh failed without a message (exit 255)".into(),
        (Some(code), _) => format!("exit {code} without a message"),
        (None, _) => "killed by a signal".into(),
    }
}

/// A failed nix command's summary: the timeout, else nix's innermost
/// error, else `<what> failed`.
fn nix_summary(out: &Output, what: &str, timeout: Option<Duration>) -> String {
    if out.timed_out {
        format!("{what} {}", timed_out(timeout))
    } else {
        nix::error_summary(&out.stderr, &format!("{what} failed"))
    }
}

fn nix_detail(spec: &CommandSpec, stderr: &str) -> String {
    detail(spec, &nix::clean_stderr(stderr), None)
}

/// An error's detail: `$ <command>`, the cleaned stderr (its end, when
/// long), then `hint: …`.
fn detail(spec: &CommandSpec, stderr: &str, hint: Option<&str>) -> String {
    let mut out = format!("$ {}", spec.display());
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        out.push('\n');
        if stderr.len() > MAX_DETAIL_STDERR {
            let mut start = stderr.len() - MAX_DETAIL_STDERR;
            while !stderr.is_char_boundary(start) {
                start += 1;
            }
            out.push('…');
            out.push_str(&stderr[start..]);
        } else {
            out.push_str(stderr);
        }
    }
    if let Some(hint) = hint {
        out.push_str("\nhint: ");
        out.push_str(hint);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::VecDeque;
    use std::fmt::Write as _;
    use std::sync::MutexGuard;

    use tokio::sync::mpsc;

    use crate::cache::DeployedRev;
    use crate::config::{ActivateOn, Kind, Sources};
    use crate::util::HumanDuration;

    // ─── a scripted runner ──────────────────────────────────────────────

    /// What the fake does for one call.
    #[derive(Debug, Clone, Default)]
    struct Reply {
        /// Virtual milliseconds until it answers.
        delay: u64,
        code: Option<i32>,
        /// `{nonce}` becomes the probe script's nonce.
        stdout: String,
        stderr: String,
        /// The call fails to spawn (at once) with this errno.
        spawn_errno: Option<i32>,
    }

    impl Reply {
        fn ok(stdout: impl Into<String>) -> Reply {
            Reply {
                code: Some(0),
                stdout: stdout.into(),
                ..Reply::default()
            }
        }

        fn exit(code: i32, stderr: impl Into<String>) -> Reply {
            Reply {
                code: Some(code),
                stderr: stderr.into(),
                ..Reply::default()
            }
        }

        fn not_found() -> Reply {
            Reply {
                spawn_errno: Some(libc::ENOENT),
                ..Reply::default()
            }
        }

        fn after(mut self, ms: u64) -> Reply {
            self.delay = ms;
            self
        }

        fn stdout(mut self, stdout: impl Into<String>) -> Reply {
            self.stdout = stdout.into();
            self
        }

        fn with_stderr(mut self, stderr: impl Into<String>) -> Reply {
            self.stderr = stderr.into();
            self
        }
    }

    /// One call the fake saw, with virtual times in milliseconds.
    #[derive(Debug, Clone)]
    struct Call {
        key: String,
        spec: CommandSpec,
        timeout: Option<Duration>,
        start: u64,
        /// Unset while running, and for cancelled calls.
        end: Option<u64>,
        cancelled: bool,
    }

    impl Call {
        fn has(&self, arg: &str) -> bool {
            self.spec.args.iter().any(|a| a == arg)
        }
    }

    #[derive(Default)]
    struct Script {
        replies: HashMap<String, VecDeque<Reply>>,
        calls: Vec<Call>,
        running: HashMap<String, usize>,
        peak: HashMap<String, usize>,
    }

    impl Script {
        /// The next reply for `key` (queued replies in order, the last one
        /// repeating); `eval-ro <attr>` falls back to `eval <attr>`.
        fn next_reply(&mut self, key: &str) -> Reply {
            let fallback = key
                .strip_prefix("eval-ro ")
                .map(|attr| format!("eval {attr}"));
            let key = match fallback {
                Some(fallback) if !self.replies.contains_key(key) => fallback,
                _ => key.to_string(),
            };
            match self.replies.get_mut(&key) {
                Some(queue) if queue.len() > 1 => queue.pop_front().unwrap_or_default(),
                Some(queue) => queue.front().cloned().unwrap_or_default(),
                None => Reply::exit(99, format!("fake: nothing scripted for {key}")),
            }
        }
    }

    /// A [`Runner`] answering from a script, under paused tokio time: delays
    /// are virtual, timeouts are honored (`timed_out`, with whatever the
    /// reply printed), and every call is logged — cancelled ones too.
    struct Fake {
        t0: Instant,
        script: Mutex<Script>,
    }

    impl Fake {
        fn new() -> Arc<Fake> {
            Arc::new(Fake {
                t0: Instant::now(),
                script: Mutex::default(),
            })
        }

        /// Queue `reply` for calls with `key` (see [`key`]).
        fn on(&self, key: &str, reply: Reply) -> &Fake {
            self.lock()
                .replies
                .entry(key.to_string())
                .or_default()
                .push_back(reply);
            self
        }

        /// Answer calls with `key` with `reply` only (instead of what was
        /// queued).
        fn set(&self, key: &str, reply: Reply) -> &Fake {
            self.lock()
                .replies
                .insert(key.to_string(), VecDeque::from([reply]));
            self
        }

        fn runner(self: &Arc<Self>) -> Runner {
            let fake = Arc::clone(self);
            Arc::new(move |spec, timeout| Box::pin(Arc::clone(&fake).run(spec, timeout)))
        }

        fn lock(&self) -> MutexGuard<'_, Script> {
            self.script.lock().unwrap_or_else(PoisonError::into_inner)
        }

        fn now(&self) -> u64 {
            u64::try_from(self.t0.elapsed().as_millis()).unwrap()
        }

        fn calls(&self) -> Vec<Call> {
            self.lock().calls.clone()
        }

        fn calls_to(&self, key: &str) -> Vec<Call> {
            self.calls().into_iter().filter(|c| c.key == key).collect()
        }

        fn keys(&self) -> Vec<String> {
            self.calls().into_iter().map(|c| c.key).collect()
        }

        /// The most calls of a class (`probe`, `eval`, …) that overlapped.
        fn peak(&self, class: &str) -> usize {
            self.lock().peak.get(class).copied().unwrap_or(0)
        }

        fn running(&self) -> usize {
            self.lock().running.values().sum()
        }

        async fn run(
            self: Arc<Self>,
            spec: CommandSpec,
            timeout: Option<Duration>,
        ) -> io::Result<Output> {
            let key = key(&spec);
            let class = class(&key);
            let (reply, index) = {
                let mut script = self.lock();
                let reply = script.next_reply(&key);
                script.calls.push(Call {
                    key,
                    spec: spec.clone(),
                    timeout,
                    start: self.now(),
                    end: None,
                    cancelled: false,
                });
                let running = script.running.entry(class.clone()).or_default();
                *running += 1;
                let running = *running;
                let peak = script.peak.entry(class.clone()).or_default();
                *peak = (*peak).max(running);
                (reply, script.calls.len() - 1)
            };
            let mut call = InFlight {
                fake: &self,
                index,
                class,
                done: false,
            };
            if let Some(errno) = reply.spawn_errno {
                call.end();
                return Err(io::Error::from_raw_os_error(errno));
            }
            let delay = Duration::from_millis(reply.delay);
            let (wait, timed_out) = match timeout {
                Some(timeout) if delay > timeout => (timeout, true),
                _ => (delay, false),
            };
            tokio::time::sleep(wait).await;
            call.end();
            let nonce = probe_nonce(&spec).unwrap_or_default();
            Ok(Output {
                code: if timed_out { None } else { reply.code },
                stdout: reply.stdout.replace("{nonce}", &nonce),
                stderr: reply.stderr,
                elapsed: wait,
                timed_out,
                truncated: false,
            })
        }
    }

    /// A call in flight; dropped unfinished, it was cancelled.
    struct InFlight<'a> {
        fake: &'a Fake,
        index: usize,
        class: String,
        done: bool,
    }

    impl InFlight<'_> {
        fn end(&mut self) {
            self.done = true;
            self.record(false);
        }

        fn record(&self, cancelled: bool) {
            let now = self.fake.now();
            let mut script = self.fake.lock();
            let call = &mut script.calls[self.index];
            if cancelled {
                call.cancelled = true;
            } else {
                call.end = Some(now);
            }
            if let Some(running) = script.running.get_mut(&self.class) {
                *running -= 1;
            }
        }
    }

    impl Drop for InFlight<'_> {
        fn drop(&mut self) {
            if !self.done {
                self.record(true);
            }
        }
    }

    /// What a call is, for scripting: `version`, `metadata <flake>`,
    /// `eval <attr>` / `eval-ro <attr>`, `hosts <locked>`, `probe <login>`,
    /// `closure <login>`, `upload <login>` (`<login>` is `local` for `sh -s`
    /// here), `raw <login> <command>`, `control <login> <command>` (`ssh
    /// -O`), `interactive local`.
    fn key(spec: &CommandSpec) -> String {
        let args: Vec<&str> = spec.args.iter().map(String::as_str).collect();
        let stdin = spec.stdin.as_deref().unwrap_or_default();
        let script = if stdin.contains("hostctl-probe=1") {
            "probe"
        } else if stdin.contains("@closure") {
            "closure"
        } else if stdin.starts_with("umask 077") {
            "upload"
        } else {
            "script"
        };
        let last = args.last().copied().unwrap_or_default();
        match spec.program.as_str() {
            "ssh" => {
                let at = args.iter().position(|a| *a == "--").map_or(0, |i| i + 1);
                let login = args.get(at).copied().unwrap_or("?");
                let rest = args.get(at + 1..).unwrap_or_default();
                if let Some(o) = args[..at].iter().position(|a| *a == "-O") {
                    let command = args.get(o + 1).copied().unwrap_or("?");
                    format!("control {login} {command}")
                } else if rest == ["sh", "-s"] {
                    format!("{script} {login}")
                } else {
                    format!("raw {login} {}", rest.join(" "))
                }
            }
            "sh" if args == ["-s"] => format!("{script} local"),
            "sh" => "interactive local".into(),
            _ if args == ["--version"] => "version".into(),
            _ if args.contains(&"metadata") => format!("metadata {last}"),
            // `commit-time <url>` (NixConfig::commit_time).
            _ if last.contains("builtins.fetchTree") => {
                let url = last.split('"').nth(1).unwrap_or("?");
                format!("commit-time {url}")
            }
            _ if args.contains(&"--raw") => {
                let attr = last.split_once("#.").map_or(last, |(_, attr)| attr);
                let attr = attr.strip_suffix(".outPath").unwrap_or(attr);
                if args.contains(&"--read-only") {
                    format!("eval-ro {attr}")
                } else {
                    format!("eval {attr}")
                }
            }
            _ if args.contains(&"--apply") => {
                let installable = args.iter().find(|a| a.ends_with("#.")).copied();
                format!(
                    "hosts {}",
                    installable.unwrap_or("?").trim_end_matches("#.")
                )
            }
            _ => format!("nix {}", args.join(" ")),
        }
    }

    /// `eval-ro` counts as `eval`.
    fn class(key: &str) -> String {
        match key.split(' ').next().unwrap_or_default() {
            "eval-ro" => "eval".into(),
            class => class.into(),
        }
    }

    /// The nonce of the builtin probe script on `spec`'s stdin.
    fn probe_nonce(spec: &CommandSpec) -> Option<String> {
        const MARK: &str = "nonce=%s\\n' ";
        let stdin = spec.stdin.as_deref()?;
        let at = stdin.find(MARK)? + MARK.len();
        Some(
            stdin[at..]
                .chars()
                .take_while(char::is_ascii_hexdigit)
                .collect(),
        )
    }

    // ─── fixtures ───────────────────────────────────────────────────────

    const NIX_VERSION: &str = "nix (Nix) 2.35.3";
    const GITHUB: &str = "github:o/r";
    const OTHER: &str = "github:o/other";
    const LOCAL: &str = "path:/home/jj/nix-config";
    const REV1: &str = "42ea902131b93c04d098d3bcd02f29305b4bad7c";
    const REV2: &str = "1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d";
    const IFD: &str = include_str!("../tests/fixtures/eval-ifd-read-only.stderr");
    const MISSING_ATTR: &str = include_str!("../tests/fixtures/eval-missing-attr.stderr");
    const GIT_DIRTY: &str = include_str!("../tests/fixtures/metadata-git-dirty.json");
    const FETCH_FAILED: &str = "\
error:
       … while fetching the input 'github:o/r'

       error: unable to download 'https://api.github.com/repos/o/r/commits/HEAD': Could not connect to server (7)
";
    const RATE_LIMITED: &str = "\
error:
       … while fetching the input 'github:o/r'

       error: unable to download 'https://api.github.com/repos/o/r/commits/HEAD': HTTP error 403

       response body:

       {\"message\":\"API rate limit exceeded for 203.0.113.7.\"}
";
    const KEX_RESET: &str = "kex_exchange_identification: read: Connection reset by peer\r\nConnection reset by 10.0.0.5 port 22\r\n";
    const REFUSED: &str = "ssh: connect to host web1 port 22: Connection refused\n";
    const ACCEPT_NEW_74: &str = "command-line line 0: unsupported option \"accept-new\".\n";

    fn github_json(flake: &str, rev: &str) -> String {
        let repo = flake.strip_prefix("github:").unwrap_or(flake);
        format!(
            r#"{{"url":"github:{repo}/{rev}?narHash=sha256-{rev}","revision":"{rev}","lastModified":1792240011,"fingerprint":"fp-{repo}-{rev}","locked":{{"type":"github","rev":"{rev}","narHash":"sha256-{rev}","lastModified":1792240011}}}}"#
        )
    }

    fn github(flake: &str, rev: &str) -> FlakeInfo {
        nix::parse_metadata(flake, &github_json(flake, rev), "").unwrap()
    }

    fn path_json(hash: &str) -> String {
        format!(
            r#"{{"url":"{LOCAL}?lastModified=1792240484&narHash=sha256-{hash}","lastModified":1792240484,"fingerprint":"fp-{hash}","locked":{{"type":"path","narHash":"sha256-{hash}","lastModified":1792240484}}}}"#
        )
    }

    /// A store path whose name tells it apart in event lines; `c` repeated
    /// is its hash (Nix's base32 has no `e`, `o`, `t`, `u`).
    fn sp(c: char, name: &str) -> StorePath {
        let path = format!("/nix/store/{}-{name}", c.to_string().repeat(32));
        StorePath::parse(&path).unwrap_or_else(|| panic!("not a store path: {path}"))
    }

    fn attr(name: &str) -> String {
        format!("homeConfigurations.{name}.activationPackage")
    }

    /// A home-manager host on `github:o/r`, reached as `ssh <name>`.
    fn remote(name: &str) -> Host {
        Host {
            name: name.into(),
            kind: Kind::HomeManager,
            flake: GITHUB.into(),
            attr: attr(name),
            target: Target::Ssh(SshTarget {
                destination: name.into(),
                user: None,
                host: name.into(),
                port: None,
                options: vec![],
                forced_command: None,
            }),
            profile: None,
            activate: Kind::HomeManager.default_activate().map(str::to_owned),
            activate_on: ActivateOn::Target,
            optional: false,
            note: None,
            sources: Sources::default(),
        }
    }

    fn local(name: &str) -> Host {
        Host {
            target: Target::Local { assumed: false },
            ..remote(name)
        }
    }

    fn on_flake(flake: &str, host: Host) -> Host {
        Host {
            flake: flake.into(),
            ..host
        }
    }

    /// Like the desk→Mac tunnel: the route only answers `current-system`.
    fn forced(name: &str) -> Host {
        let mut host = remote(name);
        if let Target::Ssh(t) = &mut host.target {
            t.forced_command = Some("current-system".into());
        }
        host
    }

    fn ids(hosts: Vec<Host>) -> Vec<(HostId, Host)> {
        hosts.into_iter().enumerate().collect()
    }

    fn framed(body: &str) -> String {
        format!("hostctl-probe=1 nonce={{nonce}}\n{body}end={{nonce}}\n")
    }

    /// A probe answer: `current` is active.
    fn running(current: &StorePath) -> Reply {
        Reply::ok(framed(&format!(
            "platform=x86_64-linux\ncurrent={current}\ngeneration=9\nactivated=1792238369\n"
        )))
    }

    /// `nix eval --raw` printing `out`.
    fn evaluates(out: &StorePath) -> Reply {
        Reply::ok(out.as_str())
    }

    /// Multiplexing off: every remote run opens a connection.
    fn config() -> Config {
        Config {
            jobs: Some(8),
            ssh: SshConfig {
                persist: HumanDuration::from_secs(0),
                ..SshConfig::default()
            },
            ..Config::default()
        }
    }

    fn engine(config: &Config, cache: Cache, fake: &Arc<Fake>) -> Engine {
        Engine::assemble(config, cache, Freshness::Auto, fake.runner(), None)
    }

    /// `nix --version` (5 ms) and `github:o/r` at [`REV1`] (300 ms).
    fn world() -> Arc<Fake> {
        let fake = Fake::new();
        fake.on("version", Reply::ok(format!("{NIX_VERSION}\n")).after(5))
            .on(
                &format!("metadata {GITHUB}"),
                Reply::ok(github_json(GITHUB, REV1)).after(300),
            );
        fake
    }

    fn key_of(info: &FlakeInfo) -> String {
        Cache::key(NIX_VERSION, &[], info).unwrap()
    }

    // ─── driving the engine ─────────────────────────────────────────────

    type Seen = Vec<(u64, Event)>;

    /// Collect what `work` sends, stamped with the virtual time.
    async fn collect(
        work: impl Future<Output = ()>,
        mut rx: mpsc::UnboundedReceiver<Event>,
    ) -> Seen {
        let t0 = Instant::now();
        let gather = async {
            let mut seen = Vec::new();
            while let Some(event) = rx.recv().await {
                seen.push((u64::try_from(t0.elapsed().as_millis()).unwrap(), event));
            }
            seen
        };
        let ((), seen) = tokio::join!(work, gather);
        seen
    }

    async fn check(engine: &Engine, hosts: &[(HostId, Host)], known: &[FlakeInfo]) -> Seen {
        let (tx, rx) = mpsc::unbounded_channel();
        collect(async move { engine.check(hosts, known, &tx).await }, rx).await
    }

    async fn probe(engine: &Engine, host: &Host) -> ProbeResult {
        let (tx, rx) = mpsc::unbounded_channel();
        let seen = collect(async move { engine.probe(7, host, &tx).await }, rx).await;
        match seen.as_slice() {
            [
                (_, Event::ProbeStarted(7)),
                (_, Event::ProbeFinished { id: 7, result, .. }),
            ] => result.clone(),
            [
                (_, Event::ProbeStarted(7)),
                (_, Event::Unrunnable(_)),
                (_, Event::ProbeFinished { id: 7, result, .. }),
            ] => result.clone(),
            other => panic!("{:?}", lines(other)),
        }
    }

    async fn diff(
        engine: &Engine,
        host: &Host,
        current: &StorePath,
        desired: &StorePath,
    ) -> (Seen, DiffResult) {
        let (tx, rx) = mpsc::unbounded_channel();
        let seen = collect(
            async move { engine.diff(3, host, current, desired, &tx).await },
            rx,
        )
        .await;
        let result = match seen.as_slice() {
            [
                (
                    _,
                    Event::DiffStarted {
                        id: 3,
                        current: c1,
                        desired: d1,
                    },
                ),
                (
                    _,
                    Event::DiffFinished {
                        id: 3,
                        current: c2,
                        desired: d2,
                        result,
                    },
                ),
            ] if [c1, c2] == [current; 2] && [d1, d2] == [desired; 2] => result.clone(),
            other => panic!("{:?}", lines(other)),
        };
        (seen, result)
    }

    fn show(event: &Event) -> String {
        match event {
            Event::RunStarted => "run+".into(),
            Event::RunFinished => "run-".into(),
            Event::FlakeStarted { url } => format!("flake+ {url}"),
            Event::FlakeFinished {
                url,
                result: Ok(info),
            } => {
                let mut line = format!("flake- {url} {}", info.short_rev());
                if let Some(stale) = &info.stale {
                    let _ = write!(line, " stale: {stale}");
                }
                line
            }
            Event::FlakeFinished {
                url,
                result: Err(e),
            } => format!("flake- {url} error: {}", e.summary),
            Event::ProbeStarted(id) => format!("probe+ {id}"),
            Event::ProbeFinished {
                id,
                result: Ok(report),
                deployed,
            } => {
                let current = report.current.as_ref().map_or("none", StorePath::name);
                let mut line = format!("probe- {id} {current}");
                if let Some(DeployedRev { rev: Some(rev), .. }) = deployed {
                    let _ = write!(line, " (deployed {})", &rev[..7]);
                }
                line
            }
            Event::ProbeFinished {
                id, result: Err(e), ..
            } => {
                format!("probe- {id} {:?}: {}", e.kind, e.error.summary)
            }
            Event::RevisionDated {
                flake,
                rev,
                last_modified,
            } => format!("dated {flake}@{} {last_modified}", &rev[..7]),
            Event::EvalStarted(id) => format!("eval+ {id}"),
            Event::EvalFinished {
                id,
                result: Ok(desired),
            } => format!(
                "eval- {id} {}{}",
                desired.path.name(),
                if desired.cached { " (cached)" } else { "" }
            ),
            Event::EvalFinished { id, result: Err(e) } => {
                format!("eval- {id} error: {}", e.summary)
            }
            Event::DiffStarted { id, .. } => format!("diff+ {id}"),
            Event::DiffFinished { id, result, .. } => match result {
                Ok(DiffOutcome::Diff { diff }) => {
                    format!("diff- {id} {} changes", diff.changes.len())
                }
                Ok(DiffOutcome::DesiredMissing) => format!("diff- {id} desired missing"),
                Err(e) => format!("diff- {id} error: {}", e.summary),
            },
            Event::Error(e) => format!("error: {}", e.summary),
            Event::Unrunnable(e) => format!("unrunnable: {}", e.summary),
            Event::Interrupted => "interrupted".into(),
        }
    }

    fn lines(seen: &[(u64, Event)]) -> Vec<String> {
        seen.iter()
            .map(|(t, e)| format!("{t} {}", show(e)))
            .collect()
    }

    /// The lines without their times.
    fn shown(seen: &[(u64, Event)]) -> Vec<String> {
        seen.iter().map(|(_, e)| show(e)).collect()
    }

    fn eval_errors(seen: &[(u64, Event)]) -> Vec<(HostId, ErrorInfo)> {
        seen.iter()
            .filter_map(|(_, e)| match e {
                Event::EvalFinished {
                    id,
                    result: Err(error),
                } => Some((*id, error.clone())),
                _ => None,
            })
            .collect()
    }

    fn cache_of(engine: &Engine) -> Cache {
        engine.with_cache(|cache| cache.clone())
    }

    // ─── checks ─────────────────────────────────────────────────────────

    #[tokio::test(start_paused = true)]
    async fn a_check_pipelines_probes_resolution_and_evaluations() {
        let fake = world();
        let (web1_old, web1_new) = (sp('1', "web1-old"), sp('2', "web1-new"));
        let (web2, bigmem) = (sp('3', "web2"), sp('4', "bigmem"));
        fake.on("probe web1", running(&web1_old).after(250))
            .on("probe web2", running(&web2).after(400))
            .on("probe local", running(&bigmem).after(10))
            .on(
                &format!("eval-ro {}", attr("web1")),
                evaluates(&web1_new).after(3000),
            )
            .on(
                &format!("eval-ro {}", attr("web2")),
                evaluates(&web2).after(2000),
            )
            .on(
                &format!("eval-ro {}", attr("bigmem")),
                evaluates(&bigmem).after(1000),
            );
        let hosts = ids(vec![remote("web1"), remote("web2"), local("bigmem")]);
        let engine = engine(&config(), Cache::in_memory(), &fake);
        let seen = check(&engine, &hosts, &[]).await;
        assert_eq!(
            lines(&seen),
            [
                "0 run+",
                "0 probe+ 0",
                "0 probe+ 1",
                "0 probe+ 2",
                "0 flake+ github:o/r",
                "10 probe- 2 bigmem",
                "250 probe- 0 web1-old",
                "300 flake- github:o/r 42ea902",
                "300 eval+ 0",
                "300 eval+ 1",
                "300 eval+ 2",
                "400 probe- 1 web2",
                "1300 eval- 2 bigmem",
                "2300 eval- 1 web2",
                "3300 eval- 0 web1-new",
                "3300 run-",
            ]
        );
        // Probes, version and metadata all started at once.
        let starts: Vec<(String, u64)> = fake
            .calls()
            .iter()
            .map(|c| (c.key.clone(), c.start))
            .collect();
        for key in [
            "version",
            "metadata github:o/r",
            "probe web1",
            "probe web2",
            "probe local",
        ] {
            assert!(starts.contains(&(key.to_string(), 0)), "{key}: {starts:?}");
        }
        let metadata = &fake.calls_to("metadata github:o/r")[0];
        assert!(metadata.has("--refresh"));
        let eval = &fake.calls_to(&format!("eval-ro {}", attr("web1")))[0];
        let locked = format!(
            "github:o/r/{REV1}?narHash=sha256-{REV1}#.{}.outPath",
            attr("web1")
        );
        assert_eq!(eval.spec.args.last(), Some(&locked));
        assert_eq!(eval.timeout, Some(Duration::from_secs(600)), "eval-timeout");
        assert_eq!(
            fake.calls_to("probe web1")[0].timeout,
            Some(Duration::from_secs(20))
        );

        // Results and hints are recorded.
        let mut cache = cache_of(&engine);
        let key = key_of(&github(GITHUB, REV1));
        assert_eq!(cache.out_path(&key, &attr("web1"), 0), Some(web1_new));
        assert_eq!(
            cache.hint(GITHUB, &attr("web1")),
            Some(Hint {
                eval_ms: 3000,
                read_only_fails: false
            })
        );
        assert_eq!(cache.resolution(GITHUB).unwrap().info, github(GITHUB, REV1));

        // Desired paths carry the revision.
        let desired = seen.iter().find_map(|(_, e)| match e {
            Event::EvalFinished {
                id: 0,
                result: Ok(d),
            } => Some(d.clone()),
            _ => None,
        });
        assert_eq!(desired.and_then(|d| d.rev).as_deref(), Some(REV1));
    }

    #[tokio::test(start_paused = true)]
    async fn evaluations_overlap_at_most_jobs() {
        let fake = world();
        let names = ["a", "b", "c", "d", "e"];
        for (name, c) in names.iter().zip("abcdf".chars()) {
            fake.on(
                &format!("eval-ro {}", attr(name)),
                evaluates(&sp(c, name)).after(1000),
            )
            .on("probe local", running(&sp('z', "current")));
        }
        let mut config = config();
        config.jobs = Some(2);
        let hosts = ids(names.iter().map(|n| local(n)).collect());
        let engine = engine(&config, Cache::in_memory(), &fake);
        let seen = check(&engine, &hosts, &[github(GITHUB, REV1)]).await;
        assert_eq!(fake.peak("eval"), 2);
        let evals: Vec<String> = lines(&seen)
            .into_iter()
            .filter(|l| l.contains(" eval"))
            .collect();
        assert_eq!(
            evals,
            [
                "5 eval+ 0",
                "5 eval+ 1",
                // Results first, then the freed slots are refilled.
                "1005 eval- 0 a",
                "1005 eval- 1 b",
                "1005 eval+ 2",
                "1005 eval+ 3",
                "2005 eval- 2 c",
                "2005 eval- 3 d",
                "2005 eval+ 4",
                "3005 eval- 4 e",
            ]
        );
        assert_eq!(lines(&seen).last().map(String::as_str), Some("3005 run-"));
        // A known flake isn't resolved again.
        assert!(!shown(&seen).iter().any(|l| l.starts_with("flake")));
        assert!(!fake.keys().iter().any(|k| k.starts_with("metadata")));
    }

    #[tokio::test(start_paused = true)]
    async fn hosts_sharing_an_attribute_share_its_evaluation() {
        let fake = world();
        let shared = "packages.x86_64-linux.toplevel";
        let out = sp('s', "shared");
        fake.on(&format!("eval-ro {shared}"), evaluates(&out).after(1000))
            .on(
                &format!("eval-ro {}", attr("c")),
                evaluates(&sp('c', "c")).after(500),
            );
        for name in ["a", "b", "c"] {
            fake.on(&format!("probe {name}"), running(&out).after(100));
        }
        let hosts = ids(vec![
            Host {
                attr: shared.into(),
                ..remote("a")
            },
            remote("c"),
            Host {
                attr: shared.into(),
                ..remote("b")
            },
        ]);
        let engine = engine(&config(), Cache::in_memory(), &fake);
        let seen = check(&engine, &hosts, &[]).await;
        assert_eq!(fake.calls_to(&format!("eval-ro {shared}")).len(), 1);
        let evals: Vec<String> = lines(&seen)
            .into_iter()
            .filter(|l| l.contains(" eval"))
            .collect();
        assert_eq!(
            evals,
            [
                "300 eval+ 0",
                "300 eval+ 2",
                "300 eval+ 1",
                "800 eval- 1 c",
                "1300 eval- 0 shared",
                "1300 eval- 2 shared",
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_longest_evaluations_start_first() {
        let fake = world();
        let mut cache = Cache::in_memory();
        for (name, eval_ms) in [("a", 1000), ("b", 5000), ("c", 3000)] {
            let hint = Hint {
                eval_ms,
                read_only_fails: false,
            };
            cache.record_hint(GITHUB, &attr(name), hint);
        }
        for name in ["a", "b", "c", "d"] {
            fake.on(
                &format!("eval-ro {}", attr(name)),
                evaluates(&sp('x', name)).after(100),
            )
            .on(&format!("probe {name}"), running(&sp('y', "old")));
        }
        let mut config = config();
        config.jobs = Some(1);
        let hosts = ids(["a", "b", "c", "d"].map(remote).to_vec());
        let engine = engine(&config, cache, &fake);
        let seen = check(&engine, &hosts, &[github(GITHUB, REV1)]).await;
        let started: Vec<String> = shown(&seen)
            .into_iter()
            .filter(|l| l.starts_with("eval+"))
            .collect();
        assert_eq!(started, ["eval+ 1", "eval+ 2", "eval+ 0", "eval+ 3"]);
        let order: Vec<String> = fake
            .keys()
            .into_iter()
            .filter(|k| k.starts_with("eval"))
            .collect();
        assert_eq!(
            order,
            ["b", "c", "a", "d"].map(|n| format!("eval-ro {}", attr(n)))
        );
        // The hints now say what these evaluations took.
        assert_eq!(
            cache_of(&engine)
                .hint(GITHUB, &attr("b"))
                .map(|h| h.eval_ms),
            Some(100)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cached_results_need_no_evaluation() {
        let fake = world();
        let info = github(GITHUB, REV1);
        let mut cache = Cache::in_memory();
        cache.record_resolution(&info, util::now_unix() - 5);
        cache.record_out_path(&key_of(&info), &info, &attr("a"), &sp('a', "a-new"), 1);
        fake.on("probe a", running(&sp('a', "a-new")).after(50))
            .on("probe b", running(&sp('b', "b-old")).after(50))
            .on(
                &format!("eval-ro {}", attr("b")),
                evaluates(&sp('c', "b-new")).after(1000),
            );
        let hosts = ids(vec![remote("a"), remote("b")]);
        let engine = engine(&config(), cache, &fake);
        let seen = check(&engine, &hosts, &[]).await;
        assert_eq!(
            lines(&seen),
            [
                "0 run+",
                "0 probe+ 0",
                "0 probe+ 1",
                "0 flake+ github:o/r",
                "0 flake- github:o/r 42ea902",
                "5 eval- 0 a-new (cached)",
                "5 eval+ 1",
                "50 probe- 0 a-new (deployed 42ea902)",
                "50 probe- 1 b-old",
                "1005 eval- 1 b-new",
                "1005 run-",
            ]
        );
        assert_eq!(
            fake.keys()
                .iter()
                .filter(|k| k.starts_with("eval") || k.starts_with("metadata"))
                .count(),
            1,
            "{:?}",
            fake.keys()
        );
        // The next check finds both.
        let seen = check(&engine, &hosts, &[]).await;
        assert!(!shown(&seen).iter().any(|l| l.starts_with("eval+")));
        assert_eq!(
            fake.keys().iter().filter(|k| k.starts_with("eval")).count(),
            1
        );
        assert_eq!(fake.calls_to("version").len(), 1, "asked once per engine");
    }

    #[tokio::test(start_paused = true)]
    async fn read_only_failures_retry_in_the_same_slot_and_are_remembered() {
        let fake = world();
        let (a, b) = (attr("a"), attr("b"));
        fake.on(
            &format!("metadata {GITHUB}"),
            Reply::ok(github_json(GITHUB, REV2)),
        )
        .on(&format!("eval-ro {a}"), Reply::exit(1, IFD).after(500))
        .on(&format!("eval {a}"), evaluates(&sp('a', "a")).after(1000))
        .on(&format!("eval-ro {b}"), evaluates(&sp('b', "b")).after(100))
        .on("probe local", running(&sp('z', "z")));
        let mut config = config();
        config.jobs = Some(1);
        let hosts = ids(vec![local("a"), local("b")]);
        let cache = Cache::in_memory();
        let engine = Engine::assemble(&config, cache, Freshness::Force, fake.runner(), None);
        let seen = check(&engine, &hosts, &[]).await;
        let evals: Vec<String> = lines(&seen)
            .into_iter()
            .filter(|l| l.contains(" eval"))
            .collect();
        assert_eq!(
            evals,
            [
                "300 eval+ 0",
                "1800 eval- 0 a",
                "1800 eval+ 1",
                "1900 eval- 1 b"
            ]
        );
        let order: Vec<String> = fake
            .keys()
            .into_iter()
            .filter(|k| k.starts_with("eval"))
            .collect();
        assert_eq!(
            order,
            [
                format!("eval-ro {a}"),
                format!("eval {a}"),
                format!("eval-ro {b}")
            ]
        );
        assert_eq!(
            cache_of(&engine).hint(GITHUB, &a),
            Some(Hint {
                eval_ms: 1500,
                read_only_fails: true
            })
        );
        // A new revision: `a` is evaluated without --read-only straight away.
        let seen = check(&engine, &hosts, &[]).await;
        assert!(shown(&seen).contains(&"flake- github:o/r 1a2b3c4".to_string()));
        let order: Vec<String> = fake
            .keys()
            .into_iter()
            .filter(|k| k.starts_with("eval"))
            .skip(3)
            .collect();
        assert_eq!(order, [format!("eval {a}"), format!("eval-ro {b}")]);
    }

    #[tokio::test(start_paused = true)]
    async fn read_only_can_be_turned_off() {
        let fake = world();
        fake.on(&format!("eval {}", attr("a")), evaluates(&sp('a', "a")))
            .on("probe local", running(&sp('a', "a")));
        let mut config = config();
        config.nix.read_only = false;
        let engine = engine(&config, Cache::in_memory(), &fake);
        check(&engine, &ids(vec![local("a")]), &[]).await;
        assert_eq!(fake.calls_to(&format!("eval {}", attr("a"))).len(), 1);
        assert!(!fake.keys().iter().any(|k| k.starts_with("eval-ro")));
    }

    #[tokio::test(start_paused = true)]
    async fn a_flake_that_fails_to_resolve_fails_its_hosts() {
        let fake = world();
        fake.set(
            &format!("metadata {GITHUB}"),
            Reply::exit(1, FETCH_FAILED).after(1300),
        )
        .on(
            &format!("metadata {OTHER}"),
            Reply::ok(github_json(OTHER, REV2)).after(200),
        )
        .on(
            &format!("eval-ro {}", attr("c")),
            evaluates(&sp('c', "c")).after(500),
        );
        for name in ["a", "b", "c"] {
            fake.on(
                &format!("probe {name}"),
                running(&sp('0', "old")).after(100),
            );
        }
        let hosts = ids(vec![remote("a"), remote("b"), on_flake(OTHER, remote("c"))]);
        let engine = engine(&config(), Cache::in_memory(), &fake);
        let seen = check(&engine, &hosts, &[]).await;
        let why = "unable to download 'https://api.github.com/repos/o/r/commits/HEAD': Could not connect to server (7)";
        assert_eq!(
            lines(&seen),
            [
                "0 run+".to_string(),
                "0 probe+ 0".into(),
                "0 probe+ 1".into(),
                "0 probe+ 2".into(),
                "0 flake+ github:o/r".into(),
                "0 flake+ github:o/other".into(),
                "100 probe- 0 old".into(),
                "100 probe- 1 old".into(),
                "100 probe- 2 old".into(),
                "200 flake- github:o/other 1a2b3c4".into(),
                "200 eval+ 2".into(),
                "700 eval- 2 c".into(),
                format!("1300 flake- github:o/r error: {why}"),
                format!("1300 eval- 0 error: {why}"),
                format!("1300 eval- 1 error: {why}"),
                "1300 run-".into(),
            ]
        );
        let errors = eval_errors(&seen);
        let flake_error = seen.iter().find_map(|(_, e)| match e {
            Event::FlakeFinished { result: Err(e), .. } => Some(e.clone()),
            _ => None,
        });
        assert_eq!(Some(&errors[0].1), flake_error.as_ref(), "the same error");
        assert!(errors[0].1.detail.starts_with("$ nix --extra-experimental-features 'nix-command flakes' flake metadata --json --no-write-lock-file --refresh github:o/r\nerror:"), "{}", errors[0].1.detail);
        assert!(!fake.keys().iter().any(|k| k.contains(&attr("a"))));
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_resolution_falls_back_to_the_last_good_one() {
        let fake = world();
        fake.set(
            &format!("metadata {GITHUB}"),
            Reply::exit(1, RATE_LIMITED).after(400),
        )
        .on(
            &format!("eval-ro {}", attr("a")),
            evaluates(&sp('a', "a")).after(100),
        )
        .on("probe a", running(&sp('a', "a")));
        let good = github(GITHUB, REV1);
        let resolved_at = util::now_unix() - 3600;
        let mut cache = Cache::in_memory();
        cache.record_resolution(&good, resolved_at);
        let engine = engine(&config(), cache, &fake);
        let seen = check(&engine, &ids(vec![remote("a")]), &[]).await;
        let stale = "unable to download 'https://api.github.com/repos/o/r/commits/HEAD': HTTP error 403 \
                     (configure access-tokens = github.com=<token> in nix.conf)";
        assert!(
            lines(&seen).contains(&format!("400 flake- github:o/r 42ea902 stale: {stale}")),
            "{:?}",
            lines(&seen)
        );
        assert!(
            lines(&seen).contains(&"500 eval- 0 a".to_string()),
            "{:?}",
            lines(&seen)
        );
        // Evaluated at the last good revision; the stale result isn't recorded.
        let eval = &fake.calls_to(&format!("eval-ro {}", attr("a")))[0];
        assert!(eval.spec.args.last().unwrap().starts_with(&good.locked));
        let resolution = cache_of(&engine).resolution(GITHUB).cloned().unwrap();
        assert_eq!(
            (resolution.info, resolution.resolved_at),
            (good, resolved_at)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn rate_limits_without_a_fallback_suggest_access_tokens() {
        let fake = world();
        fake.set(&format!("metadata {GITHUB}"), Reply::exit(1, RATE_LIMITED));
        let engine = engine(&config(), Cache::in_memory(), &fake);
        let error = engine.resolve(GITHUB).await.unwrap_err();
        assert_eq!(
            error.summary,
            "unable to download 'https://api.github.com/repos/o/r/commits/HEAD': HTTP error 403 \
             (configure access-tokens = github.com=<token> in nix.conf)"
        );
        assert!(
            error.detail.contains("API rate limit exceeded"),
            "{}",
            error.detail
        );
        assert!(
            error
                .detail
                .ends_with("\nhint: configure access-tokens = github.com=<token> in nix.conf"),
            "{}",
            error.detail
        );
        // Other failures get no such hint.
        let fake = world();
        fake.set(&format!("metadata {GITHUB}"), Reply::exit(1, FETCH_FAILED));
        let engine = self::engine(&config(), Cache::in_memory(), &fake);
        let error = engine.resolve(GITHUB).await.unwrap_err();
        assert!(!error.summary.contains("access-tokens") && !error.detail.contains("hint:"));
    }

    #[tokio::test(start_paused = true)]
    async fn nix_falling_back_to_its_own_copy_is_stale_and_not_recorded() {
        let fake = world();
        let stderr = "warning: unable to download 'https://api.github.com/repos/o/r/commits/HEAD': \
                      HTTP error 403; using cached version\n";
        fake.set(
            &format!("metadata {GITHUB}"),
            Reply::ok(github_json(GITHUB, REV1)).with_stderr(stderr),
        );
        let engine = engine(&config(), Cache::in_memory(), &fake);
        let info = engine.resolve(GITHUB).await.unwrap();
        assert_eq!(
            info.stale.as_deref(),
            Some(
                "unable to download 'https://api.github.com/repos/o/r/commits/HEAD': HTTP error 403 \
                 (configure access-tokens = github.com=<token> in nix.conf)"
            )
        );
        assert_eq!(cache_of(&engine).resolution(GITHUB), None);
    }

    #[tokio::test(start_paused = true)]
    async fn young_resolutions_are_reused_only_for_flakes_fetched_remotely() {
        let fake = world();
        fake.on(
            &format!("metadata {LOCAL}"),
            Reply::ok(path_json("new")).after(50),
        );
        let now = util::now_unix();
        let local_info = nix::parse_metadata(LOCAL, &path_json("old"), "").unwrap();
        let mut cache = Cache::in_memory();
        cache.record_resolution(&github(GITHUB, REV2), now - 10);
        cache.record_resolution(&local_info, now - 10);
        cache.record_resolution(&github(OTHER, REV2), now - 120);
        fake.on(
            &format!("metadata {OTHER}"),
            Reply::ok(github_json(OTHER, REV1)),
        );
        let engine = engine(&config(), cache, &fake);
        assert_eq!(engine.resolve(GITHUB).await.unwrap(), github(GITHUB, REV2));
        assert!(fake.calls_to(&format!("metadata {GITHUB}")).is_empty());
        // A path: resolution pins a narHash that every edit changes.
        let info = engine.resolve(LOCAL).await.unwrap();
        assert!(info.locked.contains("sha256-new"), "{}", info.locked);
        let call = &fake.calls_to(&format!("metadata {LOCAL}"))[0];
        assert!(call.has("--refresh"));
        // Older than nix.refresh (60s): refreshed.
        assert_eq!(engine.resolve(OTHER).await.unwrap(), github(OTHER, REV1));
        assert_eq!(fake.calls_to(&format!("metadata {OTHER}")).len(), 1);
        // nix.refresh = 0: always.
        let mut config = config();
        config.nix.refresh = HumanDuration::from_secs(0);
        let mut cache = Cache::in_memory();
        cache.record_resolution(&github(GITHUB, REV2), util::now_unix());
        let engine = self::engine(&config, cache, &fake);
        assert_eq!(engine.resolve(GITHUB).await.unwrap(), github(GITHUB, REV1));
    }

    #[tokio::test(start_paused = true)]
    async fn force_always_refreshes_and_offline_never_does() {
        let fake = world();
        let mut cache = Cache::in_memory();
        cache.record_resolution(&github(GITHUB, REV2), util::now_unix() - 86_400);
        let auto = engine(&config(), cache, &fake);

        let offline = auto.with_freshness(Freshness::Offline);
        assert_eq!(
            offline.resolve(GITHUB).await.unwrap(),
            github(GITHUB, REV2),
            "any age"
        );
        let error = offline.resolve(LOCAL).await.unwrap_err();
        assert_eq!(
            error.summary,
            "path:/home/jj/nix-config was never resolved, and --offline only uses cached resolutions"
        );
        assert!(fake.calls().is_empty());

        let force = auto.with_freshness(Freshness::Force);
        assert_eq!(force.resolve(GITHUB).await.unwrap(), github(GITHUB, REV1));
        assert_eq!(force.resolve(GITHUB).await.unwrap(), github(GITHUB, REV1));
        assert_eq!(
            fake.calls_to(&format!("metadata {GITHUB}")).len(),
            2,
            "even when young"
        );
        // Clones share the cache: the resolution is now young for everyone.
        assert_eq!(auto.resolve(GITHUB).await.unwrap(), github(GITHUB, REV1));
        assert_eq!(fake.calls_to(&format!("metadata {GITHUB}")).len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn eval_failures_explain_themselves_and_timeouts_apply() {
        let fake = world();
        fake.on(
            &format!("eval-ro {}", attr("a")),
            Reply::exit(1, MISSING_ATTR).after(100),
        )
        .on(&format!("eval-ro {}", attr("b")), Reply::ok("«thunk»"))
        .on(
            &format!("eval-ro {}", attr("c")),
            evaluates(&sp('c', "c")).after(10_000),
        )
        .on(&format!("eval-ro {}", attr("d")), Reply::exit(1, "\n"));
        for name in ["a", "b", "c", "d"] {
            fake.on(&format!("probe {name}"), running(&sp('0', "old")));
        }
        let mut config = config();
        config.eval_timeout = HumanDuration::from_secs(5);
        let engine = engine(&config, Cache::in_memory(), &fake);
        let hosts = ids(["a", "b", "c", "d"].map(remote).to_vec());
        let seen = check(&engine, &hosts, &[github(GITHUB, REV1)]).await;
        let errors = eval_errors(&seen);
        let summaries: Vec<(HostId, &str)> = errors
            .iter()
            .map(|(id, e)| (*id, e.summary.as_str()))
            .collect();
        assert_eq!(
            summaries,
            [
                (1, "nix eval printed \"«thunk»\", not a store path"),
                (3, "nix eval failed"),
                (
                    0,
                    "flake 'path:/tmp/hostctl-ifd' does not provide attribute 'packages.x86_64-linux.nope.outPath', 'legacyPackages.x86_64-linux.nope.outPath' or 'nope.outPath'"
                ),
                (2, "nix eval timed out after 5s"),
            ]
        );
        let detail = &errors[2].1.detail;
        assert!(
            detail.starts_with("$ nix --extra-experimental-features 'nix-command flakes' eval --raw --read-only --no-eval-cache"),
            "{detail}"
        );
        assert!(detail.ends_with(MISSING_ATTR.trim_end()), "{detail}");
        assert_eq!(
            fake.calls_to(&format!("eval-ro {}", attr("c")))[0].timeout,
            Some(Duration::from_secs(5))
        );
        // Failures aren't results; their hints are kept.
        let mut cache = cache_of(&engine);
        let key = key_of(&github(GITHUB, REV1));
        assert!(
            ["a", "b", "c", "d"]
                .iter()
                .all(|n| cache.out_path(&key, &attr(n), 0).is_none())
        );
        assert_eq!(
            cache.hint(GITHUB, &attr("c")).map(|h| h.eval_ms),
            Some(5000)
        );
        assert_eq!(
            cache.hint(GITHUB, &attr("a")).map(|h| h.read_only_fails),
            Some(false)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn uncacheable_flakes_and_settings_always_evaluate() {
        // A dirty tree has no identity, so no key.
        let fake = world();
        fake.on("metadata .", Reply::ok(GIT_DIRTY))
            .on(&format!("eval-ro {}", attr("a")), evaluates(&sp('a', "a")))
            .on("probe local", running(&sp('a', "a")));
        let hosts = ids(vec![on_flake(".", local("a"))]);
        let engine = engine(&config(), Cache::in_memory(), &fake);
        for _ in 0..2 {
            check(&engine, &hosts, &[]).await;
        }
        assert_eq!(fake.calls_to(&format!("eval-ro {}", attr("a"))).len(), 2);
        assert_eq!(
            fake.calls_to("metadata .").len(),
            2,
            "local flakes are always resolved"
        );
        assert!(cache_of(&engine).data().flakes.is_empty());

        // cache = false: no version lookup, cached results ignored.
        let fake = world();
        fake.on(&format!("eval-ro {}", attr("a")), evaluates(&sp('a', "a")))
            .on("probe local", running(&sp('a', "a")));
        let info = github(GITHUB, REV1);
        let mut cache = Cache::in_memory();
        cache.record_out_path(&key_of(&info), &info, &attr("a"), &sp('b', "stale"), 1);
        let mut config = config();
        config.cache = false;
        let engine = self::engine(&config, cache, &fake);
        let seen = check(&engine, &ids(vec![local("a")]), std::slice::from_ref(&info)).await;
        assert!(
            shown(&seen).contains(&"eval- 0 a".to_string()),
            "{:?}",
            shown(&seen)
        );
        assert!(fake.calls_to("version").is_empty());

        // No version from nix: no caching either.
        let fake = world();
        fake.set("version", Reply::exit(1, "what?"))
            .on(&format!("eval-ro {}", attr("a")), evaluates(&sp('a', "a")))
            .on("probe local", running(&sp('a', "a")));
        let mut cache = Cache::in_memory();
        cache.record_out_path(&key_of(&info), &info, &attr("a"), &sp('b', "stale"), 1);
        let engine = self::engine(&self::config(), cache, &fake);
        let seen = check(&engine, &ids(vec![local("a")]), &[info]).await;
        assert!(
            shown(&seen).contains(&"eval- 0 a".to_string()),
            "{:?}",
            shown(&seen)
        );
        assert!(
            !shown(&seen)
                .iter()
                .any(|l| l.starts_with("error") || l.starts_with("unrunnable"))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_nix_that_cannot_run_is_reported_once() {
        let fake = Fake::new();
        fake.on("version", Reply::not_found())
            .on(&format!("metadata {GITHUB}"), Reply::not_found())
            .on(&format!("metadata {OTHER}"), Reply::not_found())
            .on(&format!("eval-ro {}", attr("d")), Reply::not_found());
        for name in ["a", "b", "c", "d"] {
            fake.on(
                &format!("probe {name}"),
                running(&sp('0', "old")).after(100),
            );
        }
        let hosts = ids(vec![
            remote("a"),
            remote("b"),
            on_flake(OTHER, remote("c")),
            on_flake("github:o/known", remote("d")),
        ]);
        let engine = engine(&config(), Cache::in_memory(), &fake);
        let known = [github("github:o/known", REV1)];
        let seen = check(&engine, &hosts, &known).await;
        let message = "nix.command = \"nix\" could not be run: No such file or directory (os error 2); \
                       is Nix installed and on PATH?";
        let errors: Vec<&ErrorInfo> = seen
            .iter()
            .filter_map(|(_, e)| match e {
                Event::Error(error) | Event::Unrunnable(error) => Some(error),
                _ => None,
            })
            .collect();
        assert_eq!(errors, [&ErrorInfo::msg(message)]);
        assert!(
            seen.iter()
                .any(|(_, e)| matches!(e, Event::Unrunnable(error) if error.summary == message)),
            "{:?}",
            lines(&seen)
        );
        let failed = eval_errors(&seen);
        assert_eq!(failed.len(), 4);
        for (_, error) in &failed {
            assert_eq!(error, &ErrorInfo::new("nix couldn't be run", message));
        }
        assert!(shown(&seen).contains(&"flake- github:o/r error: nix couldn't be run".to_string()));
        assert!(
            shown(&seen).contains(&"flake- github:o/other error: nix couldn't be run".to_string())
        );
        assert_eq!(
            seen.iter()
                .filter(|(_, e)| matches!(e, Event::ProbeFinished { result: Ok(_), .. }))
                .count(),
            4
        );
        // Outside a check, resolve says the same.
        assert_eq!(
            engine.resolve(GITHUB).await.unwrap_err(),
            ErrorInfo::msg(message)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_cache_names_the_revision_a_host_runs() {
        let fake = world();
        let old = sp('0', "old");
        fake.on("probe a", running(&old)).on(
            &format!("eval-ro {}", attr("a")),
            evaluates(&sp('n', "new")),
        );
        let previous = github(GITHUB, REV2);
        let mut cache = Cache::in_memory();
        cache.record_out_path(&key_of(&previous), &previous, &attr("a"), &old, 1);
        let engine = engine(&config(), cache, &fake);
        let seen = check(&engine, &ids(vec![remote("a")]), &[]).await;
        let deployed = seen.iter().find_map(|(_, e)| match e {
            Event::ProbeFinished { deployed, .. } => Some(deployed.clone()),
            _ => None,
        });
        assert_eq!(
            deployed,
            Some(Some(DeployedRev {
                flake: GITHUB.into(),
                rev: Some(REV2.into()),
                last_modified: Some(1792240011),
            }))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn at_most_ten_new_connections_open_at_once() {
        let names: Vec<String> = (0..15).map(|i| format!("web{i}")).collect();
        let script = |fake: &Fake| {
            for name in &names {
                fake.on(&format!("probe {name}"), running(&sp('a', "a")).after(100));
            }
        };
        let hosts = ids(names.iter().map(|n| remote(n)).collect());
        let fake = world();
        script(&fake);
        let engine = engine(&config(), Cache::in_memory(), &fake);
        let seen = check(&engine, &hosts, &[]).await;
        assert_eq!(fake.peak("probe"), MAX_NEW_CONNECTIONS);
        let last_probe = seen
            .iter()
            .filter(|(_, e)| matches!(e, Event::ProbeFinished { .. }))
            .map(|(t, _)| *t)
            .max();
        assert_eq!(last_probe, Some(200), "two rounds");

        // Live masters (their sockets exist) don't count.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("ssh");
        let mut config = config();
        config.ssh.persist = HumanDuration::from_secs(600);
        std::fs::create_dir(&dir).unwrap();
        for (_, host) in &hosts {
            let Some(target) = host.ssh() else {
                unreachable!()
            };
            match remote::mux_for(target, &config.ssh, Some(&dir)) {
                Mux::Private { socket, .. } => std::fs::write(socket, "").unwrap(),
                Mux::Off => {
                    eprintln!("skipping: {dir:?} is too long for control sockets");
                    return;
                }
            }
        }
        let fake = world();
        script(&fake);
        let engine = Engine::assemble(
            &config,
            Cache::in_memory(),
            Freshness::Auto,
            fake.runner(),
            Some(dir),
        );
        check(&engine, &hosts, &[]).await;
        assert_eq!(fake.peak("probe"), 15);

        // Neither do local hosts.
        let fake = world();
        fake.on("probe local", running(&sp('a', "a")).after(100));
        let engine = self::engine(&self::config(), Cache::in_memory(), &fake);
        let locals = ids(names.iter().map(|n| local(n)).collect());
        check(&engine, &locals, &[]).await;
        assert_eq!(fake.peak("probe"), 15);
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_a_check_cancels_its_work_and_stops_its_events() {
        let fake = world();
        fake.on("probe a", running(&sp('a', "a")).after(200))
            .on("probe b", running(&sp('b', "b")).after(5000))
            .on(
                &format!("eval-ro {}", attr("a")),
                evaluates(&sp('a', "a")).after(3000),
            )
            .on(
                &format!("eval-ro {}", attr("b")),
                evaluates(&sp('b', "b")).after(3000),
            );
        let hosts = ids(vec![remote("a"), remote("b")]);
        let engine = engine(&config(), Cache::in_memory(), &fake);
        let (tx, mut rx) = mpsc::unbounded_channel();
        {
            let check = pin!(engine.check(&hosts, &[], &tx));
            tokio::select! {
                () = check => panic!("finished early"),
                () = tokio::time::sleep(Duration::from_millis(1000)) => {}
            }
        }
        let mut seen = Vec::new();
        while let Ok(event) = rx.try_recv() {
            seen.push(show(&event));
        }
        assert_eq!(
            seen,
            [
                "run+",
                "probe+ 0",
                "probe+ 1",
                "flake+ github:o/r",
                "probe- 0 a",
                "flake- github:o/r 42ea902",
                "eval+ 0",
                "eval+ 1",
            ]
        );
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert!(rx.try_recv().is_err(), "nothing after the drop");
        assert_eq!(fake.running(), 0);
        let cancelled: Vec<String> = fake
            .calls()
            .into_iter()
            .filter(|c| c.cancelled)
            .map(|c| c.key)
            .collect();
        assert_eq!(
            cancelled,
            [
                "probe b".to_string(),
                format!("eval-ro {}", attr("a")),
                format!("eval-ro {}", attr("b"))
            ]
        );
        assert!(fake.calls().iter().all(|c| c.cancelled != c.end.is_some()));
    }

    #[tokio::test(start_paused = true)]
    async fn a_finished_check_saves_the_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("hostctl").join("cache.json");
        let fake = world();
        fake.on("probe local", running(&sp('a', "a")))
            .on(&format!("eval-ro {}", attr("a")), evaluates(&sp('a', "a")));
        let engine = engine(&config(), Cache::open(path.clone()), &fake);
        check(&engine, &ids(vec![local("a")]), &[]).await;
        let mut saved = Cache::open(path);
        let key = key_of(&github(GITHUB, REV1));
        assert_eq!(saved.out_path(&key, &attr("a"), 0), Some(sp('a', "a")));
        assert!(saved.resolution(GITHUB).is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn a_free_slot_takes_the_longest_evaluation_of_any_flake() {
        let fake = world();
        let mut cache = Cache::in_memory();
        for (flake, name, eval_ms) in [
            (GITHUB, "a1", 100),
            (GITHUB, "a2", 200),
            (OTHER, "b1", 9000),
        ] {
            let hint = Hint {
                eval_ms,
                read_only_fails: false,
            };
            cache.record_hint(flake, &attr(name), hint);
        }
        fake.on(
            &format!("metadata {OTHER}"),
            Reply::ok(github_json(OTHER, REV2)).after(50),
        );
        for name in ["a1", "a2", "b1"] {
            fake.on(
                &format!("eval-ro {}", attr(name)),
                evaluates(&sp('a', name)).after(100),
            )
            .on("probe local", running(&sp('a', "a")));
        }
        let mut config = config();
        config.jobs = Some(1);
        let hosts = ids(vec![local("a1"), local("a2"), on_flake(OTHER, local("b1"))]);
        let engine = engine(&config, cache, &fake);
        let seen = check(&engine, &hosts, &[github(GITHUB, REV1)]).await;
        let evals: Vec<String> = lines(&seen)
            .into_iter()
            .filter(|l| l.contains(" eval"))
            .collect();
        // a2 (200 ms) takes the slot first; b1 (9 s), queued at 50 ms by
        // its flake's resolution, goes before a1.
        assert_eq!(
            evals,
            [
                "5 eval+ 1",
                "105 eval- 1 a2",
                "105 eval+ 2",
                "205 eval- 2 b1",
                "205 eval+ 0",
                "305 eval- 0 a1",
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_nix_version_that_could_not_run_is_asked_again_next_time() {
        let fake = world();
        fake.set("version", Reply::not_found())
            .on("version", Reply::ok(NIX_VERSION))
            .on(&format!("eval-ro {}", attr("a")), evaluates(&sp('a', "a")))
            .on("probe local", running(&sp('a', "a")));
        let info = github(GITHUB, REV1);
        let engine = engine(&config(), Cache::in_memory(), &fake);
        let hosts = ids(vec![local("a")]);
        let seen = check(&engine, &hosts, std::slice::from_ref(&info)).await;
        assert!(
            shown(&seen)
                .iter()
                .any(|l| l.starts_with("unrunnable: nix.command")),
            "{:?}",
            shown(&seen)
        );
        assert!(
            cache_of(&engine).data().flakes.is_empty(),
            "no version, no key"
        );
        let seen = check(&engine, &hosts, std::slice::from_ref(&info)).await;
        assert!(
            !shown(&seen)
                .iter()
                .any(|l| l.starts_with("error") || l.starts_with("unrunnable")),
            "{:?}",
            shown(&seen)
        );
        assert_eq!(fake.calls_to("version").len(), 2);
        let mut cache = cache_of(&engine);
        assert_eq!(
            cache.out_path(&key_of(&info), &attr("a"), 0),
            Some(sp('a', "a"))
        );
    }

    /// A probe answer whose generation records the revision it came from.
    fn running_rev(current: &StorePath, rev: &str) -> Reply {
        Reply::ok(framed(&format!(
            "platform=aarch64-darwin\ncurrent={current}\ngeneration=71\nactivated=1792238369\nrevision={rev}\n"
        )))
    }

    #[tokio::test(start_paused = true)]
    async fn the_revision_a_host_runs_is_dated_once_after_the_check() {
        let fake = world();
        fake.on(
            "probe web1",
            running_rev(&sp('b', "branch"), REV2).after(10),
        )
        .on(
            &format!("eval-ro {}", attr("web1")),
            evaluates(&sp('c', "main")).after(100),
        )
        .on(
            &format!("commit-time {GITHUB}/{REV2}"),
            Reply::ok("1792300000").after(50),
        );
        let hosts = ids(vec![remote("web1")]);
        let engine = engine(&config(), Cache::in_memory(), &fake);
        let seen = lines(&check(&engine, &hosts, &[]).await);
        // After the probe and the evaluation, not in their way.
        assert_eq!(
            &seen[seen.len() - 3..],
            [
                "400 eval- 0 main",
                "450 dated github:o/r@1a2b3c4 1792300000",
                "450 run-"
            ],
            "{seen:#?}"
        );
        let call = &fake.calls_to(&format!("commit-time {GITHUB}/{REV2}"))[0];
        assert_eq!(call.timeout, Some(DATING_TIMEOUT));
        assert_eq!(
            call.spec.args.last().map(String::as_str),
            Some(
                format!("builtins.toString (builtins.fetchTree \"{GITHUB}/{REV2}\").lastModified")
                    .as_str()
            ),
            "only the tree is fetched, the flake isn't read"
        );
        // Now the cache knows: the next probe comes with the time.
        let seen = check(&engine, &hosts, &[]).await;
        assert!(!lines(&seen).iter().any(|line| line.contains("dated")));
        assert_eq!(
            fake.calls_to(&format!("commit-time {GITHUB}/{REV2}")).len(),
            1
        );
        let deployed = seen.iter().find_map(|(_, event)| match event {
            Event::ProbeFinished { deployed, .. } => deployed.clone(),
            _ => None,
        });
        assert_eq!(
            deployed,
            Some(DeployedRev {
                flake: GITHUB.into(),
                rev: Some(REV2.into()),
                last_modified: Some(1792300000),
            })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn revisions_are_dated_only_when_they_can_be() {
        // The compared revision itself, a dirty or short one, or a lookup
        // that fails: no date, and the check still ends.
        let fake = world();
        let cases = [
            ("same", REV1.to_string()),
            ("dirty", format!("{REV2}-dirty")),
            ("short", REV2[..7].to_string()),
            ("failing", "e".repeat(40)),
            ("garbled", "f".repeat(40)),
        ];
        for (name, rev) in &cases {
            fake.on(&format!("probe {name}"), running_rev(&sp('b', name), rev))
                .on(
                    &format!("eval-ro {}", attr(name)),
                    evaluates(&sp('c', name)),
                );
        }
        let lookup = |rev: &str| format!("commit-time {GITHUB}/{rev}");
        fake.on(&lookup(&"e".repeat(40)), Reply::exit(1, FETCH_FAILED))
            .on(&lookup(&"f".repeat(40)), Reply::ok("soon"));
        let hosts = ids(cases.iter().map(|(name, _)| remote(name)).collect());
        let engine = engine(&config(), Cache::in_memory(), &fake);
        let seen = lines(&check(&engine, &hosts, &[]).await);
        assert!(!seen.iter().any(|line| line.contains("dated")), "{seen:#?}");
        assert_eq!(seen.last().map(String::as_str), Some("300 run-"));
        let lookups = || {
            let mut keys: Vec<String> = fake
                .calls()
                .iter()
                .map(|c| c.key.clone())
                .filter(|key| key.starts_with("commit-time "))
                .collect();
            keys.sort();
            keys
        };
        assert_eq!(
            lookups(),
            [lookup(&"e".repeat(40)), lookup(&"f".repeat(40))]
        );
        // A failed lookup isn't repeated at the next check, only on --refresh.
        check(&engine, &hosts, &[]).await;
        assert_eq!(lookups().len(), 2);
        let refresh = engine.with_freshness(Freshness::Force);
        check(&refresh, &hosts, &[]).await;
        assert_eq!(lookups().len(), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn dating_has_a_budget_and_takes_turns() {
        let fake = world();
        let revs: Vec<String> = (0..6).map(|i| format!("{i}").repeat(40)).collect();
        for (i, rev) in revs.iter().enumerate() {
            let name = format!("h{i}");
            fake.on(&format!("probe {name}"), running_rev(&sp('b', &name), rev))
                .on(
                    &format!("eval-ro {}", attr(&name)),
                    evaluates(&sp('c', &name)),
                )
                // Hangs past every limit but the last, which answers.
                .on(
                    &format!("commit-time {GITHUB}/{rev}"),
                    if i == 5 {
                        Reply::ok("1792300000").after(10)
                    } else {
                        Reply::ok("1792300000").after(25_000)
                    },
                );
        }
        let hosts = ids((0..6).map(|i| remote(&format!("h{i}"))).collect());
        let engine = engine(&config(), Cache::in_memory(), &fake);
        let seen = lines(&check(&engine, &hosts, &[]).await);
        // Four at once, each cut at 20 s; the last two start at 20 s and
        // h5 is dated then; h4 is cut by the 30 s budget.
        let starts: Vec<u64> = fake
            .calls()
            .iter()
            .filter(|c| c.key.starts_with("commit-time "))
            .map(|c| c.start)
            .collect();
        assert_eq!(starts, [300, 300, 300, 300, 20_300, 20_300], "{seen:#?}");
        assert!(
            seen.contains(&"20310 dated github:o/r@5555555 1792300000".to_string()),
            "{seen:#?}"
        );
        assert_eq!(seen.last().map(String::as_str), Some("30300 run-"));
    }

    #[tokio::test(start_paused = true)]
    async fn offline_checks_date_nothing() {
        let fake = world();
        fake.on("probe web1", running_rev(&sp('b', "branch"), REV2));
        let info = github(GITHUB, REV1);
        let mut cache = Cache::in_memory();
        cache.record_resolution(&info, util::now_unix());
        cache.record_out_path(&key_of(&info), &info, &attr("web1"), &sp('c', "main"), 1);
        let engine = Engine::assemble(&config(), cache, Freshness::Offline, fake.runner(), None);
        let seen = lines(&check(&engine, &ids(vec![remote("web1")]), &[]).await);
        assert!(!seen.iter().any(|line| line.contains("dated")), "{seen:#?}");
        let mut keys = fake.keys();
        keys.sort();
        assert_eq!(keys, ["probe web1", "version"]);
    }

    #[tokio::test(start_paused = true)]
    async fn offline_checks_use_cached_resolutions_and_results_only() {
        let fake = world();
        for name in ["a", "b", "c"] {
            fake.on(
                &format!("eval-ro {}", attr(name)),
                evaluates(&sp('a', name)).after(100),
            )
            .on(&format!("probe {name}"), running(&sp('0', "old")));
        }
        let info = github(GITHUB, REV1);
        let mut cache = Cache::in_memory();
        cache.record_resolution(&info, util::now_unix() - 7 * 86_400);
        cache.record_out_path(&key_of(&info), &info, &attr("a"), &sp('a', "a"), 1);
        let engine = Engine::assemble(&config(), cache, Freshness::Offline, fake.runner(), None);
        // Ids need not be 0..n: they index the front-end's hosts.
        let hosts = vec![
            (4, remote("a")),
            (6, remote("b")),
            (9, on_flake(OTHER, remote("c"))),
        ];
        let seen = check(&engine, &hosts, &[]).await;
        let never = "github:o/other was never resolved, and --offline only uses cached resolutions";
        let missing = "not cached; run hostctl once without --offline";
        assert_eq!(
            lines(&seen),
            [
                "0 run+".to_string(),
                "0 probe+ 4".into(),
                "0 probe+ 6".into(),
                "0 probe+ 9".into(),
                "0 flake+ github:o/r".into(),
                "0 flake+ github:o/other".into(),
                "0 probe- 4 old".into(),
                "0 probe- 6 old".into(),
                "0 probe- 9 old".into(),
                "0 flake- github:o/r 42ea902".into(),
                format!("0 flake- github:o/other error: {never}"),
                format!("0 eval- 9 error: {never}"),
                "5 eval- 4 a (cached)".into(),
                format!("5 eval- 6 error: {missing}"),
                "5 run-".into(),
            ]
        );
        // Nothing but the probes and `nix --version` (for the cache key).
        let mut keys = fake.keys();
        keys.sort();
        assert_eq!(keys, ["probe a", "probe b", "probe c", "version"]);
        let errors = eval_errors(&seen);
        assert!(
            errors[1].1.detail.contains("--offline evaluates nothing"),
            "{}",
            errors[1].1.detail
        );

        // Nor with the cache off: there is nothing to go on.
        let fake = world();
        fake.on("probe a", running(&sp('a', "a")));
        let mut cache = Cache::in_memory();
        cache.record_resolution(&info, util::now_unix());
        let mut config = config();
        config.cache = false;
        let engine = Engine::assemble(&config, cache, Freshness::Offline, fake.runner(), None);
        let seen = check(&engine, &ids(vec![remote("a")]), &[]).await;
        assert!(
            shown(&seen).contains(&format!("eval- 0 error: {missing}")),
            "{:?}",
            shown(&seen)
        );
        assert_eq!(fake.keys(), ["probe a"]);
    }

    #[tokio::test(start_paused = true)]
    async fn hints_survive_evaluations_without_read_only() {
        let fake = world();
        fake.on(
            &format!("eval {}", attr("a")),
            evaluates(&sp('a', "a")).after(300),
        )
        .on("probe local", running(&sp('a', "a")));
        let mut cache = Cache::in_memory();
        let hint = Hint {
            eval_ms: 9000,
            read_only_fails: true,
        };
        cache.record_hint(GITHUB, &attr("a"), hint);
        let mut config = config();
        config.nix.read_only = false;
        let engine = engine(&config, cache, &fake);
        check(&engine, &ids(vec![local("a")]), &[github(GITHUB, REV1)]).await;
        assert_eq!(
            cache_of(&engine).hint(GITHUB, &attr("a")),
            Some(Hint {
                eval_ms: 300,
                read_only_fails: true
            })
        );
    }

    // ─── probes ─────────────────────────────────────────────────────────

    #[tokio::test(start_paused = true)]
    async fn probes_are_classified_by_what_came_back() {
        let ok = |current| Ok(current);
        let unreachable = |why: &str| Err((ProbeFailure::Unreachable, why.to_string()));
        let failed = |why: &str| Err((ProbeFailure::Failed, why.to_string()));
        let partial = "hostctl-probe=1 nonce={nonce}\nplatform=x86_64-linux\n";
        type Want<'a> = Result<&'a str, (ProbeFailure, String)>;
        let cases: Vec<(Reply, Want)> = vec![
            (running(&sp('a', "gen")), ok("gen")),
            // A complete answer counts whatever the exit code.
            (
                Reply {
                    code: Some(1),
                    ..running(&sp('a', "gen"))
                },
                ok("gen"),
            ),
            (
                Reply {
                    code: Some(255),
                    ..running(&sp('a', "gen"))
                },
                ok("gen"),
            ),
            // Login-shell noise before the frame is fine.
            (
                Reply::ok(format!(
                    "Welcome!\n{}",
                    framed(&format!("current={}\n", sp('a', "gen")))
                )),
                ok("gen"),
            ),
            (
                Reply::default().after(30_000),
                unreachable("timed out after 20s"),
            ),
            (Reply::ok(partial).after(30_000), failed(INCOMPLETE)),
            (Reply::ok(partial), failed(INCOMPLETE)),
            (
                Reply::exit(
                    255,
                    "ssh: Could not resolve hostname web1: Name or service not known\r\n",
                ),
                unreachable("ssh: Could not resolve hostname web1: Name or service not known"),
            ),
            (
                Reply::exit(255, "\n"),
                unreachable("ssh failed without a message (exit 255)"),
            ),
            (
                Reply::ok("Welcome!\n"),
                failed("unexpected output: \"Welcome!\""),
            ),
            (
                Reply::ok("Welcome!\n").after(30_000),
                failed("unexpected output: \"Welcome!\" (timed out after 20s)"),
            ),
            (
                Reply::exit(127, "bash: line 1: sh: command not found\n"),
                failed("bash: line 1: sh: command not found"),
            ),
            (Reply::exit(0, ""), failed("printed nothing (exit 0)")),
            (Reply::exit(1, "\n"), failed("printed nothing (exit 1)")),
            // An answer cut short by ssh dying is no less cut short.
            (
                Reply {
                    code: Some(255),
                    ..Reply::ok(partial)
                },
                failed(INCOMPLETE),
            ),
            (Reply::default(), failed("killed by a signal")),
            (
                Reply::ok(framed("current=/nix/store/x\n")),
                failed("bad current= value \"/nix/store/x\""),
            ),
        ];
        for (reply, want) in cases {
            let fake = Fake::new();
            fake.on("probe web1", reply.clone());
            let engine = engine(&config(), Cache::in_memory(), &fake);
            let got = probe(&engine, &remote("web1")).await;
            let got = match &got {
                Ok(report) => Ok(report.current.as_ref().map_or("none", StorePath::name)),
                Err(e) => Err((e.kind, e.error.summary.clone())),
            };
            assert_eq!(got, want, "{reply:?}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn probe_errors_name_the_command_and_give_hints() {
        let fake = Fake::new();
        let changed = "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n\
                       @    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\n\
                       Host key for web1 has changed and you have requested strict checking.\n\
                       Host key verification failed.\r\n";
        fake.on("probe web1", Reply::exit(255, changed));
        let engine = engine(&config(), Cache::in_memory(), &fake);
        let error = probe(&engine, &remote("web1")).await.unwrap_err();
        assert_eq!(error.kind, ProbeFailure::Unreachable);
        assert_eq!(error.error.summary, "Host key verification failed.");
        let spec = &fake.calls()[0].spec;
        assert_eq!(
            error.error.detail,
            format!(
                "$ {}\n{}\nhint: the host key of web1 changed (reinstalled?): if that's expected, run ssh-keygen -R web1",
                spec.display(),
                changed.trim_end().replace('\r', "")
            )
        );
        assert!(
            error.error.detail.starts_with("$ ssh -T -o BatchMode=yes"),
            "{}",
            error.error.detail
        );
    }

    #[tokio::test(start_paused = true)]
    async fn local_probes_run_sh_without_ssh() {
        let fake = Fake::new();
        fake.on("probe local", running(&sp('a', "gen")));
        let engine = engine(&config(), Cache::in_memory(), &fake);
        assert!(probe(&engine, &local("bigmem")).await.is_ok());
        let call = &fake.calls()[0];
        assert_eq!(
            (call.spec.program.as_str(), call.spec.args.as_slice()),
            ("sh", &["-s".to_string()][..])
        );
        assert_eq!(call.spec.max_output, probe::MAX_OUTPUT);

        let fake = Fake::new();
        fake.on("probe local", Reply::not_found());
        let engine = self::engine(&config(), Cache::in_memory(), &fake);
        let (tx, rx) = mpsc::unbounded_channel();
        let host = local("bigmem");
        let seen = collect(
            async {
                engine.probe(0, &host, &tx).await;
                drop(tx)
            },
            rx,
        )
        .await;
        assert_eq!(
            shown(&seen),
            [
                "probe+ 0",
                "probe- 0 Unreachable: couldn't run sh: No such file or directory (os error 2)"
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn forced_command_routes_send_their_command() {
        let fake = Fake::new();
        let system = sp('d', "darwin-system-25.11");
        fake.on("raw mac current-system", Reply::ok(format!("{system}\n")));
        let engine = engine(&config(), Cache::in_memory(), &fake);
        let report = probe(&engine, &forced("mac")).await.unwrap();
        assert_eq!(report.current, Some(system));
        let call = &fake.calls()[0];
        assert_eq!(call.spec.stdin, None);
    }

    #[tokio::test(start_paused = true)]
    async fn an_ssh_that_cannot_run_is_reported_once() {
        let fake = world();
        for name in ["a", "b", "c"] {
            fake.on(&format!("probe {name}"), Reply::not_found()).on(
                &format!("eval-ro {}", attr(name)),
                evaluates(&sp('a', name)),
            );
        }
        let mut config = config();
        config.ssh.command = "ssh".into();
        let engine = engine(&config, Cache::in_memory(), &fake);
        let seen = check(&engine, &ids(["a", "b", "c"].map(remote).to_vec()), &[]).await;
        let message = "ssh.command = \"ssh\" could not be run: No such file or directory (os error 2); \
                       is OpenSSH installed and on PATH?";
        let shown = shown(&seen);
        assert_eq!(
            shown
                .iter()
                .filter(|l| l.starts_with("error") || l.starts_with("unrunnable"))
                .collect::<Vec<_>>(),
            [&format!("unrunnable: {message}")]
        );
        for id in 0..3 {
            assert!(
                shown.contains(&format!("probe- {id} Unreachable: ssh couldn't be run")),
                "{shown:?}"
            );
        }
        let probe_error = seen.iter().find_map(|(_, e)| match e {
            Event::ProbeFinished { result: Err(e), .. } => Some(e.error.clone()),
            _ => None,
        });
        assert_eq!(probe_error.map(|e| e.detail), Some(message.to_string()));
    }

    #[tokio::test(start_paused = true)]
    async fn transient_ssh_failures_are_retried_once_after_a_jittered_delay() {
        let fake = Fake::new();
        fake.on("probe web1", Reply::exit(255, KEX_RESET).after(50))
            .on("probe web1", running(&sp('a', "gen")).after(100));
        let engine = engine(&config(), Cache::in_memory(), &fake);
        assert!(probe(&engine, &remote("web1")).await.is_ok());
        let calls = fake.calls_to("probe web1");
        assert_eq!(calls.len(), 2);
        let pause = calls[1].start - calls[0].end.unwrap();
        assert!((100..=500).contains(&pause), "{pause}");

        // Only once.
        let fake = Fake::new();
        fake.on("probe web1", Reply::exit(255, KEX_RESET));
        let engine = self::engine(&config(), Cache::in_memory(), &fake);
        let error = probe(&engine, &remote("web1")).await.unwrap_err();
        assert_eq!(
            (error.kind, error.error.summary.as_str()),
            (
                ProbeFailure::Unreachable,
                "Connection reset by 10.0.0.5 port 22"
            )
        );
        assert_eq!(fake.calls().len(), 2);

        // Permanent failures aren't retried.
        let fake = Fake::new();
        fake.on("probe web1", Reply::exit(255, REFUSED));
        let engine = self::engine(&config(), Cache::in_memory(), &fake);
        assert!(probe(&engine, &remote("web1")).await.is_err());
        assert_eq!(fake.calls().len(), 1);
    }

    /// An engine with multiplexing in a fresh directory, or `None` when the
    /// temp dir is too long for control sockets.
    fn multiplexing(fake: &Arc<Fake>, tmp: &tempfile::TempDir) -> Option<Engine> {
        let mut config = config();
        config.ssh.persist = HumanDuration::from_secs(600);
        let dir = tmp.path().join("hostctl").join("ssh");
        let Some(target) = remote("web1").ssh().cloned() else {
            unreachable!()
        };
        if remote::mux_for(&target, &config.ssh, Some(&dir)) == Mux::Off {
            eprintln!("skipping: {dir:?} is too long for control sockets");
            return None;
        }
        Some(Engine::assemble(
            &config,
            Cache::in_memory(),
            Freshness::Auto,
            fake.runner(),
            Some(dir),
        ))
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_exit_255_over_the_master_is_retried_without_it() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = Fake::new();
        fake.on("probe web1", Reply::exit(255, ""))
            .on("probe web1", Reply::exit(255, REFUSED));
        let Some(engine) = multiplexing(&fake, &tmp) else {
            return;
        };
        let error = probe(&engine, &remote("web1")).await.unwrap_err();
        assert_eq!(
            (error.kind, error.error.summary.as_str()),
            (
                ProbeFailure::Unreachable,
                "ssh: connect to host web1 port 22: Connection refused"
            )
        );
        let calls = fake.calls();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].has("ControlMaster=auto") && !calls[0].has("ControlPath=none"));
        assert!(calls[1].has("ControlMaster=no") && calls[1].has("ControlPath=none"));
        assert_eq!(calls[1].start, calls[0].end.unwrap(), "at once");

        // Banners alone don't count as a message.
        let fake = Fake::new();
        let banner = "** WARNING: connection is not using a post-quantum key exchange algorithm.\n\
                      ** This session may be vulnerable to \"store now, decrypt later\" attacks.\n";
        fake.on("probe web1", Reply::exit(255, banner))
            .on("probe web1", Reply::exit(255, REFUSED));
        let tmp = tempfile::tempdir().unwrap();
        let Some(engine) = multiplexing(&fake, &tmp) else {
            return;
        };
        assert!(probe(&engine, &remote("web1")).await.is_err());
        assert_eq!(fake.calls().len(), 2);

        // Without multiplexing there is nothing to capture.
        let fake = Fake::new();
        fake.on("probe web1", Reply::exit(255, ""));
        let engine = self::engine(&config(), Cache::in_memory(), &fake);
        let error = probe(&engine, &remote("web1")).await.unwrap_err();
        assert_eq!(
            error.error.summary,
            "ssh failed without a message (exit 255)"
        );
        assert_eq!(fake.calls().len(), 1);
    }

    /// Make it look as if `engine`'s private master for `host` were up
    /// (multiplexing on): its socket exists.
    fn master_up(engine: &Engine, host: &Host) -> PathBuf {
        let dir = engine.inner.control_dir.clone().expect("multiplexing");
        let Some(target) = host.ssh() else {
            unreachable!()
        };
        let Mux::Private { socket, .. } = remote::mux_for(target, &engine.config().ssh, Some(&dir))
        else {
            unreachable!()
        };
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&socket, "").unwrap();
        socket
    }

    #[tokio::test(start_paused = true)]
    async fn a_master_that_stopped_answering_is_told_to_exit_and_bypassed() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = Fake::new();
        // Over the master, nothing comes until the timeout (its connection
        // died without a reset); without it, the host answers.
        fake.on("probe web1", Reply::default().after(30_000))
            .on("probe web1", running(&sp('a', "gen")).after(300))
            .on(
                "control web1 exit",
                Reply::exit(0, "Exit request sent.\r\n"),
            );
        let Some(engine) = multiplexing(&fake, &tmp) else {
            return;
        };
        let socket = master_up(&engine, &remote("web1"));
        let report = probe(&engine, &remote("web1")).await.unwrap();
        assert_eq!(report.current, Some(sp('a', "gen")));
        let calls = fake.calls();
        let keys: Vec<&str> = calls.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, ["probe web1", "control web1 exit", "probe web1"]);
        assert!(calls[0].has("ControlMaster=auto"));
        assert_eq!(calls[0].end, Some(20_000), "the probe timed out");
        assert!(calls[1].has(&format!("ControlPath={}", socket.display())));
        assert_eq!(calls[1].timeout, Some(EXIT_MASTER_TIMEOUT));
        assert_eq!(calls[1].start, 20_000);
        assert!(calls[2].has("ControlMaster=no") && calls[2].has("ControlPath=none"));
        assert_eq!(calls[2].start, calls[1].end.unwrap());
        assert_eq!(calls[2].timeout, Some(Duration::from_secs(20)));

        // Once: what the retry says stands.
        let tmp = tempfile::tempdir().unwrap();
        let fake = Fake::new();
        fake.on("probe web1", Reply::default().after(30_000)).on(
            "control web1 exit",
            Reply::exit(255, "Control socket connect: x\r\n"),
        );
        let Some(engine) = multiplexing(&fake, &tmp) else {
            return;
        };
        master_up(&engine, &remote("web1"));
        let error = probe(&engine, &remote("web1")).await.unwrap_err();
        assert_eq!(
            (error.kind, error.error.summary.as_str()),
            (ProbeFailure::Unreachable, "timed out after 20s")
        );
        assert!(
            error.error.detail.contains("ControlPath=none"),
            "{}",
            error.error.detail
        );
        assert_eq!(fake.calls().len(), 3);

        // A new connection that timed out rode no master: no retry. Nor
        // when the host answered something.
        let partial = "hostctl-probe=1 nonce={nonce}\nplatform=x86_64-linux\n";
        for (reply, up) in [
            (Reply::default().after(30_000), false),
            (Reply::ok(partial).after(30_000), true),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let fake = Fake::new();
            fake.on("probe web1", reply);
            let Some(engine) = multiplexing(&fake, &tmp) else {
                return;
            };
            if up {
                master_up(&engine, &remote("web1"));
            }
            assert!(probe(&engine, &remote("web1")).await.is_err());
            assert_eq!(fake.keys(), ["probe web1"], "master up: {up}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_proxy_failure_the_master_hides_is_retried_without_it() {
        // All ControlPersist leaves of a failing ProxyJump's error (OpenSSH
        // 9.9); unmultiplexed, ssh says why.
        let closed = "Connection closed by UNKNOWN port 65535\r\n";
        let why = "ssh: Could not resolve hostname jump: Name or service not known\r\n";
        let tmp = tempfile::tempdir().unwrap();
        let fake = Fake::new();
        fake.on("probe web1", Reply::exit(255, closed))
            .on("probe web1", Reply::exit(255, format!("{why}{closed}")));
        let Some(engine) = multiplexing(&fake, &tmp) else {
            return;
        };
        let error = probe(&engine, &remote("web1")).await.unwrap_err();
        assert_eq!(
            (error.kind, error.error.summary.as_str()),
            (
                ProbeFailure::Unreachable,
                "ssh: Could not resolve hostname jump: Name or service not known"
            )
        );
        assert!(
            error.error.detail.ends_with(&format!(
                "\n{}{}\nhint: jump doesn't resolve: set the host's destination \
                 (e.g. user@jump.example.com) or give it a Host entry in ~/.ssh/config",
                why.replace('\r', ""),
                closed.trim_end()
            )),
            "{}",
            error.error.detail
        );
        let calls = fake.calls();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(calls[0].has("ControlMaster=auto") && !calls[0].has("ControlPath=none"));
        assert!(calls[1].has("ControlMaster=no") && calls[1].has("ControlPath=none"));
        assert_eq!(calls[1].start, calls[0].end.unwrap(), "at once");

        // A MaxStartups drop behind a ProxyJump is still worth a retry
        // over the master.
        let kex =
            format!("kex_exchange_identification: Connection closed by remote host\r\n{closed}");
        let tmp = tempfile::tempdir().unwrap();
        let fake = Fake::new();
        fake.on("probe web1", Reply::exit(255, kex))
            .on("probe web1", running(&sp('a', "gen")));
        let Some(engine) = multiplexing(&fake, &tmp) else {
            return;
        };
        assert!(probe(&engine, &remote("web1")).await.is_ok());
        let calls = fake.calls();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(calls[1].has("ControlMaster=auto"), "{calls:?}");
        assert!(calls[1].start >= calls[0].end.unwrap() + RETRY_DELAY_MS.0);

        // Without a master, what ssh says comes first.
        let fake = Fake::new();
        fake.on(
            "probe web1",
            Reply::exit(255, format!("proxy: no route to the jump host\r\n{closed}")),
        );
        let engine = self::engine(&config(), Cache::in_memory(), &fake);
        let error = probe(&engine, &remote("web1")).await.unwrap_err();
        assert_eq!(error.error.summary, "proxy: no route to the jump host");
        assert_eq!(fake.calls().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn the_control_directory_is_made_private_on_first_remote_use() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = Fake::new();
        fake.on("probe local", running(&sp('a', "a")))
            .on("probe web1", running(&sp('a', "a")));
        let Some(engine) = multiplexing(&fake, &tmp) else {
            return;
        };
        let dir = tmp.path().join("hostctl").join("ssh");
        assert!(probe(&engine, &local("bigmem")).await.is_ok());
        assert!(!dir.exists(), "no remote host yet");
        assert!(probe(&engine, &remote("web1")).await.is_ok());
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let call = &fake.calls_to("probe web1")[0];
        let control_path = call
            .spec
            .args
            .iter()
            .find_map(|a| a.strip_prefix("ControlPath="))
            .unwrap();
        assert!(Path::new(control_path).starts_with(&dir), "{control_path}");

        // When it can't be made, multiplexing is off.
        let fake = Fake::new();
        fake.on("probe web1", running(&sp('a', "a")));
        let blocker = tmp.path().join("file");
        std::fs::write(&blocker, "").unwrap();
        let mut config = config();
        config.ssh.persist = HumanDuration::from_secs(600);
        let engine = Engine::assemble(
            &config,
            Cache::in_memory(),
            Freshness::Auto,
            fake.runner(),
            Some(blocker.join("ssh")),
        );
        assert!(probe(&engine, &remote("web1")).await.is_ok());
        assert!(fake.calls()[0].has("ControlPath=none"));
    }

    #[tokio::test(start_paused = true)]
    async fn an_ssh_without_accept_new_gets_strict_checking_from_then_on() {
        let fake = Fake::new();
        fake.on("probe web1", Reply::exit(255, ACCEPT_NEW_74))
            .on("probe web1", running(&sp('a', "a")))
            .on("probe web2", running(&sp('a', "a")))
            .on("upload web2", Reply::ok("/tmp/hostctl.x\n"));
        let engine = engine(&config(), Cache::in_memory(), &fake);
        assert!(probe(&engine, &remote("web1")).await.is_ok());
        let calls = fake.calls();
        assert!(
            calls[0].has("StrictHostKeyChecking=accept-new")
                && !calls[0].has("StrictHostKeyChecking=yes")
        );
        assert!(
            calls[1].has("StrictHostKeyChecking=yes")
                && !calls[1].has("StrictHostKeyChecking=accept-new")
        );
        // It sticks: for other hosts, other commands and clones.
        let other = engine.with_freshness(Freshness::Force);
        assert!(probe(&other, &remote("web2")).await.is_ok());
        assert_eq!(fake.calls().len(), 3);
        assert!(fake.calls()[2].has("StrictHostKeyChecking=yes"));
        let spec = engine
            .prepare_interactive(&remote("web2"), "true")
            .await
            .unwrap()
            .spec;
        assert!(fake.calls()[3].has("StrictHostKeyChecking=yes"));
        assert!(spec.args.windows(2).any(|w| w == STRICT_HOST_KEYS));
    }

    // ─── diffs ──────────────────────────────────────────────────────────

    /// A closure listing: each root's closure (`None`: missing), then `@end`.
    fn listing(roots: &[(&StorePath, Option<&[ClosureEntry]>)]) -> String {
        let mut out = String::from("Welcome!\n");
        for (root, closure) in roots {
            match closure {
                Some(entries) => {
                    let _ = writeln!(out, "@closure {root}");
                    for entry in *entries {
                        let _ = writeln!(out, "{}\t{}", entry.path, entry.size);
                    }
                }
                None => {
                    let _ = writeln!(out, "@missing {root}");
                }
            }
        }
        out.push_str("@end\n");
        out
    }

    struct Generations {
        current: StorePath,
        desired: StorePath,
        before: Vec<ClosureEntry>,
        after: Vec<ClosureEntry>,
    }

    fn generations() -> Generations {
        let entry = |path: StorePath, size| ClosureEntry { path, size };
        let current = sp('1', "home-manager-generation");
        let desired = sp('2', "home-manager-generation");
        Generations {
            before: vec![
                entry(current.clone(), 1000),
                entry(sp('3', "atuin-18.0.0"), 5_000_000),
            ],
            after: vec![
                entry(desired.clone(), 1000),
                entry(sp('4', "atuin-18.1.0"), 5_100_000),
            ],
            current,
            desired,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn diffs_list_both_closures_on_the_target_and_the_desired_one_here() {
        let g = generations();
        let fake = Fake::new();
        fake.on(
            "closure web1",
            Reply::ok(listing(&[
                (&g.current, Some(&g.before)),
                (&g.desired, Some(&g.after)),
            ]))
            .after(200),
        )
        .on(
            "closure local",
            Reply::ok(listing(&[(&g.desired, None)])).after(50),
        );
        let engine = engine(&config(), Cache::in_memory(), &fake);
        let (seen, result) = diff(&engine, &remote("web1"), &g.current, &g.desired).await;
        assert_eq!(lines(&seen), ["0 diff+ 3", "200 diff- 3 1 changes"]);
        let Ok(DiffOutcome::Diff { diff }) = result else {
            panic!("{result:?}")
        };
        assert_eq!(diff, closure::diff(&g.before, &g.after));
        let calls = fake.calls();
        assert_eq!(
            calls
                .iter()
                .map(|c| (c.key.as_str(), c.start))
                .collect::<Vec<_>>(),
            [("closure web1", 0), ("closure local", 0)]
        );
        let remote_script = calls[0].spec.stdin.clone().unwrap();
        assert_eq!(
            remote_script,
            actions::closure_script(&[&g.current, &g.desired])
        );
        assert_eq!(
            calls[1].spec.stdin.clone().unwrap(),
            actions::closure_script(&[&g.desired])
        );
        assert!(
            calls
                .iter()
                .all(|c| c.spec.max_output == actions::MAX_CLOSURE_OUTPUT)
        );
        assert_eq!(calls[0].timeout, Some(Duration::from_secs(60)));
    }

    #[tokio::test(start_paused = true)]
    async fn the_desired_closure_comes_from_wherever_it_is() {
        let g = generations();
        // Only here.
        let fake = Fake::new();
        fake.on(
            "closure web1",
            Reply::ok(listing(&[
                (&g.current, Some(&g.before)),
                (&g.desired, None),
            ])),
        )
        .on(
            "closure local",
            Reply::ok(listing(&[(&g.desired, Some(&g.after))])),
        );
        let engine = engine(&config(), Cache::in_memory(), &fake);
        let (_, result) = diff(&engine, &remote("web1"), &g.current, &g.desired).await;
        assert_eq!(
            result,
            Ok(DiffOutcome::Diff {
                diff: closure::diff(&g.before, &g.after)
            })
        );

        // Nowhere.
        let fake = Fake::new();
        fake.on(
            "closure web1",
            Reply::ok(listing(&[
                (&g.current, Some(&g.before)),
                (&g.desired, None),
            ])),
        )
        .on("closure local", Reply::ok(listing(&[(&g.desired, None)])));
        let engine = self::engine(&config(), Cache::in_memory(), &fake);
        let (seen, result) = diff(&engine, &remote("web1"), &g.current, &g.desired).await;
        assert_eq!(result, Ok(DiffOutcome::DesiredMissing));
        assert_eq!(shown(&seen)[1], "diff- 3 desired missing");

        // Not here either, because this machine's listing failed.
        let fake = Fake::new();
        fake.on(
            "closure web1",
            Reply::ok(listing(&[
                (&g.current, Some(&g.before)),
                (&g.desired, None),
            ])),
        )
        .on(
            "closure local",
            Reply::exit(
                1,
                "error: cannot open connection to remote store 'daemon'\n",
            ),
        );
        let engine = self::engine(&config(), Cache::in_memory(), &fake);
        let (_, result) = diff(&engine, &remote("web1"), &g.current, &g.desired).await;
        assert_eq!(
            result.unwrap_err().summary,
            "couldn't list closures on this machine: error: cannot open connection to remote store 'daemon'"
        );

        // Not needed from here: a failure here doesn't matter.
        let fake = Fake::new();
        fake.on(
            "closure web1",
            Reply::ok(listing(&[
                (&g.current, Some(&g.before)),
                (&g.desired, Some(&g.after)),
            ])),
        )
        .on(
            "closure local",
            Reply::exit(127, "hostctl: nix-store not found on PATH\n"),
        );
        let engine = self::engine(&config(), Cache::in_memory(), &fake);
        let (_, result) = diff(&engine, &remote("web1"), &g.current, &g.desired).await;
        assert!(matches!(result, Ok(DiffOutcome::Diff { .. })), "{result:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn diffs_fail_clearly() {
        let g = generations();
        let run = |reply: Reply| {
            let fake = Fake::new();
            fake.on("closure web1", reply)
                .on("closure local", Reply::ok(listing(&[(&g.desired, None)])));
            fake
        };
        for (reply, want) in [
            (
                Reply::exit(
                    127,
                    "hostctl: nix-store not found on PATH (/usr/bin:/bin)\n",
                ),
                "nix-store isn't installed on web1",
            ),
            (
                Reply::exit(255, REFUSED),
                "couldn't list closures on web1: ssh: connect to host web1 port 22: Connection refused",
            ),
            (
                Reply::default().after(120_000),
                "couldn't list closures on web1: timed out after 1m",
            ),
            (
                Reply::ok(listing(&[(&g.current, None), (&g.desired, None)])),
                "the current generation isn't valid in web1's store: /nix/store/11111111111111111111111111111111-home-manager-generation",
            ),
            (
                Reply::ok(listing(&[
                    (&g.desired, Some(&g.after)),
                    (&g.current, Some(&g.before)),
                ])),
                "web1 listed other closures than requested",
            ),
            (
                Reply::ok(listing(&[(&g.current, Some(&g.before))])),
                "web1 listed other closures than requested",
            ),
            (
                Reply::ok("@closure /nix/store/x\n@end\n"),
                "couldn't read the closures listed on web1: closure listing line 1: not a store path: \"@closure /nix/store/x\"",
            ),
            (
                Reply::ok(format!("@closure {}\n", g.current)),
                "couldn't read the closures listed on web1: closure listing ended early (no @end): the connection dropped or the output was cut",
            ),
        ] {
            let fake = run(reply.clone());
            let engine = engine(&config(), Cache::in_memory(), &fake);
            let (_, result) = diff(&engine, &remote("web1"), &g.current, &g.desired).await;
            assert_eq!(
                result.map_err(|e| e.summary),
                Err(want.to_string()),
                "{reply:?}"
            );
        }
        // The detail has the command and what ssh said.
        let fake = run(Reply::exit(
            255,
            "ssh: Could not resolve hostname web1: Name or service not known\n",
        ));
        let engine = engine(&config(), Cache::in_memory(), &fake);
        let (_, result) = diff(&engine, &remote("web1"), &g.current, &g.desired).await;
        let detail = result.unwrap_err().detail;
        assert!(detail.starts_with("$ ssh -T "), "{detail}");
        assert!(detail.contains("\nhint: web1 doesn't resolve"), "{detail}");
    }

    #[tokio::test(start_paused = true)]
    async fn local_hosts_and_forced_routes_diff_in_this_machines_store() {
        let g = generations();
        let both = listing(&[(&g.current, Some(&g.before)), (&g.desired, Some(&g.after))]);
        for host in [local("bigmem"), forced("mac")] {
            let fake = Fake::new();
            fake.on("closure local", Reply::ok(both.clone()));
            let engine = engine(&config(), Cache::in_memory(), &fake);
            let (_, result) = diff(&engine, &host, &g.current, &g.desired).await;
            assert!(matches!(result, Ok(DiffOutcome::Diff { .. })), "{result:?}");
            assert_eq!(fake.keys(), ["closure local"], "{}", host.name);
            assert_eq!(
                fake.calls()[0].spec.stdin.clone().unwrap(),
                actions::closure_script(&[&g.current, &g.desired])
            );
        }
        // What isn't here can't be had over a forced route.
        let fake = Fake::new();
        fake.on(
            "closure local",
            Reply::ok(listing(&[(&g.current, None), (&g.desired, Some(&g.after))])),
        );
        let engine = engine(&config(), Cache::in_memory(), &fake);
        let (_, result) = diff(&engine, &forced("mac"), &g.current, &g.desired).await;
        let error = result.unwrap_err();
        assert!(error.summary.starts_with("the current generation of mac isn't in this machine's store, and mac's ssh route can't list closures"), "{}", error.summary);
        let fake = Fake::new();
        fake.on(
            "closure local",
            Reply::ok(listing(&[
                (&g.current, Some(&g.before)),
                (&g.desired, None),
            ])),
        );
        let engine = self::engine(&config(), Cache::in_memory(), &fake);
        let (_, result) = diff(&engine, &forced("mac"), &g.current, &g.desired).await;
        assert_eq!(result, Ok(DiffOutcome::DesiredMissing));
        // nix-store missing here.
        let fake = Fake::new();
        fake.on("closure local", Reply::exit(127, ""));
        let engine = self::engine(&config(), Cache::in_memory(), &fake);
        let (_, result) = diff(&engine, &local("bigmem"), &g.current, &g.desired).await;
        assert_eq!(
            result.unwrap_err().summary,
            "nix-store isn't installed on bigmem"
        );
    }

    // ─── interactive commands ───────────────────────────────────────────

    #[tokio::test(start_paused = true)]
    async fn interactive_commands_run_here_for_local_hosts_and_push_tools() {
        let fake = Fake::new();
        let engine = engine(&config(), Cache::in_memory(), &fake);
        let here = remote::interactive_command(
            &Target::Local { assumed: false },
            &config().ssh,
            "make it so",
            None,
        );
        assert_eq!(
            engine
                .prepare_interactive(&local("bigmem"), "make it so")
                .await,
            Ok(Prepared {
                spec: here.clone(),
                upload: None
            })
        );
        let push = Host {
            activate_on: ActivateOn::Local,
            ..forced("mac")
        };
        assert_eq!(
            engine.prepare_interactive(&push, "make it so").await,
            Ok(Prepared {
                spec: here,
                upload: None
            })
        );
        assert!(fake.calls().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn remote_interactive_commands_are_uploaded_first() {
        let fake = Fake::new();
        fake.on(
            "upload web1",
            Reply::ok("Welcome to web1!\n  /tmp/hostctl.AbC123  \n").after(30),
        );
        let engine = engine(&config(), Cache::in_memory(), &fake);
        let host = remote("web1");
        let prepared = engine.prepare_interactive(&host, "echo hi").await.unwrap();
        assert_eq!(prepared.upload.as_deref(), Some("/tmp/hostctl.AbC123"));
        let spec = prepared.spec;
        let want = remote::interactive_command(
            &host.target,
            &config().ssh,
            "echo hi",
            Some("/tmp/hostctl.AbC123"),
        );
        assert_eq!(spec, want);
        assert_eq!(
            spec.args[spec.args.len() - 2..],
            ["sh", "/tmp/hostctl.AbC123"]
        );
        let call = &fake.calls()[0];
        let Some(target) = host.ssh() else {
            unreachable!()
        };
        let upload = remote::upload_command(
            target,
            &config().ssh,
            &Mux::Off,
            &remote::interactive_script("echo hi", true),
        );
        assert_eq!(call.spec, upload);
        assert_eq!(call.timeout, Some(Duration::from_secs(20)));
    }

    #[tokio::test(start_paused = true)]
    async fn an_upload_is_discarded_over_the_unattended_channel() {
        let fake = Fake::new();
        fake.on("script web1", Reply::ok(""));
        let engine = engine(&config(), Cache::in_memory(), &fake);
        let host = remote("web1");
        engine.discard_upload(&host, "/tmp/hostctl.AbC123").await;
        let calls = fake.calls();
        assert_eq!(calls.len(), 1);
        let Some(target) = host.ssh() else {
            unreachable!()
        };
        let want = remote::script_command(
            &host.target,
            &config().ssh,
            &Mux::Off,
            "rm -f -- /tmp/hostctl.AbC123\n",
        );
        assert_eq!(calls[0].spec, want);
        assert!(target.forced_command.is_none());
        assert_eq!(calls[0].timeout, Some(CLEANUP_TIMEOUT));
        // A failure is nobody's business; nothing that isn't a path we
        // uploaded is touched, and nothing runs for local hosts.
        let fake = Fake::new();
        fake.on("script web1", Reply::exit(255, REFUSED));
        let engine = self::engine(&config(), Cache::in_memory(), &fake);
        engine.discard_upload(&host, "/tmp/hostctl.x").await;
        engine
            .discard_upload(&host, "/tmp/hostctl.x; rm -rf ~")
            .await;
        engine
            .discard_upload(&local("bigmem"), "/tmp/hostctl.x")
            .await;
        assert_eq!(fake.calls().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn interactive_commands_refuse_what_cannot_work() {
        for (reply, want) in [
            (
                Reply::ok("/tmp/hostctl.a b\n"),
                "uploading the command to web1 printed \"/tmp/hostctl.a b\", not the path of a temporary file",
            ),
            (
                Reply::ok("tmp/hostctl.x\n"),
                "uploading the command to web1 printed \"tmp/hostctl.x\", not the path of a temporary file",
            ),
            (
                Reply::ok(""),
                "uploading the command to web1 printed \"\", not the path of a temporary file",
            ),
            (
                Reply::exit(255, REFUSED),
                "couldn't upload the command to web1: ssh: connect to host web1 port 22: Connection refused",
            ),
            (
                Reply::exit(1, "").stdout("/tmp/hostctl.x\n"),
                "couldn't upload the command to web1: exit 1 without a message",
            ),
            (
                Reply::not_found(),
                "ssh.command = \"ssh\" could not be run: No such file or directory (os error 2); is OpenSSH installed and on PATH?",
            ),
        ] {
            let fake = Fake::new();
            fake.on("upload web1", reply.clone());
            let engine = engine(&config(), Cache::in_memory(), &fake);
            let error = engine
                .prepare_interactive(&remote("web1"), "true")
                .await
                .unwrap_err();
            assert_eq!(error.summary, want, "{reply:?}");
        }
        let fake = Fake::new();
        let engine = engine(&config(), Cache::in_memory(), &fake);
        let error = engine
            .prepare_interactive(&forced("mac"), "true")
            .await
            .unwrap_err();
        assert_eq!(
            error.summary,
            "mac only answers its probe command: nothing else can run there"
        );
        assert!(fake.calls().is_empty());
    }

    // ─── discovery ──────────────────────────────────────────────────────

    const HOSTS_JSON: &str = r#"{"nixos":["web1"],"darwin":[],"home":["bigmem"],"inventory":{"web1":{"destination":"root@web1.lan"}}}"#;

    #[tokio::test(start_paused = true)]
    async fn discovery_is_one_evaluation_then_cached() {
        let fake = world();
        let locked = github(GITHUB, REV1).locked;
        fake.on(&format!("hosts {locked}"), Reply::ok(HOSTS_JSON).after(70));
        let engine = engine(&config(), Cache::in_memory(), &fake);
        let (info, hosts) = engine.discover(GITHUB).await.unwrap();
        assert_eq!(info, github(GITHUB, REV1));
        assert_eq!(hosts, nix::parse_hosts(HOSTS_JSON).unwrap());
        let call = &fake.calls_to(&format!("hosts {locked}"))[0];
        assert_eq!(call.spec, config().nix.hosts(&locked, "lib.hostctl"));
        assert_eq!(call.start, 300, "after resolving");
        assert_eq!(fake.calls_to("version")[0].start, 0, "alongside resolving");
        assert_eq!(engine.discover(GITHUB).await.unwrap().1, hosts);
        assert_eq!(fake.calls_to(&format!("hosts {locked}")).len(), 1);

        // Another inventory attribute is another key.
        let mut config = config();
        config.inventory = "fleet".into();
        let other = Engine::assemble(
            &config,
            cache_of(&engine),
            Freshness::Auto,
            fake.runner(),
            None,
        );
        other.discover(GITHUB).await.unwrap();
        assert_eq!(fake.calls_to(&format!("hosts {locked}")).len(), 2);
        assert_eq!(
            fake.calls_to(&format!("hosts {locked}"))[1].spec,
            config.nix.hosts(&locked, "fleet")
        );

        // The cache is saved on request.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cache.json");
        let saving = Engine::assemble(
            &self::config(),
            Cache::open(path.clone()),
            Freshness::Auto,
            fake.runner(),
            None,
        );
        saving.discover(GITHUB).await.unwrap();
        assert!(!path.exists());
        saving.save_cache();
        assert!(
            Cache::open(path)
                .data()
                .flakes
                .values()
                .any(|entry| entry.hosts.is_some())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_hosts_read_last_are_at_hand_without_running_anything() {
        let fake = world();
        let locked = github(GITHUB, REV1).locked;
        fake.on(&format!("hosts {locked}"), Reply::ok(HOSTS_JSON));
        let engine = engine(&config(), Cache::in_memory(), &fake);
        assert_eq!(engine.cached_hosts(GITHUB), None);
        let (_, hosts) = engine.discover(GITHUB).await.unwrap();
        let calls = fake.calls().len();
        assert_eq!(engine.cached_hosts(GITHUB), Some(hosts));
        assert_eq!(fake.calls().len(), calls);
        assert_eq!(engine.cached_hosts("github:o/other"), None);
        // Another inventory attribute's hosts aren't these.
        let mut fleet = config();
        fleet.inventory = "fleet".into();
        let other = Engine::assemble(
            &fleet,
            cache_of(&engine),
            Freshness::Auto,
            fake.runner(),
            None,
        );
        assert_eq!(other.cached_hosts(GITHUB), None);
        let mut uncached = config();
        uncached.cache = false;
        let uncached = Engine::assemble(
            &uncached,
            cache_of(&engine),
            Freshness::Auto,
            fake.runner(),
            None,
        );
        assert_eq!(uncached.cached_hosts(GITHUB), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_check_and_discovery_beside_it_resolve_the_flake_once() {
        let fake = world();
        let locked = github(GITHUB, REV1).locked;
        fake.on(&format!("hosts {locked}"), Reply::ok(HOSTS_JSON).after(70))
            .on("probe web1", running(&sp('a', "home-manager-generation")))
            .on(&format!("eval {}", attr("web1")), evaluates(&sp('a', "x")));
        let engine = engine(&config(), Cache::in_memory(), &fake).with_freshness(Freshness::Force);
        let hosts = ids(vec![remote("web1")]);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (discovered, ()) =
            tokio::join!(engine.discover(GITHUB), engine.check(&hosts, &[], &tx));
        assert_eq!(discovered.unwrap().0, github(GITHUB, REV1));
        let metadata = format!("metadata {GITHUB}");
        assert_eq!(fake.calls_to(&metadata).len(), 1, "{:?}", fake.keys());
        // The check reported the resolution it shared as its own.
        let mut seen = Vec::new();
        while let Ok(event) = rx.try_recv() {
            seen.push(show(&event));
        }
        assert!(
            seen.contains(&"flake- github:o/r 42ea902".to_string()),
            "{seen:?}"
        );
        // Done, it isn't kept: the next one runs again.
        engine.resolve(GITHUB).await.unwrap();
        assert_eq!(fake.calls_to(&metadata).len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_shared_resolution_nobody_awaits_any_more_is_cancelled() {
        let fake = world();
        let engine = engine(&config(), Cache::in_memory(), &fake).with_freshness(Freshness::Force);
        tokio::select! {
            _ = async { tokio::join!(engine.resolve(GITHUB), engine.resolve(GITHUB)) } => {
                panic!("resolved in no time")
            }
            () = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
        let calls = fake.calls_to(&format!("metadata {GITHUB}"));
        assert_eq!(calls.len(), 1);
        assert!(calls[0].cancelled, "{calls:?}");
        // Another freshness is another resolution.
        let auto = engine.with_freshness(Freshness::Auto);
        let (forced, auto) = tokio::join!(engine.resolve(GITHUB), auto.resolve(GITHUB));
        assert_eq!(forced, auto);
        assert_eq!(fake.calls_to(&format!("metadata {GITHUB}")).len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn discovery_failures_name_the_flake() {
        let locked = github(GITHUB, REV1).locked;
        for (reply, want) in [
            (
                Reply::exit(1, "error: attribute 'hostctl' missing\n"),
                "can't read the hosts of github:o/r: attribute 'hostctl' missing",
            ),
            (
                Reply::ok(r#"{"inventory":{"web1":{"colour":"red"}}}"#),
                "can't read the hosts of github:o/r: inventory host \"web1\": unknown field `colour`",
            ),
            (
                Reply::not_found(),
                "nix.command = \"nix\" could not be run: No such file or directory (os error 2); is Nix installed and on PATH?",
            ),
        ] {
            let fake = world();
            fake.on(&format!("hosts {locked}"), reply.clone());
            let engine = engine(&config(), Cache::in_memory(), &fake);
            let error = engine.discover(GITHUB).await.unwrap_err();
            assert!(
                error.summary.starts_with(want),
                "{reply:?}: {}",
                error.summary
            );
        }
        let fake = world();
        fake.set(&format!("metadata {GITHUB}"), Reply::exit(1, FETCH_FAILED));
        let engine = engine(&config(), Cache::in_memory(), &fake);
        let error = engine.discover(GITHUB).await.unwrap_err();
        assert!(
            error.summary.starts_with("unable to download"),
            "{}",
            error.summary
        );
    }

    #[tokio::test(start_paused = true)]
    async fn offline_discovery_reads_the_cache_only() {
        let fake = world();
        let locked = github(GITHUB, REV1).locked;
        let discovery = format!("hosts {locked}");
        fake.on(&discovery, Reply::ok(HOSTS_JSON));
        let online = engine(&config(), Cache::in_memory(), &fake);
        let (_, hosts) = online.discover(GITHUB).await.unwrap();
        let offline = online.with_freshness(Freshness::Offline);
        assert_eq!(offline.discover(GITHUB).await.unwrap().1, hosts);
        assert_eq!(fake.calls_to(&discovery).len(), 1);

        // Another inventory attribute was never read.
        let mut config = config();
        config.inventory = "fleet".into();
        let cache = cache_of(&online);
        let other = Engine::assemble(&config, cache, Freshness::Offline, fake.runner(), None);
        let error = other.discover(GITHUB).await.unwrap_err();
        assert_eq!(
            error.summary,
            "can't read the hosts of github:o/r: not cached; run hostctl once without --offline"
        );
        assert_eq!(fake.calls_to(&discovery).len(), 1, "no evaluation");
    }

    // ─── plumbing ───────────────────────────────────────────────────────

    #[test]
    fn engines_and_their_futures_are_send() {
        fn send<T: Send>(_: &T) {}
        fn shared<T: Send + Sync>() {}
        shared::<Engine>();
        let engine = engine(&config(), Cache::in_memory(), &Fake::new());
        let (tx, _rx) = mpsc::unbounded_channel();
        let (host, path) = (remote("a"), sp('a', "a"));
        send(&engine.check(&[], &[], &tx));
        send(&engine.probe(0, &host, &tx));
        send(&engine.diff(0, &host, &path, &path, &tx));
        send(&engine.prepare_interactive(&host, "x"));
        send(&engine.discard_upload(&host, "/tmp/hostctl.x"));
        send(&engine.resolve("x"));
        send(&engine.discover("x"));
    }

    #[tokio::test(start_paused = true)]
    async fn an_empty_check_still_starts_and_finishes() {
        let fake = Fake::new();
        let engine = engine(&config(), Cache::in_memory(), &fake);
        assert_eq!(lines(&check(&engine, &[], &[]).await), ["0 run+", "0 run-"]);
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn engines_take_their_settings_from_the_config() {
        let mut config = config();
        config.jobs = Some(3);
        let engine = Engine::with_runner(
            &config,
            Cache::in_memory(),
            Freshness::Offline,
            Fake::new().runner(),
        );
        assert_eq!(engine.inner.evals.available_permits(), 3);
        assert_eq!(
            engine.inner.connections.available_permits(),
            MAX_NEW_CONNECTIONS
        );
        assert_eq!(engine.inner.control_dir, None, "persist = 0");
        assert_eq!(engine.freshness, Freshness::Offline);
        assert_eq!(engine.config(), &config);
        config.ssh.persist = HumanDuration::from_secs(600);
        let engine = Engine::new(&config, Cache::in_memory(), Freshness::Auto);
        assert_eq!(engine.inner.control_dir, remote::control_dir());
        config.jobs = None;
        let engine = Engine::with_runner(
            &config,
            Cache::in_memory(),
            Freshness::Auto,
            Fake::new().runner(),
        );
        assert_eq!(engine.inner.evals.available_permits(), config.jobs());
    }

    #[test]
    fn freshness_is_judged_in_whole_seconds() {
        let minute = Some(Duration::from_secs(60));
        assert!(is_fresh(100, 100, minute));
        assert!(is_fresh(100, 159, minute));
        assert!(!is_fresh(100, 160, minute));
        assert!(!is_fresh(100, 99, minute), "from the future");
        assert!(!is_fresh(100, 100, None));
        assert!(!is_fresh(i64::MIN, i64::MAX, minute));
    }

    #[test]
    fn ssh_stderr_is_cleaned_of_noise() {
        let stderr = "Warning: Permanently added 'web1' (ED25519) to the list of known hosts.\r\n\
                      ** WARNING: connection is not using a post-quantum key exchange algorithm.\n\
                      ** This session may be vulnerable to \"store now, decrypt later\" attacks.\n\
                      \n\
                      jj@web1: Permission denied (publickey).\r\n";
        assert_eq!(
            clean_ssh_stderr(stderr),
            "jj@web1: Permission denied (publickey)."
        );
        assert_eq!(clean_ssh_stderr(""), "");
        assert_eq!(last_line("a\n b \n\n"), Some("b"));
        assert_eq!(last_line(" \n"), None);
    }

    #[test]
    fn a_proxy_closing_says_nothing() {
        let closed = "Connection closed by UNKNOWN port 65535";
        assert_eq!(
            reason_line(&format!("proxy: no route\n{closed}\n")),
            Some("proxy: no route")
        );
        assert_eq!(reason_line(&format!("{closed}\n")), Some(closed));
        assert_eq!(reason_line("a\n b \n\n"), Some("b"));
        assert_eq!(reason_line(" \n"), None);
        let banner = "** WARNING: connection is not using a post-quantum key exchange algorithm.\n";
        for nothing in ["", "\r\n", banner, &format!("{banner}{closed}\r\n")] {
            assert!(says_nothing(nothing), "{nothing:?}");
        }
        let kex = "kex_exchange_identification: Connection closed by remote host";
        for something in [&format!("{kex}\r\n{closed}\r\n"), "proxy: no route\n"] {
            assert!(!says_nothing(something), "{something:?}");
        }
    }

    #[test]
    fn details_keep_the_end_of_long_stderr() {
        let spec = CommandSpec::new("nix").arg("eval");
        assert_eq!(detail(&spec, "", None), "$ nix eval");
        assert_eq!(
            detail(&spec, " oops \n", Some("try this")),
            "$ nix eval\noops\nhint: try this"
        );
        let long = format!("{}é{}", "x".repeat(MAX_DETAIL_STDERR), "end");
        let kept = detail(&spec, &long, None);
        assert!(
            kept.starts_with("$ nix eval\n…") && kept.ends_with("éend"),
            "{}",
            &kept[..40]
        );
        assert!(kept.len() < MAX_DETAIL_STDERR + 32);
    }

    #[test]
    fn retry_delays_are_jittered_within_bounds() {
        let delays: Vec<Duration> = (0..200).map(|_| retry_delay()).collect();
        assert!(
            delays.iter().all(|d| (100..=500).contains(&d.as_millis())),
            "{delays:?}"
        );
        let mut distinct = delays.clone();
        distinct.sort();
        distinct.dedup();
        assert!(distinct.len() > 50, "{distinct:?}");
    }

    #[test]
    fn hosts_are_grouped_by_flake_then_attribute() {
        let hosts = ids(vec![
            remote("a"),
            on_flake(OTHER, remote("b")),
            Host {
                attr: attr("a"),
                ..remote("c")
            },
            on_flake(OTHER, remote("d")),
        ]);
        let known = [github(OTHER, REV1)];
        let flakes = group_by_flake(&hosts, &known);
        let a = attr("a");
        type Shape<'a> = (&'a str, bool, Vec<(&'a str, Vec<HostId>)>);
        let shape: Vec<Shape> = flakes
            .iter()
            .map(|f| (f.url, f.known.is_some(), f.attrs.clone()))
            .collect();
        let (b, d) = (attr("b"), attr("d"));
        assert_eq!(
            shape,
            [
                (GITHUB, false, vec![(a.as_str(), vec![0, 2])]),
                (
                    OTHER,
                    true,
                    vec![(b.as_str(), vec![1]), (d.as_str(), vec![3])]
                ),
            ]
        );
    }
}
