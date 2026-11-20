use crate::{
    Entry, Library, Swatch, SymbolTile, icons, native,
    store::{self, Profile, Store},
    temporary,
};
use anyhow::{Context, Result, ensure};
use slint::{
    Color, ComponentHandle, Image, Model, ModelRc, SharedString, Timer, TimerMode, VecModel,
};
use std::{
    cell::RefCell,
    collections::{BTreeSet, HashMap, HashSet},
    path::PathBuf,
    rc::Rc,
    time::Duration,
};
use uuid::Uuid;

fn model<T: Clone + 'static>(rows: Vec<T>) -> ModelRc<T> {
    Rc::new(VecModel::from(rows)).into()
}
fn tone(i: usize) -> Color {
    let (_, hex) = icons::COLORS[i];
    Color::from_argb_encoded(0xff000000 | u32::from_str_radix(&hex[1..], 16).expect("palette RGB"))
}
fn relative(when: u64) -> String {
    if when == 0 {
        return "Not opened".into();
    }
    match store::now().saturating_sub(when) / 60 {
        0 => "Just now".into(),
        n @ 1..60 => format!("{n} min ago"),
        n @ 60..1440 => format!("{} h ago", n / 60),
        n => format!("{} d ago", n / 1440),
    }
}
fn open_worker(id: &str) -> Result<()> {
    let id = Uuid::parse_str(id)?;
    std::process::Command::new(std::env::current_exe()?)
        .args(["--open", &id.to_string()])
        .spawn()?;
    Ok(())
}
#[derive(Default)]
struct State {
    selected: HashSet<Uuid>,
    anchor: Option<Uuid>,
    images: HashMap<(usize, usize), Image>,
    destination: PathBuf,
    temps: HashSet<Uuid>,
    previous: Option<Snapshot>,
}
/// Last rendered catalog, profile activity, temporary activity, and minute.
type Snapshot = (Vec<Profile>, Vec<bool>, Vec<(Uuid, bool)>, u64);
impl State {
    fn image(&mut self, symbol: usize, color: usize) -> Result<Image> {
        if let Some(img) = self.images.get(&(symbol, color)) {
            return Ok(img.clone());
        }
        let img = icons::preview(symbol, color)?;
        self.images.insert((symbol, color), img.clone());
        Ok(img)
    }
    fn entry(&mut self, p: &Profile, live: bool) -> Result<Entry> {
        Ok(Entry {
            id: p.id.to_string().into(),
            name: p.name.as_str().into(),
            group: if p.collection.is_empty() {
                "Unfiled"
            } else {
                &p.collection
            }
            .into(),
            icon: self.image(p.symbol, p.color)?,
            tint: tone(p.color),
            live,
            recent: relative(p.opened).into(),
            selected: self.selected.contains(&p.id),
            temporary: false,
        })
    }
    fn temp_entry(&mut self, t: &temporary::Session) -> Result<Entry> {
        let symbol = icons::SYMBOLS
            .iter()
            .position(|(n, _)| *n == "Hourglass")
            .unwrap_or(0);
        let color = icons::COLORS
            .iter()
            .position(|(n, _)| *n == "Slate")
            .unwrap_or(0);
        Ok(Entry {
            id: t.id.to_string().into(),
            name: "Temporary session".into(),
            group: if t.running {
                "Ad hoc  ·  erased when Edge exits"
            } else {
                "Ad hoc  ·  cleanup pending"
            }
            .into(),
            icon: self.image(symbol, color)?,
            tint: tone(color),
            live: t.running,
            recent: relative(t.started).into(),
            selected: false,
            temporary: true,
        })
    }
    fn refresh(&mut self, ui: &Library, s: &Store, force: bool) -> Result<()> {
        let mut ps = s.read()?.profiles;
        let by_name = ui.get_by_name();
        ps.sort_by(|a, b| {
            (if by_name {
                std::cmp::Ordering::Equal
            } else {
                b.opened.cmp(&a.opened)
            })
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then(a.id.cmp(&b.id))
        });
        let status: Vec<_> = ps
            .iter()
            .map(|p| native::running(p.id))
            .collect::<Result<_>>()?;
        let temps = temporary::sessions(s)?;
        let temp_status: Vec<_> = temps.iter().map(|t| (t.id, t.running)).collect();
        self.temps = temps.iter().map(|t| t.id).collect();
        let minute = store::now() / 60;
        if !force
            && self.previous.as_ref().is_some_and(|(p, l, t, m)| {
                p == &ps && l == &status && t == &temp_status && *m == minute
            })
        {
            return Ok(());
        }
        let sec = ui.get_section();
        let group = ui.get_group_filter();
        let query = ui.get_search().trim().to_lowercase();
        let mut groups = BTreeSet::<String>::new();
        let (mut rows, mut pinned) = (vec![], vec![]);
        let (mut total, mut live, mut trash) = (0, 0, 0);
        for t in &temps {
            if t.running {
                live += 1;
            } else {
                trash += 1;
            }
            if (sec == "Running" && t.running) || (sec == "Trash" && !t.running) {
                let e = self.temp_entry(t)?;
                if e.name.to_lowercase().contains(&query) || e.group.to_lowercase().contains(&query)
                {
                    rows.push(e);
                }
            }
        }
        for (p, &active) in ps.iter().zip(&status) {
            if p.trashed {
                trash += 1;
            } else {
                total += 1;
                if active {
                    live += 1;
                }
                if !p.collection.is_empty() {
                    groups.insert(p.collection.clone());
                }
                if p.pinned && pinned.len() < 3 {
                    pinned.push(self.entry(p, active)?);
                }
            }
            if p.trashed != (sec == "Trash")
                || (sec == "Running" && !active)
                || (sec == "Collection" && p.collection != group.as_str())
            {
                continue;
            }
            if !p.name.to_lowercase().contains(&query)
                && !p.collection.to_lowercase().contains(&query)
            {
                continue;
            }
            rows.push(self.entry(p, active)?);
        }
        self.selected
            .retain(|id| rows.iter().any(|r| r.id.as_str() == id.to_string()));
        ui.set_groups(model(groups.into_iter().map(SharedString::from).collect()));
        ui.set_pinned(model(pinned));
        ui.set_entries(model(rows));
        ui.set_total(total);
        ui.set_live_count(live);
        ui.set_trash_count(trash);
        ui.set_marked(self.selected.len() as i32);
        self.previous = Some((ps, status, temp_status, minute));
        Ok(())
    }
    fn symbols(&mut self, ui: &Library) -> Result<()> {
        let q = ui.get_symbol_search().trim().to_lowercase();
        let color = ui.get_edit_color() as usize;
        let mut tiles = vec![];
        for (i, &(name, _)) in icons::SYMBOLS.iter().enumerate() {
            if name.to_lowercase().contains(&q) {
                tiles.push(SymbolTile {
                    index: i as i32,
                    name: name.into(),
                    art: self.image(i, color)?,
                });
            }
        }
        ui.set_symbols(model(tiles));
        self.preview(ui)
    }
    fn preview(&mut self, ui: &Library) -> Result<()> {
        let index = ui.get_edit_symbol() as usize;
        ui.set_preview(self.image(index, ui.get_edit_color() as usize)?);
        ui.set_preview_name(icons::SYMBOLS[index].0.into());
        Ok(())
    }
    fn set_folder(&mut self, ui: &Library, path: PathBuf) {
        ui.set_folder(path.to_string_lossy().as_ref().into());
        self.destination = path;
    }
}

fn build_library(store: Store) -> Result<(Library, Timer)> {
    let ui = Library::new()?;
    let state = Rc::new(RefCell::new(State::default()));
    ui.set_palette(model(
        icons::COLORS
            .iter()
            .enumerate()
            .map(|(i, (name, _))| Swatch {
                name: (*name).into(),
                tone: tone(i),
            })
            .collect(),
    ));
    ui.set_section("All profiles".into());
    let recovery = temporary::recover(&store);
    state.borrow_mut().refresh(&ui, &store, true)?;
    let sessions = temporary::sessions(&store)?;
    let open = sessions.iter().filter(|t| t.running).count();
    let pending = sessions.len() - open;
    let mut notes = vec![];
    if open > 0 {
        notes.push(format!(
            "{open} temporary session(s) still running — see Running."
        ));
    }
    if pending > 0 {
        notes.push(format!(
            "{pending} temporary session(s) could not be erased automatically — see Trash."
        ));
    }
    if let Err(e) = recovery {
        notes.push(format!("Temporary cleanup deferred: {e:#}"));
    }
    ui.set_notice(notes.join("   ").into());
    // Every callback reports errors in its current view, without dismissing an unfinished form.
    fn report(ui: &Library, action: impl FnOnce() -> Result<()>) {
        if let Err(e) = action() {
            ui.set_notice(format!("{e:#}").into());
        }
    }
    macro_rules! bind {
        ($on:ident, |$u:ident,$st:ident,$s:ident $(,$arg:ident)*| $body:block) => {{
            let weak = ui.as_weak();
            let state = state.clone();
            let store = store.clone();
            ui.$on(move |$($arg),*| {
                if let Some($u) = weak.upgrade() {
                    let mut borrow = state.borrow_mut();
                    let $st = &mut *borrow;
                    let $s = &store;
                    report(&$u, || $body);
                }
            });
        }};
    }
    bind!(on_refresh, |u, st, s| { st.refresh(&u, s, true) });
    bind!(on_filter, |u, st, s, section, group| {
        u.set_page(0);
        u.set_search("".into());
        u.set_notice("".into());
        u.set_section(section);
        u.set_group_filter(group);
        st.selected.clear();
        st.refresh(&u, s, true)
    });
    bind!(on_toggle_sort, |u, st, s| {
        u.set_by_name(!u.get_by_name());
        st.refresh(&u, s, true)
    });
    bind!(on_toggle_selection, |u, st, s, id| {
        let id = Uuid::parse_str(&id)?;
        st.anchor = Some(id);
        if !st.selected.remove(&id) {
            st.selected.insert(id);
        }
        st.refresh(&u, s, true)
    });
    bind!(on_select_row, |u, st, s, id, additive, range| {
        let id = Uuid::parse_str(&id)?;
        let ids = u
            .get_entries()
            .iter()
            .map(|e| Uuid::parse_str(&e.id))
            .collect::<Result<Vec<_>, _>>()?;
        if !additive {
            st.selected.clear();
        }
        if range
            && let Some(a) = ids.iter().position(|v| Some(*v) == st.anchor)
            && let Some(b) = ids.iter().position(|v| *v == id)
        {
            st.selected.extend(&ids[a.min(b)..=a.max(b)]);
        } else {
            if !additive || !st.selected.remove(&id) {
                st.selected.insert(id);
            }
            st.anchor = Some(id);
        }
        st.refresh(&u, s, true)
    });
    bind!(on_edit, |u, st, s, id| {
        let mut c = s.read()?;
        let fresh = Profile {
            id: Uuid::nil(),
            name: "".into(),
            collection: u.get_group_filter().to_string(),
            symbol: c.profiles.len() % icons::SYMBOLS.len(),
            color: c.profiles.len() % icons::COLORS.len(),
            pinned: false,
            trashed: false,
            opened: 0,
            shortcuts: vec![],
        };
        let p = if id.is_empty() {
            &fresh
        } else {
            c.profile(Uuid::parse_str(&id)?)?
        };
        u.set_editor_id(id);
        u.set_edit_name(p.name.as_str().into());
        u.set_edit_group(p.collection.as_str().into());
        u.set_edit_pin(p.pinned);
        u.set_edit_symbol(p.symbol as i32);
        u.set_edit_color(p.color as i32);
        u.set_symbol_search("".into());
        st.symbols(&u)?;
        u.set_notice("".into());
        u.set_page(1);
        Ok(())
    });
    bind!(on_filter_symbols, |u, st, _s| { st.symbols(&u) });
    bind!(on_choose_color, |u, st, _s, i| {
        u.set_edit_color(i);
        st.symbols(&u)
    });
    bind!(on_choose_symbol, |u, st, _s, i| {
        u.set_edit_symbol(i);
        st.preview(&u)
    });
    bind!(on_save, |u, st, s| {
        let text = u.get_editor_id();
        let id = if text.is_empty() {
            None
        } else {
            Some(Uuid::parse_str(&text)?)
        };
        let saved = s.save(
            id,
            &u.get_edit_name(),
            &u.get_edit_group(),
            u.get_edit_symbol() as usize,
            u.get_edit_color() as usize,
            u.get_edit_pin(),
        )?;
        st.selected.clear();
        st.selected.insert(saved);
        u.set_search("".into());
        u.set_section("All profiles".into());
        u.set_group_filter("".into());
        u.set_page(0);
        u.set_notice(format!("Saved {}.", u.get_edit_name().trim()).into());
        st.refresh(&u, s, true)
    });
    bind!(on_cancel, |u, st, s| {
        u.set_page(0);
        u.set_notice("".into());
        st.refresh(&u, s, true)
    });
    bind!(on_launch, |u, st, _s, id| {
        let uuid = Uuid::parse_str(&id)?;
        if st.temps.contains(&uuid) {
            ensure!(
                native::focus(uuid)?,
                "Could not find a window for this temporary session."
            );
            return Ok(());
        }
        open_worker(&id)?;
        u.set_notice("Opening profile…".into());
        Ok(())
    });
    bind!(on_launch_first, |u, st, s| {
        if u.get_section() != "Trash"
            && let Some(e) = u.get_entries().row_data(0)
        {
            open_worker(&e.id)?;
            st.refresh(&u, s, true)?;
        }
        Ok(())
    });
    bind!(on_trash, |u, st, s, id| {
        let uuid = Uuid::parse_str(&id)?;
        if st.temps.contains(&uuid) {
            if native::running(uuid)?
                && rfd::MessageDialog::new()
                    .set_title("End temporary session?")
                    .set_description(
                        "All Edge windows of this temporary session will close and its data will be erased.",
                    )
                    .set_buttons(rfd::MessageButtons::YesNo)
                    .show()
                    != rfd::MessageDialogResult::Yes
            {
                return Ok(());
            }
            temporary::discard(s, uuid)?;
            u.set_notice("Temporary session erased.".into());
            return st.refresh(&u, s, true);
        }
        let trash = u.get_section() != "Trash";
        let links = s.trash(uuid, trash)?;
        u.set_notice(
            match (trash, links) {
                (true, 0) => {
                    "Moved to Trash. You can restore this profile and its data at any time.".into()
                }
                (true, n) => format!(
                    "Moved to Trash and removed {n} shortcut(s). Restoring brings them back."
                ),
                (false, 0) => "Profile restored.".into(),
                (false, n) => format!("Profile restored with {n} shortcut(s)."),
            }
            .into(),
        );
        st.refresh(&u, s, true)
    });
    bind!(on_temporary, |u, _st, _s| {
        std::process::Command::new(std::env::current_exe()?)
            .arg("--temporary")
            .spawn()?;
        u.set_notice("Starting a fresh temporary Edge session…".into());
        Ok(())
    });
    bind!(on_prepare_export, |u, st, s| {
        let selected: Vec<_> = u.get_entries().iter().filter(|e| e.selected).collect();
        ensure!(!selected.is_empty(), "Select one or more profiles first.");
        let last = s.read()?.last_folder.filter(|p| p.is_dir());
        u.set_has_last(last.is_some());
        st.set_folder(
            &u,
            match last {
                Some(p) => p,
                None => native::desktop()?,
            },
        );
        u.set_export_list(model(selected));
        u.set_page(2);
        u.set_notice("".into());
        Ok(())
    });
    bind!(on_pick_folder, |u, st, s, mode| {
        let path = match mode.as_str() {
            "desktop" => Some(native::desktop()?),
            "last" => Some(s.read()?.last_folder.context("No previous folder.")?),
            _ => rfd::FileDialog::new()
                .set_title("Choose a folder for profile shortcuts")
                .set_directory(&st.destination)
                .pick_folder(),
        };
        if let Some(path) = path {
            st.set_folder(&u, path);
        }
        Ok(())
    });
    bind!(on_export_links, |u, st, s| {
        let ids = u
            .get_export_list()
            .iter()
            .map(|e| Uuid::parse_str(&e.id))
            .collect::<Result<Vec<_>, _>>()?;
        s.shortcuts(&ids, &st.destination)?;
        u.set_page(0);
        u.set_notice(
            format!(
                "Created {} shortcut(s) in {}.",
                ids.len(),
                st.destination.display()
            )
            .into(),
        );
        st.selected.clear();
        st.refresh(&u, s, true)
    });
    let timer = Timer::default();
    let weak = ui.as_weak();
    timer.start(TimerMode::Repeated, Duration::from_secs(3), move || {
        if let Some(u) = weak.upgrade()
            && u.get_page() == 0
            && let Ok(mut st) = state.try_borrow_mut()
            && let Err(e) = st.refresh(&u, &store, false)
        {
            u.set_notice(format!("{e:#}").into());
        }
    });
    Ok((ui, timer))
}
pub fn run(store: Store) -> Result<()> {
    let (ui, _timer) = build_library(store)?;
    Ok(ui.run()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::platform::{
        Platform, WindowAdapter,
        software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
    };

    #[test]
    fn profile_studio_library_and_export_share_the_same_selection() -> Result<()> {
        struct TestPlatform(Rc<MinimalSoftwareWindow>);
        impl Platform for TestPlatform {
            fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
                Ok(self.0.clone())
            }
        }
        let win = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(TestPlatform(win.clone()))).unwrap();
        let dir = tempfile::tempdir()?;
        let s = Store(dir.path().into());
        let folder = dir.path().join("Desktop customers");
        std::fs::create_dir(&folder)?;
        s.update(|c| {
            c.last_folder = Some(folder.clone());
            Ok(())
        })?;
        let (u, _timer) = build_library(s.clone())?;
        win.set_size(slint::PhysicalSize::new(1220, 770));
        u.show()?;
        let capture = |name: &str| -> Result<()> {
            let shot = u.window().take_snapshot()?;
            if let Some(folder) = std::env::var_os("SWITCHBOARD_TEST_SNAPSHOTS") {
                image::save_buffer(
                    PathBuf::from(folder).join(format!("{name}.png")),
                    shot.as_bytes(),
                    shot.width(),
                    shot.height(),
                    image::ColorType::Rgba8,
                )?;
            }
            Ok(())
        };
        u.invoke_edit("".into());
        u.set_edit_name("Lab-M365".into());
        u.set_edit_group("Labs".into());
        u.set_edit_pin(true);
        u.invoke_choose_symbol(1);
        u.invoke_choose_color(1);
        assert_eq!(u.get_symbols().row_count(), 100);
        capture("profile-studio")?;
        u.set_symbol_search("spark".into());
        u.invoke_filter_symbols();
        assert_eq!(u.get_symbols().row_data(0).unwrap().index, 99);
        u.invoke_save();
        assert_eq!(u.get_page(), 0);
        for i in 1..=57 {
            s.save(
                None,
                &format!("Customer{i:02}"),
                if i % 3 == 0 { "Internal" } else { "Customers" },
                (i + 99) % 100,
                i % 6,
                i < 3,
            )?;
        }
        u.invoke_refresh();
        assert_eq!(u.get_entries().row_count(), 58);
        capture("profile-library-live")?;
        u.set_search("lab".into());
        u.invoke_refresh();
        assert_eq!(u.get_entries().row_count(), 1);
        let id = u.get_entries().row_data(0).unwrap().id;
        u.invoke_prepare_export();
        assert_eq!(u.get_page(), 2);
        capture("shortcut-export-live")?;
        u.invoke_export_links();
        let link = folder.join("Lab-M365.lnk");
        assert!(link.exists());
        u.invoke_trash(id.clone());
        assert!(!link.exists(), "trash removes tracked shortcuts");
        u.invoke_filter("Trash".into(), "".into());
        assert_eq!(u.get_entries().row_count(), 1);
        u.invoke_trash(id);
        assert_eq!(u.get_trash_count(), 0);
        assert!(link.exists(), "restore recreates tracked shortcuts");
        let leftover = Uuid::new_v4();
        let dir = s.0.join("Temporary").join(leftover.to_string());
        std::fs::create_dir_all(dir.join("Data"))?;
        std::fs::write(dir.join(".switchboard-session"), leftover.to_string())?;
        u.invoke_refresh();
        assert_eq!(u.get_trash_count(), 1);
        let row = u.get_entries().row_data(0).unwrap();
        assert!(row.temporary && !row.live);
        u.invoke_trash(row.id);
        assert!(!dir.exists());
        assert_eq!(u.get_trash_count(), 0);
        Ok(())
    }
}
