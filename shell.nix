{ pkgs ? import <nixpkgs> {} }:

pkgs.mkShell {
  nativeBuildInputs = with pkgs; [ pkg-config rustc cargo ];
  buildInputs = with pkgs; [ alsa-lib ];
}