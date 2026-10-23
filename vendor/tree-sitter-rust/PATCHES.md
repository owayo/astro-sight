# Rust grammar patch

This directory vendors `tree-sitter-rust` 0.24.2 under its original MIT license.
The published crate is the baseline: SHA-256
`439e577dbe07423ec2582ac62c7531120dbfccfa6e5f92406f93dd271a120e45`.
Its VCS record names upstream commit
`e2bee853694a1d3e0f6ef308fe3674542fec95d7`.
The grammar, scanner, Rust bindings, queries, license and generator metadata
originate from that crate. Packaging is reduced to the Rust build and grammar
regeneration; author names are retained and contact emails are omitted.
Standalone Rust tests use the CLI's tree-sitter 0.27 runtime (ABI 15).

Upstream: <https://github.com/tree-sitter/tree-sitter-rust>.
Local regression: <https://github.com/owayo/astro-sight/issues/135>.

## Change

Accept underscores after each hex digit of a braced Unicode escape in both
`char_literal` and `escape_sequence`: `/u\{([0-9a-fA-F]_*)+\}/`.
For example, `"\u{4f_dd}\u{5b__58_}"` must not corrupt the syntax tree or hide
following declarations and calls. Rust permits repeated and trailing underscores
in these escapes: <https://doc.rust-lang.org/reference/tokens.html#character-literals>.

This patch preserves the upstream rule's unbounded digit count, its legacy
unbraced escapes and its handling of prefixes. It does not add Unicode scalar
validation or byte-literal restrictions. No source preprocessing is performed:
all ranges and byte columns refer to the original input.
The scanner, queries, Rust bindings, node kinds and fields remain unchanged.

Parser regressions exercise ordinary strings, character literals, C strings,
repeated and trailing underscores, case and zero-padded six-digit scalar values,
raw strings, escaped backslashes and malformed braced escapes. They also check
declarations and call byte ranges after the string, with real UTF-8 characters
before the call and with LF and CRLF. CLI regressions cover AST diagnostics,
symbols, references and calls at their original positions.

## Regenerate

Use Node.js and tree-sitter CLI **0.26.6**, with grammar ABI **15**:

```sh
cd vendor/tree-sitter-rust
tree-sitter generate --abi 15
```

Generated C parser, headers and grammar metadata are checked in, so normal Cargo
builds need no Node.js or grammar generator. Compare regenerations of the
unmodified and patched grammar with the same CLI before upgrading the generator.
The published and regenerated `node-types.json` and all three parser headers
are byte-identical. An unmodified regeneration changes only the generated C
language metadata's patch version from 1 to 2, matching `tree-sitter.json`'s
0.24.2; the patched regeneration also includes this metadata correction.
The grammar patch itself changes only the two escape regular expressions in
`grammar.json` and their lexer states in `parser.c`.

Before removing this patch in favor of an upstream release, run the Rust parser
and CLI regressions and the upstream corpus.
