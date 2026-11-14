# NixOS module

`nixosModules.default` provides two halves, each inert unless enabled, so a
machine can run either or both:

* `services.replique.server` (`server.nix`): the server as a systemd service,
  for a NAS or any NixOS host. See [docs/server.md](../docs/server.md), "On NixOS".
* `services.replique.client` and `services.replique.jobs` (`client.nix`): the
  client, one service and timer per job, the polkit rule for blocking sleep, the
  Storage Box SSH alias and `replique-<job>` restic wrappers. See
  [docs/client.md](../docs/client.md), "Module reference", and `example.nix` for
  the PC's two jobs.

| File | What |
| --- | --- |
| `package.nix` | `replique-server` and `replique-client`, built with crane |
| `example.nix` | The PC's jobs on the module, evaluated by `checks.<system>.example` |
| `test.nix` | `checks.<system>.vm`: pairing, runs, a refused job, a mirror and an outage on three VMs |
| `tests/mock-server.py` | The stand-in server `test.nix` uses for now |
