# Diff Analysis

The commands here take a unified diff (with `--git`, the `git diff` of the working tree) and examine what a change affects. `context` returns the impact, `impact` stops with exit 1 while impacts are unresolved, and `review` returns the impact, co-changes, public API changes, and dead symbols in one run. `dead-code` and `cochange` also work without a diff.

## context: Smart Context (Diff → Impact Analysis)

`context` takes a unified diff and analyzes the scope of the change, to support AI code review. Function signature changes are matched on identifier boundaries, so two different functions whose names only share a prefix, such as `foo` and `foo_bar`, are not confused.

```bash
# Get the git diff automatically and analyze the impact (recommended)
astro-sight context --dir . --git

# Analyze staged changes
astro-sight context --dir . --git --staged

# Use a custom base
astro-sight context --dir . --git --base HEAD~3

# Pipe from stdin
git diff HEAD~1 | astro-sight context --dir .

# Pass the diff as an inline string
astro-sight context --dir . --diff "$(git diff HEAD~1)"

# Read the diff from a file
git diff HEAD~1 > /tmp/changes.diff
astro-sight context --dir . --diff-file /tmp/changes.diff
```

`--base` of `context` / `impact` / `review` is passed straight to `git diff` / `git show` / `git blame`, so values that start with `-`, contain NUL, or are empty are rejected with `INVALID_REQUEST` (to keep something like `--output=/path` from being taken as an option).

Output:
```json
{
  "changes": [
    {
      "path": "src/engine/symbols/mod.rs",
      "hunks": [{ "old_start": 10, "old_count": 5, "new_start": 10, "new_count": 8 }],
      "affected_symbols": [
        { "name": "extract_symbols", "kind": "function", "change_type": "modified" }
      ],
      "signature_changes": [
        { "name": "extract_symbols", "old_signature": "fn extract_symbols(...)", "new_signature": "fn extract_symbols(..., include_refs: bool)" }
      ],
      "impacted_callers": [
        { "path": "src/commands.rs", "name": "cmd_symbols", "line": 166 }
      ]
    }
  ]
}
```

Callers fall into 3 groups by confidence and by how breaking they are. "Blocking" here means a finding that makes `impact` or `review --hook` return exit 1.

- `impacted_callers`: actual call sites. Those left outside the diff are blocking for `impact`. References to symbols whose number of arguments changed, and references that cannot be decided, also stay here
- `low_confidence_callers`: generic names whose owner (the class or type a method belongs to) cannot be determined, and same-name references in TS/Rust without evidence of a direct import
- `informational_callers`: function-value references to symbols whose name and number of arguments did not change, and import lines of modified symbols whose name did not change. Not blocking

## impact: Detect Unresolved Impacts (for Stop Hooks)

From the result of `context`, `impact` flags impacts on files outside the diff as "unresolved". It is meant for the Stop hook of an AI agent: when affected code is left untouched, it blocks and prompts the agent to continue. `impact` only follows symbols that still exist in the changed tree, so deleted symbols are not detected (exit 0 even if calls remain). To stop on deletions too, use `review --dir . --git --hook`, which detects removed public APIs as `api.rm`.

Signature changes are matched on identifier boundaries as in `context`, so a change to a test helper or a derived name does not spread as if the base function had changed. Local symbols declared inside a function are not used as starting points of cross-file impacts. TypeScript/JavaScript, Rust, Python, Go, Java, and Kotlin are supported, and nested Kotlin functions are told apart from top-level functions with the same name.

```bash
# Get the git diff automatically and detect unresolved impacts (recommended)
astro-sight impact --dir . --git

# Check staged changes
astro-sight impact --dir . --git --staged

# Use a custom base
astro-sight impact --dir . --git --base HEAD~3

# Pipe from stdin
git diff HEAD~1 | astro-sight impact --dir .
```

- Nothing unresolved → exit 0 (no output)
- Something unresolved → text on stderr + exit 1
- `--dir` is not in a git repository → exit 0 (no output, with or without `--hook`; see "Skipping Directories Outside Git" below)

Output (on exit 1):
```text
Unresolved impacts found:

src/engine/symbols/mod.rs changed [extract_symbols]:
  → src/service.rs:284 [extract_symbols]
  → src/commands/api_changes/exported.rs:45 [extract_symbols]
```

Line numbers in the text output are 1-based so that an editor can open them as they are (`line` / `ln` in JSON output are 0-based).

An example with claw-hooks (write it in the global configuration `~/.config/claw-hooks/config.toml`; claw-hooks ignores `stop_hooks` written in a project's `.claw-hooks.toml`):
```toml
[[stop_hooks]]
commands = ["astro-sight impact --git --dir ."]
condition = { command_exists = "astro-sight" }
```

If you register the command directly in the Claude Code hooks settings, exit 1 on a finding is treated as a non-blocking error and Claude does not stop. With `stop_hooks` that have a `condition`, as in the example above, claw-hooks returns the command failure to Claude as a block, and Claude stops.

### Skipping Directories Outside Git

When a command that accepts `--git` (`context` / `impact` / `review` / `dead-code` / `cochange`) runs in a directory outside git, the failure of the internal `git diff` is not treated as an error. The run is skipped as "nothing to analyze" and ends normally with **exit 0**. This keeps the Claude Code Stop hook from blocking while you edit in a directory outside git, such as `~/.config`.

- `--hook` (`review` / `impact`) → no output on stdout or stderr, exit 0
- Normal CLI use → an empty normal result with a machine-readable `skipped` field, exit 0. You can tell "no changes" from "not in git" (`impact`, which has no structured output, prints nothing)

```json
{ "...": "...", "skipped": { "reason": "not_git_repository", "source": "git", "message": "--git was requested but --dir is not inside a git worktree" } }
```

The check uses `git rev-parse --is-inside-work-tree` (with `LC_ALL=C`), so worktrees, submodules, and bare repositories are detected correctly. **Real errors** (an invalid `--base`, git not runnable, a broken repository, missing permissions) still return `exit 1`. Passing a diff through `--diff` / `--diff-file` / stdin does not go through this check, so nothing changes there.

### Limit on Untracked Files

`--git` (without `--staged`) includes untracked source files in the analysis as new files (so that references to untracked files created in the same piece of work are not reported as unresolved impacts outside the diff). **Untracked files larger than 256KB or 5,000 lines are left out**, though. Pulling in generated output such as code generator results or huge fixtures would make every exported symbol in them a candidate for the API diff, `review` would take tens of minutes, and the Stop hook would time out. In a measurement, a `review` that finished in 1.75 seconds without untracked files did not finish within 10 minutes after adding an untracked file with 22,000 `pub fn` in total.

Tracked files are not limited. A file that has been committed or added can be seen as intentionally put up for review. An untracked file, on the other hand, has not been added yet; whether it will be committed is unknown, and a huge one is likely generated.

Excluded files are not dropped silently; they are listed in `truncations` (so that they are not mistaken for reviewed files):

```json
{ "...": "...", "truncations": [{ "path": "generated.rs", "reason": "untracked_file_too_large", "message": "untracked file excluded from --git analysis: lines 80000 exceeds limit 5000" }] }
```

With `--hook`, this is printed as `trunc: [{"f": "generated.rs", "r": "untracked_file_too_large"}]` (it reports the analyzed scope rather than a finding, so it does not cause exit 1). `impact`, which has no structured JSON, prints it as a `note:` line on stderr. `--staged` / `--diff` / `--diff-file` respect the scope given explicitly and do not pull in untracked files at all.

### Reporting Sources That Cannot Be Analyzed

`dead-code` / `review` also list in `truncations` the **source files that exist in the directory but that no backend could analyze**. Declaring a symbol dead without counting the references inside unreadable files would report live symbols as dead (for example a TypeScript function used only from the `<script>` of a `.vue` file). The report lets you tell "there are no references" from "references could not be observed".

```json
{ "...": "...", "truncations": [{ "reason": "unanalyzable_source", "message": "1 \".vue\" file(s) were not analyzed (no parser for this language); references inside them are not counted (e.g. src/App.vue)" }] }
```

Only **extensions that are certainly program or template languages** are reported (`.vue` / `.svelte` / `.astro` / `.erb` / `.razor` / `.scala` / `.dart` / `.lua`, and so on). Files outside the scan also include images, archives, and data, and reporting all of them would bury the sources that are really missed in noise. The report is folded into one entry per extension, with at most 10 extensions and 3 example paths. When there are no such files, `truncations` is not printed at all.

`dead-code` reports the unanalyzable sources in the scope where references were counted (the whole directory). Narrowing the dead candidates with `--glob` or `--git` still counts references in the whole directory, so the scope of the report is not narrowed either.

### Default Exclusions

The impact analysis of `context` / `impact` / `review` leaves third-party dependencies and build artifacts out of the cross-file reference search by default. This keeps generic method names such as `new` / `save` / `find` / `update`, pouring in from third-party and generated code, from burying the impacts in tens of thousands of false positives.

- Vendored code and package managers: `vendor`, `node_modules`, `bower_components`, `.venv`, `venv`, `.tox`, `Pods`, `Carthage`
- Build artifacts: `target`, `build`, `dist`, `out`, `.build`, `DerivedData`, `bin`, `obj`, `coverage`, `.next`, `.nuxt`, `.svelte-kit`, `.turbo`, `CMakeFiles`
  - Among the `bin` directories, `src/bin/` of a Cargo package (directly under `src`, whose parent has a `Cargo.toml`) holds the sources of binary targets and is not excluded. An explicit `--exclude-dir bin` excludes every `bin`

To turn off the default exclusions:

```bash
ASTRO_SIGHT_INCLUDE_VENDOR_FOR_IMPACT=1 astro-sight impact --dir . --git
```

The exclusion of `.gitignore`d and hidden files and the detection of generated files (through `refs::collect_files`) are separate mechanisms from these default exclusions. Of these, only the exclusion of generated files can be turned off, with `--include-generated` or `skip_generated = false`.

### Additional Exclusions You Specify

To exclude directories whose names are not in the fixed list (`pjproject-2.15`, `openssl_64_1.1.1c`, `third_party`, and so on), or to narrow the impact analysis with finer glob patterns, use `--exclude-dir` / `--exclude-glob`. The same options work in `context` / `impact` / `review`, and they are **added** to the fixed list (they do not replace the default exclusions).

```bash
# Exclude a vendored C library
astro-sight impact --dir . --git \
  --exclude-dir pjproject-2.15 \
  --exclude-dir openssl_64_1.1.1c

# Cover several versions with a glob
astro-sight impact --dir . --git \
  --exclude-glob '**/openssl_*1.1.1*/**'

# In review, the same options apply to both impact and dead_symbols
astro-sight review --dir . --git \
  --exclude-dir pjproject-2.15 \
  --exclude-glob '**/openssl_*/**'
```

`--exclude-glob` is treated as a negative pattern of `ignore::overrides` (no leading `!` needed; relative to the workspace). Invalid glob syntax is rejected with `INVALID_REQUEST` before the run.

## review: Structured Diff Review

On top of the impact analysis of `context`, `review` returns candidates for missed changes from `cochange`, the public API diff, and dead symbols in a single run. It is meant for PR reviews and pre-merge checks.

With `--git --base <rev>`, the blame analysis of `missing_cochanges` uses the same base. When a PR with several commits is reviewed as a whole, the diff and the co-change candidates cover the same range.

`missing_cochanges` only proposes pairs that changed together 3 or more times (`--cochange-min-samples`, default 3). With blame on the changed lines, starting points with only 2 evidence commits are common, and pairs that "changed together only once" would line up at the top with confidence 1.0. To look at small samples exploratively, pass `--cochange-min-samples 2` (the standalone `cochange` command keeps the default of 2). Deduplicating the candidates and picking the top 10 use the same smoothed `score` as the standalone command. This keeps a small sample such as 3/3 from mechanically ranking above a sufficient sample such as 30/40, while the raw confidence is still used for showing the evidence and for the threshold.

Lock files and the dependency declaration files that correspond to a source (`Cargo.toml` / `package.json` / `pyproject.toml`, and so on) are not proposed in `missing_cochanges`. In a commit that adds a dependency, these files always change together with the source, so the historical correlation is 100%. That correlation holds only "when a dependency is added", though, and has no causal link to changes of the code that neither add nor remove an import. A dependency declaration file is left out only for the pairing with the nearest one in the same ecosystem as the source (a pair from different ecosystems, such as `Cargo.toml` and a Python script, stays a candidate). The standalone `cochange` command keeps showing dependency declaration files as the fact that they "changed together in the past" (lock files are generated, so both leave them out).

**The relation between an external snapshot and the test that generates it is treated as directional.** A diff that only updates a snapshot does not get "the generating test may be missing a change". A snapshot is also updated when the output of the code under test changes, so "if the test changes, the snapshot changes too" is a fair expectation, but the converse is not. The other direction stays: when a test changes and its snapshot is missing from the diff, it is proposed as before. Only pairs that meet all of the following are suppressed; if any of them cannot be confirmed, the pair stays a candidate as before:

- The snapshot's parent directory is exactly `__snapshots__`, and its path with the trailing `.snap` removed once matches the missing candidate exactly (the standard convention shared by Jest / Vitest / Bun: `tests/__snapshots__/widget.test.tsx.snap` → `tests/widget.test.tsx`)
- The generating test exists as a regular file
- **The first line of the snapshot exactly matches a known runner header** (`// Vitest Snapshot v1, …`, and so on). The path convention alone would also catch handwritten fixtures and `.snap` files for other uses, so the file itself has to show that it is generated output

This is independent of `linguist-generated` in `.gitattributes` (that declares generated files in general, and removes the pair from the candidates in both directions). Custom snapshot resolvers, inline snapshots, and formats other than `.snap` are not covered, and all of them keep the historical correlation as information as before. The global `--include-generated` also turns this direction off. The standalone `cochange` command is for exploration and is not directional.

This does not prove that no test change is needed (you can update only the expected values and forget a needed change in the test logic). It is a recommendation policy: for pairs in the standard generation relation, a change in the reverse direction is not requested on the basis of historical correlation alone.

`api_changes.compatible_modified` lists changes where the signature string changes but existing calls stay compatible. The following changes are informational and not blocking for `--hook`:

- Wrapping a React component in a HOC
- Removing an object member that nothing references
- Adding optional / default parameters at the end of a top-level TS/TSX function (`trailing_optional_params`)
- Adding keyword-only parameters with defaults or positional parameters with defaults at the end of a top-level Python function or of a method of a class directly under the module (`trailing_optional_params`). When decorators differ or there are several definitions with the same name, the change stays blocking to be safe

The `impacts` tied to the same symbol are not reported as breaking either; they only appear as `mod_compat` information. To detect unreferenced object members, the removed keys are pre-extracted all at once with Aho-Corasick instead of searching the whole repository once per key, and each JS/TS file is parsed at most once. When collecting, reading, or parsing a file fails, the change is not downgraded to compatible and stays blocking as before.

Value bindings such as `export const` are compared as whole declarations (including the initializer), but **when the value itself is a function, its body is left out of the comparison** (consistent with a body change of `export function` not being an api.mod). Only the bodies of the following functions are left out:

- An arrow function or function expression that is the value itself (including ones in parentheses or with `as` / `satisfies`)
- A function wrapped in React's `memo` / `forwardRef`
- A function that is a member of an object literal (methods and `key: () => ...`)

Parameters, type annotations, and added or removed keys are still compared. Callbacks passed to other calls (such as `create((set) => ({ ... }))`) have bodies that decide the shape of a store or the value itself, so their bodies are compared too. Destructured bindings (`export const { a, b } = obj`) are compared by "the path to that name + the initializer" (arrays keep positions), so that adding or removing other bindings of the same destructuring does not make the remaining bindings an api.mod. Patterns with default values, computed keys, or rest elements are compared as whole declarations.

Changes to Python's public type contracts that can be classified by direction get `{kind, breaks}` as `api_changes.modified[].contract_change` (`api.mod[].contract` in hook output). This covers changes to whether `TypedDict` keys are required, and changes to the value set of a direct `Literal` type alias directly under the module. For `Literal`, a narrower value set is reported as `literal_values_narrowed` (breaks the producer side) and a wider one as `literal_values_widened` (breaks the consumer side). This applies only when the `Literal` can be proven to come from `typing` / `typing_extensions` and its values are only strings without escapes or prefixes, decimal integers, booleans, or `None`. When the meaning cannot be determined statically, as with replaced values, a dynamic `__all__`, a shadowed name, or star imports, the direction is not guessed and the change stays a normal blocking `api.mod`. Test files are not covered, following the existing public API surface conventions. A type alias entry is a pseudo-symbol with `kind = "type"`, and it is not added to what `symbols` / `refs` / `dead-code` analyze.

Symbols that are called implicitly at run time are excluded differently in the API diff and in dead-code. PHPUnit conventions, TS/JS constructors, and Flyway migrations are excluded from both public surfaces. Laravel relations and Angular lifecycle hooks, on the other hand, are excluded from dead-code but stay in the API diff, so that changes to externally visible signatures are not missed. `--framework` selects dead-code conventions and does not change this API diff boundary across the board.

```bash
# Get the git diff automatically and review it (recommended)
astro-sight review --dir . --git

# Review staged changes
astro-sight review --dir . --git --staged

# Use a custom base
astro-sight review --dir . --git --base HEAD~3

# Use a patch or PR diff you already have
astro-sight review --dir . --diff-file /tmp/pr.patch
```

Output:
```json
{
  "impact": { "changes": [...] },
  "missing_cochanges": [
    { "file": "src/service.rs", "expected_with": "src/commands.rs", "confidence": 0.75 }
  ],
  "api_changes": {
    "added": [],
    "removed": [],
    "modified": [
      {
        "name": "greet",
        "kind": "function",
        "file": "src/new.rs",
        "old_signature": "pub fn greet() -> i32 {",
        "new_signature": "pub fn greet(name: &str) -> i32 {"
      }
    ]
  },
  "dead_symbols": []
}
```

## dead-code: Detect Dead Code

`dead-code` detects symbols that are exported but never referenced. With a diff, only the files related to the change are scanned; without one, the whole project is scanned.

```bash
# Scan the whole project
astro-sight dead-code --dir .

# Scan only Rust files
astro-sight dead-code --dir . --glob "**/*.rs"

# Scan only the files related to the git diff
astro-sight dead-code --dir . --git

# Only the files related to staged changes
astro-sight dead-code --dir . --git --staged
```

Output:
```json
{
  "dir": "/path/to/project",
  "scanned_files": 48,
  "dead_symbols": [
    { "name": "unused_helper", "kind": "function", "file": "src/utils.rs", "line": 12 },
    { "name": "OldConfig", "kind": "struct", "file": "src/config.rs", "line": 40 }
  ]
}
```

`line` is the line of the declaration (0-based). Symbols referenced only from tests are not included in `dead_symbols`; they are listed separately in `test_only_symbols`.

When symbols with the same name exist in several files, they are skipped to avoid false findings. Class members in TS/JS and PHP are an exception, judged only when their owner can be inferred safely and uniquely. In PHP, `Owner::method()` and `self::method()` inside the same class count as definite references. When there are references whose owner cannot be determined, such as `$obj->method()` or callable strings, the symbol is skipped as before. `static::` can reach a subclass override through late static binding, so it is not resolved as a definite reference. Static calls through a class / trait / enum that `use`s a trait count as references only for a trait method reached uniquely. If the composing class has a concrete method with the same name, the resolution does not go to the trait, following PHP's resolution order.

### Excluding Run-Time Conventions Automatically

Symbols that frameworks and test runners call dynamically, by naming convention or reflection, cannot be traced to a caller through identifier-level cross-file references and would be false findings. The following conventions are therefore excluded from dead-code automatically:

- **PHPUnit**: `*Test` / `*TestCase` / `*IntegrationTest` / `*FeatureTest` classes and `testXxx` / `setUp` / `tearDown` / `setUpBeforeClass` / `tearDownAfterClass` methods
- **Python unittest**: classes that inherit `unittest.TestCase` (and `unittest.IsolatedAsyncioTestCase`), with indirect inheritance within the same file resolved to a fixed point, and their `test_*` / `setUp` / `tearDown` / `setUpClass` / `tearDownClass` / `addCleanup` / `addClassCleanup` methods
- **Python pytest**: top-level `test_*` functions in `test_*.py` / `*_test.py` files, and every function in `conftest.py`
- **Python framework registration decorators**: functions, methods, and classes with registration decorators of Typer / Click / FastAPI / Flask / Django / Celery / pytest, and so on
- **Python dynamic protocol methods**: `*_open` / `*_request` / `*_response` / `http_error_*` of the `urllib.request.BaseHandler` family, and the `on_*` callbacks of watchdog's `FileSystemEventHandler` family. Only methods of classes that directly inherit the known base classes are excluded
- **Angular**: lifecycle hooks of classes decorated with `@Component` / `@Directive` (`ngOnInit` / `ngOnDestroy` / `ngOnChanges` / `ngDoCheck` / `ngAfterContentInit` / `ngAfterContentChecked` / `ngAfterViewInit` / `ngAfterViewChecked`) are excluded because the Angular runtime calls them in the change detection cycle. `transform` of `@Pipe` classes (called from `| name` in templates) and `ngOnDestroy` of `@Injectable` / `@Pipe` classes (called when a service / pipe is destroyed) are excluded too. Other hooks of services and pipes (`ngOnInit`, and so on) are not called by Angular and are not excluded
- **Program entry points**: the following are excluded (they stay in the API diff)
  - C / C++: `main` at global scope
  - Kotlin: top-level `fun main`, and `@JvmStatic fun main` directly inside an `object` / `companion object`
  - Java: non-private `void main` (with no parameters or a single `String[]`, including Java 25 instance main)
  - C#: `static Main` (with no parameters or a single `string[]`)
  - In Java / C# / Kotlin, the type that declares the entry point (including the outer types of a nested type) is excluded too

### Generated Files

Files detected as generated (a generated-file declaration comment at the top, `linguist-generated` in `.gitattributes`, and so on) are left out of the **candidates** for dead by default. References inside generated files are always counted, though. Generated code calls hand-written code at run time, as when generated gRPC handlers (`*_grpc.pb.go`) call a hand-written server implementation, and dropping those references would report live symbols as dead.

Files left out of the candidates are listed in `generated_candidates_skipped`, in the same shape as `skipped` of `refs` (not printed when there are none). `--include-generated` (or `skip_generated = false`) makes the symbols of generated files candidates too.

```json
{ "...": "...", "generated_candidates_skipped": { "generated": 1, "paths": ["api/greeter_grpc.pb.go"] } }
```

### Detecting Frameworks Automatically

Even without `--framework`, the `nextjs` preset is applied automatically when a `package.json` directly under `<dir>` or in a monorepo package has a `next` key in `dependencies` / `devDependencies`. The convention globs detected this way are limited to paths relative to each Next.js workspace, so `app/**/page.tsx` in a sibling workspace that is not Next.js is not excluded. `node_modules`, generated output, and symlinks are not searched, and `peerDependencies` / `optionalDependencies` are ignored because they tend to trigger the preset by mistake. An explicit choice (`--framework laravel`, and so on) always wins over the detection.

```bash
# Applied automatically when a package.json at the root or in a workspace has `next`
astro-sight dead-code --dir .
astro-sight review --dir . --git
```

### Leaving Bin-Only Rust Crates Out of the API Diff

`api_changes` of `review` (`added` / `removed` / `modified`) automatically leaves out `pub fn` changes in bin-only Rust crates (no `src/lib.rs` and no `[lib]` section in `Cargo.toml`). A `pub fn` of a bin-only crate cannot be reached from outside the crate, so adding, removing, or changing its signature is never a compatibility problem of a public API. Even when the same diff deletes `src/lib.rs`, if the crate was a library crate at the base revision, the removal of the old public API correctly stays in `removed`.

## cochange: Detect Co-Change Patterns

`cochange` finds files that tend to change together with the given files, from git blame and diff-tree. `missing_cochanges` of `review --git --base <rev>` uses the same analysis.

```bash
# Take the starting files from the git diff
astro-sight cochange --dir . --git --base HEAD~5

# Name the starting files explicitly
astro-sight cochange --dir . --paths src/service.rs

# Follow renames / copies
astro-sight cochange --dir . --git --base HEAD~10 --rename --copy
```

`--paths-file` is read with a 100MB limit, and an empty list returns `INVALID_REQUEST`. `--min-confidence` accepts only finite values in `0.0..=1.0`, and `--smoothing-alpha` / `--smoothing-beta` only finite non-negative values. Source files given with `--paths` / `--paths-file` must be relative paths under `--dir`; paths containing `..`, absolute paths, and Windows paths with a drive prefix are rejected with `PATH_OUT_OF_BOUNDS`.

**Generated output is excluded both as a starting point and as a candidate.** Files written together by batch jobs, code generation, or builds almost always land in the same commit, so otherwise "mechanical simultaneous updates" would be presented as co-changes with high confidence. The decision gives `linguist-generated` in `.gitattributes` the highest priority (`set` / `true` excludes the file, and `unset` / `false` keeps it without looking at header markers). Without that attribute, generated-file markers at the top of the file (`@generated` / `DO NOT EDIT`, and so on) decide. If neither decides, the file stays a candidate. Extensions and update frequency are not used, so declare generated JSON / CSV files, which cannot hold comments, in `.gitattributes`.

```gitattributes
data/*.json linguist-generated=true
fixtures/hand-maintained.yaml -linguist-generated
```

**The denominators of the remaining starting points do not change** with the exclusion (removing commits in which generated files were present from the denominator would turn `1/2` into `1/1`, a selection bias). The numbers of exclusions are in `excluded_generated_sources` / `filtered_generated_candidates` of `diagnostics`. When `git check-attr` cannot be run, nothing is excluded at all (reported as `GeneratedAttrLookupFailed`), and candidates are not removed just because the decision is impossible. The global `--include-generated` (or the equivalent `skip_generated = false` in `config.toml`) includes generated output too. The option affects not only the standalone `cochange` but also `missing_cochanges` of `review`.
