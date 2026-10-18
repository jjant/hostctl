//! Building the processes hostctl runs: `sh` locally, or `ssh` to a host.
//! Everything here is plain data so command lines are unit-testable. See
//! DESIGN.md ("SSH").

use std::borrow::Cow;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::config::{SshConfig, SshTarget, Target};

/// Default cap on captured stdout/stderr per process.
pub const DEFAULT_MAX_OUTPUT: usize = 4 << 20;

/// A process to spawn (see [`crate::exec`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    /// Written to the child's stdin, which is then closed; `None` → null stdin.
    pub stdin: Option<String>,
    /// Extra environment variables.
    pub env: Vec<(String, String)>,
    /// Bytes kept per stream (stdout, stderr); the rest is discarded.
    pub max_output: usize,
}

impl Default for CommandSpec {
    fn default() -> Self {
        CommandSpec {
            program: String::new(),
            args: Vec::new(),
            stdin: None,
            env: Vec::new(),
            max_output: DEFAULT_MAX_OUTPUT,
        }
    }
}

impl CommandSpec {
    pub fn new(program: impl Into<String>) -> Self {
        CommandSpec {
            program: program.into(),
            ..Default::default()
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn stdin(mut self, input: impl Into<String>) -> Self {
        self.stdin = Some(input.into());
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn max_output(mut self, bytes: usize) -> Self {
        self.max_output = bytes;
        self
    }

    /// The command line, shell-quoted, for messages (`ssh -T … sh -s`).
    /// Arguments may keep a `=` unquoted (`-o BatchMode=yes`): only in
    /// command position, or leading in zsh, is it special.
    pub fn display(&self) -> String {
        let arg = |a: &String| -> String {
            match a.split_once('=') {
                Some((key, value))
                    if !key.is_empty()
                        && matches!(sh_quote(key), Cow::Borrowed(_))
                        && (value.is_empty() || matches!(sh_quote(value), Cow::Borrowed(_)))
                        && !value.contains('=') =>
                {
                    a.clone()
                }
                _ => sh_quote(a).into_owned(),
            }
        };
        std::iter::once(sh_quote(&self.program).into_owned())
            .chain(self.args.iter().map(arg))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Quote `s` as one word for POSIX sh, bash, zsh and fish alike. Non-empty
/// words made only of `[A-Za-z0-9@+:,./_-]` pass through; anything else is
/// single-quoted, with `'` and `\` emitted outside the quotes as `\'` and
/// `\\`, which all four shells read the same way (fish treats `\'`/`\\`
/// specially *inside* single quotes, so they never appear there).
pub fn sh_quote(s: &str) -> Cow<'_, str> {
    let safe = !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"@+:,./_-".contains(&b));
    if safe {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\'' => out.push_str("'\\''"),
            '\\' => out.push_str("'\\\\'"),
            _ => out.push(c),
        }
    }
    out.push('\'');
    Cow::Owned(out)
}

/// Prepended to every remote/interactive script that needs Nix: non-login
/// sessions often lack it on `PATH` (`sh -l` doesn't help on some systems).
/// The Nix directories are appended, so the session's own choices win;
/// `/run/wrappers/bin` goes first because NixOS's setuid `sudo` lives there
/// and the plain copy in `sw/bin` refuses to run.
pub const PATH_PRELUDE: &str = r#"PATH="/run/wrappers/bin:$PATH:/nix/var/nix/profiles/default/bin:/run/current-system/sw/bin:$HOME/.nix-profile/bin:${XDG_STATE_HOME:-$HOME/.local/state}/nix/profile/bin:/etc/profiles/per-user/${USER:-${LOGNAME:-}}/bin"; export PATH"#;

/// A parsed ssh destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    pub user: Option<String>,
    /// Name or address; IPv6 without brackets.
    pub host: String,
    pub port: Option<u16>,
}

/// What OpenSSH ≥ 9.6 refuses in user and host names given on its command
/// line: older clients pass them to the shell that runs a `ProxyCommand`
/// using `%h`/`%r`.
const SHELL_META: &[char] = &[
    '\'', '`', '"', '$', '\\', ';', '&', '<', '>', '|', '(', ')', '{', '}',
];

/// Parse `host`, `user@host`, `ssh://[user@]host[:port]` (IPv6 as `[::1]`
/// inside the URI; bare elsewhere). Errors on empty parts, a bad port, a
/// port outside the URI form, a URI path, whitespace, control characters,
/// shell metacharacters (see `SHELL_META`) or a leading `-` (option
/// injection) in the destination, user or host.
pub fn parse_destination(destination: &str) -> Result<Destination, String> {
    let bad = |why: &str| Err(format!("invalid ssh destination {destination:?}: {why}"));
    if destination.is_empty() {
        return bad("empty");
    }
    if destination.starts_with('-') {
        return bad("starts with '-'");
    }
    if destination.chars().any(char::is_whitespace) {
        return bad("contains whitespace");
    }
    if let Some(c) = destination
        .chars()
        .find(|&c| c.is_control() || SHELL_META.contains(&c))
    {
        return bad(&format!("contains {c:?}"));
    }
    let (rest, uri) = match destination.strip_prefix("ssh://") {
        Some(rest) => (rest.trim_end_matches('/'), true),
        None => (destination, false),
    };
    if uri && rest.contains('/') {
        return bad("a path isn't allowed");
    }
    let (user, hostport) = match rest.rsplit_once('@') {
        Some(("", _)) => return bad("empty user"),
        Some((user, hostport)) => (Some(user), hostport),
        None => (None, rest),
    };
    if user.is_some_and(|u| u.starts_with('-')) {
        return bad("user starts with '-'");
    }
    let (host, port) = if let Some(v6) = hostport.strip_prefix('[') {
        let Some((host, after)) = v6.split_once(']') else {
            return bad("unterminated '['");
        };
        match after.strip_prefix(':') {
            Some(port) if uri => (host, Some(port)),
            None if after.is_empty() => (host, None),
            _ => return bad("unexpected text after ']'"),
        }
    } else if let Some((host, port)) = hostport
        .split_once(':')
        .filter(|(_, port)| !port.contains(':'))
    {
        if !uri {
            return bad("give a port as ssh://host:port");
        }
        (host, Some(port))
    } else if uri && hostport.contains(':') {
        return bad("an IPv6 address needs brackets: ssh://[address]:port");
    } else {
        (hostport, None)
    };
    if host.is_empty() {
        return bad("empty host");
    }
    if host.starts_with('-') {
        return bad("host starts with '-'");
    }
    if host.contains(['[', ']', '/']) {
        return bad("unexpected '[', ']' or '/' in the host");
    }
    let port = match port {
        Some(p) => match p.parse::<u16>() {
            Ok(n) if n > 0 && p.bytes().all(|b| b.is_ascii_digit()) => Some(n),
            _ => return bad("bad port"),
        },
        None => None,
    };
    Ok(Destination {
        user: user.map(str::to_string),
        host: host.to_string(),
        port,
    })
}

/// Unattended ssh options (DESIGN.md "SSH"), before the mux options.
pub const UNATTENDED_OPTIONS: &[&str] = &[
    "-T",
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=10",
    "-o",
    "StrictHostKeyChecking=accept-new",
    "-o",
    "ClearAllForwardings=yes",
    "-o",
    "ForwardAgent=no",
    "-o",
    "ForwardX11=no",
    "-o",
    "PermitLocalCommand=no",
];

/// Interactive (activation) options: a TTY; forwardings from the user's
/// config are cleared, but agent forwarding and prompts stay available.
pub const INTERACTIVE_OPTIONS: &[&str] = &[
    "-t",
    "-o",
    "ControlMaster=no",
    "-o",
    "ClearAllForwardings=yes",
];

/// Connection multiplexing for unattended runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mux {
    /// hostctl's private ControlMaster at this socket, kept for `persist`.
    Private { socket: PathBuf, persist_secs: u64 },
    /// No multiplexing at all (also used to capture swallowed errors).
    Off,
}

impl Mux {
    /// The `-o` options selecting this mux. A private master also sends
    /// keepalives (`ServerAliveInterval=5`, `ServerAliveCountMax=2`), so
    /// that one whose connection died without a reset (a sleep, a VPN
    /// change, a NAT drop) notices within seconds and exits, instead of
    /// stalling every run over it until TCP gives up.
    pub fn options(&self) -> Vec<String> {
        match self {
            Mux::Private {
                socket,
                persist_secs,
            } => vec![
                "-o".into(),
                "ControlMaster=auto".into(),
                "-o".into(),
                control_path_option(socket),
                "-o".into(),
                format!("ControlPersist={persist_secs}"),
                "-o".into(),
                "ServerAliveInterval=5".into(),
                "-o".into(),
                "ServerAliveCountMax=2".into(),
            ],
            Mux::Off => vec![
                "-o".into(),
                "ControlMaster=no".into(),
                "-o".into(),
                "ControlPath=none".into(),
                "-o".into(),
                "ControlPersist=no".into(),
            ],
        }
    }
}

/// `ControlPath=<socket>`, with the `%` ssh would expand as a token
/// escaped.
fn control_path_option(socket: &Path) -> String {
    format!(
        "ControlPath={}",
        socket.to_string_lossy().replace('%', "%%")
    )
}

/// Where hostctl's control sockets live: `$XDG_RUNTIME_DIR/hostctl/ssh`,
/// else `~/.cache/hostctl/ssh` (the caller creates it with mode 0700).
pub fn control_dir() -> Option<PathBuf> {
    control_dir_in(
        std::env::var_os("XDG_RUNTIME_DIR"),
        std::env::var_os("HOME"),
    )
}

/// [`control_dir`] for these `$XDG_RUNTIME_DIR` and `$HOME`; empty or
/// relative values don't count (as the XDG spec says).
fn control_dir_in(runtime_dir: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let absolute = |dir: Option<OsString>| dir.map(PathBuf::from).filter(|d| d.is_absolute());
    match absolute(runtime_dir) {
        Some(dir) => Some(dir.join("hostctl/ssh")),
        None => absolute(home).map(|home| home.join(".cache/hostctl/ssh")),
    }
}

/// The private control socket for `target`: `<dir>/<16 hex chars>`, a hash
/// of everything that identifies the connection (destination, port,
/// per-host and global options), short enough for macOS's 104-byte limit.
pub fn control_socket(dir: &Path, target: &SshTarget, ssh: &SshConfig) -> PathBuf {
    // Each field ends in NUL, which no argument can contain, after a count
    // of the options: distinct connections never share a key.
    let mut key = String::with_capacity(128);
    let _ = write!(key, "{}\0", target.options.len() + ssh.options.len());
    for field in std::iter::once(&ssh.command)
        .chain(&target.options)
        .chain(&ssh.options)
    {
        key.push_str(field);
        key.push('\0');
    }
    let _ = write!(
        key,
        "{}\0{}\0{}\0",
        target.user.as_deref().unwrap_or(""),
        target.host,
        target.port.unwrap_or(0)
    );
    dir.join(format!("{:016x}", crate::util::fnv1a64(key.as_bytes())))
}

/// Longest usable socket path: a master first listens on
/// `<path>.<16 random chars>`, and `sun_path` holds 104 bytes with the NUL
/// on macOS (108 on Linux).
const MAX_SOCKET_PATH: usize = 104 - 1 - 17;

/// Whether ssh can take `socket` as its ControlPath: short enough, and
/// nothing its option parser would split or unquote.
fn usable_socket(socket: &Path) -> bool {
    socket.to_str().is_some_and(|s| {
        s.len() <= MAX_SOCKET_PATH
            && !s
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || "\"'\\".contains(c))
    })
}

/// The mux for `target` under `ssh` settings: [`Mux::Off`] when `persist`
/// is 0, there is no `dir`, or the socket path wouldn't work.
pub fn mux_for(target: &SshTarget, ssh: &SshConfig, dir: Option<&Path>) -> Mux {
    match (ssh.persist.non_zero(), dir) {
        (Some(persist), Some(dir)) => {
            let socket = control_socket(dir, target, ssh);
            if usable_socket(&socket) {
                Mux::Private {
                    socket,
                    persist_secs: persist.as_secs().max(1),
                }
            } else {
                Mux::Off
            }
        }
        _ => Mux::Off,
    }
}

/// `ssh <host options> <global options> <defaults> <mux> [-p PORT] -- [user@]host`.
///
/// The user's options come first because ssh keeps the first value of an
/// option. A default `-o Key=…` the user already set is left out entirely:
/// ssh would ignore it, but an older ssh still rejects a value it doesn't
/// know (OpenSSH < 7.6 and `StrictHostKeyChecking=accept-new`).
fn ssh_argv(
    ssh: &SshConfig,
    target: &SshTarget,
    defaults: &[&str],
    mux: Option<&Mux>,
) -> CommandSpec {
    let user = || {
        target
            .options
            .iter()
            .chain(&ssh.options)
            .map(String::as_str)
    };
    let mux = mux.map(Mux::options).unwrap_or_default();
    let mut ours = defaults
        .iter()
        .copied()
        .chain(mux.iter().map(String::as_str));
    let mut spec = CommandSpec::new(&ssh.command).args(user());
    while let Some(arg) = ours.next() {
        if arg != "-o" {
            spec.args.push(arg.to_string());
        } else if let Some(option) = ours.next()
            && !sets_option(user(), option_keyword(option))
        {
            spec.args.extend(["-o".to_string(), option.to_string()]);
        }
    }
    if let Some(port) = target.port {
        spec = spec.arg("-p").arg(port.to_string());
    }
    let login = match &target.user {
        Some(user) => format!("{user}@{}", target.host),
        None => target.host.clone(),
    };
    spec.arg("--").arg(login)
}

/// Whether ssh arguments `args` set option `keyword` (`-o Key=value`,
/// `-oKey value`; keywords are case-insensitive).
fn sets_option<'a>(args: impl IntoIterator<Item = &'a str>, keyword: &str) -> bool {
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let option = match arg.strip_prefix("-o") {
            Some("") => args.next().unwrap_or_default(),
            Some(inline) => inline,
            None => continue,
        };
        if option_keyword(option).eq_ignore_ascii_case(keyword) {
            return true;
        }
    }
    false
}

/// The `Key` of an ssh option written `Key=value` or `Key value`.
fn option_keyword(option: &str) -> &str {
    let option = option.trim_start();
    let end = option
        .find(|c: char| c == '=' || c.is_ascii_whitespace())
        .unwrap_or(option.len());
    &option[..end]
}

/// Run a POSIX sh `script` on `target`, fed on stdin to `sh -s`.
pub fn script_command(target: &Target, ssh: &SshConfig, mux: &Mux, script: &str) -> CommandSpec {
    match target {
        Target::Local { .. } => CommandSpec::new("sh").arg("-s"),
        Target::Ssh(t) => ssh_argv(ssh, t, UNATTENDED_OPTIONS, Some(mux)).args(["sh", "-s"]),
    }
    .stdin(script)
}

/// Send `command` verbatim as the ssh command (what a forced-command
/// endpoint sees as `SSH_ORIGINAL_COMMAND`).
pub fn raw_command(target: &SshTarget, ssh: &SshConfig, mux: &Mux, command: &str) -> CommandSpec {
    ssh_argv(ssh, target, UNATTENDED_OPTIONS, Some(mux)).arg(command)
}

/// Tell hostctl's master for `target` at `socket` to exit (`ssh -o
/// ControlPath=<socket> -O exit … -- [user@]host`), e.g. one that stopped
/// answering. `None` when the user's options set a ControlPath of their
/// own: then the master in use isn't hostctl's to stop.
pub fn exit_master_command(
    target: &SshTarget,
    ssh: &SshConfig,
    socket: &Path,
) -> Option<CommandSpec> {
    let user = target
        .options
        .iter()
        .chain(&ssh.options)
        .map(String::as_str);
    if sets_option(user, "ControlPath") {
        return None;
    }
    let control_path = control_path_option(socket);
    let defaults = ["-o", control_path.as_str(), "-O", "exit"];
    Some(ssh_argv(ssh, target, &defaults, None))
}

/// The body of an interactive script: deletes itself (`$0`) when it was
/// uploaded, sets [`PATH_PRELUDE`], then runs `command`.
pub fn interactive_script(command: &str, uploaded: bool) -> String {
    let mut script = String::new();
    if uploaded {
        script.push_str("rm -f \"$0\"\n");
    }
    script.push_str(PATH_PRELUDE);
    script.push('\n');
    script.push_str(command);
    script.push('\n');
    script
}

/// Store `script` in a private temp file on the target and print its path:
/// the shell-agnostic way to run an interactive command. The script travels
/// inside the `sh -s` program itself, so the remote login shell only ever
/// parses `sh -s`.
pub fn upload_command(target: &SshTarget, ssh: &SshConfig, mux: &Mux, script: &str) -> CommandSpec {
    script_command(
        &Target::Ssh(target.clone()),
        ssh,
        mux,
        &upload_script(script),
    )
}

/// The `sh -s` program behind [`upload_command`]: it writes `script`
/// verbatim (a quoted here-document expands nothing) into a fresh 0600
/// file in `$TMPDIR` (else, or if that fails, `/tmp`) and prints its path.
fn upload_script(script: &str) -> String {
    let mut eof = String::from("HOSTCTL_EOF");
    while script.lines().any(|line| line == eof) {
        eof.push('_');
    }
    let mut out = String::with_capacity(script.len() + 2 * eof.len() + 160);
    let _ = writeln!(
        out,
        r#"umask 077 && f=$(mktemp "${{TMPDIR:-/tmp}}/hostctl.XXXXXX" 2>/dev/null || mktemp /tmp/hostctl.XXXXXX) && cat >"$f" <<'{eof}' && printf '%s\n' "$f""#
    );
    out.push_str(script);
    if !script.is_empty() && !script.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&eof);
    out.push('\n');
    out
}

/// Whether an uploaded script's path is safe to put on a remote command
/// line (`[A-Za-z0-9/._-]+`, absolute).
pub fn is_safe_remote_path(path: &str) -> bool {
    path.starts_with('/')
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b))
}

/// Run `command` interactively (with a TTY). On an ssh target with
/// `uploaded` — the path [`upload_command`] printed, which must satisfy
/// [`is_safe_remote_path`] — that is `ssh -t … sh <uploaded>`; otherwise
/// (local hosts, `activate-on = "local"`) it runs here, via `sh -c <script>`.
pub fn interactive_command(
    target: &Target,
    ssh: &SshConfig,
    command: &str,
    uploaded: Option<&str>,
) -> CommandSpec {
    match (target, uploaded) {
        // Quoting changes nothing for a safe path, and defuses any other.
        (Target::Ssh(t), Some(path)) => ssh_argv(ssh, t, INTERACTIVE_OPTIONS, None)
            .arg("sh")
            .arg(sh_quote(path)),
        _ => CommandSpec::new("sh").args(["-c", &interactive_script(command, false)]),
    }
}

/// A hint for a recognized ssh failure, from its stderr (e.g. `Could not
/// resolve hostname` → set `destination`; `Permission denied (publickey…)` →
/// BatchMode needs a key in the agent; `REMOTE HOST IDENTIFICATION HAS
/// CHANGED`/`Host key verification failed` → `ssh-keygen -R <host>`;
/// `Cannot execute command-line and remote command` → `RemoteCommand` in ssh
/// config, add `-o RemoteCommand=none` to the host's `ssh-options`; an ssh
/// older than 7.6 rejecting `accept-new` → a newer `ssh.command`). Only
/// ssh's own messages count, not a remote command's. `reads_config`: whether
/// that ssh read ~/.ssh/config ([`reads_ssh_config`]), so a hint may send
/// the user there.
pub fn ssh_hint(stderr: &str, reads_config: bool) -> Option<String> {
    let name = |prefix: &str, end: &str| {
        line_part(stderr, prefix, end).map(|name| crate::util::sanitize_terminal(name).into_owned())
    };
    if stderr.contains("Cannot execute command-line and remote command") {
        return Some(
            r#"the ssh config sets RemoteCommand for this host: give the host ssh-options = ["-o", "RemoteCommand=none"]"#
                .into(),
        );
    }
    if stderr.contains(r#"unsupported option "accept-new""#) {
        return Some(
            r#"this ssh predates OpenSSH 7.6 (no StrictHostKeyChecking=accept-new): set ssh.command to a newer ssh, or add "-o", "StrictHostKeyChecking=yes" to ssh.options (hosts must then be in known_hosts already)"#
                .into(),
        );
    }
    let changed = stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED");
    if changed || stderr.contains("Host key verification failed") {
        if let Some(host) = name("host key is known for ", " and you have requested") {
            return Some(format!(
                "{host} isn't in known_hosts and strict host key checking is on: connect once with ssh to check and accept its key"
            ));
        }
        // "Host key for web1 has changed" (newer), "ECDSA host key for …" (7.x).
        let host = name("ost key for ", " has changed");
        let forget = host.as_deref().map_or(Cow::Borrowed("<host>"), sh_quote);
        return Some(match &host {
            Some(host) => format!(
                "the host key of {host} changed (reinstalled?): if that's expected, run ssh-keygen -R {forget}"
            ),
            None if changed => format!(
                "the host key changed (reinstalled?): if that's expected, run ssh-keygen -R {forget}"
            ),
            None => format!(
                "host key verification failed: if the host was reinstalled, run ssh-keygen -R {forget}"
            ),
        });
    }
    if let Some(host) = name("Could not resolve hostname ", ":") {
        return Some(if stderr.contains("Temporary failure in name resolution") {
            format!("looking up {host} failed: check the network and DNS")
        } else {
            let config = if reads_config {
                " or give it a Host entry in ~/.ssh/config"
            } else {
                ""
            };
            format!(
                "{host} doesn't resolve: set the host's destination (e.g. user@{host}.example.com){config}"
            )
        });
    }
    if stderr.contains("Too many authentication failures") {
        return Some(if reads_config {
            "the server stopped after too many keys: set IdentitiesOnly yes and the right IdentityFile for this host in ~/.ssh/config"
                .into()
        } else {
            r#"the server stopped after too many keys: give the host ssh-options = ["-o", "IdentitiesOnly=yes", "-i", "<its key>"]"#
                .into()
        });
    }
    if stderr.lines().any(is_auth_failure) {
        return Some(
            "authentication failed, and hostctl's ssh can't prompt (BatchMode=yes): add the key to your agent (ssh-add) or use one without a passphrase"
                .into(),
        );
    }
    None
}

/// Whether ssh run with these user arguments (a host's `ssh-options`, then
/// `ssh.options`) reads ~/.ssh/config: not when they pass `-F`.
pub fn reads_ssh_config<'a>(args: impl IntoIterator<Item = &'a String>) -> bool {
    !args.into_iter().any(|arg| arg.starts_with("-F"))
}

/// The non-empty text between `prefix` and `end` on the line where `prefix`
/// first occurs.
fn line_part<'a>(text: &'a str, prefix: &str, end: &str) -> Option<&'a str> {
    let rest = &text[text.find(prefix)? + prefix.len()..];
    let line = rest.split('\n').next().unwrap_or_default();
    Some(&line[..line.find(end)?]).filter(|part| !part.is_empty())
}

/// `[user@host: ]Permission denied (publickey,password).`: ssh's own
/// authentication failure, not some remote command's "Permission denied".
fn is_auth_failure(line: &str) -> bool {
    const DENIED: &str = "Permission denied (";
    line.find(DENIED)
        .and_then(|at| line[at + DENIED.len()..].trim_end().strip_suffix(")."))
        .is_some_and(|methods| {
            !methods.is_empty()
                && methods
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-,@.".contains(&b))
        })
}

/// How ssh names the other end of a ProxyCommand or ProxyJump, whose
/// address it doesn't know.
const UNKNOWN_PEER: &str = "UNKNOWN port 65535";

/// `Connection closed by UNKNOWN port 65535`: ssh's connection through a
/// ProxyCommand or ProxyJump ended. With ControlPersist that line is all
/// that is left of a failing proxy's own error (the master has its stderr).
pub fn is_proxy_closed(line: &str) -> bool {
    line.trim()
        .strip_prefix("Connection closed by ")
        .is_some_and(|peer| peer == UNKNOWN_PEER)
}

/// Whether an ssh failure is worth one retry: sshd's MaxStartups drops
/// (`kex_exchange_identification`, `Connection reset by peer`,
/// `Connection closed by … port …` during key exchange) — unless stderr
/// also shows a failure a retry can't fix (resolution, authentication,
/// host keys, configuration, timeouts, refused or unroutable connections).
/// A proxy's end closing (`… by UNKNOWN port 65535`) is no drop: the proxy
/// failed, and says why only without multiplexing.
pub fn is_transient_ssh_failure(stderr: &str) -> bool {
    const PERMANENT: &[&str] = &[
        "Could not resolve hostname",
        "Permission denied (",
        "Host key verification failed",
        "Too many authentication failures",
        "Cannot execute command-line",
        "unsupported option",
        "Bad configuration option",
        "timed out",
        "Connection refused",
        "No route to host",
        "Network is unreachable",
    ];
    if PERMANENT.iter().any(|p| stderr.contains(p)) {
        return false;
    }
    stderr.lines().map(str::trim_start).any(|line| {
        // kex_exchange_identification (OpenSSH ≥ 7.9), ssh_exchange_… before.
        line.contains("_exchange_identification: ")
            || (!line.contains(UNKNOWN_PEER)
                && (line.contains("Connection reset by ")
                    || (line.starts_with("Connection closed by ") && line.contains(" port "))))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::exec;
    use crate::util::HumanDuration;

    use std::os::unix::fs::PermissionsExt as _;
    use std::time::Duration;

    fn d(user: Option<&str>, host: &str, port: Option<u16>) -> Destination {
        Destination {
            user: user.map(str::to_string),
            host: host.into(),
            port,
        }
    }

    #[test]
    fn parses_destinations() {
        assert_eq!(parse_destination("bigmem"), Ok(d(None, "bigmem", None)));
        assert_eq!(
            parse_destination("jj@desk.example.com"),
            Ok(d(Some("jj"), "desk.example.com", None))
        );
        assert_eq!(
            parse_destination("ssh://devuser1@localhost:2022"),
            Ok(d(Some("devuser1"), "localhost", Some(2022)))
        );
        assert_eq!(parse_destination("ssh://web1"), Ok(d(None, "web1", None)));
        assert_eq!(parse_destination("ssh://web1/"), Ok(d(None, "web1", None)));
        assert_eq!(
            parse_destination("ssh://web1:22/"),
            Ok(d(None, "web1", Some(22)))
        );
        assert_eq!(
            parse_destination("ssh://[::1]:22"),
            Ok(d(None, "::1", Some(22)))
        );
        assert_eq!(parse_destination("ssh://[::1]"), Ok(d(None, "::1", None)));
        assert_eq!(
            parse_destination("root@[fe80::1]"),
            Ok(d(Some("root"), "fe80::1", None))
        );
        assert_eq!(
            parse_destination("root@fe80::1"),
            Ok(d(Some("root"), "fe80::1", None))
        );
        assert_eq!(parse_destination("::1"), Ok(d(None, "::1", None)));
        assert_eq!(
            parse_destination("a@b@host"),
            Ok(d(Some("a@b"), "host", None))
        );
        assert_eq!(parse_destination("10.0.0.5"), Ok(d(None, "10.0.0.5", None)));
        assert_eq!(
            parse_destination("ssh://jj.ant_dev@host-1.example.com:65535"),
            Ok(d(Some("jj.ant_dev"), "host-1.example.com", Some(65535)))
        );
    }

    #[test]
    fn rejects_bad_destinations() {
        for bad in [
            "",
            "-oProxyCommand=x",
            "a b",
            "a\tb",
            "ssh://",
            "ssh://@h",
            "@h",
            "ssh://h:0",
            "ssh://h:99999",
            "ssh://h:x",
            "ssh://h:",
            "ssh://h:+22",
            "u@-h",
            "[::1",
            "[::1]x",
            "[::1]:22",
            "ssh://[::1]x",
            "h\n",
            "h\u{1b}",
            // A port needs the URI form; IPv6 in a URI needs brackets.
            "host:22",
            "jj@host:2222",
            "ssh://fe80::1",
            "ssh://::1",
            // Paths.
            "ssh://host/path",
            "ssh://host:22/x",
            "host/x",
            "h]",
            // Option injection through the user.
            "ssh://-oProxyCommand=x@h",
            "-x@h",
            // Shell metacharacters (ProxyCommand %h/%r on OpenSSH < 9.6).
            "h;id",
            "$(id)@h",
            "h`id`",
            "h|x",
            "h&",
            "a(b)",
            "{a,b}",
            "h<x",
            "h>x",
            "it's",
            "x\"y",
            "back\\slash",
        ] {
            assert!(parse_destination(bad).is_err(), "{bad:?}");
        }
        let err = parse_destination("host:22").unwrap_err();
        assert!(err.contains("ssh://host:port"), "{err}");
    }

    /// Strings that break naive quoting in at least one shell.
    const NASTY: &[&str] = &[
        "",
        "plain",
        "two words",
        "  leading and trailing  ",
        "tab\there",
        "new\nline",
        "\n",
        "trailing newline\n",
        "cr\r\nlf",
        "'",
        "''",
        "it's",
        "'quoted'",
        "\"",
        "\"double\" quotes",
        "mixed '\"' quotes",
        "\\",
        "\\\\",
        "back\\slash",
        "trailing\\",
        "\\'",
        "'\\",
        "\\n\\t",
        "'\\''",
        "$",
        "$HOME",
        "${HOME}",
        "$(id)",
        "$((1+1))",
        "`id`",
        "$'c' $\"d\"",
        "${x:-y}",
        "a\\$b",
        "!",
        "!!",
        "!$",
        "a!b",
        "^",
        "^x",
        "*",
        "?",
        "[a]",
        "*.rs",
        "**/*",
        "=x",
        "=ls",
        "a=b",
        "~",
        "~root",
        "~/x",
        "x~",
        "#",
        "a#b",
        "#comment",
        "%",
        "%self",
        "%1",
        "%s %d",
        "{a,b}",
        "{1..3}",
        "{}",
        "--",
        "-",
        "-n",
        "-e",
        "--help",
        ";",
        "&",
        "&&",
        "|",
        "||",
        "<",
        ">",
        ">>",
        "(",
        ")",
        "()",
        "a;b",
        "x&y",
        "a|b",
        "(sub)",
        "@",
        "@{u}",
        "+",
        ":",
        ",",
        ".",
        "..",
        "/",
        "./x",
        "_",
        "é",
        "日本語",
        "🦀",
        "ünïcödé ✓",
        "\u{202e}rtl",
        "\u{1}\u{1b}[0m\u{7f}",
        "\u{a0}nbsp",
        "'$(rm -rf /)'",
        "\"$(rm -rf /)\"",
        "end with '",
    ];

    fn corpus() -> Vec<String> {
        let mut corpus: Vec<String> = NASTY.iter().map(|s| s.to_string()).collect();
        corpus.push(NASTY.concat());
        corpus.push(format!("{}'\\", "x".repeat(10_000)));
        corpus
    }

    /// The executable `name` on PATH.
    fn which(name: &str) -> Option<PathBuf> {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|dir| dir.join(name))
            .find(|p| {
                p.metadata()
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            })
    }

    /// [`which`] for a shell. When it's missing the test is skipped — unless
    /// HOSTCTL_TEST_REQUIRE_SHELLS=1 and it's a `required` shell.
    fn find_shell(name: &str, required: bool) -> Option<PathBuf> {
        let found = which(name);
        if found.is_none() {
            let require = std::env::var("HOSTCTL_TEST_REQUIRE_SHELLS").as_deref() == Ok("1");
            assert!(
                !(required && require),
                "{name} is required (HOSTCTL_TEST_REQUIRE_SHELLS=1) but not on PATH"
            );
            eprintln!("skipping {name}: not on PATH");
        }
        found
    }

    /// `script` run by `shell` with its startup files out of the way.
    fn shell_command(shell: &str, script: &str) -> Option<CommandSpec> {
        let (required, flags): (bool, &[&str]) = match shell {
            "sh" | "dash" => (true, &[]),
            "bash" => (true, &["--norc", "--noprofile"]),
            "zsh" => (true, &["-f"]),
            "fish" => (true, &["--no-config"]),
            "busybox" => (false, &["sh"]),
            _ => unreachable!("{shell}"),
        };
        let program = find_shell(shell, required)?;
        Some(
            CommandSpec::new(program.to_string_lossy())
                .args(flags.iter().copied())
                .args(["-c", script])
                .env("BASH_ENV", "")
                .env("ENV", "")
                .max_output(64 << 20),
        )
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    /// Every corpus string, quoted, survives `<shell> -c "printf '%s' …"`.
    fn quoting_roundtrips_through(shell: &str) {
        const SEP: &str = "|HOSTCTL-SEP|";
        let corpus = corpus();
        let script: String = corpus
            .iter()
            .map(|s| format!("printf '%s{SEP}' {}\n", sh_quote(s)))
            .collect();
        let Some(spec) = shell_command(shell, &script) else {
            return;
        };
        let out = block_on(exec::run(&spec, Some(Duration::from_secs(20)))).unwrap();
        assert_eq!(out.code, Some(0), "{shell}: {}", out.stderr);
        let printed: Vec<&str> = out.stdout.split(SEP).collect();
        assert_eq!(printed.len(), corpus.len() + 1, "{shell}: {:?}", out.stdout);
        for (want, got) in corpus.iter().zip(&printed) {
            assert_eq!(got, want, "{shell} misread {}", sh_quote(want));
        }
    }

    #[test]
    fn quoting_roundtrips_through_sh() {
        quoting_roundtrips_through("sh");
    }

    #[test]
    fn quoting_roundtrips_through_dash() {
        quoting_roundtrips_through("dash");
    }

    #[test]
    fn quoting_roundtrips_through_bash() {
        quoting_roundtrips_through("bash");
    }

    #[test]
    fn quoting_roundtrips_through_zsh() {
        quoting_roundtrips_through("zsh");
    }

    #[test]
    fn quoting_roundtrips_through_fish() {
        quoting_roundtrips_through("fish");
    }

    #[test]
    fn quoting_roundtrips_through_busybox_sh() {
        quoting_roundtrips_through("busybox");
    }

    #[test]
    fn quoting_shape() {
        assert!(matches!(sh_quote("a-Z_0.9/@+:,"), Cow::Borrowed(_)));
        assert_eq!(sh_quote(""), "''");
        assert_eq!(sh_quote("a b"), "'a b'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert_eq!(sh_quote(r"a\b"), r"'a'\\'b'");
        for s in ["=x", "~", "%self", "#", "{a,b}", "*", "!", "é"] {
            assert_eq!(sh_quote(s), format!("'{s}'"));
        }
        // fish reads `\\` and `\'` specially inside single quotes: never there.
        for s in corpus() {
            let quoted = sh_quote(&s);
            let (mut inside, mut chars) = (false, quoted.chars());
            while let Some(c) = chars.next() {
                match (inside, c) {
                    (true, '\\') => panic!("backslash inside quotes: {quoted:?}"),
                    (_, '\'') => inside = !inside,
                    (false, '\\') => assert!(matches!(chars.next(), Some('\\' | '\''))),
                    (false, c) => assert!(c.is_ascii_alphanumeric() || "@+:,./_-".contains(c)),
                    (true, _) => {}
                }
            }
            assert!(!inside, "unterminated: {quoted:?}");
        }
        let spec = CommandSpec::new("ssh").args(["-o", "a b", "--", "h", "it's"]);
        assert_eq!(spec.display(), r"ssh -o 'a b' -- h 'it'\''s'");
    }

    fn ssh(options: &[&str]) -> SshConfig {
        SshConfig {
            command: "ssh".into(),
            options: options.iter().map(|s| s.to_string()).collect(),
            persist: HumanDuration::from_secs(600),
        }
    }

    fn target(destination: &str, options: &[&str]) -> SshTarget {
        let parsed = parse_destination(destination).unwrap();
        SshTarget {
            destination: destination.into(),
            user: parsed.user,
            host: parsed.host,
            port: parsed.port,
            options: options.iter().map(|s| s.to_string()).collect(),
            forced_command: None,
        }
    }

    fn private(socket: &str) -> Mux {
        Mux::Private {
            socket: socket.into(),
            persist_secs: 600,
        }
    }

    const UNATTENDED: [&str; 15] = [
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "StrictHostKeyChecking=accept-new",
        "-o",
        "ClearAllForwardings=yes",
        "-o",
        "ForwardAgent=no",
        "-o",
        "ForwardX11=no",
        "-o",
        "PermitLocalCommand=no",
    ];

    fn argv(parts: &[&[&str]]) -> Vec<String> {
        parts.concat().iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn script_command_argv() {
        let t = target("ssh://jj@web1:2222", &["-i", "/k"]);
        let spec = script_command(
            &Target::Ssh(t.clone()),
            &ssh(&["-F", "/cfg"]),
            &private("/run/user/1/hostctl/ssh/0123456789abcdef"),
            "echo hi\n",
        );
        assert_eq!(spec.program, "ssh");
        assert_eq!(
            spec.args,
            argv(&[
                &["-i", "/k"],
                &["-F", "/cfg"],
                &UNATTENDED,
                &[
                    "-o",
                    "ControlMaster=auto",
                    "-o",
                    "ControlPath=/run/user/1/hostctl/ssh/0123456789abcdef",
                    "-o",
                    "ControlPersist=600",
                    "-o",
                    "ServerAliveInterval=5",
                    "-o",
                    "ServerAliveCountMax=2"
                ],
                &["-p", "2222", "--", "jj@web1", "sh", "-s"],
            ])
        );
        assert_eq!(spec.stdin.as_deref(), Some("echo hi\n"));
        assert_eq!(spec.max_output, DEFAULT_MAX_OUTPUT);

        let spec = script_command(&Target::Ssh(target("web1", &[])), &ssh(&[]), &Mux::Off, "x");
        assert_eq!(
            spec.args,
            argv(&[
                &UNATTENDED,
                &[
                    "-o",
                    "ControlMaster=no",
                    "-o",
                    "ControlPath=none",
                    "-o",
                    "ControlPersist=no"
                ],
                &["--", "web1", "sh", "-s"],
            ])
        );

        let local = script_command(
            &Target::Local { assumed: false },
            &ssh(&["-v"]),
            &Mux::Off,
            "x",
        );
        assert_eq!(
            (local.program.as_str(), local.args.as_slice()),
            ("sh", &["-s".to_string()][..])
        );
        assert_eq!(local.stdin.as_deref(), Some("x"));
    }

    #[test]
    fn raw_command_argv() {
        let t = target("ssh://devuser1@localhost:2022", &[]);
        let spec = raw_command(&t, &ssh(&[]), &Mux::Off, "current-system");
        assert_eq!(
            spec.args,
            argv(&[
                &UNATTENDED,
                &[
                    "-o",
                    "ControlMaster=no",
                    "-o",
                    "ControlPath=none",
                    "-o",
                    "ControlPersist=no"
                ],
                &["-p", "2022", "--", "devuser1@localhost", "current-system"],
            ])
        );
        assert_eq!(spec.stdin, None);
        let v6 = raw_command(
            &target("ssh://root@[fe80::1]:22", &[]),
            &ssh(&[]),
            &Mux::Off,
            "x",
        );
        assert_eq!(
            v6.args[v6.args.len() - 5..],
            ["-p", "22", "--", "root@fe80::1", "x"].map(String::from)[..]
        );
    }

    #[test]
    fn exit_master_argv() {
        let t = target("ssh://jj@web1:2222", &["-i", "/k"]);
        let socket = Path::new("/run/user/1/hostctl/ssh/0123456789abcdef");
        let spec = exit_master_command(&t, &ssh(&["-F", "/cfg"]), socket).unwrap();
        assert_eq!(spec.program, "ssh");
        assert_eq!(
            spec.args,
            argv(&[
                &["-i", "/k", "-F", "/cfg"],
                &["-o", "ControlPath=/run/user/1/hostctl/ssh/0123456789abcdef"],
                &["-O", "exit", "-p", "2222", "--", "jj@web1"],
            ])
        );
        assert_eq!(spec.stdin, None);
        // `%` survives ssh's token expansion.
        let odd = exit_master_command(&t, &ssh(&[]), Path::new("/home/100%/s")).unwrap();
        assert!(
            odd.args.iter().any(|a| a == "ControlPath=/home/100%%/s"),
            "{odd:?}"
        );
        // A ControlPath of the user's isn't hostctl's master to stop.
        for theirs in [&["-o", "ControlPath=/mine"][..], &["-ocontrolpath /mine"]] {
            let socket = Path::new("/s");
            assert_eq!(exit_master_command(&t, &ssh(theirs), socket), None);
            let host = target("web1", theirs);
            assert_eq!(exit_master_command(&host, &ssh(&[]), socket), None);
        }
    }

    #[test]
    fn options_the_user_set_are_not_repeated() {
        let t = target("web1", &["-oStrictHostKeyChecking yes"]);
        let s = ssh(&[
            "-o",
            "batchmode=no",
            "-o",
            "ControlPath=/mine",
            "-p",
            "22",
            "-o",
            "ServerAliveInterval=30",
        ]);
        let spec = script_command(&Target::Ssh(t), &s, &private("/s/0123456789abcdef"), "");
        let args: Vec<&str> = spec.args.iter().map(String::as_str).collect();
        assert_eq!(
            args,
            [
                "-oStrictHostKeyChecking yes",
                "-o",
                "batchmode=no",
                "-o",
                "ControlPath=/mine",
                "-p",
                "22",
                "-o",
                "ServerAliveInterval=30",
                "-T",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ClearAllForwardings=yes",
                "-o",
                "ForwardAgent=no",
                "-o",
                "ForwardX11=no",
                "-o",
                "PermitLocalCommand=no",
                "-o",
                "ControlMaster=auto",
                "-o",
                "ControlPersist=600",
                "-o",
                "ServerAliveCountMax=2",
                "--",
                "web1",
                "sh",
                "-s",
            ]
        );
        assert!(sets_option(["-o", "Foo=1"], "foo"));
        assert!(sets_option(["-oFoo=1"], "FOO"));
        assert!(sets_option(["-o", " Foo bar"], "Foo"));
        assert!(!sets_option(["-o", "FooBar=1"], "Foo"));
        assert!(!sets_option(["Foo=1", "-p", "-o"], "Foo"));
        assert!(!sets_option(["-o"], "x"));
    }

    #[test]
    fn interactive_command_argv() {
        let t = target("ssh://jj@web1:2222", &["-i", "/k"]);
        let spec = interactive_command(
            &Target::Ssh(t.clone()),
            &ssh(&["-F", "/cfg"]),
            "ignored",
            Some("/tmp/hostctl.AbC123"),
        );
        assert_eq!(
            spec.args,
            argv(&[
                &["-i", "/k", "-F", "/cfg"],
                &[
                    "-t",
                    "-o",
                    "ControlMaster=no",
                    "-o",
                    "ClearAllForwardings=yes"
                ],
                &["-p", "2222", "--", "jj@web1", "sh", "/tmp/hostctl.AbC123"],
            ])
        );
        assert_eq!(spec.stdin, None);
        let odd = interactive_command(&Target::Ssh(t.clone()), &ssh(&[]), "", Some("/tmp/a b;x"));
        assert_eq!(odd.args.last().map(String::as_str), Some("'/tmp/a b;x'"));

        // Without an upload (local hosts, activate-on = "local"): here.
        let want = CommandSpec::new("sh").args(["-c", &interactive_script("make it so", false)]);
        for target in [Target::Local { assumed: true }, Target::Ssh(t)] {
            assert_eq!(
                interactive_command(&target, &ssh(&["-v"]), "make it so", None),
                want
            );
        }
    }

    #[test]
    fn upload_command_argv() {
        let t = target("jj@web1", &[]);
        let spec = upload_command(&t, &ssh(&[]), &private("/s/0123456789abcdef"), "echo hi\n");
        assert_eq!(
            spec.args[spec.args.len() - 4..],
            ["--", "jj@web1", "sh", "-s"].map(String::from)[..]
        );
        let stdin = spec.stdin.unwrap();
        assert!(stdin.starts_with("umask 077 && "), "{stdin}");
        assert!(stdin.ends_with("\necho hi\nHOSTCTL_EOF\n"), "{stdin}");
    }

    #[test]
    fn upload_delimiter_never_occurs_in_the_script() {
        assert!(
            upload_script("").ends_with("<<'HOSTCTL_EOF' && printf '%s\\n' \"$f\"\nHOSTCTL_EOF\n")
        );
        assert!(upload_script("x").ends_with("\nx\nHOSTCTL_EOF\n"));
        let tricky = "HOSTCTL_EOF\nHOSTCTL_EOF_\n";
        let script = upload_script(tricky);
        assert!(script.contains("<<'HOSTCTL_EOF__'"), "{script}");
        assert!(
            script.ends_with("\nHOSTCTL_EOF\nHOSTCTL_EOF_\nHOSTCTL_EOF__\n"),
            "{script}"
        );
    }

    fn run_sh(spec: &CommandSpec) -> exec::Output {
        block_on(exec::run(spec, Some(Duration::from_secs(10)))).unwrap()
    }

    #[test]
    fn upload_then_run_for_real() {
        let tmp = std::env::temp_dir();
        if !is_safe_remote_path(&tmp.to_string_lossy()) {
            eprintln!("skipping: temp dir {tmp:?} isn't a safe remote path");
            return;
        }
        let dir = tempfile::Builder::new()
            .prefix("hostctl")
            .tempdir_in(&tmp)
            .unwrap();
        let ran = dir.path().join("ran");
        let command = format!(
            "printf '%s\\n' \"$PATH\" >{}\necho \"it's $((6 * 7))\"",
            sh_quote(&ran.to_string_lossy())
        );
        let script = interactive_script(&command, true);
        let upload = upload_command(&target("web1", &[]), &ssh(&[]), &Mux::Off, &script);
        // What the remote `sh -s` gets, run here instead.
        let receiver = CommandSpec::new("sh")
            .arg("-s")
            .stdin(upload.stdin.unwrap())
            .env("TMPDIR", dir.path().to_string_lossy());
        let out = run_sh(&receiver);
        assert_eq!(out.code, Some(0), "{}", out.stderr);
        let path = out.stdout.strip_suffix('\n').unwrap();
        assert!(is_safe_remote_path(path), "{path:?}");
        assert!(Path::new(path).starts_with(dir.path()), "{path:?}");
        let meta = std::fs::metadata(path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_to_string(path).unwrap(), script);

        let out = run_sh(&CommandSpec::new("sh").arg(path));
        assert_eq!(out.code, Some(0), "{}", out.stderr);
        assert_eq!(out.stdout, "it's 42\n");
        assert!(!Path::new(path).exists(), "the script deletes itself");
        let seen = std::fs::read_to_string(&ran).unwrap();
        let inherited = std::env::var("PATH").unwrap_or_default();
        assert!(
            seen.starts_with(&format!("/run/wrappers/bin:{inherited}:")),
            "{seen}"
        );
        assert!(
            seen.contains(":/nix/var/nix/profiles/default/bin:"),
            "{seen}"
        );
        assert!(
            seen.trim_end().ends_with("/bin") && seen.contains("/etc/profiles/per-user/"),
            "{seen}"
        );
        assert!(!seen.contains('$'), "{seen}");
    }

    #[test]
    fn upload_falls_back_to_tmp_when_tmpdir_is_gone() {
        let receiver = CommandSpec::new("sh")
            .arg("-s")
            .stdin(upload_script("true\n"))
            .env("TMPDIR", "/nonexistent/hostctl-test");
        let out = run_sh(&receiver);
        if out.code != Some(0) {
            eprintln!("skipping: no writable /tmp ({})", out.stderr);
            return;
        }
        let path = out.stdout.trim_end();
        assert!(path.starts_with("/tmp/hostctl."), "{path:?}");
        assert_eq!(std::fs::read_to_string(path).unwrap(), "true\n");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn interactive_script_runs_locally_with_the_prelude() {
        let script = interactive_script("printf '%s' \"$PATH\"", false);
        assert!(!script.contains("rm -f"));
        assert!(interactive_script("x", true).starts_with("rm -f \"$0\"\n"));
        let spec = interactive_command(
            &Target::Local { assumed: false },
            &ssh(&[]),
            "printf '%s' \"$PATH\"",
            None,
        );
        assert_eq!(spec.args, ["-c".to_string(), script]);
        let out = run_sh(&spec);
        assert_eq!(out.code, Some(0), "{}", out.stderr);
        assert!(
            out.stdout.ends_with("/bin") && out.stdout.contains(":/run/current-system/sw/bin:"),
            "{}",
            out.stdout
        );
    }

    #[test]
    fn generated_scripts_pass_shellcheck() {
        let Some(shellcheck) = which("shellcheck") else {
            eprintln!("skipping: shellcheck not on PATH");
            return;
        };
        let command = "out=$(nix build --no-link --print-out-paths 'x#.y') && [ \"$out\" = /nix/store/z ] && \"$out/activate\"";
        for script in [
            interactive_script(command, true),
            interactive_script(command, false),
            upload_script(&interactive_script(command, true)),
        ] {
            let spec = CommandSpec::new(shellcheck.to_string_lossy())
                .args(["-s", "sh", "-"])
                .stdin(script.clone());
            let out = run_sh(&spec);
            assert_eq!(out.code, Some(0), "{}\n{script}", out.stdout);
        }
    }

    #[test]
    fn safe_remote_paths() {
        assert!(is_safe_remote_path("/tmp/hostctl.AbC123"));
        assert!(is_safe_remote_path(
            "/var/folders/zz/zyxvpxvq6csfxvn_n0000000000000/T//hostctl.x-y"
        ));
        for bad in [
            "",
            "tmp/x",
            "/tmp/a b",
            "/tmp/$(id)",
            "/tmp/x;y",
            "/tmp/x\n",
            "/tmp/'",
            "/tmp/é",
        ] {
            assert!(!is_safe_remote_path(bad), "{bad:?}");
        }
    }

    #[test]
    fn control_dir_prefers_the_runtime_dir() {
        let os = |s: &str| Some(OsString::from(s));
        assert_eq!(
            control_dir_in(os("/run/user/1000"), os("/home/jj")),
            Some(PathBuf::from("/run/user/1000/hostctl/ssh"))
        );
        assert_eq!(
            control_dir_in(None, os("/Users/jj")),
            Some(PathBuf::from("/Users/jj/.cache/hostctl/ssh"))
        );
        assert_eq!(
            control_dir_in(os(""), os("/home/jj")),
            Some(PathBuf::from("/home/jj/.cache/hostctl/ssh"))
        );
        assert_eq!(
            control_dir_in(os("relative"), os("/home/jj")),
            Some(PathBuf::from("/home/jj/.cache/hostctl/ssh"))
        );
        assert_eq!(control_dir_in(None, os("relative")), None);
        assert_eq!(control_dir_in(None, None), None);
        // The real environment gives an absolute path, or none at all.
        assert!(control_dir().is_none_or(|dir| dir.is_absolute() && dir.ends_with("hostctl/ssh")));
    }

    #[test]
    fn control_sockets_identify_connections() {
        let dir = Path::new("/run/user/1000/hostctl/ssh");
        let socket = |t: &SshTarget, s: &SshConfig| control_socket(dir, t, s);
        let base = socket(&target("jj@web1", &[]), &ssh(&[]));
        let name = base.file_name().unwrap().to_str().unwrap();
        assert_eq!(base.parent(), Some(dir));
        assert_eq!(name.len(), 16);
        assert!(
            name.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        // Stable, and the configured spelling doesn't matter.
        assert_eq!(socket(&target("ssh://jj@web1", &[]), &ssh(&[])), base);
        let others = [
            socket(&target("web1", &[]), &ssh(&[])),
            socket(&target("root@web1", &[]), &ssh(&[])),
            socket(&target("jj@web2", &[]), &ssh(&[])),
            socket(&target("ssh://jj@web1:22", &[]), &ssh(&[])),
            socket(&target("ssh://jj@web1:2222", &[]), &ssh(&[])),
            socket(&target("jj@web1", &["-i", "/k"]), &ssh(&[])),
            socket(&target("jj@web1", &[]), &ssh(&["-i", "/k"])),
            socket(&target("jj@web1", &["-i/k"]), &ssh(&[])),
            socket(
                &target("jj@web1", &[]),
                &SshConfig {
                    command: "/opt/ssh".into(),
                    ..ssh(&[])
                },
            ),
        ];
        let mut all: Vec<&PathBuf> = others.iter().chain([&base]).collect();
        all.sort();
        all.dedup();
        assert_eq!(all.len(), others.len(), "collision among {others:?}");
        // Whether an option is per host or global doesn't change the argv.
        assert_eq!(
            socket(&target("jj@web1", &["-i"]), &ssh(&["/k"])),
            socket(&target("jj@web1", &["-i", "/k"]), &ssh(&[]))
        );
    }

    #[test]
    fn mux_for_falls_back_to_off() {
        let t = target("jj@web1", &[]);
        let dir = Path::new("/run/user/1000/hostctl/ssh");
        let mux = mux_for(&t, &ssh(&[]), Some(dir));
        assert_eq!(
            mux,
            Mux::Private {
                socket: control_socket(dir, &t, &ssh(&[])),
                persist_secs: 600
            }
        );
        let no_persist = SshConfig {
            persist: HumanDuration::from_secs(0),
            ..ssh(&[])
        };
        assert_eq!(mux_for(&t, &no_persist, Some(dir)), Mux::Off);
        assert_eq!(mux_for(&t, &ssh(&[]), None), Mux::Off);
        let short = SshConfig {
            persist: HumanDuration(Duration::from_millis(300)),
            ..ssh(&[])
        };
        assert!(matches!(
            mux_for(&t, &short, Some(dir)),
            Mux::Private {
                persist_secs: 1,
                ..
            }
        ));
        // 69 + 1 + 16 = 86 bytes is the most that fits.
        let longest = format!("/{}", "d".repeat(68));
        assert!(matches!(
            mux_for(&t, &ssh(&[]), Some(Path::new(&longest))),
            Mux::Private { .. }
        ));
        let too_long = format!("/{}", "d".repeat(69));
        assert_eq!(mux_for(&t, &ssh(&[]), Some(Path::new(&too_long))), Mux::Off);
        for odd in [
            "/home/John Smith/.cache",
            "/home/it's",
            "/home/a\"b",
            "/home/a\\b",
        ] {
            assert_eq!(
                mux_for(&t, &ssh(&[]), Some(Path::new(odd))),
                Mux::Off,
                "{odd}"
            );
        }
        // `%` survives ssh's token expansion.
        let percent = mux_for(&t, &ssh(&[]), Some(Path::new("/home/100%")));
        let options = percent.options();
        assert!(
            options[3].starts_with("ControlPath=/home/100%%/"),
            "{options:?}"
        );
    }

    #[test]
    fn real_ssh_reads_the_control_path_as_given() {
        let Some(ssh_bin) = which("ssh") else {
            eprintln!("skipping: ssh not on PATH");
            return;
        };
        let mux = private("/run/user/1000/hostctl/ssh/100%-0123456789abcdef");
        // `-F /dev/null` keeps the user's ssh config (and its `Match exec`) out.
        let spec = CommandSpec::new(ssh_bin.to_string_lossy())
            .args(["-F", "/dev/null", "-G"])
            .args(mux.options())
            .arg("web1");
        let out = run_sh(&spec);
        assert_eq!(out.code, Some(0), "{}", out.stderr);
        for want in [
            "controlpath /run/user/1000/hostctl/ssh/100%-0123456789abcdef",
            "serveraliveinterval 5",
            "serveralivecountmax 2",
        ] {
            assert!(
                out.stdout.lines().any(|l| l == want),
                "{want}: {}",
                out.stdout
            );
        }
    }

    #[test]
    fn real_ssh_takes_the_exit_command() {
        let Some(ssh_bin) = which("ssh") else {
            eprintln!("skipping: ssh not on PATH");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("nothing-here");
        let config = SshConfig {
            command: ssh_bin.to_string_lossy().into_owned(),
            ..ssh(&["-F", "/dev/null"])
        };
        let spec = exit_master_command(&target("web1", &[]), &config, &socket).unwrap();
        // No master: it says so (and connects to nothing).
        let out = run_sh(&spec);
        assert_eq!(out.code, Some(255), "{}", out.stderr);
        assert!(
            out.stderr.contains("Control socket connect("),
            "{}",
            out.stderr
        );
    }

    // Real OpenSSH messages (9.x on Linux/macOS, 7.4 on an older Linux).
    const RESOLVE_LINUX: &str =
        "ssh: Could not resolve hostname bigmem2: Name or service not known\r\n";
    const RESOLVE_MAC: &str =
        "ssh: Could not resolve hostname mac-m1: nodename nor servname provided, or not known\n";
    const RESOLVE_DNS_DOWN: &str =
        "ssh: Could not resolve hostname github.com: Temporary failure in name resolution\n";
    const DENIED: &str =
        "devuser1@web1: Permission denied (publickey,gssapi-keyex,gssapi-with-mic).\n";
    const DENIED_74: &str =
        "Permission denied (publickey,gssapi-keyex,gssapi-with-mic,password).\r\n";
    const DENIED_REMOTE: &str = "sh: 3: cannot create /root/x: Permission denied\n";
    const TOO_MANY: &str = "Received disconnect from 10.0.0.5 port 22:2: Too many authentication failures\nDisconnected from 10.0.0.5 port 22\n";
    const CHANGED: &str = "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@
@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @
@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@
IT IS POSSIBLE THAT SOMEONE IS DOING SOMETHING NASTY!
Someone could be eavesdropping on you right now (man-in-the-middle attack)!
It is also possible that a host key has just been changed.
The fingerprint for the ED25519 key sent by the remote host is
SHA256:OBQ3qR0A4N8uLW8n+Mo2yhm7lfGcnn3EOjm1Y4VJYpw.
Please contact your system administrator.
Add correct host key in /home/jj/.ssh/known_hosts to get rid of this message.
Offending ED25519 key in /home/jj/.ssh/known_hosts:12
  remove with:
  ssh-keygen -f '/home/jj/.ssh/known_hosts' -R 'web1'
Host key for web1 has changed and you have requested strict checking.
Host key verification failed.
";
    const CHANGED_74: &str = "@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @
Offending ECDSA key in /home/jj/.ssh/known_hosts:3
ECDSA host key for [localhost]:2022 has changed and you have requested strict checking.
Host key verification failed.
";
    const UNKNOWN_STRICT: &str = "No ED25519 host key is known for web1 and you have requested strict checking.\nHost key verification failed.\n";
    const REMOTE_COMMAND: &str = "Cannot execute command-line and remote command.\n";
    const ACCEPT_NEW_74: &str = "command-line line 0: unsupported option \"accept-new\".\n";
    const KEX_RESET: &str = "kex_exchange_identification: read: Connection reset by peer\nConnection reset by 10.0.0.5 port 22\n";
    const KEX_CLOSED: &str = "kex_exchange_identification: Connection closed by remote host\nConnection closed by 10.0.0.5 port 22\n";
    const EXCHANGE_74: &str = "ssh_exchange_identification: read: Connection reset by peer\n";
    const EXCHANGE_74_CLOSED: &str =
        "ssh_exchange_identification: Connection closed by remote host\n";
    const CLOSED: &str = "Connection closed by 10.0.0.5 port 22\n";
    const TIMEOUT: &str = "ssh: connect to host 10.0.0.5 port 22: Connection timed out\n";
    const TIMEOUT_MAC: &str = "ssh: connect to host mac-m1 port 22: Operation timed out\n";
    const REFUSED: &str = "ssh: connect to host web1 port 22: Connection refused\n";
    const NO_ROUTE: &str = "ssh: connect to host 10.0.0.9 port 22: No route to host\n";
    const JUMP_DOWN: &str = "ssh: connect to host jump port 22: Connection timed out\nConnection closed by UNKNOWN port 65535\n";
    /// A failing ProxyCommand/ProxyJump's error, all ControlPersist leaves of it.
    const PROXY_CLOSED: &str = "Connection closed by UNKNOWN port 65535\r\n";
    /// sshd's MaxStartups drop, through a ProxyJump.
    const KEX_VIA_JUMP: &str = "kex_exchange_identification: Connection closed by remote host\r\nConnection closed by UNKNOWN port 65535\r\n";
    const BANNER: &str =
        "Connection timed out during banner exchange\nConnection to 10.0.0.5 port 22 timed out\n";

    #[test]
    fn hints_for_known_ssh_failures() {
        let hint = |stderr: &str| {
            ssh_hint(stderr, true).unwrap_or_else(|| panic!("no hint for {stderr:?}"))
        };
        for (stderr, host) in [(RESOLVE_LINUX, "bigmem2"), (RESOLVE_MAC, "mac-m1")] {
            let h = hint(stderr);
            assert!(
                h.starts_with(&format!("{host} doesn't resolve")) && h.contains("destination"),
                "{h}"
            );
        }
        assert!(hint(RESOLVE_DNS_DOWN).contains("looking up github.com failed"));
        for stderr in [DENIED, DENIED_74] {
            let h = hint(stderr);
            assert!(h.contains("BatchMode") && h.contains("ssh-add"), "{h}");
        }
        assert!(hint(TOO_MANY).contains("IdentitiesOnly"));
        assert_eq!(
            hint(CHANGED),
            "the host key of web1 changed (reinstalled?): if that's expected, run ssh-keygen -R web1"
        );
        assert_eq!(
            hint(CHANGED_74),
            "the host key of [localhost]:2022 changed (reinstalled?): if that's expected, run ssh-keygen -R '[localhost]:2022'"
        );
        assert!(hint(UNKNOWN_STRICT).starts_with("web1 isn't in known_hosts"));
        assert_eq!(
            hint("Host key verification failed.\n"),
            "host key verification failed: if the host was reinstalled, run ssh-keygen -R <host>"
        );
        let banner = CHANGED.lines().take(3).collect::<Vec<_>>().join("\n");
        assert_eq!(
            hint(&banner),
            "the host key changed (reinstalled?): if that's expected, run ssh-keygen -R <host>"
        );
        assert!(hint(REMOTE_COMMAND).contains(r#"["-o", "RemoteCommand=none"]"#));
        let h = hint(ACCEPT_NEW_74);
        assert!(
            h.contains("OpenSSH 7.6") && h.contains("ssh.command"),
            "{h}"
        );
        for stderr in [
            "",
            DENIED_REMOTE,
            KEX_RESET,
            KEX_CLOSED,
            CLOSED,
            TIMEOUT,
            REFUSED,
            "bash: line 1: nix: command not found\n",
            "Permission denied (publickey) and more\n",
        ] {
            assert_eq!(ssh_hint(stderr, true), None, "{stderr:?}");
        }
    }

    #[test]
    fn hints_skip_the_ssh_config_ssh_doesnt_read() {
        let hint = |stderr: &str| ssh_hint(stderr, false).unwrap();
        let h = hint(RESOLVE_LINUX);
        assert_eq!(
            h,
            "bigmem2 doesn't resolve: set the host's destination (e.g. user@bigmem2.example.com)"
        );
        let h = hint(TOO_MANY);
        assert!(
            h.contains(r#"ssh-options = ["-o", "IdentitiesOnly=yes", "-i""#)
                && !h.contains("~/.ssh/config"),
            "{h}"
        );
        let args = |args: &[&str]| args.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        assert!(reads_ssh_config(&args(&["-o", "ForwardAgent=no"])));
        assert!(!reads_ssh_config(&args(&["-F", "/dev/null"])));
        assert!(!reads_ssh_config(&args(&["-o", "X=y", "-F/dev/null"])));
    }

    #[test]
    fn hints_sanitize_what_they_quote() {
        let stderr =
            "ssh: Could not resolve hostname evil\u{1b}]0;x\u{7}: Name or service not known\n";
        let h = ssh_hint(stderr, true).unwrap();
        assert!(
            !h.contains('\u{1b}') && h.starts_with("evil\\u{1b}]0;x\\u{7}"),
            "{h}"
        );
    }

    #[test]
    fn transient_failures_are_maxstartups_drops() {
        for stderr in [
            KEX_RESET,
            KEX_CLOSED,
            EXCHANGE_74,
            EXCHANGE_74_CLOSED,
            CLOSED,
            KEX_VIA_JUMP,
        ] {
            assert!(is_transient_ssh_failure(stderr), "{stderr:?}");
        }
        for stderr in [
            "",
            RESOLVE_LINUX,
            RESOLVE_DNS_DOWN,
            DENIED,
            DENIED_74,
            TOO_MANY,
            CHANGED,
            UNKNOWN_STRICT,
            REMOTE_COMMAND,
            ACCEPT_NEW_74,
            TIMEOUT,
            TIMEOUT_MAC,
            REFUSED,
            NO_ROUTE,
            JUMP_DOWN,
            BANNER,
            "Connection closed by remote host\n",
            // A proxy that failed: retrying over the same master is no use.
            PROXY_CLOSED,
            "Connection reset by UNKNOWN port 65535\n",
        ] {
            assert!(!is_transient_ssh_failure(stderr), "{stderr:?}");
        }
    }

    #[test]
    fn a_proxy_closing_is_recognized() {
        assert!(is_proxy_closed(PROXY_CLOSED));
        assert!(is_proxy_closed(" Connection closed by UNKNOWN port 65535"));
        for line in [
            CLOSED,
            "Connection closed by UNKNOWN port 655350",
            "kex_exchange_identification: Connection closed by remote host",
            "ssh: connect to host jump port 22: Connection timed out",
        ] {
            assert!(!is_proxy_closed(line), "{line:?}");
        }
    }

    #[test]
    fn display_keeps_option_assignments_readable() {
        let spec = CommandSpec::new("ssh").args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ControlPath=/tmp/a b",
            "a=b=c",
            "=x",
            "it's",
        ]);
        assert_eq!(
            spec.display(),
            "ssh -o BatchMode=yes -o 'ControlPath=/tmp/a b' 'a=b=c' '=x' 'it'\\''s'"
        );
    }
}
