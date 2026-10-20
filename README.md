# hostctl

hostctl checks which of your Nix hosts (NixOS, nix-darwin and standalone
home-manager) run the configuration your flake builds for them now. For each
host it evaluates the configuration at the flake's latest revision, reads the
host's active generation over ssh, and compares the two; nothing is built. It
can then show what activating would change, and activate it.

It uses your own `nix` and `ssh`, so your ssh config, agents and substituters
apply. The hosts only need a POSIX `sh`.

## Usage

```sh
hostctl --flake github:you/nix-config
```

It checks every host and prints a table (on a terminal, redrawn in place as the
hosts answer):

```console
$ hostctl status
github:you/nix-config @ 42ea902 (2h ago)

  ✗ desk (this host)  outdated       on 1a2b3c4 (3d old)
  – laptop            offline        reachable from the desk only
  ✓ nas               up to date
  ○ work              not activated  no home-manager profile

update: hostctl activate desk work
```

Without a config file, every `nixosConfigurations`, `darwinConfigurations` and
`homeConfigurations` entry of the flake is a host, reached as `ssh <name>`, so
a `Host` entry in `~/.ssh/config` is all a host needs. The machine you run
hostctl on (`this host`) is checked without ssh. Anything else (another destination
or user, a host behind a tunnel, a custom activation command) goes in
`~/.config/hostctl/config.toml`; see [Configuration](docs/reference.md#configuration).

```text
hostctl status [HOST…]     print the table (--json for scripts)
hostctl diff HOST          what activating HOST would change
hostctl activate HOST…     activate the generation that was compared (--all: every host)
hostctl hosts              list the hosts and how each one is reached
```

`status` exits with 0 when every host is up to date, 1 when some host runs
something else and 2 when one is unreachable or failed.

## Install

To try it:

```sh
nix run github:jjant/hostctl -- --flake github:you/nix-config
```

To install it, add the flake as an input:

```nix
inputs.hostctl = {
  url = "github:jjant/hostctl";
  inputs.nixpkgs.follows = "nixpkgs";
};
```

and enable its module, here for home-manager (`nixosModules.default` and
`darwinModules.default` work the same way):

```nix
{ inputs, ... }:
{
  imports = [ inputs.hostctl.homeModules.default ];

  programs.hostctl = {
    enable = true;
    settings.flake = "github:you/nix-config";
  };
}
```

`settings` becomes hostctl's config file, checked when your configuration
builds. The package alone is `inputs.hostctl.packages.${system}.default`.

## More

- [docs/reference.md](docs/reference.md): the configuration, statuses and exit
  codes, JSON output, recipes (a host behind a tunnel, push deployments, a shell
  prompt) and security.
- [DESIGN.md](DESIGN.md): how it works inside, and how to work on it.

## License

[MIT](LICENSE)
