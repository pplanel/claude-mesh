{
  description = "claude-mesh: keep Claude Code's state in sync across Macs, over airshell";

  # Not following anyone's nixpkgs on purpose: consumers get this flake's own
  # pin, so their build hashes match what CI pushed to the binary cache
  # (ghcr.io/pplanel/claude-mesh/nix-cache).
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs = {
    self,
    nixpkgs,
  }: let
    system = "aarch64-darwin";
    pkgs = nixpkgs.legacyPackages.${system};
    manifest = (pkgs.lib.importTOML ./Cargo.toml).package;
  in {
    packages.${system} = {
      claude-mesh = pkgs.rustPlatform.buildRustPackage {
        pname = manifest.name;
        inherit (manifest) version;
        src = pkgs.lib.fileset.toSource {
          root = ./.;
          fileset = pkgs.lib.fileset.unions [./Cargo.toml ./Cargo.lock ./src ./tests];
        };
        cargoLock.lockFile = ./Cargo.lock;
        # The end-to-end tests start three servers on localhost ports, which
        # the build sandbox allows on macOS; `nc` stands in for airshell-connect.
        nativeCheckInputs = [pkgs.netcat];
        meta = {
          description = manifest.description or "Keep Claude Code's state in sync across Macs";
          homepage = "https://github.com/pplanel/claude-mesh";
          mainProgram = "claude-mesh";
          platforms = ["aarch64-darwin"];
        };
      };
      default = self.packages.${system}.claude-mesh;
    };

    devShells.${system}.default = pkgs.mkShell {
      inputsFrom = [self.packages.${system}.claude-mesh];
      packages = [pkgs.clippy pkgs.rustfmt];
    };
  };
}
