use super::{on_ui, with_controller, Controller};
use crate::cores::core_for_platform;
use crate::paths;
use crate::play::CoreIdentity;
use crate::romm::client::{Client, Error};
use crate::saves::{self, GameSaves, Keep, SramConflict, SramOutcome};
use crate::store::{GameDetail, Store};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tr::tr;

pub(super) struct PendingLaunch {
    pub detail: GameDetail,
    pub rom: PathBuf,
    pub jit: bool,
    pub core: PathBuf,
    pub conflict: SramConflict,
}

pub(super) fn conflict_text(conflict: &SramConflict) -> String {
    let when = |t: &Option<String>| {
        t.as_deref()
            .and_then(crate::sync::parse_iso)
            .map(|ms| {
                let time =
                    std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms.max(0) as u64);
                crate::sync::iso_utc(time)
                    .replace('T', " ")
                    .replace("+00:00", " UTC")
            })
            .unwrap_or_else(|| tr!("unknown"))
    };
    tr!(
        "This computer: {0}. Server: {1}. The one you don't choose is kept as a backup.",
        when(&conflict.local_updated_at),
        when(&conflict.server_updated_at)
    )
}

pub(super) fn version_key(rom_id: i64) -> String {
    format!("core_version:{rom_id}")
}

pub(super) async fn sync_game(
    client: &Client,
    store: &Mutex<Store>,
    device_id: &str,
    game: &GameSaves,
    core_version: Option<&str>,
) -> Result<SramOutcome, Error> {
    let result = async {
        let last = store.lock().unwrap().in_game_save_record(game.rom_id);
        let sram = saves::sync_sram(client, device_id, game, last.as_ref()).await?;
        saves::remember_outcome(store, game, &sram);
        if !matches!(sram, SramOutcome::Conflict(_)) {
            if core_version.is_none() && crate::cores::saves_states(&game.emulator) {
                return Err(Error::Decode(
                    "the core version is needed to sync save states".into(),
                ));
            }
            if let Some(version) = core_version {
                saves::sync_states(client, store, game, version).await?;
            }
        }
        Ok(sram)
    }
    .await;
    let mut store = store.lock().unwrap();
    match &result {
        Ok(SramOutcome::Conflict(_)) | Err(_) => store.add_pending(game.rom_id),
        Ok(_) => store.remove_pending(game.rom_id),
    }
    result
}

impl Controller {
    pub(super) fn finish_save_sync(&self) {
        let stopped = self
            .running
            .borrow()
            .as_ref()
            .is_some_and(crate::play::RunningGame::stop_core);
        if stopped {
            self.running.borrow_mut().take();
            let detail = self.playing.borrow_mut().take();
            if let Some(detail) = detail {
                self.after_play(detail, None);
            }
        }
        let task = self.save_task.borrow_mut().take();
        if let Some(task) = task {
            // Keep the runtime alive for the current upload. If offline or too slow, the
            // durable queue remains and retries on the next launch.
            if self
                .shared
                .rt
                .block_on(async {
                    tokio::time::timeout(std::time::Duration::from_secs(10), task).await
                })
                .is_err()
            {
                tracing::warn!("save sync is still pending at shutdown; it will retry next launch");
            }
        }
    }

    pub(super) fn start_save_retry(&self) {
        let timer = slint::Timer::default();
        timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_secs(30),
            || with_controller(|c| c.sync_pending()),
        );
        *self.save_retry.borrow_mut() = Some(timer);
    }

    pub(super) fn sync_device(&self) -> Option<String> {
        let store = self.shared.store.lock().unwrap();
        let scopes = store.get("scopes")?;
        scopes
            .split(' ')
            .any(|s| s == "devices.write")
            .then(|| store.get("device_uuid"))
            .flatten()
    }

    pub(super) fn update_pairing_prompt(&self) {
        let signed_in = self.client.borrow().is_some();
        let prompt = if self.sync_device().is_none() {
            Some(tr!("Pair again to turn on save sync"))
        } else if !self.has_scope("collections.write") {
            Some(tr!("Pair again to use favorites and collections"))
        } else if !self.has_scope("roms.user.write") {
            Some(tr!("Pair again to share when you last played"))
        } else if !self.has_scope("roms.user.read") {
            Some(tr!("Pair again to see how long you've played"))
        } else if self.ra_account().is_some() && !self.has_scope("me.write") {
            Some(tr!(
                "Pair again to keep RetroAchievements progress current in RomM"
            ))
        } else {
            None
        };
        if let Some(ui) = self.ui() {
            ui.set_needs_pairing(signed_in && prompt.is_some());
            ui.set_pair_prompt(prompt.unwrap_or_default().into());
        }
    }

    pub(super) fn pair_again(&self) {
        let Some(server) = self.client.borrow().as_ref().map(|c| c.base().to_string()) else {
            return;
        };
        if let Some(ui) = self.ui() {
            ui.set_server_url(server.trim_end_matches('/').into());
        }
        self.connect();
    }

    pub(super) fn game_saves(&self, detail: &GameDetail) -> Option<GameSaves> {
        let core = core_for_platform(&detail.platform_slug)?;
        let dir = paths::game_save_dir(&paths::data_dir(), &self.server(), detail.id);
        if core.id == "melondsds" {
            if let Err(e) = crate::saves::adopt_desmume_saves(&dir) {
                tracing::warn!("moving {}'s DeSmuME save to melonDS: {e}", detail.title);
            }
        }
        let rom = detail.local_path.as_deref().map(std::path::Path::new);
        let (save_file, save_emulator) = crate::saves::in_game_save(core.id, &dir, rom);
        Some(GameSaves {
            rom_id: detail.id,
            dir,
            title: detail.title.clone(),
            emulator: core.id.to_string(),
            save_file,
            save_emulator,
        })
    }

    pub(super) fn clear_conflict(&self) {
        if self.pending_launch.borrow_mut().take().is_some() {
            self.preparing.set(false);
        }
        if let Some(ui) = self.ui() {
            ui.set_save_conflict(false);
        }
    }

    pub(super) fn show_conflict(&self, pending: PendingLaunch) {
        if let Some(ui) = self.ui() {
            ui.set_conflict_text(conflict_text(&pending.conflict).into());
            ui.set_save_conflict(true);
            ui.set_game_status("".into());
        }
        *self.pending_launch.borrow_mut() = Some(pending);
    }

    pub(super) fn keep_save(&self, keep: Keep) {
        let Some(pending) = self.pending_launch.borrow_mut().take() else {
            return;
        };
        let (Some(client), Some(device), Some(game)) = (
            self.client.borrow().clone(),
            self.sync_device(),
            self.game_saves(&pending.detail),
        ) else {
            self.preparing.set(false);
            if let Some(ui) = self.ui() {
                ui.set_save_conflict(false);
            }
            return;
        };
        if let Some(ui) = self.ui() {
            ui.set_save_conflict(false);
            ui.set_game_status(tr!("Syncing your save…").into());
        }
        let conflict = pending.conflict.clone();
        let store = self.shared.store.clone();
        self.shared.rt.spawn(async move {
            let result = async {
                let outcome = saves::resolve_sram(&client, &device, &game, &conflict, keep).await?;
                saves::remember_outcome(&store, &game, &outcome);
                let version = store.lock().unwrap().get(&version_key(game.rom_id));
                sync_game(&client, &store, &device, &game, version.as_deref()).await
            }
            .await;
            on_ui(move |c| {
                match result {
                    Err(e) => {
                        tracing::warn!("resolving save conflict: {e}");
                        c.preparing.set(false);
                        c.game_status(pending.detail.id, tr!("Couldn't sync saves: {e}.", e));
                        return;
                    }
                    Ok(SramOutcome::Conflict(conflict)) => {
                        return c.show_conflict(PendingLaunch {
                            conflict,
                            ..pending
                        });
                    }
                    Ok(_) => {}
                }
                c.start_game(pending.detail, pending.rom, pending.jit, pending.core);
            });
        });
    }

    pub(super) fn after_play(&self, detail: GameDetail, identity: Option<CoreIdentity>) {
        if let Some((_, version)) = &identity {
            self.shared
                .store
                .lock()
                .unwrap()
                .set(&version_key(detail.id), version);
        }
        let Some(game) = self.game_saves(&detail) else {
            return;
        };
        // Persist before starting network work: quitting the app mid-upload must not forget it.
        self.shared.store.lock().unwrap().add_pending(detail.id);
        let Some(device) = self.sync_device() else {
            self.shared.store.lock().unwrap().add_pending(detail.id);
            return self.game_status(
                detail.id,
                tr!(
                    "Save sync is off. Choose \"{}\" in the library.",
                    tr!("Pair again to turn on save sync")
                ),
            );
        };
        let client = self.client.borrow().clone();
        let Some(client) = client.filter(|_| !self.offline.get()) else {
            self.shared.store.lock().unwrap().add_pending(detail.id);
            return self.game_status(
                detail.id,
                tr!("Saves will sync when the server is reachable."),
            );
        };
        let store = self.shared.store.clone();
        let version = identity.map(|(_, v)| v).or_else(|| {
            self.shared
                .store
                .lock()
                .unwrap()
                .get(&version_key(detail.id))
        });
        let id = detail.id;
        self.syncing_game.set(Some(id));
        self.game_status(id, tr!("Syncing your saves…"));
        let task = self.shared.rt.spawn(async move {
            let result = sync_game(&client, &store, &device, &game, version.as_deref()).await;
            on_ui(move |c| c.after_play_synced(id, result));
        });
        *self.save_task.borrow_mut() = Some(task);
    }

    fn after_play_synced(&self, id: i64, result: Result<SramOutcome, Error>) {
        self.syncing_game.set(None);
        let status = match result {
            Ok(SramOutcome::Conflict(_)) => {
                tr!("The save here and the one on RomM differ. You'll be asked which to keep next time you play.")
            }
            Ok(_) => tr!("In-game save and save states synced."),
            Err(Error::Unreachable) => {
                self.shared.store.lock().unwrap().add_pending(id);
                tr!("Saves will sync when the server is reachable.")
            }
            Err(e) => {
                tracing::warn!("syncing saves for {id}: {e}");
                tr!("Couldn't sync saves: {e}. Will retry automatically.", e)
            }
        };
        self.game_status(id, status);
        if let Some(detail) = self.current_game().filter(|d| d.id == id) {
            self.show_save_slots(&detail);
        }
    }

    pub(super) fn sync_pending(&self) {
        self.sync_idle_saves(false);
    }

    /// Refresh the selected game's saves as well as uploads left over from earlier sessions.
    pub(super) fn refresh_game_saves(&self) {
        self.sync_idle_saves(true);
    }

    fn sync_idle_saves(&self, include_current: bool) {
        let busy = self.running.borrow().is_some()
            || self.preparing.get()
            || self.syncing_game.get().is_some();
        let (Some(client), Some(device)) = (self.client.borrow().clone(), self.sync_device())
        else {
            return;
        };
        if busy || self.offline.get() {
            return;
        }
        let jobs: Vec<(GameSaves, Option<String>, Option<PathBuf>)> = {
            let store = self.shared.store.lock().unwrap();
            let mut ids = store.pending();
            if let Some(id) = include_current.then(|| self.current_game_id()).flatten() {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
            ids.into_iter()
                .filter_map(|id| {
                    let detail = store.game(id)?;
                    Some((detail, store.get(&version_key(id))))
                })
                .collect::<Vec<_>>()
        }
        .into_iter()
        .filter_map(|(detail, version)| {
            let core = core_for_platform(&detail.platform_slug)?;
            let installed = self.shared.cores.installed(core);
            // Nothing can resume yet until a core is installed; the launch path will check it.
            if version.is_none() && installed.is_none() && include_current {
                return None;
            }
            Some((self.game_saves(&detail)?, version, installed))
        })
        .collect();
        if jobs.is_empty() {
            return;
        }
        // One save operation at a time. In particular, never replace files under a running core.
        self.syncing_game.set(Some(jobs[0].0.rom_id));
        let store: Arc<Mutex<Store>> = self.shared.store.clone();
        let task = self.shared.rt.spawn(async move {
            let mut results = Vec::new();
            for (game, mut version, installed) in jobs {
                if version.is_none() && crate::cores::saves_states(&game.emulator) {
                    if let Some(core) = installed {
                        match crate::session::core_version(&core).await {
                            Ok(found) => {
                                store.lock().unwrap().set(&version_key(game.rom_id), &found);
                                version = Some(found);
                            }
                            Err(e) => tracing::warn!("reading the core version: {e:#}"),
                        }
                    }
                }
                let result = sync_game(&client, &store, &device, &game, version.as_deref()).await;
                results.push((game.rom_id, result));
            }
            on_ui(move |c| {
                for (id, result) in results {
                    c.after_play_synced(id, result);
                }
            });
        });
        *self.save_task.borrow_mut() = Some(task);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::romm::client::tests::base_of;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn game(dir: &std::path::Path) -> GameSaves {
        GameSaves {
            rom_id: 5027,
            dir: dir.to_owned(),
            title: "Super Mario World".into(),
            emulator: "snes9x".into(),
            save_file: saves::SRAM_FILE.into(),
            save_emulator: "snes9x".into(),
        }
    }

    async fn no_sram(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/api/saves"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn prelaunch_sync_waits_for_the_phone_state_even_when_no_battery_save_exists() {
        let server = MockServer::start().await;
        no_sram(&server).await;
        Mock::given(method("GET"))
            .and(path("/api/states"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
                "id": 31, "rom_id": 5027, "emulator": "snes9x",
                "file_name": "Super Mario World [2026-11-06_00-14-31].autosave.state.auto",
                "updated_at": "2026-11-06T00:21:21Z"
            }])))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/states/31/content"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"phone progress"))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let store = Mutex::new(Store::open_in_memory().unwrap());
        store.lock().unwrap().add_pending(5027);
        let client = Client::new(base_of(&server, "/")).with_token("test".into());
        sync_game(&client, &store, "device", &game(dir.path()), Some("1.63"))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("auto.state")).unwrap(),
            b"phone progress"
        );
        assert_eq!(
            store
                .lock()
                .unwrap()
                .state_record(5027, "auto")
                .unwrap()
                .remote_id,
            Some(31)
        );
        assert!(store.lock().unwrap().pending().is_empty());
    }

    #[tokio::test]
    async fn failed_state_sync_survives_restart_and_only_success_clears_the_queue() {
        let server = MockServer::start().await;
        no_sram(&server).await;
        Mock::given(method("GET"))
            .and(path("/api/states"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("romp.db");
        let store = Mutex::new(Store::open(&db).unwrap());
        let client = Client::new(base_of(&server, "/")).with_token("test".into());
        assert!(matches!(
            sync_game(&client, &store, "device", &game(dir.path()), Some("1.63")).await,
            Err(Error::Status(503))
        ));
        drop(store);
        let store = Mutex::new(Store::open(&db).unwrap());
        assert_eq!(store.lock().unwrap().pending(), [5027]);
        server.reset().await;
        no_sram(&server).await;
        Mock::given(method("GET"))
            .and(path("/api/states"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .expect(1)
            .mount(&server)
            .await;
        sync_game(&client, &store, "device", &game(dir.path()), Some("1.63"))
            .await
            .unwrap();
        assert!(store.lock().unwrap().pending().is_empty());
    }

    #[tokio::test]
    async fn battery_conflict_stays_pending_and_does_not_replace_states() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/saves"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
                "id": 16, "rom_id": 5027, "emulator": "snes9x", "slot": "autosave",
                "file_name": "Super Mario World.srm", "updated_at": "2026-11-06T09:00:00Z",
                "content_hash": "different"
            }])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/states"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .expect(0)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(saves::SRAM_FILE), b"local save").unwrap();
        std::fs::write(dir.path().join("auto.state"), b"local state").unwrap();
        let store = Mutex::new(Store::open_in_memory().unwrap());
        let client = Client::new(base_of(&server, "/")).with_token("test".into());
        assert!(matches!(
            sync_game(&client, &store, "device", &game(dir.path()), Some("1.63"))
                .await
                .unwrap(),
            SramOutcome::Conflict(_)
        ));
        assert_eq!(store.lock().unwrap().pending(), [5027]);
        assert_eq!(
            std::fs::read(dir.path().join("auto.state")).unwrap(),
            b"local state"
        );
    }

    #[tokio::test]
    async fn missing_core_metadata_does_not_discard_a_pending_state_upload() {
        let server = MockServer::start().await;
        no_sram(&server).await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("auto.state"), b"unsent progress").unwrap();
        let store = Mutex::new(Store::open_in_memory().unwrap());
        store.lock().unwrap().add_pending(5027);
        let client = Client::new(base_of(&server, "/")).with_token("test".into());
        assert!(
            sync_game(&client, &store, "device", &game(dir.path()), None)
                .await
                .is_err()
        );
        assert_eq!(store.lock().unwrap().pending(), [5027]);
        assert_eq!(
            std::fs::read(dir.path().join("auto.state")).unwrap(),
            b"unsent progress"
        );
    }

    #[test]
    fn conflict_text_names_both_times() {
        let text = conflict_text(&SramConflict {
            save_id: Some(1),
            server_updated_at: Some("2026-09-26T10:00:00+00:00".into()),
            local_updated_at: None,
        });
        assert!(text.contains("Server: 2026-09-26 10:00:00 UTC"));
        assert!(text.contains("This computer: unknown"));
        let text = conflict_text(&SramConflict {
            save_id: Some(1),
            server_updated_at: Some("2026-09-26T12:00:00.5+02:00".into()),
            local_updated_at: None,
        });
        assert!(text.contains("Server: 2026-09-26 10:00:00 UTC"));
    }
}
