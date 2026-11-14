# services.replique.client and services.replique.jobs: Workstream B fills this in
# (see the implementation plan, Workstream B, "NixOS module").
self:
{ lib, ... }:
{
  options.services.replique.client = {
    enable = lib.mkEnableOption "the Réplique client and its backup jobs";
  };
}
