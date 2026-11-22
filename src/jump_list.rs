use crate::{
    native,
    store::{Profile, Store},
};
use anyhow::{Context, Result};
use std::{cmp::Reverse, collections::HashSet};
use windows::{
    Win32::{
        Foundation::E_NOINTERFACE,
        System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance},
        UI::Shell::{
            Common::{IObjectArray, IObjectCollection},
            DestinationList, EnumerableObjectCollection, ICustomDestinationList, IShellLinkW,
            SetCurrentProcessExplicitAppUserModelID,
        },
    },
    core::{HSTRING, IUnknown, Interface},
};

pub const APP_ID: &str = "Qxsch.Switchboard";

pub fn initialize() -> Result<()> {
    unsafe { SetCurrentProcessExplicitAppUserModelID(&HSTRING::from(APP_ID))? };
    Ok(())
}

fn recent_profiles(profiles: &[Profile], removed: &HashSet<String>, slots: u32) -> Vec<Profile> {
    let mut recent: Vec<_> = profiles
        .iter()
        .filter(|p| !p.trashed && p.opened != 0 && !removed.contains(&format!("--open {}", p.id)))
        .cloned()
        .collect();
    recent.sort_by_key(|p| (Reverse(p.opened), p.name.to_lowercase(), p.id));
    recent.truncate(5.min(slots as usize));
    recent
}

fn removed_arguments(removed: &IObjectArray) -> Result<HashSet<String>> {
    let mut arguments = HashSet::new();
    for i in 0..unsafe { removed.GetCount()? } {
        let object: IUnknown = unsafe { removed.GetAt(i)? };
        let link = match object.cast::<IShellLinkW>() {
            Ok(link) => link,
            Err(error) if error.code() == E_NOINTERFACE => continue,
            Err(error) => return Err(error.into()),
        };
        let mut buffer = vec![0u16; 32768];
        unsafe { link.GetArguments(&mut buffer)? };
        let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        arguments.insert(String::from_utf16(&buffer[..end])?);
    }
    Ok(arguments)
}

fn rebuild(list: &ICustomDestinationList, store: &Store, profiles: &[Profile]) -> Result<()> {
    let mut slots = 0;
    let removed: IObjectArray = unsafe { list.BeginList(&mut slots)? };
    let result = (|| -> Result<()> {
        let removed = removed_arguments(&removed)?;
        let recent = recent_profiles(profiles, &removed, slots);
        if !recent.is_empty() {
            let collection: IObjectCollection = unsafe {
                CoCreateInstance(&EnumerableObjectCollection, None, CLSCTX_INPROC_SERVER)?
            };
            for profile in &recent {
                let link = native::profile_link(profile, &store.icon_path(profile.id))?;
                unsafe { collection.AddObject(&link)? };
            }
            let array: IObjectArray = collection.cast()?;
            unsafe { list.AppendCategory(&HSTRING::from("Recent profiles"), &array) }
                .context("Windows could not show recent profiles. Check the Start settings for recently opened items and any organization privacy policy.")?;
        }
        unsafe { list.CommitList()? };
        Ok(())
    })();
    if let Err(error) = result {
        if let Err(abort) = unsafe { list.AbortList() } {
            return Err(error.context(format!("Could not abort the Jump List update: {abort}")));
        }
        return Err(error);
    }
    Ok(())
}

/// Serializes publication with catalog writes, including opens from other processes.
pub fn refresh(store: &Store) -> Result<Vec<Profile>> {
    let _lock = store.lock()?;
    let profiles = store.read()?.profiles;
    let store = store.clone();
    native::with_com(move || {
        let list: ICustomDestinationList =
            unsafe { CoCreateInstance(&DestinationList, None, CLSCTX_INPROC_SERVER)? };
        unsafe { list.SetAppID(&HSTRING::from(APP_ID))? };
        rebuild(&list, &store, &profiles)?;
        Ok(profiles)
    })
    .context("Could not update Switchboard's taskbar recent profiles")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(index: u128, opened: u64, trashed: bool) -> Profile {
        Profile {
            id: uuid::Uuid::from_u128(index),
            name: format!("Profile {index}"),
            collection: String::new(),
            symbol: 0,
            color: 0,
            pinned: false,
            trashed,
            opened,
            shortcuts: vec![],
        }
    }

    #[test]
    fn recent_list_is_limited_to_five_opened_nontrashed_profiles_newest_first() {
        let mut profiles: Vec<_> = (1..=8).map(|i| profile(i, i as u64, false)).collect();
        profiles.push(profile(9, 999, true));
        profiles.push(profile(10, 0, false));
        let recent = recent_profiles(&profiles, &HashSet::new(), 20);
        assert_eq!(
            recent.iter().map(|p| p.opened).collect::<Vec<_>>(),
            [8, 7, 6, 5, 4]
        );
    }

    #[test]
    fn recent_list_respects_windows_removals_and_available_slots() {
        let profiles: Vec<_> = (1..=8).map(|i| profile(i, i as u64, false)).collect();
        let removed = HashSet::from([format!("--open {}", profiles[7].id)]);
        assert_eq!(
            recent_profiles(&profiles, &removed, 20)
                .iter()
                .map(|p| p.opened)
                .collect::<Vec<_>>(),
            [7, 6, 5, 4, 3]
        );
        assert_eq!(recent_profiles(&profiles, &removed, 2).len(), 2);
        assert!(recent_profiles(&profiles, &removed, 0).is_empty());
    }

    #[test]
    fn recent_list_tracks_renames_opens_trash_and_restore_without_changing_ids() {
        let mut profiles = vec![profile(1, 1, false), profile(2, 2, false)];
        profiles[0].name = "Renamed customer".into();
        profiles[0].opened = 3;
        profiles[0].symbol = 99;
        profiles[0].color = 5;
        let recent = recent_profiles(&profiles, &HashSet::new(), 5);
        assert_eq!(recent[0], profiles[0]);
        profiles[0].trashed = true;
        assert_eq!(
            recent_profiles(&profiles, &HashSet::new(), 5),
            [profiles[1].clone()]
        );
        profiles[0].trashed = false;
        assert_eq!(
            recent_profiles(&profiles, &HashSet::new(), 5)[0].id,
            profiles[0].id
        );
        assert!(recent_profiles(&[], &HashSet::new(), 5).is_empty());
    }

    #[test]
    fn windows_removed_links_are_matched_by_profile_id_not_display_name() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store(dir.path().into());
        native::with_com(move || {
            let collection: IObjectCollection = unsafe {
                CoCreateInstance(&EnumerableObjectCollection, None, CLSCTX_INPROC_SERVER)?
            };
            let mut old = profile(1, 1, false);
            old.name = "Old name".into();
            let link = native::profile_link(&old, &store.icon_path(old.id))?;
            unsafe { collection.AddObject(&link)? };
            let array: IObjectArray = collection.cast()?;
            let removed = removed_arguments(&array)?;
            old.name = "New name".into();
            assert!(recent_profiles(&[old], &removed, 5).is_empty());
            Ok(())
        })
    }

    #[test]
    #[ignore = "requires Windows Shell recent items; creates and removes an isolated test Jump List"]
    fn windows_shell_commits_and_clears_recent_profiles() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store(dir.path().into());
        let id = store.save(None, "Jump List smoke test", "", 0, 0, false)?;
        store.update(|catalog| {
            catalog.profile(id)?.opened = 1;
            Ok(())
        })?;
        native::with_com(move || {
            let app_id = HSTRING::from(format!("Qxsch.Switchboard.Test{}", uuid::Uuid::new_v4()));
            let list: ICustomDestinationList =
                unsafe { CoCreateInstance(&DestinationList, None, CLSCTX_INPROC_SERVER)? };
            unsafe { list.SetAppID(&app_id)? };
            let result = (|| {
                rebuild(&list, &store, &store.read()?.profiles)?;
                store.trash(id, true)?;
                rebuild(&list, &store, &store.read()?.profiles)?;
                Ok(())
            })();
            unsafe { list.DeleteList(&app_id)? };
            result
        })
    }
}
