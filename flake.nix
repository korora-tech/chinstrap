{
  description = "Chinstrap: a native GPUI screen recorder for Linux";

  nixConfig = {
    extra-substituters = [ "https://korora-tech.cachix.org" ];
    extra-trusted-public-keys = [
      "korora-tech.cachix.org-1:42bs8TVW3uvxvMnuJtkXP5lkHFK7++Sral9uRDautuA="
    ];
  };

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      # pkg-config: gstreamer-1.0, gstreamer-pbutils-1.0 (gstreamer-rs links against these).
      buildInputs =
        pkgs:
        (with pkgs; [
          fontconfig
          freetype
          libxcb
          libxkbcommon
          vulkan-loader
          vulkan-headers
          wayland
          glib
        ])
        ++ (with pkgs.gst_all_1; [
          gstreamer
          gst-plugins-base
        ]);

      # Runtime GStreamer plugins. pipewire.out provides lib/gstreamer-1.0/libgstpipewire.so.
      # Never reference gst_all_1.gst-vaapi (removed from nixpkgs; throws on evaluation).
      gstPlugins =
        pkgs:
        (with pkgs.gst_all_1; [
          gstreamer
          gst-plugins-base
          gst-plugins-good
          gst-plugins-bad
          gst-plugins-ugly
        ])
        ++ [ pkgs.pipewire ];

      # Nix sets SOURCE_DATE_EPOCH, which makes ggml default GGML_NATIVE=OFF *and* every SIMD flag OFF.
      # Pin an x86-64-v3 baseline (whisper-rs-sys forwards GGML_* env vars to cmake).
      ggmlEnv =
        pkgs:
        pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isx86_64 {
          GGML_NATIVE = "OFF";
          GGML_SSE42 = "ON";
          GGML_AVX = "ON";
          GGML_AVX2 = "ON";
          GGML_BMI2 = "ON";
          GGML_FMA = "ON";
          GGML_F16C = "ON";
        };

      gstPluginPath = pkgs: pkgs.lib.makeSearchPathOutput "lib" "lib/gstreamer-1.0" (gstPlugins pkgs);

      # GPUI dlopens Vulkan, Wayland and xkbcommon at runtime.
      runtimeLibraryPath =
        pkgs:
        pkgs.lib.makeLibraryPath (
          with pkgs;
          [
            libxkbcommon
            vulkan-loader
            wayland
          ]
        );
    in
    {
      packages = forAllSystems (pkgs: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "chinstrap";
          version = "0.1.0";
          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./src
              # Integration tests (worker orphan protection) run in checkPhase too.
              (pkgs.lib.fileset.maybeMissing ./tests)
            ];
          };

          cargoLock = {
            lockFile = ./Cargo.lock;
            allowBuiltinFetchGit = true;
          };

          nativeBuildInputs = with pkgs; [
            makeWrapper
            pkg-config
            # whisper-rs-sys: cmake builds whisper.cpp/ggml; bindgen needs libclang; glslc compiles ggml-vulkan shaders.
            cmake
            shaderc
            rustPlatform.bindgenHook
          ];
          # cmake is only for whisper-rs-sys's build.rs; keep its setup hook out of configurePhase.
          dontUseCmakeConfigure = true;
          env = ggmlEnv pkgs;
          buildInputs = buildInputs pkgs ++ gstPlugins pkgs;

          postFixup = ''
            wrapProgram $out/bin/chinstrap \
              --prefix LD_LIBRARY_PATH : ${runtimeLibraryPath pkgs} \
              --set GST_PLUGIN_SYSTEM_PATH_1_0 ${gstPluginPath pkgs} \
              --unset GST_PLUGIN_PATH_1_0 \
              --unset GST_PLUGIN_PATH
          '';

          meta = {
            description = "A native GPUI screen recorder for Linux";
            license = pkgs.lib.licenses.asl20;
            mainProgram = "chinstrap";
            platforms = systems;
          };
        };
      });

      homeModules = {
        default = self.homeModules.chinstrap;
        chinstrap = import ./nix/hm-module.nix self;
      };

      # Evaluates the Home Manager module against stub options.
      checks = forAllSystems (
        pkgs:
        let
          inherit (nixpkgs) lib;
          system = pkgs.stdenv.hostPlatform.system;
          hmStub = {
            options = {
              home.packages = lib.mkOption {
                type = lib.types.listOf lib.types.package;
                default = [ ];
              };
              assertions = lib.mkOption {
                type = lib.types.listOf lib.types.attrs;
                default = [ ];
              };
              xdg.configFile = lib.mkOption {
                type = lib.types.attrsOf (lib.types.attrsOf lib.types.anything);
                default = { };
              };
            };
          };
          eval =
            settings:
            (lib.evalModules {
              modules = [
                hmStub
                self.homeModules.default
                {
                  _module.args.pkgs = pkgs;
                  programs.chinstrap = settings;
                }
              ];
            }).config;
          plain = eval { enable = true; };
          # A stand-in binary, so the wrapper check does not build Chinstrap.
          stub = pkgs.writeShellScriptBin "chinstrap" "true";
          full = eval {
            enable = true;
            package = stub;
            settings = {
              whisper_lang = "en";
              min_free_mb = 4096;
              preview = false;
            };
            glossary = [
              "Rawkode"
              "Kubernetes"
            ];
            replacements."raw code" = "Rawkode";
          };
          bad = eval {
            enable = true;
            glossary = [ "# not a term" ];
            replacements."a => b" = "c";
          };
          checks = [
            (lib.assertMsg (
              plain.home.packages == [ self.packages.${system}.default ]
            ) "installs the package unwrapped without settings")
            (lib.assertMsg (plain.xdg.configFile == { }) "leaves the vocabulary files alone by default")
            (lib.assertMsg (lib.all (a: a.assertion) full.assertions) "assertions hold")
            (lib.assertMsg (
              lib.length (lib.filter (a: !a.assertion) bad.assertions) == 2
            ) "invalid vocabulary is refused")
            (lib.assertMsg (
              full.xdg.configFile."chinstrap/glossary.txt".text == "Rawkode\nKubernetes\n"
            ) "writes the glossary")
            (lib.assertMsg (
              full.xdg.configFile."chinstrap/replacements.txt".text == "raw code => Rawkode\n"
            ) "writes the replacement rules")
          ];
        in
        {
          hm-module = pkgs.runCommand "chinstrap-hm-module-check" { } ''
            echo ${toString (lib.all lib.id checks)} > $out
            # The wrapper sets each setting as CHINSTRAP_<NAME>; booleans become 1 or 0.
            wrapper=${lib.getExe full.programs.chinstrap.finalPackage}
            grep -q "CHINSTRAP_WHISPER_LANG='en'" $wrapper
            grep -q "CHINSTRAP_MIN_FREE_MB='4096'" $wrapper
            grep -q "CHINSTRAP_PREVIEW='0'" $wrapper
          '';
        }
      );

      apps = forAllSystems (pkgs: {
        default = {
          type = "app";
          program = nixpkgs.lib.getExe self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        };
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.default ];
          packages = with pkgs; [
            cargo
            clippy
            rust-analyzer
            rustc
            rustfmt
            # gst-launch-1.0, gst-inspect-1.0, gst-device-monitor-1.0, gst-discoverer-1.0
            gst_all_1.gstreamer.bin
            gst_all_1.gst-plugins-base
          ];
          LD_LIBRARY_PATH = runtimeLibraryPath pkgs;
          env = ggmlEnv pkgs;
          GST_PLUGIN_SYSTEM_PATH_1_0 = gstPluginPath pkgs;
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
