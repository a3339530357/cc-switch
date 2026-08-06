//! WSL 发行版内 OpenCode 数据库的发现与本地暂存
//!
//! ## 背景
//!
//! cc-switch 以 Windows 桌面程序运行时，opencode 可能跑在 WSL 发行版里。
//! 它的 SQLite 库位于 WSL 的 ext4 虚拟磁盘上
//! （`/home/<user>/.local/share/opencode/opencode.db`），
//! 与 Windows 侧 `C:\Users\<user>\...` 互不相通，因此默认同步扫不到。
//!
//! ## 方案
//!
//! 1. `wsl.exe --list --quiet` 枚举已安装发行版（不会唤醒发行版）；
//! 2. 通过 UNC 路径 `\\wsl$\<distro>\home\...` 或 `\\wsl.localhost\<distro>\home\...`
//!    探测发行版内的用户主目录（发行版未运行则直接跳过，此时也不会有 opencode 活动）；
//! 3. 找到 `opencode.db` 后，把主库与 `-wal` 复制到
//!    `~/.cc-switch/wsl-opencode/<distro>/` 本地缓存，再交给 SQLite 只读打开。
//!    避免在 9P/UNC 文件系统上直接打开 WAL 库带来的锁与读写问题。
//!
//! 同步水位记录的是远端文件的 mtime，因此远端无变化时不会重复复制。
//!
//! 与 `wsl.exe` 打交道的底层原语（发行版枚举、输出解码、UNC 根探测）在
//! [`crate::services::wsl`]，本模块只保留 opencode 特有的发现与暂存逻辑。

#[cfg(target_os = "windows")]
use crate::config::get_app_config_dir;
#[cfg(target_os = "windows")]
use crate::error::AppError;
#[cfg(target_os = "windows")]
use std::path::{Path, PathBuf};

/// 一个可同步的 WSL opencode 数据源。
#[cfg(target_os = "windows")]
#[derive(Debug, Clone)]
pub struct WslOpencodeSource {
    pub distro: String,
    /// 远端 opencode.db 的 UNC 路径
    pub remote_db_path: PathBuf,
    /// 远端 opencode.db（含 -wal）的最新 mtime，纳秒
    pub remote_modified_nanos: i64,
}

/// 发现所有可用的 WSL opencode 数据源。
///
/// 仅在 Windows 上启用：`wsl.exe --list --quiet` 失败（WSL 未安装）时返回空列表。
#[cfg(target_os = "windows")]
pub fn discover_sources() -> Vec<WslOpencodeSource> {
    use crate::services::wsl::{first_readable_unc_root, list_distros};

    let mut sources = Vec::new();
    for distro in list_distros() {
        // 先取能访问的 UNC 根（`\\wsl$\` 最兼容，`\\wsl.localhost\` 兜底）。
        // 发行版未运行时 UNC 访问会失败，直接跳过——此时 distro 里也不会有活动会话。
        let Some(unc_root) = first_readable_unc_root(&distro) else {
            continue;
        };

        // 枚举 /home 下的用户主目录
        let mut candidates = Vec::new();
        if let Ok(entries) = std::fs::read_dir(unc_root.join("home")) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    candidates.push(path.join(".local").join("share").join("opencode"));
                }
            }
        }
        // root 用户
        candidates.push(
            unc_root
                .join("root")
                .join(".local")
                .join("share")
                .join("opencode"),
        );

        for data_dir in candidates {
            let remote_db_path = data_dir.join("opencode.db");
            if !remote_db_path.exists() {
                continue;
            }
            if let Some(modified) = remote_modified_nanos(&remote_db_path) {
                sources.push(WslOpencodeSource {
                    distro: distro.clone(),
                    remote_db_path,
                    remote_modified_nanos: modified,
                });
                // 每个发行版只取第一个命中的用户
                break;
            }
        }
    }
    sources
}

/// 远端 opencode.db 的最新 mtime（纳秒），与 `-wal` 取较大值。
/// WAL 模式下新提交先落在 `-wal`，主库要等 checkpoint 才更新。
#[cfg(target_os = "windows")]
fn remote_modified_nanos(db_path: &Path) -> Option<i64> {
    use crate::services::session_usage::metadata_modified_nanos;

    let mut m = std::fs::metadata(db_path)
        .ok()
        .map(|md| metadata_modified_nanos(&md))?;
    if let Ok(wal_meta) = std::fs::metadata(db_path.with_extension("db-wal")) {
        m = m.max(metadata_modified_nanos(&wal_meta));
    }
    Some(m)
}

/// 把 WSL 内的 opencode 数据库（含 -wal）复制到本地缓存，返回本地库路径。
///
/// 不复制 `-shm`：它是进程内共享内存映射，交给 SQLite 打开时按 WAL 自行重建即可。
/// 复制前先清掉本地旧副本的 `-wal` / `-shm`，避免远端已 checkpoint 而本地残留旧 WAL，
/// 导致旧 WAL 被回放到更新的主库上。
#[cfg(target_os = "windows")]
pub fn stage_source(source: &WslOpencodeSource) -> Result<PathBuf, AppError> {
    let cache_dir = get_app_config_dir()
        .join("wsl-opencode")
        .join(&source.distro);
    std::fs::create_dir_all(&cache_dir).map_err(|e| AppError::io(&cache_dir, e))?;

    let local_db = cache_dir.join("opencode.db");
    let local_wal = cache_dir.join("opencode.db-wal");
    let local_shm = cache_dir.join("opencode.db-shm");

    let _ = std::fs::remove_file(&local_wal);
    let _ = std::fs::remove_file(&local_shm);

    std::fs::copy(&source.remote_db_path, &local_db)
        .map_err(|e| AppError::io(&source.remote_db_path, e))?;

    let remote_wal = source.remote_db_path.with_extension("db-wal");
    if remote_wal.exists() {
        std::fs::copy(&remote_wal, &local_wal).map_err(|e| AppError::io(&remote_wal, e))?;
    }

    log::info!(
        "[OPENCODE-SYNC] 已暂存 WSL opencode 数据库: {} -> {}",
        source.remote_db_path.display(),
        local_db.display()
    );

    Ok(local_db)
}
