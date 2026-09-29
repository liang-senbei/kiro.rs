//! 文件系统工具函数

use std::io;
use std::path::{Path, PathBuf};

/// 原子写文件：先写同目录临时文件再 rename 覆盖目标。
///
/// `std::fs::write` 会先截断再写入，进程若在两步之间被杀（OOM / 断电 / `docker kill`），
/// 目标文件会变成空文件或半截 JSON，下次启动时状态丢失或加载失败。同目录 rename 是原子的，
/// 读者只会看到旧内容或新内容。
///
/// 临时文件或 rename 失败时（如目标是单文件 bind mount、Windows 下文件被占用、目录不可写）
/// 回退为直接写入，保证不比原先的 `std::fs::write` 更差。
///
/// 临时文件名固定（与 token_manager 回写凭据的做法一致），同一路径的写入需由调用方串行化。
pub fn write_atomic(path: &Path, contents: impl AsRef<[u8]>) -> io::Result<()> {
    let contents = contents.as_ref();
    let tmp = tmp_path(path);
    let atomic = std::fs::write(&tmp, contents).and_then(|()| std::fs::rename(&tmp, path));
    match atomic {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            tracing::debug!("原子写入 {} 失败（{}），回退为直接写入", path.display(), e);
            std::fs::write(path, contents)
        }
    }
}

/// `foo.json` → `foo.json.tmp`（与目标同目录，保证 rename 不跨文件系统）
fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kiro_test_write_atomic_{}_{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn replaces_content_and_leaves_no_tmp() {
        let dir = test_dir("replace");
        let path = dir.join("state.json");
        std::fs::write(&path, "old-content-longer-than-new").unwrap();

        write_atomic(&path, "new").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        assert!(!tmp_path(&path).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn falls_back_to_direct_write_when_tmp_unusable() {
        let dir = test_dir("fallback");
        let path = dir.join("state.json");
        // 用同名目录占住临时文件路径，迫使原子路径失败
        std::fs::create_dir_all(tmp_path(&path)).unwrap();

        write_atomic(&path, "fallback").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fallback");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
