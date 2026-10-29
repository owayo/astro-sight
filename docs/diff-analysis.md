# Diff Analysis

The commands here take a unified diff (with `--git`, the `git diff` of the working tree) and examine what a change affects. `context` returns the impact, `impact` stops with exit 1 while impacts are unresolved, and `review` returns the impact, co-changes, public API changes, and dead symbols in one run. `dead-code` and `cochange` also work without a diff.

Rust declarations touched only by LF/CRLF conversion are excluded as impact origins when the old and new ASTs prove their complete text, ownership, and attributes unchanged. Only physical CRLF is normalized; changes to literal spaces, blank lines, or `\r` / `\n` escapes remain visible. Duplicate names, changed paths, reconstruction/parse failures, large inputs, and deep ownership retain the conservative result. This applies to working-tree, staged, and external diffs, including the impact portion of review.

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

For Rust, `affected_symbols[].kind` is `impl` when an impl header changes, an impl without methods is added, or deleted lines may include a method declaration. Adding a method without deleting lines reports the method without reporting its owning type as changed. Edits with deleted lines keep the `impl` record because a removed method cannot be ruled out from the new tree alone. An attribute-only change also keeps the `impl` record because the method symbol does not include attributes. The `symbols` command still includes the impl owner for method ownership.

Rust signature attribution checks the declared function name in the AST. A parameter named `text` in `apply(text: &str)` does not make that line the signature of a separate `text` method. A new method in a mixed addition/deletion hunk is reported as `added` when it is a unique callable directly inside a file-root inherent impl, all its lines are added, and the old AST proves that the name did not belong to a callable, associated item, or import. Existing same-name items, trait or nested impls, generating macros or attributes, uncertain Unicode identity, parse failures, reconstruction failures, and possible removals in other files keep the classification conservative. This does not change the existing proofs for body-only edits, compatibility, or private methods.

Callers fall into 3 groups by confidence and by how breaking they are. "Blocking" here means a finding that makes `impact` or `review --hook` return exit 1.

- `impacted_callers`: actual call sites. Those left outside the diff are blocking for `impact`. References to symbols whose number of arguments changed, and references that cannot be decided, also stay here
- `low_confidence_callers`: generic names whose owner (the class or type a method belongs to) cannot be determined, and same-name references in TS/Rust without evidence of a direct import
- `informational_callers`: function-value references to symbols whose name and number of arguments did not change, and import lines of modified symbols whose name did not change. Not blocking

### JS / TS references independent of the signature

For top-level JS / TS / TSX functions, `modified_closed_in_diff` (`api.mod_closed`) excludes same-name occurrences in another file when their use does not depend on the function signature. Both revisions must contain a unique direct top-level function declaration without visible rebinding. Changes from values or other symbol kinds do not use this proof. The supported sites are noncomputed `property_signature` names with directly written noncallable predefined types such as `boolean`, `string`, and `number`, literal types, or unions of those types; and parameters used only for truthiness. Type aliases, callable properties, method signatures, and mock object keys remain unproven.

Parameter proofs additionally require file-wide strict mode, established by a static import/export statement or an exact `"use strict"` directive in the file directive prologue. Dynamic import, function-local directives, and class-only strictness are not used as evidence. Sloppy or unknown strictness remains unproven because reflective argument access can occur outside the function. Property-declaration proofs do not require strict mode.

Parameters must be plain identifiers or direct object-pattern shorthands. Every same-name use in the function, including nested closures, is checked. Only ternary / `if` / `while` / `do` / `for` conditions and prefix `!` operands are accepted, through bounded parentheses. A call, value escape, return, assignment, same-name declaration, or type use keeps the entire binding unproven. Defaults, rest parameters, renamed destructuring, and constructor parameter properties are excluded. Functions referring to `arguments` remain unproven because parameters can be accessed indirectly.

Parse errors, clues for `with` or `eval`, ambient declarations, and declaration files prevent this proof. Unicode escapes in any identifier node kind also prevent it; escaped names are not guessed. API closure still requires at least one actual target call verified against the changed argument count and changed lines. Alias imports use the separate proof below. Single and batch `refs` occurrences and dead-code counts are unchanged.

`context` and `impact` also route these proven sites in other files to `low_confidence_callers`. Both revisions must contain a unique direct function declaration, and the modified declaration itself must be a top-level function with a signature change. Mixed same-name affected methods or other declarations prevent this proof. Removed or added declarations, symbol-kind changes, and import sites do not qualify. Disabling the confidence filter disables this route. Direct `impact --staged` and review fallback without a staged snapshot remain conservative. When `review --staged` uses a complete index snapshot, only references inside that snapshot provide evidence.

### TypeScript Named Import Aliases

For a unique ordinary top-level TS/TSX function, a direct relative `import { total as sum } from './pricing'` can contribute calls to `api.mod_closed`. The import must resolve to the changed declaration's file with no competing candidate. Every use of the local alias must resolve to that import and be a direct call. Inner parameters and local declarations that shadow the alias are independent. Both the original-name callers and alias callers must satisfy the changed function's argument count and the existing changed-line checks, including multiline calls. A complete index snapshot supplies the evidence for `review --staged`.

Re-exports, type-only imports, function-value escapes, `typeof`, binding the value to another name, unsupported calls, and uncertain resolution remain blocking. An additional JS/TS/TSX inventory checks escaped identifiers and module-object transports outside the name index. Namespace/default imports, export stars, and direct `require`/dynamic imports are accepted only when they resolve to a different relative file; an unrelated but unresolved transport also prevents closure. `rootDirs`, `moduleSuffixes`, `extends`, configuration escapes, unreadable inputs, parse errors, links, and dynamic-code clues prevent the proof. Names are simple ASCII identifiers. Type-member obligations retain their existing policy.

The inventory is bounded to 10,000 sources, 2 MiB per source, 64 MiB total, 100,000 AST nodes, and 100,000 walk entries. Alias-reference scanning has separate limits of 64 aliases and 100,000 nodes. Configuration checks allow 1 MiB per file, 10,000 directory entries, and 128 ancestor levels. Exceeding a limit keeps the API blocking. File selection follows the existing ignore/generated-source policy; external users and arbitrary runtime reflection are outside this proof. `refs`, dead-code counts, and ordinary occurrence output are unchanged.

### TypeScript Function-Value Handoffs

When an exported function's explicit return type and a receiving interface's callback return type change together, unchanged `execute({ clone: cloneText })` handoffs can close both API changes as `mod_closed`. The producer, interface, and consumer must be unique directly exported root declarations. The interface must contain one required function-type property, and the consumer must receive that interface as its only required parameter. Both old and new callback assignments are checked. Other signature tokens, including the consumer's explicit return contract, must stay equal.

The proof handles `void`, primitive types, and mutable/readonly arrays. It limits the consumer body to discarded callback calls, primitive literal returns, and returning an array callback result's `.length` as `number`. Direct producer calls, an unchanged receiving interface, callback mutation/extraction/escape, inferred consumer returns, and unrecognized uses remain unproven. It checks every observed producer/interface/consumer reference and module transport in both source versions before closing either API change. Standalone `impact --git` uses the same diff-scoped evidence; ordinary references and `context` output remain intact.

The inventory is limited to 512 TS/TSX files, 8 MiB across both versions, 100,000 AST nodes, depth 128, and 2,048 candidate comparisons. Aliased/default/namespace imports, reexports, dynamic loaders, JS or embedded sources, JSDoc, parse/read failures, links, and uncertain configuration (`extends`, `rootDirs`, `moduleSuffixes`, `paths`, or `baseUrl`) prevent this proof. This verifies the observed internal handoffs and bounded consumer dependencies, without establishing general API compatibility or replacing TypeScript checking.

### TypeScript Required Property Additions

Adding a required property to an exported TS / TSX interface or object type alias can appear in `modified_closed_in_diff` (`api.mod_closed`) when every site that produces a value of the type, and every function that receives one, is followed in the same diff. Each reference to the type must be one of these:

- The exact return type of a function or method. Every `return` must produce a proven value (defined below). Async functions are limited to the next form; generators are excluded
- A top-level ordinary function declaration returning `Row[]`, `Array<Row>`, or `readonly Row[]`, or an async declaration returning `Promise<Row[]>`, `Promise<Array<Row>>`, `Promise<readonly Row[]>`, or `Promise<Row>`. Every array return must call `.map` on a required parameter with an array type, or on an array literal, using an inline arrow with one unannotated parameter that returns only proven values. Expression and block bodies are supported; neither the callback nor the outer function may fall through. `Promise<Row>` must directly return a proven value. Receiver aliases, writes, and escapes; callback parameter writes; same-named methods on arbitrary objects; `filter`, `flatMap`, spreads, assertions, and `await` remain unproven. Declarations or imports of `Array`, `ReadonlyArray`, or `Promise`, prototype references, and aliases of these built-ins within the analysis scope also prevent the proof. Prototype changes made by external code are not resolved
- `const x: Row = <value>` or `<value> satisfies Row`, where the value is proven
- A required parameter without a default value in a top-level function declaration (`function label(row: Row)`). The body must not reassign the parameter, bind its value to another name or store it in an object or array, or read its type with `typeof`. Reading its members (`row.id`) and passing it to a call are fine. A separate name search must find at least one direct call of the function, and every reference to it must be an import, an unaliased export, or a direct call that passes a proven value at that position
- An unaliased `export { Row }`. Occurrences in the value namespace (a same-name variable, JSX component, or property name) do not refer to the type and are ignored

A proven value is one of: an object literal that contains each added property exactly once, on a changed line, with a value whose type fits the added member (a key with an escape, such as `n\u006fte`, names a different property than written, so any such key prevents the proof); a call to a top-level function declaration whose return type is exactly the type (a call through a namespace, `rows.toRow()`, does not count); or an identifier bound to a required parameter or variable annotated exactly with the type. Value types are checked against `string`, `number`, `boolean`, `bigint`, `null`, `undefined`, literal types (bigint literals included), unions and arrays of these, and `any` / `unknown`. For property values, an unannotated variable is typed only when it is a `const` with a primitive literal, because object and array literals widen. Inside a property value, `satisfies` and `as const` (except on a primitive literal) change how the literal is typed, so such values are not proven; `undefined` is not trusted when the file rebinds that name.

For a required addition, a local `const x: Row = <value>` inside a function or block is audited only through uses that resolve to that lexical declaration. Same-name variables and parameters in other files or scopes are excluded. A `typeof` use, alias, container storage, or duplicate declaration of the same binding prevents the proof. Passing a different inner binding to a consumer requires a separate proof of that value. Narrowing and removal retain their existing audits.

Required additions also accept `const picked = makeRow(); label(picked)` in TS/TSX when a simple const declaration precedes the reference and its initializer directly calls a unique top-level function with an explicit return annotation exactly matching the target type. Unaliased relative imports use the existing resolution checks. Uses of the variable are audited against its lexical binding; `typeof`, aliases, container storage, duplicate declarations, and writes prevent the proof. This covers local variables and private top-level variables in modules. Exporting/importing the variable, JS, script globals, `let`, destructuring, alias chains, assertions, generic/overloaded functions, explicit type arguments, spread arguments, namespace calls, async functions, and nullable return types remain unproven. Parentheses around the call are accepted.

Required additions also support copies such as `{ ...snapshot, title }`. Every spread source must be an identifier proven to have the target type (including the direct-call const results described above), or a direct call to a unique ordinary top-level function explicitly returning that type. Copied added properties need no changed-line evidence. A later explicit override still requires the existing value-type and changed-line proof; the last proven spread can overwrite an earlier explicit value. Unknown spreads remain unproven regardless of their position. Sources involving `any`, `Partial`, assertions, generic/overloaded calls, explicit type arguments, spread arguments, arrays, or Promises are excluded, as are copies with computed/escaped keys, duplicate explicit added keys, or methods.

The exception to the spread-source alias audit applies only when the copy is returned by an ordinary top-level function explicitly annotated with the target type, initializes `const x: Row`, or occurs under `satisfies Row`. The destination's type binding, value, callers, and producer-name `typeof`/alias audits must still pass through the existing type-reference obligations. Unannotated copy variables and containers do not qualify. Property narrowing and removal do not use this copy proof.

A reference in another file counts only when it is an unaliased named import whose relative specifier resolves to exactly one existing file, the declaration file. A `tsconfig*.json` or `jsconfig*.json` mentioning `rootDirs` or `moduleSuffixes` between the importing file and the workspace root prevents the proof. The names that bind produced values (functions, variables, variables initialized with `satisfies`, and the objects or arrays that hold such a value) are also searched. Any of the following prevents the proof:

- A `typeof` use, which copies the type without naming it, as in `ReturnType<typeof toRow>`
- An aliased import or re-export of the name
- Binding the value to another name or storing it in an object or array (`const make = toRow`, `{ toRow }`), also through conditional expressions, `&&` / `||` / `??`, and comma expressions
- Taking the value out of a container and binding it to another name (`const it = box.item`, `const [first] = list`), including destructuring with a key not written as an identifier (`const { "item": picked } = box`, `{ [key]: picked }`, `{ 0: picked }`)
- A value taken out of an unnamed container (`[<value> satisfies Row][0]`), because where it goes cannot be followed by name
- One of these names, or the name of a receiving function, written as a string in a value index, a destructuring key, or a type index (`rows["toRow"]`, `(typeof rows)["toRow"]`), which takes the value out of a namespace object without naming it. This includes strings wrapped in `as const`, parentheses, conditional expressions, computed keys (`{ ["toRow"]: make }`), and type unions (`["toRow" | "makeRow"]`), and it also stops on an unrelated dictionary index (`labels["toRow"]`)
- A mention in an embedded source such as `.vue`
- A mention of the type or one of these names in a JSDoc comment (`/** ... */`) in a JS-family file. JSDoc is type checked under `checkJs` but is a comment, so it is not a reference; the type is read even without `{...}`. Ordinary comments are not read as types and are ignored

A producer exported as default or under another name, or one that is a class member, also prevents the proof, because its users cannot be found by name; a class member's type is copied through the class type (`Repo["find"]`, `implements Repo`). Taking a value out with a string held in a variable or passed to a function (`rows[key]`, `get(rows, "toRow")`), and a type carried by inference when the value is passed as a callback (`ids.map(toRow)`), are not tracked.

For this kind of change, a reference line inside the changed lines is not accepted as evidence. Required additions outside the supported form stay blocking even when every reference line changed: generic declarations, `extends`, index signatures, required methods, call / construct signatures, member types outside the list above, string-named properties, member names that collide (including an optional member added at the same time) or cannot be compared as written (numeric names such as `1` and `0x1`, escapes), and self-referential types. References in other positions (`Row[]` and `Promise<Row>` outside the returns above, property types of other types, `implements`, `as Row`, type arguments, callbacks, methods, arrow functions, optional parameters) also keep the change blocking. Before this proof, such a change was closed when every reference line changed.

The proof checks references to the type name and to the names of the functions, variables, and containers that produce values, within the syntax listed above. Outside the limited const proof above, types carried by inference through call results, arguments, or returns are not tracked, for example assigning `toRow()` to a variable annotated with a different type whose `note` property conflicts, or `typeof r` after `const r = toRow("a")`.

### TypeScript Existing Property Type Narrowing

Removing atoms from a required property's scalar union in an exported top-level interface or object type alias can also produce `api.mod_closed` under a limited proof: for example, `string | null` to `string`, `string | undefined` to `string`, or `"ready" | "done"` to `"ready"`. The new atom set must be a proper subset of the old set, compared as written. All properties of the type must have simple scalar types; names, declaration headers, and other members must remain unchanged. Generics, inheritance, index signatures, function-valued or unknown members, ambiguous declarations, optional-property changes, simultaneous member additions, and quoted or numeric property changes remain unproven. Unsupported existing-member changes and object types whose uniqueness or parsing cannot be verified do not fall back to evidence that every reference line changed.

Producers are limited to top-level ordinary functions returning exactly `Row` and `const x: Row`. Every produced value must fit the new type, with the affected property on a changed line. Fallthrough, object literals with methods or getters, and values depending on `any`, `unknown`, type assertions, or non-null assertions remain unproven. Even an explicitly annotated const has its initializer checked for assignability to the annotation and traced through preceding consts, up to 16 links; cycles, forward references, and mutable variables are rejected. Typed parameters use their annotations when their body does not reassign them; the origins of primitive arguments passed by callers are not traced. Array and Promise producers, `satisfies`, arrows, and class members are outside this narrowing proof.

Receiving functions need explicit return annotations. Uses of annotated variables and parameters, and direct producer-call results, are checked for writes to narrowed properties: assignment, compound assignment, updates, delete, destructuring targets, and for-loop targets. **Comparisons with literals disjoint from the new type are retained as blocking by policy**; this does not reproduce TypeScript's comparison diagnostics. A whole value may be passed only to a receiving function whose body and calls are checked by the same proof. Unknown calls, aliases of a value or narrowed property, string-key access, unproven member calls, and inferred return types remain unproven. Existing guards for `typeof`, aliased imports and exports, JSDoc, and similar hidden uses still apply.

This is not whole-program type checking or runtime write resolution. Inference without a type or producer name, changes through external code or reflection, and values arriving through other types are not traced. `refs` and dead-code occurrence contracts are unchanged.

### TypeScript Property Removals

Removing only property signatures from an exported root interface or object type alias can produce `api.mod_closed` when every observed producer and consumer is proven to have followed the change. Declaration headers and remaining members must stay unchanged; all old and new members must be uniquely named identifier properties. Scalar-only changes retain the existing value-type checks. Required and optional properties can be removed; removed members themselves must have scalar types. Generics, inheritance, index signatures, methods, function-valued or unsupported members, self references, ambiguous or unparseable declarations, and mixed additions or type changes stay blocking, including when every reference line was edited.

Producers are limited to ordinary root functions returning exactly `Row` and local `const x: Row` values. Remaining required properties must be present with compatible values; unlike additions and narrowing, these fields may be on unchanged lines. Values depending on `any`, `unknown`, assertions, unsafe initializers, spread, computed keys, methods, getters, or fallthrough remain unproven. The existing bounded const-initializer checks apply. Consumers require explicit return annotations and verified direct calls. The audit also follows annotated Row values and direct producer-call results: dynamic or string-key access such as `const key = "extra"; row[key]`, destructuring, aliases, unknown calls, and unverified value escapes remain blocking. `satisfies`, arrow producers, class members, imported Row constants, and inferred returns remain outside this removal proof. Addition and narrowing proofs retain their existing contracts.

Scalar-only removal also supports unique ordinary root functions with explicit `Row[]`, `Array<Row>`, or `readonly Row[]` returns, and async `Promise<Row[]>` returns. Every return must directly map an array through an expression-bodied arrow returning an object literal. Existing array/builtin audits apply, together with value-type checks and removed-key checks. Callback parameter values are proven only when the receiver is an annotated required array parameter with no writes or escapes.

Observed array uses are limited to `toRows(...).map(label)` and `(await toRows(...)).map(label)`. Proven producer declarations and wrapper shapes are registered, and the consumer reference and producer result share one verifier. The consumer must be an ordinary function with one required Row parameter and an explicit primitive/void result; generics, overloads, async functions, and generators remain unproven. Aliases, `typeof`, array escapes, thisArg, spread, optional chains, explicit type arguments, block callbacks, forwarding arrows, `.then`, and indirect await forms are rejected. This does not extend non-scalar invariant proofs, addition, or narrowing.

When retained members include scalar arrays, `readonly T[]`, `Array<T>`, `ReadonlyArray<T>`, or `Record<string, scalar>`, a separate proof requires unchanged values. Only built-in names used by retained generic annotations are audited. Declarations, imports, aliases, and writes prevent the proof; ordinary value reads such as `Array.isArray` are allowed. `T[]` and `readonly T[]` alone need no built-in-name audit. Producers are unique ordinary root functions with returned object literals and root `const x: Row` literals. After excluding the target type declaration and removed-key entries, the complete file token sequence must match the base revision. Parameter declarations, bindings, and all retained values therefore remain unchanged, including present optional fields. Values are limited to literals, array/object literals, and verified producer parameters; calls, outside variables, named types, and assertions remain unproven. Parameter types are limited to scalar types, `T[]`, and `readonly T[]`; generic parameter annotations remain unproven. New producers, inline argument literals, local constants, map callbacks, and `satisfies` are outside this proof. Limits of 2 MiB per file, 100,000 nodes, and eight value-nesting levels, unmatched producers, or other edits in the same file keep the change blocking. Scalar-only removal, addition, and narrowing retain their existing policies.

A separate scan requires removed names to be absent from property/member names, object and destructuring keys, string access keys, and string literal types, including `Pick<Row, "extra">` and `Row["extra"]`. Matching keys on unrelated objects or types also keep the change blocking. Ordinary string values and comments do not count as keys, except JS JSDoc that may supply types. Files containing escapes are parsed even when the raw removed name is absent; escaped identifier names or keys remain unproven. Unparsed embedded sources containing a removed name or an escape, read failures, relevant parse failures, `keyof Row`, `typeof`, aliases and hidden imports/exports retain the conservative result. Staged review uses the index snapshot. This bounded check does not trace general inference, external code, or reflection; `refs` and dead-code occurrences are unchanged.

### TypeScript String-Union Member Removal

Removing members from a directly exported root type alias made only of string literals can produce `api.mod_closed` when the observed value flows are proven. The declaration header and the order and spelling of retained members must stay unchanged. Duplicate or escaped declaration literals, generics, alias expansion, and mixed additions remain unproven. Once this removal shape is recognized, unsupported uses stay blocking even when every reference line was edited. Addition-only/reorder-only changes and old non-string aliases keep their existing classification. When old literal members disappear into a general type such as `string`, this proof also remains conservative. Only same-path, unchanged-name candidates participate; renames retain existing handling. This additional proof does not apply when the old alias itself cannot be recognized, including old-input limits or an alias wrapped in ERROR.

Allowed type occurrences are root `const X: Record<Mode, primitive>` objects, root `const X: Mode[]` / `readonly Mode[]` arrays, and required Mode parameters of ordinary root functions with explicit primitive/void results. Primitive types are limited to the `string`, `number`, `boolean`, and `bigint` keywords, plus `void` for results. Other parameters must also be annotated required primitives; optional/default/rest/destructured parameters and generics remain unsupported. Record keys must exactly match the retained set, and array elements must be retained string literals. Cross-file names require a direct, unaliased relative named import with one verified target. Independent same-name types are separated by lexical binding; ambiguous ownership remains unproven. At least one supported slot and observed value use are required; unreferenced or type-only uses do not close the change.

Arrays may flow through inline single-parameter expression arrows in a chain of `.filter(...)` calls ending in `.map(...)`. Filters use known boolean consumers or retained-literal comparisons; map results use those verified comparisons, known primitive consumers, or Record index reads. Consumer Mode arguments and Record indices accept only retained string literals, verified consumer parameters, or registered array callback parameters. Mode values may appear only in these calls/indices, retained-literal comparisons, or verified switch discriminants. Aliases, escapes, writes, typeof/keyof, re-exports, unknown arguments, assertions, optional calls, spread, type arguments, and thisArg remain blocking.

A supplementary residue scan checks exact cooked values in strings, static templates, literal types, and property keys. An unrelated occurrence of a removed value also blocks the proof; substrings and unrelated interpolated templates do not. Common JS and Unicode escapes are decoded. Numeric-like removed values also disable the raw-name filter; numeric or computed static keys then remain unproven instead of approximating JavaScript number-to-string conversion. Relevant JSDoc, embedded sources, and JSON are audited under the reference scan's ignore/hidden/generated policy. Limits are 100,000 walk entries, 10,000 selected files, 2 MiB per file, 64 MiB total, 512 parsed files, and 100,000 AST nodes. For recognized removal candidates, read, parse, walk, source-link, resolution, builtin, or limit failures retain the conservative result. Parse and node budgets are shared across one API-change detection run.

Relative-import ownership audits the caller's logical and physical ancestors up to the filesystem root. Resolution settings in tsconfig/jsconfig are read regardless of ignore rules; unknown settings, project references, and read failures remain unproven. Directory-import candidates with package.json also remain unproven. Sources tried before enumerated candidates are also audited for existence and metadata failures, including implementations before explicit declaration specifiers and declarations or directory entries appended to JS suffixes by legacy resolution modes. Config entries, bytes, and depth have shared limits. A non-ancestor config selected on the command line and external bundler rules such as platform suffixes are outside this proof.

Known loader forms such as `createRequire`, member `require`, `require.context`, and `import.meta.glob` also remain unproven. JSX HTML entities are not cooked by the supplementary scan, and JSX target uses remain unsupported. Sources excluded as generated, arbitrary loader aliases, and dynamically synthesized names are outside the proof.

The guarantee covers only these observed static flows. External consumers, runtime data, and reflection are not proven. Record constants keep `const_value` and strict-const behavior; standalone impact, refs, and dead-code occurrence contracts are unchanged.

### Rust Parameter Patterns

Renaming or destructuring a typed ordinary parameter does not change how Rust callers pass arguments. For example, changing `fn double(_n: i32)` to `fn double(n: i32)`, or renaming a tuple pattern with the same tuple type, does not produce `api.mod` or a function signature impact when both declarations and their import environments can be verified. The body may change at the same time. Displayed signatures keep their parameter names.

The proof preserves parameter types, count and order, return types, receivers, generics, bounds, lifetimes, qualifiers, ABI, visibility, enclosing declaration headers and allowed attribute values. Attributes outside the small built-in allowlist, `cfg_attr`, `unsafe(...)` attribute wrappers, parameter attributes, macro invocations in a header, parse errors and ambiguous declarations keep the existing conservative result. Attributes on out-of-line module declarations and external macro expansion are not resolved. Type names are compared as written; this proof does not resolve simultaneous changes to local type definitions or transitive aliases.

This comparison applies to function items. Containers keep their existing checks: a one-line trait can still report `api.mod` for the trait itself, and an impl owner can still have an impact. References and dead-code occurrence counts are unchanged.

### Python Relative Import Bindings

An unaliased, unconditional module-level `from .cli import main` can bind `main()` to a different function than an unrelated changed `legacy.main`. Impact analysis excludes that use only when the relative module resolves uniquely inside the workspace and contains a single, undecorated, direct definition of the imported function. References to the actual defining module, `.pyi` changes, and import-name occurrences keep their existing handling. `refs` and dead-code reference counts are unchanged.

The proof requires package initializers containing only comments, plain string literals, or `pass`. Rebinding, duplicate imports, star imports, dynamic namespace operations (including `__dict__` and frame namespace access), parse failures, and module `__getattr__` prevent it. Absolute imports, aliases, namespace packages, class bodies, type positions, lambdas, and comprehensions keep the conservative result. Arbitrary runtime replacement from another file, custom import hooks, and string-based monkey patching are outside this static proof. Package and target evidence is reused within one analysis and discarded before the next query; imported names unrelated to the changed symbols are skipped.

### Python Optional Parameter Removal

Removing optional keyword-only parameters or trailing defaulted positional parameters from a unique, undecorated top-level Python function can produce `api.mod_closed`. Remaining parameters, their modes, annotations and defaults, and the rest of the header must be unchanged. All observed uses in the current source must be direct calls accepted by the new signature, the declaration itself, or an unaliased absolute import resolved to that declaration; at least one call is required. Calls need not have changed in the diff. The hook does not block on their impacts, while ordinary impact and reference output stays available.

Aliases, module transport, re-exports, function values, ambiguous bindings, dynamic namespace access, non-ASCII identifiers, variadic or positional-only signatures, parse failures and incomplete inventories remain unproven. The inventory follows the reference scan's ignore and hidden-file scope; other source languages, links or junctions, read/walk failures, and limits of 512 source files, 2 MiB per file, 8 MiB total, 100,000 AST nodes or walk entries, and depth 128 prevent the proof. Staged review uses the index snapshot. This is a repository-local call proof; external consumers and arbitrary runtime reflection are outside its scope.

### Python Local Function Definitions and Attribute Writes

Impact analysis can also identify a unique, undecorated function declared directly in the reference file as the owner of a bare use after its declaration. Rebinding, duplicate or conditional definitions, decorators, and the namespace barriers above keep the reference unproven.

For a modified function proven to be a normal module function on both sides, the attribute name on the left of a plain assignment is excluded from callers. Attribute calls, function values, augmented assignments, references on the right, and changes to methods or properties retain their existing handling. These two proofs are disabled when `context --staged` or `impact --staged` reads callers from the working tree while using index sources. `review --staged` can apply them within its index snapshot. The `refs` occurrence list and dead-code counts are unchanged.

## impact: Detect Unresolved Impacts (for Stop Hooks)

From the result of `context`, `impact` flags impacts on files outside the diff as "unresolved". It is meant for the Stop hook of an AI agent: when affected code is left untouched, it blocks and prompts the agent to continue. `impact` only follows symbols that still exist in the changed tree, so deleted symbols are not detected (exit 0 even if calls remain). To stop on deletions too, use `review --dir . --git --hook`, which detects removed public APIs as `api.rm`.

Signature changes are matched on identifier boundaries as in `context`, so a change to a test helper or a derived name does not spread as if the base function had changed. Local symbols declared inside a function are not used as starting points of cross-file impacts. TypeScript/JavaScript, Rust, Python, Go, Java, and Kotlin are supported, and nested Kotlin functions are told apart from top-level functions with the same name.

For a unique root TypeScript/TSX function with one inline object parameter, type changes confined to properties that remain optional on both sides can make individual JSX callers informational. The caller must use a direct identifier tag and omit every changed prop. Supplying a prop, even as `undefined`, JSX spreads, type arguments, mismatched tags, and implicit children for a changed `children` prop do not qualify. Any bytes between paired tags, including spaces, tabs and line breaks, count as children. Existing low-confidence results retain priority.

The source proof requires unchanged headers, patterns and explicit return types, with no whole-parameter or pattern defaults. Prop types and explicit return types are limited to primitives, literals and static inline objects; named types and other unknown forms remain unproven. Without a return annotation, every outer return must directly contain JSX or a string, number, boolean or null literal after removing parentheses. A final top-level return must prevent fallthrough, and the old/new sets of return categories must match. Nested functions and classes are skipped. File-level JSX pragma comments prevent this inferred-return proof. Required changes, overloads and top-level prop additions/removals do not qualify. Evidence is specific to each source and function name. Other body behavior is not verified, and the global `api.mod` is retained; `refs` and dead-code occurrences are unchanged.

For Rust, a private declaration directly in a source file can be used by child modules in other files. Impact analysis includes those callers. For a leaf module with no external children, it excludes same-name references in other files when it can rule out source inclusion. For modules with children, it excludes references outside the declaration's subtree only when the old and new visibility, ordinary module paths, and source layout can be verified. Unclear module layouts or source-generating macros keep the candidate. Private methods inside `impl` blocks use a separate rule. When a unique private inherent method has the same owner before and after the change, method references in descendant modules are included and proven sibling modules are excluded. Bare function calls and imports are excluded. A same-name definition in the reference file, a different written owner, a receiver with no owner evidence in the file, or an unproven module subtree lowers the reference to low confidence. Unproven references with `--staged` are also low confidence.

When the only edit in a Rust source file adds `pub(crate)` or `pub` to a direct private item, unchanged references inside its former private subtree are informational. References outside that subtree are excluded because they could not have named the old item. This requires an unchanged ordinary Cargo module layout and no glob imports anywhere in the analyzed project: widening visibility can otherwise introduce an ambiguous name through a glob. Other edits in the same diff, uncertain ownership, and narrower or more complex visibility changes retain the normal blocking classification.

For Rust type and free-function references, impact analysis can follow named imports, `pub use` chains, and glob imports through ordinary Cargo modules. It excludes a same-name reference only when the resolved declaration belongs to a different file and the bindings used for that proof are unchanged across the diff. An alias import remains blocking under the original name when it is used, publicly re-exported, or may be used by a child module. An alias proven to come from a standard crate or registry dependency remains informational. A registry dependency counts only when the Cargo configuration Cargo reads for the package (`.cargo/config(.toml)` in the package's ancestors and `$CARGO_HOME/config(.toml)`, which applies wherever the project lives) and the environment are readable and cannot redirect dependency sources (`[patch]`, `[source]`, `paths`), replace the compiler (`rustc`, wrappers, or overrides that `[env]` passes to rustc), or add `--extern` flags. Settings passed with `cargo --config`, and configuration that Cargo reads only when run from a directory below the package root, are not visible. Other external imports, conditional bindings, ambiguous globs, changed bindings, unusual source layouts, and staged analysis retain the conservative result. This ownership proof does not change `refs` or dead-code output.

Cargo dependency reachability can also exclude same-name references in a different package when that package cannot reach the changed package. This scan accepts unqualified standard expression macros and built-in derives and attributes. It checks macro definitions and imports across the workspace, including declarations, dependencies and library names that can rebind `std`, `core` or `alloc`; possible shadowing of a used standard name prevents the proof. Dependency checks cover both `dev-dependencies` / `build-dependencies` and the legacy `dev_dependencies` / `build_dependencies` spellings, including target-specific tables. Macro arguments are token trees, so an argument containing `use`, `extern`, a macro definition or an attribute remains unproven. Globs are limited to standard-crate paths and paths containing only `crate`, `self` and `super`. A test module's `use super::*` is accepted; `use crate::facade::*` is not, because `facade` may refer to an external crate through multiple aliases.

Unknown macros, custom derives, attribute macros and qualified macro calls (including `std::println!`) remain unproven. The scan does not resolve an unknown macro's defining crate or its dependencies. Existing barriers for `include!`, `path` attributes, custom build layouts, read or parse failures, LFS pointers and other uncertain source configurations remain in place. Removed Rust free-function classification shares this reachability scan with impact analysis; `refs` and dead-code occurrences are unchanged.

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
- `--base` is omitted and there is no commit yet (HEAD is unborn) → exit 0 (no output; see the same section)

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

The check uses `git rev-parse --is-inside-work-tree` (with `LC_ALL=C`), so worktrees, submodules, and bare repositories are detected correctly. **Real errors** (an invalid `--base`, a base that cannot resolve to a commit, git not runnable, a broken repository, missing permissions) return `exit 1`.

A repository without any commit is skipped in the same way when `--base` is omitted. Right after `git init` or `git checkout --orphan`, HEAD does not point to a commit yet, so there is no base to compare against. `skipped.reason` is `unborn_head`. An explicit `--base` is never skipped: the diff against that base is analyzed, and a base that does not resolve to a commit, even `--base HEAD`, still returns exit 1. When `cochange` gets `--paths` / `--paths-file`, it continues with those files only, as it does outside git. Whether HEAD is unborn is checked with `git show-ref --exists` (git 2.43 or later); older git does not skip and keeps exit 1. Passing a diff through `--diff` / `--diff-file` / stdin does not go through this check.

`review --git` makes at most three attempts when the index or working tree changes during analysis, or when `git diff` reports a short read while indexing a file. Each attempt starts by acquiring the diff again and resolving the base to a commit. That commit stays fixed for the attempt's diff, cochange, and API analysis; moving HEAD alone does not invalidate it. Failed results and temporary snapshots are discarded.

If all three attempts fail for these reasons, both normal review and `--hook` return `REVIEW_INPUT_CHANGED` with **exit 2**. A single JSON error is printed to stdout; `--hook` also prints a short instruction to rerun review on stderr. Other errors and blocking hook findings keep exit 1. Explicit `--diff`, `--diff-file`, and stdin inputs are not retried.

With `--staged`, the index contents are written to a temporary directory, and impact, API, and dead-code analysis all read those contents. Unstaged edits in the working tree do not enter the result (for large repositories, see "Size limit of the staged snapshot").

### Limit on Untracked Files

`--git` (without `--staged`) includes untracked source files in the analysis as new files (so that references to untracked files created in the same piece of work are not reported as unresolved impacts outside the diff). **Untracked files larger than 256KB or 5,000 lines are left out**, though. Pulling in generated output such as code generator results or huge fixtures would make every exported symbol in them a candidate for the API diff, `review` would take tens of minutes, and the Stop hook would time out. In a measurement, a `review` that finished in 1.75 seconds without untracked files did not finish within 10 minutes after adding an untracked file with 22,000 `pub fn` in total.

Tracked files are not limited. A file that has been committed or added can be seen as intentionally put up for review. An untracked file, on the other hand, has not been added yet; whether it will be committed is unknown, and a huge one is likely generated.

Excluded files are not dropped silently; they are listed in `truncations` (so that they are not mistaken for reviewed files):

```json
{ "...": "...", "truncations": [{ "path": "generated.rs", "reason": "untracked_file_too_large", "message": "untracked file excluded from --git analysis: lines 80000 exceeds limit 5000" }] }
```

With `--hook`, this is printed as `trunc: [{"f": "generated.rs", "r": "untracked_file_too_large"}]` (it reports the analyzed scope rather than a finding, so it does not cause exit 1). `impact`, which has no structured JSON, prints it as a `note:` line on stderr. `--staged` / `--diff` / `--diff-file` respect the scope given explicitly and do not pull in untracked files at all.

### Size limit of the staged snapshot

`review --git --staged` writes every regular file in the index to a temporary directory before analyzing it. When the repository has more than 50,000 files or 512 MiB in total, it does not write them and reads the working tree instead (only the changed files are read from the index). This is not an error; the switch is reported in `truncations`:

```json
{ "...": "...", "truncations": [{ "reason": "staged_snapshot_unavailable", "message": "staged snapshot was not created: 61234 files / 734003200 bytes exceed the limit (50000 files / 536870912 bytes); the working tree matched the index, so files were read from the working tree" }] }
```

On this path, references found in the working tree do not match the content being committed when it has unstaged edits. So when the working tree differs from the index, the checks that move a change to a non-blocking category because no references remain (`mod_closed` / `mod_compat` / `rm_dead`) are not used, and the change stays blocking. With `--hook`, this is printed as `trunc: [{"r": "staged_snapshot_unavailable"}]` (it reports the analyzed scope, so it does not cause exit 1 by itself).

Staged diffs containing Swift files require the snapshot for protocol witness detection. If it exceeds the limit, `review` and Swift `dead-code --git --staged` return an error, so unstaged conformances cannot remove a staged dead candidate.

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

`review --hook` keeps `api.mod` and `api.const_value` visible but treats them as nonblocking when the same review's `test_only_symbols` contains the same file, name, and symbol kind with a known declaration line. This also applies under `--strict-public-const-values`; normal API classifications are unchanged. Any reported coverage gap in the review or impact analysis prevents this downgrade. Production uses and re-exports, unreferenced exports, and same-name symbols in other files do not establish test-only use; a mixed list still blocks on its production changes. Removal, callback, and type-annotation policies are unchanged. This is a policy based on observed repository references, without guaranteeing the absence of external consumers.

On top of the impact analysis of `context`, `review` returns candidates for missed changes from `cochange`, the public API diff, and dead symbols in a single run. It is meant for PR reviews and pre-merge checks.

### TypeScript namespace name collisions

When a separate namespace exports the same name as a directly exported root function, interface, type alias, or simple variable, TS/TSX API comparison can recover the root declaration's changed signature from its AST position. Both versions must use the same TS/TSX grammar and prove a unique root binding. Changed duplicate names remain in the reference index even when the new root signature equals an old namespace signature. A namespace-only edit does not become a root API change.

Recovered candidates use the existing compatibility, argument-count, and type-member checks. A required argument left missing stays blocking even if its call line was reformatted. Existing multiple-definition guards can also keep a recovered variable or type change blocking after its uses are edited. Overloads, declaration merging, rebinding, destructured variables, indirect/default exports, and parse failures are outside this recovery. Other languages and the `refs` / dead-code occurrence contracts are unchanged.

### JavaScript and TypeScript literal constant values

For an otherwise unchanged exported `const` declaration without a type annotation, changes between regular-expression literals use `api_changes.const_value_changes` (`api.const_value` in hook output). Pattern and flag changes retain the same `RegExp` kind. Templates without substitutions, including empty templates and escapes, share the existing string-literal kind, so string/template replacements also use this value policy. This applies to JS, TS and TSX, including staged review.

The default hook treats these value changes as informational; `--strict-public-const-values` makes them blocking. This policy does not establish equivalent runtime behavior: flags such as `g` and `y` can change how a regular expression is used. Changes between different kinds, templates with substitutions, tagged templates, assertions and `satisfies` retain their previous handling. Declaration-header checks and the separate scalar-leaf rules for static object/array constants are unchanged.

### Rust internal-module constant values

For an ordinary change at the same path, a Rust `pub const` or non-`mut` `pub static` whose name, type, visibility, and binding kind remain unchanged while only its initializer changes uses the existing `api_changes.const_value_changes` bucket (`api.const_value` in hook output). This value-policy information is retained before external-API suppression, including Cargo bin-only crates and private modules in libraries. The default hook does not stop on unchanged references solely because of this classification. `--strict-public-const-values` makes the same classification blocking.

This is a value-change policy, not a compatibility or external-reachability proof. Public-surface decisions for `added`, `removed`, and `modified` remain unchanged. Type or visibility changes, `static mut`, and ambiguous pairings do not use this correction. Moves and renames retain their existing checks. Normal output and staged hooks use the same classification; `refs` occurrence contracts are unchanged.

### Rust free-function removals and qualified names

Removing an unused module free function can produce `removed_dead` (`api.rm_dead`) when surviving occurrences of its name provably refer to another item. The proof handles directly declared types such as `Engine::run()`, module paths such as `crate::engine::run()`, and bare calls to another function declared in the same module. A remaining `crate::launcher::run()` or an unresolved `use` of the removed function stays in blocking `removed` (`api.rm`). Rust compares identifiers after NFC normalization; non-ASCII import or block-item bindings remain unproven when equivalent spellings cannot be ruled out. `refs` and dead-code occurrence lists are unchanged.

Cross-file evidence uses the Cargo manifests discovered under the analysis root and their workspace/member/path-dependency closure. It enumerates explicit targets, including roots without a Rust extension, build scripts, and standard library/bin/test/example/bench roots. Each explicit `mod` declaration contributes a module occurrence with its own parent and search directory. A file can belong to several roots; every known membership must prove the same destination, and an unknown membership prevents the proof. Conditional or ambiguous module edges are all inspected and are never selected arbitrarily.

External paths such as `beta::items::run()` and `::beta::items::run()` can also prove a surviving direct function in an ordinary, non-optional local path dependency. Dependency renames, library names, workspace inheritance, and explicit library roots are checked against the same Cargo input closure. Paths use the canonical package identities recorded during input auditing, without folding case-sensitive directory names. Non-ASCII type-namespace declarations prevent dependency fallback when NFC-equivalent local names cannot be ruled out; existing file-local proofs and explicit `::` paths remain available. Every membership must agree; build-script memberships, conditional dependency-root cfg attributes, competing dependency targets, re-exports, unproven shadowing or generated extern aliases, unsupported dependency fields, and compiler/source overrides keep the removal blocking. Registry/git dependencies are not resolved by this proof. Ambiguous move matching is unchanged.

Reachable source inclusion (`#[path]`, `include!`), unsafe declarative-macro templates, symlinks at or below the analysis root, read/parse failures, and resource limits prevent cross-file evidence. Unexpanded module-scope item macros cause their module search directories to be audited recursively, including ignored sources. Ancestor directory aliases are canonicalized for the layout graph, while both logical and physical ancestors are audited for Cargo inputs. Cargo configuration is read for the analysis root as the execution directory, its ancestors, and the effective `CARGO_HOME`; source replacements and unsupported config includes prevent the proof. Independent ignored packages outside the input closure, independent crates outside the analysis root, configuration used only from another execution directory, and placement changes generated by unexpanded procedural macros or macro definitions outside the analyzed inputs remain outside this guarantee. Evidence that is entirely local to a file does not depend on this layout proof.

### Python member removals with unresolved receivers

`api_changes.removed_unresolved` (`api.rm_unresolved` in hook output) reports confirmed removals whose remaining member reads have unresolved receivers and a restricted type demand. These entries have `confidence: "low"` and do not block the hook. They do not establish that the removed member has no callers; `removed_dead` keeps its existing proof requirements. Shell parse recovery continues to use the separate `uncertain_removals` / `rm_unverified` category.

This classification requires an undecorated ordinary method without old member rebinding or attribute-write clues, its class still present at the old path, complete static reference input, and at least one surviving field declaration owned by another class. Every occurrence must have an existing unrelated-field proof or an unresolved receiver at a supported use site. Field declarations explain the name collision; they do not prove the receiver's type. Unrelated field reads retain the existing non-escape checks for the removed owner class, including uses through inheritance or aliases. Ambiguous moves are excluded.

Supported demands are unary `+`, `-`, `~`; arithmetic or bit operations with an integer or float literal on the other side (excluding `%`); ordering against such a literal; and one-argument, unshadowed `int`, `float`, `abs`, `round`, `range`, or `len`. `sum` accepts a single generator or list comprehension, with no start or a numeric literal start. Value flow is limited to parentheses, conditional-expression branches, and comprehension bodies with one `for` clause. Every enclosing generator must pass directly to an unshadowed `sum`, allowing only bounded parentheses in between; escaped generators remain blocking. List comprehensions are evaluated eagerly.

Receivers are limited to unannotated plain parameters and those comprehension targets. After bounded parenthesis normalization, an unresolved comprehension iterable must be an unannotated, unreassigned parameter or an attribute chain without calls rooted in an unreassigned parameter, without a default value. A typed root must resolve to an ordinary class whose first field is assigned once from an unannotated, unreassigned `__init__` parameter. Explicit field types, typed constructor inputs, decorated members, and unresolved root types remain blocking. Typed iterables, local aliases, and explicit constructor/factory results remain blocking unless the existing field proof already resolves the read.

Calls, known class/instance receivers, `self` / `cls`, escaped bound methods, properties and static/class methods remain blocking. So do truthiness, equality, string formatting, unknown consumers, `any`, `sorted`, `max`, and collections returned without a supported consumer. An enclosing `try` or `with`, name shadowing, evidence of access to `builtins`, parse failures, and incomplete input also prevent this classification. Single and batch `refs` output is unchanged.

The inference assumes that evaluating these operations did not intentionally rely on a method value raising `TypeError` in the old program. `sum` additionally assumes ordinary numeric accumulation: clues for `__add__` or `__radd__` in the old candidate file or current analysis scope prevent the `sum_element` classification. External custom numeric types remain unknown. Runtime validity is not checked. Exception handling across function boundaries, external callers, and runtime changes to builtins or classes outside the observed static input remain unknown. The result therefore reports the demand and its location for review, rather than claiming safety:

```json
{"name":"Board.slot","kind":"method","file":"shop/board.py","refs_internal":0,"reason":"unresolved_receiver_type_demand","confidence":"low","references":[{"file":"shop/report.py","line":3,"column":20,"demand":"sum_element"}],"field_owners":[{"file":"shop/board.py","class":"Item","line":4,"column":4}]}
```

Reference and field-declaration positions are zero-based. Hook entries use `n`, `f`, `r`, `refs: [{f,l,c,d}]`, and `field_owners: [{f,n,l,c}]`, while retaining `confidence: "low"`. If a staged snapshot cannot be created and the working tree differs from the index, these entries return to blocking `removed` along with other reference-based downgrades.

With `--git --base <rev>`, the blame analysis of `missing_cochanges` uses the same base. When a PR with several commits is reviewed as a whole, the diff and the co-change candidates cover the same range.

`missing_cochanges` only proposes pairs that changed together 3 or more times (`--cochange-min-samples`, default 3). With blame on the changed lines, starting points with only 2 evidence commits are common, and pairs that "changed together only once" would line up at the top with confidence 1.0. To look at small samples exploratively, pass `--cochange-min-samples 2` (the standalone `cochange` command keeps the default of 2). Deduplicating the candidates and picking the top 10 use the same smoothed `score` as the standalone command. This keeps a small sample such as 3/3 from mechanically ranking above a sufficient sample such as 30/40, and the raw confidence is used for showing the evidence and for the threshold.

Lock files and the dependency declaration files that correspond to a source (`Cargo.toml` / `package.json` / `pyproject.toml`, and so on) are not proposed in `missing_cochanges`. In a commit that adds a dependency, these files always change together with the source, so the historical correlation is 100%. That correlation holds only "when a dependency is added", though, and has no causal link to changes of the code that neither add nor remove an import. A dependency declaration file is left out only for the pairing with the nearest one in the same ecosystem as the source (a pair from different ecosystems, such as `Cargo.toml` and a Python script, stays a candidate). The standalone `cochange` command keeps showing dependency declaration files as the fact that they "changed together in the past" (lock files are generated, so both leave them out).

**The relation between an external snapshot and the test that generates it is treated as directional.** A diff that only updates a snapshot does not get "the generating test may be missing a change". A snapshot is also updated when the output of the code under test changes, so "if the test changes, the snapshot changes too" is a fair expectation, but the converse is not. The other direction stays: when a test changes and its snapshot is missing from the diff, the snapshot is proposed. Only pairs that meet all of the following are suppressed; if any of them cannot be confirmed, the pair stays a candidate:

- The snapshot's parent directory is exactly `__snapshots__`, and its path with the trailing `.snap` removed once matches the missing candidate exactly (the standard convention shared by Jest / Vitest / Bun: `tests/__snapshots__/widget.test.tsx.snap` → `tests/widget.test.tsx`)
- The generating test exists as a regular file
- **The first line of the snapshot exactly matches a known runner header** (`// Vitest Snapshot v1, …`, and so on). The path convention alone would also catch handwritten fixtures and `.snap` files for other uses, so the file itself has to show that it is generated output

This is independent of `linguist-generated` in `.gitattributes` (that declares generated files in general, and removes the pair from the candidates in both directions). Custom snapshot resolvers, inline snapshots, and formats other than `.snap` are not covered, and for all of them the historical correlation is shown as information. The global `--include-generated` also turns this direction off. The standalone `cochange` command is for exploration and is not directional.

This does not prove that no test change is needed (you can update only the expected values and forget a needed change in the test logic). It is a recommendation policy: for pairs in the standard generation relation, a change in the reverse direction is not requested on the basis of historical correlation alone.

`api_changes.compatible_modified` lists changes where the signature string changes but existing calls stay compatible. The following changes are informational and not blocking for `--hook`:

- Wrapping a React component in a HOC
- Rewriting a Rust root function header using a unique, non-generic unrestricted `pub type`, or expanding that alias back into its type (`transparent_rust_type_alias`). Old and new aliases and import environments are checked in their respective files.
- Removing an object member that nothing references
- Adding optional / default parameters at the end of a top-level TS/TSX function (`trailing_optional_params`)
- Adding optional properties to an inline TypeScript object parameter type, including the unique object branch of an otherwise unchanged union (`optional_object_props`)
- Adding keyword-only parameters with defaults or positional parameters with defaults at the end of a top-level Python function or of a method of a class directly under the module (`trailing_optional_params`). When decorators differ or there are several definitions with the same name, the change stays blocking to be safe
- Adding `const` to a Rust function or inherent method (`rust_const_fn_added`). Calls from outside a const context behave the same, so the callers stay in `impact_info`. Only the `const` keyword may differ: the rest of the declaration, its attributes, the enclosing module or `impl` header, the file's imports, and the declarations in the same file of the names the declaration uses (such as a type alias, a `use` alias, or an array length constant, followed through the names they use) must stay the same. When the function returns `impl Trait`, its body and the same-file declarations the body uses must stay the same too, because they decide the hidden type and the auto traits it leaks. Macro token trees retain their original text, and impl headers (including empty impls), nested `impl` blocks, and macro invocations are compared with their enclosing declaration and attributes. Names referenced by those opaque declarations also enter the dependency check. A unique referenced function may also gain `const` while keeping its body tokens and the rest of its normalized header contract unchanged. The enclosing function's own `const` token is omitted, so an unchanged `assert!` in the function does not prevent the proof. Location-sensitive or unproven macro invocations must keep their line and Unicode column; aliases and nested macro tokens stay unproven. Only simple assertions and unique preceding root macro definitions without nested macros, attributes, or calls may omit this position check, and uncertain same-file macro bindings prevent that exception. Header macros remain unsupported. Removing `const`, or changing parameters, the return type, generics, `unsafe`, `async`, `extern`, or the visibility at the same time, stays blocking. When the file has another declaration with the same name (a method of another `impl`, a function split by `cfg`), the callers cannot be told apart and the `impacts` stay

Moving a Rust root free function or constant while making a compatible declaration change can report both `moved` and `const_value_changes` (initializer change) or `compatible_modified` (transparent type alias or a const addition to a free function). Const additions require the full declaration-dependency proof from #113 plus cross-file type-name resolution. Opaque `impl Trait` returns and const additions in rename diffs remain unproven. Names, kinds, declarations and destinations must be unique. This additional proof follows ordinary Cargo modules with an explicit edition and direct non-optional path dependencies, including renamed dependency keys. Every reference must use a proven direct destination path or a bare name in the destination file. Old-path references, import/re-export aliases, ambiguous ownership, narrowed visibility and incompatible changes retain the existing removal classification. The initializer's value is not evaluated; `const_value` means its declared type is unchanged.

The move proof checks primitive names for shadowing and normalizes raw identifiers in type-binding indexes. Glob imports and unknown attributes that could hide primitive names also prevent the proof. It accepts imports rooted at std / core / alloc as type evidence only when that root is unbound in both files. Dependency-name checks normalize raw identifiers and decline non-ASCII identifiers in the reference file because NFC-equivalent bindings are not resolved. It checks old and new source inventories once and declines unknown macros or attributes, derives, source inclusion, symlinks, build scripts, Cargo configuration, custom targets and incomplete reference input. Limits are 10,000 entries per inventory and 512 Rust files / 8 MiB across both sides. Generated code and compilation contexts outside the analysis root are not resolved. Existing exact-signature moves, moved-member changes and the occurrence contracts of `refs` and dead-code are unchanged.

The `impacts` tied to the same symbol are not reported as breaking either; they only appear as `mod_compat` information. For `rust_const_fn_added`, the `impacts` follow the impact analysis instead, which keeps them when another declaration in the file has the same name. To detect unreferenced object members, the removed keys are pre-extracted all at once with Aho-Corasick instead of searching the whole repository once per key, and each JS/TS file is parsed at most once. When collecting, reading, or parsing a file fails, the change is not downgraded to compatible and stays blocking.

For `optional_object_props`, object destructuring can also append bindings for optional properties added to the type in the same change. Existing elements must remain an exact token prefix, including aliases and defaults. New elements may be shorthand bindings or shorthand bindings with literal defaults: strings, numbers, booleans, null, templates without substitutions, or negative numbers. Parameter kind, the whole-parameter default, and the rest of the signature must stay unchanged.

When bindings are added, other union branches must be `undefined`. Defaults must match a property type made only of `string`, `number`, `boolean`, `null`, or a union of these. Aliases, other literal types, unknown type forms, BigInt defaults, legacy octal numbers, and numeric string/template escapes remain unproven.

This destructuring proof rejects rest, computed or nested patterns, duplicate keys or bindings, new aliases, and keys already present in the old type. It also rejects names referenced elsewhere in the signature, including another parameter's default and `typeof` inside a property or return type. Direct eval, escaped identifiers, and unsupported member forms remain unproven. The existing type-only classification is unchanged.

New bindings must also avoid direct-body lexical and function declarations, and any `var` binding in the same function scope. Unknown declaration forms remain blocking. Ordinary body references are outside this compatibility proof.

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

For JavaScript, TypeScript, and TSX, named default-exported functions and classes also receive references from module dependencies. This covers static default imports, namespace imports, default and namespace re-exports, `require()`, and dynamic `import()` with a string or a template literal without substitutions. Type-only forms such as `import type` and `typeof import()` also count as dependencies. A namespace can be passed to a loader such as `React.lazy`, so these dependencies conservatively keep the default declaration alive even without a local `.default` access. Other named exports and class members keep their existing reference rules; side-effect-only static imports and `export *` do not consume the default export. Dependencies found only in test files produce `test_only_symbols`.

This module lookup supports relative paths, extensionless and `index` paths, and source extension substitution such as `./panel.js` referring to `panel.tsx`. It checks candidate paths within the workspace and retains all plausible matches. It does not resolve package names, configured path aliases, escaped specifiers, or bundler query strings. Identifier occurrences returned by `refs` are unchanged.

When symbols with the same name exist in several files, they are skipped to avoid false findings. Class members in TS/JS and PHP are an exception, judged only when their owner can be inferred safely and uniquely. In PHP, `Owner::method()` and `self::method()` inside the same class count as definite references. When there are references whose owner cannot be determined, such as `$obj->method()` or callable strings, the symbol is skipped. `static::` can reach a subclass override through late static binding, so it is not resolved as a definite reference. Static calls through a class / trait / enum that `use`s a trait count as references only for a trait method reached uniquely. If the composing class has a concrete method with the same name, the resolution does not go to the trait, following PHP's resolution order.

For Ruby, a static constant receiver such as `Builder.new`, `Demo::Builder.new`, or `::Demo::Builder.new` can add potential-use evidence for that class's directly declared instance `initialize`. Lexically nested modules and calls in other files are supported. Test-only constructor calls produce `test_only_symbols`; production calls keep the initializer out of both lists. The lookup checks all Ruby reference files, including tests and generated files excluded from the candidates. A closer module or other constant binding prevents lookup from falling back to a different outer class.

This Ruby evidence requires one ordinary class and one direct instance initializer. It is withheld for inheritance, class reopenings, qualified class/module declarations, ambiguous public candidate names, singleton construction overrides, refinements, attribute generators, alias/removal/visibility changes, dynamic constant or method mutation, reflection, and external source loading. Predefined constructor factories such as top-level `Struct` do not establish ordinary instance construction. Calls inside `defined?` operands supply no evidence. A direct call in the same file before the initializer definition supplies no evidence; singleton-method receivers retain their definition-time evaluation position. The inventory is bounded to 10,000 reference files, 2 MiB per Ruby file, and 64 MiB of Ruby source, with limits on syntax depth, nodes, and retained facts. A read/parse failure or a limit discards all accumulated constructor evidence. These cases retain the existing classifier and can still report an implicitly used initializer: withholding evidence does not establish that it is unused. This is static potential-use analysis within the supported syntax, not execution-order analysis. Existing duplicate-name handling, public API extraction, and `refs` occurrences are unchanged; unused initializers remain candidates.

### Excluding Run-Time Conventions Automatically

Symbols that frameworks and test runners call dynamically, by naming convention or reflection, cannot be traced to a caller through identifier-level cross-file references and would be false findings. The following conventions are therefore excluded from dead-code automatically:

- **PHPUnit**: `*Test` / `*TestCase` / `*IntegrationTest` / `*FeatureTest` classes and `testXxx` / `setUp` / `tearDown` / `setUpBeforeClass` / `tearDownAfterClass` methods
- **Python unittest**: classes that inherit `unittest.TestCase` (and `unittest.IsolatedAsyncioTestCase`), with indirect inheritance within the same file resolved to a fixed point, and their `test_*` / `setUp` / `tearDown` / `setUpClass` / `tearDownClass` / `addCleanup` / `addClassCleanup` methods
- **Python pytest**: top-level `test_*` functions in `test_*.py` / `*_test.py` files, and every function in `conftest.py`
- **Python framework registration decorators**: functions, methods, and classes with registration decorators of Typer / Click / FastAPI / Flask / Django / Celery / pytest, and so on. A decorator imported under another name (`from celery import shared_task as st`) is matched by its original name. Methods defined in a class body that Pydantic calls during validation and serialization (`@model_validator` / `@field_validator` / `@field_serializer` / `@model_serializer` / `@computed_field` / `@root_validator`) are excluded too. The v1 `@validator` is excluded only when it is imported from `pydantic`
- **Python dynamic protocol methods**: `*_open` / `*_request` / `*_response` / `http_error_*` of the `urllib.request.BaseHandler` family, and the `on_*` callbacks of watchdog's `FileSystemEventHandler` family. Only methods of classes that directly inherit the known base classes are excluded
- **Angular**: lifecycle hooks of classes decorated with `@Component` / `@Directive` (`ngOnInit` / `ngOnDestroy` / `ngOnChanges` / `ngDoCheck` / `ngAfterContentInit` / `ngAfterContentChecked` / `ngAfterViewInit` / `ngAfterViewChecked`) are excluded because the Angular runtime calls them in the change detection cycle. `transform` of `@Pipe` classes (called from `| name` in templates) and `ngOnDestroy` of `@Injectable` / `@Pipe` classes (called when a service / pipe is destroyed) are excluded too. Other hooks of services and pipes (`ngOnInit`, and so on) are not called by Angular and are not excluded
- **PHP annotations and attributes**: methods and classes whose docblock has an annotation starting with an uppercase letter (`@Route`, `@Entity`, ...) or a PHPUnit annotation such as `@test` / `@dataProvider`, and those with a PHP 8 attribute of the same name (`#[Route]`, `#[Test]`, ...)
- **Methods called through syntax**: Kotlin `operator fun` and Swift `callAsFunction`. They are called through operators or call syntax, not by name. They stay in the API diff
- **Swift external protocol witnesses**: `applicationShouldTerminateAfterLastWindowClosed(_:)`, `menuNeedsUpdate(_:)`, `hash(into:)`, `makeNSView(context:)`, and `updateNSView(_:context:)` are excluded when the owning type's SDK/standard-library conformance and the method signature are established. Helpers and unused owners remain candidates. Additional requirements can be declared in [configuration](configuration.md#swift-protocol-methods); API changes and `refs` are unchanged
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

### Files whose references could not be counted

When a file whose references are counted cannot be read (over the 100MB limit, no permission, ...) or fails to parse, candidates whose names appear in that file are not reported as dead. They are listed in `unverified_symbols`, and `dead_symbols` keeps only the symbols confirmed to have no references. The file is reported as `source_analysis_failed` in `truncations`, whether or not any candidate was withheld.

```json
{ "...": "...", "unverified_symbols": [{ "name": "load_fixture", "kind": "function", "file": "src/fixtures.rs", "line": 3 }], "truncations": [{ "path": "data/huge.rs", "reason": "source_analysis_failed", "message": "references in this file were not counted (the file exceeds the read limit (115343360 bytes > 104857600 bytes)); 1 candidate name(s) appear in it, so they are not reported as dead (1 candidate(s) in total were withheld from dead/test-only)" }] }
```

A file over the limit is not parsed. Its content is read in fixed-size chunks only to check which candidate names appear in it. When a file cannot be opened or is larger than 1GiB, it is unknown which names appear, so every candidate is withheld. When only a test file could not be counted, `test_only_symbols` is not withheld: the absence of references from production files does not depend on test files. With `review --hook`, this appears in `trunc` only and does not cause exit 1 by itself.

### Detecting Frameworks Automatically

Even without `--framework`, the `nextjs` preset is applied automatically when a `package.json` directly under `<dir>` or in a monorepo package has a `next` key in `dependencies` / `devDependencies`. The convention globs detected this way are limited to paths relative to each Next.js workspace, so `app/**/page.tsx` in a sibling workspace that is not Next.js is not excluded. `node_modules`, generated output, and symlinks are not searched, and `peerDependencies` / `optionalDependencies` are ignored because they tend to trigger the preset by mistake. An explicit choice (`--framework laravel`, and so on) always wins over the detection.

```bash
# Applied automatically when a package.json at the root or in a workspace has `next`
astro-sight dead-code --dir .
astro-sight review --dir . --git
```

### Leaving Bin-Only Rust Crates Out of the API Diff

`api_changes` of `review` (`added` / `removed` / `modified`) automatically leaves out `pub fn` changes in bin-only Rust crates (no `src/lib.rs` and no `[lib]` section in `Cargo.toml`). A `pub fn` of a bin-only crate cannot be reached from outside the crate, so adding, removing, or changing its signature is never a compatibility problem of a public API. Even when the same diff deletes `src/lib.rs`, if the crate was a library crate at the base revision, the removal of the old public API correctly stays in `removed`.

For a removed Rust free function, the API check uses existing ownership proofs first. For unresolved same-name occurrences, it can discard those proven unable to reach the removed package through Cargo dependencies in both the old and new trees, then rerun the existing removal classification. This proof also checks the startup environment, logical and physical ancestors, and `CARGO_HOME` configuration; `--extern`, compiler replacement, and other uncertain inputs prevent it. Staged analysis cannot establish the original Cargo configuration location, so it does not use this proof. It shares the standard macro, derive and attribute scan described above with impact analysis: `println!` or `#[derive(Debug)]` alone does not block the proof. Removed functions with attributes or an ABI, and references that may still name the removed package, remain outside this proof. When the same diff rewrites a reference file, only unqualified occurrences proven to bind to a free function directly in that file may be discarded.

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

## Shell parse errors

This classification applies only when both versions use the Bash grammar. Replacing an API from another language with a shell function of the same name retains the existing deletion checks. Extensionless files are checked against their original shebang as well.

If a Bash/zsh function disappears from the parsed symbols but its declaration header remains inside an `ERROR` region, `review` reports it in `api_changes.uncertain_removals` instead of confirming a deletion. Each item includes the name, file, zero-based line, old signature, and `reason: "parse_error_region"`. The hook emits `api.rm_unverified` with `{n,f,l,r}`. These entries are informational; a confirmed deletion in the same file still blocks. Duplicate declarations and ambiguous string/heredoc contents keep the existing conservative deletion checks.

`truncations` (hook: `trunc`) also reports `parse_error_region` for changed Bash/zsh files with parser errors. This means declarations, references, and signature changes in those regions could not be verified; exit 0 does not establish compatibility. The parser does not invent replacement symbols, and the analysis of healthy regions continues.

When an explicit diff deletes a supported source file but its old content is absent from, differs from, or cannot be fully checked against the requested base, `review` reports `diff_base_mismatch` in `truncations` (hook: `trunc`). The hook blocks with `api.diff_base_mismatch`. Any extracted deleted symbols stay in blocking `api.rm`; if no old source is available, the truncation is the only finding. Deleting documentation or a Git submodule does not trigger this check. `--git` fixes submodule output to the short gitlink form regardless of `diff.submodule`. Use the revision before deletion as `--base` when available.
