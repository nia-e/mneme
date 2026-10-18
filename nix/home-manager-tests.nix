# Deliberately external to flake.checks: production users do not acquire a Home
# Manager input just to install the client. Supply pinned pkgs/homeManager/mneme.
{ pkgs, homeManager, mneme }:
let
  inherit (pkgs) lib;
  package = mneme.packages.${pkgs.stdenv.hostPlatform.system}.client;
  alternative = pkgs.runCommand "mneme-test-package-override" { } ''mkdir "$out"'';
  configKey = "mneme/memory.toml";
  fileKey = ".config/${configKey}";
  mkHome = modules: homeManager.lib.homeManagerConfiguration {
    inherit pkgs;
    modules = [
      mneme.homeManagerModules.default
      {
        home.username = "mneme-test";
        home.homeDirectory = "/home/mneme-test";
        home.stateVersion = "26.05";
      }
    ] ++ modules;
  };
  baseline = mkHome [ ];
  disabled = mkHome [ { programs.mneme.settings.schema = "ignored-while-disabled"; } ];
  unmanaged = mkHome [ { programs.mneme.enable = true; } ];
  handwritten = mkHome [ {
    programs.mneme.enable = true;
    xdg.configFile.${configKey}.text = "existing handwritten file";
  } ];
  overridden = mkHome [ { programs.mneme = { enable = true; package = alternative; }; } ];
  settings = {
    schema = "mneme.config.v1";
    read_default = "fixture";
    background.enabled = false;
    workspace = { profile = "default"; allow_stores = [ "fixture" ]; };
    stores.fixture = {
      id = "01ARZ3NDEKTSV4RRFFQ69G5FA1";
      owner = {
        url = "ssh://fixture@owner.invalid";
        db = "project";
        ssh_mcp_port = 19001;
        path = "/srv/mneme/fixture data/memory.db";
      };
      active = {
        workspace = "any";
        profiles = [ "default" "private" ];
        excluded_roots = [ "/synthetic/excluded path" ];
      };
      permissions = { read = true; write = false; };
      delivery = { core = false; context = false; };
      background = { recall = false; record = false; maintenance = false; };
    };
  };
  configured = mkHome [ { programs.mneme = { enable = true; inherit settings; }; } ];
  # Exercise normal module merging, not just a single complete attribute set.
  merged = mkHome [
    { programs.mneme = { enable = true; settings = builtins.removeAttrs settings [ "background" ]; }; }
    { programs.mneme.settings.background.enabled = false; }
  ];
  customXdg = mkHome [ {
    programs.mneme = { enable = true; inherit settings; };
    xdg.configHome = "/home/mneme-test/custom-config";
  } ];
  customBaseline = mkHome [ { xdg.configHome = "/home/mneme-test/custom-config"; } ];
  empty = mkHome [ { programs.mneme = { enable = true; settings = { }; }; } ];
  unknown = mkHome [ {
    programs.mneme = { enable = true; settings = settings // { misspelled_native_field = true; }; };
  } ];
  wrongType = mkHome [ {
    programs.mneme = { enable = true; settings = settings // { background.enabled = "false"; }; };
  } ];
  textCollision = mkHome [ {
    programs.mneme = { enable = true; inherit settings; };
    xdg.configFile.${configKey}.text = "old native config";
  } ];
  homeTextCollision = mkHome [ {
    programs.mneme = { enable = true; inherit settings; };
    home.file."/home/mneme-test/${fileKey}".text = "old native config";
  } ];
  sourceCollision = mkHome [ {
    programs.mneme = { enable = true; inherit settings; };
    xdg.configFile.${configKey}.source = pkgs.writeText "conflicting-mneme-config" "old native config";
  } ];
  failures = home: lib.filter (entry: !entry.assertion) home.config.assertions;
  files = home: lib.sort builtins.lessThan (map (file: file.target) (builtins.attrValues home.config.home.file));
  packages = home: map toString home.config.home.packages;
  test = name: condition: { inherit name condition; };
  tests = [
    (test "disabled emits no files or package even with settings"
      (files disabled == files baseline && packages disabled == packages baseline))
    (test "enabled with null settings installs only the default package"
      (files unmanaged == files baseline
        && lib.subtractLists (packages baseline) (packages unmanaged) == [ (toString package) ]))
    (test "null settings preserve separately managed handwritten configuration"
      (handwritten.config.xdg.configFile.${configKey}.text == "existing handwritten file"
        && failures handwritten == [ ]))
    (test "explicit package override replaces the default"
      (lib.subtractLists (packages baseline) (packages overridden) == [ (toString alternative) ]))
    (test "settings add exactly the native configuration file, without force"
      (lib.subtractLists (files baseline) (files configured) == [ fileKey ]
        && !configured.config.xdg.configFile.${configKey}.force))
    (test "settings merge through ordinary Home Manager definitions"
      (merged.config.programs.mneme.settings == settings))
    (test "custom XDG directory controls the generated file target"
      (lib.subtractLists (files customBaseline) (files customXdg) == [ "custom-config/${configKey}" ]))
    (test "explicit empty settings still manage the file"
      (lib.subtractLists (files baseline) (files empty) == [ fileKey ]))
    (test "configuration does not add activation hooks, services, timers or environment"
      (builtins.attrNames configured.config.home.activation == builtins.attrNames baseline.config.home.activation
        && configured.config.systemd.user.services == baseline.config.systemd.user.services
        && configured.config.systemd.user.timers == baseline.config.systemd.user.timers
        && configured.config.home.sessionVariables == baseline.config.home.sessionVariables))
    (test "valid and schema-invalid TOML both reach native validation unchanged"
      (lib.all (home: failures home == [ ]) [ baseline disabled unmanaged overridden configured merged customXdg empty unknown wrongType ]))
    (test "old inline TOML and new settings cannot silently shadow one another"
      (!(builtins.tryEval (builtins.deepSeq textCollision.config.programs.mneme.settings true)).success))
    (test "a direct home.file text collision cannot silently shadow native settings"
      (!(builtins.tryEval (builtins.deepSeq homeTextCollision.config.home.file."/home/mneme-test/${fileKey}".text true)).success))
    (test "a conflicting explicit source remains an evaluation error"
      (!(builtins.tryEval (builtins.deepSeq sourceCollision.config.xdg.configFile.${configKey}.source true)).success))
  ];
  passed = builtins.foldl' (ok: entry: assert lib.assertMsg entry.condition entry.name; ok) true tests;
  fixture = home: relative: {
    generation = toString home.config.home-files;
    inherit relative;
    source = toString home.config.xdg.configFile.${configKey}.source;
  };
  manifest = pkgs.writeText "mneme-home-manager-test-manifest.json" (builtins.toJSON {
    inherit settings;
    package = toString package;
    checks = map (entry: entry.name) tests;
    fixtures = {
      valid = fixture configured fileKey;
      merged = fixture merged fileKey;
      customXdg = fixture customXdg "custom-config/${configKey}";
      empty = fixture empty fileKey;
      unknown = fixture unknown fileKey;
      wrongType = fixture wrongType fileKey;
    };
  });
in
assert passed;
pkgs.runCommand "mneme-home-manager-tests" { nativeBuildInputs = [ pkgs.python3 ]; } ''
  python3 ${./home-manager-test.py} ${manifest}
  mkdir "$out"
  cp ${manifest} "$out/manifest.json"
''
