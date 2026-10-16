{
  description = "Tools that deploy Gradient workers";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
  };

  outputs = { self, nixpkgs, crane }: let
    systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
    forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
  in {
    packages = forAllSystems (pkgs: rec {
      gradient-gha-autoscaler = pkgs.callPackage ./nix/package.nix { craneLib = crane.mkLib pkgs; };
      default = gradient-gha-autoscaler;
    });

    checks = forAllSystems (pkgs: {
      inherit (self.packages.${pkgs.stdenv.hostPlatform.system}.gradient-gha-autoscaler) tests clippy fmt;
    });

    nixosModules = rec {
      gradient-gha-autoscaler = import ./nix/module.nix self;
      default = gradient-gha-autoscaler;
    };
  };
}
