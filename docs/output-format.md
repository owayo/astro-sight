# Output Format

`--format json|toon|auto` selects the output format. The default is `json`.

| | JSON | TOON | auto |
|---|---|---|---|
| Default | ✅ | | |
| Specification | RFC 8259 | [TOON v3](https://github.com/toon-format/toon-rust) | Whichever is estimated to use fewer tokens |
| `--pretty` | Applies | Ignored (TOON is indented by design) | Applies only when JSON is chosen |
| Cache | Compact output only | Not used | Not used |

## Compact JSON Keys

JSON output is compact by default (`--pretty` indents it). In compact mode, key names are shortened to save tokens:

- `language` → `lang` (calls, imports, lint, sequence, compact ast/symbols)
- `location` → `path` (compact ast/symbols)
- `references` → `refs`, `line` → `ln`, `column` → `col`, `context` → `ctx` (refs)
- `source` → `src` (imports)
- `kind`: `"definition"` → `"def"`, `"reference"` → `"ref"` (refs)
- `SymbolKind`: `"function"` → `"fn"`, `"interface"` → `"iface"`, `"variable"` → `"var"`, and so on (compact symbols)
- `calls`: grouped by caller, with each callee reduced to `{name, ln, col}`

Compact output (ast/symbols):
```json
{"path":"src/main.rs","lang":"rust","schema":{"range":"[startLine,startCol,endLine,endCol]"},"ast":[...]}
{"path":"src/main.rs","lang":"rust","symbols":[{"name":"main","kind":"fn","ln":20}]}
```

`--full` on `ast` / `symbols` prints the complete form with unshortened keys (`location`, `language`, `hash`, `range`, and so on). `--pretty` only indents and keeps the keys shortened (only `calls` switches to the complete form with `--pretty`). Only `doctor` and the MCP `initialize` response include a `version` field.

## What TOON Is

[TOON](https://toonformat.dev/) (Token-Oriented Object Notation) expresses the JSON data model with indentation and tables. It hands the same content to an LLM in fewer tokens.

```bash
astro-sight symbols --path src/main.rs --format toon
```

```toon
path: src/main.rs
lang: rust
symbols[3]{name,kind,ln,cx}:
  MAX,const,0,null
  alpha,fn,1,2
  beta,fn,4,1
```

The same content in JSON looks like this. TOON saves the key names that JSON repeats for every element.

```json
{"path":"src/main.rs","lang":"rust","symbols":[{"name":"MAX","kind":"const","ln":0},{"name":"alpha","kind":"fn","ln":1,"cx":2},{"name":"beta","kind":"fn","ln":4,"cx":1}]}
```

The conversion uses [`toon-format` 0.5.0](https://github.com/toon-format/toon-rust), with `default-features = false` to avoid its CLI/TUI dependencies, and encodes with the default settings (comma delimiter, 2-space indentation). The supported specification is **TOON v3**. An empty array is `[0]:`, and a named empty array is `items[0]:`.

Single and batch output are verified with the library's strict decoder. No newline is added at the end of the document (JSON / NDJSON end with a newline). How much TOON saves depends on the content; use `--format auto` to choose the format automatically.

## auto: Choose the Format with Fewer Tokens

`--format auto` **encodes each output as both compact JSON and TOON and picks the one with the smaller estimated token count**. A tie goes to JSON (the default format, and the one consumers handle best).

```bash
astro-sight symbols --path src/main.rs --format auto
```

### Why Not Plain Character Counts

In BPE tokenizers, **newlines and indentation cost about 1 token per line**, so comparing plain character counts overrates TOON, which has more lines. In the following case, the winner flips between character count and actual token count (the same with both `o200k_base` and `cl100k_base`):

| | Characters | Tokens |
|---|---:|---:|
| `{"a":1,"b":2,"c":3,"d":4}` | 25 | **17** |
| `a: 1` `b: 2` `c: 3` `d: 4` (4 lines) | **19** | 19 |

The comparison therefore uses `characters + 4 × newlines`. When moving to TOON v3 (2026-09-24), **63 pairs** of single output, batch output, and limited refs output were measured again with tiktoken, and the existing factor of 4 was kept.

| Tokenizer | Factor 3: total loss / max loss | Factor 4: total loss / max loss |
|---|---:|---:|
| `o200k_base` | 6 / 6 tokens | 0 / 0 tokens |
| `cl100k_base` | 12 / 9 tokens | 3 / 3 tokens |

The loss is the difference from whichever of JSON and TOON actually uses fewer tokens. These figures come from the sample above and are not an upper bound for arbitrary output; the factor is measured again whenever the output shape changes. Leaving a real tokenizer out of the binary avoids the extra data size and the differences between models' tokenizers, and keeps the choice for a given input fixed.

`--token-budget` ([output limits](usage.md#output-limits-and-result_summary)) **does not use this metric as is**. The metric only compares formats with each other, so the absolute value of the factor does not matter. A budget, on the other hand, is an absolute number given by the user ("up to N tokens"). Unless the two scales match, a budget of 3,000 would print only about 900 tokens. The measured `metric / actual tokens` ratio (252 samples × 2 tokenizers) is p05=3.00 / p50=3.42 / min=2.73, so the budget check divides the metric by 3 (rounded toward staying within the budget).

### Properties

- **The estimated token count never exceeds either candidate** (auto only picks the smaller one, so it cannot be worse than either). When the [output limits](usage.md#output-limits-and-result_summary) apply, though, the **number of results** that fits in the budget differs by format. In a measurement, `refs --name new` gave 64 results / 2,591 tokens in JSON and 83 results / 2,548 tokens in TOON, and auto chose TOON, returning more information for the same budget
- The choice depends only on the input, so it is **deterministic**: the same input and the same version always give the same format
- Both formats do get chosen. With the earlier factor of 3, 1,127 samples of astro-sight output chose **JSON in 564 and TOON in 563** (`ast` tends to favor JSON; `symbols` / `refs` / `calls` tend to favor TOON)
- `--pretty` only decides how a chosen JSON is rendered. The comparison itself always uses compact JSON and TOON, so `--pretty` has no effect when TOON wins
- An empty object (`{}`) is an empty document in TOON, that is, no output at all, so auto chooses JSON. Otherwise the user could not tell "the result is empty" from "nothing was printed" (an explicit `--format toon` prints the empty document as the specification says)
- On the outputs listed in "Outputs That Stay JSON" below, `auto` falls back to JSON without an error. It is not an unsatisfiable request to print TOON, and choosing JSON is a valid result of auto

**Approximation in batch mode**: `--paths` / `--paths-file` / `--dir` do not buffer every result, so the winner cannot be decided after seeing all records. **The first 32 records in output order are rendered in both formats as a sample, and the measured winner is applied to the rest**. The sample size is a constant that does not depend on parallelism (the number of CPUs / `ASTRO_SIGHT_BATCH_WORKERS`), so the same input gets the same format at any parallelism. Only the sample is encoded twice; the analysis itself is a single pass on every path, and formats never get mixed within one output.

## Outputs That Stay JSON

The following 3 outputs are contracts whose consumers expect JSON, so `--format json|toon` does not apply to them (`auto` just chooses JSON and does not fail).

| Output | Reason |
|---|---|
| `session` | An NDJSON protocol of "1 line = 1 request / 1 response" |
| `review --hook` | A JSON contract consumed by the Claude Code Stop hook (compact JSON on stderr) |
| Error output `{"error":{...}}` | A machine-readable contract parsed by existing scripts |

`impact` has no structured output and only prints text to stderr, with or without `--hook`, so `--format` does not apply to it either.

An explicit `--format toon` on the command line is an error for these outputs (an unsatisfiable request). `format = "toon"` in `config.toml` is only the default display format for all commands, so these outputs silently fall back to JSON (setting it should not break a hook or a session).

## Batch Output Shape

`--paths` / `--paths-file` / `--dir` print NDJSON (1 record per line) in JSON, and **a single document with one root array** in TOON.

```toon
[2]:
  - path: a.rs
    lang: rust
    symbols[3]{name,kind,ln}:
      MAX,const,1
      alpha,fn,2
      beta,fn,5
  - path: b.rs
    lang: rust
    symbols[1]{name,kind,ln}:
      gamma,fn,1
```

The outer array uses the list form (`- ` items), not the tabular form. The tabular form needs information that is known only after every element has been seen, which conflicts with the design requirement of not buffering all results (keeping peak RSS independent of the number of inputs). The element count `[N]` is known in advance from the number of input paths, so the header alone can be written first. The inner arrays use the tabular form, and they account for most of the savings.

In batch mode, arrays that would become tabular if all elements were converted at once are still printed in the list form. Encoding of each element is left to the library, and tests check that the strict decoder restores the same values. A failed analysis also counts as an element, so the count in the header matches the number of elements printed. `refs --names` already holds every result, so it is converted in one go.

## Normalizing Nullable Columns

astro-sight's compact JSON omits a field such as `cx` (cyclomatic complexity) entirely in elements that have no value for it. TOON's tabular form, however, requires **every element of an array to have the same set of keys**. Encoded naively, output such as symbols falls back to the list form and becomes **more verbose than JSON** (measured +33%).

So when the objects in an array differ only in that some keys are missing, astro-sight fills the missing keys with `null` to make the tabular form possible. It does so **only when the result is shorter than the strict encoding** (a tie goes to the strict one), so filling never makes the TOON longer.

- Only arrays whose elements are all non-empty objects with primitive values are filled. Columns that contain objects are not
- The column order is fixed to the order in which keys first appear while scanning the elements (deterministic)
- **A structural round trip with the JSON form is not guaranteed.** When decoded, keys that JSON omitted show up as `null`. The meaning as a DTO (`None` of `Option<T>`) is preserved

This normalization is a decision astro-sight makes about its own DTOs; the `--format toon` encoder itself stays a pure implementation of the specification. Filling missing keys with null in arbitrary JSON would make an explicit null indistinguishable from an unset key.
