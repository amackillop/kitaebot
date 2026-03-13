# Prebuilt lightpanda headless browser.
# https://github.com/lightpanda-io/browser
{
  lib,
  stdenv,
  fetchurl,
  autoPatchelfHook,
}:

let
  version = "0.2.5";

  srcs = {
    x86_64-linux = fetchurl {
      url = "https://github.com/lightpanda-io/browser/releases/download/v${version}/lightpanda-x86_64-linux";
      hash = "sha256-EqkRwRk3jIknIeL3rqc24aL+73YIvOcw5I6I4zwp+TQ=";
    };
    aarch64-linux = fetchurl {
      url = "https://github.com/lightpanda-io/browser/releases/download/v${version}/lightpanda-aarch64-linux";
      hash = "sha256-9wQIcK0fuVehSJZIQwYs8wO/uDaloEjr4t0ACIQR21Y=";
    };
  };
in
stdenv.mkDerivation {
  pname = "lightpanda";
  inherit version;

  src =
    srcs.${stdenv.hostPlatform.system}
      or (throw "lightpanda: unsupported platform ${stdenv.hostPlatform.system}");

  dontUnpack = true;

  nativeBuildInputs = lib.optionals stdenv.hostPlatform.isLinux [ autoPatchelfHook ];

  installPhase = ''
    install -Dm755 $src $out/bin/lightpanda
  '';

  meta = {
    description = "Lightweight headless browser built for AI and automation";
    homepage = "https://github.com/lightpanda-io/browser";
    license = lib.licenses.agpl3Only;
    platforms = builtins.attrNames srcs;
    mainProgram = "lightpanda";
  };
}
