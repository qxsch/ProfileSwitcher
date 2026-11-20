use crate::{native, store::Store};
use anyhow::{Result, ensure};
use std::{fs, os::windows::fs::MetadataExt, path::Path, thread::sleep, time::Duration};
use uuid::Uuid;

const MARKER: &str = ".switchboard-session";
fn is_directory(path: &Path) -> Result<bool> {
    let meta = fs::symlink_metadata(path)?;
    Ok(meta.is_dir() && meta.file_attributes() & 0x400 == 0)
}
fn clean(store: &Store, id: Uuid) -> Result<()> {
    let root = store.0.join("Temporary");
    let dir = root.join(id.to_string());
    if !dir.try_exists()? {
        return Ok(());
    }
    ensure!(
        is_directory(&root)? && is_directory(&dir)?,
        "Refusing to clean a redirected temporary folder."
    );
    ensure!(
        fs::read_to_string(dir.join(MARKER))? == id.to_string(),
        "Temporary profile ownership marker does not match."
    );
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
    fs::remove_file(dir.join(MARKER))?;
    fs::remove_dir(dir)?;
    Ok(())
}
pub fn recover(store: &Store) -> Result<()> {
    let _lock = store.lock()?;
    let root = store.0.join("Temporary");
    if !root.try_exists()? {
        return Ok(());
    }
    ensure!(is_directory(&root)?, "Temporary folder is redirected.");
    for item in fs::read_dir(root)? {
        let item = item?;
        let Ok(id) = Uuid::parse_str(&item.file_name().to_string_lossy()) else {
            continue;
        };
        if item.file_name() != id.to_string().as_str()
            || !is_directory(&item.path())?
            || !item.path().join(MARKER).is_file()
        {
            continue;
        }
        if !native::running(id)? {
            clean(store, id)?;
        }
    }
    Ok(())
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
            native::stop_test_session(id)?;
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
