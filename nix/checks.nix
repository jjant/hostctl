# `nix flake check`: the package (build and tests), clippy, formatting, the
# module under all three module systems, and on x86_64-linux a VM test over
# a real sshd.
{ self, pkgs }:

let
  inherit (pkgs) lib;
  hostctl = self.packages.${pkgs.stdenv.hostPlatform.system}.hostctl;

  # Stand-ins for the options the module defines, typed like upstream's.
  packagesOption = lib.mkOption {
    type = lib.types.listOf lib.types.package;
    default = [ ];
  };
  filesOption = lib.mkOption {
    type = lib.types.attrsOf (
      lib.types.submodule { options.source = lib.mkOption { type = lib.types.path; }; }
    );
    default = { };
  };
  systemTarget = class: module: {
    inherit class module;
    stub.options.environment = {
      systemPackages = packagesOption;
      etc = filesOption;
    };
    packages = config: config.environment.systemPackages;
    files = config: config.environment.etc;
  };
  targets = {
    home-manager = {
      class = "homeManager";
      module = self.homeModules.default;
      stub.options = {
        home.packages = packagesOption;
        xdg.configFile = filesOption;
      };
      packages = config: config.home.packages;
      files = config: config.xdg.configFile;
    };
    nixos = systemTarget "nixos" self.nixosModules.default;
    darwin = systemTarget "darwin" self.darwinModules.default;
  };

  # The config of a `target` system with `programs.hostctl = hostctlConfig`.
  evaluate =
    {
      target,
      class ? targets.${target}.class,
    }:
    hostctlConfig:
    (lib.evalModules {
      inherit class;
      modules = [
        targets.${target}.module
        targets.${target}.stub
        {
          _module.args.pkgs = pkgs;
          programs.hostctl = hostctlConfig;
        }
      ];
    }).config;

  full = {
    flake = "github:you/nix-config";
    jobs = 4;
    exclude = [ "*-iso" ];
    nix.options = [
      "--option"
      "eval-cache"
      "false"
    ];
    defaults.destination = "{name}.lan";
    hosts = {
      web1 = {
        kind = "nixos";
        destination = "root@web1";
      };
      mac = {
        kind = "darwin";
        destination = "ssh://jj@localhost:2022";
        probe = "current-system";
        optional = true;
        note = ''only via the "desk" tunnel \o/'';
      };
      # `kind` defaults to null, which is dropped; the empty table stays.
      "jj@desk" = { };
    };
  };

  # settings → what `builtins.fromTOML` must read back from the file.
  cases = [
    {
      settings = full;
      expected = full;
    }
    {
      # The declared defaults (flake = null, hosts = { }) leave no trace.
      settings.ssh.persist = "1m";
      expected.ssh.persist = "1m";
    }
  ];

  moduleCheck =
    target:
    let
      inherit (targets.${target}) packages files;
      enabled =
        settings:
        evaluate { inherit target; } {
          enable = true;
          validate = false;
          inherit settings;
        };
      file = settings: (files (enabled settings))."hostctl/config.toml".source;
      disabled = evaluate { inherit target; } { settings = full; };
      fails = value: !(builtins.tryEval (builtins.deepSeq value value)).success;
      wrongClass = if target == "home-manager" then "nixos" else "homeManager";
    in
    assert lib.assertMsg (
      packages (enabled full) == [ hostctl ]
      && lib.attrNames (files (enabled full)) == [ "hostctl/config.toml" ]
    ) "${target}: expected the package and hostctl/config.toml";
    assert lib.assertMsg (
      files (enabled { }) == { } && packages (enabled { }) == [ hostctl ]
    ) "${target}: empty settings must not write a file";
    assert lib.assertMsg (
      files disabled == { } && packages disabled == [ ]
    ) "${target}: a disabled module must define nothing";
    assert lib.assertMsg (fails
      (enabled { hosts.web1.kind = "NixOS"; }).programs.hostctl.settings
    ) "${target}: a misspelled kind must not evaluate";
    assert lib.assertMsg (fails
      (evaluate {
        inherit target;
        class = wrongClass;
      } { enable = true; }).programs.hostctl.enable
    ) "${target}: the module must refuse ${wrongClass} configurations";
    pkgs.runCommand "hostctl-module-${target}" { } ''
      export HOME=$TMPDIR
      check() {
        actual=$(${lib.getExe' pkgs.nix "nix-instantiate"} --eval --strict --json --store dummy:// \
          --expr "builtins.fromTOML (builtins.readFile $1)")
        if [ "$actual" != "$2" ]; then
          printf '%s\n  expected %s\n  got      %s\n' "$1" "$2" "$actual" >&2
          exit 1
        fi
      }
      ${lib.concatMapStrings (case: ''
        check ${file case.settings} ${lib.escapeShellArg (builtins.toJSON case.expected)}
      '') cases}
      touch $out
    '';

  # Two "generations" of a custom host, in both VMs' stores.
  generation = n: pkgs.writeTextDir "generation-${toString n}" "";
  generations = [
    (generation 1)
    (generation 2)
  ];
  flake = pkgs.writeText "flake.nix" ''
    { outputs = _: { generation.outPath = "${generation 1}"; }; }
  '';
in
{
  inherit hostctl;

  clippy = hostctl.overrideAttrs (old: {
    pname = "hostctl-clippy";
    nativeBuildInputs = old.nativeBuildInputs ++ [ pkgs.clippy ];
    buildPhase = ''
      runHook preBuild
      cargo clippy --all-targets --offline -j "$NIX_BUILD_CORES" -- -D warnings
      runHook postBuild
    '';
    doCheck = false;
    installPhase = "touch $out";
    dontFixup = true;
  });

  fmt =
    pkgs.runCommand "hostctl-fmt"
      {
        nativeBuildInputs = [
          pkgs.cargo
          pkgs.rustfmt
          pkgs.nixfmt
        ];
        src = lib.fileset.toSource {
          root = ../.;
          fileset = lib.fileset.fileFilter (
            file:
            lib.any file.hasExt [
              "rs"
              "nix"
              "toml"
            ]
          ) ../.;
        };
      }
      ''
        export HOME=$TMPDIR
        cd "$src"
        cargo fmt --check
        find . -name '*.nix' -exec nixfmt --check {} +
        touch $out
      '';

  module-home-manager = moduleCheck "home-manager";
  module-nixos = moduleCheck "nixos";
  module-darwin = moduleCheck "darwin";

  # With validate = true (the default), a valid config builds and a
  # misspelled key fails the build, naming the key.
  module-validate =
    let
      file =
        settings: validate:
        (targets.home-manager.files (
          evaluate { target = "home-manager"; } {
            enable = true;
            inherit settings validate;
          }
        ))."hostctl/config.toml".source;
    in
    pkgs.runCommand "hostctl-module-validate"
      { typo = pkgs.testers.testBuildFailure (file { timout = "20s"; } true); }
      ''
        cmp ${file full true} ${file full false}
        grep -F timout "$typo/testBuildFailure.log"
        touch $out
      '';
}
// lib.optionalAttrs (pkgs.stdenv.hostPlatform.system == "x86_64-linux") {
  # A controller checks a custom host on a target over real ssh: up to date
  # (0), outdated once the target's profile moves (1), unreachable once sshd
  # is gone (2). Needs KVM.
  vm = pkgs.testers.runNixOSTest {
    name = "hostctl-ssh";
    nodes.target = {
      services.openssh.enable = true;
      virtualisation.additionalPaths = generations;
    };
    nodes.controller = {
      imports = [ self.nixosModules.default ];
      virtualisation.additionalPaths = generations;
      programs.hostctl = {
        enable = true;
        settings = {
          flake = "/root/flake";
          hosts.target = {
            kind = "custom";
            attr = "generation";
            profile = "/nix/var/nix/profiles/hostctl-test";
            destination = "root@target";
          };
        };
      };
    };
    testScript = ''
      import json

      def profile(path):
          target.succeed(f"nix-env -p /nix/var/nix/profiles/hostctl-test --set {path}")

      def expect(code, status):
          rc, out = controller.execute("hostctl status --json")
          print(out)
          assert rc == code, f"exit {rc}, expected {code}"
          assert [h["status"] for h in json.loads(out)["hosts"]] == [status]

      start_all()
      target.wait_for_unit("sshd.service")
      controller.wait_for_unit("multi-user.target")

      controller.succeed(
          "install -d -m 700 /root/.ssh",
          "ssh-keygen -q -t ed25519 -N \"\" -f /root/.ssh/id_ed25519",
      )
      key = controller.succeed("cat /root/.ssh/id_ed25519.pub").strip()
      target.succeed(
          "install -d -m 700 /root/.ssh",
          f"echo '{key}' >> /root/.ssh/authorized_keys",
      )
      controller.succeed("install -D -m 644 ${flake} /root/flake/flake.nix")
      profile("${generation 1}")

      with subtest("up to date"):
          expect(0, "up-to-date")

      with subtest("outdated once the profile moves"):
          profile("${generation 2}")
          expect(1, "outdated")

      with subtest("unreachable once sshd is gone"):
          target.succeed("systemctl stop sshd.service")
          # Including the session hostctl's ControlMaster keeps open.
          target.execute("pkill sshd")
          expect(2, "unreachable")
    '';
  };
}
