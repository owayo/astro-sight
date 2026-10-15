# Bash grammar patch

This directory vendors `tree-sitter-bash` 0.25.1 under its original MIT license.
The published crate is the baseline: SHA-256
`9e5ec769279cc91b561d3df0d8a5deb26b0ad40d183127f409494d6d8fc53062`.
Its VCS record names upstream commit
`a06c2e4415e9bc0346c6b86d401879ffb44058f7`.
The grammar, scanner, Rust bindings, highlighting query, license and metadata
originate from that crate. Packaging is reduced to the Rust build and grammar
regeneration; author names are retained and contact emails are omitted.
Standalone Rust tests use the CLI's tree-sitter 0.27 runtime (ABI 15).

Upstream: <https://github.com/tree-sitter/tree-sitter-bash>.
Local regression: <https://github.com/owayo/astro-sight/issues/59>.

## Change

In `echo "${label:+ (${label}) }"`, the scanner rejects concatenation at the
closing parenthesis after the nested expansion. The existing expansion-word
scanner can read that literal `)`, but the parser cannot reach it without a
concatenation separator.

Append a hidden `_concat_in_expansion` external token and allow it only in
`_concatenation_in_expansion`. The scanner emits this zero-width separator only
before `)`, and the following expansion word consumes the parenthesis. Disable
the token during the all-symbols-valid error-recovery probe. Keep the ordinary
concatenation token, scanner state and serialization unchanged. This confines
the new path to a parameter expansion's operand or replacement, preserving
ordinary command, subshell and array boundaries.

The patch changes no named node kinds or fields. Regression tests retain the
original complete ASTs for brace groups, subshells, command substitutions,
unquoted nested expansions, heredocs, nested defaults, arrays and literal
operands. They also check all eight default/alternate/error/assignment operators,
declarations before and after the expression, identifier positions, Unicode byte
columns, references and malformed-source diagnostics.
The published and regenerated `node-types.json` files are byte-identical.

## Regenerate

Use Node.js and tree-sitter CLI **0.26.6**, with grammar ABI **15**:

```sh
cd vendor/tree-sitter-bash
tree-sitter generate --abi 15
```

The original crate's parser was generated with CLI 0.25.10. Generated C parser,
headers and grammar metadata are checked in, so normal Cargo builds need no
Node.js or grammar generator. A generator upgrade may alter unrelated tables
and should be reviewed separately.
CLI 0.26.6 also regenerates `tree_sitter/array.h` with its strict-aliasing fix;
that header is identical for the unmodified and patched grammar regenerations.

Before removing this patch in favor of an upstream release, run the Bash
grammar regression tests, the CLI tests and the upstream corpus. This patch
does not evaluate shell code.
