{
  description = "weatui — NWS severe weather alerting and radar TUI";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in {
      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          # System rustc is 1.69.0 and cannot build this dependency set.
          # nixpkgs-unstable currently ships 1.97.0.
          #
          # No openssl/pkg-config here on purpose: reqwest 0.13 defaults to
          # rustls, and `cargo tree -i openssl-sys` finds no match, so there
          # is nothing in the graph that needs a system TLS library.
          #
          # cmake is for aws-lc-sys (rustls' default crypto backend), which
          # builds its C sources rather than linking a system library.
          packages = with pkgs; [
            rustc
            cargo
            rustfmt
            clippy
            rust-analyzer
            cmake
          ];
        };
      });
    };
}
