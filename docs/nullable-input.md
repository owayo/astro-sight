# TypeScript nullable inputs

`review` can classify an input annotation that adds only `null` as
`compatible_modified` with reason `nullable_input_widening`. The first supported
cases are named top-level function declarations with explicit unchanged return
annotations, unchanged bodies and parameter shapes, and primitive or literal
input types, including properties written directly in an inline object type.
Function types and unresolved aliases are outside this proof.

The proof scans repository consumers using the existing bounded source,
configuration, dependency and ignored-file audits. Every use must be an ordinary
direct call, without a spread or type arguments. Function values, writes,
re-exports, type extraction (`Parameters`, `ComponentProps`, etc.) and unresolved
imports retain blocking behavior. Public package names and package entry metadata
(`exports`, `main`, `types`, etc.) also prevent this repository-local proof. A
workspace without package publication metadata is analyzed within the repository
scope; this is not a claim about arbitrary external consumers.

Normal JSON exposes `api_changes.type_change_evidence`; hook JSON exposes
`api.type_change_evidence` with the same shape. Each entry identifies a file and
symbol, and lists changes with `position` (`input` or `return`), zero-based
`parameter` for inputs, `property_path`, `direction` (`widening` or `narrowing`),
and `member: "null"`. `usage: "direct_calls_proven"` denotes the closed direct-call
proof; `unproven` preserves the need for review. `scope: "repository"` states its
boundary. Empty evidence arrays are omitted. TOON retains this information.

Evidence alone does not alter blocking. Input narrowing, either return direction,
function-valued props and string-union member additions are not covered by this
compatibility rule. Staged review uses the index snapshot, and an unavailable
snapshot omits this evidence instead of substituting worktree facts.
