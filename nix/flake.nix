{
  description = "Disposable Proxmox integration lab and Rust development tools";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { nixpkgs, ... }:
    let
      systems = [ "aarch64-darwin" "x86_64-darwin" "aarch64-linux" "x86_64-linux" ];
      forSystems = nixpkgs.lib.genAttrs systems;
    in {
      devShells = forSystems (system:
        let pkgs = import nixpkgs { inherit system; };
        in {
          checks = pkgs.mkShell {
            packages = with pkgs; [ cargo rustc rustfmt clippy python3 pkg-config actionlint ];
          };
          default = pkgs.mkShell {
            packages = with pkgs; [
              qemu python3 openssh curl xorriso gnupg cacert
              cargo rustc rustfmt clippy pkg-config actionlint
              zig cargo-zigbuild
            ];
          };
        });
    };
}
