# The NixOS module.
#
# Its whole job is to install the binaries and write /etc/snout/config.json. The
# host name and flake path are runtime configuration rather than being baked
# into the program, so one build of snout serves every machine, and adopting it
# on a new host is a few lines rather than a rebuild of the tool.
#
# No overlay is installed. An overlay would rebuild every package that depends
# on the ones it touches, and snout depends on nothing and is depended on by
# nothing — `package` is an ordinary option instead.
{ self }:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.programs.snout;

  format = pkgs.formats.json { };

  # Durations are validated here rather than only at start-up, so a typo fails
  # during `nixos-rebuild` instead of inside a systemd unit that then restarts
  # every ten seconds. Same grammar the binary accepts: a run of decimal
  # magnitude/unit pairs, largest first, e.g. "2h", "90m", "1h30m".
  duration = lib.types.strMatching "^([0-9]+(\\.[0-9]+)?(ns|us|ms|s|m|h))+$";

  # The named options are merged into `settings`, which *is* the generated
  # document. Anything the module does not model can therefore still be set
  # without waiting for a new option to be added here.
  settings = lib.recursiveUpdate (
    {
      inherit (cfg) flake host logLevel;
      daemon = {
        inherit (cfg.daemon) enable interval settleDelay;
        backoff = { inherit (cfg.daemon.backoff) min max; };
      };
      reload = {
        inherit (cfg.reload)
          requireEnv
          spawn
          hintAfterDeploy
          afterDeploy
          notes
          components
          post
          ;
      };
    }
    # Omitted rather than written empty: the config layers merge key by key, so
    # an empty string here would be an override that defeats $XDG_CACHE_HOME.
    // lib.optionalAttrs (cfg.cacheDir != "") { inherit (cfg) cacheDir; }
  ) cfg.settings;

  configFile = format.generate "snout-config.json" settings;
in
{
  options.programs.snout = {
    enable = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Whether to install snout, the Simple NixOS Update Tool.

        This installs both halves: `snout`, the command you type, and
        `snout-daemon`, which owns the update cache. `snout` requires the
        daemon — with it stopped, every command reports it as unavailable
        rather than falling back to doing the work itself.
      '';
    };

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "snout.packages.\${system}.default";
      description = "The snout package to install.";
    };

    flake = lib.mkOption {
      type = lib.types.str;
      example = "/etc/nixos";
      description = ''
        Absolute path to the working copy of the flake that snout operates on.

        Deliberately the working tree and not a store path: pointing at the
        flake's own store path would make every deploy rebuild the revision that
        is already running, which is the opposite of what it is for.
      '';
    };

    host = lib.mkOption {
      type = lib.types.str;
      default = config.networking.hostName;
      defaultText = lib.literalExpression "config.networking.hostName";
      description = "The nixosConfigurations attribute snout builds and deploys.";
    };

    cacheDir = lib.mkOption {
      type = lib.types.str;
      default = "";
      example = "/var/cache/snout";
      description = ''
        Where to keep cached answers, scratch copies and the check lock.

        Empty follows $XDG_CACHE_HOME, falling back to ~/.cache/snout. Set it
        only when that is not writable or not where you want several gigabytes
        of scratch flake to land.
      '';
    };

    logLevel = lib.mkOption {
      type = lib.types.enum [
        "error"
        "warn"
        "info"
        "debug"
      ];
      default = "info";
      description = ''
        How much the daemon writes to the journal.

        Only the daemon logs at all; `snout` writes to the terminal it was
        typed in. So this is really the verbosity of
        `journalctl --user -u snout-daemon`. At "debug" it also forwards nix's
        own output, which on a cold nixpkgs is thousands of lines.
      '';
    };

    daemon = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = ''
          Run the background checker as a systemd user service.

          Nothing else checks for updates. `snout` never starts a check of its
          own — it only reads what the daemon has found — so with this off,
          `snout list-updates` answers "nothing checked yet" forever.
          `snout-daemon` also refuses to start while this is false, so the
          setting means the same thing however it is launched.
        '';
      };

      interval = lib.mkOption {
        type = duration;
        default = "1h";
        example = "6h";
        description = ''
          How often to ask whether the flake inputs have moved.

          Nothing paces the package-level diff directly. It runs when a check
          finds that the cached list no longer describes the current lock, and
          so inherits this as its upper bound. A tick that finds nothing new
          costs a lock comparison and no build.
        '';
      };

      settleDelay = lib.mkOption {
        type = duration;
        default = "5m";
        description = ''
          How long after start-up to leave the first check alone.

          Login is the worst moment to start fetching, and the answer on disk is
          usually still good.
        '';
      };

      backoff = {
        min = lib.mkOption {
          type = duration;
          default = "5m";
          description = "How long to wait after the first transient failure, doubling thereafter.";
        };
        max = lib.mkOption {
          type = duration;
          default = "2h";
          description = "The longest the transient-failure backoff may grow to.";
        };
      };
    };

    reload = {
      requireEnv = lib.mkOption {
        type = lib.types.str;
        default = "";
        example = "WAYLAND_DISPLAY";
        description = ''
          An environment variable that must be set for `reload` to do anything.

          The spawn template generally talks to a compositor, so running outside
          that session would start things into nothing. Empty disables the
          guard.
        '';
      };

      spawn = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        example = [
          "swaymsg"
          "exec"
          "--"
          "{}"
        ];
        description = ''
          How to launch a restarted daemon. `{}` is replaced with its name.

          Going through the session's own launcher rather than forking from
          snout is what parents the process correctly and lets it outlive the
          terminal the reload was typed into. Required when any component is of
          kind "daemon".
        '';
      };

      hintAfterDeploy = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = ''
          After a successful switch, print one line suggesting a reload.

          A reminder, not a diagnosis: snout does no staleness detection, so
          whether a reload is actually wanted is yours to decide.
        '';
      };

      afterDeploy = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = "Reload automatically after a successful switch, instead of only hinting.";
      };

      notes = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        example = [ "Compositor config changes need a re-login to take effect" ];
        description = ''
          Lines printed after a reload.

          For the things a reload cannot fix and a human has to be told about.
          Plain strings, so snout needs to know nothing about what they mean.
        '';
      };

      components = lib.mkOption {
        default = [ ];
        description = ''
          What `reload` restarts, in order. Restarted unconditionally — there is
          no staleness check.
        '';
        example = lib.literalExpression ''
          [
            { name = "waybar"; kind = "daemon"; }
            { name = "mako";   kind = "daemon"; }
            { name = "polkit-gnome-authentication-agent-1"; kind = "systemd-user"; }
          ]
        '';
        type = lib.types.listOf (
          lib.types.submodule {
            options = {
              name = lib.mkOption {
                type = lib.types.str;
                description = "Program name for \"daemon\", unit name for \"systemd-user\", label otherwise.";
              };
              kind = lib.mkOption {
                type = lib.types.enum [
                  "daemon"
                  "systemd-user"
                  "command"
                ];
                description = ''
                  How to restart it.

                  "daemon" stops the process and relaunches it through
                  `reload.spawn`; it tolerates nixpkgs' wrapper renaming, so a
                  program running as `.name-wrapped` is still matched.
                  "systemd-user" runs `systemctl --user restart`.
                  "command" is the escape hatch, using `stop` and `start`.
                '';
              };
              unit = lib.mkOption {
                type = lib.types.str;
                default = "";
                description = "Unit name for \"systemd-user\", when it differs from `name`.";
              };
              stop = lib.mkOption {
                type = lib.types.listOf lib.types.str;
                default = [ ];
                description = "Command that stops it, for kind \"command\".";
              };
              start = lib.mkOption {
                type = lib.types.listOf lib.types.str;
                default = [ ];
                description = "Command that starts it, for kind \"command\".";
              };
            };
          }
        );
      };

      post = lib.mkOption {
        default = [ ];
        description = "Commands run after every component has been restarted.";
        example = lib.literalExpression ''
          [ { name = "compositor config"; command = [ "swaymsg" "reload" ]; } ]
        '';
        type = lib.types.listOf (
          lib.types.submodule {
            options = {
              name = lib.mkOption {
                type = lib.types.str;
                description = "Label shown in the reload summary.";
              };
              command = lib.mkOption {
                type = lib.types.listOf lib.types.str;
                description = "Command to run.";
              };
            };
          }
        );
      };
    };

    settings = lib.mkOption {
      type = format.type;
      default = { };
      description = ''
        Raw contents of /etc/snout/config.json.

        The named options above are merged into this, and anything set here
        wins. It exists so a setting snout gains before this module models it is
        still reachable.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];

    environment.etc."snout/config.json".source = configFile;

    # A *user* service, not a system one. The check needs no privileges — it
    # builds as the invoking user against a scratch copy of the flake — and it
    # writes into the user's cache, which is where the surfaces read it from.
    #
    # Bound to default.target rather than graphical-session.target so that
    # checks still happen on a machine that was booted without a desktop.
    systemd.user.services.snout-daemon = lib.mkIf cfg.daemon.enable {
      description = "snout update checker";
      documentation = [ "https://github.com/frdrkand/snout" ];
      wantedBy = [ "default.target" ];

      serviceConfig = {
        ExecStart = "${cfg.package}/bin/snout-daemon";
        Restart = "on-failure";
        RestartSec = 10;
      };

      # Everything a check shells out to. Deliberately short: the daemon builds
      # against a scratch copy of the flake and never touches the desktop — it
      # raises no notifications, activates nothing and runs no reload, so
      # nothing from the session belongs here.
      path = [
        config.nix.package
        pkgs.git
      ];
    };
  };
}
