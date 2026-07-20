#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Codex dispatch lands in the next implementation slice"
    )
)]

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process;
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

const STATE_VERSION: u8 = 1;
const STATE_LOCK_RETRIES: usize = 200;
const STATE_LOCK_SLEEP: Duration = Duration::from_millis(5);
const STALE_LOCK_AGE: Duration = Duration::from_mins(1);
const STALE_STATE_AGE: Duration = Duration::from_hours(7 * 24);
const SWEEP_ENTRY_LIMIT: usize = 64;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SessionState {
    version: u8,
    pub known_route_ids: BTreeSet<String>,
    pub live_route_ids: BTreeSet<String>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            known_route_ids: BTreeSet::new(),
            live_route_ids: BTreeSet::new(),
        }
    }
}

#[derive(Debug)]
pub struct LoadedState {
    pub state: SessionState,
    pub was_reset: bool,
    persistence: Option<Persistence>,
}

impl LoadedState {
    fn unavailable() -> Self {
        Self {
            state: SessionState::default(),
            was_reset: false,
            persistence: None,
        }
    }

    const fn available(state: SessionState, was_reset: bool, persistence: Persistence) -> Self {
        Self {
            state,
            was_reset,
            persistence: Some(persistence),
        }
    }
}

#[derive(Debug)]
struct Persistence {
    file: PathBuf,
    _lock: StateLock,
}

#[derive(Debug)]
struct StateLock {
    path: PathBuf,
}

impl Drop for StateLock {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir(&self.path)
            && error.kind() != io::ErrorKind::NotFound
        {
            eprintln!(
                "prompt context router could not remove state lock {}: {error}",
                self.path.display()
            );
        }
    }
}

/// Loads one session's state while holding its lock until [`save_state`] or
/// the returned value is dropped. All filesystem failures yield empty state.
#[must_use]
pub fn load_state(cache_root: &Path, project_root: &Path, session_id: &str) -> LoadedState {
    sweep_stale_sessions(cache_root);

    let file = match state_file(cache_root, project_root, session_id) {
        Ok(Some(file)) => file,
        Ok(None) => {
            eprintln!("prompt context router rejected unsafe session id");
            return LoadedState::unavailable();
        }
        Err(error) => {
            eprintln!("prompt context router could not resolve state path: {error}");
            return LoadedState::unavailable();
        }
    };
    let Some(lock) = acquire_state_lock(&file) else {
        return LoadedState::unavailable();
    };
    let persistence = Persistence {
        file: file.clone(),
        _lock: lock,
    };

    match fs::read_to_string(&file) {
        Ok(text) => match serde_json::from_str::<SessionState>(&text) {
            Ok(state) if state.version == STATE_VERSION => {
                LoadedState::available(state, false, persistence)
            }
            Ok(_) | Err(_) => LoadedState::available(SessionState::default(), true, persistence),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            LoadedState::available(SessionState::default(), false, persistence)
        }
        Err(error) => {
            eprintln!(
                "prompt context router could not read state {}: {error}",
                file.display()
            );
            LoadedState::available(SessionState::default(), true, persistence)
        }
    }
}

/// Atomically persists a loaded state. Call only after hook stdout is flushed.
/// Returns `false` when persistence is unavailable; hook processing stays open.
#[must_use]
pub fn save_state(mut loaded: LoadedState) -> bool {
    let Some(persistence) = loaded.persistence.take() else {
        return false;
    };
    write_state(&persistence.file, &loaded.state)
}

fn state_file(
    cache_root: &Path,
    project_root: &Path,
    session_id: &str,
) -> io::Result<Option<PathBuf>> {
    if !is_safe_session_id(session_id) {
        return Ok(None);
    }
    let canonical_root = fs::canonicalize(project_root)?;
    Ok(Some(
        cache_root
            .join(project_hash(&canonical_root))
            .join(format!("{session_id}.json")),
    ))
}

fn is_safe_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && !matches!(session_id, "." | "..")
        && session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// FNV-1a, inlined for a stable namespace across Rust releases: the standard
/// `DefaultHasher` algorithm is unspecified, and a toolchain upgrade must not
/// silently rotate every project's cache directory.
fn project_hash(project_root: &Path) -> String {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for byte in project_root.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{hash:016x}")
}

fn acquire_state_lock(file: &Path) -> Option<StateLock> {
    let path = file.with_extension("json.lock");
    let parent = path.parent()?;
    if let Err(error) = fs::create_dir_all(parent) {
        eprintln!(
            "prompt context router could not create state directory {}: {error}",
            parent.display()
        );
        return None;
    }

    for _ in 0..STATE_LOCK_RETRIES {
        match fs::create_dir(&path) {
            Ok(()) => return Some(StateLock { path }),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if !remove_stale_lock(&path) {
                    thread::sleep(STATE_LOCK_SLEEP);
                }
            }
            Err(error) => {
                eprintln!(
                    "prompt context router could not create state lock {}: {error}",
                    path.display()
                );
                return None;
            }
        }
    }

    eprintln!(
        "prompt context router timed out acquiring state lock {}",
        path.display()
    );
    None
}

fn remove_stale_lock(path: &Path) -> bool {
    if !is_older_than(path, STALE_LOCK_AGE) {
        return false;
    }

    match fs::remove_dir(path) {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => true,
        Err(error) => {
            eprintln!(
                "prompt context router could not remove stale state lock {}: {error}",
                path.display()
            );
            false
        }
    }
}

fn write_state(file: &Path, state: &SessionState) -> bool {
    let mut output = match serde_json::to_vec(state) {
        Ok(output) => output,
        Err(error) => {
            eprintln!("prompt context router could not serialize state: {error}");
            return false;
        }
    };
    output.push(b'\n');

    let temporary = file.with_extension(format!("json.tmp-{}", process::id()));
    if let Err(error) = fs::write(&temporary, output) {
        eprintln!(
            "prompt context router could not write temporary state {}: {error}",
            temporary.display()
        );
        return false;
    }
    if let Err(error) = fs::rename(&temporary, file) {
        eprintln!(
            "prompt context router could not replace state {}: {error}",
            file.display()
        );
        let _ = fs::remove_file(&temporary);
        return false;
    }
    true
}

fn is_older_than(path: &Path, max_age: Duration) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age >= max_age)
}

fn sweep_stale_sessions(cache_root: &Path) {
    let _ = sweep_stale_sessions_with(cache_root, STALE_STATE_AGE, SWEEP_ENTRY_LIMIT);
}

fn sweep_stale_sessions_with(
    cache_root: &Path,
    max_age: Duration,
    max_entries: usize,
) -> (usize, usize) {
    let Ok(namespaces) = fs::read_dir(cache_root) else {
        return (0, 0);
    };
    let mut inspected = 0;
    let mut removed = 0;

    for namespace in namespaces.flatten() {
        if inspected == max_entries {
            break;
        }
        inspected += 1;
        let directory = namespace.path();
        if !directory.is_dir() {
            continue;
        }

        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            if inspected == max_entries {
                return (inspected, removed);
            }
            inspected += 1;
            let path = entry.path();
            if !is_older_than(&path, max_age) {
                continue;
            }

            let result = if path.is_dir() {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            };
            match result {
                Ok(()) => removed += 1,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => eprintln!(
                    "prompt context router could not sweep stale state {}: {error}",
                    path.display()
                ),
            }
        }

        let _ = fs::remove_dir(&directory);
    }

    (inspected, removed)
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs::{File, FileTimes};
    use std::time::SystemTime;

    use tempfile::TempDir;

    use super::*;

    fn test_roots() -> Result<(TempDir, TempDir), Box<dyn Error>> {
        Ok((tempfile::tempdir()?, tempfile::tempdir()?))
    }

    fn file_for(
        cache_root: &Path,
        project_root: &Path,
        session_id: &str,
    ) -> Result<PathBuf, Box<dyn Error>> {
        state_file(cache_root, project_root, session_id)?
            .ok_or_else(|| "session id was unexpectedly rejected".into())
    }

    #[test]
    fn state_round_trips_through_save_and_load() -> Result<(), Box<dyn Error>> {
        let (cache, project) = test_roots()?;
        let mut loaded = load_state(cache.path(), project.path(), "session");
        loaded.state.known_route_ids.insert("known".to_owned());
        loaded.state.live_route_ids.insert("live".to_owned());
        if !save_state(loaded) {
            return Err("state was not saved".into());
        }

        let loaded = load_state(cache.path(), project.path(), "session");
        assert_eq!(
            (
                loaded.state.known_route_ids,
                loaded.state.live_route_ids,
                loaded.was_reset,
            ),
            (
                BTreeSet::from(["known".to_owned()]),
                BTreeSet::from(["live".to_owned()]),
                false,
            )
        );
        Ok(())
    }

    #[test]
    fn malformed_json_loads_empty_state_with_reset_condition() -> Result<(), Box<dyn Error>> {
        let (cache, project) = test_roots()?;
        let file = file_for(cache.path(), project.path(), "session")?;
        fs::create_dir_all(file.parent().ok_or("state path had no parent")?)?;
        fs::write(file, "not json")?;

        let loaded = load_state(cache.path(), project.path(), "session");
        assert_eq!(
            (loaded.state, loaded.was_reset),
            (SessionState::default(), true)
        );
        Ok(())
    }

    #[test]
    fn wrong_version_loads_empty_state_with_reset_condition() -> Result<(), Box<dyn Error>> {
        let (cache, project) = test_roots()?;
        let file = file_for(cache.path(), project.path(), "session")?;
        fs::create_dir_all(file.parent().ok_or("state path had no parent")?)?;
        fs::write(
            file,
            r#"{"version":2,"known_route_ids":[],"live_route_ids":[]}"#,
        )?;

        let loaded = load_state(cache.path(), project.path(), "session");
        assert_eq!(
            (loaded.state, loaded.was_reset),
            (SessionState::default(), true)
        );
        Ok(())
    }

    #[test]
    fn missing_file_loads_empty_state_without_reset_condition() -> Result<(), Box<dyn Error>> {
        let (cache, project) = test_roots()?;

        let loaded = load_state(cache.path(), project.path(), "session");
        assert_eq!(
            (loaded.state, loaded.was_reset),
            (SessionState::default(), false)
        );
        Ok(())
    }

    #[test]
    fn atomic_save_leaves_no_temporary_file() -> Result<(), Box<dyn Error>> {
        let (cache, project) = test_roots()?;
        let loaded = load_state(cache.path(), project.path(), "session");
        if !save_state(loaded) {
            return Err("state was not saved".into());
        }
        let file = file_for(cache.path(), project.path(), "session")?;
        let mut entries = fs::read_dir(file.parent().ok_or("state path had no parent")?)?
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect::<Vec<_>>();
        entries.sort_unstable();

        assert_eq!(entries, ["session.json"]);
        Ok(())
    }

    #[test]
    fn session_id_rejects_path_traversal() {
        assert!(!is_safe_session_id("../x"));
    }

    #[test]
    fn sweep_removes_only_stale_entries_within_bound() -> Result<(), Box<dyn Error>> {
        let (cache, project) = test_roots()?;
        let stale = ["stale-a", "stale-b", "stale-c"]
            .map(|session| file_for(cache.path(), project.path(), session))
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;
        let fresh = file_for(cache.path(), project.path(), "fresh")?;
        fs::create_dir_all(fresh.parent().ok_or("state path had no parent")?)?;
        for path in stale.iter().chain(std::iter::once(&fresh)) {
            fs::write(path, "{}")?;
        }
        let stale_time = SystemTime::now()
            .checked_sub(Duration::from_hours(2))
            .ok_or("could not derive stale timestamp")?;
        for path in &stale {
            File::options()
                .write(true)
                .open(path)?
                .set_times(FileTimes::new().set_modified(stale_time))?;
        }

        let (inspected, removed) =
            sweep_stale_sessions_with(cache.path(), Duration::from_hours(1), 3);
        let stale_remaining = stale.iter().filter(|path| path.exists()).count();

        assert_eq!(inspected, 3);
        assert!((1..=2).contains(&removed));
        assert!(fresh.exists());
        assert!(stale_remaining >= 1);
        Ok(())
    }
}
