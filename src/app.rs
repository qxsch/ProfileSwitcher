//! The library window. The UI thread only renders cached data and queues work: every catalog,
//! filesystem, Job Object, and Edge call runs on one background worker, so the window stays
//! responsive however long an action takes.
use crate::{
    ActivityRow, ActivityScope, Entry, Library, Swatch, SymbolTile, icons, native,
    store::{self, Profile, Store},
    temporary,
};
use anyhow::{Result, ensure};
use slint::{
    Color, ComponentHandle, Image, Model, ModelRc, SharedString, Timer, TimerMode, VecModel,
};
use std::{
    cell::RefCell,
    collections::{BTreeSet, HashMap, HashSet},
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    rc::Rc,
    sync::{Arc, Mutex, mpsc},
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
fn open_worker(id: Uuid) -> Result<()> {
    std::process::Command::new(std::env::current_exe()?)
        .args(["--open", &id.to_string()])
        .spawn()?;
    Ok(())
}
#[derive(Clone, PartialEq)]
struct ActivityEvent {
    at: chrono::DateTime<chrono::Local>,
    action: String,
    status: ActivityScope,
    detail: String,
}
impl ActivityEvent {
    fn new(action: &str, status: ActivityScope, detail: &str) -> Self {
        Self {
            at: chrono::Local::now(),
            action: action.into(),
            status,
            detail: detail.into(),
        }
    }
}
fn push_activity(activity: &mut Vec<ActivityEvent>, event: ActivityEvent) {
    activity.push(event);
    if activity.len() > 30 {
        activity.drain(..activity.len() - 30);
    }
}
fn append_activity(store: &Store, event: &ActivityEvent, message: &str) -> Result<()> {
    // On a fresh install, startup tasks log before any action has created the data folder.
    fs::create_dir_all(&store.0)?;
    let path = store.0.join("switchboard.log");
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    serde_json::to_writer(
        &mut file,
        &serde_json::json!({"time": event.at.timestamp(), "event": format!("{} · {message}", event.action)}),
    )?;
    writeln!(file)?;
    file.flush()?;
    Ok(())
}

/// Everything the library renders, gathered on the worker.
#[derive(Clone, Default, PartialEq)]
struct Data {
    profiles: Vec<Profile>,
    live: Vec<bool>,
    temps: Vec<temporary::Session>,
    last_folder: Option<PathBuf>,
}
fn load(s: &Store) -> Result<Data> {
    let c = s.read()?;
    let live = c
        .profiles
        .iter()
        .map(|p| native::running(p.id))
        .collect::<Result<_>>()?;
    Ok(Data {
        live,
        temps: temporary::sessions(s)?,
        last_folder: c.last_folder.filter(|p| p.is_dir()),
        profiles: c.profiles,
    })
}

/// What the UI should do after a task succeeds.
#[derive(Default)]
struct Outcome {
    notice: String,
    /// Return to the full library view.
    home: bool,
    /// Replace the selection.
    select: Option<Vec<Uuid>>,
}
fn said(notice: impl Into<String>) -> Outcome {
    Outcome {
        notice: notice.into(),
        ..Default::default()
    }
}
type Task = Box<dyn FnOnce(&Store, &Progress) -> Result<Outcome> + Send>;

struct TaskSpec {
    id: Option<Uuid>,
    kind: String,
    label: String,
    run: Task,
}
#[derive(Clone, Default, PartialEq)]
struct ActionState {
    kind: String,
    label: String,
    busy: bool,
    error: String,
}
struct Progress {
    store: Store,
    mail: Arc<Mutex<Mailbox>>,
    weak: slint::Weak<Library>,
    id: Option<Uuid>,
    kind: String,
}
impl Progress {
    fn phase(&self, label: &str) -> Result<()> {
        let event = ActivityEvent::new(&self.kind, ActivityScope::Progress, label);
        let log_error = append_activity(&self.store, &event, label).err();
        {
            let mut mail = self.mail.lock().unwrap();
            push_activity(&mut mail.activity, event);
            if let Some(error) = log_error {
                mail.last_error = Some(format!("Could not write switchboard.log: {error:#}"));
            }
            if let Some(id) = self.id {
                let state = mail.actions.entry(id).or_default();
                state.label = label.into();
                state.busy = true;
            }
        }
        let _ = self.weak.upgrade_in_event_loop(|ui| ui.invoke_deliver());
        Ok(())
    }
}

#[derive(Default)]
struct Mailbox {
    data: Option<Result<Data, String>>,
    outcomes: Vec<Result<Outcome, String>>,
    pending: usize,
    polling: bool,
    actions: HashMap<Uuid, ActionState>,
    activity: Vec<ActivityEvent>,
    last_error: Option<String>,
}
struct Worker {
    tx: mpsc::Sender<Option<TaskSpec>>,
    mail: Arc<Mutex<Mailbox>>,
}
impl Worker {
    fn spawn(store: Store, weak: slint::Weak<Library>) -> Self {
        let (tx, rx) = mpsc::channel::<Option<TaskSpec>>();
        let mail = Arc::new(Mutex::new(Mailbox::default()));
        let shared = mail.clone();
        std::thread::spawn(move || {
            // Tasks run one at a time, so actions never race each other on the catalog.
            while let Ok(task) = rx.recv() {
                let outcome = task.map(|task| {
                    let TaskSpec {
                        id,
                        kind,
                        label,
                        run,
                    } = task;
                    let progress = Progress {
                        store: store.clone(),
                        mail: shared.clone(),
                        weak: weak.clone(),
                        id,
                        kind: label.clone(),
                    };
                    let started = progress.phase("Starting…");
                    let result = started
                        .and_then(|()| run(&store, &progress))
                        .map_err(|e| format!("{e:#}"));
                    let (event, log_status) = match &result {
                        Ok(outcome) => (
                            ActivityEvent::new(&label, ActivityScope::Completed, &outcome.notice),
                            "completed",
                        ),
                        Err(error) => (
                            ActivityEvent::new(&label, ActivityScope::Failed, error),
                            "failed",
                        ),
                    };
                    let log_error = append_activity(&store, &event, log_status)
                        .err()
                        .map(|e| format!("{e:#}"));
                    let mut m = shared.lock().unwrap();
                    push_activity(&mut m.activity, event);
                    if let Some(id) = id {
                        if let Err(error) = &result {
                            let state = m.actions.entry(id).or_insert_with(|| ActionState {
                                kind: kind.clone(),
                                label: label.clone(),
                                busy: false,
                                error: String::new(),
                            });
                            state.busy = false;
                            state.error = error.clone();
                        } else {
                            m.actions.remove(&id);
                        }
                    }
                    if let Err(error) = &result {
                        m.last_error = Some(error.clone());
                    } else if let Some(error) = log_error {
                        m.last_error = Some(format!(
                            "Action completed, but its log could not be written: {error}"
                        ));
                    }
                    result
                });
                let data = load(&store).map_err(|e| format!("{e:#}"));
                {
                    let mut m = shared.lock().unwrap();
                    if let Err(error) = &data {
                        m.last_error = Some(error.clone());
                    }
                    if let Some(o) = outcome {
                        m.outcomes.push(o);
                        m.pending = m.pending.saturating_sub(1);
                    } else {
                        m.polling = false;
                    }
                    m.data = Some(data);
                }
                let _ = weak.upgrade_in_event_loop(|u| u.invoke_deliver());
            }
        });
        Self { tx, mail }
    }
    fn queue_named(
        &self,
        ui: &Library,
        label: &str,
        task: impl FnOnce(&Store) -> Result<Outcome> + Send + 'static,
    ) {
        self.queue_spec(
            ui,
            TaskSpec {
                id: None,
                kind: "action".into(),
                label: label.into(),
                run: Box::new(move |store, _| task(store)),
            },
        );
    }
    fn queue_action(
        &self,
        ui: &Library,
        id: Uuid,
        kind: &str,
        label: &str,
        task: impl FnOnce(&Store, &Progress) -> Result<Outcome> + Send + 'static,
    ) {
        self.queue_spec(
            ui,
            TaskSpec {
                id: Some(id),
                kind: kind.into(),
                label: label.into(),
                run: Box::new(task),
            },
        );
    }
    fn queue_spec(&self, ui: &Library, task: TaskSpec) {
        let mut mail = self.mail.lock().unwrap();
        if task
            .id
            .is_some_and(|id| mail.actions.get(&id).is_some_and(|action| action.busy))
        {
            return;
        }
        mail.pending += 1;
        if let Some(id) = task.id {
            mail.actions.insert(
                id,
                ActionState {
                    kind: task.kind.clone(),
                    label: task.label.clone(),
                    busy: true,
                    error: String::new(),
                },
            );
        }
        ui.set_busy(true);
        if let Err(error) = self.tx.send(Some(task)) {
            let Some(TaskSpec { id, label, .. }) = error.0 else {
                unreachable!("only queued tasks are sent to the worker");
            };
            mail.pending = mail.pending.saturating_sub(1);
            let message = format!("Background worker stopped before {label} could start.");
            mail.last_error = Some(message.clone());
            mail.outcomes.push(Err(message));
            if let Some(id) = id {
                mail.actions.remove(&id);
            }
        }
        drop(mail);
    }
    fn poll(&self) {
        let mut m = self.mail.lock().unwrap();
        if !m.polling && m.pending == 0 {
            m.polling = true;
            if self.tx.send(None).is_err() {
                m.polling = false;
                m.last_error =
                    Some("Background worker stopped; refresh or restart Switchboard.".into());
            }
        }
    }
}

#[derive(Default)]
struct State {
    data: Data,
    activity: Vec<ActivityEvent>,
    selected: HashSet<Uuid>,
    anchor: Option<Uuid>,
    images: HashMap<(usize, usize), Image>,
    destination: PathBuf,
    actions: HashMap<Uuid, ActionState>,
    confirm: Option<TaskSpec>,
    rendered: Option<(Data, u64)>,
}
impl State {
    fn image(&mut self, symbol: usize, color: usize) -> Result<Image> {
        if let Some(img) = self.images.get(&(symbol, color)) {
            return Ok(img.clone());
        }
        let img = icons::preview(symbol, color)?;
        self.images.insert((symbol, color), img.clone());
        Ok(img)
    }
    fn temp(&self, id: Uuid) -> Option<&temporary::Session> {
        self.data.temps.iter().find(|t| t.id == id)
    }
    fn decorate(&self, entry: &mut Entry) {
        if let Ok(id) = Uuid::parse_str(&entry.id)
            && let Some(action) = self.actions.get(&id)
        {
            entry.action_label = action.label.as_str().into();
            entry.action_kind = action.kind.as_str().into();
            entry.action_busy = action.busy;
            entry.action_error = action.error.as_str().into();
        }
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
            action_label: "".into(),
            action_kind: "".into(),
            action_busy: false,
            action_error: "".into(),
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
            action_label: "".into(),
            action_kind: "".into(),
            action_busy: false,
            action_error: "".into(),
        })
    }
    /// Rebuilds the visible models from cached data; never touches the disk.
    fn render(&mut self, ui: &Library, force: bool) -> Result<()> {
        let minute = store::now() / 60;
        if !force
            && self
                .rendered
                .as_ref()
                .is_some_and(|(d, m)| d == &self.data && *m == minute)
        {
            return Ok(());
        }
        let data = self.data.clone();
        let by_name = ui.get_by_name();
        let mut order: Vec<_> = (0..data.profiles.len()).collect();
        order.sort_by(|&a, &b| {
            let (a, b) = (&data.profiles[a], &data.profiles[b]);
            (if by_name {
                std::cmp::Ordering::Equal
            } else {
                b.opened.cmp(&a.opened)
            })
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then(a.id.cmp(&b.id))
        });
        let sec = ui.get_section();
        let group = ui.get_group_filter();
        let query = ui.get_search().trim().to_lowercase();
        let mut groups = BTreeSet::<String>::new();
        let (mut rows, mut pinned) = (vec![], vec![]);
        let (mut total, mut live, mut trash) = (0, 0, 0);
        for t in &data.temps {
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
        for i in order {
            let (p, active) = (&data.profiles[i], data.live[i]);
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
        for r in &mut rows {
            r.selected = Uuid::parse_str(&r.id).is_ok_and(|id| self.selected.contains(&id));
            self.decorate(r);
        }
        for r in &mut pinned {
            self.decorate(r);
        }
        ui.set_groups(model(groups.into_iter().map(SharedString::from).collect()));
        ui.set_pinned(model(pinned));
        ui.set_entries(model(rows));
        ui.set_total(total);
        ui.set_live_count(live);
        ui.set_trash_count(trash);
        ui.set_marked(self.selected.len() as i32);
        self.rendered = Some((data, minute));
        Ok(())
    }
    fn deliver(&mut self, ui: &Library, w: &Worker) -> Result<()> {
        let (data, outcomes, pending, actions, activity, error) = {
            let mut m = w.mail.lock().unwrap();
            (
                m.data.take(),
                std::mem::take(&mut m.outcomes),
                m.pending,
                m.actions.clone(),
                m.activity.clone(),
                m.last_error.clone(),
            )
        };
        ui.set_busy(pending > 0);
        if self.activity != activity {
            self.activity = activity;
            if ui.get_activity_open() {
                self.render_activity(ui);
            }
        }
        if let Some(error) = error {
            ui.set_error(error.into());
        }
        let actions_changed = self.actions != actions;
        self.actions = actions;
        let mut force = actions_changed;
        for o in outcomes {
            match o {
                Ok(o) => {
                    if o.home {
                        ui.set_search("".into());
                        ui.set_section("All profiles".into());
                        ui.set_group_filter("".into());
                        ui.set_page(0);
                        force = true;
                    }
                    if let Some(ids) = o.select {
                        self.selected = ids.into_iter().collect();
                        force = true;
                    }
                    if !o.notice.is_empty() {
                        ui.set_notice(o.notice.into());
                    }
                }
                Err(e) => ui.set_notice(e.into()),
            }
        }
        match data {
            Some(Ok(d)) => {
                self.data = d;
                self.render(ui, force)
            }
            Some(Err(e)) => {
                ui.set_notice(e.clone().into());
                ui.set_error(e.into());
                Ok(())
            }
            None => self.render(ui, force),
        }
    }
    fn render_activity(&self, ui: &Library) {
        let query = ui.get_activity_search().trim().to_lowercase();
        let scope = ui.get_activity_scope();
        let rows = self
            .activity
            .iter()
            .rev()
            .filter(|e| scope == ActivityScope::All || e.status == scope)
            .filter(|e| {
                query.is_empty()
                    || e.action.to_lowercase().contains(&query)
                    || e.detail.to_lowercase().contains(&query)
            })
            .map(|e| ActivityRow {
                clock: e.at.format("%H:%M:%S").to_string().into(),
                day: e.at.format("%b %d").to_string().into(),
                action: e.action.as_str().into(),
                detail: e.detail.as_str().into(),
                status: e.status,
            })
            .collect();
        ui.set_activity_total(self.activity.len() as i32);
        ui.set_activity_rows(model(rows));
    }
    fn ask(&mut self, ui: &Library, title: &str, text: &str, action: &str, task: TaskSpec) {
        ui.set_confirm_title(title.into());
        ui.set_confirm_text(text.into());
        ui.set_confirm_action(action.into());
        self.confirm = Some(task);
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

fn startup(s: &Store) -> Result<Outcome> {
    let recovery = temporary::recover(s);
    let sessions = temporary::sessions(s)?;
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
    Ok(said(notes.join("   ")))
}
/// Deletes every requested item while reporting each step and preserving partial-failure details.
fn purge(s: &Store, profiles: Vec<Uuid>, temps: Vec<Uuid>, progress: &Progress) -> Result<Outcome> {
    let items = profiles
        .into_iter()
        .map(|id| (id, false))
        .chain(temps.into_iter().map(|id| (id, true)))
        .collect::<Vec<_>>();
    let (mut done, mut failed) = (0, Vec::new());
    for (index, (id, temp)) in items.iter().copied().enumerate() {
        progress.phase(&format!("Deleting item {} of {}…", index + 1, items.len()))?;
        let result = if temp {
            temporary::discard_with_progress(s, id, |phase| progress.phase(phase))
        } else {
            s.delete(id)
        };
        match result {
            Ok(()) => done += 1,
            Err(error) => failed.push(format!("{id}: {error:#}")),
        }
    }
    if failed.is_empty() {
        Ok(said(format!("Permanently deleted {done} item(s).")))
    } else {
        anyhow::bail!(
            "Deleted {done} item(s); {} could not be deleted: {}",
            failed.len(),
            failed.join("; ")
        )
    }
}

fn build_library(store: Store) -> Result<(Library, Timer, Timer, Rc<Worker>)> {
    let ui = Library::new()?;
    let state = Rc::new(RefCell::new(State::default()));
    let worker = Rc::new(Worker::spawn(store.clone(), ui.as_weak()));
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
    worker.queue_named(&ui, "Checking temporary sessions", startup);
    fn report(ui: &Library, action: impl FnOnce() -> Result<()>) {
        if let Err(e) = action() {
            let error = format!("{e:#}");
            ui.set_notice(error.clone().into());
            ui.set_error(error.into());
        }
    }
    // Every callback reports errors in its current view, without dismissing an unfinished form.
    macro_rules! bind {
        ($on:ident, |$u:ident,$st:ident,$w:ident $(,$arg:ident)*| $body:block) => {{
            let weak = ui.as_weak();
            let state = state.clone();
            let worker = worker.clone();
            ui.$on(move |$($arg),*| {
                if let Some($u) = weak.upgrade()
                    && let Ok(mut borrow) = state.try_borrow_mut()
                {
                    let $st = &mut *borrow;
                    let $w = &*worker;
                    report(&$u, || $body);
                }
            });
        }};
    }
    bind!(on_deliver, |u, st, w| { st.deliver(&u, w) });
    bind!(on_reload, |u, _st, w| {
        w.queue_named(&u, "Refreshing profile library", |_| Ok(Outcome::default()));
        Ok(())
    });
    bind!(on_refresh, |u, st, _w| { st.render(&u, true) });
    bind!(on_filter, |u, st, _w, section, group| {
        u.set_page(0);
        u.set_search("".into());
        u.set_notice("".into());
        u.set_section(section);
        u.set_group_filter(group);
        st.selected.clear();
        st.render(&u, true)
    });
    bind!(on_toggle_sort, |u, st, _w| {
        u.set_by_name(!u.get_by_name());
        st.render(&u, true)
    });
    bind!(on_toggle_selection, |u, st, _w, id| {
        let id = Uuid::parse_str(&id)?;
        st.anchor = Some(id);
        if !st.selected.remove(&id) {
            st.selected.insert(id);
        }
        st.render(&u, true)
    });
    bind!(on_select_row, |u, st, _w, id, additive, range| {
        let id = Uuid::parse_str(&id)?;
        let ids = u
            .get_entries()
            .iter()
            .filter(|e| !e.temporary)
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
        st.render(&u, true)
    });
    bind!(on_edit, |u, st, _w, id| {
        let n = st.data.profiles.len();
        let p = if id.is_empty() {
            Profile {
                id: Uuid::nil(),
                name: "".into(),
                collection: u.get_group_filter().to_string(),
                symbol: n % icons::SYMBOLS.len(),
                color: n % icons::COLORS.len(),
                pinned: false,
                trashed: false,
                opened: 0,
                shortcuts: vec![],
            }
        } else {
            let id = Uuid::parse_str(&id)?;
            st.data
                .profiles
                .iter()
                .find(|p| p.id == id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("This profile is no longer in the library."))?
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
    bind!(on_filter_symbols, |u, st, _w| { st.symbols(&u) });
    bind!(on_choose_color, |u, st, _w, i| {
        u.set_edit_color(i);
        st.symbols(&u)
    });
    bind!(on_choose_symbol, |u, st, _w, i| {
        u.set_edit_symbol(i);
        st.preview(&u)
    });
    bind!(on_save, |u, _st, w| {
        let text = u.get_editor_id();
        let id = if text.is_empty() {
            None
        } else {
            Some(Uuid::parse_str(&text)?)
        };
        let (name, group) = (
            u.get_edit_name().to_string(),
            u.get_edit_group().to_string(),
        );
        let (symbol, color, pin) = (
            u.get_edit_symbol() as usize,
            u.get_edit_color() as usize,
            u.get_edit_pin(),
        );
        w.queue_named(
            &u,
            if id.is_none() {
                "Creating profile"
            } else {
                "Saving profile"
            },
            move |s| {
                let saved = s.save(id, &name, &group, symbol, color, pin)?;
                Ok(Outcome {
                    notice: format!("Saved {}.", name.trim()),
                    home: true,
                    select: Some(vec![saved]),
                })
            },
        );
        Ok(())
    });
    bind!(on_cancel, |u, st, _w| {
        u.set_page(0);
        u.set_notice("".into());
        st.render(&u, true)
    });
    fn launch(u: &Library, st: &State, w: &Worker, id: Uuid) {
        if st.temp(id).is_some() {
            w.queue_action(
                u,
                id,
                "open",
                "Switching to temporary session",
                move |_, p| {
                    p.phase("Finding the temporary Edge window…")?;
                    ensure!(
                        native::focus(id)?,
                        "Could not find a window for this temporary session."
                    );
                    Ok(Outcome::default())
                },
            );
        } else {
            u.set_notice("Opening profile…".into());
            w.queue_action(u, id, "open", "Opening profile", move |_, p| {
                p.phase("Starting the isolated Edge profile…")?;
                open_worker(id).map(|_| Outcome::default())
            });
        }
    }
    bind!(on_launch, |u, st, w, id| {
        launch(&u, st, w, Uuid::parse_str(&id)?);
        Ok(())
    });
    bind!(on_launch_first, |u, st, w| {
        if u.get_section() != "Trash"
            && let Some(e) = u.get_entries().row_data(0)
        {
            launch(&u, st, w, Uuid::parse_str(&e.id)?);
        }
        Ok(())
    });
    bind!(on_trash, |u, st, w, id| {
        let id = Uuid::parse_str(&id)?;
        let retry = st
            .actions
            .get(&id)
            .is_some_and(|action| !action.busy && !action.error.is_empty());
        if let Some(t) = st.temp(id) {
            let running = t.running;
            let end = move |s: &Store, p: &Progress| {
                temporary::discard_with_progress(s, id, |phase| p.phase(phase))?;
                Ok(said("Temporary session ended and erased."))
            };
            if running && !retry {
                st.ask(
                    &u,
                    "End temporary session?",
                    "All Edge windows of this session will close and its browsing data will be erased.",
                    "End session",
                    TaskSpec {
                        id: Some(id),
                        kind: "end".into(),
                        label: "Ending temporary session".into(),
                        run: Box::new(end),
                    },
                );
            } else {
                w.queue_action(
                    &u,
                    id,
                    if running { "end" } else { "erase" },
                    if running {
                        "Ending temporary session"
                    } else {
                        "Erasing leftover session"
                    },
                    end,
                );
            }
            return Ok(());
        }
        let trash = u.get_section() != "Trash";
        w.queue_action(
            &u,
            id,
            if trash { "trash" } else { "restore" },
            if trash {
                "Moving profile to Trash"
            } else {
                "Restoring profile"
            },
            move |s, p| {
                p.phase(if trash {
                    "Removing tracked shortcuts…"
                } else {
                    "Restoring tracked shortcuts…"
                })?;
                let links = s.trash(id, trash)?;
                Ok(said(match (trash, links) {
                    (true, 0) => {
                        "Moved to Trash. You can restore this profile and its data at any time."
                            .into()
                    }
                    (true, n) => format!(
                        "Moved to Trash and removed {n} shortcut(s). Restoring brings them back."
                    ),
                    (false, 0) => "Profile restored.".into(),
                    (false, n) => format!("Profile restored with {n} shortcut(s)."),
                }))
            },
        );
        Ok(())
    });
    bind!(on_delete, |u, st, w, id| {
        let id = Uuid::parse_str(&id)?;
        if st.actions.get(&id).is_some_and(|action| {
            action.kind == "delete" && !action.busy && !action.error.is_empty()
        }) {
            w.queue_action(
                &u,
                id,
                "delete",
                "Deleting profile permanently",
                move |s, p| {
                    p.phase("Erasing browser data and tracked shortcuts…")?;
                    purge(s, vec![id], vec![], p)
                },
            );
            return Ok(());
        }
        let name = st
            .data
            .profiles
            .iter()
            .find(|p| p.id == id)
            .map_or_else(|| "this profile".into(), |p| p.name.clone());
        st.ask(
            &u,
            &format!("Delete {name} forever?"),
            "Its browser data — cookies, history, passwords, and sign-ins — is erased. This cannot be undone.",
            "Delete forever",
            TaskSpec {
                id: Some(id),
                kind: "delete".into(),
                label: "Deleting profile permanently".into(),
                run: Box::new(move |s, p| {
                    p.phase("Erasing browser data and tracked shortcuts…")?;
                    purge(s, vec![id], vec![], p)
                }),
            },
        );
        Ok(())
    });
    bind!(on_empty_trash, |u, st, _w| {
        let profiles: Vec<_> = st
            .data
            .profiles
            .iter()
            .filter(|p| p.trashed)
            .map(|p| p.id)
            .collect();
        let temps: Vec<_> = st
            .data
            .temps
            .iter()
            .filter(|t| !t.running)
            .map(|t| t.id)
            .collect();
        ensure!(
            !profiles.is_empty() || !temps.is_empty(),
            "Trash is already empty."
        );
        st.ask(
            &u,
            &format!("Empty Trash ({} item(s))?", profiles.len() + temps.len()),
            "All trashed profiles and their browser data are erased permanently. This cannot be undone.",
            "Empty Trash",
            TaskSpec {
                id: None,
                kind: "empty-trash".into(),
                label: "Emptying Trash".into(),
                run: Box::new(move |s, p| purge(s, profiles, temps, p)),
            },
        );
        Ok(())
    });
    bind!(on_confirm, |u, st, w, yes| {
        u.set_confirm_title("".into());
        if let Some(task) = st.confirm.take()
            && yes
        {
            w.queue_spec(&u, task);
        }
        Ok(())
    });
    bind!(on_temporary, |u, _st, w| {
        u.set_notice("Starting a fresh temporary Edge session…".into());
        w.queue_named(&u, "Starting temporary session", |_| {
            std::process::Command::new(std::env::current_exe()?)
                .arg("--temporary")
                .spawn()?;
            Ok(Outcome::default())
        });
        Ok(())
    });
    bind!(on_prepare_export, |u, st, _w| {
        let selected: Vec<_> = u.get_entries().iter().filter(|e| e.selected).collect();
        ensure!(!selected.is_empty(), "Select one or more profiles first.");
        let last = st.data.last_folder.clone();
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
    bind!(on_pick_folder, |u, st, _w, mode| {
        match mode.as_str() {
            "desktop" => st.set_folder(&u, native::desktop()?),
            "last" => {
                let last = st.data.last_folder.clone();
                st.set_folder(
                    &u,
                    last.ok_or_else(|| anyhow::anyhow!("No previous folder."))?,
                );
            }
            _ => {
                // The native picker runs on its own thread so the library keeps painting.
                let start = st.destination.clone();
                let weak = u.as_weak();
                std::thread::spawn(move || {
                    if let Some(path) = rfd::FileDialog::new()
                        .set_title("Choose a folder for profile shortcuts")
                        .set_directory(start)
                        .pick_folder()
                    {
                        let path = path.to_string_lossy().into_owned();
                        let _ = weak
                            .upgrade_in_event_loop(move |u| u.invoke_folder_chosen(path.into()));
                    }
                });
            }
        }
        Ok(())
    });
    bind!(on_folder_chosen, |u, st, _w, path| {
        st.set_folder(&u, PathBuf::from(path.as_str()));
        Ok(())
    });
    bind!(on_clear_error, |u, _st, w| {
        w.mail.lock().unwrap().last_error = None;
        u.set_error("".into());
        Ok(())
    });
    bind!(on_show_error, |u, _st, _w, text| {
        u.set_activity_open(false);
        u.set_error_details(text);
        Ok(())
    });
    bind!(on_show_activity, |u, st, _w| {
        u.set_error_details("".into());
        u.set_activity_scope(ActivityScope::All);
        u.set_activity_search("".into());
        st.render_activity(&u);
        u.set_activity_open(true);
        Ok(())
    });
    bind!(on_filter_activity, |u, st, _w| {
        st.render_activity(&u);
        Ok(())
    });
    bind!(on_close_panel, |u, _st, _w| {
        u.set_activity_open(false);
        u.set_error_details("".into());
        Ok(())
    });
    bind!(on_export_links, |u, st, w| {
        let ids = u
            .get_export_list()
            .iter()
            .map(|e| Uuid::parse_str(&e.id))
            .collect::<Result<Vec<_>, _>>()?;
        let folder = st.destination.clone();
        w.queue_named(&u, "Creating profile shortcuts", move |s| {
            s.shortcuts(&ids, &folder)?;
            Ok(Outcome {
                notice: format!("Created {} shortcut(s) in {}.", ids.len(), folder.display()),
                home: false,
                select: Some(vec![]),
            })
        });
        u.set_page(0);
        Ok(())
    });
    let timer = Timer::default();
    let weak = ui.as_weak();
    let poll_worker = worker.clone();
    timer.start(TimerMode::Repeated, Duration::from_secs(2), move || {
        if let Some(u) = weak.upgrade() {
            poll_worker.poll();
            u.invoke_deliver();
        }
    });
    let spinner_timer = Timer::default();
    let weak = ui.as_weak();
    spinner_timer.start(TimerMode::Repeated, Duration::from_millis(100), move || {
        if let Some(ui) = weak.upgrade()
            && ui.get_busy()
        {
            ui.set_spinner_frame((ui.get_spinner_frame() + 1) % 4);
        }
    });
    Ok((ui, timer, spinner_timer, worker))
}
pub fn run(store: Store) -> Result<()> {
    let (ui, _timer, _spinner_timer, _worker) = build_library(store)?;
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
    fn activity_log_is_written_before_the_data_folder_exists() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let s = Store(dir.path().join("EdgeProfileSwitcher"));
        let event = ActivityEvent::new("Startup check", ActivityScope::Progress, "Starting…");
        append_activity(&s, &event, "Starting…")?;
        let log = fs::read_to_string(s.0.join("switchboard.log"))?;
        assert!(log.contains("Startup check · Starting…"));
        Ok(())
    }
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
        let (u, _timer, _spinner_timer, worker) = build_library(s.clone())?;
        // No event loop runs here, so pull worker results the way the timer would.
        let settle = || {
            for _ in 0..1000 {
                std::thread::sleep(Duration::from_millis(10));
                u.invoke_deliver();
                if !u.get_busy() {
                    return;
                }
            }
            panic!("worker did not finish");
        };
        settle();
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
        settle();
        assert_eq!(u.get_page(), 0);
        let target = Uuid::parse_str(&u.get_entries().row_data(0).unwrap().id)?;
        u.invoke_show_activity();
        u.window().take_snapshot()?; // Instantiate the conditional dialog and its focused search field.
        let press = |text: SharedString| {
            use slint::platform::WindowEvent;
            u.window()
                .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
            u.window().dispatch_event(WindowEvent::KeyReleased { text });
        };
        press("s".into());
        assert_eq!(u.get_activity_search(), "s");
        assert_eq!(u.get_search(), "");
        u.set_activity_scope(ActivityScope::Failed);
        u.set_activity_search("  EXPECTED TEST  ".into());
        u.invoke_filter_activity();
        assert_eq!(u.get_activity_rows().row_count(), 0);
        let start = std::time::Instant::now();
        worker.queue_action(
            &u,
            target,
            "test",
            "Testing background responsiveness",
            |_, progress| {
                progress.phase("Waiting in the background…")?;
                std::thread::sleep(Duration::from_millis(120));
                anyhow::bail!("expected test failure")
            },
        );
        worker.queue_action(&u, target, "open", "Duplicate launch", |_, _| {
            panic!("a second action for a busy profile must not be queued")
        });
        assert_eq!(worker.mail.lock().unwrap().pending, 1);
        assert!(start.elapsed() < Duration::from_millis(50));
        assert!(u.get_busy());
        for _ in 0..100 {
            u.invoke_deliver();
            if u.get_entries()
                .iter()
                .any(|entry| entry.action_busy && entry.action_label.contains("Waiting"))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(u.get_entries().iter().any(|entry| entry.action_busy));
        while worker.mail.lock().unwrap().pending > 0 {
            u.invoke_deliver();
            std::thread::sleep(Duration::from_millis(5));
        }
        u.invoke_deliver();
        let row = u
            .get_entries()
            .iter()
            .find(|entry| entry.id.as_str() == target.to_string())
            .unwrap();
        assert!(!row.action_busy && row.action_error.contains("expected test failure"));
        assert!(u.get_error().contains("expected test failure"));
        let rows = u.get_activity_rows();
        assert_eq!(
            rows.row_count(),
            1,
            "new failures appear under active filters"
        );
        assert_eq!(rows.row_data(0).unwrap().detail, "expected test failure");
        u.set_activity_scope(ActivityScope::Completed);
        u.invoke_filter_activity();
        assert_eq!(u.get_activity_rows().row_count(), 0);
        u.invoke_close_panel();
        u.invoke_clear_error();
        let log = std::fs::read_to_string(s.0.join("switchboard.log"))?;
        assert!(log.contains("Testing background responsiveness"));
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
        u.invoke_reload();
        settle();
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
        settle();
        let link = folder.join("Lab-M365.lnk");
        assert!(link.exists());
        u.invoke_trash(id.clone());
        settle();
        assert!(!link.exists(), "trash removes tracked shortcuts");
        u.invoke_filter("Trash".into(), "".into());
        assert_eq!(u.get_entries().row_count(), 1);
        u.invoke_trash(id.clone());
        settle();
        assert_eq!(u.get_trash_count(), 0);
        assert!(link.exists(), "restore recreates tracked shortcuts");
        u.invoke_filter("All profiles".into(), "".into());
        u.invoke_show_activity();
        let events = u.get_activity_rows();
        assert_eq!(events.row_count(), u.get_activity_total() as usize);
        assert_eq!(events.row_data(0).unwrap().status, ActivityScope::Completed);
        assert_eq!(events.row_data(0).unwrap().action, "Restoring profile");
        capture("recent-activity")?;
        press(slint::platform::Key::Escape.into());
        assert!(!u.get_activity_open());
        u.invoke_filter("Trash".into(), "".into());

        let leftover = Uuid::new_v4();
        let temp_dir = s.0.join("Temporary").join(leftover.to_string());
        std::fs::create_dir_all(temp_dir.join("Data"))?;
        std::fs::write(temp_dir.join(".switchboard-session"), leftover.to_string())?;
        u.invoke_reload();
        settle();
        assert_eq!(u.get_trash_count(), 1);
        let row = u.get_entries().row_data(0).unwrap();
        assert!(row.temporary && !row.live);
        u.invoke_trash(row.id);
        settle();
        assert!(!temp_dir.exists());
        assert_eq!(u.get_trash_count(), 0);

        let data = s.0.join("Profiles").join(id.as_str());
        std::fs::create_dir_all(&data)?;
        std::fs::write(data.join("Cookies"), "x")?;
        u.invoke_filter("All profiles".into(), "".into());
        u.invoke_trash(id.clone());
        settle();
        u.invoke_filter("Trash".into(), "".into());
        u.invoke_empty_trash();
        assert!(!u.get_confirm_title().is_empty());
        u.invoke_confirm(true);
        settle();
        assert_eq!(u.get_trash_count(), 0);
        assert!(!data.exists() && !link.exists());
        assert!(!s.icon_path(Uuid::parse_str(&id)?).exists());
        assert_eq!(s.read()?.profiles.len(), 57);
        Ok(())
    }
}
