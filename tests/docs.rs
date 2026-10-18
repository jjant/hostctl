//! The README's examples are real: its `hostctl status` is what the binary
//! prints for the fleet it shows (against the fakes of tests/support).

mod common;

use common::*;
use serde_json::json;

const FLAKE: &str = "github:you/nix-config";
/// What desk runs: a revision three days old.
const OLD: &str = "1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b";
const NEW: &str = "42ea902131b93c04d098d3bcd02f29305b4bad7c";

/// The block after `$ hostctl status` in README.md.
fn readme_status() -> &'static str {
    let readme = include_str!("../README.md");
    let (_, after) = readme
        .split_once("$ hostctl status\n")
        .expect("README.md has a `$ hostctl status` example");
    after.split("```").next().unwrap()
}

#[test]
fn the_readme_status_example_is_what_hostctl_prints() {
    let w = World::new();
    w.raw_config(&format!(
        "{}self = \"desk\"\n[hosts.laptop]\noptional = true\nnote = \"reachable from the desk only\"\n",
        w.base_config().replace("github:o/r", FLAKE)
    ));
    w.discover(json!({ "nixos": ["desk", "nas"], "darwin": ["laptop"], "home": ["work"] }));
    let old = w.flake_at(FLAKE, OLD);
    let mut metadata: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&old.0).unwrap()).unwrap();
    let three_days = now() - 3 * 86400 - 2 * 3600;
    metadata["lastModified"] = json!(three_days);
    metadata["locked"]["lastModified"] = json!(three_days);
    std::fs::write(&old.0, metadata.to_string()).unwrap();

    let desk = "nixosConfigurations.desk.config.system.build.toplevel";
    let running = store('y', "nixos-system-desk-25.11");
    w.eval(desk, &running);
    w.this_machine("desk").system(&running, 59, &running);
    let nas = store('n', "nixos-system-nas-25.11");
    w.eval("nixosConfigurations.nas.config.system.build.toplevel", &nas);
    w.host("nas").system(&nas, 112, &nas);
    w.eval(
        "darwinConfigurations.laptop.system",
        &store('l', "darwin-system-25.11"),
    );
    w.host("laptop")
        .unreachable("ssh: connect to host laptop port 22: No route to host");
    w.eval(&hm_attr("work"), &hm('w'));
    w.host("work"); // reachable, no home-manager profile
    w.run(&["status"]).code(1);

    // The flake moved on; desk still runs the old revision (as the cache
    // remembers).
    w.flake_at(FLAKE, NEW);
    w.eval(desk, &store('q', "nixos-system-desk-25.11"));
    let run = w.run(&["status", "--plain", "--refresh"]);
    run.code(1);
    assert_eq!(run.stdout, readme_status(), "{run}");
}
