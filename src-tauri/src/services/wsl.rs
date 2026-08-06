//! WSL 交互的通用原语
//!
//! cc-switch 以 Windows 桌面程序运行时，用户的 CLI 工具可能跑在 WSL 发行版里。
//! 本模块收拢与 `wsl.exe` 打交道的底层细节（发行版枚举、输出解码、UNC 根探测、
//! 在发行版内执行命令），供 [`crate::services::wsl_opencode`] 与
//! [`crate::services::wsl_sessions`] 共用。
//!
//! 纯函数（解码、解析、名称校验）在所有平台编译，便于在 CI 上跑单测；
//! 真正调用 `wsl.exe` 的部分仅在 Windows 上编译。

#[cfg(target_os = "windows")]
use crate::error::AppError;
use std::path::PathBuf;

/// 在发行版内执行命令的默认超时。
///
/// 取 20 秒与 OMO 模型拉取（`commands/misc.rs`）保持一致：冷启动一个休眠的
/// 发行版可能需要数秒，而正常的 `find` 枚举通常在一秒内返回。
#[cfg(target_os = "windows")]
pub(crate) const WSL_COMMAND_TIMEOUT_SECS: u64 = 20;

/// WSL 发行版名称合法性校验（跨平台，便于测试）。
/// 只允许字母、数字、连字符、下划线和点。
pub(crate) fn is_valid_distro_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// 解码 `wsl.exe` 的 stdout 字节。
///
/// `wsl.exe` 在无控制台（GUI 进程捕获输出）时以 **UTF-16LE**（每字符后跟
/// `0x00`）输出，直接按 UTF-8 解码会得到带 NUL 的乱码。这里先按 UTF-16LE
/// 解码，非 UTF-16LE 时回退到 UTF-8 lossy。
///
/// 判据：ASCII 内容转 UTF-16LE 后，每个字符的高字节都是 `0x00`，因此只要
/// 字节数为偶数且奇数位（高字节）大量为 0，就按 UTF-16LE 处理。
#[cfg(any(target_os = "windows", test))]
pub(crate) fn decode_wsl_stdout(bytes: &[u8]) -> String {
    let bytes = bytes
        .strip_prefix(&[0xFF, 0xFE])
        .or_else(|| bytes.strip_prefix(&[0xFE, 0xFF]))
        .unwrap_or(bytes);

    let utf16le = bytes.len() % 2 == 0 && bytes.len() >= 2 && {
        let zeroes = bytes[1..].iter().step_by(2).filter(|&&b| b == 0).count();
        let high_bytes = bytes[1..].len().div_ceil(2);
        zeroes * 2 >= high_bytes
    };

    if utf16le {
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16(&units).unwrap_or_default()
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
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

/// 枚举已安装的 WSL 发行版。
///
/// `--list --quiet` 不会唤醒发行版。WSL 未安装时 `wsl.exe` 不存在，返回空列表。
/// 非 Windows 平台上没有 WSL，恒为空——让上层的发现逻辑保持单一实现，
/// 无需在每一层重复 `#[cfg]`。
pub(crate) fn list_distros() -> Vec<String> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        use std::process::Command;

        const CREATE_NO_WINDOW: u32 = 0x08000000;

        match Command::new("wsl.exe")
            .args(["--list", "--quiet"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            Ok(out) if out.status.success() => {
                parse_wsl_list_output(&decode_wsl_stdout(&out.stdout))
            }
            _ => Vec::new(),
        }
    }

    #[cfg(not(target_os = "windows"))]
    Vec::new()
}

/// 探测 WSL 发行版可访问的 UNC 根路径。
///
/// `\\wsl$\` 最兼容，`\\wsl.localhost\` 兜底。发行版未运行时 UNC 访问会失败，
/// 返回 `None`——调用方据此跳过该发行版，避免主动唤醒它。
pub(crate) fn first_readable_unc_root(distro: &str) -> Option<PathBuf> {
    if !is_valid_distro_name(distro) {
        return None;
    }
    for server in ["wsl$", "wsl.localhost"] {
        let root = PathBuf::from(format!(r"\\{server}\{distro}"));
        if root.join("home").exists() {
            return Some(root);
        }
    }
    None
}

/// 在指定发行版内执行命令，返回解码后的 stdout。
///
/// `args` 以 exec 形式传给 `wsl.exe -d <distro> --`，**不经过 shell**，因此
/// 参数里的空格、引号、`$` 等都不会被重新解释，没有命令注入面。
///
/// 超时由发行版内的 `timeout` 承担：`wsl.exe` 被杀掉并不会终止发行版里的子
/// 进程（它们的父进程在 Linux 侧），所以必须让超时在 Linux 侧生效。
#[cfg(target_os = "windows")]
pub(crate) fn run_in_distro(distro: &str, args: &[&str]) -> Result<String, AppError> {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    const CREATE_NO_WINDOW: u32 = 0x08000000;

    if !is_valid_distro_name(distro) {
        return Err(AppError::Config(format!("非法的 WSL 发行版名: {distro}")));
    }

    let timeout_secs = WSL_COMMAND_TIMEOUT_SECS.to_string();
    let mut argv: Vec<&str> = vec!["-d", distro, "--", "timeout", "-k", "2", &timeout_secs];
    argv.extend_from_slice(args);

    let output = Command::new("wsl.exe")
        .args(&argv)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| AppError::Config(format!("[WSL:{distro}] 执行失败: {e}")))?;

    if !output.status.success() {
        let stderr = decode_wsl_stdout(&output.stderr);
        return Err(AppError::Config(format!(
            "[WSL:{distro}] 命令返回非零退出码: {}",
            stderr.trim()
        )));
    }

    Ok(decode_wsl_stdout(&output.stdout))
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

    #[test]
    fn test_decode_wsl_stdout_utf16le() {
        // 真实抓到的 wsl.exe --list --quiet 输出（UTF-16LE + CRLF）
        let raw = b"U\x00b\x00u\x00n\x00t\x00u\x00\r\x00\n\x00d\x00o\x00c\x00k\x00e\x00r\x00-\x00d\x00e\x00s\x00k\x00t\x00o\x00p\x00\r\x00\n\x00";
        let decoded = decode_wsl_stdout(raw);
        assert_eq!(decoded, "Ubuntu\r\ndocker-desktop\r\n");
        assert_eq!(
            parse_wsl_list_output(&decoded),
            vec!["Ubuntu", "docker-desktop"]
        );
    }

    #[test]
    fn test_decode_wsl_stdout_utf8_fallback() {
        assert_eq!(decode_wsl_stdout(b"Ubuntu\ndebian\n"), "Ubuntu\ndebian\n");
        assert_eq!(decode_wsl_stdout(b""), "");
    }

    #[test]
    fn test_decode_wsl_stdout_utf16le_with_bom() {
        let raw = b"\xff\xfeU\x00b\x00u\x00n\x00t\x00u\x00\r\x00\n\x00";
        assert_eq!(decode_wsl_stdout(raw), "Ubuntu\r\n");
    }

    #[test]
    fn test_decode_wsl_stdout_end_to_end_utf16le() {
        // 端到端：真实字节 -> 发行版列表
        let raw = b"U\x00b\x00u\x00n\x00t\x00u\x00\r\x00\n\x00d\x00o\x00c\x00k\x00e\x00r\x00-\x00d\x00e\x00s\x00k\x00t\x00o\x00p\x00\r\x00\n\x00";
        let distros = parse_wsl_list_output(&decode_wsl_stdout(raw));
        assert_eq!(distros, vec!["Ubuntu", "docker-desktop"]);
    }
}
