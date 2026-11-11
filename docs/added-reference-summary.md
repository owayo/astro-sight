# Added export reference summaries / 追加 export の参照サマリ

`review` adds `api_changes.added_reference_summary` to normal JSON and TOON;
`review --hook` adds the same array at the root as `added_reference_summary`.
It covers added public names before the existing usage filter, including names
omitted from `api.add` because another changed file already uses them. A confirmed
move is excluded. Existing API buckets, `add_scope`, blocking categories and exit
codes retain their meaning. The hook can therefore emit informational JSON with
an empty `blocking_categories` even when it previously emitted nothing.

Each entry identifies `file`, `name`, `kind`, and optional Rust `inline_module`.
`in_diff` and `outside_diff` partition references by membership of the **file** in
the input diff; they do not partition changed versus unchanged lines inside that
file. Both the producer's own references and cross-file references are counted.
Definition occurrences are excluded; imports, re-exports, type references and
ordinary uses remain occurrences. The explicit scope is:

```json
{
  "new_side": "working_tree",
  "partition": "diff_files",
  "references": "non_definition_identifier_occurrences",
  "attribution": "name_match",
  "selection": "refs_default_workspace_scan"
}
```

This is the default workspace reference inventory (including its ignore, hidden
file and generated-file policies), matched by identifier name, or the bare name
of a qualified member. Same-name occurrences can belong to another declaration;
alias uses, dynamic access and external consumers are not proven by this count.
A zero count means zero observed occurrences in that scope, not an unused API.
This evidence is independent of the existing actual-use filter for `api.add`.

For the `cube` example with one import and one call in `reader.ts`, producer-only
input yields `in_diff.total: 0`, `outside_diff.total: 2` and retains `api.add`.
Including `reader.ts` yields `in_diff.total: 2`, `outside_diff.total: 0` and still
suppresses that entry from `api.add`. No consumer yields two exact zero counts
when the selected scan is complete.

Each partition has `observed`, nullable `total`, `complete`, `references`, and
`omitted`. Up to 20 locations are shown in path/line/column order. `observed`
counts all observed locations before that display limit; `omitted` gives the
number removed from the sample. Positions are zero based and paths use `/`.
The array of exported names is not capped. When the input inventory is incomplete,
`total` is **null** and `complete` is false: `observed` is only a lower bound.
`scan` retains `failed_files`, `parse_error_files`, `scan_failed`, `unknown_references`, full scan
`truncations`, and optional `generated_files_skipped` (with its own omitted-path
sample disclosure). Unsupported sources, failed reads/parses and skipped generated
sources cannot produce a definitive zero for the whole workspace.

With `--git --staged`, both changed and unchanged files come from the index
snapshot and `new_side` is `index`. Unstaged edits cannot add or remove this
evidence. When the bounded index snapshot cannot be prepared, this array is
omitted and the existing `staged_snapshot_unavailable` truncation explains the
missing scan; working-tree evidence is never relabeled as index evidence.

通常 JSON / TOON は `api_changes.added_reference_summary`、hook は直下の
`added_reference_summary` に同じ情報を載せる。差分内外は行でなくファイルの所属で
分け、定義以外の識別子出現 (import・型注釈も含む) を数える。同名の別定義や別名利用を
解決した件数ではなく、未使用の断定にも使わない。既存の `api.add` と blocking は変えない。
件数の `total` は走査が完全なときだけ確定し、未完了は null とする。位置は各区分20件まで
表示し、全観測数と省略数を併記する。staged は index だけを証拠にし、snapshot が作れない
ときはサマリを省いて既存の truncation に理由を残す。
