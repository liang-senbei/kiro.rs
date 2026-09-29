//! 文件系统工具函数

use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// 原子写文件：先写同目录临时文件再 rename 覆盖目标。
///
/// `std::fs::write` 会先截断再写入，进程若在两步之间被杀（OOM / `docker kill`），
/// 目标文件会变成空文件或半截 JSON，下次启动时状态丢失或加载失败。同目录 rename 是原子的，
/// 读者只会看到旧内容或新内容。（未 fsync，不承诺掉电后的持久性。）
///
/// rename 替换的是目录项，为了不改变目标文件原有的属性：
/// - 目标是符号链接时替换链接指向的真实文件，链接本身保留；
/// - 临时文件在写入内容前就对齐目标的属主与权限位（如含密钥的 0600 配置）；
/// - 目标有多个硬链接、不是普通文件或当前用户不可写时不走 rename，直接写入，
///   避免拆散硬链接或绕过只读保护。
///
/// 临时文件或 rename 失败时（如目标是单文件 bind mount、Windows 下文件被占用、目录不可写、
/// 无权 chown）回退为直接写入，保证不比原先的 `std::fs::write` 更差；但磁盘满 / 配额超限时
/// 不回退——直接写入会先截断目标再写不完，反而毁掉旧内容，此时返回错误、保留旧文件。
///
/// 临时文件名固定（与 token_manager 回写凭据的做法一致），同一路径的写入需由调用方串行化。
pub fn write_atomic(path: &Path, contents: impl AsRef<[u8]>) -> io::Result<()> {
    let contents = contents.as_ref();
    let Some((target, meta)) = replace_target(path) else {
        return fs::write(path, contents);
    };
    let tmp = tmp_path(&target);
    let atomic = write_tmp(&tmp, contents, meta.as_ref()).and_then(|()| fs::rename(&tmp, &target));
    match atomic {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            if is_out_of_space(&e) {
                return Err(e);
            }
            tracing::debug!("原子写入 {} 失败（{}），回退为直接写入", path.display(), e);
            fs::write(path, contents)
        }
    }
}

/// 确定 rename 要替换的真实文件及其元数据（新文件为 `None`）。
/// 返回 `None` 表示不宜走 rename，应直接写入。
fn replace_target(path: &Path) -> Option<(PathBuf, Option<Metadata>)> {
    let meta = match fs::symlink_metadata(path) {
        // rename 会替换链接本身，改为替换链接指向的文件；悬空链接交给直接写入
        Ok(m) if m.file_type().is_symlink() => {
            let real = fs::canonicalize(path).ok()?;
            let meta = fs::metadata(&real).ok()?;
            return replaceable(&meta, &real).then_some((real, Some(meta)));
        }
        Ok(m) => m,
        // 不存在（或无法 stat）按新文件处理，真正的错误交给写入时报告
        Err(_) => return Some((path.to_path_buf(), None)),
    };
    replaceable(&meta, path).then(|| (path.to_path_buf(), Some(meta)))
}

/// 普通文件、无其他硬链接、且当前用户可写（rename 只需目录写权限，会绕过目标的只读保护）
fn replaceable(meta: &Metadata, path: &Path) -> bool {
    meta.is_file() && !has_other_links(meta) && OpenOptions::new().write(true).open(path).is_ok()
}

/// 写临时文件。替换已有文件时，在写入内容前对齐属主与权限位，避免密钥内容短暂以默认权限落盘。
fn write_tmp(tmp: &Path, contents: &[u8], meta: Option<&Metadata>) -> io::Result<()> {
    // 清掉崩溃残留，确保 create_new 拿到的是本次新建、权限可控的文件
    let _ = fs::remove_file(tmp);
    let mut file = create_tmp(tmp, meta)?;
    file.write_all(contents)
}

#[cfg(unix)]
fn create_tmp(tmp: &Path, meta: Option<&Metadata>) -> io::Result<File> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    let Some(meta) = meta else {
        return options.open(tmp);
    };
    let file = options.mode(0o600).open(tmp)?;
    let created = file.metadata()?;
    if (created.uid(), created.gid()) != (meta.uid(), meta.gid()) {
        std::os::unix::fs::fchown(&file, Some(meta.uid()), Some(meta.gid()))?;
    }
    // chown 会清除 setuid/setgid 位，权限放在属主之后设置
    file.set_permissions(meta.permissions())?;
    Ok(file)
}

#[cfg(not(unix))]
fn create_tmp(tmp: &Path, _meta: Option<&Metadata>) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(tmp)
}

/// rename 会让其他硬链接继续指向旧内容
#[cfg(unix)]
fn has_other_links(meta: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    meta.nlink() > 1
}

#[cfg(not(unix))]
fn has_other_links(_meta: &Metadata) -> bool {
    false
}

/// 磁盘满 / 配额超限：直接写入同样写不完，还会先截断目标，不能作为回退
fn is_out_of_space(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded
    )
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
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

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

    #[test]
    fn out_of_space_is_not_retried_as_direct_write() {
        assert!(is_out_of_space(&io::Error::from(
            io::ErrorKind::StorageFull
        )));
        assert!(is_out_of_space(&io::Error::from(
            io::ErrorKind::QuotaExceeded
        )));
        assert!(!is_out_of_space(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));
        // 真实 syscall 返回的 ENOSPC 也要命中
        #[cfg(target_os = "linux")]
        assert!(is_out_of_space(&io::Error::from_raw_os_error(28)));
    }

    #[cfg(unix)]
    #[test]
    fn keeps_permission_bits() {
        let dir = test_dir("mode");
        let path = dir.join("config.json");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let ino = std::fs::metadata(&path).unwrap().ino();

        write_atomic(&path, "new").unwrap();

        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert_ne!(meta.ino(), ino, "应经 rename 替换而非原地写");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn writes_through_symlink() {
        let dir = test_dir("symlink");
        let real = dir.join("real.json");
        let link = dir.join("config.json");
        std::fs::write(&real, "old").unwrap();
        std::os::unix::fs::symlink("real.json", &link).unwrap();

        write_atomic(&link, "new").unwrap();

        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "new");
        assert!(!tmp_path(&real).exists() && !tmp_path(&link).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn keeps_hard_links_in_sync() {
        let dir = test_dir("hardlink");
        let a = dir.join("a.json");
        let b = dir.join("b.json");
        std::fs::write(&a, "old").unwrap();
        std::fs::hard_link(&a, &b).unwrap();

        write_atomic(&a, "new").unwrap();

        assert_eq!(std::fs::read_to_string(&b).unwrap(), "new");
        let (ma, mb) = (
            std::fs::metadata(&a).unwrap(),
            std::fs::metadata(&b).unwrap(),
        );
        assert_eq!(ma.ino(), mb.ino());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn respects_read_only_target() {
        let dir = test_dir("readonly");
        let path = dir.join("config.json");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
        // root 无视权限位，此时与 std::fs::write 一样可写，只校验权限保留
        let writable = std::fs::OpenOptions::new().write(true).open(&path).is_ok();

        let result = write_atomic(&path, "new");

        if writable {
            result.unwrap();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        } else {
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");
        }
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o400);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 需要 root（普通用户无权把文件交给他人），否则跳过
    #[cfg(unix)]
    #[test]
    fn keeps_owner_when_privileged() {
        let dir = test_dir("owner");
        let path = dir.join("config.json");
        std::fs::write(&path, "old").unwrap();
        if std::os::unix::fs::chown(&path, Some(65534), Some(65534)).is_err() {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let ino = std::fs::metadata(&path).unwrap().ino();

        write_atomic(&path, "new").unwrap();

        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!((meta.uid(), meta.gid()), (65534, 65534));
        assert_eq!(meta.permissions().mode() & 0o777, 0o640);
        assert_ne!(meta.ino(), ino, "应经 rename 替换而非原地写");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
