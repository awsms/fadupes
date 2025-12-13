use hound::WavReader;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use rayon::prelude::*;
use std::collections::HashSet;
use std::fs::read_link;
use std::fs::File;
use std::io::Write; // New import to handle writing to log files
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use walkdir::WalkDir;

#[derive(Debug, Clone)]
pub struct AudioFile {
    pub file_path: String,
    pub file_name: String,
    pub total_samples: u64,
    pub sample_rate: u32,
    pub bit_depth: u32,
    pub channels: u32,
    pub peak_level: f32,
    pub rms_db_level: f64,
    pub crc32: String,
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
            crc32: String::default(),
        }
    }
}

impl AudioFile {
    // Walk through the directory to find audio files (FLAC and WAV) in parallel with progress bar
    pub fn walk_dir(
        dir: &PathBuf,
        scanned_dirs: &HashSet<PathBuf>,
        list_files: bool,
        skip_unique_size: bool,
    ) -> Vec<AudioFile> {
        // Lazily create the error log file on first error
        let error_log_file: Arc<Mutex<Option<File>>> = Arc::new(Mutex::new(None));

        // Collect the list of audio files to process
        let files_to_process: Vec<_> = WalkDir::new(dir)
            .follow_links(true) // Enable following symlinks
            .sort_by_file_name()
            .into_iter()
            .filter_map(|e| e.ok())
            .filter_map(|f| {
                let path = f.path();

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

                // Filter by file extension (flac or wav) and file size
                let Some(extension) = f.path().extension() else {
                    return None;
                };

                let size_ok = std::fs::metadata(f.path())
                    .map(|meta| meta.len() <= 300 * 1024 * 1024) // Check if file is <= 300MB
                    .unwrap_or(false);

                if (extension == "flac" || extension == "wav") && size_ok {
                    let size = std::fs::metadata(f.path()).map(|m| m.len()).unwrap_or(0);
                    Some((f, size))
                } else {
                    None
                }
            })
            .collect();

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

        let audio_files: Vec<AudioFile> = if list_files {
            let start_counter = Arc::new(AtomicUsize::new(0));
            let size_counts = if skip_unique_size {
                let mut counts = std::collections::HashMap::new();
                for (_, size) in &files_to_process {
                    *counts.entry(*size).or_insert(0usize) += 1;
                }
                Some(counts)
            } else {
                None
            };

            files_to_process
                .par_iter()
                .filter_map(|(entry, size)| {
                    let path_str = entry.path().to_string_lossy().to_string();
                    let progress = progress_bar.clone();

                    if skip_unique_size
                        && size_counts
                            .as_ref()
                            .and_then(|map| map.get(size))
                            .copied()
                            .unwrap_or(0)
                            <= 1
                    {
                        if let Some(ref mp) = list_mp {
                            let _ = mp.println(format!(
                                "Skipping unique-size file: {}",
                                entry.path().display()
                            ));
                        }
                        progress.inc(1);
                        return None;
                    }

                    let start_order = start_counter.fetch_add(1, Ordering::Relaxed) + 1;
                    let per_file_pb = list_mp.as_ref().map(|mp| {
                        let pb = mp.add(ProgressBar::new_spinner());
                        pb.set_style(
                            ProgressStyle::with_template("{spinner} {msg}")
                                .expect("Failed to create file progress bar template")
                                .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
                        );
                        pb.enable_steady_tick(Duration::from_millis(100));
                        pb.set_message(format!("[{}/{}] {}", start_order, total_files, path_str));
                        pb
                    });

                    let result = match AudioFile::process_audio_file(entry) {
                        Ok(audio_file) => Some(audio_file),
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

                    progress.inc(1);

                    if let Some(pb) = per_file_pb {
                        pb.finish_and_clear();
                    }

                    result
                })
                .collect()
        } else {
            let size_counts = if skip_unique_size {
                let mut counts = std::collections::HashMap::new();
                for (_, size) in &files_to_process {
                    *counts.entry(*size).or_insert(0usize) += 1;
                }
                Some(counts)
            } else {
                None
            };

            files_to_process
                .par_iter()
                .filter_map(|(entry, size)| {
                    let path_str = entry.path().to_string_lossy().to_string();
                    let progress = progress_bar.clone();

                    if skip_unique_size
                        && size_counts
                            .as_ref()
                            .and_then(|map| map.get(size))
                            .copied()
                            .unwrap_or(0)
                            <= 1
                    {
                        progress.inc(1);
                        return None;
                    }

                    let result = match AudioFile::process_audio_file(entry) {
                        Ok(audio_file) => Some(audio_file),
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

                    progress.inc(1);
                    result
                })
                .collect()
        };

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
                    reader.samples().map(|sample| sample.unwrap_or(0) as i16),
                    stream_info.bits_per_sample as i32,
                );
                audio_file.peak_level = peak_level;
                audio_file.rms_db_level = rms_db_level;
            }
            "wav" => {
                let mut reader =
                    WavReader::open(entry.path()).map_err(|_| ProcessError::NonFlacError)?;
                let spec = reader.spec();
                audio_file.total_samples = reader.duration() as u64;
                audio_file.sample_rate = spec.sample_rate;
                audio_file.bit_depth = spec.bits_per_sample as u32;
                audio_file.channels = spec.channels as u32;

                let (peak_level, rms_db_level) = Self::accumulate_metrics(
                    reader.samples::<i16>().map(|s| s.unwrap_or(0)),
                    spec.bits_per_sample as i32,
                );
                audio_file.peak_level = peak_level;
                audio_file.rms_db_level = rms_db_level;
            }
            _ => return Err(ProcessError::UnsupportedBitDepth),
        }

        Ok(audio_file)
    }

    fn accumulate_metrics<I>(samples: I, bit_depth: i32) -> (f32, f64)
    where
        I: Iterator<Item = i16>,
    {
        let max_amplitude = Self::get_max_amplitude(bit_depth) as f64;
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
            max_abs as f32 / i16::MAX as f32
        };

        let rms_db_level = if count == 0 {
            f64::NEG_INFINITY
        } else {
            let rms_amplitude = (squared_sum / count as f64).sqrt();
            20.0 * rms_amplitude.log10()
        };

        (peak_level, rms_db_level)
    }

    fn get_max_amplitude(bit_depth: i32) -> i32 {
        match bit_depth {
            16 => i16::MAX as i32,
            _ => i16::MAX as i32,
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
