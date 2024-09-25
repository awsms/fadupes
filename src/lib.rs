use crc32fast::Hasher;
use hound::WavReader;
use std::fs::File;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;
use tracing::{info, warn};
use std::sync::{Arc, Mutex};
use rayon::prelude::*;
use std::collections::HashMap;

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
    // Walk through the directory to find audio files (FLAC and WAV) in parallel
    pub fn walk_dir(dir: &PathBuf) -> Arc<Mutex<HashMap<String, Vec<AudioFile>>>> {
        let file_map = Arc::new(Mutex::new(HashMap::new()));

        WalkDir::new(dir)
            .sort_by_file_name()
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|f| {
                if let Some(extension) = f.path().extension() {
                    extension == "flac" || extension == "wav"
                } else {
                    false
                }
            })
            .par_bridge()
            .filter_map(|entry| {
                info!("Processing file: {:?}", entry.path());
                match Self::process_audio_file(&entry) {
                    Ok(audio_file) => Some(audio_file),
                    Err(err) => {
                        warn!("Error processing file {:?}: {}", entry.path(), err);
                        None
                    }
                }
            })
            .for_each(|audio_file| {
                let file_stem = Path::new(&audio_file.file_name)
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().to_string())
                    .unwrap_or_else(|| "unknown".to_string()); // Handle None safely

                let mut map = file_map.lock().unwrap();
                map.entry(file_stem)
                    .or_insert_with(Vec::new)
                    .push(audio_file);
            });

        file_map
    } 

    // Process individual audio files (FLAC and WAV)
    pub fn process_audio_file(entry: &walkdir::DirEntry) -> Result<AudioFile, ProcessError> {
        let extension = entry.path().extension().and_then(|ext| ext.to_str()).unwrap_or("");
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

                let samples: Vec<i16> = reader.samples().map(|sample| sample.unwrap_or(0) as i16).collect();
                audio_file.crc32 = Self::generate_crc32_16bit(&samples);
                audio_file.peak_level = Self::calculate_peak_level_16bit(&samples);
                audio_file.rms_db_level = Self::calculate_rms_db_level(samples, 16);
            }
            "wav" => {
                let mut reader = WavReader::open(entry.path()).map_err(|_| ProcessError::NonFlacError)?;
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
        let squared_sum: f64 = samples.iter().map(|sample| {
            let normalized_sample = *sample as f64 / max_amplitude;
            normalized_sample * normalized_sample
        }).sum();

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
