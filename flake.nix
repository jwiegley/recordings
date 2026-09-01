{
  description = "Watch, transcribe, clean, and archive voice recordings";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
        hp = pkgs.haskellPackages;
        pkg = hp.callCabal2nix "recordings" ./. { };
      in {
        packages.default = pkg;
        checks.default = pkg;
        devShells.default = hp.shellFor {
          packages = _: [ pkg ];
          nativeBuildInputs = [
            hp.cabal-install
            hp.haskell-language-server
            pkgs.hlint
            hp.fourmolu
          ];
        };
      });
}
