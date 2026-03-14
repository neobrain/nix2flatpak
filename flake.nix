{
  description = "nix2flatpak — convert Nix packages into Flatpak images using proper runtimes";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};

        nix2flatpak-scripts = pkgs.rustPlatform.buildRustPackage {
          pname = "nix2flatpak-scripts";
          version = "0.1.0";
          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./src
            ];
          };
          cargoLock.lockFile = ./Cargo.lock;
        };

        mkFlatpak = pkgs.callPackage ./lib/mkFlatpak.nix {
          inherit nix2flatpak-scripts;
          inherit (pkgs) patchelf ostree flatpak file;
        };

      in {
        lib = {
          inherit mkFlatpak;
        };

        packages = {
          inherit nix2flatpak-scripts;

          # Example: KCalc as a Flatpak
          kcalc-flatpak = mkFlatpak {
            appId = "org.kde.kcalc";
            package = pkgs.kdePackages.kcalc;
            runtime = "org.kde.Platform//6.10";
            runtimeIndex = ./runtimes/org.kde.Platform/6.10/runtime-index.json;
            permissions = {
              share = [ "ipc" ];
              sockets = [ "fallback-x11" "wayland" "pulseaudio" ];
              devices = [ "dri" ];
            };
          };

          # Dolphin Emulator as a Flatpak
          dolphin-emu-flatpak = mkFlatpak {
            appId = "org.DolphinEmu.dolphin-emu";
            package = pkgs.dolphin-emu;
            runtime = "org.kde.Platform//6.10";
            runtimeIndex = ./runtimes/org.kde.Platform/6.10/runtime-index.json;
            command = "dolphin-emu";
            permissions = {
              share = [ "network" "ipc" ];
              sockets = [ "x11" "wayland" "pulseaudio" ];  # x11 needed: Dolphin forces xcb
              devices = [ "all" ];  # gamepads, USB adapters
              filesystems = [ "host:ro" ];  # access game files
            };
          };
        };

        checks = {
          # Structural validation of the KCalc Flatpak build
          kcalc-flatpak-structure = pkgs.callPackage ./tests/kcalc-flatpak.nix {
            kcalc-flatpak = self.packages.${system}.kcalc-flatpak;
            inherit (pkgs) patchelf file;
          };
        };

        devShells.default = pkgs.mkShell {
          packages = [
            pkgs.cargo
            pkgs.rustc
            pkgs.patchelf
            pkgs.ostree
            pkgs.flatpak
            pkgs.file
          ];
        };
      }
    ) // {
      # Non-system-specific outputs
      overlays = {
        # Placeholder — populated once runtime indexes are generated
        # org_kde_Platform_6_8 = import ./lib/overlays.nix { ... };
      };
    };
}
