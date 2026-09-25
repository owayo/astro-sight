# Supported Languages

| Language | Extension | Crate | Version |
|---|---|---|---|
| <img src="https://img.shields.io/badge/-000000?logo=rust&amp;logoColor=white" height="16"> Rust | `.rs` | `tree-sitter-rust` | 0.24 |
| <img src="https://img.shields.io/badge/-A8B9CC?logo=c&amp;logoColor=white" height="16"> C | `.c`, `.h` (default) | `tree-sitter-c` | 0.24 |
| <img src="https://img.shields.io/badge/-00599C?logo=cplusplus&amp;logoColor=white" height="16"> C++ | `.cpp`, `.cc`, `.cxx`, `.hpp`, `.hh`, `.hxx`, `.h` (when C++ syntax is detected) | `tree-sitter-cpp` | 0.23 |
| <img src="https://img.shields.io/badge/-3776AB?logo=python&amp;logoColor=white" height="16"> Python | `.py`, `.pyi` | `tree-sitter-python` | 0.25 |
| <img src="https://img.shields.io/badge/-F7DF1E?logo=javascript&amp;logoColor=black" height="16"> JavaScript | `.js`, `.mjs`, `.cjs`, `.jsx` | `tree-sitter-javascript` | 0.25 |
| <img src="https://img.shields.io/badge/-3178C6?logo=typescript&amp;logoColor=white" height="16"> TypeScript | `.ts`, `.mts`, `.cts` | `tree-sitter-typescript` | 0.23 |
| <img src="https://img.shields.io/badge/-61DAFB?logo=react&amp;logoColor=black" height="16"> TSX | `.tsx` | `tree-sitter-typescript` | 0.23 |
| <img src="https://img.shields.io/badge/-00ADD8?logo=go&amp;logoColor=white" height="16"> Go | `.go` | `tree-sitter-go` | 0.25 |
| <img src="https://img.shields.io/badge/-777BB4?logo=php&amp;logoColor=white" height="16"> PHP | `.php`, `.phtml` | `tree-sitter-php` | 0.24 |
| <img src="https://img.shields.io/badge/-ED8B00?logo=openjdk&amp;logoColor=white" height="16"> Java | `.java` | `tree-sitter-java` | 0.23 |
| <img src="https://img.shields.io/badge/-7F52FF?logo=kotlin&amp;logoColor=white" height="16"> Kotlin | `.kt`, `.kts` | `tree-sitter-kotlin` | 0.3.5 * |
| <img src="https://img.shields.io/badge/-F05138?logo=swift&amp;logoColor=white" height="16"> Swift | `.swift` | `tree-sitter-swift` | 0.7 |
| <img src="https://img.shields.io/badge/-512BD4?logo=dotnet&amp;logoColor=white" height="16"> C# | `.cs` | `tree-sitter-c-sharp` | 0.23 |
| <img src="https://img.shields.io/badge/-4EAA25?logo=gnubash&amp;logoColor=white" height="16"> Bash | `.sh`, `.bash`, `.zsh` | `tree-sitter-bash` | 0.25 |
| <img src="https://img.shields.io/badge/-CC342D?logo=ruby&amp;logoColor=white" height="16"> Ruby | `.rb`, `.rake`, `.gemspec` | `tree-sitter-ruby` | [owayo/tree-sitter-ruby](https://github.com/owayo/tree-sitter-ruby) |
| <img src="https://img.shields.io/badge/-F7A41D?logo=zig&amp;logoColor=white" height="16"> Zig | `.zig`, `.zon` | `tree-sitter-zig` | 1.1 |

All 16 languages get precise symbol extraction through tree-sitter queries. Ruby supports Unicode identifiers, and method names containing characters subject to simple case folding, such as `ſ` and `K`, are extracted without gaps. Index assignments with spaces, `{}` right after unparenthesized lambda parameters, empty regular expressions, a block right after an unparenthesized call, and heredocs with an empty identifier also parse without syntax errors.

`.h` is treated as a C header by default. It is parsed as C++ only when it contains markers of C++-only syntax and the C++ parser gives clearly fewer parse errors. This keeps C headers from being treated as C++ by accident, while `symbols` / `review` / `dead-code` do not miss definitions in C++ headers such as `class Foo { public: ... }` or `struct X : Base<X> {}`.

> **\* About the Kotlin version:** `tree-sitter-kotlin` 0.3.8 and later depend on `tree-sitter` `>=0.21, <0.23`. `tree-sitter` in that range declares `links = "tree-sitter"`, just like the 0.27 that astro-sight itself uses. Cargo allows only one package with the same `links` value in a dependency graph, so dependency resolution fails. `tree-sitter` 0.20, which 0.3.5 depends on, does not declare `links` and can coexist with 0.27, so the dependency is pinned to 0.3.5.
>
> ```text
> error: failed to select a version for `tree-sitter`.
>     ... required by package `tree-sitter-kotlin v0.3.8`
> versions that meet the requirements `>=0.21, <0.23` are: 0.22.6, 0.22.5, 0.22.4, 0.22.3, 0.22.2, 0.22.1, 0.21.0
>
> package `tree-sitter` links to the native library `tree-sitter`, but it conflicts with a previous package which links to `tree-sitter` as well:
> package `tree-sitter v0.27.0`
> ```
