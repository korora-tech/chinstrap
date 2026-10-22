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
