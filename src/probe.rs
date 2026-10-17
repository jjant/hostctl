//! Reading a host's active generation: hostctl's builtin probe script, the
//! parser for its framed output (and for forced-command routes' answers),
//! and the probe command for a host. See DESIGN.md ("Probing the current
//! generation").

use std::fmt::Write as _;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;

use crate::config::{Host, Kind, SshConfig, Target};
use crate::remote::{self, CommandSpec, Mux};
use crate::storepath::StorePath;
use crate::util;

/// Cap on a probe's captured output.
pub const MAX_OUTPUT: usize = 1 << 20;

/// What a probe found.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeReport {
    /// The active generation; `None` when the host has none (never activated).
    pub current: Option<StorePath>,
    /// Why there is no generation (the script's `none=` line).
    pub none_reason: Option<String>,
    /// `x86_64-linux`, `aarch64-darwin`, … (from `uname`), when reported.
    pub platform: Option<String>,
    /// NixOS: the toplevel whose specialisation `current` is.
    pub toplevel: Option<StorePath>,
    /// NixOS: the active specialisation's name.
    pub specialisation: Option<String>,
    /// What the profile points at, when that isn't the deployed generation
    /// (NixOS/darwin after `boot`, `test`, a rollback or a failed switch).
    pub profile: Option<StorePath>,
    /// The deployed generation's number: the `N` of the profile's
    /// `<name>-<N>-link` it is reached through.
    pub generation: Option<u64>,
    /// When `current` was activated (unix seconds): the mtime of the
    /// profile / `current-system` symlink itself.
    pub activated: Option<i64>,
    /// The flake revision the generation was built from
    /// (`configurationRevision`), when it records one.
    pub revision: Option<String>,
    /// NixOS: the booted generation.
    pub booted: Option<StorePath>,
    /// NixOS: booted kernel, initrd or kernel modules differ from current.
    pub reboot_required: bool,
}

impl ProbeReport {
    /// What to compare against the desired generation: the parent toplevel
    /// of an active specialisation, else `current`.
    pub fn deployed(&self) -> Option<&StorePath> {
        self.toplevel.as_ref().or(self.current.as_ref())
    }
}

/// How a probe's output must be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// hostctl's script: framed by `hostctl-probe=1 nonce=<n>` … `end=<n>`.
    Builtin { nonce: String },
    /// A forced-command route's answer: the protocol's `key=value` lines
    /// without framing, or just a store path on a line of its own.
    Command,
}

/// The result of parsing probe output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    Report(Box<ProbeReport>),
    /// The frame started but never ended: the connection dropped or timed
    /// out after the host answered.
    Incomplete,
    /// Nothing usable (no frame / no store path / bad values); the reason.
    Invalid(String),
}

/// A fresh nonce for one probe run: 16 hex chars from the time, the pid and
/// a counter (uniqueness, not secrecy).
pub fn nonce() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seed = format!("{time}:{}:{n}", std::process::id());
    format!("{:016x}", crate::util::fnv1a64(seed.as_bytes()))
}

/// The POSIX sh script that reports the active generation of a `kind`
/// host, reading `profile` (absolute, or starting with `$HOME/`) instead of
/// the kind's default location when given, framed with `nonce`.
///
/// Output (one `key=value` per line; see DESIGN.md): `hostctl-probe=1
/// nonce=<nonce>`, then `platform`, `current` or `none=<why>`, `toplevel` +
/// `specialisation`, `profile`, `generation`, `activated`, `revision`,
/// `booted`, `reboot=1`, then `end=<nonce>`. Exits 0 once it runs, quiet on
/// stderr. It is a single `{ … }` group, so a truncated upload runs nothing.
///
/// Requirements:
/// - portable: dash, `bash --posix`, busybox sh, macOS sh; only `uname`,
///   `readlink` (no `-f`), `stat` (GNU `-c %Y`, else BSD `-f %m`), `sed`
///   (darwin) and `id` (without `$USER`); one `readlink` per symlink hop and
///   no process per generation (NixOS scans compare with `test -ef`);
/// - every absolute path is prefixed with `${HOSTCTL_PROBE_ROOT}`, and so
///   are absolute symlink targets outside it (so a chroot store, `--store
///   local?root=…`, works as a root); reported paths are as seen inside it;
///   `$HOME`, `$XDG_STATE_HOME` and `$USER` (fallback `id -un`) come from
///   the environment;
/// - symlinks resolved with a bounded `readlink` loop (relative targets
///   too); a dangling link is `none=<link> dangles`, a missing one
///   `none=<link> does not exist`;
/// - home-manager: `profile`, else the first existing of
///   `${XDG_STATE_HOME:-$HOME/.local/state}/nix/profiles/home-manager`,
///   `/nix/var/nix/profiles/per-user/$USER/home-manager` (`none=no
///   home-manager profile`);
/// - nixos/darwin: `current` = `/run/current-system`, profile =
///   `/nix/var/nix/profiles/system` (or `profile`), reported as `profile=`
///   only when it isn't the deployed generation. When `current` isn't the
///   profile's target, the newest `system-<N>-link` to it gives the
///   generation; failing that (nixos), the newest
///   `system-<N>-link/specialisation/<name>` to it gives `toplevel`,
///   `specialisation` and `generation`. nixos: `booted`, `reboot` (booted
///   kernel, initrd or kernel-modules ≠ current's);
/// - custom: `profile` only (`none=no profile configured` without one);
/// - `revision`: darwin `$current/darwin-version.json`
///   (`configurationRevision`, extracted with sed), nixos `$current/sw/bin/nixos-version
///   --configuration-revision`, else any kind `$current/configuration-revision`;
/// - `generation`: the `N` of the `<name>-<N>-link` through which the
///   deployed generation is reached;
/// - `activated`: lstat mtime of the profile link (home-manager, custom) or
///   of `/run/current-system`.
pub fn builtin_script(kind: Kind, profile: Option<&str>, nonce: &str) -> String {
    let nonce = remote::sh_quote(nonce);
    let mut s = String::with_capacity(3072);
    s.push_str(PRELUDE);
    let _ = writeln!(s, "printf 'hostctl-probe=1 nonce=%s\\n' {nonce}");
    s.push_str(HELPERS);
    match (kind, profile) {
        (Kind::Nixos | Kind::Darwin, _) => {
            s.push_str("C=$R/run/current-system P=");
            match profile {
                Some(profile) => push_path(&mut s, profile),
                None => s.push_str("$R/nix/var/nix/profiles/system"),
            }
            s.push('\n');
            s.push_str(SYSTEM_HEAD);
            let nixos = kind == Kind::Nixos;
            if nixos {
                s.push_str(NIXOS_SPECIALISATIONS);
            }
            s.push_str(SYSTEM_REPORT);
            if nixos {
                s.push_str(NIXOS_REVISION);
                s.push_str(NIXOS_BOOTED);
            } else {
                s.push_str(DARWIN_REVISION);
            }
            s.push_str(SYSTEM_TAIL);
        }
        (Kind::HomeManager | Kind::Custom, Some(profile)) => {
            s.push_str("p=");
            push_path(&mut s, profile);
            s.push('\n');
            s.push_str(PROFILE_HEAD);
            s.push_str("kp none \"$p\" ' does not exist'");
            s.push_str(PROFILE_TAIL);
        }
        (Kind::HomeManager, None) => {
            s.push_str(HOME_MANAGER_PROFILE);
            s.push_str(PROFILE_HEAD);
            s.push_str("kv none 'no home-manager profile'");
            s.push_str(PROFILE_TAIL);
        }
        (Kind::Custom, None) => s.push_str("kv none 'no profile configured'\n"),
    }
    let _ = write!(s, "printf 'end=%s\\n' {nonce}\n}}\n");
    s
}

/// Append `path` as one shell word: `$HOME/…` (or `~/…`) expands at run
/// time, an absolute path is taken inside the probe root.
fn push_path(s: &mut String, path: &str) {
    let home = path
        .strip_prefix("$HOME/")
        .or_else(|| path.strip_prefix("~/"));
    if let Some(rest) = home {
        s.push_str("\"$HOME\"/");
        s.push_str(&remote::sh_quote(rest));
        return;
    }
    // Config only allows absolute (or `~/`) profiles; `./` keeps a relative
    // one meaning what it says.
    let base = if path.starts_with('/') {
        "\"$R\""
    } else {
        "./"
    };
    s.push_str(base);
    s.push_str(&remote::sh_quote(path));
}

// The script, piece by piece. The `{` group makes the shell read all of it
// before running any of it: a truncated upload runs nothing, and nothing it
// runs can read the rest of stdin. `set +efu` undoes options bash may
// inherit (an exported SHELLOPTS). Values are printed one per line (`kv`
// cuts at the first newline), paths as seen inside the root `$R` (`kp`).

const PRELUDE: &str = r#"{
set +efu
exec 2>/dev/null
R=${HOSTCTL_PROBE_ROOT%/} nl='
'
"#;

/// `follow` leaves the final target in `$t` and, in `$g`, the `N` of the
/// first `<name>-<N>-link` on the way. Real chains are at most 3 hops
/// (default -> per-user profile -> generation -> store); 16 ends loops fast.
/// `activated` tries GNU `stat` (BSD's rejects `-c`); both lstat by default.
/// `revision` prints its argument, else the generation's
/// `configuration-revision` file.
const HELPERS: &str = r##"kv() { printf '%s=%s\n' "$1" "${2%%"$nl"*}"; }
kp() {
	case $2 in "$R"/*) set -- "$1" "${2#"$R"}" "$3" ;; esac
	kv "$1" "$2$3"
}
follow() {
	t=$1 g='' i=0
	while [ -L "$t" ]; do
		[ "$i" -lt 16 ] || return 1
		i=$((i + 1))
		l=$(readlink "$t") || return 1
		case $l in
		"$R"/*) t=$l ;;
		/*) t=$R$l ;;
		*) t=${t%/*}/$l ;;
		esac
		if [ -z "$g" ]; then
			g=${t##*/}
			case $g in *-link) g=${g%-link} && g=${g##*-} ;; *) g= ;; esac
			case $g in *[!0-9]*) g= ;; esac
		fi
	done
	[ -e "$t" ]
}
activated() {
	a=$(stat -c %Y "$1") || a=$(stat -f %m "$1") || a=
	case $a in '' | *[!0-9]*) ;; *) kv activated "$a" ;; esac
}
revision() {
	v=$2
	[ -n "$v" ] || [ ! -r "$1/configuration-revision" ] || IFS= read -r v <"$1/configuration-revision"
	[ -z "$v" ] || kv revision "$v"
}
u=$(uname -sm)
case ${u##* } in arm64) m=aarch64 ;; amd64) m=x86_64 ;; *) m=${u##* } ;; esac
case ${u%% *} in Linux) o=linux ;; Darwin) o=darwin ;; FreeBSD) o=freebsd ;; *) o=${u%% *} ;; esac
[ -z "$u" ] || kv platform "$m-$o"
"##;

/// home-manager's default profiles; `$p` is read by `PROFILE_*`.
const HOME_MANAGER_PROFILE: &str = r#"p=${XDG_STATE_HOME:-$HOME/.local/state}/nix/profiles/home-manager
[ -L "$p" ] || [ -e "$p" ] || p=$R/nix/var/nix/profiles/per-user/${USER:-$(id -un)}/home-manager
"#;

/// The generation profile `$p` points at; the missing-profile line goes
/// between head and tail.
const PROFILE_HEAD: &str = "if [ ! -L \"$p\" ] && [ ! -e \"$p\" ]; then\n\t";
const PROFILE_TAIL: &str = r#"
elif follow "$p"; then
	kp current "$t"
	[ -z "$g" ] || kv generation "$g"
	activated "$p"
	revision "$t"
else
	kp none "$p" ' dangles'
fi
"#;

// NixOS/darwin: `$C` is current-system, `$P` the system profile. When the
// profile doesn't point at current, the generation links are scanned with
// `-ef` (same file, symlinks followed; no process per link), newest wins:
// generations may repeat a path, and glob order isn't numeric.
const SYSTEM_HEAD: &str = r#"newer() {
	n=${1%-link}
	n=${n##*-}
	case $n in '' | *[!0-9]*) return 1 ;; esac
	[ -z "$gen" ] || [ "$n" -gt "$gen" ]
}
if follow "$C"; then
	cur=$t gen='' top='' spec='' prof=''
	if follow "$P"; then
		prof=$t
		[ "$prof" != "$cur" ] || gen=$g
	fi
	if [ -z "$gen" ]; then
		for l in "$P"-*-link; do
			if [ "$l" -ef "$cur" ] && newer "$l"; then gen=$n; fi
		done
	fi
"#;
const NIXOS_SPECIALISATIONS: &str = r#"	if [ -z "$gen" ]; then
		for s in "$P"-*-link/specialisation/*; do
			l=${s%/specialisation/*}
			if [ "$s" -ef "$cur" ] && newer "$l"; then gen=$n top=$l spec=${s##*/}; fi
		done
	fi
"#;
const SYSTEM_REPORT: &str = r#"	kp current "$cur"
	dep=$cur
	if [ -n "$top" ] && follow "$top"; then
		dep=$t
		kp toplevel "$t"
		kv specialisation "$spec"
	fi
	[ -z "$prof" ] || [ "$prof" = "$dep" ] || kp profile "$prof"
	[ -z "$gen" ] || kv generation "$gen"
	activated "$C"
	v=
"#;
const NIXOS_REVISION: &str = r#"	[ ! -x "$cur/sw/bin/nixos-version" ] || v=$("$cur/sw/bin/nixos-version" --configuration-revision </dev/null)
	revision "$cur" "$v"
"#;
const DARWIN_REVISION: &str = r#"	[ ! -r "$cur/darwin-version.json" ] || v=$(sed -n 's/.*"configurationRevision"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$cur/darwin-version.json")
	revision "$cur" "$v"
"#;
const NIXOS_BOOTED: &str = r#"	B=$R/run/booted-system
	if follow "$B"; then
		kp booted "$t"
		for f in kernel initrd kernel-modules; do
			if { [ -e "$B/$f" ] || [ -e "$cur/$f" ]; } && [ ! "$B/$f" -ef "$cur/$f" ]; then
				kv reboot 1
				break
			fi
		done
	fi
"#;
const SYSTEM_TAIL: &str = r#"elif [ -L "$C" ]; then
	kp none "$C" ' dangles'
else
	kp none "$C" ' does not exist'
fi
"#;

/// Parse a probe's stdout according to `mode`. Only `\n`-terminated lines
/// count (a cut-off last line is ignored); `\r` and surrounding whitespace
/// are trimmed; unknown keys are ignored; the first occurrence of a key
/// wins; values are validated (store paths, numbers, `reboot` 0/1), text
/// values are sanitized for the terminal. Builtin: lines before the frame
/// (login-shell noise, which may even share the header's line) and after
/// `end` are ignored; no frame → `Invalid`; frame without `end` →
/// `Incomplete`. Command: protocol lines if any `current=`/`none=` line is
/// present, else the first line that is a store path. An answer without
/// `current` carries no `toplevel`/`specialisation`.
pub fn parse(stdout: &str, mode: &Mode) -> Parsed {
    let mut lines = complete_lines(stdout);
    let mut fields = Fields::default();
    match mode {
        Mode::Builtin { nonce } => {
            if !lines.any(|line| is_header(line, nonce)) {
                return Parsed::Invalid(unexpected(stdout));
            }
            for line in lines {
                if line.strip_prefix("end=") == Some(nonce.as_str()) {
                    return fields.parsed();
                }
                fields.add(line);
            }
            Parsed::Incomplete
        }
        Mode::Command => {
            let mut bare = None;
            for line in lines {
                fields.add(line);
                if bare.is_none() {
                    bare = StorePath::parse(line);
                }
            }
            if fields.answered() {
                fields.parsed()
            } else if let Some(current) = bare {
                Parsed::Report(Box::new(ProbeReport {
                    current: Some(current),
                    ..ProbeReport::default()
                }))
            } else {
                Parsed::Invalid(unexpected(stdout))
            }
        }
    }
}

/// The `\n`-terminated lines of `s`, trimmed.
fn complete_lines(s: &str) -> impl Iterator<Item = &str> {
    s[..s.rfind('\n').map_or(0, |i| i + 1)]
        .lines()
        .map(str::trim)
}

/// `hostctl-probe=1 nonce=<nonce>`, possibly preceded by output that didn't
/// end its line (an rc file setting the terminal title, say).
fn is_header(line: &str, nonce: &str) -> bool {
    line.strip_suffix(nonce)
        .and_then(|l| l.strip_suffix(" nonce="))
        .is_some_and(|l| l.ends_with("hostctl-probe=1"))
}

/// Why output without an answer is unusable, quoting its first line.
fn unexpected(stdout: &str) -> String {
    match stdout.lines().map(str::trim).find(|l| !l.is_empty()) {
        Some(line) => format!("unexpected output: {:?}", util::truncate(line, 80)),
        None => "no output".into(),
    }
}

/// The first value of each protocol key, unvalidated.
#[derive(Default)]
struct Fields<'a> {
    platform: Option<&'a str>,
    current: Option<&'a str>,
    none: Option<&'a str>,
    toplevel: Option<&'a str>,
    specialisation: Option<&'a str>,
    profile: Option<&'a str>,
    generation: Option<&'a str>,
    activated: Option<&'a str>,
    revision: Option<&'a str>,
    booted: Option<&'a str>,
    reboot: Option<&'a str>,
}

impl<'a> Fields<'a> {
    fn add(&mut self, line: &'a str) {
        let Some((key, value)) = line.split_once('=') else {
            return;
        };
        let slot = match key.trim_end() {
            "platform" => &mut self.platform,
            "current" => &mut self.current,
            "none" => &mut self.none,
            "toplevel" => &mut self.toplevel,
            "specialisation" => &mut self.specialisation,
            "profile" => &mut self.profile,
            "generation" => &mut self.generation,
            "activated" => &mut self.activated,
            "revision" => &mut self.revision,
            "booted" => &mut self.booted,
            "reboot" => &mut self.reboot,
            _ => return,
        };
        slot.get_or_insert(value.trim_start());
    }

    /// Whether the generation question was answered at all.
    fn answered(&self) -> bool {
        self.current.is_some() || self.none.is_some()
    }

    fn parsed(&self) -> Parsed {
        match self.report() {
            Ok(report) => Parsed::Report(Box::new(report)),
            Err(why) => Parsed::Invalid(why),
        }
    }

    fn report(&self) -> Result<ProbeReport, String> {
        let current = store_path("current", self.current)?;
        if current.is_none() && self.none.is_none() {
            return Err("the probe reported neither current= nor none=".into());
        }
        let active = current.is_some();
        let toplevel = store_path("toplevel", self.toplevel)?;
        Ok(ProbeReport {
            none_reason: text(self.none).filter(|_| !active),
            platform: text(self.platform),
            toplevel: toplevel.filter(|_| active),
            specialisation: text(self.specialisation).filter(|_| active),
            profile: store_path("profile", self.profile)?,
            generation: number("generation", self.generation)?,
            activated: number("activated", self.activated)?,
            revision: text(self.revision),
            booted: store_path("booted", self.booted)?,
            reboot_required: match self.reboot {
                None | Some("0") => false,
                Some("1") => true,
                Some(value) => return Err(bad("reboot", value)),
            },
            current,
        })
    }
}

fn bad(key: &str, value: &str) -> String {
    format!("bad {key}= value {:?}", util::truncate(value, 80))
}

fn store_path(key: &str, value: Option<&str>) -> Result<Option<StorePath>, String> {
    value
        .map(|v| StorePath::parse(v).ok_or_else(|| bad(key, v)))
        .transpose()
}

fn number<T: FromStr>(key: &str, value: Option<&str>) -> Result<Option<T>, String> {
    value
        .map(|v| v.parse().map_err(|_| bad(key, v)))
        .transpose()
}

fn text(value: Option<&str>) -> Option<String> {
    value
        .filter(|v| !v.is_empty())
        .map(|v| util::sanitize_terminal(v).into_owned())
}

/// The command that probes `host` and how to read its output. Local hosts
/// and normal ssh routes run [`builtin_script`]; a forced-command route
/// sends its command (and parses in [`Mode::Command`]).
pub fn command(host: &Host, ssh: &SshConfig, mux: &Mux) -> (CommandSpec, Mode) {
    let builtin = |target: &Target| {
        let nonce = nonce();
        let script = builtin_script(host.kind, host.profile.as_deref(), &nonce);
        (
            remote::script_command(target, ssh, mux, &script).max_output(MAX_OUTPUT),
            Mode::Builtin { nonce },
        )
    };
    match &host.target {
        Target::Ssh(t) => match &t.forced_command {
            Some(command) => (
                remote::raw_command(t, ssh, mux, command).max_output(MAX_OUTPUT),
                Mode::Command,
            ),
            None => builtin(&host.target),
        },
        Target::Local { .. } => builtin(&host.target),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ActivateOn, Sources, SshTarget};
    use std::collections::BTreeMap;
    use std::fs;
    use std::io::Write as _;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    // ── parse ────────────────────────────────────────────────────────────

    const NONCE: &str = "0123456789abcdef";
    /// bigmem's home-manager generations 9 and 8 (tests/fixtures/closure-hm-*.root).
    const HM9: &str = "/nix/store/ykn4vz9vpqccp1n36p50irs3dwrjfkk6-home-manager-generation";
    const HM8: &str = "/nix/store/afvwn2ldnj8z50fisjnj40xpdq7602y4-home-manager-generation";
    const TOP: &str = "/nix/store/1a2b3c4d5f6g7h8i9j0k1l2m3n4p5q6r-nixos-system-bigmem-25.11";
    const SPEC: &str = "/nix/store/9s8r7q6p5n4m3l2k1j0i9h8g7f6d5c4b-nixos-system-bigmem-25.11";

    fn sp(path: &str) -> StorePath {
        StorePath::parse(path).unwrap()
    }

    fn builtin() -> Mode {
        Mode::Builtin {
            nonce: NONCE.into(),
        }
    }

    fn framed(body: &str) -> String {
        format!("hostctl-probe=1 nonce={NONCE}\n{body}end={NONCE}\n")
    }

    fn report(stdout: &str, mode: &Mode) -> ProbeReport {
        match parse(stdout, mode) {
            Parsed::Report(report) => *report,
            other => panic!("{other:?} from {stdout:?}"),
        }
    }

    fn invalid(stdout: &str, mode: &Mode) -> String {
        match parse(stdout, mode) {
            Parsed::Invalid(why) => why,
            other => panic!("{other:?} from {stdout:?}"),
        }
    }

    fn current(path: &str) -> ProbeReport {
        ProbeReport {
            current: Some(sp(path)),
            ..ProbeReport::default()
        }
    }

    #[test]
    fn parses_every_key_of_a_frame() {
        let out = framed(&format!(
            "platform=x86_64-linux\ncurrent={SPEC}\ntoplevel={TOP}\nspecialisation=gaming\n\
             profile={HM9}\ngeneration=42\nactivated=1792238369\nrevision=42ea902\nbooted={HM8}\nreboot=1\n"
        ));
        assert_eq!(
            report(&out, &builtin()),
            ProbeReport {
                current: Some(sp(SPEC)),
                none_reason: None,
                platform: Some("x86_64-linux".into()),
                toplevel: Some(sp(TOP)),
                specialisation: Some("gaming".into()),
                profile: Some(sp(HM9)),
                generation: Some(42),
                activated: Some(1_792_238_369),
                revision: Some("42ea902".into()),
                booted: Some(sp(HM8)),
                reboot_required: true,
            }
        );
    }

    #[test]
    fn ignores_noise_around_the_frame() {
        let out = format!(
            "Welcome to bigmem!\ncurrent={HM8}\n{}current={HM8}\nend={NONCE}\n",
            framed(&format!("current={HM9}\n"))
        );
        assert_eq!(report(&out, &builtin()), current(HM9));
    }

    #[test]
    fn finds_a_header_glued_to_unterminated_noise() {
        // An rc file that sets the terminal title without a newline.
        let out = format!("\x1b]0;bigmem\x07{}", framed(&format!("current={HM9}\n")));
        assert_eq!(report(&out, &builtin()), current(HM9));
    }

    #[test]
    fn a_frame_without_end_is_incomplete() {
        let header = format!("hostctl-probe=1 nonce={NONCE}\n");
        assert_eq!(parse(&header, &builtin()), Parsed::Incomplete);
        assert_eq!(
            parse(&format!("{header}current={HM9}\n"), &builtin()),
            Parsed::Incomplete
        );
        // The end marker itself cut off.
        let cut = framed(&format!("current={HM9}\n"));
        assert_eq!(parse(cut.trim_end(), &builtin()), Parsed::Incomplete);
        assert_eq!(parse(&cut[..cut.len() - 3], &builtin()), Parsed::Incomplete);
        // Someone else's end doesn't close ours.
        let other = format!("{header}current={HM9}\nend=ffffffffffffffff\n");
        assert_eq!(parse(&other, &builtin()), Parsed::Incomplete);
    }

    #[test]
    fn only_our_nonce_frames_an_answer() {
        let theirs = format!(
            "hostctl-probe=1 nonce=ffffffffffffffff\ncurrent={HM8}\nend=ffffffffffffffff\n"
        );
        assert_eq!(
            invalid(&theirs, &builtin()),
            "unexpected output: \"hostctl-probe=1 nonce=ffffffffffffffff\""
        );
        let both = format!("{theirs}{}", framed(&format!("current={HM9}\n")));
        assert_eq!(report(&both, &builtin()), current(HM9));
        let no_version = format!("hostctl-probe=2 nonce={NONCE}\ncurrent={HM9}\nend={NONCE}\n");
        assert!(matches!(parse(&no_version, &builtin()), Parsed::Invalid(_)));
    }

    #[test]
    fn output_without_a_frame_is_invalid() {
        assert_eq!(invalid("", &builtin()), "no output");
        assert_eq!(invalid(" \n\r\n", &builtin()), "no output");
        assert_eq!(
            invalid("This account is currently not available.\n", &builtin()),
            "unexpected output: \"This account is currently not available.\""
        );
        // Even an unterminated line is quoted, escaped and shortened.
        let why = invalid(&format!("\x1b[31m{}", "x".repeat(200)), &builtin());
        assert!(
            why.starts_with("unexpected output: \"\\u{1b}[31mxxx"),
            "{why}"
        );
        assert!(why.ends_with("…\""), "{why}");
        assert!(why.len() < 120, "{why}");
    }

    #[test]
    fn reads_crlf_and_surrounding_whitespace() {
        let out = framed(&format!("current={HM9}\ngeneration=9\n"))
            .replace('\n', "\r\n")
            .replace("generation=9", "  generation = 9 \t");
        assert_eq!(
            report(&out, &builtin()),
            ProbeReport {
                generation: Some(9),
                ..current(HM9)
            }
        );
    }

    #[test]
    fn rejects_invalid_values() {
        let path_in_object = format!("{}/bin/home-manager", HM9);
        for (line, key) in [
            ("current=generation-9".to_string(), "current"),
            (format!("current={path_in_object}"), "current"),
            ("current=".to_string(), "current"),
            (
                format!("current={HM9}\ntoplevel=/run/current-system"),
                "toplevel",
            ),
            (
                format!("current={HM9}\nprofile=/nix/var/nix/profiles/system"),
                "profile",
            ),
            (format!("current={HM9}\nbooted=yes"), "booted"),
            (format!("current={HM9}\ngeneration=nine"), "generation"),
            (format!("current={HM9}\ngeneration=-1"), "generation"),
            (format!("current={HM9}\nactivated=1.5"), "activated"),
            (format!("current={HM9}\nreboot=yes"), "reboot"),
            (format!("current={HM9}\nreboot="), "reboot"),
        ] {
            let why = invalid(&framed(&format!("{line}\n")), &builtin());
            assert!(
                why.starts_with(&format!("bad {key}= value ")),
                "{line}: {why}"
            );
        }
        assert_eq!(
            invalid(&framed("platform=x86_64-linux\n"), &builtin()),
            "the probe reported neither current= nor none="
        );
        assert_eq!(
            invalid(&framed(""), &builtin()),
            "the probe reported neither current= nor none="
        );
    }

    #[test]
    fn the_first_occurrence_of_a_key_wins() {
        let out = framed(&format!(
            "current={HM9}\ncurrent={HM8}\ngeneration=9\ngeneration=8\n"
        ));
        assert_eq!(
            report(&out, &builtin()),
            ProbeReport {
                generation: Some(9),
                ..current(HM9)
            }
        );
        // Even when the first one is bad.
        let out = framed(&format!("current=bad\ncurrent={HM9}\n"));
        assert_eq!(invalid(&out, &builtin()), "bad current= value \"bad\"");
    }

    #[test]
    fn ignores_unknown_keys_and_other_lines() {
        let out = framed(&format!(
            "\nweather=sunny\nno equals sign\n=orphan value\ncurrent={HM9}\nfuture-key=1\nreboot=0\n"
        ));
        assert_eq!(report(&out, &builtin()), current(HM9));
    }

    #[test]
    fn none_answers_carry_their_reason() {
        let out = framed("platform=aarch64-linux\nnone=no home-manager profile\n");
        assert_eq!(
            report(&out, &builtin()),
            ProbeReport {
                none_reason: Some("no home-manager profile".into()),
                platform: Some("aarch64-linux".into()),
                ..ProbeReport::default()
            }
        );
        assert_eq!(
            report(&framed("none=\n"), &builtin()),
            ProbeReport::default()
        );
        let r = report(&framed("none=a=b\n"), &builtin());
        assert_eq!(r.none_reason.as_deref(), Some("a=b"));
        // Without a current generation there is no specialisation of it.
        let out = framed(&format!(
            "none=gone\ntoplevel={TOP}\nspecialisation=gaming\n"
        ));
        assert_eq!(report(&out, &builtin()).deployed(), None);
        // With one, the reason is moot.
        let out = framed(&format!("none=gone\ncurrent={HM9}\n"));
        assert_eq!(report(&out, &builtin()), current(HM9));
    }

    #[test]
    fn text_values_are_sanitized() {
        let out = framed(&format!(
            "current={HM9}\nrevision=42ea902\x1b]52;c;eA==\x07\nspecialisation=\u{202e}gaming\nplatform=\n"
        ));
        let r = report(&out, &builtin());
        assert_eq!(
            r.revision.as_deref(),
            Some("42ea902\\u{1b}]52;c;eA==\\u{7}")
        );
        assert_eq!(r.specialisation.as_deref(), Some("\\u{202e}gaming"));
        assert_eq!(r.platform, None);
    }

    #[test]
    fn command_mode_takes_a_bare_store_path() {
        // mac-open-recv's `current-system` answer: `readlink /run/current-system`.
        let darwin = "/nix/store/8ggr67sci43hlp8ba6rhjrdhhj5490am-darwin-system-25.11.4cff07d";
        assert_eq!(
            report(&format!("{darwin}\n"), &Mode::Command),
            current(darwin)
        );
        // The first line that is a store path, after any noise.
        let out = format!("Last login: Fri Oct 17\n{darwin}\r\n{HM9}\n");
        assert_eq!(report(&out, &Mode::Command), current(darwin));
        // A cut-off path is not an answer (it could still parse as a shorter path).
        assert_eq!(
            invalid(&darwin[..60], &Mode::Command),
            format!("unexpected output: {:?}", &darwin[..60])
        );
    }

    #[test]
    fn command_mode_reads_protocol_lines_without_a_frame() {
        let out = format!("platform=aarch64-darwin\ncurrent={HM9}\ngeneration=93\n");
        assert_eq!(
            report(&out, &Mode::Command),
            ProbeReport {
                platform: Some("aarch64-darwin".into()),
                generation: Some(93),
                ..current(HM9)
            }
        );
        // The protocol answer wins over a bare path.
        let out = format!("{HM8}\ncurrent={HM9}\n");
        assert_eq!(report(&out, &Mode::Command), current(HM9));
        assert_eq!(
            report("none=not a nix-darwin host\n", &Mode::Command)
                .none_reason
                .as_deref(),
            Some("not a nix-darwin host")
        );
        let out = format!("current=garbage\n{HM9}\n");
        assert_eq!(
            invalid(&out, &Mode::Command),
            "bad current= value \"garbage\""
        );
    }

    #[test]
    fn command_mode_rejects_garbage() {
        assert_eq!(invalid("", &Mode::Command), "no output");
        assert_eq!(
            invalid("mac-open-recv: unknown mode\n", &Mode::Command),
            "unexpected output: \"mac-open-recv: unknown mode\""
        );
        assert_eq!(
            invalid("/run/current-system\n", &Mode::Command),
            "unexpected output: \"/run/current-system\""
        );
        // A frame is just more lines in command mode.
        let out = framed(&format!("current={HM9}\n"));
        assert_eq!(report(&out, &Mode::Command), current(HM9));
    }

    #[test]
    fn deployed_prefers_the_specialisations_toplevel() {
        assert_eq!(current(SPEC).deployed(), Some(&sp(SPEC)));
        let r = ProbeReport {
            toplevel: Some(sp(TOP)),
            ..current(SPEC)
        };
        assert_eq!(r.deployed(), Some(&sp(TOP)));
        assert_eq!(ProbeReport::default().deployed(), None);
    }

    #[test]
    fn reports_serialize_in_camel_case() {
        let r = ProbeReport {
            none_reason: Some("no home-manager profile".into()),
            reboot_required: true,
            ..ProbeReport::default()
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["noneReason"], "no home-manager profile");
        assert_eq!(json["rebootRequired"], true);
        assert_eq!(json["current"], serde_json::Value::Null);
    }

    #[test]
    fn nonces_are_fresh_hex() {
        let nonces: std::collections::HashSet<String> = (0..1000).map(|_| nonce()).collect();
        assert_eq!(nonces.len(), 1000);
        for n in &nonces {
            assert_eq!(n.len(), 16, "{n}");
            assert!(
                n.bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
                "{n}"
            );
        }
    }

    // ── command ──────────────────────────────────────────────────────────

    fn host(target: Target, profile: Option<&str>) -> Host {
        Host {
            name: "web1".into(),
            kind: Kind::HomeManager,
            flake: "github:jjant/nix-config".into(),
            attr: "homeConfigurations.web1.activationPackage".into(),
            target,
            profile: profile.map(Into::into),
            activate: None,
            activate_on: ActivateOn::Target,
            optional: false,
            note: None,
            sources: Sources::default(),
        }
    }

    fn ssh(forced_command: Option<&str>) -> Target {
        Target::Ssh(SshTarget {
            destination: "jj@web1".into(),
            user: Some("jj".into()),
            host: "web1".into(),
            port: None,
            options: vec![],
            forced_command: forced_command.map(Into::into),
        })
    }

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn local_hosts_run_the_script_with_sh() {
        let local = host(Target::Local { assumed: false }, Some("$HOME/p"));
        let (spec, mode) = command(&local, &SshConfig::default(), &Mux::Off);
        let Mode::Builtin { nonce } = &mode else {
            panic!("{mode:?}")
        };
        assert_eq!(spec.program, "sh");
        assert_eq!(spec.args, strings(&["-s"]));
        assert_eq!(
            spec.stdin,
            Some(builtin_script(Kind::HomeManager, Some("$HOME/p"), nonce))
        );
        assert_eq!(spec.max_output, MAX_OUTPUT);
        let (_, again) = command(&local, &SshConfig::default(), &Mux::Off);
        assert_ne!(again, mode, "every probe gets a fresh nonce");
    }

    #[test]
    fn ssh_hosts_get_the_script_on_stdin() {
        let (spec, mode) = command(&host(ssh(None), None), &SshConfig::default(), &Mux::Off);
        let Mode::Builtin { nonce } = &mode else {
            panic!("{mode:?}")
        };
        assert_eq!(spec.program, "ssh");
        assert!(
            spec.args
                .ends_with(&strings(&["--", "jj@web1", "sh", "-s"])),
            "{:?}",
            spec.args
        );
        assert_eq!(
            spec.stdin,
            Some(builtin_script(Kind::HomeManager, None, nonce))
        );
        assert_eq!(spec.max_output, MAX_OUTPUT);
    }

    #[test]
    fn forced_command_routes_send_their_command() {
        let (spec, mode) = command(
            &host(ssh(Some("current-system")), None),
            &SshConfig::default(),
            &Mux::Off,
        );
        assert_eq!(mode, Mode::Command);
        assert!(
            spec.args
                .ends_with(&strings(&["--", "jj@web1", "current-system"])),
            "{:?}",
            spec.args
        );
        assert_eq!(spec.stdin, None);
        assert_eq!(spec.max_output, MAX_OUTPUT);
    }

    // ── builtin_script: shape ────────────────────────────────────────────

    #[test]
    fn scripts_are_one_group_framed_by_the_nonce() {
        for kind in Kind::ALL {
            for profile in [None, Some("$HOME/p")] {
                let s = builtin_script(kind, profile, NONCE);
                assert!(s.starts_with("{\nset +efu\nexec 2>/dev/null\n"), "{kind}");
                assert!(
                    s.ends_with(&format!("printf 'end=%s\\n' {NONCE}\n}}\n")),
                    "{kind}"
                );
                assert_eq!(s.matches(NONCE).count(), 2, "{kind}");
                for banned in ["local ", "[[ ", "readlink -f", "realpath"] {
                    assert!(!s.contains(banned), "{kind}: {banned}");
                }
            }
        }
        // A nonce is data, not code.
        let s = builtin_script(Kind::Custom, None, "x'; rm -rf /; '");
        assert!(
            s.contains("printf 'end=%s\\n' 'x'\\''; rm -rf /; '\\'''\n"),
            "{s}"
        );
    }

    #[test]
    fn profiles_are_embedded_as_one_word() {
        let p = |kind, profile| builtin_script(kind, Some(profile), NONCE);
        assert!(p(Kind::Custom, "$HOME/my 'p'").contains("\np=\"$HOME\"/'my '\\''p'\\'''\n"));
        assert!(p(Kind::Custom, "~/p").contains("\np=\"$HOME\"/p\n"));
        assert!(p(Kind::HomeManager, "/opt/p").contains("\np=\"$R\"/opt/p\n"));
        assert!(p(Kind::Nixos, "/a b/system").contains("P=\"$R\"'/a b/system'\n"));
        assert!(p(Kind::Darwin, "$HOME/sys").contains("P=\"$HOME\"/sys\n"));
        assert!(p(Kind::Custom, "rel/p").contains("\np=./rel/p\n"));
        // A profile replaces the defaults.
        assert!(!p(Kind::HomeManager, "/opt/p").contains("XDG_STATE_HOME"));
    }

    #[test]
    fn scripts_only_carry_what_their_kind_needs() {
        let nixos = builtin_script(Kind::Nixos, None, NONCE);
        let darwin = builtin_script(Kind::Darwin, None, NONCE);
        let hm = builtin_script(Kind::HomeManager, None, NONCE);
        assert!(nixos.contains("/specialisation/*") && nixos.contains("booted-system"));
        assert!(nixos.contains("nixos-version") && !nixos.contains("darwin-version"));
        assert!(!darwin.contains("/specialisation/*") && !darwin.contains("booted-system"));
        assert!(darwin.contains("darwin-version.json") && !darwin.contains("nixos-version"));
        assert!(!hm.contains("current-system") && !hm.contains("sed "));
        for s in [&nixos, &darwin, &hm] {
            assert!(s.len() < 3000, "{}", s.len());
        }
    }

    // ── builtin_script: run for real against fake trees ─────────────────

    /// Every fake symlink's own mtime, so `activated=` is deterministic.
    const LINK_MTIME: i64 = 1_700_000_000;

    struct Shell {
        name: &'static str,
        program: PathBuf,
        args: &'static [&'static str],
    }

    fn which(program: &str) -> Option<PathBuf> {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|dir| dir.join(program))
            .find(|path| {
                path.metadata()
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            })
    }

    /// The POSIX shells on `PATH` every script runs under. The first four
    /// must exist with `HOSTCTL_TEST_REQUIRE_SHELLS=1` (Nix checks); ksh and
    /// zsh's sh emulation add coverage when present.
    fn shells() -> &'static [Shell] {
        static SHELLS: OnceLock<Vec<Shell>> = OnceLock::new();
        SHELLS.get_or_init(|| {
            let require = std::env::var("HOSTCTL_TEST_REQUIRE_SHELLS").as_deref() == Ok("1");
            let candidates: [(&str, &str, &[&str], bool); 6] = [
                ("sh", "sh", &[], true),
                ("dash", "dash", &[], true),
                ("bash --posix", "bash", &["--posix"], true),
                ("busybox sh", "busybox", &["sh"], true),
                ("ksh", "ksh", &[], false),
                ("zsh --emulate sh", "zsh", &["--emulate", "sh"], false),
            ];
            let mut found = Vec::new();
            for (name, program, args, required) in candidates {
                match which(program) {
                    Some(program) => found.push(Shell {
                        name,
                        program,
                        args,
                    }),
                    None if require && required => {
                        panic!("{name} is not on PATH (HOSTCTL_TEST_REQUIRE_SHELLS=1)")
                    }
                    None => eprintln!("probe tests: {name} not on PATH, skipped"),
                }
            }
            found
        })
    }

    /// Set `path`'s mtime; `follow = false` sets a symlink's own.
    fn set_mtime(path: &Path, secs: i64, follow: bool) {
        use std::os::unix::ffi::OsStrExt;
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: all-zero is a valid timespec; the public fields are set next.
        let mut time: libc::timespec = unsafe { std::mem::zeroed() };
        time.tv_sec = secs as libc::time_t;
        let times = [time, time];
        let flags = if follow { 0 } else { libc::AT_SYMLINK_NOFOLLOW };
        // SAFETY: `c_path` is NUL-terminated and `times` has the two entries
        // utimensat reads.
        let rc = unsafe { libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), flags) };
        assert_eq!(
            rc,
            0,
            "utimensat {path:?}: {}",
            std::io::Error::last_os_error()
        );
    }

    /// A fake host. `root/` is `$HOSTCTL_PROBE_ROOT` (with `$HOME` at
    /// `/home`), `bin/` comes first on `PATH` (shims).
    struct Fake {
        tmp: tempfile::TempDir,
        root: PathBuf,
        env: BTreeMap<String, String>,
    }

    impl Fake {
        fn new() -> Fake {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("root");
            let bin = tmp.path().join("bin");
            fs::create_dir_all(root.join("home")).unwrap();
            fs::create_dir_all(&bin).unwrap();
            let path = std::env::var("PATH").unwrap_or_default();
            let env = BTreeMap::from([
                ("PATH".into(), format!("{}:{path}", bin.display())),
                ("HOME".into(), root.join("home").display().to_string()),
                ("USER".into(), "tester".into()),
                ("HOSTCTL_PROBE_ROOT".into(), root.display().to_string()),
            ]);
            Fake { tmp, root, env }
        }

        /// `path` (as seen inside the root) on the real filesystem.
        fn real(&self, path: &str) -> PathBuf {
            self.root.join(path.trim_start_matches('/'))
        }

        fn set(&mut self, key: &str, value: impl Into<String>) {
            self.env.insert(key.into(), value.into());
        }

        fn unset(&mut self, key: &str) {
            self.env.remove(key);
        }

        /// A store object: the directory `/nix/store/<32 × hash>-<name>`.
        fn store(&self, hash: char, name: &str) -> String {
            let path = format!("/nix/store/{}-{name}", hash.to_string().repeat(32));
            fs::create_dir_all(self.real(&path)).unwrap();
            path
        }

        fn file(&self, at: &str, contents: &str) {
            let at = self.real(at);
            fs::create_dir_all(at.parent().unwrap()).unwrap();
            fs::write(at, contents).unwrap();
        }

        /// An executable sh script at `at` (`bin/` for shims on `PATH`).
        fn script(&self, at: &Path, body: &str) {
            fs::create_dir_all(at.parent().unwrap()).unwrap();
            let sh = which("sh").unwrap();
            util::write_executable(at, &format!("#!{}\n{body}", sh.display()));
        }

        fn shim(&self, name: &str, body: &str) {
            self.script(&self.tmp.path().join("bin").join(name), body);
        }

        /// A symlink at `at`; absolute targets point into the root, relative
        /// ones are kept as they are.
        fn link(&self, at: &str, to: &str) {
            let to = if to.starts_with('/') {
                self.real(to)
            } else {
                PathBuf::from(to)
            };
            self.raw_link(at, &to);
        }

        /// A symlink at `at` with exactly the target `to`.
        fn raw_link(&self, at: &str, to: &Path) {
            let at = self.real(at);
            fs::create_dir_all(at.parent().unwrap()).unwrap();
            symlink(to, &at).unwrap();
            set_mtime(&at, LINK_MTIME, false);
        }

        /// A Nix profile: `<link>-<n>-link -> path` per generation, and
        /// `<link> -> <name>-<current>-link`.
        fn profile(&self, link: &str, generations: &[(u32, &str)], current: u32) {
            let name = link.rsplit('/').next().unwrap();
            for (n, path) in generations {
                self.link(&format!("{link}-{n}-link"), path);
            }
            self.link(link, &format!("{name}-{current}-link"));
        }

        /// A NixOS toplevel whose kernel, initrd and kernel modules are the
        /// store objects with hashes `boot[0..3]`.
        fn nixos(&self, hash: char, boot: [char; 3]) -> String {
            let top = self.store(hash, "nixos-system-bigmem-25.11");
            let kernel = self.store(boot[0], "linux-6.12.41");
            let initrd = self.store(boot[1], "initrd-linux-6.12.41");
            let modules = self.store(boot[2], "linux-6.12.41-modules");
            self.file(&format!("{kernel}/bzImage"), "");
            self.file(&format!("{initrd}/initrd"), "");
            self.link(&format!("{top}/kernel"), &format!("{kernel}/bzImage"));
            self.link(&format!("{top}/initrd"), &format!("{initrd}/initrd"));
            self.link(&format!("{top}/kernel-modules"), &modules);
            top
        }

        fn run(&self, shell: &Shell, script: &str) -> std::process::Output {
            let mut child = Command::new(&shell.program)
                .args(shell.args)
                .arg("-s")
                .env_clear()
                .envs(&self.env)
                .current_dir(self.tmp.path())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            // A shell that stops reading early may close the pipe first.
            let _ = child.stdin.take().unwrap().write_all(script.as_bytes());
            child.wait_with_output().unwrap()
        }

        /// Run `script` under every shell: each must exit 0, stay quiet on
        /// stderr and print exactly what the others print. `None` without
        /// shells.
        fn run_all(&self, script: &str) -> Option<String> {
            let mut agreed: Option<(&str, String)> = None;
            for shell in shells() {
                let out = self.run(shell, script);
                let stdout = String::from_utf8(out.stdout).unwrap();
                let stderr = String::from_utf8_lossy(&out.stderr);
                assert!(
                    out.status.success() && stderr.is_empty(),
                    "{}: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
                    shell.name,
                    out.status
                );
                match &agreed {
                    Some((first, expected)) => {
                        assert_eq!(&stdout, expected, "{} disagrees with {first}", shell.name)
                    }
                    None => agreed = Some((shell.name, stdout)),
                }
            }
            agreed.map(|(_, stdout)| stdout)
        }

        /// The probe's report for `kind`, from every shell.
        fn probe(&self, kind: Kind, profile: Option<&str>) -> Option<ProbeReport> {
            let nonce = nonce();
            let stdout = self.run_all(&builtin_script(kind, profile, &nonce))?;
            match parse(&stdout, &Mode::Builtin { nonce }) {
                Parsed::Report(report) => Some(*report),
                other => panic!("{other:?} from:\n{stdout}"),
            }
        }
    }

    /// The platform every fake host reports, as Nix names this machine.
    fn native() -> ProbeReport {
        let os = match std::env::consts::OS {
            "macos" => "darwin",
            os => os,
        };
        ProbeReport {
            platform: Some(format!("{}-{os}", std::env::consts::ARCH)),
            ..ProbeReport::default()
        }
    }

    fn generation(path: &str, n: u64) -> ProbeReport {
        ProbeReport {
            current: Some(sp(path)),
            generation: Some(n),
            activated: Some(LINK_MTIME),
            ..native()
        }
    }

    fn none(why: &str) -> ProbeReport {
        ProbeReport {
            none_reason: Some(why.into()),
            ..native()
        }
    }

    const HM_PROFILE: &str = "/home/.local/state/nix/profiles/home-manager";

    #[test]
    fn home_manager_reads_the_xdg_profile() {
        // bigmem's profile: home-manager -> home-manager-9-link -> /nix/store/…,
        // with its real generations (tests/fixtures).
        let fake = Fake::new();
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let roots: Vec<(u32, String)> = [1, 5, 8, 9]
            .into_iter()
            .map(|n| {
                let root = fs::read_to_string(fixtures.join(format!("closure-hm-{n}.root")));
                let root = root.unwrap().trim().to_string();
                fs::create_dir_all(fake.real(&root)).unwrap();
                (n, root)
            })
            .collect();
        let generations: Vec<(u32, &str)> = roots.iter().map(|(n, p)| (*n, p.as_str())).collect();
        fake.profile(HM_PROFILE, &generations, 9);
        let Some(r) = fake.probe(Kind::HomeManager, None) else {
            return;
        };
        assert_eq!(r, generation(HM9, 9));
    }

    #[test]
    fn home_manager_honors_xdg_state_home() {
        let mut fake = Fake::new();
        let default = fake.store('a', "home-manager-generation");
        let xdg = fake.store('b', "home-manager-generation");
        fake.profile(HM_PROFILE, &[(1, &default)], 1);
        fake.profile(
            "/state/nix/profiles/home-manager",
            &[(6, &default), (7, &xdg)],
            7,
        );
        let state = fake.real("/state").display().to_string();
        fake.set("XDG_STATE_HOME", state);
        let Some(r) = fake.probe(Kind::HomeManager, None) else {
            return;
        };
        assert_eq!(r, generation(&xdg, 7));
        // Empty means unset.
        fake.set("XDG_STATE_HOME", "");
        assert_eq!(
            fake.probe(Kind::HomeManager, None),
            Some(generation(&default, 1))
        );
    }

    #[test]
    fn home_manager_falls_back_to_the_legacy_per_user_profile() {
        let fake = Fake::new();
        let legacy = fake.store('c', "home-manager-generation");
        fake.profile(
            "/nix/var/nix/profiles/per-user/tester/home-manager",
            &[(3, &legacy)],
            3,
        );
        let Some(r) = fake.probe(Kind::HomeManager, None) else {
            return;
        };
        assert_eq!(r, generation(&legacy, 3));
        // Once the XDG profile exists, it wins.
        let xdg = fake.store('d', "home-manager-generation");
        fake.profile(HM_PROFILE, &[(1, &xdg)], 1);
        assert_eq!(
            fake.probe(Kind::HomeManager, None),
            Some(generation(&xdg, 1))
        );
    }

    #[test]
    fn home_manager_without_user_asks_id() {
        let mut fake = Fake::new();
        fake.unset("USER");
        fake.shim("id", "[ \"$1\" = -un ] && echo shimuser\n");
        let hm = fake.store('f', "home-manager-generation");
        fake.profile(
            "/nix/var/nix/profiles/per-user/shimuser/home-manager",
            &[(2, &hm)],
            2,
        );
        let Some(r) = fake.probe(Kind::HomeManager, None) else {
            return;
        };
        assert_eq!(r, generation(&hm, 2));
    }

    #[test]
    fn follows_relative_and_chained_links() {
        let fake = Fake::new();
        let hm = fake.store('g', "home-manager-generation");
        fake.link("/home/gens/hm-3-link", &hm);
        fake.link(HM_PROFILE, "../../../../gens/hm-3-link");
        let Some(r) = fake.probe(Kind::HomeManager, None) else {
            return;
        };
        assert_eq!(r, generation(&hm, 3));

        // default -> per-user/root/profile -> profile-5-link -> /nix/store/…
        let env = fake.store('h', "user-environment");
        fake.profile(
            "/nix/var/nix/profiles/per-user/root/profile",
            &[(5, &env)],
            5,
        );
        fake.link(
            "/nix/var/nix/profiles/default",
            "/nix/var/nix/profiles/per-user/root/profile",
        );
        assert_eq!(
            fake.probe(Kind::Custom, Some("/nix/var/nix/profiles/default")),
            Some(generation(&env, 5))
        );
    }

    #[test]
    fn absolute_targets_outside_the_root_are_taken_inside_it() {
        // What a chroot store (`--store local?root=…`) leaves: profiles
        // linking to /nix/store/… paths that only exist under its root.
        let fake = Fake::new();
        let hm = fake.store('i', "home-manager-generation");
        fake.raw_link(&format!("{HM_PROFILE}-4-link"), Path::new(&hm));
        fake.link(HM_PROFILE, "home-manager-4-link");
        let Some(r) = fake.probe(Kind::HomeManager, None) else {
            return;
        };
        assert_eq!(r, generation(&hm, 4));
    }

    #[test]
    fn a_trailing_slash_on_the_root_is_ignored() {
        let mut fake = Fake::new();
        let hm = fake.store('j', "home-manager-generation");
        fake.profile(HM_PROFILE, &[(1, &hm)], 1);
        let root = format!("{}/", fake.root.display());
        fake.set("HOSTCTL_PROBE_ROOT", root);
        let Some(r) = fake.probe(Kind::HomeManager, None) else {
            return;
        };
        assert_eq!(r, generation(&hm, 1));
    }

    #[test]
    fn without_a_root_paths_are_taken_as_they_are() {
        // Production: no HOSTCTL_PROBE_ROOT, so links into the temp dir are
        // followed and reported unchanged.
        let mut fake = Fake::new();
        fake.unset("HOSTCTL_PROBE_ROOT");
        let hm = fake.store('r', "home-manager-generation");
        fake.profile(HM_PROFILE, &[(2, &hm)], 2);
        let Some(r) = fake.probe(Kind::HomeManager, None) else {
            return;
        };
        let real = fake.real(&hm).display().to_string();
        assert_eq!(r, generation(&real, 2));
    }

    #[test]
    fn dangling_links_have_no_generation() {
        let fake = Fake::new();
        fake.link(HM_PROFILE, "home-manager-59-link");
        let dangles = none(&format!("{HM_PROFILE} dangles"));
        let Some(r) = fake.probe(Kind::HomeManager, None) else {
            return;
        };
        assert_eq!(r, dangles);
        // The generation link exists, its store path doesn't.
        let gone = format!("/nix/store/{}-home-manager-generation", "k".repeat(32));
        fake.link(&format!("{HM_PROFILE}-59-link"), &gone);
        assert_eq!(fake.probe(Kind::HomeManager, None), Some(dangles.clone()));
        // A broken XDG profile is reported, not skipped for the legacy one.
        let legacy = fake.store('l', "home-manager-generation");
        fake.profile(
            "/nix/var/nix/profiles/per-user/tester/home-manager",
            &[(1, &legacy)],
            1,
        );
        assert_eq!(fake.probe(Kind::HomeManager, None), Some(dangles));
        // NixOS.
        fake.link("/run/current-system", &gone);
        assert_eq!(
            fake.probe(Kind::Nixos, None),
            Some(none("/run/current-system dangles"))
        );
    }

    #[test]
    fn link_chains_are_bounded() {
        // 16 hops still resolve (profiles take at most 3); a loop dangles.
        let fake = Fake::new();
        let env = fake.store('b', "env");
        for n in 0..15 {
            fake.link(&format!("/chain/{n}"), &(n + 1).to_string());
        }
        fake.link("/chain/15", &env);
        let Some(r) = fake.probe(Kind::Custom, Some("/chain/0")) else {
            return;
        };
        assert_eq!(
            r,
            ProbeReport {
                current: Some(sp(&env)),
                activated: Some(LINK_MTIME),
                ..native()
            }
        );
        fake.link("/loop/a", "b");
        fake.link("/loop/b", "/loop/a");
        assert_eq!(
            fake.probe(Kind::Custom, Some("/loop/a")),
            Some(none("/loop/a dangles"))
        );
    }

    #[test]
    fn missing_profiles_have_no_generation() {
        let fake = Fake::new();
        let Some(r) = fake.probe(Kind::HomeManager, None) else {
            return;
        };
        assert_eq!(r, none("no home-manager profile"));
        assert_eq!(
            fake.probe(Kind::HomeManager, Some("$HOME/hm")),
            Some(none("/home/hm does not exist"))
        );
        assert_eq!(
            fake.probe(Kind::Custom, Some("/opt/p")),
            Some(none("/opt/p does not exist"))
        );
        assert_eq!(
            fake.probe(Kind::Custom, None),
            Some(none("no profile configured"))
        );
        for kind in [Kind::Nixos, Kind::Darwin] {
            assert_eq!(
                fake.probe(kind, None),
                Some(none("/run/current-system does not exist"))
            );
        }
    }

    #[test]
    fn custom_profiles_with_awkward_names() {
        let fake = Fake::new();
        let a = fake.store('a', "env");
        let b = fake.store('b', "env");
        let c = fake.store('c', "home-manager-generation");
        // Absolute: inside the root.
        let absolute = "/opt/it's a \"profile\" \\ $HOME/p";
        fake.profile(absolute, &[(3, &a)], 3);
        let Some(r) = fake.probe(Kind::Custom, Some(absolute)) else {
            return;
        };
        assert_eq!(r, generation(&a, 3));
        // `$HOME/…`: expanded on the host, the rest taken literally.
        fake.profile("/home/my profiles/$USER 'p'", &[(4, &b)], 4);
        let in_home = "$HOME/my profiles/$USER 'p'";
        assert_eq!(
            fake.probe(Kind::Custom, Some(in_home)),
            Some(generation(&b, 4))
        );
        // home-manager reads only the configured profile.
        fake.profile(HM_PROFILE, &[(1, &c)], 1);
        assert_eq!(
            fake.probe(Kind::HomeManager, Some(in_home)),
            Some(generation(&b, 4))
        );
    }

    #[test]
    fn a_profile_may_be_a_store_path_itself() {
        let fake = Fake::new();
        let env = fake.store('l', "env");
        let Some(r) = fake.probe(Kind::Custom, Some(&env)) else {
            return;
        };
        assert_eq!(r.current, Some(sp(&env)));
        assert_eq!(r.generation, None);
    }

    #[test]
    fn revision_from_the_generation() {
        let fake = Fake::new();
        let hm = fake.store('m', "home-manager-generation");
        fake.file(&format!("{hm}/configuration-revision"), "42ea902bbbe2\n");
        fake.profile(HM_PROFILE, &[(1, &hm)], 1);
        let Some(r) = fake.probe(Kind::HomeManager, None) else {
            return;
        };
        assert_eq!(r.revision.as_deref(), Some("42ea902bbbe2"));
        // Without a trailing newline too.
        fake.file(&format!("{hm}/configuration-revision"), "42ea902-dirty");
        let r = fake.probe(Kind::HomeManager, None).unwrap();
        assert_eq!(r.revision.as_deref(), Some("42ea902-dirty"));
    }

    #[test]
    fn activated_is_the_links_own_mtime() {
        let fake = Fake::new();
        let hm = fake.store('n', "home-manager-generation");
        fake.profile(HM_PROFILE, &[(1, &hm)], 1);
        set_mtime(&fake.real(HM_PROFILE), 1_600_000_000, false);
        set_mtime(
            &fake.real(&format!("{HM_PROFILE}-1-link")),
            1_610_000_000,
            false,
        );
        set_mtime(&fake.real(&hm), 1_620_000_000, true);
        let Some(r) = fake.probe(Kind::HomeManager, None) else {
            return;
        };
        assert_eq!(r.activated, Some(1_600_000_000));

        let top = fake.nixos('b', ['1', '1', '1']);
        fake.profile("/nix/var/nix/profiles/system", &[(1, &top)], 1);
        fake.link("/run/current-system", &top);
        set_mtime(&fake.real("/run/current-system"), 1_630_000_000, false);
        set_mtime(&fake.real(&top), 1_640_000_000, true);
        let r = fake.probe(Kind::Nixos, None).unwrap();
        assert_eq!(r.activated, Some(1_630_000_000));
    }

    #[test]
    fn bsd_stat_is_used_where_gnu_stat_is_missing() {
        let fake = Fake::new();
        // BSD stat has no -c; `-f %m` is its (lstat) mtime.
        fake.shim(
            "stat",
            "case $1 in\n-f) [ \"$2\" = %m ] && echo 1234567890 ;;\n\
             *) echo \"stat: illegal option -- ${1#-}\" >&2; exit 1 ;;\nesac\n",
        );
        let hm = fake.store('p', "home-manager-generation");
        fake.profile(HM_PROFILE, &[(1, &hm)], 1);
        let Some(r) = fake.probe(Kind::HomeManager, None) else {
            return;
        };
        assert_eq!(r.activated, Some(1_234_567_890));
    }

    #[test]
    fn platforms_are_named_like_nix_systems() {
        for (uname, system) in [
            ("Darwin arm64", "aarch64-darwin"),
            ("Darwin x86_64", "x86_64-darwin"),
            ("Linux aarch64", "aarch64-linux"),
            ("Linux x86_64", "x86_64-linux"),
            ("FreeBSD amd64", "x86_64-freebsd"),
            ("SunOS i86pc", "i86pc-SunOS"),
        ] {
            let fake = Fake::new();
            fake.shim("uname", &format!("[ \"$1\" = -sm ] && echo '{uname}'\n"));
            let Some(r) = fake.probe(Kind::HomeManager, None) else {
                return;
            };
            assert_eq!(r.platform.as_deref(), Some(system), "{uname}");
        }
    }

    #[test]
    fn always_prints_a_frame() {
        // No uname, readlink or stat at all: nothing resolves, yet the frame
        // is complete, stderr quiet and the exit code 0.
        let mut fake = Fake::new();
        let hm = fake.store('q', "home-manager-generation");
        fake.profile(HM_PROFILE, &[(1, &hm)], 1);
        let empty = fake.tmp.path().join("bin").display().to_string();
        fake.set("PATH", empty);
        let Some(r) = fake.probe(Kind::HomeManager, None) else {
            return;
        };
        assert_eq!(
            r,
            ProbeReport {
                none_reason: Some(format!("{HM_PROFILE} dangles")),
                ..ProbeReport::default()
            }
        );
        assert!(fake.probe(Kind::Nixos, None).unwrap().current.is_none());
    }

    #[test]
    fn inherited_shell_options_are_reset() {
        // bash takes options from an exported SHELLOPTS: nounset would stop
        // the script before its frame, noglob would blind the scans.
        let mut fake = Fake::new();
        let parent = fake.nixos('b', ['1', '1', '1']);
        let spec = fake.nixos('s', ['1', '1', '1']);
        fake.link(&format!("{parent}/specialisation/gaming"), &spec);
        fake.profile("/nix/var/nix/profiles/system", &[(1, &parent)], 1);
        fake.link("/run/current-system", &spec);
        fake.set("SHELLOPTS", "errexit:nounset:noglob");
        let Some(r) = fake.probe(Kind::Nixos, None) else {
            return;
        };
        assert_eq!(
            r,
            ProbeReport {
                toplevel: Some(sp(&parent)),
                specialisation: Some("gaming".into()),
                ..generation(&spec, 1)
            }
        );
    }

    #[test]
    fn a_truncated_script_runs_nothing() {
        let fake = Fake::new();
        let script = builtin_script(Kind::Nixos, None, NONCE);
        for shell in shells() {
            for cut in [1, script.len() / 3, script.len() / 2, script.len() - 2] {
                let out = fake.run(shell, &script[..cut]);
                assert!(
                    out.stdout.is_empty() && !out.status.success(),
                    "{} ran a script cut at {cut}: {}",
                    shell.name,
                    String::from_utf8_lossy(&out.stdout)
                );
            }
        }
    }

    #[test]
    fn nixos_running_its_profile() {
        let fake = Fake::new();
        let old = fake.nixos('a', ['1', '1', '1']);
        let new = fake.nixos('b', ['1', '1', '1']);
        fake.profile(
            "/nix/var/nix/profiles/system",
            &[(41, &old), (42, &new)],
            42,
        );
        fake.link("/run/current-system", &new);
        fake.link("/run/booted-system", &old);
        let Some(r) = fake.probe(Kind::Nixos, None) else {
            return;
        };
        // Booted from another generation with the same kernel: no reboot.
        assert_eq!(
            r,
            ProbeReport {
                booted: Some(sp(&old)),
                ..generation(&new, 42)
            }
        );
    }

    #[test]
    fn nixos_needs_a_reboot_when_boot_files_differ() {
        for booted in [['2', '1', '1'], ['1', '2', '1'], ['1', '1', '2']] {
            let fake = Fake::new();
            let old = fake.nixos('a', booted);
            let new = fake.nixos('b', ['1', '1', '1']);
            fake.profile("/nix/var/nix/profiles/system", &[(1, &old), (2, &new)], 2);
            fake.link("/run/current-system", &new);
            fake.link("/run/booted-system", &old);
            let Some(r) = fake.probe(Kind::Nixos, None) else {
                return;
            };
            assert!(r.reboot_required, "{booted:?}");
            assert_eq!(r.booted, Some(sp(&old)));
        }
    }

    #[test]
    fn nixos_reboot_compares_only_boot_files_that_exist() {
        // Containers have no kernel or initrd.
        let fake = Fake::new();
        let old = fake.store('a', "nixos-system-container-25.11");
        let new = fake.store('b', "nixos-system-container-25.11");
        fake.profile("/nix/var/nix/profiles/system", &[(1, &old), (2, &new)], 2);
        fake.link("/run/current-system", &new);
        fake.link("/run/booted-system", &old);
        let Some(r) = fake.probe(Kind::Nixos, None) else {
            return;
        };
        assert!(!r.reboot_required);
        // Gaining a kernel does need one.
        fake.link(
            &format!("{new}/kernel-modules"),
            &fake.store('m', "modules"),
        );
        assert!(fake.probe(Kind::Nixos, None).unwrap().reboot_required);
    }

    #[test]
    fn nixos_specialisation_maps_back_to_its_toplevel() {
        let fake = Fake::new();
        let old = fake.nixos('a', ['1', '1', '1']);
        let parent = fake.nixos('b', ['1', '1', '1']);
        let gaming = fake.nixos('g', ['2', '2', '2']);
        let other = fake.nixos('h', ['1', '1', '1']);
        fake.link(&format!("{parent}/specialisation/gaming"), &gaming);
        fake.link(&format!("{parent}/specialisation/other"), &other);
        fake.profile(
            "/nix/var/nix/profiles/system",
            &[(41, &old), (42, &parent)],
            42,
        );
        fake.link("/run/current-system", &gaming);
        fake.link("/run/booted-system", &gaming);
        let Some(r) = fake.probe(Kind::Nixos, None) else {
            return;
        };
        // The profile points at the parent: nothing pending, so no `profile`.
        assert_eq!(
            r,
            ProbeReport {
                toplevel: Some(sp(&parent)),
                specialisation: Some("gaming".into()),
                booted: Some(sp(&gaming)),
                ..generation(&gaming, 42)
            }
        );
        assert_eq!(r.deployed(), Some(&sp(&parent)));
    }

    #[test]
    fn nixos_specialisation_of_an_older_generation() {
        let fake = Fake::new();
        let old = fake.nixos('a', ['1', '1', '1']);
        let new = fake.nixos('b', ['1', '1', '1']);
        let spec = fake.nixos('s', ['1', '1', '1']);
        fake.link(&format!("{old}/specialisation/no-gpu"), &spec);
        fake.profile(
            "/nix/var/nix/profiles/system",
            &[(41, &old), (42, &new)],
            42,
        );
        fake.link("/run/current-system", &spec);
        let Some(r) = fake.probe(Kind::Nixos, None) else {
            return;
        };
        assert_eq!(
            r,
            ProbeReport {
                toplevel: Some(sp(&old)),
                specialisation: Some("no-gpu".into()),
                profile: Some(sp(&new)),
                ..generation(&spec, 41)
            }
        );
    }

    #[test]
    fn nixos_boot_leaves_the_profile_ahead() {
        // `nixos-rebuild boot`: the profile moved on, current didn't.
        let fake = Fake::new();
        let running = fake.nixos('a', ['1', '1', '1']);
        let staged = fake.nixos('b', ['2', '2', '2']);
        fake.profile(
            "/nix/var/nix/profiles/system",
            &[(42, &running), (43, &staged)],
            43,
        );
        fake.link("/run/current-system", &running);
        fake.link("/run/booted-system", &running);
        let Some(r) = fake.probe(Kind::Nixos, None) else {
            return;
        };
        assert_eq!(
            r,
            ProbeReport {
                profile: Some(sp(&staged)),
                booted: Some(sp(&running)),
                ..generation(&running, 42)
            }
        );
    }

    #[test]
    fn nixos_generation_is_the_newest_link_numerically() {
        // Generations can repeat a path; glob order puts 10 before 9.
        let fake = Fake::new();
        let a = fake.nixos('a', ['1', '1', '1']);
        let b = fake.nixos('b', ['1', '1', '1']);
        fake.profile(
            "/nix/var/nix/profiles/system",
            &[(9, &a), (10, &a), (11, &b), (100, &b)],
            100,
        );
        fake.link("/run/current-system", &a);
        let Some(r) = fake.probe(Kind::Nixos, None) else {
            return;
        };
        assert_eq!(r.generation, Some(10));
        assert_eq!(r.profile, Some(sp(&b)));
    }

    #[test]
    fn nixos_test_activation_has_no_generation() {
        // `nixos-rebuild test`: current is in no generation.
        let fake = Fake::new();
        let a = fake.nixos('a', ['1', '1', '1']);
        let tested = fake.nixos('z', ['1', '1', '1']);
        fake.profile("/nix/var/nix/profiles/system", &[(1, &a)], 1);
        fake.link("/run/current-system", &tested);
        let Some(r) = fake.probe(Kind::Nixos, None) else {
            return;
        };
        assert_eq!(
            r,
            ProbeReport {
                current: Some(sp(&tested)),
                profile: Some(sp(&a)),
                activated: Some(LINK_MTIME),
                ..native()
            }
        );
    }

    #[test]
    fn nixos_reads_a_custom_system_profile() {
        let fake = Fake::new();
        let a = fake.nixos('a', ['1', '1', '1']);
        fake.profile("/nix/var/nix/profiles/system-profiles/work", &[(7, &a)], 7);
        fake.link("/run/current-system", &a);
        let Some(r) = fake.probe(
            Kind::Nixos,
            Some("/nix/var/nix/profiles/system-profiles/work"),
        ) else {
            return;
        };
        assert_eq!(r, generation(&a, 7));
    }

    #[test]
    fn nixos_revision_from_nixos_version() {
        let fake = Fake::new();
        let top = fake.nixos('b', ['1', '1', '1']);
        let sw = fake.store('w', "system-path");
        fake.link(&format!("{top}/sw"), &sw);
        fake.script(
            &fake.real(&format!("{sw}/bin/nixos-version")),
            "case $1 in\n--configuration-revision) echo 42ea902bbbe2 ;;\n*) echo '25.11 (Xantusia)' ;;\nesac\n",
        );
        fake.profile("/nix/var/nix/profiles/system", &[(1, &top)], 1);
        fake.link("/run/current-system", &top);
        let Some(r) = fake.probe(Kind::Nixos, None) else {
            return;
        };
        assert_eq!(r.revision.as_deref(), Some("42ea902bbbe2"));
        // Unknown to nixos-version (it fails): the generation's file, if any.
        fake.script(
            &fake.real(&format!("{sw}/bin/nixos-version")),
            "echo unknown >&2; exit 1\n",
        );
        assert_eq!(fake.probe(Kind::Nixos, None).unwrap().revision, None);
        fake.file(&format!("{top}/configuration-revision"), "from-file\n");
        assert_eq!(
            fake.probe(Kind::Nixos, None).unwrap().revision.as_deref(),
            Some("from-file")
        );
    }

    #[test]
    fn darwin_revision_from_darwin_version_json() {
        let fake = Fake::new();
        let system = fake.store('d', "darwin-system-25.11.4cff07d");
        let json = format!("/nix/store/{}-darwin-version.json", "j".repeat(32));
        // As nix-darwin writes it (pkgs.formats.json, a link into the store).
        fake.file(
            &json,
            "{\n  \"configurationRevision\": \"42ea902bbbe25ffd\",\n  \"darwinLabel\": \"25.11.4cff07d\",\n  \
             \"darwinRevision\": \"4cff07de74b50e64\"\n}\n",
        );
        fake.link(&format!("{system}/darwin-version.json"), &json);
        fake.profile("/nix/var/nix/profiles/system", &[(93, &system)], 93);
        fake.link("/run/current-system", &system);
        // nix-darwin has no booted-system; it's never reported.
        fake.link("/run/booted-system", &system);
        let Some(r) = fake.probe(Kind::Darwin, None) else {
            return;
        };
        assert_eq!(
            r,
            ProbeReport {
                revision: Some("42ea902bbbe25ffd".into()),
                ..generation(&system, 93)
            }
        );
        // Compact JSON too; a configuration without a revision has none.
        fake.file(
            &json,
            "{\"darwinLabel\":\"x\",\"configurationRevision\":\"abc-dirty\",\"z\":\"y\"}",
        );
        assert_eq!(
            fake.probe(Kind::Darwin, None).unwrap().revision.as_deref(),
            Some("abc-dirty")
        );
        fake.file(&json, "{\n  \"darwinLabel\": \"25.11.4cff07d\"\n}\n");
        assert_eq!(fake.probe(Kind::Darwin, None).unwrap().revision, None);
    }

    #[test]
    fn subprocesses_dont_grow_with_generations() {
        // One readlink per symlink hop, plus uname and stat; scanning 30
        // generations and their specialisations takes only builtins.
        let fake = Fake::new();
        let log = fake.tmp.path().join("exec.log");
        for tool in ["readlink", "stat", "uname", "id", "sed"] {
            if let Some(real) = which(tool) {
                fake.shim(
                    tool,
                    &format!(
                        "echo {tool} >>'{}'\nexec '{}' \"$@\"\n",
                        log.display(),
                        real.display()
                    ),
                );
            }
        }
        let hm = fake.store('a', "home-manager-generation");
        fake.profile(HM_PROFILE, &[(1, &hm), (2, &hm)], 2);
        let mut generations = Vec::new();
        for n in 1..=30u32 {
            generations.push((n, fake.store('a', &format!("nixos-system-{n}"))));
        }
        let parent = &generations[29].1;
        let spec = fake.nixos('9', ['2', '2', '2']);
        fake.link(&format!("{parent}/specialisation/gaming"), &spec);
        let generations: Vec<(u32, &str)> =
            generations.iter().map(|(n, p)| (*n, p.as_str())).collect();
        fake.profile("/nix/var/nix/profiles/system", &generations, 30);
        fake.link("/run/current-system", &spec);
        fake.link("/run/booted-system", &spec);

        let count = |shell: &Shell, kind| {
            let _ = fs::remove_file(&log);
            let out = fake.run(shell, &builtin_script(kind, None, NONCE));
            assert!(out.status.success());
            let mut counts = BTreeMap::new();
            for tool in fs::read_to_string(&log).unwrap_or_default().lines() {
                *counts.entry(tool.to_string()).or_insert(0) += 1;
            }
            counts
        };
        let expect = |pairs: &[(&str, usize)]| -> BTreeMap<String, usize> {
            pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
        };
        for shell in shells() {
            assert_eq!(
                count(shell, Kind::HomeManager),
                expect(&[("readlink", 2), ("stat", 1), ("uname", 1)]),
                "{}",
                shell.name
            );
            // current-system, the profile (2 hops), the specialisation's
            // parent and booted-system.
            assert_eq!(
                count(shell, Kind::Nixos),
                expect(&[("readlink", 5), ("stat", 1), ("uname", 1)]),
                "{}",
                shell.name
            );
        }
    }

    #[test]
    fn home_manager_probe_takes_milliseconds() {
        let fake = Fake::new();
        let hm = fake.store('a', "home-manager-generation");
        fake.profile(HM_PROFILE, &[(1, &hm), (2, &hm)], 2);
        let script = builtin_script(Kind::HomeManager, None, NONCE);
        for shell in shells() {
            let mut times: Vec<Duration> = (0..5)
                .map(|_| {
                    let start = Instant::now();
                    assert!(fake.run(shell, &script).status.success());
                    start.elapsed()
                })
                .collect();
            times.sort();
            eprintln!(
                "home-manager probe, {}: min {:?}, median {:?}",
                shell.name, times[0], times[2]
            );
            // About 10 ms; the slack is for loaded machines.
            assert!(
                times[0] < Duration::from_millis(200),
                "{}: {times:?}",
                shell.name
            );
        }
    }

    #[test]
    fn scripts_pass_shellcheck() {
        let Some(shellcheck) = which("shellcheck") else {
            eprintln!("shellcheck not on PATH, skipped");
            return;
        };
        for (kind, profile) in [
            (Kind::HomeManager, None),
            (Kind::HomeManager, Some("$HOME/my 'profile'")),
            (Kind::Custom, Some("/opt/it's \"here\"")),
            (Kind::Custom, None),
            (Kind::Nixos, None),
            (
                Kind::Nixos,
                Some("/nix/var/nix/profiles/system-profiles/work"),
            ),
            (Kind::Darwin, None),
        ] {
            let mut child = Command::new(&shellcheck)
                .args(["--norc", "--shell=sh", "-"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let script = builtin_script(kind, profile, NONCE);
            child
                .stdin
                .take()
                .unwrap()
                .write_all(script.as_bytes())
                .unwrap();
            let out = child.wait_with_output().unwrap();
            assert!(
                out.status.success(),
                "{kind} {profile:?}:\n{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    /// Where a `--store` value keeps `/nix/store`: an absolute path is a
    /// chroot store's root, else its `root=` parameter.
    fn store_root(store: &str) -> Option<String> {
        if store.starts_with('/') {
            return Some(store.to_string());
        }
        let (_, query) = store.split_once('?')?;
        query
            .split('&')
            .find_map(|kv| kv.strip_prefix("root="))
            .map(str::to_string)
    }

    #[test]
    fn store_roots() {
        assert_eq!(store_root("/tmp/s").as_deref(), Some("/tmp/s"));
        assert_eq!(store_root("local?root=/tmp/s").as_deref(), Some("/tmp/s"));
        assert_eq!(
            store_root("local?a=b&root=/tmp/s&c").as_deref(),
            Some("/tmp/s")
        );
        assert_eq!(store_root("daemon"), None);
    }

    #[test]
    fn real_nix_profiles() {
        if std::env::var("HOSTCTL_TEST_NIX").as_deref() != Ok("1") {
            eprintln!("set HOSTCTL_TEST_NIX=1 to probe real Nix profiles");
            return;
        }
        let (Some(nix_store), Some(nix_env)) = (which("nix-store"), which("nix-env")) else {
            eprintln!("nix-store or nix-env not on PATH, skipped");
            return;
        };
        let (store, _turn) = crate::util::test_nix_store();
        let nix = |program: &Path, args: &[&str]| {
            let mut cmd = Command::new(program);
            if let Some(store) = &store {
                cmd.args(["--store", store]);
            }
            let out = cmd.args(args).output().unwrap();
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(out.status.success(), "{program:?} {args:?}: {stderr}");
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        let mut fake = Fake::new();
        // A chroot store's objects live under its root, which must then be
        // the probe root; the daemon's store needs no root.
        let (root, profile_dir, profile) = match store.as_deref().and_then(store_root) {
            Some(root) => {
                // The store may outlive this test (a developer's): a fresh
                // directory per run, removed at the end.
                let name = format!("hostctl-probe-test-{}", nonce());
                let dir = Path::new(&root).join(&name);
                (root, dir, format!("/{name}/p"))
            }
            None => {
                let dir = fake.tmp.path().join("profiles");
                let profile = dir.join("p").display().to_string();
                (String::new(), dir, profile)
            }
        };
        fake.set("HOSTCTL_PROBE_ROOT", root);
        fs::create_dir_all(&profile_dir).unwrap();
        let link = profile_dir.join("p").display().to_string();
        let mut paths = Vec::new();
        for (n, contents) in [(1, "one"), (2, "two")] {
            let object = fake.tmp.path().join(format!("hostctl-probe-{contents}"));
            fs::write(&object, contents).unwrap();
            let path = nix(&nix_store, &["--add", &object.display().to_string()]);
            nix(&nix_env, &["--profile", &link, "--set", &path]);
            let Some(r) = fake.probe(Kind::Custom, Some(&profile)) else {
                return;
            };
            assert_eq!(r.current, Some(sp(&path)), "generation {n}");
            assert_eq!(r.generation, Some(n));
            assert!(r.activated.is_some());
            paths.push(path);
        }
        // Rolling back is just another target for the profile link.
        nix(&nix_env, &["--profile", &link, "--rollback"]);
        let r = fake.probe(Kind::Custom, Some(&profile)).unwrap();
        assert_eq!((r.current, r.generation), (Some(sp(&paths[0])), Some(1)));
        let _ = fs::remove_dir_all(&profile_dir);
    }
}
