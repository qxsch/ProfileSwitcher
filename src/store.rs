use crate::{icons, native};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Profile {
    pub id: Uuid,
    pub name: String,
    pub collection: String,
    pub symbol: usize,
    pub color: usize,
    pub pinned: bool,
    pub trashed: bool,
    pub opened: u64,
}
#[derive(Default, Debug, Serialize, Deserialize)]
pub struct Catalog {
    pub profiles: Vec<Profile>,
    pub last_folder: Option<PathBuf>,
}
impl Catalog {
    pub fn profile(&mut self, id: Uuid) -> Result<&mut Profile> {
        self.profiles
            .iter_mut()
            .find(|p| p.id == id)
            .context("This profile is no longer in the library.")
    }
}
#[derive(Clone)]
pub struct Store(pub PathBuf);

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl Store {
    pub fn local() -> Result<Self> {
        Ok(Self(native::local_data()?.join("EdgeProfileSwitcher")))
    }
    pub fn lock(&self) -> Result<File> {
        fs::create_dir_all(&self.0)?;
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .read(true)
            .open(self.0.join("library.lock"))?;
        f.lock()?;
        Ok(f)
    }
    pub fn read(&self) -> Result<Catalog> {
        let file = match File::open(self.0.join("library.json")) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Catalog::default()),
            Err(e) => return Err(e.into()),
        };
        let catalog: Catalog = serde_json::from_reader(file)
            .context("Could not read library.json; your library has not been overwritten")?;
        for p in &catalog.profiles {
            icons::valid(p.symbol, p.color)?;
        }
        Ok(catalog)
    }
    pub fn update<T>(&self, action: impl FnOnce(&mut Catalog) -> Result<T>) -> Result<T> {
        let _lock = self.lock()?;
        let mut catalog = self.read()?;
        let result = action(&mut catalog)?;
        let mut tmp = tempfile::NamedTempFile::new_in(&self.0)?;
        serde_json::to_writer_pretty(&mut tmp, &catalog)?;
        tmp.flush()?;
        tmp.as_file().sync_all()?;
        tmp.persist(self.0.join("library.json"))?;
        Ok(result)
    }
    pub fn save(
        &self,
        id: Option<Uuid>,
        name: &str,
        collection: &str,
        symbol: usize,
        color: usize,
        pinned: bool,
    ) -> Result<Uuid> {
        let name = name.trim();
        ensure!(
            !name.is_empty() && name.chars().count() <= 96 && !name.chars().any(char::is_control),
            "Enter a profile name of 1–96 printable characters."
        );
        ensure!(
            collection.chars().count() <= 64,
            "Collection names must be at most 64 characters."
        );
        icons::valid(symbol, color)?;
        let id_out = id.unwrap_or_else(Uuid::new_v4);
        self.update(|c| {
            let p = if id.is_some() {
                c.profile(id_out)?
            } else {
                c.profiles.push(Profile {
                    id: id_out,
                    name: String::new(),
                    collection: String::new(),
                    symbol,
                    color,
                    pinned,
                    trashed: false,
                    opened: 0,
                });
                c.profiles.last_mut().unwrap()
            };
            ensure!(!p.trashed, "Restore the profile before editing it.");
            p.name = name.into();
            p.collection = collection.trim().into();
            p.symbol = symbol;
            p.color = color;
            p.pinned = pinned;
            self.write_icon(p)?;
            Ok(id_out)
        })
    }
    pub fn icon_path(&self, id: Uuid) -> PathBuf {
        self.0.join("Icons").join(format!("{id}.ico"))
    }
    fn write_icon(&self, p: &Profile) -> Result<()> {
        let dir = self.0.join("Icons");
        fs::create_dir_all(&dir)?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        icons::write_ico(&mut tmp, p.symbol, p.color)?;
        tmp.persist(self.icon_path(p.id))?;
        Ok(())
    }
    pub fn trash(&self, id: Uuid, trashed: bool) -> Result<()> {
        self.update(|c| {
            if trashed && native::running(id)? {
                bail!("Close this profile's Edge processes before moving it to Trash.");
            }
            c.profile(id)?.trashed = trashed;
            Ok(())
        })
    }
    pub fn launch(&self, id: Uuid) -> Result<Option<native::Job>> {
        self.update(|c| {
            let p = c.profile(id)?;
            ensure!(
                !p.trashed,
                "This profile is in Trash. Restore it before opening it."
            );
            let dir = self.0.join("Profiles").join(id.to_string());
            fs::create_dir_all(&dir)?;
            let job = if native::focus(id)? {
                None
            } else {
                Some(native::launch_edge(id, &dir, false)?)
            };
            p.opened = now();
            Ok(job)
        })
    }
    pub fn shortcuts(&self, ids: &[Uuid], folder: &Path) -> Result<()> {
        ensure!(folder.is_dir(), "Choose an existing folder.");
        self.update(|c| {
            for (i, &id) in ids.iter().enumerate() {
                let p = c.profile(id)?;
                ensure!(
                    !p.trashed,
                    "Cannot create a shortcut for a trashed profile."
                );
                native::shortcut(p, &self.icon_path(id), folder).with_context(|| {
                    format!(
                        "Created {i} of {} shortcuts; could not create the link for {}.",
                        ids.len(),
                        p.name
                    )
                })?;
            }
            c.last_folder = Some(folder.into());
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn renaming_and_recoloring_preserve_identity_and_trash_is_reversible() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store(dir.path().into());
        let id = s
            .save(None, "Customer01", "Production", 0, 0, false)
            .unwrap();
        let first = fs::read(s.icon_path(id)).unwrap();
        s.save(Some(id), "Renamed", "Labs", 99, 5, true).unwrap();
        assert_ne!(first, fs::read(s.icon_path(id)).unwrap());
        s.trash(id, true).unwrap();
        assert!(s.launch(id).is_err());
        s.trash(id, false).unwrap();
        let c = s.read().unwrap();
        assert_eq!(c.profiles.len(), 1);
        assert_eq!(
            (c.profiles[0].id, c.profiles[0].symbol, c.profiles[0].color),
            (id, 99, 5)
        );
    }
    #[test]
    fn corrupt_catalog_is_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store(dir.path().into());
        fs::write(s.0.join("library.json"), "broken").unwrap();
        assert!(s.save(None, "test", "", 0, 0, false).is_err());
        assert_eq!(
            fs::read_to_string(s.0.join("library.json")).unwrap(),
            "broken"
        );
    }
}
