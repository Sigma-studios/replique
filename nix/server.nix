# services.replique.server: Workstream A fills this in (see the implementation
# plan, Workstream A, task 11).
self:
{ lib, ... }:
{
  options.services.replique.server = {
    enable = lib.mkEnableOption "the Réplique server";
  };
}
