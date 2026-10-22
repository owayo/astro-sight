# Kotlin grammar patch

This directory vendors `tree-sitter-kotlin` **0.3.5**, published crate SHA-256
`8df217a0e1fec649f3e13157de932439f3d37ea4e265038dd0873971ef56e726`.
Its VCS record names upstream commit
`51e451be6c18e2cc123249f49edf8e26fc50311c`.
Grammar, scanner, Rust bindings and query originate from that crate; the MIT
license is retained from that upstream commit. Packaging is limited to Rust
build inputs and grammar regeneration metadata.

Upstream: <https://github.com/fwcd/tree-sitter-kotlin>.
Local regression: <https://github.com/owayo/astro-sight/issues/102>.

## Change

A brace-free `when` body ending in `return offset + 1` absorbs the next
parenthesized condition as call arguments, then absorbs `else` as an infix
operator. The ordinary automatic-semicolon scanner deliberately refuses these
boundaries to preserve multiline calls and inner `if/else` expressions.

Append a hidden external token used only at a complete `when_entry` boundary.
On a newline before `(`, issue a zero-width separator only after finding a
top-level branch arrow, balancing delimiters and skipping comments, quoted
strings, characters and backtick identifiers. An `else` must be followed by
the branch arrow. Preserve `mark_end` at the original boundary; the normal
parser consumes and validates the complete following condition. Nested block
comments are supported, including comments adjacent to tokens. Stop at a new
top-level expression line so an ordinary multiline call cannot borrow the
arrow of a subsequent branch. Failed speculative probes return immediately,
so no other external token reads from a future position. Disable this token
during the all-symbols-valid recovery probe. Scanner state/serialization and
existing token indexes are unchanged.

The probe allows at most 128 nested delimiters and 65,536 main-loop
iterations (comment and quoted-token scans consume complete tokens).
Unknown string interpolation and a slash that cannot be proved
to start a comment retain the original grammar's behavior. This patch does
not claim to repair all existing Kotlin parsing errors.
Top-level casts/type checks and anonymous functions/objects are also left to
the original grammar because a function type can contain its own arrow.
Explicit type arguments are balanced
as angle brackets; an unmatched comparison `<` conservatively fails the probe.
The minus probe consumes `--` as a token so postfix decrement followed by a
comparison (`value-->0`) cannot provide a spurious branch arrow.
Raw string closing quote runs are consumed completely, matching the existing
scanner for three, four or five quotes.

ABI **14**, the legacy Rust binding and astro-sight's existing FFI bridge are
retained. No named node kinds or fields change. CLI 0.26.6 adds root/extra
metadata and the unnamed `?` entry compared with the published generated
node metadata; regeneration of the unmodified grammar with the same CLI
produces a byte-identical `node-types.json` (SHA-256
`9f34960613341d7ee3d60eedf20599445caf987d262a1d69beee99984cc7154c`).

## Regenerate and validate

Use Node.js and tree-sitter CLI **0.26.6**, grammar ABI **14**:

```sh
cd vendor/tree-sitter-kotlin
tree-sitter generate --abi 14
```

Generated parser C, headers and metadata are checked in; normal Cargo builds
need no generator or Node.js. Keep the `=0.3.5` dependency pin and local path
patch together. A Kotlin-ng migration requires a separate query/extraction
compatibility review.

Regression tests in `grammar_regressions_kotlin.rs` verify entry/return text
and byte ranges, LF/CRLF, comments and quoted delimiters/arrows, nested
expressions, ordinary multiline calls/infix/if-else continuations and malformed
diagnostics. CLI regression compares symbols, references, calls and imports
against the equivalent braced form and checks the AST structure.

```sh
cargo test --locked --lib kotlin
cargo test --locked --test integration kotlin
```
