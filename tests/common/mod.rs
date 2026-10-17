//! The world hostctl's integration tests run the real binary in: a private
//! temporary directory with the fake `nix`, `ssh` and `nix-store` of
//! `tests/support`, their scenario files, a config file, and an environment
//! of its own (nothing from the caller's but `PATH`), so tests never touch
//! `~/.config`, `~/.cache`, the network or a real ssh.
//!
//! Layout of a world (`$HOSTCTL_FAKE` is `fake/`; see the fakes for the
//! scenario files they read):
//!
//! ```text
//! home/  tmp/  xdg/{config/hostctl/config.toml,cache}  r/ (XDG_RUNTIME_DIR)
//! fake/bin/{nix,ssh,nix-store}   the fakes, first on PATH
//! fake/flakes/ evals/ builds/ hosts.json   what `nix` answers
//! fake/closures/                 this machine's store, for `nix-store`
//! fake/hosts/<host>/             a host `ssh` reaches (or this machine)
//! fake/log                       every nix and ssh call
//! ```

#![allow(dead_code)] // Each test binary uses its own part of this.

use std::cell::RefCell;
use std::ffi::{CString, OsString};
use std::fs;
use std::io::{Read as _, Write as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::symlink;
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The flake every world compares against unless a test says otherwise.
pub const FLAKE: &str = "github:o/r";
/// Revisions (40 hex digits) for flakes to be at.
pub const REV1: &str = "1111111111111111111111111111111111111111";
pub const REV2: &str = "2222222222222222222222222222222222222222";

/// How long a run may take before the test gives up on it.
const RUN_TIMEOUT: Duration = Duration::from_secs(60);

/// `/nix/store/<c × 32>-<name>`: a store path with a recognizable hash.
pub fn store(c: char, name: &str) -> String {
    format!("/nix/store/{}-{name}", c.to_string().repeat(32))
}

/// The out path of home-manager configuration `c` (`aaaa…-home-manager-generation`).
pub fn hm(c: char) -> String {
    store(c, "home-manager-generation")
}

/// The attribute hostctl evaluates for home-manager host `name`.
pub fn hm_attr(name: &str) -> String {
    format!("homeConfigurations.{name}.activationPackage")
}

/// Unix time now.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Give the symlink `path` itself the modification time `unix` (what the
/// probe reports as the activation time).
fn set_link_mtime(path: &Path, unix: i64) {
    let c = CString::new(path.as_os_str().as_bytes()).unwrap();
    let time = libc::timespec {
        tv_sec: unix as libc::time_t,
        tv_nsec: 0,
    };
    let times = [time, time];
    // SAFETY: a valid C string and two timespecs.
    let rc = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            c.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    assert_eq!(rc, 0, "utimensat {}", path.display());
}

/// The fakes' file names for a flake or attribute.
fn key(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) {
    let path = path.as_ref();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
}

/// Write the executable `path` (parents created) from a child process. Were
/// the write fd ever open in this (multi-threaded) test process, a fork in
/// another test could inherit it, and running the file would fail with
/// ETXTBSY until that fork execs.
pub fn write_executable(path: &Path, contents: impl AsRef<[u8]>) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut child = Command::new("sh")
        .args(["-c", "cat >\"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(contents.as_ref())
        .unwrap();
    assert!(
        child.wait().unwrap().success(),
        "writing {}",
        path.display()
    );
}

/// A symlink at `link` (parents created) pointing at `target`.
fn link(target: impl AsRef<Path>, link: impl AsRef<Path>) {
    let link = link.as_ref();
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    let _ = fs::remove_file(link);
    symlink(target, link).unwrap_or_else(|e| panic!("linking {}: {e}", link.display()));
}

/// A private directory: under `/tmp` when possible, which keeps control
/// socket paths short enough for macOS (`$TMPDIR` is long there), else
/// under `$TMPDIR`.
pub fn tempdir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("h")
        .tempdir_in("/tmp")
        .or_else(|_| tempfile::Builder::new().prefix("h").tempdir())
        .expect("a temporary directory")
}

/// What a run of hostctl did.
#[derive(Debug)]
pub struct Run {
    /// The exit code; `-<signal>` when killed by one.
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    /// stdout parsed as JSON.
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|e| panic!("stdout isn't JSON ({e}):\n{self}"))
    }

    /// Panics (showing everything) unless the exit code is `code`.
    #[track_caller]
    pub fn code(&self, code: i32) -> &Self {
        assert_eq!(self.code, code, "exit code\n{self}");
        self
    }
}

impl std::fmt::Display for Run {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "--- exit {}\n--- stdout\n{}--- stderr\n{}---",
            self.code, self.stdout, self.stderr
        )
    }
}

/// Wait for `child` (at most [`RUN_TIMEOUT`]; then its process group is
/// killed and the test fails), collecting its output.
pub fn finish(mut child: Child) -> Run {
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let stdout = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        s
    });
    let stderr = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    let deadline = Instant::now() + RUN_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            // SAFETY: a plain syscall; the child leads its own group.
            unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
            let _ = child.wait();
            panic!(
                "hostctl ran for more than {RUN_TIMEOUT:?}\n--- stdout\n{}\n--- stderr\n{}",
                stdout.join().unwrap(),
                stderr.join().unwrap()
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    Run {
        code: status
            .code()
            .unwrap_or_else(|| -status.signal().unwrap_or(0)),
        stdout: stdout.join().unwrap(),
        stderr: stderr.join().unwrap(),
    }
}

/// `program` with a clean environment (only `PATH` is inherited), no stdin,
/// piped output and no controlling terminal (its own session), so nothing
/// can prompt on the terminal running the tests.
pub fn command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut cmd = Command::new(program);
    cmd.env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: setsid(2) is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd
}

/// A pseudo-terminal to run hostctl on the way a user does ([`Pty::spawn`]):
/// everything it writes is collected from the master side, keys are typed
/// into it, and the terminal's settings can be read when it is done.
pub struct Pty {
    master: fs::File,
    slave: fs::File,
    output: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
}

impl Pty {
    /// A terminal of `cols` × `rows`.
    pub fn new(cols: u16, rows: u16) -> Pty {
        use std::os::fd::FromRawFd as _;
        // ptsname(3) returns a static buffer.
        static PTSNAME: std::sync::Mutex<()> = std::sync::Mutex::new(());
        // SAFETY: plain calls; the descriptors are checked and then owned.
        let (master, slave) = unsafe {
            let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
            assert!(
                master >= 0,
                "posix_openpt: {}",
                std::io::Error::last_os_error()
            );
            assert_eq!(libc::grantpt(master), 0);
            assert_eq!(libc::unlockpt(master), 0);
            let name = {
                let _lock = PTSNAME.lock().unwrap();
                let name = libc::ptsname(master);
                assert!(!name.is_null());
                std::ffi::CStr::from_ptr(name).to_owned()
            };
            let slave = libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY);
            assert!(
                slave >= 0,
                "opening the pty: {}",
                std::io::Error::last_os_error()
            );
            let size = libc::winsize {
                ws_row: rows,
                ws_col: cols,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            assert_eq!(libc::ioctl(master, libc::TIOCSWINSZ, &size), 0);
            (fs::File::from_raw_fd(master), fs::File::from_raw_fd(slave))
        };
        let output = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut reader = master.try_clone().unwrap();
        let collected = std::sync::Arc::clone(&output);
        // Until the slave side is gone (EIO) or the test is over.
        std::thread::spawn(move || {
            let mut buf = [0; 4096];
            while let Ok(n @ 1..) = reader.read(&mut buf) {
                collected.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        Pty {
            master,
            slave,
            output,
        }
    }

    /// Start `cmd` (a [`World::command`], which makes it a session leader)
    /// with this terminal as its controlling terminal, stdin, stdout and
    /// stderr.
    pub fn spawn(&self, mut cmd: Command) -> Child {
        let stdio = || Stdio::from(self.slave.try_clone().unwrap());
        cmd.stdin(stdio()).stdout(stdio()).stderr(stdio());
        // SAFETY: ioctl(2) is async-signal-safe; it runs after setsid.
        unsafe {
            cmd.pre_exec(|| {
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        cmd.spawn().expect("hostctl runs")
    }

    /// Type `keys`.
    pub fn send(&self, keys: &[u8]) {
        use std::io::Write as _;
        (&self.master).write_all(keys).unwrap();
    }

    /// Everything written so far, escape sequences and all.
    pub fn output(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }

    /// Wait (20 s at most) until `text` has been written.
    #[track_caller]
    pub fn wait_for(&self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !self.output().contains(text) {
            assert!(
                Instant::now() < deadline,
                "{text:?} never showed up in:\n{:?}",
                self.output()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// What the terminal shows now ([`Screen`]), if it is `cols` × `rows`.
    pub fn screen(&self, cols: usize, rows: usize) -> Screen {
        let mut screen = Screen::new(cols, rows);
        screen.feed(&self.output());
        screen
    }

    /// Wait (20 s at most) until a `cols` × `rows` terminal shows `text`
    /// (on one row).
    #[track_caller]
    pub fn wait_for_screen(&self, cols: usize, rows: usize, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !self.screen(cols, rows).contains(text) {
            assert!(
                Instant::now() < deadline,
                "{text:?} never showed up on the screen:\n{}",
                self.screen(cols, rows).rows().join("\n")
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The terminal's settings now.
    pub fn termios(&self) -> libc::termios {
        use std::os::fd::AsRawFd as _;
        // SAFETY: all-zero is a valid termios, which tcgetattr fills in.
        let mut termios: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: a valid descriptor and a pointer to a live termios.
        assert_eq!(
            unsafe { libc::tcgetattr(self.slave.as_raw_fd(), &mut termios) },
            0
        );
        termios
    }
}

/// Just enough of a terminal to see what the dashboard drew: text (every
/// character one column), `\r`, `\n`, cursor positioning and moves, erasing
/// (`CSI J`, `CSI K`); colors and modes are ignored, OSC strings skipped,
/// and entering or leaving the alternate screen clears it.
pub struct Screen {
    grid: Vec<Vec<char>>,
    row: usize,
    col: usize,
}

impl Screen {
    pub fn new(cols: usize, rows: usize) -> Screen {
        Screen {
            grid: vec![vec![' '; cols]; rows],
            row: 0,
            col: 0,
        }
    }

    fn clear(&mut self, rows: std::ops::Range<usize>) {
        for row in rows {
            self.grid[row].fill(' ');
        }
    }

    pub fn feed(&mut self, text: &str) {
        let (rows, cols) = (self.grid.len(), self.grid[0].len());
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\r' => self.col = 0,
                '\n' => {
                    if self.row + 1 == rows {
                        self.grid.remove(0);
                        self.grid.push(vec![' '; cols]);
                    } else {
                        self.row += 1;
                    }
                }
                '\x1b' => match chars.next() {
                    Some('[') => {
                        let mut params = String::new();
                        let end = loop {
                            match chars.next() {
                                Some(c) if ('@'..='~').contains(&c) => break c,
                                Some(c) => params.push(c),
                                None => return,
                            }
                        };
                        let private = params.starts_with('?');
                        let numbers: Vec<usize> = params
                            .trim_start_matches('?')
                            .split(';')
                            .map(|n| n.parse().unwrap_or(0))
                            .collect();
                        let n = numbers[0].max(1);
                        match end {
                            'H' | 'f' => {
                                self.row = (n - 1).min(rows - 1);
                                self.col =
                                    (numbers.get(1).copied().unwrap_or(1).max(1) - 1).min(cols - 1);
                            }
                            'A' => self.row = self.row.saturating_sub(n),
                            'B' => self.row = (self.row + n).min(rows - 1),
                            'C' => self.col = (self.col + n).min(cols - 1),
                            'D' => self.col = self.col.saturating_sub(n),
                            'J' if numbers[0] == 2 => self.clear(0..rows),
                            'J' => {
                                self.grid[self.row][self.col..].fill(' ');
                                self.clear(self.row + 1..rows);
                            }
                            'K' => self.grid[self.row][self.col..].fill(' '),
                            'h' | 'l' if private && numbers[0] == 1049 => self.clear(0..rows),
                            _ => {}
                        }
                    }
                    // OSC: up to BEL or ST.
                    Some(']') => {
                        while let Some(c) = chars.next() {
                            if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                                break;
                            }
                        }
                    }
                    _ => {}
                },
                c if c.is_control() => {}
                c => {
                    if self.col < cols {
                        self.grid[self.row][self.col] = c;
                        self.col += 1;
                    }
                }
            }
        }
    }

    /// The rows, without trailing blanks.
    pub fn rows(&self) -> Vec<String> {
        self.grid
            .iter()
            .map(|row| row.iter().collect::<String>().trim_end().to_string())
            .collect()
    }

    /// Whether a row shows `text`.
    pub fn contains(&self, text: &str) -> bool {
        self.rows().iter().any(|row| row.contains(text))
    }
}

/// Wait (at most [`RUN_TIMEOUT`]) for a child started on a [`Pty`]; its
/// exit code, `-<signal>` when killed by one.
pub fn wait(mut child: Child) -> i32 {
    let deadline = Instant::now() + RUN_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status
                .code()
                .unwrap_or_else(|| -status.signal().unwrap_or(0));
        }
        if Instant::now() > deadline {
            // SAFETY: a plain syscall; the child leads its own group.
            unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
            let _ = child.wait();
            panic!("hostctl ran for more than {RUN_TIMEOUT:?}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// One call of a fake, from its `start`/`end` lines in the log.
#[derive(Debug, Clone)]
pub struct Call {
    /// `nix` or `ssh`.
    pub tool: String,
    pub args: String,
    /// Line numbers of its `start` and `end` (none if it was killed).
    pub start: usize,
    pub end: Option<usize>,
    pub code: Option<i32>,
}

impl Call {
    /// The command after ssh's `-- <destination>`.
    pub fn remote_command(&self) -> &str {
        let after = self.args.split_once(" -- ").map_or("", |(_, rest)| rest);
        after.split_once(' ').map_or("", |(_, command)| command)
    }

    /// ssh's destination (after `--`).
    pub fn destination(&self) -> &str {
        let after = self.args.split_once(" -- ").map_or("", |(_, rest)| rest);
        after.split(' ').next().unwrap_or("")
    }
}

/// Whether every call in `calls` started before any of them ended.
pub fn overlapping(calls: &[Call]) -> bool {
    let last_start = calls.iter().map(|c| c.start).max();
    let first_end = calls.iter().map(|c| c.end.unwrap_or(usize::MAX)).min();
    calls.len() > 1 && last_start < first_end
}

/// Whether each of `calls` ended before the next one started.
pub fn sequential(calls: &[Call]) -> bool {
    let mut calls = calls.to_vec();
    calls.sort_by_key(|c| c.start);
    calls
        .windows(2)
        .all(|w| w[0].end.is_some_and(|end| end < w[1].start))
}

/// A world for one test.
pub struct World {
    dir: tempfile::TempDir,
    /// `$HOSTCTL_FAKE`.
    pub fake: PathBuf,
    /// The config file hostctl finds (`$XDG_CONFIG_HOME/hostctl/config.toml`).
    pub config_file: PathBuf,
    /// Appended to the base config.
    config: RefCell<String>,
    /// The host that is this machine: its root is `HOSTCTL_PROBE_ROOT` and
    /// its home `HOME`.
    local: RefCell<Option<String>>,
    env: RefCell<Vec<(OsString, Option<OsString>)>>,
    /// When every fake generation was activated (unix seconds): three days
    /// and an hour ago.
    pub activated: i64,
}

impl Default for World {
    fn default() -> Self {
        World::new()
    }
}

impl World {
    /// A world whose config (in the XDG config directory) points at the
    /// fakes, compares against [`FLAKE`] and allows 8 evaluations at once.
    /// Nothing exists in it yet: no flake revision, no hosts.
    pub fn new() -> World {
        let dir = tempdir();
        let root = dir.path();
        let fake = root.join("fake");
        for sub in ["home", "tmp", "r", "xdg/cache", "xdg/config/hostctl"] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        for sub in ["flakes", "evals", "builds", "hosts", "closures"] {
            fs::create_dir_all(fake.join(sub)).unwrap();
        }
        write(fake.join("log"), "");
        // Discovery finds nothing unless a test says otherwise.
        write(
            fake.join("hosts.json"),
            r#"{"nixos":[],"darwin":[],"home":[],"inventory":null}"#,
        );
        let support = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support");
        for (from, to) in [
            ("fake-nix", "nix"),
            ("fake-ssh", "ssh"),
            ("fake-nix-store", "nix-store"),
        ] {
            let bin = fake.join("bin").join(to);
            write_executable(&bin, fs::read(support.join(from)).unwrap());
        }
        let world = World {
            config_file: root.join("xdg/config/hostctl/config.toml"),
            dir,
            fake,
            config: RefCell::new(String::new()),
            local: RefCell::new(None),
            env: RefCell::new(Vec::new()),
            activated: now() - 3 * 86400 - 3600,
        };
        world.write_config();
        world
    }

    /// The world's directory.
    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    /// The base config: the fakes, [`FLAKE`], 8 jobs. Dotted keys only, so
    /// whatever a test appends (tables included) still parses.
    pub fn base_config(&self) -> String {
        let bin = self.fake.join("bin");
        format!(
            "flake = {FLAKE:?}\njobs = 8\nnix.command = {:?}\nssh.command = {:?}\n",
            bin.join("nix").display().to_string(),
            bin.join("ssh").display().to_string(),
        )
    }

    /// Append `toml` to the config file (top-level keys must come before
    /// any table the test adds, as usual; `nix.*`/`ssh.*` are dotted keys).
    pub fn config(&self, toml: &str) -> &Self {
        {
            let mut config = self.config.borrow_mut();
            config.push_str(toml);
            if !toml.ends_with('\n') {
                config.push('\n');
            }
        }
        self.write_config();
        self
    }

    /// Replace the config file with exactly `toml` (no base config).
    pub fn raw_config(&self, toml: &str) {
        write(&self.config_file, toml);
    }

    fn write_config(&self) {
        write(
            &self.config_file,
            format!("{}{}", self.base_config(), self.config.borrow()),
        );
    }

    /// Set (or with `None`, remove) an environment variable for the runs.
    pub fn env(&self, key: &str, value: Option<&str>) -> &Self {
        self.env
            .borrow_mut()
            .push((key.into(), value.map(OsString::from)));
        self
    }

    // ─── nix ─────────────────────────────────────────────────────────

    /// `nix flake metadata` of `flake` says it is a github flake at `rev`,
    /// last modified two hours ago.
    pub fn flake_at(&self, flake: &str, rev: &str) -> Answer {
        let path = flake.strip_prefix("github:").expect("a github: flake");
        let (owner, repo) = path.split_once('/').unwrap();
        let modified = now() - 2 * 3600 - 60;
        let nar = format!("sha256-{}", &rev[..8]);
        let json = serde_json::json!({
            "description": "a test flake",
            "fingerprint": format!("fingerprint-{rev}"),
            "lastModified": modified,
            "locked": {
                "lastModified": modified,
                "narHash": nar,
                "owner": owner,
                "repo": repo,
                "rev": rev,
                "type": "github",
            },
            "originalUrl": flake,
            "revision": rev,
            "url": World::locked(flake, rev),
        });
        let file = self
            .fake
            .join("flakes")
            .join(key(flake))
            .join("metadata.json");
        write(&file, json.to_string());
        Answer(file)
    }

    /// [`World::flake_at`] for [`FLAKE`].
    pub fn flake(&self, rev: &str) -> Answer {
        self.flake_at(FLAKE, rev)
    }

    /// The locked URL [`World::flake_at`] gives `flake` at `rev`.
    pub fn locked(flake: &str, rev: &str) -> String {
        format!("{flake}/{rev}?narHash=sha256-{}%3D", &rev[..8])
    }

    /// `attr` evaluates to `out`.
    pub fn eval(&self, attr: &str, out: &str) -> Answer {
        let file = self.fake.join("evals").join(key(attr));
        write(&file, format!("{out}\n"));
        Answer(file)
    }

    /// Evaluating `attr` fails with `stderr`.
    pub fn eval_fails(&self, attr: &str, stderr: &str) -> Answer {
        let answer = self.eval(attr, "");
        fs::remove_file(&answer.0).unwrap();
        answer.stderr(stderr).exit(1);
        answer
    }

    /// Discovery (`nix eval --json … --apply`) answers `json`.
    pub fn discover(&self, json: serde_json::Value) -> Answer {
        let file = self.fake.join("hosts.json");
        write(&file, json.to_string());
        Answer(file)
    }

    /// `nix build` of `attr` realises `closure` (its out path first) on the
    /// host it runs on.
    pub fn build(&self, attr: &str, closure: &[(&str, u64)]) {
        write(
            self.fake.join("builds").join(key(attr)),
            closure_file(closure),
        );
    }

    /// This machine's store has `closure`, the closure of its first path.
    pub fn local_closure(&self, closure: &[(&str, u64)]) {
        write(
            self.fake.join("closures").join(base_name(closure[0].0)),
            closure_file(closure),
        );
    }

    // ─── hosts ───────────────────────────────────────────────────────

    /// Host `name`'s fake (created if needed).
    pub fn host(&self, name: &str) -> FakeHost {
        let dir = self.fake.join("hosts").join(name);
        fs::create_dir_all(dir.join("root/home")).unwrap();
        FakeHost {
            dir,
            activated: self.activated,
        }
    }

    /// Host `name` is this machine: runs get its root as
    /// `HOSTCTL_PROBE_ROOT` and its home as `HOME` (whether hostctl takes it
    /// for this machine is up to the config, `self` or `--self`).
    pub fn this_machine(&self, name: &str) -> FakeHost {
        *self.local.borrow_mut() = Some(name.to_string());
        self.host(name)
    }

    // ─── running ─────────────────────────────────────────────────────

    /// hostctl with `args`, in this world's environment (see [`command`]):
    /// `PATH` starts with the fakes; `HOME`, `TMPDIR` and the XDG
    /// directories are the world's (`XDG_STATE_HOME` unset); `NO_COLOR`
    /// and `USER=tester` are set.
    pub fn command(&self, args: &[&str]) -> Command {
        let root = self.root();
        let mut cmd = command(env!("CARGO_BIN_EXE_hostctl"));
        let mut path = OsString::from(self.fake.join("bin"));
        path.push(":");
        path.push(std::env::var_os("PATH").unwrap_or_default());
        cmd.args(args)
            .current_dir(root)
            .env("PATH", path)
            .env("HOME", root.join("home"))
            .env("TMPDIR", root.join("tmp"))
            .env("XDG_CONFIG_HOME", root.join("xdg/config"))
            .env("XDG_CACHE_HOME", root.join("xdg/cache"))
            .env("XDG_RUNTIME_DIR", root.join("r"))
            .env("HOSTCTL_FAKE", &self.fake)
            .env("NO_COLOR", "1")
            .env("USER", "tester");
        if let Some(name) = self.local.borrow().as_deref() {
            let host = self.host(name);
            cmd.env("HOSTCTL_PROBE_ROOT", host.root())
                .env("HOME", host.root().join("home"));
        }
        for (key, value) in self.env.borrow().iter() {
            match value {
                Some(value) => cmd.env(key, value),
                None => cmd.env_remove(key),
            };
        }
        cmd
    }

    /// Run hostctl with `args` to completion.
    pub fn run(&self, args: &[&str]) -> Run {
        finish(self.command(args).spawn().expect("hostctl runs"))
    }

    // ─── what happened ───────────────────────────────────────────────

    /// The fakes' log.
    pub fn log(&self) -> String {
        fs::read_to_string(self.fake.join("log")).unwrap()
    }

    /// Forget the calls so far.
    pub fn clear_log(&self) {
        write(self.fake.join("log"), "");
    }

    /// Every call of the fakes, in the order they started.
    pub fn calls(&self) -> Vec<Call> {
        let mut calls: Vec<Call> = Vec::new();
        let mut open: Vec<(String, usize)> = Vec::new();
        for (n, line) in self.log().lines().enumerate() {
            let mut words = line.splitn(4, ' ');
            match (words.next(), words.next(), words.next()) {
                (Some("start"), Some(pid), Some(tool)) => {
                    open.retain(|(p, _)| p != pid);
                    open.push((pid.to_string(), calls.len()));
                    calls.push(Call {
                        tool: tool.to_string(),
                        args: words.next().unwrap_or("").to_string(),
                        start: n,
                        end: None,
                        code: None,
                    });
                }
                (Some("end"), Some(pid), Some(code)) => {
                    let at = open.iter().position(|(p, _)| p == pid);
                    let (_, i) = open.remove(at.unwrap_or_else(|| panic!("unmatched {line:?}")));
                    calls[i].end = Some(n);
                    calls[i].code = code.parse().ok();
                }
                _ => panic!("unexpected log line {line:?}"),
            }
        }
        calls
    }

    /// The calls of `nix` whose arguments contain `pattern` (e.g. `eval --raw`).
    pub fn nix_calls(&self, pattern: &str) -> Vec<Call> {
        self.calls()
            .into_iter()
            .filter(|c| c.tool == "nix" && c.args.contains(pattern))
            .collect()
    }

    /// The evaluations (`nix eval --raw`).
    pub fn evals(&self) -> Vec<Call> {
        self.nix_calls("eval --raw")
    }

    /// The `nix flake metadata` calls.
    pub fn resolutions(&self) -> Vec<Call> {
        self.nix_calls("flake metadata")
    }

    /// The ssh calls to `destination`.
    pub fn ssh_calls(&self, destination: &str) -> Vec<Call> {
        self.calls()
            .into_iter()
            .filter(|c| c.tool == "ssh" && c.destination() == destination)
            .collect()
    }

    /// The cache file, parsed (`null` if there is none).
    pub fn cache(&self) -> serde_json::Value {
        match fs::read_to_string(self.root().join("xdg/cache/hostctl/cache.json")) {
            Ok(text) => serde_json::from_str(&text).unwrap(),
            Err(_) => serde_json::Value::Null,
        }
    }
}

/// A scenario answer file of the fake nix, to which these add siblings.
pub struct Answer(pub PathBuf);

impl Answer {
    fn sibling(&self, ext: &str, contents: impl AsRef<[u8]>) -> &Self {
        let mut name = self.0.file_name().unwrap().to_os_string();
        name.push(ext);
        write(self.0.with_file_name(name), contents);
        self
    }

    /// Wait this many seconds before answering.
    pub fn delay(&self, secs: f64) -> &Self {
        self.sibling(".delay", format!("{secs}\n"))
    }

    /// Wait (10 s at most) until `n` evaluations have started, or for a
    /// flake's metadata, until `n` ssh calls have: only concurrent callers
    /// get past this quickly.
    pub fn rendezvous(&self, n: usize) -> &Self {
        self.sibling(".await", format!("{n}\n"))
    }

    pub fn stderr(&self, text: &str) -> &Self {
        self.sibling(".stderr", text)
    }

    pub fn exit(&self, code: i32) -> &Self {
        self.sibling(".exit", format!("{code}\n"))
    }

    /// Evaluations only: `--read-only` fails like import-from-derivation.
    pub fn read_only_fails(&self) -> &Self {
        self.sibling(".readonly-fails", "")
    }

    /// The call (and what it runs) ignores SIGTERM: only SIGKILL stops it.
    pub fn ignore_sigterm(&self) -> &Self {
        self.sibling(".ignore-sigterm", "")
    }

    /// Whether a call with [`Answer::ignore_sigterm`] got as far as
    /// ignoring it.
    pub fn ignoring_sigterm(&self) -> bool {
        let mut name = self.0.file_name().unwrap().to_os_string();
        name.push(".ignoring-sigterm");
        self.0.with_file_name(name).exists()
    }
}

/// A fake host (see tests/support/fake-ssh).
pub struct FakeHost {
    pub dir: PathBuf,
    activated: i64,
}

impl FakeHost {
    /// Its file system, as the probe sees it.
    pub fn root(&self) -> PathBuf {
        self.dir.join("root")
    }

    /// `path` (absolute) inside its root.
    pub fn at(&self, path: &str) -> PathBuf {
        self.root().join(path.trim_start_matches('/'))
    }

    /// `out` exists in its store.
    pub fn has(&self, out: &str) -> &Self {
        fs::create_dir_all(self.at(out)).unwrap();
        self
    }

    /// Where profile `profile` (absolute, or `~/…`) is, and the name of its
    /// generation `generation`'s link next to it.
    fn profile_link(&self, profile: &str, generation: u64) -> (PathBuf, String) {
        let path = match profile.strip_prefix("~/") {
            Some(rest) => self.root().join("home").join(rest),
            None => self.at(profile),
        };
        let name = path.file_name().unwrap().to_str().unwrap();
        let generation = format!("{name}-{generation}-link");
        (path, generation)
    }

    /// `profile` (absolute, or relative to its home) points at generation
    /// `generation` of it, which is `out`.
    pub fn profile(&self, profile: &str, generation: u64, out: &str) -> &Self {
        let (path, generation) = self.profile_link(profile, generation);
        link(&generation, &path);
        set_link_mtime(&path, self.activated);
        link(out, path.with_file_name(generation));
        self.has(out)
    }

    /// Generation `generation` of `profile` (as for [`FakeHost::profile`])
    /// is `out`, without the profile pointing at it.
    pub fn generation(&self, profile: &str, generation: u64, out: &str) -> &Self {
        let (path, generation) = self.profile_link(profile, generation);
        link(out, path.with_file_name(generation));
        self.has(out)
    }

    /// Its home-manager profile is at generation `generation`, `out`.
    pub fn home_manager(&self, generation: u64, out: &str) -> &Self {
        self.profile("~/.local/state/nix/profiles/home-manager", generation, out)
    }

    /// NixOS/darwin: it runs `current`, and its system profile is at
    /// generation `generation`, `profile`.
    pub fn system(&self, current: &str, generation: u64, profile: &str) -> &Self {
        link(current, self.at("/run/current-system"));
        set_link_mtime(&self.at("/run/current-system"), self.activated);
        self.has(current);
        self.profile("/nix/var/nix/profiles/system", generation, profile)
    }

    /// Generation `out` records the flake revision `rev`.
    pub fn revision(&self, out: &str, rev: &str) -> &Self {
        write(
            self.at(out).join("configuration-revision"),
            format!("{rev}\n"),
        );
        self
    }

    /// Command `name` on its PATH is the shell script `script`.
    pub fn bin(&self, name: &str, script: &str) -> &Self {
        let bin = self.dir.join("bin").join(name);
        write_executable(&bin, script);
        self
    }

    /// ssh fails with `stderr` (exit 255).
    pub fn unreachable(&self, stderr: &str) -> &Self {
        write(self.dir.join("unreachable"), format!("{stderr}\n"));
        self
    }

    /// An interactive run (`ssh -t … sh <file>`) fails to connect with
    /// `stderr` (exit 255); unattended ones work.
    pub fn interactive_unreachable(&self, stderr: &str) -> &Self {
        write(
            self.dir.join("interactive-unreachable"),
            format!("{stderr}\n"),
        );
        self
    }

    /// Files left in its temporary directory.
    pub fn temporary_files(&self) -> Vec<String> {
        match fs::read_dir(self.at("/tmp")) {
            Ok(dir) => dir
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// While multiplexed, ssh exits 255 without a word.
    pub fn silent_255(&self) -> &Self {
        write(self.dir.join("silent-255"), "");
        self
    }

    /// ssh waits this many seconds before running anything.
    pub fn delay(&self, secs: f64) -> &Self {
        write(self.dir.join("delay"), format!("{secs}\n"));
        self
    }

    /// ssh waits until `n` ssh calls have started.
    pub fn rendezvous(&self, n: usize) -> &Self {
        write(self.dir.join("await"), format!("{n}\n"));
        self
    }

    /// A forced-command route: anything but `sh` gets `answer`.
    pub fn forced(&self, answer: &str) -> &Self {
        write(self.dir.join("forced"), answer);
        self
    }

    /// Its store has `closure`, the closure of its first path, and the fake
    /// `nix-store` answers for it.
    pub fn closure(&self, closure: &[(&str, u64)]) -> &Self {
        write(
            self.dir.join("closures").join(base_name(closure[0].0)),
            closure_file(closure),
        );
        let bin = self.dir.join("bin/nix-store");
        if !bin.exists() {
            let support = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support");
            write_executable(&bin, fs::read(support.join("fake-nix-store")).unwrap());
        }
        self
    }

    /// The last script an interactive ssh run (`sh <file>`) ran.
    pub fn interactive_script(&self) -> Option<String> {
        fs::read_to_string(self.dir.join("interactive.sh")).ok()
    }
}

fn base_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap()
}

fn closure_file(closure: &[(&str, u64)]) -> String {
    closure
        .iter()
        .map(|(path, size)| format!("{path}\t{size}\n"))
        .collect()
}

/// Whether `s` is how hostctl prints a run's duration (`18ms`, `1.2s`,
/// `12s`, `1m05s`).
fn is_duration(s: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if let Some(ms) = s.strip_suffix("ms") {
        return digits(ms);
    }
    let Some(secs) = s.strip_suffix('s') else {
        return false;
    };
    match secs.split_once(['.', 'm']) {
        Some((a, b)) => digits(a) && digits(b),
        None => digits(secs),
    }
}

/// `text` with the duration that ends a status table's footer replaced by
/// `<took>`, the only part of a table that changes from run to run.
pub fn masked(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let (body, newline) = match line.strip_suffix('\n') {
            Some(body) => (body, "\n"),
            None => (line, ""),
        };
        match body.rsplit_once(" · ") {
            Some((rest, last)) if is_duration(last) => {
                out.push_str(rest);
                out.push_str(" · <took>");
            }
            _ if is_duration(body) => out.push_str("<took>"),
            _ => out.push_str(body),
        }
        out.push_str(newline);
    }
    out
}
