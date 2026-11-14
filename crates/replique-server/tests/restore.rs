//! Website restores of a fixture snapshot: unlock, browse, download a file and
//! a folder as a zip; the password never shows up in the logs and is forgotten
//! on lock, logout and after the idle timeout.

mod common;

use std::io::Read;

use common::*;
use replique_proto::{DeviceKind, PairRequest, PairResponse, paths};
use reqwest::StatusCode;

const PASSWORD: &str = "Zebra-unique-restic-pass-8472";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unlock_browse_download_and_forget() {
    let server = start("[restore]\nidle_timeout_secs = 3").await;
    let browser = Browser::login(&server.url).await;

    // A paired device with a real repository holding one snapshot.
    let (_, location) = browser.post("/devices", &[("name", "PC"), ("kind", "nixos")]).await;
    let device_id = location.trim_start_matches("/devices/").to_string();
    let code = pairing_code(&browser, &device_id).await;
    let reply: PairResponse = reqwest::Client::new()
        .post(format!("{}{}", server.url, paths::PAIR))
        .json(&PairRequest {
            code,
            device_name: "PC".into(),
            kind: DeviceKind::Nixos,
            ssh_public_key: device_public_key(),
            client_version: None,
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(reply.device_id, device_id);
    let repo_dir = server.local_root().join("replique/sigma/pc/restic");
    restic(&repo_dir, PASSWORD, &["init"]);
    let data = server.dir.path().join("data");
    std::fs::create_dir_all(data.join("Photos/2019")).unwrap();
    std::fs::write(data.join("notes.txt"), "bonjour, réplique").unwrap();
    std::fs::write(data.join("été.txt"), "accents").unwrap();
    let photo: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
    std::fs::write(data.join("Photos/2019/IMG_2041.CR2"), &photo).unwrap();
    restic(&repo_dir, PASSWORD, &["backup", data.to_str().unwrap()]);
    let repo = server.app.store.repository_of(&device_id).unwrap().unwrap();
    let base = format!("/repos/{}", repo.id);

    // Locked: browsing asks for the password.
    let (status, location) = browser.get(&format!("{base}/snapshots")).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{location}");
    let (_, form) = browser.get(&format!("{base}/unlock")).await;
    assert!(form.contains("never written to disk or to logs"));

    // A wrong password is refused.
    let (status, page) = browser.post(&format!("{base}/unlock"), &[("password", "not-it")]).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Wrong password."), "{page}");

    // The right one unlocks.
    let (status, location) = browser.post(&format!("{base}/unlock"), &[("password", PASSWORD)]).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{location}");
    assert_eq!(location, format!("{base}/snapshots"));
    let (status, page) = browser.get(&format!("{base}/snapshots")).await;
    assert_eq!(status, StatusCode::OK);
    let snapshot = extract(&page, "browse?snapshot=", "&").expect("a snapshot link");
    assert_eq!(snapshot.len(), 64);

    // Browse into the backed-up folder.
    let dir = data.to_str().unwrap();
    let enc = |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
    let (status, page) = browser
        .get(&format!("{base}/browse?snapshot={snapshot}&path={}", enc(dir)))
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page.contains("notes.txt") && page.contains("Photos/") && page.contains("été.txt"), "{page}");

    // Download one file: the exact bytes, streamed.
    let file = format!("{dir}/Photos/2019/IMG_2041.CR2");
    let r = browser
        .http
        .get(format!("{}{base}/download?snapshot={snapshot}&path={}", server.url, enc(&file)))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert!(r.headers()["content-disposition"].to_str().unwrap().contains("IMG_2041.CR2"));
    assert_eq!(r.bytes().await.unwrap().to_vec(), photo);

    // Download a folder as a zip.
    let r = browser
        .http
        .get(format!("{}{base}/download?zip=1&snapshot={snapshot}&path={}", server.url, enc(dir)))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()["content-type"], "application/zip");
    let bytes = r.bytes().await.unwrap();
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes.to_vec())).unwrap();
    let name = (0..zip.len())
        .map(|i| zip.by_index(i).unwrap().name().to_string())
        .find(|n| n.ends_with("notes.txt"))
        .expect("notes.txt in the zip");
    let mut text = String::new();
    zip.by_name(&name).unwrap().read_to_string(&mut text).unwrap();
    assert_eq!(text, "bonjour, réplique");

    // A missing file is an error page, not a broken download.
    let (status, _) = browser
        .get(&format!("{base}/download?snapshot={snapshot}&path={}", enc(&format!("{dir}/nope"))))
        .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    // Arguments that could read as options are refused.
    let (status, _) = browser.get(&format!("{base}/download?snapshot=--help&path=/x")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Lock forgets the password.
    let (status, _) = browser.post(&format!("{base}/lock"), &[]).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, _) = browser.get(&format!("{base}/snapshots")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(server.app.unlocks.count(), 0);

    // So does staying idle past the timeout.
    browser.post(&format!("{base}/unlock"), &[("password", PASSWORD)]).await;
    assert_eq!(browser.get(&format!("{base}/snapshots")).await.0, StatusCode::OK);
    tokio::time::sleep(std::time::Duration::from_millis(3200)).await;
    assert_eq!(browser.get(&format!("{base}/snapshots")).await.0, StatusCode::SEE_OTHER);

    // And logging out.
    browser.post(&format!("{base}/unlock"), &[("password", PASSWORD)]).await;
    assert_eq!(server.app.unlocks.count(), 1);
    let (status, _) = browser.post("/logout", &[]).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(server.app.unlocks.count(), 0);
    let (status, _) = browser.get(&format!("{base}/snapshots")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    // Through all of it, the password never reached a log line.
    let logs = logs();
    assert!(logs.contains("repository unlocked"), "logs were captured");
    assert!(logs.contains("restore download"));
    assert!(!logs.contains(PASSWORD), "the password leaked into the logs");
}
