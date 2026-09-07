//! Local favorites store: the source of truth for favorites when no
//! favorites server is configured ("local mode"). A single JSON array of
//! [`FavItem`] at `<data_dir>/gifdeck/favorites.json`.
//!
//! The server and the local store are alternatives, never a fallback for
//! one another; the only crossover is the explicit `favs --export` /
//! `favs --import` commands.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::favs::{now_epoch, FavItem};
use crate::providers::GifResult;

#[derive(Debug, Clone)]
pub struct LocalStore {
    path: PathBuf,
}

impl LocalStore {
    /// Default store at `<data_dir>/gifdeck/favorites.json`.
    pub fn new() -> Self {
        let path = dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from(".local/share"))
            .join("gifdeck")
            .join("favorites.json");
        LocalStore { path }
    }

    /// Store at an explicit path (tests, tools).
    #[allow(dead_code)] // exercise code paths from the test suite
    pub fn at(path: impl Into<PathBuf>) -> Self {
        LocalStore { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the full list. A missing file is an empty list (fresh
    /// install); a damaged file is an `Err` so callers never silently
    /// treat real favorites as absent — a toggle that then wrote back
    /// would destroy them.
    pub fn load(&self) -> Result<Vec<FavItem>> {
        match std::fs::read_to_string(&self.path) {
            Ok(contents) => serde_json::from_str::<Vec<FavItem>>(&contents).map_err(|e| {
                anyhow::anyhow!(
                    "invalid local favorites store at {}: {e} (fix or remove the file)",
                    self.path.display()
                )
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => anyhow::bail!(
                "could not read local favorites store at {}: {e}",
                self.path.display()
            ),
        }
    }

    /// Atomically replace the store: write a temp file in the same
    /// directory (0600 on Unix — favorites data, same class as the
    /// config file) and rename over the old one, so a crash never
    /// leaves a partial store. On Windows the user-profile ACLs already
    /// provide the equivalent protection.
    pub fn save_all(&self, items: &[FavItem]) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| anyhow::anyhow!("failed to create {}: {e}", dir.display()))?;
        }
        let json = serde_json::to_string_pretty(items)?;
        let tmp = self.path.with_extension("json.tmp");
        {
            let mut f = std::fs::File::create(&tmp)
                .map_err(|e| anyhow::anyhow!("failed to create {}: {e}", tmp.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            }
            f.write_all(json.as_bytes())?;
        }
        std::fs::rename(&tmp, &self.path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            anyhow::anyhow!("failed to move {} into place: {e}", self.path.display())
        })?;
        Ok(())
    }

    /// Insert or update (by id) a favorite; returns the stored item.
    /// `added_at`/`use_count` of an existing entry are preserved.
    pub fn upsert(&self, gif: &GifResult) -> Result<FavItem> {
        let mut items = self.load()?;
        let mut stored = FavItem::from_gif(gif);
        match items.iter_mut().find(|f| f.id == gif.id) {
            Some(existing) => {
                stored.added_at = existing.added_at.clone();
                stored.use_count = existing.use_count;
                *existing = stored.clone();
            }
            None => items.push(stored.clone()),
        }
        self.save_all(&items)?;
        Ok(stored)
    }

    /// Remove a favorite by id; idempotent (missing id is not an error).
    pub fn remove(&self, id: &str) -> Result<()> {
        let mut items = self.load()?;
        let before = items.len();
        items.retain(|f| f.id != id);
        if items.len() != before {
            self.save_all(&items)?;
        }
        Ok(())
    }

    /// Record a "use" of a favorite: bump `use_count` and set `last_used`.
    /// Missing id is a silent no-op (nothing to count); the updated stats
    /// are returned so callers can surface them if they ever want to.
    pub fn increment_use(&self, id: &str) -> Result<FavItem> {
        let mut items = self.load()?;
        let Some(existing) = items.iter_mut().find(|f| f.id == id) else {
            return Ok(FavItem {
                id: id.to_string(),
                url: String::new(),
                preview: String::new(),
                provider: String::new(),
                title: String::new(),
                use_count: 0,
                added_at: None,
                last_used: None,
            });
        };
        existing.use_count = existing.use_count.saturating_add(1);
        existing.last_used = Some(now_epoch());
        let updated = existing.clone();
        self.save_all(&items)?;
        Ok(updated)
    }
}

impl Default for LocalStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::Provider;

    fn gif(id: &str) -> GifResult {
        GifResult {
            id: id.into(),
            title: format!("gif {id}"),
            url: format!("https://x/{id}.gif"),
            preview_url: format!("https://x/{id}-preview.gif"),
            provider: Provider::Klipy,
        }
    }

    /// A store per test name: tests run in parallel and each needs its own
    /// file (the atomic write would race on a shared temp path).
    fn store_named(name: &str) -> (LocalStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "gifdeck-store-{}-{:?}-{name}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("favorites.json");
        let _ = std::fs::remove_file(&path);
        (LocalStore::at(&path), path)
    }

    #[test]
    fn missing_file_is_empty_list() {
        let (s, path) = store_named("missing");
        assert!(s.load().unwrap().is_empty());
        assert_eq!(s.path(), path.as_path());
    }

    #[test]
    fn save_all_round_trips() {
        let (s, _) = store_named("roundtrip");
        let items = vec![FavItem::from_gif(&gif("a")), FavItem::from_gif(&gif("b"))];
        s.save_all(&items).unwrap();
        let loaded = s.load().unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].id, "a");
        assert_eq!(loaded[1].url, "https://x/b.gif");
        assert_eq!(loaded[1].provider, "klipy");
    }

    #[test]
    fn save_all_is_atomic_and_private() {
        let (s, _) = store_named("atomic");
        s.save_all(&[FavItem::from_gif(&gif("a"))]).unwrap();
        // No temp file left behind…
        let tmp = s.path().with_extension("json.tmp");
        assert!(!tmp.exists());
        // …and on Unix the store is 0600 (favorites data is
        // secret-class; Windows relies on profile ACLs instead).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(s.path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn save_all_creates_parent_dirs() {
        let dir = std::env::temp_dir().join(format!("gifdeck-store-deep-{}", std::process::id()));
        let s = LocalStore::at(dir.join("nested/favorites.json"));
        s.save_all(&[FavItem::from_gif(&gif("a"))]).unwrap();
        assert!(s.path().exists());
        let _ = std::fs::remove_file(s.path());
    }

    #[test]
    fn invalid_file_is_an_error_not_empty() {
        let (s, path) = store_named("invalid");
        std::fs::write(&path, "{ not favorites ").unwrap();
        let err = s.load().unwrap_err().to_string();
        assert!(err.contains("invalid local favorites store"), "got: {err}");
        // And upsert must refuse to write over a damaged store.
        assert!(s.upsert(&gif("a")).is_err());
        assert!(s.remove("a").is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn upsert_inserts_then_updates_in_place() {
        let (s, _) = store_named("upsert");
        let first = s.upsert(&gif("a")).unwrap();
        assert!(first.added_at.is_some(), "new entries get a timestamp");
        s.upsert(&gif("b")).unwrap();
        // Update "a" with a new URL: id and position stay, added_at kept.
        let mut updated = gif("a");
        updated.url = "https://x/a-v2.gif".into();
        s.upsert(&updated).unwrap();
        let items = s.load().unwrap();
        assert_eq!(items.len(), 2, "upsert never duplicates");
        assert_eq!(items[0].id, "a");
        assert_eq!(items[0].url, "https://x/a-v2.gif");
        assert_eq!(items[0].added_at, first.added_at);
        assert_eq!(items[1].id, "b");
    }

    #[test]
    fn remove_deletes_and_is_idempotent() {
        let (s, _) = store_named("remove");
        s.upsert(&gif("a")).unwrap();
        s.upsert(&gif("b")).unwrap();
        s.remove("a").unwrap();
        let items = s.load().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "b");
        s.remove("nope").unwrap();
        assert_eq!(s.load().unwrap().len(), 1);
    }

    #[test]
    fn increment_use_bumps_and_persists() {
        let (s, _) = store_named("use-bump");
        let fav = s.upsert(&gif("a")).unwrap();
        assert_eq!(fav.use_count, 0);
        assert!(fav.last_used.is_none());

        let first = s.increment_use("a").unwrap();
        assert_eq!(first.use_count, 1);
        assert!(first.last_used.is_some());

        let second = s.increment_use("a").unwrap();
        assert_eq!(second.use_count, 2);

        let items = s.load().unwrap();
        assert_eq!(items[0].use_count, 2, "bump is persisted");
        // Compare against the last write: `first` and `second` may land
        // on different epoch seconds, so `first` would race.
        assert_eq!(items[0].last_used, second.last_used);
        // added_at is preserved, like upsert does.
        assert_eq!(items[0].added_at, fav.added_at);
    }

    #[test]
    fn increment_use_missing_id_is_a_noop() {
        let (s, _) = store_named("use-missing");
        s.upsert(&gif("a")).unwrap();
        let noop = s.increment_use("nope").unwrap();
        assert_eq!(noop.id, "nope");
        assert_eq!(noop.use_count, 0);
        assert!(noop.last_used.is_none());
        let items = s.load().unwrap();
        assert_eq!(items.len(), 1, "no entry created");
        assert_eq!(items[0].use_count, 0, "existing entry untouched");
    }

    #[test]
    fn increment_use_refuses_damaged_store() {
        let (s, path) = store_named("use-damaged");
        std::fs::write(&path, "{ broken").unwrap();
        assert!(s.increment_use("a").is_err());
        let _ = std::fs::remove_file(&path);
    }
}
