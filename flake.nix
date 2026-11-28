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

        hatchzip-gui = pkgs.callPackage ./packaging/nix/gui.nix {
          src = self;
        };

        hatchzip-gui-rar = pkgs.callPackage ./packaging/nix/gui.nix {
          src = self;
        };
      in {
        default = hatchzip;
        inherit hatchzip hatchzip-rar hatchzip-gui hatchzip-gui-rar;
      }
    );

    apps = forAllSystems (
      system: let
        packages = self.packages.${system};
        mkApp = package: {
          type = "app";
          program = "${package}/bin/${package.meta.mainProgram}";
          inherit (package) meta;
        };
        cli = mkApp packages.hatchzip;
      in
        {
          default = cli;
          hatchzip = cli;
          hatchzip-rar = mkApp packages.hatchzip-rar;
          hatchzip-gui = mkApp packages.hatchzip-gui;
          hatchzip-gui-rar = mkApp packages.hatchzip-gui-rar;
        }
        // nixpkgs.lib.optionalAttrs (builtins.hasAttr "hatchzip-gui" packages) {
        }
    );
  };
}
