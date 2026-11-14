# Protocol fixtures

One example of each message in `docs/protocol.md`. The Rust tests
(`crates/replique-proto/tests/fixtures.rs`) and the Android app's unit tests
parse every file here, so both sides agree on the same bytes. Add a fixture with
every new message or field.

| File | Type (`replique-proto`) | Endpoint |
| --- | --- | --- |
| `pair_request.json` | `PairRequest` | `POST /pair` body |
| `pair_response.json` | `PairResponse` | `POST /pair` reply |
| `pairing_link.txt` | `PairingLink` | the QR code's content |
| `job_spec_backup.json`, `job_spec_manual.json`, `job_spec_mirror.json` | `JobSpec` | `PUT /jobs/{name}` body |
| `ack.json` | `Ack` | `PUT /jobs/{name}` reply |
| `device_config.json` | `DeviceConfig` | `GET /device/config` reply |
| `mirror_sources.json` | `MirrorSources` | `GET /mirror-sources` reply |
| `run_start.json` | `StartRun` | `POST /runs` body |
| `run_start_reply.json` | `StartRunReply` | `POST /runs` reply |
| `run_progress.json`, `run_progress_phase.json` | `Progress` | `POST /runs/{id}/progress` body |
| `run_finish_*.json` | `FinishRun` | `POST /runs/{id}/finish` body |
| `safe_to_delete.json`, `safe_to_delete_none.json` | `SafeToDelete` | `GET /devices/{id}/safe-to-delete` reply |
| `error.json` | `ApiError` | any non-2xx reply |
