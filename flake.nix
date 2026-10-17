{
  description = "hostctl: are your Nix hosts running the configuration their flake describes?";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs =
    { nixpkgs, ... }:
    let
      inherit (nixpkgs) lib;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forAllSystems = f: lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer
            cargo-nextest
            cargo-insta
          ];
          env.RUST_SRC_PATH = pkgs.rustPlatform.rustLibSrc;
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt-tree);
    };
}
