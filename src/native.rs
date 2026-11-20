use crate::store::Profile;
use anyhow::{Context, Result, bail, ensure};
use std::{
    env,
    ffi::OsStr,
    mem::size_of,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
};
use uuid::Uuid;
use windows::{
    Win32::{
        Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND, HANDLE, HWND, LPARAM},
        System::{Com::*, JobObjects::*, SystemServices::JOB_OBJECT_QUERY, Threading::*},
        UI::{Shell::*, WindowsAndMessaging::*},
    },
    core::{BOOL, GUID, HRESULT, HSTRING, Interface, PCWSTR, PWSTR},
};

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
fn known_folder(id: &GUID) -> Result<PathBuf> {
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None)?;
        let path = std::ffi::OsString::from_wide(p.as_wide());
        CoTaskMemFree(Some(p.0.cast()));
        Ok(path.into())
    }
}
pub fn local_data() -> Result<PathBuf> {
    known_folder(&FOLDERID_LocalAppData)
}
pub fn desktop() -> Result<PathBuf> {
    known_folder(&FOLDERID_Desktop)
}
fn edge() -> Result<PathBuf> {
    for base in [
        env::var_os("ProgramFiles(x86)"),
        env::var_os("ProgramFiles"),
        Some(local_data()?.into()),
    ]
    .into_iter()
    .flatten()
    {
        let exe = PathBuf::from(base).join(r"Microsoft\Edge\Application\msedge.exe");
        if exe.is_file() {
            return Ok(exe);
        }
    }
    bail!("Microsoft Edge was not found. Install the stable Windows version of Edge and try again.")
}
fn job_name(id: Uuid) -> HSTRING {
    format!(r"Global\Switchboard.Edge.{id}").into()
}
fn existing_job(id: Uuid) -> Result<Option<Handle>> {
    match unsafe { OpenJobObjectW(JOB_OBJECT_QUERY, false, &job_name(id)) } {
        Ok(h) => Ok(Some(Handle(h))),
        Err(e) if e.code() == HRESULT::from_win32(ERROR_FILE_NOT_FOUND.0) => Ok(None),
        Err(e) => Err(e.into()),
    }
}
pub struct Job(Handle);
impl Job {
    pub fn active(&self) -> Result<bool> {
        active(&self.0)
    }
}
fn active(job: &Handle) -> Result<bool> {
    let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    unsafe {
        QueryInformationJobObject(
            Some(job.0),
            JobObjectBasicAccountingInformation,
            (&raw mut info).cast(),
            size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            None,
        )?;
    }
    Ok(info.ActiveProcesses > 0)
}
pub fn running(id: Uuid) -> Result<bool> {
    existing_job(id)?.as_ref().map(active).unwrap_or(Ok(false))
}

// Attaching the job at creation leaves no window for a Chromium child to escape accounting.
fn launch_in_job(id: Uuid, exe: &Path, arguments: &OsStr, temporary: bool) -> Result<Job> {
    let job = Handle(unsafe { CreateJobObjectW(None, &job_name(id))? });
    if temporary {
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&raw const info).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )?;
        }
    }
    let mut bytes = 0;
    unsafe {
        let _ = InitializeProcThreadAttributeList(None, 1, None, &mut bytes);
    }
    ensure!(
        bytes > 0,
        "Windows could not allocate a process attribute list."
    );
    let mut buffer = vec![0usize; bytes.div_ceil(size_of::<usize>())];
    let list = LPPROC_THREAD_ATTRIBUTE_LIST(buffer.as_mut_ptr().cast());
    unsafe {
        InitializeProcThreadAttributeList(Some(list), 1, None, &mut bytes)?;
    }
    struct Attributes(LPPROC_THREAD_ATTRIBUTE_LIST);
    impl Drop for Attributes {
        fn drop(&mut self) {
            unsafe { DeleteProcThreadAttributeList(self.0) }
        }
    }
    let attributes = Attributes(list);
    let mut cmd = vec![34u16];
    cmd.extend(exe.as_os_str().encode_wide());
    cmd.extend([34, 32]);
    cmd.extend(arguments.encode_wide());
    cmd.push(0);
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.lpAttributeList = attributes.0;
    let mut pi = PROCESS_INFORMATION::default();
    unsafe {
        UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
            Some((&raw const job.0).cast()),
            size_of::<HANDLE>(),
            None,
            None,
        )?;
        CreateProcessW(
            &HSTRING::from(exe.as_os_str()),
            Some(PWSTR(cmd.as_mut_ptr())),
            None,
            None,
            false,
            EXTENDED_STARTUPINFO_PRESENT,
            None,
            PCWSTR::null(),
            &startup.StartupInfo,
            &mut pi,
        )
        .context("Windows could not start the isolated process")?;
    }
    let (_process, _thread) = (Handle(pi.hProcess), Handle(pi.hThread));
    Ok(Job(job))
}
pub fn launch_edge(id: Uuid, dir: &Path, temporary: bool) -> Result<Job> {
    let mut args = std::ffi::OsString::from(
        "--no-first-run --no-default-browser-check --disable-background-mode --new-window \"--user-data-dir=",
    );
    args.push(dir);
    args.push("\"");
    launch_in_job(id, &edge()?, &args, temporary)
}

pub fn focus(id: Uuid) -> Result<bool> {
    let Some(job) = existing_job(id)? else {
        return Ok(false);
    };
    struct Search {
        job: HANDLE,
        window: Option<HWND>,
    }
    unsafe extern "system" fn visit(window: HWND, param: LPARAM) -> BOOL {
        let search = unsafe { &mut *(param.0 as *mut Search) };
        if search.window.is_some() || !unsafe { IsWindowVisible(window) }.as_bool() {
            return true.into();
        }
        let mut class = [0u16; 64];
        let n = unsafe { GetClassNameW(window, &mut class) };
        if !class[..n as usize]
            .iter()
            .copied()
            .eq("Chrome_WidgetWin_1".encode_utf16())
        {
            return true.into();
        }
        let mut pid = 0;
        unsafe {
            GetWindowThreadProcessId(window, Some(&mut pid));
        }
        if let Ok(h) = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
            let h = Handle(h);
            let mut belongs = BOOL(0);
            if unsafe { IsProcessInJob(h.0, Some(search.job), &mut belongs) }.is_ok()
                && belongs.as_bool()
            {
                search.window = Some(window);
            }
        }
        true.into()
    }
    let mut s = Search {
        job: job.0,
        window: None,
    };
    unsafe {
        EnumWindows(Some(visit), LPARAM((&raw mut s) as isize))?;
    }
    if let Some(window) = s.window {
        unsafe {
            if IsIconic(window).as_bool() {
                let _ = ShowWindow(window, SW_RESTORE);
            }
            if SetForegroundWindow(window).as_bool() {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn link_name(name: &str) -> String {
    let clean: String = name
        .chars()
        .take(80)
        .map(|c| {
            if c.is_control() || r#"<>:"/\|?*"#.contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let clean = clean.trim_matches([' ', '.']);
    let stem = clean
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_uppercase();
    let reserved = matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || (stem.starts_with("COM") || stem.starts_with("LPT"))
        && stem.chars().count() == 4
        && stem.ends_with([
            '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', '¹', '²', '³',
        ]);
    if clean.is_empty() {
        "Profile".into()
    } else if reserved {
        format!("_{clean}")
    } else {
        clean.into()
    }
}
pub fn shortcut(p: &Profile, icon: &Path, folder: &Path) -> Result<PathBuf> {
    let name = link_name(&p.name);
    let base = folder.to_owned();
    write_link(p, icon, folder, move |i| {
        Some(base.join(if i == 1 {
            format!("{name}.lnk")
        } else {
            format!("{name} ({i}).lnk")
        }))
    })?
    .context("There are too many shortcuts with this name in the destination.")
}
/// Recreates a link at its exact previous path; returns false if that path is already taken.
pub fn shortcut_at(p: &Profile, icon: &Path, dest: &Path) -> Result<bool> {
    let folder = dest.parent().context("Shortcut has no folder.")?;
    let dest = dest.to_owned();
    Ok(write_link(p, icon, folder, move |i| (i == 1).then(|| dest.clone()))?.is_some())
}
/// True only for a Switchboard link that opens this profile, so user files are never removed.
pub fn is_profile_link(path: &Path, id: Uuid) -> bool {
    let arg: Vec<u8> = format!("--open {id}")
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("lnk"))
        && std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file() && m.len() < 1 << 20)
        && std::fs::read(path).is_ok_and(|b| {
            b.starts_with(&[0x4c, 0, 0, 0]) && b.windows(arg.len()).any(|s| s == arg)
        })
}
fn write_link(
    p: &Profile,
    icon: &Path,
    folder: &Path,
    candidate: impl Fn(u32) -> Option<PathBuf> + Send + 'static,
) -> Result<Option<PathBuf>> {
    // Keep COM apartment ownership independent of the UI backend.
    let (id, name, icon, folder) = (p.id, p.name.clone(), icon.to_owned(), folder.to_owned());
    std::thread::spawn(move || -> Result<Option<PathBuf>> {
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        }
        struct Com;
        impl Drop for Com {
            fn drop(&mut self) {
                unsafe { CoUninitialize() }
            }
        }
        let _com = Com;
        let link: IShellLinkW =
            unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)? };
        let exe = env::current_exe()?;
        unsafe {
            link.SetPath(&HSTRING::from(exe.as_os_str()))?;
            link.SetArguments(&HSTRING::from(format!("--open {id}")))?;
            link.SetIconLocation(&HSTRING::from(icon.as_os_str()), 0)?;
            link.SetDescription(&HSTRING::from(format!("Open {name} in Switchboard")))?;
        }
        let tmp = tempfile::Builder::new()
            .suffix(".lnk")
            .tempfile_in(&folder)?
            .into_temp_path();
        let persist: IPersistFile = link.cast()?;
        unsafe {
            persist.Save(&HSTRING::from(tmp.as_os_str()), true)?;
        }
        for i in 1..=10000 {
            let Some(dest) = candidate(i) else { break };
            // MoveFileW never replaces an existing link, even when two exports race.
            match unsafe {
                windows::Win32::Storage::FileSystem::MoveFileW(
                    &HSTRING::from(tmp.as_os_str()),
                    &HSTRING::from(dest.as_os_str()),
                )
            } {
                Ok(()) => return Ok(Some(dest)),
                Err(e)
                    if [HRESULT::from_win32(183), HRESULT::from_win32(80)].contains(&e.code()) =>
                {
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(None)
    })
    .join()
    .map_err(|_| anyhow::anyhow!("Shortcut writer stopped unexpectedly."))?
}

/// Ends every process in a session's job; used only for disposable temporary sessions.
pub fn stop_session(id: Uuid) -> Result<()> {
    use windows::Win32::System::SystemServices::JOB_OBJECT_TERMINATE;
    let h = Handle(unsafe { OpenJobObjectW(JOB_OBJECT_TERMINATE, false, &job_name(id))? });
    unsafe { TerminateJobObject(h.0, 0)? };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shortcut_names_are_safe_and_collision_free() {
        let d = tempfile::tempdir().unwrap();
        let p = Profile {
            id: Uuid::new_v4(),
            name: "NUL.txt".into(),
            collection: String::new(),
            symbol: 0,
            color: 0,
            pinned: false,
            trashed: false,
            opened: 0,
            shortcuts: vec![],
        };
        let icon = d.path().join("test.ico");
        let a = shortcut(&p, &icon, d.path()).unwrap();
        let b = shortcut(&p, &icon, d.path()).unwrap();
        assert_eq!(a.file_name().unwrap(), "_NUL.txt.lnk");
        assert_ne!(a, b);
        assert!(is_profile_link(&a, p.id));
        assert!(!is_profile_link(&a, Uuid::new_v4()));
        assert!(!shortcut_at(&p, &icon, &a).unwrap());
    }

    #[test]
    fn watcher_tracks_descendants_after_launcher_exits() {
        let id = Uuid::new_v4();
        let exe = PathBuf::from(env::var_os("SystemRoot").unwrap()).join(r"System32\cmd.exe");
        let job = launch_in_job(
            id,
            &exe,
            OsStr::new("/d /c start \"\" /b ping.exe -n 3 127.0.0.1"),
            false,
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            job.active().unwrap(),
            "process exited while holding the job handle"
        );
        assert!(
            running(id).unwrap(),
            "named job not visible with live handle"
        );
        for _ in 0..50 {
            if !running(id).unwrap() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
        panic!("job did not drain");
    }
}
