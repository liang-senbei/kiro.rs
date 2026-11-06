//! 各凭据最近一次查到的 Kiro 余额
//!
//! Admin 服务每次更新余额缓存时整份同步过来，Anthropic 路由的 `/v1/usage`
//! 只读它来汇总池子剩余积分，不必依赖 Admin 服务本身。

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
    /// 用 Admin 余额缓存的最新内容整份替换
    pub fn replace_all(&self, entries: HashMap<u64, BoardEntry>) {
        *self.entries.write() = entries;
    }

    pub fn snapshot(&self) -> HashMap<u64, BoardEntry> {
        self.entries.read().clone()
    }
}
