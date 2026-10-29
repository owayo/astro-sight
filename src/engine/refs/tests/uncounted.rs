use super::*;

#[test]
fn stream_scan_keeps_escaped_format_names_across_chunks() {
    let names = vec!["VALUE".to_string(), "OTHER".to_string()];
    let acs = build_batch_acs(&names).unwrap();
    let source = b"format!(\"{VA\\u{4c}UE}\")";
    for chunk in 1..=12 {
        assert_eq!(
            stream_present_indices_from(&acs, &names, source.as_slice(), chunk, u64::MAX),
            Some(vec![0, 1]),
            "chunk={chunk}"
        );
    }
}

/// 読み込めないファイルのストリーム走査は、チャンクの境界をまたぐ名前も拾い、
/// ファイル全体を一度に検索した場合と同じ集合を返す。
#[test]
fn stream_scan_matches_whole_source_across_chunk_boundaries() {
    let names: Vec<String> = ["alpha", "beta", "gamma", "r#gen"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let acs = build_batch_acs(&names).unwrap();
    let text = b"xx alpha yy BETA zz gen".to_vec();
    let mut expected: Vec<usize> = ac_present_indices_multi(&acs, &text, names.len())
        .into_iter()
        .collect();
    expected.sort_unstable();
    assert_eq!(
        expected,
        vec![0, 1, 3],
        "大文字小文字と r# を外した形も拾う"
    );
    for chunk in 1..=9 {
        assert_eq!(
            stream_present_indices_from(&acs, &names, text.as_slice(), chunk, u64::MAX),
            Some(expected.clone()),
            "chunk={chunk}"
        );
    }
    // 上限を超えて読んだら、途中までの結果で絞らずに None (全部の名前を参照しうる)
    assert_eq!(
        stream_present_indices_from(&acs, &names, text.as_slice(), 4, 8),
        None
    );
}

/// 途中で読めなくなったら None (途中までに見つかった名前だけに絞らない)。
#[test]
fn stream_scan_failure_midway_is_none() {
    struct FailAfterFirstRead(bool);
    impl std::io::Read for FailAfterFirstRead {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.0 {
                return Err(std::io::Error::other("broken"));
            }
            self.0 = true;
            let data = b"alpha ";
            buf[..data.len()].copy_from_slice(data);
            Ok(data.len())
        }
    }
    let names = vec!["alpha".to_string(), "beta".to_string()];
    let acs = build_batch_acs(&names).unwrap();
    assert_eq!(
        stream_present_indices_from(&acs, &names, FailAfterFirstRead(false), 16, u64::MAX),
        None
    );
}

/// 件数集計で数えられなかったファイルを、参照しうる名前と一緒に返す。
///
/// 旧実装は読み込み・構文解析の失敗を黙って捨て、そのファイルからの参照を 0 件にしていた。
/// 対照: 読めるファイルの件数は従来どおり数える。
#[test]
fn uncounted_files_are_reported_with_possible_names() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("ok.ts"), "alpha();\n").unwrap();
    // 言語を判定できないファイル (構文解析に進めない)。名前の出現は分かる。
    std::fs::write(dir.path().join("notes.unknownext"), "beta and gamma\n").unwrap();
    let names: Vec<String> = ["alpha", "beta", "gamma", "delta"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let files = vec![
        dir.path().join("ok.ts"),
        dir.path().join("notes.unknownext"),
    ];

    // 開けないファイル (権限なし)。root で動いていて読めてしまう環境では確かめない。
    #[cfg(unix)]
    let (files, locked) = {
        use std::os::unix::fs::PermissionsExt;
        let mut files = files;
        let path = dir.path().join("locked.ts");
        std::fs::write(&path, "delta();\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let locked = std::fs::read(&path).is_err();
        if locked {
            files.push(path);
        }
        (files, locked)
    };
    #[cfg(not(unix))]
    let locked = false;

    let result = count_non_definition_refs_split_in_files(&names, &files, |path| {
        path.file_name()
            .is_some_and(|name| name == "notes.unknownext")
    })
    .unwrap();
    assert_eq!(result.counts["alpha"], (1, 0), "読めるファイルは数える");

    let notes = result
        .uncounted
        .iter()
        .find(|file| file.path.ends_with("notes.unknownext"))
        .expect("言語を判定できないファイルを申告する");
    assert_eq!(
        notes.possible_names,
        Some(vec![1, 2]),
        "現れる beta / gamma"
    );
    assert!(notes.is_test, "is_test の判定を引き継ぐ");
    assert_eq!(notes.cause, PARSE_FAILURE_CAUSE);

    if locked {
        let locked_file = result
            .uncounted
            .iter()
            .find(|file| file.path.ends_with("locked.ts"))
            .expect("開けないファイルを申告する");
        assert_eq!(locked_file.possible_names, None, "中身を確かめられない");
        assert!(
            locked_file.cause.contains("could not be opened"),
            "{}",
            locked_file.cause
        );
        assert!(!locked_file.is_test);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                dir.path().join("locked.ts"),
                std::fs::Permissions::from_mode(0o644),
            )
            .unwrap();
        }
    }
    // パス順 (rayon の完了順に依らない)
    let paths: Vec<_> = result
        .uncounted
        .iter()
        .map(|file| file.path.clone())
        .collect();
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted);
    assert_eq!(result.uncounted.len(), 1 + usize::from(locked));
}
