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
        pkgs = import nixpkgs {inherit system;};

        hatchzip = pkgs.callPackage ./nix/package.nix {
          src = self;
        };

        hatchzip-unfree = pkgs.callPackage ./nix/package.nix {
          src = self;
          enableUnfree = true;
        };
      in {
        default = hatchzip;
        inherit hatchzip hatchzip-unfree;
      }
    );

    apps = forAllSystems (
      system: let
        packages = self.packages.${system};
      in {
        default = {
          type = "app";
          program = "${packages.hatchzip}/bin/hatchzip";
        };

        hatchzip = {
          type = "app";
          program = "${packages.hatchzip}/bin/hatchzip";
        };

        hatchzip-unfree = {
          type = "app";
          program = "${packages.hatchzip-unfree}/bin/hatchzip";
        };
      }
    );

    devShells = forAllSystems (
      system: let
        pkgs = import nixpkgs {inherit system;};
      in {
        default = pkgs.mkShell {
          packages = with pkgs; [
            rustc
            cargo
            rustfmt
            clippy
          ];
        };
      }
    );
  };
}
