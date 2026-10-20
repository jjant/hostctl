# hostctl, nixpkgs style: `pkgs.callPackage ./package.nix { }`.
{
  lib,
  stdenv,
  rustPlatform,
  installShellFiles,
  linkFarm,
  dash,
  bash,
  zsh,
  fish,
  busybox,
  nix,
  git,
}:

let
  # busybox isn't packaged for Darwin.
  hasBusybox = lib.meta.availableOn stdenv.buildPlatform busybox;
in
rustPlatform.buildRustPackage {
  pname = "hostctl";
  inherit ((lib.importTOML ../Cargo.toml).package) version;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
      ../tests
      ../examples
      # The tests check the docs' pictures and examples against the program.
      ../README.md
      ../DESIGN.md
      ../docs
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  # Test with the test profile: overflow checks and debug assertions on, and
  # no LTO link per test binary.
  checkType = "debug";

  nativeBuildInputs = [ installShellFiles ];

  # The probe script runs under each of these shells; the real-Nix tests
  # evaluate path and git flakes with this nix against a store in $TMPDIR.
  nativeCheckInputs = [
    dash
    bash
    zsh
    fish
    (lib.getBin nix) # not its -dev outputs (twice the closure)
    git
  ]
  # Only `busybox` itself: its applet links (sh, sed, stat, find, …) would
  # shadow coreutils for the whole build.
  ++ lib.optional hasBusybox (linkFarm "busybox" { "bin/busybox" = lib.getExe busybox; });

  env = {
    HOSTCTL_TEST_NIX = "1";
    INSTA_UPDATE = "no";
  }
  # A missing shell fails the tests instead of skipping them (busybox can't
  # be provided on Darwin).
  // lib.optionalAttrs hasBusybox { HOSTCTL_TEST_REQUIRE_SHELLS = "1"; };

  preCheck = ''
    export HOME=$TMPDIR
    export HOSTCTL_TEST_NIX_STORE="local?root=$TMPDIR/nix-store"
  '';

  postInstall = lib.optionalString (stdenv.buildPlatform.canExecute stdenv.hostPlatform) ''
    installShellCompletion --cmd hostctl \
      --bash <($out/bin/hostctl completions bash) \
      --fish <($out/bin/hostctl completions fish) \
      --zsh <($out/bin/hostctl completions zsh)
  '';

  meta = {
    description = "Check whether your Nix hosts run their flake's current configuration";
    homepage = "https://github.com/jjant/hostctl";
    license = lib.licenses.mit;
    mainProgram = "hostctl";
    platforms = lib.platforms.unix;
  };
}
