# NixOS module for the browser-session stack: a persistent headless Chrome plus
# the three host-side daemons (listener, reaper, takeover) that cooperate around
# it. The MCP server itself is a stdio subprocess spawned by your MCP client /
# proxy — it is intentionally NOT managed here; point it at `browserUrl` and the
# shared `stateDir`.
#
# The flake exports this pre-wired as `nixosModules.default` with `package`
# defaulted to the flake's build. Consumers set at least `chrome.package` (the
# Chrome to run) and, if using takeover, `takeover.chromeWsBase`.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.browser-session;
  exe = lib.getExe' cfg.package "browser-session";

  # Base Chrome flags common to any headless-CDP setup. GPU/ozone specifics are
  # deployment-dependent, so they come in via `chrome.extraArgs`.
  chromeArgs = [
    "--no-sandbox"
    "--disable-dev-shm-usage"
    "--user-data-dir=${cfg.chrome.dataDir}"
    "--remote-debugging-address=127.0.0.1"
    "--remote-debugging-port=${toString cfg.chrome.port}"
    # Chrome rejects WebSocket upgrades whose Origin it doesn't recognise; a
    # reverse proxy fronting CDP needs this.
    "--remote-allow-origins=*"
    # Drop navigator.webdriver + the automation blink features bot-detectors key
    # on.
    "--disable-blink-features=AutomationControlled"
    "--window-size=1920,1080"
  ]
  ++ cfg.chrome.extraArgs;

  daemonHardening = {
    Restart = "always";
    RestartSec = 2;
    StandardOutput = "journal";
    StandardError = "journal";
  };
in
{
  options.services.browser-session = {
    enable = lib.mkEnableOption "the browser-session stack (isolated browser sessions over a shared Chrome)";

    package = lib.mkOption {
      type = lib.types.package;
      description = ''
        The `browser-session` multi-call package (provides the `mcp`, `listener`,
        `reaper` and `takeover` subcommands). Defaults to this flake's build when
        consumed via `nixosModules.default`.
      '';
    };

    stateDir = lib.mkOption {
      type = lib.types.path;
      default = "/var/lib/browser-session-mcp";
      description = ''
        Directory the daemons and the (externally-run) MCP share: the state
        file, per-session NDJSON logs, saved cookie states, and takeover tickets.
      '';
    };

    user = lib.mkOption {
      type = lib.types.str;
      default = "root";
      description = ''
        User the host daemons run as, and owner of `stateDir`. Defaults to root
        because the state dir is typically shared with the MCP running in a
        root container.
      '';
    };

    group = lib.mkOption {
      type = lib.types.str;
      default = "root";
      description = "Group owning `stateDir`.";
    };

    browserUrl = lib.mkOption {
      type = lib.types.str;
      default = "http://127.0.0.1:${toString cfg.chrome.port}";
      defaultText = lib.literalExpression ''"http://127.0.0.1:''${toString cfg.chrome.port}"'';
      description = ''
        DevTools endpoint the listener and reaper connect to. Defaults to the
        Chrome managed here; set it explicitly to drive an external Chrome (in
        which case set `chrome.enable = false`).
      '';
    };

    chrome = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = "Run a persistent headless Chrome exposing the DevTools Protocol on 127.0.0.1.";
      };
      package = lib.mkOption {
        type = lib.types.package;
        example = lib.literalExpression "pkgs.chrome-headless-shell";
        description = "Chrome package to run (e.g. chrome-headless-shell or chromium).";
      };
      executable = lib.mkOption {
        type = lib.types.str;
        default = "chrome-headless-shell";
        description = "Binary within `chrome.package` to exec (e.g. \"chromium\").";
      };
      port = lib.mkOption {
        type = lib.types.port;
        default = 9222;
        description = "DevTools port bound to 127.0.0.1.";
      };
      dataDir = lib.mkOption {
        type = lib.types.path;
        default = "/var/lib/chrome";
        description = "Chrome user-data-dir; persists cookies/storage across restarts.";
      };
      user = lib.mkOption {
        type = lib.types.str;
        default = "chrome-headless";
        description = "User the Chrome service runs as (created when it matches this default).";
      };
      group = lib.mkOption {
        type = lib.types.str;
        default = "chrome-headless";
        description = "Group for the Chrome service.";
      };
      extraArgs = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        example = [
          "--use-gl=angle"
          "--use-angle=vulkan"
        ];
        description = "Extra Chrome flags, e.g. the GPU/ozone setup for real-GPU WebGL.";
      };
      environment = lib.mkOption {
        type = lib.types.attrsOf lib.types.str;
        default = { };
        description = "Extra environment for the Chrome service (e.g. LD_LIBRARY_PATH for a GPU driver).";
      };
    };

    listener.enable = lib.mkOption {
      type = lib.types.bool;
      default = cfg.enable;
      description = ''
        Run the always-on CDP listener that writes every console + network event
        to per-session NDJSON under `stateDir`. This is the only source of the
        `list_console_messages` / `list_network_requests` logs.
      '';
    };

    reaper = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = cfg.enable;
        description = "Periodically close idle BrowserContexts and orphan tabs.";
      };
      interval = lib.mkOption {
        type = lib.types.str;
        default = "30min";
        description = "How often the reaper runs (systemd OnUnitActiveSec).";
      };
      maxIdleHours = lib.mkOption {
        type = lib.types.ints.positive;
        default = 24;
        description = "Close sessions idle longer than this many hours.";
      };
    };

    takeover = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = cfg.enable;
        description = ''
          Serve the human-takeover page: a live view of a session's active page
          so a human can complete a login/passkey the agent must not see.
        '';
      };
      address = lib.mkOption {
        type = lib.types.str;
        default = "127.0.0.1";
        description = "Address the takeover daemon binds.";
      };
      port = lib.mkOption {
        type = lib.types.port;
        default = 9223;
        description = "Port the takeover daemon binds.";
      };
      chromeWsBase = lib.mkOption {
        type = lib.types.str;
        default = "";
        example = "wss://chrome.example.com";
        description = ''
          Public base URL of the CDP WebSocket the takeover page's browser
          connects to directly. Required when takeover is enabled.
        '';
      };
    };
  };

  config = lib.mkIf cfg.enable (
    lib.mkMerge [
      # Shared state directory (logs 0755, saved cookies 0700 — they're bearer
      # credentials).
      {
        systemd.tmpfiles.rules = [
          "d ${cfg.stateDir} 0755 ${cfg.user} ${cfg.group} - -"
          "d ${cfg.stateDir}/logs 0755 ${cfg.user} ${cfg.group} - -"
          "d ${cfg.stateDir}/states 0700 ${cfg.user} ${cfg.group} - -"
        ];
      }

      # Persistent headless Chrome.
      (lib.mkIf cfg.chrome.enable {
        users.users = lib.mkIf (cfg.chrome.user == "chrome-headless") {
          chrome-headless = {
            isSystemUser = true;
            group = cfg.chrome.group;
            home = cfg.chrome.dataDir;
            createHome = false;
          };
        };
        users.groups = lib.mkIf (cfg.chrome.group == "chrome-headless") {
          chrome-headless = { };
        };

        systemd.tmpfiles.rules = [
          "d ${cfg.chrome.dataDir} 0750 ${cfg.chrome.user} ${cfg.chrome.group} - -"
        ];

        systemd.services.chrome-headless = {
          description = "Persistent headless Chrome for automation";
          after = [ "network.target" ];
          wantedBy = [ "multi-user.target" ];
          environment = cfg.chrome.environment;
          serviceConfig = {
            Type = "simple";
            User = cfg.chrome.user;
            Group = cfg.chrome.group;
            WorkingDirectory = cfg.chrome.dataDir;
            Restart = "on-failure";
            RestartSec = 5;
            ExecStart = lib.concatStringsSep " " (
              [ "${cfg.chrome.package}/bin/${cfg.chrome.executable}" ] ++ chromeArgs
            );
          };
        };
      })

      # Always-on console + network capture.
      (lib.mkIf cfg.listener.enable {
        systemd.services.browser-session-listener = {
          description = "browser-session event listener (console + network → NDJSON)";
          after = lib.optional cfg.chrome.enable "chrome-headless.service";
          requires = lib.optional cfg.chrome.enable "chrome-headless.service";
          wantedBy = [ "multi-user.target" ];
          environment = {
            BROWSER_URL = cfg.browserUrl;
            LOGS_DIR = "${cfg.stateDir}/logs";
          };
          serviceConfig = {
            Type = "simple";
            ExecStart = "${exe} listener";
            User = cfg.user;
            Group = cfg.group;
          }
          // daemonHardening;
        };
      })

      # Idle-session reaper (oneshot on a timer).
      (lib.mkIf cfg.reaper.enable {
        systemd.services.browser-session-reaper = {
          description = "Close idle browser-session sessions";
          after = lib.optional cfg.chrome.enable "chrome-headless.service";
          requires = lib.optional cfg.chrome.enable "chrome-headless.service";
          environment = {
            BROWSER_URL = cfg.browserUrl;
            STATE_FILE = "${cfg.stateDir}/state.json";
            MAX_IDLE_HOURS = toString cfg.reaper.maxIdleHours;
          };
          serviceConfig = {
            Type = "oneshot";
            ExecStart = "${exe} reaper";
            User = cfg.user;
            Group = cfg.group;
            StandardOutput = "journal";
            StandardError = "journal";
          };
        };

        systemd.timers.browser-session-reaper = {
          description = "Periodic reaper for browser-session";
          wantedBy = [ "timers.target" ];
          timerConfig = {
            # Give Chrome a chance to come up before the first sweep.
            OnBootSec = "5min";
            OnUnitActiveSec = cfg.reaper.interval;
            Persistent = true;
          };
        };
      })

      # Human-takeover page server.
      (lib.mkIf cfg.takeover.enable {
        assertions = [
          {
            assertion = cfg.takeover.chromeWsBase != "";
            message = "services.browser-session.takeover.chromeWsBase must be set (e.g. wss://chrome.example.com) when takeover is enabled.";
          }
        ];

        systemd.services.browser-session-takeover = {
          description = "browser-session human-takeover page server";
          after = [ "network.target" ];
          wantedBy = [ "multi-user.target" ];
          environment = {
            TAKEOVER_BIND = "${cfg.takeover.address}:${toString cfg.takeover.port}";
            TAKEOVER_DIR = "${cfg.stateDir}/takeover";
            CHROME_WS_BASE = cfg.takeover.chromeWsBase;
          };
          serviceConfig = {
            Type = "simple";
            ExecStart = "${exe} takeover";
            User = cfg.user;
            Group = cfg.group;
          }
          // daemonHardening;
        };

        # Tickets carry a sessionId + targetId (not secrets), but the token is the
        # only guard on the live link — keep the tree owner-only.
        systemd.tmpfiles.rules = [
          "d ${cfg.stateDir}/takeover 0700 ${cfg.user} ${cfg.group} - -"
          "d ${cfg.stateDir}/takeover/tokens 0700 ${cfg.user} ${cfg.group} - -"
          "d ${cfg.stateDir}/takeover/done 0700 ${cfg.user} ${cfg.group} - -"
          "d ${cfg.stateDir}/takeover/claims 0700 ${cfg.user} ${cfg.group} - -"
          "d ${cfg.stateDir}/takeover/stealth 0700 ${cfg.user} ${cfg.group} - -"
        ];
      })
    ]
  );
}
