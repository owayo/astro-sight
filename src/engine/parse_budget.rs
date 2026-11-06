//! 大きいファイルの構文解析を、同時に扱うソースの大きさの予算で制限する。
//!
//! tree-sitter の構文木はソースの約 20〜30 倍のメモリを使い (24 MB の生成された `parser.c`
//! 1 本で約 650 MiB)、数値の並ぶ表のような密なソースでは 100 倍を超える。全ファイルを rayon で
//! 並列に解析すると、大きいファイルがワーカーの数まで同時に解析され、ピークメモリが大きい
//! ファイルの数とスレッド数に比例して上限なく増える。
//!
//! [`ParseBudget`] の閾値以上のファイルだけを予算の対象にし、実行中と予約済みの大きさの合計が
//! 予算に収まるものだけを同時に実行する ([`BudgetScheduler`])。予算を超える 1 本は、ほかの
//! 大きいファイルを実行していないときに単独で実行する。待つファイルはワーカーを止めずに
//! 待ち行列へ入り、先のジョブが終わったときに取り出されて `rayon::scope` に spawn される。
//! 閾値未満のファイルは予算の外で従来どおり並列に処理するので、同時に扱うソースの大きさは
//! おおよそ `max(予算, 最大のファイル) + ワーカー数 × 閾値` に収まる。メモリの厳密な上限では
//! ない (Parser が thread-local に残す作業領域や、木の大きさとソースの大きさの比は内容で違う)。

use std::sync::{Mutex, MutexGuard, PoisonError};

use anyhow::Result;

use crate::error::{AstroError, ErrorCode};

/// 予算を MiB 単位で上書きする環境変数。0 は大きいファイルを 1 本ずつ解析する。
pub const PARSE_BUDGET_ENV: &str = "ASTRO_SIGHT_PARSE_BUDGET_MIB";

const MIB: u64 = 1024 * 1024;

/// 予算の対象にするファイルの大きさの下限 (`parser::read_file` が mmap に切り替える大きさ)。
/// 予算の外で同時に扱う大きさ (ワーカー数 × 閾値) を 1 MiB 程度に抑え、予算をほぼ全体に効かせる。
/// これ以上のファイルは通常のリポジトリでは少なく、既定の予算なら 500 本ほどが同時に入るので、
/// 通常の速度は変えない (開き直す 1 回ぶんの open が増えるだけ)。
const LARGE_FILE_THRESHOLD: u64 = 64 * 1024;

/// 既定の予算 (MiB)。24 MB 級の `parser.c` を 2 本同時には解析しない大きさ。
const DEFAULT_LIMIT_MIB: u64 = 32;

/// 大きいファイルの構文解析の予算。走査 1 回ごとに読む (同じ走査の途中では変えない)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseBudget {
    /// この大きさ (バイト) 以上のファイルを予算の対象にする。
    threshold: u64,
    /// 予算の対象のファイルを同時に扱う大きさ (バイト) の合計の上限。
    limit: u64,
}

impl Default for ParseBudget {
    fn default() -> Self {
        Self {
            threshold: LARGE_FILE_THRESHOLD,
            limit: DEFAULT_LIMIT_MIB * MIB,
        }
    }
}

impl ParseBudget {
    /// 既定の予算を [`PARSE_BUDGET_ENV`] で上書きして返す。値が非負の整数 (MiB) でないか、
    /// バイトに直すと溢れるなら、黙って既定に戻さず失敗させる。
    pub fn from_env() -> Result<Self> {
        Self::from_env_value(std::env::var_os(PARSE_BUDGET_ENV).as_deref())
    }

    fn from_env_value(value: Option<&std::ffi::OsStr>) -> Result<Self> {
        let Some(value) = value else {
            return Ok(Self::default());
        };
        let limit = value
            .to_str()
            .filter(|text| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|text| text.parse::<u64>().ok())
            .and_then(|mib| mib.checked_mul(MIB))
            .ok_or_else(|| {
                AstroError::new(
                    ErrorCode::InvalidRequest,
                    format!(
                        "{PARSE_BUDGET_ENV} must be a non-negative integer number of MiB \
                         (0 parses large files one at a time): {}",
                        value.to_string_lossy()
                    ),
                )
            })?;
        Ok(Self {
            limit,
            ..Self::default()
        })
    }

    /// 閾値と予算をバイトで指定する (テスト用)。
    #[cfg(test)]
    pub(crate) const fn new(threshold: u64, limit: u64) -> Self {
        Self { threshold, limit }
    }

    /// 予算の対象にする大きさか。
    pub(crate) fn is_large(&self, size: u64) -> bool {
        size >= self.threshold
    }
}

/// [`BudgetScheduler`] のジョブの終わり方。
pub(crate) enum JobOutcome<J> {
    Done,
    /// 開き直したら予約より大きくなっていた。予約を返し、この大きさで待ち行列に入れ直す。
    Resubmit {
        size: u64,
        job: J,
    },
}

/// 予算に入る大きいジョブだけを同時に実行するスケジューラ (走査 1 回ぶん)。
///
/// ジョブは `rayon::scope` の中で [`Self::submit`] する。予算に入ればその場で予約して spawn し、
/// 入らなければ待ち行列に入れてすぐ戻る (ワーカーを待たせない)。ジョブが終わると予約を返し、
/// 待ち行列を大きい順に見て、入るものをすべて予約して spawn する (先頭が入らなくても後ろの
/// 入るものは起動する)。待ち行列が空でないときは必ず予約中のジョブがあり、その終わりで
/// 取り出されるので、scope が終わる時点で待ち行列は空になる。
///
/// 予算の中 (`run`) で rayon の並列処理を呼んではならない。待ちのジョブを同じスレッドで
/// 実行したとき、予算を持ったまま自分の予約の解放を待つことになる。
pub(crate) struct BudgetScheduler<'a, J> {
    limit: u64,
    run: &'a (dyn Fn(J, u64) -> JobOutcome<J> + Sync),
    state: Mutex<SchedulerState<J>>,
}

struct SchedulerState<J> {
    /// 実行中と、予約して spawn する前のジョブの大きさの合計。
    in_use: u64,
    /// 予算を待つジョブ。大きい順で、同じ大きさは入った順。
    waiting: Vec<(u64, J)>,
}

impl<J> SchedulerState<J> {
    /// ほかに予約が無ければ大きさに関わらず入れる (予算を超える 1 本は単独で実行する)。
    fn admits(&self, size: u64, limit: u64) -> bool {
        self.in_use == 0
            || self
                .in_use
                .checked_add(size)
                .is_some_and(|total| total <= limit)
    }

    fn enqueue(&mut self, size: u64, job: J) {
        let at = self.waiting.partition_point(|(queued, _)| *queued >= size);
        self.waiting.insert(at, (size, job));
    }

    /// 入る待ちのジョブを大きい順にすべて予約して取り出す。
    fn take_admissible(&mut self, limit: u64) -> Vec<(u64, J)> {
        let mut taken = Vec::new();
        let mut at = 0;
        while at < self.waiting.len() {
            if self.admits(self.waiting[at].0, limit) {
                let (size, job) = self.waiting.remove(at);
                self.in_use += size;
                taken.push((size, job));
            } else {
                at += 1;
            }
        }
        taken
    }
}

impl<'a, J: Send + 'a> BudgetScheduler<'a, J> {
    /// `run(job, 予約した大きさ)` でジョブを実行するスケジューラを作る。
    pub(crate) fn new(
        budget: ParseBudget,
        run: &'a (dyn Fn(J, u64) -> JobOutcome<J> + Sync),
    ) -> Self {
        Self {
            limit: budget.limit,
            run,
            state: Mutex::new(SchedulerState {
                in_use: 0,
                waiting: Vec::new(),
            }),
        }
    }

    /// 大きさ `size` のジョブを投入する。予算に入れば spawn し、入らなければ待ち行列に入れる。
    pub(crate) fn submit<'s>(&'s self, scope: &rayon::Scope<'s>, size: u64, job: J)
    where
        'a: 's,
    {
        let mut state = self.lock();
        if state.admits(size, self.limit) {
            state.in_use += size;
            drop(state);
            self.spawn(scope, size, job);
        } else {
            state.enqueue(size, job);
        }
    }

    /// 走査の終わりに、予約と待ち行列が空になったことを確かめる。残っていれば数えていない
    /// ジョブがあるので、件数を返さずに失敗させる。
    pub(crate) fn finish(self) -> Result<()> {
        let state = self
            .state
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        if state.in_use != 0 || !state.waiting.is_empty() {
            return Err(AstroError::new(
                ErrorCode::InternalError,
                format!(
                    "parse budget scheduler finished with {} reserved bytes and {} queued jobs",
                    state.in_use,
                    state.waiting.len()
                ),
            )
            .into());
        }
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, SchedulerState<J>> {
        // ロックの中では利用者のコードを動かさないので、毒は自分の算術の失敗だけ。
        // 解放 (Drop) の中で panic を重ねないよう、毒があっても状態を使い続ける。
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 予約済みのジョブを spawn する。予約はジョブの終わり (panic を含む) に返す。
    fn spawn<'s>(&'s self, scope: &rayon::Scope<'s>, size: u64, job: J)
    where
        'a: 's,
    {
        scope.spawn(move |scope| {
            let mut reservation = Reservation {
                scheduler: self,
                scope,
                size,
                resubmit: None,
            };
            if let JobOutcome::Resubmit { size, job } = (self.run)(job, size) {
                reservation.resubmit = Some((size, job));
            }
        });
    }

    fn release<'s>(&'s self, scope: &rayon::Scope<'s>, size: u64, resubmit: Option<(u64, J)>)
    where
        'a: 's,
    {
        let ready = {
            let mut state = self.lock();
            state.in_use = state.in_use.saturating_sub(size);
            if let Some((size, job)) = resubmit {
                state.enqueue(size, job);
            }
            state.take_admissible(self.limit)
        };
        for (size, job) in ready {
            self.spawn(scope, size, job);
        }
    }
}

/// 実行中のジョブの予約。ジョブが返した (木とソースを捨てた) 後か、panic で巻き戻るときに
/// 予約を返し、待ちのジョブを起動する。
struct Reservation<'r, 's, 'a: 's, J: Send + 'a> {
    scheduler: &'s BudgetScheduler<'a, J>,
    scope: &'r rayon::Scope<'s>,
    size: u64,
    resubmit: Option<(u64, J)>,
}

impl<'a: 's, 's, J: Send + 'a> Drop for Reservation<'_, 's, 'a, J> {
    fn drop(&mut self) {
        self.scheduler
            .release(self.scope, self.size, self.resubmit.take());
    }
}

/// `items` の各項目に `f` を並列に適用し、結果を入力の順に返す。`size` が予算の閾値以上の
/// 項目は [`BudgetScheduler`] で予算に入るぶんだけを同時に実行し、閾値未満はその場で実行する。
///
/// `size` は `f` が読むファイルの大きさ (メタデータの値でよい)。`f` の中で rayon の並列処理を
/// 呼んではならない ([`BudgetScheduler`] と同じ)。スケジューラが壊れた (起こらないはず) ときは
/// `Err` を返し、呼び出し側は予算を掛けずにやり直す。
pub(crate) fn map_with_budget<T, R, S, F>(
    items: &[T],
    budget: ParseBudget,
    size: S,
    f: F,
) -> Result<Vec<R>>
where
    T: Sync,
    R: Send + Sync,
    S: Fn(&T) -> u64 + Sync,
    F: Fn(&T) -> R + Sync,
{
    use rayon::prelude::*;

    let results: Vec<std::sync::OnceLock<R>> = (0..items.len())
        .map(|_| std::sync::OnceLock::new())
        .collect();
    let run = |ix: usize, _reserved: u64| {
        // 1 項目は 1 回しか実行しない (失敗したら下の確かめで分かる)。
        let _ = results[ix].set(f(&items[ix]));
        JobOutcome::Done
    };
    let scheduler = BudgetScheduler::new(budget, &run);
    rayon::scope(|scope| {
        items.par_iter().enumerate().for_each(|(ix, item)| {
            let bytes = size(item);
            if budget.is_large(bytes) {
                scheduler.submit(scope, bytes, ix);
            } else {
                let _ = results[ix].set(f(item));
            }
        });
    });
    scheduler.finish()?;
    results
        .into_iter()
        .map(|cell| cell.into_inner())
        .collect::<Option<Vec<R>>>()
        .ok_or_else(|| {
            AstroError::new(
                ErrorCode::InternalError,
                "parse budget scheduler finished with an item that never ran",
            )
            .into()
        })
}

#[cfg(test)]
mod tests;
