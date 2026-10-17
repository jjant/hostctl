# hostctl — design (v2, after design review)

`hostctl` answers one question fast: **is every host running the configuration
its flake currently describes?** It started as a `hosts status` shell script
and is now a standalone Rust tool with a TUI, for NixOS, nix-darwin and
standalone home-manager hosts.

Non-goals: being a deployment orchestrator (deploy-rs, colmena), a daemon, or an
SSH library. hostctl shells out to the user's `nix` and `ssh`, so their
configuration (ProxyCommand, agents, certificates, substituters, access tokens)
just works.

## Definition of "up to date"

A host is up to date when the store path of its **active generation** equals the
out path its configuration evaluates to at the flake's **locked** URL (the `url`
of `nix flake metadata`) — i.e. activating `{flake}` would change nothing.
Evaluation only; nothing is built. Configurations that embed `self.rev` /
`lastModified` (e.g. nix-darwin's `configurationRevision`) only compare equal
when activated from a reference carrying the same values; every default
activation template uses `{flake}` for that reason.

| kind           | desired (`attr`, absolute: evaluated as `#.<attr>`)    | current (read on the host)                                     |
| -------------- | ------------------------------------------------------ | -------------------------------------------------------------- |
| `nixos`        | `nixosConfigurations.<n>.config.system.build.toplevel` | `/run/current-system` (specialisations map back to their parent) |
| `darwin`       | `darwinConfigurations.<n>.system`                      | `/run/current-system`                                          |
| `home-manager` | `homeConfigurations.<n>.activationPackage`             | `${XDG_STATE_HOME:-~/.local/state}/nix/profiles/home-manager`, then `/nix/var/nix/profiles/per-user/$USER/home-manager` |
| `custom`       | required `attr`                                        | required `profile` (a symlink)                                 |

Statuses (most urgent first, which is also the "sort by status" order):

| status          | icon | meaning                                                         | exit |
| --------------- | ---- | --------------------------------------------------------------- | ---- |
| `eval-failed`   | `!`  | the configuration doesn't evaluate                              | 2    |
| `probe-failed`  | `!`  | reachable, but the answer was unusable                          | 2    |
| `unreachable`   | `?`  | ssh failed / timed out before answering                         | 2    |
| `outdated`      | `✗`  | active ≠ desired                                                | 1    |
| `not-activated` | `○`  | reachable, no generation at all                                 | 1    |
| `pending`       | spin | still being checked                                             | 2    |
| `offline`       | `–`  | unreachable, but the host is `optional`                         | 0    |
| `up-to-date`    | `✓`  | active = desired                                                | 0    |

Outdated hosts get a **drift** qualifier: `on <rev> (<age> old)` (deployed
revision known from the generation itself — `configurationRevision` — or from
hostctl's cache), `same rev, different build` (revision equal, paths differ:
activated from a flake reference without `rev`, or with overridden inputs),
`staged` (the profile already points at the desired generation; switch/reboot
pending), or `unknown generation` (not built from a revision hostctl has
checked: a local checkout, another branch). Badges: `⟳ reboot` (NixOS booted
kernel/initrd/modules differ), `specialisation <name>`.

Exit codes: `0` all up to date (offline optional hosts allowed), `1` some
outdated / not activated, `2` trouble (unreachable, failed, pending, stale flake,
global error) — trouble wins, `3` the check couldn't run (usage or config error,
no host selected, nix or ssh can't be run at all), `130` interrupted.

## Measurements (bigmem, 5 hosts, nix 2.35.3; design review re-measured)

| step                                                           | time / RSS          |
| -------------------------------------------------------------- | ------------------- |
| shell script: 2 × `nix eval --apply mapAttrs` (home + darwin)   | 11.7 s              |
| one `nix eval --read-only` **per host**, all in parallel        | 5.0–5.4 s           |
| single eval normal / `--read-only` (warm)                       | 3.9 s / 2.9–3.3 s   |
| per-eval peak RSS: home-manager / darwin / minimal NixOS        | 0.6 / 0.9 / 0.64 GB |
| 64 parallel evals, default / with `--option tarball-ttl` max    | 24.2 s / 4.6 s      |
| `nix flake metadata --json --refresh` (warm / cold)             | 0.05–0.3 / 1.3 s    |
| discovery + inventory (`#.` + `--apply`), warm                  | 0.07 s              |
| ssh probe: new connection / private master / Mac via tunnel     | 0.25–0.48 s / 20 ms / 0.95→0.25 s |

Consequences:

1. **One `nix eval` process per host** (deduplicated per `(flake, attr)`),
   bounded by `jobs` (default `min(CPUs, MemAvailable / 1 GiB)`), longest
   evaluations first (durations remembered in the cache).
2. Evals run with `--read-only` (no `.drv` writes; retried without it on
   import-from-derivation, remembered per host), `--no-eval-cache` (our own
   cache; avoids SQLite contention), `--no-write-lock-file` (never touch the
   user's repo) and `--option tarball-ttl 4294967295` (pure evaluation only
   fetches locked inputs; without it every eval re-resolves rev-pinned
   `fetchGit` refs *inside Nix's per-input fetcher lock*, serializing parallel
   evals once the 1 h TTL lapses).
3. **Result cache** keyed by the Nix implementation + options + flake identity:
   an unchanged flake costs zero evals.
4. **Probes start at t=0**, concurrently with flake resolution, over a
   **hostctl-private ssh ControlMaster** (`ssh.persist`, default 10 min), so
   repeated runs and re-probes cost ~20 ms per host.
5. **Flake resolution is cached for `nix.refresh` (60 s)** — the GitHub API's
   own max-age and its unauthenticated quota is 60 requests/hour — and falls back
   to the last good resolution (marked stale) when refreshing fails.
6. **Pipelined results**: each host's status is shown as soon as both its
   current and desired paths are known.

Warm `hostctl status` ≈ max(metadata-or-cached ≤ 0.3 s, slowest probe ≈ 0.25 s).

## Architecture

```
 cli ─► Config::load + overrides ─► (inventory? Engine::discover) ─► resolve_hosts ─► [Host]
                                                                                  │
          AppState ◄── Event ── Engine::check(hosts) ── probes ∥ flake metadata → evals (≤ jobs)
             │                        ▲                    (runner: exec::run, injectable)
             ├─► tui (fullscreen dashboard)     Engine::{probe, diff, prepare_interactive}
             ├─► live status table (redraw in place on a TTY)
             └─► plain text / JSON
```

- `engine` sends `model::Event`s over an unbounded tokio channel; front-ends
  fold them into `model::AppState` with `apply` (pure, unit-tested) and render.
- Every process is a `remote::CommandSpec` run by the engine's **runner**
  (`exec::run` in production, a scripted fake in tests with paused tokio time).
- One check at a time. Cancellation = dropping the check future (all its work is
  inside it — no detached spawns); children are killed with their whole process
  group. Interactive actions cancel a running check first.
- Discovery is a pre-step (`Engine::discover`), not part of a check: the host
  list is fixed before checking, `HostId` is an index.

### Modules

| module         | responsibility                                                                 |
| -------------- | ------------------------------------------------------------------------------ |
| `util`         | durations, ages, sizes, glob, base64/OSC 52, truncation, terminal sanitizing   |
| `storepath`    | store path parsing; Nix `DrvName` split + `compareVersions`                     |
| `closure`      | closure diff (same grouping as `nix store diff-closures`)                       |
| `config`       | TOML schema, loading, overrides, validation, resolution into `Host`s            |
| `local`        | which hosts are this machine                                                   |
| `probe`        | builtin POSIX-sh probe script + framed output parser                           |
| `remote`       | `CommandSpec`, quoting, ssh argv (mux, destinations), PATH prelude             |
| `exec`         | process runner (setsid, process-group kill, caps), interactive runner          |
| `nix`          | nix command builders, metadata/eval/inventory parsing, stderr cleanup          |
| `cache`        | on-disk cache: out paths, resolutions, inventories, eval hints, reverse lookup |
| `model`        | `Event`, `AppState`, status/drift derivation, summary, exit code               |
| `engine`       | orchestration (parallel probes/resolution/evals), discover, diff, uploads      |
| `actions`      | activation commands, copy text, closure/build scripts                          |
| `output`       | plain table, JSON report structs, diff text, hosts listing                     |
| `live`         | `status` on a terminal: in-place redraw of the plain table                     |
| `tui`          | fullscreen dashboard (`app` = pure state machine, `view` = rendering)          |
| `cli`          | clap definitions and dispatch                                                  |

## Probing the current generation

The builtin probe is a POSIX sh script fed on **stdin** to `sh -s` (locally, or
`ssh … sh -s`), so the remote login shell's syntax never matters. Output is
framed by a per-run nonce so truncated output is never mistaken for an answer:

```
hostctl-probe=1 nonce=8f3a1c
platform=x86_64-linux
current=/nix/store/…-home-manager-generation      # or: none=<why>  (never activated)
toplevel=/nix/store/…           # nixos: parent toplevel when a specialisation is active
specialisation=<name>
profile=/nix/store/…            # what the profile points at (may differ from current)
generation=59                   # the profile's generation number
activated=1792238369            # lstat mtime of the profile / current-system link
revision=42ea902…               # configurationRevision (darwin-version.json, nixos-version, or $current/configuration-revision)
booted=/nix/store/…             # nixos
reboot=1                        # nixos: booted kernel/initrd/modules differ
end=8f3a1c
```

Semantics: `profile=` appears only when the profile points somewhere other than
the deployed generation (so its presence means a switch/reboot is pending);
`generation` is the *deployed* generation's number (after `nixos-rebuild boot`,
the newest `system-N-link` pointing at the running system; for a specialisation,
its parent's). A configured `profile` replaces the default locations.

Portable (dash, bash --posix, busybox sh, ksh, macOS sh): `uname`, `readlink`
(bounded loop, no `-f`), `stat -c %Y` / `stat -f %m`, `sed`, `test -ef` for the
generation/specialisation scans (no processes), `id -un` only when `$USER` is
unset; `set +efu` guards against an exported `SHELLOPTS`. All absolute paths are
prefixed with `$HOSTCTL_PROBE_ROOT` (empty in production; absolute link targets
are moved inside it and reported paths are stripped of it) so tests run the real
script against fake trees. Measured: 5–12 ms for home-manager; 4 processes.

Classification of a probe run: complete framed output → use it, whatever the
exit code; framed but incomplete → `probe-failed` ("connection dropped / timed
out after connecting"); no output and (spawn error, exit 255, timeout) →
`unreachable`; anything else → `probe-failed`.

**Forced-command routes**: `probe = "current-system"` is a property of the ssh
route (e.g. the desk→Mac reverse tunnel whose forced command only answers
`current-system`): the raw command is sent instead of the script, its output may
use the protocol lines without framing or just print a store path, and remote
diff/activation over that route are unavailable. When such a host is the local
machine, the builtin probe and normal activation apply.

## SSH

Destinations (`host`, `user@host`, `ssh://[user@]host[:port]`, `[v6]`) are
parsed by hostctl and passed as `-p PORT -- user@host` (OpenSSH < 7.7 can't parse
`ssh://` URIs, and RHEL 7-era hosts run 7.4). Unattended argv:

```
ssh <host ssh-options> <ssh.options>
    -T -o BatchMode=yes -o ConnectTimeout=10 -o StrictHostKeyChecking=accept-new
    -o ClearAllForwardings=yes -o ForwardAgent=no -o ForwardX11=no -o PermitLocalCommand=no
    -o ControlMaster=auto -o ControlPath=<runtime>/hostctl/ssh/<hash> -o ControlPersist=10m
    [-p PORT] -- [user@]host sh -s
```

User options come first (ssh keeps the first value of an option). The control
socket is private (`$XDG_RUNTIME_DIR/hostctl/ssh`, else `~/.cache/hostctl/ssh`,
mode 0700, short hashed names for macOS's 104-byte limit), so an interactive
`ssh <alias>` never rides hostctl's forwarding-less connection.
`ssh.persist = "0s"` uses `-o ControlMaster=no -o ControlPersist=no` instead.
The private masters use `ServerAliveInterval=5` and `ServerAliveCountMax=2`, so
a dead connection is noticed in about 10 s; a probe that times out over a
master is retried once without it. When ssh exits 255 with no message, or with
just `Connection closed by UNKNOWN port 65535` (ControlPersist hides
ProxyCommand errors), the probe is retried once without multiplexing to
capture the real reason; it is also retried once on
`kex_exchange_identification` / connection resets (sshd MaxStartups). At most
10 *new* connections (no live control socket) are opened at once. Known
failures get hints (unresolvable host, BatchMode auth failure, changed host
key, `RemoteCommand` in ssh config).

Remote commands that need Nix (closure listing, builds, activation) start with a
**PATH prelude** — `/run/wrappers/bin` prepended (NixOS's setuid `sudo` must win),
then appended `/nix/var/nix/profiles/default/bin`, `/run/current-system/sw/bin`,
`$HOME/.nix-profile/bin`, `${XDG_STATE_HOME:-$HOME/.local/state}/nix/profile/bin`,
`/etc/profiles/per-user/$USER/bin` — because non-login sessions often lack Nix,
and `sh -l` doesn't always add it.

Destinations are validated strictly (no shell metacharacters, controls, leading
`-` in user or host, `host:port` only inside `ssh://`). hostctl drops one of its
own `-o Key=…` defaults when the user's options already set `Key` (equivalent
under first-value-wins, and avoids OpenSSH < 7.6 rejecting `accept-new` even when
overridden); on `unsupported option "accept-new"` the engine retries with
`StrictHostKeyChecking=yes` and keeps it.

## Flake resolution and evaluation

```
nix --extra-experimental-features 'nix-command flakes' flake metadata --json --no-write-lock-file [--refresh] <flake>
nix … eval --raw --read-only --no-eval-cache --no-write-lock-file --option tarball-ttl 4294967295 '<locked>#.<attr>.outPath'
nix … eval --json --read-only --no-eval-cache --no-write-lock-file --option tarball-ttl 4294967295 '<locked>#.' \
      --apply 'f: { nixos = attrNames (f.nixosConfigurations or {}); darwin = …; home = …; inventory = <f.lib.hostctl or null>; }'
```

- Every host of a flake is compared against the **same** locked URL; each
  distinct flake is resolved once, in parallel.
- `nix.refresh` (default `60s`): a resolution younger than that is reused from
  the cache without calling nix; older ones run with `--refresh`. `--refresh`
  (CLI) forces it. `--offline` never refreshes and never evaluates: only cached
  resolutions and results are used (a host without them shows "not cached; run
  hostctl once without --offline"); hosts are still probed.
- Metadata stderr is inspected: `; using cached version` / download failures ⇒
  `FlakeInfo.stale` (header badge, exit 2); `not writing modified lock file` ⇒
  `FlakeInfo.lock_outdated` (warning, results not cached). If resolution fails
  outright, the last good resolution is used, marked stale.
- `FlakeInfo::fetchable_remotely()` (github/gitlab/sourcehut, git+https/ssh,
  http tarballs) — local flakes (`path:`, `git+file:`, dirty trees) can't be
  activated/built *on a remote target* (use `activate-on = "local"`).

## Host inventory

Hosts come from, merged field by field (later wins): **discovery** (names and
kinds of `nixosConfigurations`/`darwinConfigurations`/`homeConfigurations`;
`discover`, default on when a flake is known) → the flake's **inventory**
attribute (`inventory`, default `lib.hostctl`; `{ hosts.<name> = { …same keys
as [hosts] }; }`, versioned with the flake, so every machine and teammate sees
the hosts of the revision being compared) → `[defaults]` → `[hosts.<name>]`
tables → CLI. `exclude` globs drop hosts (installer ISOs, CI configs). An
inventory can't set `ssh-options` or `probe` (ProxyCommand/LocalCommand would run
code on this machine; a raw command would run on the host) — those come only
from the local config.
Discovered names are used as ssh destinations only if they look like
`[user@]host`. A name in several outputs resolves by kind precedence nixos >
darwin > home-manager (reported by `hostctl hosts`).

Discovery and inventory are one pure eval of the locked flake (0.07 s; pure
even with `--impure` in `nix.options`), cached by flake identity.

## Cache (`$XDG_CACHE_HOME/hostctl/cache.json`)

```json
{ "version": 2,
  "flakes": { "<key>": { "flake": "github:jjant/nix-config", "rev": "42ea902…", "lastModified": 1792240011,
                         "used": 1792240100, "outs": { "<attr>": "/nix/store/…" },
                         "hosts": { "discovered": […], "inventory": {…} } } },
  "resolutions": { "github:jjant/nix-config": { "info": { …FlakeInfo }, "resolvedAt": 1792240100 } },
  "hints": { "github:jjant/nix-config#<attr>": { "evalMs": 3400, "readOnlyFails": false } } }
```

- `<key>` = hash of: full first line of `nix --version` (Nix, Lix and
  Determinate differ), `nix.options`, the store dir, and the flake identity
  (`fingerprint`, else narHash + rev/dirtyRev + lastModified + revCount).
- Nothing is cached for dirty trees, `lock_outdated` flakes, or when
  `nix.options` contains `--impure`.
- Written atomically (unique temp + rename) under `flock(cache.lock)` after
  merging with the on-disk copy; 64 most recently used flake entries kept.
- Reverse lookup (`deployed_rev`) names the revision a host runs when the
  generation itself doesn't record it.

## Closure diff (`d`/Enter in the TUI, `hostctl diff <host>`)

1. One script on the target prints the closures (`nix-store -qR`,
   `nix-store -q --size`; any Nix version) of the current and desired paths;
   concurrently the local store is asked for the desired one.
2. If the desired generation exists nowhere, the user may build/fetch it **on the
   target** (interactive, progress visible, never activates:
   `nix build --no-link --print-out-paths {flake}#.<attr>`), then diff.
3. Grouping/semantics match `nix store diff-closures` (validated against real
   fixtures); versions classified with Nix's `compareVersions`.

Forced-command routes: diff only from the local store.

## Activation (`a` in the TUI, `hostctl activate <host>…`)

Per-host `activate` template + `activate-on` (`target`, default: run on the host;
`local`: run here, for push tools). Defaults (all use `{flake}`):

| kind           | default `activate`                                                                 |
| -------------- | ---------------------------------------------------------------------------------- |
| `nixos`        | `/run/current-system/sw/bin/nixos-rebuild switch --flake {flake}#{name} --sudo`     |
| `darwin`       | `sudo /run/current-system/sw/bin/darwin-rebuild switch --flake {flake}#{name}`      |
| `home-manager` | `out=$(nix --extra-experimental-features 'nix-command flakes' build --no-link --print-out-paths {flake}#.{attr}) && [ "$out" = {out} ] && "$out/activate"` (no CLI needed; refuses anything but the compared generation) |
| `custom`       | none                                                                               |

Placeholders (values shell-quoted): `{flake}` locked URL, `{flake_url}` as
configured, `{rev}`, `{name}`, `{attr}`, `{out}` desired store path,
`{destination}` as configured, `{ssh_host}` host part. Recipes: `nix run
{flake}#activate` (flakes with an activate app), `nixos-rebuild switch --flake
{flake}#{name} --target-host {destination} --sudo` / `deploy {flake}#{name}` /
`colmena apply --on {name}` with `activate-on = "local"`. `activate = ""` or
`false` disables.

Remote execution is shell-agnostic: the command (PATH prelude + template) is
uploaded over the unattended channel — inside the `sh -s` program as a quoted
here-document, so the login shell only ever parses `sh -s` — into a private temp
file (`umask 077; mktemp`), then run with `ssh -t … sh <path>` (the script
deletes itself). The
TUI suspends (after cancelling a running check), runs it with inherited stdio
(hostctl ignores SIGINT/SIGQUIT meanwhile, like system(3)), waits for Enter,
resumes and re-probes. The confirmation shows the exact command, never cut
(wrapped; when the box can't grow enough it scrolls, with where it runs kept
in view), and exactly that string is executed. `y` copies a paste-safe command:
`sh -c '<command>'` when it runs here (templates are POSIX sh; fish or zsh
would misread them bare), `hostctl activate <name>` for remote hosts — with the
`-c`/`--flake`/`--self` given, like every command hostctl suggests.

## Self detection

1. `--self NAME` / `HOSTCTL_SELF` / config `self`: exactly that host (unknown
   name ⇒ config error).
2. Vetoes: a destination with a user other than `$USER`, a non-default port or a
   loopback address is never local.
3. A host is local when its destination's host part or one of its `hostnames`
   equals the local hostname (both dotted ⇒ exact match; a dot-less name matches
   the other's first label); its bare name counts only if it has no dotted
   destination/hostnames.
4. Failing that, on macOS, the only `darwin` host whose destination is unset or
   loopback — marked "assumed local" in the UI/JSON.

## Configuration

Search order: `--config` → `$HOSTCTL_CONFIG` → `$XDG_CONFIG_HOME/hostctl/config.toml`
(`~/.config/…`) → `/etc/hostctl/config.toml`. No file ⇒ defaults. Unknown keys are
errors; `hostctl config check FILE` validates a file purely (no env, network or
`$HOME`), which the Nix modules run at build time.

Flake: `--flake` → `HOSTCTL_FLAKE` → `flake` → `.` when `./flake.nix` exists
(with no config file, messages say the flake came from `./flake.nix`) → error
explaining how to set one. Flakes starting with `-` are rejected.

```toml
flake = "github:jjant/nix-config"
self = "bigmem"                 # force which host is this machine
jobs = 8                        # concurrent evals (default: min(CPUs, MemAvailable / 1 GiB))
timeout = "20s"                 # per probe
eval-timeout = "10m"            # per eval (0 = none)
discover = true                 # add the flake's *Configurations (default: true)
inventory = "lib.hostctl"       # flake attribute with { hosts.<name> = {…}; } (read if present)
exclude = ["*-iso"]             # host globs to drop
cache = true

[nix]
command = "nix"
options = []                    # extra args for every nix invocation
read-only = true
refresh = "60s"                 # re-resolve the branch head when older than this (0 = always)

[ssh]
command = "ssh"
options = []                    # extra args; take precedence over hostctl's
persist = "10m"                 # private ControlMaster lifetime (0 = no multiplexing)

[ui]
refresh-interval = "0s"         # dashboard auto-refresh (0 = manual; ≥ 60s advised for github: flakes)

[defaults]                      # every host (host tables override)
destination = "{name}"          # template ({name})
# ssh-options, activate, activate-on, profile

[hosts.al2-x86_64]
kind = "home-manager"
destination = "devhost-devuser1-1a-8d41e07a.dc-west1.example.com"
activate = "nix run {flake}#activate"
# flake, attr, profile, hostnames = [...], ssh-options = [...], activate-on,
# optional = true, note = "…"

[hosts.mac-m1]
kind = "darwin"
destination = "ssh://devuser1@localhost:2022"   # reverse tunnel from a desk
probe = "current-system"                        # its forced command (ssh route only)
optional = true                                 # only reachable from the desk the Mac is attached to
note = "reachable only from a desk you ssh into from the Mac"
```

Nix: `homeModules.default`, `nixosModules.default`, `darwinModules.default`
expose `programs.hostctl.{enable,package,settings}`; `settings` is freeform TOML
(RFC 42) with typed `flake` and `hosts.<name>.kind`, nulls stripped, validated at
build time with `hostctl config check`.

## CLI

```
hostctl                      # dashboard on a terminal, `status` otherwise
hostctl status [PATTERN…] [--json] [--plain]
hostctl diff HOST [--build] [--json]
hostctl activate HOST… [--yes] [--force]
hostctl hosts [PATTERN…]     # resolved hosts, where they come from, how they're reached
hostctl config show|path|check [FILE]
hostctl completions SHELL
global: -c/--config, -f/--flake, -j/--jobs, --self NAME, --no-cache,
        --refresh (force re-resolving), --offline (no refreshes or evaluations; probes run),
        --color auto|always|never
```

Patterns are globs over host names, plus `@local`. A pattern that matches
nothing is an error (exit 3). `status` on a terminal redraws the plain table in
place while it fits (else a one-line progress on stderr) and ends with the same
bytes as `--plain`, no line cut; in tmux it never floods the scrollback, and
Ctrl-Z suspends it. The footer lists `update: hostctl activate <outdated…>`
(with the `-c`/`--flake`/`--self` given).

## TUI

Mid-check, at 120 columns
(`src/tui/snapshots/hostctl__tui__view__tests__mixed_120x32.snap`):

```
 hostctl  github:jjant/nix-config @ 42ea902 · 2h ago               ⠼ checking 11/12   2 ✓  4 ✗  1 ○  2 !  1 ?  1 –  1 …
╭──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────╮
│   HOST           KIND          STATUS            GEN  ACTIVATED  DETAIL                                              │
│ ▶ al2-aarch64    home-manager  ✗ outdated         93  4mo ago    on 1a2b3c4 (4mo old) · graviton desk                │
│   al2-x86_64     home-manager  ○ not activated                   no home-manager profile                             │
│   al2023-x86_64  home-manager  ⠼ evaluating 11s   12  3d ago                                                         │
│   bigmem •       home-manager  ✗ outdated          9  12m ago    unknown generation                                  │
│   builder        nixos         ! probe failed                    probe output ended early (connection dropped?)      │
│   desk           nixos         ! eval failed      41  1w ago     path 'wdfh0jh5-gen.drv' is not valid                │
│   mac-m1         darwin        – offline                         reachable only from a desk you ssh into from the M… │
│   pi             nixos         ? unreachable                     ssh: Could not resolve hostname pi: Name or servic… │
│   web1           nixos         ✓ up to date      230  2h ago     ⟳ reboot · specialisation gaming                    │
│   web2           darwin        ✗ outdated         51  1d ago     staged: switch/reboot pending                       │
│   web3           nixos         ✗ outdated         77  30m ago    same rev, different build                           │
│   web4           nixos         ✓ up to date        5  1mo ago                                                        │
╰──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────╯
╭ al2-aarch64 · home-manager · x86_64-linux · devhost-devuser1-2c-2b7c5f39.dc-west2.example.com ───────────────────────╮
│ current  aaaaaaaa-home-manager-generation · gen 93 · activated 4mo ago                                               │
│ desired  bbbbbbbb-home-manager-generation · 42ea902                                                                  │
│ drift    on 1a2b3c4 (4mo old), per hostctl's cache                                                                   │
│ compare  https://github.com/jjant/nix-config/compare/1a2b3c4d5e6f...42ea902131b93c04d098d3bcd02f29305b4bad7c         │
│ note     graviton desk                                                                                               │
│ activate on al2-aarch64: out=$(nix --extra-experimental-features 'nix-command flakes' build --no-link                │
│          --print-out-paths 'github:jjant/nix-config/42ea902131b93c04d098d3bcd02f29305b4bad7c?narHash=sha256-rTwky4CP │
│          Wade4oeZ81h5PcDt8%2Be8kXznGgaiDqYFhFk%3D'#.homeConfigurations.al2-aarch64.activationPackage) && [ "$out" =  │
│          /nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-home-manager-generation ] && "$out/activate"                    │
│ timings  probe 25ms · eval 3.4s                                                                                      │
│                                                                                                                      │
│                                                                                                                      │
│                                                                                                                      │
╰──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────╯
 ↑↓ select  enter diff  a activate  y copy  r refresh  / filter  s sort  ? help  q quit
```

- Header: the primary flake `@ rev · age`, `· stale` / `· flake.lock out of
  date`, status counts and, while a check runs, `⠋ checking N/M` (hosts done
  of all; hosts keep their previous status meanwhile). Why a flake is stale
  goes on the line under the header, with errors not tied to a host (the
  latest, `+N more`), and in full in the details of that flake's hosts.
- Columns HOST · STATUS · GEN · ACTIVATED · DETAIL (+ KIND when wide). When
  narrow, DETAIL truncates first, then KIND, GEN, ACTIVATED go; HOST and STATUS
  never do. The local host is marked `•`. The selection is `▶` and its HOST
  cell reversed: no background under the status colors.
- Confirmations run only on `y` (`n`/Esc cancel; Enter just says so) and show
  the whole command: the box grows, then scrolls (↑↓) with where it runs kept
  in view.
- `d`/Enter opens a full-screen scrollable diff view (`a` activates, `r` diffs
  again, `y` copies, `q`/Esc back); when the new generation is in neither this
  machine's nor the host's store, it says a binary cache may have it and that
  `b` fetches or builds it (on the host, or here for local hosts and
  `activate-on = "local"`). On the dashboard `r` is ignored (with a flash)
  while a check runs. Actions on a busy host are refused.
- `app.rs` is a pure state machine (`on_key → Command`), `view.rs` renders it
  (insta snapshots of `TestBackend` buffers at 40/60/80/120 columns), `mod.rs`
  owns the terminal: the first check starts before terminal setup; the loop
  sleeps until `App::next_wakeup` (spinner only while busy, flash expiry, age
  labels, refresh timer), drains the channel before drawing, ≤ 1 frame/16 ms.
- Terminal restored on exit, error, panic and signals. `NO_COLOR` honored.
  Untrusted text (remote output, names) is sanitized before it reaches the
  terminal (ratatui also drops control characters).

## Process execution

`exec::run`: children start in their own session (`setsid`, no controlling
terminal, so Ctrl-C and tty prompts don't reach them), `GIT_TERMINAL_PROMPT=0`,
stdin written concurrently with reading output, output capped per call
(`CommandSpec::max_output`). Timeout or drop ⇒ `killpg(SIGTERM)`, 200 ms,
`killpg(SIGKILL)`, then at most 100 ms of draining (grandchildren such as
ProxyCommands can't hold it open). On a normal exit pipes are drained for at most
100 ms and leftover descendants are left alone (a ControlPersist master's
ProxyCommand is that master's transport). hostctl handles SIGINT/SIGTERM/SIGHUP:
in-flight work is reported as interrupted (not unreachable), the terminal is
restored, the cache flushed, exit 130.

## Security

Discovery/inventory always run in pure mode, even when `nix.options` has
`--impure` (a flake can't read the caller's environment into host names).
Untrusted strings are sanitized (`util::sanitize_terminal`) at the boundary.
Placeholders are shell-quoted; `--` precedes destinations; flakes starting with
`-` are rejected; remote scripts travel on stdin; uploaded activation scripts
are private temp files.

## Testing

- Unit tests next to each module (parsers, quoting, config, diff, reducer, keys).
- Probe script: run for real under dash, `bash --posix`, busybox sh against
  `$HOSTCTL_PROBE_ROOT` trees, BSD `stat` via a PATH shim. Quoting: round-trips
  through sh, bash, zsh, fish. With `HOSTCTL_TEST_REQUIRE_SHELLS=1` (Nix checks) a
  missing shell fails instead of skipping.
- Engine: in-process with a scripted runner under paused tokio time — exact
  event sequences, ≤ `jobs` overlapping evals, cache hits spawn no eval, IFD
  retry, flake failure fan-out, stale fallback, cancellation.
- TUI: `insta` snapshots; key handling unit tests.
- `tests/`: the real binary (`CARGO_BIN_EXE_hostctl`) against fake `nix`/`ssh`
  (`#!/bin/sh`; the fake ssh runs `sh -s` locally against per-destination
  `HOSTCTL_PROBE_ROOT` trees) — CLI wiring, exit codes, JSON, diff, activation;
  parallelism via overlap logs, never wall time.
- Real Nix (`HOSTCTL_TEST_NIX=1`, on in Nix checks via a daemon-less
  `--store local?root=$TMPDIR/store`): a `builtins.derivation`-only flake and a
  `custom` local host — up to date ⇒ 0, repointed profile ⇒ 1, second run spawns
  no eval, flake dir untouched.
- `nix flake check`: package (+ tests), clippy `-D warnings`, rustfmt + nixfmt,
  module evaluation for all three targets, and (Linux) a NixOS VM test with a
  real sshd between a controller and a target.
