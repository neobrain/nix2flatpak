{ runCommand, patchelf, file, kcalc }:

runCommand "test-kcalc-structure" {
  nativeBuildInputs = [ patchelf file ];
} ''
  set -euo pipefail

  flatpak_dir="${kcalc}/flatpak-dir"
  bundle="${kcalc}/org.kde.kcalc.flatpak"

  echo "=== Test 1: metadata file exists and has correct app ID ==="
  grep -q "name=org.kde.kcalc" "$flatpak_dir/metadata"
  echo "PASS"

  echo "=== Test 2: metadata references org.kde.Platform runtime ==="
  grep -q "runtime=org.kde.Platform" "$flatpak_dir/metadata"
  echo "PASS"

  echo "=== Test 3: metadata has command=kcalc ==="
  grep -q "command=kcalc" "$flatpak_dir/metadata"
  echo "PASS"

  echo "=== Test 4: app binary exists ==="
  test -e "$flatpak_dir/files/bin/kcalc"
  echo "PASS"

  echo "=== Test 5: .flatpak bundle exists and is non-empty ==="
  test -s "$bundle"
  echo "PASS"

  echo "=== Test 6: no ELF binary has /nix/store as interpreter ==="
  fail=0
  while IFS= read -r f; do
    if file "$f" | grep -q "ELF"; then
      interp=$(patchelf --print-interpreter "$f" 2>/dev/null || true)
      if echo "$interp" | grep -q "/nix/store"; then
        echo "FAIL: $f has nix store interpreter: $interp"
        fail=1
      fi
    fi
  done < <(find "$flatpak_dir/files" -type f -executable)
  if [ "$fail" -eq 1 ]; then exit 1; fi
  echo "PASS"

  echo "=== Test 7: no ELF binary has bare /nix/store in RPATH ==="
  fail=0
  while IFS= read -r f; do
    if file "$f" | grep -q "ELF"; then
      rpath=$(patchelf --print-rpath "$f" 2>/dev/null || true)
      echo "$rpath" | tr ':' '\n' | while read -r entry; do
        if echo "$entry" | grep -q "^/nix/store"; then
          echo "FAIL: $f has bare /nix/store RPATH entry: $entry"
          exit 1
        fi
      done
    fi
  done < <(find "$flatpak_dir/files" -type f -executable)
  echo "PASS"

  echo "=== Test 8: bundle size is reasonable ==="
  bundle_size=$(stat -c%s "$bundle")
  max_size=$((500 * 1024 * 1024))
  bundle_mb=$(($bundle_size / 1024 / 1024))
  if [ "$bundle_size" -gt "$max_size" ]; then
    echo "FAIL: bundle is ''${bundle_mb} MiB, expected < 500 MiB"
    exit 1
  fi
  echo "PASS — bundle is ''${bundle_mb} MiB"

  echo "=== All tests passed ==="
  mkdir -p $out
  echo "PASS" > $out/result
''
