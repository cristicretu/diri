{
  description = "Diri: run and monitor coding agents in local and SSH sessions";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    # Provides the exact toolchain pinned in diri/rust-toolchain.toml, which
    # nixpkgs' rustc does not track.
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
    }:
    let
      inherit (nixpkgs) lib;
      # Only the platform this flake is built and run on. The Linux beta also
      # supports aarch64; add it here once someone verifies it.
      system = "x86_64-linux";
      pkgs = import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
      };

      toolchainFile = ./diri/rust-toolchain.toml;

      # GPUI dlopens these at runtime.
      runtimeLibraries =
        p: with p; [
          vulkan-loader
          wayland
          libxkbcommon
        ];

      # What scripts/package-linux.sh builds, one cargo invocation per package
      # as there, so features are not unified across the app and the Engine.
      builds = [
        {
          package = "diri-app";
          bins = [ "diri" ];
        }
        {
          package = "dirijor-mcp";
          bins = [
            "dirijor"
            "dirijor-mcp"
          ];
        }
        {
          package = "diri-engine";
          bins = [
            "dirijord-rs"
            "diri-holder"
            "diri-ssh-askpass"
          ];
        }
      ];
      binaries = lib.concatMap (build: build.bins) builds;

      # The Linux packages' binary list is the reference; fail evaluation when
      # the two drift instead of shipping a package missing a binary.
      packagerBinaries =
        let
          text = builtins.readFile ./diri/scripts/linux-packager-config.py;
          header = "\nBINARIES = (";
          tuple = builtins.head (lib.splitString ")" (lib.last (lib.splitString header text)));
        in
        assert lib.assertMsg (lib.hasInfix header text) "linux-packager-config.py no longer defines BINARIES";
        map builtins.head (builtins.filter builtins.isList (builtins.split "\"([^\"]+)\"" tuple));
      checkedBinaries =
        assert lib.assertMsg (lib.sort lib.lessThan packagerBinaries == lib.sort lib.lessThan (
          binaries ++ [ "diri-remote" ]
        )) "flake.nix binaries differ from BINARIES in diri/scripts/linux-packager-config.py";
        binaries;

      diri =
        {
          lib,
          rust-bin,
          makeRustPlatform,
          pkgsStatic,
          pkg-config,
          patchelf,
          imagemagick,
          copyDesktopItems,
          makeDesktopItem,
          fontconfig,
          freetype,
          libxkbcommon,
          libxcb,
          vulkan-loader,
          wayland,
        }:
        let
          version = (lib.importTOML ./diri/crates/diri-app/Cargo.toml).package.version;
          # Only what the build reads, so documentation and script edits do
          # not rebuild the app.
          src = lib.fileset.toSource {
            root = ./diri;
            fileset = lib.fileset.unions [
              ./diri/.cargo
              ./diri/Cargo.toml
              ./diri/Cargo.lock
              ./diri/assets
              ./diri/crates
              ./diri/vendor
            ];
          };

          toolchain = rust-bin.fromRustupToolchainFile toolchainFile;
          rustPlatform = makeRustPlatform {
            cargo = toolchain;
            rustc = toolchain;
          };
          # Git dependencies (GPUI, xcb, ...) are fetched from the revisions
          # pinned in Cargo.lock, so dependency bumps need no hash updates here.
          # Vendored once and shared with the static Helper.
          cargoDeps = rustPlatform.importCargoLock {
            lockFile = ./diri/Cargo.lock;
            allowBuiltinFetchGit = true;
          };

          # The Engine uploads this Helper to SSH hosts, which need not have
          # Nix, so it is static like the release's musl Helpers.
          # Cross-compiled with the musl target rather than through pkgsStatic,
          # whose same-architecture build links glibc build scripts with the
          # musl compiler and they crash.
          staticTarget = pkgsStatic.stdenv.hostPlatform.rust.rustcTarget;
          staticToolchain = toolchain.override { targets = [ staticTarget ]; };
          remoteHelper =
            (makeRustPlatform {
              cargo = staticToolchain;
              rustc = staticToolchain;
            }).buildRustPackage
              {
                pname = "diri-remote";
                inherit version src cargoDeps;
                env."CARGO_TARGET_${lib.toUpper (builtins.replaceStrings [ "-" ] [ "_" ] staticTarget)}_LINKER" =
                  "${pkgsStatic.stdenv.cc}/bin/${pkgsStatic.stdenv.cc.targetPrefix}cc";
                buildPhase = ''
                  runHook preBuild
                  cargo build --release --locked --offline -j $NIX_BUILD_CORES \
                    --target ${staticTarget} --package diri-remote --bin diri-remote
                  runHook postBuild
                '';
                installPhase = ''
                  runHook preInstall
                  install -Dm755 target/${staticTarget}/release/diri-remote $out/bin/diri-remote
                  runHook postInstall
                '';
                doCheck = false;
              };
        in
        rustPlatform.buildRustPackage {
          pname = "diri";
          inherit version src cargoDeps;

          buildPhase = ''
            runHook preBuild
            ${lib.concatMapStringsSep "\n" (
              build:
              "cargo build --release --locked --offline -j $NIX_BUILD_CORES --package ${build.package} "
              + lib.concatMapStringsSep " " (bin: "--bin ${bin}") build.bins
            ) builds}
            runHook postBuild
          '';

          # Engine tests spawn real PTYs, login shells and Git repositories,
          # which the build sandbox does not provide. CI runs them.
          doCheck = false;

          nativeBuildInputs = [
            pkg-config
            patchelf
            imagemagick
            copyDesktopItems
          ];

          buildInputs = [
            fontconfig
            freetype
            libxkbcommon
            libxcb
          ];

          desktopItems = [
            (makeDesktopItem {
              name = "diri";
              desktopName = "diri";
              comment = "Run and monitor coding agents in local and direct SSH sessions";
              exec = "diri";
              icon = "diri";
              categories = [ "Development" ];
              startupNotify = true;
              startupWMClass = "com.dirijor.diri";
            })
          ];

          # Same layout as the Linux packages: the Engine finds its siblings
          # next to its executable and its resources in <prefix>/lib/diri.
          installPhase = ''
            runHook preInstall
            for bin in ${lib.escapeShellArgs checkedBinaries}; do
              install -Dm755 "target/release/$bin" "$out/bin/$bin"
            done
            install -Dm755 ${remoteHelper}/bin/diri-remote $out/bin/diri-remote

            mkdir -p $out/lib/diri
            cp -r crates/diri-engine/manifests $out/lib/diri/manifests
            # Count, not just presence, as in scripts/package.sh: a missing
            # manifest silently downgrades that agent to a bare login shell.
            source_manifests=$(find crates/diri-engine/manifests -name '*.json' -type f | wc -l)
            bundled_manifests=$(find $out/lib/diri/manifests -name '*.json' -type f | wc -l)
            if [[ ! -f $out/lib/diri/manifests/codex.json || $bundled_manifests != "$source_manifests" || $bundled_manifests -lt 20 ]]; then
              echo "error: bundled Agent catalog is incomplete: $bundled_manifests manifest(s) bundled, $source_manifests in source (expected at least 20)" >&2
              exit 1
            fi

            # hicolor has no 1024x1024 directory, so ship the sizes it lists.
            for size in 512 256 128 64 48 32; do
              mkdir -p $out/share/icons/hicolor/''${size}x''${size}/apps
              magick assets/icon.png -resize ''${size}x''${size} \
                $out/share/icons/hicolor/''${size}x''${size}/apps/diri.png
            done
            runHook postInstall
          '';

          # A RUNPATH rather than an LD_LIBRARY_PATH wrapper: the Engine
          # inherits diri's environment and passes it to every terminal it
          # starts, so a wrapper would leak these paths into user sessions.
          postFixup = ''
            patchelf --add-rpath ${
              lib.makeLibraryPath [
                vulkan-loader
                wayland
                libxkbcommon
              ]
            } $out/bin/diri
          '';

          passthru = { inherit toolchain; };

          meta = {
            description = "Run and monitor coding agents in local and SSH sessions";
            homepage = "https://github.com/cristicretu/diri";
            license = lib.licenses.agpl3Only;
            mainProgram = "diri";
            platforms = [ "x86_64-linux" ];
          };
        };
    in
    {
      overlays.default = lib.composeManyExtensions [
        rust-overlay.overlays.default
        (final: _prev: { diri = final.callPackage diri { }; })
      ];

      packages.${system} = {
        diri = pkgs.callPackage diri { };
        default = self.packages.${system}.diri;
      };

      apps.${system}.default = {
        type = "app";
        program = "${self.packages.${system}.diri}/bin/diri";
        meta.description = "Launch the diri desktop app";
      };

      devShells.${system}.default = pkgs.mkShell {
        inputsFrom = [ self.packages.${system}.diri ];
        packages = [
          (self.packages.${system}.diri.toolchain.override {
            extensions = [
              "rust-src"
              "rust-analyzer"
            ];
          })
        ];
        # `cargo run` binaries are not patched. A RUNPATH, not LD_LIBRARY_PATH,
        # lets them find GPUI's dlopened libraries: a dev Engine passes its
        # environment to every terminal it starts.
        CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS = "-C link-arg=-Wl,-rpath,${lib.makeLibraryPath (runtimeLibraries pkgs)}";
      };
    };
}
