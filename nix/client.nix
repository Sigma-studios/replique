# services.replique.client and services.replique.jobs: backup jobs on a NixOS
# machine, each run by replique-client and reported to Réplique. Replaces
# services.restic.backups: every option of the user's earlier restic jobs has a
# counterpart here (see docs/client.md, "From services.restic.backups").
#
# Per job: a oneshot service `replique-<job>` running `replique-client run
# <job>`, a timer when it has a schedule, and a `replique-<job>` command exposing
# restic on the job's repository (`replique-hetzner snapshots`, `mount`,
# `restore`), like nixpkgs' restic-<name> wrappers.
self:
{
  config,
  lib,
  pkgs,
  utils,
  ...
}:
let
  cfg = config.services.replique.client;
  jobs = config.services.replique.jobs;
  inherit (lib)
    literalExpression
    mkEnableOption
    mkIf
    mkOption
    optionalAttrs
    types
    ;
  system = pkgs.stdenv.hostPlatform.system;
  exe = lib.getExe cfg.package;

  keepCount = types.nullOr (types.either types.ints.unsigned (types.enum [ "unlimited" ]));

  retentionType = types.submodule {
    options = {
      policy = mkOption {
        type = types.enum [
          "keep-deleted"
          "versions"
          "never-prune"
        ];
        default = "keep-deleted";
        description = ''
          `keep-deleted`: a file deleted on this machine is never pruned from its
          backups; only old versions of files that still exist age out, per
          `keep`. `versions`: plain `restic forget --prune` with `keep` (deleted
          files go once their last snapshot ages out). `never-prune`: nothing is
          ever forgotten.
        '';
      };
      keep = mkOption {
        description = "restic's `--keep-*` rules (null: not passed).";
        default = {
          daily = 7;
          weekly = 4;
          monthly = 12;
        };
        example = literalExpression ''{ daily = 7; weekly = 4; monthly = "unlimited"; }'';
        type = types.submodule {
          options =
            lib.genAttrs [ "last" "hourly" "daily" "weekly" "monthly" "yearly" ] (
              n:
              mkOption {
                type = keepCount;
                default = null;
                description = "`--keep-${n}`: a count or \"unlimited\".";
              }
            )
            // {
              within = mkOption {
                type = types.nullOr types.str;
                default = null;
                example = "14d";
                description = "`--keep-within`, a duration.";
              };
            };
        };
      };
    };
  };

  jobType = types.submodule (
    { name, ... }:
    {
      options = {
        kind = mkOption {
          type = types.enum [
            "backup"
            "mirror"
          ];
          default = "backup";
          description = ''
            `backup`: restic backup of `paths` into this device's repository.
            `mirror`: an encrypted byte copy of another device's repository (the
            phone's), pulled through its read-only twin login.
          '';
        };
        user = mkOption {
          type = types.str;
          default = "root";
          example = "sigma";
          description = ''
            The user the job runs as. It joins the `replique-client` group,
            which reads the paired secrets in `stateDir`.
          '';
        };
        paths = mkOption {
          type = types.listOf types.str;
          default = [ ];
          example = [ "/home/sigma" ];
          description = "What a backup job backs up.";
        };
        exclude = mkOption {
          type = types.listOf types.str;
          default = [ ];
          example = [
            "/home/sigma/.cache"
            "$RECYCLE.BIN"
          ];
          description = ''
            restic exclude patterns. Write them as you mean them: a literal `$`
            (`$RECYCLE.BIN`) is turned into `?` before restic, which would
            otherwise expand it as a variable.
          '';
        };
        excludeCaches = mkOption {
          type = types.bool;
          default = false;
          description = "restic `--exclude-caches`: skip folders with a `CACHEDIR.TAG` (cargo's `target/`).";
        };
        generatedExcludesFor = mkOption {
          type = types.listOf types.str;
          default = [ ];
          example = [ "/home/sigma" ];
          description = ''
            Folders scanned before each run for build output that has no
            `CACHEDIR.TAG`: Unity (`Library/`, `Temp/`… next to `Assets/` and
            `ProjectSettings/`), Unreal (`Intermediate/`, `DerivedDataCache/`,
            `Binaries/`, `Saved/` next to a `.uproject`), .NET (`bin/`, `obj/`
            next to a project file) and Godot (`.godot/`).
          '';
        };
        requireMounts = mkOption {
          type = types.listOf types.str;
          default = [ ];
          example = [ "/mnt/windows/c" ];
          description = ''
            Paths that must be mountpoints: if one is not, the run is refused and
            reported failed instead of backing up an empty folder.
          '';
        };
        schedule = mkOption {
          type = types.nullOr types.str;
          default = "daily";
          example = "*-*-* 03:00";
          description = "systemd `OnCalendar` for the job's timer; null for a job only run by hand (`systemctl start replique-${name}`).";
        };
        persistent = mkOption {
          type = types.bool;
          default = true;
          description = "Run at boot when the machine was off at the scheduled time (timer `Persistent=`).";
        };
        randomizedDelay = mkOption {
          type = types.nullOr types.str;
          default = null;
          example = "1h";
          description = "Timer `RandomizedDelaySec=`.";
        };
        retention = mkOption {
          type = retentionType;
          default = { };
          description = "Which snapshots the prune after each backup forgets. Ignored for a mirror.";
        };
        checkSubset = mkOption {
          type = types.nullOr types.str;
          default = null;
          example = "5%";
          description = "`restic check --read-data-subset` after each backup, reported as its own run; null for none.";
        };
        nice = mkOption {
          type = types.nullOr (types.ints.between (-20) 19);
          default = null;
          example = 10;
          description = "The service's `Nice=`.";
        };
        ioIdle = mkOption {
          type = types.bool;
          default = false;
          description = "Idle I/O scheduling (`IOSchedulingClass=idle`).";
        };
        inhibitSleep = mkOption {
          type = types.bool;
          default = false;
          description = ''
            Block sleep while the job runs (systemd-inhibit). Adds the polkit
            rule allowing `user` to, which a system service needs.
          '';
        };
        extraBackupArgs = mkOption {
          type = types.listOf types.str;
          default = [ ];
          example = [ "--one-file-system" ];
          description = "More `restic backup` arguments.";
        };
        host = mkOption {
          type = types.nullOr types.str;
          default = null;
          description = "restic `--host` for backups and retention (null: the machine's hostname).";
        };
        repository = mkOption {
          type = types.nullOr types.str;
          default = null;
          example = "/mnt/nas/restic";
          description = "Any restic repository instead of the paired Storage Box sub-account.";
        };
        passwordFile = mkOption {
          type = types.nullOr types.str;
          default = null;
          description = ''
            A restic password file instead of the paired one. For a mirror: the
            source's password, only needed for `replique-${name}` (the mirror
            itself never decrypts).
          '';
        };
        mirrorSource = mkOption {
          type = types.nullOr types.str;
          default = null;
          example = "dev_01JCX5B2K9P4Q8R1S6T3U7V0CD";
          description = "Mirror: the device id whose repository is copied, as the website shows it.";
        };
        mirrorPath = mkOption {
          type = types.nullOr types.str;
          default = null;
          example = "/home/sigma/Backups/phone";
          description = "Mirror: where the copy lives (a plain restic repository).";
        };
        mirrorRemovedPath = mkOption {
          type = types.nullOr types.str;
          default = null;
          description = "Mirror: where files gone from the source are moved (default `<mirrorPath>.removed`).";
        };
        keepRemovedDays = mkOption {
          type = types.ints.positive;
          default = 30;
          description = "Mirror: days files gone from the source are kept.";
        };
        mirrorFrom = mkOption {
          type = types.nullOr types.str;
          default = null;
          description = "Mirror: an rclone path to copy from instead of the login the server lists (a NAS share).";
        };
      };
    }
  );

  nonNull = lib.filterAttrs (_: v: v != null);

  # The job as replique-client reads it from /etc/replique/client.json.
  jobJson =
    job:
    nonNull {
      inherit (job)
        kind
        paths
        exclude
        schedule
        host
        repository
        ;
      exclude_caches = job.excludeCaches;
      generated_excludes_for = job.generatedExcludesFor;
      require_mounts = job.requireMounts;
      check_subset = job.checkSubset;
      extra_backup_args = job.extraBackupArgs;
      password_file = job.passwordFile;
      retention =
        if job.kind == "mirror" then
          null
        else if job.retention.policy == "never-prune" then
          { policy = "never-prune"; }
        else
          {
            inherit (job.retention) policy;
            keep = nonNull job.retention.keep;
          };
      mirror_source = job.mirrorSource;
      mirror_path = job.mirrorPath;
      removed_path = job.mirrorRemovedPath;
      keep_removed_days = job.keepRemovedDays;
      mirror_from = job.mirrorFrom;
    };

  configFile = pkgs.writeText "replique-client.json" (
    builtins.toJSON (nonNull {
      inherit (cfg) server;
      state_dir = cfg.stateDir;
      ssh_alias = cfg.sshAlias;
      jobs = lib.mapAttrs (_: jobJson) jobs;
    })
  );

  jobUsers = lib.unique (lib.mapAttrsToList (_: j: j.user) jobs);
  sleepUsers = lib.unique (
    lib.mapAttrsToList (_: j: j.user) (lib.filterAttrs (_: j: j.inhibitSleep) jobs)
  );

  command = name: job: [
    exe
    (if job.kind == "mirror" then "mirror" else "run")
    name
  ];

  wrapper =
    name: _:
    pkgs.writeShellScriptBin "replique-${name}" ''
      exec ${exe} restic ${lib.escapeShellArg name} "$@"
    '';
in
{
  options.services.replique = {
    client = {
      enable = mkEnableOption "the Réplique client and its backup jobs";

      package = mkOption {
        type = types.package;
        default = self.packages.${system}.replique-client;
        defaultText = literalExpression "replique.packages.\${system}.replique-client";
        description = "The replique-client package (it brings its own restic, rclone and ssh).";
      };

      server = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "https://replique.example.com";
        description = ''
          The server's URL. Pairing stores the URL it was given; this one wins
          when set, so the server can move without pairing again.
        '';
      };

      stateDir = mkOption {
        type = types.str;
        default = "/var/lib/replique-client";
        description = ''
          Pairing's secrets (report token, restic password, SSH key) and the
          runs' spool and status. Owned by the `replique-client` user and group;
          readable by job users only.
        '';
      };

      sshAlias = mkOption {
        type = types.str;
        default = "replique-storage";
        description = ''
          The SSH host alias of this device's Storage Box login (port 23, the
          paired key, the host keys from pairing), included in the system's
          ssh_config: `sftp replique-storage` works for job users.
        '';
      };
    };

    jobs = mkOption {
      type = types.attrsOf jobType;
      default = { };
      description = "Backup and mirror jobs, by name (`a-z`, `0-9`, `-`, `_`).";
      example = literalExpression ''
        {
          hetzner = {
            user = "sigma";
            paths = [ "/home/sigma" ];
            exclude = [ "/home/sigma/.cache" ];
            excludeCaches = true;
            generatedExcludesFor = [ "/home/sigma" ];
            randomizedDelay = "1h";
            checkSubset = "5%";
            nice = 10;
            ioIdle = true;
            inhibitSleep = true;
          };
        }
      '';
    };
  };

  config = lib.mkMerge [
    {
      warnings = lib.optional (
        jobs != { } && !cfg.enable
      ) "services.replique.jobs is set but services.replique.client.enable is false: no job will run.";
    }
    (mkIf cfg.enable {
      assertions =
        lib.mapAttrsToList (name: _: {
          assertion = builtins.match "[a-z0-9][a-z0-9_-]{0,63}" name != null;
          message = "services.replique.jobs.${name}: job names are 1 to 64 of a-z, 0-9, - and _, starting with a letter or digit.";
        }) jobs
        ++ lib.mapAttrsToList (name: j: {
          assertion = j.kind != "backup" || j.paths != [ ];
          message = "services.replique.jobs.${name}: a backup job needs paths.";
        }) jobs
        ++ lib.mapAttrsToList (name: j: {
          assertion =
            j.kind != "mirror" || (j.mirrorPath != null && (j.mirrorSource != null || j.mirrorFrom != null));
          message = "services.replique.jobs.${name}: a mirror job needs mirrorPath and mirrorSource.";
        }) jobs
        ++ lib.mapAttrsToList (name: j: {
          assertion =
            j.kind != "backup" || j.retention.policy == "never-prune" || nonNull j.retention.keep != { };
          message = "services.replique.jobs.${name}: retention.keep is empty, so nothing would ever be forgotten; use policy = \"never-prune\".";
        }) jobs;

      environment.etc."replique/client.json".source = configFile;
      environment.systemPackages = [ cfg.package ] ++ lib.mapAttrsToList wrapper jobs;

      # Owner of the state directory and its files. Root and the job users reach
      # them through the group; ssh refuses a group-readable key only when the
      # user running it owns the key, which neither does.
      # Not `replique`: the server's module may own that name on a machine
      # running both.
      users.users.replique-client = {
        isSystemUser = true;
        group = "replique-client";
        home = cfg.stateDir;
      };
      users.groups.replique-client.members = lib.filter (u: u != "root") jobUsers;

      # setgid: files created inside take the group, so a run by one job user
      # leaves spool and status readable by the others.
      systemd.tmpfiles.rules = [ "d ${cfg.stateDir} 2770 replique-client replique-client - -" ];

      # `ssh replique-storage` / `sftp replique-storage` by hand. `Match all`
      # first, in case configuration before this left a Host block open.
      programs.ssh.extraConfig = ''
        Match all
        Include ${cfg.stateDir}/ssh/config
      '';

      security.polkit = mkIf (sleepUsers != [ ]) {
        enable = true;
        # A system service running as a user has no session, so logind asks
        # polkit, which wants interactive authentication for blocking sleep.
        extraConfig = ''
          // Réplique: these users' backup jobs block sleep while they run.
          polkit.addRule(function (action, subject) {
            if (action.id == "org.freedesktop.login1.inhibit-block-sleep" &&
                ${builtins.toJSON sleepUsers}.indexOf(subject.user) >= 0) {
              return polkit.Result.YES;
            }
          });
        '';
      };

      systemd.services = lib.mapAttrs' (
        name: job:
        lib.nameValuePair "replique-${name}" {
          description = "Réplique ${job.kind} job ${name}";
          wants = [ "network-online.target" ];
          after = [ "network-online.target" ];
          # A rebuild must not kill a backup that is running.
          restartIfChanged = false;
          environment = {
            REPLIQUE_CONFIG = "/etc/replique/client.json";
            RESTIC_CACHE_DIR = "/var/cache/replique-${name}";
          };
          serviceConfig = {
            Type = "oneshot";
            User = job.user;
            SupplementaryGroups = [ "replique-client" ];
            ExecStart = utils.escapeSystemdExecArgs (
              lib.optionals job.inhibitSleep [
                "${config.systemd.package}/bin/systemd-inhibit"
                "--what=sleep"
                "--mode=block"
                "--who=Réplique"
                "--why=Backup job ${name}"
              ]
              ++ command name job
            );
            CacheDirectory = "replique-${name}";
            CacheDirectoryMode = "0700";
            # On shutdown the client waits for restic to stop and reports the run
            # interrupted (spooled if the network is already gone).
            TimeoutStopSec = "2min";
          }
          // optionalAttrs (job.nice != null) { Nice = job.nice; }
          // optionalAttrs job.ioIdle { IOSchedulingClass = "idle"; };
        }
      ) jobs;

      systemd.timers = lib.mapAttrs' (
        name: job:
        lib.nameValuePair "replique-${name}" {
          description = "Réplique ${job.kind} job ${name}";
          wantedBy = [ "timers.target" ];
          timerConfig = {
            OnCalendar = job.schedule;
            Persistent = job.persistent;
          }
          // optionalAttrs (job.randomizedDelay != null) { RandomizedDelaySec = job.randomizedDelay; };
        }
      ) (lib.filterAttrs (_: j: j.schedule != null) jobs);
    })
  ];
}
