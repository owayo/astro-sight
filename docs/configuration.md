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

## Swift protocol methods

`dead-code` and `review` omit the five known witnesses for `NSApplicationDelegate`, `NSMenuDelegate`, `Hashable`, and `NSViewRepresentable` from both `dead_symbols` and `test_only_symbols`. Other public methods in the conforming type or extension remain candidates. The owning type can still be dead. API changes and `refs` output are unaffected.

Add external requirements with `[[swift_protocol_methods]]`:

```toml
[[swift_protocol_methods]]
protocol = "Acme.RefreshDelegate"
method = "refresh"                 # name = "refresh" is also accepted
labels = ["_"]                    # external labels, in declaration order
parameter_types = ["Swift.String"]
return_type = "Swift.Void"
static = false                    # optional; defaults to instance dispatch
```

The protocol name must include its module. Method names and labels must be Swift identifiers, and every parameter and the return value needs a valid Swift type. `labels` and `parameter_types` must have the same length. Invalid rows, missing fields, duplicate requirements, and unknown row fields fail configuration loading, including direct deserialization of `Config` and startup of `session` / `mcp`. Those transports do not expose dead-code or review commands.

The comparison includes the name, external labels, instance/static dispatch, parameter types, and return type. Use qualified type names to avoid ambiguity between imports. An ambiguous short name or a local declaration shadowing an SDK name prevents exclusion. Async, throwing, generic, conditional, and other unsupported declarations remain candidates. This is a syntax proof, without compiler type inference or expansion of aliases.

Conformances and implementations are collected from the full reference scan, even with `--git` or `--glob`. Separate files are joined only for ordinary literal Swift Package Manager targets under `Sources/<target>` or `Tests/<target>`, without custom source paths or dynamic manifests. Other layouts use evidence within the same file. Staged Swift dead-code analysis reads candidates, conformances, and references from the index snapshot; unstaged changes do not provide evidence. The existing duplicate-name/reference rules still apply to the remaining candidates, so calls to a witness can also keep a same-name helper from being reported.

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
