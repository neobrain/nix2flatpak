{ lib, stdenv, nix2flatpak-scripts, patchelf, ostree, flatpak, file
, callPackage
}:

{ appId
, package
, runtime                    # e.g., "org.kde.Platform//6.10"
, runtimeIndex               # path to runtime-index.json
, command ? package.meta.mainProgram or (lib.getName package)
, sdk ? null                 # default: inferred from runtime
, permissions ? {}
, desktopFile ? null
, icon ? null
, appdata ? null
, extraLibs ? []
, trustRuntime ? []
, extraEnv ? {}
}:

let
  # Parse runtime string
  runtimeParts = lib.splitString "//" runtime;
  runtimeName = builtins.elemAt runtimeParts 0;
  runtimeBranch = builtins.elemAt runtimeParts 1;

  # Flatpak arch
  archMap = {
    "x86_64-linux" = "x86_64";
    "aarch64-linux" = "aarch64";
    "i686-linux" = "i386";
  };
  flatpakArch = archMap.${stdenv.hostPlatform.system} or
    (throw "Unsupported system: ${stdenv.hostPlatform.system}");

  # Architecture triplet for patchelf
  tripletMap = {
    "x86_64-linux" = "x86_64-linux-gnu";
    "aarch64-linux" = "aarch64-linux-gnu";
    "i686-linux" = "i386-linux-gnu";
  };
  archTriplet = tripletMap.${stdenv.hostPlatform.system} or
    (throw "Unsupported system: ${stdenv.hostPlatform.system}");

  branch = "stable";

  generateMetadata = callPackage ./metadata.nix { };

  metadata = generateMetadata {
    inherit appId runtime sdk command permissions extraEnv;
    system = stdenv.hostPlatform.system;
  };

in stdenv.mkDerivation {
  pname = "flatpak-${appId}";
  version = package.version or "0";

  dontUnpack = true;
  dontFixup = true;

  nativeBuildInputs = [ nix2flatpak-scripts patchelf ostree flatpak file ];

  exportReferencesGraph = [ "closure" package ];

  buildPhase = ''
    runHook preBuild

    echo "=== Step 1: Analyzing closure ==="
    nix2flatpak-analyze-closure \
      --package ${package} \
      --runtime-index ${runtimeIndex} \
      --closure-file closure \
      --output dedup-plan.json

    echo "=== Step 2: Rewriting files ==="
    mkdir -p flatpak-build/files
    nix2flatpak-rewrite-for-flatpak \
      --dedup-plan dedup-plan.json \
      --output-dir flatpak-build/files \
      --arch-triplet ${archTriplet} \
      --patchelf ${patchelf}/bin/patchelf \
      --runtime-index ${runtimeIndex}

    echo "=== Step 3: Setting up metadata ==="
    cp ${metadata} flatpak-build/metadata

    echo "=== Step 4: Desktop integration ==="
    mkdir -p flatpak-build/export/share/applications
    mkdir -p flatpak-build/export/share/icons

    # Copy .desktop file — Flatpak requires it to be named ${appId}.desktop
    ${if desktopFile != null then ''
      cp ${desktopFile} flatpak-build/export/share/applications/${appId}.desktop
    '' else ''
      # Auto-detect from package — take the first .desktop file found
      if [ -d "${package}/share/applications" ]; then
        for f in ${package}/share/applications/*.desktop; do
          if [ -f "$f" ]; then
            cp "$f" flatpak-build/export/share/applications/${appId}.desktop
            # Also copy to files/share for the app to find
            mkdir -p flatpak-build/files/share/applications
            cp "$f" flatpak-build/files/share/applications/${appId}.desktop
            break
          fi
        done
      fi
    ''}

    # Rewrite desktop files in export
    for f in flatpak-build/export/share/applications/*.desktop; do
      if [ -f "$f" ]; then
        sed -i \
          -e 's|Exec=/nix/store/[^/]*/bin/||' \
          -e 's|TryExec=/nix/store/[^/]*/bin/||' \
          -e 's|^Icon=.*|Icon=${appId}|' \
          "$f"
      fi
    done

    # Copy icons (only PNG — SVG validation requires gdk-pixbuf SVG loader
    # which is unavailable in the nix build sandbox)
    ${if icon != null then ''
      # TODO: detect icon size and format
      mkdir -p flatpak-build/export/share/icons/hicolor/scalable/apps
      cp ${icon} flatpak-build/export/share/icons/hicolor/scalable/apps/${appId}.svg
    '' else ''
      if [ -d "${package}/share/icons" ]; then
        find ${package}/share/icons -name "*.png" | while read -r pngfile; do
          relpath="''${pngfile#${package}/share/icons/}"
          destdir="flatpak-build/export/share/icons/$(dirname "$relpath")"
          mkdir -p "$destdir"
          # Rename icon to match app ID (Flatpak requirement)
          cp "$pngfile" "$destdir/${appId}.png"
        done
      fi
    ''}

    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall

    mkdir -p $out

    echo "=== Step 5: Creating OSTree repo and Flatpak bundle ==="
    ostree --repo=$out/repo init --mode=archive

    # Use flatpak build-export instead of raw ostree commit
    # This sets xa.metadata and other Flatpak-specific commit metadata
    flatpak build-export \
      --disable-sandbox \
      --subject="nix2flatpak build of ${appId}" \
      $out/repo \
      flatpak-build \
      ${branch}

    flatpak build-bundle \
      $out/repo \
      $out/${appId}.flatpak \
      ${appId} \
      ${branch}

    # Keep unpacked dir for inspection/testing
    cp -r flatpak-build $out/flatpak-dir

    # Build info
    cat > $out/build-info.json << 'BUILDINFO'
    {
      "appId": "${appId}",
      "runtime": "${runtime}",
      "command": "${command}",
      "nixPackage": "${package.name or "unknown"}",
      "bundleFile": "${appId}.flatpak"
    }
    BUILDINFO

    runHook postInstall
  '';

  meta = {
    description = "Flatpak bundle of ${appId}";
  };
}
