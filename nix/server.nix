# services.replique.server: Réplique's server on a NixOS host (a NAS, or any
# machine), as one hardened systemd service with a dynamic user.
#
# Secrets never enter the Nix store: the admin password hash and the Hetzner
# token are files named here, handed to the service as systemd credentials, and
# the generated replique.toml only points at them. Everything else the server
# keeps (database, master key, its SSH key, restic's cache) is in
# /var/lib/replique.
self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.replique.server;
  inherit (lib)
    literalExpression
    mkEnableOption
    mkIf
    mkOption
    optional
    optionalAttrs
    types
    ;
  system = pkgs.stdenv.hostPlatform.system;

  toml = pkgs.formats.toml { };
  # systemd puts credentials here; the path is fixed for a given unit name.
  credentials = "/run/credentials/replique-server.service";

  settings = lib.recursiveUpdate cfg.settings (
    {
      state_dir = "/var/lib/replique";
      restore.restic = lib.getExe cfg.resticPackage;
    }
    // optionalAttrs (cfg.adminPasswordHashFile != null) {
      admin_password_hash_file = "${credentials}/admin-password-hash";
    }
    // optionalAttrs (cfg.hetznerTokenFile != null) {
      storage_box.hetzner_token_file = "${credentials}/hetzner-token";
    }
  );
  # TOML has no null: an unset option is left out, so the server uses its default.
  settingsFile = toml.generate "replique.toml" (lib.filterAttrsRecursive (_: v: v != null) settings);

  port = lib.toInt (lib.last (lib.splitString ":" cfg.settings.listen));
in
{
  options.services.replique.server = {
    enable = mkEnableOption "the Réplique server";

    package = mkOption {
      type = types.package;
      default = self.packages.${system}.replique-server;
      defaultText = literalExpression "replique.packages.\${system}.replique-server";
      description = "The replique-server package.";
    };

    resticPackage = mkOption {
      type = types.package;
      default = pkgs.restic;
      defaultText = literalExpression "pkgs.restic";
      description = "restic, which serves website restores.";
    };

    adminPasswordHashFile = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "/run/secrets/replique-admin-hash";
      description = ''
        A file holding the admin password's argon2 hash, as printed by
        `replique-server hash-password`. Without it nobody can log in.
      '';
    };

    hetznerTokenFile = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "/run/secrets/hetzner-token";
      description = ''
        A file holding the Hetzner API token (read and write: it creates
        sub-accounts). When null, the token is typed on the Settings page and
        stored encrypted in the database.
      '';
    };

    openFirewall = mkOption {
      type = types.bool;
      default = false;
      description = "Open the port of `settings.listen` (better: put a reverse proxy with HTTPS in front).";
    };

    settings = mkOption {
      description = ''
        replique.toml (every option: docs/server.md in the replique repository).
        `state_dir`, `restore.restic` and the secret file paths are set by this
        module.
      '';
      default = { };
      example = literalExpression ''
        {
          public_url = "https://replique.example.com";
          owner_name = "Sigma";
          storage_box = {
            id = 718677;
            measure_via = "twin";
            key_install = "install-ssh-key";
          };
        }
      '';
      type = types.submodule {
        freeformType = toml.type;
        options = {
          listen = mkOption {
            type = types.str;
            default = "127.0.0.1:8080";
            description = "Address and port of the website and the device API.";
          };
          public_url = mkOption {
            type = types.str;
            example = "https://replique.example.com";
            description = "The address devices and browsers use; it goes into pairing links.";
          };
          owner_name = mkOption {
            type = types.str;
            default = "admin";
            description = "The owner of every device; its slug starts each repository's home on the box.";
          };
          storage_box = mkOption {
            default = { };
            type = types.submodule {
              freeformType = toml.type;
              options = {
                id = mkOption {
                  type = types.nullOr types.ints.positive;
                  default = null;
                  description = "The Storage Box's id in Hetzner's API (also settable on the Settings page).";
                };
                measure_via = mkOption {
                  type = types.enum [
                    "twin"
                    "main"
                  ];
                  default = "twin";
                  description = ''
                    How repositories are read for measurements and restores:
                    through a read-only twin sub-account per device, or through
                    the main account (with Réplique's key added to it by hand).
                  '';
                };
                key_install = mkOption {
                  type = types.enum [
                    "install-ssh-key"
                    "sftp"
                    "manual"
                  ];
                  default = "install-ssh-key";
                  description = ''
                    How pairing puts a device's key on its sub-account:
                    `install-ssh-key` over SSH port 23, writing
                    `.ssh/authorized_keys` over SFTP, or a command shown to the
                    admin.
                  '';
                };
              };
            };
          };
        };
      };
    };
  };

  config = mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];

    networking.firewall.allowedTCPPorts = mkIf cfg.openFirewall [ port ];

    systemd.services.replique-server = {
      description = "Réplique server";
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      wantedBy = [ "multi-user.target" ];
      restartTriggers = [ settingsFile ];
      environment.REPLIQUE_LOG = lib.mkDefault "info,russh=warn,russh_sftp=warn";
      serviceConfig = {
        ExecStart = "${lib.getExe cfg.package} --config ${settingsFile} serve";
        Restart = "on-failure";
        RestartSec = 5;
        DynamicUser = true;
        StateDirectory = "replique";
        StateDirectoryMode = "0700";
        LoadCredential =
          optional (cfg.adminPasswordHashFile != null) "admin-password-hash:${cfg.adminPasswordHashFile}"
          ++ optional (cfg.hetznerTokenFile != null) "hetzner-token:${cfg.hetznerTokenFile}";
        # It talks HTTP, SSH and HTTPS, and runs restic; nothing else.
        NoNewPrivileges = true;
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectKernelLogs = true;
        ProtectControlGroups = true;
        ProtectClock = true;
        ProtectHostname = true;
        RestrictAddressFamilies = [
          "AF_INET"
          "AF_INET6"
          "AF_UNIX"
        ];
        RestrictNamespaces = true;
        RestrictRealtime = true;
        LockPersonality = true;
        SystemCallArchitectures = "native";
        CapabilityBoundingSet = "";
        UMask = "0077";
      };
    };
  };
}
