//! 账号级突发限流的排队放行
//!
//! Kiro 按账号限制请求速率（429 + `USER_REQUEST_RATE_EXCEEDED`）：实测一波并发约十几个
//! 请求能直接通过，之后大约每秒补回一个名额。多个客户端共用一个凭据时，被拒的请求如果
//! 各自退避后几乎同时重发，会一起去抢刚补出来的一两个名额，大部分再次 429，
//! 几轮重试用完后整批失败。
//!
//! 这里给每个凭据记一个放行间隔：凭据刚被限流时，发往它的请求按到达顺序领取放行时刻，
//! 相邻两个相隔 `spacing`，于是重发变成逐个出队。再被拒就放大间隔，被接受则逐步缩小；
//! 一段时间没有再被限流且队列已空，就回到不排队的状态。

use parking_lot::Mutex;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// 初始放行间隔，与实测的名额补充速度（约每秒一个）一致
const MIN_SPACING: Duration = Duration::from_millis(1_000);
/// 放行间隔上限
const MAX_SPACING: Duration = Duration::from_millis(8_000);
/// 最后一次被限流后，这么久内仍按间隔放行（让桶里的名额补回来）
const QUIET_PERIOD: Duration = Duration::from_secs(10);

#[derive(Debug)]
struct Gate {
    spacing: Duration,
    /// 队尾：下一个请求最早的放行时刻
    next_release: Instant,
    last_limited: Instant,
    last_widened: Instant,
}

/// 按凭据排队放行被突发限流的请求
#[derive(Debug, Default)]
pub struct BurstPacer {
    gates: Mutex<HashMap<u64, Gate>>,
}

impl BurstPacer {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录凭据 `id` 刚被突发限流，返回队列排空还需多久（至少一个放行间隔）。
    ///
    /// 同一波里同时被拒的请求只算一次：一个间隔内最多放大一次间隔。
    pub fn on_limited(&self, id: u64, now: Instant) -> Duration {
        let mut gates = self.gates.lock();
        let gate = gates.entry(id).or_insert_with(|| Gate {
            spacing: MIN_SPACING,
            next_release: now,
            last_limited: now,
            last_widened: now,
        });
        if now.saturating_duration_since(gate.last_widened) >= gate.spacing {
            gate.spacing = (gate.spacing * 3 / 2).min(MAX_SPACING);
            gate.last_widened = now;
        }
        gate.last_limited = now;
        gate.next_release = gate.next_release.max(now + gate.spacing);
        gate.next_release.saturating_duration_since(now)
    }

    /// 为发往凭据 `id` 的请求领取放行时刻；可以立即发送时返回 `None`。
    pub fn reserve(&self, id: u64, now: Instant) -> Option<Instant> {
        let mut gates = self.gates.lock();
        let gate = gates.get_mut(&id)?;
        if gate.next_release <= now && now.saturating_duration_since(gate.last_limited) >= QUIET_PERIOD
        {
            gates.remove(&id);
            return None;
        }
        let slot = gate.next_release.max(now);
        gate.next_release = slot + gate.spacing;
        (slot > now).then_some(slot)
    }

    /// 凭据 `id` 的请求被上游接受：缩小放行间隔
    pub fn on_accepted(&self, id: u64) {
        if let Some(gate) = self.gates.lock().get_mut(&id) {
            gate.spacing = (gate.spacing * 4 / 5).max(MIN_SPACING);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: f64) -> Duration {
        Duration::from_secs_f64(s)
    }

    #[test]
    fn credentials_never_limited_are_not_paced() {
        let pacer = BurstPacer::new();
        let now = Instant::now();
        assert_eq!(pacer.reserve(1, now), None);
        pacer.on_limited(2, now);
        assert_eq!(pacer.reserve(1, now), None, "其它凭据不受影响");
    }

    #[test]
    fn rejected_wave_is_released_one_per_spacing_in_arrival_order() {
        let pacer = BurstPacer::new();
        let t0 = Instant::now();
        // 同一波 5 个请求同时被拒：只算一次，间隔保持 1s
        for _ in 0..5 {
            assert_eq!(pacer.on_limited(7, t0), MIN_SPACING);
        }
        let slots: Vec<_> = (0..5).map(|_| pacer.reserve(7, t0).unwrap()).collect();
        let expected: Vec<_> = (1..=5).map(|k| t0 + MIN_SPACING * k).collect();
        assert_eq!(slots, expected);
    }

    #[test]
    fn repeated_limits_widen_spacing_up_to_cap_and_success_narrows_it() {
        let pacer = BurstPacer::new();
        let t0 = Instant::now();
        pacer.on_limited(1, t0);
        // 一个间隔后再被拒：放大 1.5 倍
        let mut now = t0 + secs(1.0);
        pacer.on_limited(1, now);
        assert_eq!(pacer.gates.lock()[&1].spacing, secs(1.5));
        for _ in 0..10 {
            now += MAX_SPACING;
            pacer.on_limited(1, now);
        }
        assert_eq!(pacer.gates.lock()[&1].spacing, MAX_SPACING);

        for _ in 0..20 {
            pacer.on_accepted(1);
        }
        assert_eq!(pacer.gates.lock()[&1].spacing, MIN_SPACING);
    }

    #[test]
    fn limit_pushes_queue_tail_and_reports_drain_time() {
        let pacer = BurstPacer::new();
        let t0 = Instant::now();
        pacer.on_limited(1, t0);
        for _ in 0..3 {
            pacer.reserve(1, t0);
        }
        // 队尾在 t0+4s；半秒后又有请求被拒（间隔未到，不放大）：返回排空时间 3.5s
        assert_eq!(pacer.on_limited(1, t0 + secs(0.5)), secs(3.5));
        assert_eq!(pacer.reserve(1, t0 + secs(0.5)), Some(t0 + secs(4.0)));
    }

    #[test]
    fn keeps_spacing_during_quiet_period_then_stops_pacing() {
        let pacer = BurstPacer::new();
        let t0 = Instant::now();
        pacer.on_limited(1, t0);
        assert_eq!(pacer.reserve(1, t0), Some(t0 + MIN_SPACING));

        // 队列已排空但还在静默期：本请求立即放行，下一个仍要隔一个间隔
        let t = t0 + secs(5.0);
        assert_eq!(pacer.reserve(1, t), None);
        assert_eq!(pacer.reserve(1, t), Some(t + MIN_SPACING));

        // 静默期过后且队列排空：不再排队，条目被清理
        let later = t0 + QUIET_PERIOD + secs(5.0);
        assert_eq!(pacer.reserve(1, later), None);
        assert_eq!(pacer.reserve(1, later), None);
        assert!(pacer.gates.lock().is_empty());
    }
}
