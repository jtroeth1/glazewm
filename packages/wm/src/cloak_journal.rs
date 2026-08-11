use std::{
  env, fs,
  path::{Path, PathBuf},
};

/// Name of the journal file within the `GlazeWM` config directory.
const JOURNAL_FILE_NAME: &str = "cloaked-windows.txt";

/// Records the windows this instance manages so a later instance can
/// uncloak them if this one dies without cleaning up.
///
/// Cloaking is invisible to `IsWindowVisible` and survives process
/// death, so a hard kill of both the WM and its watcher would otherwise
/// leave windows permanently hidden from `visible_windows()`. The
/// journal is the only way a later instance can tell a window cloaked by
/// a dead WM apart from one parked on another virtual desktop or a
/// suspended UWP app, since all three report `DWM_CLOAKED_SHELL`.
pub struct CloakJournal {
  /// Path of the journal file, or `None` if no config directory could be
  /// resolved.
  path: Option<PathBuf>,

  /// Entries most recently written, used to skip redundant writes.
  last: Vec<(isize, String)>,

  /// Number of times the journal file has actually been rewritten.
  writes: usize,
}

impl CloakJournal {
  /// Creates a journal pointing at the `GlazeWM` config directory.
  #[must_use]
  pub fn new() -> Self {
    let path = journal_path();

    if path.is_none() {
      tracing::warn!(
        "Unable to resolve config directory. Cloaked windows will not be \
         journalled for recovery."
      );
    }

    Self {
      path,
      last: Vec::new(),
      writes: 0,
    }
  }

  /// Rewrites the journal when the managed set changed. Cheap no-op
  /// when unchanged.
  pub fn record(&mut self, entries: Vec<(isize, String)>) {
    if entries == self.last {
      return;
    }

    let Some(path) = self.path.clone() else {
      self.last = entries;
      return;
    };

    let contents = serialize_entries(&entries);

    if let Some(parent) = path.parent() {
      if let Err(err) = fs::create_dir_all(parent) {
        tracing::warn!("Failed to create cloak journal directory: {err}");
        return;
      }
    }

    if let Err(err) = fs::write(&path, contents) {
      tracing::warn!("Failed to write cloak journal: {err}");
      return;
    }

    self.last = entries;
    self.writes += 1;
  }

  /// Uncloaks every journalled window that is still cloaked and still
  /// belongs to the recorded process, then deletes the journal.
  ///
  /// Returns the number of windows uncloaked. The count is informational;
  /// the recovery itself is the side effect that matters.
  pub fn recover() -> usize {
    let Some(path) = journal_path() else {
      return 0;
    };

    Self::recover_at(&path)
  }

  /// Implements [`CloakJournal::recover`] for a specific journal path.
  fn recover_at(path: &Path) -> usize {
    let Ok(contents) = fs::read_to_string(path) else {
      return 0;
    };

    let entries = parse_entries(&contents);
    let mut recovered = 0;

    for (handle, process_name) in &entries {
      if uncloak_entry(*handle, process_name) {
        recovered += 1;
      }
    }

    if let Err(err) = fs::remove_file(path) {
      tracing::warn!("Failed to delete cloak journal: {err}");
    }

    if !entries.is_empty() {
      tracing::info!(
        "Cloak journal: uncloaked {recovered} of {} journalled windows.",
        entries.len()
      );
    }

    recovered
  }

  /// Number of times the journal file has been rewritten.
  ///
  /// Used to assert that unchanged sets don't cause writes.
  #[cfg(test)]
  fn writes(&self) -> usize {
    self.writes
  }

  /// Creates a journal at an explicit path, for tests only.
  #[cfg(test)]
  fn at(path: PathBuf) -> Self {
    Self {
      path: Some(path),
      last: Vec::new(),
      writes: 0,
    }
  }
}

impl Default for CloakJournal {
  fn default() -> Self {
    Self::new()
  }
}

/// Resolves the journal file path within the `GlazeWM` config directory.
///
/// Mirrors how `UserConfig` resolves its own path, so the journal always
/// lands beside `config.yaml`.
fn journal_path() -> Option<PathBuf> {
  let config_dir = env::var("GLAZEWM_CONFIG_PATH")
    .ok()
    .map(PathBuf::from)
    .and_then(|path| path.parent().map(Path::to_path_buf))
    .or_else(|| home::home_dir().map(|dir| dir.join(".glzr/glazewm")))?;

  Some(config_dir.join(JOURNAL_FILE_NAME))
}

/// Serializes entries as one `<handle><TAB><process name>` line each.
///
/// Handles are written as decimal integers, matching the `i64` range of
/// a Windows `HWND` on 64-bit targets.
fn serialize_entries(entries: &[(isize, String)]) -> String {
  use std::fmt::Write;

  let mut contents = String::new();

  for (handle, process_name) in entries {
    // Writing to a `String` is infallible.
    let _ = writeln!(contents, "{handle}\t{process_name}");
  }

  contents
}

/// Parses journal contents, skipping malformed lines.
fn parse_entries(contents: &str) -> Vec<(isize, String)> {
  contents
    .lines()
    .filter_map(|line| {
      let (handle, process_name) = line.split_once('\t')?;
      let handle =
        isize::try_from(handle.trim().parse::<i64>().ok()?).ok()?;

      if process_name.is_empty() {
        return None;
      }

      Some((handle, process_name.to_string()))
    })
    .collect()
}

/// Uncloaks a single journalled window if it is still the same window.
///
/// Guards against uncloaking windows that `GlazeWM` never cloaked: the
/// handle must still be valid, still report as cloaked, and still belong
/// to the journalled process.
#[cfg(target_os = "windows")]
fn uncloak_entry(handle: isize, process_name: &str) -> bool {
  use wm_platform::{NativeWindow, NativeWindowWindowsExt};

  let window = NativeWindow::from_handle(handle);

  if !window.is_valid() {
    return false;
  }

  if !window.is_cloaked().unwrap_or(false) {
    return false;
  }

  if window.process_name().ok().as_deref() != Some(process_name) {
    return false;
  }

  if let Err(err) = window.set_cloaked(false) {
    tracing::warn!(
      "Failed to uncloak journalled window {handle} \
       [{process_name}]: {err:?}"
    );

    return false;
  }

  tracing::info!(
    "Cloak journal: uncloaked window {handle} [{process_name}]."
  );

  true
}

/// Non-Windows stub. Cloaking only exists on Windows.
#[cfg(not(target_os = "windows"))]
fn uncloak_entry(_handle: isize, _process_name: &str) -> bool {
  false
}

#[cfg(test)]
mod tests {
  use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::{SystemTime, UNIX_EPOCH},
  };

  use super::{
    parse_entries, serialize_entries, CloakJournal, JOURNAL_FILE_NAME,
  };

  /// Counter for unique temp directories per test.
  static COUNTER: AtomicUsize = AtomicUsize::new(0);

  /// Creates a unique temp directory path for a test journal.
  fn temp_journal() -> std::path::PathBuf {
    let nanos = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .map_or(0, |dur| dur.as_nanos());

    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);

    let dir = std::env::temp_dir()
      .join(format!("glazewm-cloak-journal-{nanos}-{unique}"));

    dir.join(JOURNAL_FILE_NAME)
  }

  #[test]
  fn record_round_trips_through_the_journal_file() {
    let path = temp_journal();
    let mut journal = CloakJournal::at(path.clone());

    let entries = vec![
      (395_510_isize, "alacritty".to_string()),
      (3_214_098_isize, "Code".to_string()),
    ];

    journal.record(entries.clone());

    let contents = std::fs::read_to_string(&path).unwrap_or_default();

    assert_eq!(parse_entries(&contents), entries);

    let _ = std::fs::remove_dir_all(path.parent().unwrap_or(&path));
  }

  #[test]
  fn malformed_lines_are_skipped() {
    let contents = concat!(
      "12\talacritty\n",
      "not-a-handle\tfirefox\n",
      "no-tab-separator\n",
      "\n",
      "34\t\n",
      "  56\tCode\n",
    );

    assert_eq!(
      parse_entries(contents),
      vec![
        (12_isize, "alacritty".to_string()),
        (56_isize, "Code".to_string()),
      ]
    );
  }

  #[test]
  fn unchanged_set_does_not_rewrite_the_journal() {
    let path = temp_journal();
    let mut journal = CloakJournal::at(path.clone());

    let entries = vec![(7_isize, "alacritty".to_string())];

    journal.record(entries.clone());
    assert_eq!(journal.writes(), 1);

    journal.record(entries.clone());
    assert_eq!(journal.writes(), 1);

    journal.record(vec![(8_isize, "alacritty".to_string())]);
    assert_eq!(journal.writes(), 2);

    let _ = std::fs::remove_dir_all(path.parent().unwrap_or(&path));
  }

  #[test]
  fn serialization_uses_tab_separated_lines() {
    let entries = vec![(1_isize, "a".to_string()), (2, "b".to_string())];

    assert_eq!(serialize_entries(&entries), "1\ta\n2\tb\n");
  }

  #[test]
  fn recovering_a_missing_journal_is_a_no_op() {
    assert_eq!(CloakJournal::recover_at(&temp_journal()), 0);
  }
}
