use std::collections::HashMap;
use std::ffi::OsStr;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use super::*;

/// 環境変数は MiB の非負の整数だけを受け付け、それ以外は既定に戻さず失敗させる。
#[test]
fn budget_from_env_accepts_only_non_negative_mib() {
    assert_eq!(
        ParseBudget::from_env_value(None).unwrap(),
        ParseBudget::default()
    );
    let limit = |text: &str| ParseBudget::from_env_value(Some(OsStr::new(text))).map(|b| b.limit);
    assert_eq!(limit("0").unwrap(), 0, "0 は大きいファイルを 1 本ずつ");
    assert_eq!(limit("32").unwrap(), 32 * MIB);
    assert_eq!(limit("007").unwrap(), 7 * MIB);
    assert_eq!(
        limit("17592186044415").unwrap(),
        (u64::MAX >> 20) << 20,
        "バイトに直して収まる最大"
    );
    for invalid in [
        "",
        " 32",
        "32 ",
        "-1",
        "+5",
        "1.5",
        "32MiB",
        "abc",
        "17592186044416",
        "18446744073709551615",
        "99999999999999999999999",
    ] {
        let error = limit(invalid).expect_err(invalid).to_string();
        assert!(error.contains(PARSE_BUDGET_ENV), "{invalid}: {error}");
    }
    // 予算だけを変え、閾値は既定のまま。
    assert_eq!(
        ParseBudget::from_env_value(Some(OsStr::new("1")))
            .unwrap()
            .threshold,
        ParseBudget::default().threshold
    );
}

#[test]
fn large_file_threshold_is_inclusive() {
    let budget = ParseBudget::default();
    assert!(!budget.is_large(LARGE_FILE_THRESHOLD - 1));
    assert!(budget.is_large(LARGE_FILE_THRESHOLD));
    assert_eq!(LARGE_FILE_THRESHOLD, 64 * 1024);
}

/// 予約が無ければ大きさに関わらず入れ、予約があれば合計が予算以下のときだけ入れる。
#[test]
fn admission_respects_budget_and_admits_oversize_alone() {
    let state = |in_use| SchedulerState::<()> {
        in_use,
        waiting: Vec::new(),
    };
    assert!(state(0).admits(25, 10), "予算を超える 1 本は単独で入る");
    assert!(state(5).admits(5, 10), "合計が予算ちょうど");
    assert!(!state(5).admits(6, 10), "合計が予算 + 1");
    assert!(state(0).admits(3, 0), "予算 0 でも単独なら入る");
    assert!(!state(1).admits(1, 0), "予算 0 は 1 本ずつ");
    assert!(
        !state(u64::MAX).admits(1, u64::MAX),
        "合計が溢れるなら入れない"
    );
}

/// 待ち行列は大きい順に並べ、先頭が入らなくても後ろの入るものを取り出す。
#[test]
fn take_admissible_skips_blocked_head() {
    let mut state = SchedulerState {
        in_use: 6,
        waiting: Vec::new(),
    };
    for (size, job) in [(2, "b"), (5, "a"), (4, "c"), (2, "d")] {
        state.enqueue(size, job);
    }
    let order: Vec<_> = state.waiting.iter().map(|(_, job)| *job).collect();
    assert_eq!(
        order,
        ["a", "c", "b", "d"],
        "大きい順、同じ大きさは入った順"
    );
    let taken: Vec<_> = state
        .take_admissible(10)
        .into_iter()
        .map(|(_, job)| job)
        .collect();
    assert_eq!(taken, ["c"], "5 は入らず、4 は入る (6 + 4 = 10)");
    assert_eq!(state.in_use, 10);
    let rest: Vec<_> = state.waiting.iter().map(|(_, job)| *job).collect();
    assert_eq!(rest, ["a", "b", "d"]);
}

/// 模擬ジョブの実行を記録する。予算の判定はスケジューラとは別にここでもう一度行い、
/// ジョブの終わり (模擬の木を捨てた後) まで大きさを占有したものとして数える。予約を早く
/// 返すスケジューラだと、次のジョブの開始時に占有が予算を超えて見える。
struct Probe {
    limit: u64,
    active: Mutex<u64>,
    violations: Mutex<Vec<String>>,
    runs: Mutex<HashMap<&'static str, usize>>,
    started: Mutex<Vec<&'static str>>,
    started_changed: Condvar,
}

/// 模擬の構文木。捨てるときに占有を返す。
struct FakeTree<'p> {
    probe: &'p Probe,
    size: u64,
}

impl Drop for FakeTree<'_> {
    fn drop(&mut self) {
        *self.probe.active.lock().unwrap() -= self.size;
    }
}

const WAIT: Duration = Duration::from_secs(30);

impl Probe {
    fn new(limit: u64) -> Self {
        Self {
            limit,
            active: Mutex::new(0),
            violations: Mutex::new(Vec::new()),
            runs: Mutex::new(HashMap::new()),
            started: Mutex::new(Vec::new()),
            started_changed: Condvar::new(),
        }
    }

    /// 予算の判定をし直して占有を始める。
    fn enter(&self, id: &'static str, size: u64) -> FakeTree<'_> {
        {
            let mut active = self.active.lock().unwrap();
            if *active != 0 && *active + size > self.limit {
                self.violations.lock().unwrap().push(format!(
                    "{id}: {size} bytes started while {active} were active"
                ));
            }
            *active += size;
        }
        *self.runs.lock().unwrap().entry(id).or_default() += 1;
        self.started.lock().unwrap().push(id);
        self.started_changed.notify_all();
        FakeTree { probe: self, size }
    }

    /// `id` のジョブが始まるまで待つ。始まらなければ false (直列にされた)。
    fn wait_started(&self, id: &str) -> bool {
        let started = self.started.lock().unwrap();
        let (started, timeout) = self
            .started_changed
            .wait_timeout_while(started, WAIT, |started| !started.contains(&id))
            .unwrap();
        drop(started);
        !timeout.timed_out()
    }

    fn assert_clean(&self) {
        let violations = self.violations.lock().unwrap();
        assert!(violations.is_empty(), "{violations:?}");
        assert_eq!(*self.active.lock().unwrap(), 0, "占有が残っていない");
    }

    fn runs(&self, id: &str) -> usize {
        self.runs.lock().unwrap().get(id).copied().unwrap_or(0)
    }
}

fn pool(threads: usize) -> rayon::ThreadPool {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .unwrap()
}

/// 多数のジョブで、同時の占有が予算 (か単独の 1 本) を超えず、各ジョブを 1 回ずつ実行する。
#[test]
fn scheduler_never_exceeds_budget_and_runs_each_job_once() {
    const IDS: [&str; 12] = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l"];
    let sizes = [7, 6, 5, 4, 3, 3, 2, 12, 9, 1, 10, 11];
    let probe = Probe::new(10);
    let run = |id: &'static str, reserved: u64| {
        let _tree = probe.enter(id, reserved);
        std::thread::sleep(Duration::from_millis(2));
        JobOutcome::Done
    };
    let scheduler = BudgetScheduler::new(ParseBudget::new(1, 10), &run);
    let jobs: Vec<_> = IDS.iter().copied().zip(sizes).collect();
    pool(8).install(|| {
        rayon::scope(|scope| {
            use rayon::prelude::*;
            jobs.par_iter()
                .for_each(|&(id, size)| scheduler.submit(scope, size, id));
        });
    });
    scheduler.finish().unwrap();
    probe.assert_clean();
    for id in IDS {
        assert_eq!(probe.runs(id), 1, "{id}");
    }
}

/// 予算に一緒に入るジョブは重ねて実行する (直列にしない)。
#[test]
fn scheduler_overlaps_jobs_that_fit_together() {
    let probe = Probe::new(10);
    let saw_other = Mutex::new(Vec::new());
    let run = |id: &'static str, reserved: u64| {
        let _tree = probe.enter(id, reserved);
        let other = if id == "a" { "b" } else { "a" };
        saw_other
            .lock()
            .unwrap()
            .push((id, probe.wait_started(other)));
        JobOutcome::Done
    };
    let scheduler = BudgetScheduler::new(ParseBudget::new(1, 10), &run);
    pool(4).install(|| {
        rayon::scope(|scope| {
            scheduler.submit(scope, 5, "a");
            scheduler.submit(scope, 5, "b");
        });
    });
    scheduler.finish().unwrap();
    probe.assert_clean();
    let mut saw_other = saw_other.into_inner().unwrap();
    saw_other.sort();
    assert_eq!(saw_other, [("a", true), ("b", true)]);
}

/// ジョブの終わりに待ち行列を見るとき、先頭 (5) が入らなくても後ろの入るもの (4) を起動する。
#[test]
fn scheduler_starts_fitting_job_behind_blocked_head() {
    let probe = Probe::new(10);
    let a_saw_c = Mutex::new(None);
    let submitted = Mutex::new(false);
    let submitted_changed = Condvar::new();
    let run = |id: &'static str, reserved: u64| {
        let _tree = probe.enter(id, reserved);
        match id {
            "a" => *a_saw_c.lock().unwrap() = Some(probe.wait_started("c")),
            // b と c を投入し終えるまで d は予算を埋めたままにし、c を待ち行列へ入れる。
            "d" => drop(
                submitted_changed
                    .wait_timeout_while(submitted.lock().unwrap(), WAIT, |done| !*done)
                    .unwrap(),
            ),
            _ => {}
        }
        JobOutcome::Done
    };
    let scheduler = BudgetScheduler::new(ParseBudget::new(1, 10), &run);
    pool(4).install(|| {
        rayon::scope(|scope| {
            // a (6) は c が始まるまで終わらない。d (4) で予算が埋まるので b と c は待ち行列へ入り、
            // d の終わりに 5 の b は入らず (6 + 5)、4 の c は入る (6 + 4)。
            scheduler.submit(scope, 6, "a");
            scheduler.submit(scope, 4, "d");
            scheduler.submit(scope, 5, "b");
            scheduler.submit(scope, 4, "c");
            *submitted.lock().unwrap() = true;
            submitted_changed.notify_all();
        });
    });
    scheduler.finish().unwrap();
    probe.assert_clean();
    assert_eq!(*a_saw_c.lock().unwrap(), Some(true));
    for id in ["a", "b", "c", "d"] {
        assert_eq!(probe.runs(id), 1, "{id}");
    }
}

/// 予算を超える 1 本はほかの大きいジョブと重ねないが、予算の外の小さい処理は止めない。
#[test]
fn oversize_job_runs_alone_but_small_work_continues() {
    let probe = Probe::new(10);
    let small_ran = Mutex::new(false);
    let small_ran_changed = Condvar::new();
    let saw_small = Mutex::new(None);
    let run = |id: &'static str, reserved: u64| {
        let _tree = probe.enter(id, reserved);
        if id == "huge" {
            let ran = small_ran.lock().unwrap();
            let (_ran, timeout) = small_ran_changed
                .wait_timeout_while(ran, WAIT, |ran| !*ran)
                .unwrap();
            *saw_small.lock().unwrap() = Some(!timeout.timed_out());
        }
        JobOutcome::Done
    };
    let scheduler = BudgetScheduler::new(ParseBudget::new(1, 10), &run);
    pool(4).install(|| {
        rayon::scope(|scope| {
            scheduler.submit(scope, 25, "huge");
            scheduler.submit(scope, 3, "after");
            // 予算の外の小さいファイルの処理に当たる。huge の実行中に動けること。
            scope.spawn(|_| {
                if probe.wait_started("huge") {
                    *small_ran.lock().unwrap() = true;
                    small_ran_changed.notify_all();
                }
            });
        });
    });
    scheduler.finish().unwrap();
    probe.assert_clean();
    assert_eq!(*saw_small.lock().unwrap(), Some(true));
    assert_eq!(probe.runs("after"), 1);
}

/// ワーカー 1 本でも、投入が終わった後に待ち行列を最後まで実行する。
#[test]
fn single_worker_drains_queue_after_producer_finishes() {
    const IDS: [&str; 5] = ["a", "b", "c", "d", "e"];
    let probe = Probe::new(10);
    let run = |id: &'static str, reserved: u64| {
        let _tree = probe.enter(id, reserved);
        JobOutcome::Done
    };
    let scheduler = BudgetScheduler::new(ParseBudget::new(1, 10), &run);
    pool(1).install(|| {
        rayon::scope(|scope| {
            use rayon::prelude::*;
            IDS.par_iter()
                .for_each(|&id| scheduler.submit(scope, 8, id));
        });
    });
    scheduler.finish().unwrap();
    probe.assert_clean();
    for id in IDS {
        assert_eq!(probe.runs(id), 1, "{id}");
    }
}

/// ジョブが panic しても予約を返し、待ちのジョブを実行してから panic を伝える。
#[test]
fn panicking_job_releases_budget_and_remaining_jobs_run() {
    let probe = Probe::new(10);
    let run = |id: &'static str, reserved: u64| {
        let _tree = probe.enter(id, reserved);
        if id == "boom" {
            panic!("模擬の失敗");
        }
        JobOutcome::Done
    };
    let scheduler = BudgetScheduler::new(ParseBudget::new(1, 10), &run);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pool(4).install(|| {
            rayon::scope(|scope| {
                scheduler.submit(scope, 6, "boom");
                scheduler.submit(scope, 6, "b");
                scheduler.submit(scope, 6, "c");
            });
        });
    }));
    assert!(outcome.is_err(), "panic を伝える");
    scheduler.finish().unwrap();
    probe.assert_clean();
    for id in ["boom", "b", "c"] {
        assert_eq!(probe.runs(id), 1, "{id}");
    }
}

/// 開き直して大きくなっていたジョブは、予約を返して新しい大きさで入れ直す。
#[test]
fn resubmitted_job_waits_for_its_new_size() {
    let probe = Probe::new(10);
    let run = |id: &'static str, reserved: u64| {
        let _tree = probe.enter(id, reserved);
        if id == "grows" && reserved == 2 {
            return JobOutcome::Resubmit { size: 9, job: id };
        }
        if id == "other" {
            // grows が 9 で入り直しても、8 の other とは重ならない。
            std::thread::sleep(Duration::from_millis(20));
        }
        JobOutcome::Done
    };
    let scheduler = BudgetScheduler::new(ParseBudget::new(1, 10), &run);
    pool(4).install(|| {
        rayon::scope(|scope| {
            scheduler.submit(scope, 2, "grows");
            scheduler.submit(scope, 8, "other");
        });
    });
    scheduler.finish().unwrap();
    probe.assert_clean();
    assert_eq!(
        probe.runs("grows"),
        2,
        "元の大きさと新しい大きさで 1 回ずつ"
    );
    assert_eq!(probe.runs("other"), 1);
}

/// 予約か待ち行列が残ったまま終えたら、件数を返さず失敗させる。
#[test]
fn finish_rejects_leftover_jobs() {
    let run = |_: &'static str, _: u64| JobOutcome::Done;
    let scheduler = BudgetScheduler::new(ParseBudget::new(1, 10), &run);
    scheduler.lock().enqueue(3, "left");
    let error = scheduler.finish().unwrap_err().to_string();
    assert!(error.contains("1 queued jobs"), "{error}");
}

/// `map_with_budget` は入力の順に結果を返し、各項目を 1 回ずつ実行する。閾値以上の項目は
/// 予算の中で実行し (占有が予算か単独の 1 本を超えない)、閾値未満は予算の外で実行する。
#[test]
fn map_with_budget_keeps_order_and_bounds_large_items() {
    const IDS: [&str; 10] = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"];
    let sizes: [u64; 10] = [7, 1, 6, 12, 2, 5, 9, 1, 4, 8];
    let items: Vec<(&'static str, u64)> = IDS.iter().copied().zip(sizes).collect();
    for threads in [1, 4] {
        // 閾値 3: 1 と 2 の項目は予算の外。
        let probe = Probe::new(10);
        let results = pool(threads).install(|| {
            map_with_budget(
                &items,
                ParseBudget::new(3, 10),
                |(_, size)| *size,
                |&(id, size)| {
                    let tree = (size >= 3).then(|| probe.enter(id, size));
                    std::thread::sleep(Duration::from_millis(2));
                    drop(tree);
                    if size < 3 {
                        *probe.runs.lock().unwrap().entry(id).or_default() += 1;
                    }
                    id.to_uppercase()
                },
            )
            .unwrap()
        });
        let expected: Vec<String> = IDS.iter().map(|id| id.to_uppercase()).collect();
        assert_eq!(results, expected, "threads={threads}");
        probe.assert_clean();
        for id in IDS {
            assert_eq!(probe.runs(id), 1, "{id} threads={threads}");
        }
    }
}
