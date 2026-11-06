"""dead-code の構文解析の予算 (#253) の計測に使った合成コーパスを作る。

使い方: python -I make_corpora.py <出力先>

- many-large: 4 MiB 前後の C ファイル 24 本 (候補にも参照の集計にも入る数値の表)
- ac-miss: 候補の名前を含まない 20 MiB 前後の C ファイル 8 本
- lexer-only: 8 MiB の Xojo ファイル 1 本 (tree-sitter を使わない字句の経路)
- below-t: 1 MiB を 1 バイト下回る TS ファイル 48 本

どれも git の作業ツリーにし、候補 (export した関数) は小さい TS / C のファイルに置く。
"""

import os
import subprocess
import sys

MIB = 1024 * 1024
ROW = "  {1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16},\n"


def write(path: str, data: str) -> None:
    """改行を LF のまま UTF-8 で書く。"""
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as handle:
        handle.write(data.encode("utf-8"))


def fill(line: str, size: int) -> str:
    """`line` を繰り返して `size` バイト前後にした文字列を返す。

    Returns:
        繰り返した文字列。
    """
    return line * (size // len(line))


def lib(root: str, names: list[str]) -> None:
    """`names` を export する小さい TS ファイルを置く。"""
    body = "".join(
        f"export function {name}(): number {{\n  return 1;\n}}\n\n" for name in names
    )
    write(os.path.join(root, "src", "lib.ts"), body)


def many_large(root: str) -> None:
    """4 MiB 前後の数値の表の C ファイル 24 本と、それが呼ぶ関数を置く。"""
    names = [f"scanner_hook_{i}" for i in range(24)] + ["unused_hook"]
    decls = "".join(f"int {name}(int value);\n" for name in names)
    defs = "".join(f"int {name}(int value) {{\n  return value;\n}}\n" for name in names)
    write(os.path.join(root, "src", "hooks.h"), decls)
    write(os.path.join(root, "src", "hooks.c"), '#include "hooks.h"\n' + defs)
    for i in range(24):
        table = fill(ROW, 4 * MIB + i * 4096)
        body = (
            '#include "../src/hooks.h"\n'
            f"static const int table_{i}[][16] = {{\n{table}}};\n"
            f"int use_{i}(void) {{ return scanner_hook_{i}(table_{i}[0][0]); }}\n"
        )
        write(os.path.join(root, "gen", f"table{i}.c"), body)


def ac_miss(root: str) -> None:
    """候補の名前を含まない 20 MiB 前後の C ファイル 8 本を置く。"""
    lib(root, ["usedSmall", "unusedSmall"])
    write(
        os.path.join(root, "src", "main.ts"),
        'import { usedSmall } from "./lib";\nusedSmall();\n',
    )
    for i in range(8):
        body = f"static const int data_{i}[][16] = {{\n{fill(ROW, 20 * MIB)}}};\n"
        write(os.path.join(root, "gen", f"data{i}.c"), body)


def lexer_only(root: str) -> None:
    """8 MiB の Xojo ファイル 1 本を置く。"""
    lib(root, ["usedFromXojo", "unusedXojo"])
    line = "Dim total As Integer = 1 + 2 + 3 + 4 + 5 + 6 + 7 + 8\n"
    body = "Sub Run()\n" + fill(line, 8 * MIB) + "usedFromXojo()\nEnd Sub\n"
    write(os.path.join(root, "gen", "Big.xojo_code"), body)


def below_t(root: str) -> None:
    """1 MiB を 1 バイト下回る TS ファイル 48 本を置く。"""
    lib(root, [f"usedBelow{i}" for i in range(48)] + ["unusedBelow"])
    line = "const row = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];\n"
    for i in range(48):
        head = f'import {{ usedBelow{i} }} from "../src/lib";\nusedBelow{i}();\n'
        body = head + fill(line, MIB - 1 - len(head))
        body += " " * (MIB - 1 - len(body.encode("utf-8")))
        write(os.path.join(root, "gen", f"part{i}.ts"), body)


def main() -> int:
    """各コーパスを作って git の作業ツリーにする。

    Returns:
        終了コード。
    """
    out = sys.argv[1]
    makers = [
        ("many-large", many_large),
        ("ac-miss", ac_miss),
        ("lexer-only", lexer_only),
        ("below-t", below_t),
    ]
    for name, make in makers:
        root = os.path.join(out, name)
        make(root)
        subprocess.run(["git", "init", "-q", root], check=True)
        subprocess.run(["git", "-C", root, "add", "-A"], check=True)
        identity = ["-c", "user.name=bench", "-c", "user.email=bench@example.invalid"]
        commit = ["git", "-C", root, *identity, "commit", "-qm", "corpus"]
        subprocess.run(commit, check=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
