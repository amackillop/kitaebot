{
  pkgs,
  modulesPath,
  ...
}:

{
  imports = [
    (modulesPath + "/virtualisation/qemu-vm.nix")
  ];

  system.stateVersion = "25.11";

  # Basic system settings
  networking.hostName = "kitaebot";

  # Essential packages
  environment.systemPackages = with pkgs; [
    vim
    git
    curl
    htop
  ];
}
