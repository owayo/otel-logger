//! OTLP の受信を、プロセス全体で同時に扱う件数と、受け付けからの期限で制限する。
//!
//! 1 リクエストの上限 (`OTLP_MAX_REQUEST_BYTES`) だけでは総メモリを抑えられない。
//! gzip の body は展開後に最大 32MiB まで溜まるため、終端を送らずに止めた接続を並べるだけで、
//! 数 KB の送信から数百 MB を抱えさせられた (レビューでの実測: 8 接続・合計 245KB で RSS が
//! 15MB から 264MB)。既定の bind は 0.0.0.0 なので外部から誘発できる。
//!
//! HTTP と gRPC、全 signal で 1 つの上限を共有し、body を読む前 (gRPC は decode の前) に
//! 枠を取る。枠は保存と集計が終わるまで持つ。上限に達したら、すぐには拒否せず期限まで待つ。
//! 1 batch の処理は数 ms で、Codex は終了時に複数のプロセスから logs / traces / metrics を
//! 並行して送るため、瞬間的な重なりで拒否すると、retry しない exporter では欠測になる。
//! 期限を過ぎた分と、待機の上限を超えた分だけを retryable な status (HTTP 503 /
//! gRPC Unavailable) で返す。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

/// 同時に処理する request 数の既定値 (HTTP / gRPC、全 signal の合計)。
pub const DEFAULT_MAX_INFLIGHT_REQUESTS: usize = 8;

/// 同時に処理する request 数として設定できる上限 (`Semaphore::new` が受け付ける最大値)。
pub const MAX_INFLIGHT_REQUESTS: usize = Semaphore::MAX_PERMITS;

/// 受け付けから body を読み終えるまでの既定の期限。OTLP exporter の既定 timeout は
/// export 全体で 10 秒なので、半分で読み終え、残りを decode・保存・応答に回す。
pub const DEFAULT_RECEIVE_TIMEOUT: Duration = Duration::from_secs(5);

/// 処理中の枠が空くのを待てる request 数の、処理中の上限に対する倍率。待っている request は
/// まだ body を読んでいないので軽いが、件数を限らないと task と接続が積み上がる。
const WAITING_PER_INFLIGHT: usize = 8;

/// 受信の枠を配る。clone は軽く、HTTP と gRPC で同じものを共有する。
#[derive(Clone, Debug)]
pub struct ReceiveLimits {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    slots: Arc<Semaphore>,
    /// 枠が空くのを待っている request の数。
    waiting: AtomicUsize,
    max_waiting: usize,
    timeout: Duration,
}

/// 受け付けた request が持つ枠。clone を全部 drop した時に枠を返す。
///
/// handler が cancel されても保存の task が clone を持つので、保存が終わるまで枠は戻らない。
/// 戻すと、cancel を繰り返して上限を超える数の保存を同時に走らせられる。
#[derive(Clone, Debug)]
pub struct ReceivePermit {
    _permit: Arc<OwnedSemaphorePermit>,
}

/// 枠を渡せなかった理由。どちらも retryable な status で返す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejected {
    /// 待機の上限を超えた。
    Busy,
    /// 期限までに枠が空かなかった。
    TimedOut,
}

impl Rejected {
    /// 応答に載せる理由。
    pub fn message(self) -> &'static str {
        match self {
            Self::Busy => "receiver is busy; retry later",
            Self::TimedOut => "receiver is busy; timed out waiting for a free slot",
        }
    }
}

impl ReceiveLimits {
    /// `max_inflight` は 1 以上、`Semaphore::MAX_PERMITS` 以下であること (設定の検証で保証する)。
    pub fn new(max_inflight: usize, timeout: Duration) -> Self {
        Self {
            inner: Arc::new(Inner {
                slots: Arc::new(Semaphore::new(max_inflight)),
                waiting: AtomicUsize::new(0),
                max_waiting: max_inflight.saturating_mul(WAITING_PER_INFLIGHT),
                timeout,
            }),
        }
    }

    /// 受け付け時点からの期限。枠の待機と body の読み取りで共有する。
    pub fn deadline(&self) -> Instant {
        // 巨大な設定値で Instant の加算が溢れても panic させない (実質的に期限なし)。
        Instant::now()
            .checked_add(self.inner.timeout)
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(60 * 60 * 24 * 365))
    }

    /// 枠を取る。空いていなければ `deadline` まで待つ。
    pub async fn admit(&self, deadline: Instant) -> Result<ReceivePermit, Rejected> {
        if let Ok(permit) = Arc::clone(&self.inner.slots).try_acquire_owned() {
            return Ok(ReceivePermit {
                _permit: Arc::new(permit),
            });
        }
        let _waiting = WaitingGuard::enter(&self.inner)?;
        match tokio::time::timeout_at(deadline, Arc::clone(&self.inner.slots).acquire_owned()).await
        {
            Ok(Ok(permit)) => Ok(ReceivePermit {
                _permit: Arc::new(permit),
            }),
            // Semaphore は close しないので来ないが、来ても待たずに返す。
            Ok(Err(_)) => Err(Rejected::Busy),
            Err(_) => Err(Rejected::TimedOut),
        }
    }

    /// 今すぐ使える枠の数 (テスト用)。
    #[cfg(test)]
    fn available(&self) -> usize {
        self.inner.slots.available_permits()
    }
}

/// 待機中の件数を数え、drop で戻す。上限を超えたら入れない。
struct WaitingGuard<'a> {
    inner: &'a Inner,
}

impl<'a> WaitingGuard<'a> {
    fn enter(inner: &'a Inner) -> Result<Self, Rejected> {
        let waiting = inner.waiting.fetch_add(1, Ordering::AcqRel) + 1;
        let guard = Self { inner };
        if waiting > inner.max_waiting {
            return Err(Rejected::Busy);
        }
        Ok(guard)
    }
}

impl Drop for WaitingGuard<'_> {
    fn drop(&mut self) {
        self.inner.waiting.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 上限まではすぐ通し、超えた分は枠が空くまで待たせる。全 clone を drop した時に枠が戻る。
    #[tokio::test]
    async fn waits_for_a_free_slot_and_releases_it_when_every_clone_is_dropped() {
        let limits = ReceiveLimits::new(1, Duration::from_secs(5));
        let first = limits.admit(limits.deadline()).await.unwrap();
        let held_by_task = first.clone();
        assert_eq!(limits.available(), 0);

        let waiter = {
            let limits = limits.clone();
            tokio::spawn(async move { limits.admit(limits.deadline()).await })
        };
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished(), "枠が空くまで待つ");

        drop(first);
        tokio::task::yield_now().await;
        assert!(
            !waiter.is_finished(),
            "保存の task が clone を持つ間は枠を返さない"
        );
        drop(held_by_task);
        let second = waiter.await.unwrap();
        assert!(second.is_ok(), "枠が空けば待っていた request が通る");
    }

    /// 期限までに枠が空かなければ TimedOut、待機の上限を超えたら Busy で返す。
    #[tokio::test(start_paused = true)]
    async fn rejects_after_the_deadline_or_beyond_the_waiting_limit() {
        let limits = ReceiveLimits::new(1, Duration::from_secs(5));
        let _held = limits.admit(limits.deadline()).await.unwrap();

        let started = Instant::now();
        assert_eq!(
            limits.admit(limits.deadline()).await.unwrap_err(),
            Rejected::TimedOut
        );
        assert_eq!(started.elapsed(), Duration::from_secs(5));

        // 待機の上限 (1 × WAITING_PER_INFLIGHT) を埋めると、次はすぐ Busy になる。
        let deadline = Instant::now() + Duration::from_secs(60);
        let waiters: Vec<_> = (0..WAITING_PER_INFLIGHT)
            .map(|_| {
                let limits = limits.clone();
                tokio::spawn(async move { limits.admit(deadline).await })
            })
            .collect();
        tokio::task::yield_now().await;
        let started = Instant::now();
        assert_eq!(limits.admit(deadline).await.unwrap_err(), Rejected::Busy);
        assert_eq!(started.elapsed(), Duration::ZERO, "上限を超えたら待たない");
        for waiter in waiters {
            waiter.abort();
        }
    }
}
