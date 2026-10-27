# Home Manager module: `programs.chinstrap`. Takes this flake (`self`) for its packages.
#
# Chinstrap reads its settings from `CHINSTRAP_*` environment variables, so `settings` is baked
# into a wrapper around the binary: it then applies however Chinstrap is started (a compositor
# keybind, a launcher, a shell). The transcription worker inherits the environment.
self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.programs.chinstrap;
  inherit (pkgs.stdenv.hostPlatform) system;

  envName = name: "CHINSTRAP_${lib.toUpper name}";
  envValue = value: if lib.isBool value then (if value then "1" else "0") else toString value;

  # The app's own checks (vocab.rs): such a line would be skipped or misread.
  valid =
    text: !(lib.hasInfix "\n" text || lib.hasInfix "\r" text || lib.hasPrefix "#" (lib.trim text));

  wrapped = pkgs.symlinkJoin {
    name = "chinstrap-configured";
    paths = [ cfg.package ];
    nativeBuildInputs = [ pkgs.makeWrapper ];
    postBuild = ''
      wrapProgram $out/bin/chinstrap ${
        lib.concatStringsSep " " (
          lib.mapAttrsToList (
            name: value: "--set ${lib.escapeShellArg (envName name)} ${lib.escapeShellArg (envValue value)}"
          ) cfg.settings
        )
      }
    '';
    inherit (cfg.package) meta;
  };
in
{
  options.programs.chinstrap = {
    enable = lib.mkEnableOption "Chinstrap, a native GPUI screen recorder for Linux";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${system}.default;
      defaultText = lib.literalExpression "chinstrap.packages.\${system}.default";
      description = "The Chinstrap package.";
    };

    finalPackage = lib.mkOption {
      type = lib.types.package;
      readOnly = true;
      default = if cfg.settings == { } then cfg.package else wrapped;
      defaultText = lib.literalMD "`package`, wrapped with `settings` when any are set";
      description = "The installed package: `package` with `settings` set in its environment.";
    };

    settings = lib.mkOption {
      type = lib.types.attrsOf (
        lib.types.oneOf [
          lib.types.bool
          lib.types.int
          lib.types.str
          lib.types.path
        ]
      );
      default = { };
      example = lib.literalExpression ''
        {
          output_dir = "''${config.home.homeDirectory}/Videos/Chinstrap";
          min_free_mb = 4096;
          frame_rate = "60";
          whisper_model = "large-v3-turbo-q8_0";
          whisper_lang = "en";
        }
      '';
      description = ''
        Environment for Chinstrap: each `name = value` is set as `CHINSTRAP_<NAME>` (upper-cased)
        in a wrapper around the binary. `true` and `false` become `1` and `0`. See the README's
        Configuration table for the variables and their values.
      '';
    };

    glossary = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [
        "Rawkode"
        "Kubernetes"
      ];
      description = ''
        Terms Whisper should spell your way, written to {file}`$XDG_CONFIG_HOME/chinstrap/glossary.txt`.
        When set, the file is read-only and the Vocabulary panel refuses to edit it; leave it empty
        to edit the glossary in the app.
      '';
    };

    replacements = lib.mkOption {
      type = lib.types.attrsOf lib.types.str;
      default = { };
      example = {
        "raw code" = "Rawkode";
        "cube control" = "kubectl";
      };
      description = ''
        Rules applied after transcription (`from => to`), written to
        {file}`$XDG_CONFIG_HOME/chinstrap/replacements.txt`. When set, the file is read-only and
        the Vocabulary panel refuses to edit it; leave it empty to edit the rules in the app.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = lib.all (from: valid from && !lib.hasInfix "=>" from) (lib.attrNames cfg.replacements);
        message = "programs.chinstrap.replacements: a `from` cannot contain `=>` or a line break, or start with `#`.";
      }
      {
        assertion = lib.all valid cfg.glossary;
        message = "programs.chinstrap.glossary: a term cannot contain a line break or start with `#`.";
      }
    ];

    home.packages = [ cfg.finalPackage ];

    xdg.configFile."chinstrap/glossary.txt" = lib.mkIf (cfg.glossary != [ ]) {
      text = lib.concatMapStrings (term: "${term}\n") cfg.glossary;
    };

    xdg.configFile."chinstrap/replacements.txt" = lib.mkIf (cfg.replacements != { }) {
      text = lib.concatStrings (lib.mapAttrsToList (from: to: "${from} => ${to}\n") cfg.replacements);
    };
  };
}
