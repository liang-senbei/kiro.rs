//! 各凭据最近一次查到的 Kiro 余额（`/v1/usage` 汇总池子用）
//!
//! Admin 服务每次改动余额缓存时同步过来，`/v1/usage` 只读这张表，不必依赖 Admin 服务本身。
//!
//! 和 Admin 缓存不同，这里的条目只在凭据删除时移除：Admin 作废某个账号的缓存（切换超额、
//! 重新登录）只是要求下次重新查，上次查到的数仍是这个账号最近的已知余额，池子不该因此变小
//! 或查不到。为此单独落盘（`kiro_pool_balance.json`），重启后也还在。
//!
//! 只在启用 Admin API 时存在（余额后台刷新随 Admin 服务启动）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};

/// 一个凭据的余额
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardEntry {
    pub remaining: f64,
    pub usage_limit: f64,
    pub current_usage: f64,
    /// 下次重置时间（Unix 秒）
    #[serde(default)]
    pub next_reset_at: Option<f64>,
    /// 查到这份余额的时间（Unix 秒）
    pub cached_at: f64,
}

#[derive(Debug, Default)]
pub struct BalanceBoard {
    entries: RwLock<HashMap<u64, BoardEntry>>,
    /// 落盘文件；持有这把锁时写文件，同一文件的写入因此串行
    file: Mutex<Option<PathBuf>>,
}

pub type SharedBalanceBoard = Arc<BalanceBoard>;

impl BalanceBoard {
    /// 指定落盘文件，并读入上次保存的余额（文件不存在或损坏时从空表开始）
    pub fn attach_file(&self, path: PathBuf) {
        let loaded = load(&path);
        {
            let mut board = self.entries.write();
            for (id, entry) in loaded {
                board.entry(id).or_insert(entry);
            }
        }
        *self.file.lock() = Some(path);
    }

    /// 用 Admin 余额缓存的当前内容更新：缓存里有的凭据以缓存为准，缓存里没有的保留上次的数。
    ///
    /// 调用方须持有 Admin 缓存锁，让更新按缓存改动的先后应用。不比较查到的时间：
    /// 系统时钟回拨后新查到的余额时间戳可能更小，照样要生效。
    pub fn publish(&self, entries: impl IntoIterator<Item = (u64, BoardEntry)>) {
        let changed = {
            let mut board = self.entries.write();
            let mut changed = false;
            for (id, entry) in entries {
                if board.get(&id) != Some(&entry) {
                    board.insert(id, entry);
                    changed = true;
                }
            }
            changed
        };
        if changed {
            self.save();
        }
    }

    /// 凭据删除后移除它的余额
    pub fn forget(&self, id: u64) {
        let removed = self.entries.write().remove(&id).is_some();
        if removed {
            self.save();
        }
    }

    pub fn snapshot(&self) -> HashMap<u64, BoardEntry> {
        self.entries.read().clone()
    }

    fn save(&self) {
        let file = self.file.lock();
        let Some(path) = file.as_ref() else {
            return;
        };
        // 在文件锁内取快照：后写的一定是更新的内容
        let map: HashMap<String, BoardEntry> = self
            .entries
            .read()
            .iter()
            .map(|(id, entry)| (id.to_string(), entry.clone()))
            .collect();
        let result = serde_json::to_vec_pretty(&map)
            .map_err(std::io::Error::other)
            .and_then(|json| crate::common::fs::write_atomic(path, json));
        if let Err(e) = result {
            tracing::warn!("保存池子余额失败: {}", e);
        }
    }
}

fn load(path: &Path) -> HashMap<u64, BoardEntry> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    match serde_json::from_str::<HashMap<String, BoardEntry>>(&content) {
        Ok(map) => map
            .into_iter()
            .filter_map(|(id, entry)| Some((id.parse().ok()?, entry)))
            .collect(),
        Err(e) => {
            tracing::warn!("解析池子余额失败，将忽略: {}", e);
            HashMap::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试一个独立文件，测试结束删掉
    struct TempFile(PathBuf);

    impl TempFile {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "kiro_test_pool_balance_{}_{}.json",
                name,
                std::process::id()
            ));
            let _ = std::fs::remove_file(&path);
            Self(path)
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

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
    fn publish_keeps_entries_missing_from_the_update() {
        let board = BalanceBoard::default();
        board.publish([(1, entry(40.0, 10.0)), (2, entry(30.0, 10.0))]);
        // Admin 作废了 #1 的缓存：池子里仍用上次查到的数
        board.publish([(2, entry(20.0, 20.0))]);
        let snap = board.snapshot();
        assert_eq!(snap[&1].remaining, 40.0);
        assert_eq!(snap[&2].remaining, 20.0);
    }

    #[test]
    fn publish_applies_the_latest_update_even_if_the_clock_went_back() {
        let board = BalanceBoard::default();
        board.publish([(1, entry(10.0, 2_000.0))]);
        board.publish([(1, entry(40.0, 1_000.0))]);
        assert_eq!(board.snapshot()[&1].remaining, 40.0);
    }

    #[test]
    fn forget_removes_a_deleted_credential() {
        let board = BalanceBoard::default();
        board.publish([(1, entry(10.0, 1.0))]);
        board.forget(1);
        assert!(board.snapshot().is_empty());
    }

    #[test]
    fn survives_a_restart_through_its_own_file() {
        let file = TempFile::new("restart");
        let path = file.0.clone();

        let board = BalanceBoard::default();
        board.attach_file(path.clone());
        board.publish([(1, entry(40.0, 10.0)), (2, entry(30.0, 10.0))]);
        board.forget(2);

        // 重启：Admin 缓存里已经没有 #1（被作废），池子仍从自己的文件拿到它
        let restarted = BalanceBoard::default();
        restarted.attach_file(path);
        let snap = restarted.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[&1], entry(40.0, 10.0));
    }

    #[test]
    fn a_corrupt_file_starts_empty() {
        let file = TempFile::new("corrupt");
        std::fs::write(&file.0, "not json").unwrap();
        let board = BalanceBoard::default();
        board.attach_file(file.0.clone());
        assert!(board.snapshot().is_empty());
    }
}
