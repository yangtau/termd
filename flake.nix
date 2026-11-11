{
  description = "termd: preview Markdown in the terminal as a rendered page (Kitty graphics protocol)";

  # CI 构建后推送到 Cachix，用户直接下载构建结果，不需要本地编译。
  nixConfig = {
    extra-substituters = [ "https://termd.cachix.org" ];
    extra-trusted-public-keys = [ "termd.cachix.org-1:oAIvUJ+LNA+th44rHngB2vdqz6fcDzUdDCl6W6uXGHA=" ];
  };

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      inherit (nixpkgs) lib;

      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];

      forAllSystems = f: lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      cargoToml = lib.importTOML ./Cargo.toml;

      termdPackage =
        { lib, rustPlatform }:
        rustPlatform.buildRustPackage {
          pname = "termd";
          inherit (cargoToml.package) version;

          # 只取构建需要的文件，改 README 等不会导致重新构建 / 缓存失效
          src = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./src
            ];
          };

          cargoLock.lockFile = ./Cargo.lock;

          meta = {
            description = "Preview Markdown in the terminal as a rendered page (Kitty graphics protocol)";
            homepage = "https://github.com/yangtau/termd";
            mainProgram = "termd";
            platforms = systems;
          };
        };
    in
    {
      packages = forAllSystems (pkgs: rec {
        termd = pkgs.callPackage termdPackage { };
        default = termd;
      });

      overlays.default = final: _prev: {
        termd = final.callPackage termdPackage { };
      };

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.termd ];
          packages = [
            pkgs.clippy
            pkgs.rustfmt
            pkgs.rust-analyzer
          ];
        };
      });
    };
}
