use crc32fast::Hasher;
use hound::WavReader;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::fs::read_link;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::io::Write; // New import to handle writing to log files
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
    ) -> Arc<Mutex<HashMap<String, Vec<AudioFile>>>> {
        let file_map = Arc::new(Mutex::new(HashMap::new()));
        let file_map_clone = Arc::clone(&file_map);

        // Create or open the error log file
        let log_error_path = "identical_files_errors.log"; // Path for the error log file
        let error_log_file = Arc::new(Mutex::new(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_error_path)
                .expect("Unable to open error log file"),
        ));

        // Collect the list of audio files to process
        let files_to_process: Vec<_> = WalkDir::new(dir)
            .follow_links(true) // Enable following symlinks
            .sort_by_file_name()
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|f| {
                let path = f.path();

                // Check if it's a symlink and resolve it
                if let Ok(symlink_target) = read_link(path) {
                    // If symlink points to one of the directories being scanned, ignore it
                    if scanned_dirs.contains(&symlink_target) {
                        eprintln!(
                            "Skipping symlink pointing to a scanned dir: {}",
                            path.display()
                        );
                        return false;
                    }
                }

                // Filter by file extension (flac or wav) and file size
                if let Some(extension) = f.path().extension() {
                    (extension == "flac" || extension == "wav")
                        && std::fs::metadata(f.path())
                            .map(|meta| meta.len() <= 300 * 1024 * 1024) // Check if file is <= 300MB
                            .unwrap_or(false)
                } else {
                    false
                }
            })
            .collect();

        let total_files = files_to_process.len();
        let multi_progress = MultiProgress::new(); // MultiProgress to handle multiple progress bars

        // General progress bar for all files
        let general_progress_bar = multi_progress.add(ProgressBar::new(total_files as u64));
        general_progress_bar.set_style(
            ProgressStyle::with_template("Total Progress: [{wide_bar}] {pos}/{len} ({eta})")
                .expect("Failed to create general progress bar template")
                .progress_chars("#>-"),
        );

        // Process each file in parallel, showing individual progress bars for each
        let _ = std::thread::spawn(move || {
            files_to_process
                .par_iter()
                .map(|entry| {
                    let path_str = entry.path().to_string_lossy().to_string();

                    // Get the file size in bytes
                    let file_size = std::fs::metadata(entry.path())
                        .map(|metadata| metadata.len())
                        .unwrap_or(0);

                    // Create an individual progress bar for each file based on its size in bytes
                    let file_progress_bar = multi_progress.add(ProgressBar::new(file_size));
                    file_progress_bar.set_style(
                        ProgressStyle::with_template(
                            "{msg}\n[{wide_bar}] {bytes}/{total_bytes} ({eta})",
                        )
                        .expect("Failed to create file progress bar template")
                        .progress_chars("█░"),
                    );

                    // Set the message to the current filename
                    file_progress_bar.set_message(format!("Processing: {}", path_str));

                    // Simulate or perform real byte-based processing, incrementing the progress bar
                    let mut file = std::fs::File::open(entry.path()).expect("Failed to open file");
                    let mut buffer = [0u8; 8192]; // Read 8 KB chunks

                    let mut bytes_processed = 0;
                    while let Ok(bytes_read) = file.read(&mut buffer) {
                        if bytes_read == 0 {
                            break; // End of file
                        }
                        bytes_processed += bytes_read as u64;
                        file_progress_bar.set_position(bytes_processed); // Update progress based on bytes read
                    }

                    file_progress_bar.finish_and_clear(); // Clear the progress bar once done

                    // Process the audio file and collect its metadata
                    match AudioFile::process_audio_file(entry) {
                        Ok(audio_file) => {
                            // Clone the `audio_file` before adding it to the map
                            let audio_file_clone = audio_file.clone();

                            // Add processed file to the map
                            let file_stem = Path::new(&audio_file.file_name)
                                .file_stem()
                                .map(|stem| stem.to_string_lossy().to_string())
                                .unwrap_or_else(|| "unknown".to_string());

                            let mut map = file_map_clone.lock().unwrap();
                            map.entry(file_stem)
                                .or_insert_with(Vec::new)
                                .push(audio_file_clone);

                            Some(audio_file)
                        }
                        Err(err) => {
                            let error_message = format!("Error processing file: {}: {:?}", path_str, err);
                            println!("{}", error_message);
                            // Log the error to the log file using Arc<Mutex<File>>
                            let mut error_log = error_log_file.lock().unwrap();
                            writeln!(error_log, "{}", error_message)
                                .expect("Failed to write to error log file");
                            None
                        }
                    }
                })
                .for_each(|_| {
                    // Increment the general progress bar after each file is processed
                    general_progress_bar.inc(1);
                });

            general_progress_bar.finish_with_message("All files processed");
        })
        .join()
        .expect("Thread failed");

        file_map
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

                let samples: Vec<i16> = reader
                    .samples()
                    .map(|sample| sample.unwrap_or(0) as i16)
                    .collect();
                audio_file.crc32 = Self::generate_crc32_16bit(&samples);
                audio_file.peak_level = Self::calculate_peak_level_16bit(&samples);
                audio_file.rms_db_level = Self::calculate_rms_db_level(samples, 16);
            }
            "wav" => {
                let mut reader =
                    WavReader::open(entry.path()).map_err(|_| ProcessError::NonFlacError)?;
                let spec = reader.spec();
                audio_file.total_samples = reader.duration() as u64;
                audio_file.sample_rate = spec.sample_rate;
                audio_file.bit_depth = spec.bits_per_sample as u32;
                audio_file.channels = spec.channels as u32;

                let samples: Vec<i16> = reader.samples::<i16>().map(|s| s.unwrap_or(0)).collect();
                audio_file.crc32 = Self::generate_crc32_16bit(&samples);
                audio_file.peak_level = Self::calculate_peak_level_16bit(&samples);
                audio_file.rms_db_level = Self::calculate_rms_db_level(samples, 16);
            }
            _ => return Err(ProcessError::UnsupportedBitDepth),
        }

        Ok(audio_file)
    }

    fn calculate_rms_db_level(samples: Vec<i16>, bit_depth: i32) -> f64 {
        if samples.is_empty() {
            return f64::NEG_INFINITY;
        }

        let max_amplitude = Self::get_max_amplitude(bit_depth) as f64;
        let squared_sum: f64 = samples
            .iter()
            .map(|sample| {
                let normalized_sample = *sample as f64 / max_amplitude;
                normalized_sample * normalized_sample
            })
            .sum();

        let rms_amplitude = (squared_sum / samples.len() as f64).sqrt();
        20.0 * rms_amplitude.log10()
    }

    fn calculate_peak_level_16bit(samples: &[i16]) -> f32 {
        let max_amplitude = samples.iter().map(|sample| sample.abs()).max().unwrap_or(0);
        max_amplitude as f32 / i16::MAX as f32
    }

    fn generate_crc32_16bit(samples: &[i16]) -> String {
        let mut crc32 = Hasher::new();
        for sample in samples {
            crc32.update(&sample.to_le_bytes());
        }
        format!("{:08X}", crc32.finalize())
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
