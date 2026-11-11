{
  description = "VibeRadio — Rust/egui internet radio client";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-24.05";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs = { self, nixpkgs, rust-overlay }:
    let
      pkgs = import nixpkgs {
        system = "x86_64-linux";
        overlays = [ rust-overlay.overlays.default ];
      };
    in {
      devShells.x86_64-linux = {
        default = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [ pkg-config rustc cargo ];
          buildInputs = with pkgs; [ alsa-lib ];
        };
      };
    };
}