//! WSL 发行版内会话日志的发现与枚举
//!
//! ## 背景
//!
//! cc-switch 以 Windows 桌面程序运行时，Claude Code / Codex / Gemini /
//! Grok Build 可能跑在 WSL 发行版里，会话日志落在 WSL 的 ext4 虚拟磁盘上，
//! 与 Windows 侧互不相通，默认同步扫不到。
//!
//! ## 方案
//!
//! 9P/UNC 的单次文件操作延迟远高于本地 NTFS，而增量同步需要对每个文件取
//! mtime，文件数量级是上千。因此：
//!
//! 1. 优先在发行版内跑一次 `find ... -printf '%T@\t%s\t%p\n'`，在 ext4 原生侧
//!    一把拿到全部路径、mtime 和大小；稳态下 UNC 操作数为 0。
//! 2. `find` 不可用（busybox 等无 GNU findutils 的发行版）时，回落到直接
//!    遍历 UNC 目录。
//!
//! 拿到文件清单后，读取文件内容仍走 UNC——只有 mtime 变化的文件才会被读，
//! 数量很少。

use std::path::{Path, PathBuf};

/// 一个 WSL 发行版内选定的用户主目录。
#[derive(Debug, Clone)]
pub struct WslHome {
    pub distro: String,
    /// 发行版的 UNC 根，如 `\\wsl$\Ubuntu`
    pub unc_root: PathBuf,
    /// 选定用户的 Linux 主目录，如 `/home/alice`
    pub linux_home: String,
}

/// 枚举到的一个会话日志文件。
#[derive(Debug, Clone)]
pub struct WslFile {
    /// 文件的 UNC 路径，可直接交给 `std::fs` 读取
    pub unc_path: PathBuf,
    /// 文件 mtime（纳秒）
    pub modified_nanos: i64,
    /// 文件字节数。Codex 的 stamp 校验和 Grok 的体积上限都需要它；
    /// 一并从 `find` 取回，调用方就完全不必再走 9P `stat`。
    pub size: u64,
}

/// 受支持工具在用户主目录下的会话根（相对路径）。
///
/// 每项是 `(相对根, 文件名 glob, 相对根的最大深度)`。深度语义与各工具在宿主
/// 侧的扫描器严格对齐，避免 WSL 侧和 Windows 侧的统计口径不一致。
pub(crate) const CLAUDE_ROOTS: &[(&str, &str, u32)] = &[(".claude/projects", "*.jsonl", 6)];
pub(crate) const CODEX_ROOTS: &[(&str, &str, u32)] = &[
    (".codex/sessions", "*.jsonl", 4),
    (".codex/archived_sessions", "*.jsonl", 1),
];
pub(crate) const GEMINI_ROOTS: &[(&str, &str, u32)] = &[(".gemini/tmp", "session-*.json", 3)];
pub(crate) const GROK_ROOTS: &[(&str, &str, u32)] = &[
    (".grok/sessions", "updates.jsonl", 16),
    (".grok/archived_sessions", "updates.jsonl", 16),
];

/// 探测用户主目录时，判定「这个用户在跑受支持的工具」所依据的目录。
const TOOL_MARKER_DIRS: &[&str] = &[".claude/projects", ".codex", ".gemini", ".grok"];

/// 受支持的工具。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WslTool {
    Claude,
    Codex,
    Gemini,
    GrokBuild,
}

impl WslTool {
    fn roots(self) -> &'static [(&'static str, &'static str, u32)] {
        match self {
            WslTool::Claude => CLAUDE_ROOTS,
            WslTool::Codex => CODEX_ROOTS,
            WslTool::Gemini => GEMINI_ROOTS,
            WslTool::GrokBuild => GROK_ROOTS,
        }
    }

    /// 人类可读名，用于弹窗和日志。
    pub fn display_name(self) -> &'static str {
        match self {
            WslTool::Claude => "Claude Code",
            WslTool::Codex => "Codex",
            WslTool::Gemini => "Gemini",
            WslTool::GrokBuild => "Grok Build",
        }
    }

    pub fn all() -> [WslTool; 4] {
        [
            WslTool::Claude,
            WslTool::Codex,
            WslTool::Gemini,
            WslTool::GrokBuild,
        ]
    }
}

/// 布局谓词：给定会话根下的相对路径分量，判断该文件是否属于宿主扫描器
/// 会收集的形状。
///
/// `find` 给的是全递归结果，是宿主固定深度扫描的**超集**，必须过滤回同样的
/// 形状——否则 WSL 侧会比 Windows 侧多收文件，两边统计口径不一致。
pub(crate) fn matches_layout(tool: WslTool, rel_root: &str, components: &[&str]) -> bool {
    match tool {
        // 对齐 session_usage::collect_jsonl_files 的三种固定形状
        WslTool::Claude => match components.len() {
            // 项目/主会话.jsonl
            2 => true,
            // 项目/SESSION_ID/subagents/*.jsonl
            4 => components[2] == "subagents",
            // 项目/SESSION_ID/subagents/workflows/wf_*/*.jsonl
            6 => components[2] == "subagents" && components[3] == "workflows",
            _ => false,
        },
        // 对齐 session_usage_codex::collect_codex_session_files：
        // sessions 下按日期分区递归（深度上限 4），archived_sessions 是扁平目录
        WslTool::Codex => {
            if rel_root.ends_with("archived_sessions") {
                components.len() == 1
            } else {
                (1..=4).contains(&components.len())
            }
        }
        // 对齐 session_usage_gemini::collect_gemini_session_files：
        // tmp/<project_hash>/chats/session-*.json
        WslTool::Gemini => components.len() == 3 && components[1] == "chats",
        // 对齐 session_usage_grokbuild::collect_files_named：任意深度递归找
        // updates.jsonl，深度上限由 GROK_ROOTS 的 16 承担
        WslTool::GrokBuild => !components.is_empty(),
    }
}

/// 收集某个工具在所有 WSL 发行版内的会话文件。
///
/// 用户未开启 WSL 用量同步时返回空列表——这是唯一的开关判定点，各
/// `session_usage_*` 模块无需重复检查。非 Windows 上 `discover_homes`
/// 返回空列表，因此本函数天然是空操作。
pub fn collect_files(tool: WslTool) -> Vec<WslFile> {
    if !crate::settings::get_settings().enable_wsl_usage_sync {
        return Vec::new();
    }

    let mut files = Vec::new();
    for home in discover_homes() {
        files.extend(collect_files_for_home(&home, tool));
    }
    files
}

/// 收集单个主目录下某工具的会话文件（不检查开关，供探测和测试复用）。
pub(crate) fn collect_files_for_home(home: &WslHome, tool: WslTool) -> Vec<WslFile> {
    let mut files = Vec::new();
    for (rel_root, name_glob, max_depth) in tool.roots() {
        let root_prefix = unc_root_for(home, rel_root);
        for file in enumerate(home, rel_root, name_glob, *max_depth) {
            let Ok(rel) = file.unc_path.strip_prefix(&root_prefix) else {
                continue;
            };
            let components: Vec<&str> = rel
                .components()
                .filter_map(|c| c.as_os_str().to_str())
                .collect();
            if matches_layout(tool, rel_root, &components) {
                files.push(file);
            }
        }
    }
    files
}

/// 某个会话根在 UNC 下的绝对路径。
pub(crate) fn unc_root_for(home: &WslHome, rel_root: &str) -> PathBuf {
    let mut root = home.unc_root.clone();
    for part in home.linux_home.trim_start_matches('/').split('/') {
        if !part.is_empty() {
            root.push(part);
        }
    }
    for part in rel_root.split('/') {
        root.push(part);
    }
    root
}

/// 把 `find` 的一行 `%T@` 输出转成纳秒时间戳。
///
/// `%T@` 形如 `1754467200.1234567890`（秒 + 小数秒）。**不能走 f64**：纳秒
/// 量级是 1.7e18，需要 19 位有效数字，而 f64 只有约 15–17 位，
/// `parse::<f64>() * 1e9` 会丢掉纳秒位。这里按小数点切分，两段各自按字符串
/// 转 `i64` 再合成。
///
/// 负数（1970 年之前的 mtime）返回 `None`，由调用方回落到 UNC stat。
#[cfg(any(target_os = "windows", test))]
pub(crate) fn parse_epoch_to_nanos(raw: &str) -> Option<i64> {
    let raw = raw.trim();
    if raw.is_empty() || raw.starts_with('-') {
        return None;
    }

    let (secs_part, frac_part) = match raw.split_once('.') {
        Some((s, f)) => (s, f),
        None => (raw, ""),
    };

    let secs: i64 = secs_part.parse().ok()?;

    // 小数部分补齐/截断到 9 位（纳秒）
    let mut frac_digits: String = frac_part.chars().take(9).collect();
    if !frac_digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    while frac_digits.len() < 9 {
        frac_digits.push('0');
    }
    let nanos: i64 = if frac_digits.is_empty() {
        0
    } else {
        frac_digits.parse().ok()?
    };

    secs.checked_mul(1_000_000_000)?.checked_add(nanos)
}

/// 把 WSL 内的绝对 Linux 路径转成 UNC 路径。
///
/// `/home/alice/.claude/projects/x.jsonl` → `\\wsl$\Ubuntu\home\alice\.claude\projects\x.jsonl`
///
/// 拒绝非绝对路径和含 `..` 的路径：`find` 的输出本身是可信的，但把外部进程
/// 的输出直接拼进文件路径时做一次边界检查是廉价的纵深防御。
#[cfg(any(target_os = "windows", test))]
pub(crate) fn linux_path_to_unc(unc_root: &std::path::Path, linux_path: &str) -> Option<PathBuf> {
    let rest = linux_path.strip_prefix('/')?;
    if rest.is_empty() {
        return None;
    }

    let mut result = unc_root.to_path_buf();
    for component in rest.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            return None;
        }
        result.push(component);
    }
    Some(result)
}

/// 极简 glob 匹配，只支持最多一个 `*`。
///
/// 实际用到的模式只有 `*.jsonl`、`session-*.json`、`updates.jsonl` 三种，
/// 引入完整 glob 依赖不划算。
pub(crate) fn glob_matches(name: &str, pattern: &str) -> bool {
    match pattern.split_once('*') {
        None => name == pattern,
        Some((prefix, suffix)) => {
            name.len() >= prefix.len() + suffix.len()
                && name.starts_with(prefix)
                && name.ends_with(suffix)
        }
    }
}

/// `find` 输出中的一条记录。mtime 不可解析时为 `None`，由调用方回落到 stat。
#[cfg(any(target_os = "windows", test))]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FindEntry {
    pub unc_path: PathBuf,
    pub modified_nanos: Option<i64>,
    pub size: u64,
}

/// 解析 `find -printf '%T@\t%s\t%p\n'` 的输出。
///
/// 每行形如 `1754467200.1234567890\t4096\t/home/alice/.claude/projects/x.jsonl`。
/// 按**前两个**制表符切分，路径取剩余全部，因此路径中含制表符不会破坏解析。
///
/// 含换行符的路径会被拆成两行，第二行解析失败即丢弃——属于可接受的降级：
/// 会话文件名遵循 UUID / 时间戳格式，项目目录名是路径转义形式，正常不含换行。
#[cfg(any(target_os = "windows", test))]
pub(crate) fn parse_find_output(output: &str, unc_root: &std::path::Path) -> Vec<FindEntry> {
    let mut entries = Vec::new();
    for line in output.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(3, '\t');
        let (Some(mtime_raw), Some(size_raw), Some(linux_path)) =
            (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let Some(unc_path) = linux_path_to_unc(unc_root, linux_path) else {
            continue;
        };
        entries.push(FindEntry {
            unc_path,
            modified_nanos: parse_epoch_to_nanos(mtime_raw),
            size: size_raw.trim().parse().unwrap_or(0),
        });
    }
    entries
}

/// 发现所有可同步的 WSL 用户主目录。
///
/// 每个发行版只取一个用户：把 `/home/*` 按字典序排序，取第一个包含任意受支持
/// 工具目录的主目录；都不命中则回落到 `/root`。排序是为了让结果稳定——
/// `read_dir` 的返回顺序未定义，不排序会导致多用户发行版上每次选中的用户不同。
pub fn discover_homes() -> Vec<WslHome> {
    use crate::services::wsl::{first_readable_unc_root, list_distros};

    let mut homes = Vec::new();
    for distro in list_distros() {
        // 发行版未运行时 UNC 访问会失败，直接跳过——不主动唤醒它，
        // 睡着的发行版也不会有新会话。
        let Some(unc_root) = first_readable_unc_root(&distro) else {
            continue;
        };

        let mut candidates: Vec<(String, PathBuf)> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(unc_root.join("home")) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                candidates.push((format!("/home/{name}"), path));
            }
        }
        candidates.sort_by(|a, b| a.0.cmp(&b.0));
        candidates.push(("/root".to_string(), unc_root.join("root")));

        for (linux_home, unc_home) in candidates {
            if !has_tool_marker(&unc_home) {
                continue;
            }
            homes.push(WslHome {
                distro: distro.clone(),
                unc_root: unc_root.clone(),
                linux_home,
            });
            break;
        }
    }
    homes
}

/// 主目录下是否存在任何受支持工具的目录。
pub(crate) fn has_tool_marker(unc_home: &Path) -> bool {
    TOOL_MARKER_DIRS.iter().any(|rel| {
        let mut path = unc_home.to_path_buf();
        for part in rel.split('/') {
            path.push(part);
        }
        path.is_dir()
    })
}

/// 枚举某个会话根下匹配 `name_glob` 的文件。
///
/// 优先走发行版内的 `find`；失败时回落到目录遍历。回落路径是纯 `std::fs`，
/// 在所有平台上编译并可测试——这是最难在真机上验证的分支，必须能在 CI 覆盖。
pub fn enumerate(home: &WslHome, rel_root: &str, name_glob: &str, max_depth: u32) -> Vec<WslFile> {
    #[cfg(target_os = "windows")]
    {
        let linux_root = format!("{}/{}", home.linux_home, rel_root);
        match enumerate_via_find(home, &linux_root, name_glob, max_depth) {
            Ok(files) => return files,
            Err(e) => {
                // find 不可用（busybox 无 GNU findutils）是正常情况，不打扰用户
                log::debug!(
                    "[WSL-SESSION] {} 的 find 枚举失败，回落到目录遍历: {e}",
                    home.distro
                );
            }
        }
    }

    enumerate_via_walk(home, rel_root, name_glob, max_depth)
}

/// 在发行版内跑 `find`，一次拿到全部路径和 mtime。
///
/// `-printf` 的格式串以字面量传入（转义由 `find` 自己解释），整条命令走
/// exec 形式不经 shell，没有命令注入面。
///
/// `find` 默认不跟随符号链接，因此 `-type f` 天然排除符号链接文件，也不会
/// 递归进符号链接目录——与 Grok 宿主扫描器刻意跳过 symlink 的安全语义一致。
#[cfg(target_os = "windows")]
fn enumerate_via_find(
    home: &WslHome,
    linux_root: &str,
    name_glob: &str,
    max_depth: u32,
) -> Result<Vec<WslFile>, crate::error::AppError> {
    let max_depth_str = max_depth.to_string();
    let output = crate::services::wsl::run_in_distro(
        &home.distro,
        &[
            "find",
            linux_root,
            "-maxdepth",
            &max_depth_str,
            "-type",
            "f",
            "-name",
            name_glob,
            "-printf",
            "%T@\\t%s\\t%p\\n",
        ],
    )?;

    let entries = parse_find_output(&output, &home.unc_root);
    Ok(entries
        .into_iter()
        .map(|entry| {
            // mtime 不可解析（1970 年之前等异常值）时回落到单文件 stat，
            // 避免因为一个畸形时间戳丢掉整个文件的用量。
            let modified_nanos = entry.modified_nanos.unwrap_or_else(|| {
                std::fs::metadata(&entry.unc_path)
                    .map(|m| crate::services::session_usage::metadata_modified_nanos(&m))
                    .unwrap_or(0)
            });
            WslFile {
                unc_path: entry.unc_path,
                modified_nanos,
                size: entry.size,
            }
        })
        .collect())
}

/// 回落路径：直接遍历目录（Windows 上就是 UNC 路径）。
///
/// 与 `find` 的语义对齐：只收文件、不跟随符号链接、深度上限相同。
pub(crate) fn enumerate_via_walk(
    home: &WslHome,
    rel_root: &str,
    name_glob: &str,
    max_depth: u32,
) -> Vec<WslFile> {
    let mut root = home.unc_root.clone();
    for part in home.linux_home.trim_start_matches('/').split('/') {
        root.push(part);
    }
    for part in rel_root.split('/') {
        root.push(part);
    }

    let mut files = Vec::new();
    walk_dir(&root, name_glob, 1, max_depth, &mut files);
    files
}

fn walk_dir(dir: &Path, name_glob: &str, depth: u32, max_depth: u32, files: &mut Vec<WslFile>) {
    if depth > max_depth {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        // `entry.metadata()` 不跟随符号链接，与 find 的默认行为一致
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_symlink() {
            continue;
        }
        let path = entry.path();
        if metadata.is_dir() {
            walk_dir(&path, name_glob, depth + 1, max_depth, files);
        } else if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| glob_matches(n, name_glob))
        {
            files.push(WslFile {
                unc_path: path,
                modified_nanos: crate::services::session_usage::metadata_modified_nanos(&metadata),
                size: metadata.len(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_epoch_keeps_nanosecond_precision() {
        // f64 在这个量级只有 ~15-17 位有效数字，会丢掉末尾纳秒位；
        // 字符串解析必须逐位精确。
        let nanos = parse_epoch_to_nanos("1754467200.1234567890").unwrap();
        assert_eq!(nanos, 1_754_467_200_123_456_789);

        // 相邻 1 纳秒的两个时间戳必须能区分开
        let a = parse_epoch_to_nanos("1754467200.000000001").unwrap();
        let b = parse_epoch_to_nanos("1754467200.000000002").unwrap();
        assert_eq!(b - a, 1);
    }

    #[test]
    fn parse_epoch_pads_short_fractions() {
        assert_eq!(parse_epoch_to_nanos("100.5").unwrap(), 100_500_000_000);
        assert_eq!(parse_epoch_to_nanos("100").unwrap(), 100_000_000_000);
        assert_eq!(parse_epoch_to_nanos("0").unwrap(), 0);
    }

    #[test]
    fn parse_epoch_rejects_invalid() {
        assert!(parse_epoch_to_nanos("").is_none());
        assert!(parse_epoch_to_nanos("-1.5").is_none());
        assert!(parse_epoch_to_nanos("abc").is_none());
        assert!(parse_epoch_to_nanos("100.abc").is_none());
    }

    #[test]
    fn linux_path_converts_to_unc() {
        let root = PathBuf::from(r"\\wsl$\Ubuntu");
        assert_eq!(
            linux_path_to_unc(&root, "/home/alice/.claude/projects/x.jsonl").unwrap(),
            PathBuf::from(r"\\wsl$\Ubuntu").join(
                "home/alice/.claude/projects/x.jsonl".replace('/', std::path::MAIN_SEPARATOR_STR)
            )
        );
    }

    #[test]
    fn linux_path_rejects_traversal_and_relative() {
        let root = PathBuf::from(r"\\wsl$\Ubuntu");
        assert!(linux_path_to_unc(&root, "home/alice/x.jsonl").is_none());
        assert!(linux_path_to_unc(&root, "/home/../../etc/passwd").is_none());
        assert!(linux_path_to_unc(&root, "/").is_none());
        assert!(linux_path_to_unc(&root, "").is_none());
    }

    #[test]
    fn glob_matches_supported_patterns() {
        assert!(glob_matches("a.jsonl", "*.jsonl"));
        assert!(glob_matches("session-abc.json", "session-*.json"));
        assert!(glob_matches("updates.jsonl", "updates.jsonl"));

        assert!(!glob_matches("a.json", "*.jsonl"));
        assert!(!glob_matches("other-abc.json", "session-*.json"));
        assert!(!glob_matches("updates.json", "updates.jsonl"));
        // 前后缀不能重叠：`session-.json` 长度刚好，`session.json` 不够
        assert!(glob_matches("session-.json", "session-*.json"));
        assert!(!glob_matches("session.json", "session-*.json"));
    }

    #[test]
    fn parse_find_output_handles_tabs_in_paths() {
        let root = PathBuf::from(r"\\wsl$\Ubuntu");
        // 路径含制表符：按第一个 \t 切分，路径部分保持完整
        let output = "1754467200.5\t4096\t/home/alice/we\tird/x.jsonl\n";
        let entries = parse_find_output(output, &root);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].modified_nanos, Some(1_754_467_200_500_000_000));
        assert_eq!(entries[0].size, 4096);
        assert!(entries[0].unc_path.to_string_lossy().contains("we\tird"));
    }

    #[test]
    fn parse_find_output_skips_malformed_lines() {
        let root = PathBuf::from(r"\\wsl$\Ubuntu");
        let output = concat!(
            "1754467200.0\t10\t/home/alice/a.jsonl\n",
            "no-tab-here\n",
            "\n",
            "1754467201.0\t10\trelative/path.jsonl\n",
            "1754467202.0\t20\t/home/alice/b.jsonl\n",
        );
        let entries = parse_find_output(output, &root);
        assert_eq!(entries.len(), 2);
        assert!(entries[0].unc_path.to_string_lossy().ends_with("a.jsonl"));
        assert!(entries[1].unc_path.to_string_lossy().ends_with("b.jsonl"));
    }

    #[test]
    fn parse_find_output_marks_unparsable_mtime_for_stat_fallback() {
        let root = PathBuf::from(r"\\wsl$\Ubuntu");
        let output = "-5.0\t123\t/home/alice/ancient.jsonl\n";
        let entries = parse_find_output(output, &root);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].modified_nanos, None,
            "无法解析的 mtime 必须留给调用方 stat 回落，不能丢掉文件"
        );
    }

    #[test]
    fn parse_find_output_tolerates_crlf() {
        let root = PathBuf::from(r"\\wsl$\Ubuntu");
        let output = "1754467200.0\t99\t/home/alice/a.jsonl\r\n";
        let entries = parse_find_output(output, &root);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].unc_path.to_string_lossy().ends_with("a.jsonl"));
    }

    // 布局谓词必须和宿主扫描器收集的形状严格一致，否则 WSL 侧和 Windows 侧
    // 的统计口径会不同。下面的形状直接对应各 collect_* 函数的既有单测。

    #[test]
    fn claude_layout_matches_host_shapes() {
        let t = WslTool::Claude;
        let root = ".claude/projects";
        // 主会话：项目/x.jsonl
        assert!(matches_layout(t, root, &["project", "main.jsonl"]));
        // 普通子 agent：项目/SID/subagents/x.jsonl
        assert!(matches_layout(
            t,
            root,
            &["project", "sid", "subagents", "agent.jsonl"]
        ));
        // Workflow 子 agent：项目/SID/subagents/workflows/wf_x/x.jsonl
        assert!(matches_layout(
            t,
            root,
            &[
                "project",
                "sid",
                "subagents",
                "workflows",
                "wf_1",
                "a.jsonl"
            ]
        ));
    }

    #[test]
    fn claude_layout_rejects_shapes_host_does_not_scan() {
        let t = WslTool::Claude;
        let root = ".claude/projects";
        // 直接放在 projects 根下（宿主只扫项目目录内）
        assert!(!matches_layout(t, root, &["stray.jsonl"]));
        // 深度对但中间目录名不对
        assert!(!matches_layout(
            t,
            root,
            &["project", "sid", "other", "agent.jsonl"]
        ));
        assert!(!matches_layout(
            t,
            root,
            &["project", "sid", "subagents", "other", "wf_1", "a.jsonl"]
        ));
        // 奇数深度不属于任何已知形状
        assert!(!matches_layout(t, root, &["project", "sid", "a.jsonl"]));
    }

    #[test]
    fn codex_layout_separates_sessions_from_archived() {
        let t = WslTool::Codex;
        // sessions 支持 1..=4 层（日期分区 YYYY/MM/DD 之外也容忍扁平摆放）
        assert!(matches_layout(t, ".codex/sessions", &["a.jsonl"]));
        assert!(matches_layout(
            t,
            ".codex/sessions",
            &["2026", "08", "06", "a.jsonl"]
        ));
        assert!(!matches_layout(
            t,
            ".codex/sessions",
            &["2026", "08", "06", "extra", "a.jsonl"]
        ));

        // archived_sessions 是扁平目录，只收第一层
        assert!(matches_layout(t, ".codex/archived_sessions", &["a.jsonl"]));
        assert!(!matches_layout(
            t,
            ".codex/archived_sessions",
            &["sub", "a.jsonl"]
        ));
    }

    #[test]
    fn gemini_layout_requires_chats_dir() {
        let t = WslTool::Gemini;
        let root = ".gemini/tmp";
        assert!(matches_layout(
            t,
            root,
            &["hash", "chats", "session-1.json"]
        ));
        assert!(!matches_layout(
            t,
            root,
            &["hash", "other", "session-1.json"]
        ));
        assert!(!matches_layout(t, root, &["hash", "session-1.json"]));
    }

    #[test]
    fn grok_layout_accepts_any_depth() {
        let t = WslTool::GrokBuild;
        let root = ".grok/sessions";
        assert!(matches_layout(t, root, &["updates.jsonl"]));
        assert!(matches_layout(t, root, &["a", "b", "c", "updates.jsonl"]));
        assert!(!matches_layout(t, root, &[]));
    }

    /// 在临时目录里搭一个假的 WSL 主目录。`unc_root` 用真实路径代替 UNC，
    /// 遍历逻辑本身与路径前缀无关，因此能在非 Windows 上完整验证。
    fn fake_home(root: &Path) -> WslHome {
        WslHome {
            distro: "Ubuntu".to_string(),
            unc_root: root.to_path_buf(),
            linux_home: "/home/alice".to_string(),
        }
    }

    #[test]
    fn walk_fallback_collects_claude_layout_and_filters_the_rest() {
        // 回落路径（find 不可用时）是最难在真机上验证的分支，这里端到端跑一遍：
        // 建目录 -> 遍历 -> 布局过滤，确认收到的正是宿主扫描器会收的那些文件。
        let tmp = tempfile::tempdir().expect("tempdir");
        let projects = tmp.path().join("home/alice/.claude/projects");

        let main = projects.join("proj");
        let subagents = main.join("sid/subagents");
        let workflows = subagents.join("workflows/wf_1");
        std::fs::create_dir_all(&workflows).unwrap();

        std::fs::write(main.join("main.jsonl"), "{}").unwrap();
        std::fs::write(subagents.join("agent.jsonl"), "{}").unwrap();
        std::fs::write(workflows.join("wf.jsonl"), "{}").unwrap();
        // 非 .jsonl 不该被收
        std::fs::write(main.join("notes.txt"), "x").unwrap();
        // 直接躺在 projects 根下的文件，宿主不扫，这里也不该收
        std::fs::write(projects.join("stray.jsonl"), "{}").unwrap();

        let home = fake_home(tmp.path());
        let files = collect_files_for_home(&home, WslTool::Claude);

        let names: Vec<String> = files
            .iter()
            .map(|f| {
                f.unc_path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .to_string()
            })
            .collect();

        assert_eq!(files.len(), 3, "实际收到: {names:?}");
        assert!(names.contains(&"main.jsonl".to_string()));
        assert!(names.contains(&"agent.jsonl".to_string()));
        assert!(names.contains(&"wf.jsonl".to_string()));
        assert!(!names.contains(&"stray.jsonl".to_string()));
        assert!(!names.contains(&"notes.txt".to_string()));

        // mtime 和 size 必须都带回来，否则调用方还得再走一次 9P stat，
        // "稳态零 UNC 操作"的设计就落空了
        assert!(files.iter().all(|f| f.modified_nanos > 0));
        assert!(files.iter().all(|f| f.size > 0));
    }

    #[test]
    fn walk_fallback_separates_codex_sessions_from_archived() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let codex = tmp.path().join("home/alice/.codex");
        let dated = codex.join("sessions/2026/08/06");
        let archived = codex.join("archived_sessions");
        std::fs::create_dir_all(&dated).unwrap();
        std::fs::create_dir_all(archived.join("nested")).unwrap();

        std::fs::write(dated.join("a.jsonl"), "{}").unwrap();
        std::fs::write(archived.join("b.jsonl"), "{}").unwrap();
        // archived 是扁平目录，子目录里的文件宿主不收
        std::fs::write(archived.join("nested/c.jsonl"), "{}").unwrap();

        let home = fake_home(tmp.path());
        let files = collect_files_for_home(&home, WslTool::Codex);
        let names: Vec<String> = files
            .iter()
            .map(|f| {
                f.unc_path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .to_string()
            })
            .collect();

        assert!(names.contains(&"a.jsonl".to_string()));
        assert!(names.contains(&"b.jsonl".to_string()));
        assert!(
            !names.contains(&"c.jsonl".to_string()),
            "archived_sessions 的子目录不该被收，实际: {names:?}"
        );
    }

    #[test]
    fn walk_fallback_returns_nothing_when_root_is_missing() {
        // 用户没装某个工具是常态，不能 panic，也不该报错
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = fake_home(tmp.path());
        assert!(collect_files_for_home(&home, WslTool::Gemini).is_empty());
        assert!(collect_files_for_home(&home, WslTool::GrokBuild).is_empty());
    }
}
