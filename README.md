# FindAudioDupes (fadupes)

fadupes is an experimental CLI tool to help you dupecheck your music collection.

## Features

- **Directory Scanning**: Easily scan one or multiple directories for audio files.
- **Recursive Scanning**: Directories are processed recursively.
- **Duplicate Identification**: Identifies identical audio files by comparing audio properties.
- **Logging**: Path of duplicate files are logged for later review/scripting their deletion.

## Installation

Make sure you have Rust and Cargo installed on your machine. Clone the repository and run:

```bash
cargo build --release
```

## Usage

```bash
fadupes -i <path>
fadupes -i <directory1> <directory2> <file1> <file2> ...
fadupes -i <file1> <file2> "file"* ...
```

### Arguments

- `-i, --input`: Specify one or more files/directories to scan for audio files (required).
- `--nosym`: Ignore symlinks instead of following them while scanning.
- `--resume`: Load/save scan progress to a state file so an interrupted run can continue.
- `--state-file`: Path to the state file used with `--resume` (default: `fadupes_state.json`). Supplying this flag implies `--resume`.

Resume notes:
- State files are written where you run the command (or the path you pass to `--state-file`).
- On Ctrl+C the tool saves the cache before exiting.

## Example

```bash
fadupes -i /path/to/audio1 /path/to/audio2.flac /path/to/audio3.wav
```

## How It Works

1. **Input Handling**: The tool accepts one or more directory paths and scans for audio files within. Also accepts files directly.
2. **Audio File Processing**: Each audio file is analyzed to extract key properties such as sample rate, bit depth, and CRC32 checksum.
3. **Duplicate Comparison**: The audio files are compared based on their properties, and identical files are identified and logged.
4. **Output**: Unique and duplicate files are printed to the console, and duplicates are saved in `identical_files.log`.

## Dependencies

- `rayon`: For parallel processing.
- `hound`: For WAV file reading.
- `claxon`: For FLAC file reading.
- `crc32fast`: For CRC32 calculations.
- `walkdir`: For traversing directories.

## Current limitations

Only WAV and FLAC supported for now.

## TODO
- [ ] Build a database of tracks that will be updated whenever this program is executed.
- [ ] Add a flag to delete dupes, which will let the user pick the file to keep/delete in the terminal.
- [ ] Display current progress during the scan, including the files being processed and a percentage completion. 
