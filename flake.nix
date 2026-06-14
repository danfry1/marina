{
  description = "marina — a developer-process cockpit (TUI + CLI)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
        marina = pkgs.rustPlatform.buildRustPackage {
          pname = "marina";
          version = "0.1.0";
          src = pkgs.lib.cleanSource ./.;
          cargoLock.lockFile = ./Cargo.lock;

          # The integration tests need lsof / /proc / sockets that aren't
          # available in the Nix build sandbox; they run in CI instead.
          doCheck = false;

          meta = with pkgs.lib; {
            description = "A developer-process cockpit — dev servers and processes, resolved into names you recognize";
            homepage = "https://github.com/danfry1/marina";
            license = licenses.asl20;
            mainProgram = "marina";
          };
        };
      in
      {
        packages.default = marina;

        apps.default = {
          type = "app";
          program = "${marina}/bin/marina";
        };

        devShells.default = pkgs.mkShell {
          packages = with pkgs; [ cargo rustc clippy rustfmt rust-analyzer ];
        };

        formatter = pkgs.nixpkgs-fmt;
      });
}
