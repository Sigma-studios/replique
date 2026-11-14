# services.replique: the server (a NAS, or any NixOS host) and the client with its
# backup jobs (the PC). Each half lives in its own file and does nothing unless
# enabled, so a machine can run either or both.
self:
{ ... }:
{
  imports = [
    (import ./server.nix self)
    (import ./client.nix self)
  ];
}
