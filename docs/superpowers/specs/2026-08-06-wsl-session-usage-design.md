# Windows 宿主同步 WSL 内的会话使用统计

日期：2026-08-06
状态：设计已确认，待实现

## 背景

cc-switch 作为 Windows 桌面程序运行时，用户的 Claude Code / Codex / Gemini /
Grok Build 可能跑在 WSL 发行版里。这些工具的会话日志落在 WSL 的 ext4 虚拟磁盘上
（`/home/<user>/.claude/projects` 等），与 Windows 侧 `C:\Users\<user>\...`
互不相通，因此现有同步完全扫不到，用量统计缺失。

OpenCode 已在 `services/wsl_opencode.rs` 里解决了同一问题，但它只有一个 SQLite
文件，采取的是「整库复制到本地缓存再解析」的策略。Claude / Codex / Gemini /
Grok 的语料是成百上千个 JSONL/JSON 小文件、体积可能上 GB，复制成本远高于
OpenCode 场景，需要另一套发现与读取策略。

## 目标

- Windows 宿主上同步 WSL 发行版内 Claude Code、Codex、Gemini、Grok Build 的
  会话用量。
- 稳态（无新会话）下的额外开销接近零。
- 用户可控：首次检测到 WSL 内的工具时询问，用户同意才开启。
- 顺带把 `wsl_opencode.rs` 里可复用的原语提取为公共层。

## 非目标

- 不支持每个发行版的多个用户主目录（每个发行版只取第一个命中的用户）。
- 不支持按发行版或按工具的细粒度开关（只有一个总开关）。
- 不改变任何工具的解析、去重、计费逻辑；本设计只解决「文件在哪、什么时候读」。

## 核心权衡：如何读 WSL 里的文件

9P/UNC 的单次文件操作延迟远高于本地 NTFS。现有同步对每个文件都要先
`fs::metadata` 拿 mtime 做增量判断，文件数量级是上千。

考虑过三个方案：

**A. 纯 UNC 直读** — 把 `\\wsl$\<distro>\home\<user>\.claude\projects` 当普通
目录，现有代码零改动复用。代价是每轮同步在 9P 上做上千次 `read_dir` +
`metadata`，即使文件都没变也躲不掉。

**B. `wsl.exe` 批量枚举 + UNC 按需读** — 一次 `find` 调用在 ext4 原生侧拿到
全部路径和 mtime，与游标比对后只对真正变化的文件走 UNC 读内容。稳态下 UNC
操作数为 0。代价是 `find -printf` 是 GNU findutils 扩展，busybox 环境没有。

**C. 全量复制到本地缓存** — 照搬 `wsl_opencode.rs`。对上千个文件、GB 级语料，
复制成本比 stat 高一个数量级，且这里没有 SQLite WAL 那样必须规避的锁问题。

**决定：采用 B，以 A 作为回落。** 拿到稳态零开销，同时在非 GNU 发行版上不会
直接失效。C 不适用于本场景。

## 架构

三层结构。

### 第 1 层：`services/wsl.rs` — 通用原语

从现有 `wsl_opencode.rs` 提取（含其单测），使两个模块共享同一套实现：

- `is_valid_distro_name(name) -> bool`
- `decode_wsl_stdout(bytes) -> String`（UTF-16LE 判据 + UTF-8 回落）
- `parse_wsl_list_output(output) -> Vec<String>`

新增：

- `list_distros() -> Vec<String>`
- `first_readable_unc_root(distro) -> Option<PathBuf>`
- `run_in_distro(distro, args, deadline) -> Result<String, AppError>`
  —— `wsl.exe -d <distro> -- <argv>` 的 exec 形式，不经 shell，无命令注入面；
  执行前校验发行版名。

`wsl_opencode.rs` 改为依赖本层，删除重复实现。

### 第 2 层：`services/wsl_sessions.rs` — 会话根发现与文件枚举

```rust
pub struct WslHome {
    pub distro: String,
    pub unc_root: PathBuf,   // \\wsl$\Ubuntu
    pub linux_home: String,  // /home/alice
}

pub struct WslFile {
    pub unc_path: PathBuf,
    pub modified_nanos: i64,
}

pub fn discover_homes() -> Vec<WslHome>;
pub fn enumerate(home: &WslHome, rel_root: &str, name_glob: &str, max_depth: u32)
    -> Vec<WslFile>;
```

`discover_homes` 枚举发行版 → 探测可读 UNC 根 → 选定用户主目录：把 `/home/*`
按字典序排序，取**第一个包含任意受支持工具目录**的主目录；都不命中则回落到
`/root`。排序是为了让结果稳定 —— `read_dir` 的返回顺序未定义，不排序会导致
多用户发行版上每次选中的用户可能不同。

`enumerate` 先尝试：

```
wsl.exe -d <distro> -- find <linux_root> -maxdepth <N> -type f -name <glob> -printf '%T@\t%p\n'
```

`-printf` 的格式串以字面量传入（Rust 侧写作 `"%T@\\t%p\\n"`），转义由 `find`
自己解释，不经 shell。

`find` 默认不跟随符号链接，因此 `-type f` 天然排除了符号链接文件，也不会descend
进符号链接目录 —— 与 Grok 宿主扫描器刻意跳过 symlink 的安全语义一致。

退出码非 0 或输出无法解析时，回落到 UNC 目录遍历（方案 A）。

### 第 3 层：各 `session_usage_*.rs` 挂接

每个工具提供一个纯函数「布局谓词」，对 `enumerate` 结果做过滤，保证 WSL 侧与
宿主侧的扫描规则严格一致（否则两侧统计口径会不同）：

| 工具 | 根目录 | glob | maxdepth | 谓词要点 |
|------|--------|------|----------|----------|
| Claude | `~/.claude/projects` | `*.jsonl` | 6 | 项目/`*.jsonl`；项目/SID/`subagents`/`*.jsonl`；项目/SID/`subagents/workflows/wf_*`/`*.jsonl` |
| Codex | `~/.codex` | `*.jsonl` | 4 | `sessions/YYYY/MM/DD/*.jsonl`；`archived_sessions/*.jsonl` |
| Gemini | `~/.gemini/tmp` | `session-*.json` | 3 | `tmp/<hash>/chats/session-*.json` |
| Grok | `~/.grok` | `updates.jsonl` | 16 | `sessions/**` 与 `archived_sessions/**` 下的 `updates.jsonl` |

`find` 给的是全递归结果，是宿主固定深度扫描的超集，所以必须用谓词过滤回同样
的形状。谓词是纯函数，直接单测。

## 关键决策

### mtime 精度必须走字符串解析

`%T@` 输出形如 `1754467200.1234567890`。游标是纳秒 `i64`，量级 1.7e18 需要
19 位有效数字，而 f64 只有约 15–17 位 —— `parse::<f64>() * 1e9` 会丢纳秒位。

实现：按 `.` 切分，整数秒与小数部分各自按字符串转 `i64` 再合成，小数部分
补齐/截断到 9 位。

### mtime 来源在 find 与 UNC 之间切换是安全的

9P 看到的 mtime 与 ext4 原生 mtime 未必逐位相同，回落发生时游标来源改变，
可能触发一次多余重扫。这不会导致重复计费：

1. `last_line_offset` 跳过已处理行；
2. `request_id` 主键 + `INSERT OR IGNORE` 再兜一层。

代价仅为一次多余的文件读取，可接受。

### 不主动唤醒发行版

先探测 UNC 根是否可读，只有可读的发行版才执行 `find`。保持
`wsl_opencode.rs` 现有的「休眠发行版直接跳过」语义 —— 睡着的发行版不会有新
会话。

### 同步 key 直接用 UNC 路径

`session_log_sync.file_path` 以路径为主键，UNC 路径天然唯一，与 Windows 侧
记录不会撞车，无需额外前缀。

### 含换行符的路径会被丢弃

`find -printf '%T@\t%p\n'` 以换行分隔记录，路径若含换行会被拆成两行，第二行
解析失败即丢弃。属于可接受的降级：会话文件名遵循 UUID / 时间戳格式，项目目录
名是路径转义形式，正常不含换行。

## opt-in 流程

沿用仓库既有的 `*_confirmed: Option<bool>` 模式（`proxy_confirmed`、
`usage_confirmed`、`first_run_notice_confirmed`）。

`AppSettings` 新增：

```rust
/// 是否同步 WSL 发行版内的会话用量（默认关闭）
pub enable_wsl_usage_sync: bool,
/// 是否已询问过 WSL 检测；None = 尚未询问
pub wsl_usage_prompt_confirmed: Option<bool>,
```

启动时（仅 Windows，且 `wsl_usage_prompt_confirmed` 为空）在后台探测：

- 探测到工具 → 前端弹出 `WslUsageDetectedDialog`，列出「Ubuntu：Claude Code、
  Codex」这样的结果。「开启统计」写 `enable_wsl_usage_sync = true` 并触发一次
  同步；「暂不」只写 `wsl_usage_prompt_confirmed = true`。
- 未探测到 → 直接写 `wsl_usage_prompt_confirmed = true`，永不打扰。

设置页提供同一个开关，方便用户反悔。

非 Windows 平台整个模块 `#[cfg(target_os = "windows")]`；设置字段保留但不生效，
避免 `settings.json` 跨平台漂移。

## 错误处理

- `wsl.exe` 不存在（未装 WSL）→ `list_distros` 返回空，整条路径静默跳过。
- UNC 根不可读（发行版未运行）→ 跳过该发行版，不报错。
- `find` 失败 → 回落 UNC 遍历，记 `log::debug`，不打扰用户。
- 单个文件读取失败 → 计入 `SessionSyncResult.errors`，不中断其他文件，与宿主
  侧现有行为一致。
- `find` 超时 → 20 秒 deadline，超时后杀进程并回落。

## 测试

纯函数单测：

- `parse_find_output`：小数秒精度（纳秒不丢位）、非法行、路径含空格、路径含
  制表符、空输出。
- 四个布局谓词：命中形状、拒绝越界深度、拒绝错误目录名。
- 回落判定：退出码非 0 / 输出为空 / 全部行不可解析。
- `wsl.rs` 迁移过来的既有单测保持通过。

前端测试：

- `WslUsageDetectedDialog` 渲染与两个按钮的落库行为。
- 四语言 i18n key 完整性（对齐 `tests/config/managementListLocales.test.ts`）。

真机 WSL 路径无法在 CI 覆盖，靠纯函数边界测试 + 本地实测验证。

## 影响面

新增：
- `src-tauri/src/services/wsl.rs`
- `src-tauri/src/services/wsl_sessions.rs`
- `src/components/WslUsageDetectedDialog.tsx`

修改：
- `src-tauri/src/services/wsl_opencode.rs`（改用公共层）
- `src-tauri/src/services/session_usage.rs`（Claude 挂接）
- `src-tauri/src/services/session_usage_codex.rs`（Codex 挂接）
- `src-tauri/src/services/session_usage_gemini.rs`（Gemini 挂接）
- `src-tauri/src/services/session_usage_grokbuild.rs`（Grok 挂接）
- `src-tauri/src/settings.rs`（两个新字段）
- `src-tauri/src/lib.rs`（启动探测）
- `src/types.ts`、`src/App.tsx`、设置页、`src/i18n/locales/*.json`
