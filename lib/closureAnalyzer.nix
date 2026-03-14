{ runCommand, nix2flatpak-scripts }:

{ package, runtimeIndex }:

runCommand "dedup-plan-${package.name or "unknown"}" {
  exportReferencesGraph = [ "closure" package ];
} ''
  ${nix2flatpak-scripts}/bin/nix2flatpak-analyze-closure \
    --package ${package} \
    --runtime-index ${runtimeIndex} \
    --closure-file closure \
    --output $out
''
