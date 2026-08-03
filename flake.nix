{
  description = "fork-assembler - assemble stacks of live fork branches with tracked resolutions";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    let
      mkForkAssembler = pkgs:
        pkgs.rustPlatform.buildRustPackage {
          pname = "fork-assembler";
          version = "0.1.0";
          src = self;
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = [ pkgs.makeWrapper ];
          nativeCheckInputs = [ pkgs.git ];
          postInstall = ''
            wrapProgram $out/bin/fork-assembler --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.git ]}
          '';
        };
    in
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
      in
      {
        packages.default = mkForkAssembler pkgs;

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
      overlays.default = final: prev: { fork-assembler = mkForkAssembler final; };

      # Authoritative agent instructions. Consuming maintenance flakes
      # re-export this text so their tiny discovery skill always loads the
      # guide from the exact fork-assembler revision they pin.
      lib.agentGuide = builtins.readFile ./AGENT_GUIDE.md;

      # Dev shell for a maintenance repo that consumes fork-assembler: the compiled
      # tool plus the commands its workflows shell out to.
      lib.mkMaintenanceShell = { pkgs, extraPackages ? [ ] }:
        pkgs.mkShell {
          packages = [
            (mkForkAssembler pkgs)
            pkgs.git
            pkgs.gh
            pkgs.just
          ] ++ extraPackages;
        };

      templates.default = {
        path = ./templates/maintenance;
        description = "fork-assembler maintenance repository: manifest, resolutions, dev shell, direnv";
      };
    };
}
