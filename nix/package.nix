# replique-server and replique-client, built from source with crane.
{
  pkgs,
  crane,
}:
let
  inherit (pkgs) lib;

  # The minimal profile: building needs no docs, clippy or rustfmt, and the
  # default profile's docs alone are hundreds of megabytes in the store.
  craneLib = (crane.mkLib pkgs).overrideToolchain (p: p.rust-bin.stable.latest.minimal);

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../crates
      # The proto crate's tests read the shared fixtures.
      ../proto
    ];
  };

  common = {
    inherit src;
    version = (lib.importTOML ../Cargo.toml).workspace.package.version;
    strictDeps = true;
    # `cargo test` needs restic and, for some tests, a local SFTP server; it runs
    # in the dev shell and CI rather than in the package build.
    doCheck = false;
  };

  # Dependencies of the whole workspace, built once for both binaries.
  cargoArtifacts = craneLib.buildDepsOnly (common // { pname = "replique"; });

  bin =
    name: description: extra:
    craneLib.buildPackage (
      common
      // {
        pname = name;
        inherit cargoArtifacts;
        cargoExtraArgs = "--locked --bin ${name}";
        meta = {
          inherit description;
          homepage = "https://github.com/Sigma-studios/replique";
          license = lib.licenses.mit;
          mainProgram = name;
          platforms = lib.platforms.linux;
        };
      }
      // extra
    );
in
{
  replique-server =
    bin "replique-server" "Réplique's server: API, website, measurements and alerts"
      { };
  # restic, rclone (mirrors) and ssh (restic's SFTP backend) come along, after
  # whatever PATH already has.
  replique-client =
    bin "replique-client" "Runs restic on a device and reports every run to Réplique"
      {
        nativeBuildInputs = [ pkgs.makeWrapper ];
        postInstall = ''
          wrapProgram $out/bin/replique-client \
            --suffix PATH : ${
              lib.makeBinPath [
                pkgs.restic
                pkgs.rclone
                pkgs.openssh
              ]
            }
        '';
      };
}
