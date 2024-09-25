use fadupes::AudioFile;
use clap::{crate_version, value_parser, Arg, ArgAction, Command, ValueHint};
use std::io::Write;
use std::path::PathBuf;
use rayon::prelude::*;

fn main() {
    let matches = Command::new("Audio dupechecker")
        .version(crate_version!())
        .author("menfou")
        .about("Compares audio files in a given directory or multiple inputs and identifies identical files")
        .arg(
            Arg::new("input")
                .short('i')
                .long("input")
                .help("Sets the directory to scan for audio files")
                .required(true)
                .num_args(1..)
                .value_hint(ValueHint::FilePath)
                .value_parser(value_parser!(PathBuf)),
        )
        .arg(
            Arg::new("verbose")
                .short('v')
                .long("verbose")
                .action(ArgAction::SetTrue)
                .help("Enables verbose (debug) output"),
        )
        .get_matches();

    let inputs: Vec<PathBuf> = matches
        .get_many::<PathBuf>("input")
        .unwrap()
        .cloned()
        .collect();

    // Collect all the audio files from all inputs
    let audio_files: Vec<AudioFile> = inputs
    .par_iter() // Process directories in parallel
    .flat_map(|input| {
        let full_path = std::fs::canonicalize(input).unwrap_or_else(|e| {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        });

        // Walk through directory and collect all audio files into a vector
        let file_map = AudioFile::walk_dir(&full_path);
        let map = file_map.lock().unwrap(); // Access the locked HashMap
        map.values() // Access the `Vec<AudioFile>` for each key
            .flat_map(|files| files.iter()) // Work with references to `AudioFile`
            .cloned() // If clone is needed, keep this line; otherwise, remove it
            .collect::<Vec<AudioFile>>() // Collect owned `AudioFile` instances
    })
    .collect();

    compare_audio_files(&audio_files);
}

fn compare_audio_files(audio_files: &[AudioFile]) {
    println!("Comparing {} audio files...", audio_files.len());

    let mut file_map = std::collections::HashMap::new();
    let log_file_path = "identical_files.log"; // path for the log file (current dir)

    // open the log file in append mode (creates it if not exists), currently it's a simple txt file
    let mut log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file_path)
        .expect("Unable to open log file");

    let mut unique_files = Vec::new();

    for file in audio_files {
        println!("Processing file: {}", file.file_path);
        println!("CRC32: {}", file.crc32);

        let key = format!(
            "{}-{}-{}-{}-{}-{}",
            file.total_samples,
            file.sample_rate,
            file.bit_depth,
            file.channels,
            file.peak_level,
            file.rms_db_level
        );

        file_map.entry(key).or_insert_with(Vec::new).push(file);
    }

    for (_, files) in &file_map {
        if files.len() > 1 {
            println!("The following files are identical:");
            writeln!(log_file, "#").expect("Failed to write to log file"); // Add separator for each dupes group

            for file in files {
                println!(" - {}", file.file_path);
                writeln!(log_file, "{}", file.file_path).expect("Failed to write to log file");
            }
        } else {
            unique_files.push(files[0]);
        }
    }

    if !unique_files.is_empty() {
        println!("The following files are unique:");
        for file in unique_files {
            println!(" - {}", file.file_path);
        }
    } else {
        println!("No unique files found.");
    }
}
