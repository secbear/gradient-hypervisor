{
  description = "Tools that deploy Gradient workers";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
    gradient.url = "github:wavelens/gradient/v2.0.1";
  };

  outputs = { self, nixpkgs, crane, gradient }: let
    inherit (nixpkgs) lib;
    systems = [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" ];
    forAllSystems = f: lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

    mkWorkerImage = { pkgs, gradient, gradient-nix }:
      pkgs.callPackage ./nix/images/gradient-worker.nix { inherit gradient gradient-nix; };
  in {
    packages = forAllSystems (pkgs: let
      system = pkgs.stdenv.hostPlatform.system;
    in rec {
      gradient-autoscaler = pkgs.callPackage ./nix/packages/gradient-autoscaler.nix { craneLib = crane.mkLib pkgs; };
      default = gradient-autoscaler;
    } // lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
      gradient-autoscaler-image = pkgs.callPackage ./nix/images/gradient-autoscaler.nix {
        inherit (self.packages.${system}) gradient-autoscaler;
      };

      gradient-worker-image = mkWorkerImage {
        inherit pkgs;
        inherit (gradient.packages.${system}) gradient gradient-nix;
      };

      gradient-kubernetes-test-image = pkgs.callPackage ./nix/tests/kubernetes/image.nix {
        autoscaler = self.packages.${system}.gradient-autoscaler;
        inherit (gradient.packages.${system}) gradient gradient-nix;
      };
    });

    checks = forAllSystems (pkgs: {
      inherit (self.packages.${pkgs.stdenv.hostPlatform.system}.gradient-autoscaler) tests clippy fmt;
      helm = pkgs.callPackage ./nix/tests/helm.nix { };
      kubernetes-fixture = pkgs.callPackage ./nix/tests/kubernetes/check.nix { };
    } // lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
      module = pkgs.callPackage ./nix/tests/module.nix {
        nixosModule = self.nixosModules.gradient-autoscaler;
      };
    } // lib.optionalAttrs (
      pkgs.stdenv.hostPlatform.isLinux
      && builtins.hasAttr "gradient-worker-readiness" (gradient.checks.${pkgs.stdenv.hostPlatform.system} or { })
    ) {
      worker-readiness-preview = pkgs.callPackage ./nix/tests/readiness-preview {
        inherit gradient;
        system = pkgs.stdenv.hostPlatform.system;
        autoscaler = self.packages.${pkgs.stdenv.hostPlatform.system}.gradient-autoscaler;
      };
    });

    devShells = forAllSystems (pkgs: {
      default = pkgs.mkShell {
        packages = with pkgs; [
          cargo
          cargo-nextest
          clippy
          kubernetes-helm
          rustc
          rustfmt
          sqlite
        ];
      };
    });

    lib = { inherit mkWorkerImage; };

    nixosModules = rec {
      gradient-autoscaler = import ./nix/modules/gradient-autoscaler.nix self;
      default = gradient-autoscaler;
    };
  };
}
