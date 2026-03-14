{ runCommand, nix2flatpak-scripts, patchelf, file }:

{ dedupPlan, archTriplet ? "x86_64-linux-gnu" }:

runCommand "flatpak-rewritten" {
  nativeBuildInputs = [ patchelf file ];
} ''
  ${nix2flatpak-scripts}/bin/nix2flatpak-rewrite-for-flatpak \
    --dedup-plan ${dedupPlan} \
    --output-dir $out \
    --arch-triplet ${archTriplet} \
    --patchelf ${patchelf}/bin/patchelf
''
