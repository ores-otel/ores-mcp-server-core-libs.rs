{
  description = "Rust 1.88 MCP development and SOPS secret tooling";

  inputs = {
    # Immutable revisions keep the tool source fixed even before a local Nix
    # installation materializes flake.lock.
    nixpkgs.url = "github:NixOS/nixpkgs/ffb3c9b700e759be2ef13237c9d8f953b32a1e46";
    rust-overlay = {
      url = "github:oxalica/rust-overlay/c84e121aaede7ef8c7bd9fb5154ccc1599e07816";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { nixpkgs, rust-overlay, ... }:
    let
      supportedSystems = [
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-darwin"
        "x86_64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
    in
    {
      devShells = forAllSystems (system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };
          rustToolchain = pkgs.rust-bin.stable."1.88.0".default.override {
            extensions = [ "clippy" "rust-src" "rustfmt" ];
          };
        in
        {
          default = pkgs.mkShell {
            packages = [
              pkgs.age
              pkgs.just
              pkgs.sops
              rustToolchain
            ];
            RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
          };
        });
    };
}
