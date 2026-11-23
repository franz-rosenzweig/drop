//! The save archive: a zstd-compressed tar holding `drop-saves.json` and one
//! numbered folder per save location.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    io::{Cursor, Read},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use log::warn;
use serde::{Deserialize, Serialize};

use super::{
    SyncError,
    locate::{Resolved, SaveFile, safe_rel},
};

const MANIFEST: &str = "drop-saves.json";
const MAX_UNPACKED_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub version: u32,
    pub created: u64,
    /// The newest file time in the archive, for picking a winner when two
    /// machines both changed their saves.
    pub newest: u64,
    pub locations: Vec<ManifestLocation>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ManifestLocation {
    /// The path template, exactly as the server lists it.
    pub path: String,
    /// The folder in the archive holding this location's files.
    pub dir: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredFile {
    pub rel: String,
    pub mtime: u64,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct Archive {
    pub manifest: Manifest,
    /// Files by location template.
    pub files: BTreeMap<String, Vec<StoredFile>>,
}

impl Archive {
    pub fn read(bytes: &[u8]) -> Result<Self, SyncError> {
        let decoder = zstd::Decoder::new(Cursor::new(bytes))?;
        let mut tar = tar::Archive::new(decoder);
        let mut manifest: Option<Manifest> = None;
        let mut raw = Vec::new();
        let mut total = 0u64;
        for entry in tar.entries()? {
            let mut entry = entry?;
            if !entry.header().entry_type().is_file() {
                continue;
            }
            let name = String::from_utf8(entry.path_bytes().into_owned())
                .map_err(|_| SyncError::BadArchive("entry name isn't UTF-8".into()))?;
            let mtime = entry.header().mtime().unwrap_or(0);
            let mut data = Vec::new();
            entry.read_to_end(&mut data)?;
            total += data.len() as u64;
            if total > MAX_UNPACKED_BYTES {
                return Err(SyncError::TooLarge);
            }
            if name == MANIFEST {
                manifest = Some(
                    serde_json::from_slice(&data)
                        .map_err(|e| SyncError::BadArchive(e.to_string()))?,
                );
            } else {
                raw.push((name, mtime, data));
            }
        }
        let manifest = manifest.ok_or_else(|| SyncError::BadArchive("no manifest".into()))?;

        let templates: HashMap<&str, &str> = manifest
            .locations
            .iter()
            .map(|l| (l.dir.as_str(), l.path.as_str()))
            .collect();
        let mut files: BTreeMap<String, Vec<StoredFile>> = BTreeMap::new();
        for (name, mtime, data) in raw {
            let Some((dir, rel)) = name.split_once('/') else {
                continue;
            };
            let Some(template) = templates.get(dir) else {
                continue;
            };
            files
                .entry((*template).to_owned())
                .or_default()
                .push(StoredFile {
                    rel: rel.to_owned(),
                    mtime,
                    data,
                });
        }
        Ok(Archive { manifest, files })
    }
}

/// Packs this machine's saves. Locations this machine doesn't use (another
/// platform's folders, say) are carried over from `carry`, the server's
/// latest archive, so one machine's upload never drops another's saves.
pub fn build(
    local: &[(Resolved, Vec<SaveFile>)],
    carry: Option<&Archive>,
) -> Result<Vec<u8>, SyncError> {
    let mut locations = Vec::new();
    let mut entries: Vec<(String, u64, Vec<u8>)> = Vec::new();
    let mut newest = 0;

    for (resolved, files) in local.iter().filter(|(_, files)| !files.is_empty()) {
        let dir = locations.len().to_string();
        for file in files {
            entries.push((
                format!("{dir}/{}", file.rel),
                file.mtime,
                fs::read(&file.path)?,
            ));
            newest = newest.max(file.mtime);
        }
        locations.push(ManifestLocation {
            path: resolved.template.clone(),
            dir,
        });
    }

    if let Some(carry) = carry {
        let here: HashSet<&str> = local.iter().map(|(r, _)| r.template.as_str()).collect();
        for (template, files) in &carry.files {
            if here.contains(template.as_str()) || files.is_empty() {
                continue;
            }
            let dir = locations.len().to_string();
            for file in files {
                entries.push((format!("{dir}/{}", file.rel), file.mtime, file.data.clone()));
                newest = newest.max(file.mtime);
            }
            locations.push(ManifestLocation {
                path: template.clone(),
                dir,
            });
        }
    }

    let manifest = Manifest {
        version: 1,
        created: now_secs(),
        newest,
        locations,
    };
    let manifest_json =
        serde_json::to_vec_pretty(&manifest).map_err(|e| SyncError::BadArchive(e.to_string()))?;

    let mut tar = tar::Builder::new(zstd::Encoder::new(Vec::new(), 3)?);
    append(&mut tar, MANIFEST, manifest.created, &manifest_json)?;
    for (name, mtime, data) in &entries {
        append(&mut tar, name, *mtime, data)?;
    }
    Ok(tar.into_inner()?.finish()?)
}

fn append<W: std::io::Write>(
    tar: &mut tar::Builder<W>,
    name: &str,
    mtime: u64,
    data: &[u8],
) -> Result<(), SyncError> {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(mtime);
    tar.append_data(&mut header, name, data)?;
    Ok(())
}

/// Writes the archive's files for the locations this machine uses. Existing
/// files are replaced; files the archive doesn't mention are left alone.
pub fn apply(
    archive: &Archive,
    resolved: &[Resolved],
    case_sensitive: bool,
) -> Result<usize, SyncError> {
    let mut written = 0;
    for location in resolved {
        let Some(files) = archive.files.get(&location.template) else {
            continue;
        };
        for file in files {
            let Some(rel) = safe_rel(&file.rel, &location.pattern, case_sensitive) else {
                warn!("skipping save file outside its location: {}", file.rel);
                continue;
            };
            let target = location.base.join(rel);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            // Write beside the file, then swap it in, so a crash never leaves
            // half a save behind.
            let mut tmp_name = target.file_name().unwrap_or_default().to_os_string();
            tmp_name.push(".drop-sync");
            let tmp = target.with_file_name(tmp_name);
            fs::write(&tmp, &file.data)?;
            fs::File::options()
                .write(true)
                .open(&tmp)?
                .set_modified(UNIX_EPOCH + Duration::from_secs(file.mtime))?;
            fs::rename(&tmp, &target)?;
            written += 1;
        }
    }
    Ok(written)
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
