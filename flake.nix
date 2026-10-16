{
  description = "Diri: run and monitor coding agents in local and SSH sessions";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      # Only the platform this flake is built and run on. The Linux beta also
      # supports aarch64; add it here once someone verifies it.
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};

      diri =
        {
          lib,
          rustPlatform,
          pkgsStatic,
          pkg-config,
          patchelf,
          copyDesktopItems,
          makeDesktopItem,
          fontconfig,
          freetype,
          libxkbcommon,
          wayland,
          vulkan-loader,
          libxcb,
        }:
        let
          version = (lib.importTOML ./diri/crates/diri-app/Cargo.toml).package.version;
          src = ./diri;
          # Git dependencies (GPUI, xcb, ...) are fetched from the revisions
          # pinned in Cargo.lock, so dependency bumps need no hash updates here.
          cargoLock = {
            lockFile = ./diri/Cargo.lock;
            allowBuiltinFetchGit = true;
          };

          # The Engine uploads this Helper to SSH hosts, which need not have
          # Nix, so it is static like the release's musl Helpers.
          remoteHelper = pkgsStatic.rustPlatform.buildRustPackage {
            pname = "diri-remote";
            inherit version src cargoLock;
            cargoBuildFlags = [
              "--package"
              "diri-remote"
            ];
            doCheck = false;
          };

          # What scripts/package-linux.sh ships in usr/bin, less diri-remote.
          binaries = [
            "diri"
            "dirijor"
            "dirijor-mcp"
            "dirijord-rs"
            "diri-holder"
            "diri-ssh-askpass"
          ];
          # GPUI dlopens these at runtime.
          runtimeLibraries = [
            vulkan-loader
            wayland
            libxkbcommon
          ];
        in
        rustPlatform.buildRustPackage {
          pname = "diri";
          inherit version src cargoLock;

          cargoBuildFlags = lib.concatMap (name: [
            "--bin"
            name
          ]) binaries;

          # Engine tests spawn real PTYs, login shells and Git repositories,
          # which the build sandbox does not provide. CI runs them.
          doCheck = false;

          nativeBuildInputs = [
            pkg-config
            patchelf
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
          postInstall = ''
            install -Dm755 ${remoteHelper}/bin/diri-remote $out/bin/diri-remote
            mkdir -p $out/lib/diri
            cp -r crates/diri-engine/manifests $out/lib/diri/manifests
            install -Dm644 assets/icon.png $out/share/icons/hicolor/1024x1024/apps/diri.png
          '';

          # A RUNPATH rather than an LD_LIBRARY_PATH wrapper: the Engine
          # inherits diri's environment and passes it to every terminal it
          # starts, so a wrapper would leak these paths into user sessions.
          postFixup = ''
            patchelf --add-rpath ${lib.makeLibraryPath runtimeLibraries} $out/bin/diri
          '';

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
      overlays.default = final: _prev: {
        diri = final.callPackage diri { };
      };

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
        packages = with pkgs; [
          cargo
          rustc
          clippy
          rustfmt
          rust-analyzer
        ];
        # `cargo run` binaries are not patched; let them find GPUI's dlopened
        # libraries the usual way.
        LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (
          with pkgs;
          [
            vulkan-loader
            wayland
            libxkbcommon
          ]
        );
      };
    };
}
