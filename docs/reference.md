# hostctl reference

The details the [README](../README.md) leaves out.

- [Command line](#command-line)
- [Statuses](#statuses)
- [Configuration](#configuration)
- [Inventory](#inventory)
- [Nix modules](#nix-modules)
- [JSON](#json)
- [Recipes](#recipes)
- [How it works](#how-it-works)
- [Security](#security)

## Command line

```text
hostctl [PATTERN…]              status (with its flags too)
hostctl status [PATTERN…]       check and print a table   [--json] [--plain]
hostctl diff HOST               what activating would change (closure diff)   [--build] [--json]
hostctl activate HOST…|--all    activate the compared generation   [--yes] [--force]
hostctl hosts [PATTERN…]        the resolved hosts, where they come from, how they're reached
hostctl config show|path|check [FILE]
hostctl completions bash|zsh|fish|elvish|powershell

  -c, --config FILE    config file ($HOSTCTL_CONFIG)
  -f, --flake FLAKE    flake to compare against ($HOSTCTL_FLAKE)
  -j, --jobs N         concurrent evaluations
      --self HOST      treat HOST as this machine ($HOSTCTL_SELF)
      --no-cache       don't read or write the result cache
      --refresh        re-resolve flakes now
      --offline        never refresh flakes or evaluate: cached results only (hosts are still probed)
      --color WHEN     auto (honours NO_COLOR), always, never
```

Patterns are globs over host names (`web*`, `al2-[xa]*`), plus `@local` for
this machine; a pattern that matches nothing is an error. On a terminal,
`status` redraws the table in place while it checks (without flooding tmux's
scrollback; `Ctrl-Z` suspends it) and ends with exactly what `--plain` prints,
no line cut. The `update: hostctl activate …` it suggests keeps the
`-c`/`--flake`/`--self` you passed. With `--offline`, hosts that have no cached
results show `not cached; run hostctl once without --offline`.

Shell completions (installed with the package; else `hostctl completions fish
> ~/.config/fish/completions/hostctl.fish` and the like) ask hostctl itself what
to complete, so they also offer the host names: the configured hosts and those
the last check found in the flake, read from the config and hostctl's cache,
without touching the network.

`diff` lists package changes the way `nix store diff-closures` groups them,
from both closures read on the host. When the desired generation exists
nowhere yet, `--build` builds or fetches it on the
host first, interactively and without activating. `activate` asks, runs the
activation with your terminal attached and re-checks the host. With several
hosts it first builds all their new generations at once, without a terminal,
wherever each activation runs, then activates them one at a time; a host whose
build fails isn't activated (activated alone, it builds on the terminal).
Before asking it shows the command when it comes from a template of yours or of
the flake's inventory; hostctl's own commands (the defaults below) are shown if
they fail, and `status --json` has every host's (`activate.command`).

## Statuses

From most to least urgent:

| status          | icon | meaning                                                                                     | exit |
| --------------- | ---- | ------------------------------------------------------------------------------------------- | ---- |
| `eval-failed`   | `!`  | the configuration doesn't evaluate                                                          | 2    |
| `probe-failed`  | `!`  | the host answered, but the answer was unusable                                              | 2    |
| `unreachable`   | `?`  | ssh failed or timed out before an answer                                                    | 2    |
| `outdated`      | `✗`  | the active generation isn't the desired one                                                 | 1    |
| `not-activated` | `○`  | reachable, but there is no generation at all                                                | 1    |
| `pending`       | `…`  | still being checked                                                                         | 2    |
| `offline`       | `–`  | unreachable, but the host is `optional`                                                     | 0    |
| `ahead`         | `↑`  | not the desired generation, but its generation records a commit made after the compared one | 1    |
| `up-to-date`    | `✓`  | the active generation is the desired one                                                    | 0    |

**Ahead** is for a branch being tried out: the host runs a commit made after
the compared one (by commit time, not history: once the compared branch gets a
later commit, the host is outdated again), as its generation records it. It
isn't in the `update:` suggestion, `hostctl activate` skips it unless
`--force`, and activating it warns that it goes back. It reads `on feat/x
(878d66c, 8h newer)` when `git ls-remote --heads` of the flake's repository
(with git on `PATH`) has a branch at that commit. To date a recorded revision it
hasn't evaluated, hostctl fetches that commit of the flake's repository
during the check (github, gitlab, sourcehut or git flakes; once per commit,
again an hour after a failure or on `--refresh`, never `--offline`); the host
shows `looking up its commit` meanwhile.

**Drift** says why an outdated host differs: `on <rev> (<age> old)` (the
revision it runs, recorded by the generation itself as `configurationRevision`
or remembered by hostctl from an earlier check), `on <rev> (<age> newer)` for
an ahead host, `same rev, different build`
(activated from a reference without `rev`, or with overridden inputs), `staged`
(the profile already points at the desired generation; a switch or reboot is
pending) or nothing when the revision is unknown (the generation doesn't
record it and hostctl hasn't evaluated one that builds it: a local checkout,
another branch). Badges: `⟳ reboot` (a
NixOS host booted another kernel, initrd or modules than it runs) and
`specialisation <name>`.

**Exit codes**: `0` everything is up to date (offline optional hosts allowed);
`1` some host is outdated, not activated or ahead; `2` trouble: a host is unreachable,
failed or still pending, a flake couldn't be refreshed, or another error — trouble
wins over outdated; `3` the check couldn't run (usage or config error, no host
selected, `nix` or `ssh` can't be run at all); `130` interrupted.

## Configuration

hostctl reads the first of `--config FILE`, `$HOSTCTL_CONFIG`,
`$XDG_CONFIG_HOME/hostctl/config.toml` (`~/.config/…`) and
`/etc/hostctl/config.toml`; with none, the defaults apply. Unknown keys are
errors. `hostctl config check FILE` validates a file without reading anything
else; `hostctl config show` prints the effective configuration.

The flake comes from `--flake`, else `$HOSTCTL_FLAKE`, else `flake`, else `.`
when `./flake.nix` exists (with no config file, messages then say the flake
came from `./flake.nix`). Flakes starting with `-` are rejected. Durations are
written `"500ms"`, `"20s"`, `"10m"`, `"1h30m"`, `"2d"` or as a number of
seconds.

```toml
flake = "github:you/nix-config"
exclude = ["*-iso"]  # say, installer images among the nixosConfigurations

[ssh]
options = ["-o", "StrictHostKeyChecking=yes"]

[defaults]
destination = "{name}.lan"

[hosts.laptop]
kind = "darwin"
destination = "ssh://you@localhost:2022"
probe = "current-system"
optional = true
note = "reachable from the desk only"

[hosts.web1]
destination = "deploy@web1.example.com"
activate = "nixos-rebuild switch --flake {flake}#{name} --target-host {destination} --sudo"
activate-on = "local"
```

### Top level

| key            | default                  | meaning                                                                                      |
| -------------- | ------------------------ | -------------------------------------------------------------------------------------------- |
| `flake`        |                          | the flake every host is compared against (`--flake`, `$HOSTCTL_FLAKE` override it)           |
| `self`         |                          | force which host is this machine (`--self`, `$HOSTCTL_SELF` override it)                     |
| `jobs`         | min(CPUs, available GiB) | concurrent evaluations (`-j`); each takes 0.6–0.9 GB                                         |
| `timeout`      | `"20s"`                  | per probe                                                                                    |
| `eval-timeout` | `"10m"`                  | per evaluation; `0` = none                                                                   |
| `discover`     | `true`                   | add the flake's `nixosConfigurations`, `darwinConfigurations`, `homeConfigurations` as hosts |
| `inventory`    | `"lib.hostctl"`          | flake attribute holding `{ hosts.<name> = { … }; }` ([Inventory](#inventory)); `""` disables |
| `exclude`      | `[]`                     | globs of host names to drop (installer ISOs, CI configurations)                              |
| `cache`        | `true`                   | reuse the results of unchanged flakes (`--no-cache`)                                         |

| key                   | default | meaning                                                                                                                  |
| --------------------- | ------- | ------------------------------------------------------------------------------------------------------------------------ |
| `nix.command`         | `"nix"` | the nix executable                                                                                                       |
| `nix.options`         | `[]`    | extra arguments for every nix invocation (`--impure` disables the cache; discovery stays pure)                           |
| `nix.read-only`       | `true`  | evaluate with `--read-only` first (retried without it on import-from-derivation)                                         |
| `nix.refresh`         | `"60s"` | re-resolve a flake's branch when the last resolution is older than this; `0` = on every check                            |
| `ssh.command`         | `"ssh"` | the ssh executable                                                                                                       |
| `ssh.options`         | `[]`    | extra arguments for every ssh run; they win over hostctl's own (with `-F FILE`, ssh reads FILE instead of ~/.ssh/config) |
| `ssh.persist`         | `"10m"` | lifetime of hostctl's private ControlMaster connections; `0` = no multiplexing                                           |

### Hosts

`[hosts.<name>]` tables (keyed by configuration name), `[defaults]` (applied to
every host) and inventory entries take these keys, except that an inventory
can't set `probe` or `ssh-options` ([Inventory](#inventory)):

| key           | meaning                                                                                                                                             |
| ------------- | --------------------------------------------------------------------------------------------------------------------------------------------------- |
| `kind`        | `nixos`, `darwin`, `home-manager` or `custom`; default: the output the host was discovered in, else `custom` when `attr` is set                     |
| `destination` | `host`, `user@host`, `ssh://[user@]host[:port]` (IPv6 as `[::1]`); default: the name, if it looks like `[user@]host`; `[defaults]` may use `{name}` |
| `flake`       | compare this host against another flake                                                                                                             |
| `attr`        | the attribute whose out path is the desired generation (absolute, evaluated as `#.<attr>`); required for `custom`                                   |
| `profile`     | the profile symlink to read instead of the kind's (absolute or `~/…`); required for `custom` unless `probe` is set                                  |
| `probe`       | a [forced-command route](#laptop-behind-a-reverse-tunnel): send this command instead of the probe script                                            |
| `activate`    | the activation command template, or `false` / `""` to disable                                                                                       |
| `activate-on` | `"target"` (default: run on the host) or `"local"` (run here, for push tools)                                                                       |
| `hostnames`   | extra names that identify this host as the local machine                                                                                            |
| `ssh-options` | extra ssh arguments for this host (before `ssh.options`)                                                                                            |
| `optional`    | being unreachable is expected: shown as `offline`, not trouble                                                                                      |
| `note`        | free text shown next to the host's status                                                                                                           |

What each kind compares, and how it activates by default:

| kind           | desired (`attr`)                                       | current (read on the host)                                                                                              | default `activate`                                                              |
| -------------- | ------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------- |
| `nixos`        | `nixosConfigurations.<n>.config.system.build.toplevel` | `/run/current-system` (a specialisation counts as its parent)                                                           | `/run/current-system/sw/bin/nixos-rebuild switch --flake {flake}#{name} --sudo` |
| `darwin`       | `darwinConfigurations.<n>.system`                      | `/run/current-system`                                                                                                   | `sudo /run/current-system/sw/bin/darwin-rebuild switch --flake {flake}#{name}`  |
| `home-manager` | `homeConfigurations.<n>.activationPackage`             | `${XDG_STATE_HOME:-~/.local/state}/nix/profiles/home-manager`, then `/nix/var/nix/profiles/per-user/$USER/home-manager` | builds `{flake}#.{attr}`, checks it is `{out}`, runs its `activate`             |
| `custom`       | `attr`                                                 | `profile`                                                                                                               | none                                                                            |

Activation templates are plain shell with these placeholders, substituted
shell-quoted: `{flake}` (the locked URL that was compared), `{flake_url}` (as
configured), `{rev}`, `{name}`, `{attr}`, `{out}` (the desired store path),
`{destination}` and `{ssh_host}`. Every default uses `{flake}`, so activating
produces exactly the generation that was compared. hostctl's own builds
(home-manager's default, `diff --build`) pass `--accept-flake-config`, so the
flake's `nixConfig` (say, its binary cache) applies without nix asking about
each setting. Flakes with an `activate` app can use `activate = "nix run
{flake}#activate"`.

The local machine: `self` / `--self` if set; otherwise the host whose
destination or `hostnames` match this machine's hostname (a dot-less name
matches the first label of a dotted one), never one whose destination has
another user, a non-default port or a loopback address. On a Mac with no match,
the only `darwin` host without a remote destination is assumed local.

## Inventory

Hosts are merged field by field from, in order (later wins): discovery → the
flake's inventory → `[defaults]` → `[hosts.<name>]` → the command line. The
inventory is an attribute of the flake itself (`lib.hostctl` by default), so
how to reach each host is versioned with the configuration and every machine
and teammate sees the hosts of the revision being compared. It is read in the
same pure evaluation as discovery, and cached with the flake's results.

An inventory takes the keys of [`[hosts.<name>]`](#hosts) except `probe` and
`ssh-options`: those would run commands on the host or on your machine, and
the flake being checked needn't be yours, so they come only from your config
(hostctl ignores them in an inventory, with a warning).

If your flake already generates its configurations from a registry of
machines, generate the inventory from it too:

```nix
# hosts/default.nix
{
  laptop = { os = "darwin"; arch = "aarch64"; };
  desk = { os = "linux"; arch = "x86_64"; hostname = "desk.example.com"; };
  build = { os = "linux"; arch = "aarch64"; hostname = "build.example.com"; };
}
```

```nix
# flake.nix, next to the darwinConfigurations / homeConfigurations built from it
lib.hostctl.hosts = builtins.mapAttrs (
  _: host:
  if host.os == "darwin" then
    {
      destination = "ssh://you@localhost:2022";
      optional = true;
      note = "reachable from a desk you ssh into from the laptop";
    }
  else
    { destination = host.hostname; }
) (import ./hosts);
```

The laptop's `probe = "current-system"` then goes in your config
([below](#laptop-behind-a-reverse-tunnel)). (With flake-parts, the inventory
is `flake.lib.hostctl.hosts`.) `hostctl hosts` shows the result: each host,
its kind, how it is reached, which layers define it and what is evaluated.

## Nix modules

- **Package**: `inputs.hostctl.packages.${system}.default`, or
  `inputs.hostctl.overlays.default` for `pkgs.hostctl`. Note that nixpkgs
  already has an unrelated `hostctl` (an `/etc/hosts` manager), which the
  overlay replaces. `nix/package.nix` is a plain `callPackage` file, if you'd
  rather build it with your own nixpkgs.
- **Modules**: `homeModules.default` (home-manager), `nixosModules.default` and
  `darwinModules.default`. They install hostctl (built with your
  configuration's `pkgs`) and write its config file, which is checked with
  `hostctl config check` at build time, so a misspelled key fails the build
  instead of every later run.

```nix
{ inputs, ... }:
{
  imports = [ inputs.hostctl.homeModules.default ];

  programs.hostctl = {
    enable = true;
    settings = {
      flake = "github:you/nix-config";
      exclude = [ "*-iso" ]; # say, installer images among the nixosConfigurations
      hosts.laptop = {
        kind = "darwin";
        destination = "ssh://you@localhost:2022";
        probe = "current-system";
        optional = true;
      };
    };
  };
}
```

| option                      | default                | meaning                                                                                                                                                                                |
| --------------------------- | ---------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `programs.hostctl.enable`   | `false`                | install hostctl                                                                                                                                                                        |
| `programs.hostctl.package`  | built with your `pkgs` | the package to install                                                                                                                                                                 |
| `programs.hostctl.settings` | `{ }`                  | `config.toml` as Nix ([Configuration](#configuration)); written to `$XDG_CONFIG_HOME/hostctl/config.toml` (home-manager) or `/etc/hostctl/config.toml` unless empty; nulls are dropped |
| `programs.hostctl.validate` | `true`                 | check the generated file at build time                                                                                                                                                 |

`settings` is freeform: `flake` and `hosts.<name>.kind` are typed, everything
else is passed through and left to `hostctl config check`.

## JSON

`hostctl status --json` prints one object (schema version 1; absent values are
`null`, never omitted):

```json
{
  "version": 1,
  "summary": { "total": 4, "upToDate": 1, "outdated": 2, "ahead": 0, "trouble": 0, "pending": 0, "offline": 1 },
  "flakes": [
    {
      "flake": "github:you/nix-config",
      "locked": "github:you/nix-config/42ea902…?narHash=sha256-…",
      "rev": "42ea902…",
      "lastModified": 1792240011,
      "stale": null,
      "lockOutdated": false,
      "error": null
    }
  ],
  "hosts": [
    {
      "name": "desk",
      "kind": "nixos",
      "local": "local",
      "destination": null,
      "flake": "github:you/nix-config",
      "attr": "nixosConfigurations.desk.config.system.build.toplevel",
      "status": "outdated",
      "drift": { "type": "revision", "rev": "1a2b3c4…", "lastModified": 1791980811, "fromGeneration": false, "newer": false, "branches": [] },
      "current": "/nix/store/ykn4vz9v…-nixos-system-desk",
      "desired": "/nix/store/wqnds0yy…-nixos-system-desk",
      "cached": true,
      "generation": 59,
      "activated": 1791980000,
      "platform": "x86_64-linux",
      "revision": null,
      "specialisation": null,
      "booted": "/nix/store/ykn4vz9v…-nixos-system-desk",
      "rebootRequired": false,
      "note": null,
      "error": null,
      "activate": { "command": "/run/current-system/sw/bin/nixos-rebuild switch --flake …", "runsOn": "here" },
      "timings": { "probeMs": 12, "evalMs": null }
    }
  ],
  "errors": []
}
```

`summary.outdated` counts outdated and not-activated hosts; `summary.ahead`
the ahead ones. `local` is
`"local"`, `"assumed"` or `null` (remote). `drift` is `null` unless the host
is outdated; its `type` is `revision`, `same-revision`, `staged` or `unknown`,
and a `revision` drift also has the deployed `rev`, its `lastModified` (`null`
unless hostctl knows it), `fromGeneration` (`true` when the generation records
the revision itself, `false` when hostctl's cache remembered it) and `newer`
(`true` when the generation's own revision was committed after the compared
one: the host is `ahead`; `false` when that isn't known) and `branches` (those
whose head that revision is). `activate` is `null` when hostctl has no activation for the
host, else the exact `command` and where it runs, `runsOn`: `"here"` (this
machine) or `"remote"` (on the host, over ssh). `error` is `{ "summary",
"detail" }`. `activated` and `lastModified` are Unix seconds. `hostctl diff
--json` prints `{ "version": 1, "host", "diff" }` with every change, its
versions and sizes.

## Recipes

### Laptop behind a reverse tunnel

A laptop that nothing can reach, but from which you ssh into a desk with
`RemoteForward 2022 localhost:22`, can still be checked from that desk. On
the laptop, let the desk's key run nothing but a receiver that answers
`current-system`:

```text
# laptop: ~/.ssh/authorized_keys
restrict,from="127.0.0.1,::1",command="/usr/local/bin/hostctl-receiver" ssh-ed25519 AAAA… desk
```

```sh
#!/bin/sh
# /usr/local/bin/hostctl-receiver
case "$SSH_ORIGINAL_COMMAND" in
  current-system) exec /usr/bin/readlink /run/current-system ;;
  *) echo "unsupported" >&2; exit 1 ;;
esac
```

```toml
[hosts.laptop]
kind = "darwin"
destination = "ssh://you@localhost:2022"
probe = "current-system"
optional = true
note = "reachable from a desk you ssh into from the laptop"
```

The answer may be just a store path, or the probe's `key=value` lines
(`current=…`, `generation=…`, `revision=…`) for more detail. Over such a route
hostctl can't diff or activate; on the laptop itself the host is local and
everything works as usual.

### Push deployments

Run the activation here instead of on the host. `{flake}` is the locked URL
that was compared, so the tool deploys exactly that revision:

```toml
[hosts.web1]                 # nixos-rebuild
destination = "deploy@web1.example.com"
activate = "nixos-rebuild switch --flake {flake}#{name} --target-host {destination} --sudo"
activate-on = "local"

[defaults]                   # deploy-rs, for every host
activate = "deploy {flake}#{name}"
activate-on = "local"
```

colmena deploys the hive in the working directory rather than a flake
reference; use `activate = "colmena apply --on {name}"` with
`activate-on = "local"` and run hostctl from that checkout with `flake = "."`.

### Shell prompt

Mark the prompt when this machine is behind. `--offline` never refreshes the
flake or evaluates, it only uses what earlier runs cached (without that, the
host shows `not cached; run hostctl once without --offline`), and `@local` is
probed without ssh, so this costs a local probe and nothing else:

```sh
# bash, or zsh with `setopt prompt_subst`
hostctl_prompt() {
  [ "$(hostctl status --offline --json @local 2>/dev/null | jq '.summary.outdated')" = 1 ] && printf '✗ '
}
PS1='$(hostctl_prompt)'"$PS1"
```

```fish
function fish_right_prompt
    if test "$(hostctl status --offline --json @local 2>/dev/null | jq '.summary.outdated')" = 1
        set_color yellow; echo -n '✗ outdated'; set_color normal
    end
end
```

## How it works

A host is **up to date** when the store path of its active generation equals
the out path its configuration evaluates to at the flake's **locked** URL
(`nix flake metadata`'s `url`): activating that revision would change nothing.
Only evaluation is involved; nothing is built.

1. **Probe, from t = 0.** A small POSIX sh script, fed on stdin to `sh -s`
   (locally or over ssh, so the remote login shell never matters), reports the
   active generation, the profile it came from, its generation number and
   activation time, the recorded revision, and on NixOS the booted system and
   active specialisation. Its output is framed by a per-run nonce, so a
   truncated answer is never mistaken for one.
2. **Resolve each flake once**, in parallel with the probes:
   `nix flake metadata --json`, reused from hostctl's cache for `nix.refresh`
   (60 s: GitHub's own cache lifetime, and its unauthenticated API allows 60
   requests an hour). If refreshing fails, the last good resolution is used and
   marked stale.
3. **Evaluate each host's configuration** in its own `nix eval` process
   (shared by hosts with the same attribute), at most `jobs` at a time, longest
   first, as soon as its flake is resolved.
4. **Show each host as soon as** both its current and desired paths are known.

What makes it fast:

- **Parallel processes, not one big eval.** One `nix eval --apply mapAttrs`
  over every configuration is single-threaded; one process per host uses
  every core, bounded by available memory.
- **`--read-only --no-eval-cache --no-write-lock-file --option tarball-ttl
  4294967295`.** No `.drv` writes; no SQLite contention on Nix's eval cache
  (hostctl has its own); never touching your repository; and no re-resolving of
  already-locked inputs, which Nix otherwise does inside a per-input lock once
  its 1 h TTL lapses, serializing parallel evaluations.
- **A result cache** keyed by the Nix implementation and version, `nix.options`
  and the flake's content identity: an unchanged flake costs no evaluation.
- **A private ssh ControlMaster** (`ssh.persist`): repeated probes cost ~20 ms
  instead of a new connection, without touching your own ssh multiplexing. It
  sends keepalives (`ServerAliveInterval=5`, `ServerAliveCountMax=2`), so a
  dead connection is noticed in seconds; a probe that times out over it is
  retried once without it, and so is one whose ProxyCommand failure the master
  hid (`Connection closed by UNKNOWN port 65535`), to show the real reason.

Measured on a 128-core machine with 5 hosts and Nix 2.35.3:

| step                                                           | time / RSS                          |
| -------------------------------------------------------------- | ----------------------------------- |
| the shell script this replaced (2 × `nix eval` over all hosts) | 11.7 s                              |
| one `nix eval --read-only` per host, all in parallel           | 5.0–5.4 s                           |
| a single evaluation, normal / `--read-only` (warm)             | 3.9 s / 2.9–3.3 s                   |
| peak RSS per evaluation: home-manager / nix-darwin / NixOS     | 0.6 / 0.9 / 0.64 GB                 |
| 64 parallel evaluations, default / with `tarball-ttl` maxed    | 24.2 s / 4.6 s                      |
| `nix flake metadata --json --refresh`, warm / cold             | 0.05–0.3 s / 1.3 s                  |
| discovery + inventory (warm)                                   | 0.07 s                              |
| ssh probe: new connection / private master / via a tunnel      | 0.25–0.48 s / 20 ms / 0.95 → 0.25 s |

A warm `hostctl status` on an unchanged flake takes about as long as the
slowest probe, a quarter of a second.

## Security

- hostctl only reads, except for the activations and builds you confirm (or
  pass `--yes` to). It runs `nix`, `ssh`, `sh` and your activation templates,
  nothing else.
- Discovery and inventory are pure evaluations of the locked flake, even with
  `--impure` in `nix.options`: a flake can't read your environment into host
  names or commands. An inventory can't set `probe` or `ssh-options`.
- Unattended ssh runs with `BatchMode=yes`, `ConnectTimeout=10`,
  `ClearAllForwardings=yes`, `ForwardAgent=no`, `ForwardX11=no` and
  `PermitLocalCommand=no`. New host keys are accepted on first contact
  (`StrictHostKeyChecking=accept-new`), changed ones are refused; put
  `-o StrictHostKeyChecking=yes` in `ssh.options` to require known hosts.
- The ControlMaster sockets live in a private directory (`0700`, under
  `$XDG_RUNTIME_DIR`, else `~/.cache`), so your interactive `ssh` never rides
  hostctl's forwarding-less connections.
- Scripts travel on stdin, never on a remote command line; placeholders are
  shell-quoted; destinations are parsed and passed after `--`; destinations and
  flakes starting with `-` are rejected. Interactive commands are uploaded into
  a private temp file (`umask 077`, `mktemp`) that deletes itself.
- What a host says is only data: its revision is used to date a commit, by
  fetching that commit's tree from the compared flake's repository (no
  evaluation, no inputs). A host can name any commit that repository serves
  (on GitHub, also its forks' commits), so a host that lies about its
  revision can make itself look ahead, but no more. Branch names come from
  `git ls-remote` of that repository, which never prompts for credentials.
- Everything a host or flake says (names, paths, errors) is stripped of
  terminal control sequences before it is displayed.
- The cache (`$XDG_CACHE_HOME/hostctl/cache.json`) holds store paths and
  revisions, never credentials.
- For forced-command routes, pin the key with `restrict`, `from=` and
  `command=` as above, and keep the receiver to a fixed vocabulary.
