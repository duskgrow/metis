{ pkgs, system }:

let
  inherit (pkgs) lib;
  version = lib.strings.removeSuffix "\n" (builtins.readFile ../.node-version);
  target = {
    x86_64-linux = {
      directory = "linux-x64";
      hash = "sha256-VapxU/nYjyjXZfza1a5pRbXA+Yo2iBcDgX5MRQ+nZ0I=";
    };
    aarch64-linux = {
      directory = "linux-arm64";
      hash = "sha256-WMlSBQH2ritS1bIQRE4kudDAKaWMUBG3l7wf5xBYhvY=";
    };
    x86_64-darwin = {
      directory = "darwin-x64";
      hash = "sha256-SjtryBVCFUQwglEo2aJ56LNk6NkFgVROUG73V5/Rq28=";
    };
    aarch64-darwin = {
      directory = "darwin-arm64";
      hash = "sha256-RHe59477d3RM9etXoOlZTbpmRms4tOk/qfNcuQeglaY=";
    };
  }.${system} or (throw "no pinned Node.js binary is configured for ${system}");
in
pkgs.stdenv.mkDerivation {
  pname = "nodejs";
  inherit version;

  src = pkgs.fetchurl {
    url = "https://nodejs.org/dist/v${version}/node-v${version}-${target.directory}.tar.xz";
    inherit (target) hash;
  };

  nativeBuildInputs = lib.optionals pkgs.stdenv.hostPlatform.isLinux [ pkgs.autoPatchelfHook ];
  buildInputs = lib.optionals pkgs.stdenv.hostPlatform.isLinux [ pkgs.stdenv.cc.cc.lib ];

  dontConfigure = true;
  dontBuild = true;

  installPhase = ''
    mkdir -p $out
    cp -R bin include lib share LICENSE $out/
  '';

  # installCheckPhase runs after fixup, including autoPatchelfHook on NixOS.
  doInstallCheck = true;
  installCheckPhase = ''
    $out/bin/node --version
  '';

  meta = {
    description = "Pinned official Node.js binary for the dsh integration";
    homepage = "https://nodejs.org/";
    license = lib.licenses.mit;
    mainProgram = "node";
    sourceProvenance = [ lib.sourceTypes.binaryNativeCode ];
  };
}
