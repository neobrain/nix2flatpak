{ lib, writeText }:

{ appId
, runtime          # e.g., "org.kde.Platform//6.10"
, sdk ? null
, command
, permissions ? {}
, extraEnv ? {}
, system ? builtins.currentSystem
}:

let
  # Parse runtime: "org.kde.Platform//6.10" → name + branch
  runtimeParts = lib.splitString "//" runtime;
  runtimeName = builtins.elemAt runtimeParts 0;
  runtimeBranch = builtins.elemAt runtimeParts 1;

  # Infer SDK from runtime
  actualSdk = if sdk != null
    then sdk
    else builtins.replaceStrings [ "Platform" ] [ "Sdk" ] runtimeName;

  # Map nix system to flatpak arch
  archMap = {
    "x86_64-linux" = "x86_64";
    "aarch64-linux" = "aarch64";
    "i686-linux" = "i386";
  };
  flatpakArch = archMap.${system} or (throw "Unsupported system: ${system}");

  joinPerms = perms: lib.concatStringsSep ";" (perms ++ [ "" ]);

  contextLines = lib.filter (s: s != "") [
    (lib.optionalString (permissions ? share && permissions.share != [])
      "shared=${joinPerms permissions.share}")
    (lib.optionalString (permissions ? sockets && permissions.sockets != [])
      "sockets=${joinPerms permissions.sockets}")
    (lib.optionalString (permissions ? devices && permissions.devices != [])
      "devices=${joinPerms permissions.devices}")
    (lib.optionalString (permissions ? filesystems && permissions.filesystems != [])
      "filesystems=${joinPerms permissions.filesystems}")
  ];

  envLines = lib.mapAttrsToList (k: v: "${k}=${v}") extraEnv;

in writeText "flatpak-metadata-${appId}" (
  lib.concatStringsSep "\n" ([
    "[Application]"
    "name=${appId}"
    "runtime=${runtimeName}/${flatpakArch}/${runtimeBranch}"
    "sdk=${actualSdk}/${flatpakArch}/${runtimeBranch}"
    "command=${command}"
    ""
    "[Context]"
  ] ++ contextLines
    ++ [ "" ]
    ++ lib.optionals (envLines != []) (
      [ "[Environment]" ] ++ envLines ++ [ "" ]
    )
  ) + "\n"
)
