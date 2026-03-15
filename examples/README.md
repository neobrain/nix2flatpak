# nix2flatpak examples

These examples illustrate how to turn nixpkgs packages into Flatpak bundles
while minimizing unnecessary Nix store dependencies. Each package is built
against a Flatpak runtime (KDE or GNOME), and the build pipeline deduplicates
libraries already provided by the runtime — shipping only the delta. Optional
heavyweight dependencies like QtWebEngine and OpenCV are disabled where possible,
matching what official Flatpak maintainers do.

## GNOME Calculator

```sh
flatpak install --user $(nix build .#gnome-calculator-flatpak --no-link --print-out-paths)/*.flatpak
```

## KCalc (KDE calculator)

```sh
flatpak install --user $(nix build .#kcalc-flatpak --no-link --print-out-paths)/*.flatpak
```

## NeoChat (KDE Matrix client)

```sh
flatpak install --user $(nix build .#neochat-flatpak --no-link --print-out-paths)/*.flatpak
```

## Dolphin (emulator)

```sh
flatpak install --user $(nix build .#dolphin-emu-flatpak --no-link --print-out-paths)/*.flatpak
```
