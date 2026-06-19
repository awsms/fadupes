use heed::types::{SerdeJson, Str};
use heed::{CompactionOption, Database, Env, EnvFlags, EnvOpenOptions, WithoutTls};
use hound::WavReader;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use rayon::prelude::*;
use serde::de::Deserializer;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs::File;
use std::fs::read_link;
use std::io::ErrorKind;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};
use walkdir::WalkDir;

#[derive(Clone, Debug)]
pub enum SizeFilter {
    Lt(u64),
    Gt(u64),
    Range(u64, u64), // inclusive
}

impl SizeFilter {
    pub fn should_ignore(&self, bytes: u64) -> bool {
        match *self {
            SizeFilter::Lt(n) => bytes < n,
            SizeFilter::Gt(n) => bytes > n,
            SizeFilter::Range(a, b) => bytes >= a && bytes <= b,
        }
    }
}

pub fn parse_size_filter(s: &str) -> Result<SizeFilter, String> {
    let s = s.trim();

    // range: "3MB..800MB"
    if let Some((a, b)) = s.split_once("..") {
        let a = parse_size_bytes(a.trim())?;
        let b = parse_size_bytes(b.trim())?;
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        return Ok(SizeFilter::Range(lo, hi));
    }

    // "<3MB" or ">800MB"
    let (op, rest) = s.split_at(1);
    let n = parse_size_bytes(rest.trim())?;
    match op {
        "<" => Ok(SizeFilter::Lt(n)),
        ">" => Ok(SizeFilter::Gt(n)),
        _ => Err(
            "expected '<', '>', or '..' range (examples: \"<3MB\", \">800MB\", \"3MB..800MB\")"
                .into(),
        ),
    }
}

fn parse_size_bytes(s: &str) -> Result<u64, String> {
    let s = s.trim();

    // Split into number + suffix
    let mut i = 0usize;
    for (idx, ch) in s.char_indices() {
        if ch.is_ascii_digit() || ch == '.' {
            i = idx + ch.len_utf8();
        } else {
            break;
        }
    }
    if i == 0 {
        return Err(format!("missing number in \"{s}\""));
    }

    let num_str = &s[..i];
    let unit_str = s[i..].trim().to_ascii_lowercase();

    let value: f64 = num_str
        .parse()
        .map_err(|_| format!("bad number \"{num_str}\""))?;

    let mult: f64 = match unit_str.as_str() {
        "" | "b" => 1.0,
        "kb" | "k" => 1024.0,
        "mb" | "m" => 1024.0 * 1024.0,
        "gb" | "g" => 1024.0 * 1024.0 * 1024.0,
        _ => return Err(format!("unknown unit \"{unit_str}\" (use B/KB/MB/GB)")),
    };

    let bytes = value * mult;
    if !bytes.is_finite() || bytes < 0.0 {
        return Err(format!("invalid size \"{s}\""));
    }
    Ok(bytes.round() as u64)
}

// Fallback RMS value used when data is missing or non-finite
fn default_rms_db_level() -> f64 {
    -1000.0
}

fn deserialize_rms_db_level<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: Deserializer<'de>,
{
    // Accept null/missing and clamp non-finite to the fallback
    let val = Option::<f64>::deserialize(deserializer)?;
    let v = val.unwrap_or_else(default_rms_db_level);
    if v.is_finite() {
        Ok(v)
    } else {
        Ok(default_rms_db_level())
    }
}

fn clean_rms_db_level(v: f64) -> f64 {
    if v.is_finite() {
        v
    } else {
        default_rms_db_level()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioFile {
    pub file_path: String,
    pub total_samples: u64,
    pub sample_rate: u32,
    pub bit_depth: u32,
    pub channels: u32,
    pub peak_level: f32,
    #[serde(
        default = "default_rms_db_level",
        deserialize_with = "deserialize_rms_db_level"
    )]
    pub rms_db_level: f64,
    pub file_size: u64,
    pub modified_secs: u64,
}

impl Default for AudioFile {
    fn default() -> Self {
        Self {
            file_path: String::default(),
            total_samples: 0,
            sample_rate: 0,
            bit_depth: 0,
            channels: 0,
            peak_level: 0.0,
            rms_db_level: 0.0,
            file_size: 0,
            modified_secs: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CachedEntry {
    pub analysis_version: u32,
    pub total_samples: u64,
    pub sample_rate: u32,
    pub bit_depth: u32,
    pub channels: u32,
    pub peak_level: f32,
    pub rms_db_level: f64,
    pub file_size: u64,
    pub modified_secs: u64,
}

#[derive(Debug, Deserialize)]
struct CurrentCachedEntry {
    #[serde(default)]
    analysis_version: u32,
    total_samples: u64,
    sample_rate: u32,
    bit_depth: u32,
    channels: u32,
    peak_level: f32,
    #[serde(
        default = "default_rms_db_level",
        deserialize_with = "deserialize_rms_db_level"
    )]
    rms_db_level: f64,
    file_size: u64,
    modified_secs: u64,
}

#[derive(Debug, Deserialize)]
struct LegacyCachedEntry {
    audio_file: AudioFile,
    file_size: u64,
    modified_secs: u64,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum CachedEntryFormat {
    Current(CurrentCachedEntry),
    Legacy(LegacyCachedEntry),
}

impl CachedEntry {
    pub fn from_audio_file(audio_file: &AudioFile, file_size: u64, modified_secs: u64) -> Self {
        Self {
            analysis_version: CURRENT_ANALYSIS_VERSION,
            total_samples: audio_file.total_samples,
            sample_rate: audio_file.sample_rate,
            bit_depth: audio_file.bit_depth,
            channels: audio_file.channels,
            peak_level: audio_file.peak_level,
            rms_db_level: audio_file.rms_db_level,
            file_size,
            modified_secs,
        }
    }

    pub fn to_audio_file(&self, file_path: String) -> AudioFile {
        AudioFile {
            file_path,
            total_samples: self.total_samples,
            sample_rate: self.sample_rate,
            bit_depth: self.bit_depth,
            channels: self.channels,
            peak_level: self.peak_level,
            rms_db_level: self.rms_db_level,
            file_size: self.file_size,
            modified_secs: self.modified_secs,
        }
    }

    fn is_valid_for(&self, file_size: u64, modified_secs: u64) -> bool {
        self.analysis_version == CURRENT_ANALYSIS_VERSION
            && self.file_size == file_size
            && self.modified_secs == modified_secs
    }
}

impl From<CurrentCachedEntry> for CachedEntry {
    fn from(entry: CurrentCachedEntry) -> Self {
        Self {
            analysis_version: entry.analysis_version,
            total_samples: entry.total_samples,
            sample_rate: entry.sample_rate,
            bit_depth: entry.bit_depth,
            channels: entry.channels,
            peak_level: entry.peak_level,
            rms_db_level: entry.rms_db_level,
            file_size: entry.file_size,
            modified_secs: entry.modified_secs,
        }
    }
}

impl From<LegacyCachedEntry> for CachedEntry {
    fn from(entry: LegacyCachedEntry) -> Self {
        Self {
            analysis_version: 0,
            total_samples: entry.audio_file.total_samples,
            sample_rate: entry.audio_file.sample_rate,
            bit_depth: entry.audio_file.bit_depth,
            channels: entry.audio_file.channels,
            peak_level: entry.audio_file.peak_level,
            rms_db_level: entry.audio_file.rms_db_level,
            file_size: entry.file_size,
            modified_secs: entry.modified_secs,
        }
    }
}

impl<'de> Deserialize<'de> for CachedEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match CachedEntryFormat::deserialize(deserializer)? {
            CachedEntryFormat::Current(entry) => entry.into(),
            CachedEntryFormat::Legacy(entry) => entry.into(),
        })
    }
}

#[derive(Debug, Clone, Eq, Hash, PartialEq)]
enum FileIdentity {
    #[cfg(unix)]
    Unix { dev: u64, ino: u64 },
    #[cfg(not(unix))]
    CanonicalPath(PathBuf),
}

impl FileIdentity {
    fn from_path(_path: &Path, metadata: &std::fs::Metadata) -> Option<Self> {
        #[cfg(unix)]
        {
            Some(Self::Unix {
                dev: metadata.dev(),
                ino: metadata.ino(),
            })
        }

        #[cfg(not(unix))]
        {
            std::fs::canonicalize(_path).ok().map(Self::CanonicalPath)
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SeenFiles {
    data: Arc<Mutex<HashSet<FileIdentity>>>,
}

impl SeenFiles {
    pub fn new() -> Self {
        Self::default()
    }

    fn insert(&self, identity: FileIdentity) -> bool {
        self.data
            .lock()
            .map(|mut seen| seen.insert(identity))
            .unwrap_or(true)
    }
}

fn symlink_target_is_inside_scanned_input(
    symlink_target: &Path,
    scanned_dirs: &HashSet<PathBuf>,
) -> bool {
    scanned_dirs.iter().any(|root| symlink_target.starts_with(root))
}

type StateDb = Database<Str, SerdeJson<CachedEntry>>;
pub type StateResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const STATE_DB_NAME: &str = "audio";
const STATE_DB_EXTENSION: &str = "mdb";
const LEGACY_JSON_EXTENSION: &str = "json";
const STATE_MAP_SIZE: usize = 1024 * 1024 * 1024;
const CURRENT_ANALYSIS_VERSION: u32 = 1;

#[derive(Debug, Clone)]
pub struct StatePaths {
    pub db_path: PathBuf,
    pub legacy_json_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ResumeCache {
    pub path: PathBuf,
    pub data: Arc<Mutex<HashMap<String, CachedEntry>>>,
    pub save_every: usize,
    pub pending: Arc<AtomicUsize>,
    env: Option<Env<WithoutTls>>,
    db: Option<StateDb>,
    save_lock: Arc<Mutex<()>>,
    save_on_drop: bool,
}

impl ResumeCache {
    pub fn resolve_state_paths(path: PathBuf) -> StatePaths {
        let is_json = path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case(LEGACY_JSON_EXTENSION));

        let db_path = if is_json {
            path.with_extension(STATE_DB_EXTENSION)
        } else {
            path.clone()
        };
        let legacy_json_path = if is_json {
            path
        } else {
            db_path.with_extension(LEGACY_JSON_EXTENSION)
        };

        StatePaths {
            db_path,
            legacy_json_path,
        }
    }

    pub fn load(path: PathBuf, save_every: usize) -> Self {
        Self::load_with_options(path, save_every, true, true, true)
    }

    pub fn load_read_only(path: PathBuf, save_every: usize) -> Self {
        Self::load_with_options(path, save_every, false, false, false)
    }

    fn load_with_options(
        path: PathBuf,
        save_every: usize,
        save_on_drop: bool,
        backup_on_error: bool,
        allow_writes: bool,
    ) -> Self {
        let paths = Self::resolve_state_paths(path);
        let mut data = HashMap::new();
        let mut env = None;
        let mut db = None;

        match Self::open_db(&paths.db_path, allow_writes) {
            Ok(Some((opened_env, opened_db))) => {
                if allow_writes {
                    if let Err(err) = Self::migrate_legacy_json(
                        &opened_env,
                        opened_db,
                        &paths.legacy_json_path,
                        backup_on_error,
                    ) {
                        eprintln!(
                            "Warning: failed to migrate legacy state file {}: {err}",
                            paths.legacy_json_path.display()
                        );
                    }
                }
                env = Some(opened_env);
                db = Some(opened_db);
            }
            Ok(None) => {
                if !allow_writes {
                    data = Self::load_legacy_json(&paths.legacy_json_path, backup_on_error)
                        .unwrap_or_default();
                }
            }
            Err(err) => {
                eprintln!(
                    "Warning: failed to open state database {}: {err}. Starting with empty state.",
                    paths.db_path.display()
                );
            }
        }

        ResumeCache {
            path: paths.db_path,
            data: Arc::new(Mutex::new(data)),
            save_every,
            pending: Arc::new(AtomicUsize::new(0)),
            env,
            db,
            save_lock: Arc::new(Mutex::new(())),
            save_on_drop,
        }
    }

    fn open_db(path: &Path, allow_writes: bool) -> StateResult<Option<(Env<WithoutTls>, StateDb)>> {
        if allow_writes {
            std::fs::create_dir_all(path)?;
        } else if !path.is_dir() {
            return Ok(None);
        }

        let mut options = EnvOpenOptions::new().read_txn_without_tls();
        options.map_size(STATE_MAP_SIZE).max_dbs(2);
        if !allow_writes {
            // Dry-run/query reads should not need write access to the LMDB environment.
            unsafe {
                options.flags(EnvFlags::READ_ONLY);
            }
        }
        // Heed wraps LMDB's memory map. The path is owned by fadupes and all access goes through Heed.
        let env = unsafe { options.open(path)? };

        let db = if allow_writes {
            let mut wtxn = env.write_txn()?;
            let db = env.create_database(&mut wtxn, Some(STATE_DB_NAME))?;
            wtxn.commit()?;
            db
        } else {
            let rtxn = env.read_txn()?;
            let Some(db) = env.open_database(&rtxn, Some(STATE_DB_NAME))? else {
                return Ok(None);
            };
            rtxn.commit()?;
            db
        };

        Ok(Some((env, db)))
    }

    fn load_legacy_json(
        path: &Path,
        backup_on_error: bool,
    ) -> Option<HashMap<String, CachedEntry>> {
        match std::fs::File::open(path) {
            Ok(file) => match serde_json::from_reader::<_, HashMap<String, CachedEntry>>(file) {
                Ok(map) => Some(map),
                Err(err) => {
                    eprintln!(
                        "Warning: failed to parse legacy state file {}: {err}. Starting with empty state.",
                        path.display()
                    );
                    if backup_on_error {
                        backup_broken(path, &format!("{err}"));
                    }
                    None
                }
            },
            Err(err) if err.kind() == ErrorKind::NotFound => None,
            Err(err) => {
                eprintln!(
                    "Warning: failed to open legacy state file {}: {err}. Starting with empty state.",
                    path.display()
                );
                if backup_on_error {
                    backup_broken(path, &format!("{err}"));
                }
                None
            }
        }
    }

    fn migrate_legacy_json(
        env: &Env<WithoutTls>,
        db: StateDb,
        legacy_path: &Path,
        backup_on_error: bool,
    ) -> StateResult<()> {
        if !legacy_path.exists() {
            return Ok(());
        }

        let rtxn = env.read_txn()?;
        let db_is_empty = db.is_empty(&rtxn)?;
        drop(rtxn);
        if !db_is_empty {
            return Ok(());
        }

        let Some(entries) = Self::load_legacy_json(legacy_path, backup_on_error) else {
            return Ok(());
        };
        if entries.is_empty() {
            return Ok(());
        }

        let mut wtxn = env.write_txn()?;
        for (file_path, entry) in &entries {
            db.put(&mut wtxn, file_path, entry)?;
        }
        wtxn.commit()?;

        let migrated_path = legacy_path.with_extension("json.migrated");
        match std::fs::rename(legacy_path, &migrated_path) {
            Ok(_) => eprintln!(
                "Migrated legacy state file {} to {} entries in {}. Old JSON moved to {}.",
                legacy_path.display(),
                entries.len(),
                env.path().display(),
                migrated_path.display()
            ),
            Err(err) => eprintln!(
                "Warning: migrated legacy state file {} but failed to move it to {}: {}",
                legacy_path.display(),
                migrated_path.display(),
                err
            ),
        }

        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    // Cache entry is valid only if size + modified time match (cheap change detector)
    pub fn lookup(
        &self,
        file_path: &Path,
        file_size: u64,
        modified_secs: u64,
    ) -> Option<AudioFile> {
        let path_key = file_path.to_string_lossy().to_string();
        if let Some(audio_file) = self.data.lock().ok().and_then(|map| {
            map.get(&path_key).and_then(|entry| {
                if entry.is_valid_for(file_size, modified_secs) {
                    Some(entry.to_audio_file(path_key.clone()))
                } else {
                    None
                }
            })
        }) {
            return Some(audio_file);
        }

        let env = self.env.as_ref()?;
        let db = self.db?;
        let rtxn = env.read_txn().ok()?;
        db.get(&rtxn, &path_key).ok().flatten().and_then(|entry| {
            if entry.is_valid_for(file_size, modified_secs) {
                Some(entry.to_audio_file(path_key))
            } else {
                None
            }
        })
    }

    pub fn store(&self, audio_file: AudioFile, file_size: u64, modified_secs: u64) {
        if let Ok(mut map) = self.data.lock() {
            map.insert(
                audio_file.file_path.clone(),
                CachedEntry::from_audio_file(&audio_file, file_size, modified_secs),
            );
        }

        // Throttle disk writes: save cache every 'save_every' inserts (AtomicUsize so threads coordinate cheaply)
        let count = self.pending.fetch_add(1, Ordering::Relaxed) + 1;
        if count >= self.save_every {
            // Reset the counter before saving so new inserts can keep counting while we write
            self.pending.store(0, Ordering::Relaxed);
            let _ = self.save();
        }
    }

    pub fn save(&self) -> StateResult<()> {
        let _lock = self.save_lock.lock().unwrap();
        let Some(env) = self.env.as_ref() else {
            return Ok(());
        };
        let Some(db) = self.db else {
            return Ok(());
        };

        let pending_entries = {
            let map = self.data.lock().unwrap();
            if map.is_empty() {
                self.pending.store(0, Ordering::Relaxed);
                return Ok(());
            }
            map.clone()
        };

        let mut wtxn = env.write_txn()?;
        for (file_path, entry) in &pending_entries {
            db.put(&mut wtxn, file_path, entry)?;
        }
        wtxn.commit()?;

        let mut map = self.data.lock().unwrap();
        for file_path in pending_entries.keys() {
            map.remove(file_path);
        }
        self.pending.store(0, Ordering::Relaxed);
        Ok(())
    }

    pub fn all_audio_files(&self) -> StateResult<Vec<AudioFile>> {
        let mut files = Vec::new();
        let mut seen_paths = HashSet::new();

        if let (Some(env), Some(db)) = (self.env.as_ref(), self.db) {
            let rtxn = env.read_txn()?;
            for result in db.iter(&rtxn)? {
                let (file_path, entry) = result?;
                seen_paths.insert(file_path.to_string());
                files.push(entry.to_audio_file(file_path.to_string()));
            }
        }

        let map = self.data.lock().unwrap();
        for (file_path, entry) in map.iter() {
            if seen_paths.insert(file_path.clone()) {
                files.push(entry.to_audio_file(file_path.clone()));
            }
        }

        Ok(files)
    }

    pub fn cleanup_missing(&self, roots: &[PathBuf], dry_run: bool) -> StateResult<CleanupReport> {
        let mut checked_entries = 0usize;
        let mut stale_keys = Vec::new();

        if let (Some(env), Some(db)) = (self.env.as_ref(), self.db) {
            let rtxn = env.read_txn()?;
            for result in db.iter(&rtxn)? {
                let (file_path, _) = result?;
                if cleanup_path_is_checked(file_path, roots) {
                    checked_entries += 1;
                    if !Path::new(file_path).is_file() {
                        stale_keys.push(file_path.to_string());
                    }
                }
            }
        }

        {
            let map = self.data.lock().unwrap();
            for file_path in map.keys() {
                if cleanup_path_is_checked(file_path, roots) {
                    checked_entries += 1;
                    if !Path::new(file_path).is_file() {
                        stale_keys.push(file_path.clone());
                    }
                }
            }
        }

        stale_keys.sort_unstable();
        stale_keys.dedup();

        let stale_entries = stale_keys.len();
        if !dry_run && stale_entries > 0 {
            if let (Some(env), Some(db)) = (self.env.as_ref(), self.db) {
                let mut wtxn = env.write_txn()?;
                for key in &stale_keys {
                    db.delete(&mut wtxn, key)?;
                }
                wtxn.commit()?;
            }

            let mut map = self.data.lock().unwrap();
            for key in &stale_keys {
                map.remove(key);
            }
        }

        Ok(CleanupReport {
            checked_entries,
            stale_entries,
            stale_paths: stale_keys,
        })
    }

    pub fn compact(self) -> StateResult<Option<CompactReport>> {
        self.save()?;

        let Some(env) = self.env.as_ref() else {
            return Ok(None);
        };
        if self.db.is_none() {
            return Ok(None);
        }

        let data_path = self.path.join("data.mdb");
        if !data_path.is_file() {
            return Ok(None);
        }

        let tmp_path = self.path.join("data.mdb.compact");
        let backup_path = self.path.join("data.mdb.precompact");
        let before_bytes = std::fs::metadata(&data_path)?.len();
        let compacted = env.copy_to_path(&tmp_path, CompactionOption::Enabled)?;
        compacted.sync_all()?;
        drop(compacted);
        let after_bytes = std::fs::metadata(&tmp_path)?.len();

        let db_path = self.path.clone();
        drop(self);

        if backup_path.exists() {
            std::fs::remove_file(&backup_path)?;
        }
        std::fs::rename(&data_path, &backup_path)?;
        if let Err(err) = std::fs::rename(&tmp_path, &data_path) {
            let _ = std::fs::rename(&backup_path, &data_path);
            return Err(Box::new(err));
        }
        std::fs::remove_file(&backup_path)?;

        Ok(Some(CompactReport {
            db_path,
            before_bytes,
            after_bytes,
        }))
    }
}

impl Drop for ResumeCache {
    fn drop(&mut self) {
        if self.save_on_drop {
            let _ = self.save();
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CleanupReport {
    pub checked_entries: usize,
    pub stale_entries: usize,
    pub stale_paths: Vec<String>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CompactReport {
    pub db_path: PathBuf,
    pub before_bytes: u64,
    pub after_bytes: u64,
}

fn cleanup_path_is_checked(file_path: &str, roots: &[PathBuf]) -> bool {
    let path = Path::new(file_path);
    roots.is_empty() || roots.iter().any(|root| path.starts_with(root))
}

#[derive(Debug, Clone)]
struct AudioCandidate {
    path: PathBuf,
    file_size: u64,
    modified_secs: u64,
}

impl AudioFile {
    // Shared helper: decide if an entry should be skipped (unique size) or served from cache.
    fn skip_or_cached(
        path: &Path,
        size: u64,
        modified_secs: u64,
        skip_unique_size: bool,
        size_counts: Option<&HashMap<u64, usize>>,
        resume_cache: Option<&Arc<ResumeCache>>,
    ) -> (bool, Option<AudioFile>) {
        let is_unique_skip = skip_unique_size
            && size_counts
                .and_then(|map| map.get(&size))
                .copied()
                .unwrap_or(0)
                <= 1;

        let cached = resume_cache.and_then(|cache| cache.lookup(path, size, modified_secs));

        (is_unique_skip, cached)
    }

    // Walk through the directory to find audio files (FLAC and WAV) in parallel with progress bar
    pub fn walk_dir(
        dir: &PathBuf,
        scanned_dirs: &HashSet<PathBuf>,
        list_files: bool,
        skip_unique_size: bool,
        ignore_symlinks: bool,
        resume_cache: Option<Arc<ResumeCache>>,
        ignore_size: Option<&SizeFilter>,
        seen_files: SeenFiles,
    ) -> Vec<AudioFile> {
        // Lazily open the error log only if we hit an error (shared across threads via Mutex<Option<File>>)
        let error_log_file: Arc<Mutex<Option<File>>> = Arc::new(Mutex::new(None));

        // Collect the list of audio files to process
        // Build the full candidate list up front; we need it to compute unique-size skips
        // and to seed the progress bar with already-cached or skipped entries on resume.
        let files_to_process: Vec<AudioCandidate> = WalkDir::new(dir)
            .follow_links(!ignore_symlinks) // Follow symlinks by default; skip loop-back symlinks into input roots (and skip all symlinks if --nosym is set)
            .sort_by_file_name()
            .into_iter()
            .filter_entry(|entry| {
                if !entry.path_is_symlink() {
                    return true;
                }

                if ignore_symlinks {
                    return false;
                }

                if let Ok(symlink_target) =
                    std::fs::canonicalize(entry.path()).or_else(|_| read_link(entry.path()))
                    && symlink_target_is_inside_scanned_input(&symlink_target, scanned_dirs)
                {
                    eprintln!(
                        "Skipping symlink pointing inside a scanned input: {}",
                        entry.path().display()
                    );
                    return false;
                }

                true
            })
            .filter_map(|e| e.ok())
            .filter_map(|f| {
                let path = f.path();

                let file_path = if f.path_is_symlink() {
                    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
                } else {
                    path.to_path_buf()
                };

                let Ok(metadata) = std::fs::metadata(&file_path) else {
                    return None;
                };
                if !metadata.is_file() {
                    return None;
                }

                let size = metadata.len();
                // Apply optional ignore filter from --ignore-size
                if ignore_size.is_some_and(|flt| flt.should_ignore(size)) {
                    return None;
                }

                let size_ok = metadata.len() <= 800 * 1024 * 1024; // Check if file is <= 800MB

                // Filter by file extension (flac or wav) and file size
                let Some(extension) = file_path.extension() else {
                    return None;
                };

                if (extension == "flac" || extension == "wav") && size_ok {
                    let modified_secs = metadata
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let identity = FileIdentity::from_path(&file_path, &metadata)?;
                    if !seen_files.insert(identity) {
                        return None;
                    }

                    Some(AudioCandidate {
                        path: file_path,
                        file_size: size,
                        modified_secs,
                    })
                } else {
                    None
                }
            })
            .collect();

        // Precompute size counts if we need to skip unique sizes
        let size_counts = if skip_unique_size {
            let mut counts = std::collections::HashMap::new();
            for candidate in &files_to_process {
                *counts.entry(candidate.file_size).or_insert(0usize) += 1;
            }
            Some(counts)
        } else {
            None
        };

        // Count how many entries are already satisfied (cached) or will be skipped (unique size)
        let initial_processed = files_to_process
            .iter()
            .filter(|candidate| {
                let (is_unique_skip, cached) = Self::skip_or_cached(
                    &candidate.path,
                    candidate.file_size,
                    candidate.modified_secs,
                    skip_unique_size,
                    size_counts.as_ref(),
                    resume_cache.as_ref(),
                );

                is_unique_skip || cached.is_some()
            })
            .count();

        let total_files = files_to_process.len();

        let (progress_bar, list_mp) = if list_files {
            let mp = Arc::new(MultiProgress::new());
            let total_pb = mp.add(ProgressBar::new(total_files as u64));
            total_pb.set_style(
                ProgressStyle::with_template("Total Progress: [{wide_bar}] {pos}/{len} ({eta})")
                    .expect("Failed to create general progress bar template")
                    .progress_chars("#>-"),
            );
            (total_pb, Some(mp))
        } else {
            let pb = ProgressBar::new(total_files as u64);
            pb.set_style(
                ProgressStyle::with_template("Total Progress: [{wide_bar}] {pos}/{len} ({eta})")
                    .expect("Failed to create general progress bar template")
                    .progress_chars("#>-"),
            );
            (pb, None)
        };
        // Seed the progress bar with pre-accounted work so resume shows correct totals.
        progress_bar.set_position(initial_processed as u64);

        let audio_files: Vec<AudioFile> = if list_files {
            let start_counter = Arc::new(AtomicUsize::new(initial_processed));
            // Limiti UI noise, cap to <= 8 spinner lines and reuse them by assigning files round-robin to a "slot"
            let max_bars = std::cmp::max(1, std::cmp::min(rayon::current_num_threads(), 8));
            let list_bars: Arc<Vec<ProgressBar>> = Arc::new(
                (0..max_bars)
                    .map(|_| {
                        let pb = list_mp
                            .as_ref()
                            .expect("list_mp must exist when list_files is true")
                            .add(ProgressBar::new_spinner());
                        pb.set_style(
                            ProgressStyle::with_template("{spinner} {msg}")
                                .expect("Failed to create file progress bar template")
                                .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
                        );
                        pb.enable_steady_tick(Duration::from_millis(100));
                        pb
                    })
                    .collect(),
            );
            files_to_process
                .par_iter()
                .filter_map(|candidate| {
                    let path_str = candidate.path.to_string_lossy().to_string();
                    let progress = progress_bar.clone();

                    let (is_unique_skip, cached) = Self::skip_or_cached(
                        &candidate.path,
                        candidate.file_size,
                        candidate.modified_secs,
                        skip_unique_size,
                        size_counts.as_ref(),
                        resume_cache.as_ref(),
                    );
                    let already_processed = is_unique_skip || cached.is_some();

                    if is_unique_skip {
                        if let Some(ref mp) = list_mp {
                            let _ = mp.println(format!(
                                "Skipping unique-size file: {}",
                                candidate.path.display()
                            ));
                        }
                        return None;
                    }

                    if let Some(audio_file) = cached {
                        if !already_processed {
                            progress.inc(1);
                        }
                        return Some(audio_file);
                    }

                    let start_order = start_counter.fetch_add(1, Ordering::Relaxed) + 1;
                    let per_file_pb = {
                        let bars = Arc::clone(&list_bars);
                        let slot = (start_order - 1) % max_bars;
                        let pb = &bars[slot];
                        pb.set_message(format!("[{}/{}] {}", start_order, total_files, path_str));
                        Some(pb.clone())
                    };

                    let result = match AudioFile::process_audio_path(&candidate.path) {
                        Ok(mut audio_file) => {
                            audio_file.file_size = candidate.file_size;
                            audio_file.modified_secs = candidate.modified_secs;
                            if let Some(cache) = resume_cache.as_ref() {
                                cache.store(
                                    audio_file.clone(),
                                    candidate.file_size,
                                    candidate.modified_secs,
                                );
                            }
                            Some(audio_file)
                        }
                        Err(err) => {
                            let error_message =
                                format!("Error processing file: {}: {:?}", path_str, err);
                            eprintln!("{}", error_message);
                            let mut error_log = error_log_file.lock().unwrap();
                            if error_log.is_none() {
                                *error_log = Some(
                                    std::fs::OpenOptions::new()
                                        .create(true)
                                        .append(true)
                                        .open("identical_files_errors.log")
                                        .expect("Unable to open error log file"),
                                );
                            }
                            if let Some(file) = error_log.as_mut() {
                                writeln!(file, "{}", error_message)
                                    .expect("Failed to write to error log file");
                            }
                            None
                        }
                    };

                    if !already_processed {
                        progress.inc(1);
                    }

                    if let Some(pb) = per_file_pb {
                        pb.set_message(String::new());
                    }

                    result
                })
                .collect()
        } else {
            files_to_process
                .par_iter()
                .filter_map(|candidate| {
                    let path_str = candidate.path.to_string_lossy().to_string();
                    let progress = progress_bar.clone();

                    let (is_unique_skip, cached) = Self::skip_or_cached(
                        &candidate.path,
                        candidate.file_size,
                        candidate.modified_secs,
                        skip_unique_size,
                        size_counts.as_ref(),
                        resume_cache.as_ref(),
                    );
                    let already_processed = is_unique_skip || cached.is_some();

                    if is_unique_skip {
                        return None;
                    }

                    if let Some(audio_file) = cached {
                        if !already_processed {
                            progress.inc(1);
                        }
                        return Some(audio_file);
                    }

                    let result = match AudioFile::process_audio_path(&candidate.path) {
                        Ok(mut audio_file) => {
                            audio_file.file_size = candidate.file_size;
                            audio_file.modified_secs = candidate.modified_secs;
                            if let Some(cache) = resume_cache.as_ref() {
                                cache.store(
                                    audio_file.clone(),
                                    candidate.file_size,
                                    candidate.modified_secs,
                                );
                            }
                            Some(audio_file)
                        }
                        Err(err) => {
                            let error_message =
                                format!("Error processing file: {}: {:?}", path_str, err);
                            eprintln!("{}", error_message);
                            let mut error_log = error_log_file.lock().unwrap();
                            if error_log.is_none() {
                                *error_log = Some(
                                    std::fs::OpenOptions::new()
                                        .create(true)
                                        .append(true)
                                        .open("identical_files_errors.log")
                                        .expect("Unable to open error log file"),
                                );
                            }
                            if let Some(file) = error_log.as_mut() {
                                writeln!(file, "{}", error_message)
                                    .expect("Failed to write to error log file");
                            }
                            None
                        }
                    };

                    if !already_processed {
                        progress.inc(1);
                    }
                    result
                })
                .collect()
        };

        if let Some(cache) = resume_cache.as_ref() {
            let _ = cache.save();
        }

        progress_bar.finish_with_message("All files processed");
        audio_files
    }

    // Process individual audio files (FLAC and WAV)
    pub fn process_audio_file(entry: &walkdir::DirEntry) -> Result<AudioFile, ProcessError> {
        Self::process_audio_path(entry.path())
    }

    pub fn process_audio_path(path: &Path) -> Result<AudioFile, ProcessError> {
        let extension = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");
        let mut audio_file = AudioFile {
            file_path: path.to_string_lossy().to_string(), // Store the full path
            ..Default::default()
        };

        match extension {
            "flac" => {
                let mut reader = Self::load_flac(path)?;
                let stream_info = reader.streaminfo();
                let total_samples = stream_info.samples.ok_or(ProcessError::NoSamplesFound)?;
                audio_file.total_samples = total_samples;
                audio_file.sample_rate = stream_info.sample_rate;
                audio_file.bit_depth = stream_info.bits_per_sample;
                audio_file.channels = stream_info.channels;

                let (peak_level, rms_db_level) = Self::accumulate_metrics(
                    reader.samples().map(|sample| sample.unwrap_or(0)),
                    stream_info.bits_per_sample as i32,
                );
                audio_file.peak_level = peak_level;
                audio_file.rms_db_level = clean_rms_db_level(rms_db_level);
            }
            "wav" => {
                let mut reader = WavReader::open(path).map_err(|_| ProcessError::NonFlacError)?;
                let spec = reader.spec();
                audio_file.total_samples = reader.duration() as u64;
                audio_file.sample_rate = spec.sample_rate;
                audio_file.bit_depth = spec.bits_per_sample as u32;
                audio_file.channels = spec.channels as u32;

                // Read with the correct sample width so 24/32-bit WAVs are handled correctly
                let (peak_level, rms_db_level) = match spec.bits_per_sample {
                    8 => Self::accumulate_metrics(
                        reader.samples::<i8>().map(|s| s.unwrap_or(0) as i32),
                        8,
                    ),
                    16 => Self::accumulate_metrics(
                        reader.samples::<i16>().map(|s| s.unwrap_or(0) as i32),
                        16,
                    ),
                    24 | 32 => Self::accumulate_metrics(
                        reader.samples::<i32>().map(|s| s.unwrap_or(0)),
                        spec.bits_per_sample as i32,
                    ),
                    _ => return Err(ProcessError::UnsupportedBitDepth),
                };
                audio_file.peak_level = peak_level;
                audio_file.rms_db_level = clean_rms_db_level(rms_db_level);
            }
            _ => return Err(ProcessError::UnsupportedBitDepth),
        }

        Ok(audio_file)
    }

    // Single-pass over samples: compute peak + RMS(dB). Empty input => fallback dB to avoid log10(0)
    fn accumulate_metrics<I>(samples: I, bit_depth: i32) -> (f32, f64)
    where
        I: Iterator<Item = i32>,
    {
        let max_amplitude = Self::get_max_amplitude(bit_depth) as f64;
        if max_amplitude <= 0.0 {
            return (0.0, default_rms_db_level());
        }

        let mut max_abs = 0i32;
        let mut squared_sum = 0f64;
        let mut count = 0u64;

        for sample in samples {
            let abs = sample.abs() as i32;
            if abs > max_abs {
                max_abs = abs;
            }

            let normalized = sample as f64 / max_amplitude;
            squared_sum += normalized * normalized;
            count += 1;
        }

        let peak_level = if max_abs == 0 {
            0.0
        } else {
            max_abs as f32 / Self::get_max_amplitude(bit_depth) as f32
        };

        let rms_db_level = if count == 0 {
            default_rms_db_level()
        } else {
            let rms_amplitude = (squared_sum / count as f64).sqrt();
            if rms_amplitude > 0.0 {
                20.0 * rms_amplitude.log10()
            } else {
                default_rms_db_level()
            }
        };

        (peak_level, rms_db_level)
    }

    fn get_max_amplitude(bit_depth: i32) -> i32 {
        match bit_depth {
            8 => i8::MAX as i32,
            16 => i16::MAX as i32,
            24 => (1 << 23) - 1,
            32 => i32::MAX,
            _ => 0,
        }
    }

    fn load_flac(path: &Path) -> Result<claxon::FlacReader<File>, ProcessError> {
        let flac_file = File::open(path)?;
        let reader = claxon::FlacReader::new(flac_file)?;
        Ok(reader)
    }
}

#[derive(Debug)]
pub enum ProcessError {
    IoError(std::io::Error),
    FlacError(claxon::Error),
    NonFlacError,
    NoSamplesFound,
    UnsupportedBitDepth,
}

impl std::fmt::Display for ProcessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProcessError::IoError(err) => write!(f, "IO error: {}", err),
            ProcessError::FlacError(err) => write!(f, "FLAC error: {}", err),
            ProcessError::NonFlacError => write!(f, "Unsupported non-FLAC file found"),
            ProcessError::NoSamplesFound => write!(f, "No samples found"),
            ProcessError::UnsupportedBitDepth => write!(f, "Unsupported bit depth"),
        }
    }
}

impl From<std::io::Error> for ProcessError {
    fn from(err: std::io::Error) -> ProcessError {
        ProcessError::IoError(err)
    }
}

impl From<claxon::Error> for ProcessError {
    fn from(err: claxon::Error) -> ProcessError {
        ProcessError::FlacError(err)
    }
}

fn backup_broken(path: &Path, reason: &str) {
    let broken = if let Some(ext) = path.extension() {
        let mut new_ext = OsString::from(ext);
        new_ext.push(".broken");
        path.with_extension(new_ext)
    } else {
        path.with_extension("broken")
    };

    match std::fs::rename(path, &broken) {
        Ok(_) => eprintln!(
            "State file moved to {} due to load error: {}",
            broken.display(),
            reason
        ),
        Err(err) => eprintln!(
            "Warning: failed to move state file {} to {} after error {}: {}",
            path.display(),
            broken.display(),
            reason,
            err
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_audio_file() -> AudioFile {
        AudioFile {
            file_path: "/music/example.flac".to_string(),
            total_samples: 12345,
            sample_rate: 44100,
            bit_depth: 16,
            channels: 2,
            peak_level: 0.75,
            rms_db_level: -12.5,
            file_size: 98765,
            modified_secs: 1234567890,
        }
    }

    #[test]
    fn cached_entry_serializes_without_path_or_audio_file_wrapper() {
        let audio_file = sample_audio_file();
        let entry = CachedEntry::from_audio_file(
            &audio_file,
            audio_file.file_size,
            audio_file.modified_secs,
        );

        let value = serde_json::to_value(&entry).unwrap();

        assert!(value.get("audio_file").is_none());
        assert!(value.get("file_path").is_none());
        assert_eq!(value["analysis_version"], json!(CURRENT_ANALYSIS_VERSION));
        assert_eq!(value["total_samples"], json!(12345));
        assert_eq!(value["file_size"], json!(98765));
        assert_eq!(value["modified_secs"], json!(1234567890));
    }

    #[test]
    fn cached_entry_reads_legacy_nested_format() {
        let value = json!({
            "audio_file": {
                "file_path": "/legacy/example.flac",
                "total_samples": 456,
                "sample_rate": 48000,
                "bit_depth": 24,
                "channels": 2,
                "peak_level": 0.5,
                "rms_db_level": -8.25,
                "file_size": 111,
                "modified_secs": 222
            },
            "file_size": 333,
            "modified_secs": 444
        });

        let entry: CachedEntry = serde_json::from_value(value).unwrap();
        let audio_file = entry.to_audio_file("/legacy/example.flac".to_string());

        assert_eq!(audio_file.file_path, "/legacy/example.flac");
        assert_eq!(audio_file.total_samples, 456);
        assert_eq!(audio_file.sample_rate, 48000);
        assert_eq!(audio_file.bit_depth, 24);
        assert_eq!(audio_file.file_size, 333);
        assert_eq!(audio_file.modified_secs, 444);
        assert_eq!(entry.analysis_version, 0);
    }

    #[test]
    fn lookup_rejects_stale_analysis_version() {
        let temp_dir = test_temp_dir("stale-version");
        let state_path = temp_dir.join("state.mdb");
        let audio_path = temp_dir.join("track.flac");
        std::fs::write(&audio_path, []).unwrap();

        let mut stale_entry = cached_entry_for(&audio_path);
        stale_entry.analysis_version = 0;

        let cache = ResumeCache::load(state_path, 250);
        cache
            .data
            .lock()
            .unwrap()
            .insert(audio_path.to_string_lossy().to_string(), stale_entry);

        assert!(
            cache
                .lookup(
                    &audio_path,
                    sample_audio_file().file_size,
                    sample_audio_file().modified_secs,
                )
                .is_none()
        );

        std::fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn cleanup_missing_dry_run_and_scope() {
        let temp_dir = test_temp_dir("cleanup");
        let root = temp_dir.join("root");
        let outside = temp_dir.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        let existing_path = root.join("existing.flac");
        let missing_path = root.join("missing.flac");
        let outside_missing_path = outside.join("missing.flac");
        std::fs::write(&existing_path, []).unwrap();

        let state_path = temp_dir.join("state.mdb");
        let cache = ResumeCache::load(state_path.clone(), 250);
        store_test_entry(&cache, &existing_path);
        store_test_entry(&cache, &missing_path);
        store_test_entry(&cache, &outside_missing_path);
        cache.save().unwrap();

        let dry_run = cache
            .cleanup_missing(std::slice::from_ref(&root), true)
            .unwrap();
        assert_eq!(
            dry_run,
            CleanupReport {
                checked_entries: 2,
                stale_entries: 1,
                stale_paths: vec![missing_path.to_string_lossy().to_string()],
            }
        );
        assert_eq!(cache.all_audio_files().unwrap().len(), 3);

        let cleanup = cache
            .cleanup_missing(std::slice::from_ref(&root), false)
            .unwrap();
        assert_eq!(
            cleanup,
            CleanupReport {
                checked_entries: 2,
                stale_entries: 1,
                stale_paths: vec![missing_path.to_string_lossy().to_string()],
            }
        );

        let saved = cache.all_audio_files().unwrap();
        assert!(
            saved
                .iter()
                .any(|file| file.file_path == existing_path.to_string_lossy().as_ref())
        );
        assert!(
            !saved
                .iter()
                .any(|file| file.file_path == missing_path.to_string_lossy().as_ref())
        );
        assert!(
            saved
                .iter()
                .any(|file| file.file_path == outside_missing_path.to_string_lossy().as_ref())
        );

        std::fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn read_only_load_does_not_backup_or_rewrite_broken_state() {
        let temp_dir = test_temp_dir("readonly");
        let state_path = temp_dir.join("state.json");
        let broken_path = state_path.with_extension("json.broken");
        std::fs::write(&state_path, "{not-json").unwrap();

        {
            let cache = ResumeCache::load_read_only(state_path.clone(), 250);
            assert_eq!(cache.data.lock().unwrap().len(), 0);
        }

        assert!(state_path.exists());
        assert!(!broken_path.exists());
        assert_eq!(std::fs::read_to_string(&state_path).unwrap(), "{not-json");

        std::fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn symlink_targets_inside_scanned_inputs_are_skipped() {
        let root = PathBuf::from("/music/flac");
        let mut scanned_dirs = HashSet::new();
        scanned_dirs.insert(root.clone());

        assert!(symlink_target_is_inside_scanned_input(
            &root.join("OK"),
            &scanned_dirs
        ));
        assert!(symlink_target_is_inside_scanned_input(
            &root,
            &scanned_dirs
        ));
        assert!(!symlink_target_is_inside_scanned_input(
            Path::new("/mnt/archive/OK"),
            &scanned_dirs
        ));
    }

    #[test]
    fn load_migrates_legacy_json_to_heed_database() {
        let temp_dir = test_temp_dir("migration");
        let legacy_path = temp_dir.join("state.json");
        let db_path = temp_dir.join("state.mdb");
        let audio_path = temp_dir.join("track.flac");
        let migrated_path = legacy_path.with_extension("json.migrated");

        let entries = HashMap::from([(
            audio_path.to_string_lossy().to_string(),
            cached_entry_for(&audio_path),
        )]);
        serde_json::to_writer(File::create(&legacy_path).unwrap(), &entries).unwrap();

        {
            let cache = ResumeCache::load(legacy_path.clone(), 250);
            assert_eq!(cache.path(), db_path.as_path());

            let files = cache.all_audio_files().unwrap();
            assert_eq!(files.len(), 1);
            assert_eq!(files[0].file_path, audio_path.to_string_lossy().as_ref());
        }

        assert!(db_path.is_dir());
        assert!(!legacy_path.exists());
        assert!(migrated_path.exists());

        std::fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn heed_state_accepts_long_path_keys() {
        let temp_dir = test_temp_dir("long-key");
        let state_path = temp_dir.join("state.mdb");
        let long_relative_path = std::iter::repeat_n("nested", 120)
            .collect::<Vec<_>>()
            .join("/");
        let audio_path = temp_dir.join(long_relative_path).join("track.flac");
        assert!(audio_path.to_string_lossy().len() > 511);

        let cache = ResumeCache::load(state_path, 250);
        store_test_entry(&cache, &audio_path);
        cache.save().unwrap();

        let files = cache.all_audio_files().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].file_path, audio_path.to_string_lossy().as_ref());

        std::fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn read_only_heed_state_can_iter_entries() {
        let temp_dir = test_temp_dir("readonly-heed");
        let state_path = temp_dir.join("state.mdb");
        let audio_path = temp_dir.join("track.flac");
        std::fs::write(&audio_path, []).unwrap();

        {
            let cache = ResumeCache::load(state_path.clone(), 250);
            store_test_entry(&cache, &audio_path);
            cache.save().unwrap();
        }

        let cache = ResumeCache::load_read_only(state_path, 250);
        let files = cache.all_audio_files().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].file_path, audio_path.to_string_lossy().as_ref());

        let report = cache.cleanup_missing(&[], true).unwrap();
        assert_eq!(
            report,
            CleanupReport {
                checked_entries: 1,
                stale_entries: 0,
                stale_paths: Vec::new(),
            }
        );

        std::fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn compact_rewrites_database_and_preserves_entries() {
        let temp_dir = test_temp_dir("compact");
        let state_path = temp_dir.join("state.mdb");
        let audio_path = temp_dir.join("track.flac");
        std::fs::write(&audio_path, []).unwrap();

        {
            let cache = ResumeCache::load(state_path.clone(), 250);
            store_test_entry(&cache, &audio_path);
            cache.save().unwrap();
            let report = cache.compact().unwrap().unwrap();
            assert_eq!(report.db_path, state_path);
            assert!(report.before_bytes > 0);
            assert!(report.after_bytes > 0);
        }

        let cache = ResumeCache::load_read_only(state_path, 250);
        let files = cache.all_audio_files().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].file_path, audio_path.to_string_lossy().as_ref());

        std::fs::remove_dir_all(temp_dir).unwrap();
    }

    fn store_test_entry(cache: &ResumeCache, path: &Path) {
        let mut audio_file = sample_audio_file();
        audio_file.file_path = path.to_string_lossy().to_string();
        cache.store(
            audio_file.clone(),
            audio_file.file_size,
            audio_file.modified_secs,
        );
    }

    fn cached_entry_for(path: &Path) -> CachedEntry {
        let mut audio_file = sample_audio_file();
        audio_file.file_path = path.to_string_lossy().to_string();
        CachedEntry::from_audio_file(&audio_file, audio_file.file_size, audio_file.modified_secs)
    }

    fn test_temp_dir(name: &str) -> PathBuf {
        let now = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("fadupes-{name}-{}-{now}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
