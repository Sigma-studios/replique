# Three VMs: `storage`, an SFTP server on port 23 standing in for the Storage
# Box; `server`, a stand-in for Réplique's server (tests/mock-server.py, the
# device endpoints of docs/protocol.md); `client`, a NixOS machine using
# services.replique with jobs shaped like the PC's. The client pairs with an
# existing key and password (`--adopt`, as the PC will), backs up, prunes and
# checks; a job whose mount is missing is refused; the mirror copies the
# repository through the login /mirror-sources lists; reports made while the
# server is down arrive once it is back.
#
#   nix build .#checks.x86_64-linux.vm -L     (needs KVM)
{ pkgs, self }:
let
  # The "existing" key and password the client adopts. A key in the Nix store
  # is world-readable: fine for a test, never for a real machine.
  key = pkgs.runCommand "replique-test-key" { nativeBuildInputs = [ pkgs.openssh ]; } ''
    mkdir $out
    ssh-keygen -q -t ed25519 -N "" -C test -f $out/key
  '';
  password = pkgs.writeText "restic-password" "vm test password\n";
in
pkgs.testers.runNixOSTest {
  name = "replique-client";

  nodes.storage = {
    services.openssh = {
      enable = true;
      ports = [ 23 ];
    };
    networking.firewall.allowedTCPPorts = [ 23 ];
    # Its key is installed by the test script: reading a built file here would
    # make evaluating the test build it first.
    users.users.u1-sub1.isNormalUser = true;
    # The sub-account sees its repository as /home/restic.
    systemd.tmpfiles.rules = [ "d /home/restic 0700 u1-sub1 users - -" ];
  };

  nodes.server = {
    networking.firewall.allowedTCPPorts = [ 8080 ];
    systemd.services.replique-mock = {
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      path = [ pkgs.openssh ];
      serviceConfig = {
        ExecStart = "${pkgs.python3.interpreter} ${./tests/mock-server.py} 8080 /var/lib/replique-mock/state.json storage 23 u1-sub1";
        StateDirectory = "replique-mock";
      };
    };
    environment.systemPackages = [ pkgs.curl ];
  };

  nodes.client = {
    imports = [ self.nixosModules.default ];
    virtualisation.memorySize = 1536;
    users.users.alice.isNormalUser = true;
    environment.systemPackages = [ pkgs.restic ];

    services.replique.client = {
      enable = true;
      server = "http://server:8080";
    };
    services.replique.jobs = {
      home = {
        user = "alice";
        paths = [ "/home/alice" ];
        exclude = [
          "/home/alice/.cache"
          "$RECYCLE.BIN"
        ];
        excludeCaches = true;
        generatedExcludesFor = [ "/home/alice" ];
        randomizedDelay = "1h";
        retention.keep = {
          daily = 7;
          weekly = 4;
          monthly = "unlimited";
        };
        checkSubset = "10%";
        nice = 10;
        ioIdle = true;
        inhibitSleep = true;
      };
      windows = {
        user = "alice";
        paths = [ "/mnt/windows/Users" ];
        requireMounts = [ "/mnt/windows" ];
        schedule = null;
      };
      phone = {
        kind = "mirror";
        user = "alice";
        mirrorSource = "dev_PHONE";
        mirrorPath = "/home/alice/Backups/phone";
      };
    };
  };

  testScript = ''
    import json

    def server_state():
        return json.loads(server.succeed("curl -fsS http://localhost:8080/debug/state"))

    def runs(kind):
        return [r for r in server_state()["runs"].values() if r["start"]["kind"] == kind]

    start_all()
    storage.wait_for_open_port(23)
    storage.succeed("install -D -o u1-sub1 -m 600 ${key}/key.pub /home/u1-sub1/.ssh/authorized_keys")
    server.wait_for_unit("replique-mock.service")
    server.wait_for_open_port(8080)
    client.wait_for_unit("multi-user.target")

    with subtest("the module's units and files"):
        client.succeed("systemctl cat replique-home.service | grep -F 'systemd-inhibit'")
        client.succeed("systemctl cat replique-home.service | grep -F 'Nice=10'")
        client.succeed("systemctl cat replique-home.service | grep -F 'IOSchedulingClass=idle'")
        client.succeed("systemctl list-timers | grep -F replique-home.timer")
        client.succeed("systemctl list-timers | grep -F replique-phone.timer")
        client.fail("systemctl cat replique-windows.timer")
        client.succeed("test \"$(stat -c '%U:%G %a' /var/lib/replique-client)\" = 'replique-client:replique-client 2770'")
        client.succeed("id -nG alice | grep -w replique-client")
        client.succeed("grep -F 'Include /var/lib/replique-client/ssh/include.conf' /etc/ssh/ssh_config")
        client.succeed("grep -F 'org.freedesktop.login1.inhibit-block-sleep' /etc/polkit-1/rules.d/*.rules")
        cfg = json.loads(client.succeed("cat /etc/replique/client.json"))
        assert cfg["jobs"]["home"]["retention"] == {
            "policy": "keep-deleted",
            "keep": {"daily": 7, "weekly": 4, "monthly": "unlimited"},
        }, cfg

    with subtest("pairing with the existing key and password"):
        client.succeed("replique-client pair --adopt ${key}/key ${password} http://server:8080 K7QM-2XPA >&2")
        assert server_state()["pairs"][0]["ssh_public_key"].split()[1] in client.succeed("cat ${key}/key.pub")
        for f in ["token", "restic-password", "ssh/id_ed25519"]:
            client.succeed(f"test \"$(stat -c '%U:%G %a' /var/lib/replique-client/{f})\" = 'replique-client:replique-client 640'")
        # Included by /etc/ssh/ssh_config: OpenSSH insists it be root's.
        client.succeed("test \"$(stat -c '%U %a' /var/lib/replique-client/ssh/include.conf)\" = 'root 644'")
        client.succeed("echo 'ls /home/restic' | sudo -u alice sftp -b - replique-storage")
        client.succeed("echo 'ls /home/restic' | sftp -b - replique-storage")

    with subtest("a backup, its prune and its check are three reported runs"):
        client.succeed(
            "sudo -u alice sh -c '"
            "mkdir -p ~/Documents ~/Games/Unity/Assets ~/Games/Unity/ProjectSettings ~/Games/Unity/Library ~/.cache"
            " && echo thesis > ~/Documents/thesis.tex && echo s > ~/Games/Unity/Assets/scene"
            " && echo p > ~/Games/Unity/ProjectSettings/p && echo big > ~/Games/Unity/Library/big"
            " && echo c > ~/.cache/c'"
        )
        client.succeed("systemctl start replique-home.service")
        backup = runs("backup")[0]["finish"]
        assert backup["status"] == "success", backup
        snapshot = backup["summary"]["snapshot_id"]
        assert runs("prune")[0]["finish"]["prune"]["policy"] == "keep-deleted"
        check = runs("check")[0]["finish"]
        assert check["status"] == "success" and snapshot in check["check"]["snapshot_ids"], check
        assert server_state()["jobs"]["home"]["rules"]["check_subset"] == "10%"

        snaps = json.loads(client.succeed("sudo -u alice replique-home snapshots --json"))
        assert [s["id"] for s in snaps] == [snapshot], snaps
        files = client.succeed("sudo -u alice replique-home ls latest")
        assert "/home/alice/Documents/thesis.tex" in files, files
        assert "/home/alice/Games/Unity/Assets/scene" in files, files
        assert "Library" not in files and ".cache" not in files, files
        client.succeed("sudo -u alice replique-client status | grep -E 'home +backup +success'")

    with subtest("a job whose mount is missing is refused, and says why"):
        client.fail("systemctl start replique-windows.service")
        f = runs("backup")[-1]["finish"]
        assert f["status"] == "failed" and f["message"] == "/mnt/windows is not a mountpoint", f

    with subtest("the mirror copies the repository through the listed login"):
        client.succeed("systemctl start replique-phone.service")
        m = runs("mirror")[0]["finish"]
        assert m["status"] == "success", m
        assert m["mirror"]["snapshot_ids"] == [snapshot], m
        assert m["mirror"]["hash_mismatches"] == [], m
        client.succeed(
            "RESTIC_PASSWORD_FILE=${password} restic -r /home/alice/Backups/phone --no-lock snapshots | grep -F "
            + snapshot[:8]
        )

    with subtest("reports made while the server is down arrive later"):
        server.succeed("systemctl stop replique-mock")
        client.succeed("sudo -u alice sh -c 'echo more > ~/Documents/notes'")
        client.succeed("systemctl start replique-home.service")
        client.succeed("ls /var/lib/replique-client/spool/*.json")
        server.succeed("systemctl start replique-mock")
        server.wait_for_open_port(8080)
        client.succeed("systemctl start replique-home.service")
        client.fail("ls /var/lib/replique-client/spool/*.json")
        backups = [r for r in runs("backup") if r["finish"]["status"] == "success"]
        assert len(backups) == 3, backups
  '';
}
