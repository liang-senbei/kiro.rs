//! 各凭据最近一次查到的 Kiro 余额
//!
//! Admin 服务每次更新余额缓存时同步过来，Anthropic 路由的 `/v1/usage`
//! 只读它来汇总池子剩余积分，不必依赖 Admin 服务本身。只在启用 Admin API 时存在
//! （余额后台刷新随 Admin 服务启动）。

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

/// 一个凭据的余额
#[derive(Debug, Clone, PartialEq)]
pub struct BoardEntry {
    pub remaining: f64,
    pub usage_limit: f64,
    pub current_usage: f64,
    /// 下次重置时间（Unix 秒）
    pub next_reset_at: Option<f64>,
    /// 查到这份余额的时间（Unix 秒）
    pub cached_at: f64,
}

#[derive(Debug, Default)]
pub struct BalanceBoard {
    entries: RwLock<HashMap<u64, BoardEntry>>,
}

pub type SharedBalanceBoard = Arc<BalanceBoard>;

impl BalanceBoard {
    /// 用 Admin 余额缓存的内容更新，同一凭据保留查到时间较新的那份。
    ///
    /// Admin 缓存里被作废（删掉）的条目这里不删：作废只是让 Admin 下次重新查，
    /// 上次查到的数仍是这个账号最近的已知余额，池子不该因此变小或查不到。
    /// 凭据删除时用 [`Self::forget`]。
    pub fn merge(&self, entries: HashMap<u64, BoardEntry>) {
        let mut board = self.entries.write();
        for (id, entry) in entries {
            match board.get(&id) {
                Some(current) if current.cached_at > entry.cached_at => {}
                _ => {
                    board.insert(id, entry);
                }
            }
        }
    }

    /// 凭据删除后移除它的余额
    pub fn forget(&self, id: u64) {
        self.entries.write().remove(&id);
    }

    pub fn snapshot(&self) -> HashMap<u64, BoardEntry> {
        self.entries.read().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(remaining: f64, cached_at: f64) -> BoardEntry {
        BoardEntry {
            remaining,
            usage_limit: 50.0,
            current_usage: 50.0 - remaining,
            next_reset_at: None,
            cached_at,
        }
    }

    #[test]
    fn merge_keeps_entries_missing_from_the_update() {
        let board = BalanceBoard::default();
        board.merge(HashMap::from([
            (1, entry(40.0, 10.0)),
            (2, entry(30.0, 10.0)),
        ]));
        // Admin 作废了 #1 的缓存：池子里仍用上次查到的数
        board.merge(HashMap::from([(2, entry(20.0, 20.0))]));
        let snap = board.snapshot();
        assert_eq!(snap[&1].remaining, 40.0);
        assert_eq!(snap[&2].remaining, 20.0);
    }

    #[test]
    fn merge_never_goes_back_to_an_older_balance() {
        let board = BalanceBoard::default();
        board.merge(HashMap::from([(1, entry(10.0, 20.0))]));
        board.merge(HashMap::from([(1, entry(40.0, 10.0))]));
        assert_eq!(board.snapshot()[&1].remaining, 10.0);
    }

    #[test]
    fn forget_removes_a_deleted_credential() {
        let board = BalanceBoard::default();
        board.merge(HashMap::from([(1, entry(10.0, 1.0))]));
        board.forget(1);
        assert!(board.snapshot().is_empty());
    }
}
