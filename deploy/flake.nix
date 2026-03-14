# Local deployment configuration
#
# Build:  nix build ./deploy
# Run:    ./result/bin/run-kitaebot-vm
# SSH:    ssh -p 2222 root@localhost
#
# This flake imports the kitaebot VM module and applies local settings.
# For production, create a similar flake with kitaebot.dev = false.
{
  description = "Kitaebot deployment";

  inputs = {
    kitaebot.url = "path:..";
    nixpkgs.follows = "kitaebot/nixpkgs";
    determinate.url = "https://flakehub.com/f/DeterminateSystems/determinate/*";
  };

  outputs =
    {
      self,
      nixpkgs,
      kitaebot,
      determinate,
      ...
    }:
    {
      nixosConfigurations.kitaebot = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        modules = [
          kitaebot.nixosModules.vm
          determinate.nixosModules.default
          ./configuration.nix
          { kitaebot.package = kitaebot.packages.x86_64-linux.default; }
        ];
      };

      packages.x86_64-linux.default = self.nixosConfigurations.kitaebot.config.system.build.vm;
    };
}
