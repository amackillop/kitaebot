{
  description = "Kitaebot - Personal AI agent in a NixOS VM";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
  };

  outputs =
    { self, nixpkgs, ... }:
    let
      system = "x86_64-linux";
    in
    {
      nixosConfigurations.kitaebot = nixpkgs.lib.nixosSystem {
        inherit system;
        modules = [
          ./configuration.nix
        ];
      };

      # Convenience alias for building the VM
      packages.${system}.default = self.nixosConfigurations.kitaebot.config.system.build.vm;
    };
}
