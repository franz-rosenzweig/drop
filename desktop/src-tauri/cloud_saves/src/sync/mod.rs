//! Cloud save sync: pull the server's copy before a game starts, push this
//! machine's copy after it quits.
//!
//! Each game uses one save slot (the lowest-numbered one). The server keeps a
//! short history of uploads per slot. Locally, a small state file remembers
//! what was last synced, so we can tell "the other machine played" from "this
//! machine played" from "both did". When both did, the newer saves win and the
//! older ones are kept: in the server's history, or in a local backup folder.

pub mod archive;
pub mod locate;
#[cfg(test)]
mod tests;

use std::{
    fmt::Display,
    fs, io,
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use database::platform::Platform;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use archive::Archive;
use locate::{Host, Resolved, SaveFile};

/// Local backups kept per game, from before a download replaced saves.
const BACKUPS_KEPT: usize = 5;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveLocation {
    pub path: String,
    pub platform: Option<Platform>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveSlot {
    pub index: i64,
    #[serde(default)]
    pub history_object_ids: Vec<String>,
    #[serde(default)]
    pub history_checksums: Vec<String>,
}

impl SaveSlot {
    /// The newest upload: (object id, sha256).
    pub fn latest(&self) -> Option<(&str, &str)> {
        Some((
            self.history_object_ids.last()?.as_str(),
            self.history_checksums.last()?.as_str(),
        ))
    }
}

/// The server's save API.
#[async_trait]
pub trait SaveServer: Send + Sync {
    async fn locations(&self, game_id: &str) -> Result<Vec<SaveLocation>, SyncError>;
    async fn slots(&self, game_id: &str) -> Result<Vec<SaveSlot>, SyncError>;
    async fn create_slot(&self, game_id: &str) -> Result<SaveSlot, SyncError>;
    async fn push(&self, game_id: &str, index: i64, archive: Vec<u8>) -> Result<(), SyncError>;
    async fn fetch(&self, object_id: &str) -> Result<Vec<u8>, SyncError>;
}

/// One installed game, as the sync needs to know it.
#[derive(Clone, Debug)]
pub struct SyncContext {
    pub game_id: String,
    pub install_dir: PathBuf,
    pub target_platform: Platform,
    /// Where sync state, the archive cache and backups live.
    pub data_dir: PathBuf,
}

#[derive(Debug)]
pub enum SyncError {
    Io(io::Error),
    Server(String),
    ChecksumMismatch,
    BadArchive(String),
    TooLarge,
    BadGameId,
}

impl Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncError::Io(e) => write!(f, "file error: {e}"),
            SyncError::Server(e) => write!(f, "server error: {e}"),
            SyncError::ChecksumMismatch => write!(f, "downloaded save doesn't match its checksum"),
            SyncError::BadArchive(e) => write!(f, "unreadable save archive: {e}"),
            SyncError::TooLarge => write!(f, "saves are too large to sync"),
            SyncError::BadGameId => write!(f, "unexpected game id"),
        }
    }
}

impl std::error::Error for SyncError {}

impl From<io::Error> for SyncError {
    fn from(e: io::Error) -> Self {
        SyncError::Io(e)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum PullOutcome {
    NoLocations,
    NothingOnServer,
    UpToDate,
    /// Both sides changed and this machine's saves are newer; they go up when
    /// the game quits.
    KeptLocal,
    Restored(usize),
}

#[derive(Debug, PartialEq, Eq)]
pub enum PushOutcome {
    NoLocations,
    NoFiles,
    Unchanged,
    Uploaded(usize),
}

/// What was last synced for a game.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct SyncState {
    /// sha256 of the server's newest upload at the last sync.
    checksum: Option<String>,
    /// Fingerprint of this machine's save files right after the last sync.
    fingerprint: Option<String>,
}

/// Brings this machine's saves up to date before the game starts.
pub async fn pull(
    server: &dyn SaveServer,
    ctx: &SyncContext,
    host: &Host,
) -> Result<PullOutcome, SyncError> {
    check_game_id(&ctx.game_id)?;
    let resolved = resolve(server, ctx, host).await?;
    if resolved.is_empty() {
        return Ok(PullOutcome::NoLocations);
    }
    let slots = server.slots(&ctx.game_id).await?;
    let Some((object_id, checksum)) = pick_slot(&slots).and_then(SaveSlot::latest) else {
        return Ok(PullOutcome::NothingOnServer);
    };
    let mut state = load_state(ctx);
    if state.checksum.as_deref() == Some(checksum) {
        return Ok(PullOutcome::UpToDate);
    }

    let bytes = server.fetch(object_id).await?;
    if sha256_hex(&bytes) != checksum {
        return Err(SyncError::ChecksumMismatch);
    }
    let archive = Archive::read(&bytes)?;
    write_cache(ctx, &bytes)?;

    let local = locate::collect_all(&resolved, host.case_sensitive())?;
    let local_fingerprint = fingerprint(&local)?;
    let local_changed = local_fingerprint.is_some() && local_fingerprint != state.fingerprint;
    if local_changed && newest(&local) > archive.manifest.newest {
        return Ok(PullOutcome::KeptLocal);
    }

    if local_fingerprint.is_some() {
        backup(ctx, &local)?;
    }
    let restored = archive::apply(&archive, &resolved, host.case_sensitive())?;
    let after = locate::collect_all(&resolved, host.case_sensitive())?;
    state.checksum = Some(checksum.to_owned());
    state.fingerprint = fingerprint(&after)?;
    save_state(ctx, &state)?;
    Ok(PullOutcome::Restored(restored))
}

/// Uploads this machine's saves after the game quits, if they changed.
pub async fn push(
    server: &dyn SaveServer,
    ctx: &SyncContext,
    host: &Host,
) -> Result<PushOutcome, SyncError> {
    check_game_id(&ctx.game_id)?;
    let resolved = resolve(server, ctx, host).await?;
    if resolved.is_empty() {
        return Ok(PushOutcome::NoLocations);
    }
    let local = locate::collect_all(&resolved, host.case_sensitive())?;
    let Some(local_fingerprint) = fingerprint(&local)? else {
        return Ok(PushOutcome::NoFiles);
    };
    let mut state = load_state(ctx);
    if state.fingerprint.as_deref() == Some(local_fingerprint.as_str()) {
        return Ok(PushOutcome::Unchanged);
    }

    let slots = server.slots(&ctx.game_id).await?;
    let slot = match pick_slot(&slots) {
        Some(slot) => slot.clone(),
        None => server.create_slot(&ctx.game_id).await?,
    };
    let carry = match slot.latest() {
        Some((object_id, checksum)) => Some(Archive::read(
            &latest_bytes(server, ctx, object_id, checksum).await?,
        )?),
        None => None,
    };

    let bytes = archive::build(&local, carry.as_ref())?;
    let size = bytes.len();
    let checksum = sha256_hex(&bytes);
    server.push(&ctx.game_id, slot.index, bytes.clone()).await?;
    write_cache(ctx, &bytes)?;
    state.checksum = Some(checksum);
    state.fingerprint = Some(local_fingerprint);
    save_state(ctx, &state)?;
    Ok(PushOutcome::Uploaded(size))
}

async fn resolve(
    server: &dyn SaveServer,
    ctx: &SyncContext,
    host: &Host,
) -> Result<Vec<Resolved>, SyncError> {
    let locations = server.locations(&ctx.game_id).await?;
    Ok(locate::resolve_all(
        &locations,
        ctx.target_platform,
        &ctx.install_dir,
        host,
    ))
}

fn pick_slot(slots: &[SaveSlot]) -> Option<&SaveSlot> {
    slots.iter().min_by_key(|s| s.index)
}

/// The server's newest archive, from the local cache when it's the same one.
async fn latest_bytes(
    server: &dyn SaveServer,
    ctx: &SyncContext,
    object_id: &str,
    checksum: &str,
) -> Result<Vec<u8>, SyncError> {
    if let Ok(cached) = fs::read(cache_path(ctx))
        && sha256_hex(&cached) == checksum
    {
        return Ok(cached);
    }
    let bytes = server.fetch(object_id).await?;
    if sha256_hex(&bytes) != checksum {
        return Err(SyncError::ChecksumMismatch);
    }
    Ok(bytes)
}

/// A hash of every save file's location, path and contents. None when there
/// are no save files at all.
fn fingerprint(local: &[(Resolved, Vec<SaveFile>)]) -> Result<Option<String>, SyncError> {
    let mut sets: Vec<&(Resolved, Vec<SaveFile>)> = local
        .iter()
        .filter(|(_, files)| !files.is_empty())
        .collect();
    if sets.is_empty() {
        return Ok(None);
    }
    sets.sort_by(|a, b| a.0.template.cmp(&b.0.template));
    let mut hasher = Sha256::new();
    for (resolved, files) in sets {
        for file in files {
            let data = fs::read(&file.path)?;
            hasher.update(resolved.template.as_bytes());
            hasher.update([0]);
            hasher.update(file.rel.as_bytes());
            hasher.update([0]);
            hasher.update((data.len() as u64).to_le_bytes());
            hasher.update(&data);
        }
    }
    Ok(Some(hex::encode(hasher.finalize())))
}

fn newest(local: &[(Resolved, Vec<SaveFile>)]) -> u64 {
    local
        .iter()
        .flat_map(|(_, files)| files.iter().map(|f| f.mtime))
        .max()
        .unwrap_or(0)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Game ids are server UUIDs; they become file names here.
fn check_game_id(game_id: &str) -> Result<(), SyncError> {
    if game_id.is_empty()
        || !game_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(SyncError::BadGameId);
    }
    Ok(())
}

fn state_path(ctx: &SyncContext) -> PathBuf {
    ctx.data_dir
        .join("state")
        .join(format!("{}.json", ctx.game_id))
}

fn cache_path(ctx: &SyncContext) -> PathBuf {
    ctx.data_dir
        .join("cache")
        .join(format!("{}.tar.zst", ctx.game_id))
}

fn load_state(ctx: &SyncContext) -> SyncState {
    fs::read(state_path(ctx))
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default()
}

fn save_state(ctx: &SyncContext, state: &SyncState) -> Result<(), SyncError> {
    let data = serde_json::to_vec_pretty(state).expect("sync state serialises");
    write_file(&state_path(ctx), &data)
}

fn write_cache(ctx: &SyncContext, bytes: &[u8]) -> Result<(), SyncError> {
    write_file(&cache_path(ctx), bytes)
}

fn write_file(path: &Path, data: &[u8]) -> Result<(), SyncError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, data)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Keeps this machine's saves before a download replaces them.
fn backup(ctx: &SyncContext, local: &[(Resolved, Vec<SaveFile>)]) -> Result<(), SyncError> {
    let dir = ctx.data_dir.join("backups").join(&ctx.game_id);
    fs::create_dir_all(&dir)?;
    let bytes = archive::build(local, None)?;
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    write_file(&dir.join(format!("{millis}.tar.zst")), &bytes)?;

    let mut kept: Vec<PathBuf> = fs::read_dir(&dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "zst"))
        .collect();
    kept.sort();
    let excess = kept.len().saturating_sub(BACKUPS_KEPT);
    for old in &kept[..excess] {
        fs::remove_file(old)?;
    }
    Ok(())
}
