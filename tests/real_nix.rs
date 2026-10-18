//! hostctl end to end against a real `nix`: only with `HOSTCTL_TEST_NIX=1`
//! (on in the Nix checks). `HOSTCTL_TEST_NIX_STORE` (e.g.
//! `local?root=$TMPDIR/nix-store`, a daemon-less chroot store) is passed to
//! every nix invocation as `--store`; without it, the default store is used.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use common::{Run, command, finish, tempdir, write_executable};

fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

/// Where a `--store` value keeps `/nix/store`: an absolute path is a chroot
/// store's root, else its `root=` parameter; `None` for the default layout.
fn store_root(store: &str) -> Option<PathBuf> {
    if store.starts_with('/') {
        return Some(store.into());
    }
    let (_, query) = store.split_once('?')?;
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix("root="))
        .map(PathBuf::from)
}

/// `program args…` (with `--store` when testing against a private store),
/// which must succeed; its stdout, trimmed.
fn nix(program: &Path, store: Option<&str>, home: &Path, args: &[&str]) -> String {
    let mut cmd = Command::new(program);
    if let Some(store) = store {
        cmd.args(["--store", store]);
    }
    let out = cmd
        .args(args)
        .env("HOME", home)
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{program:?} {args:?}: {stderr}");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir)
        .env("GIT_AUTHOR_NAME", "hostctl")
        .env("GIT_AUTHOR_EMAIL", "hostctl@localhost")
        .env("GIT_COMMITTER_NAME", "hostctl")
        .env("GIT_COMMITTER_EMAIL", "hostctl@localhost")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// Every file under `dir` but `.git` (whose index Nix's libgit2 may
/// legitimately refresh), with its contents.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for entry in fs::read_dir(&d).unwrap() {
            let path = entry.unwrap().path();
            if path.file_name() == Some(".git".as_ref()) {
                continue;
            }
            if path.is_dir() {
                todo.push(path);
            } else {
                files.insert(path.clone(), fs::read(&path).unwrap());
            }
        }
    }
    files
}

/// Point the profile symlink `profile` at its generation `n`, which is
/// `out` (what `nix-env --set` leaves behind).
fn set_profile(profile: &Path, n: u64, out: &str) {
    let name = profile.file_name().unwrap().to_str().unwrap();
    let generation = profile.with_file_name(format!("{name}-{n}-link"));
    let _ = fs::remove_file(&generation);
    symlink(out, &generation).unwrap();
    let _ = fs::remove_file(profile);
    symlink(generation.file_name().unwrap(), profile).unwrap();
}

#[test]
fn a_real_flake_against_a_real_profile() {
    if std::env::var("HOSTCTL_TEST_NIX").as_deref() != Ok("1") {
        eprintln!("skipped: set HOSTCTL_TEST_NIX=1 to run hostctl against a real nix");
        return;
    }
    let real_nix = which("nix").expect("HOSTCTL_TEST_NIX=1 needs nix on PATH");
    let nix_store = which("nix-store").expect("HOSTCTL_TEST_NIX=1 needs nix-store on PATH");
    assert!(
        which("git").is_some(),
        "HOSTCTL_TEST_NIX=1 needs git on PATH"
    );
    let store = std::env::var("HOSTCTL_TEST_NIX_STORE")
        .ok()
        .filter(|s| !s.is_empty());
    let dir = tempdir();
    let root = dir.path();
    let home = root.join("home");
    fs::create_dir_all(&home).unwrap();

    // Two "generations": plain files added to the store, nothing to build.
    let generations: Vec<String> = (1..=2)
        .map(|n| {
            let file = root.join(format!("hostctl-e2e-generation-{n}"));
            fs::write(&file, format!("generation {n}\n")).unwrap();
            nix(
                &nix_store,
                store.as_deref(),
                &home,
                &["--add", file.to_str().unwrap()],
            )
        })
        .collect();

    // A git flake without inputs whose `generation` is the first one.
    let flake = root.join("flake");
    fs::create_dir(&flake).unwrap();
    fs::write(
        flake.join("flake.nix"),
        format!(
            "{{\n  description = \"hostctl's end-to-end test\";\n  \
             outputs = _: {{ generation.outPath = \"{}\"; }};\n}}\n",
            generations[0]
        ),
    )
    .unwrap();
    git(&flake, &["init", "-q"]);
    git(&flake, &["add", "flake.nix"]);
    git(&flake, &["commit", "-q", "-m", "init"]);
    let untouched = snapshot(&flake);

    // The host is this machine; its profile lives in the probe root, which
    // for a chroot store is the store's root (so its links resolve).
    let probe_root = store.as_deref().and_then(store_root);
    let (profile_dir, profile) = match &probe_root {
        Some(store_root) => {
            let name = format!(
                "hostctl-e2e-{}",
                root.file_name().unwrap().to_str().unwrap()
            );
            (store_root.join(&name), format!("/{name}/profile"))
        }
        None => {
            let dir = root.join("profiles");
            let profile = dir.join("profile").display().to_string();
            (dir, profile)
        }
    };
    fs::create_dir_all(&profile_dir).unwrap();
    let profile_link = profile_dir.join("profile");
    set_profile(&profile_link, 1, &generations[0]);

    // `nix` on PATH logs its arguments, then is the real one.
    let bin = root.join("bin");
    fs::create_dir(&bin).unwrap();
    let log = root.join("nix.log");
    write_executable(
        &bin.join("nix"),
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >>'{}'\nexec '{}' \"$@\"\n",
            log.display(),
            real_nix.display()
        ),
    );

    let options = match &store {
        Some(store) => format!("nix.options = [\"--store\", {store:?}]\n"),
        None => String::new(),
    };
    let config = root.join("config.toml");
    fs::write(
        &config,
        format!(
            "flake = {:?}\nself = \"box\"\n{options}\
             [hosts.box]\nattr = \"generation\"\nprofile = {profile:?}\n",
            flake.display().to_string()
        ),
    )
    .unwrap();

    let run = |args: &[&str]| -> Run {
        let mut path = std::ffi::OsString::from(&bin);
        path.push(":");
        path.push(std::env::var_os("PATH").unwrap_or_default());
        let mut cmd = command(env!("CARGO_BIN_EXE_hostctl"));
        cmd.args(args)
            .current_dir(root)
            .env("PATH", path)
            .env("HOME", &home)
            .env("USER", "tester")
            .env("TMPDIR", root)
            .env("XDG_CONFIG_HOME", root.join("xdg/config"))
            .env("XDG_CACHE_HOME", root.join("xdg/cache"))
            .env("XDG_RUNTIME_DIR", root.join("xdg/run"))
            .env("HOSTCTL_CONFIG", &config)
            .env("NO_COLOR", "1");
        if let Some(probe_root) = &probe_root {
            cmd.env("HOSTCTL_PROBE_ROOT", probe_root);
        }
        finish(cmd.spawn().unwrap())
    };
    let evals = || {
        fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains(" eval --raw "))
            .count()
    };

    let first = run(&["status", "--json"]);
    first.code(0);
    let report = first.json();
    let host = &report["hosts"][0];
    assert_eq!(host["name"], "box", "{first}");
    assert_eq!(host["kind"], "custom");
    assert_eq!(host["local"], "local");
    assert_eq!(host["status"], "up-to-date", "{first}");
    assert_eq!(host["current"], generations[0].as_str());
    assert_eq!(host["desired"], generations[0].as_str());
    assert_eq!(host["generation"], 1);
    assert_eq!(host["cached"], false);
    let flake_report = &report["flakes"][0];
    assert!(
        flake_report["locked"]
            .as_str()
            .is_some_and(|locked| locked.starts_with("git+file://")),
        "{flake_report}"
    );
    assert_eq!(flake_report["lockOutdated"], false, "{flake_report}");
    assert!(evals() >= 1, "an evaluation ran");

    // Nothing changed: the result comes from the cache.
    fs::remove_file(&log).unwrap();
    let second = run(&["status", "--plain"]);
    second.code(0);
    assert!(
        second
            .stdout
            .contains("\n  ✓ box (this host)  up to date\n"),
        "{second}"
    );
    assert_eq!(
        evals(),
        0,
        "no evaluation the second time:\n{}",
        fs::read_to_string(&log).unwrap_or_default()
    );

    // The profile moves on to generation 2: box is outdated.
    set_profile(&profile_link, 2, &generations[1]);
    let third = run(&["status", "--plain"]);
    third.code(1);
    assert!(
        third.stdout.contains("\n  ✗ box (this host)  outdated\n"),
        "{third}"
    );

    assert!(!flake.join("flake.lock").exists(), "no lock file written");
    assert_eq!(snapshot(&flake), untouched, "the flake directory changed");
    let _ = fs::remove_dir_all(&profile_dir);
}
