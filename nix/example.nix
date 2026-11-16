# The PC's backup jobs, as they were with services.restic.backups, written for
# services.replique (docs/client.md shows the mapping option by option). The
# flake's `example` check evaluates it, so it stays a valid configuration.
# Paths marked "adjust" depend on the machine.
{
  services.replique.client = {
    enable = true;
    server = "https://replique.sigma-dev.eu";
  };

  services.replique.jobs = {
    # All of /home/sigma, daily.
    hetzner = {
      user = "sigma";
      paths = [ "/home/sigma" ];
      exclude = [
        "/home/sigma/.local/share/Steam"
        "/home/sigma/.cache"
        "/home/sigma/.rustup"
        "/home/sigma/.cargo/registry"
        "/home/sigma/Games/Heroic"
        "/home/sigma/Backups"
        "/home/sigma/Datasets/large-dataset" # adjust: the 116 GB downloadable dataset
        # Browser caches outside ~/.cache.
        "/home/sigma/.config/*/Default/Service Worker/CacheStorage"
        "/home/sigma/.config/*/Default/Code Cache"
        "/home/sigma/.config/*/Default/GPUCache"
      ];
      # cargo's target/ (and anything else with a CACHEDIR.TAG)…
      excludeCaches = true;
      # …and Unity, Unreal, .NET and Godot build output, found before each run.
      generatedExcludesFor = [ "/home/sigma" ];
      schedule = "daily";
      persistent = true;
      randomizedDelay = "1h";
      # Was: --keep-daily 7 --keep-weekly 4 --keep-monthly unlimited. Now files
      # deleted from the PC are kept for good as well.
      retention = {
        policy = "keep-deleted";
        keep = {
          daily = 7;
          weekly = 4;
          monthly = "unlimited";
        };
      };
      checkSubset = "5%";
      nice = 10;
      ioIdle = true;
      inhibitSleep = true;
    };

    # The Windows partitions, mounted read-only by udisks; run by hand.
    windows = {
      user = "sigma";
      paths = [
        "/run/media/sigma/Windows/Users" # adjust: C:\Users
        "/run/media/sigma/Data" # adjust: the data HDD
      ];
      requireMounts = [
        "/run/media/sigma/Windows"
        "/run/media/sigma/Data"
      ];
      exclude = [
        "$RECYCLE.BIN"
        "System Volume Information"
        "/run/media/sigma/Windows/Users/*/AppData/Local/Temp"
        "/run/media/sigma/Windows/Users/*/AppData/Local/*/Cache"
        "/run/media/sigma/Windows/Users/*/AppData/Local/Packages"
        "/run/media/sigma/Data/Games" # adjust
        "/run/media/sigma/Data/Installers" # adjust
        "/run/media/sigma/Data/Program Files" # adjust
      ];
      generatedExcludesFor = [ "/run/media/sigma/Data" ];
      schedule = null;
      retention = {
        policy = "keep-deleted";
        keep = {
          daily = 7;
          weekly = 4;
          monthly = "unlimited";
        };
      };
      checkSubset = "5%";
      nice = 10;
      ioIdle = true;
      inhibitSleep = true;
    };

    # The phone's repository, copied daily (after Phase 2 pairs the phone and
    # assigns this mirror on the website).
    phone = {
      kind = "mirror";
      user = "sigma";
      mirrorSource = "dev_01JCX5B2K9P4Q8R1S6T3U7V0CD"; # adjust: the phone's device id
      mirrorPath = "/home/sigma/Backups/phone";
      schedule = "daily";
      randomizedDelay = "1h";
      nice = 10;
      ioIdle = true;
    };
  };

  # The Bitwarden desktop app's vault, encrypted with the master password, sent
  # to Réplique whenever it changes.
  services.replique.bitwardenCopy = {
    enable = true;
    user = "sigma";
  };
}
