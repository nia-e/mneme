{
  description = "Mneme remote client and development shell";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      version = (builtins.fromTOML (builtins.readFile ./crates/mnemed/Cargo.toml)).package.version;
      # Allow build inputs, not an entire checkout: local stores, caches and
      # archives must never become derivation inputs. See docs/nix.md for the
      # separate flake-fetch boundary (a raw path input is copied before this).
      source = nixpkgs.lib.cleanSourceWith {
        src = ./.;
        name = "mneme-source";
        filter = path: type:
          let
            lib = nixpkgs.lib;
            relative = lib.removePrefix (toString ./. + "/") (toString path);
            parts = lib.splitString "/" relative;
            roots = [ "crates" "vendor/mnestic" "integrations" ".agents/skills/mneme-bootstrap/scripts" ];
            allowed = builtins.elem relative [ "Cargo.toml" "Cargo.lock" "LICENSE-MIT" "LICENSE-APACHE" ]
              || builtins.any (root: relative == root || lib.hasPrefix (root + "/") relative
                || (type == "directory" && lib.hasPrefix (relative + "/") root)) roots;
            excluded = builtins.any (part: builtins.elem part
              [ ".git" ".mneme" "target" "__pycache__" ".pytest_cache" ".fastembed_cache" ]) parts;
          in allowed && !excluded && type != "symlink";
      };
      packagesFor = system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          client = pkgs.rustPlatform.buildRustPackage {
            pname = "mneme-client";
            inherit version;
            src = source;
            cargoLock.lockFile = ./Cargo.lock;
            buildNoDefaultFeatures = true;
            cargoBuildFlags = [ "--package" "mnemed" "--package" "mneme-mcp" ];
            # No Cozo/SQLite backend, ONNX runtime, model download, or HTTP
            # listener is included. Outbound HTTP and SSH remain available.
            checkPhase = ''
              runHook preCheck
              cargo test --offline --locked --release --target ${pkgs.stdenv.hostPlatform.rust.rustcTarget} -j "$NIX_BUILD_CORES" --no-default-features -p mneme-config -p mneme-store-client
              cargo test --offline --locked --release --target ${pkgs.stdenv.hostPlatform.rust.rustcTarget} -j "$NIX_BUILD_CORES" --no-default-features -p mnemed --test cli_config
              cargo test --offline --locked --release --target ${pkgs.stdenv.hostPlatform.rust.rustcTarget} -j "$NIX_BUILD_CORES" --no-default-features -p mneme-mcp configured_server::tests
              runHook postCheck
            '';
            postInstall = ''
              install -Dm644 ${./LICENSE-MIT} "$out/share/licenses/mneme/LICENSE-MIT"
              install -Dm644 ${./LICENSE-APACHE} "$out/share/licenses/mneme/LICENSE-APACHE"
              install -Dm644 ${pkgs.writeText "mneme-build-info.json" (builtins.toJSON {
                schema = "mneme.nix-package.v1";
                package = "client";
                inherit version;
                sourceRevision = self.rev or self.dirtyRev or null;
                sourcePath = toString source;
                cargoLockSha256 = builtins.hashFile "sha256" ./Cargo.lock;
                nixpkgsRevision = nixpkgs.rev or null;
                defaultFeatures = false;
                features = [ ];
                binaries = [ "mnemed" "mneme-mcp" ];
              })} "$out/share/mneme/build-info.json"
            '';
            postFixup = ''
              (cd "$out"; sha256sum bin/mnemed bin/mneme-mcp > share/mneme/binary-sha256.txt)
            '';
            meta = {
              description = "Mneme clients for existing remote memory owners (without Cozo/ONNX)";
              license = with pkgs.lib.licenses; [ mit asl20 ];
              mainProgram = "mnemed";
              platforms = systems;
            };
          };
        in { inherit client; default = client; };
    in {
      packages = forAllSystems packagesFor;
      apps = forAllSystems (system:
        let package = self.packages.${system}.client;
        in {
          default = self.apps.${system}.mnemed;
          mnemed = {
            type = "app";
            program = "${package}/bin/mnemed";
            meta.description = "Mneme remote command-line client";
          };
          mneme-mcp = {
            type = "app";
            program = "${package}/bin/mneme-mcp";
            meta.description = "Mneme configured stdio MCP client";
          };
        });
      checks = forAllSystems (system:
        let pkgs = nixpkgs.legacyPackages.${system};
        in {
          client = self.packages.${system}.client;
          client-smoke = pkgs.runCommand "mneme-client-smoke" {
            nativeBuildInputs = [ pkgs.python3 ];
          } ''
            python3 ${./nix/client-smoke.py} ${self.packages.${system}.client}
            touch "$out"
          '';
        });
      devShells = forAllSystems (system:
        let pkgs = nixpkgs.legacyPackages.${system};
        in {
          default = pkgs.mkShell {
            packages = with pkgs; [ cargo rustc rustfmt clippy pkg-config python3 ];
            shellHook = ''
              echo 'Mneme client development: cargo build --locked --no-default-features -p mnemed -p mneme-mcp'
              echo 'Full server / ONNX builds are not qualified by this shell.'
            '';
          };
        });
    };
}
