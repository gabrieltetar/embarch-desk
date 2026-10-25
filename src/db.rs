//! The state file and the one process that owns it.
//!
//! Hooks fire on every tool call, so a write marks the state dirty and a
//! flusher persists it at most every couple of seconds — atomically, via a
//! temp file and a rename. Acted-on tiles older than a week move to
//! `history/YYYY-MM.jsonl`, which only ever grows.

use crate::paths;
use crate::store::{State, Tile};
use anyhow::{Context, Result};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

pub struct Db {
    dir: PathBuf,
    state: Mutex<State>,
    dirty: AtomicBool,
}

impl Db {
    pub fn open(dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join("state.json");
        let mut state: State = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b).with_context(|| format!("parsing {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        // Lines from before notes existed are notes of kind done; they get
        // their ids here.
        let migrated = state.migrate();
        Ok(Self { dir, state: Mutex::new(state), dirty: AtomicBool::new(migrated) })
    }

    /// The data dir `state.json` lives in, which the reference index shares.
    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    pub fn read<R>(&self, f: impl FnOnce(&State) -> R) -> R {
        f(&self.state.lock().expect("state lock"))
    }

    pub fn write<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        let r = f(&mut self.state.lock().expect("state lock"));
        self.dirty.store(true, Ordering::Release);
        r
    }

    /// A write that may change nothing: `f` says whether it did, and only
    /// then is the state marked dirty — a periodic pass with nothing to do
    /// leaves the file alone.
    pub fn update<R>(&self, f: impl FnOnce(&mut State) -> (R, bool)) -> R {
        let (r, changed) = f(&mut self.state.lock().expect("state lock"));
        if changed {
            self.dirty.store(true, Ordering::Release);
        }
        r
    }

    pub fn flush(&self) -> Result<()> {
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        let bytes = self.read(serde_json::to_vec_pretty)?;
        if let Err(e) = paths::write_private(&self.dir.join("state.json"), &bytes) {
            self.dirty.store(true, Ordering::Release);
            return Err(e);
        }
        Ok(())
    }

    pub fn archive(&self, tiles: &[Tile]) -> Result<()> {
        if tiles.is_empty() {
            return Ok(());
        }
        let hist = self.dir.join("history");
        std::fs::create_dir_all(&hist)?;
        for t in tiles {
            let month = t.date.get(..7).unwrap_or("unknown");
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(hist.join(format!("{month}.jsonl")))?;
            writeln!(f, "{}", serde_json::to_string(t)?)?;
        }
        Ok(())
    }
}

pub fn open_default() -> Result<Db> {
    Db::open(paths::data_dir()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_survives_a_reopen_and_a_clean_flush_writes_nothing() {
        let dir = std::env::temp_dir().join(format!("desk-db-{}", paths::random_hex(4).unwrap()));
        let db = Db::open(dir.clone()).unwrap();
        db.flush().unwrap();
        assert!(!dir.join("state.json").exists());
        let id = db.write(|s| s.create_item("Bootload", None, 1));
        db.flush().unwrap();
        let again = Db::open(dir.clone()).unwrap();
        assert_eq!(again.read(|s| s.items[&id].title.clone()), "Bootload");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
