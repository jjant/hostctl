//! hostctl's command line end to end: the real binary against the fake
//! `nix`, `ssh` and `nix-store` of tests/support (see tests/common).

mod common;

use std::time::{Duration, Instant};

use common::*;
use serde_json::{Value, json};

/// Discovery finds these home-manager configurations.
fn discover_home(w: &World, names: &[&str]) {
    w.discover(json!({ "home": names }));
}

/// Home-manager host `name` runs `current` (generation 5); its
/// configuration evaluates to `desired`.
fn home_host(w: &World, name: &str, current: &str, desired: &str) -> FakeHost {
    w.eval(&hm_attr(name), desired);
    let host = w.host(name);
    host.home_manager(5, current);
    host
}

/// The table of a run, with its duration masked.
fn table(run: &Run) -> String {
    masked(&run.stdout)
}

// ─── status and exit codes ──────────────────────────────────────────────

#[test]
fn everything_up_to_date_exits_0() {
    let w = World::new();
    w.flake(REV1);
    discover_home(&w, &["web1", "web2"]);
    home_host(&w, "web1", &hm('a'), &hm('a'));
    home_host(&w, "web2", &hm('b'), &hm('b'));

    let run = w.run(&["status", "--plain"]);
    run.code(0);
    assert_eq!(
        table(&run),
        "github:o/r @ 1111111 (2h ago)\n\
         \n  web1  ✓ up to date  gen 5 · 3d ago\
         \n  web2  ✓ up to date  gen 5 · 3d ago\n\
         \n2 up to date · <took>\n"
    );
    assert_eq!(run.stderr, "");
}

#[test]
fn outdated_and_never_activated_hosts_exit_1() {
    let w = World::new();
    w.flake(REV1);
    discover_home(&w, &["web1", "web2", "web3"]);
    home_host(&w, "web1", &hm('a'), &hm('a'));
    home_host(&w, "web2", &hm('b'), &hm('c'));
    w.eval(&hm_attr("web3"), &hm('d'));
    w.host("web3"); // reachable, but no home-manager profile

    let run = w.run(&["status", "--plain"]);
    run.code(1);
    assert_eq!(
        table(&run),
        "github:o/r @ 1111111 (2h ago)\n\
         \n  web1  ✓ up to date     gen 5 · 3d ago\
         \n  web2  ✗ outdated       gen 5 · 3d ago · unknown generation\
         \n  web3  ○ not activated  no home-manager profile\n\
         \n1 outdated · 1 not activated · 1 up to date · <took>\
         \nupdate: hostctl activate web2 web3\n"
    );
}

#[test]
fn without_a_command_or_a_terminal_hostctl_prints_the_status() {
    let w = World::new();
    w.flake(REV1);
    discover_home(&w, &["web1"]);
    home_host(&w, "web1", &hm('a'), &hm('b'));

    let plain = w.run(&["status", "--plain"]);
    let default = w.run(&[]);
    default.code(1);
    assert_eq!(table(&default), table(&plain));
    assert!(default.stdout.contains("web1  ✗ outdated"), "{default}");
}

#[test]
fn an_unreachable_host_exits_2() {
    let w = World::new();
    w.flake(REV1);
    discover_home(&w, &["web1", "web2"]);
    home_host(&w, "web1", &hm('a'), &hm('a'));
    home_host(&w, "web2", &hm('b'), &hm('b'))
        .unreachable("ssh: connect to host web2 port 22: Connection refused");

    let run = w.run(&["status", "--plain"]);
    run.code(2);
    assert!(
        run.stdout.contains(
            "\n  web2  ? unreachable  ssh: connect to host web2 port 22: Connection refused\n"
        ),
        "{run}"
    );
    assert!(
        run.stdout.contains("\n1 unreachable · 1 up to date · "),
        "{run}"
    );
}

#[test]
fn trouble_wins_over_outdated() {
    let w = World::new();
    w.flake(REV1);
    discover_home(&w, &["web1", "web2"]);
    home_host(&w, "web1", &hm('a'), &hm('b'));
    w.eval(&hm_attr("web2"), &hm('c'));
    w.host("web2")
        .unreachable("ssh: Could not resolve hostname web2: Name or service not known");

    let run = w.run(&["status", "--plain"]);
    run.code(2);
    // The hint goes to the details (JSON, dashboard), not the table.
    assert!(
        run.stdout
            .contains("  web2  ? unreachable  ssh: Could not resolve hostname web2: Name or service not known\n"),
        "{run}"
    );
    let json = w.run(&["status", "--json"]).json();
    let detail = json["hosts"][1]["error"]["detail"].as_str().unwrap();
    assert!(
        detail.contains("hint: web2 doesn't resolve: set the host's destination"),
        "{detail}"
    );
}

#[test]
fn an_unusable_answer_is_a_failed_probe_and_exits_2() {
    let w = World::new();
    w.flake(REV1);
    discover_home(&w, &["web1"]);
    // Its `sh` isn't one: whatever it prints is no probe answer.
    home_host(&w, "web1", &hm('a'), &hm('a')).bin("sh", "#!/bin/sh\necho 'Welcome to web1!'\n");

    let run = w.run(&["status", "--plain"]);
    run.code(2);
    assert!(
        run.stdout
            .contains("  web1  ! probe failed  unexpected output: \"Welcome to web1!\"\n"),
        "{run}"
    );
}

#[test]
fn a_configuration_that_does_not_evaluate_exits_2() {
    let w = World::new();
    w.flake(REV1);
    discover_home(&w, &["web1", "web2"]);
    home_host(&w, "web1", &hm('a'), &hm('a'));
    w.host("web2").home_manager(5, &hm('b'));
    w.eval_fails(
        &hm_attr("web2"),
        "error:\n       … while evaluating the attribute 'activationPackage'\n\n       \
         error: attribute 'web2' missing\n",
    );

    let run = w.run(&["status", "--plain"]);
    run.code(2);
    assert!(
        run.stdout
            .contains("  web2  ! eval failed  attribute 'web2' missing\n"),
        "{run}"
    );
    let json = w.run(&["status", "--json", "--no-cache"]).json();
    let error = &json["hosts"][1]["error"];
    assert_eq!(error["summary"], "attribute 'web2' missing");
    let detail = error["detail"].as_str().unwrap();
    assert!(
        detail.starts_with(&format!(
            "$ {} --extra-experimental-features 'nix-command flakes' eval --raw --read-only",
            w.fake.join("bin/nix").display()
        )),
        "the detail names the command: {detail}"
    );
    assert!(
        detail.ends_with("error: attribute 'web2' missing"),
        "{detail}"
    );
}

#[test]
fn a_stale_flake_exits_2() {
    let w = World::new();
    w.flake(REV1).stderr(
        "warning: error: unable to download 'https://api.github.com/repos/o/r/commits/HEAD': \
         HTTP error 503; using cached version\n",
    );
    discover_home(&w, &["web1"]);
    home_host(&w, "web1", &hm('a'), &hm('a'));

    let run = w.run(&["status", "--plain"]);
    run.code(2);
    assert!(
        run.stdout.starts_with(
            "github:o/r @ 1111111 (2h ago) · stale: unable to download \
             'https://api.github.com/repos/o/r/commits/HEAD': HTTP error 503\n"
        ),
        "{run}"
    );
    assert!(run.stdout.contains("  web1  ✓ up to date"), "{run}");
}

#[test]
fn a_flake_that_can_t_be_refreshed_falls_back_to_its_last_resolution() {
    let w = World::new();
    w.flake(REV1);
    discover_home(&w, &["web1"]);
    home_host(&w, "web1", &hm('a'), &hm('a'));
    w.run(&["status"]).code(0);

    // GitHub is down now.
    w.flake(REV1)
        .stderr("error: unable to download 'https://api.github.com/repos/o/r/commits/HEAD': HTTP error 503\n")
        .exit(1);
    let run = w.run(&["status", "--plain", "--refresh"]);
    run.code(2);
    assert!(
        run.stdout.starts_with(
            "github:o/r @ 1111111 (2h ago) · stale: unable to download \
             'https://api.github.com/repos/o/r/commits/HEAD': HTTP error 503\n"
        ),
        "{run}"
    );
    assert!(run.stdout.contains("  web1  ✓ up to date"), "{run}");
}

#[test]
fn config_errors_exit_3() {
    let w = World::new();
    w.flake(REV1);
    w.config("timout = \"20s\"\n");
    let run = w.run(&["status"]);
    run.code(3);
    assert!(
        run.stderr.starts_with(&format!(
            "error: invalid config file {}: ",
            w.config_file.display()
        )),
        "{run}"
    );
    assert!(run.stderr.contains("unknown field `timout`"), "{run}");
    assert_eq!(run.stdout, "");
    assert_eq!(w.calls().len(), 0, "nothing runs: {}", w.log());

    let w = World::new();
    w.config("timeout = \"0s\"\n[hosts.web1]\nkind = \"nixos\"\nprofile = \"relative\"\n");
    let run = w.run(&["status"]);
    run.code(3);
    assert!(
        run.stderr.contains(
            "2 problems:\n  timeout must be greater than 0\n  \
             host \"web1\": profile \"relative\" must be absolute or start with ~/\n"
        ),
        "{run}"
    );

    let w = World::new();
    w.config("flake = [\n");
    let run = w.run(&["status"]);
    run.code(3);
    assert!(run.stderr.contains("TOML parse error"), "{run}");
}

#[test]
fn error_messages_reach_the_terminal_sanitized() {
    let w = World::new();
    // The parse error quotes the line, escape sequence and all.
    w.config("\x1b]0;pwned\x07bogus = 1\n");
    let run = w.run(&["status"]);
    run.code(3);
    assert!(
        run.stderr.contains("\\u{1b}]0;pwned\\u{7}bogus = 1"),
        "{run}"
    );
    let missing = w.root().join("x\x1b[2J.toml");
    let run = w.run(&["-c", missing.to_str().unwrap(), "status"]);
    run.code(3);
    assert!(run.stderr.contains("x\\u{1b}[2J.toml"), "{run}");
    for run in [&run, &w.run(&["status"])] {
        assert!(!run.stderr.contains(['\x1b', '\x07']), "{:?}", run.stderr);
    }
}

#[test]
fn usage_errors_exit_3_and_help_exits_0() {
    let w = World::new();
    let run = w.run(&["--bogus"]);
    run.code(3);
    assert!(
        run.stderr.contains("unexpected argument '--bogus'"),
        "{run}"
    );
    w.run(&["status", "--refresh", "--offline"]).code(3);
    w.run(&["activate"]).code(3);

    let help = w.run(&["--help"]);
    help.code(0);
    assert!(
        help.stdout
            .contains("Exit codes: 0 up to date, 1 outdated, 2 trouble"),
        "{help}"
    );
    let version = w.run(&["--version"]);
    version.code(0);
    assert_eq!(
        version.stdout,
        format!("hostctl {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn the_dashboard_needs_a_terminal() {
    let w = World::new();
    let run = w.run(&["tui"]);
    run.code(3);
    assert_eq!(
        run.stderr,
        "error: the dashboard needs a terminal (stdout isn't one): use `hostctl status`\n"
    );
}

#[test]
fn no_hosts_at_all_exits_3() {
    let w = World::new();
    w.flake(REV1);
    let run = w.run(&["status"]);
    run.code(3);
    assert_eq!(
        run.stderr,
        format!(
            "error: no hosts: github:o/r has no nixosConfigurations, darwinConfigurations or \
             homeConfigurations (nor lib.hostctl.hosts); add [hosts.<name>] tables to {}, or \
             point --flake at another flake\n",
            w.config_file.display()
        )
    );

    // Without a flake, hostctl says how to give one.
    let w = World::new();
    w.raw_config(&w.base_config().replace("flake = \"github:o/r\"\n", ""));
    let run = w.run(&["status"]);
    run.code(3);
    assert!(
        run.stderr.starts_with(
            "error: no hosts to check: point --flake (or flake = \"…\" in the config)"
        ),
        "{run}"
    );
}

#[test]
fn discovery_that_fails_without_configured_hosts_exits_3() {
    let w = World::new();
    w.flake(REV1);
    w.discover(json!(null))
        .stderr("error: syntax error, unexpected '}'\n")
        .exit(1);
    let run = w.run(&["status"]);
    run.code(3);
    assert!(
        run.stderr.starts_with(
            "error: can't read the hosts of github:o/r: syntax error, unexpected '}'\n"
        ),
        "{run}"
    );

    // With configured hosts, those are checked anyway.
    w.config("[hosts.web1]\nkind = \"home-manager\"\n");
    home_host(&w, "web1", &hm('a'), &hm('a'));
    let run = w.run(&["status", "--plain"]);
    run.code(0);
    assert_eq!(
        run.stderr,
        "warning: can't read the hosts of github:o/r: syntax error, unexpected '}'; \
         checking the configured hosts only\n"
    );
}

#[test]
fn a_nix_that_can_t_be_run_exits_3_before_checking() {
    let w = World::new();
    let nix = w.fake.join("bin/nix").display().to_string();
    w.raw_config(&w.base_config().replace(&nix, "/nonexistent/nix"));
    let run = w.run(&["status"]);
    run.code(3);
    assert!(
        run.stderr.starts_with(
            "error: can't read the hosts of github:o/r: nix.command = \"/nonexistent/nix\" \
             could not be run: No such file or directory"
        ),
        "{run}"
    );
}

#[test]
fn a_nix_that_can_t_be_run_while_checking_exits_3() {
    let w = World::new();
    w.config("discover = false\ninventory = \"\"\n[hosts.web1]\nkind = \"home-manager\"\n");
    home_host(&w, "web1", &hm('a'), &hm('a'));
    let nix = w.fake.join("bin/nix").display().to_string();
    w.raw_config(
        &std::fs::read_to_string(&w.config_file)
            .unwrap()
            .replace(&nix, "/nonexistent/nix"),
    );
    let run = w.run(&["status", "--json"]);
    run.code(3);
    let report = run.json();
    assert_eq!(report["hosts"][0]["status"], "eval-failed");
    assert_eq!(
        report["errors"],
        json!([{
            "summary": "nix.command = \"/nonexistent/nix\" could not be run: No such file or \
                        directory (os error 2); is Nix installed and on PATH?",
            "detail": "nix.command = \"/nonexistent/nix\" could not be run: No such file or \
                       directory (os error 2); is Nix installed and on PATH?",
        }])
    );
}

#[test]
fn an_ssh_that_can_t_be_run_is_reported_once() {
    let w = World::new();
    w.flake(REV1);
    discover_home(&w, &["web1", "web2"]);
    home_host(&w, "web1", &hm('a'), &hm('a'));
    home_host(&w, "web2", &hm('b'), &hm('b'));
    let ssh = w.fake.join("bin/ssh").display().to_string();
    w.raw_config(&w.base_config().replace(&ssh, "/nonexistent/ssh"));
    let run = w.run(&["status", "--plain"]);
    // The check couldn't run.
    run.code(3);
    assert_eq!(
        table(&run),
        "github:o/r @ 1111111 (2h ago)\n\
         \n  web1  ? unreachable  ssh couldn't be run\
         \n  web2  ? unreachable  ssh couldn't be run\n\
         \n2 unreachable · <took>\
         \nerror: ssh.command = \"/nonexistent/ssh\" could not be run: No such file or directory \
         (os error 2); is OpenSSH installed and on PATH?\n"
    );
}

#[test]
fn colors_only_when_asked_for_without_a_terminal() {
    let w = World::new();
    w.flake(REV1);
    discover_home(&w, &["web1"]);
    home_host(&w, "web1", &hm('a'), &hm('a'));
    let auto = w.run(&["status", "--plain"]);
    auto.code(0);
    assert!(!auto.stdout.contains('\x1b'), "{auto}");
    // `always` beats NO_COLOR (which the world sets) and the missing terminal.
    let always = w.run(&["status", "--plain", "--color", "always"]);
    always.code(0);
    assert!(
        always
            .stdout
            .contains("  web1  \x1b[32m✓ up to date\x1b[0m  \x1b[2mgen 5 · 3d ago\x1b[0m\n"),
        "{always:?}"
    );
}

#[test]
fn a_pattern_that_matches_nothing_exits_3() {
    let w = World::new();
    w.flake(REV1);
    discover_home(&w, &["web1", "web2"]);
    let run = w.run(&["status", "web*", "db*"]);
    run.code(3);
    assert_eq!(
        run.stderr,
        "error: no host matches \"db*\" (hosts: web1, web2)\n"
    );
    let run = w.run(&["status", "@local"]);
    run.code(3);
    assert_eq!(
        run.stderr,
        "error: no host matches \"@local\" (hosts: web1, web2); none is this machine \
         (set self, or pass --self HOST)\n"
    );
    assert!(
        w.ssh_calls("web1").is_empty(),
        "nothing is probed: {}",
        w.log()
    );
}

#[test]
fn an_interrupted_check_exits_130() {
    let w = World::new();
    w.flake(REV1);
    discover_home(&w, &["web1"]);
    home_host(&w, "web1", &hm('a'), &hm('a'));
    w.eval(&hm_attr("web1"), &hm('a')).delay(30.0);

    let child = w.command(&["status", "--plain"]).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !w.log().contains(" eval --raw ") {
        assert!(
            Instant::now() < deadline,
            "no evaluation started: {}",
            w.log()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let interrupted = Instant::now();
    // SAFETY: a plain syscall; the child leads its own process group.
    unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGINT) };
    let run = finish(child);
    run.code(130);
    assert!(
        interrupted.elapsed() < Duration::from_secs(10),
        "the sleeping evaluation was killed"
    );
    assert!(run.stdout.contains("  web1  … interrupted\n"), "{run}");
    assert!(run.stdout.contains("\n1 pending\n"), "{run}");
    let eval = &w.evals()[0];
    assert_eq!(eval.end, None, "the evaluation was killed: {}", w.log());
}

#[test]
fn an_interrupted_check_leaves_no_process_behind() {
    let w = World::new();
    w.flake(REV1);
    discover_home(&w, &["web1"]);
    home_host(&w, "web1", &hm('a'), &hm('a'));
    // Only SIGKILL stops it, which comes 200 ms after the SIGTERM: later
    // than hostctl would be gone if it didn't wait.
    let answer = w.eval(&hm_attr("web1"), &hm('a'));
    answer.delay(30.0).ignore_sigterm();

    let child = w.command(&["status", "--plain"]).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !answer.ignoring_sigterm() {
        assert!(
            Instant::now() < deadline,
            "no evaluation started: {}",
            w.log()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let log = w.log();
    let eval: libc::pid_t = log
        .lines()
        .find(|line| line.contains(" eval --raw "))
        .and_then(|line| line.split(' ').nth(1)?.parse().ok())
        .unwrap_or_else(|| panic!("no evaluation in the log: {log}"));
    // SAFETY: a plain syscall; the child leads its own process group.
    unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGINT) };
    finish(child).code(130);
    // The evaluation leads its own process group: none of it is left.
    let gone = || {
        // SAFETY: signal 0 only asks whether the group has any process.
        unsafe { libc::killpg(eval, 0) != 0 }
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    while !gone() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let survived = !gone();
    if survived {
        // SAFETY: as above; nothing may outlive the test either.
        unsafe { libc::killpg(eval, libc::SIGKILL) };
    }
    assert!(!survived, "the evaluation outlived hostctl:\n{}", w.log());
}

// ─── JSON ───────────────────────────────────────────────────────────────

/// The keys every host of `status --json` has, null or not (sorted).
const HOST_KEYS: [&str; 22] = [
    "activate",
    "activated",
    "attr",
    "booted",
    "cached",
    "current",
    "desired",
    "destination",
    "drift",
    "error",
    "flake",
    "generation",
    "kind",
    "local",
    "name",
    "note",
    "platform",
    "rebootRequired",
    "revision",
    "specialisation",
    "status",
    "timings",
];

fn keys(value: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value
        .as_object()
        .unwrap_or_else(|| panic!("not an object: {value}"))
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

#[test]
fn the_json_report_has_every_field() {
    let w = World::new();
    w.flake(REV1);
    w.discover(json!({ "home": ["web1", "web2", "web3"], "darwin": ["mac"] }));
    w.config(
        "[hosts.mac]\noptional = true\nnote = \"reachable from the desk only\"\n\
         [hosts.printer]\nattr = \"packages.aarch64-linux.printer\"\nprofile = \"/nix/var/nix/profiles/printer\"\n",
    );
    home_host(&w, "web1", &hm('a'), &hm('a'));
    // web2 runs an older revision, which its generation records.
    home_host(&w, "web2", &hm('c'), &hm('d')).revision(&hm('c'), REV2);
    w.host("web3");
    w.eval(&hm_attr("web3"), &hm('f'));
    w.eval(
        "darwinConfigurations.mac.system",
        &store('m', "darwin-system-26.05"),
    );
    w.host("mac")
        .unreachable("ssh: connect to host mac port 22: No route to host");
    let printer = store('p', "printer-image");
    w.eval("packages.aarch64-linux.printer", &printer);
    w.host("printer")
        .profile("/nix/var/nix/profiles/printer", 3, &printer);

    let run = w.run(&["status", "--json"]);
    run.code(1);
    let report = run.json();
    assert_eq!(
        keys(&report),
        ["errors", "flakes", "hosts", "summary", "version"]
    );
    assert_eq!(report["version"], 1);
    assert_eq!(report["errors"], json!([]));
    assert_eq!(
        report["summary"],
        json!({ "total": 5, "upToDate": 2, "outdated": 2, "trouble": 0, "pending": 0, "offline": 1 })
    );
    let locked = World::locked(FLAKE, REV1);
    let modified = report["flakes"][0]["lastModified"].as_i64().unwrap();
    assert!((now() - 2 * 3600 - 120..=now()).contains(&modified));
    assert_eq!(
        report["flakes"],
        json!([{
            "flake": FLAKE,
            "locked": locked,
            "rev": REV1,
            "lastModified": modified,
            "stale": null,
            "lockOutdated": false,
            "error": null,
        }])
    );

    let hosts = report["hosts"].as_array().unwrap();
    for host in hosts {
        assert_eq!(keys(host), HOST_KEYS, "{host}");
        assert_eq!(keys(&host["timings"]), ["evalMs", "probeMs"], "{host}");
    }
    let by_name = |name: &str| {
        hosts
            .iter()
            .find(|h| h["name"] == name)
            .unwrap_or_else(|| panic!("no host {name}"))
    };
    let names: Vec<&str> = hosts.iter().map(|h| h["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["mac", "printer", "web1", "web2", "web3"]);

    let web1 = by_name("web1");
    assert_eq!(web1["status"], "up-to-date");
    assert_eq!(web1["kind"], "home-manager");
    assert_eq!(web1["local"], Value::Null);
    assert_eq!(web1["destination"], "web1");
    assert_eq!(web1["attr"], hm_attr("web1"));
    assert_eq!(web1["current"], hm('a'));
    assert_eq!(web1["desired"], hm('a'));
    assert_eq!(web1["cached"], false);
    assert_eq!(web1["generation"], 5);
    assert_eq!(web1["activated"], w.activated);
    assert_eq!(web1["drift"], Value::Null);
    assert_eq!(web1["error"], Value::Null);
    assert_eq!(web1["revision"], Value::Null);
    assert_eq!(web1["rebootRequired"], false);
    assert!(web1["timings"]["probeMs"].is_u64() && web1["timings"]["evalMs"].is_u64());
    let platform = web1["platform"].as_str().unwrap();
    assert!(
        platform.ends_with("-linux") || platform.ends_with("-darwin"),
        "{platform}"
    );
    assert_eq!(
        web1["activate"],
        json!({
            "command": format!(
                "out=$(nix --extra-experimental-features 'nix-command flakes' build --no-link \
                 --print-out-paths '{locked}'#.homeConfigurations.web1.activationPackage) && \
                 [ \"$out\" = {} ] && \"$out/activate\"",
                hm('a')
            ),
            "runsOn": "remote",
        })
    );

    let web2 = by_name("web2");
    assert_eq!(web2["status"], "outdated");
    assert_eq!(web2["revision"], REV2);
    assert_eq!(
        web2["drift"],
        json!({ "type": "revision", "rev": REV2, "lastModified": null, "fromGeneration": true })
    );

    let web3 = by_name("web3");
    assert_eq!(web3["status"], "not-activated");
    assert_eq!(web3["current"], Value::Null);
    assert_eq!(web3["generation"], Value::Null);
    assert_eq!(web3["drift"], Value::Null);
    assert!(
        web3["activate"].is_object(),
        "not activated hosts can be activated"
    );

    let mac = by_name("mac");
    assert_eq!(mac["status"], "offline");
    assert_eq!(mac["kind"], "darwin");
    assert_eq!(mac["note"], "reachable from the desk only");
    assert_eq!(mac["current"], Value::Null);
    assert_eq!(
        mac["error"]["summary"],
        "ssh: connect to host mac port 22: No route to host"
    );
    assert!(mac["timings"]["evalMs"].is_u64(), "{mac}");

    let printer_host = by_name("printer");
    assert_eq!(printer_host["status"], "up-to-date");
    assert_eq!(printer_host["kind"], "custom");
    assert_eq!(printer_host["generation"], 3);
    assert_eq!(
        printer_host["activate"],
        Value::Null,
        "custom hosts have no default"
    );
}

// ─── probes, evaluations and the cache ──────────────────────────────────

#[test]
fn probes_and_evaluations_run_in_parallel() {
    let w = World::new();
    w.flake(REV1);
    let names = ["web1", "web2", "web3"];
    discover_home(&w, &names);
    for (name, c) in names.into_iter().zip(['a', 'b', 'c']) {
        // Each answers only once all three have started (or after 10 s).
        home_host(&w, name, &hm(c), &hm(c)).rendezvous(3);
        w.eval(&hm_attr(name), &hm(c)).rendezvous(3);
    }
    w.run(&["status"]).code(0);
    let probes: Vec<Call> = w.calls().into_iter().filter(|c| c.tool == "ssh").collect();
    assert_eq!(probes.len(), 3, "{}", w.log());
    assert!(overlapping(&probes), "probes in parallel:\n{}", w.log());
    let evals = w.evals();
    assert_eq!(evals.len(), 3, "{}", w.log());
    assert!(overlapping(&evals), "evaluations in parallel:\n{}", w.log());
}

#[test]
fn probes_start_while_the_flake_resolves() {
    let w = World::new();
    w.config(
        "discover = false\ninventory = \"\"\n\
         [hosts.web1]\nkind = \"home-manager\"\n[hosts.web2]\nkind = \"home-manager\"\n",
    );
    // The resolution answers once both probes have started.
    w.flake(REV1).rendezvous(2);
    home_host(&w, "web1", &hm('a'), &hm('a'));
    home_host(&w, "web2", &hm('b'), &hm('b'));
    w.run(&["status"]).code(0);
    let resolution = &w.resolutions()[0];
    let probes: Vec<Call> = w.calls().into_iter().filter(|c| c.tool == "ssh").collect();
    assert_eq!(probes.len(), 2);
    assert!(
        probes.iter().all(|p| Some(p.start) < resolution.end),
        "probes started before the resolution ended:\n{}",
        w.log()
    );
    assert!(w.nix_calls("eval --json").is_empty(), "no discovery");
}

#[test]
fn one_job_evaluates_one_configuration_at_a_time() {
    let w = World::new();
    w.flake(REV1);
    let names = ["web1", "web2", "web3"];
    discover_home(&w, &names);
    for (name, c) in names.into_iter().zip(['a', 'b', 'c']) {
        home_host(&w, name, &hm(c), &hm(c));
        w.eval(&hm_attr(name), &hm(c)).delay(0.2);
    }
    w.run(&["status", "-j", "1"]).code(0);
    let evals = w.evals();
    assert_eq!(evals.len(), 3, "{}", w.log());
    assert!(sequential(&evals), "one at a time:\n{}", w.log());
}

#[test]
fn hosts_with_the_same_attribute_share_one_evaluation() {
    let w = World::new();
    w.flake(REV1);
    w.config(
        "[defaults]\nattr = \"packages.x86_64-linux.kiosk\"\nprofile = \"/nix/var/nix/profiles/kiosk\"\n\
         [hosts.kiosk1]\n[hosts.kiosk2]\n",
    );
    let kiosk = store('k', "kiosk-image");
    w.eval("packages.x86_64-linux.kiosk", &kiosk);
    w.host("kiosk1")
        .profile("/nix/var/nix/profiles/kiosk", 2, &kiosk);
    w.host("kiosk2")
        .profile("/nix/var/nix/profiles/kiosk", 4, &kiosk);
    let run = w.run(&["status", "--plain"]);
    run.code(0);
    assert_eq!(
        table(&run),
        "github:o/r @ 1111111 (2h ago)\n\
         \n  kiosk1  ✓ up to date  gen 2 · 3d ago\
         \n  kiosk2  ✓ up to date  gen 4 · 3d ago\n\
         \n2 up to date · <took>\n"
    );
    assert_eq!(w.evals().len(), 1, "{}", w.log());
}

#[test]
fn import_from_derivation_is_evaluated_without_read_only_from_then_on() {
    let w = World::new();
    w.flake(REV1);
    discover_home(&w, &["web1"]);
    home_host(&w, "web1", &hm('a'), &hm('a'));
    w.eval(&hm_attr("web1"), &hm('a')).read_only_fails();
    w.run(&["status"]).code(0);
    let evals = w.evals();
    assert_eq!(evals.len(), 2, "{}", w.log());
    assert!(evals[0].args.contains(" --read-only "), "{}", evals[0].args);
    assert!(!evals[1].args.contains("--read-only"), "{}", evals[1].args);

    // The next revision is evaluated normally right away.
    w.flake(REV2);
    w.clear_log();
    w.run(&["status", "--refresh"]).code(0);
    let evals = w.evals();
    assert_eq!(evals.len(), 1, "{}", w.log());
    assert!(!evals[0].args.contains("--read-only"), "{}", evals[0].args);
}

#[test]
fn a_second_run_evaluates_nothing() {
    let w = World::new();
    w.flake(REV1);
    discover_home(&w, &["web1", "web2"]);
    home_host(&w, "web1", &hm('a'), &hm('a'));
    home_host(&w, "web2", &hm('b'), &hm('c'));
    let first = w.run(&["status", "--plain"]);
    first.code(1);
    assert_eq!(w.evals().len(), 2, "{}", w.log());
    assert!(
        w.cache()["flakes"]
            .as_object()
            .is_some_and(|f| !f.is_empty())
    );

    w.clear_log();
    let second = w.run(&["status", "--plain"]);
    second.code(1);
    assert_eq!(table(&second), table(&first));
    assert!(w.evals().is_empty(), "no evaluation:\n{}", w.log());
    assert!(
        w.resolutions().is_empty(),
        "a resolution younger than nix.refresh is reused:\n{}",
        w.log()
    );
    assert!(
        w.nix_calls("eval --json").is_empty(),
        "the hosts are cached too:\n{}",
        w.log()
    );
    assert_eq!(w.ssh_calls("web1").len() + w.ssh_calls("web2").len(), 2);

    let json = w.run(&["status", "--json"]).json();
    for host in json["hosts"].as_array().unwrap() {
        assert_eq!(host["cached"], true, "{host}");
        assert_eq!(host["timings"]["evalMs"], Value::Null, "{host}");
    }
}

#[test]
fn no_cache_evaluates_again_and_leaves_the_cache_alone() {
    let w = World::new();
    w.flake(REV1);
    discover_home(&w, &["web1", "web2"]);
    home_host(&w, "web1", &hm('a'), &hm('a'));
    home_host(&w, "web2", &hm('b'), &hm('c'));
    w.run(&["status"]).code(1);
    let cache = w.cache();
    assert_ne!(cache, Value::Null);

    w.clear_log();
    let run = w.run(&["status", "--plain", "--no-cache"]);
    run.code(1);
    assert_eq!(w.evals().len(), 2, "{}", w.log());
    assert_eq!(w.resolutions().len(), 1, "{}", w.log());
    assert_eq!(w.nix_calls("eval --json").len(), 1, "{}", w.log());
    assert!(
        w.nix_calls("--version").is_empty(),
        "no cache key, no version:\n{}",
        w.log()
    );
    assert_eq!(w.cache(), cache, "the cache file is untouched");
}

#[test]
fn offline_uses_only_what_the_cache_has() {
    let w = World::new();
    w.flake(REV1);
    discover_home(&w, &["web1"]);
    home_host(&w, "web1", &hm('a'), &hm('b'));

    // Never resolved: nothing to go on.
    let run = w.run(&["status", "--offline"]);
    run.code(3);
    assert_eq!(
        run.stderr,
        "error: can't read the hosts of github:o/r: github:o/r was never resolved, and \
         --offline only uses cached resolutions\n"
    );
    assert!(w.resolutions().is_empty(), "{}", w.log());

    let online = w.run(&["status", "--plain"]);
    online.code(1);
    // Even with every resolution out of date, --offline doesn't look.
    w.config("nix.refresh = \"0s\"\n");
    w.clear_log();
    let run = w.run(&["status", "--plain", "--offline"]);
    run.code(1);
    assert_eq!(table(&run), table(&online));
    assert!(w.resolutions().is_empty(), "no metadata call:\n{}", w.log());
    assert!(w.evals().is_empty(), "no evaluation:\n{}", w.log());
    assert!(w.nix_calls("eval --json").is_empty(), "{}", w.log());
    assert_eq!(w.ssh_calls("web1").len(), 1, "probes still run");

    // While online, nix.refresh = 0 resolves on every run.
    w.clear_log();
    w.run(&["status"]).code(1);
    assert_eq!(w.resolutions().len(), 1, "{}", w.log());
}

#[test]
fn offline_evaluates_nothing_the_cache_lacks() {
    let w = World::new();
    w.flake(REV1);
    discover_home(&w, &["web1", "web2"]);
    home_host(&w, "web1", &hm('a'), &hm('a'));
    w.host("web2").home_manager(5, &hm('b'));
    w.eval_fails(&hm_attr("web2"), "error: attribute 'web2' missing\n");
    w.run(&["status"]).code(2);

    // Nothing is cached for web2, whose evaluation failed.
    w.clear_log();
    let run = w.run(&["status", "--plain", "--offline"]);
    run.code(2);
    assert!(
        run.stdout
            .contains("\n  web2  ! eval failed  not cached; run hostctl once without --offline\n"),
        "{run}"
    );
    assert!(run.stdout.contains("\n  web1  ✓ up to date  "), "{run}");
    assert!(w.evals().is_empty(), "no evaluation:\n{}", w.log());

    // Nor are the hosts of another inventory attribute.
    w.config("inventory = \"lib.fleet\"\n");
    let run = w.run(&["status", "--offline"]);
    run.code(3);
    assert!(
        run.stderr.starts_with(
            "error: can't read the hosts of github:o/r: not cached; run hostctl once without \
             --offline\n  --offline evaluates nothing"
        ),
        "{run}"
    );
    assert!(w.nix_calls("eval --json").is_empty(), "{}", w.log());
}

#[test]
fn the_cache_remembers_which_revision_a_host_runs() {
    let w = World::new();
    w.flake(REV1);
    discover_home(&w, &["web1"]);
    home_host(&w, "web1", &hm('a'), &hm('a'));
    let first = w.run(&["status", "--json"]);
    first.code(0);
    let modified = first.json()["flakes"][0]["lastModified"].clone();

    // The flake moves on; web1 still runs the generation of REV1.
    w.flake(REV2);
    w.eval(&hm_attr("web1"), &hm('b'));
    let run = w.run(&["status", "--plain", "--refresh"]);
    run.code(1);
    assert!(
        run.stdout.starts_with("github:o/r @ 2222222 (2h ago)\n"),
        "{run}"
    );
    assert!(
        run.stdout
            .contains("\n  web1  ✗ outdated  gen 5 · 3d ago · on 1111111 (2h old)\n"),
        "{run}"
    );
    let json = w.run(&["status", "--json"]).json();
    assert_eq!(
        json["hosts"][0]["drift"],
        json!({ "type": "revision", "rev": REV1, "lastModified": modified, "fromGeneration": false })
    );
}

// ─── where hosts come from ──────────────────────────────────────────────

/// A flake whose discovery and inventory describe `web1` (nixos), `jj@desk`
/// (home-manager), `printer` (custom, inventory only) and an installer ISO;
/// the config excludes the ISO, sets a destination template and adds `mac`
/// behind a reverse tunnel.
fn layered_world() -> World {
    let w = World::new();
    w.flake(REV1);
    w.discover(json!({
        "nixos": ["web1", "installer-iso"],
        "home": ["jj@desk"],
        "inventory": {
            "web1": {
                "note": "the web server",
                "ssh-options": ["-o", "ProxyCommand=sh -c 'touch /tmp/pwned'"],
            },
            "printer": {
                "attr": "packages.aarch64-linux.printer",
                "profile": "/nix/var/nix/profiles/printer",
            },
        },
    }));
    w.config(
        "exclude = [\"*-iso\"]\n\
         [defaults]\ndestination = \"{name}.lan\"\n\
         [hosts.mac]\nkind = \"darwin\"\ndestination = \"ssh://jj@localhost:2022\"\nprobe = \"current-system\"\n",
    );
    w
}

#[test]
fn hosts_shows_every_layer_and_its_warnings() {
    let w = layered_world();
    let run = w.run(&["hosts"]);
    run.code(0);
    assert_eq!(
        run.stdout,
        "jj@desk  home-manager  jj@desk.lan                                      discovered             homeConfigurations.\"jj@desk\".activationPackage\n\
         mac      darwin        ssh://jj@localhost:2022 (probe: current-system)  config                 darwinConfigurations.mac.system\n\
         printer  custom        printer.lan                                      inventory              packages.aarch64-linux.printer\n\
         web1     nixos         web1.lan                                         discovered, inventory  nixosConfigurations.web1.config.system.build.toplevel\n\
         \n\
         warning: host \"web1\": ignoring ssh-options from the flake's inventory (set them in your config)\n"
    );
    assert_eq!(run.stderr, "");
    assert!(w.evals().is_empty() && w.calls().iter().all(|c| c.tool == "nix"));

    // Patterns select there too.
    let run = w.run(&["hosts", "w*"]);
    run.code(0);
    assert!(
        run.stdout
            .starts_with("web1  nixos  web1.lan  discovered, inventory  "),
        "{run}"
    );
}

#[test]
fn checks_reach_hosts_the_way_the_layers_say() {
    let w = layered_world();
    let desk = hm('d');
    w.eval("homeConfigurations.\"jj@desk\".activationPackage", &desk);
    w.host("desk.lan").home_manager(8, &desk);
    let web1 = store('w', "nixos-system-web1-26.05");
    w.eval(
        "nixosConfigurations.web1.config.system.build.toplevel",
        &web1,
    );
    w.host("web1.lan").system(&web1, 41, &web1);
    let printer = store('p', "printer-image");
    w.eval("packages.aarch64-linux.printer", &printer);
    w.host("printer.lan")
        .profile("/nix/var/nix/profiles/printer", 3, &printer);
    let mac = store('m', "darwin-system-26.05");
    w.eval("darwinConfigurations.mac.system", &mac);
    w.host("localhost").forced(&format!("{mac}\n"));

    let run = w.run(&["status", "--plain"]);
    run.code(0);
    assert_eq!(
        table(&run),
        "github:o/r @ 1111111 (2h ago)\n\
         \n  jj@desk  ✓ up to date  gen 8 · 3d ago\
         \n  mac      ✓ up to date\
         \n  printer  ✓ up to date  gen 3 · 3d ago\
         \n  web1     ✓ up to date  gen 41 · 3d ago · the web server\n\
         \n4 up to date · <took>\n"
    );
    assert_eq!(
        run.stderr,
        "warning: host \"web1\": ignoring ssh-options from the flake's inventory (set them in your config)\n"
    );
    let mut destinations: Vec<String> = w
        .calls()
        .iter()
        .filter(|c| c.tool == "ssh")
        .map(|c| c.destination().to_string())
        .collect();
    destinations.sort();
    assert_eq!(
        destinations,
        ["jj@desk.lan", "jj@localhost", "printer.lan", "web1.lan"]
    );
    for call in w.calls() {
        assert!(!call.args.contains("ProxyCommand"), "{}", call.args);
    }
    let mac_call = &w.ssh_calls("jj@localhost")[0];
    assert!(
        mac_call
            .args
            .ends_with(" -p 2022 -- jj@localhost current-system"),
        "the forced command, not the script: {}",
        mac_call.args
    );
}

#[test]
fn patterns_pick_hosts_by_name_or_this_machine() {
    let w = World::new();
    w.flake(REV1);
    discover_home(&w, &["bigmem", "db1", "web1", "web2"]);
    w.config("self = \"bigmem\"\n");
    w.this_machine("bigmem").home_manager(9, &hm('a'));
    w.eval(&hm_attr("bigmem"), &hm('a'));
    for (name, c) in [("db1", 'b'), ("web1", 'c'), ("web2", 'd')] {
        home_host(&w, name, &hm(c), &hm(c));
    }

    let run = w.run(&["status", "--plain", "web*"]);
    run.code(0);
    assert_eq!(
        table(&run),
        "github:o/r @ 1111111 (2h ago)\n\
         \n  web1  ✓ up to date  gen 5 · 3d ago\
         \n  web2  ✓ up to date  gen 5 · 3d ago\n\
         \n2 up to date · <took>\n"
    );
    let probed: Vec<String> = w
        .calls()
        .iter()
        .filter(|c| c.tool == "ssh")
        .map(|c| c.destination().to_string())
        .collect();
    assert_eq!(probed.len(), 2);
    assert!(probed.contains(&"web1".into()) && probed.contains(&"web2".into()));

    // This machine is probed without ssh.
    w.clear_log();
    let run = w.run(&["status", "--plain", "@local"]);
    run.code(0);
    assert_eq!(
        table(&run),
        "github:o/r @ 1111111 (2h ago)\n\
         \n  bigmem •  ✓ up to date  gen 9 · 3d ago\n\
         \n1 up to date · • this machine · <took>\n"
    );
    assert!(w.calls().iter().all(|c| c.tool == "nix"), "{}", w.log());

    // Overlapping patterns: each host once, in order.
    let json = w.run(&["status", "--json", "w*2", "@local", "web*"]).json();
    let names: Vec<&str> = json["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["bigmem", "web1", "web2"]);
    assert_eq!(json["hosts"][0]["local"], "local");
    assert_eq!(json["hosts"][0]["destination"], Value::Null);
}

#[test]
fn an_unreachable_optional_host_is_offline_and_exits_0() {
    let w = World::new();
    w.flake(REV1);
    discover_home(&w, &["web1"]);
    w.config(
        "[hosts.mac]\nkind = \"darwin\"\noptional = true\nnote = \"reachable from the desk only\"\n",
    );
    home_host(&w, "web1", &hm('a'), &hm('a'));
    w.eval(
        "darwinConfigurations.mac.system",
        &store('m', "darwin-system-26.05"),
    );
    w.host("mac")
        .unreachable("ssh: connect to host mac port 22: No route to host");
    let run = w.run(&["status", "--plain"]);
    run.code(0);
    assert_eq!(
        table(&run),
        "github:o/r @ 1111111 (2h ago)\n\
         \n  mac   – offline     reachable from the desk only\
         \n  web1  ✓ up to date  gen 5 · 3d ago\n\
         \n1 offline · 1 up to date · <took>\n"
    );
}

// ─── ssh ────────────────────────────────────────────────────────────────

#[test]
fn forced_command_routes_send_their_command() {
    let w = World::new();
    w.flake(REV1);
    w.config(
        "[hosts.mac]\nkind = \"darwin\"\ndestination = \"ssh://jj@localhost:2022\"\nprobe = \"current-system\"\n\
         [hosts.air]\nkind = \"darwin\"\ndestination = \"jj@air.lan\"\nprobe = \"current-system\"\n\
         [hosts.mini]\nkind = \"darwin\"\ndestination = \"jj@mini.lan\"\nprobe = \"current-system\"\n",
    );
    let mac = store('m', "darwin-system-26.05");
    w.eval("darwinConfigurations.mac.system", &mac);
    w.host("localhost").forced(&format!("{mac}\n"));
    // A receiver may answer with the probe's lines instead of a bare path.
    let air = store('a', "darwin-system-26.05");
    w.eval(
        "darwinConfigurations.air.system",
        &store('b', "darwin-system-26.05"),
    );
    w.host("air.lan")
        .forced(&format!("current={air}\ngeneration=12\nrevision={REV2}\n"));
    // One that doesn't know `current-system` at all.
    w.eval(
        "darwinConfigurations.mini.system",
        &store('n', "darwin-system-26.05"),
    );
    w.host("mini.lan");

    let run = w.run(&["status", "--plain"]);
    run.code(2);
    assert_eq!(
        table(&run),
        "github:o/r @ 1111111 (2h ago)\n\
         \n  air   ✗ outdated      gen 12 · on 2222222\
         \n  mac   ✓ up to date\
         \n  mini  ! probe failed  mac-open-recv: unknown mode\n\
         \n1 probe failed · 1 outdated · 1 up to date · <took>\n"
    );
    for (destination, end) in [
        ("jj@localhost", " -p 2022 -- jj@localhost current-system"),
        ("jj@air.lan", " -- jj@air.lan current-system"),
        ("jj@mini.lan", " -- jj@mini.lan current-system"),
    ] {
        let calls = w.ssh_calls(destination);
        assert_eq!(calls.len(), 1, "{}", w.log());
        assert!(calls[0].args.ends_with(end), "{}", calls[0].args);
    }
    // Nothing but the probe command runs over such a route.
    let json = w.run(&["status", "--json"]).json();
    assert_eq!(json["hosts"][0]["name"], "air");
    assert_eq!(json["hosts"][0]["activate"], Value::Null);
    let run = w.run(&["activate", "--yes", "air"]);
    run.code(2);
    assert!(
        run.stdout
            .contains("\nair: can't activate: air only answers its probe command\n"),
        "{run}"
    );
}

#[test]
fn a_probe_that_times_out_is_unreachable() {
    let w = World::new();
    w.flake(REV1);
    w.config("timeout = \"1s\"\n");
    discover_home(&w, &["web1", "web2"]);
    home_host(&w, "web1", &hm('a'), &hm('a')).delay(30.0);
    home_host(&w, "web2", &hm('b'), &hm('b'));
    let started = Instant::now();
    let run = w.run(&["status", "--plain"]);
    run.code(2);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "killed after the timeout, took {:?}",
        started.elapsed()
    );
    assert!(
        run.stdout
            .contains("\n  web1  ? unreachable  timed out after 1s\n"),
        "{run}"
    );
    assert!(run.stdout.contains("\n  web2  ✓ up to date  "), "{run}");
    let calls = w.ssh_calls("web1");
    assert_eq!(calls.len(), 1, "a timeout isn't retried: {}", w.log());
    assert_eq!(calls[0].end, None, "the probe was killed");
}

#[test]
fn a_silent_ssh_failure_is_retried_without_multiplexing() {
    let w = World::new();
    w.flake(REV1);
    discover_home(&w, &["web1"]);
    home_host(&w, "web1", &hm('a'), &hm('a'))
        .silent_255()
        .unreachable("ssh: connect to host web1 port 22: Connection refused");
    let run = w.run(&["status", "--plain"]);
    run.code(2);
    assert!(
        run.stdout.contains(
            "\n  web1  ? unreachable  ssh: connect to host web1 port 22: Connection refused\n"
        ),
        "the reason the retry captured: {run}"
    );
    let calls = w.ssh_calls("web1");
    assert_eq!(calls.len(), 2, "{}", w.log());
    assert!(
        calls[0]
            .args
            .contains(" -o ControlMaster=auto -o ControlPath="),
        "{}",
        calls[0].args
    );
    assert_eq!(calls[0].code, Some(255));
    assert!(
        calls[1]
            .args
            .contains(" -o ControlMaster=no -o ControlPath=none -o ControlPersist=no "),
        "{}",
        calls[1].args
    );
}

#[test]
fn a_proxy_failure_the_master_hides_is_retried_without_multiplexing() {
    let w = World::new();
    w.flake(REV1);
    discover_home(&w, &["web1"]);
    let host = home_host(&w, "web1", &hm('a'), &hm('a'));
    // What OpenSSH 9.9 says when its ProxyJump host doesn't resolve; with
    // ControlPersist, only the last line is left.
    let closed = "Connection closed by UNKNOWN port 65535";
    std::fs::write(host.dir.join("silent-255"), format!("{closed}\r\n")).unwrap();
    host.unreachable(&format!(
        "ssh: Could not resolve hostname jump: Name or service not known\r\n{closed}\r"
    ));
    let run = w.run(&["status", "--plain"]);
    run.code(2);
    assert!(
        run.stdout.contains(
            "\n  web1  ? unreachable  ssh: Could not resolve hostname jump: Name or service not known\n"
        ),
        "the reason the retry captured: {run}"
    );
    let calls = w.ssh_calls("web1");
    assert_eq!(calls.len(), 2, "{}", w.log());
    assert!(
        calls[0]
            .args
            .contains(" -o ControlMaster=auto -o ControlPath="),
        "{}",
        calls[0].args
    );
    assert!(
        calls[1]
            .args
            .contains(" -o ControlMaster=no -o ControlPath=none -o ControlPersist=no "),
        "{}",
        calls[1].args
    );
    let json = w.run(&["status", "--json"]).json();
    let detail = json["hosts"][0]["error"]["detail"].as_str().unwrap();
    assert!(
        detail.ends_with(&format!(
            "\nssh: Could not resolve hostname jump: Name or service not known\n{closed}\n\
             hint: jump doesn't resolve: set the host's destination (e.g. user@jump.example.com) \
             or give it a Host entry in ~/.ssh/config"
        )),
        "{detail}"
    );
}

#[test]
fn ssh_options_come_first_and_win() {
    let w = World::new();
    w.flake(REV1);
    discover_home(&w, &["web1"]);
    w.config(
        "ssh.options = [\"-o\", \"ConnectTimeout=3\"]\nssh.persist = \"0s\"\n\
         [hosts.web1]\nssh-options = [\"-o\", \"StrictHostKeyChecking=yes\"]\n",
    );
    home_host(&w, "web1", &hm('a'), &hm('a'));
    w.run(&["status"]).code(0);
    let call = &w.ssh_calls("web1")[0];
    assert_eq!(
        call.args,
        "-o StrictHostKeyChecking=yes -o ConnectTimeout=3 -T -o BatchMode=yes \
         -o ClearAllForwardings=yes -o ForwardAgent=no -o ForwardX11=no -o PermitLocalCommand=no \
         -o ControlMaster=no -o ControlPath=none -o ControlPersist=no -- web1 sh -s"
    );
}

// ─── diff ───────────────────────────────────────────────────────────────

const KIB: u64 = 1 << 10;
const MIB: u64 = 1 << 20;

/// web1 runs `hm('a')`, whose closure is in its store, and should run
/// `hm('b')`; returns the new closure (not stored anywhere yet).
fn diff_world() -> (World, Vec<(String, u64)>) {
    let w = World::new();
    w.flake(REV1);
    discover_home(&w, &["web1"]);
    let host = home_host(&w, "web1", &hm('a'), &hm('b'));
    let bash = store('5', "bash-5.2p37");
    host.closure(&[
        (&hm('a'), 4 * KIB),
        (&store('f', "firefox-131.0"), 100 * MIB),
        (&bash, MIB),
        (&store('h', "htop-3.3.0"), 512 * KIB),
    ]);
    let new = vec![
        (hm('b'), 4 * KIB),
        (store('g', "firefox-132.0"), 102 * MIB),
        (bash, MIB),
        (store('z', "zenoh-1.10.0"), 5 * MIB),
    ];
    (w, new)
}

fn borrowed(closure: &[(String, u64)]) -> Vec<(&str, u64)> {
    closure.iter().map(|(p, s)| (p.as_str(), *s)).collect()
}

const DIFF: &str = "web1: 4 → 4 paths · 101.5 MiB → 108.0 MiB (+6.5 MiB)
  firefox  131.0 → 132.0    +2.0 MiB  upgraded
  htop     3.3.0 → ∅      -512.0 KiB  removed
  zenoh    ∅ → 1.10.0       +5.0 MiB  added
";

#[test]
fn diff_lists_what_activating_would_change() {
    let (w, new) = diff_world();
    w.host("web1").closure(&borrowed(&new));
    let run = w.run(&["diff", "web1"]);
    run.code(0);
    assert_eq!(run.stdout, DIFF);
    assert_eq!(run.stderr, "");
    // The probe, then one script listing both closures on web1.
    let calls = w.ssh_calls("web1");
    assert_eq!(calls.len(), 2, "{}", w.log());

    let run = w.run(&["diff", "web1", "--json"]);
    run.code(0);
    assert_eq!(
        run.json(),
        json!({
            "version": 1,
            "host": "web1",
            "diff": {
                "changes": [
                    {
                        "name": "firefox", "removed": ["131.0"], "added": ["132.0"],
                        "inBefore": true, "inAfter": true,
                        "sizeBefore": 100 * MIB, "sizeAfter": 102 * MIB,
                    },
                    {
                        "name": "htop", "removed": ["3.3.0"], "added": [],
                        "inBefore": true, "inAfter": false,
                        "sizeBefore": 512 * KIB, "sizeAfter": 0,
                    },
                    {
                        "name": "zenoh", "removed": [], "added": ["1.10.0"],
                        "inBefore": false, "inAfter": true,
                        "sizeBefore": 0, "sizeAfter": 5 * MIB,
                    },
                ],
                "pathsBefore": 4,
                "pathsAfter": 4,
                "sizeBefore": 4 * KIB + 100 * MIB + MIB + 512 * KIB,
                "sizeAfter": 4 * KIB + 102 * MIB + MIB + 5 * MIB,
            },
        })
    );
}

#[test]
fn diff_says_when_the_new_generation_is_nowhere_yet() {
    let (w, new) = diff_world();
    let run = w.run(&["diff", "web1"]);
    run.code(2);
    assert_eq!(
        run.stderr,
        "error: the new generation of web1 isn't built anywhere yet: \
         `hostctl diff --build web1` builds it on web1\n"
    );
    assert_eq!(run.stdout, "");

    // This machine's store is asked too.
    w.local_closure(&borrowed(&new));
    let run = w.run(&["diff", "web1"]);
    run.code(0);
    assert_eq!(run.stdout, DIFF);
}

#[test]
fn diff_needs_one_host_that_could_be_checked() {
    let w = World::new();
    w.flake(REV1);
    discover_home(&w, &["web1", "web2"]);
    home_host(&w, "web1", &hm('a'), &hm('b'));
    home_host(&w, "web2", &hm('c'), &hm('d'))
        .unreachable("ssh: connect to host web2 port 22: Connection refused");
    let run = w.run(&["diff", "web*"]);
    run.code(3);
    assert_eq!(
        run.stderr,
        "error: diff needs exactly one host, but \"web*\" matches web1, web2\n"
    );
    assert!(
        w.ssh_calls("web1").is_empty(),
        "nothing was checked: {}",
        w.log()
    );

    let run = w.run(&["diff", "web2"]);
    run.code(2);
    assert!(
        run.stderr.starts_with(
            "error: web2's current generation is unknown (unreachable): \
             ssh: connect to host web2 port 22: Connection refused\n  $ "
        ),
        "{run}"
    );
}

#[test]
fn diff_build_builds_the_new_generation_on_the_host_first() {
    let (w, new) = diff_world();
    w.build(&hm_attr("web1"), &borrowed(&new));
    let command = format!(
        "nix --extra-experimental-features 'nix-command flakes' build --no-link \
         --print-out-paths --no-write-lock-file '{}#.homeConfigurations.web1.activationPackage'",
        World::locked(FLAKE, REV1)
    );
    let run = w.run(&["diff", "web1", "--build", "--json"]);
    run.code(0);
    // The build talks on stderr, so that stdout is just the JSON.
    assert_eq!(
        run.stderr,
        format!(
            "building the new generation of web1 on web1:\n  {command}\n{}\n",
            hm('b')
        )
    );
    assert_eq!(run.json()["diff"]["pathsAfter"], 4);
    assert_eq!(
        w.host("web1").interactive_script().unwrap(),
        format!("rm -f \"$0\"\n{PATH_PRELUDE}\n{command}\n")
    );
}

// ─── activate ───────────────────────────────────────────────────────────

/// What every command run on a host or interactively starts with.
const PATH_PRELUDE: &str = r#"PATH="/run/wrappers/bin:$PATH:/nix/var/nix/profiles/default/bin:/run/current-system/sw/bin:$HOME/.nix-profile/bin:${XDG_STATE_HOME:-$HOME/.local/state}/nix/profile/bin:/etc/profiles/per-user/${USER:-${LOGNAME:-}}/bin"; export PATH"#;

/// web1 runs generation 5, `hm('a')`; generation 6, `hm('b')`, is the
/// desired one and is built. Its activation switches the profile.
fn activation_world() -> (World, FakeHost) {
    let w = World::new();
    w.flake(REV1);
    discover_home(&w, &["web1"]);
    w.config(
        "[hosts.web1]\nactivate = 'ln -sfn home-manager-6-link \"$HOME/.local/state/nix/profiles/home-manager\" && echo activated {name} {out}'\n",
    );
    let host = home_host(&w, "web1", &hm('a'), &hm('b'));
    host.generation("~/.local/state/nix/profiles/home-manager", 6, &hm('b'));
    (w, host)
}

#[test]
fn activate_runs_the_command_on_the_host_and_checks_it_again() {
    let (w, host) = activation_world();
    let run = w.run(&["activate", "--yes", "web1"]);
    run.code(0);
    let command = format!(
        "ln -sfn home-manager-6-link \"$HOME/.local/state/nix/profiles/home-manager\" && \
         echo activated web1 {}",
        hm('b')
    );
    assert_eq!(
        masked(&run.stdout),
        format!(
            "github:o/r @ 1111111 (2h ago)\n\
             \n  web1  ✗ outdated  gen 5 · 3d ago · unknown generation\n\
             \n1 outdated · <took>\
             \nupdate: hostctl activate web1\n\
             \nweb1: {command}\n  runs on web1\n\
             \n==> activating web1 on web1\n\
             activated web1 {}\n\
             ✓ web1 is up to date\n",
            hm('b')
        )
    );
    assert_eq!(run.stderr, "");

    // Probe; upload (over the unattended channel); the run, with a
    // terminal, of exactly that command after the PATH prelude; probe again.
    let calls = w.ssh_calls("web1");
    let commands: Vec<&str> = calls.iter().map(Call::remote_command).collect();
    assert_eq!(commands.len(), 4, "{}", w.log());
    assert_eq!(commands[0], "sh -s");
    assert_eq!(commands[1], "sh -s");
    assert_eq!(commands[3], "sh -s");
    let uploaded = commands[2].strip_prefix("sh ").unwrap();
    assert!(
        uploaded.starts_with(&format!("{}/hostctl.", host.at("/tmp").display())),
        "a private temporary file: {uploaded}"
    );
    assert!(
        calls[2]
            .args
            .starts_with("-t -o ControlMaster=no -o ClearAllForwardings=yes -- web1 sh "),
        "{}",
        calls[2].args
    );
    assert_eq!(
        host.interactive_script().unwrap(),
        format!("rm -f \"$0\"\n{PATH_PRELUDE}\n{command}\n")
    );
    assert!(
        !std::path::Path::new(uploaded).exists(),
        "it deleted itself"
    );
}

#[test]
fn activate_without_a_terminal_needs_yes() {
    let w = World::new();
    w.flake(REV1);
    discover_home(&w, &["web1"]);
    home_host(&w, "web1", &hm('a'), &hm('b'));
    let run = w.run(&["activate", "web1"]);
    run.code(3);
    assert!(
        run.stdout.contains(&format!(
            "\nweb1: out=$(nix --extra-experimental-features 'nix-command flakes' build \
             --no-link --print-out-paths '{}'#.homeConfigurations.web1.activationPackage) && \
             [ \"$out\" = {} ] && \"$out/activate\"\n  runs on web1\n",
            World::locked(FLAKE, REV1),
            hm('b')
        )),
        "the plan shows the exact command: {run}"
    );
    assert!(
        run.stderr
            .starts_with("error: can't ask for confirmation (")
            && run
                .stderr
                .ends_with("): pass --yes to activate without asking\n"),
        "{run}"
    );
    assert_eq!(w.ssh_calls("web1").len(), 1, "only the probe: {}", w.log());
}

#[test]
fn activate_has_nothing_to_do_when_everything_is_up_to_date() {
    let w = World::new();
    w.flake(REV1);
    discover_home(&w, &["web1"]);
    home_host(&w, "web1", &hm('a'), &hm('a'));
    let run = w.run(&["activate", "web1"]);
    run.code(0);
    assert!(
        run.stdout
            .ends_with("\nnothing to activate: up to date (--force activates anyway)\n"),
        "{run}"
    );
}

#[test]
fn activating_this_machine_runs_here() {
    let w = World::new();
    w.flake(REV1);
    discover_home(&w, &["bigmem"]);
    w.config(
        "self = \"bigmem\"\n\
         [hosts.bigmem]\nactivate = 'ln -sfn home-manager-6-link \"$HOME/.local/state/nix/profiles/home-manager\"'\n",
    );
    w.eval(&hm_attr("bigmem"), &hm('b'));
    w.this_machine("bigmem")
        .home_manager(5, &hm('a'))
        .generation("~/.local/state/nix/profiles/home-manager", 6, &hm('b'));
    let run = w.run(&["activate", "-y", "@local"]);
    run.code(0);
    assert!(
        run.stdout.ends_with(
            "\nbigmem: ln -sfn home-manager-6-link \"$HOME/.local/state/nix/profiles/home-manager\"\n  \
             runs here\n\n==> activating bigmem here\n✓ bigmem is up to date\n"
        ),
        "{run}"
    );
    assert!(
        w.calls().iter().all(|c| c.tool == "nix"),
        "no ssh: {}",
        w.log()
    );
}

// ─── config, completions ────────────────────────────────────────────────

#[test]
fn config_show_prints_the_effective_configuration() {
    let w = World::new();
    w.config("[hosts.web1]\nkind = \"nixos\"\nnote = \"the web server\"\n");
    let run = w.run(&["config", "show", "--flake", "github:o/other", "-j", "3"]);
    run.code(0);
    for line in [
        "flake = \"github:o/other\"\n",
        "jobs = 3\n",
        "timeout = \"20s\"\n",
        "eval-timeout = \"10m\"\n",
        "discover = true\n",
        "inventory = \"lib.hostctl\"\n",
        "cache = true\n",
        "\n[hosts.web1]\nkind = \"nixos\"\nnote = \"the web server\"\n",
    ] {
        assert!(run.stdout.contains(line), "{line:?} in\n{run}");
    }
    assert!(
        run.stdout.contains(&format!(
            "command = {:?}\n",
            w.fake.join("bin/nix").display().to_string()
        )),
        "{run}"
    );
    // What it prints is a valid config.
    let shown = w.root().join("shown.toml");
    std::fs::write(&shown, &run.stdout).unwrap();
    w.run(&["config", "check", shown.to_str().unwrap()]).code(0);
}

#[test]
fn config_path_says_which_file_is_used_and_where_hostctl_looks() {
    let w = World::new();
    let file = w.config_file.display().to_string();
    let search = format!(
        "search order:\n  --config FILE\n  $HOSTCTL_CONFIG\n  {file}\n  /etc/hostctl/config.toml\n"
    );
    let run = w.run(&["config", "path"]);
    run.code(0);
    assert_eq!(run.stdout, format!("{file}\n{search}"));

    // $HOSTCTL_CONFIG (or --config) wins, even when it doesn't exist yet.
    let other = w.root().join("other.toml");
    w.env("HOSTCTL_CONFIG", Some(other.to_str().unwrap()));
    let run = w.run(&["config", "path"]);
    assert_eq!(run.stdout, format!("{}\n{search}", other.display()));
    let run = w.run(&["status"]);
    run.code(3);
    assert!(
        run.stderr.starts_with(&format!(
            "error: can't read config file {}",
            other.display()
        )),
        "{run}"
    );

    if !std::path::Path::new("/etc/hostctl/config.toml").exists() {
        w.env("HOSTCTL_CONFIG", None);
        std::fs::remove_file(&w.config_file).unwrap();
        let run = w.run(&["config", "path"]);
        assert_eq!(run.stdout, format!("none\n{search}"));
    }
}

#[test]
fn config_check_reads_nothing_but_the_file() {
    let w = World::new();
    let good = w.root().join("good.toml");
    std::fs::write(
        &good,
        "flake = \"github:o/r\"\n[hosts.web1]\nkind = \"nixos\"\ndestination = \"root@web1\"\n",
    )
    .unwrap();
    let bad = w.root().join("bad.toml");
    std::fs::write(&bad, "[hosts.web1]\nkind = \"nixos\"\ntimout = \"20s\"\n").unwrap();
    let check = |file: &std::path::Path| {
        let mut cmd = command(env!("CARGO_BIN_EXE_hostctl"));
        cmd.args(["config", "check"])
            .arg(file)
            .env_clear()
            .env("HOME", "/nonexistent");
        finish(cmd.spawn().unwrap())
    };
    let run = check(&good);
    run.code(0);
    assert_eq!(run.stdout, format!("ok: {}\n", good.display()));
    let run = check(&bad);
    run.code(3);
    assert!(
        run.stderr
            .starts_with(&format!("error: invalid config file {}: ", bad.display())),
        "{run}"
    );
    assert!(run.stderr.contains("timout"), "{run}");

    // Not even the config in use is read, however broken it is.
    w.config("bogus = 1\n");
    w.run(&["config", "check", good.to_str().unwrap()]).code(0);
    let run = w.run(&["config", "check"]);
    run.code(3);
    assert!(
        run.stderr.contains("bogus"),
        "without a file: the one in use\n{run}"
    );
}

#[test]
fn completions_for_bash_zsh_and_fish() {
    let w = World::new();
    // They don't need a valid config.
    w.config("bogus = 1\n");
    for shell in ["bash", "zsh", "fish"] {
        let run = w.run(&["completions", shell]);
        run.code(0);
        assert!(run.stdout.len() > 1000, "{shell}: {run}");
        for word in ["hostctl", "activate", "offline"] {
            assert!(run.stdout.contains(word), "{shell} completes {word}");
        }
    }
    w.run(&["completions", "tcsh"]).code(3);
}
