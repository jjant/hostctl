{
  description = "hostctl: are your Nix hosts running the configuration their flake describes?";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs =
    { self, nixpkgs }:
    let
      inherit (nixpkgs) lib;
      # nixpkgs 26.11 dropped x86_64-darwin.
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forAllSystems = f: lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      # `programs.hostctl` for one module system; see ./nix/module.nix.
      module = target: lib.modules.importApply ./nix/module.nix { inherit target; };
    in
    {
      packages = forAllSystems (pkgs: {
        hostctl = pkgs.callPackage ./nix/package.nix { };
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.hostctl;
      });

      # nixpkgs already has an unrelated `hostctl` (guumaster/hostctl, an
      # /etc/hosts manager); this overlay shadows it.
      overlays.default = final: _prev: {
        hostctl = final.callPackage ./nix/package.nix { };
      };

      homeModules.default = module "home-manager";
      nixosModules.default = module "nixos";
      darwinModules.default = module "darwin";

      checks = forAllSystems (pkgs: import ./nix/checks.nix { inherit self pkgs; });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          # cargo and rustc, plus the test inputs: the shells, busybox, nix, git.
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.hostctl ];
          packages = with pkgs; [
            clippy
            rustfmt
            rust-analyzer
            cargo-nextest
            cargo-insta
            nixfmt
          ];
          env.RUST_SRC_PATH = pkgs.rustPlatform.rustLibSrc;
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt-tree);
    };
}
