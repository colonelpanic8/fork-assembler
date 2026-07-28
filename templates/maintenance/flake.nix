{
  description = "fork-fold maintenance repository";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    fork-fold.url = "github:colonelpanic8/fork-fold";
    fork-fold.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { nixpkgs, fork-fold, ... }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      devShells = forAllSystems (system: {
        default = fork-fold.lib.mkMaintenanceShell {
          pkgs = nixpkgs.legacyPackages.${system};
        };
      });

      # The checked-in skill is only a discovery stub. Its full instructions
      # are loaded from this value, which follows the pinned fork-fold input.
      lib.forkFoldAgentGuide = fork-fold.lib.agentGuide;
    };
}
