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

#[cfg(target_os = "windows")]
use crate::config::get_app_config_dir;
#[cfg(target_os = "windows")]
use crate::error::AppError;
#[cfg(target_os = "windows")]
use std::path::{Path, PathBuf};

/// WSL 发行版名称合法性校验（跨平台，便于测试）。
/// 只允许字母、数字、连字符、下划线和点。
#[cfg(any(target_os = "windows", test))]
pub(crate) fn is_valid_distro_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// 解析 `wsl.exe --list --quiet` 输出，返回发行版名称列表。
///
/// 兼容两种情况：
/// - 新版 `-q` 只输出发行版名（每行一个）；
/// - 老版本可能带 `Windows Subsystem for Linux Distributions:` 标题行，
///   或 `*` 前缀 / ` (Default)` 后缀。
#[cfg(any(target_os = "windows", test))]
pub(crate) fn parse_wsl_list_output(output: &str) -> Vec<String> {
    output
        .lines()
        .map(|line| line.trim().trim_start_matches('*').trim())
        .map(|line| line.trim_end_matches(" (Default)").trim())
        .filter(|line| {
            !line.is_empty()
                && !line.eq_ignore_ascii_case("Windows Subsystem for Linux Distributions:")
        })
        .map(str::to_string)
        .filter(|name| is_valid_distro_name(name))
        .collect()
}

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
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    const CREATE_NO_WINDOW: u32 = 0x08000000;

    let distros = match Command::new("wsl.exe")
        .args(["--list", "--quiet"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(out) if out.status.success() => {
            parse_wsl_list_output(&String::from_utf8_lossy(&out.stdout))
        }
        _ => return Vec::new(),
    };

    let mut sources = Vec::new();
    for distro in distros {
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

/// 探测 WSL 发行版可访问的 UNC 根路径。
#[cfg(target_os = "windows")]
fn first_readable_unc_root(distro: &str) -> Option<PathBuf> {
    for server in ["wsl$", "wsl.localhost"] {
        let root = PathBuf::from(format!(r"\\{server}\{distro}"));
        if root.join("home").exists() {
            return Some(root);
        }
    }
    None
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_wsl_list_output_names_only() {
        let output = "Ubuntu\nDebian\nopencode-dev\n";
        assert_eq!(
            parse_wsl_list_output(output),
            vec!["Ubuntu", "Debian", "opencode-dev"]
        );
    }

    #[test]
    fn test_parse_wsl_list_output_header_and_markers() {
        let output = "Windows Subsystem for Linux Distributions:\n* Ubuntu (Default)\n  Debian\n";
        assert_eq!(parse_wsl_list_output(output), vec!["Ubuntu", "Debian"]);
    }

    #[test]
    fn test_parse_wsl_list_output_empty() {
        assert!(parse_wsl_list_output("").is_empty());
        assert!(parse_wsl_list_output(" \n\r\n").is_empty());
    }

    #[test]
    fn test_parse_wsl_list_output_filters_invalid() {
        // 包含非法字符/超长的行应被过滤
        let output = "Ubuntu\nbad distro\nx;y\n";
        assert_eq!(parse_wsl_list_output(output), vec!["Ubuntu"]);
    }

    #[test]
    fn test_is_valid_distro_name() {
        assert!(is_valid_distro_name("Ubuntu"));
        assert!(is_valid_distro_name("Ubuntu-22.04"));
        assert!(is_valid_distro_name("my_distro"));
        assert!(!is_valid_distro_name(""));
        assert!(!is_valid_distro_name("has space"));
        assert!(!is_valid_distro_name("bad;name"));
        assert!(!is_valid_distro_name(&"a".repeat(65)));
    }
}
