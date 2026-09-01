{
  description = "Watch, transcribe, clean, and archive voice recordings";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachSystem [ "aarch64-darwin" "x86_64-linux" "aarch64-linux" ] (system:
      let
        pkgs = import nixpkgs { inherit system; };
        recordings = pkgs.rustPlatform.buildRustPackage {
          pname = "recordings";
          version = "1.0.0";
          src = self;
          cargoLock.lockFile = ./Cargo.lock;
          strictDeps = true;
          nativeBuildInputs = with pkgs; [ clippy rustfmt ];
          preBuild = ''
            cargo fmt --all -- --check
            cargo clippy --all-targets --all-features -- -D warnings
          '';
          meta = with pkgs.lib; {
            description = "Watch, transcribe, clean, and archive voice recordings";
            license = licenses.bsd3;
            mainProgram = "recordings";
          };
        };
      in {
        packages.default = recordings;
        checks.default = recordings;
        devShells.default = pkgs.mkShell {
          packages = with pkgs; [ cargo clippy rustc rustfmt ];
        };
      });
}
