//! The SSH side against a real SSH/SFTP server standing in for the Storage
//! Box: key installation with a password (both ways), measuring with
//! Réplique's key, restic through `replique-server sftp-bridge`, host key
//! pinning and deleting a repository.
//!
//! Needs a server: `REPLIQUE_TEST_SFTP=127.0.0.1:2222` with a user `dev`
//! (password in `REPLIQUE_TEST_SFTP_PASSWORD`, default `DevPass-1234`) that
//! has a shell and an `install-ssh-key` command appending stdin to its
//! `authorized_keys`. docs/server.md shows the container used. Skipped when
//! the variable is unset.

mod common;

use std::io::Write;

use common::*;
use replique_server::restore::Restic;
use replique_server::secrets::Secret;
use replique_server::ssh::{self, Auth, Endpoint, Trust};
use replique_server::storage::Access;

fn target() -> Option<(String, u16, Secret)> {
    let addr = std::env::var("REPLIQUE_TEST_SFTP").ok()?;
    let (host, port) = addr.rsplit_once(':')?;
    let password =
        std::env::var("REPLIQUE_TEST_SFTP_PASSWORD").unwrap_or_else(|_| "DevPass-1234".into());
    Some((host.to_string(), port.parse().ok()?, Secret::new(password)))
}

fn restic_with(
    args: &[std::ffi::OsString],
    password: &str,
    extra: &[&str],
) -> std::process::Output {
    let mut child = std::process::Command::new("restic")
        .args(args)
        .args(extra)
        .env_remove("RESTIC_PASSWORD")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{password}\n").as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "restic {extra:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn storage_box_over_sftp() {
    let Some((host, port, password)) = target() else {
        eprintln!("skipped: set REPLIQUE_TEST_SFTP=host:port to run against an SFTP server");
        return;
    };
    for mode in ["sftp", "install-ssh-key"] {
        let server = start_with(&format!("key_install = \"{mode}\"\nport = {port}"), false).await;
        let app = &server.app;
        let endpoint = Endpoint {
            host: host.clone(),
            port,
            username: "dev".into(),
        };
        // Pairing's step: the device's key and Réplique's, with the password once.
        let device_key = device_public_key();
        let seen = app
            .install_keys(
                &endpoint,
                "",
                &password,
                &[device_key.clone(), app.public_key_line()],
            )
            .await
            .unwrap_or_else(|e| panic!("{mode}: {e:#}"));
        assert!(seen.is_some());
        // The host key is remembered and handed to devices.
        assert!(app.store.host_key(&host, port).unwrap().is_some());
        assert_eq!(app.known_hosts_lines(&host, port).len(), 1);
        assert!(app.known_hosts_lines(&host, port)[0].starts_with(&format!("[{host}]:{port} ")));

        // From now on Réplique's key logs in.
        let conn = app
            .ssh_connect(&endpoint, Auth::Key(&app.ssh_key))
            .await
            .unwrap();
        let sftp = conn.sftp().await.unwrap();
        let keys = String::from_utf8(sftp.read(".ssh/authorized_keys").await.unwrap()).unwrap();
        assert!(keys.contains("test-device") && keys.contains("replique-server"));
        let _ = sftp.close().await;
        conn.close().await;

        // restic through the bridge, as website restores run it.
        let access = Access::Sftp {
            endpoint: endpoint.clone(),
            path: "/home/dev/restic".into(),
        };
        let args = app.restic_repo_args(&access).unwrap();
        let repo_password = "sftp-test-password";
        restic_with(&args, repo_password, &["init"]);
        let data = server.dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("hello.txt"), "over the bridge").unwrap();
        restic_with(&args, repo_password, &["backup", data.to_str().unwrap()]);

        // Measured with Réplique's key.
        let (bytes, files) = app.measure(&access).await.unwrap();
        assert!(bytes > 500 && files > 4, "{bytes} bytes in {files} files");

        // Browsed and downloaded through the bridge.
        let restic = Restic::new(app, &access, Secret::new(repo_password.into())).unwrap();
        let snapshots = restic.snapshots().await.unwrap();
        assert_eq!(snapshots.len(), 1);
        let dump = restic
            .dump("latest", &format!("{}/hello.txt", data.display()), false)
            .await
            .unwrap();
        let body = axum::body::to_bytes(dump.into_body(), 1 << 20)
            .await
            .unwrap();
        assert_eq!(&body[..], b"over the bridge");

        // A different host key is refused.
        let other = ssh::load_or_create_key(&server.dir.path().join("other")).unwrap();
        let refused = ssh::connect(
            &endpoint,
            Auth::Key(&app.ssh_key),
            Trust {
                keys: vec![other.public_key().clone()],
            },
        )
        .await;
        assert!(refused.is_err());

        // Removing a device with its data deletes the repository.
        app.delete_repository_data(&endpoint, "", &password)
            .await
            .unwrap();
        assert!(
            app.measure(&access).await.is_err(),
            "the repository is gone"
        );
    }
}
