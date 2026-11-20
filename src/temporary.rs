use crate::{native, store::Store};
use anyhow::{Result, bail, ensure};
use std::{
    fs,
    os::windows::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::mpsc,
    thread::sleep,
    time::{Duration, Instant},
};
use uuid::Uuid;

const MARKER: &str = ".switchboard-session";
const GRACEFUL_CLOSE_LIMIT: Duration = Duration::from_secs(5);
const JOB_EXIT_LIMIT: Duration = Duration::from_secs(5);
const LOCK_RELEASE_LIMIT: Duration = Duration::from_secs(5);
const DELETE_LIMIT: Duration = Duration::from_secs(30);
const CLEANUP_RETRY: Duration = Duration::from_millis(100);

fn cleanup_name(id: Uuid) -> String {
    format!(".cleanup-{id}")
}
fn cleanup_marker(root: &Path, id: Uuid) -> PathBuf {
    root.join(format!(".cleanup-{id}.marker"))
}
pub fn is_directory(path: &Path) -> Result<bool> {
    let meta = fs::symlink_metadata(path)?;
    Ok(meta.is_dir() && meta.file_attributes() & 0x400 == 0)
}
fn find_dir(root: &Path, id: Uuid) -> Option<PathBuf> {
    [root.join(id.to_string()), root.join(cleanup_name(id))]
        .into_iter()
        .find(|path| path.is_dir())
}
fn validate_dir(root: &Path, dir: &Path, id: Uuid) -> Result<()> {
    ensure!(
        is_directory(root)? && is_directory(dir)?,
        "Refusing to clean a redirected temporary folder."
    );
    let marker = dir.join(MARKER);
    if marker.is_file() {
        ensure!(
            fs::read_to_string(marker)? == id.to_string(),
            "Temporary profile ownership marker does not match."
        );
    } else {
        let expected = cleanup_name(id);
        ensure!(
            dir.file_name()
                .is_some_and(|name| name == expected.as_str())
                && fs::read_to_string(cleanup_marker(root, id))? == id.to_string(),
            "Temporary cleanup marker is missing or does not match."
        );
    }
    Ok(())
}
fn clean(store: &Store, id: Uuid) -> Result<()> {
    let root = store.0.join("Temporary");
    let Some(dir) = find_dir(&root, id) else {
        let marker = cleanup_marker(&root, id);
        if marker.try_exists()? {
            ensure!(
                fs::read_to_string(&marker)? == id.to_string(),
                "Temporary cleanup marker does not match."
            );
            fs::remove_file(marker)?;
        }
        return Ok(());
    };
    validate_dir(&root, &dir, id)?;
    ensure!(
        !native::running(id)?,
        "Temporary profile still has running processes."
    );
    // Keep the ownership marker until all browser files are gone, even if a file is locked.
    let data = dir.join("Data");
    if data.try_exists()? {
        ensure!(
            is_directory(&data)?,
            "Refusing to clean redirected browser data."
        );
        fs::remove_dir_all(data)?;
    }
    let marker = dir.join(MARKER);
    if marker.try_exists()? {
        fs::remove_file(marker)?;
    }
    fs::remove_dir(dir)?;
    let cleanup = cleanup_marker(&root, id);
    if cleanup.try_exists()? {
        ensure!(
            fs::read_to_string(&cleanup)? == id.to_string(),
            "Temporary cleanup marker does not match."
        );
        fs::remove_file(cleanup)?;
    }
    Ok(())
}
#[derive(Clone, Debug, PartialEq)]
pub struct Session {
    pub id: Uuid,
    pub started: u64,
    pub running: bool,
}
fn owned(item: &fs::DirEntry, root: &Path) -> Option<Uuid> {
    let name = item.file_name();
    let path = item.path();
    if !is_directory(&path).ok()? {
        return None;
    }
    if let Ok(id) = Uuid::parse_str(&name.to_string_lossy())
        && name == id.to_string().as_str()
        && fs::read_to_string(path.join(MARKER)).ok()?.trim() == id.to_string()
    {
        return Some(id);
    }
    let text = name.to_string_lossy();
    let id = Uuid::parse_str(text.strip_prefix(".cleanup-")?).ok()?;
    (text == cleanup_name(id)
        && fs::read_to_string(cleanup_marker(root, id)).ok()?.trim() == id.to_string())
    .then_some(id)
}
/// Lists live sessions and leftovers whose automatic cleanup has not completed yet.
pub fn sessions(store: &Store) -> Result<Vec<Session>> {
    let root = store.0.join("Temporary");
    if !root.try_exists()? {
        return Ok(vec![]);
    }
    ensure!(is_directory(&root)?, "Temporary folder is redirected.");
    let mut out = vec![];
    for item in fs::read_dir(root)? {
        let item = item?;
        if let Some(id) = owned(&item, &store.0.join("Temporary")) {
            let marker = item.path().join(MARKER);
            let marker = if marker.is_file() {
                marker
            } else {
                cleanup_marker(&store.0.join("Temporary"), id)
            };
            let started = fs::metadata(marker)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());
            out.push(Session {
                id,
                started,
                running: native::running(id)?,
            });
        }
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.started));
    Ok(out)
}
trait CleanupOps {
    fn now(&self) -> Duration;
    fn wait(&mut self, duration: Duration);
    fn running(&mut self) -> Result<bool>;
    fn request_close(&mut self) -> Result<()>;
    fn stop_job(&mut self) -> Result<()>;
    fn file_lockers(&mut self, timeout: Duration) -> Result<Vec<native::LockingProcess>>;
    fn terminate_edge(
        &mut self,
        pid: u32,
        started: windows::Win32::Foundation::FILETIME,
    ) -> Result<()>;
    fn erase(&mut self, timeout: Duration) -> Result<()>;
}
fn wait_for(
    ops: &mut impl CleanupOps,
    timeout: Duration,
    mut ready: impl FnMut(&mut dyn CleanupOps) -> Result<bool>,
) -> Result<bool> {
    let deadline = ops.now() + timeout;
    loop {
        if ready(ops)? {
            return Ok(true);
        }
        if ops.now() >= deadline {
            return Ok(false);
        }
        ops.wait(Duration::from_millis(100).min(deadline - ops.now()));
    }
}
fn end_session(
    ops: &mut impl CleanupOps,
    mut progress: impl FnMut(&str) -> Result<()>,
) -> Result<()> {
    progress("Asking Edge to close…")?;
    ops.request_close()?;
    if !wait_for(ops, GRACEFUL_CLOSE_LIMIT, |ops| Ok(!ops.running()?))? {
        progress("Closing the temporary Edge process group…")?;
        ops.stop_job()?;
        ensure!(
            wait_for(ops, JOB_EXIT_LIMIT, |ops| Ok(!ops.running()?))?,
            "The temporary Edge process group did not exit before the 5-second limit."
        );
    }
    progress("Finding processes holding the profile data…")?;
    let lock_deadline = ops.now() + LOCK_RELEASE_LIMIT;
    let lockers = ops.file_lockers(LOCK_RELEASE_LIMIT)?;
    if lockers.iter().any(|locker| locker.edge) {
        progress("Closing Edge processes holding profile files…")?;
    }
    for locker in lockers.iter().filter(|locker| locker.edge) {
        ops.terminate_edge(locker.pid, locker.started)?;
    }
    let lockers = wait_for_lockers(ops, lock_deadline, lockers)?;
    if !lockers.is_empty() {
        let pids = lockers
            .iter()
            .map(|locker| locker.pid.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "Processes still hold profile files after the 5-second limit (processes: {pids}); only verified Edge lock owners were terminated."
        );
    }
    progress("Erasing temporary browser data…")?;
    ops.erase(DELETE_LIMIT)
}
fn wait_for_lockers(
    ops: &mut impl CleanupOps,
    deadline: Duration,
    mut lockers: Vec<native::LockingProcess>,
) -> Result<Vec<native::LockingProcess>> {
    loop {
        if lockers.is_empty() || ops.now() >= deadline {
            return Ok(lockers);
        }
        let remaining = deadline.saturating_sub(ops.now());
        lockers = ops.file_lockers(remaining)?;
        if lockers.is_empty() || ops.now() >= deadline {
            return Ok(lockers);
        }
        ops.wait(Duration::from_millis(100).min(deadline - ops.now()));
    }
}
fn retry_delete(
    ops: &mut impl CleanupOps,
    timeout: Duration,
    mut remove: impl FnMut() -> Result<()>,
) -> Result<()> {
    let deadline = ops.now() + timeout;
    let mut delay = CLEANUP_RETRY;
    loop {
        match remove() {
            Ok(()) => return Ok(()),
            Err(error) if ops.now() >= deadline => return Err(error),
            Err(_) => {
                let remaining = deadline.saturating_sub(ops.now());
                ops.wait(delay.min(remaining));
                delay = (delay * 2).min(Duration::from_secs(2));
            }
        }
    }
}

struct RealCleanup<'a> {
    store: &'a Store,
    id: Uuid,
    started: Instant,
}
impl CleanupOps for RealCleanup<'_> {
    fn now(&self) -> Duration {
        self.started.elapsed()
    }
    fn wait(&mut self, duration: Duration) {
        sleep(duration);
    }
    fn running(&mut self) -> Result<bool> {
        native::running(self.id)
    }
    fn request_close(&mut self) -> Result<()> {
        native::close_session_windows(self.id).map(|_| ())
    }
    fn stop_job(&mut self) -> Result<()> {
        if native::running(self.id)? {
            native::stop_session(self.id)?;
        }
        Ok(())
    }
    fn file_lockers(&mut self, timeout: Duration) -> Result<Vec<native::LockingProcess>> {
        let deadline = Instant::now() + timeout;
        let root = self.store.0.join("Temporary");
        let dir = find_dir(&root, self.id);
        let Some(dir) = dir else {
            return Ok(vec![]);
        };
        validate_dir(&root, &dir, self.id)?;
        let data = dir.join("Data");
        if !data.try_exists()? {
            return Ok(vec![]);
        }
        ensure!(
            is_directory(&data)?,
            "Refusing to inspect redirected browser data."
        );
        let mut files = Vec::new();
        let mut pending = vec![data];
        while let Some(path) = pending.pop() {
            ensure!(
                Instant::now() < deadline,
                "Inspecting the temporary data folder exceeded the 5-second limit."
            );
            for entry in fs::read_dir(path)? {
                ensure!(
                    Instant::now() < deadline,
                    "Inspecting the temporary data folder exceeded the 5-second limit."
                );
                let entry = entry?;
                let child = entry.path();
                let metadata = fs::symlink_metadata(&child)?;
                if metadata.is_dir() && metadata.file_attributes() & 0x400 == 0 {
                    pending.push(child);
                } else if metadata.is_file() {
                    files.push(child);
                }
            }
        }
        if files.is_empty() {
            return Ok(vec![]);
        }
        let mut lockers = Vec::new();
        for batch in files.chunks(1000) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(
                !remaining.is_zero(),
                "Restart Manager file-lock inspection exceeded the 5-second limit."
            );
            let paths = batch.to_vec();
            let (tx, rx) = mpsc::sync_channel(1);
            std::thread::spawn(move || {
                let _ = tx.send(native::processes_locking(&paths));
            });
            let found = rx.recv_timeout(remaining).map_err(|error| {
                anyhow::anyhow!("Restart Manager file-lock inspection timed out: {error}")
            })??;
            for process in found {
                if !lockers.contains(&process) {
                    lockers.push(process);
                }
            }
        }
        Ok(lockers)
    }
    fn terminate_edge(
        &mut self,
        pid: u32,
        started: windows::Win32::Foundation::FILETIME,
    ) -> Result<()> {
        native::terminate_edge_process(pid, started).map(|_| ())
    }
    fn erase(&mut self, timeout: Duration) -> Result<()> {
        let root = self.store.0.join("Temporary");
        let deadline = Instant::now() + timeout;
        let lock = loop {
            match self.store.try_lock()? {
                Some(lock) => break lock,
                None => {
                    if Instant::now() >= deadline {
                        bail!("Timed out waiting to safely update temporary-session metadata.");
                    }
                    sleep(Duration::from_millis(50));
                }
            }
        };
        let original = root.join(self.id.to_string());
        let tombstone = root.join(cleanup_name(self.id));
        if original.try_exists()? {
            validate_dir(&root, &original, self.id)?;
            let marker = cleanup_marker(&root, self.id);
            fs::write(&marker, self.id.to_string())?;
            if let Err(error) = fs::rename(&original, &tombstone) {
                let _ = fs::remove_file(marker);
                return Err(error.into());
            }
        }
        if !tombstone.try_exists()? {
            let marker = cleanup_marker(&root, self.id);
            if marker.try_exists()? {
                ensure!(
                    fs::read_to_string(&marker)? == self.id.to_string(),
                    "Temporary cleanup marker does not match."
                );
                fs::remove_file(marker)?;
            }
            drop(lock);
            return Ok(());
        }
        validate_dir(&root, &tombstone, self.id)?;
        drop(lock);

        retry_delete(self, timeout, || {
            if !tombstone.try_exists()? {
                Ok(())
            } else {
                remove_readonly_tree(&tombstone)
            }
        })?;
        let marker = cleanup_marker(&root, self.id);
        if marker.try_exists()? {
            fs::remove_file(marker)?;
        }
        Ok(())
    }
}
fn remove_readonly_tree(path: &Path) -> Result<()> {
    fn clear_readonly(path: &Path) -> Result<()> {
        let metadata = fs::metadata(path)?;
        let attributes = metadata.file_attributes();
        if attributes & 1 != 0 {
            let attributes = if attributes & !1 == 0 {
                windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL.0
            } else {
                attributes & !1
            };
            unsafe {
                windows::Win32::Storage::FileSystem::SetFileAttributesW(
                    &windows::core::HSTRING::from(path.as_os_str()),
                    windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(attributes),
                )?;
            }
        }
        Ok(())
    }
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_attributes() & 0x400 != 0 {
        if metadata.is_dir() {
            fs::remove_dir(path)?;
        } else {
            fs::remove_file(path)?;
        }
        return Ok(());
    }
    if metadata.is_file() {
        clear_readonly(path)?;
        fs::remove_file(path)?;
        return Ok(());
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            remove_readonly_tree(&entry?.path())?;
        }
        clear_readonly(path)?;
        fs::remove_dir(path)?;
    }
    Ok(())
}
/// Ends a live session or erases a leftover without blocking the caller's thread.
pub fn discard_with_progress(
    store: &Store,
    id: Uuid,
    progress: impl FnMut(&str) -> Result<()>,
) -> Result<()> {
    let root = store.0.join("Temporary");
    let marker = cleanup_marker(&root, id);
    if find_dir(&root, id).is_none() && !marker.try_exists()? {
        return Ok(());
    }
    if let Some(dir) = find_dir(&root, id) {
        validate_dir(&root, &dir, id)?;
    } else {
        ensure!(
            fs::read_to_string(&marker)? == id.to_string(),
            "Temporary cleanup marker does not match."
        );
    }
    let mut ops = RealCleanup {
        store,
        id,
        started: Instant::now(),
    };
    end_session(&mut ops, progress)
}
pub fn discard(store: &Store, id: Uuid) -> Result<()> {
    discard_with_progress(store, id, |_| Ok(()))
}
/// Best-effort: a locked session is left in place and reported as a leftover by `sessions`.
pub fn recover(store: &Store) -> Result<()> {
    let root = store.0.join("Temporary");
    if !root.try_exists()? {
        return Ok(());
    }
    ensure!(is_directory(&root)?, "Temporary folder is redirected.");
    let ids = {
        let _lock = store.lock()?;
        let mut ids = Vec::new();
        for item in fs::read_dir(&root)? {
            if let Some(id) = owned(&item?, &root) {
                ids.push(id);
            }
        }
        for item in fs::read_dir(&root)? {
            let item = item?;
            let name = item.file_name().to_string_lossy().into_owned();
            if let Some(text) = name
                .strip_prefix(".cleanup-")
                .and_then(|n| n.strip_suffix(".marker"))
                && let Ok(id) = Uuid::parse_str(text)
                && name == format!(".cleanup-{id}.marker")
                && fs::read_to_string(item.path())?.trim() == id.to_string()
            {
                ids.push(id);
            }
        }
        ids.sort();
        ids.dedup();
        ids
    };
    let mut failures = Vec::new();
    for id in ids {
        if !native::running(id)?
            && let Err(error) = discard(store, id)
        {
            failures.push(format!("{id}: {error:#}"));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!("Temporary cleanup deferred: {}", failures.join("; "))
    }
}
pub fn run(store: &Store) -> Result<()> {
    let id = Uuid::new_v4();
    let job = {
        let _lock = store.lock()?;
        let root = store.0.join("Temporary");
        fs::create_dir_all(&root)?;
        ensure!(is_directory(&root)?, "Temporary folder is redirected.");
        let dir = root.join(id.to_string());
        fs::create_dir(&dir)?;
        fs::write(dir.join(MARKER), id.to_string())?;
        match native::launch_edge(id, &dir.join("Data"), true) {
            Ok(job) => job,
            Err(e) => {
                clean(store, id)?;
                return Err(e);
            }
        }
    };
    while job.active()? {
        sleep(Duration::from_millis(750));
    }
    for attempt in 0..10 {
        let result = {
            let _lock = store.lock()?;
            clean(store, id)
        };
        match result {
            Ok(()) => return Ok(()),
            Err(e) if attempt == 9 => return Err(e),
            Err(_) => sleep(Duration::from_secs(1)),
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct FakeCleanup {
        now: Duration,
        close_requested: bool,
        close_after: Option<Duration>,
        stop_requested: bool,
        stop_after: Option<Duration>,
        stop_calls: usize,
        lockers: Vec<native::LockingProcess>,
        terminated: Vec<u32>,
        hold_locks: bool,
        erased: bool,
        erase_timeout: Option<Duration>,
    }
    impl CleanupOps for FakeCleanup {
        fn now(&self) -> Duration {
            self.now
        }
        fn wait(&mut self, duration: Duration) {
            self.now += duration;
        }
        fn running(&mut self) -> Result<bool> {
            if self.close_requested && self.close_after.is_some_and(|at| self.now >= at) {
                return Ok(false);
            }
            if self.stop_requested && self.stop_after.is_some_and(|at| self.now >= at) {
                return Ok(false);
            }
            Ok(true)
        }
        fn request_close(&mut self) -> Result<()> {
            self.close_requested = true;
            Ok(())
        }
        fn stop_job(&mut self) -> Result<()> {
            self.stop_calls += 1;
            self.stop_requested = true;
            Ok(())
        }
        fn file_lockers(&mut self, _timeout: Duration) -> Result<Vec<native::LockingProcess>> {
            Ok(self.lockers.clone())
        }
        fn terminate_edge(
            &mut self,
            pid: u32,
            _started: windows::Win32::Foundation::FILETIME,
        ) -> Result<()> {
            self.terminated.push(pid);
            if !self.hold_locks {
                self.lockers.retain(|locker| locker.pid != pid);
            }
            Ok(())
        }
        fn erase(&mut self, timeout: Duration) -> Result<()> {
            self.erased = true;
            self.erase_timeout = Some(timeout);
            Ok(())
        }
    }

    #[test]
    fn graceful_end_avoids_force_kill_and_reports_all_phases() -> Result<()> {
        let mut fake = FakeCleanup {
            close_after: Some(Duration::from_millis(200)),
            ..Default::default()
        };
        let mut phases = Vec::new();
        end_session(&mut fake, |phase| {
            phases.push(phase.to_owned());
            Ok(())
        })?;
        assert_eq!(fake.stop_calls, 0);
        assert!(fake.erased);
        assert_eq!(fake.erase_timeout, Some(DELETE_LIMIT));
        assert_eq!(
            phases,
            [
                "Asking Edge to close…",
                "Finding processes holding the profile data…",
                "Erasing temporary browser data…"
            ]
        );
        Ok(())
    }

    #[test]
    fn forced_end_terminates_only_reported_locking_edge_processes() -> Result<()> {
        let mut fake = FakeCleanup {
            stop_after: Some(Duration::from_millis(300)),
            lockers: vec![
                native::LockingProcess {
                    pid: 41,
                    started: windows::Win32::Foundation::FILETIME::default(),
                    edge: true,
                },
                native::LockingProcess {
                    pid: 72,
                    started: windows::Win32::Foundation::FILETIME::default(),
                    edge: true,
                },
            ],
            ..Default::default()
        };
        end_session(&mut fake, |_| Ok(()))?;
        assert_eq!(fake.stop_calls, 1);
        assert_eq!(fake.terminated, [41, 72]);
        assert!(fake.erased);
        Ok(())
    }

    #[test]
    fn hung_edge_and_locked_files_stop_at_their_phase_deadlines() -> Result<()> {
        let mut hung = FakeCleanup::default();
        let error = end_session(&mut hung, |_| Ok(())).unwrap_err();
        assert!(error.to_string().contains("did not exit"));
        assert_eq!(hung.now, GRACEFUL_CLOSE_LIMIT + JOB_EXIT_LIMIT);
        assert!(!hung.erased);

        let mut locked = FakeCleanup {
            close_after: Some(Duration::ZERO),
            lockers: vec![native::LockingProcess {
                pid: 99,
                started: windows::Win32::Foundation::FILETIME::default(),
                edge: false,
            }],
            hold_locks: true,
            ..Default::default()
        };
        let error = end_session(&mut locked, |_| Ok(())).unwrap_err();
        assert!(error.to_string().contains("still hold profile files"));
        assert_eq!(locked.now, LOCK_RELEASE_LIMIT);
        assert!(locked.terminated.is_empty());
        assert!(!locked.erased);
        Ok(())
    }

    #[test]
    fn deletion_retries_with_backoff_and_honors_its_deadline() -> Result<()> {
        let mut fake = FakeCleanup::default();
        let mut attempts = 0;
        retry_delete(&mut fake, Duration::from_secs(5), || {
            attempts += 1;
            if attempts < 4 {
                anyhow::bail!("file is temporarily locked")
            }
            Ok(())
        })?;
        assert_eq!(attempts, 4);
        assert_eq!(fake.now, Duration::from_millis(700));

        let mut fake = FakeCleanup::default();
        let error = retry_delete(&mut fake, Duration::from_millis(350), || {
            anyhow::bail!("persistent file lock")
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "persistent file lock");
        assert_eq!(fake.now, Duration::from_millis(350));
        Ok(())
    }

    #[test]
    fn recovery_only_removes_owned_sessions() {
        let d = tempfile::tempdir().unwrap();
        let s = Store(d.path().into());
        let root = s.0.join("Temporary");
        let owned = Uuid::new_v4();
        let a = root.join(owned.to_string());
        let b = root.join(Uuid::new_v4().to_string());
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join(MARKER), owned.to_string()).unwrap();
        recover(&s).unwrap();
        assert!(!a.exists());
        assert!(b.exists());
    }
    #[test]
    fn recovery_finishes_a_marked_cleanup_tombstone() {
        let d = tempfile::tempdir().unwrap();
        let store = Store(d.path().into());
        let root = store.0.join("Temporary");
        let id = Uuid::new_v4();
        let tombstone = root.join(cleanup_name(id));
        fs::create_dir_all(tombstone.join("Data")).unwrap();
        fs::write(tombstone.join(MARKER), id.to_string()).unwrap();
        fs::write(cleanup_marker(&root, id), id.to_string()).unwrap();
        fs::write(tombstone.join("Data").join("Local State"), "partial").unwrap();
        recover(&store).unwrap();
        assert!(!tombstone.exists());
        assert!(!cleanup_marker(&root, id).exists());
    }
    #[test]
    #[ignore = "opens a real, disposable Edge window; requires Microsoft Edge on Windows"]
    fn real_edge_session_cleans_up_only_after_exit() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = Store(tmp.path().into());
        let root = store.0.join("Temporary");
        let worker = std::thread::spawn({
            let s = store.clone();
            move || run(&s)
        });
        let mut session = None;
        let mut data_written = false;
        for _ in 0..180 {
            if worker.is_finished() {
                break;
            }
            if let Ok(entries) = fs::read_dir(&root)
                && let Some(Ok(entry)) = entries.into_iter().next()
                && let Ok(id) = Uuid::parse_str(&entry.file_name().to_string_lossy())
                && native::running(id)?
            {
                session = Some(id);
                ensure!(
                    sessions(&store)?.iter().any(|t| t.id == id && t.running),
                    "Live temporary session is not listed as running."
                );
                if entry.path().join(r"Data\Local State").is_file()
                    || entry.path().join(r"Data\Default").is_dir()
                {
                    recover(&store)?;
                    data_written = entry.path().is_dir();
                    break;
                }
            }
            sleep(Duration::from_millis(250));
        }
        if let Some(id) = session
            && native::running(id)?
        {
            native::stop_session(id)?;
        }
        ensure!(
            session.is_some() || worker.is_finished(),
            "Edge did not start in the allotted time."
        );
        worker.join().unwrap()?;
        ensure!(
            data_written,
            "Edge did not initialize a disposable profile."
        );
        assert_eq!(fs::read_dir(root)?.count(), 0);
        Ok(())
    }
}
