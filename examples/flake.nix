{
  description = "nix2flatpak examples — nixpkgs packages converted to Flatpak bundles";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    nix2flatpak.url = "github:neobrain/nix2flatpak";
  };

  outputs = { self, nixpkgs, flake-utils, nix2flatpak }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};

        mkFlatpak = nix2flatpak.lib.${system}.mkFlatpak;

        # pkgs with insecure olm allowed and lighter deps
        pkgsForNeochat = import nixpkgs {
          inherit system;
          config.permittedInsecurePackages = [ "olm-3.2.16" ];
          overlays = [
            (final: prev: {
              kdePackages = prev.kdePackages.overrideScope (kfinal: kprev: {
                # Build kquickimageeditor without opencv (~44 MB with openblas).
                # OpenCV is optional; the official KDE Flatpak builds without it.
                kquickimageeditor = kprev.kquickimageeditor.overrideAttrs (old: {
                  buildInputs = builtins.filter
                    (dep: !(dep ? pname && dep.pname == "opencv"))
                    old.buildInputs;
                });
              });
            })
          ];
        };

      in {
        packages = {
          # GNOME Calculator (GNOME runtime)
          gnome-calculator = mkFlatpak {
            appId = "org.gnome.Calculator";
            package = pkgs.gnome-calculator;
            runtime = "org.gnome.Platform//49";
            runtimeIndex = ../runtimes/org.gnome.Platform/49/runtime-index.json;
            permissions = {
              share = [ "ipc" ];
              sockets = [ "fallback-x11" "wayland" ];
              devices = [ "dri" ];
            };
          };

          # KCalc (KDE calculator)
          kcalc = mkFlatpak {
            appId = "org.kde.kcalc";
            package = pkgs.kdePackages.kcalc;
            runtime = "org.kde.Platform//6.10";
            runtimeIndex = ../runtimes/org.kde.Platform/6.10/runtime-index.json;
            permissions = {
              share = [ "ipc" ];
              sockets = [ "fallback-x11" "wayland" "pulseaudio" ];
              devices = [ "dri" ];
            };
            skipAbiChecks = true;
          };

          # NeoChat (KDE Matrix client)
          neochat = mkFlatpak {
            appId = "org.kde.neochat";
            package = pkgsForNeochat.kdePackages.neochat.override {
              # QtWebView is optional and pulls in QtWebEngine (~375 MB of Chromium).
              # The official KDE Flatpak builds without it too.
              qtwebview = null;
            };
            runtime = "org.kde.Platform//6.10";
            runtimeIndex = ../runtimes/org.kde.Platform/6.10/runtime-index.json;
            permissions = {
              share = [ "network" "ipc" ];
              sockets = [ "fallback-x11" "wayland" "pulseaudio" ];
              devices = [ "dri" ];
              filesystems = [ "xdg-download" ];
            };
            skipAbiChecks = true;
          };

          # Signal Desktop (Electron/GNOME runtime)
          signal-desktop = mkFlatpak {
            appId = "org.signal.Signal";
            package = pkgs.signal-desktop;
            runtime = "org.gnome.Platform//49";
            runtimeIndex = ../runtimes/org.gnome.Platform/49/runtime-index.json;
            command = "signal-desktop";
            extraEnv = {
              ELECTRON_DISABLE_SANDBOX = "1";  # SUID sandbox doesn't work inside Flatpak
            };
            permissions = {
              share = [ "network" "ipc" ];
              sockets = [ "x11" "wayland" "pulseaudio" ];
              devices = [ "all" ];  # camera, microphone
              filesystems = [ "xdg-download" ];
              talk-names = [ "org.freedesktop.Notifications" "org.freedesktop.secrets" ];
            };
            skipAbiChecks = true;
          };

          # Processing (Java creative coding IDE)
          # TODO: Investigate using extension runtimes (org.freedesktop.Sdk.Extension.openjdk17)
          #       to avoid bundling JDKs.
          processing = mkFlatpak {
            appId = "org.processing.Processing";
            appName = "Processing IDE";
            developer = "Processing Foundation";

            # Override batik to use jdk17 instead of the default jre (jdk21).
            # Processing runs on jdk17; batik's CLI wrappers reference jre,
            # which pulls jdk21 (~400 MB) into the closure even though
            # processing only uses the batik *library*.
            package = pkgs.processing.override {
              batik = pkgs.batik.override { jre = pkgs.jdk17; };
            };
            runtime = "org.gnome.Platform//49";
            runtimeIndex = ../runtimes/org.gnome.Platform/49/runtime-index.json;
            command = "Processing";
            icon = "${pkgs.processing}/lib/app/resources/lib/icons/app-256.png";
            permissions = {
              share = [ "network" "ipc" ];  # network for library downloads
              sockets = [ "x11" "wayland" ];
              devices = [ "dri" ];
              filesystems = [ "home" ];  # sketches stored in ~/sketchbook
            };
            skipAbiChecks = true;
          };

          # Dolphin
          dolphin-emu = mkFlatpak {
            appId = "org.DolphinEmu.dolphin-emu";
            package = pkgs.dolphin-emu;
            runtime = "org.kde.Platform//6.10";
            runtimeIndex = ../runtimes/org.kde.Platform/6.10/runtime-index.json;
            command = "dolphin-emu";
            permissions = {
              share = [ "network" "ipc" ];
              sockets = [ "x11" "wayland" "pulseaudio" ];  # x11 needed: Dolphin forces xcb
              devices = [ "all" ];  # gamepads, USB adapters
              filesystems = [ "host:ro" ];  # access game files
            };
            skipAbiChecks = true;
          };
        };

        checks = {
          kcalc-structure = pkgs.callPackage ../tests/kcalc.nix {
            kcalc = self.packages.${system}.kcalc;
            inherit (pkgs) patchelf file;
          };

          gnome-calculator-structure = pkgs.callPackage ../tests/gnome-calculator.nix {
            gnome-calculator = self.packages.${system}.gnome-calculator;
            inherit (pkgs) patchelf file;
          };
        };
      }
    );
}
