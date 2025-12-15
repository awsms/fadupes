# fadupes - FindAudioDupes

`fadupes` is a CLI tool to help you find identical audio files in a music collection by scanning directories and files recursively and comparing audio characteristics.

---

## Features

- **Recursive directory scanning**
  - Accepts one or more directories and/or files as input
- **Parallel processing**
  - Uses multiple threads for faster scans
- **Progress display**
  - Global progress bar
  - Optional per-file “currently scanning” output
- **Resumable scans**
  - Optional JSON state file
  - Automatically loads existing state
  - Periodically saved during processing
  - Saved automatically on Ctrl+C
- **Symlink handling**
  - Follows symlinks by default
  - Option to ignore symlinks
  - Avoids loop-back symlinks into scanned roots
- **Filtering options**
  - Ignore files by size (`<`, `>`, or range)
  - Skip files with a unique byte size for faster scans
- **Logging**
  - Duplicate groups written to `identical_files.log`
  - Processing errors written to `identical_files_errors.log`

---

## Building

You need Rust and Cargo installed.

```bash
cargo build --release
````

The binary will be available at:

```bash
target/release/fadupes
```

---

## Usage

### Basic usage

```bash
fadupes -i <path>
fadupes -i <directory1> <directory2>
fadupes -i <file1> <file2>
```

Inputs may be directories, files, or any combination of both.

---

## Command-line options

### Required

* `-i, --input <PATHS...>`

  * One or more files or directories to scan

### Optional

* `--nolist`

  * Disable per-file list output (keeps only the global progress bar)

* `--nosym`

  * Ignore symlinks instead of following them

* `--checkpoint <N>`

  * Save the resume JSON file every `N` scanned files
  * Default: `250`

* `--skip-unique-size`

  * Skip files whose byte size appears only once
    (faster, but may miss duplicates)

* `--ignore-size <EXPR>`

  * Ignore files by size
  * Examples:

    * `<3MB`
    * `>800MB`
    * `3MB..800MB`

* `--state-file <PATH>`

  * Path to the resume state file
  * Default: `fadupes_state.json`

* `--no-resume`

  * Disable loading and saving of the resume state

---

## Resume behavior

* State files are written in the current working directory by default
* If the state file exists, it is loaded automatically
* The state is saved periodically during the scan (tune with `--checkpoint`)
* On Ctrl+C, the state is saved before exiting

---

## How duplicate detection works

Each audio file is decoded and analyzed to extract audio properties.
Files are considered identical if all of the following match:

* Total sample count
* Sample rate
* Bit depth
* Channel count
* Peak level
* RMS level (dB)

Files sharing the same characteristics are grouped together as duplicates.

---

## Output

* **Console**

  * Duplicate groups are printed to stdout

* **Files**

  * `identical_files.log`

    * Appended with duplicate file paths (grouped)
  * `identical_files_errors.log`

    * Created only if errors occur during processing

---

## Supported formats and limits

* Supported formats: **WAV**, **FLAC**
* Files larger than **800 MB** are currently skipped

---

## Development

Run in debug mode:

```bash
cargo run -- -i /path/to/music
```

---

## Current limitations

* Only WAV and FLAC are supported
* Duplicate detection is based on decoded audio characteristics, not tags
* No built-in deletion or interactive duplicate management

---

## TODO

- [ ] Additional audio formats
- [ ] Interactive duplicate handling
- [ ] Persistent audio database of all scans
