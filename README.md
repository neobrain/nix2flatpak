# nix2flatpak: Create Flatpak bundles from Nix

This tool lets you distribute Nix packages as Flatpak bundles/repositories, allowing users to easily install your software via `flatpak install` without requiring Nix on their end.

Application binaries are automatically patched to use libraries from the official Flatpak runtimes (KDE/GNOME/…) to minimize application sizes, to enable security updates, and to ensure proper system integration.
Dependencies not present in the runtimes are bundled from the Nix store.

## Usage

Add nix2flatpak as a flake input and call `mkFlatpak`:

```nix
packages.${system}.gnome-calculator = mkFlatpak {
  appId = "org.gnome.Calculator";
  package = pkgs.gnome-calculator;
  runtime = "org.gnome.Platform/49";
  permissions = {
    share = [ "ipc" ];
    sockets = [ "fallback-x11" "wayland" ];
    devices = [ "dri" ];
  };
};
```

Build and install the ~2 MiB bundle:

```sh
nix build .#gnome-calculator
flatpak install --user result/*.flatpak
```

See the [examples](./examples/) directory for complete examples covering GNOME, KDE, Electron, and Java applications. Pre-built runtime indexes are in the [runtimes](./runtimes/) directory.

### mkFlatpak parameters

| Parameter       | Required | Description                                                                              |
| --------------- | -------- | ---------------------------------------------------------------------------------------- |
| `appId`         | yes      | Flatpak application ID (e.g. `org.gnome.Calculator`)                                     |
| `package`       | yes      | Nix derivation to convert                                                                |
| `runtime`       | yes      | Target runtime (e.g. `"org.gnome.Platform/49"`, `"org.kde.Platform/6.10"`)               |
| `runtimeIndex`  |          | Path to the runtime's `runtime-index.json` (default: inferred from runtime)              |
| `command`       |          | Executable to launch (default: `meta.mainProgram` or package name)                       |
| `sdk`           |          | SDK name (default: inferred from runtime)                                                |
| `permissions`   |          | Flatpak sandbox permissions (`share`, `sockets`, `devices`, `filesystems`, `talk-names`) |
| `desktopFile`   |          | Custom `.desktop` file (default: auto-detected from package)                             |
| `icon`          |          | Icon file, PNG or SVG (default: auto-detected from package)                              |
| `appdata`       |          | AppStream metadata file                                                                  |
| `appName`       |          | Flatpak metadata (e.g. `GNOME Calculator`)                                               |
| `developer`     |          | Flatpak metadata (e.g. `The GNOME Project`)                                              |
| `extraEnv`      |          | Extra environment variables (e.g. `{ ELECTRON_DISABLE_SANDBOX = "1"; }`)                 |
| `extraLibs`     |          | Additional store paths to force-include                                                  |
| `skipAbiChecks` |          | Bypass glibc/libstdc++/Qt version compatibility checks (default: `false`)                |

## Tricks

### Reduce bundle bloat with dependency overrides

Some nixpkgs packages pull in heavy optional dependencies that the application doesn't actually need. You can override these away before passing the package to `mkFlatpak`.

For example when building NeoChat, nixpkgs pulls in qtwebview even though it's an optional dependency. Dropping it (like the official NeoChat Flatpak does) saves ~375 MB of disk space:

```nix
package = pkgs.kdePackages.neochat.override {
  qtwebview = null;
};
```

Some more overrides like this are demonstrated in the [examples](./examples/flake.nix).

## How it works

The conversion pipeline has three stages implemented using custom tooling:

1. **Index the Flatpak runtime:** Scans the filesystem for each Flatpak runtime and builds a JSON index of all shared libraries, executables, and data files. This informs the rest of the pipeline about data that is provided by the runtime.

2. **Analyze and trim the Nix closure:** Libraries not present in the runtime must be copied from the Nix store to the bundle. This step walks every Nix store path in the application's Nix closure and compares it against the index of the chosen Flatpak runtime. Any files provided by the runtime are considered redundant and dropped. The remaining data (including the application itself) is copied into a temporary bundle directory.

3. **Rewrite for Flatpak:** All references to Nix store paths are eliminated throughout the bundle directory. Most notably, any ELF binaries are patched using _patchelf_ to resolve libraries: Instead of using the global `/nix/store`, it will either use the Flatpak runtime or the trimmed nix store from step 2 (selected according to the index built in step 1).

Finally, the rewritten tree is committed to an _OSTree_ repository and exported as a `.flatpak` bundle via `flatpak build-export` and `flatpak build-bundle`.

## Disclaimer and compatibility notes

This project is a fun party trick, but replacing an existing Flatpak pipeline with it is probably not a great idea. `nix2flatpak` hasn't seen extensive testing, and the strong guarantees provided by nix and Flatpak only get you so far when you involve black magic. However it may still come in handy if you're trying to evaluate Flatpak as a platform or if you just want to quickly set up _any_ binary distribution method.

There are some fundamental constraints you should be aware of:

**Library compatibility:** Swapping out Nix libraries for Flatpak-provided ones works out surprisingly often but may break due to ABI differences. Large version gaps make this more likely, particularly for things like libstdc++. To minimize potential friction, avoid using `skipAbiChecks` and instead pin your nixpkgs version such that your Nix library versions are as close as possible to those in the chosen Flatpak runtime. Also consider using `extraLibs` to force certain Nix libraries to be preferred.

**Flathub:** Flathub requires applications to be built entirely from source using `flatpak-builder`. This policy is incompatible with `nix2flatpak`, which uses a bespoke mechanism to assemble files built with Nix tooling. However, you can distribute [single-file bundles](https://docs.flatpak.org/en/latest/single-file-bundles.html) (great for CI) or [self-host repositories](https://docs.flatpak.org/en/latest/hosting-a-repository.html).

## Other projects

I'm not aware of any other project that can create Flatpak bundles/repositories from Nix derivations. Here's a quick comparison to avoid confusion:

- **[nix-flatpak](https://github.com/gmodena/nix-flatpak):** Declaratively manages installation of already existing Flatpaks, but doesn't build them
- **[NixPak](https://github.com/nixpak/nixpak):** Sandboxes _Nix packages_ with bubblewrap + portals, but doesn't deal with Flatpak at all (it's just a similar sandboxing setup)
- **[flatpak-builder](https://docs.flatpak.org/en/latest/flatpak-builder.html):** Official Flatpak build tool that requires dependencies to be declared and built from source using a custom pipeline; support for external package managers is very limited
