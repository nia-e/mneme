{ self }:
{ config, lib, pkgs, ... }:

let
  cfg = config.programs.mneme;
  tomlFormat = pkgs.formats.toml { };
in
{
  options.programs.mneme = {
    enable = lib.mkEnableOption "the Mneme remote client";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.client;
      defaultText = lib.literalExpression "mneme.packages.\${pkgs.stdenv.hostPlatform.system}.client";
      description = ''
        Mneme package to install. The default is this flake's remote client,
        without Cozo/ONNX or an HTTP listener.
      '';
    };

    settings = lib.mkOption {
      type = lib.types.nullOr tomlFormat.type;
      default = null;
      description = ''
        Configuration serialized to {file}`$XDG_CONFIG_HOME/mneme/memory.toml`
        when Mneme is enabled. A null value leaves that file unmanaged, so an
        existing hand-written configuration can be kept. An explicit attribute
        set, including an empty one, opts into Home Manager file management.
        Remove any existing `xdg.configFile."mneme/memory.toml"`
        text or source declaration when migrating to these settings.

        These are Mneme's native TOML settings, not a separate Nix schema.
        Mneme validates their meaning when it loads the file. This module adds
        no store selections or defaults, contacts no owners, and activates no
        services, hooks, MCP registrations, or recording.

        The generated file is readable in the Nix store. Do not put passwords,
        tokens, private keys, or other secrets in these settings. Native
        `token_env` settings name environment variables populated outside Nix;
        SSH private keys must also stay outside the Nix store.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];
    assertions = lib.optional (cfg.settings != null) {
      assertion = config.xdg.configFile."mneme/memory.toml".text == null;
      message = ''
        programs.mneme.settings cannot be combined with
        xdg.configFile."mneme/memory.toml".text. Remove the old file declaration
        or leave programs.mneme.settings = null.
      '';
    };
    xdg.configFile."mneme/memory.toml" = lib.mkIf (cfg.settings != null) {
      source = tomlFormat.generate "mneme-memory.toml" cfg.settings;
    };
  };
}
