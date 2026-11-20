{
  description = "HatchZip - archive manager";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs = {
    self,
    nixpkgs,
  }: let
    systems = [
      "x86_64-linux"
      "aarch64-linux"
      "x86_64-darwin"
      "aarch64-darwin"
    ];

    forAllSystems = nixpkgs.lib.genAttrs systems;
  in {
    packages = forAllSystems (
      system: let
        pkgs = nixpkgs.legacyPackages.${system};

        hatchzip = pkgs.callPackage ./packaging/nix/package.nix {
          src = self;
        };

        hatchzip-rar = pkgs.callPackage ./packaging/nix/package.nix {
          src = self;
          enableRar = true;
        };
      in {
        default = hatchzip;
        inherit hatchzip hatchzip-rar;
      }
    );

    apps = forAllSystems (
      system: let
        packages = self.packages.${system};
        hatchzip = {
          type = "app";
          program = "${packages.hatchzip}/bin/hatchzip";
        };
      in {
        default = hatchzip;
        inherit hatchzip;

        hatchzip-rar = {
          type = "app";
          program = "${packages.hatchzip-rar}/bin/hatchzip";
        };
      }
    );
  };
}
