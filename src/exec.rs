//! Running processes: captured with a timeout, or interactively on the
//! terminal. See DESIGN.md ("Process execution").

use std::io;
use std::pin::pin;
use std::process::{ExitStatus, Stdio};
use std::sync::{OnceLock, mpsc};
use std::time::Duration;

use libc::c_int;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::{Child, ChildStdin, Command};
use tokio::time::Instant;

use crate::remote::CommandSpec;

/// Between a process group's SIGTERM and its SIGKILL.
const KILL_GRACE: Duration = Duration::from_millis(200);

/// How long pipes are still read once the child is gone. What it wrote is
/// already buffered, so this only bounds descendants holding them open.
const DRAIN: Duration = Duration::from_millis(100);

/// Read size for output past the cap, which is discarded.
const DISCARD_CHUNK: usize = 64 << 10;

/// What a captured run produced.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Output {
    /// Exit code; `None` if killed by a signal (including our timeout kill).
    pub code: Option<i32>,
    /// Lossy UTF-8, at most `max_output` bytes.
    pub stdout: String,
    /// Lossy UTF-8, at most `max_output` bytes.
    pub stderr: String,
    pub elapsed: Duration,
    pub timed_out: bool,
    /// Some output was dropped because of `max_output`.
    pub truncated: bool,
}

impl Output {
    /// Exited with 0 before the timeout.
    pub fn success(&self) -> bool {
        !self.timed_out && self.code == Some(0)
    }
}

/// Spawn `spec` and wait for it:
///
/// - the child starts in a new session (`setsid` in `pre_exec`): its own
///   process group, no controlling terminal — so Ctrl-C and tty prompts
///   (askpass, git, ProxyCommands) never reach it;
/// - environment: inherited plus `spec.env` plus `GIT_TERMINAL_PROMPT=0`;
///   stdin is `spec.stdin` (written concurrently with reading output, then
///   closed) or null;
/// - stdout/stderr are captured up to `spec.max_output` bytes each (more is
///   read and discarded; `truncated` set);
/// - on `timeout`, and when the future is dropped, the whole process group
///   gets SIGTERM, then SIGKILL after 200 ms; after a kill, pipes are drained
///   for at most 100 ms (grandchildren may hold them open) and whatever was
///   read is returned with `timed_out = true`;
/// - when the child exits by itself, pipes are drained for at most 100 ms
///   too, and descendants it left running are left alone (a ControlPersist
///   master's ProxyCommand is that master's transport);
/// - `Err` only when the process can't be spawned (e.g. program not found).
pub async fn run(spec: &CommandSpec, timeout: Option<Duration>) -> std::io::Result<Output> {
    let start = Instant::now();
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .envs(spec.env.iter().map(|(key, value)| (key, value)))
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(match spec.stdin {
            Some(_) => Stdio::piped(),
            None => Stdio::null(),
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: `new_session` only calls setsid(2), which is async-signal-safe.
    unsafe { command.pre_exec(new_session) };
    let mut child = command.spawn()?;
    let stdin = child.stdin.take();
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let mut child = Group::new(child);
    let mut out = Capture::new(spec.max_output);
    let mut err = Capture::new(spec.max_output);

    let (status, timed_out) = {
        let input = spec.stdin.as_deref().unwrap_or_default().as_bytes();
        let mut io = pin!(async {
            tokio::join!(feed(stdin, input), out.read(stdout), err.read(stderr));
        });
        let mut io_done = false;
        let mut deadline = pin!(async {
            match timeout.and_then(|t| start.checked_add(t)) {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        });
        let exited = loop {
            tokio::select! {
                biased;
                status = child.wait() => break Some(status),
                () = &mut io, if !io_done => io_done = true,
                () = &mut deadline => break None,
            }
        };
        match exited {
            Some(status) => {
                child.disarm();
                if !io_done {
                    let _ = tokio::time::timeout(DRAIN, &mut io).await;
                }
                (status, false)
            }
            None => {
                child.signal(libc::SIGTERM);
                // Keep reading through the grace period: last words count.
                let grace_end = Instant::now() + KILL_GRACE;
                if !io_done {
                    io_done = tokio::time::timeout_at(grace_end, &mut io).await.is_ok();
                }
                // The leader stays unreaped until after SIGKILL, so the
                // group id can't have been reused by then.
                tokio::time::sleep_until(grace_end).await;
                child.signal(libc::SIGKILL);
                let drained = tokio::time::timeout(DRAIN, async {
                    let ((), status) = tokio::join!(
                        async {
                            if !io_done {
                                (&mut io).await;
                            }
                        },
                        child.wait(),
                    );
                    status
                })
                .await;
                child.disarm();
                (drained.ok().flatten(), true)
            }
        }
    };

    let (stdout, out_cut) = out.into_string();
    let (stderr, err_cut) = err.into_string();
    Ok(Output {
        code: status.and_then(|s| s.code()),
        stdout,
        stderr,
        elapsed: start.elapsed(),
        timed_out,
        truncated: out_cut || err_cut,
    })
}

/// Run `spec` attached to the terminal (inheriting stdin/stdout/stderr and
/// the foreground process group; `spec.stdin` is ignored) and wait for it
/// without blocking the runtime. While it runs, hostctl ignores SIGINT and
/// SIGQUIT (like `system(3)`), so Ctrl-C reaches only the child; the
/// previous dispositions are restored afterwards (also when the future is
/// dropped; the child is then left running). Calls are serialized.
pub async fn run_interactive(spec: &CommandSpec) -> std::io::Result<ExitStatus> {
    // The terminal and signal dispositions belong to the whole process.
    static TERMINAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _terminal = TERMINAL.lock().await;
    let ignoring = IgnoreInterrupts::start()?;
    let saved = ignoring.saved;
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .envs(spec.env.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    // Ignored signals stay ignored across exec, so the child gets back the
    // dispositions hostctl had (handlers become SIG_DFL on exec).
    // SAFETY: only sigaction(2), which is async-signal-safe, runs in the child.
    unsafe { command.pre_exec(move || restore_interrupts(&saved)) };
    command.spawn()?.wait().await
}

/// `pre_exec` hook: a new session, so no controlling terminal and a fresh
/// process group led by the child.
fn new_session() -> io::Result<()> {
    // SAFETY: setsid(2) takes no arguments and touches no memory.
    if unsafe { libc::setsid() } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Write `input` to the child's stdin, then close it. A child that exits
/// without reading everything is fine (the write fails with EPIPE).
async fn feed(stdin: Option<ChildStdin>, input: &[u8]) {
    if let Some(mut stdin) = stdin {
        let _ = stdin.write_all(input).await;
    }
}

/// The spawned child, a session leader, so its pid is also its process
/// group's id. While armed, dropping this kills the whole group: SIGTERM
/// at once, SIGKILL [`KILL_GRACE`] later.
struct Group {
    child: Child,
    pgid: Option<libc::pid_t>,
}

impl Group {
    fn new(child: Child) -> Group {
        // Never 0 or 1: killpg would signal our own group or every process.
        let pgid = child
            .id()
            .and_then(|id| libc::pid_t::try_from(id).ok())
            .filter(|&id| id > 1);
        Group { child, pgid }
    }

    fn signal(&self, signal: c_int) {
        if let Some(pgid) = self.pgid {
            killpg(pgid, signal);
        }
    }

    /// Nothing is killed on drop any more.
    fn disarm(&mut self) {
        self.pgid = None;
    }

    /// Reap the leader; `None` if waiting failed.
    async fn wait(&mut self) -> Option<ExitStatus> {
        self.child.wait().await.ok()
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        if let Some(pgid) = self.pgid.take()
            && killpg(pgid, libc::SIGTERM)
        {
            kill_later(pgid);
        }
    }
}

/// killpg(2); whether some process got the signal.
fn killpg(pgid: libc::pid_t, signal: c_int) -> bool {
    // SAFETY: a plain syscall; callers pass a child's group id (> 1).
    unsafe { libc::killpg(pgid, signal) == 0 }
}

/// SIGKILL `pgid` after [`KILL_GRACE`], from a helper thread, since a
/// dropped run can't wait. Every deadline is "now + grace", so they arrive
/// in order and a FIFO is all the scheduling needed.
fn kill_later(pgid: libc::pid_t) {
    type Queue = mpsc::Sender<(std::time::Instant, libc::pid_t)>;
    static QUEUE: OnceLock<Option<Queue>> = OnceLock::new();
    let queue = QUEUE.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<(std::time::Instant, libc::pid_t)>();
        std::thread::Builder::new()
            .name("hostctl-kill".into())
            .spawn(move || {
                for (at, pgid) in rx {
                    std::thread::sleep(at.saturating_duration_since(std::time::Instant::now()));
                    killpg(pgid, libc::SIGKILL);
                }
            })
            .ok()
            .map(|_| tx)
    });
    let at = std::time::Instant::now() + KILL_GRACE;
    if queue.as_ref().is_none_or(|tx| tx.send((at, pgid)).is_err()) {
        // No helper thread: better no grace than no kill.
        killpg(pgid, libc::SIGKILL);
    }
}

/// One output stream, kept up to a cap.
struct Capture {
    kept: Vec<u8>,
    cap: usize,
    truncated: bool,
}

impl Capture {
    fn new(cap: usize) -> Capture {
        Capture {
            kept: Vec::new(),
            cap,
            truncated: false,
        }
    }

    /// Read `pipe` to its end, keeping the first `cap` bytes. Reading on
    /// past the cap keeps the child from blocking on a full pipe.
    async fn read(&mut self, pipe: Option<impl AsyncRead + Unpin>) {
        let Some(mut pipe) = pipe else { return };
        let mut discard = Vec::new();
        loop {
            let room = self.cap - self.kept.len();
            let read = if room > 0 {
                (&mut pipe).take(room as u64).read_buf(&mut self.kept).await
            } else {
                if discard.is_empty() {
                    discard.resize(DISCARD_CHUNK, 0);
                }
                let read = pipe.read(&mut discard).await;
                self.truncated |= matches!(read, Ok(n) if n > 0);
                read
            };
            if !matches!(read, Ok(n) if n > 0) {
                return;
            }
        }
    }

    /// The text (lossy UTF-8, at most `cap` bytes) and whether anything was
    /// dropped. A character cut in half by the cap is dropped whole rather
    /// than turned into U+FFFD.
    fn into_string(self) -> (String, bool) {
        let Capture {
            kept,
            cap,
            mut truncated,
        } = self;
        let mut text = match String::from_utf8(kept) {
            Ok(text) => text,
            Err(e) => {
                let valid = e.utf8_error().valid_up_to();
                let cut_char = truncated && e.utf8_error().error_len().is_none();
                let mut bytes = e.into_bytes();
                if cut_char {
                    bytes.truncate(valid);
                }
                String::from_utf8(bytes)
                    .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
            }
        };
        // Replacement characters are longer than the bytes they replace.
        if text.len() > cap {
            let mut end = cap;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            truncated = true;
        }
        (text, truncated)
    }
}

const INTERRUPTS: [c_int; 2] = [libc::SIGINT, libc::SIGQUIT];

/// SIGINT and SIGQUIT are ignored while this lives, as during `system(3)`;
/// the saved dispositions come back on drop.
struct IgnoreInterrupts {
    saved: [libc::sigaction; 2],
}

impl IgnoreInterrupts {
    fn start() -> io::Result<IgnoreInterrupts> {
        // SAFETY: all-zero is a valid sigaction (SIG_DFL, empty mask, no flags).
        let mut ignore: libc::sigaction = unsafe { std::mem::zeroed() };
        ignore.sa_sigaction = libc::SIG_IGN;
        let mut saved = [ignore; 2];
        for (i, &signal) in INTERRUPTS.iter().enumerate() {
            // SAFETY: a valid signal number and valid pointers.
            if unsafe { libc::sigaction(signal, &ignore, &mut saved[i]) } != 0 {
                let error = io::Error::last_os_error();
                let _ = restore_interrupts(&saved[..i]);
                return Err(error);
            }
        }
        Ok(IgnoreInterrupts { saved })
    }
}

impl Drop for IgnoreInterrupts {
    fn drop(&mut self) {
        let _ = restore_interrupts(&self.saved);
    }
}

/// Reinstall `saved` dispositions of [`INTERRUPTS`] (in that order).
/// Async-signal-safe: also runs between fork and exec.
fn restore_interrupts(saved: &[libc::sigaction]) -> io::Result<()> {
    for (&signal, action) in INTERRUPTS.iter().zip(saved) {
        // SAFETY: a valid signal number and a sigaction from sigaction(2).
        if unsafe { libc::sigaction(signal, action, std::ptr::null_mut()) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::Path;

    const LONG: Duration = Duration::from_secs(10);

    fn sh(script: &str) -> CommandSpec {
        CommandSpec::new("sh").args(["-c", script])
    }

    async fn run_ok(spec: &CommandSpec) -> Output {
        run(spec, Some(LONG)).await.expect("spawn")
    }

    /// Whether `pid` is a live process (zombies count as dead).
    fn alive(pid: i32) -> bool {
        if Path::new("/proc/self/stat").exists() {
            return std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
                let state = stat
                    .rsplit_once(") ")
                    .and_then(|(_, rest)| rest.chars().next());
                !matches!(state, Some('Z' | 'X'))
            });
        }
        // SAFETY: signal 0 only checks for existence.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// Wait up to `within` for `pid` to die.
    async fn gone(pid: i32, within: Duration) -> bool {
        let until = std::time::Instant::now() + within;
        while alive(pid) {
            if std::time::Instant::now() > until {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        true
    }

    fn pids(text: &str) -> Vec<i32> {
        text.lines()
            .map(|l| l.trim().parse().expect("pid"))
            .collect()
    }

    #[tokio::test]
    async fn feeds_stdin_and_captures_stdout() {
        let out = run_ok(&CommandSpec::new("cat").stdin("hello\nwörld")).await;
        assert_eq!(out.stdout, "hello\nwörld");
        assert_eq!(out.stderr, "");
        assert_eq!(out.code, Some(0));
        assert!(out.success() && !out.timed_out && !out.truncated);
        assert!(out.elapsed > Duration::ZERO && out.elapsed < LONG);
    }

    #[tokio::test]
    async fn stdin_is_null_unless_given() {
        // `cat` would hang on an inherited terminal.
        assert_eq!(run_ok(&CommandSpec::new("cat")).await.stdout, "");
        assert_eq!(run_ok(&CommandSpec::new("cat").stdin("")).await.stdout, "");
    }

    #[tokio::test]
    async fn moves_8_mib_through_all_three_pipes_at_once() {
        // tee blocks on whichever pipe isn't drained: any sequencing of
        // writing stdin and reading stdout/stderr would deadlock.
        let alphabet = b"0123456789abcdefghijklmnopqrstuvwxyz\n";
        let input: String = (0..8 << 20)
            .map(|i| alphabet[i % alphabet.len()] as char)
            .collect();
        let spec = sh("tee /dev/stderr")
            .stdin(input.clone())
            .max_output(16 << 20);
        let out = run_ok(&spec).await;
        assert_eq!(
            out.code,
            Some(0),
            "{}",
            &out.stderr[..out.stderr.len().min(200)]
        );
        assert!(out.stdout == input, "stdout differs");
        assert!(out.stderr == input, "stderr differs");
        assert!(!out.truncated);
    }

    #[tokio::test]
    async fn unread_stdin_is_not_an_error() {
        let out = run_ok(&CommandSpec::new("true").stdin("x".repeat(8 << 20))).await;
        assert_eq!(out.code, Some(0));
        assert!(out.success());
    }

    #[tokio::test]
    async fn runs_overlap_on_a_single_thread() {
        let spec = sh("sleep 0.3; echo done").stdin("x");
        let start = std::time::Instant::now();
        let outs = futures_util::future::join_all((0..32).map(|_| run(&spec, Some(LONG)))).await;
        // One after the other would take 9.6 s.
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "{:?}",
            start.elapsed()
        );
        assert!(
            outs.iter()
                .all(|out| out.as_ref().unwrap().stdout == "done\n")
        );
    }

    #[tokio::test]
    async fn captures_stderr_separately() {
        let out = run_ok(&sh("echo out; echo err >&2; echo more >&2")).await;
        assert_eq!(out.stdout, "out\n");
        assert_eq!(out.stderr, "err\nmore\n");
    }

    #[tokio::test]
    async fn reports_exit_codes() {
        for code in [0, 1, 3, 42, 255] {
            let out = run_ok(&sh(&format!("exit {code}"))).await;
            assert_eq!(out.code, Some(code));
            assert_eq!(out.success(), code == 0);
            assert!(!out.timed_out);
        }
    }

    #[tokio::test]
    async fn death_by_signal_has_no_code() {
        for signal in ["TERM", "KILL", "USR1"] {
            let out = run_ok(&sh(&format!("echo before; kill -{signal} $$"))).await;
            assert_eq!(out.code, None, "{signal}");
            assert_eq!(out.stdout, "before\n");
            assert!(!out.timed_out && !out.success());
        }
    }

    /// Run `script` into a 300 ms timeout and check the run ended within
    /// 0.5 s of it, killed. Only if the child didn't even reach the output
    /// `ready` looks for (a very loaded machine), retry with more time.
    async fn run_into_timeout(script: &str, ready: impl Fn(&Output) -> bool) -> Output {
        for timeout in [300, 1000, 3000].map(Duration::from_millis) {
            let start = std::time::Instant::now();
            let out = run(&sh(script), Some(timeout)).await.unwrap();
            let took = start.elapsed();
            assert!(out.timed_out && !out.success(), "{out:?}");
            assert_eq!(out.code, None);
            // The group gets its grace period after SIGTERM, every time.
            assert!(took >= timeout + KILL_GRACE, "{took:?}");
            assert!(took < timeout + Duration::from_millis(500), "{took:?}");
            assert!(out.elapsed >= timeout && out.elapsed <= took);
            if ready(&out) {
                return out;
            }
        }
        panic!("{script:?} never got going");
    }

    #[tokio::test]
    async fn timeout_returns_partial_output() {
        let out = run_into_timeout("echo partial; sleep 30", |out| !out.stdout.is_empty()).await;
        assert_eq!(out.stdout, "partial\n");
    }

    #[tokio::test]
    async fn timeout_kills_grandchildren_holding_the_pipes() {
        let out =
            run_into_timeout("sleep 30 & echo $!; wait", |out| out.stdout.ends_with('\n')).await;
        let [sleeper] = pids(&out.stdout)[..] else {
            panic!("{out:?}")
        };
        assert!(
            gone(sleeper, Duration::from_secs(1)).await,
            "sleep {sleeper} survived"
        );
    }

    #[tokio::test]
    async fn timeout_kills_a_proxy_that_ignores_sigterm() {
        // Like ssh stuck behind a ProxyCommand: the leader dies of SIGTERM,
        // the proxy ignores it and keeps the pipes open until SIGKILL.
        let script = "(trap '' TERM; exec sleep 30) & echo $!; echo connecting >&2; exec sleep 30";
        let out = run_into_timeout(script, |out| out.stderr.ends_with('\n')).await;
        assert_eq!(out.stderr, "connecting\n");
        let [proxy] = pids(&out.stdout)[..] else {
            panic!("{out:?}")
        };
        assert!(
            gone(proxy, Duration::from_secs(1)).await,
            "proxy {proxy} survived"
        );
    }

    #[tokio::test]
    async fn exit_returns_promptly_and_spares_descendants() {
        // A ControlPersist master's ProxyCommand outlives the ssh client
        // and keeps its stderr: the run must end anyway, without killing it.
        let start = std::time::Instant::now();
        let out = run(&sh("sleep 5 & echo $!"), None).await.unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "{:?}",
            start.elapsed()
        );
        assert_eq!(out.code, Some(0));
        assert!(!out.timed_out);
        let [sleeper] = pids(&out.stdout)[..] else {
            panic!("{out:?}")
        };
        assert!(alive(sleeper));
        // SAFETY: plain syscall on the pid the script reported.
        unsafe { libc::kill(sleeper, libc::SIGKILL) };
    }

    /// Start `script` (given a file to write pids into) and wait until it
    /// wrote `count` of them. Dropping the returned run cancels it.
    async fn start(
        script: &str,
        count: usize,
    ) -> (impl Future<Output = io::Result<Output>> + Unpin, Vec<i32>) {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pids");
        let spec = sh(script).args(["sh", file.to_str().unwrap()]);
        let mut running = Box::pin(async move { run(&spec, None).await });
        let until = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            tokio::select! {
                biased;
                out = &mut running => panic!("finished early: {out:?}"),
                () = tokio::time::sleep(Duration::from_millis(5)) => {}
            }
            let text = std::fs::read_to_string(&file).unwrap_or_default();
            if text.lines().count() >= count && text.ends_with('\n') {
                return (running, pids(&text));
            }
            assert!(std::time::Instant::now() < until, "no pids in {file:?}");
        }
    }

    /// Whether `pid` ignores `signal`; `None` without /proc.
    fn ignores(pid: i32, signal: c_int) -> Option<bool> {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        let mask = status.lines().find_map(|l| l.strip_prefix("SigIgn:"))?;
        let mask = u64::from_str_radix(mask.trim(), 16).ok()?;
        Some(mask & (1 << (signal - 1)) != 0)
    }

    #[tokio::test]
    async fn dropping_the_future_kills_the_group() {
        let script = r#"echo $$ >"$1"; sleep 30 & echo $! >>"$1"; wait"#;
        let (running, pids) = start(script, 2).await;
        drop(running);
        for pid in pids {
            assert!(gone(pid, Duration::from_secs(1)).await, "{pid} survived");
        }
    }

    #[tokio::test]
    async fn dropping_the_future_sigkills_what_ignores_sigterm() {
        let script = r#"trap '' TERM; echo $$ >"$1"; sleep 30 & echo $! >>"$1"; wait"#;
        let (running, pids) = start(script, 2).await;
        // Only the SIGKILL that follows the SIGTERM can get these.
        for &pid in &pids {
            assert_ne!(ignores(pid, libc::SIGTERM), Some(false), "{pid}");
        }
        drop(running);
        for pid in pids {
            assert!(gone(pid, Duration::from_secs(2)).await, "{pid} survived");
        }
    }

    #[tokio::test]
    async fn caps_each_stream_separately() {
        let input = "a".repeat(10_000);
        let out = run_ok(
            &CommandSpec::new("cat")
                .stdin(input.clone())
                .max_output(1000),
        )
        .await;
        assert_eq!(out.stdout, input[..1000]);
        assert!(out.truncated);
        // Reading went on past the cap, so cat wasn't blocked.
        assert_eq!(out.code, Some(0));

        let spec = sh("cat >&2; echo small").stdin(input).max_output(1000);
        let out = run_ok(&spec).await;
        assert_eq!(out.stdout, "small\n");
        assert_eq!(out.stderr.len(), 1000);
        assert!(out.truncated);

        let exact = run_ok(&CommandSpec::new("cat").stdin("12345").max_output(5)).await;
        assert_eq!(exact.stdout, "12345");
        assert!(!exact.truncated);

        let none = run_ok(&CommandSpec::new("cat").stdin("x").max_output(0)).await;
        assert_eq!(none.stdout, "");
        assert!(none.truncated);
    }

    #[tokio::test]
    async fn cap_never_splits_a_character() {
        let cat = |cap| CommandSpec::new("cat").stdin("ééé").max_output(cap);
        let out = run_ok(&cat(3)).await;
        assert_eq!((out.stdout.as_str(), out.truncated), ("é", true));
        let out = run_ok(&cat(4)).await;
        assert_eq!((out.stdout.as_str(), out.truncated), ("éé", true));
        let out = run_ok(&cat(6)).await;
        assert_eq!((out.stdout.as_str(), out.truncated), ("ééé", false));
    }

    #[tokio::test]
    async fn invalid_utf8_is_replaced_within_the_cap() {
        let out = run_ok(&sh(r"printf 'ok\377\n'")).await;
        assert_eq!(out.stdout, "ok\u{fffd}\n");
        assert!(!out.truncated);
        // Each replacement is 3 bytes for 1: the cap still holds.
        let out = run_ok(&sh(r"printf '\377\377\377\377'").max_output(4)).await;
        assert_eq!(out.stdout, "\u{fffd}");
        assert!(out.truncated);
    }

    #[tokio::test]
    async fn missing_program_is_a_spawn_error() {
        let err = run(
            &CommandSpec::new("hostctl-test-no-such-program"),
            Some(LONG),
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("not-executable");
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        let err = run(&CommandSpec::new(script.to_str().unwrap()), Some(LONG))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }

    #[tokio::test]
    async fn environment_is_inherited_extended_and_git_cant_prompt() {
        let spec = sh(r#"printf '%s|%s|%s' "$HOSTCTL_TEST_VAR" "$GIT_TERMINAL_PROMPT" "$PATH""#)
            .env("HOSTCTL_TEST_VAR", "a b'c")
            .env("GIT_TERMINAL_PROMPT", "1");
        let out = run_ok(&spec).await;
        let path = std::env::var("PATH").unwrap_or_default();
        assert_eq!(out.stdout, format!("a b'c|0|{path}"));
    }

    #[tokio::test]
    async fn child_has_no_controlling_terminal() {
        let out = run_ok(&sh("(: </dev/tty) 2>/dev/null && echo tty || echo none")).await;
        assert_eq!(out.stdout, "none\n");
    }

    #[tokio::test]
    async fn child_leads_its_own_session() {
        if !Path::new("/proc/self/stat").exists() {
            eprintln!("skipping: no /proc");
            return;
        }
        let out = run_ok(&CommandSpec::new("cat").arg("/proc/self/stat")).await;
        let (pid, rest) = out.stdout.split_once(" (").unwrap();
        let fields: Vec<&str> = rest.rsplit_once(") ").unwrap().1.split(' ').collect();
        // state ppid pgrp session tty_nr
        assert_eq!(fields[2], pid, "process group");
        assert_eq!(fields[3], pid, "session");
        assert_eq!(fields[4], "0", "controlling terminal");
    }

    /// Interactive runs change process-wide signal state; tests that
    /// inspect it must not overlap with each other.
    static SIGNAL_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn disposition(signal: c_int) -> libc::sighandler_t {
        // SAFETY: all-zero is a valid sigaction; a null `act` only queries.
        let mut current: libc::sigaction = unsafe { std::mem::zeroed() };
        unsafe { libc::sigaction(signal, std::ptr::null(), &mut current) };
        current.sa_sigaction
    }

    #[tokio::test]
    async fn interactive_returns_the_exit_status() {
        let _serial = SIGNAL_TESTS.lock().await;
        let status = run_interactive(&sh("exit 7")).await.unwrap();
        assert_eq!(status.code(), Some(7));
        let status = run_interactive(&sh("test \"$X\" = y").env("X", "y"))
            .await
            .unwrap();
        assert!(status.success());
    }

    #[tokio::test]
    async fn interactive_spawn_error_restores_dispositions() {
        let _serial = SIGNAL_TESTS.lock().await;
        let before = INTERRUPTS.map(disposition);
        let err = run_interactive(&CommandSpec::new("hostctl-test-no-such-program"))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert_eq!(INTERRUPTS.map(disposition), before);
    }

    #[tokio::test]
    async fn interactive_ignores_interrupts_only_in_the_parent() {
        let _serial = SIGNAL_TESTS.lock().await;
        if !Path::new("/proc/self/status").exists() {
            eprintln!("skipping: no /proc");
            return;
        }
        let before = INTERRUPTS.map(disposition);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("status");
        // `exec`: bash ignores SIGQUIT itself, but not in what it runs.
        let script = r#"exec grep -h '^SigIgn' /proc/self/status /proc/$PPID/status >"$1""#;
        let status = run_interactive(&sh(script).args(["sh", file.to_str().unwrap()]))
            .await
            .unwrap();
        assert!(status.success());
        let masks: Vec<u64> = std::fs::read_to_string(&file)
            .unwrap()
            .lines()
            .map(|l| u64::from_str_radix(l.split_whitespace().nth(1).unwrap(), 16).unwrap())
            .collect();
        let [child, parent] = masks[..] else {
            panic!("{masks:?}")
        };
        let bits = |signal: c_int| 1u64 << (signal - 1);
        for (signal, previous) in INTERRUPTS.into_iter().zip(before) {
            assert_ne!(parent & bits(signal), 0, "parent ignores signal {signal}");
            let inherited = previous == libc::SIG_IGN;
            assert_eq!(
                child & bits(signal) != 0,
                inherited,
                "child, signal {signal}"
            );
        }
        assert_eq!(INTERRUPTS.map(disposition), before);
    }

    #[tokio::test]
    async fn dropping_an_interactive_run_restores_dispositions() {
        let _serial = SIGNAL_TESTS.lock().await;
        let before = INTERRUPTS.map(disposition);
        let dir = tempfile::tempdir().unwrap();
        let hold = dir.path().join("hold");
        std::fs::write(&hold, "").unwrap();
        // Runs while `hold` exists (until `dir` goes), however slow the machine.
        let spec =
            sh(r#"while [ -e "$1" ]; do sleep 0.01; done"#).args(["sh", hold.to_str().unwrap()]);
        let mut running = Box::pin(run_interactive(&spec));
        tokio::select! {
            biased;
            status = &mut running => panic!("finished early: {status:?}"),
            () = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
        assert_eq!(INTERRUPTS.map(disposition), [libc::SIG_IGN; 2]);
        drop(running);
        assert_eq!(INTERRUPTS.map(disposition), before);
    }

    #[test]
    fn capture_strings() {
        let capture = |bytes: &[u8], cap, truncated| {
            Capture {
                kept: bytes.to_vec(),
                cap,
                truncated,
            }
            .into_string()
        };
        assert_eq!(capture(b"abc", 3, false), ("abc".into(), false));
        // Cut by the cap: the partial character goes.
        assert_eq!(
            capture("aé".as_bytes()[..2].as_ref(), 2, true),
            ("a".into(), true)
        );
        // Not cut: a dangling byte is the child's own and gets replaced.
        assert_eq!(capture(b"a\xc3", 10, false), ("a\u{fffd}".into(), false));
        assert_eq!(capture(b"\xff\xfe", 3, false), ("\u{fffd}".into(), true));
        assert_eq!(capture(b"", 0, false), (String::new(), false));
    }
}
