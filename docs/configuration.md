# Configuration

`astro-sight init` generates a configuration file in TOML. It is written to `~/.config/astro-sight/config.toml` by default, and `--path` changes the location. An existing file at the same path is overwritten without confirmation, so move your current settings aside first if you want to keep them.

```toml
# Write debug logs to files (default: false)
debug = false

# Log directory (default: ~/.config/astro-sight/logs)
# log_path = "~/.config/astro-sight/logs"

# Default output format: "json" | "toon" | "auto" (default: json)
format = "json"

# Leave generated files out of directory scans (default: true)
skip_generated = true
```

When `log_path` is omitted, astro-sight uses `logs/` in the directory of the configuration file it loaded. The same applies when a custom file is given with `--config /path/to/config.toml`. An explicit `log_path` is respected as explicit, even when its value equals the default path.

## Cache

A cache stores the compact output of single-file `ast` / `symbols` runs, keyed with BLAKE3. When the file content or the astro-sight version changes, the hash changes and the cache entry is invalidated automatically. The version is part of the key so that an old result is never returned when a change in the analysis logic or the output schema changes the result for the same content.

- **Commands**: `ast` and `symbols` (single-file mode only)
- **Cache key**: `BLAKE3(astro-sight version + canonical path + BLAKE3(file content))` plus a command-specific suffix (per combination of options)
- **path/lang separation**: the `ast` / `symbols` responses contain `path` and `lang`, so the same content in a different file or with a different extension gets a separate cache entry
- **Location**: `~/.cache/astro-sight/v<version>/`
- **Directory shards**: subdirectories are split by the first 2 characters of the hash (for example `v26.9.100/ab/cdef1234....symbols.json`)
- **Generation GC**: the version is part of the cache key, so every release invalidates all entries. Invalidated entries are not deleted by themselves, so unreachable data would pile up with every update if left alone (measured on a development machine: 176MB, almost all of it unreachable). Each generation therefore has its own directory, and the first run of a new generation deletes the old ones. Only old version directories (`v26.8.111`) and flat 2-digit hex shards that are not divided by generation (`00`–`ff`) are deleted. **Nothing with any other name is deleted** (it is under the user's `~/.cache`, so anything astro-sight cannot be sure it created is left alone). Deletion failures are ignored (analysis should keep working even when the cleanup fails)
- **`--pretty` skips the cache** (only compact output is cached)
- **`--no-cache`** disables the cache
