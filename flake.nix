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

        # Python with dependencies for our scripts
        scriptsPython = pkgs.python3.withPackages (ps: [
          ps.pyelftools
        ]);

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
