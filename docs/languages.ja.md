# 対応言語

| 言語 | 拡張子 | クレート | 版 |
|---|---|---|---|
| <img src="https://img.shields.io/badge/-000000?logo=rust&amp;logoColor=white" height="16"> Rust | `.rs` | `tree-sitter-rust` | 0.24 |
| <img src="https://img.shields.io/badge/-A8B9CC?logo=c&amp;logoColor=white" height="16"> C | `.c`, `.h`（既定） | `tree-sitter-c` | 0.24 |
| <img src="https://img.shields.io/badge/-00599C?logo=cplusplus&amp;logoColor=white" height="16"> C++ | `.cpp`, `.cc`, `.cxx`, `.hpp`, `.hh`, `.hxx`, `.h`（C++ 構文検出時） | `tree-sitter-cpp` | 0.23 |
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

上記 16 言語は tree-sitter クエリによる精密なシンボル抽出に対応。Ruby は Unicode 識別子に対応し、simple case folding の対象となる `ſ` / `K` などを含むメソッド名も欠落なく抽出する。空白を挟む添字代入、括弧なし lambda 仮引数直後の `{}`、空正規表現、括弧なし呼び出し直後の block、空識別子 heredoc も構文エラーなく解析できる。

`.h` は既定では C ヘッダとして扱う。C++ 専用構文のマーカーがあり、C++ parser の方が明確に parse error が少ない場合だけ C++ として解析する。C ヘッダを不用意に C++ 扱いせずに、`class Foo { public: ... }` や `struct X : Base<X> {}` のような C++ ヘッダでの `symbols` / `review` / `dead-code` の取りこぼしを抑える。

> **\* Kotlin バージョンについて:** `tree-sitter-kotlin` 0.3.8 以降は `tree-sitter` の `>=0.21, <0.23` に依存する。この範囲の `tree-sitter` は、本体が使う 0.27 と同じく `links = "tree-sitter"` を宣言している。Cargo は同じ `links` を持つパッケージを依存グラフに 1 つしか置けないため、依存解決の段階で失敗する。0.3.5 が依存する `tree-sitter` 0.20 は `links` を宣言していないので 0.27 と共存でき、現在は 0.3.5 に固定している。
>
> ```text
> error: failed to select a version for `tree-sitter`.
>     ... required by package `tree-sitter-kotlin v0.3.8`
> versions that meet the requirements `>=0.21, <0.23` are: 0.22.6, 0.22.5, 0.22.4, 0.22.3, 0.22.2, 0.22.1, 0.21.0
>
> package `tree-sitter` links to the native library `tree-sitter`, but it conflicts with a previous package which links to `tree-sitter` as well:
> package `tree-sitter v0.27.0`
> ```
