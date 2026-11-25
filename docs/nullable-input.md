# TypeScript input union widening

`review` can classify an input annotation that adds only `null` as
`compatible_modified` with reason `nullable_input_widening`. The first supported
cases are named top-level function declarations with explicit unchanged return
annotations, unchanged bodies and parameter shapes, and primitive or literal
input types, including properties written directly in an inline object type.
Function types and unresolved aliases require review.

For a top-level parameter, adding other union members gives the reason
`parameter_union_widening`. The new union must retain every old member.
Parentheses and member order may change. For example, `string | undefined`
accepts the old `string` input, and `B | C | A` retains both members of
`A | B`. Named members can refer to unchanged, non-generic local type aliases
or simple interfaces containing data properties.

The extended check verifies uses of the widened parameter in the declaration.
A function with an inferred return can have an empty body or directly return a
literal or an untagged template string. A returned template can interpolate a
scalar parameter directly or with a literal fallback, as in `${prefix ?? ""}`.
Returning the widened parameter itself, passing it to another function, or
dereferencing it without a guard keeps the change under review.
For the extended check, an `@ts-expect-error` comment in a source file that
mentions the function also requires review. An old call that now accepts its
argument can leave that directive unused.

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
and the changed `member` (for example, `"null"`, `"undefined"`, or `"C"`).
`usage: "direct_calls_proven"` denotes the closed direct-call
proof; `unproven` preserves the need for review. `scope: "repository"` states its
boundary. Empty evidence arrays are omitted. TOON retains this information.

Input narrowing, return-type changes and function-valued props retain blocking
behavior. Staged review uses the index snapshot, and an unavailable
snapshot omits this evidence instead of substituting worktree facts.
