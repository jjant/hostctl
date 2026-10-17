//! The state every front-end renders — hosts and the outcomes of their
//! probes, evaluations and diffs — and the [`Event`]s that update it.
//! [`AppState::apply`] is the only mutator, so it is the whole contract
//! between the engine and the front-ends.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::cache::DeployedRev;
use crate::closure::ClosureDiff;
use crate::config::Host;
use crate::nix::FlakeInfo;
use crate::probe::ProbeReport;
use crate::storepath::StorePath;

/// Index of a host in [`AppState::hosts`].
pub type HostId = usize;

/// A failure: a one-line `summary` for tables, and the full `detail`
/// (cleaned stderr, the command that failed, hints) for the details pane.
/// Constructors sanitize both for the terminal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ErrorInfo {
    pub summary: String,
    pub detail: String,
}

impl ErrorInfo {
    pub fn new(summary: impl AsRef<str>, detail: impl AsRef<str>) -> Self {
        let clean = |s: &str| crate::util::sanitize_terminal(s.trim()).into_owned();
        ErrorInfo {
            summary: clean(summary.as_ref()),
            detail: clean(detail.as_ref()),
        }
    }

    /// An error whose summary is all there is to say.
    pub fn msg(message: impl AsRef<str>) -> Self {
        ErrorInfo::new(message.as_ref(), message.as_ref())
    }
}

/// Why a probe failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProbeFailure {
    /// Couldn't get there: spawn error, ssh exit 255 or a timeout, with no
    /// answer from the host.
    Unreachable,
    /// Got there, but the answer was unusable (incomplete, invalid, non-zero
    /// exit).
    Failed,
    /// Cancelled by Ctrl-C.
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProbeError {
    pub kind: ProbeFailure,
    pub error: ErrorInfo,
}

pub type ProbeResult = Result<ProbeReport, ProbeError>;

/// The out path a host's configuration evaluates to at a locked revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Desired {
    pub path: StorePath,
    /// The flake revision it was evaluated at, when known.
    pub rev: Option<String>,
    /// Came from the cache (no evaluation ran).
    pub cached: bool,
}

pub type EvalResult = Result<Desired, ErrorInfo>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum DiffOutcome {
    Diff {
        diff: ClosureDiff,
    },
    /// The desired generation is neither on the host nor in the local store;
    /// it has to be built/fetched on the host first.
    DesiredMissing,
}

pub type DiffResult = Result<DiffOutcome, ErrorInfo>;

/// The latest outcome of a unit of work. The previous value is kept while it
/// re-runs, so a refresh doesn't blank the screen.
#[derive(Debug, Clone)]
pub struct Slot<T> {
    last: Option<(T, Option<Duration>)>,
    running_since: Option<Instant>,
    /// When `last` was stored.
    finished_at: Option<Instant>,
}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Slot {
            last: None,
            running_since: None,
            finished_at: None,
        }
    }
}

impl<T> Slot<T> {
    pub fn start(&mut self, now: Instant) {
        self.running_since = Some(now);
    }

    /// Store a result; its duration is measured from the matching
    /// [`start`](Self::start) (`None` if there was none, e.g. cache hits).
    pub fn finish(&mut self, value: T, now: Instant) {
        let took = self.running_since.take().map(|s| now.duration_since(s));
        self.last = Some((value, took));
        self.finished_at = Some(now);
    }

    /// Forget the value; a run in flight stays in flight.
    pub fn clear_value(&mut self) {
        self.last = None;
        self.finished_at = None;
    }

    /// The value was stored at or after `since`, and no newer run is in
    /// flight.
    pub fn finished_since(&self, since: Instant) -> bool {
        !self.is_running() && self.finished_at.is_some_and(|at| at >= since)
    }

    pub fn stop(&mut self) {
        self.running_since = None;
    }

    pub fn get(&self) -> Option<&T> {
        self.last.as_ref().map(|(v, _)| v)
    }

    /// How long the run that produced the value took.
    pub fn took(&self) -> Option<Duration> {
        self.last.as_ref().and_then(|(_, d)| *d)
    }

    pub fn is_running(&self) -> bool {
        self.running_since.is_some()
    }

    /// For how long the current run has been going.
    pub fn running_for(&self, now: Instant) -> Option<Duration> {
        self.running_since.map(|s| now.duration_since(s))
    }
}

/// A host's status. Declared from most to least urgent, which is the order
/// "sort by status" uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    EvalFailed,
    ProbeFailed,
    Unreachable,
    Outdated,
    NotActivated,
    Pending,
    Offline,
    UpToDate,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::EvalFailed => "eval failed",
            Status::ProbeFailed => "probe failed",
            Status::Unreachable => "unreachable",
            Status::Outdated => "outdated",
            Status::NotActivated => "not activated",
            Status::Pending => "pending",
            Status::Offline => "offline",
            Status::UpToDate => "up to date",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Status::EvalFailed | Status::ProbeFailed => "!",
            Status::Unreachable => "?",
            Status::Outdated => "✗",
            Status::NotActivated => "○",
            Status::Pending => "…",
            Status::Offline => "–",
            Status::UpToDate => "✓",
        }
    }

    /// The host couldn't be checked.
    pub fn is_trouble(self) -> bool {
        matches!(
            self,
            Status::EvalFailed | Status::ProbeFailed | Status::Unreachable
        )
    }

    /// Activating would change something.
    pub fn needs_activation(self) -> bool {
        matches!(self, Status::Outdated | Status::NotActivated)
    }
}

/// Why an outdated host differs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum Drift {
    /// The profile already points at the desired generation; a switch or
    /// reboot is pending.
    Staged,
    /// Built from the compared revision but a different store path: activated
    /// from a flake reference without `rev`, or with overridden inputs.
    SameRevision,
    /// Running a generation of another revision.
    Revision {
        rev: String,
        last_modified: Option<i64>,
        /// The generation records it (`configurationRevision`), as opposed to
        /// hostctl's cache.
        from_generation: bool,
    },
    Unknown,
}

/// One host and what is known about it.
#[derive(Debug, Clone)]
pub struct HostState {
    pub host: Host,
    pub probe: Slot<ProbeResult>,
    pub eval: Slot<EvalResult>,
    /// On demand (`d`).
    pub diff: Slot<DiffResult>,
    /// The `(current, desired)` the running/last diff was computed for.
    pub diff_inputs: Option<(StorePath, StorePath)>,
    /// Which cached revision the current generation belongs to, if any.
    pub deployed: Option<DeployedRev>,
}

impl HostState {
    pub fn new(host: Host) -> Self {
        HostState {
            host,
            probe: Slot::default(),
            eval: Slot::default(),
            diff: Slot::default(),
            diff_inputs: None,
            deployed: None,
        }
    }

    pub fn report(&self) -> Option<&ProbeReport> {
        self.probe.get()?.as_ref().ok()
    }

    /// The deployed generation to compare ([`ProbeReport::deployed`]).
    pub fn current(&self) -> Option<&StorePath> {
        self.report()?.deployed()
    }

    pub fn desired(&self) -> Option<&Desired> {
        self.eval.get()?.as_ref().ok()
    }

    /// Derived from the latest values (stale ones count while a refresh
    /// runs). An eval failure wins over probe failures, which win over the
    /// comparison.
    pub fn status(&self) -> Status {
        if let Some(Err(_)) = self.eval.get() {
            return Status::EvalFailed;
        }
        match self.probe.get() {
            Some(Err(e)) => match e.kind {
                ProbeFailure::Failed => Status::ProbeFailed,
                ProbeFailure::Unreachable if self.host.optional => Status::Offline,
                ProbeFailure::Unreachable => Status::Unreachable,
                ProbeFailure::Interrupted => Status::Pending,
            },
            Some(Ok(report)) => match (report.deployed(), self.desired()) {
                (None, _) => Status::NotActivated,
                (Some(current), Some(desired)) if *current == desired.path => Status::UpToDate,
                (Some(_), Some(_)) => Status::Outdated,
                (Some(_), None) => Status::Pending,
            },
            None => Status::Pending,
        }
    }

    /// Why an outdated host differs; `None` unless [`Status::Outdated`].
    pub fn drift(&self) -> Option<Drift> {
        if self.status() != Status::Outdated {
            return None;
        }
        let report = self.report()?;
        let desired = self.desired()?;
        if report.profile.as_ref() == Some(&desired.path) {
            return Some(Drift::Staged);
        }
        let (rev, last_modified, from_generation) = match (&report.revision, &self.deployed) {
            (Some(rev), _) => (rev.clone(), None, true),
            (
                None,
                Some(DeployedRev {
                    rev: Some(rev),
                    last_modified,
                    ..
                }),
            ) => (rev.clone(), *last_modified, false),
            _ => return Some(Drift::Unknown),
        };
        if desired.rev.as_deref() == Some(rev.as_str()) {
            Some(Drift::SameRevision)
        } else {
            Some(Drift::Revision {
                rev,
                last_modified,
                from_generation,
            })
        }
    }

    /// Something is in flight for this host.
    pub fn busy(&self) -> bool {
        self.probe.is_running() || self.eval.is_running() || self.diff.is_running()
    }

    /// What a pending host is waiting for, for its status cell.
    pub fn activity(&self) -> Option<&'static str> {
        if self.diff.is_running() {
            Some("diffing")
        } else if self.eval.is_running() {
            Some("evaluating")
        } else if self.probe.is_running() {
            Some("probing")
        } else if self.status() == Status::Pending {
            Some("waiting")
        } else {
            None
        }
    }
}

#[derive(Debug, Clone)]
pub struct FlakeState {
    /// As configured.
    pub url: String,
    pub info: Slot<Result<FlakeInfo, ErrorInfo>>,
    /// Hosts compared against this flake.
    pub hosts: usize,
}

/// Everything the engine reports.
#[derive(Debug, Clone)]
pub enum Event {
    /// A check started.
    RunStarted,
    FlakeStarted {
        url: String,
    },
    FlakeFinished {
        url: String,
        result: Result<FlakeInfo, ErrorInfo>,
    },
    ProbeStarted(HostId),
    ProbeFinished {
        id: HostId,
        result: ProbeResult,
        /// Reverse cache lookup of the probed generation.
        deployed: Option<DeployedRev>,
    },
    EvalStarted(HostId),
    EvalFinished {
        id: HostId,
        result: EvalResult,
    },
    DiffStarted {
        id: HostId,
        current: StorePath,
        desired: StorePath,
    },
    DiffFinished {
        id: HostId,
        current: StorePath,
        desired: StorePath,
        result: DiffResult,
    },
    /// A problem not tied to one host.
    Error(ErrorInfo),
    /// `nix` or `ssh` can't be run at all: the check couldn't run (exit 3),
    /// whatever the hosts say. Shown like an [`Event::Error`].
    Unrunnable(ErrorInfo),
    /// Ctrl-C: work in flight is reported as interrupted.
    Interrupted,
    /// The check started by the last `RunStarted` is complete.
    RunFinished,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub total: usize,
    pub up_to_date: usize,
    /// Outdated or not activated.
    pub outdated: usize,
    pub trouble: usize,
    pub pending: usize,
    pub offline: usize,
}

#[derive(Debug, Clone)]
pub struct AppState {
    pub hosts: Vec<HostState>,
    pub flakes: Vec<FlakeState>,
    /// The current/last check.
    pub run: Slot<()>,
    pub config_path: Option<PathBuf>,
    /// How to run hostctl again in this context, shell-quoted: what the
    /// commands hostctl suggests or copies start with (`hostctl`, plus the
    /// `-c`/`-f`/`--self` this run was given).
    pub command: String,
    /// Problems not tied to a host, newest last.
    pub errors: Vec<ErrorInfo>,
    /// `nix` or `ssh` couldn't be run ([`Event::Unrunnable`], whose error
    /// is in `errors` too).
    pub unrunnable: bool,
    pub interrupted: bool,
}

impl Default for AppState {
    fn default() -> Self {
        AppState {
            hosts: Vec::new(),
            flakes: Vec::new(),
            run: Slot::default(),
            config_path: None,
            command: "hostctl".into(),
            errors: Vec::new(),
            unrunnable: false,
            interrupted: false,
        }
    }
}

impl AppState {
    pub fn new(hosts: Vec<Host>) -> Self {
        let mut state = AppState::default();
        for host in hosts {
            match state.flakes.iter_mut().find(|f| f.url == host.flake) {
                Some(flake) => flake.hosts += 1,
                None => state.flakes.push(FlakeState {
                    url: host.flake.clone(),
                    info: Slot::default(),
                    hosts: 1,
                }),
            }
            state.hosts.push(HostState::new(host));
        }
        state
    }

    /// Seed already-resolved flakes (e.g. from discovery), as if resolved.
    pub fn with_flakes(mut self, known: &[FlakeInfo], now: Instant) -> Self {
        for info in known {
            if let Some(f) = self.flakes.iter_mut().find(|f| f.url == info.flake) {
                f.info.finish(Ok(info.clone()), now);
            }
        }
        self
    }

    /// Fold one event into the state. Events for unknown hosts or flakes
    /// are ignored. A result that changes a host's current or desired path
    /// clears its diff; a diff result for other paths than the host's
    /// current ones is dropped.
    pub fn apply(&mut self, event: Event, now: Instant) {
        match event {
            Event::RunStarted => self.run.start(now),
            Event::FlakeStarted { url } => {
                if let Some(f) = self.flakes.iter_mut().find(|f| f.url == url) {
                    f.info.start(now);
                }
            }
            Event::FlakeFinished { url, result } => {
                if let Some(f) = self.flakes.iter_mut().find(|f| f.url == url) {
                    f.info.finish(result, now);
                }
            }
            Event::ProbeStarted(id) => {
                if let Some(h) = self.hosts.get_mut(id) {
                    h.probe.start(now);
                }
            }
            Event::ProbeFinished {
                id,
                result,
                deployed,
            } => {
                if let Some(h) = self.hosts.get_mut(id) {
                    let old = h.current().cloned();
                    h.probe.finish(result, now);
                    if h.current() != old.as_ref() {
                        h.diff.clear_value();
                    }
                    h.deployed = deployed;
                }
            }
            Event::EvalStarted(id) => {
                if let Some(h) = self.hosts.get_mut(id) {
                    h.eval.start(now);
                }
            }
            Event::EvalFinished { id, result } => {
                if let Some(h) = self.hosts.get_mut(id) {
                    let old = h.desired().map(|d| d.path.clone());
                    h.eval.finish(result, now);
                    if h.desired().map(|d| &d.path) != old.as_ref() {
                        h.diff.clear_value();
                    }
                }
            }
            Event::DiffStarted {
                id,
                current,
                desired,
            } => {
                if let Some(h) = self.hosts.get_mut(id) {
                    h.diff.start(now);
                    h.diff_inputs = Some((current, desired));
                }
            }
            Event::DiffFinished {
                id,
                current,
                desired,
                result,
            } => {
                if let Some(h) = self.hosts.get_mut(id) {
                    let inputs = Some((current, desired));
                    if h.diff_inputs != inputs {
                        return; // superseded by a newer diff
                    }
                    let fresh = inputs.as_ref().is_some_and(|(c, d)| {
                        h.current() == Some(c) && h.desired().map(|x| &x.path) == Some(d)
                    });
                    if fresh {
                        h.diff.finish(result, now);
                    } else {
                        h.diff.stop();
                        h.diff.clear_value();
                    }
                }
            }
            Event::Error(error) => self.errors.push(error),
            Event::Unrunnable(error) => {
                self.unrunnable = true;
                self.errors.push(error);
            }
            Event::Interrupted => {
                self.interrupted = true;
                let interrupted = ErrorInfo::msg("interrupted");
                for h in &mut self.hosts {
                    if h.probe.is_running() {
                        h.probe.finish(
                            Err(ProbeError {
                                kind: ProbeFailure::Interrupted,
                                error: interrupted.clone(),
                            }),
                            now,
                        );
                    }
                    // An interrupted eval keeps its previous value (if any) and
                    // otherwise reads as pending, not as a broken config.
                    h.eval.stop();
                    h.diff.stop();
                }
                for f in &mut self.flakes {
                    f.info.stop();
                }
                self.run.stop();
            }
            Event::RunFinished => self.run.finish((), now),
        }
    }

    pub fn summary(&self) -> Summary {
        let mut s = Summary {
            total: self.hosts.len(),
            ..Summary::default()
        };
        for h in &self.hosts {
            match h.status() {
                Status::UpToDate => s.up_to_date += 1,
                Status::Outdated | Status::NotActivated => s.outdated += 1,
                Status::Pending => s.pending += 1,
                Status::Offline => s.offline += 1,
                Status::EvalFailed | Status::ProbeFailed | Status::Unreachable => s.trouble += 1,
            }
        }
        s
    }

    /// DESIGN.md exit codes: `130` interrupted; `3` ("couldn't run") if
    /// `nix` or `ssh` couldn't be run at all (the CLI decides the other 3s
    /// before there is a state); `2` if any host is in trouble or pending,
    /// a global error occurred or a flake is stale; else `1` if any host
    /// needs activation; else `0`.
    pub fn exit_code(&self) -> u8 {
        let s = self.summary();
        let stale = self
            .flakes
            .iter()
            .any(|f| matches!(f.info.get(), Some(Ok(info)) if info.stale.is_some()));
        if self.interrupted {
            130
        } else if self.unrunnable {
            3
        } else if s.trouble > 0 || s.pending > 0 || !self.errors.is_empty() || stale {
            2
        } else if s.outdated > 0 {
            1
        } else {
            0
        }
    }

    pub fn flake(&self, url: &str) -> Option<&FlakeState> {
        self.flakes.iter().find(|f| f.url == url)
    }

    /// The resolved flake a host is compared against, if resolution succeeded.
    pub fn flake_info(&self, url: &str) -> Option<&FlakeInfo> {
        self.flake(url)?.info.get()?.as_ref().ok()
    }

    /// The flake most hosts use (first on ties), for headers.
    pub fn primary_flake(&self) -> Option<&FlakeState> {
        self.flakes
            .iter()
            .enumerate()
            .max_by_key(|(i, f)| (f.hosts, std::cmp::Reverse(*i)))
            .map(|(_, f)| f)
    }

    /// Any work in flight (a check, or a per-host action such as a diff).
    pub fn is_busy(&self) -> bool {
        self.run.is_running() || self.hosts.iter().any(HostState::busy)
    }

    /// While a check runs: how many hosts it is done with (probed and
    /// evaluated since it started), and how many there are. Hosts keep
    /// their previous results meanwhile, so this is what says a refresh
    /// is still going.
    pub fn check_progress(&self) -> Option<(usize, usize)> {
        let started = self.run.running_since?;
        let done = self
            .hosts
            .iter()
            .filter(|h| h.probe.finished_since(started) && h.eval.finished_since(started))
            .count();
        Some((done, self.hosts.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ActivateOn, Kind, Sources, SshTarget, Target};

    fn host(name: &str, flake: &str) -> Host {
        Host {
            name: name.into(),
            kind: Kind::HomeManager,
            flake: flake.into(),
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
            activate: None,
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

    fn report(c: char) -> ProbeReport {
        ProbeReport {
            current: Some(path(c)),
            ..ProbeReport::default()
        }
    }

    fn desired(c: char) -> Desired {
        Desired {
            path: path(c),
            rev: Some("rev-new".into()),
            cached: false,
        }
    }

    fn probed(state: &mut AppState, id: HostId, result: ProbeResult) {
        let now = Instant::now();
        state.apply(Event::ProbeStarted(id), now);
        state.apply(
            Event::ProbeFinished {
                id,
                result,
                deployed: None,
            },
            now + Duration::from_millis(5),
        );
    }

    fn evaluated(state: &mut AppState, id: HostId, result: EvalResult) {
        state.apply(Event::EvalFinished { id, result }, Instant::now());
    }

    fn failure(kind: ProbeFailure) -> ProbeResult {
        Err(ProbeError {
            kind,
            error: ErrorInfo::msg("ssh: connect to host x port 22: Connection refused"),
        })
    }

    #[test]
    fn status_follows_probe_and_eval() {
        let mut s = AppState::new(vec![host("a", "f")]);
        assert_eq!(s.hosts[0].status(), Status::Pending);
        probed(&mut s, 0, Ok(report('a')));
        assert_eq!(s.hosts[0].status(), Status::Pending);
        assert_eq!(s.hosts[0].probe.took(), Some(Duration::from_millis(5)));
        evaluated(&mut s, 0, Ok(desired('a')));
        assert_eq!(s.hosts[0].status(), Status::UpToDate);
        evaluated(&mut s, 0, Ok(desired('b')));
        assert_eq!(s.hosts[0].status(), Status::Outdated);
        probed(&mut s, 0, failure(ProbeFailure::Unreachable));
        assert_eq!(s.hosts[0].status(), Status::Unreachable);
        probed(&mut s, 0, failure(ProbeFailure::Failed));
        assert_eq!(s.hosts[0].status(), Status::ProbeFailed);
        probed(
            &mut s,
            0,
            Ok(ProbeReport {
                none_reason: Some("no home-manager profile".into()),
                ..ProbeReport::default()
            }),
        );
        assert_eq!(s.hosts[0].status(), Status::NotActivated);
        evaluated(&mut s, 0, Err(ErrorInfo::msg("attribute missing")));
        assert_eq!(s.hosts[0].status(), Status::EvalFailed);
    }

    #[test]
    fn optional_hosts_go_offline_instead_of_unreachable() {
        let mut h = host("mac", "f");
        h.optional = true;
        let mut s = AppState::new(vec![h]);
        probed(&mut s, 0, failure(ProbeFailure::Unreachable));
        evaluated(&mut s, 0, Ok(desired('a')));
        assert_eq!(s.hosts[0].status(), Status::Offline);
        assert_eq!(s.exit_code(), 0);
    }

    #[test]
    fn specialisations_compare_their_toplevel() {
        let mut s = AppState::new(vec![host("a", "f")]);
        probed(
            &mut s,
            0,
            Ok(ProbeReport {
                current: Some(path('s')),
                toplevel: Some(path('a')),
                specialisation: Some("gaming".into()),
                ..ProbeReport::default()
            }),
        );
        evaluated(&mut s, 0, Ok(desired('a')));
        assert_eq!(s.hosts[0].status(), Status::UpToDate);
    }

    #[test]
    fn drift_explains_outdated_hosts() {
        let mut s = AppState::new(vec![host("a", "f")]);
        evaluated(&mut s, 0, Ok(desired('b')));
        probed(&mut s, 0, Ok(report('a')));
        assert_eq!(s.hosts[0].drift(), Some(Drift::Unknown));
        probed(
            &mut s,
            0,
            Ok(ProbeReport {
                profile: Some(path('b')),
                ..report('a')
            }),
        );
        assert_eq!(s.hosts[0].drift(), Some(Drift::Staged));
        probed(
            &mut s,
            0,
            Ok(ProbeReport {
                revision: Some("rev-new".into()),
                ..report('a')
            }),
        );
        assert_eq!(s.hosts[0].drift(), Some(Drift::SameRevision));
        probed(
            &mut s,
            0,
            Ok(ProbeReport {
                revision: Some("rev-old".into()),
                ..report('a')
            }),
        );
        assert_eq!(
            s.hosts[0].drift(),
            Some(Drift::Revision {
                rev: "rev-old".into(),
                last_modified: None,
                from_generation: true
            })
        );
        s.apply(
            Event::ProbeFinished {
                id: 0,
                result: Ok(report('a')),
                deployed: Some(DeployedRev {
                    flake: "f".into(),
                    rev: Some("rev-cached".into()),
                    last_modified: Some(7),
                }),
            },
            Instant::now(),
        );
        assert_eq!(
            s.hosts[0].drift(),
            Some(Drift::Revision {
                rev: "rev-cached".into(),
                last_modified: Some(7),
                from_generation: false
            })
        );
        evaluated(&mut s, 0, Ok(desired('a')));
        assert_eq!(s.hosts[0].drift(), None);
    }

    #[test]
    fn keeps_stale_values_while_refreshing() {
        let mut s = AppState::new(vec![host("a", "f")]);
        probed(&mut s, 0, Ok(report('a')));
        evaluated(&mut s, 0, Ok(desired('a')));
        s.apply(Event::ProbeStarted(0), Instant::now());
        s.apply(Event::EvalStarted(0), Instant::now());
        assert_eq!(s.hosts[0].status(), Status::UpToDate);
        assert!(s.hosts[0].busy());
        assert_eq!(s.hosts[0].activity(), Some("evaluating"));
    }

    fn diff_started(s: &mut AppState, c: char, d: char) {
        s.apply(
            Event::DiffStarted {
                id: 0,
                current: path(c),
                desired: path(d),
            },
            Instant::now(),
        );
    }

    fn diff_finished(s: &mut AppState, c: char, d: char) {
        s.apply(
            Event::DiffFinished {
                id: 0,
                current: path(c),
                desired: path(d),
                result: Ok(DiffOutcome::DesiredMissing),
            },
            Instant::now(),
        );
    }

    #[test]
    fn diffs_carry_their_inputs() {
        let mut s = AppState::new(vec![host("a", "f")]);
        probed(&mut s, 0, Ok(report('a')));
        evaluated(&mut s, 0, Ok(desired('b')));
        diff_started(&mut s, 'a', 'b');
        diff_finished(&mut s, 'a', 'b');
        assert!(s.hosts[0].diff.get().is_some());
        probed(&mut s, 0, Ok(report('a')));
        assert!(
            s.hosts[0].diff.get().is_some(),
            "same current keeps the diff"
        );
        probed(&mut s, 0, Ok(report('c')));
        assert!(s.hosts[0].diff.get().is_none());

        // A diff that finishes after the desired path changed is dropped,
        // and stops spinning.
        diff_started(&mut s, 'c', 'b');
        evaluated(&mut s, 0, Ok(desired('d')));
        assert!(
            s.hosts[0].diff.is_running(),
            "clearing keeps the run in flight"
        );
        diff_finished(&mut s, 'c', 'b');
        assert!(s.hosts[0].diff.get().is_none());
        assert!(!s.hosts[0].diff.is_running());

        // A diff superseded by a newer one is ignored entirely.
        diff_started(&mut s, 'c', 'd');
        diff_started(&mut s, 'c', 'd');
        diff_finished(&mut s, 'x', 'y');
        assert!(s.hosts[0].diff.is_running());
        diff_finished(&mut s, 'c', 'd');
        assert!(s.hosts[0].diff.get().is_some());
    }

    #[test]
    fn summary_and_exit_codes() {
        let mut s = AppState::new(vec![host("a", "f"), host("b", "f"), host("c", "g")]);
        assert_eq!(s.exit_code(), 2, "pending hosts are trouble");
        for id in 0..3 {
            probed(&mut s, id, Ok(report('a')));
            evaluated(&mut s, id, Ok(desired('a')));
        }
        assert_eq!(s.exit_code(), 0);
        evaluated(&mut s, 1, Ok(desired('b')));
        assert_eq!(s.exit_code(), 1);
        probed(&mut s, 2, failure(ProbeFailure::Unreachable));
        assert_eq!(s.exit_code(), 2);
        assert_eq!(
            s.summary(),
            Summary {
                total: 3,
                up_to_date: 1,
                outdated: 1,
                trouble: 1,
                pending: 0,
                offline: 0,
            }
        );
        let mut ok = AppState::new(vec![]);
        ok.apply(
            Event::Error(ErrorInfo::msg("nix not found")),
            Instant::now(),
        );
        assert_eq!(ok.exit_code(), 2);
    }

    #[test]
    fn a_check_that_could_not_run_exits_3() {
        let mut s = AppState::new(vec![host("a", "f"), host("b", "f")]);
        probed(&mut s, 0, Ok(report('a')));
        evaluated(&mut s, 0, Ok(desired('a')));
        probed(&mut s, 1, failure(ProbeFailure::Unreachable));
        let error = ErrorInfo::msg("ssh.command = \"ssh\" could not be run");
        s.apply(Event::Unrunnable(error.clone()), Instant::now());
        assert!(s.unrunnable);
        assert_eq!(s.errors, [error], "shown like any global error");
        assert_eq!(s.exit_code(), 3, "over trouble");
        // Whatever the hosts say.
        let mut s = AppState::new(vec![host("a", "f")]);
        probed(&mut s, 0, Ok(report('a')));
        evaluated(&mut s, 0, Ok(desired('a')));
        s.apply(Event::Unrunnable(ErrorInfo::msg("x")), Instant::now());
        assert_eq!(s.exit_code(), 3);
        // An interruption still wins.
        s.apply(Event::Interrupted, Instant::now());
        assert_eq!(s.exit_code(), 130);
    }

    #[test]
    fn stale_flakes_are_trouble() {
        let mut s = AppState::new(vec![host("a", "f")]);
        probed(&mut s, 0, Ok(report('a')));
        evaluated(&mut s, 0, Ok(desired('a')));
        let info = FlakeInfo {
            flake: "f".into(),
            locked: "f/x".into(),
            kind: None,
            locked_url: None,
            rev: None,
            dirty_rev: None,
            last_modified: None,
            rev_count: None,
            fingerprint: None,
            nar_hash: None,
            description: None,
            stale: Some("rate limited".into()),
            lock_outdated: false,
        };
        s.apply(
            Event::FlakeFinished {
                url: "f".into(),
                result: Ok(info),
            },
            Instant::now(),
        );
        assert_eq!(s.exit_code(), 2);
    }

    #[test]
    fn interruption_marks_work_in_flight() {
        let mut s = AppState::new(vec![host("a", "f"), host("b", "f")]);
        s.apply(Event::RunStarted, Instant::now());
        s.apply(Event::ProbeStarted(0), Instant::now());
        s.apply(Event::EvalStarted(1), Instant::now());
        s.apply(Event::FlakeStarted { url: "f".into() }, Instant::now());
        s.apply(Event::Interrupted, Instant::now());
        assert!(!s.flakes[0].info.is_running());
        assert!(!s.is_busy());
        assert!(matches!(
            s.hosts[0].probe.get(),
            Some(Err(ProbeError {
                kind: ProbeFailure::Interrupted,
                ..
            }))
        ));
        assert!(
            s.hosts[1].eval.get().is_none(),
            "interrupted evals aren't failures"
        );
        assert_eq!(s.hosts[1].status(), Status::Pending);
        assert_eq!(s.exit_code(), 130);
    }

    #[test]
    fn tracks_flakes_and_runs() {
        let mut s = AppState::new(vec![host("a", "f"), host("b", "g"), host("c", "g")]);
        assert_eq!(s.command, "hostctl", "without context, plain hostctl");
        assert_eq!(s.flakes.len(), 2);
        assert_eq!(s.primary_flake().unwrap().url, "g");
        s.apply(Event::RunStarted, Instant::now());
        assert!(s.is_busy());
        s.apply(Event::FlakeStarted { url: "f".into() }, Instant::now());
        assert!(s.flake("f").unwrap().info.is_running());
        s.apply(Event::RunFinished, Instant::now());
        assert!(!s.run.is_running());
        // Unknown ids and flakes are ignored.
        probed(&mut s, 99, Ok(report('a')));
        s.apply(Event::FlakeStarted { url: "nope".into() }, Instant::now());
        assert_eq!(s.flakes.len(), 2);
    }

    #[test]
    fn check_progress_counts_the_hosts_the_running_check_is_done_with() {
        let mut s = AppState::new(vec![host("a", "f"), host("b", "f"), host("c", "f")]);
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        // An earlier check answered for a.
        s.apply(Event::ProbeStarted(0), t0);
        s.apply(
            Event::ProbeFinished {
                id: 0,
                result: Ok(report('a')),
                deployed: None,
            },
            t0,
        );
        s.apply(
            Event::EvalFinished {
                id: 0,
                result: Ok(desired('a')),
            },
            t0,
        );
        assert_eq!(s.check_progress(), None, "no check runs");

        s.apply(Event::RunStarted, at(10));
        for id in 0..3 {
            s.apply(Event::ProbeStarted(id), at(10));
        }
        assert_eq!(s.check_progress(), Some((0, 3)), "old answers don't count");
        for id in 0..3 {
            let result = Ok(report('a'));
            let finished = Event::ProbeFinished {
                id,
                result,
                deployed: None,
            };
            s.apply(finished, at(20));
        }
        // a from the cache, b evaluating, c queued for an evaluation slot.
        s.apply(
            Event::EvalFinished {
                id: 0,
                result: Ok(Desired {
                    cached: true,
                    ..desired('a')
                }),
            },
            at(30),
        );
        s.apply(Event::EvalStarted(1), at(30));
        assert_eq!(s.check_progress(), Some((1, 3)));
        for (id, result) in [
            (1, Ok(desired('b'))),
            (2, Err(ErrorInfo::msg("attribute missing"))),
        ] {
            s.apply(Event::EvalFinished { id, result }, at(35));
        }
        assert_eq!(s.check_progress(), Some((3, 3)));
        s.apply(Event::RunFinished, at(40));
        assert_eq!(s.check_progress(), None);
        // An interrupted check is over too.
        s.apply(Event::RunStarted, at(50));
        s.apply(Event::Interrupted, at(60));
        assert_eq!(s.check_progress(), None);
    }

    #[test]
    fn drift_serializes_camel_case() {
        let json = serde_json::to_value(Drift::Revision {
            rev: "abc".into(),
            last_modified: Some(1),
            from_generation: true,
        })
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({"type": "revision", "rev": "abc", "lastModified": 1, "fromGeneration": true})
        );
    }

    #[test]
    fn error_info_is_sanitized() {
        let e = ErrorInfo::new("  bad\u{1b}[2J  ", "detail\u{7}");
        assert_eq!(e.summary, "bad\\u{1b}[2J");
        assert_eq!(e.detail, "detail\\u{7}");
    }
}
