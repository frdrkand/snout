# A NixOS configuration that exercises the module, for `nix flake check`.
#
# Evaluating it for real is what catches an option typo, a broken default, or a
# type that does not accept what the README says it does.
#
# It sets every option the module defines, deliberately: an option nobody sets
# here is an option nothing evaluates. `package` is the one exception — it
# defaults to the flake's own build, and overriding it here would check
# something other than what ships.
{ pkgs, ... }:
{
  boot.loader.grub.enable = false;
  fileSystems."/" = {
    device = "/dev/null";
    fsType = "ext4";
  };
  networking.hostName = "checkhost";
  system.stateVersion = "24.05";

  programs.snout = {
    enable = true;
    flake = "/etc/nixos";
    host = "checkhost";
    cacheDir = "/var/cache/snout";
    logLevel = "debug";

    daemon = {
      enable = true;
      interval = "6h";
      settleDelay = "90s";
      backoff = {
        min = "1m";
        max = "1h30m";
      };
    };

    reload = {
      requireEnv = "WAYLAND_DISPLAY";
      spawn = [
        "swaymsg"
        "exec"
        "--"
        "{}"
      ];
      hintAfterDeploy = true;
      afterDeploy = false;
      notes = [ "Compositor config changes need a re-login to take effect" ];
      components = [
        {
          name = "mako";
          kind = "daemon";
        }
        {
          name = "polkit-agent";
          kind = "systemd-user";
          unit = "polkit-gnome-authentication-agent-1";
        }
        {
          name = "wallpaper";
          kind = "command";
          stop = [
            "pkill"
            "-x"
            "swaybg"
          ];
          start = [
            "swaybg"
            "-i"
            "/dev/null"
          ];
        }
      ];
      post = [
        {
          name = "compositor config";
          command = [
            "swaymsg"
            "reload"
          ];
        }
      ];
    };

    # The escape hatch, exercised so its type is evaluated too.
    settings.daemon.backoff.max = "1h30m";
  };

  # The unit puts nix and git on the daemon's PATH; pin nix so the check does
  # not depend on whatever the host's default happens to be.
  nix.package = pkgs.nixVersions.stable;
}
