//! Configuration: the TOML schema, where it is loaded from, CLI overrides,
//! and how configured, discovered and inventory hosts resolve into the
//! [`Host`]s a check runs over. DESIGN.md ("Configuration", "Host
//! inventory") is the user-facing description.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};

use crate::local::{Candidate, LocalInfo, Locality};
use crate::remote::Destination;
use crate::util::HumanDuration;

/// What a host runs. Decides the defaults for what to evaluate, where the
/// active generation lives, and how to activate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    Nixos,
    Darwin,
    HomeManager,
    Custom,
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::Nixos, Kind::Darwin, Kind::HomeManager, Kind::Custom];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Nixos => "nixos",
            Kind::Darwin => "darwin",
            Kind::HomeManager => "home-manager",
            Kind::Custom => "custom",
        }
    }

    /// The flake output holding this kind's configurations.
    pub fn output(self) -> Option<&'static str> {
        match self {
            Kind::Nixos => Some("nixosConfigurations"),
            Kind::Darwin => Some("darwinConfigurations"),
            Kind::HomeManager => Some("homeConfigurations"),
            Kind::Custom => None,
        }
    }

    /// The absolute attribute (evaluated as `#.<attr>`) whose out path is the
    /// desired generation of configuration `name`, e.g.
    /// `homeConfigurations."jj@mac".activationPackage`.
    pub fn default_attr(self, name: &str) -> Option<String> {
        let rest = match self {
            Kind::Nixos => "config.system.build.toplevel",
            Kind::Darwin => "system",
            Kind::HomeManager => "activationPackage",
            Kind::Custom => return None,
        };
        Some(format!(
            "{}.{}.{rest}",
            self.output()?,
            crate::nix::attr_name(name)
        ))
    }

    /// The activation template (see [`crate::actions::expand`]). Every
    /// default uses `{flake}` (the compared locked URL) so that activating
    /// produces exactly the compared generation.
    pub fn default_activate(self) -> Option<&'static str> {
        match self {
            Kind::Nixos => Some(
                "/run/current-system/sw/bin/nixos-rebuild switch --flake {flake}#{name} --sudo",
            ),
            Kind::Darwin => {
                Some("sudo /run/current-system/sw/bin/darwin-rebuild switch --flake {flake}#{name}")
            }
            Kind::HomeManager => Some(
                "out=$(nix --extra-experimental-features 'nix-command flakes' build --no-link \
                 --print-out-paths {flake}#.{attr}) && [ \"$out\" = {out} ] && \"$out/activate\"",
            ),
            Kind::Custom => None,
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `activate = "<template>"`, or `activate = false` / `""` to disable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum ActivateSetting {
    /// Only `false` is valid (`true` is a config error).
    Enabled(bool),
    Command(String),
}

// By hand: a derived `untagged` impl can only say "data did not match any
// variant". `true` is accepted here so that `validate` can name the host.
impl<'de> Deserialize<'de> for ActivateSetting {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl de::Visitor<'_> for Visitor {
            type Value = ActivateSetting;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an activation command, or false to disable activation")
            }

            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(ActivateSetting::Enabled(v))
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(ActivateSetting::Command(v.to_owned()))
            }

            fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(ActivateSetting::Command(v))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

/// Where an activation command runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActivateOn {
    /// On the host itself (over ssh unless it is this machine).
    #[default]
    Target,
    /// On this machine (push tools: `nixos-rebuild --target-host`, deploy-rs, colmena).
    Local,
}

/// The whole config file. Every field has a default; unknown keys are errors.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Config {
    /// Default flake for every host (`--flake` and `HOSTCTL_FLAKE` override it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flake: Option<String>,
    /// Concurrent evaluations; default: [`default_jobs`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jobs: Option<usize>,
    /// Per-probe timeout (> 0).
    pub timeout: HumanDuration,
    /// Per-evaluation timeout; 0 = none.
    pub eval_timeout: HumanDuration,
    /// Force which host is this machine (`--self`, `HOSTCTL_SELF` override it).
    #[serde(rename = "self", skip_serializing_if = "Option::is_none")]
    pub self_host: Option<String>,
    /// Add the default flake's `*Configurations` as hosts.
    pub discover: bool,
    /// Flake attribute holding `{ hosts.<name> = { … }; }` (read when it
    /// exists); `""` disables.
    pub inventory: String,
    /// Globs of host names to drop.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    /// Reuse evaluation results of unchanged flakes.
    pub cache: bool,
    pub nix: NixConfig,
    pub ssh: SshConfig,
    /// Applied to every host; `destination` may use `{name}`.
    #[serde(skip_serializing_if = "HostConfig::is_empty")]
    pub defaults: HostConfig,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub hosts: BTreeMap<String, HostConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            flake: None,
            jobs: None,
            timeout: HumanDuration::from_secs(20),
            eval_timeout: HumanDuration::from_secs(600),
            self_host: None,
            discover: true,
            inventory: "lib.hostctl".into(),
            exclude: Vec::new(),
            cache: true,
            nix: NixConfig::default(),
            ssh: SshConfig::default(),
            defaults: HostConfig::default(),
            hosts: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct NixConfig {
    /// The nix executable.
    pub command: String,
    /// Extra arguments for every local nix invocation.
    pub options: Vec<String>,
    /// Evaluate with `--read-only` first.
    pub read_only: bool,
    /// Re-resolve a flake's branch head when the last resolution is older
    /// than this; 0 = on every check.
    pub refresh: HumanDuration,
}

impl Default for NixConfig {
    fn default() -> Self {
        NixConfig {
            command: "nix".into(),
            options: Vec::new(),
            read_only: true,
            refresh: HumanDuration::from_secs(60),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct SshConfig {
    /// The ssh executable.
    pub command: String,
    /// Extra arguments for every ssh run; they take precedence over
    /// hostctl's own (ssh keeps the first value of an option).
    pub options: Vec<String>,
    /// Lifetime of hostctl's private ControlMaster connections; 0 = don't
    /// multiplex.
    pub persist: HumanDuration,
}

impl Default for SshConfig {
    fn default() -> Self {
        SshConfig {
            command: "ssh".into(),
            options: Vec::new(),
            persist: HumanDuration::from_secs(600),
        }
    }
}

/// One `[hosts.<name>]` table, the `[defaults]` table, or one entry of a
/// flake inventory. `<name>` is the configuration's name in the flake.
/// Unset fields fall through to the next layer (see
/// [`Config::resolve_hosts`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct HostConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<Kind>,
    /// ssh destination: `host`, `user@host`, `ssh://[user@]host[:port]`.
    /// Default: the host's name (if it looks like `[user@]host`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flake: Option<String>,
    /// Absolute attribute whose out path is the desired generation
    /// (overrides the kind's default; required for `custom`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attr: Option<String>,
    /// Profile symlink to read instead of the kind's default location
    /// (absolute, or `~/…`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Forced-command ssh route: send this instead of the probe script.
    /// Ignored when the host is this machine.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activate: Option<ActivateSetting>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activate_on: Option<ActivateOn>,
    /// Extra names identifying this host as the local machine.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostnames: Option<Vec<String>>,
    /// Extra ssh arguments for this host (before the global ones).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_options: Option<Vec<String>>,
    /// Unreachable is expected (shown as `offline`, not trouble).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub optional: Option<bool>,
    /// Free text shown next to the host's status reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl HostConfig {
    pub fn is_empty(&self) -> bool {
        *self == HostConfig::default()
    }

    /// Overlay `other` onto `self`: every field `other` sets replaces ours.
    pub fn merge(&mut self, other: &HostConfig) {
        fn set<T: Clone>(ours: &mut Option<T>, theirs: &Option<T>) {
            if theirs.is_some() {
                ours.clone_from(theirs);
            }
        }
        // Destructured so that a new field can't be forgotten here.
        let HostConfig {
            kind,
            destination,
            flake,
            attr,
            profile,
            probe,
            activate,
            activate_on,
            hostnames,
            ssh_options,
            optional,
            note,
        } = other;
        set(&mut self.kind, kind);
        set(&mut self.destination, destination);
        set(&mut self.flake, flake);
        set(&mut self.attr, attr);
        set(&mut self.profile, profile);
        set(&mut self.probe, probe);
        set(&mut self.activate, activate);
        set(&mut self.activate_on, activate_on);
        set(&mut self.hostnames, hostnames);
        set(&mut self.ssh_options, ssh_options);
        set(&mut self.optional, optional);
        set(&mut self.note, note);
    }
}

/// Hosts a flake describes about itself, from one pure evaluation
/// ([`crate::nix`] `hosts` command).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FlakeHosts {
    /// `(name, kind)` of every `nixos`/`darwin`/`homeConfigurations` entry.
    pub discovered: Vec<(String, Kind)>,
    /// The inventory attribute's `hosts`, if the flake has one.
    pub inventory: BTreeMap<String, HostConfig>,
}

/// Where a host's commands run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// This machine: no ssh. `assumed` when only the "single darwin host on
    /// a Mac" rule picked it.
    Local {
        assumed: bool,
    },
    Ssh(SshTarget),
}

/// A parsed ssh destination plus per-host ssh settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    /// As configured, for display and `{destination}`.
    pub destination: String,
    pub user: Option<String>,
    /// Host name or address (IPv6 without brackets).
    pub host: String,
    pub port: Option<u16>,
    /// Per-host ssh arguments.
    pub options: Vec<String>,
    /// Forced-command route: this is sent instead of the probe script, and
    /// there is no shell (no remote diff/build/activation).
    pub forced_command: Option<String>,
}

/// Which layers defined a host (for `hostctl hosts`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Sources {
    pub discovered: bool,
    pub inventory: bool,
    pub configured: bool,
}

/// A fully resolved host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    pub name: String,
    pub kind: Kind,
    /// The flake (as configured, unlocked) this host is compared against.
    pub flake: String,
    /// Absolute attribute path (no leading `.`/`#`).
    pub attr: String,
    pub target: Target,
    /// Profile override, `~/` already rewritten to `$HOME/` form for the script.
    pub profile: Option<String>,
    /// Activation template; `None` when disabled.
    pub activate: Option<String>,
    pub activate_on: ActivateOn,
    pub optional: bool,
    pub note: Option<String>,
    pub sources: Sources,
}

impl Host {
    pub fn is_local(&self) -> bool {
        matches!(self.target, Target::Local { .. })
    }

    /// `local`, `local (assumed)` or the ssh destination, for display.
    pub fn reached_via(&self) -> &str {
        match &self.target {
            Target::Local { assumed: false } => "local",
            Target::Local { assumed: true } => "local (assumed)",
            Target::Ssh(ssh) => &ssh.destination,
        }
    }

    pub fn ssh(&self) -> Option<&SshTarget> {
        match &self.target {
            Target::Ssh(ssh) => Some(ssh),
            Target::Local { .. } => None,
        }
    }
}

/// CLI flags that override config values.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides {
    pub flake: Option<String>,
    pub jobs: Option<usize>,
    pub self_host: Option<String>,
    pub no_cache: bool,
}

/// The result of [`Config::resolve_hosts`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolved {
    /// Sorted by name.
    pub hosts: Vec<Host>,
    /// Things worth telling the user (e.g. a name found in two outputs).
    pub warnings: Vec<String>,
}

/// The most evaluations [`Config::jobs`] allows at once, whatever `jobs` or
/// `--jobs` say: far beyond any machine's memory (each eval peaks at
/// 0.6–0.9 GB), and safely below what a semaphore can count.
const MAX_JOBS: usize = 1 << 16;

/// `min(CPUs, MemAvailable / 1 GiB)`, at least 1 (each eval peaks at
/// 0.6–0.9 GB).
pub fn default_jobs() -> usize {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let by_memory = crate::util::mem_available_bytes()
        .map(|b| (b / (1 << 30)) as usize)
        .unwrap_or(cpus);
    cpus.min(by_memory).max(1)
}

impl Config {
    /// Parse and [`validate`](Self::validate) a config. Unknown keys are
    /// errors whose message names the key and line.
    pub fn from_toml(s: &str) -> anyhow::Result<Config> {
        let config: Config = toml::from_str(s)?;
        config.validate()?;
        Ok(config)
    }

    /// [`from_toml`](Self::from_toml) on a file; errors name the file. Reads
    /// nothing but `path` (no environment, `$HOME` or network), so the Nix
    /// modules can run it in a build sandbox.
    pub fn from_file(path: &Path) -> anyhow::Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("can't read config file {}", path.display()))?;
        parse_file(path, &text)
    }

    /// The implicit config locations, in search order:
    /// `$XDG_CONFIG_HOME/hostctl/config.toml` (or `~/.config/hostctl/config.toml`),
    /// then `/etc/hostctl/config.toml`.
    pub fn search_paths() -> Vec<PathBuf> {
        search_paths_in(
            std::env::var_os("XDG_CONFIG_HOME").as_deref(),
            std::env::var_os("HOME").as_deref(),
        )
    }

    /// Load `explicit`, else `$HOSTCTL_CONFIG` (when set and non-empty),
    /// else the first existing [`search_paths`](Self::search_paths) entry. A
    /// missing explicit/env file is an error; finding nothing implicitly
    /// yields `(Config::default(), None)`. An empty `explicit` path counts as
    /// none (clap passes an empty `$HOSTCTL_CONFIG` on as `--config ""`).
    pub fn load(explicit: Option<&Path>) -> anyhow::Result<(Config, Option<PathBuf>)> {
        let env = std::env::var_os("HOSTCTL_CONFIG");
        load_from(explicit, env.as_deref(), &Config::search_paths())
    }

    /// Apply CLI overrides (they win over the file). Empty strings and
    /// `jobs = 0` count as not given: clap passes empty `$HOSTCTL_FLAKE` /
    /// `$HOSTCTL_SELF` values through. A flake is checked like the file's
    /// (see [`validate`](Self::validate)); on error nothing is applied.
    pub fn apply(&mut self, overrides: &Overrides) -> anyhow::Result<()> {
        let given = |value: &Option<String>| value.as_ref().filter(|v| !v.is_empty()).cloned();
        if let Some(problem) = given(&overrides.flake)
            .as_deref()
            .and_then(|flake| dash_flake("--flake (or $HOSTCTL_FLAKE)", flake))
        {
            anyhow::bail!(problem);
        }
        if let Some(flake) = given(&overrides.flake) {
            self.flake = Some(flake);
        }
        if let Some(name) = given(&overrides.self_host) {
            self.self_host = Some(name);
        }
        if let Some(jobs) = overrides.jobs.filter(|&jobs| jobs > 0) {
            self.jobs = Some(jobs);
        }
        if overrides.no_cache {
            self.cache = false;
        }
        Ok(())
    }

    /// Semantic checks beyond the schema, with messages naming the host and
    /// field (`host "mac": profile "nix/profile" must be absolute or start
    /// with ~/`); every problem is reported, one per line:
    /// - `timeout` > 0; `jobs` ≥ 1 when set; `nix.command`/`ssh.command`
    ///   non-empty; `flake` and `self` non-empty when set; `exclude` entries
    ///   non-empty;
    /// - every host and `[defaults]`: non-empty name; non-empty
    ///   `destination`/`flake`/`attr`/`probe` when set; `attr` without a
    ///   leading `.`/`#`; no `flake` (the top-level one either) starting
    ///   with `-`, which nix would read as an option; `activate = true` is
    ///   an error (only `false` or a command); `profile` absolute or `~/…`;
    ///   `destination` parses ([`crate::remote::parse_destination`], after
    ///   `{name}` substitution: the table's name, or `name` for
    ///   `[defaults]`);
    /// - `kind = "custom"` needs `attr` and `profile` (unless `probe` is
    ///   set), counting what `[defaults]` provides.
    pub fn validate(&self) -> anyhow::Result<()> {
        let mut problems = Problems::default();
        if self.timeout.is_zero() {
            problems.push("timeout must be greater than 0".into());
        }
        if self.jobs == Some(0) {
            problems.push("jobs must be at least 1".into());
        }
        if self.nix.command.is_empty() {
            problems.push("nix.command must not be empty".into());
        }
        if self.ssh.command.is_empty() {
            problems.push("ssh.command must not be empty".into());
        }
        if self.flake.as_deref() == Some("") {
            problems.push("flake must not be empty".into());
        }
        if let Some(problem) = self.flake.as_deref().and_then(|f| dash_flake("flake", f)) {
            problems.push(problem);
        }
        if self.self_host.as_deref() == Some("") {
            problems.push("self must not be empty".into());
        }
        if !self.inventory.is_empty()
            && !self.inventory.split('.').all(|part| {
                let mut bytes = part.bytes();
                matches!(bytes.next(), Some(b) if b.is_ascii_alphabetic() || b == b'_')
                    && bytes.all(|b| b.is_ascii_alphanumeric() || b"_'-".contains(&b))
            })
        {
            problems.push(format!(
                "inventory {:?} must be \"\" or a dot-separated attribute path like \"lib.hostctl\"",
                self.inventory
            ));
        }
        if self.exclude.iter().any(String::is_empty) {
            problems.push("exclude must not contain empty patterns".into());
        }
        check_fields(&mut problems, "[defaults]", &self.defaults);
        check_destination(&mut problems, "[defaults]", &self.defaults, "name");
        for (name, host) in &self.hosts {
            let label = host_label(name);
            if name.is_empty() {
                problems.push(format!("{label}: the name must not be empty"));
            }
            check_fields(&mut problems, &label, host);
            check_destination(&mut problems, &label, host, name);
            if host.kind.or(self.defaults.kind) == Some(Kind::Custom) {
                let set = |field: fn(&HostConfig) -> bool| field(host) || field(&self.defaults);
                check_custom(
                    &mut problems,
                    &label,
                    set(|h| h.attr.is_some()),
                    set(|h| h.profile.is_some() || h.probe.is_some()),
                );
            }
        }
        problems.into_result()
    }

    /// The config as TOML (what `hostctl config show` prints); round-trips
    /// through [`from_toml`](Self::from_toml).
    pub fn to_toml(&self) -> String {
        // Can't fail for this schema (string keys, no bare enums or tuples);
        // still, a `config show` that explains beats a panic.
        toml::to_string(self)
            .unwrap_or_else(|e| format!("# can't show the configuration as TOML: {e}\n"))
    }

    /// `jobs`, else [`default_jobs`]; at least 1 and at most 65536.
    pub fn jobs(&self) -> usize {
        self.jobs.unwrap_or_else(default_jobs).clamp(1, MAX_JOBS)
    }

    /// Turn the layers into [`Host`]s, sorted by name. Per host, fields merge
    /// (later wins): discovered `kind` → `flake_hosts.inventory` (minus
    /// `ssh-options` and `probe`, which only the local config may set; a
    /// warning says so) → `[defaults]` → `[hosts.<name>]`. Then:
    ///
    /// - the host set is discovered names (if `discover`; otherwise discovery
    ///   is ignored altogether, kinds included) ∪ inventory names ∪
    ///   configured names, minus `exclude` globs; `flake_hosts` describes the
    ///   default flake only;
    /// - a name in several outputs keeps the kind nixos > darwin >
    ///   home-manager, with a warning (unless a later layer sets `kind`);
    /// - every merged host passes the per-host checks of
    ///   [`validate`](Self::validate) (inventory values haven't been checked
    ///   before); all problems are reported together;
    /// - kind: merged, else `custom` if `attr` is set, else an error asking
    ///   for `kind`;
    /// - flake: merged, else `self.flake`, else an error ("no flake for host …");
    /// - attr: merged, else [`Kind::default_attr`];
    /// - destination: merged (`{name}` expanded), else the name if it matches
    ///   `^[A-Za-z0-9_][A-Za-z0-9._-]*(@[A-Za-z0-9._-]+)?$`, else an error
    ///   asking for `destination` (only for hosts reached over ssh); parsed
    ///   with [`crate::remote::parse_destination`];
    /// - target: per [`crate::local::local_hosts`] with `local.forced`, else
    ///   the config's `self` (an unknown forced `self` is an error), else
    ///   [`Target::Ssh`] with `ssh-options` and `probe` as the forced command;
    /// - activate: `false`/`""` → `None`, unset → [`Kind::default_activate`];
    /// - profile: `~/x` → `$HOME/x`.
    pub fn resolve_hosts(
        &self,
        flake_hosts: Option<&FlakeHosts>,
        local: &LocalInfo,
    ) -> anyhow::Result<Resolved> {
        let mut problems = Problems::default();
        let mut warnings = Vec::new();

        // The kinds each name is discovered as, best first: `Kind`'s
        // declaration order is the precedence (nixos > darwin > home-manager).
        let mut discovered: BTreeMap<&str, Vec<Kind>> = BTreeMap::new();
        if self.discover {
            for (name, kind) in flake_hosts.into_iter().flat_map(|f| &f.discovered) {
                let kinds = discovered.entry(name).or_default();
                if let Err(at) = kinds.binary_search(kind) {
                    kinds.insert(at, *kind);
                }
            }
        }
        let inventory = flake_hosts.map(|f| &f.inventory);
        let names: BTreeSet<&str> = discovered
            .keys()
            .copied()
            .chain(
                inventory
                    .into_iter()
                    .flat_map(|i| i.keys().map(String::as_str)),
            )
            .chain(self.hosts.keys().map(String::as_str))
            .filter(|name| {
                !self
                    .exclude
                    .iter()
                    .any(|glob| crate::util::glob_match(glob, name))
            })
            .collect();

        let mut drafts = Vec::with_capacity(names.len());
        for name in names {
            let kinds = discovered.get(name).map_or(&[][..], Vec::as_slice);
            // The flake being checked may not be the user's own: its
            // inventory can't choose ssh options (ProxyCommand/LocalCommand
            // run commands here) or a raw remote command.
            let from_inventory = inventory.and_then(|i| i.get(name)).map(|entry| {
                let mut entry = entry.clone();
                let ignored: Vec<&str> = [
                    entry.ssh_options.take().map(|_| "ssh-options"),
                    entry.probe.take().map(|_| "probe"),
                ]
                .into_iter()
                .flatten()
                .collect();
                (entry, ignored)
            });
            if let Some((_, ignored)) = &from_inventory
                && !ignored.is_empty()
            {
                warnings.push(format!(
                    "host {name:?}: ignoring {} from the flake's inventory (set them in your config)",
                    ignored.join(" and ")
                ));
            }
            let layers = [
                from_inventory.as_ref().map(|(entry, _)| entry),
                Some(&self.defaults),
                self.hosts.get(name),
            ];
            let sources = Sources {
                discovered: !kinds.is_empty(),
                inventory: layers[0].is_some(),
                configured: layers[2].is_some(),
            };
            let mut config = HostConfig {
                kind: kinds.first().copied(),
                ..HostConfig::default()
            };
            for layer in layers.iter().flatten() {
                config.merge(layer);
            }
            if kinds.len() > 1 && !layers.iter().flatten().any(|l| l.kind.is_some()) {
                warnings.push(several_outputs(name, kinds));
            }
            if let Some(draft) = self.draft(name, config, sources, &mut problems) {
                drafts.push(draft);
            }
        }
        problems.into_result()?;

        let forced_by_config;
        let local = match (&local.forced, &self.self_host) {
            (None, Some(name)) => {
                forced_by_config = LocalInfo {
                    forced: Some(name.clone()),
                    ..local.clone()
                };
                &forced_by_config
            }
            _ => local,
        };
        let localities = {
            let candidates: Vec<Candidate<'_>> = drafts.iter().map(Draft::candidate).collect();
            crate::local::local_hosts(&candidates, local).map_err(anyhow::Error::msg)?
        };

        let mut problems = Problems::default();
        let hosts = drafts
            .into_iter()
            .zip(localities)
            .filter_map(|(draft, locality)| draft.finish(locality, &mut problems))
            .collect();
        problems.into_result()?;
        Ok(Resolved { hosts, warnings })
    }

    /// The first resolution step for one host: kind, flake, attr and the
    /// configured destination, or `None` after recording why not.
    fn draft<'a>(
        &self,
        name: &'a str,
        mut config: HostConfig,
        sources: Sources,
        problems: &mut Problems,
    ) -> Option<Draft<'a>> {
        let label = host_label(name);
        if name.is_empty() {
            problems.push(format!("{label}: the name must not be empty"));
            return None;
        }
        let before = problems.len();
        check_fields(problems, &label, &config);
        let kind = config
            .kind
            .or(config.attr.is_some().then_some(Kind::Custom));
        if kind.is_none() {
            problems.push(format!(
                "{label}: unknown kind; set kind = \"nixos\", \"darwin\", \"home-manager\" \
                 or \"custom\" in {}",
                table(name)
            ));
        }
        if kind == Some(Kind::Custom) {
            check_custom(
                problems,
                &label,
                config.attr.is_some(),
                config.profile.is_some() || config.probe.is_some(),
            );
        }
        let flake = config.flake.take().or_else(|| self.flake.clone());
        if flake.is_none() {
            problems.push(format!(
                "no flake for host {name:?}: set flake at the top of the config or in {}, \
                 or pass --flake",
                table(name)
            ));
        }
        let destination = match config.destination.as_deref().filter(|d| !d.is_empty()) {
            Some(template) => match expand_destination(template, name) {
                Ok(destination) => Some(destination),
                Err(e) => {
                    problems.push(format!("{label}: {e}"));
                    None
                }
            },
            None => None,
        };
        if problems.len() > before {
            return None;
        }
        let kind = kind?;
        let attr = config.attr.take().or_else(|| kind.default_attr(name))?;
        Some(Draft {
            name,
            kind,
            flake: flake?,
            attr,
            destination,
            config,
            sources,
        })
    }
}

/// A host between the two resolution steps (locality needs every host).
struct Draft<'a> {
    name: &'a str,
    kind: Kind,
    flake: String,
    attr: String,
    /// The configured destination, `{name}` expanded, and its parse.
    destination: Option<(String, Destination)>,
    /// The merged layers (without `flake` and `attr`, moved out above).
    config: HostConfig,
    sources: Sources,
}

impl Draft<'_> {
    fn candidate(&self) -> Candidate<'_> {
        Candidate {
            name: self.name,
            kind: self.kind,
            hostnames: self.config.hostnames.as_deref().unwrap_or_default(),
            destination: self.destination.as_ref().map(|(_, parsed)| parsed),
        }
    }

    /// The second step: target, activation and profile.
    fn finish(self, locality: Locality, problems: &mut Problems) -> Option<Host> {
        let Draft {
            name,
            kind,
            flake,
            attr,
            destination,
            config,
            sources,
        } = self;
        let HostConfig {
            profile,
            probe,
            activate,
            activate_on,
            ssh_options,
            optional,
            note,
            ..
        } = config;
        let target = match locality {
            Locality::Local | Locality::Assumed if kind == Kind::Custom && profile.is_none() => {
                problems.push(format!(
                    "{}: kind = \"custom\" needs profile on this machine (probe only applies over ssh)",
                    host_label(name)
                ));
                return None;
            }
            Locality::Local => Target::Local { assumed: false },
            Locality::Assumed => Target::Local { assumed: true },
            Locality::Remote => {
                let parsed = match destination {
                    Some(destination) => Ok(destination),
                    None if name_is_destination(name) => crate::remote::parse_destination(name)
                        .map(|parsed| (name.to_owned(), parsed)),
                    None => Err(format!(
                        "its name isn't an ssh destination; set destination in {} (or exclude it)",
                        table(name)
                    )),
                };
                let (destination, parsed) = match parsed {
                    Ok(parsed) => parsed,
                    Err(e) => {
                        problems.push(format!("{}: {e}", host_label(name)));
                        return None;
                    }
                };
                Target::Ssh(SshTarget {
                    destination,
                    user: parsed.user,
                    host: parsed.host,
                    port: parsed.port,
                    options: ssh_options.unwrap_or_default(),
                    forced_command: probe,
                })
            }
        };
        let activate = match activate {
            None => kind.default_activate().map(str::to_owned),
            Some(ActivateSetting::Command(command)) if !command.is_empty() => Some(command),
            Some(_) => None,
        };
        Some(Host {
            name: name.to_owned(),
            kind,
            flake,
            attr,
            target,
            profile: profile.map(|p| match p.strip_prefix("~/") {
                Some(rest) => format!("$HOME/{rest}"),
                None => p,
            }),
            activate,
            activate_on: activate_on.unwrap_or_default(),
            optional: optional.unwrap_or(false),
            note,
            sources,
        })
    }
}

/// Config problems, collected so that all of them are reported at once.
#[derive(Default)]
struct Problems(Vec<String>);

impl Problems {
    fn push(&mut self, problem: String) {
        self.0.push(problem);
    }

    fn len(&self) -> usize {
        self.0.len()
    }

    fn into_result(mut self) -> anyhow::Result<()> {
        if self.0.len() > 1 {
            let n = self.0.len();
            return Err(anyhow::anyhow!("{n} problems:\n  {}", self.0.join("\n  ")));
        }
        self.0.pop().map_or(Ok(()), |p| Err(anyhow::Error::msg(p)))
    }
}

/// `host "<name>"`, the prefix of a host's problems (`{:?}` also escapes
/// control characters in names that come from a flake).
fn host_label(name: &str) -> String {
    format!("host {name:?}")
}

/// `[hosts.<name>]` as it would be written in the config.
fn table(name: &str) -> String {
    let bare = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if bare {
        format!("[hosts.{name}]")
    } else {
        format!("[hosts.{name:?}]")
    }
}

/// The checks of one layer's own values (`[defaults]`, a `[hosts.<name>]`
/// table, or a host's merged layers) that don't need other layers.
fn check_fields(problems: &mut Problems, label: &str, host: &HostConfig) {
    let fields = [
        ("destination", &host.destination),
        ("flake", &host.flake),
        ("attr", &host.attr),
        ("probe", &host.probe),
    ];
    for (field, value) in fields {
        if value.as_deref() == Some("") {
            problems.push(format!("{label}: {field} must not be empty"));
        }
    }
    let what = format!("{label}: flake");
    if let Some(problem) = host.flake.as_deref().and_then(|f| dash_flake(&what, f)) {
        problems.push(problem);
    }
    if let Some(attr) = host.attr.as_deref().filter(|a| a.starts_with(['.', '#'])) {
        problems.push(format!(
            "{label}: attr {attr:?} must not start with '.' or '#' (it is always absolute)"
        ));
    }
    if host.activate == Some(ActivateSetting::Enabled(true)) {
        problems.push(format!(
            "{label}: activate = true is not valid; set a command, or false to disable activation"
        ));
    }
    let relative = |p: &&str| !(p.starts_with('/') || p.starts_with("~/"));
    if let Some(profile) = host.profile.as_deref().filter(relative) {
        problems.push(format!(
            "{label}: profile {profile:?} must be absolute or start with ~/"
        ));
    }
}

/// The problem with a flake reference `what` (`flake`, `host "web1":
/// flake`, …) that starts with `-`: nix would read it as an option (e.g.
/// `--impure`), like ssh would a destination.
fn dash_flake(what: &str, flake: &str) -> Option<String> {
    flake.starts_with('-').then(|| {
        format!("{what} {flake:?} must not start with '-' (nix would read it as an option)")
    })
}

/// `destination`, when set and non-empty, must parse once `{name}` is
/// replaced by `name`.
fn check_destination(problems: &mut Problems, label: &str, host: &HostConfig, name: &str) {
    if let Some(template) = host.destination.as_deref().filter(|d| !d.is_empty())
        && let Err(e) = expand_destination(template, name)
    {
        problems.push(format!("{label}: {e}"));
    }
}

fn check_custom(problems: &mut Problems, label: &str, has_attr: bool, has_profile_or_probe: bool) {
    if !has_attr {
        problems.push(format!("{label}: kind = \"custom\" needs attr"));
    }
    if !has_profile_or_probe {
        problems.push(format!(
            "{label}: kind = \"custom\" needs profile (or probe)"
        ));
    }
}

/// A configured destination with `{name}` expanded, and its parse.
fn expand_destination(template: &str, name: &str) -> Result<(String, Destination), String> {
    let destination = template.replace("{name}", name);
    let parsed = crate::remote::parse_destination(&destination)?;
    Ok((destination, parsed))
}

/// Whether a host's name can double as its ssh destination:
/// `^[A-Za-z0-9_][A-Za-z0-9._-]*(@[A-Za-z0-9._-]+)?$`.
fn name_is_destination(name: &str) -> bool {
    let word = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    };
    let (first, host) = match name.split_once('@') {
        Some((user, host)) => (user, Some(host)),
        None => (name, None),
    };
    word(first) && !first.starts_with(['.', '-']) && host.is_none_or(word)
}

/// The warning for a name found in several `*Configurations` outputs;
/// `kinds` is sorted by precedence.
fn several_outputs(name: &str, kinds: &[Kind]) -> String {
    let outputs: Vec<&str> = kinds
        .iter()
        .map(|k| k.output().unwrap_or(k.as_str()))
        .collect();
    let listed = match outputs.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} and {last}", rest.join(", ")),
        _ => outputs.concat(),
    };
    let chosen = kinds.first().map_or("", |k| k.as_str());
    format!(
        "{} is in {listed}; checking it as {chosen} (set kind in {} to choose)",
        host_label(name),
        table(name)
    )
}

/// [`Config::from_toml`] with errors naming `path`.
fn parse_file(path: &Path, text: &str) -> anyhow::Result<Config> {
    Config::from_toml(text).with_context(|| format!("invalid config file {}", path.display()))
}

/// [`Config::search_paths`] for these `$XDG_CONFIG_HOME` and `$HOME` values;
/// relative (or empty) ones are ignored, as the XDG spec asks.
fn search_paths_in(xdg_config_home: Option<&OsStr>, home: Option<&OsStr>) -> Vec<PathBuf> {
    fn absolute(dir: Option<&OsStr>) -> Option<&Path> {
        dir.map(Path::new).filter(|d| d.is_absolute())
    }
    let user_dir = absolute(xdg_config_home)
        .map(Path::to_path_buf)
        .or_else(|| absolute(home).map(|home| home.join(".config")));
    user_dir
        .map(|dir| dir.join("hostctl").join("config.toml"))
        .into_iter()
        .chain([PathBuf::from("/etc/hostctl/config.toml")])
        .collect()
}

/// [`Config::load`] with `$HOSTCTL_CONFIG` and the search paths passed in.
fn load_from(
    explicit: Option<&Path>,
    env: Option<&OsStr>,
    search: &[PathBuf],
) -> anyhow::Result<(Config, Option<PathBuf>)> {
    let chosen = explicit
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .or_else(|| env.filter(|v| !v.is_empty()).map(PathBuf::from));
    if let Some(path) = chosen {
        return Ok((Config::from_file(&path)?, Some(path)));
    }
    for path in search {
        match std::fs::read_to_string(path) {
            Ok(text) => return Ok((parse_file(path, &text)?, Some(path.clone()))),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) => {}
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("can't read config file {}", path.display()));
            }
        }
    }
    Ok((Config::default(), None))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::local::Os;

    const EXAMPLE: &str = include_str!("../examples/config.toml");
    const BIGMEM: &str = "devhost-devuser1-1a-e6904c1d.dc-west1.example.com";
    const DESKS: [(&str, &str); 4] = [
        (
            "al2-aarch64",
            "devhost-devuser1-1a-7f3a0b82.dc-west1.example.com",
        ),
        (
            "al2-x86_64",
            "devhost-devuser1-1a-8d41e07a.dc-west1.example.com",
        ),
        (
            "al2023-x86_64",
            "devhost-devuser1-1a-c2d85e16.dc-west1.example.com",
        ),
        ("bigmem", BIGMEM),
    ];
    const ALL_HOSTS: [&str; 5] = [
        "al2-aarch64",
        "al2-x86_64",
        "al2023-x86_64",
        "bigmem",
        "mac-m1",
    ];

    fn machine(hostname: &str, user: &str, os: Os) -> LocalInfo {
        LocalInfo {
            hostname: hostname.into(),
            user: user.into(),
            os,
            forced: None,
        }
    }

    /// The user's desk "bigmem" (this machine).
    fn on_bigmem() -> LocalInfo {
        machine(BIGMEM, "devuser1", Os::Linux)
    }

    /// The user's Mac.
    fn on_the_mac() -> LocalInfo {
        machine("Julians-MacBook-Pro.local", "devuser1", Os::Darwin)
    }

    /// A machine that is none of the hosts.
    fn elsewhere() -> LocalInfo {
        machine("ci-runner-7", "runner", Os::Linux)
    }

    fn forced(mut local: LocalInfo, name: &str) -> LocalInfo {
        local.forced = Some(name.into());
        local
    }

    fn discovered(entries: &[(&str, Kind)]) -> FlakeHosts {
        FlakeHosts {
            discovered: entries.iter().map(|&(n, k)| (n.to_owned(), k)).collect(),
            inventory: BTreeMap::new(),
        }
    }

    /// What discovery finds in github:jjant/nix-config.
    fn nix_config() -> FlakeHosts {
        let hm = Kind::HomeManager;
        discovered(&[
            ("mac-m1", Kind::Darwin),
            ("al2-aarch64", hm),
            ("al2-x86_64", hm),
            ("al2023-x86_64", hm),
            ("bigmem", hm),
        ])
    }

    fn parse(toml: &str) -> Config {
        Config::from_toml(toml).unwrap_or_else(|e| panic!("{e:#}"))
    }

    fn parse_error(toml: &str) -> String {
        format!("{:#}", Config::from_toml(toml).unwrap_err())
    }

    fn resolve(config: &Config, flake_hosts: Option<&FlakeHosts>, local: &LocalInfo) -> Resolved {
        config
            .resolve_hosts(flake_hosts, local)
            .unwrap_or_else(|e| panic!("{e:#}"))
    }

    fn resolve_error(
        config: &Config,
        flake_hosts: Option<&FlakeHosts>,
        local: &LocalInfo,
    ) -> String {
        format!(
            "{:#}",
            config.resolve_hosts(flake_hosts, local).unwrap_err()
        )
    }

    fn names(resolved: &Resolved) -> Vec<&str> {
        resolved.hosts.iter().map(|h| h.name.as_str()).collect()
    }

    fn local_names(resolved: &Resolved) -> Vec<&str> {
        let local = resolved.hosts.iter().filter(|h| h.is_local());
        local.map(|h| h.name.as_str()).collect()
    }

    fn host<'a>(resolved: &'a Resolved, name: &str) -> &'a Host {
        let found = resolved.hosts.iter().find(|h| h.name == name);
        found.unwrap_or_else(|| panic!("no host {name}"))
    }

    fn ssh<'a>(resolved: &'a Resolved, name: &str) -> &'a SshTarget {
        let ssh = host(resolved, name).ssh();
        ssh.unwrap_or_else(|| panic!("{name} is local"))
    }

    fn ssh_target(
        destination: &str,
        user: Option<&str>,
        host: &str,
        port: Option<u16>,
    ) -> SshTarget {
        SshTarget {
            destination: destination.into(),
            user: user.map(str::to_owned),
            host: host.into(),
            port,
            options: Vec::new(),
            forced_command: None,
        }
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// A host table with every field set.
    fn full_host() -> HostConfig {
        HostConfig {
            kind: Some(Kind::Darwin),
            destination: Some("ssh://jj@[::1]:2022".into()),
            flake: Some("git+ssh://git@github.com/jjant/nix-config?ref=main".into()),
            attr: Some(r#"darwinConfigurations."jj@mac".system"#.into()),
            profile: Some("~/.local/state/nix/profiles/home-manager".into()),
            probe: Some("current-system".into()),
            activate: Some(ActivateSetting::Enabled(false)),
            activate_on: Some(ActivateOn::Local),
            hostnames: Some(strings(&["Julians-MacBook-Pro", "mac.local"])),
            ssh_options: Some(strings(&["-o", "ProxyJump=desk"])),
            optional: Some(true),
            note: Some("\"quoted\", 'single', back\\slash, ünïcödé ✓\nsecond line".into()),
        }
    }

    /// A config with every option away from its default.
    fn rich() -> Config {
        let mut hosts = BTreeMap::new();
        hosts.insert("jj@mac".to_owned(), full_host());
        hosts.insert(
            "al2-x86_64".to_owned(),
            HostConfig {
                kind: Some(Kind::HomeManager),
                destination: Some(DESKS[1].1.into()),
                activate: Some(ActivateSetting::Command("nix run {flake}#activate".into())),
                ..HostConfig::default()
            },
        );
        hosts.insert(
            "router 1".to_owned(),
            HostConfig {
                kind: Some(Kind::Custom),
                attr: Some("packages.aarch64-linux.router".into()),
                profile: Some("/nix/var/nix/profiles/router".into()),
                activate: Some(ActivateSetting::Command(String::new())),
                ..HostConfig::default()
            },
        );
        hosts.insert("empty".to_owned(), HostConfig::default());
        Config {
            flake: Some("github:jjant/nix-config".into()),
            jobs: Some(3),
            timeout: HumanDuration(Duration::from_millis(1500)),
            eval_timeout: HumanDuration::from_secs(0),
            self_host: Some("bigmem".into()),
            discover: false,
            inventory: String::new(),
            exclude: strings(&["*-iso", "ci-[0-9]*"]),
            cache: false,
            nix: NixConfig {
                command: "/run/current-system/sw/bin/nix".into(),
                options: strings(&["--option", "warn-dirty", "false"]),
                read_only: false,
                refresh: HumanDuration::from_secs(0),
            },
            ssh: SshConfig {
                command: "/usr/bin/ssh".into(),
                options: strings(&["-F", "/dev/null"]),
                persist: HumanDuration::from_secs(5400),
            },
            defaults: HostConfig {
                destination: Some("{name}.tailnet.example".into()),
                ssh_options: Some(strings(&["-4"])),
                activate_on: Some(ActivateOn::Target),
                ..HostConfig::default()
            },
            hosts,
        }
    }

    // ── parsing ────────────────────────────────────────────────────────

    #[test]
    fn an_empty_file_is_the_defaults() {
        assert_eq!(parse(""), Config::default());
        assert_eq!(parse("# nothing but a comment\n\n"), Config::default());
        let defaults = Config::default();
        assert_eq!(defaults.timeout, HumanDuration::from_secs(20));
        assert_eq!(defaults.eval_timeout, HumanDuration::from_secs(600));
        assert!(defaults.discover && defaults.cache && defaults.nix.read_only);
        assert_eq!(defaults.inventory, "lib.hostctl");
        assert_eq!(defaults.nix.refresh, HumanDuration::from_secs(60));
        assert_eq!(defaults.ssh.persist, HumanDuration::from_secs(600));
    }

    #[test]
    fn parses_every_option() {
        let config = parse(
            r#"
            flake = "github:jjant/nix-config"
            self = "bigmem"
            jobs = 8
            timeout = 5
            eval-timeout = "0"
            discover = false
            inventory = "lib.fleet"
            exclude = ["*-iso"]
            cache = false

            [nix]
            command = "/run/current-system/sw/bin/nix"
            options = ["--option", "warn-dirty", "false"]
            read-only = false
            refresh = "1m30s"

            [ssh]
            command = "/usr/bin/ssh"
            options = ["-F", "/dev/null"]
            persist = "0s"

            [defaults]
            destination = "{name}.tailnet.example"

            [hosts."jj@mac"]
            kind = "darwin"
            destination = "ssh://jj@[::1]:2022"
            flake = "git+ssh://git@github.com/jjant/nix-config?ref=main"
            attr = 'darwinConfigurations."jj@mac".system'
            profile = "~/.local/state/nix/profiles/home-manager"
            probe = "current-system"
            activate = false
            activate-on = "local"
            hostnames = ["Julians-MacBook-Pro", "mac.local"]
            ssh-options = ["-o", "ProxyJump=desk"]
            optional = true
            note = """"quoted", 'single', back\\slash, ünïcödé ✓
            second line"""
            "#,
        );
        assert_eq!(config.flake.as_deref(), Some("github:jjant/nix-config"));
        assert_eq!(config.self_host.as_deref(), Some("bigmem"));
        assert_eq!(config.jobs, Some(8));
        assert_eq!(config.jobs(), 8);
        assert_eq!(config.timeout, HumanDuration::from_secs(5));
        assert!(config.eval_timeout.is_zero());
        assert!(!config.discover && !config.cache);
        assert_eq!(config.inventory, "lib.fleet");
        assert_eq!(config.exclude, ["*-iso"]);
        assert_eq!(
            config.nix,
            NixConfig {
                command: "/run/current-system/sw/bin/nix".into(),
                options: strings(&["--option", "warn-dirty", "false"]),
                read_only: false,
                refresh: HumanDuration::from_secs(90),
            }
        );
        assert_eq!(
            config.ssh,
            SshConfig {
                command: "/usr/bin/ssh".into(),
                options: strings(&["-F", "/dev/null"]),
                persist: HumanDuration::from_secs(0),
            }
        );
        assert_eq!(
            config.defaults.destination.as_deref(),
            Some("{name}.tailnet.example")
        );
        let mut want = full_host();
        want.note =
            Some("\"quoted\", 'single', back\\slash, ünïcödé ✓\n            second line".into());
        assert_eq!(config.hosts["jj@mac"], want);
    }

    #[test]
    fn unknown_keys_name_the_key_and_line() {
        let cases = [
            ("flake = \"f\"\nflakes = \"g\"\n", "flakes", 2),
            ("[nix]\nread_only = false\n", "read_only", 2),
            ("[ssh]\n\npersists = \"1m\"\n", "persists", 3),
            ("[ui]\nrefresh-interval = \"1m\"\n", "ui", 1),
            ("[defaults]\ndestinaton = \"{name}\"\n", "destinaton", 2),
            (
                "[hosts.bigmem]\nkind = \"home-manager\"\nhostname = \"bigmem\"\n",
                "hostname",
                3,
            ),
        ];
        for (toml, key, line) in cases {
            let error = parse_error(toml);
            assert!(
                error.contains(&format!("unknown field `{key}`")),
                "{toml:?}: {error}"
            );
            assert!(
                error.contains(&format!("TOML parse error at line {line}")),
                "{toml:?}: {error}"
            );
        }
    }

    #[test]
    fn bad_values_are_errors_naming_the_line() {
        let cases = [
            (
                "[hosts.a]\nkind = \"nixOS\"\n",
                "unknown variant `nixOS`, expected one of `nixos`, `darwin`, `home-manager`, `custom`",
            ),
            (
                "[hosts.a]\nactivate = 1\n",
                "invalid type: integer `1`, expected an activation command, or false to disable activation",
            ),
            (
                "[hosts.a]\nactivate = [\"echo\"]\n",
                "expected an activation command, or false to disable activation",
            ),
            (
                "[hosts.a]\nactivate-on = \"remote\"\n",
                "unknown variant `remote`, expected `target` or `local`",
            ),
            ("timeout = \"5 parsecs\"\n", "unknown unit \"parsecs\""),
            ("jobs = -1\n", "invalid value: integer `-1`"),
            ("discover = \"yes\"\n", "expected a boolean"),
            ("exclude = \"*-iso\"\n", "expected a sequence"),
            (
                "[hosts]\nbigmem = \"home-manager\"\n",
                "expected struct HostConfig",
            ),
        ];
        for (toml, want) in cases {
            let error = parse_error(toml);
            assert!(error.contains(want), "{toml:?}: {error}");
            assert!(error.contains("TOML parse error at line "), "{error}");
        }
    }

    #[test]
    fn activate_is_false_or_a_command() {
        let activate = |value: &str| {
            let config: Config =
                toml::from_str(&format!("[hosts.a]\nactivate = {value}\n")).unwrap();
            config.hosts["a"].activate.clone()
        };
        assert_eq!(activate("false"), Some(ActivateSetting::Enabled(false)));
        assert_eq!(activate("true"), Some(ActivateSetting::Enabled(true)));
        assert_eq!(
            activate("\"\""),
            Some(ActivateSetting::Command(String::new()))
        );
        assert_eq!(
            activate("'nix run {flake}#activate'"),
            Some(ActivateSetting::Command("nix run {flake}#activate".into()))
        );
    }

    #[test]
    fn host_tables_read_inventory_json() {
        // What `nix eval --json` prints for a flake's `lib.hostctl.hosts.<name>`.
        let json = r#"{"kind":"darwin","destination":"ssh://devuser1@localhost:2022",
            "probe":"current-system","activate":false,"activate-on":"local",
            "ssh-options":["-4"],"optional":true,"note":null}"#;
        assert_eq!(
            serde_json::from_str::<HostConfig>(json).unwrap(),
            HostConfig {
                kind: Some(Kind::Darwin),
                destination: Some("ssh://devuser1@localhost:2022".into()),
                probe: Some("current-system".into()),
                activate: Some(ActivateSetting::Enabled(false)),
                activate_on: Some(ActivateOn::Local),
                ssh_options: Some(strings(&["-4"])),
                optional: Some(true),
                ..HostConfig::default()
            }
        );
        let host: HostConfig =
            serde_json::from_str(r#"{"activate":"nix run {flake}#activate"}"#).unwrap();
        assert_eq!(
            host.activate,
            Some(ActivateSetting::Command("nix run {flake}#activate".into()))
        );
        let error = serde_json::from_str::<HostConfig>(r#"{"destinaton":"x"}"#).unwrap_err();
        assert!(
            error.to_string().contains("unknown field `destinaton`"),
            "{error}"
        );
        let error = serde_json::from_str::<HostConfig>(r#"{"activate":3}"#).unwrap_err();
        assert!(
            error.to_string().contains("expected an activation command"),
            "{error}"
        );
    }

    #[test]
    fn flake_hosts_round_trip_through_json() {
        // The cache stores them.
        let mut hosts = nix_config();
        hosts.inventory.insert("jj@mac".into(), full_host());
        hosts
            .inventory
            .insert("plain".into(), HostConfig::default());
        let json = serde_json::to_string(&hosts).unwrap();
        assert_eq!(serde_json::from_str::<FlakeHosts>(&json).unwrap(), hosts);
    }

    // ── files and search paths ─────────────────────────────────────────

    fn write(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn loaded(result: anyhow::Result<(Config, Option<PathBuf>)>) -> (String, PathBuf) {
        let (config, path) = result.unwrap_or_else(|e| panic!("{e:#}"));
        (config.flake.unwrap_or_default(), path.unwrap_or_default())
    }

    #[test]
    fn from_file_reads_a_file_and_names_it_in_errors() {
        let dir = tempfile::tempdir().unwrap();
        let good = write(dir.path(), "good.toml", EXAMPLE);
        assert_eq!(Config::from_file(&good).unwrap(), parse(EXAMPLE));

        let missing = dir.path().join("missing.toml");
        let error = format!("{:#}", Config::from_file(&missing).unwrap_err());
        let want = format!("can't read config file {}: ", missing.display());
        assert!(error.starts_with(&want), "{error}");

        let bad = write(dir.path(), "bad.toml", "flake = \"f\"\nbogus = 1\n");
        let error = format!("{:#}", Config::from_file(&bad).unwrap_err());
        let want = format!(
            "invalid config file {}: TOML parse error at line 2, column 1",
            bad.display()
        );
        assert!(error.starts_with(&want), "{error}");
        assert!(error.contains("unknown field `bogus`"), "{error}");

        let invalid = write(dir.path(), "invalid.toml", "timeout = \"0s\"\n");
        assert_eq!(
            format!("{:#}", Config::from_file(&invalid).unwrap_err()),
            format!(
                "invalid config file {}: timeout must be greater than 0",
                invalid.display()
            )
        );
    }

    #[test]
    fn search_paths_follow_the_xdg_spec() {
        let os = |s: &'static str| Some(OsStr::new(s));
        let etc = PathBuf::from("/etc/hostctl/config.toml");
        let xdg = PathBuf::from("/xdg/hostctl/config.toml");
        let home = PathBuf::from("/home/jj/.config/hostctl/config.toml");
        let cases = [
            (os("/xdg"), os("/home/jj"), vec![xdg.clone(), etc.clone()]),
            (os("/xdg"), None, vec![xdg.clone(), etc.clone()]),
            (os(""), os("/home/jj"), vec![home.clone(), etc.clone()]),
            (
                os("relative"),
                os("/home/jj"),
                vec![home.clone(), etc.clone()],
            ),
            (None, os("/home/jj"), vec![home.clone(), etc.clone()]),
            (None, None, vec![etc.clone()]),
            (os(""), os(""), vec![etc.clone()]),
            (None, os("relative/home"), vec![etc.clone()]),
        ];
        for (xdg_config_home, home, want) in cases {
            assert_eq!(
                search_paths_in(xdg_config_home, home),
                want,
                "{xdg_config_home:?} {home:?}"
            );
        }
        let real = Config::search_paths();
        assert_eq!(real.last(), Some(&etc));
        assert!(real.len() <= 2, "{real:?}");
    }

    #[test]
    fn load_prefers_explicit_then_env_then_search_paths() {
        let dir = tempfile::tempdir().unwrap();
        let a = write(dir.path(), "a.toml", "flake = \"a\"\n");
        let b = write(dir.path(), "b.toml", "flake = \"b\"\n");
        let c = write(dir.path(), "c.toml", "flake = \"c\"\n");
        let search = [c.clone()];
        let env = Some(b.as_os_str());
        assert_eq!(
            loaded(load_from(Some(&a), env, &search)),
            ("a".into(), a.clone())
        );
        assert_eq!(
            loaded(load_from(None, env, &search)),
            ("b".into(), b.clone())
        );
        assert_eq!(
            loaded(load_from(None, None, &search)),
            ("c".into(), c.clone())
        );
        // Empty values count as unset: clap hands over an empty
        // $HOSTCTL_CONFIG as an explicit "".
        let empty = Some(OsStr::new(""));
        assert_eq!(
            loaded(load_from(None, empty, &search)),
            ("c".into(), c.clone())
        );
        let nothing = Some(Path::new(""));
        assert_eq!(
            loaded(load_from(nothing, None, &search)),
            ("c".into(), c.clone())
        );
    }

    #[test]
    fn a_missing_explicit_or_env_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let c = write(dir.path(), "c.toml", "flake = \"c\"\n");
        let missing = dir.path().join("missing.toml");
        let want = format!("can't read config file {}: ", missing.display());
        let error = load_from(Some(&missing), None, std::slice::from_ref(&c)).unwrap_err();
        assert!(format!("{error:#}").starts_with(&want), "{error:#}");
        let error = load_from(None, Some(missing.as_os_str()), &[c]).unwrap_err();
        assert!(format!("{error:#}").starts_with(&want), "{error:#}");
    }

    #[test]
    fn load_takes_the_first_existing_search_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope").join("config.toml");
        let file = write(dir.path(), "file", "");
        let under_a_file = file.join("config.toml");
        let b = write(dir.path(), "b.toml", "flake = \"b\"\n");
        let c = write(dir.path(), "c.toml", "flake = \"c\"\n");
        let search = [missing.clone(), under_a_file, b.clone(), c];
        assert_eq!(loaded(load_from(None, None, &search)), ("b".into(), b));
        let nothing = load_from(None, None, &[missing]).unwrap();
        assert_eq!(nothing, (Config::default(), None));
        assert_eq!(
            load_from(None, None, &[]).unwrap(),
            (Config::default(), None)
        );
    }

    #[test]
    fn load_reports_a_found_file_it_cant_use() {
        let dir = tempfile::tempdir().unwrap();
        // A directory where the file should be exists, but can't be read.
        let error = load_from(None, None, &[dir.path().to_path_buf()]).unwrap_err();
        let want = format!("can't read config file {}: ", dir.path().display());
        assert!(format!("{error:#}").starts_with(&want), "{error:#}");
        let bad = write(dir.path(), "bad.toml", "[hosts.mac]\nprofile = \"p\"\n");
        let error = load_from(None, None, std::slice::from_ref(&bad)).unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            format!(
                "invalid config file {}: host \"mac\": profile \"p\" must be absolute or start with ~/",
                bad.display()
            )
        );
    }

    // ── overrides, validation, serialization ───────────────────────────

    #[test]
    fn overrides_win_over_the_file() {
        let file = "flake = \"github:jjant/nix-config\"\njobs = 4\nself = \"bigmem\"\n";
        let mut config = parse(file);
        config.apply(&Overrides::default()).unwrap();
        assert_eq!(config, parse(file));
        config
            .apply(&Overrides {
                flake: Some("path:/src/nix-config".into()),
                jobs: Some(2),
                self_host: Some("mac-m1".into()),
                no_cache: true,
            })
            .unwrap();
        let applied = config.clone();
        assert_eq!(applied.flake.as_deref(), Some("path:/src/nix-config"));
        assert_eq!((applied.jobs, applied.jobs()), (Some(2), 2));
        assert_eq!(applied.self_host.as_deref(), Some("mac-m1"));
        assert!(!applied.cache);
        // Empty values (an exported but empty $HOSTCTL_FLAKE) and `-j 0`
        // count as not given; `no_cache: false` doesn't turn the cache on.
        config
            .apply(&Overrides {
                flake: Some(String::new()),
                jobs: Some(0),
                self_host: Some(String::new()),
                no_cache: false,
            })
            .unwrap();
        assert_eq!(config, applied);
    }

    #[test]
    fn overrides_are_checked_like_the_file() {
        let file = "flake = \"github:jjant/nix-config\"\n";
        let mut config = parse(file);
        let error = config
            .apply(&Overrides {
                flake: Some("--impure".into()),
                jobs: Some(2),
                ..Overrides::default()
            })
            .unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            "--flake (or $HOSTCTL_FLAKE) \"--impure\" must not start with '-' \
             (nix would read it as an option)"
        );
        assert_eq!(config, parse(file), "nothing is applied");
    }

    #[test]
    fn jobs_default_to_the_machine_and_stay_in_range() {
        let config = Config::default();
        assert_eq!(config.jobs(), default_jobs());
        assert!(default_jobs() >= 1);
        let jobs = |jobs| {
            Config {
                jobs,
                ..Config::default()
            }
            .jobs()
        };
        assert_eq!(jobs(Some(0)), 1);
        assert_eq!(jobs(Some(12)), 12);
        assert_eq!(jobs(Some(usize::MAX)), MAX_JOBS);
        let huge = parse("jobs = 9223372036854775807");
        assert_eq!(huge.jobs(), MAX_JOBS);
    }

    #[test]
    fn validate_checks_global_settings() {
        let cases = [
            ("timeout = 0", "timeout must be greater than 0"),
            ("timeout = \"0ms\"", "timeout must be greater than 0"),
            ("jobs = 0", "jobs must be at least 1"),
            ("[nix]\ncommand = \"\"", "nix.command must not be empty"),
            ("[ssh]\ncommand = \"\"", "ssh.command must not be empty"),
            ("flake = \"\"", "flake must not be empty"),
            (
                "flake = \"--impure\"",
                "flake \"--impure\" must not start with '-' (nix would read it as an option)",
            ),
            ("self = \"\"", "self must not be empty"),
            (
                "exclude = [\"*-iso\", \"\"]",
                "exclude must not contain empty patterns",
            ),
        ];
        for (toml, want) in cases {
            assert_eq!(parse_error(toml), want, "{toml:?}");
        }
        // Zero means "off" everywhere else.
        parse("eval-timeout = 0\n[nix]\nrefresh = 0\n[ssh]\npersist = 0\n");
    }

    #[test]
    fn validate_names_the_host_and_field() {
        let cases = [
            (
                "destination = \"\"",
                "host \"web1\": destination must not be empty",
            ),
            ("flake = \"\"", "host \"web1\": flake must not be empty"),
            (
                "flake = \"-L\"",
                "host \"web1\": flake \"-L\" must not start with '-' (nix would read it as an option)",
            ),
            ("attr = \"\"", "host \"web1\": attr must not be empty"),
            ("probe = \"\"", "host \"web1\": probe must not be empty"),
            (
                "attr = \".packages.x86_64-linux.default\"",
                "host \"web1\": attr \".packages.x86_64-linux.default\" must not start with '.' or '#' (it is always absolute)",
            ),
            (
                "attr = \"#nixosConfigurations.web1\"",
                "host \"web1\": attr \"#nixosConfigurations.web1\" must not start with '.' or '#' (it is always absolute)",
            ),
            (
                "activate = true",
                "host \"web1\": activate = true is not valid; set a command, or false to disable activation",
            ),
            (
                "profile = \"nix/profile\"",
                "host \"web1\": profile \"nix/profile\" must be absolute or start with ~/",
            ),
            (
                "profile = \"~\"",
                "host \"web1\": profile \"~\" must be absolute or start with ~/",
            ),
            (
                "profile = \"$HOME/.nix-profile\"",
                "host \"web1\": profile \"$HOME/.nix-profile\" must be absolute or start with ~/",
            ),
            (
                "destination = \"web 1\"",
                "host \"web1\": invalid ssh destination \"web 1\": contains whitespace",
            ),
            (
                "destination = \"-oProxyCommand=x\"",
                "host \"web1\": invalid ssh destination \"-oProxyCommand=x\": starts with '-'",
            ),
            (
                "destination = \"ssh://{name}:0\"",
                "host \"web1\": invalid ssh destination \"ssh://web1:0\": bad port",
            ),
        ];
        for (line, want) in cases {
            assert_eq!(
                parse_error(&format!("[hosts.web1]\n{line}\n")),
                want,
                "{line:?}"
            );
        }
    }

    #[test]
    fn validate_checks_defaults_with_a_placeholder_name() {
        parse("[defaults]\ndestination = \"root@{name}.tailnet.example\"\n");
        let cases = [
            (
                "destination = \"{name} x\"",
                "[defaults]: invalid ssh destination \"name x\": contains whitespace",
            ),
            (
                "destination = \"-{name}\"",
                "[defaults]: invalid ssh destination \"-name\": starts with '-'",
            ),
            (
                "activate = true",
                "[defaults]: activate = true is not valid; set a command, or false to disable activation",
            ),
            (
                "profile = \"relative\"",
                "[defaults]: profile \"relative\" must be absolute or start with ~/",
            ),
            (
                "flake = \"--impure\"",
                "[defaults]: flake \"--impure\" must not start with '-' (nix would read it as an option)",
            ),
        ];
        for (line, want) in cases {
            assert_eq!(
                parse_error(&format!("[defaults]\n{line}\n")),
                want,
                "{line:?}"
            );
        }
    }

    #[test]
    fn validate_escapes_and_checks_names() {
        assert_eq!(
            parse_error("[hosts.\"\"]\nkind = \"nixos\"\n"),
            "host \"\": the name must not be empty"
        );
        // Control characters can't reach the terminal through a message.
        assert_eq!(
            parse_error("[hosts.\"evil\\u001b]0;x\"]\nflake = \"\"\n"),
            "host \"evil\\u{1b}]0;x\": flake must not be empty"
        );
    }

    #[test]
    fn validate_custom_hosts_counting_defaults() {
        assert_eq!(
            parse_error("[hosts.r]\nkind = \"custom\"\n"),
            "2 problems:\n  host \"r\": kind = \"custom\" needs attr\n  host \"r\": kind = \"custom\" needs profile (or probe)"
        );
        let attr = "attr = \"packages.x86_64-linux.r\"";
        parse(&format!(
            "[hosts.r]\nkind = \"custom\"\n{attr}\nprofile = \"/nix/var/nix/profiles/r\"\n"
        ));
        parse(&format!(
            "[hosts.r]\nkind = \"custom\"\n{attr}\nprobe = \"current-system\"\n"
        ));
        parse(&format!(
            "[defaults]\nprofile = \"~/.nix-profile\"\n[hosts.r]\nkind = \"custom\"\n{attr}\n"
        ));
        assert_eq!(
            parse_error("[defaults]\nkind = \"custom\"\nprofile = \"/p\"\n[hosts.r]\n"),
            "host \"r\": kind = \"custom\" needs attr"
        );
        // [defaults] alone isn't a host; a host can choose another kind.
        parse("[defaults]\nkind = \"custom\"\n[hosts.r]\nkind = \"nixos\"\n");
    }

    #[test]
    fn validate_reports_every_problem() {
        assert_eq!(
            parse_error("timeout = 0\n[hosts.a]\nprofile = \"p\"\n[hosts.b]\nactivate = true\n"),
            "3 problems:\n  timeout must be greater than 0\n  host \"a\": profile \"p\" must be absolute or start with ~/\n  host \"b\": activate = true is not valid; set a command, or false to disable activation"
        );
    }

    #[test]
    fn validate_accepts_valid_configs() {
        parse(EXAMPLE);
        rich().validate().unwrap();
        parse(
            r#"
            [hosts.web1]
            activate = false
            [hosts.web2]
            activate = ""
            profile = "~/.local/state/nix/profiles/home-manager"
            [hosts.web3]
            profile = "/nix/var/nix/profiles/system"
            destination = "ssh://root@[fe80::1]:2222"
            "#,
        );
    }

    #[test]
    fn merge_overlays_only_what_is_set() {
        let full = full_host();
        let mut merged = HostConfig::default();
        merged.merge(&full);
        assert_eq!(merged, full);
        merged.merge(&HostConfig::default());
        assert_eq!(merged, full);
        let patch = HostConfig {
            destination: Some("other".into()),
            optional: Some(false),
            hostnames: Some(Vec::new()),
            activate: Some(ActivateSetting::Command("x".into())),
            ..HostConfig::default()
        };
        merged.merge(&patch);
        assert_eq!(
            merged,
            HostConfig {
                destination: Some("other".into()),
                optional: Some(false),
                hostnames: Some(Vec::new()),
                activate: Some(ActivateSetting::Command("x".into())),
                ..full
            }
        );
        assert!(HostConfig::default().is_empty() && !patch.is_empty());
    }

    #[test]
    fn to_toml_round_trips() {
        let config = rich();
        let toml = config.to_toml();
        assert_eq!(parse(&toml), config, "{toml}");
        assert_eq!(parse(&parse(EXAMPLE).to_toml()), parse(EXAMPLE));
        assert_eq!(parse(&Config::default().to_toml()), Config::default());
        // Both forms of `activate`, unusual names and strings survive.
        assert!(toml.contains("activate = false"), "{toml}");
        assert!(toml.contains("[hosts.\"jj@mac\"]"), "{toml}");
        assert!(toml.contains("[hosts.empty]"), "{toml}");
    }

    #[test]
    fn to_toml_shows_the_effective_defaults() {
        assert_eq!(
            Config::default().to_toml(),
            "timeout = \"20s\"\neval-timeout = \"10m\"\ndiscover = true\n\
             inventory = \"lib.hostctl\"\ncache = true\n\n\
             [nix]\ncommand = \"nix\"\noptions = []\nread-only = true\nrefresh = \"1m\"\n\n\
             [ssh]\ncommand = \"ssh\"\noptions = []\npersist = \"10m\"\n"
        );
    }

    // ── resolution ─────────────────────────────────────────────────────

    #[test]
    fn discovery_adds_the_flake_configurations() {
        let config = parse("flake = \"github:jjant/nix-config\"");
        let resolved = resolve(&config, Some(&nix_config()), &elsewhere());
        assert_eq!(names(&resolved), ALL_HOSTS);
        assert!(resolved.warnings.is_empty());
        assert_eq!(
            *host(&resolved, "bigmem"),
            Host {
                name: "bigmem".into(),
                kind: Kind::HomeManager,
                flake: "github:jjant/nix-config".into(),
                attr: "homeConfigurations.bigmem.activationPackage".into(),
                target: Target::Ssh(ssh_target("bigmem", None, "bigmem", None)),
                profile: None,
                activate: Kind::HomeManager.default_activate().map(str::to_owned),
                activate_on: ActivateOn::Target,
                optional: false,
                note: None,
                sources: Sources {
                    discovered: true,
                    inventory: false,
                    configured: false,
                },
            }
        );
        let mac = host(&resolved, "mac-m1");
        assert_eq!(mac.kind, Kind::Darwin);
        assert_eq!(mac.attr, "darwinConfigurations.mac-m1.system");
        assert_eq!(mac.activate.as_deref(), Kind::Darwin.default_activate());
    }

    #[test]
    fn discovery_off_ignores_discovered_names_and_kinds() {
        let config =
            parse("flake = \"f\"\ndiscover = false\n[hosts.bigmem]\nkind = \"home-manager\"\n");
        let resolved = resolve(&config, Some(&nix_config()), &elsewhere());
        assert_eq!(names(&resolved), ["bigmem"]);
        assert_eq!(
            host(&resolved, "bigmem").sources,
            Sources {
                discovered: false,
                inventory: false,
                configured: true,
            }
        );
        let config = parse("flake = \"f\"\ndiscover = false\n[hosts.bigmem]\n");
        assert_eq!(
            resolve_error(&config, Some(&nix_config()), &elsewhere()),
            "host \"bigmem\": unknown kind; set kind = \"nixos\", \"darwin\", \"home-manager\" or \"custom\" in [hosts.bigmem]"
        );
    }

    #[test]
    fn without_flake_hosts_only_configured_hosts_exist() {
        let config = parse("flake = \"f\"\n[hosts.web1]\nkind = \"nixos\"\n");
        let resolved = resolve(&config, None, &elsewhere());
        assert_eq!(names(&resolved), ["web1"]);
        assert_eq!(
            host(&resolved, "web1").attr,
            "nixosConfigurations.web1.config.system.build.toplevel"
        );
        assert_eq!(
            resolve(&parse("flake = \"f\""), None, &elsewhere()),
            Resolved::default()
        );
    }

    #[test]
    fn inventory_sits_between_discovery_and_the_config() {
        let mut flake_hosts = nix_config();
        flake_hosts.inventory.insert(
            "mac-m1".into(),
            HostConfig {
                destination: Some("ssh://devuser1@localhost:2022".into()),
                optional: Some(true),
                note: Some("from the flake".into()),
                ..HostConfig::default()
            },
        );
        flake_hosts.inventory.insert(
            "web1".into(),
            HostConfig {
                kind: Some(Kind::Nixos),
                destination: Some("root@web1.example.com".into()),
                note: Some("from the flake".into()),
                ..HostConfig::default()
            },
        );
        let config = parse(
            r#"
            flake = "github:jjant/nix-config"
            [defaults]
            note = "from [defaults]"
            [hosts.mac-m1]
            probe = "current-system"
            [hosts.web1]
            destination = "root@10.0.0.1"
            "#,
        );
        let resolved = resolve(&config, Some(&flake_hosts), &elsewhere());
        let mut want: Vec<&str> = ALL_HOSTS.to_vec();
        want.push("web1");
        assert_eq!(names(&resolved), want);

        let mac = host(&resolved, "mac-m1");
        assert_eq!(mac.kind, Kind::Darwin);
        assert!(mac.optional);
        assert_eq!(mac.note.as_deref(), Some("from [defaults]"));
        assert_eq!(
            *ssh(&resolved, "mac-m1"),
            SshTarget {
                forced_command: Some("current-system".into()),
                ..ssh_target(
                    "ssh://devuser1@localhost:2022",
                    Some("devuser1"),
                    "localhost",
                    Some(2022)
                )
            }
        );
        assert_eq!(
            mac.sources,
            Sources {
                discovered: true,
                inventory: true,
                configured: true,
            }
        );

        let web1 = host(&resolved, "web1");
        assert_eq!(web1.kind, Kind::Nixos);
        assert_eq!(
            *ssh(&resolved, "web1"),
            ssh_target("root@10.0.0.1", Some("root"), "10.0.0.1", None)
        );
        assert_eq!(
            web1.sources,
            Sources {
                discovered: false,
                inventory: true,
                configured: true,
            }
        );
    }

    #[test]
    fn defaults_destination_is_a_template() {
        let config = parse(
            r#"
            flake = "f"
            [defaults]
            destination = "jj@{name}.tailnet.example"
            ssh-options = ["-4"]
            [hosts.bigmem]
            destination = "ssh://{name}.lan:2222"
            "#,
        );
        let resolved = resolve(&config, Some(&nix_config()), &elsewhere());
        assert_eq!(
            *ssh(&resolved, "al2-x86_64"),
            SshTarget {
                options: strings(&["-4"]),
                ..ssh_target(
                    "jj@al2-x86_64.tailnet.example",
                    Some("jj"),
                    "al2-x86_64.tailnet.example",
                    None
                )
            }
        );
        assert_eq!(
            *ssh(&resolved, "bigmem"),
            SshTarget {
                options: strings(&["-4"]),
                ..ssh_target("ssh://bigmem.lan:2222", None, "bigmem.lan", Some(2222))
            }
        );
    }

    #[test]
    fn host_tables_override_every_layer() {
        let mut flake_hosts = nix_config();
        flake_hosts.inventory.insert(
            "bigmem".into(),
            HostConfig {
                destination: Some("inventory.example".into()),
                optional: Some(false),
                activate: Some(ActivateSetting::Enabled(false)),
                ..HostConfig::default()
            },
        );
        let config = parse(
            r#"
            flake = "github:jjant/nix-config"
            [defaults]
            destination = "{name}.defaults.example"
            note = "from [defaults]"
            [hosts.bigmem]
            kind = "nixos"
            flake = "path:/home/devuser1/personal/nix-config"
            attr = "nixosConfigurations.big.config.system.build.toplevel"
            destination = "table.example"
            profile = "/nix/var/nix/profiles/system"
            activate = "colmena apply --on {name}"
            activate-on = "local"
            hostnames = ["big"]
            ssh-options = ["-p", "2200"]
            optional = true
            note = "from the table"
            "#,
        );
        let resolved = resolve(&config, Some(&flake_hosts), &elsewhere());
        assert!(resolved.warnings.is_empty());
        assert_eq!(
            *host(&resolved, "bigmem"),
            Host {
                name: "bigmem".into(),
                kind: Kind::Nixos,
                flake: "path:/home/devuser1/personal/nix-config".into(),
                attr: "nixosConfigurations.big.config.system.build.toplevel".into(),
                target: Target::Ssh(SshTarget {
                    options: strings(&["-p", "2200"]),
                    ..ssh_target("table.example", None, "table.example", None)
                }),
                profile: Some("/nix/var/nix/profiles/system".into()),
                activate: Some("colmena apply --on {name}".into()),
                activate_on: ActivateOn::Local,
                optional: true,
                note: Some("from the table".into()),
                sources: Sources {
                    discovered: true,
                    inventory: true,
                    configured: true,
                },
            }
        );
        // Its hostnames are used to recognize this machine.
        let resolved = resolve(
            &config,
            Some(&flake_hosts),
            &machine("big", "jj", Os::Linux),
        );
        assert_eq!(local_names(&resolved), ["bigmem"]);
    }

    #[test]
    fn exclude_drops_hosts_from_every_source() {
        let mut flake_hosts = discovered(&[
            ("installer-iso", Kind::Nixos),
            ("web1", Kind::Nixos),
            ("web2", Kind::Nixos),
            ("ci-1", Kind::HomeManager),
        ]);
        // Excluded hosts aren't checked: this one has no kind.
        flake_hosts
            .inventory
            .insert("ci-2".into(), HostConfig::default());
        let config = parse(
            r#"
            flake = "f"
            exclude = ["*-iso", "ci-[0-9]", "web[!1]"]
            [hosts.ci-3]
            kind = "nixos"
            "#,
        );
        let resolved = resolve(&config, Some(&flake_hosts), &elsewhere());
        assert_eq!(names(&resolved), ["web1"]);
    }

    #[test]
    fn a_name_in_several_outputs_is_nixos_then_darwin_then_home_manager() {
        let flake_hosts = discovered(&[
            ("bigmem", Kind::HomeManager),
            ("bigmem", Kind::Nixos),
            ("mac", Kind::HomeManager),
            ("mac", Kind::Darwin),
            ("all", Kind::HomeManager),
            ("all", Kind::Darwin),
            ("all", Kind::Nixos),
            ("twice", Kind::HomeManager),
            ("twice", Kind::HomeManager),
        ]);
        let resolved = resolve(&parse("flake = \"f\""), Some(&flake_hosts), &elsewhere());
        let kinds: Vec<(&str, Kind)> = resolved
            .hosts
            .iter()
            .map(|h| (h.name.as_str(), h.kind))
            .collect();
        assert_eq!(
            kinds,
            [
                ("all", Kind::Nixos),
                ("bigmem", Kind::Nixos),
                ("mac", Kind::Darwin),
                ("twice", Kind::HomeManager),
            ]
        );
        assert_eq!(
            resolved.warnings,
            [
                "host \"all\" is in nixosConfigurations, darwinConfigurations and homeConfigurations; checking it as nixos (set kind in [hosts.all] to choose)",
                "host \"bigmem\" is in nixosConfigurations and homeConfigurations; checking it as nixos (set kind in [hosts.bigmem] to choose)",
                "host \"mac\" is in darwinConfigurations and homeConfigurations; checking it as darwin (set kind in [hosts.mac] to choose)",
            ]
        );
        // An explicit kind decides, without a warning.
        let config = parse("flake = \"f\"\n[hosts.bigmem]\nkind = \"home-manager\"\n");
        let resolved = resolve(&config, Some(&flake_hosts), &elsewhere());
        let bigmem = host(&resolved, "bigmem");
        assert_eq!(bigmem.kind, Kind::HomeManager);
        assert_eq!(bigmem.attr, "homeConfigurations.bigmem.activationPackage");
        assert_eq!(resolved.warnings.len(), 2);
        assert!(!resolved.warnings.iter().any(|w| w.contains("\"bigmem\"")));
    }

    #[test]
    fn kind_is_required_unless_attr_makes_the_host_custom() {
        let config = parse("flake = \"f\"\n[hosts.web1]\ndestination = \"web1.example.com\"\n");
        assert_eq!(
            resolve_error(&config, None, &elsewhere()),
            "host \"web1\": unknown kind; set kind = \"nixos\", \"darwin\", \"home-manager\" or \"custom\" in [hosts.web1]"
        );
        let config = parse(
            r#"
            flake = "f"
            [hosts."router 1"]
            attr = "packages.aarch64-linux.router"
            profile = "/nix/var/nix/profiles/router"
            destination = "root@192.168.1.1"
            "#,
        );
        let resolved = resolve(&config, None, &elsewhere());
        let router = host(&resolved, "router 1");
        assert_eq!(router.kind, Kind::Custom);
        assert_eq!(router.attr, "packages.aarch64-linux.router");
        assert_eq!(router.activate, None);
    }

    #[test]
    fn every_host_needs_a_flake() {
        let config = parse("[hosts.web1]\nkind = \"nixos\"\n");
        assert_eq!(
            resolve_error(&config, None, &elsewhere()),
            "no flake for host \"web1\": set flake at the top of the config or in [hosts.web1], or pass --flake"
        );
        for toml in [
            "[hosts.web1]\nkind = \"nixos\"\nflake = \"github:o/infra\"\n",
            "[defaults]\nflake = \"github:o/infra\"\n[hosts.web1]\nkind = \"nixos\"\n",
            "flake = \"github:o/infra\"\n[hosts.web1]\nkind = \"nixos\"\n",
        ] {
            let resolved = resolve(&parse(toml), None, &elsewhere());
            assert_eq!(host(&resolved, "web1").flake, "github:o/infra", "{toml}");
        }
    }

    #[test]
    fn which_names_double_as_destinations() {
        for name in [
            "web1",
            "web1.example.com",
            "_build",
            "9lives",
            "a-b_c.d",
            "jj@mac",
            "jj@mac.local",
            "jj.doe@mac",
        ] {
            assert!(name_is_destination(name), "{name:?}");
        }
        for name in [
            "", "-web1", ".hidden", "web 1", "web1:22", "a@b@c", "jj@", "@mac", "ünï", "web1/x",
            "x;rm", "[::1]",
        ] {
            assert!(!name_is_destination(name), "{name:?}");
        }
    }

    #[test]
    fn hosts_reached_over_ssh_need_a_destination() {
        let flake_hosts = discovered(&[
            ("jj@mac", Kind::HomeManager),
            ("x86_64-linux build", Kind::Nixos),
        ]);
        let config = parse("flake = \"f\"");
        assert_eq!(
            resolve_error(&config, Some(&flake_hosts), &elsewhere()),
            "host \"x86_64-linux build\": its name isn't an ssh destination; set destination in [hosts.\"x86_64-linux build\"] (or exclude it)"
        );
        // Unless it is this machine, has a destination, or is excluded.
        let here = forced(elsewhere(), "x86_64-linux build");
        let resolved = resolve(&config, Some(&flake_hosts), &here);
        assert_eq!(
            host(&resolved, "x86_64-linux build").target,
            Target::Local { assumed: false }
        );
        assert_eq!(
            *ssh(&resolved, "jj@mac"),
            ssh_target("jj@mac", Some("jj"), "mac", None)
        );
        for toml in [
            "flake = \"f\"\n[hosts.\"x86_64-linux build\"]\ndestination = \"builder\"\n",
            "flake = \"f\"\nexclude = [\"* build\"]\n",
        ] {
            resolve(&parse(toml), Some(&flake_hosts), &elsewhere());
        }
        // A safe-looking name ssh would still read as an option is an error.
        let flake_hosts = discovered(&[("jj@-mac", Kind::HomeManager)]);
        assert_eq!(
            resolve_error(&config, Some(&flake_hosts), &elsewhere()),
            "host \"jj@-mac\": invalid ssh destination \"jj@-mac\": host starts with '-'"
        );
    }

    #[test]
    fn inventory_values_are_checked_like_the_config() {
        let mut flake_hosts = FlakeHosts::default();
        flake_hosts.inventory.insert(
            "web1".into(),
            HostConfig {
                kind: Some(Kind::Nixos),
                destination: Some("-oProxyCommand=evil".into()),
                profile: Some("relative/profile".into()),
                activate: Some(ActivateSetting::Enabled(true)),
                ..HostConfig::default()
            },
        );
        flake_hosts.inventory.insert(
            "web2".into(),
            HostConfig {
                kind: Some(Kind::Custom),
                ..HostConfig::default()
            },
        );
        flake_hosts.inventory.insert(
            "web3".into(),
            HostConfig {
                kind: Some(Kind::Nixos),
                flake: Some("--impure".into()),
                ..HostConfig::default()
            },
        );
        assert_eq!(
            resolve_error(&parse("flake = \"f\""), Some(&flake_hosts), &elsewhere()),
            "6 problems:\n  \
             host \"web1\": activate = true is not valid; set a command, or false to disable activation\n  \
             host \"web1\": profile \"relative/profile\" must be absolute or start with ~/\n  \
             host \"web1\": invalid ssh destination \"-oProxyCommand=evil\": starts with '-'\n  \
             host \"web2\": kind = \"custom\" needs attr\n  \
             host \"web2\": kind = \"custom\" needs profile (or probe)\n  \
             host \"web3\": flake \"--impure\" must not start with '-' (nix would read it as an option)"
        );
    }

    #[test]
    fn probe_is_the_forced_command_of_an_ssh_route() {
        let config = parse(
            r#"
            flake = "f"
            [hosts.mac-m1]
            kind = "darwin"
            destination = "ssh://devuser1@localhost:2022"
            probe = "current-system"
            ssh-options = ["-o", "ServerAliveInterval=5"]
            "#,
        );
        let resolved = resolve(&config, None, &on_bigmem());
        assert_eq!(
            *ssh(&resolved, "mac-m1"),
            SshTarget {
                options: strings(&["-o", "ServerAliveInterval=5"]),
                forced_command: Some("current-system".into()),
                ..ssh_target(
                    "ssh://devuser1@localhost:2022",
                    Some("devuser1"),
                    "localhost",
                    Some(2022)
                )
            }
        );
        assert_eq!(
            host(&resolved, "mac-m1").reached_via(),
            "ssh://devuser1@localhost:2022"
        );
        // This machine ignores both: no ssh at all.
        for (local, assumed) in [(on_the_mac(), true), (forced(on_bigmem(), "mac-m1"), false)] {
            let resolved = resolve(&config, None, &local);
            let mac = host(&resolved, "mac-m1");
            assert_eq!(mac.target, Target::Local { assumed });
            assert_eq!(mac.ssh(), None);
            let via = if assumed { "local (assumed)" } else { "local" };
            assert_eq!(mac.reached_via(), via);
        }
    }

    #[test]
    fn activate_is_a_command_the_kind_default_or_off() {
        let config = parse(
            r#"
            flake = "f"
            [hosts.off]
            kind = "nixos"
            activate = false
            [hosts.empty]
            kind = "nixos"
            activate = ""
            [hosts.nixos]
            kind = "nixos"
            [hosts.darwin]
            kind = "darwin"
            [hosts.home]
            kind = "home-manager"
            [hosts.command]
            kind = "home-manager"
            activate = "nix run {flake}#activate"
            [hosts.custom]
            kind = "custom"
            attr = "packages.x86_64-linux.default"
            profile = "/nix/var/nix/profiles/custom"
            "#,
        );
        let resolved = resolve(&config, None, &elsewhere());
        let activate = |name: &str| host(&resolved, name).activate.clone();
        assert_eq!(activate("off"), None);
        assert_eq!(activate("empty"), None);
        assert_eq!(
            activate("nixos").as_deref(),
            Some("/run/current-system/sw/bin/nixos-rebuild switch --flake {flake}#{name} --sudo")
        );
        assert_eq!(
            activate("darwin").as_deref(),
            Some("sudo /run/current-system/sw/bin/darwin-rebuild switch --flake {flake}#{name}")
        );
        assert_eq!(
            activate("home").as_deref(),
            Kind::HomeManager.default_activate()
        );
        assert_eq!(
            activate("command").as_deref(),
            Some("nix run {flake}#activate")
        );
        assert_eq!(activate("custom"), None);
        // `true` names no command.
        let mut flake_hosts = FlakeHosts::default();
        flake_hosts.inventory.insert(
            "web1".into(),
            HostConfig {
                kind: Some(Kind::Nixos),
                activate: Some(ActivateSetting::Enabled(true)),
                ..HostConfig::default()
            },
        );
        assert_eq!(
            resolve_error(&parse("flake = \"f\""), Some(&flake_hosts), &elsewhere()),
            "host \"web1\": activate = true is not valid; set a command, or false to disable activation"
        );
    }

    #[test]
    fn profiles_under_home_are_rewritten_for_the_probe() {
        let config = parse(
            r#"
            flake = "f"
            [hosts.home]
            kind = "home-manager"
            profile = "~/.local/state/nix/profiles/home-manager"
            [hosts.system]
            kind = "nixos"
            profile = "/nix/var/nix/profiles/system"
            [hosts.default]
            kind = "nixos"
            "#,
        );
        let resolved = resolve(&config, None, &elsewhere());
        let profile = |name: &str| host(&resolved, name).profile.clone();
        assert_eq!(
            profile("home").as_deref(),
            Some("$HOME/.local/state/nix/profiles/home-manager")
        );
        assert_eq!(
            profile("system").as_deref(),
            Some("/nix/var/nix/profiles/system")
        );
        assert_eq!(profile("default"), None);
    }

    #[test]
    fn hosts_are_sorted_by_name_whatever_their_source() {
        let mut flake_hosts = discovered(&[("zeta", Kind::Nixos), ("alpha", Kind::Nixos)]);
        flake_hosts.inventory.insert(
            "mid".into(),
            HostConfig {
                kind: Some(Kind::Nixos),
                ..HostConfig::default()
            },
        );
        let config = parse(
            "flake = \"f\"\n[hosts.beta]\nkind = \"nixos\"\n[hosts.Alpha]\nkind = \"nixos\"\n",
        );
        let resolved = resolve(&config, Some(&flake_hosts), &elsewhere());
        assert_eq!(names(&resolved), ["Alpha", "alpha", "beta", "mid", "zeta"]);
        let sources: Vec<Sources> = resolved.hosts.iter().map(|h| h.sources).collect();
        let s = |discovered, inventory, configured| Sources {
            discovered,
            inventory,
            configured,
        };
        assert_eq!(
            sources,
            [
                s(false, false, true),
                s(true, false, false),
                s(false, false, true),
                s(false, true, false),
                s(true, false, false),
            ]
        );
    }

    #[test]
    fn self_forces_the_local_host() {
        let config = parse(EXAMPLE);
        // As the CLI passes it: --self, $HOSTCTL_SELF or the config's self.
        let resolved = resolve(&config, None, &forced(on_bigmem(), "al2-x86_64"));
        assert_eq!(local_names(&resolved), ["al2-x86_64"]);
        // The config's `self` applies when the caller didn't pass one…
        let mut config = parse(EXAMPLE);
        config.self_host = Some("al2023-x86_64".into());
        let resolved = resolve(&config, None, &elsewhere());
        assert_eq!(local_names(&resolved), ["al2023-x86_64"]);
        // …and loses to one it did.
        let resolved = resolve(&config, None, &forced(elsewhere(), "al2-x86_64"));
        assert_eq!(local_names(&resolved), ["al2-x86_64"]);
        assert_eq!(
            resolve_error(&config, None, &forced(on_bigmem(), "bigmen")),
            "unknown host \"bigmen\" in --self, HOSTCTL_SELF or self \
             (hosts: al2-aarch64, al2-x86_64, al2023-x86_64, bigmem, mac-m1)"
        );
        // An excluded host is no host.
        let mut config = parse(EXAMPLE);
        config.exclude.push("bigmem".into());
        assert!(
            resolve_error(&config, None, &forced(on_bigmem(), "bigmem"))
                .starts_with("unknown host \"bigmem\"")
        );
    }

    #[test]
    fn custom_hosts_need_an_attr_and_something_to_read() {
        let config = parse(
            "flake = \"f\"\n[hosts.r]\nattr = \"packages.x86_64-linux.r\"\ndestination = \"r.lan\"\n",
        );
        assert_eq!(
            resolve_error(&config, None, &elsewhere()),
            "host \"r\": kind = \"custom\" needs profile (or probe)"
        );
        // Over ssh a probe will do; on this machine only a profile can.
        let config = parse(
            "flake = \"f\"\n[hosts.r]\nattr = \"packages.x86_64-linux.r\"\ndestination = \"r.lan\"\nprobe = \"current-system\"\n",
        );
        let resolved = resolve(&config, None, &elsewhere());
        assert_eq!(
            ssh(&resolved, "r").forced_command.as_deref(),
            Some("current-system")
        );
        assert_eq!(
            resolve_error(&config, None, &machine("r.lan", "runner", Os::Linux)),
            "host \"r\": kind = \"custom\" needs profile on this machine (probe only applies over ssh)"
        );
        let mut config = config;
        config.hosts.get_mut("r").unwrap().profile = Some("/nix/var/nix/profiles/r".into());
        let resolved = resolve(&config, None, &machine("r.lan", "runner", Os::Linux));
        assert_eq!(local_names(&resolved), ["r"]);
    }

    #[test]
    fn resolution_reports_every_problem() {
        let config = parse("[hosts.a]\nkind = \"nixos\"\n[hosts.b]\n");
        assert_eq!(
            resolve_error(&config, None, &elsewhere()),
            "3 problems:\n  \
             no flake for host \"a\": set flake at the top of the config or in [hosts.a], or pass --flake\n  \
             host \"b\": unknown kind; set kind = \"nixos\", \"darwin\", \"home-manager\" or \"custom\" in [hosts.b]\n  \
             no flake for host \"b\": set flake at the top of the config or in [hosts.b], or pass --flake"
        );
    }

    // ── examples/config.toml ───────────────────────────────────────────

    #[test]
    fn the_example_resolves_on_bigmem() {
        let config = parse(EXAMPLE);
        assert_eq!(config.flake.as_deref(), Some("github:jjant/nix-config"));
        assert_eq!(config.self_host, None);
        assert_eq!(config.exclude, ["*-iso"]);
        assert!(config.defaults.is_empty());
        // With discovery, or with the flake not resolved yet.
        for flake_hosts in [Some(nix_config()), None] {
            let resolved = resolve(&config, flake_hosts.as_ref(), &on_bigmem());
            assert_eq!(names(&resolved), ALL_HOSTS);
            assert!(resolved.warnings.is_empty());
            assert_eq!(local_names(&resolved), ["bigmem"]);
            assert_eq!(
                host(&resolved, "bigmem").target,
                Target::Local { assumed: false }
            );
            let discovered = flake_hosts.is_some();
            for (name, fqdn) in DESKS {
                let desk = host(&resolved, name);
                assert_eq!(desk.kind, Kind::HomeManager);
                assert_eq!(desk.flake, "github:jjant/nix-config");
                assert_eq!(
                    desk.attr,
                    format!("homeConfigurations.{name}.activationPackage")
                );
                assert_eq!(desk.activate.as_deref(), Some("nix run {flake}#activate"));
                assert_eq!(desk.activate_on, ActivateOn::Target);
                assert!(!desk.optional);
                assert_eq!(desk.profile, None);
                assert_eq!(
                    desk.sources,
                    Sources {
                        discovered,
                        inventory: false,
                        configured: true,
                    }
                );
                if name != "bigmem" {
                    assert_eq!(*ssh(&resolved, name), ssh_target(fqdn, None, fqdn, None));
                }
            }
            let mac = host(&resolved, "mac-m1");
            assert_eq!(mac.kind, Kind::Darwin);
            assert_eq!(mac.attr, "darwinConfigurations.mac-m1.system");
            assert_eq!(mac.activate.as_deref(), Kind::Darwin.default_activate());
            assert!(mac.optional);
            assert_eq!(
                mac.note.as_deref(),
                Some("reachable only from a desk you ssh into from the Mac")
            );
            assert_eq!(
                *ssh(&resolved, "mac-m1"),
                SshTarget {
                    forced_command: Some("current-system".into()),
                    ..ssh_target(
                        "ssh://devuser1@localhost:2022",
                        Some("devuser1"),
                        "localhost",
                        Some(2022)
                    )
                }
            );
        }
    }

    #[test]
    fn the_example_elsewhere() {
        let config = parse(EXAMPLE);
        let flake_hosts = nix_config();
        // On the Mac, the Mac is assumed local.
        let resolved = resolve(&config, Some(&flake_hosts), &on_the_mac());
        assert_eq!(local_names(&resolved), ["mac-m1"]);
        assert_eq!(
            host(&resolved, "mac-m1").target,
            Target::Local { assumed: true }
        );
        // On another desk, that desk.
        let desk = machine(DESKS[0].1, "devuser1", Os::Linux);
        let resolved = resolve(&config, Some(&flake_hosts), &desk);
        assert_eq!(local_names(&resolved), ["al2-aarch64"]);
        // On a machine that is none of them, nothing.
        let resolved = resolve(&config, Some(&flake_hosts), &elsewhere());
        assert_eq!(local_names(&resolved), Vec::<&str>::new());
        // A user other than the one the desks are configured for is still
        // the desk's user here: destinations name no user.
        let resolved = resolve(
            &config,
            Some(&flake_hosts),
            &machine(BIGMEM, "root", Os::Linux),
        );
        assert_eq!(local_names(&resolved), ["bigmem"]);
    }

    #[test]
    fn the_example_documents_every_option() {
        // Every key a config can have, from one that sets them all.
        let table: toml::Table = toml::from_str(&rich().to_toml()).unwrap();
        let mut keys = BTreeSet::new();
        for (key, value) in &table {
            match value {
                toml::Value::Table(t) if key == "hosts" => {
                    for host in t.values().filter_map(toml::Value::as_table) {
                        keys.extend(host.keys().cloned());
                    }
                }
                toml::Value::Table(t) => keys.extend(t.keys().cloned()),
                _ => {
                    keys.insert(key.clone());
                }
            }
        }
        assert!(keys.len() >= 25, "{keys:?}");
        for key in keys {
            let documented = EXAMPLE.lines().any(|line| {
                line.trim_start_matches(['#', ' '])
                    .starts_with(&format!("{key} = "))
            });
            assert!(documented, "examples/config.toml doesn't show `{key}`");
        }
    }

    #[test]
    fn inventories_cannot_set_ssh_options_or_probe() {
        let mut flake_hosts = FlakeHosts::default();
        flake_hosts.inventory.insert(
            "web1".into(),
            HostConfig {
                kind: Some(Kind::Nixos),
                destination: Some("root@web1.lan".into()),
                ssh_options: Some(vec!["-oLocalCommand=touch /tmp/pwned".into()]),
                probe: Some("rm -rf ~".into()),
                ..HostConfig::default()
            },
        );
        let config = Config {
            flake: Some("github:o/r".into()),
            ..Config::default()
        };
        let local = LocalInfo {
            hostname: "elsewhere".into(),
            user: "jj".into(),
            os: crate::local::Os::Linux,
            forced: None,
        };
        let resolved = config.resolve_hosts(Some(&flake_hosts), &local).unwrap();
        let Target::Ssh(ssh) = &resolved.hosts[0].target else {
            panic!("expected an ssh target");
        };
        assert!(ssh.options.is_empty());
        assert_eq!(ssh.forced_command, None);
        assert_eq!(
            resolved.warnings,
            [
                "host \"web1\": ignoring ssh-options and probe from the flake's inventory (set them in your config)"
            ]
        );
    }
}
