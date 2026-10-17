# `programs.hostctl` for home-manager, NixOS and nix-darwin (`target`):
# installs hostctl and writes its config.toml from `settings`. The flake
# exposes it as homeModules / nixosModules / darwinModules.default.
{ target }:
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.programs.hostctl;
  inherit (lib) mkOption types;
  toml = pkgs.formats.toml { };
  home = target == "home-manager";
  configPath = if home then "$XDG_CONFIG_HOME/hostctl/config.toml" else "/etc/hostctl/config.toml";

  # TOML has no null: unset typed options (and nulls anywhere) disappear.
  stripNulls =
    value:
    if lib.isStringLike value then
      value
    else if builtins.isAttrs value then
      lib.mapAttrs (_: stripNulls) (lib.filterAttrs (_: v: v != null) value)
    else if builtins.isList value then
      map stripNulls (builtins.filter (v: v != null) value)
    else
      value;

  # `hosts` is declared with a default, so it is always there.
  settings =
    let
      stripped = stripNulls cfg.settings;
    in
    if stripped.hosts == { } then removeAttrs stripped [ "hosts" ] else stripped;

  generated = toml.generate "hostctl-config.toml" settings;

  # The installed hostctl judges its own config; when cross-compiling, one
  # that runs on the build machine.
  checker =
    if pkgs.stdenv.buildPlatform.canExecute pkgs.stdenv.hostPlatform then
      cfg.package
    else
      pkgs.buildPackages.callPackage ./package.nix { };

  configFile =
    if cfg.validate then
      pkgs.runCommand "hostctl-config.toml"
        {
          nativeBuildInputs = [ checker ];
          preferLocalBuild = true;
          allowSubstitutes = false;
        }
        ''
          hostctl config check ${generated}
          cp ${generated} $out
        ''
    else
      generated;

  hostType = types.submodule {
    freeformType = toml.type;
    options.kind = mkOption {
      type = types.nullOr (
        types.enum [
          "nixos"
          "darwin"
          "home-manager"
          "custom"
        ]
      );
      default = null;
      description = "What the host runs (default: from the flake output it was discovered in).";
    };
  };
in
{
  _class =
    {
      home-manager = "homeManager";
      nixos = "nixos";
      darwin = "darwin";
    }
    .${target} or (throw "hostctl: unknown module target ${target}");

  options.programs.hostctl = {
    enable = lib.mkEnableOption "hostctl, which checks whether your Nix hosts run what their flake describes";

    package = mkOption {
      type = types.package;
      default = pkgs.callPackage ./package.nix { };
      defaultText = lib.literalExpression "pkgs.callPackage \"\${hostctl}/nix/package.nix\" { }";
      description = "The hostctl package (by default built with this configuration's nixpkgs).";
    };

    settings = mkOption {
      type = types.submodule {
        freeformType = toml.type;
        options = {
          flake = mkOption {
            type = types.nullOr types.str;
            default = null;
            example = "github:you/nix-config";
            description = "The flake every host is compared against.";
          };
          hosts = mkOption {
            type = types.attrsOf hostType;
            default = { };
            description = "Per-host settings (`[hosts.<name>]`), keyed by configuration name.";
          };
        };
      };
      default = { };
      example = lib.literalExpression ''
        {
          flake = "github:you/nix-config";
          exclude = [ "*-iso" ];
          hosts.mac = {
            kind = "darwin";
            destination = "ssh://you@localhost:2022";
            optional = true;
          };
        }
      '';
      description = ''
        hostctl's `config.toml` (see <https://github.com/jjant/hostctl/blob/main/docs/reference.md#configuration>),
        written to {file}`${configPath}` unless empty. Nulls are dropped.
      '';
    };

    validate = mkOption {
      type = types.bool;
      default = true;
      description = ''
        Check the generated file with `hostctl config check` at build time, so
        a misspelled key fails the build instead of every later run.
      '';
    };
  };

  config = lib.mkIf cfg.enable (
    if home then
      {
        home.packages = [ cfg.package ];
        xdg.configFile."hostctl/config.toml" = lib.mkIf (settings != { }) { source = configFile; };
      }
    else
      {
        environment.systemPackages = [ cfg.package ];
        environment.etc."hostctl/config.toml" = lib.mkIf (settings != { }) { source = configFile; };
      }
  );
}
