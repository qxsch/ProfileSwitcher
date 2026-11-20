use std::{
    fs,
    os::windows::fs::OpenOptionsExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread::sleep,
    time::{Duration, Instant},
};
use switchboard::{store::Store, temporary};
use uuid::Uuid;

struct ChildGuard(Option<Child>);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn lock_holder_child() -> anyhow::Result<()> {
    let Some(path) = std::env::var_os("SWITCHBOARD_LOCK_CHILD") else {
        return Ok(());
    };
    let path = PathBuf::from(path);
    let _file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(&path)?;
    fs::write(path.with_extension("ready"), "ready")?;
    loop {
        sleep(Duration::from_secs(1));
    }
}

#[test]
fn cleanup_waits_for_non_edge_lock_holder_then_erases_only_its_session() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store(dir.path().into());
    let id = Uuid::new_v4();
    let root = store.0.join("Temporary");
    let session = root.join(id.to_string());
    let data = session.join("Data");
    fs::create_dir_all(&data)?;
    fs::write(session.join(".switchboard-session"), id.to_string())?;
    let locked_file = data.join("Cookies");
    fs::write(&locked_file, "test browser data")?;
    let ready = locked_file.with_extension("ready");
    let mut child = ChildGuard(Some(
        Command::new(std::env::current_exe()?)
            .args(["--exact", "lock_holder_child", "--nocapture"])
            .env("SWITCHBOARD_LOCK_CHILD", &locked_file)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
    ));
    let started = Instant::now();
    while !ready.exists() {
        if child.0.as_mut().unwrap().try_wait()?.is_some() {
            anyhow::bail!("The lock-holder helper exited before acquiring its file lock.");
        }
        anyhow::ensure!(
            started.elapsed() < Duration::from_secs(5),
            "The lock-holder helper did not acquire the file lock."
        );
        sleep(Duration::from_millis(10));
    }

    let release = std::thread::spawn(move || {
        sleep(Duration::from_millis(350));
        let mut child = child.0.take().unwrap();
        let killed = child.kill();
        let waited = child.wait();
        killed?;
        waited?;
        Ok::<_, std::io::Error>(())
    });
    let mut phases = Vec::new();
    let cleanup = temporary::discard_with_progress(&store, id, |phase| {
        phases.push(phase.to_owned());
        Ok(())
    });
    let released = release.join().unwrap();
    cleanup?;
    released?;
    assert!(started.elapsed() >= Duration::from_millis(250));
    assert!(!session.exists());
    assert!(root.exists());
    assert!(
        phases
            .iter()
            .any(|phase| phase.contains("holding the profile data"))
    );
    assert!(!locked_file.exists());
    Ok(())
}
