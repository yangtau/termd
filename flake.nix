{
  description = "termd: preview Markdown in the terminal as a rendered page (Kitty graphics protocol)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      inherit (nixpkgs) lib;

      # 由 .github/workflows/release.yml 在每次发布后自动更新
      releases = lib.importJSON ./nix/releases.json;

      systems = [
        "x86_64-linux"
        "aarch64-darwin"
      ];

      forAllSystems = f: lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      # 下载并安装指定版本的预编译二进制。
      #   mkTermd pkgs "0.1.0"
      mkTermd =
        pkgs: version:
        let
          system = pkgs.stdenv.hostPlatform.system;
          release =
            releases.versions.${version}
              or (throw "termd: unknown version ${version}; known: ${lib.concatStringsSep ", " (lib.attrNames releases.versions)}");
          asset = release.${system} or (throw "termd ${version}: no prebuilt binary for ${system}");
        in
        pkgs.stdenvNoCC.mkDerivation {
          pname = "termd";
          inherit version;

          src = pkgs.fetchurl {
            url = "https://github.com/yangtau/termd/releases/download/v${version}/termd-v${version}-${asset.target}.tar.gz";
            inherit (asset) hash;
          };
          sourceRoot = ".";

          dontConfigure = true;
          dontBuild = true;
          installPhase = ''
            runHook preInstall
            install -Dm755 termd $out/bin/termd
            runHook postInstall
          '';

          meta = {
            description = "Preview Markdown in the terminal as a rendered page (Kitty graphics protocol)";
            homepage = "https://github.com/yangtau/termd";
            mainProgram = "termd";
            platforms = systems;
            sourceProvenance = [ lib.sourceTypes.binaryNativeCode ];
          };
        };

      # "0.1.0" -> "v0_1_0"，用作 flake 属性名：nix run github:yangtau/termd#v0_1_0
      attrName = version: "v" + lib.replaceStrings [ "." ] [ "_" ] version;
    in
    {
      lib = {
        inherit mkTermd releases;
      };

      packages = forAllSystems (
        pkgs:
        let
          system = pkgs.stdenv.hostPlatform.system;
          available = lib.filterAttrs (_: r: r ? ${system}) releases.versions;
          versioned = lib.mapAttrs' (v: _: lib.nameValuePair (attrName v) (mkTermd pkgs v)) available;
        in
        versioned
        // lib.optionalAttrs (releases.latest != null && available ? ${releases.latest}) rec {
          termd = mkTermd pkgs releases.latest;
          default = termd;
        }
      );

      overlays.default = final: _prev: {
        termd = mkTermd final releases.latest;
      };
    };
}
