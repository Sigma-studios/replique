{
  description = "replique: encrypted backups of every device, set up, watched and restored from one place";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      rust-overlay,
    }:
    let
      inherit (nixpkgs) lib;
      # Packages and the module target Linux (the PC, the VPS, a NAS); the dev
      # shell also works on a Mac, without the Android SDK.
      linux = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      all = linux ++ [ "aarch64-darwin" ];
      pkgsFor =
        system:
        import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
          # The Android SDK is unfree and needs its licence accepted.
          config = {
            allowUnfree = true;
            android_sdk.accept_license = true;
          };
        };
      forSystems = systems: f: lib.genAttrs systems (system: f (pkgsFor system));
    in
    {
      packages = forSystems linux (
        pkgs:
        let
          replique = import ./nix/package.nix { inherit pkgs crane; };
        in
        {
          inherit (replique) replique-server replique-client;
          default = replique.replique-server;
        }
      );

      # services.replique.{server,client,jobs}: see nix/README.md.
      nixosModules.default = import ./nix/module.nix self;
      nixosModules.replique = self.nixosModules.default;

      checks = forSystems linux (
        pkgs:
        let
          system = pkgs.stdenv.hostPlatform.system;
          # nix/example.nix (the PC's jobs) on a bare machine: the module's
          # assertions hold and it writes this client.json.
          example = nixpkgs.lib.nixosSystem {
            inherit system;
            modules = [
              self.nixosModules.default
              ./nix/example.nix
              {
                boot.loader.grub.enable = false;
                fileSystems."/" = {
                  device = "none";
                  fsType = "tmpfs";
                };
                users.users.sigma.isNormalUser = true;
                system.stateVersion = "26.05";
              }
            ];
          };
          failed = map (a: a.message) (lib.filter (a: !a.assertion) example.config.assertions);
        in
        {
          inherit (self.packages.${system}) replique-server replique-client;
          example =
            assert lib.assertMsg (failed == [ ]) (lib.concatStringsSep "\n" failed);
            example.config.environment.etc."replique/client.json".source;
          # Pairing, a backup with its prune and check, a refused job, a mirror
          # and an outage, on three VMs (needs KVM):
          # `nix build .#checks.x86_64-linux.vm -L`.
          vm = import ./nix/test.nix { inherit pkgs self; };
        }
      );

      devShells = forSystems all (
        pkgs:
        let
          android = pkgs.androidenv.composeAndroidPackages {
            platformVersions = [ "36" ];
            buildToolsVersions = [ "36.0.0" ];
            includeNDK = true;
          };
        in
        {
          default = pkgs.mkShell {
            packages =
              with pkgs;
              [
                (rust-bin.stable.latest.default.override {
                  extensions = [
                    "rust-src"
                    "rust-analyzer"
                  ];
                })
                restic
                rclone
                sqlite
                openssh
                go
                jdk17
                gradle
              ]
              ++ lib.optional stdenv.isLinux android.androidsdk;
            shellHook = lib.optionalString pkgs.stdenv.isLinux ''
              export ANDROID_HOME=${android.androidsdk}/libexec/android-sdk
              export ANDROID_SDK_ROOT=$ANDROID_HOME
            '';
          };
        }
      );

      formatter = forSystems all (pkgs: pkgs.nixfmt);
    };
}
