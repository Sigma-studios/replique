//! The Hetzner client against a mock built from the OpenAPI spec's examples.

mod common;

use std::time::Duration;

use common::{MOCK_BOX, MOCK_TOKEN, example, mock_hetzner};
use replique_server::hetzner::{AccessSettings, ApiError, Hetzner, NewSubaccount};
use replique_server::secrets::Secret;

async fn client() -> (std::sync::Arc<common::MockHetzner>, Hetzner) {
    let (mock, url) = mock_hetzner().await;
    let mut h = Hetzner::new(&url, Secret::new(MOCK_TOKEN.into())).unwrap();
    h.poll_every = Duration::from_millis(5);
    (mock, h)
}

#[test]
fn spec_examples_parse_into_our_types() {
    // The types read the spec's own examples, field for field.
    let b: serde_json::Value = example("get_storage_box")["response"]["storage_box"].clone();
    let b: replique_server::hetzner::StorageBox = serde_json::from_value(b).unwrap();
    assert_eq!(b.storage_box_type.subaccounts_limit, 200);
    assert_eq!(b.storage_box_type.size, 1_073_741_824);
    assert_eq!(b.server.as_deref(), Some("u1337.your-storagebox.de"));
    let stats = b.stats.unwrap();
    assert_eq!(
        (stats.size, stats.size_data, stats.size_snapshots),
        (0, 0, 0)
    );
    let s: replique_server::hetzner::Subaccount =
        serde_json::from_value(example("get_subaccount")["response"]["subaccount"].clone())
            .unwrap();
    assert_eq!(s.username, "u1337-sub1");
    assert!(!s.access_settings.readonly);
    let list = example("list_subaccounts")["response"]["subaccounts"].clone();
    let list: Vec<replique_server::hetzner::Subaccount> = serde_json::from_value(list).unwrap();
    assert_eq!(list[0].home_directory, "my_backups/host01.my.company");
    // Our create request carries every field of the spec's example request.
    let wanted = example("create_subaccount")["request"].clone();
    for key in [
        "home_directory",
        "password",
        "access_settings",
        "description",
        "labels",
    ] {
        assert!(
            wanted.get(key).is_some(),
            "{key} is not in the spec's request"
        );
    }
    let access: AccessSettings = serde_json::from_value(wanted["access_settings"].clone()).unwrap();
    assert_eq!(
        access,
        AccessSettings {
            reachable_externally: false,
            samba_enabled: false,
            ssh_enabled: false,
            webdav_enabled: false,
            readonly: false,
        }
    );
}

#[tokio::test]
async fn reads_the_box() {
    let (mock, h) = client().await;
    *mock.used_bytes.lock().unwrap() = 123_456;
    let b = h.storage_box(MOCK_BOX).await.unwrap();
    assert_eq!(b.id, MOCK_BOX);
    assert_eq!(b.stats.unwrap().size, 123_456);
    assert_eq!(b.username.as_deref(), Some("u12345"));
}

#[tokio::test]
async fn creates_waits_and_reads_back_a_subaccount() {
    let (mock, h) = client().await;
    let password = Secret::new("Abcdefgh1234-x".into());
    let sub = h
        .create_subaccount(
            MOCK_BOX,
            NewSubaccount {
                home_directory: "replique/sigma/pc",
                password: &password,
                access_settings: AccessSettings::ssh_only(true),
                description: "Réplique: PC (read-only)",
            },
        )
        .await
        .unwrap();
    assert_eq!(sub.home_directory, "replique/sigma/pc");
    assert!(sub.access_settings.readonly && sub.access_settings.ssh_enabled);
    assert!(sub.username.starts_with("u1337-sub"));
    let requests = mock.requests.lock().unwrap().clone();
    let (_, _, body) = requests.iter().find(|(m, _, _)| m == "POST").unwrap();
    assert_eq!(body["password"], "Abcdefgh1234-x");
    assert_eq!(body["access_settings"]["samba_enabled"], false);
    assert_eq!(body["labels"]["managed-by"], "replique");
    // The action was polled until it succeeded, then the sub-account read.
    let polls = requests
        .iter()
        .filter(|(_, p, _)| p.starts_with("/storage_boxes/actions/"))
        .count();
    assert!(polls >= 2, "action polled {polls} times");
    assert!(
        requests
            .last()
            .unwrap()
            .1
            .ends_with(&format!("/subaccounts/{}", sub.id))
    );
}

#[tokio::test]
async fn subaccount_actions_send_the_spec_fields() {
    let (mock, h) = client().await;
    let sub = h
        .create_subaccount(
            MOCK_BOX,
            NewSubaccount {
                home_directory: "a",
                password: &Secret::new("Abcdefgh1234-x".into()),
                access_settings: AccessSettings::ssh_only(false),
                description: "x",
            },
        )
        .await
        .unwrap();
    h.reset_subaccount_password(MOCK_BOX, sub.id, &Secret::new("Zyxwvuts9876-y".into()))
        .await
        .unwrap();
    h.update_access_settings(MOCK_BOX, sub.id, AccessSettings::ssh_only(true))
        .await
        .unwrap();
    h.change_home_directory(MOCK_BOX, sub.id, "replique/sigma/phone")
        .await
        .unwrap();
    let again = h.subaccount(MOCK_BOX, sub.id).await.unwrap();
    assert_eq!(again.home_directory, "replique/sigma/phone");
    assert!(again.access_settings.readonly);
    assert_eq!(h.subaccounts(MOCK_BOX).await.unwrap().len(), 1);
    h.delete_subaccount(MOCK_BOX, sub.id).await.unwrap();
    assert!(h.subaccounts(MOCK_BOX).await.unwrap().is_empty());
    let commands = mock.commands();
    assert!(
        commands
            .iter()
            .any(|c| c.ends_with("/actions/reset_subaccount_password"))
    );
    assert!(commands.iter().any(|c| c.starts_with("DELETE")));
}

#[tokio::test]
async fn failed_actions_and_errors_surface() {
    let (mock, h) = client().await;
    *mock.fail_command.lock().unwrap() = Some("create_subaccount".into());
    let err = h
        .create_subaccount(
            MOCK_BOX,
            NewSubaccount {
                home_directory: "a",
                password: &Secret::new("Abcdefgh1234-x".into()),
                access_settings: AccessSettings::ssh_only(false),
                description: "x",
            },
        )
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("action_failed"), "{err:#}");

    let (_, url) = mock_hetzner().await;
    let wrong = Hetzner::new(&url, Secret::new("wrong".into())).unwrap();
    let err = wrong.storage_box(MOCK_BOX).await.unwrap_err();
    let api = err.downcast_ref::<ApiError>().expect("an API error");
    assert_eq!(api.status.as_u16(), 401);
    assert_eq!(api.code, "unauthorized");
    // The token never appears in errors.
    assert!(!format!("{err:#}").contains("wrong"));
}
