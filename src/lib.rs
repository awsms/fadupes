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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};
use walkdir::WalkDir;

const STATE_VERSION: u32 = 1;

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
    pub file_name: String,
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
            file_name: String::default(),
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedEntry {
    pub audio_file: AudioFile,
    pub file_size: u64,
    pub modified_secs: u64,
}

#[derive(Debug, Clone)]
struct CacheBase {
    root: Option<PathBuf>,
    entries: HashMap<String, CachedEntry>, // rel path when root is Some, absolute when None
}

#[derive(Debug, Clone)]
struct CacheData {
    bases: Vec<CacheBase>,
}

impl CacheData {
    fn new() -> Self {
        Self { bases: Vec::new() }
    }

    // Find the best-matching base (longest ancestor) for a given path and return its index + key used inside that base.
    fn find_base_for_path(&self, path: &Path) -> Option<(usize, String)> {
        let mut best: Option<(usize, usize, String)> = None; // (idx, root_len, key)

        for (idx, base) in self.bases.iter().enumerate() {
            match &base.root {
                Some(root) => {
                    if path.starts_with(root) {
                        if let Ok(rel) = path.strip_prefix(root) {
                            let rel_str = rel.to_string_lossy().to_string();
                            let root_len = root.as_os_str().len();
                            if best
                                .as_ref()
                                .map_or(true, |(_, best_len, _)| root_len > *best_len)
                            {
                                best = Some((idx, root_len, rel_str));
                            }
                        }
                    }
                }
                None => {
                    let key = path.to_string_lossy().to_string();
                    if base.entries.contains_key(&key)
                        && best.as_ref().map_or(true, |(_, best_len, _)| *best_len == 0)
                    {
                        best = Some((idx, 0, key));
                    }
                }
            }
        }

        best.map(|(idx, _, key)| (idx, key))
    }

    // Ensure we have a base for this path (using preferred_root as the new base root) and return base index + key.
    fn ensure_base_for_path(&mut self, path: &Path, preferred_root: &Path) -> (usize, String) {
        if let Some(found) = self.find_base_for_path(path) {
            return found;
        }

        // Canonicalize the preferred root if possible for stability
        let mut base_root = preferred_root.to_path_buf();
        if let Ok(canon) = preferred_root.canonicalize() {
            base_root = canon;
        }

        // If the path isn't under the preferred root, fall back to an unrooted base
        let (root, key) = if path.starts_with(&base_root) {
            let rel = path
                .strip_prefix(&base_root)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string();
            (Some(base_root), rel)
        } else {
            (
                None,
                path.to_string_lossy()
                    .to_string(),
            )
        };

        let idx = self.bases.len();
        self.bases.push(CacheBase {
            root,
            entries: HashMap::new(),
        });
        (idx, key)
    }

    fn from_state_file(state: StateFile) -> Self {
        if state.version != STATE_VERSION {
            eprintln!(
                "Warning: state file version {} differs from expected {}. Attempting to read anyway.",
                state.version, STATE_VERSION
            );
        }
        let mut data = CacheData::new();
        for base in state.bases {
            let root_path = base.root.as_deref().map(PathBuf::from);
            let mut entries = HashMap::new();

            for mut entry in base.entries {
                // Determine the full path and normalize audio_file fields
                let full_path = match (&root_path, entry.rel.as_deref(), entry.path.as_deref()) {
                    (Some(root), Some(rel), _) => Some(root.join(rel)),
                    (None, _, Some(path)) => Some(PathBuf::from(path)),
                    _ => None,
                };

                if let Some(full_path) = full_path {
                    hydrate_entry(&mut entry.entry, &full_path);
                    let key = if let Some(rel) = entry.rel {
                        rel
                    } else {
                        full_path.to_string_lossy().to_string()
                    };
                    entries.insert(key, entry.entry);
                }
            }

            data.bases.push(CacheBase {
                root: root_path,
                entries,
            });
        }

        data
    }

    fn from_legacy_map(map: HashMap<String, CachedEntry>) -> Self {
        let mut base = CacheBase {
            root: None,
            entries: HashMap::new(),
        };

        for (path, mut entry) in map {
            let full_path = PathBuf::from(&path);
            hydrate_entry(&mut entry, &full_path);
            base.entries.insert(path, entry);
        }

        CacheData { bases: vec![base] }
    }
}

fn hydrate_entry(entry: &mut CachedEntry, full_path: &Path) {
    entry.audio_file.file_path = full_path.to_string_lossy().to_string();
    // Keep the duplicate fields aligned in case older caches had zeros in the AudioFile
    entry.audio_file.file_size = entry.file_size;
    entry.audio_file.modified_secs = entry.modified_secs;
}

fn state_from_cache(cache: &CacheData) -> StateFile {
    let mut bases = Vec::new();

    for base in &cache.bases {
        let mut entries: Vec<(String, CachedEntry)> =
            base.entries.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        let mut state_entries = Vec::new();
        for (key, mut entry) in entries {
            let full_path = base
                .root
                .as_ref()
                .map(|root| root.join(&key))
                .unwrap_or_else(|| PathBuf::from(&key));
            hydrate_entry(&mut entry, &full_path);

            state_entries.push(StateEntry {
                rel: base.root.as_ref().map(|_| key.clone()),
                path: if base.root.is_some() { None } else { Some(key) },
                entry,
            });
        }

        bases.push(StateBase {
            root: base
                .root
                .as_ref()
                .map(|r| r.to_string_lossy().to_string()),
            entries: state_entries,
        });
    }

    StateFile {
        version: STATE_VERSION,
        bases,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StateEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    rel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(flatten)]
    entry: CachedEntry,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StateBase {
    #[serde(skip_serializing_if = "Option::is_none")]
    root: Option<String>,
    entries: Vec<StateEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StateFile {
    version: u32,
    bases: Vec<StateBase>,
}

#[derive(Debug, Clone)]
pub struct ResumeCache {
    pub path: PathBuf,
    data: Arc<Mutex<CacheData>>,
    pub save_every: usize,
    pub pending: Arc<AtomicUsize>,
    save_lock: Arc<Mutex<()>>,
}

impl ResumeCache {
    pub fn load(path: PathBuf, save_every: usize) -> Self {
        let data = match std::fs::read_to_string(&path) {
            Ok(contents) => {
                // Try new format first
                match serde_json::from_str::<StateFile>(&contents) {
                    Ok(state) => CacheData::from_state_file(state),
                    Err(_) => match serde_json::from_str::<HashMap<String, CachedEntry>>(&contents) {
                        Ok(legacy) => {
                            eprintln!(
                                "Loaded legacy resume cache format from {} (will rewrite to v{}).",
                                path.display(),
                                STATE_VERSION
                            );
                            CacheData::from_legacy_map(legacy)
                        }
                        Err(err) => {
                            eprintln!(
                                "Warning: failed to parse state file {}: {err}. Starting with empty state.",
                                path.display()
                            );
                            backup_broken(&path, &format!("{err}"));
                            CacheData::new()
                        }
                    },
                }
            }
            Err(err) if err.kind() == ErrorKind::NotFound => CacheData::new(),
            Err(err) => {
                eprintln!(
                    "Warning: failed to open state file {}: {err}. Starting with empty state.",
                    path.display()
                );
                backup_broken(&path, &format!("{err}"));
                CacheData::new()
            }
        };

        ResumeCache {
            path,
            data: Arc::new(Mutex::new(data)),
            save_every,
            pending: Arc::new(AtomicUsize::new(0)),
            save_lock: Arc::new(Mutex::new(())),
        }
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
        let data = self.data.lock().ok()?;
        let (base_idx, key) = data.find_base_for_path(file_path)?;
        let base = data.bases.get(base_idx)?;
        base.entries.get(&key).and_then(|entry| {
            if entry.file_size == file_size && entry.modified_secs == modified_secs {
                Some(entry.audio_file.clone())
            } else {
                None
            }
        })
    }

    pub fn store(
        &self,
        mut audio_file: AudioFile,
        file_size: u64,
        modified_secs: u64,
        base_root: &Path,
    ) {
        // Keep the duplicate fields consistent
        audio_file.file_size = file_size;
        audio_file.modified_secs = modified_secs;

        if let Ok(mut data) = self.data.lock() {
            let full_path = PathBuf::from(&audio_file.file_path);
            let (base_idx, key) = data.ensure_base_for_path(&full_path, base_root);
            let entry = CachedEntry {
                audio_file,
                file_size,
                modified_secs,
            };
            if let Some(base) = data.bases.get_mut(base_idx) {
                base.entries.insert(key, entry);
            }
        }

        // Throttle disk writes: save cache every 'save_every' inserts (AtomicUsize so threads coordinate cheaply)
        let count = self.pending.fetch_add(1, Ordering::Relaxed) + 1;
        if count >= self.save_every {
            // Reset the counter before saving so new inserts can keep counting while we write
            self.pending.store(0, Ordering::Relaxed);
            let _ = self.save();
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        // Serialize writers to the temp file/rename to avoid corruption from concurrent saves
        let _lock = self.save_lock.lock().unwrap();

        let snapshot = {
            let data = self.data.lock().unwrap();
            data.clone()
        };

        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }

        // Atomic-ish save: write to a temp file then rename, so we don't leave a half-written JSON behind
        let tmp_path = self.path.with_extension("tmp");
        let file = File::create(&tmp_path)?;
        serde_json::to_writer(&file, &state_from_cache(&snapshot))?;
        file.sync_all()?; // ensure bytes hit disk before rename
        std::fs::rename(tmp_path, &self.path)?;
        Ok(())
    }
}

impl Drop for ResumeCache {
    fn drop(&mut self) {
        let _ = self.save();
    }
}

impl AudioFile {
    // Shared helper: decide if an entry should be skipped (unique size) or served from cache.
    fn skip_or_cached(
        entry: &walkdir::DirEntry,
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

        let cached = resume_cache
            .and_then(|cache| cache.lookup(entry.path(), size, modified_secs));

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
    ) -> Vec<AudioFile> {
        // Lazily open the error log only if we hit an error (shared across threads via Mutex<Option<File>>)
        let error_log_file: Arc<Mutex<Option<File>>> = Arc::new(Mutex::new(None));

        // Collect the list of audio files to process
        // Build the full candidate list up front; we need it to compute unique-size skips
        // and to seed the progress bar with already-cached or skipped entries on resume.
        let files_to_process: Vec<_> = WalkDir::new(dir)
            .follow_links(!ignore_symlinks) // Follow symlinks by default; skip loop-back symlinks into input roots (and skip all symlinks if --nosym is set)
            .sort_by_file_name()
            .into_iter()
            .filter_map(|e| e.ok())
            .filter_map(|f| {
                let path = f.path();

                if f.file_type().is_symlink() {
                    if ignore_symlinks {
                        return None;
                    }

                    // Check if it's a symlink and resolve it
                    if let Ok(symlink_target) = read_link(path) {
                        // If symlink points to one of the directories being scanned, ignore it
                        if scanned_dirs.contains(&symlink_target) {
                            eprintln!(
                                "Skipping symlink pointing to a scanned dir: {}",
                                path.display()
                            );
                            return None;
                        }
                    }
                }

                let Ok(metadata) = std::fs::metadata(f.path()) else {
                    return None;
                };

                let size = metadata.len();
                // Apply optional ignore filter from --ignore-size
                if ignore_size.is_some_and(|flt| flt.should_ignore(size)) {
                    return None;
                }

                let size_ok = metadata.len() <= 800 * 1024 * 1024; // Check if file is <= 800MB

                // Filter by file extension (flac or wav) and file size
                let Some(extension) = f.path().extension() else {
                    return None;
                };

                if (extension == "flac" || extension == "wav") && size_ok {
                    let size = metadata.len();
                    let modified_secs = metadata
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    Some((f, size, modified_secs))
                } else {
                    None
                }
            })
            .collect();

        // Precompute size counts if we need to skip unique sizes
        let size_counts = if skip_unique_size {
            let mut counts = std::collections::HashMap::new();
            for (_, size, _) in &files_to_process {
                *counts.entry(*size).or_insert(0usize) += 1;
            }
            Some(counts)
        } else {
            None
        };

        // Count how many entries are already satisfied (cached) or will be skipped (unique size)
        let initial_processed = files_to_process
            .iter()
            .filter(|(entry, size, modified_secs)| {
                let (is_unique_skip, cached) = Self::skip_or_cached(
                    entry,
                    *size,
                    *modified_secs,
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
                .filter_map(|(entry, size, modified_secs)| {
                    let path_str = entry.path().to_string_lossy().to_string();
                    let progress = progress_bar.clone();

                    let (is_unique_skip, cached) = Self::skip_or_cached(
                        entry,
                        *size,
                        *modified_secs,
                        skip_unique_size,
                        size_counts.as_ref(),
                        resume_cache.as_ref(),
                    );
                    let already_processed = is_unique_skip || cached.is_some();

                    if is_unique_skip {
                        if let Some(ref mp) = list_mp {
                            let _ = mp.println(format!(
                                "Skipping unique-size file: {}",
                                entry.path().display()
                            ));
                        }
                        return None;
                    }

                    if let Some(audio_file) = cached {
                        if let Some(ref mp) = list_mp {
                            let _ = mp.println(format!(
                                "Using cached result for: {}",
                                entry.path().display()
                            ));
                        }
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

                    let result = match AudioFile::process_audio_file(entry) {
                        Ok(mut audio_file) => {
                            audio_file.file_size = *size;
                            audio_file.modified_secs = *modified_secs;
                            if let Some(cache) = resume_cache.as_ref() {
                                cache.store(audio_file.clone(), *size, *modified_secs, dir);
                            }
                            Some(audio_file)
                        }
                        Err(err) => {
                            let error_message =
                                format!("Error processing file: {}: {:?}", path_str, err);
                            println!("{}", error_message);
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
                .filter_map(|(entry, size, modified_secs)| {
                    let path_str = entry.path().to_string_lossy().to_string();
                    let progress = progress_bar.clone();

                    let (is_unique_skip, cached) = Self::skip_or_cached(
                        entry,
                        *size,
                        *modified_secs,
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

                    let result = match AudioFile::process_audio_file(entry) {
                        Ok(mut audio_file) => {
                            audio_file.file_size = *size;
                            audio_file.modified_secs = *modified_secs;
                            if let Some(cache) = resume_cache.as_ref() {
                                cache.store(audio_file.clone(), *size, *modified_secs, dir);
                            }
                            Some(audio_file)
                        }
                        Err(err) => {
                            let error_message =
                                format!("Error processing file: {}: {:?}", path_str, err);
                            println!("{}", error_message);
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
        let extension = entry
            .path()
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("");
        let mut audio_file = AudioFile {
            file_path: entry.path().to_string_lossy().to_string(), // Store the full path
            ..Default::default()
        };

        match extension {
            "flac" => {
                let mut reader = Self::load_flac(entry.path())?;
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
                let mut reader =
                    WavReader::open(entry.path()).map_err(|_| ProcessError::NonFlacError)?;
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
