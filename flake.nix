{
  description = "weatui — NWS severe weather alerting and radar TUI";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
    in {
      devShells.${system}.default = pkgs.mkShell {
        # System rustc is 1.69.0 and cannot build this dependency set.
        # nixpkgs-unstable currently ships 1.97.0.
        #
        # No openssl/pkg-config here on purpose: reqwest 0.13 defaults to
        # rustls, and `cargo tree -i openssl-sys` finds no match, so there
        # is nothing in the graph that needs a system TLS library.
        packages = with pkgs; [
          rustc
          cargo
          rustfmt
          clippy
          rust-analyzer
        ];
      };
    };
}
