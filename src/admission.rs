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

    /// 枠が空くのを待っている request の数。
    fn waiting(limits: &ReceiveLimits) -> usize {
        limits.inner.waiting.load(Ordering::Acquire)
    }

    /// 巨大な期限でも、受け付け時の期限の計算を溢れさせない (panic させない)。
    ///
    /// `--receive-timeout-ms` は u64 のミリ秒をそのまま受け付けるので、`Instant` への加算が
    /// 溢れる値も渡りうる。溢れたら実質的に期限なしとして扱い、待っている request は
    /// 枠が空けば通す (遠い期限を「期限切れ」と取り違えない)。
    #[tokio::test(start_paused = true)]
    async fn huge_timeouts_do_not_overflow_the_deadline() {
        let one_year = Duration::from_secs(60 * 60 * 24 * 365);
        // `Duration::MAX` は `Instant` への加算が溢れる。`u64::MAX` ミリ秒は CLI で渡せる最大値。
        for timeout in [Duration::MAX, Duration::from_millis(u64::MAX)] {
            let limits = ReceiveLimits::new(1, timeout);
            let deadline = limits.deadline();
            assert!(
                deadline.saturating_duration_since(Instant::now()) >= one_year,
                "{timeout:?} の期限は実質的に期限なしになる"
            );

            let held = limits.admit(deadline).await.unwrap();
            let waiter = {
                let limits = limits.clone();
                tokio::spawn(async move { limits.admit(limits.deadline()).await })
            };
            tokio::task::yield_now().await;
            assert!(!waiter.is_finished(), "{timeout:?}: 枠が空くまで待つ");

            let started = Instant::now();
            drop(held);
            assert!(
                waiter.await.unwrap().is_ok(),
                "{timeout:?}: 枠が空けば待っていた request が通る"
            );
            assert_eq!(started.elapsed(), Duration::ZERO);
        }
    }

    /// 待機の上限ちょうどまでは待たせ、上限 + 1 件目だけを Busy で返す。
    ///
    /// 断った request の分は待機中の件数に残さない。残すと、断るたびに待てる件数が減り、
    /// やがて空きを待つだけで済む request まで Busy になる。
    #[tokio::test(start_paused = true)]
    async fn waiting_limit_admits_exactly_the_limit_and_rejects_one_more() {
        const INFLIGHT: usize = 2;
        let max_waiting = INFLIGHT * WAITING_PER_INFLIGHT;
        let limits = ReceiveLimits::new(INFLIGHT, Duration::from_secs(5));
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut held = Vec::new();
        for _ in 0..INFLIGHT {
            held.push(limits.admit(deadline).await.unwrap());
        }
        assert_eq!(limits.available(), 0);

        let spawn_waiter = || {
            let limits = limits.clone();
            tokio::spawn(async move { limits.admit(deadline).await })
        };
        let mut waiters: Vec<_> = (0..max_waiting).map(|_| spawn_waiter()).collect();
        tokio::task::yield_now().await;
        assert_eq!(waiting(&limits), max_waiting, "上限ちょうどまでは待たせる");
        assert!(waiters.iter().all(|waiter| !waiter.is_finished()));

        for _ in 0..3 {
            assert_eq!(limits.admit(deadline).await.unwrap_err(), Rejected::Busy);
            assert_eq!(
                waiting(&limits),
                max_waiting,
                "断った request は待機中に数えない"
            );
        }

        // 1 件分の枠が空くと、待っていた request の 1 件が通り、待てる件数も 1 件戻る。
        drop(held.pop());
        tokio::task::yield_now().await;
        let finished: Vec<_> = waiters
            .iter()
            .enumerate()
            .filter(|(_, waiter)| waiter.is_finished())
            .map(|(i, _)| i)
            .collect();
        assert_eq!(finished.len(), 1, "枠 1 つで通るのは 1 件だけ");
        let admitted = waiters.remove(finished[0]).await.unwrap();
        assert!(admitted.is_ok());
        assert_eq!(waiting(&limits), max_waiting - 1);

        let late = spawn_waiter();
        tokio::task::yield_now().await;
        assert!(
            !late.is_finished(),
            "空いた待機の枠には、新しい request が Busy にならずに入れる"
        );
        assert_eq!(waiting(&limits), max_waiting);
        for waiter in waiters.into_iter().chain([late]) {
            waiter.abort();
        }
    }

    /// 期限切れと cancel (接続断で handler の future が drop された場合) のどちらでも、
    /// 待機中の件数を戻し、枠を消費しない。
    ///
    /// 件数が戻らないと、待機の上限が埋まったまま以後の request がすべて Busy になる。
    #[tokio::test(start_paused = true)]
    async fn timed_out_or_cancelled_waiters_release_their_waiting_slots() {
        let limits = ReceiveLimits::new(1, Duration::from_millis(200));
        let held = limits.admit(limits.deadline()).await.unwrap();

        let timed_out: Vec<_> = (0..WAITING_PER_INFLIGHT)
            .map(|_| {
                let limits = limits.clone();
                tokio::spawn(async move { limits.admit(limits.deadline()).await })
            })
            .collect();
        for waiter in timed_out {
            assert_eq!(waiter.await.unwrap().unwrap_err(), Rejected::TimedOut);
        }
        assert_eq!(waiting(&limits), 0, "期限切れの request は待機中に残さない");

        let deadline = Instant::now() + Duration::from_secs(60);
        let cancelled: Vec<_> = (0..WAITING_PER_INFLIGHT)
            .map(|_| {
                let limits = limits.clone();
                tokio::spawn(async move { limits.admit(deadline).await })
            })
            .collect();
        tokio::task::yield_now().await;
        assert_eq!(waiting(&limits), WAITING_PER_INFLIGHT);
        for waiter in cancelled {
            waiter.abort();
            assert!(waiter.await.unwrap_err().is_cancelled());
        }
        assert_eq!(
            waiting(&limits),
            0,
            "cancel した request は待機中に残さない"
        );

        // cancel した request は枠を取っていないので、保持中の枠を返せば全部空く。
        drop(held);
        assert_eq!(limits.available(), 1);
        assert!(limits.admit(limits.deadline()).await.is_ok());
    }

    /// 枠は、受け付けた request の clone をすべて drop した時に 1 回だけ戻る。
    ///
    /// handler・保存の task・gRPC の request extension がそれぞれ clone を持つ。どれか 1 つの
    /// drop で戻すと、保存中の request が残ったまま次の request を受け付けて上限を超える。
    /// 逆に clone ごとに戻すと、枠が上限より増える。
    #[tokio::test]
    async fn permit_returns_its_slot_once_after_every_clone_is_dropped() {
        let limits = ReceiveLimits::new(2, Duration::from_secs(5));
        let permit = limits.admit(limits.deadline()).await.unwrap();
        let in_extension = permit.clone();
        let in_save_task = permit.clone();
        assert_eq!(limits.available(), 1);

        drop(permit);
        drop(in_extension);
        assert_eq!(limits.available(), 1, "保存の task が持つ間は戻さない");

        // 保存の task が終わって最後の clone が drop された時に戻る。
        tokio::spawn(async move { drop(in_save_task) })
            .await
            .unwrap();
        assert_eq!(limits.available(), 2, "戻すのは 1 枠だけで、上限を超えない");
    }

    /// 設定で許す最大の同時数でも作れる (`Semaphore::new` の上限ちょうど)。待機の上限の
    /// 計算 (同時数 × 倍率) も溢れさせない。
    #[test]
    fn the_largest_inflight_limit_can_be_constructed() {
        let limits = ReceiveLimits::new(MAX_INFLIGHT_REQUESTS, DEFAULT_RECEIVE_TIMEOUT);
        assert_eq!(limits.available(), MAX_INFLIGHT_REQUESTS);
        assert_eq!(
            limits.inner.max_waiting,
            MAX_INFLIGHT_REQUESTS * WAITING_PER_INFLIGHT
        );
    }
}
