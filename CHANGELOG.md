# Changelog

## [1.0.3] - 2025-12-14

### 🚀 Features

- *(scan)* Add `--nosym` to ignore symlinks
- *(resume)* Add JSON resume cache for processed audio files
- *(resume)* Enable auto resume/save by default + Ctrl+C persistence
- *(scan)* Hard-cap scanned files to <= 800MB
- *(cli)* Add `--ignore-size` filter expression

### 🐛 Bug Fixes

- *(list)* Cap live `--list` UI and reuse spinner lines
- *(log)* Avoid duplicate duplicate (xd) entries in logfile

### ⚙️ Miscellaneous Tasks

- *(doc)* Update README to reflect features

## [1.0.2] - 2025-12-13

### 🚀 Features

- *(log)* Add error logging to identical_files_errors.log + gitignore logs
- *(cli/perf)* Add `--skip-unique-size` and --nolist` flags

### 🐛 Bug Fixes

- *(log)* Only create identical_files_errors.log when an error happens

### ⚡ Performance

- *(metrics)* Compute peak + RMS(dB) in one pass and simplify matching

### ⚙️ Miscellaneous Tasks

- *(release)* Bump v1.0.2 + update release workflow

## [1.0.1] - 2024-09-30

### 🚀 Features

- *(scan)* Follow symlinks by default with loop-back protection

### ⚙️ Miscellaneous Tasks

- *(output)* Reduce console noise in results output

## [1.0.0] - 2024-09-26

### 🚀 Features

- Initial CLI dupe checker (WAV/FLAC scan + grouping)
- *(ui)* Add scan progress bar during processing

### 🐛 Bug Fixes

- *(progress)* Make progress reporting actually reflect work done

