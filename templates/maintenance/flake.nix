{
  description = "fork-assembler maintenance repository";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    fork-assembler.url = "github:colonelpanic8/fork-assembler";
    fork-assembler.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { nixpkgs, fork-assembler, ... }:
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
        default = fork-assembler.lib.mkMaintenanceShell {
          pkgs = nixpkgs.legacyPackages.${system};
        };
      });

      # The checked-in skill is only a discovery stub. Its full instructions
      # are loaded from this value, which follows the pinned fork-assembler input.
      lib.forkFoldAgentGuide = fork-assembler.lib.agentGuide;
    };
}
