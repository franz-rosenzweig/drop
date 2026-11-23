use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, UNIX_EPOCH},
};

use async_trait::async_trait;
use database::platform::Platform;
use futures_lite::future::block_on;

use super::{
    archive::{self, Archive},
    locate::{self, Host},
    *,
};
use crate::placeholder::{HOME, WIN_APP_DATA};

/// An in-memory stand-in for the server's save API.
#[derive(Default)]
struct FakeServer {
    locations: Vec<SaveLocation>,
    slots: Mutex<Vec<SaveSlot>>,
    objects: Mutex<HashMap<String, Vec<u8>>>,
    pushes: Mutex<usize>,
}

impl FakeServer {
    fn new(locations: &[(&str, Option<Platform>)]) -> Self {
        FakeServer {
            locations: locations
                .iter()
                .map(|(path, platform)| SaveLocation {
                    path: (*path).to_owned(),
                    platform: *platform,
                })
                .collect(),
            ..Default::default()
        }
    }

    fn pushes(&self) -> usize {
        *self.pushes.lock().unwrap()
    }

    fn latest_archive(&self) -> Archive {
        let slots = self.slots.lock().unwrap();
        let (object, _) = slots[0].latest().unwrap();
        Archive::read(&self.objects.lock().unwrap()[object]).unwrap()
    }
}

#[async_trait]
impl SaveServer for FakeServer {
    async fn locations(&self, _game_id: &str) -> Result<Vec<SaveLocation>, SyncError> {
        Ok(self.locations.clone())
    }
    async fn slots(&self, _game_id: &str) -> Result<Vec<SaveSlot>, SyncError> {
        Ok(self.slots.lock().unwrap().clone())
    }
    async fn create_slot(&self, _game_id: &str) -> Result<SaveSlot, SyncError> {
        let slot = SaveSlot {
            index: 0,
            history_object_ids: vec![],
            history_checksums: vec![],
        };
        self.slots.lock().unwrap().push(slot.clone());
        Ok(slot)
    }
    async fn push(&self, _game_id: &str, index: i64, archive: Vec<u8>) -> Result<(), SyncError> {
        let mut pushes = self.pushes.lock().unwrap();
        *pushes += 1;
        let id = format!("object-{pushes}");
        let mut slots = self.slots.lock().unwrap();
        let slot = slots.iter_mut().find(|s| s.index == index).unwrap();
        slot.history_object_ids.push(id.clone());
        slot.history_checksums.push(sha256_hex(&archive));
        self.objects.lock().unwrap().insert(id, archive);
        Ok(())
    }
    async fn fetch(&self, object_id: &str) -> Result<Vec<u8>, SyncError> {
        self.objects
            .lock()
            .unwrap()
            .get(object_id)
            .cloned()
            .ok_or_else(|| SyncError::Server("404".into()))
    }
}

/// One machine with the game installed, inside a temp folder.
struct Machine {
    _root: tempfile::TempDir,
    host: Host,
    ctx: SyncContext,
}

impl Machine {
    fn new(platform: Platform) -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let mut folders = HashMap::new();
        folders.insert(HOME, home.clone());
        if platform == Platform::Windows {
            folders.insert(WIN_APP_DATA, home.join("AppData/Roaming"));
        }
        let ctx = SyncContext {
            game_id: "0d6a5a7e-game".into(),
            install_dir: root.path().join("games/Game"),
            target_platform: platform,
            data_dir: root.path().join("drop/cloud_saves"),
        };
        fs::create_dir_all(&ctx.install_dir).unwrap();
        Machine {
            _root: root,
            host: Host {
                platform,
                folders,
                user_name: "player".into(),
            },
            ctx,
        }
    }

    fn game(&self, rel: &str) -> PathBuf {
        self.ctx.install_dir.join(rel)
    }

    fn home(&self, rel: &str) -> PathBuf {
        self.host.folders[HOME].join(rel)
    }

    fn pull(&self, server: &FakeServer) -> PullOutcome {
        block_on(pull(server, &self.ctx, &self.host)).unwrap()
    }

    fn push(&self, server: &FakeServer) -> PushOutcome {
        block_on(push(server, &self.ctx, &self.host)).unwrap()
    }

    fn backups(&self) -> usize {
        fs::read_dir(self.ctx.data_dir.join("backups").join(&self.ctx.game_id))
            .map_or(0, |d| d.count())
    }
}

fn write(path: &Path, data: &str, mtime: u64) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, data).unwrap();
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(mtime))
        .unwrap();
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

fn location(path: &str) -> SaveLocation {
    SaveLocation {
        path: path.into(),
        platform: None,
    }
}

#[test]
fn resolves_templates() {
    let m = Machine::new(Platform::Linux);
    let r = |path: &str| {
        locate::resolve(
            &location(path),
            Platform::Linux,
            &m.ctx.install_dir,
            &m.host,
        )
    };

    let save = r("<base>/www/save").unwrap();
    assert_eq!(save.base, m.game("www"));
    assert_eq!(save.pattern, vec!["save"]);

    let glob = r("<base>/save*.dat").unwrap();
    assert_eq!(glob.base, m.ctx.install_dir);
    assert_eq!(glob.pattern, vec!["save*.dat"]);

    let deep = r("<home>/Library/Containers/org/Wesnoth_*/saves").unwrap();
    assert_eq!(deep.base, m.home("Library/Containers/org"));
    assert_eq!(deep.pattern, vec!["Wesnoth_*", "saves"]);

    let user = r("<home>/<osUserName>/x").unwrap();
    assert_eq!(user.base, m.home("player"));

    assert!(
        r("<base>").is_none(),
        "a bare placeholder is the whole folder"
    );
    assert!(r("<home>/*").is_none(), "too broad");
    assert!(r("<base>/../escape").is_none());
    assert!(r("<storeUserId>/x").is_none());
    assert!(r("<root>/x").is_none());
    assert!(r("C:/Games/x").is_none(), "absolute paths aren't templates");
    assert!(
        r("<winAppData>/Studio").is_none(),
        "no such folder on Linux"
    );

    let windows_only = SaveLocation {
        path: "<base>/save".into(),
        platform: Some(Platform::Windows),
    };
    assert!(locate::resolve(&windows_only, Platform::Linux, &m.ctx.install_dir, &m.host).is_none());

    // A Windows game on a Linux machine: only paths inside the game folder.
    let proton = |path: &str| {
        locate::resolve(
            &location(path),
            Platform::Windows,
            &m.ctx.install_dir,
            &m.host,
        )
    };
    assert!(proton("<base>/save").is_some());
    assert!(proton("<home>/AppData/LocalLow/Studio").is_none());
}

#[test]
fn collects_globs_and_folders() {
    let m = Machine::new(Platform::Linux);
    write(&m.game("save1.dat"), "one", 10);
    write(&m.game("save2.dat"), "two", 20);
    write(&m.game("game.exe"), "binary", 5);
    write(&m.game("www/save/file1.rpgsave"), "a", 30);
    write(&m.game("www/save/sub/file2.rpgsave"), "b", 40);

    let resolved = locate::resolve_all(
        &[location("<base>/save*.dat"), location("<base>/www/save")],
        Platform::Linux,
        &m.ctx.install_dir,
        &m.host,
    );
    let found = locate::collect_all(&resolved, true).unwrap();
    let rels = |i: usize| found[i].1.iter().map(|f| f.rel.clone()).collect::<Vec<_>>();
    assert_eq!(rels(0), vec!["save1.dat", "save2.dat"]);
    assert_eq!(
        rels(1),
        vec!["save/file1.rpgsave", "save/sub/file2.rpgsave"]
    );
}

#[test]
fn archive_paths_stay_inside_their_location() {
    let pattern = vec!["save*.dat".to_owned()];
    assert!(locate::safe_rel("save1.dat", &pattern, true).is_some());
    assert!(locate::safe_rel("game.exe", &pattern, true).is_none());
    assert!(locate::safe_rel("../save1.dat", &pattern, true).is_none());

    let folder = vec!["save".to_owned()];
    assert!(locate::safe_rel("save/a/b.sav", &folder, true).is_some());
    assert!(locate::safe_rel("save/../../x", &folder, true).is_none());
    assert!(locate::safe_rel("other/x", &folder, true).is_none());
    assert!(locate::safe_rel("SAVE/x", &folder, false).is_some());
    assert!(locate::safe_rel("SAVE/x", &folder, true).is_none());
}

#[test]
fn two_machines_take_turns() {
    let server = FakeServer::new(&[("<base>/www/save", None)]);
    let pc = Machine::new(Platform::Windows);
    let mac = Machine::new(Platform::Windows);

    // Nothing anywhere yet.
    assert_eq!(pc.pull(&server), PullOutcome::NothingOnServer);
    assert_eq!(pc.push(&server), PushOutcome::NoFiles);

    // The PC plays first.
    write(&pc.game("www/save/file1.rpgsave"), "pc 1", 100);
    assert!(matches!(pc.push(&server), PushOutcome::Uploaded(_)));
    assert_eq!(pc.push(&server), PushOutcome::Unchanged);
    assert_eq!(server.pushes(), 1);

    // The Mac picks it up, plays, and uploads.
    assert_eq!(mac.pull(&server), PullOutcome::Restored(1));
    assert_eq!(read(&mac.game("www/save/file1.rpgsave")), "pc 1");
    assert_eq!(mac.pull(&server), PullOutcome::UpToDate);
    write(&mac.game("www/save/file1.rpgsave"), "mac 2", 200);
    write(&mac.game("www/save/file2.rpgsave"), "mac new", 200);
    assert!(matches!(mac.push(&server), PushOutcome::Uploaded(_)));

    // Back on the PC, which hasn't played since: its copy is replaced, and the
    // old one kept as a backup.
    assert_eq!(pc.pull(&server), PullOutcome::Restored(2));
    assert_eq!(read(&pc.game("www/save/file1.rpgsave")), "mac 2");
    assert_eq!(read(&pc.game("www/save/file2.rpgsave")), "mac new");
    assert_eq!(pc.backups(), 1);
    // Nothing changed by the pull counts as a change to upload.
    assert_eq!(pc.push(&server), PushOutcome::Unchanged);
}

#[test]
fn newer_saves_win_when_both_machines_played() {
    let server = FakeServer::new(&[("<base>/save", None)]);
    let pc = Machine::new(Platform::Windows);
    let mac = Machine::new(Platform::Windows);

    write(&pc.game("save/slot.sav"), "pc 1", 100);
    pc.push(&server);
    mac.pull(&server);

    // Both play without syncing; the Mac plays later.
    write(&pc.game("save/slot.sav"), "pc 2", 200);
    write(&mac.game("save/slot.sav"), "mac 2", 300);
    pc.push(&server);

    // The Mac's saves are newer, so they stay and go up on quit.
    assert_eq!(mac.pull(&server), PullOutcome::KeptLocal);
    assert_eq!(read(&mac.game("save/slot.sav")), "mac 2");
    assert!(matches!(mac.push(&server), PushOutcome::Uploaded(_)));

    // The PC's own changes are older than the Mac's: replaced, with a backup.
    write(&pc.game("save/slot.sav"), "pc 3 unsynced", 250);
    assert_eq!(pc.pull(&server), PullOutcome::Restored(1));
    assert_eq!(read(&pc.game("save/slot.sav")), "mac 2");
    assert_eq!(pc.backups(), 1);
}

#[test]
fn first_sync_on_a_machine_that_already_has_saves() {
    let server = FakeServer::new(&[("<base>/save", None)]);
    let pc = Machine::new(Platform::Windows);
    let mac = Machine::new(Platform::Windows);

    write(&pc.game("save/slot.sav"), "pc", 100);
    pc.push(&server);

    // The Mac had saves copied over by hand before sync existed, newer ones.
    write(&mac.game("save/slot.sav"), "mac", 500);
    assert_eq!(mac.pull(&server), PullOutcome::KeptLocal);
    assert!(matches!(mac.push(&server), PushOutcome::Uploaded(_)));
    assert_eq!(server.latest_archive().files["<base>/save"][0].data, b"mac");
}

#[test]
fn uploads_keep_other_platforms_saves() {
    let server = FakeServer::new(&[
        ("<base>/save", None),
        ("<winAppData>/Studio/Game", Some(Platform::Windows)),
        ("<home>/Library/Studio/Game", Some(Platform::macOS)),
    ]);
    let pc = Machine::new(Platform::Windows);
    let mac = Machine::new(Platform::macOS);

    write(&pc.game("save/shared.sav"), "pc shared", 100);
    write(
        &pc.home("AppData/Roaming/Studio/Game/settings.ini"),
        "pc only",
        100,
    );
    pc.push(&server);

    assert_eq!(mac.pull(&server), PullOutcome::Restored(1));
    assert!(
        !mac.home("AppData").exists(),
        "Windows paths stay off the Mac"
    );
    write(&mac.game("save/shared.sav"), "mac shared", 200);
    write(&mac.home("Library/Studio/Game/prefs"), "mac only", 200);
    mac.push(&server);

    let latest = server.latest_archive();
    assert_eq!(latest.files.len(), 3, "the PC's folder rides along");
    assert_eq!(latest.files["<winAppData>/Studio/Game"][0].data, b"pc only");

    assert_eq!(pc.pull(&server), PullOutcome::Restored(2));
    assert_eq!(read(&pc.game("save/shared.sav")), "mac shared");
    assert_eq!(
        read(&pc.home("AppData/Roaming/Studio/Game/settings.ini")),
        "pc only"
    );
}

#[test]
fn restores_keep_file_times() {
    let server = FakeServer::new(&[("<base>/save", None)]);
    let pc = Machine::new(Platform::Windows);
    let mac = Machine::new(Platform::Windows);
    write(&pc.game("save/slot.sav"), "pc", 1_700_000_000);
    pc.push(&server);
    mac.pull(&server);
    let mtime = fs::metadata(mac.game("save/slot.sav"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(mtime, UNIX_EPOCH + Duration::from_secs(1_700_000_000));
}

#[test]
fn refuses_a_tampered_download() {
    let server = FakeServer::new(&[("<base>/save", None)]);
    let pc = Machine::new(Platform::Windows);
    let mac = Machine::new(Platform::Windows);
    write(&pc.game("save/slot.sav"), "pc", 100);
    pc.push(&server);
    server
        .objects
        .lock()
        .unwrap()
        .values_mut()
        .for_each(|bytes| bytes.push(0));
    assert!(matches!(
        block_on(pull(&server, &mac.ctx, &mac.host)),
        Err(SyncError::ChecksumMismatch)
    ));
    assert!(!mac.game("save").exists());
}

#[test]
fn skips_archive_entries_that_escape() {
    let m = Machine::new(Platform::Linux);
    let resolved = locate::resolve_all(
        &[location("<base>/save*.dat")],
        Platform::Linux,
        &m.ctx.install_dir,
        &m.host,
    );
    let mut files = std::collections::BTreeMap::new();
    files.insert(
        "<base>/save*.dat".to_owned(),
        vec![
            archive::StoredFile {
                rel: "save1.dat".into(),
                mtime: 1,
                data: b"ok".to_vec(),
            },
            archive::StoredFile {
                rel: "../../evil".into(),
                mtime: 1,
                data: b"no".to_vec(),
            },
            archive::StoredFile {
                rel: "game.exe".into(),
                mtime: 1,
                data: b"no".to_vec(),
            },
        ],
    );
    let archive = Archive {
        manifest: archive::Manifest {
            version: 1,
            created: 0,
            newest: 1,
            locations: vec![],
        },
        files,
    };
    assert_eq!(archive::apply(&archive, &resolved, true).unwrap(), 1);
    assert_eq!(read(&m.game("save1.dat")), "ok");
    assert!(!m.game("game.exe").exists());
}

#[test]
fn keeps_five_backups() {
    let server = FakeServer::new(&[("<base>/save", None)]);
    let pc = Machine::new(Platform::Windows);
    let mac = Machine::new(Platform::Windows);
    for round in 0..7u64 {
        write(
            &pc.game("save/slot.sav"),
            &format!("pc {round}"),
            100 + round * 10,
        );
        pc.push(&server);
        mac.pull(&server);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(mac.backups(), 5);
}
