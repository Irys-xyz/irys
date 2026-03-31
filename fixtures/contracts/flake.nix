{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    foundry.url = "github:shazow/foundry.nix/monthly";
  };

  outputs = { self, nixpkgs, foundry }:
    let
      supportedSystems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
    in
    {
      devShells = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          foundryPkgs = foundry.defaultPackage.${system};
        in
        {
          default = pkgs.mkShell {
            buildInputs = [ foundryPkgs ];
          };
        }
      );
    };
}
