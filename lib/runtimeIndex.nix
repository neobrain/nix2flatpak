{ runCommand, nix2flatpak-scripts }:

{ runtimePath }:

runCommand "runtime-index" { } ''
  ${nix2flatpak-scripts}/bin/nix2flatpak-index-runtime ${runtimePath} --output $out
''
