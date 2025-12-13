use clap::{crate_version, value_parser, Arg, ArgAction, Command, ValueHint};
use fadupes::AudioFile;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;

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

    // Create a HashSet of scanned directories to pass to the walk_dir function
    let scanned_dirs: HashSet<PathBuf> = inputs.iter().cloned().collect();

    // Collect all the audio files from all inputs
    let audio_files: Vec<AudioFile> = inputs
        .into_par_iter() // Process directories in parallel
        .flat_map(|input| {
            let full_path = std::fs::canonicalize(&input).unwrap_or_else(|e| {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            });

            AudioFile::walk_dir(&full_path, &scanned_dirs).into_par_iter()
        })
        .collect();

    compare_audio_files(&audio_files);
}

fn compare_audio_files(audio_files: &[AudioFile]) {
    let log_file_path = "identical_files.log"; // path for the log file (current dir)

    // Open the log file in append mode (creates it if not exists), currently it's a simple txt file
    let mut log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file_path)
        .expect("Unable to open log file");

    let mut file_map = HashMap::new();
    let mut identical_groups = Vec::new();

    // Group files by their characteristics
    for file in audio_files {
        let key = (
            file.total_samples,
            file.sample_rate,
            file.bit_depth,
            file.channels,
            file.peak_level.to_bits(),
            file.rms_db_level.to_bits(),
        );

        file_map.entry(key).or_insert_with(Vec::new).push(file);
    }

    // Collect identical files into groups
    for (_, files) in &file_map {
        if files.len() > 1 {
            identical_groups.push(files);
        }
    }

    // Output the results and write to the log file
    if identical_groups.is_empty() {
        println!("Among {} files, no dupes were found.", audio_files.len());
    } else {
        let total_dupes: usize = identical_groups.iter().map(|g| g.len()).sum();
        println!("Found {} identical files:", total_dupes);

        writeln!(log_file, "Identical Files Found:").expect("Failed to write to log file");
        for group in identical_groups {
            writeln!(log_file, "#").expect("Failed to write to log file"); // Add separator for each dupe group
            for file in group {
                println!("{}", file.file_path);
                writeln!(log_file, "{}", file.file_path).expect("Failed to write to log file");
            }
            println!(); // Add an empty line between dupe groups
        }
    }
}
