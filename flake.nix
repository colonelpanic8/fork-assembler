{
  description = "fork-fold - assemble stacks of live fork branches with tracked resolutions";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    let
      mkForkFold = pkgs:
        pkgs.rustPlatform.buildRustPackage {
          pname = "fork-fold";
          version = "0.1.0";
          src = self;
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = [ pkgs.makeWrapper ];
          nativeCheckInputs = [ pkgs.git ];
          postInstall = ''
            wrapProgram $out/bin/fork-fold --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.git ]}
          '';
        };
    in
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
      in
      {
        packages.default = mkForkFold pkgs;

        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer
            git
            gh
          ];
        };
      })
    // {
      overlays.default = final: prev: { fork-fold = mkForkFold final; };

      # Dev shell for a maintenance repo that consumes fork-fold: the compiled
      # tool plus the commands its workflows shell out to.
      lib.mkMaintenanceShell = { pkgs, extraPackages ? [ ] }:
        pkgs.mkShell {
          packages = [
            (mkForkFold pkgs)
            pkgs.git
            pkgs.gh
            pkgs.just
          ] ++ extraPackages;
        };

      templates.default = {
        path = ./templates/maintenance;
        description = "fork-fold maintenance repository: manifest, resolutions, dev shell, direnv";
      };
    };
}
