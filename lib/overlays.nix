# Placeholder — overlays are opt-in and not required for the core pipeline.
# They annotate nixpkgs packages with passthru.flatpakRuntimeProvided = true,
# which can speed up closure analysis.
{ lib }:
{
  mkOverlay = { runtimeIndex }:
    let
      index = builtins.fromJSON (builtins.readFile runtimeIndex);
    in
    _final: _prev: {
      # TODO: implement soname-to-nixpkgs mapping
      # For now, this is a no-op overlay
    };
}
