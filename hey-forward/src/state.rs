//! What the daemon remembers between polls and restarts, and how files holding it or the
//! credentials are written.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};

/// How long a handled entry is remembered past the point it could be picked up again.
const PRUNE_MARGIN: TimeDelta = TimeDelta::hours(1);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    /// When the daemon first ran. Mail older than this is never forwarded.
    pub baseline: DateTime<Utc>,
    /// When the last poll that read everything it meant to began.
    pub checkpoint: DateTime<Utc>,
    /// The entries already dealt with — forwarded or found to match no rule — and when.
    #[serde(default)]
    pub handled: BTreeMap<i64, DateTime<Utc>>,
}

impl State {
    /// A state that takes `since` as the beginning of time.
    pub fn starting_at(since: DateTime<Utc>) -> State {
        State {
            baseline: since,
            checkpoint: since,
            handled: BTreeMap::new(),
        }
    }

    /// Forgets handled entries too old to come up again.
    pub fn prune(&mut self, floor: DateTime<Utc>) {
        let cutoff = floor - PRUNE_MARGIN;
        self.handled.retain(|_, handled_at| *handled_at >= cutoff);
    }
}

/// A [`State`] and the file it is kept in, if it is kept at all.
#[derive(Debug)]
pub struct StateFile {
    path: Option<PathBuf>,
    pub state: State,
}

impl StateFile {
    /// A state that lives only as long as the process: saving it does nothing.
    pub fn in_memory(state: State) -> StateFile {
        StateFile { path: None, state }
    }

    /// Reads the state at `path`, or starts one at `now` when there is none yet.
    pub fn load_or_start(path: PathBuf, now: DateTime<Utc>) -> Result<StateFile> {
        let state = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("state file {} is unreadable", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => State::starting_at(now),
            Err(error) => {
                return Err(error).with_context(|| format!("cannot read {}", path.display()));
            }
        };
        Ok(StateFile {
            path: Some(path),
            state,
        })
    }

    pub fn save(&self) -> Result<()> {
        match &self.path {
            Some(path) => write_private(path, &serde_json::to_vec_pretty(&self.state)?),
            None => Ok(()),
        }
    }
}

/// Writes a file only its owner can read, replacing it in one step so a crash mid-write
/// leaves the old contents rather than half of the new ones.
pub fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    let directory = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(directory)
        .with_context(|| format!("cannot create {}", directory.display()))?;
    let temporary = path.with_extension("tmp");
    let write = || -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&temporary)?;
        // The mode above only applies to a file it creates; a leftover one keeps its own.
        #[cfg(unix)]
        file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        file.write_all(contents)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    };
    write().with_context(|| format!("cannot write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(hour: u32) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(&format!("2026-11-17T{hour:02}:00:00Z"))
            .unwrap()
            .to_utc()
    }

    #[test]
    fn a_missing_file_starts_from_now_and_a_saved_one_reads_back() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested").join("state.json");

        let mut file = StateFile::load_or_start(path.clone(), at(9)).unwrap();
        assert_eq!(file.state, State::starting_at(at(9)));

        file.state.handled.insert(42, at(10));
        file.state.checkpoint = at(10);
        file.save().unwrap();

        let reread = StateFile::load_or_start(path, at(12)).unwrap();
        assert_eq!(reread.state, file.state);
    }

    #[cfg(unix)]
    #[test]
    fn files_are_private_to_their_owner() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("secret.json");
        write_private(&path, b"{}").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn pruning_keeps_what_could_still_come_up() {
        let mut state = State::starting_at(at(0));
        state.handled.insert(1, at(3));
        state.handled.insert(2, at(9));
        state.prune(at(10));
        assert_eq!(state.handled.keys().copied().collect::<Vec<_>>(), [2]);
    }
}
