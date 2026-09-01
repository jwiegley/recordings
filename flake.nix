{
  description = "Watch, transcribe, clean, and archive voice recordings";

  inputs = {
    nixpkgs.follows = "haskellNix/nixpkgs-unstable";
    haskellNix.url = "github:input-output-hk/haskell.nix";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils, haskellNix }:
    # Not eachDefaultSystem: that list includes x86_64-darwin, which
    # nixpkgs 26.11 dropped (its import now throws for that system).
    flake-utils.lib.eachSystem [ "aarch64-darwin" "x86_64-linux" "aarch64-linux" ] (system:
      let
        pkgs = import nixpkgs {
          inherit system overlays;
          inherit (haskellNix) config;
        };
        overlays = [ haskellNix.overlay
          (final: prev: {
            recordings =
              final.haskell-nix.project' {
                src = ./.;
                supportHpack = true;
                compiler-nix-name = "ghc910";
                shell = {
                  tools = {
                    cabal = {};
                    haskell-language-server = {};
                  };
                  buildInputs = with pkgs; [
                    pkg-config
                    haskellPackages.fourmolu
                    haskellPackages.hlint
                    haskellPackages.hpc
                    lefthook
                  ];
                  withHoogle = true;
                };
                # modules = [{
                #   enableLibraryProfiling = true;
                #   enableProfiling = true;
                # }];
              };
          })
        ];
        flake = pkgs.recordings.flake {
        };

        # All Haskell source directories (excluding dist-newstyle)
        hsDirs = builtins.concatStringsSep " " [
          "src"
          "test"
        ];

      in flake // {
        packages.default = flake.packages."recordings:exe:recordings";

        devShells.default = flake.devShells.default // {
          withHoogle = true;
        };

        checks = (flake.checks or {}) // {
          formatting = pkgs.runCommand "check-formatting" {
            nativeBuildInputs = [ pkgs.haskellPackages.fourmolu pkgs.findutils ];
            src = self;
          } ''
            cd $src
            find ${hsDirs} -name '*.hs' -type f \
              | xargs fourmolu --mode check
            touch $out
          '';

          hlint = pkgs.runCommand "check-hlint" {
            nativeBuildInputs = [ pkgs.haskellPackages.hlint pkgs.findutils ];
            src = self;
          } ''
            cd $src
            find ${hsDirs} -name '*.hs' -type f \
              | xargs hlint
            touch $out
          '';
        };
      });
}
