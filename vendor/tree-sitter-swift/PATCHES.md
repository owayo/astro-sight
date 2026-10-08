# Swift grammar patch

This directory vendors `tree-sitter-swift` 0.7.3 under its original MIT license.
The published crate is the baseline: SHA-256
`fe36052155b9dd69ca82b3b8f1b4ccfb2d867125ac1a4db1dd7331829242668c`.
Its VCS record names upstream commit
`b8b22bffbb3441780e6471665bacfb263741c86a` with `dirty = true`, so the crate
must not be assumed to equal that commit. The missing `grammar.js` and
`tree-sitter.json` come from that commit; the grammar rules match the crate's
`src/grammar.json`. Runtime sources, Rust bindings, queries and license originate
from the crate. Packaging is reduced to the Rust build and grammar regeneration;
contact email metadata is omitted. Redundant blank lines at EOF are trimmed
in the scanner and three query files; query contents otherwise match the crate. Standalone Rust tests use the same tree-sitter
0.27 runtime as the CLI, matching the generated ABI 15 parser.

Upstream: <https://github.com/alex-pinkus/tree-sitter-swift>.
Local regression: <https://github.com/owayo/astro-sight/issues/32>.

## Change

An optional type suffix needs an adjacent `??`, while nil coalescing can follow
whitespace, comments or a newline. The original scanner used the same external
token for both and could consume a multiline nil coalescing operator as a type
suffix after `as? String`.

Append a dedicated `_immediate_double_quest` external token and use it only in
`optional_type`. Scan it before skipping whitespace or comments. Keep the
existing nil coalescing token and scanner serialization unchanged. Disable this
new token during the all-symbols-valid error-recovery probe. Generated node-type
entries retain the upstream schema.

## Regenerate

Use Node.js and tree-sitter CLI **0.26.6**, with grammar ABI **15**:

```sh
cd vendor/tree-sitter-swift
tree-sitter generate --abi 15
```

The generated C parser and headers are checked in. Normal Cargo builds need no
Node.js or grammar generator. A generator upgrade may change unrelated tables;
keep it separate from this patch.

Before removing this vendor patch in favor of an upstream release, run the Swift
grammar regression tests (including double optional closure parameters), the
CLI grammar tests and the upstream corpus. The original crate, its unmodified
regeneration and the patched parser produced identical full ASTs for all 238
upstream corpus cases. The regression tests additionally cover the reported
multiline cast/coalescing expression and source ranges.

## Existing limitation

The adjacent-comment expression `value as? String/**/?? "fallback"` retains an
upstream AST ambiguity (`optional_type` plus `infix_expression`) in all three
parsers. This patch fixes the reported multiline expression; it does not resolve
that existing comment-trivia case. Spaced comments and newline cases are covered
by the regression tests.
