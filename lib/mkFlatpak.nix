{ lib, stdenv, nix2flatpak-scripts, patchelf, ostree, flatpak, file, librsvg
, callPackage
, runtimesDir ? null         # path to runtimes/ directory for auto-lookup
}:

{ appId
, package
, runtime                    # e.g., "org.kde.Platform/6.10"
, runtimeIndex ? null        # path to runtime-index.json (inferred from runtime if omitted)
, command ? package.meta.mainProgram or (lib.getName package)
, sdk ? null                 # default: inferred from runtime
, permissions ? {}
, icon ? null
, appdata ? null
, appName ? null              # display name (default: read from .desktop Name=)
, developer ? null            # upstream developer/author name
, extraLibs ? []
, extraEnv ? {}
, skipAbiChecks ? false       # bypass glibc/libstdc++/Qt version checks
}:

let
  # Parse runtime string
  runtimeParts = lib.splitString "/" runtime;
  runtimeName = builtins.elemAt runtimeParts 0;
  runtimeBranch = builtins.elemAt runtimeParts 1;

  resolvedRuntimeIndex =
    if runtimeIndex != null then runtimeIndex
    else if runtimesDir != null then runtimesDir + "/${runtimeName}/${runtimeBranch}/runtime-index.json"
    else throw "mkFlatpak: either runtimeIndex or runtimesDir must be provided";

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

  generateAppstream = callPackage ./appstream.nix { };

  fallbackAppstream = generateAppstream {
    inherit appId package appName developer;
  };

in stdenv.mkDerivation {
  pname = "flatpak-${appId}";
  version = package.version or "0";

  dontUnpack = true;
  dontFixup = true;

  nativeBuildInputs = [ nix2flatpak-scripts patchelf ostree flatpak file librsvg ];

  exportReferencesGraph = [ "closure" package ];

  buildPhase = ''
    runHook preBuild

    echo "=== Step 1: Analyzing closure ==="
    nix2flatpak-analyze-closure \
      --package ${package} \
      --runtime-index ${resolvedRuntimeIndex} \
      --closure-file closure \
      --output dedup-plan.json \
      ${lib.optionalString skipAbiChecks "--warn-abi-only"}

    echo "=== Step 2: Rewriting files ==="
    mkdir -p flatpak-build/files
    nix2flatpak-rewrite-for-flatpak \
      --dedup-plan dedup-plan.json \
      --output-dir flatpak-build/files \
      --arch-triplet ${archTriplet} \
      --patchelf ${patchelf}/bin/patchelf \
      --runtime-index ${resolvedRuntimeIndex}

    echo "=== Step 3: Setting up metadata ==="
    cp ${metadata} flatpak-build/metadata

    echo "=== Step 4: Desktop integration ==="
    mkdir -p flatpak-build/export/share/applications
    mkdir -p flatpak-build/export/share/icons

    # Flatpak only exports desktop files named $FLATPAK_ID.desktop,
    # $FLATPAK_ID.foo.desktop or $FLATPAK_ID-foo.desktop
    
    for f in ${package}/share/applications/${appId}{,.*,-*}.desktop; do
      [ -f "$f" ] || continue
      name=$(basename "$f")
      sed \
        -e 's|^TryExec=/nix/store/[^/]*/bin/|TryExec=|' \
        -e 's|^Exec=/nix/store/[^/]*/bin/|Exec=|' \
        -e "s|^Icon=.*|Icon=${appId}|" \
        "$f" > "flatpak-build/export/share/applications/$name"
      install -D -m644 \
        "flatpak-build/export/share/applications/$name" \
        "flatpak-build/files/share/applications/$name"
    done

    # Copy icons
    ${if icon != null then ''
      iconFile="${icon}"
      case "$iconFile" in
        *.svg)
          mkdir -p flatpak-build/export/share/icons/hicolor/scalable/apps
          cp --no-preserve=mode "$iconFile" flatpak-build/export/share/icons/hicolor/scalable/apps/${appId}.svg
          ;;
        *.png)
          # Detect PNG dimensions for proper hicolor directory
          iconSize=$(file "$iconFile" | grep -oP '\d+ x \d+' | head -1 | cut -d' ' -f1)
          iconSize=''${iconSize:-256}
          mkdir -p "flatpak-build/export/share/icons/hicolor/''${iconSize}x''${iconSize}/apps"
          cp --no-preserve=mode "$iconFile" "flatpak-build/export/share/icons/hicolor/''${iconSize}x''${iconSize}/apps/${appId}.png"
          ;;
        *)
          echo "ERROR: Unsupported icon format: $iconFile (must be .svg or .png)"
          exit 1
          ;;
      esac
    '' else ''
      if [ -d "${package}/share/icons" ]; then
        # Fixed-size PNG icons
        find ${package}/share/icons -name "*.png" | while read -r pngfile; do
          # Flatpak rejects icons larger than 512x512
          if echo "$pngfile" | grep -qE '(1024x1024|2048x2048)'; then
            continue
          fi
          relpath="''${pngfile#${package}/share/icons/}"
          destdir="flatpak-build/export/share/icons/$(dirname "$relpath")"
          mkdir -p "$destdir"
          # Rename icon to match app ID (Flatpak requirement)
          cp --no-preserve=mode "$pngfile" "$destdir/${appId}.png"
        done

        # Vector SVG icons
        find ${package}/share/icons -name "*.svg" | while read -r svgfile; do
          relpath="''${svgfile#${package}/share/icons/}"
          destdir="flatpak-build/export/share/icons/$(dirname "$relpath")"
          mkdir -p "$destdir"
          cp --no-preserve=mode "$svgfile" "$destdir/${appId}.svg"
        done
      fi
    ''}

    # Copy D-Bus services
    mkdir -p flatpak-build/export/share/dbus-1/services

    if [ -d "${package}/share/dbus-1/services" ]; then
      for svcFile in ${package}/share/dbus-1/services/*.service; do
        if [ -f "$svcFile" ]; then
          busName=$(grep -m1 '^Name=' "$svcFile" | cut -d= -f2-)

          if [ -z "$busName" ]; then
            echo "ERROR: D-Bus service file '$svcFile' has no Name= key"
            exit 1
          fi

          origExec=$(grep -m1 '^Exec=' "$svcFile" | cut -d= -f2-)

          if [ -z "$origExec" ]; then
            echo "ERROR: D-Bus service file '$svcFile' has no Exec= key"
            exit 1
          fi

          install -D -m 644 "$svcFile" "flatpak-build/files/share/dbus-1/services/$busName.service"

          rewrittenExec=$(echo "$origExec" | sed -E 's|^(/nix/store/[^/]+/bin/)([^ ]+)|\2|')
          sed "s|^Exec=.*|Exec=$rewrittenExec|" "$svcFile" \
            > "flatpak-build/export/share/dbus-1/services/$busName.service"
        fi
      done
    fi

    # Copy AppStream metadata (user-provided > package-provided > generated from nixpkgs meta)
    mkdir -p flatpak-build/export/share/metainfo
    metainfoFile="flatpak-build/export/share/metainfo/${appId}.metainfo.xml"
    ${if appdata != null then ''
      cp --no-preserve=mode ${appdata} "$metainfoFile"
    '' else ''
      appstreamFound=false
      for metainfoDir in "${package}/share/metainfo" "${package}/share/appdata"; do
        if [ -d "$metainfoDir" ]; then
          for f in "$metainfoDir"/*.xml; do
            if [ -f "$f" ]; then
              cp --no-preserve=mode "$f" "$metainfoFile"
              appstreamFound=true
              break 2
            fi
          done
        fi
      done
      if [ "$appstreamFound" = false ]; then
        cp --no-preserve=mode ${fallbackAppstream} "$metainfoFile"
      fi
    ''}

    # Inject <name> from the .desktop file if the metainfo doesn't already have one
    if ! grep -q '<name>' "$metainfoFile"; then
      desktopName=""
      deskFile="flatpak-build/export/share/applications/${appId}.desktop"
      if [ -f "$deskFile" ]; then
        desktopName=$(grep -m1 '^Name=' "$deskFile" | cut -d= -f2-)
      fi
      if [ -n "$desktopName" ]; then
        sed -i "s|</id>|</id>\n  <name>$desktopName</name>|" "$metainfoFile"
      fi
    fi

    # Generate app-info catalog (gzipped AppStream collection XML).
    # flatpak build-export --update-appstream looks for this in files/share/app-info/xmls/.
    mkdir -p flatpak-build/files/share/app-info/xmls

    # Copy icons into app-info for the appstream branch.
    # build-export looks for icons under files/share/app-info/icons/flatpak/{64x64,128x128}/.
    # Find the best available source icon from hicolor and copy it to both sizes.
    # Note that SVG is not supported here, so we must convert to PNG in that case.
    sourceIcon=""
    for candidate in \
      flatpak-build/export/share/icons/hicolor/128x128/apps/${appId}.png \
      flatpak-build/export/share/icons/hicolor/256x256/apps/${appId}.png \
      flatpak-build/export/share/icons/hicolor/512x512/apps/${appId}.png \
      flatpak-build/export/share/icons/hicolor/64x64/apps/${appId}.png \
      flatpak-build/export/share/icons/hicolor/scalable/apps/${appId}.svg; do
      if [ -f "$candidate" ]; then
        sourceIcon="$candidate"
        break
      fi
    done
    if [ -n "$sourceIcon" ]; then
      for sz in 64 128; do
        mkdir -p "flatpak-build/files/share/app-info/icons/flatpak/''${sz}x''${sz}"
        case "$sourceIcon" in
          *.svg)
            ${librsvg}/bin/rsvg-convert -w "$sz" -h "$sz" -o \
              "flatpak-build/files/share/app-info/icons/flatpak/''${sz}x''${sz}/${appId}.png" \
              "$sourceIcon"
            ;;
          *)
            cp "$sourceIcon" "flatpak-build/files/share/app-info/icons/flatpak/''${sz}x''${sz}/${appId}.png"
            ;;
        esac
      done
    fi

    {
      echo '<?xml version="1.0" encoding="UTF-8"?>'
      echo '<components version="1.0">'
      # Strip the XML declaration from the metainfo file before embedding
      sed '1{/<?xml/d}' "$metainfoFile" | sed '/<\/component>/d'
      # Insert icon references if we have icons
      if [ -n "$sourceIcon" ]; then
        echo '  <icon type="cached" width="64" height="64">${appId}.png</icon>'
        echo '  <icon type="cached" width="128" height="128">${appId}.png</icon>'
      fi
      echo '</component>'
      echo '</components>'
    } | gzip > "flatpak-build/files/share/app-info/xmls/${appId}.xml.gz"

    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall

    mkdir -p $out

    # Use bare-user-only mode: content is stored uncompressed, so
    # build-export avoids the costly zlib compression pass.
    echo "=== Step 5: Creating OSTree repo and Flatpak bundle ==="
    ostree --repo=repo init --mode=bare-user-only

    # Make gdk-pixbuf aware of librsvg's SVG loader so flatpak build-export
    # can validate SVG icons.
    export GDK_PIXBUF_MODULE_FILE="${librsvg}/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache"

    # Use flatpak build-export instead of raw ostree commit
    # This sets xa.metadata and other Flatpak-specific commit metadata
    flatpak build-export \
      --disable-sandbox \
      --disable-fsync \
      --update-appstream \
      --subject="nix2flatpak build of ${appId}" \
      repo \
      flatpak-build \
      ${branch}

    flatpak build-bundle \
      repo \
      $out/${appId}.flatpak \
      ${appId} \
      ${branch}

    # Keep unpacked dir for inspection/testing (move, not copy)
    mv flatpak-build $out/flatpak-dir

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
