---
name: astro-sight
description: >-
  Analyze source code with the astro-sight AST CLI. Use refs instead of grep/rg
  for code identifiers, including definitions and callers; review for diffs and
  PRs; context/impact during and after edits; and ast for syntax boundaries or
  parse errors. Use metrics for complexity, dependency cycles, ACD and propagation
  cost; symbols, calls, imports, sequence, cochange, dead-code, lint,
  and session for related structural analysis.
allowed-tools: Bash(astro-sight:*)
---

# astro-sight

Use AST evidence to locate code, inspect dependencies, and assess changes. Results
are static analysis within the selected scope, not a compiler check or proof of
runtime behavior. Check the installed CLI with `astro-sight <command> --help`
when an option or output field differs from this guide.

## Choose the query

Before using grep/rg, inspect the search pattern itself. If it contains a code
identifier, use `refs`, even for one file, a definition such as `fn parse`, or a
search that needs surrounding lines. Use returned locations to read more context.
Text search remains appropriate for error messages, configuration values, prose,
TODO comments, and filename discovery.

| Need | Command |
|---|---|
| Definitions and uses of one identifier, including callers | `astro-sight refs --name <name> --dir .` |
| Several identifiers | `astro-sight refs --names Foo,bar --dir .` |
| One file's occurrences | `astro-sight refs --name <name> --dir . --glob 'src/file.rs'` |
| Diff or PR review | `astro-sight review --dir . --git` |
| Affected symbols, signatures, and callers | `astro-sight context --dir . --git` |
| Unresolved caller impacts after editing | `astro-sight impact --dir . --git` |
| Exported symbols with no observed uses | `astro-sight dead-code --dir . --git` |
| Definitions and complexity | `astro-sight symbols --path <file>` |
| Complexity, dependency cycles, CCD, ACD and propagation cost | `astro-sight metrics --dir .` |
| Architecture metrics grouped by directory | `astro-sight metrics --dir . --component directory --component-depth 2` |
| Metrics compared against a Git commit | `astro-sight metrics --dir . --base HEAD` |
| Directory inventory | `astro-sight symbols --dir <dir>` |
| Calls made by a function | `astro-sight calls --path <file> --function <name>` |
| Imports and exports | `astro-sight imports --path <file>` |
| Ordered call flow | `astro-sight sequence --path <file> --function <name>` |
| Historical co-change candidates | `astro-sight cochange --dir . --paths <file>` |
| Exact syntax node | `astro-sight ast --path <file> --line <n> --col <n>` |
| Repeated AST/text rules | `astro-sight lint --path <file> --rules rules.yaml` |

`calls --function X` lists X's **callees**, not its callers. Use `refs --name X
--dir .` for callers, then inspect the occurrence: definitions, imports, and other
uses also appear in `refs`. Same-name matches do not establish binding identity.
Use `sequence` when order or a chain of three or more interactions matters.

For two or more structural queries, use `session` from the start; use `refs
--names` when only searching several names. See the examples below.

## Reference searches and coverage

`refs` matches identifier occurrences rather than arbitrary text substrings.
Results contain `refs` with `path`, `ln`, `col`, `ctx`, and `kind`
(`def`, `ref`, or `unknown`). `ctx` supplies the source line; read the file when
surrounding logic is needed. `unknown` can retain a Bash/zsh declaration header
inside a parse-error region and does not verify a definition or use.

Directory scans respect ignore rules and scope filters. Generated files are
skipped by default and reported in `skipped`; use `--include-generated` when
those files matter. Generated detection honors `.gitattributes`
`linguist-generated` overrides before filename/header heuristics.

Read coverage notices before interpreting an empty result. Public `refs` reports
unsupported source types in `truncations` (`reason: unanalyzable_source`) when their
contents may match a requested name. This filter is independent of AST hit counts:
a zero-reference query still reports a matching unsupported source, while files
without the requested name do not generate notices. Unreadable, oversized, or
undecodable sources remain reported conservatively. These files were not parsed;
inventory file types when scope-wide completeness matters. A nonempty scope with
no searchable file returns
`UNSUPPORTED_LANGUAGE`; a scope matching no files can return an empty result.
An absent notice does not prove coverage of unknown extensions, ignored files,
external consumers, or dynamic uses. Do not repeat a successful supported-source
identifier search with grep/rg merely because it returned zero. For an unsupported
or failed source, inspect the diagnostic and use text search where AST analysis
cannot cover it, stating that limitation.

### Output limits

`refs` defaults to at most 100 displayed occurrences and an estimated 3,000-token
budget. These limit output, not the search. When occurrences are omitted,
`result_summary` gives `shown`, `total`, `omitted`, and `limited_by`. Its `by_lang`,
`by_kind`, and `files` describe **omitted occurrences only**; use them to narrow
`--glob` before requesting a much larger output. `total` counts the analyzed
input, not necessarily the whole repository. When present, `complete_input:
false` explicitly warns that input coverage is incomplete.

To remove both output limits:

```bash
astro-sight refs --name <name> --dir . --max-results unlimited --token-budget unlimited
```

Setting only one limit to `unlimited` leaves the other in effect. A
`budget_exceeded` summary means even the minimized output exceeded the estimate;
raise the budget, reduce names, or narrow the scope. Sample paths may be shortened
before occurrences are cut: `paths_limited_by` / `examples_limited_by` describe
those samples, while their counts remain. No `result_summary` means no reference
occurrences were omitted, not that every possible input was analyzed. With CLI
`--names`, shared coverage notices appear only on the first result.

## Editing and reviewing code

Before changing a function or type, enumerate its uses with `refs`. A clean
working tree gives `context --git` no changes to analyze. Once edits exist, run
`context` before moving to another file and `impact` after the changes. Begin a
diff/PR review with `review`, which combines impact, API differences, dead-symbol
candidates, and historical co-change evidence.

```bash
astro-sight refs --name <name> --dir .
# Make the intended change.
astro-sight context --dir . --git
# Update affected code and validate behavior.
astro-sight impact --dir . --git
astro-sight review --dir . --git
```

Review contracts/types, implementation, callers, and tests in dependency order.
Verify findings against source and a concrete failing scenario before changing
code. An informational classification or a caller on an edited line is not a
substitute for a type check or relevant behavioral validation.

### Select the actual diff

- `--git` compares `HEAD` with the working tree, including staged changes. A clean
  tree has no diff; untracked files are not part of Git's ordinary diff.
- `--git --base <rev>` compares that revision with the working tree. For example,
  `HEAD~3` includes the last three commits **plus uncommitted changes**. Select the
  intended PR base explicitly rather than assuming `--git` reviews committed work.
- `--git --staged` selects the index. `review` analyzes an index snapshot; read
  `staged_snapshot_unavailable` if it falls back. Do not interpret a fallback with
  divergent working-tree files as a complete staged-only proof.
- `context` and `review` also accept `--diff-file changes.patch`, `--diff`, or stdin.
  Keep `--dir`, source checkout, and `--base` consistent with the patch, especially
  for deletions. `impact` accepts `--git` or stdin, not `--diff-file` / `--diff`.

### Interpret review and hook results

Normal `review` returns `impact`, `api_changes`, `dead_symbols`, and
`missing_cochanges`, with diagnostics and additional informational fields when
applicable. Inspect `impacted_callers`, `low_confidence_callers`, and
`informational_callers` separately: import/re-export occurrences and bare-name
matches are not equivalent to verified affected calls.

API changes may be blocking removals/modifications or informational categories
such as compatible changes, moves, dead cleanup, and changes closed within the
diff. Read each entry's `reason`, `unverified`, `contract_change`, or
`compatibility` when present. Compatibility proofs have bounded language and
repository scopes; aliases, ambiguous ownership, incomplete parsing, and external
consumers can remain unproven. In particular, an optional-prop removal classified
compatible within the repository does not prove compatibility for external JSX
consumers. Do not reproduce classifier rules manually or demote an unverified
finding because all matching lines changed.

- Normal `review` can exit 0 while reporting findings. Use `review --hook` when
  the command must signal blocking findings through its exit status.
- `review --hook` emits compact JSON to stderr when it has findings or coverage
  notices.
  `blocking_categories` identifies the buckets responsible for exit 1; an empty
  array means informational output. No findings or notices can mean no output.
- `--strict-public-const-values` promotes value-only const/static and callback-body
  categories to blocking; `--strict-public-type-annotations` independently blocks
  unverified annotation-only changes. A callback-body category does not prove the
  returned public shape stayed compatible.
- Hook dead scope defaults to `touched-symbols` and omits newly added exports;
  `--include-wip-dead` restores those warnings. Normal review defaults to `all`
  within the diff-selected files. Use `dead-code --dir .` for a project-wide scan.
- `impact` normally stays silent and exits 0 when no unresolved callers remain.
  Findings go to stderr with exit 1; `--hook` adds a triage hint. Errors can also
  exit 1, so inspect both streams. It is not a structured review JSON command.
- Persistent Git input changes make `review --git` return `REVIEW_INPUT_CHANGED`
  on stdout and exit 2 after bounded retries. Stabilize the index/worktree and
  rerun. Neither exit 0 nor an empty blocking list establishes complete coverage;
  read `truncations`, errors, and skip notices as well.

### Dead code and co-change evidence

Dead-code results are candidates based on observed references. Read
`unverified_symbols`, `truncations`, `generated_candidates_skipped`, and
`test_only_symbols` when present. Missing parsers, read/parse failures, framework
entrypoints, and consumers outside the repository limit deletion decisions.
Generated files are excluded as dead candidates by default, but their references
within the scan are counted because generated code can call handwritten code.
`--include-generated` also makes their symbols candidates. `--framework` adjusts
dead-symbol conventions; it does not establish API compatibility.

Read `external_entry_symbols` and their registration evidence alongside
`unverified_symbols`. Verified host entries do not block as dead code; host
registration and API compatibility are separate evidence. VS Code entries need
manifest and source-mapping evidence; unsupported mappings remain unverified.
Other hosts can declare an exact file, named export, and reason in
`.astro-sight/entrypoints.json`; invalid or stale declarations fail explicitly.

`missing_cochanges` and `expected_with` are legacy names for historical
associations. Their `interpretation`, frequency ratios, and ranking scores do not
prove that a change is required or complete. Even `verification_target` or a
recent-change resolution is a prompt to compare current dependencies with prior
work, not a requirement to edit or proof of correctness. Unknown interpretation
values remain unassessed. Check source/import evidence before acting.

Read `cochange_diagnostics` (review) or `diagnostics` (cochange), especially for
empty results: no history, new files, filtering, and failed commit scans differ
from evidence of no association. Standalone `cochange` can fall back from
changed-line blame to file history (`evidence: history`). Its default
`--min-samples` is 2; review's `--cochange-min-samples` is 3. Lower thresholds only
when the evidence warrants it. `confidence` filters the raw frequency ratio;
smoothed scores rank candidates and are not probabilities of a missing edit.

## Structural queries and sessions

`metrics` defaults to file components; `--component directory` groups by up to
`--component-depth` parent segments relative to `--dir` (positive, default 1).
Internal edges are removed and coupling/cycles recalculated after grouping.
`files` / `file_edges` preserve file detail. CD includes self, CCD sums CD,
ACD = CCD/N, and propagation cost = CCD/N² (all 0 for empty input).
Dependencies are static local imports with bounded language/resolution coverage,
not compiler-verified bindings. Rust uses declared module ownership and literal
`#[path]`; JS/TS supports selected `baseUrl` / `paths` and relative `extends`
rules. Read `resolution_summary`, `external_dependencies`,
`unresolved_dependencies`, `diagnostics`, `truncations`, `skipped`, and
`limitations`; an empty `cycles` does not prove the project acyclic. Edge
`evidence` retains source files and zero-based lines; `cycle_evidence` gives one
real representative path per SCC, not all cycles or necessarily all its members.

`--base <rev>` remeasures a raw commit snapshot with the same options and compares
it with the working tree, including selected untracked files. Inspect the full
`comparison.baseline`, `delta`, `normalization_changed` (N), `coverage_changed`,
and `assessment`; partial coverage or a changed denominator can change ACD/PC
without establishing improvement. Cycle-group differences compare SCC member
sets. Function changes retain ambiguous name groups rather than guessing overload
identity. Snapshot budget/path failures are errors; omitted links/submodules are
diagnosed. `--glob` narrows the graph inventory and metadata is read only inside
`--dir`. Session accepts
`{"command":"metrics","dir":".","component":"directory","component_depth":2,"base":"HEAD"}`;
MCP exposes `metrics_analyze` with the same options.

For multiple queries, send one JSON request per line to `session`. Each response
is a JSON line, possibly containing an error; inspect every response, not just
the process exit code. The examples use an existing source path; substitute paths
for the current project.

Bash:

```bash
printf '%s\n' \
  '{"command":"symbols","path":"src/main.rs"}' \
  '{"command":"calls","path":"src/main.rs","function":"main"}' \
  | astro-sight session
```

PowerShell:

```powershell
@(
  '{"command":"symbols","path":"src/main.rs"}'
  '{"command":"calls","path":"src/main.rs","function":"main"}'
) | astro-sight session
```

Session commands: `ast`, `symbols`, `doctor`, `calls`, `refs`, `context`,
`imports`, `lint`, `sequence`, `cochange`, `metrics`. `review`, `impact`, and `dead-code`
are CLI-only. Session fields are not CLI flags: use `column` rather than `col`,
`context_lines` rather than `context`, and arrays for `names` / `source_files`.
Session `symbols` needs a file `path`; it does not provide CLI `symbols --dir`.
Session `context` needs the actual diff in `diff`; it does not run `--git`.
Session `cochange` needs nonempty `source_files`, even when a base is supplied.

Session paths stay within the workspace. `ASTRO_SIGHT_WORKSPACE`, when set,
selects that root and the base for relative paths; invalid roots fail closed.
Do not work around `PATH_OUT_OF_BOUNDS`; use the intended workspace.

Most file-oriented CLI commands also accept `--paths a.rs,b.rs` or
`--paths-file files.txt`. Batch by that mechanism when repeating one command.
For command-specific schemas or less common options, consult `--help` instead
of transferring flags between commands.

## Output and operational notes

- JSON locations (`ln` / `line`, `col` / `column`) and `ast --line` / `--col`
  inputs are zero-based; tree-sitter columns count UTF-8 bytes. Human-readable
  `impact` stderr locations are one-based. Convert JSON line numbers when linking
  to an editor; do not reuse display coordinates as AST input without conversion.
- Compact JSON is the default unless config selects another format. `--format
  json|toon|auto` overrides config. `--pretty` formats JSON; `symbols --full`
  requests its legacy full schema and `--doc` includes docstrings.
- JSON batch results are normally NDJSON; TOON batches are one root-array document.
  `auto` uses an estimate, sampling streamed batches; it does not guarantee the
  fewest tokens for a particular model. Use explicit JSON when parsing output.
- `session`, hook protocols, and `{"error":{...}}` errors stay JSON. Explicit TOON
  is rejected for JSON-only commands; config defaults fall back and `auto` uses
  JSON. `impact` findings themselves remain stderr text.
- Supported tree-sitter languages: Rust, C, C++, Python, JavaScript, TypeScript,
  TSX, Go, PHP, Java, Kotlin, Swift, C#, Bash, Ruby, Zig. Unsupported embedded
  formats need separate inspection. `doctor` can check installed capabilities.
- Cross-file impact scans exclude vendor/build trees by default; generated files
  otherwise selected remain callers. Extra `--exclude-dir` / `--exclude-glob`
  filters on `context`, `impact`, or `review` narrow evidence. On review they also
  affect dead candidates; `--framework` affects dead candidates only. Report
  exclusions that matter to the conclusion.
- For large repositories, prefer targeted queries first. `review` has no `--glob`;
  `refs`, `symbols --dir`, and `dead-code` do. Narrowing `--dir` can lose callers
  outside that subtree; `--base` changes the diff, not the cross-file parse scope.
  Report that tradeoff. `ASTRO_SIGHT_PARSE_BUDGET_MIB` controls large-file parse
  concurrency (default 32; 0 serializes large files) without narrowing results.
- If an update appears ineffective, check `astro-sight --version` and executable
  resolution (`command -v astro-sight` in Bash; `Get-Command astro-sight` in
  PowerShell). For a mise shim, use `mise which astro-sight`. When installing from
  source, set `INSTALL_PATH` to the intended executable directory if necessary.
