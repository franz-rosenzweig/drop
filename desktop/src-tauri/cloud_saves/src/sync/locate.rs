//! Turning a save path template ("<base>/www/save", "<winAppData>/Studio/*.sav")
//! into real folders on this machine, and finding the files under them.

use std::{
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use database::platform::Platform;
use regex::{Regex, RegexBuilder};

use super::{SaveLocation, SyncError};
use crate::placeholder::*;

/// Saves bigger than this are almost certainly a bad path (a whole home
/// folder, a game install), so collecting stops there.
const MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;

/// What the placeholders in a save path stand for on this machine.
#[derive(Clone, Debug)]
pub struct Host {
    pub platform: Platform,
    pub folders: HashMap<&'static str, PathBuf>,
    pub user_name: String,
}

impl Host {
    pub fn current() -> Self {
        let mut folders = HashMap::new();
        let mut put = |key: &'static str, path: Option<PathBuf>| {
            if let Some(path) = path {
                folders.insert(key, path);
            }
        };
        put(HOME, dirs::home_dir());

        #[cfg(target_os = "windows")]
        {
            put(WIN_APP_DATA, dirs::config_dir());
            put(WIN_LOCAL_APP_DATA, dirs::data_local_dir());
            put(
                WIN_LOCAL_APP_DATA_LOW,
                dirs::home_dir().map(|h| h.join("AppData").join("LocalLow")),
            );
            put(WIN_DOCUMENTS, dirs::document_dir());
            put(WIN_PUBLIC, dirs::public_dir());
            put(
                WIN_PROGRAM_DATA,
                Some(
                    std::env::var_os("PROGRAMDATA")
                        .map_or_else(|| PathBuf::from("C:/ProgramData"), PathBuf::from),
                ),
            );
            put(
                WIN_DIR,
                Some(
                    std::env::var_os("WINDIR")
                        .map_or_else(|| PathBuf::from("C:/Windows"), PathBuf::from),
                ),
            );
        }

        #[cfg(target_os = "linux")]
        {
            put(XDG_DATA, dirs::data_dir());
            put(XDG_CONFIG, dirs::config_dir());
        }

        Host {
            platform: Platform::HOST,
            folders,
            user_name: whoami::username(),
        }
    }

    pub fn case_sensitive(&self) -> bool {
        self.platform.is_case_sensitive()
    }
}

/// A save location found on this machine. Files live under `base`, at paths
/// whose first segments match `pattern` (`*` and `?` wildcards allowed).
/// Anything below a matched folder belongs to the save too.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub template: String,
    pub base: PathBuf,
    pub pattern: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct SaveFile {
    /// Path below `base`, with forward slashes.
    pub rel: String,
    pub path: PathBuf,
    pub mtime: u64,
    pub size: u64,
}

/// Where `location` lives on this machine, if it applies here at all.
pub fn resolve(
    location: &SaveLocation,
    target: Platform,
    install_dir: &Path,
    host: &Host,
) -> Option<Resolved> {
    if location.platform.is_some_and(|p| p != target) {
        return None;
    }
    let template = location.path.trim().replace('\\', "/");
    let segments: Vec<&str> = template.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() < 2 {
        return None;
    }

    let root = match segments[0] {
        BASE => install_dir.to_path_buf(),
        // A game run through a compatibility layer (Proton on Linux) keeps its
        // other folders inside that layer, which we can't see from here.
        _ if target != host.platform => return None,
        other => host.folders.get(other)?.clone(),
    };

    let mut rest = Vec::with_capacity(segments.len() - 1);
    for segment in &segments[1..] {
        let segment = segment.replace(OS_USER_NAME, &host.user_name);
        if segment.contains(['<', '>', ':', '\0']) || segment == "." || segment == ".." {
            return None;
        }
        rest.push(segment);
    }

    let first_glob = rest.iter().position(|s| is_glob(s));
    // "<home>/*" and the like would sweep up a whole folder of other things.
    if first_glob == Some(0) && rest[0].chars().all(|c| c == '*' || c == '?') {
        return None;
    }
    // Without wildcards, the last segment may be a file or a folder; keeping it
    // in the pattern lets both cases restore to the same place.
    let split = first_glob.unwrap_or(rest.len() - 1);

    let mut base = root;
    for segment in &rest[..split] {
        base.push(segment);
    }
    Some(Resolved {
        template: location.path.clone(),
        base,
        pattern: rest[split..].to_vec(),
    })
}

/// Every location that applies here, without repeats.
pub fn resolve_all(
    locations: &[SaveLocation],
    target: Platform,
    install_dir: &Path,
    host: &Host,
) -> Vec<Resolved> {
    let mut out: Vec<Resolved> = Vec::new();
    for location in locations {
        if let Some(resolved) = resolve(location, target, install_dir, host)
            && !out.iter().any(|r| r.template == resolved.template)
        {
            out.push(resolved);
        }
    }
    out
}

/// The save files currently on disk for each location, sorted by path.
pub fn collect_all(
    resolved: &[Resolved],
    case_sensitive: bool,
) -> Result<Vec<(Resolved, Vec<SaveFile>)>, SyncError> {
    let mut budget = MAX_TOTAL_BYTES;
    resolved
        .iter()
        .map(|r| {
            let mut files = Vec::new();
            walk(
                &r.base,
                "",
                &r.pattern,
                case_sensitive,
                &mut files,
                &mut budget,
            )?;
            files.sort_by(|a, b| a.rel.cmp(&b.rel));
            Ok((r.clone(), files))
        })
        .collect()
}

fn walk(
    dir: &Path,
    prefix: &str,
    pattern: &[String],
    case_sensitive: bool,
    out: &mut Vec<SaveFile>,
    budget: &mut u64,
) -> Result<(), SyncError> {
    let Some((segment, rest)) = pattern.split_first() else {
        return Ok(());
    };
    let matches: Vec<(String, PathBuf)> = if is_glob(segment) {
        let re = glob_regex(segment, case_sensitive);
        match fs::read_dir(dir) {
            Ok(entries) => entries
                .filter_map(Result::ok)
                .filter_map(|e| {
                    let name = e.file_name().into_string().ok()?;
                    re.is_match(&name).then(|| (name, e.path()))
                })
                .collect(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        }
    } else {
        vec![(segment.clone(), dir.join(segment))]
    };

    for (name, path) in matches {
        // Symlinks are skipped: they can loop, or point outside the save.
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        let rel = format!("{prefix}{name}");
        if rest.is_empty() {
            if meta.is_file() {
                add_file(rel, path, &meta, out, budget)?;
            } else if meta.is_dir() {
                add_dir(&path, &rel, out, budget)?;
            }
        } else if meta.is_dir() {
            walk(&path, &format!("{rel}/"), rest, case_sensitive, out, budget)?;
        }
    }
    Ok(())
}

fn add_dir(
    dir: &Path,
    rel: &str,
    out: &mut Vec<SaveFile>,
    budget: &mut u64,
) -> Result<(), SyncError> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)?;
        let child = format!("{rel}/{name}");
        if meta.is_file() {
            add_file(child, path, &meta, out, budget)?;
        } else if meta.is_dir() {
            add_dir(&path, &child, out, budget)?;
        }
    }
    Ok(())
}

fn add_file(
    rel: String,
    path: PathBuf,
    meta: &fs::Metadata,
    out: &mut Vec<SaveFile>,
    budget: &mut u64,
) -> Result<(), SyncError> {
    let size = meta.len();
    if size > *budget {
        return Err(SyncError::TooLarge);
    }
    *budget -= size;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    out.push(SaveFile {
        rel,
        path,
        mtime,
        size,
    });
    Ok(())
}

pub fn is_glob(segment: &str) -> bool {
    segment.contains(['*', '?'])
}

fn glob_regex(segment: &str, case_sensitive: bool) -> Regex {
    let mut re = String::from("^");
    for c in segment.chars() {
        match c {
            '*' => re.push_str(".*"),
            '?' => re.push('.'),
            c => re.push_str(&regex::escape(c.encode_utf8(&mut [0; 4]))),
        }
    }
    re.push('$');
    RegexBuilder::new(&re)
        .case_insensitive(!case_sensitive)
        .build()
        .expect("an escaped glob is a valid regex")
}

/// A path from an archive, checked so it can only land inside the location:
/// plain segments only, and the leading ones must match the location's pattern.
pub fn safe_rel(rel: &str, pattern: &[String], case_sensitive: bool) -> Option<PathBuf> {
    let parts: Vec<&str> = rel.split('/').collect();
    if parts.len() < pattern.len()
        || parts
            .iter()
            .any(|p| p.is_empty() || *p == "." || *p == ".." || p.contains(['\\', ':', '\0']))
    {
        return None;
    }
    for (part, segment) in parts.iter().zip(pattern) {
        let ok = if is_glob(segment) {
            glob_regex(segment, case_sensitive).is_match(part)
        } else if case_sensitive {
            part == segment
        } else {
            part.eq_ignore_ascii_case(segment) || part.to_lowercase() == segment.to_lowercase()
        };
        if !ok {
            return None;
        }
    }
    Some(parts.iter().collect())
}
