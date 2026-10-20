//! What hostctl can do to a host beyond looking: activation commands (and
//! what to copy), and the scripts behind closure diffs and remote builds.
//! See DESIGN.md ("Activation", "Closure diff").

use std::fmt::Write as _;

use crate::closure::ClosureEntry;
use crate::config::{ActivateOn, Host, Kind, Target};
use crate::nix::{FEATURES, FlakeInfo};
use crate::remote::{NIX_PRELUDE, sh_quote};
use crate::storepath::StorePath;

/// Cap on a closure listing's captured output.
pub const MAX_CLOSURE_OUTPUT: usize = 64 << 20;

/// Values for an activation template's placeholders.
#[derive(Debug, Clone, Copy)]
pub struct TemplateVars<'a> {
    /// `{flake}`: the locked URL of the compared revision.
    pub locked: &'a str,
    /// `{flake_url}`: the flake as configured.
    pub flake: &'a str,
    /// `{rev}`: the compared revision (may be empty).
    pub rev: &'a str,
    /// `{name}`: the host (configuration) name.
    pub name: &'a str,
    /// `{attr}`: the evaluated attribute.
    pub attr: &'a str,
    /// `{out}`: the desired store path.
    pub out: &'a str,
    /// `{destination}`: the ssh destination as configured (empty for local hosts).
    pub destination: &'a str,
    /// `{ssh_host}`: the destination's host part (empty for local hosts).
    pub ssh_host: &'a str,
}

/// Longest placeholder names first, so `{flake_url}` isn't read as `{flake}`.
const PLACEHOLDERS: [&str; 8] = [
    "destination",
    "flake_url",
    "ssh_host",
    "flake",
    "attr",
    "name",
    "rev",
    "out",
];

/// Replace the known placeholders with their shell-quoted values. Any other
/// braces (`${HOME}`, `{a,b}`) are left alone, so templates are plain shell.
pub fn expand(template: &str, vars: &TemplateVars<'_>) -> String {
    let mut out = String::with_capacity(template.len() + 64);
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let hit = PLACEHOLDERS.iter().find_map(|&key| {
            let tail = after.strip_prefix(key)?.strip_prefix('}')?;
            Some((key, tail))
        });
        match hit {
            Some((key, tail)) => {
                let value = match key {
                    "destination" => vars.destination,
                    "flake_url" => vars.flake,
                    "ssh_host" => vars.ssh_host,
                    "flake" => vars.locked,
                    "attr" => vars.attr,
                    "name" => vars.name,
                    "rev" => vars.rev,
                    _ => vars.out,
                };
                out.push_str(&sh_quote(value));
                rest = tail;
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Where an activation command runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunsOn {
    /// This machine (a local host, or `activate-on = "local"`).
    Here,
    /// The host, over ssh.
    Remote,
}

/// A ready-to-run activation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activation {
    /// The expanded command line.
    pub command: String,
    pub runs_on: RunsOn,
    /// Shown in the confirmation (e.g. a dirty tree builds whatever is on
    /// disk at that moment; an "assumed local" host).
    pub warning: Option<String>,
}

/// The activation of `host` at the resolved `flake` towards `desired`, or
/// why there is none:
/// - the host has no template (`activate = false`, custom kind);
/// - it runs on the target over a forced-command route ("`<name>` only
///   answers its probe command");
/// - it runs on a remote target but the flake isn't
///   [`FlakeInfo::fetchable_remotely`] ("this flake only exists here: use
///   activate-on = \"local\"").
///
/// The warning names everything that makes the result less certain, joined
/// with `; `: a dirty tree, an assumed-local host, a stale resolution, an
/// out-of-date `flake.lock`.
pub fn activation(
    host: &Host,
    flake: &FlakeInfo,
    desired: &StorePath,
) -> Result<Activation, String> {
    let template = host
        .activate
        .as_deref()
        .filter(|t| !t.trim().is_empty())
        .ok_or_else(|| match host.kind {
            Kind::Custom => format!(
                "{} has no activation command: custom hosts need activate = \"…\"",
                host.name
            ),
            _ => format!(
                "activation is disabled for {} (activate = false)",
                host.name
            ),
        })?;
    let runs_on = if runs_here(host) {
        RunsOn::Here
    } else {
        remote_obstacle(host, flake)?;
        RunsOn::Remote
    };
    let ssh = host.ssh();
    let vars = TemplateVars {
        locked: &flake.locked,
        flake: &flake.flake,
        rev: flake.rev.as_deref().unwrap_or(""),
        name: &host.name,
        attr: &host.attr,
        out: desired.as_str(),
        destination: ssh.map_or("", |t| t.destination.as_str()),
        ssh_host: ssh.map_or("", |t| t.host.as_str()),
    };
    Ok(Activation {
        command: expand(template, &vars),
        runs_on,
        warning: warning(host, flake),
    })
}

/// Why a command can't run on `host`'s remote target: a forced-command
/// route has no shell, and the target has to fetch the flake itself.
fn remote_obstacle(host: &Host, flake: &FlakeInfo) -> Result<(), String> {
    if host.ssh().is_some_and(|t| t.forced_command.is_some()) {
        return Err(format!("{} only answers its probe command", host.name));
    }
    if !flake.fetchable_remotely() {
        return Err("this flake only exists here: use activate-on = \"local\"".into());
    }
    Ok(())
}

/// Everything that makes an activation of `flake` on `host` less certain
/// than "exactly the compared generation, on exactly that machine".
fn warning(host: &Host, flake: &FlakeInfo) -> Option<String> {
    let mut warnings = Vec::new();
    if flake.is_dirty() {
        warnings
            .push("the flake is a dirty tree: activation builds whatever is on disk now".into());
    }
    if host.target == (Target::Local { assumed: true }) {
        warnings.push(format!(
            "{name} is only assumed to be this machine (the only darwin host): \
             confirm with self = {name:?}",
            name = host.name
        ));
    }
    if let Some(reason) = &flake.stale {
        warnings.push(format!(
            "the flake's resolution is stale ({reason}): this activates {}, which may not be \
             the latest revision",
            flake.short_rev()
        ));
    }
    if flake.lock_outdated {
        warnings.push(
            "flake.lock is out of date: inputs it lacks are resolved again, so this may not \
             activate the compared generation"
                .to_string(),
        );
    }
    (!warnings.is_empty()).then(|| warnings.join("; "))
}

/// The command that builds (or fetches) `host`'s desired generation on its
/// target without activating it — `nix --extra-experimental-features
/// 'nix-command flakes' build --no-link --print-out-paths
/// --accept-flake-config --no-write-lock-file '<locked>#.<attr>'` — or why
/// it can't (forced-command route; non-fetchable flake on a remote target).
/// Run interactively, so progress is visible, and with the flake's
/// `nixConfig` accepted, so nix doesn't ask about each setting (the
/// generation it builds is the flake's to run anyway). Like activation, it
/// runs here when [`runs_here`].
pub fn build_command(host: &Host, flake: &FlakeInfo) -> Result<String, String> {
    if !runs_here(host) {
        remote_obstacle(host, flake)?;
    }
    let [option, features] = FEATURES;
    Ok(format!(
        "nix {} {} build --no-link --print-out-paths --accept-flake-config --no-write-lock-file {}",
        sh_quote(option),
        sh_quote(features),
        sh_quote(&format!("{}#.{}", flake.locked, host.attr)),
    ))
}

/// The fixed part of [`closure_script`], after the Nix prelude.
///
/// `closure PATH OUT` writes PATH's listing to OUT. It runs as one
/// background job per requested path, so the closures are read
/// concurrently (the time is that of the largest one, not the sum); the
/// jobs write into a private temp dir that is printed in request order.
/// Validity is only checked when `-qR` fails, which is the only way to tell
/// "not in this store" (expected for an unbuilt generation) from a real
/// failure, and saves a round trip on the common path. `xargs` may split
/// the size query into several runs; they run in order, so sizes stay in
/// path order, and `awk` refuses to pair lists of different lengths.
const CLOSURE_SCRIPT: &str = r#"set -u
command -v nix-store >/dev/null 2>&1 || {
  echo "hostctl: nix-store not found on PATH ($PATH)" >&2
  exit 127
}
d=$(mktemp -d "${TMPDIR:-/tmp}/hostctl.XXXXXX") || exit
trap 'rm -rf "$d"' EXIT
trap 'exit 1' HUP INT PIPE TERM
closure() {
  if nix-store -qR "$1" >"$2.p" 2>"$2.e"; then
    xargs nix-store -q --size <"$2.p" >"$2.s" || return
    awk -v root="$1" '
      BEGIN { sizes = ARGV[2]; ARGC = 2; print "@closure " root }
      { if ((getline size < sizes) <= 0 || size !~ /^[0-9]+$/) exit 1; print $0 "\t" size }
      END { if ((getline size < sizes) > 0) exit 1 }
    ' "$2.p" "$2.s" >"$2" && return
    echo "hostctl: nix-store -q --size doesn't line up with the closure of $1" >&2
    return 1
  fi
  missing=$(nix-store --check-validity --print-invalid "$1") || return
  [ -n "$missing" ] || { cat "$2.e" >&2; return 1; }
  printf '@missing %s\n' "$1" >"$2"
}
"#;

/// A POSIX sh script printing, for each of `paths` in order, its closure
/// with NAR sizes, or that it isn't valid in that store:
///
/// ```text
/// @closure /nix/store/…-a
/// /nix/store/…-dep1<TAB>123456
/// …
/// @missing /nix/store/…-b
/// @end
/// ```
///
/// Starts with [`NIX_PRELUDE`]; uses only `nix-store -qR` and `nix-store
/// -q --size` (any Nix version, no experimental features), one size query
/// per closure (via `xargs`, order preserved), and checks that sizes and
/// paths line up; `nix-store --check-validity --print-invalid` tells a
/// missing path from a failure. The closures are computed concurrently
/// (in a private temp dir, removed on exit). Exits non-zero only if
/// `nix-store` is missing or fails unexpectedly, and then prints no `@end`.
pub fn closure_script(paths: &[&StorePath]) -> String {
    let mut script =
        String::with_capacity(NIX_PRELUDE.len() + CLOSURE_SCRIPT.len() + 128 * paths.len() + 16);
    script.push_str(NIX_PRELUDE);
    script.push('\n');
    script.push_str(CLOSURE_SCRIPT);
    for (i, path) in paths.iter().enumerate() {
        let _ = writeln!(
            script,
            "closure {} \"$d/{i}\" & j{i}=$!",
            sh_quote(path.as_str())
        );
    }
    if !paths.is_empty() {
        script.push_str("ok=1\n");
        for i in 0..paths.len() {
            let _ = writeln!(script, "wait \"$j{i}\" || ok=");
        }
        script.push_str("[ -n \"$ok\" ] && cat");
        for i in 0..paths.len() {
            let _ = write!(script, " \"$d/{i}\"");
        }
        script.push_str(" || exit 1\n");
    }
    script.push_str("echo @end\n");
    script
}

/// Per requested path, its closure, or `None` when the path isn't valid in
/// that store.
pub type Closures = Vec<(StorePath, Option<Vec<ClosureEntry>>)>;

/// Parse [`closure_script`] output (in output order). Errors on malformed
/// lines or a missing `@end` (truncated output). Lines before the first
/// `@` line (login-shell noise) and after `@end` are ignored.
pub fn parse_closures(stdout: &str) -> Result<Closures, String> {
    let mut closures: Closures = Vec::new();
    for (i, line) in stdout.lines().enumerate() {
        let malformed =
            |what: &str| format!("closure listing line {}: {what}: {}", i + 1, excerpt(line));
        if let Some(path) = line.strip_prefix("@closure ") {
            let path = StorePath::parse(path).ok_or_else(|| malformed("not a store path"))?;
            closures.push((path, Some(Vec::new())));
        } else if let Some(path) = line.strip_prefix("@missing ") {
            let path = StorePath::parse(path).ok_or_else(|| malformed("not a store path"))?;
            closures.push((path, None));
        } else if line == "@end" {
            return Ok(closures);
        } else if let Some((_, entries)) = closures.last_mut() {
            let entries = entries
                .as_mut()
                .ok_or_else(|| malformed("entry of a missing path"))?;
            entries.push(
                parse_entry(line).ok_or_else(|| malformed("expected <store path><TAB><size>"))?,
            );
        }
    }
    Err(
        "closure listing ended early (no @end): the connection dropped or the output was cut"
            .into(),
    )
}

/// `<store path>\t<NAR size>`.
fn parse_entry(line: &str) -> Option<ClosureEntry> {
    let (path, size) = line.split_once('\t')?;
    if size.is_empty() || !size.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(ClosureEntry {
        path: StorePath::parse(path)?,
        size: size.parse().ok()?,
    })
}

/// `line` for an error message: debug-quoted, at most 80 characters.
fn excerpt(line: &str) -> String {
    match line.char_indices().nth(80) {
        Some((cut, _)) => format!("{:?}…", &line[..cut]),
        None => format!("{line:?}"),
    }
}

/// Whether `host`'s activation/build would run here.
pub fn runs_here(host: &Host) -> bool {
    host.activate_on == ActivateOn::Local || matches!(host.target, Target::Local { .. })
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write as _};
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use super::*;
    use crate::config::{Sources, SshTarget};
    use crate::util::write_executable;

    // ── Templates ────────────────────────────────────────────────────────

    fn vars() -> TemplateVars<'static> {
        TemplateVars {
            locked: "github:jjant/nix-config/42ea902?narHash=sha256-abc%3D",
            flake: "github:jjant/nix-config",
            rev: "42ea902",
            name: "bigmem",
            attr: "homeConfigurations.bigmem.activationPackage",
            out: "/nix/store/wqnds0yycqdlgdr8nn2xr6130l1glxaj-home-manager-generation",
            destination: "ssh://root@web1:2222",
            ssh_host: "web1",
        }
    }

    #[test]
    fn expands_known_placeholders_shell_quoted() {
        assert_eq!(
            expand("home-manager switch --flake {flake}#{name}", &vars()),
            "home-manager switch --flake 'github:jjant/nix-config/42ea902?narHash=sha256-abc%3D'#bigmem"
        );
        assert_eq!(
            expand(
                "nix run {flake_url}#activate -- {rev} {attr} {out}",
                &vars()
            ),
            "nix run github:jjant/nix-config#activate -- 42ea902 homeConfigurations.bigmem.activationPackage /nix/store/wqnds0yycqdlgdr8nn2xr6130l1glxaj-home-manager-generation"
        );
        assert_eq!(
            expand(
                "deploy --target-host {destination} --host {ssh_host}",
                &vars()
            ),
            "deploy --target-host ssh://root@web1:2222 --host web1"
        );
    }

    #[test]
    fn leaves_other_braces_alone() {
        assert_eq!(
            expand("echo ${HOME} {a,b} {nope} {", &vars()),
            "echo ${HOME} {a,b} {nope} {"
        );
        assert_eq!(expand("{{name}}", &vars()), "{bigmem}");
        assert_eq!(expand("", &vars()), "");
        let empty_rev = TemplateVars { rev: "", ..vars() };
        assert_eq!(expand("x{rev}y", &empty_rev), "x''y");
    }

    #[test]
    fn expansion_edge_cases() {
        assert_eq!(expand("{name}{rev}", &vars()), "bigmem42ea902");
        assert_eq!(
            expand("{flake_url}{flake}", &vars()),
            "github:jjant/nix-config'github:jjant/nix-config/42ea902?narHash=sha256-abc%3D'"
        );
        assert_eq!(
            expand("{flake_urls} {names} {Name} { name} {name", &vars()),
            "{flake_urls} {names} {Name} { name} {name"
        );
        assert_eq!(expand("ünï {name} ✓", &vars()), "ünï bigmem ✓");
        // Values are inserted, never expanded again.
        let sneaky = TemplateVars {
            name: "{flake}",
            ..vars()
        };
        assert_eq!(expand("{name}", &sneaky), "'{flake}'");
    }

    // ── Activation and builds ────────────────────────────────────────────

    const DESIRED: &str = "/nix/store/wqnds0yycqdlgdr8nn2xr6130l1glxaj-home-manager-generation";
    /// `url` of tests/fixtures/metadata-github.json.
    const GITHUB_LOCKED: &str = "github:jjant/nix-config/42ea902131b93c04d098d3bcd02f29305b4bad7c?narHash=sha256-rTwky4CPWade4oeZ81h5PcDt8%2Be8kXznGgaiDqYFhFk%3D";

    fn desired() -> StorePath {
        StorePath::parse(DESIRED).unwrap()
    }

    fn flake_info(flake: &str, locked: &str, kind: &str) -> FlakeInfo {
        FlakeInfo {
            flake: flake.into(),
            locked: locked.into(),
            kind: Some(kind.into()),
            locked_url: None,
            rev: None,
            dirty_rev: None,
            last_modified: Some(1792240011),
            rev_count: None,
            fingerprint: None,
            nar_hash: Some("sha256-rTwky4CPWade4oeZ81h5PcDt8+e8kXznGgaiDqYFhFk=".into()),
            description: None,
            stale: None,
            lock_outdated: false,
        }
    }

    fn github() -> FlakeInfo {
        FlakeInfo {
            rev: Some("42ea902131b93c04d098d3bcd02f29305b4bad7c".into()),
            ..flake_info("github:jjant/nix-config", GITHUB_LOCKED, "github")
        }
    }

    /// tests/fixtures/metadata-path.json.
    fn path_flake() -> FlakeInfo {
        flake_info(
            "/tmp/hostctl-ifd",
            "path:/tmp/hostctl-ifd?lastModified=1792240484&narHash=sha256-uS%2BcPlNAUea9SZ0ceqioQrf36oZQIcJCt8QBY8G%2B5/s%3D",
            "path",
        )
    }

    /// tests/fixtures/metadata-git.json.
    fn git_file() -> FlakeInfo {
        FlakeInfo {
            locked_url: Some("file:///tmp/hostctl-ifd".into()),
            rev: Some("a8f62b6f2c1170af6c40178c9080c87fc51fe5de".into()),
            ..flake_info(
                "git+file:///tmp/hostctl-ifd",
                "git+file:///tmp/hostctl-ifd?ref=refs/heads/main&rev=a8f62b6f2c1170af6c40178c9080c87fc51fe5de",
                "git",
            )
        }
    }

    /// tests/fixtures/metadata-git-dirty.json.
    fn dirty_git() -> FlakeInfo {
        FlakeInfo {
            locked_url: Some("file:///tmp/hostctl-ifd".into()),
            dirty_rev: Some("a8f62b6f2c1170af6c40178c9080c87fc51fe5de-dirty".into()),
            ..flake_info(
                "git+file:///tmp/hostctl-ifd",
                "git+file:///tmp/hostctl-ifd",
                "git",
            )
        }
    }

    fn host(name: &str, kind: Kind, target: Target) -> Host {
        Host {
            name: name.into(),
            kind,
            flake: "github:jjant/nix-config".into(),
            attr: kind
                .default_attr(name)
                .unwrap_or_else(|| "packages.x86_64-linux.default".into()),
            target,
            profile: None,
            activate: kind.default_activate().map(str::to_string),
            activate_on: ActivateOn::Target,
            optional: false,
            note: None,
            sources: Sources::default(),
        }
    }

    fn local() -> Target {
        Target::Local { assumed: false }
    }

    fn ssh(destination: &str) -> Target {
        let parsed = crate::remote::parse_destination(destination).unwrap();
        Target::Ssh(SshTarget {
            destination: destination.into(),
            user: parsed.user,
            host: parsed.host,
            port: parsed.port,
            options: vec![],
            forced_command: None,
        })
    }

    /// The desk→Mac reverse tunnel, whose forced command only answers probes.
    fn tunnel() -> Target {
        match ssh("ssh://devuser1@localhost:2022") {
            Target::Ssh(t) => Target::Ssh(SshTarget {
                forced_command: Some("current-system".into()),
                ..t
            }),
            local => local,
        }
    }

    const NOT_FETCHABLE: &str = "this flake only exists here: use activate-on = \"local\"";

    #[test]
    fn home_manager_default_runs_here_and_checks_what_it_built() {
        let bigmem = host("bigmem", Kind::HomeManager, local());
        let activation = activation(&bigmem, &github(), &desired()).unwrap();
        assert_eq!(
            activation,
            Activation {
                command: format!(
                    "out=$(nix --extra-experimental-features 'nix-command flakes' build --no-link \
                     --print-out-paths --accept-flake-config '{GITHUB_LOCKED}'#.homeConfigurations.bigmem.activationPackage) \
                     && [ \"$out\" = {DESIRED} ] && \"$out/activate\""
                ),
                runs_on: RunsOn::Here,
                warning: None,
            }
        );
    }

    #[test]
    fn nixos_and_darwin_defaults_run_on_the_target() {
        let web1 = host("web1", Kind::Nixos, ssh("root@web1.example.com"));
        let activation = activation(&web1, &github(), &desired()).unwrap();
        assert_eq!(
            activation.command,
            format!(
                "/run/current-system/sw/bin/nixos-rebuild switch --flake '{GITHUB_LOCKED}'#web1 --sudo"
            )
        );
        assert_eq!(activation.runs_on, RunsOn::Remote);
        assert_eq!(activation.warning, None);

        let mac = host("mac-m1", Kind::Darwin, ssh("ssh://jj@mac.example.com:2022"));
        let activation = super::activation(&mac, &github(), &desired()).unwrap();
        assert_eq!(
            activation.command,
            format!(
                "sudo /run/current-system/sw/bin/darwin-rebuild switch --flake '{GITHUB_LOCKED}'#mac-m1"
            )
        );
        assert_eq!(activation.runs_on, RunsOn::Remote);
    }

    #[test]
    fn templates_see_the_flake_the_host_and_its_destination() {
        let mut web1 = host("web1", Kind::Nixos, ssh("ssh://root@web1.example.com:2222"));
        web1.activate = Some(
            "deploy {flake}#{name} --to {destination} --host {ssh_host} --rev {rev} \
             --url {flake_url} --attr {attr} --out {out}"
                .into(),
        );
        let activation = activation(&web1, &github(), &desired()).unwrap();
        assert_eq!(
            activation.command,
            format!(
                "deploy '{GITHUB_LOCKED}'#web1 --to ssh://root@web1.example.com:2222 \
                 --host web1.example.com --rev 42ea902131b93c04d098d3bcd02f29305b4bad7c \
                 --url github:jjant/nix-config \
                 --attr nixosConfigurations.web1.config.system.build.toplevel --out {DESIRED}"
            )
        );

        // Local hosts have no destination; a dirty tree has no revision.
        let mut bigmem = host("bigmem", Kind::HomeManager, local());
        bigmem.activate = Some("x {destination} {ssh_host} {rev}".into());
        let activation = super::activation(&bigmem, &dirty_git(), &desired()).unwrap();
        assert_eq!(activation.command, "x '' '' ''");
    }

    #[test]
    fn push_tools_run_here_whatever_the_target() {
        let mut web1 = host("web1", Kind::Nixos, ssh("root@web1"));
        web1.activate_on = ActivateOn::Local;
        web1.activate = Some(
            "nixos-rebuild switch --flake {flake}#{name} --target-host {destination} --sudo".into(),
        );
        assert!(runs_here(&web1));
        // Local flakes are fine: the flake never leaves this machine.
        for flake in [github(), path_flake(), git_file(), dirty_git()] {
            let activation = activation(&web1, &flake, &desired()).unwrap();
            assert_eq!(activation.runs_on, RunsOn::Here, "{}", flake.locked);
            assert!(
                activation
                    .command
                    .ends_with("#web1 --target-host root@web1 --sudo"),
                "{}",
                activation.command
            );
        }
        // A forced-command route doesn't matter either: ssh isn't involved.
        let mut mac = host("mac-m1", Kind::Darwin, tunnel());
        mac.activate_on = ActivateOn::Local;
        let activation = activation(&mac, &github(), &desired()).unwrap();
        assert_eq!(activation.runs_on, RunsOn::Here);
    }

    #[test]
    fn refuses_what_the_target_cannot_do() {
        let mac = host("mac-m1", Kind::Darwin, tunnel());
        assert!(!runs_here(&mac));
        assert_eq!(
            activation(&mac, &github(), &desired()),
            Err("mac-m1 only answers its probe command".into())
        );
        // The route is the first problem to fix.
        assert_eq!(
            activation(&mac, &path_flake(), &desired()),
            Err("mac-m1 only answers its probe command".into())
        );
        let web1 = host("web1", Kind::Nixos, ssh("root@web1"));
        for flake in [path_flake(), git_file(), dirty_git()] {
            assert_eq!(
                activation(&web1, &flake, &desired()),
                Err(NOT_FETCHABLE.into()),
                "{}",
                flake.locked
            );
        }
        // Remote git over ssh/https is fetchable.
        let remote_git = FlakeInfo {
            locked_url: Some("ssh://git@github.com/jjant/nix-config".into()),
            ..git_file()
        };
        assert!(activation(&web1, &remote_git, &desired()).is_ok());
    }

    #[test]
    fn hosts_without_a_template_have_no_activation() {
        let mut custom = host("box", Kind::Custom, local());
        assert_eq!(custom.activate, None);
        assert_eq!(
            activation(&custom, &github(), &desired()),
            Err("box has no activation command: custom hosts need activate = \"…\"".into())
        );
        for blank in ["", " \t "] {
            custom.activate = Some(blank.into());
            assert!(
                activation(&custom, &github(), &desired()).is_err(),
                "{blank:?}"
            );
        }
        // Disabled beats every other reason.
        let mut mac = host("mac-m1", Kind::Darwin, tunnel());
        mac.activate = None;
        assert_eq!(
            activation(&mac, &path_flake(), &desired()),
            Err("activation is disabled for mac-m1 (activate = false)".into())
        );
    }

    #[test]
    fn warns_about_whatever_makes_the_result_uncertain() {
        let bigmem = host("bigmem", Kind::HomeManager, local());
        let warning = |host: &Host, flake: &FlakeInfo| {
            activation(host, flake, &desired())
                .unwrap()
                .warning
                .unwrap_or_default()
        };
        assert_eq!(warning(&bigmem, &github()), "");
        assert_eq!(
            warning(&bigmem, &dirty_git()),
            "the flake is a dirty tree: activation builds whatever is on disk now"
        );

        let mac = host("mac-m1", Kind::Darwin, Target::Local { assumed: true });
        assert_eq!(
            activation(&mac, &github(), &desired()).unwrap().runs_on,
            RunsOn::Here
        );
        assert_eq!(
            warning(&mac, &github()),
            "mac-m1 is only assumed to be this machine (the only darwin host): \
             confirm with self = \"mac-m1\""
        );

        let stale = FlakeInfo {
            stale: Some("unable to download 'https://api.github.com/repos/jjant/nix-config/commits/HEAD': HTTP error 403".into()),
            ..github()
        };
        assert_eq!(
            warning(&bigmem, &stale),
            "the flake's resolution is stale (unable to download \
             'https://api.github.com/repos/jjant/nix-config/commits/HEAD': HTTP error 403): \
             this activates 42ea902, which may not be the latest revision"
        );

        let lock = FlakeInfo {
            lock_outdated: true,
            ..github()
        };
        assert!(
            warning(&bigmem, &lock).starts_with("flake.lock is out of date: "),
            "{}",
            warning(&bigmem, &lock)
        );

        // Everything at once, in a fixed order.
        let everything = FlakeInfo {
            stale: Some("offline".into()),
            lock_outdated: true,
            ..dirty_git()
        };
        let all = warning(&mac, &everything);
        let parts: Vec<&str> = all.split("; ").collect();
        assert_eq!(parts.len(), 4, "{all}");
        assert!(parts[0].starts_with("the flake is a dirty tree"), "{all}");
        assert!(parts[1].starts_with("mac-m1 is only assumed"), "{all}");
        assert!(
            parts[2].starts_with(
                "the flake's resolution is stale (offline): this activates a8f62b6-dirty,"
            ),
            "{all}"
        );
        assert!(parts[3].starts_with("flake.lock is out of date"), "{all}");

        // Remote hosts get the flake warnings too.
        let web1 = host("web1", Kind::Nixos, ssh("web1"));
        assert!(warning(&web1, &stale).starts_with("the flake's resolution is stale"));
    }

    #[test]
    fn build_command_builds_the_compared_attribute_without_activating() {
        let bigmem = host("bigmem", Kind::HomeManager, local());
        assert_eq!(
            build_command(&bigmem, &github()).unwrap(),
            format!(
                "nix --extra-experimental-features 'nix-command flakes' build --no-link \
                 --print-out-paths --accept-flake-config --no-write-lock-file \
                 '{GITHUB_LOCKED}#.homeConfigurations.bigmem.activationPackage'"
            )
        );
        // A quoted attribute stays inside the one installable word.
        let jj = host("jj@mac", Kind::HomeManager, ssh("jj@mac.example.com"));
        assert!(build_command(&jj, &github()).unwrap().ends_with(&format!(
            " '{GITHUB_LOCKED}#.homeConfigurations.\"jj@mac\".activationPackage'"
        )));
        // Same obstacles as activation on remote targets…
        let mac = host("mac-m1", Kind::Darwin, tunnel());
        assert_eq!(
            build_command(&mac, &github()),
            Err("mac-m1 only answers its probe command".into())
        );
        assert_eq!(build_command(&jj, &path_flake()), Err(NOT_FETCHABLE.into()));
        // …but none when it runs here, where every flake exists.
        let built_here = build_command(&bigmem, &dirty_git()).unwrap();
        assert!(
            built_here.ends_with(
                " 'git+file:///tmp/hostctl-ifd#.homeConfigurations.bigmem.activationPackage'"
            ),
            "{built_here}"
        );
        let mut push = host("web1", Kind::Nixos, ssh("root@web1"));
        push.activate_on = ActivateOn::Local;
        assert!(build_command(&push, &path_flake()).is_ok());
        // Disabled activation doesn't prevent building for a diff.
        let mut disabled = host("web1", Kind::Nixos, ssh("root@web1"));
        disabled.activate = None;
        assert!(build_command(&disabled, &github()).is_ok());
    }

    // ── Closure listings ─────────────────────────────────────────────────

    /// A valid store path, distinct for every `i`.
    fn store_path(i: usize, name: &str) -> StorePath {
        const BASE32: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";
        let mut hash = [b'0'; 32];
        let mut n = i;
        for digit in hash.iter_mut().rev() {
            *digit = BASE32[n % 32];
            n /= 32;
        }
        let hash = std::str::from_utf8(&hash).unwrap();
        StorePath::parse(&format!("/nix/store/{hash}-{name}")).unwrap()
    }

    fn entry(path: &StorePath, size: u64) -> ClosureEntry {
        ClosureEntry {
            path: path.clone(),
            size,
        }
    }

    /// Not in any store.
    fn missing() -> StorePath {
        store_path(31 << 50, "hostctl-test-missing")
    }

    /// Real home-manager closures of bigmem (`nix-store -qR` + `--size`).
    const FIXTURES: [(&str, &str); 4] = [
        (
            include_str!("../tests/fixtures/closure-hm-1.root"),
            include_str!("../tests/fixtures/closure-hm-1.tsv"),
        ),
        (
            include_str!("../tests/fixtures/closure-hm-5.root"),
            include_str!("../tests/fixtures/closure-hm-5.tsv"),
        ),
        (
            include_str!("../tests/fixtures/closure-hm-8.root"),
            include_str!("../tests/fixtures/closure-hm-8.tsv"),
        ),
        (
            include_str!("../tests/fixtures/closure-hm-9.root"),
            include_str!("../tests/fixtures/closure-hm-9.tsv"),
        ),
    ];

    /// The fixture closures, read independently of [`parse_closures`].
    fn fixtures() -> Vec<(StorePath, Vec<ClosureEntry>)> {
        FIXTURES
            .iter()
            .map(|(root, tsv)| {
                let entries = tsv
                    .lines()
                    .map(|line| {
                        let (path, size) = line.split_once('\t').unwrap();
                        entry(&StorePath::parse(path).unwrap(), size.parse().unwrap())
                    })
                    .collect();
                (StorePath::parse(root.trim()).unwrap(), entries)
            })
            .collect()
    }

    /// What [`closure_script`] prints for `closures`.
    fn listing(closures: &[(StorePath, Option<Vec<ClosureEntry>>)]) -> String {
        let mut out = String::new();
        for (root, entries) in closures {
            match entries {
                Some(entries) => {
                    let _ = writeln!(out, "@closure {root}");
                    for e in entries {
                        let _ = writeln!(out, "{}\t{}", e.path, e.size);
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

    #[test]
    fn parses_closure_listings() {
        let (a, b, c, dep) = (
            store_path(1, "home-manager-generation"),
            store_path(2, "home-manager-generation"),
            store_path(3, "hm_tmuxwindownaming"),
            store_path(4, "glibc-2.44-25"),
        );
        let text = format!(
            "@closure {a}\n{dep}\t35578272\n{a}\t4096\n@missing {b}\n@closure {c}\n{c}\t0\n@end\n"
        );
        assert_eq!(
            parse_closures(&text),
            Ok(vec![
                (
                    a.clone(),
                    Some(vec![entry(&dep, 35578272), entry(&a, 4096)])
                ),
                (b, None),
                (c.clone(), Some(vec![entry(&c, 0)])),
            ])
        );
        assert_eq!(parse_closures("@end\n"), Ok(vec![]));
        // Complete even if the very last newline got lost.
        assert_eq!(parse_closures("@end"), Ok(vec![]));
        // Empty closures and sizes up to u64::MAX are well-formed.
        assert_eq!(
            parse_closures(&format!(
                "@closure {a}\n@closure {c}\n{c}\t18446744073709551615\n@end\n"
            )),
            Ok(vec![
                (a, Some(vec![])),
                (c.clone(), Some(vec![entry(&c, u64::MAX)]))
            ])
        );
    }

    #[test]
    fn ignores_noise_around_the_listing() {
        let (a, dep) = (store_path(1, "a"), store_path(2, "dep"));
        let text = format!(
            "Welcome to bigmem!\n/etc/bashrc: line 3: complete: command not found\n{dep}\t5\n\
             @closure {a}\r\n{dep}\t7\r\n@end\r\nlogout\n@closure {dep}\ngarbage\n"
        );
        assert_eq!(
            parse_closures(&text),
            Ok(vec![(a, Some(vec![entry(&dep, 7)]))])
        );
    }

    #[test]
    fn rejects_malformed_lines() {
        let (a, x) = (store_path(1, "a"), store_path(2, "x"));
        for bad in [
            format!("{x}"),
            format!("{x} 123"),
            format!("{x}\t"),
            format!("{x}\t-1"),
            format!("{x}\t+1"),
            format!("{x}\t1.5"),
            format!("{x}\t 1"),
            format!("{x}\t1 "),
            format!("{x}\t1\t2"),
            format!("{x}\t18446744073709551616"),
            format!("\t{x}\t1"),
            format!("{x}/bin/x\t1"),
            "/tmp/x\t1".into(),
            "@closure".into(),
            "@closure  /nix/store/x".into(),
            format!("@closure {x}/bin"),
            format!("@missing {x} "),
            "@frobnicate".into(),
            "@end ".into(),
            "@endless".into(),
            String::new(),
        ] {
            let text = format!("@closure {a}\n{a}\t1\n{bad}\n@end\n");
            let err = parse_closures(&text).unwrap_err();
            assert!(
                err.starts_with("closure listing line 3: "),
                "{bad:?}: {err}"
            );
        }
        assert_eq!(
            parse_closures(&format!("@missing {a}\n{a}\t1\n@end\n")),
            Err(format!(
                "closure listing line 2: entry of a missing path: \"{a}\\t1\""
            ))
        );
        assert_eq!(
            parse_closures("@closure /tmp/x\n@end\n"),
            Err("closure listing line 1: not a store path: \"@closure /tmp/x\"".into())
        );
        // Long garbage is cut short in the message.
        let err =
            parse_closures(&format!("@closure {a}\n{}\n@end\n", "ü".repeat(100_000))).unwrap_err();
        assert!(err.len() < 300, "{err}");
        assert!(err.ends_with("…"), "{err}");
    }

    #[test]
    fn truncated_listings_are_errors() {
        let (a, b, dep) = (store_path(1, "a"), store_path(2, "b"), store_path(3, "dep"));
        let text = listing(&[
            (a.clone(), Some(vec![entry(&dep, 123), entry(&a, 4096)])),
            (b, None),
        ]);
        for cut in 0..text.len() {
            let prefix = &text[..cut];
            let complete = prefix.lines().any(|line| line == "@end");
            assert_eq!(parse_closures(prefix).is_ok(), complete, "{prefix:?}");
        }
        assert_eq!(
            parse_closures(""),
            Err("closure listing ended early (no @end): the connection dropped or the output was cut".into())
        );
    }

    #[test]
    fn parses_listings_of_real_closures() {
        let fixtures = fixtures();
        let (root, entries) = &fixtures[3];
        assert_eq!(entries.len(), 702);
        assert!(entries.iter().any(|e| &e.path == root));
        assert!(entries.iter().all(|e| e.size > 0));

        let mut expected: Closures = fixtures
            .iter()
            .map(|(root, entries)| (root.clone(), Some(entries.clone())))
            .collect();
        expected.insert(2, (missing(), None));
        let text = listing(&expected);
        assert_eq!(parse_closures(&text), Ok(expected));

        // Cut anywhere before `@end`, it's an error; one corrupt line, too.
        let one = listing(&[(root.clone(), Some(entries.clone()))]);
        let end = one.len() - "@end\n".len();
        for cut in (0..end).step_by(331).chain(end - 3..=end) {
            assert!(parse_closures(&one[..cut]).is_err(), "cut at {cut}");
        }
        let corrupt = one.replacen("\t2083096\n", "\t2083096x\n", 1);
        let err = parse_closures(&corrupt).unwrap_err();
        assert!(err.starts_with("closure listing line 5: "), "{err}");
    }

    #[test]
    fn parses_big_listings_fast() {
        // 2 × 25k entries: guards against anything worse than linear.
        let entries: Vec<ClosureEntry> = (0..25_000)
            .map(|i| entry(&store_path(i, "python3.13-requests-2.32.3"), i as u64))
            .collect();
        let text = listing(&[
            (store_path(1, "a"), Some(entries.clone())),
            (store_path(2, "b"), Some(entries)),
        ]);
        let start = Instant::now();
        let closures = parse_closures(&text).unwrap();
        assert_eq!(closures[1].1.as_ref().map(Vec::len), Some(25_000));
        // Milliseconds; the slack is for loaded machines.
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn closure_script_shape() {
        let (a, odd) = (store_path(1, "a"), store_path(2, "odd?name=1"));
        let script = closure_script(&[&a, &odd]);
        assert!(
            script.starts_with(&format!("{NIX_PRELUDE}\nset -u\n")),
            "{script}"
        );
        let first = script
            .find(&format!("\nclosure {a} \"$d/0\" & j0=$!\n"))
            .unwrap();
        // `?` and `=` would glob/assign unquoted.
        let second = script
            .find(&format!("\nclosure '{odd}' \"$d/1\" & j1=$!\n"))
            .unwrap();
        assert!(first < second);
        assert!(
            script.ends_with(
                "ok=1\nwait \"$j0\" || ok=\nwait \"$j1\" || ok=\n\
                 [ -n \"$ok\" ] && cat \"$d/0\" \"$d/1\" || exit 1\necho @end\n"
            ),
            "{script}"
        );
        let empty = closure_script(&[]);
        assert!(empty.ends_with("}\necho @end\n"), "{empty}");
        assert!(
            !empty.contains("&& cat"),
            "cat without files would read stdin"
        );
    }

    // ── Running scripts for real ─────────────────────────────────────────

    struct Run {
        code: Option<i32>,
        stdout: String,
        stderr: String,
        elapsed: Duration,
    }

    /// Run `cmd` with `stdin`; a child still running after 30 s fails the
    /// test instead of hanging it.
    fn run(mut cmd: Command, stdin: &str) -> Run {
        let start = Instant::now();
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("spawning {cmd:?}: {e}"));
        let mut input = child.stdin.take().unwrap();
        let stdin = stdin.to_owned();
        // The child may exit without reading everything.
        let writer = std::thread::spawn(move || {
            let _ = input.write_all(stdin.as_bytes());
        });
        fn read_all(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<String> {
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                let _ = pipe.read_to_end(&mut bytes);
                String::from_utf8_lossy(&bytes).into_owned()
            })
        }
        let stdout = read_all(child.stdout.take().unwrap());
        let stderr = read_all(child.stderr.take().unwrap());
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if start.elapsed() > Duration::from_secs(30) {
                let _ = child.kill();
                panic!("{cmd:?} didn't finish within 30 s");
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        let elapsed = start.elapsed();
        writer.join().unwrap();
        Run {
            code: status.code(),
            stdout: stdout.join().unwrap(),
            stderr: stderr.join().unwrap(),
            elapsed,
        }
    }

    fn find_program(name: &str) -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|p| {
                p.metadata()
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            })
    }

    fn sh() -> PathBuf {
        find_program("sh").unwrap_or_else(|| "/bin/sh".into())
    }

    /// `dirs`, then the test's own PATH.
    fn search_path(dirs: &[PathBuf]) -> std::ffi::OsString {
        let inherited = std::env::var_os("PATH").unwrap_or_default();
        std::env::join_paths(
            dirs.iter()
                .cloned()
                .chain(std::env::split_paths(&inherited)),
        )
        .unwrap()
    }

    struct Shell {
        name: &'static str,
        program: PathBuf,
        args: &'static [&'static str],
    }

    impl Shell {
        fn command(&self) -> Command {
            let mut cmd = Command::new(&self.program);
            cmd.args(self.args);
            cmd
        }
    }

    /// The shells hosts run scripts with: `sh`, `dash`, `bash --posix` and
    /// `busybox sh` — those on PATH, or all of them with
    /// HOSTCTL_TEST_REQUIRE_SHELLS=1.
    fn shells() -> Vec<Shell> {
        let require = std::env::var("HOSTCTL_TEST_REQUIRE_SHELLS").is_ok_and(|v| v == "1");
        let wanted: [(&'static str, &str, &'static [&'static str]); 4] = [
            ("sh", "sh", &[]),
            ("dash", "dash", &[]),
            ("bash --posix", "bash", &["--posix"]),
            ("busybox sh", "busybox", &["sh"]),
        ];
        wanted
            .into_iter()
            .filter_map(|(name, program, args)| {
                let found = if program == "sh" {
                    Some(sh())
                } else {
                    find_program(program)
                };
                match found {
                    Some(program) => Some(Shell {
                        name,
                        program,
                        args,
                    }),
                    None if require => {
                        panic!("{name} is required (HOSTCTL_TEST_REQUIRE_SHELLS=1) but {program} isn't on PATH")
                    }
                    None => {
                        eprintln!("skipping {name}: {program} isn't on PATH");
                        None
                    }
                }
            })
            .collect()
    }

    /// A `nix-store` that answers from closures given up front, so the
    /// closure script runs for real on any machine. `FAKE_NIX_MODE` breaks
    /// it on purpose (or, `named`, reads sizes off the paths' last `-`
    /// field instead of the database); every `-q --size` call logs its
    /// argument count.
    const FAKE_NIX_STORE: &str = r#"#!/bin/sh
db=$FAKE_NIX_DB
mode=${FAKE_NIX_MODE:-}
case "$1" in
--check-validity)
  [ "$mode" = fail-validity ] && { echo "error: cannot connect to socket at '/nix/var/nix/daemon-socket/socket'" >&2; exit 1; }
  shift 2
  for p in "$@"; do [ -f "$db/${p##*/}" ] || printf '%s\n' "$p"; done
  ;;
-qR)
  [ "$mode" = hang ] && { echo hang >>"$db/calls"; exec sleep 30; }
  [ "$mode" = fail-closure ] && { echo 'error: database disk image is malformed' >&2; exit 1; }
  [ -f "$db/${2##*/}" ] || { echo "error: path '$2' is not valid" >&2; exit 1; }
  cat "$db/${2##*/}"
  ;;
-q)
  shift 2
  echo "size $#" >>"$db/calls"
  [ "$mode" = fail-size ] && { echo 'error: interrupted by the user' >&2; exit 1; }
  [ "$mode" = named ] && { printf '%s\n' "$@" | awk -F - '{ print $NF }'; exit; }
  printf '%s\n' "$@" | awk -F '\t' -v mode="$mode" '
    NR == FNR { size[$1] = $2; next }
    mode == "drop" && FNR == 1 { next }
    mode == "garbage" && FNR == 1 { print "12x"; next }
    { print size[$0] }
    END { if (mode == "extra") print 1 }' "$db/sizes" -
  ;;
*)
  echo "fake nix-store: unexpected arguments: $*" >&2
  exit 2
  ;;
esac
"#;

    struct FakeStore {
        dir: tempfile::TempDir,
    }

    impl FakeStore {
        fn new(closures: &[(StorePath, Vec<ClosureEntry>)]) -> FakeStore {
            let dir = tempfile::tempdir().unwrap();
            for sub in ["bin", "db", "tmp", "busybox"] {
                std::fs::create_dir(dir.path().join(sub)).unwrap();
            }
            write_executable(&dir.path().join("bin/nix-store"), FAKE_NIX_STORE);
            let mut sizes = String::new();
            for (root, entries) in closures {
                let mut paths = String::new();
                for e in entries {
                    let _ = writeln!(paths, "{}", e.path);
                    let _ = writeln!(sizes, "{}\t{}", e.path, e.size);
                }
                let name = root.as_str().rsplit('/').next().unwrap();
                std::fs::write(dir.path().join("db").join(name), paths).unwrap();
            }
            std::fs::write(dir.path().join("db/sizes"), sizes).unwrap();
            FakeStore { dir }
        }

        /// `<shell> -s`, finding the fake first (and, under busybox, busybox's
        /// own awk, xargs, …, as on a busybox system).
        fn command(&self, shell: &Shell, mode: &str) -> Command {
            let root = self.dir.path();
            let mut dirs = vec![root.join("bin")];
            if shell.name == "busybox sh" {
                let tools = root.join("busybox");
                for tool in ["awk", "cat", "mktemp", "rm", "xargs"] {
                    let link = tools.join(tool);
                    if !link.exists() {
                        std::os::unix::fs::symlink(&shell.program, link).unwrap();
                    }
                }
                dirs.push(tools);
            }
            let mut cmd = shell.command();
            cmd.arg("-s")
                .env("PATH", search_path(&dirs))
                .env("FAKE_NIX_DB", root.join("db"))
                .env("FAKE_NIX_MODE", mode)
                .env("TMPDIR", root.join("tmp"));
            cmd
        }

        /// The argument count of every `-q --size` call so far.
        fn size_calls(&self) -> Vec<usize> {
            std::fs::read_to_string(self.dir.path().join("db/calls"))
                .unwrap_or_default()
                .lines()
                .map(|line| line.strip_prefix("size ").unwrap().parse().unwrap())
                .collect()
        }

        /// Whatever the script left in its TMPDIR.
        fn leftovers(&self) -> Vec<PathBuf> {
            std::fs::read_dir(self.dir.path().join("tmp"))
                .unwrap()
                .map(|e| e.unwrap().path())
                .collect()
        }
    }

    #[test]
    fn closure_script_lists_real_closures_in_every_shell() {
        let fixtures = fixtures();
        let odd = store_path(7, "odd?name=1");
        let mut known = fixtures.clone();
        known.push((
            odd.clone(),
            vec![entry(&fixtures[0].1[0].path, 571168), entry(&odd, 512)],
        ));
        let fake = FakeStore::new(&known);
        let missing = missing();
        // Every fixture, a missing path, a name that needs quoting, a repeat.
        let mut paths: Vec<&StorePath> = known.iter().map(|(root, _)| root).collect();
        paths.insert(1, &missing);
        paths.push(&known[3].0);
        let mut expected: Closures = known
            .iter()
            .map(|(root, entries)| (root.clone(), Some(entries.clone())))
            .collect();
        expected.insert(1, (missing.clone(), None));
        expected.push(expected[4].clone());

        let script = closure_script(&paths);
        for shell in shells() {
            let run = run(fake.command(&shell, ""), &script);
            assert_eq!(run.code, Some(0), "{}: {}", shell.name, run.stderr);
            assert_eq!(run.stderr, "", "{}", shell.name);
            assert_eq!(
                parse_closures(&run.stdout).as_ref(),
                Ok(&expected),
                "{}",
                shell.name
            );
            assert_eq!(fake.leftovers(), Vec::<PathBuf>::new(), "{}", shell.name);
        }
    }

    #[test]
    fn closure_script_keeps_sizes_in_order_when_xargs_splits() {
        // 6000 paths, ~80 bytes each: more than one command line anywhere
        // (GNU xargs uses 128 KiB, BSD xargs at most 5000 arguments). The
        // sizes are distinct, so any reordering shows.
        let entries: Vec<ClosureEntry> = (0..6000)
            .map(|i| {
                let size = (i as u64 * 7919) % 1_000_003 + 1;
                let path = store_path(i, &format!("hostctl-synthetic-package-{i}-{size}"));
                entry(&path, size)
            })
            .collect();
        let root = entries[5999].path.clone();
        let expected = vec![(root.clone(), Some(entries.clone()))];
        let script = closure_script(&[&root]);
        for shell in shells() {
            let fake = FakeStore::new(&[(root.clone(), entries.clone())]);
            let run = run(fake.command(&shell, "named"), &script);
            assert_eq!(run.code, Some(0), "{}: {}", shell.name, run.stderr);
            assert_eq!(
                parse_closures(&run.stdout).as_ref(),
                Ok(&expected),
                "{}",
                shell.name
            );
            let calls = fake.size_calls();
            assert!(calls.len() >= 2, "{}: {calls:?}", shell.name);
            assert_eq!(calls.iter().sum::<usize>(), 6000, "{}", shell.name);
            assert!(
                run.elapsed < Duration::from_secs(30),
                "{}: {:?}",
                shell.name,
                run.elapsed
            );
        }
    }

    #[test]
    fn closure_script_refuses_sizes_that_dont_line_up() {
        let message = "doesn't line up with the closure of";
        fails_without_an_answer(&[("drop", message), ("extra", message), ("garbage", message)]);
    }

    #[test]
    fn closure_script_reports_nix_store_failures() {
        fails_without_an_answer(&[
            ("fail-size", "error: interrupted by the user"),
            ("fail-closure", "error: database disk image is malformed"),
            ("fail-validity", "error: cannot connect to socket"),
        ]);
    }

    /// Under each `(FAKE_NIX_MODE, message)`, the script must fail with
    /// that message, print no answer and leave nothing behind.
    fn fails_without_an_answer(modes: &[(&str, &str)]) {
        let fixtures = fixtures();
        let (root, _) = &fixtures[3];
        let missing = missing();
        let script = closure_script(&[root, &missing]);
        for shell in shells() {
            let fake = FakeStore::new(&fixtures[3..]);
            for &(mode, message) in modes {
                let run = run(fake.command(&shell, mode), &script);
                let what = format!("{} {mode}: {}", shell.name, run.stderr);
                assert!(!matches!(run.code, Some(0) | None), "{what}");
                // The cause, and only the cause (once per xargs batch).
                assert!(run.stderr.contains(message), "{what}");
                assert!(run.stderr.lines().all(|l| l.contains(message)), "{what}");
                assert!(!run.stdout.contains("@end"), "{what}");
                assert!(parse_closures(&run.stdout).is_err(), "{what}");
                assert_eq!(fake.leftovers(), Vec::<PathBuf>::new(), "{what}");
            }
        }
    }

    #[test]
    fn closure_script_explains_a_missing_nix_store() {
        let home = tempfile::tempdir().unwrap();
        let user = "hostctl-test-nobody";
        // The prelude also looks in fixed places; where those have Nix, it
        // can't be missing.
        let fixed = [
            "/nix/var/nix/profiles/default/bin".to_string(),
            "/run/current-system/sw/bin".into(),
            "/run/wrappers/bin".into(),
            format!("/etc/profiles/per-user/{user}/bin"),
        ];
        if let Some(dir) = fixed
            .iter()
            .find(|d| Path::new(d).join("nix-store").exists())
        {
            eprintln!("skipping: {dir} has nix-store");
            return;
        }
        let empty = home.path().join("bin");
        std::fs::create_dir(&empty).unwrap();
        let script = closure_script(&[&missing()]);
        for shell in shells() {
            let mut cmd = shell.command();
            cmd.arg("-s")
                .env_clear()
                .env("PATH", &empty)
                .env("HOME", home.path())
                .env("USER", user);
            let run = run(cmd, &script);
            assert_eq!(run.code, Some(127), "{}: {}", shell.name, run.stderr);
            assert!(
                run.stderr
                    .contains("hostctl: nix-store not found on PATH ("),
                "{}",
                run.stderr
            );
            assert_eq!(run.stdout, "");
        }
    }

    #[test]
    fn closure_script_with_no_paths_just_ends() {
        let fake = FakeStore::new(&[]);
        for shell in shells() {
            let run = run(fake.command(&shell, ""), &closure_script(&[]));
            assert_eq!(run.code, Some(0), "{}: {}", shell.name, run.stderr);
            assert_eq!(run.stdout, "@end\n");
            assert_eq!(fake.leftovers(), Vec::<PathBuf>::new());
        }
    }

    #[test]
    fn closure_script_cleans_up_when_killed() {
        use std::os::unix::process::CommandExt as _;
        let fixtures = fixtures();
        let script = closure_script(&[&fixtures[3].0, &missing()]);
        for shell in shells() {
            let fake = FakeStore::new(&fixtures[3..]);
            let mut cmd = fake.command(&shell, "hang");
            cmd.process_group(0)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let mut child = cmd.spawn().unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(script.as_bytes())
                .unwrap();
            // Once both jobs hang in nix-store, the traps are set.
            let deadline = Instant::now() + Duration::from_secs(10);
            let calls = fake.dir.path().join("db/calls");
            while std::fs::read_to_string(&calls).map_or(0, |s| s.lines().count()) < 2 {
                assert!(
                    Instant::now() < deadline,
                    "{}: the jobs never started",
                    shell.name
                );
                std::thread::sleep(Duration::from_millis(2));
            }
            assert_eq!(fake.leftovers().len(), 1, "{}", shell.name);
            // What exec::run does on a timeout.
            // SAFETY: killpg only sends a signal to our child's process group.
            unsafe { libc::killpg(child.id() as libc::pid_t, libc::SIGTERM) };
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                assert!(Instant::now() < deadline, "{}: ignored SIGTERM", shell.name);
                std::thread::sleep(Duration::from_millis(2));
            };
            assert!(!status.success(), "{}", shell.name);
            assert_eq!(fake.leftovers(), Vec::<PathBuf>::new(), "{}", shell.name);
        }
    }

    /// This machine's current home-manager generation, if it has one.
    fn home_manager_generation() -> Option<StorePath> {
        let state = match std::env::var_os("XDG_STATE_HOME") {
            Some(dir) => PathBuf::from(dir),
            None => PathBuf::from(std::env::var_os("HOME")?).join(".local/state"),
        };
        let generation = std::fs::canonicalize(state.join("nix/profiles/home-manager")).ok()?;
        StorePath::parse(generation.to_str()?)
    }

    /// The generation to list from this machine's store, unless the tests
    /// can't use that store.
    fn real_generation() -> Option<StorePath> {
        if find_program("nix-store").is_none() {
            eprintln!("skipping: nix-store isn't on PATH");
            return None;
        }
        if std::env::var_os("HOSTCTL_TEST_NIX_STORE").is_some() {
            eprintln!("skipping: HOSTCTL_TEST_NIX_STORE asks for a private store");
            return None;
        }
        let generation = home_manager_generation();
        if generation.is_none() {
            eprintln!("skipping: no home-manager profile here");
        }
        generation
    }

    fn check_real_listing(run: &Run, generation: &StorePath) {
        assert_eq!(run.code, Some(0), "{}", run.stderr);
        let closures = parse_closures(&run.stdout).unwrap();
        assert_eq!(closures.len(), 2);
        let (path, entries) = &closures[0];
        assert_eq!(path, generation);
        let entries = entries.as_ref().expect("the active generation is valid");
        assert!(entries.len() > 1, "{entries:?}");
        assert!(entries.iter().any(|e| &e.path == generation));
        assert!(entries.iter().all(|e| e.size > 0), "{entries:?}");
        assert_eq!(closures[1], (missing(), None));
        assert!(
            run.elapsed < Duration::from_secs(20),
            "took {:?}",
            run.elapsed
        );
    }

    #[test]
    fn real_closure_of_this_machines_home_manager_generation() {
        let Some(generation) = real_generation() else {
            return;
        };
        let script = closure_script(&[&generation, &missing()]);
        let mut cmd = Command::new(sh());
        cmd.arg("-s");
        check_real_listing(&run(cmd, &script), &generation);
    }

    #[test]
    fn real_closure_in_a_bare_environment_finds_nix_through_the_prelude() {
        let Some(generation) = real_generation() else {
            return;
        };
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let user = std::env::var("USER").unwrap_or_default();
        let prelude_dirs = [
            PathBuf::from("/nix/var/nix/profiles/default/bin"),
            "/run/current-system/sw/bin".into(),
            home.join(".nix-profile/bin"),
            home.join(".local/state/nix/profile/bin"),
            PathBuf::from(format!("/etc/profiles/per-user/{user}/bin")),
        ];
        if !prelude_dirs.iter().any(|d| d.join("nix-store").exists()) {
            eprintln!("skipping: nix-store isn't installed where the prelude looks");
            return;
        }
        // Like a non-login ssh session: `env -i HOME=$HOME PATH=/usr/bin:/bin sh -s`.
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-s")
            .env_clear()
            .env("HOME", &home)
            .env("PATH", "/usr/bin:/bin");
        let script = closure_script(&[&generation, &missing()]);
        check_real_listing(&run(cmd, &script), &generation);
    }

    #[test]
    fn real_closures_in_a_private_store() {
        if std::env::var("HOSTCTL_TEST_NIX").as_deref() != Ok("1") {
            eprintln!("skipping: real-Nix test (set HOSTCTL_TEST_NIX=1)");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let (store, _turn) = crate::util::test_nix_store();
        let store =
            store.unwrap_or_else(|| format!("local?root={}", tmp.path().join("store").display()));
        let cache = tmp.path().join("cache");
        let eval = |expr: &str| {
            let out = Command::new("nix")
                .args(FEATURES)
                .args(["eval", "--store", &store, "--raw", "--expr", expr])
                .env("XDG_CACHE_HOME", &cache)
                .output()
                .expect("HOSTCTL_TEST_NIX=1 needs nix on PATH");
            assert!(
                out.status.success(),
                "{expr}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            StorePath::parse(std::str::from_utf8(&out.stdout).unwrap()).unwrap()
        };
        let leaf = eval(r#"builtins.toFile "hostctl-leaf" "leaf""#);
        let top =
            eval(r#"builtins.toFile "hostctl-top" "${builtins.toFile "hostctl-leaf" "leaf"}""#);

        let script = closure_script(&[&top, &leaf, &missing()]);
        let mut cmd = Command::new(sh());
        cmd.arg("-s")
            .env("NIX_REMOTE", &store)
            .env("XDG_CACHE_HOME", &cache);
        let run = run(cmd, &script);
        assert_eq!(run.code, Some(0), "{}", run.stderr);
        let closures = parse_closures(&run.stdout).unwrap();
        let size = |path: &StorePath| -> u64 {
            let out = Command::new("nix-store")
                .args(["--store", &store, "-q", "--size", path.as_str()])
                .output()
                .unwrap();
            String::from_utf8(out.stdout)
                .unwrap()
                .trim()
                .parse()
                .unwrap()
        };
        let (leaf_size, top_size) = (size(&leaf), size(&top));
        assert!(leaf_size > 0 && top_size > 0);
        let mut top_closure = closures[0].1.clone().unwrap();
        top_closure.sort_by(|a, b| a.path.cmp(&b.path));
        let mut want = vec![entry(&leaf, leaf_size), entry(&top, top_size)];
        want.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(closures[0].0, top);
        assert_eq!(top_closure, want);
        assert_eq!(
            closures[1],
            (leaf.clone(), Some(vec![entry(&leaf, leaf_size)]))
        );
        assert_eq!(closures[2], (missing(), None));
    }

    // ── Templates under real shells ──────────────────────────────────────

    #[test]
    fn default_templates_expand_to_valid_sh() {
        let shells = shells();
        let locked = path_flake().locked;
        for kind in Kind::ALL {
            let Some(template) = kind.default_activate() else {
                assert_eq!(kind, Kind::Custom);
                continue;
            };
            for name in ["bigmem", "jj@mac", "it's"] {
                let attr = kind.default_attr(name).unwrap();
                let vars = TemplateVars {
                    locked: &locked,
                    flake: "/tmp/hostctl-ifd",
                    rev: "",
                    name,
                    attr: &attr,
                    out: DESIRED,
                    destination: "",
                    ssh_host: "",
                };
                let command = expand(template, &vars);
                let uploaded = crate::remote::interactive_script(&command, true);
                for shell in &shells {
                    for text in [&command, &uploaded] {
                        let mut cmd = shell.command();
                        cmd.args(["-n", "-c", text]);
                        let run = run(cmd, "");
                        assert_eq!(
                            run.code,
                            Some(0),
                            "{} -n: {text}\n{}",
                            shell.name,
                            run.stderr
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn placeholder_values_reach_commands_as_single_words() {
        let template =
            "printf '%s\\0' {flake} {flake_url} {rev} {name} {attr} {out} {destination} {ssh_host}";
        for shell in shells() {
            for value in [
                "it's",
                "$(echo pwned)",
                "`echo pwned`",
                "\"dq\"",
                "back\\slash\\",
                "two  spaces",
                "new\nline",
                "*?[a]",
                ";&|<>",
                "ünï ✓",
                "{name}",
                "",
            ] {
                let vars = TemplateVars {
                    locked: value,
                    flake: value,
                    rev: value,
                    name: value,
                    attr: value,
                    out: value,
                    destination: value,
                    ssh_host: value,
                };
                let mut cmd = shell.command();
                cmd.args(["-c", &expand(template, &vars)]);
                let run = run(cmd, "");
                assert_eq!(run.code, Some(0), "{}: {}", shell.name, run.stderr);
                assert_eq!(
                    run.stdout,
                    format!("{value}\0").repeat(8),
                    "{}: {value:?}",
                    shell.name
                );
            }
        }
    }

    /// A `nix` that records its arguments and "builds" `$FAKE_BUILT`.
    fn fake_nix(dir: &Path) -> PathBuf {
        let bin = dir.join("bin");
        std::fs::create_dir(&bin).unwrap();
        write_executable(
            &bin.join("nix"),
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >\"$TEST_DIR/nix-args\"\nprintf '%s\\n' \"$FAKE_BUILT\"\n",
        );
        bin
    }

    #[test]
    fn home_manager_default_activates_only_the_compared_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = fake_nix(tmp.path());
        let generation = tmp.path().join("generation");
        std::fs::create_dir(&generation).unwrap();
        write_executable(
            &generation.join("activate"),
            "#!/bin/sh\n: >\"$TEST_DIR/activated\"\n",
        );
        let activated = tmp.path().join("activated");
        let out = generation.to_str().unwrap();
        let attr = Kind::HomeManager.default_attr("jj@mac").unwrap();
        let vars = TemplateVars {
            locked: GITHUB_LOCKED,
            flake: "github:jjant/nix-config",
            rev: "42ea902131b93c04d098d3bcd02f29305b4bad7c",
            name: "jj@mac",
            attr: &attr,
            out,
            destination: "",
            ssh_host: "",
        };
        let command = expand(Kind::HomeManager.default_activate().unwrap(), &vars);
        for shell in shells() {
            let activate = |built: &str| {
                let mut cmd = shell.command();
                cmd.args(["-c", &command])
                    .env("PATH", search_path(std::slice::from_ref(&bin)))
                    .env("TEST_DIR", tmp.path())
                    .env("FAKE_BUILT", built);
                run(cmd, "")
            };
            let ran = activate(out);
            assert_eq!(ran.code, Some(0), "{}: {}", shell.name, ran.stderr);
            assert!(activated.exists(), "{}", shell.name);
            let args = std::fs::read_to_string(tmp.path().join("nix-args")).unwrap();
            assert_eq!(
                args.lines().collect::<Vec<_>>(),
                [
                    "--extra-experimental-features",
                    "nix-command flakes",
                    "build",
                    "--no-link",
                    "--print-out-paths",
                    "--accept-flake-config",
                    &format!("{GITHUB_LOCKED}#.homeConfigurations.\"jj@mac\".activationPackage"),
                ],
                "{}",
                shell.name
            );
            std::fs::remove_file(&activated).unwrap();

            let refused = activate(DESIRED);
            assert_ne!(refused.code, Some(0), "{}", shell.name);
            assert!(
                !activated.exists(),
                "{}: activated another generation",
                shell.name
            );
        }
    }

    #[test]
    fn build_command_hands_nix_one_installable() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = fake_nix(tmp.path());
        let jj = host("jj@mac", Kind::HomeManager, local());
        let command = build_command(&jj, &path_flake()).unwrap();
        for shell in shells() {
            let mut cmd = shell.command();
            cmd.args(["-c", &command])
                .env("PATH", search_path(std::slice::from_ref(&bin)))
                .env("TEST_DIR", tmp.path())
                .env("FAKE_BUILT", DESIRED);
            let ran = run(cmd, "");
            assert_eq!(ran.code, Some(0), "{}: {}", shell.name, ran.stderr);
            assert_eq!(ran.stdout, format!("{DESIRED}\n"));
            let args = std::fs::read_to_string(tmp.path().join("nix-args")).unwrap();
            assert_eq!(
                args.lines().collect::<Vec<_>>(),
                [
                    "--extra-experimental-features",
                    "nix-command flakes",
                    "build",
                    "--no-link",
                    "--print-out-paths",
                    "--accept-flake-config",
                    "--no-write-lock-file",
                    &format!(
                        "{}#.homeConfigurations.\"jj@mac\".activationPackage",
                        path_flake().locked
                    ),
                ],
                "{}",
                shell.name
            );
        }
    }

    #[test]
    fn scripts_pass_shellcheck() {
        let Some(shellcheck) = find_program("shellcheck") else {
            eprintln!("skipping: shellcheck isn't on PATH");
            return;
        };
        let (a, odd) = (store_path(1, "a"), store_path(2, "odd?name=1"));
        let mut scripts = vec![closure_script(&[&a, &odd]), closure_script(&[])];
        let locked = path_flake().locked;
        for kind in Kind::ALL {
            if let Some(template) = kind.default_activate() {
                let attr = kind.default_attr("jj@mac").unwrap();
                let vars = TemplateVars {
                    locked: &locked,
                    attr: &attr,
                    out: DESIRED,
                    ..vars()
                };
                scripts.push(expand(template, &vars));
            }
        }
        let jj = host("jj@mac", Kind::HomeManager, local());
        scripts.push(build_command(&jj, &github()).unwrap());
        for script in scripts {
            let mut cmd = Command::new(&shellcheck);
            cmd.args(["--shell=sh", "--severity=style", "-"]);
            let run = run(cmd, &script);
            assert_eq!(run.code, Some(0), "{}\n{script}", run.stdout);
        }
    }
}
