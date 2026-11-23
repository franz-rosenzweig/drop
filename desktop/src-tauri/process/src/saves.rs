//! Cloud saves around a game's run: the server's copy comes down before the
//! game starts, this machine's copy goes up after it quits. Failures are
//! logged and never stop a game from launching.

use std::{path::PathBuf, time::Duration};

use cloud_saves::sync::{
    self, PullOutcome, PushOutcome, SaveLocation, SaveServer, SaveSlot, SyncContext, SyncError,
    locate::Host,
};
use database::{
    Database, GameDownloadStatus, borrow_db_checked, db::DATA_ROOT_DIR,
    models::data::InstalledGameType,
};
use log::{info, warn};
use remote::{
    auth::generate_authorization_header, requests::generate_url, utils::DROP_CLIENT_ASYNC,
};
use serde::de::DeserializeOwned;

const TIMEOUT: Duration = Duration::from_secs(30);

/// The save API on the Drop server this client is signed in to.
pub struct DropSaveServer;

fn server_error(e: impl std::fmt::Display) -> SyncError {
    SyncError::Server(e.to_string())
}

async fn check(response: reqwest::Response) -> Result<reqwest::Response, SyncError> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    Err(SyncError::Server(format!("{status}: {body}")))
}

async fn get(path: &[&str]) -> Result<reqwest::Response, SyncError> {
    let url = generate_url(path, &[]).map_err(server_error)?;
    let response = DROP_CLIENT_ASYNC
        .get(url)
        .header("Authorization", generate_authorization_header())
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(server_error)?;
    check(response).await
}

async fn get_json<T: DeserializeOwned>(path: &[&str]) -> Result<T, SyncError> {
    let bytes = get(path).await?.bytes().await.map_err(server_error)?;
    serde_json::from_slice(&bytes).map_err(server_error)
}

#[async_trait::async_trait]
impl SaveServer for DropSaveServer {
    async fn locations(&self, game_id: &str) -> Result<Vec<SaveLocation>, SyncError> {
        get_json(&["/api/v1/client/saves", game_id, "locations"]).await
    }

    async fn slots(&self, game_id: &str) -> Result<Vec<SaveSlot>, SyncError> {
        get_json(&["/api/v1/client/saves", game_id]).await
    }

    async fn create_slot(&self, game_id: &str) -> Result<SaveSlot, SyncError> {
        let url = generate_url(&["/api/v1/client/saves", game_id], &[]).map_err(server_error)?;
        let response = DROP_CLIENT_ASYNC
            .post(url)
            .header("Authorization", generate_authorization_header())
            .timeout(TIMEOUT)
            .send()
            .await
            .map_err(server_error)?;
        let bytes = check(response).await?.bytes().await.map_err(server_error)?;
        serde_json::from_slice(&bytes).map_err(server_error)
    }

    async fn push(&self, game_id: &str, index: i64, archive: Vec<u8>) -> Result<(), SyncError> {
        let index = index.to_string();
        let url = generate_url(&["/api/v1/client/saves", game_id, &index, "push"], &[])
            .map_err(server_error)?;
        let response = DROP_CLIENT_ASYNC
            .post(url)
            .header("Authorization", generate_authorization_header())
            .header("Content-Type", "application/octet-stream")
            .body(archive)
            .timeout(TIMEOUT)
            .send()
            .await
            .map_err(server_error)?;
        check(response).await?;
        Ok(())
    }

    async fn fetch(&self, object_id: &str) -> Result<Vec<u8>, SyncError> {
        let bytes = get(&["/api/v1/client/object", object_id])
            .await?
            .bytes()
            .await
            .map_err(server_error)?;
        Ok(bytes.to_vec())
    }
}

/// What sync needs to know about an installed game, if it's ready to play.
pub fn sync_context(db: &Database, game_id: &str) -> Option<SyncContext> {
    let meta = db.applications.installed_game_version.get(game_id)?;
    let GameDownloadStatus::Installed {
        install_dir,
        install_type: InstalledGameType::Installed,
        ..
    } = db.applications.game_statuses.get(game_id)?
    else {
        return None;
    };
    Some(SyncContext {
        game_id: game_id.to_owned(),
        install_dir: PathBuf::from(install_dir),
        target_platform: meta.target_platform,
        data_dir: DATA_ROOT_DIR.join("cloud_saves"),
    })
}

/// Brings down the server's saves before launch. Never fails the launch.
pub async fn pull_before_launch(game_id: &str) {
    let Some(ctx) = sync_context(&borrow_db_checked(), game_id) else {
        return;
    };
    match sync::pull(&DropSaveServer, &ctx, &Host::current()).await {
        Ok(PullOutcome::Restored(files)) => {
            info!("cloud saves: restored {files} file(s) for {game_id}")
        }
        Ok(PullOutcome::KeptLocal) => {
            info!("cloud saves: kept this machine's newer saves for {game_id}")
        }
        Ok(outcome) => info!("cloud saves: {outcome:?} for {game_id}"),
        Err(e) => warn!("cloud saves: couldn't pull saves for {game_id}: {e}"),
    }
}

/// Sends this machine's saves up after the game quits, in the background.
pub fn push_after_exit(ctx: SyncContext) {
    tauri::async_runtime::spawn(async move {
        let game_id = ctx.game_id.clone();
        match sync::push(&DropSaveServer, &ctx, &Host::current()).await {
            Ok(PushOutcome::Uploaded(bytes)) => {
                info!("cloud saves: uploaded {bytes} bytes for {game_id}")
            }
            Ok(outcome) => info!("cloud saves: {outcome:?} for {game_id}"),
            Err(e) => warn!("cloud saves: couldn't push saves for {game_id}: {e}"),
        }
    });
}
