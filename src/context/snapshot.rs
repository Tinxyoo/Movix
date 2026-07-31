use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

const DEFAULT_MAX_WORKSPACE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const GIT_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotId(pub String);

impl SnapshotId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SnapshotId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub id: SnapshotId,
    pub label: String,
    pub timestamp: i64,
}

pub struct SnapshotRepo {
    git_dir: PathBuf,
    work_tree: PathBuf,
}

impl SnapshotRepo {
    pub fn new(workspace: &Path) -> Self {
        let home = crate::common::utils::home_dir();
        let workspace_hash = format!("{:x}", simple_hash(workspace.to_string_lossy().as_bytes()));
        let git_dir = home
            .join(".movix")
            .join("snapshots")
            .join(&workspace_hash)
            .join(".git");

        Self {
            git_dir,
            work_tree: workspace.to_path_buf(),
        }
    }

    /// 测试专用:允许把 git_dir 显式指到一个可写位置。
    /// 修复(P1.1):macOS sandbox / TCC 下 `~/.movix` 不一定可写,生产构造函数
    /// 强制走 `home_dir()/.movix/snapshots`,在沙箱测试中会 `Operation not permitted`。
    #[cfg(test)]
    fn with_git_dir(workspace: &Path, git_dir: PathBuf) -> Self {
        Self {
            git_dir,
            work_tree: workspace.to_path_buf(),
        }
    }

    /// 构建带统一参数的 Git 命令
    fn build_git_command(&self, args: &[&str]) -> Vec<String> {
        let mut full_args = vec![
            "--git-dir".to_string(),
            self.git_dir.to_string_lossy().to_string(),
            "--work-tree".to_string(),
            self.work_tree.to_string_lossy().to_string(),
        ];
        // 修复(R6/git-H6):禁用工作区仓库的危险配置(钩子/fsmonitor/gitProxy/lfs filter),
        // 防止恶意 .gitattributes/.git/config 在 add/checkout 时执行任意命令。
        // 用 -c 内联覆盖,不改仓库文件。
        for cfg in [
            "-c core.hooksPath=/dev/null",
            "-c core.fsmonitor=false",
            "-c core.gitProxy=",
            "-c filter.lfs.clean=cat",
            "-c filter.lfs.smudge=cat",
        ] {
            for part in cfg.split_whitespace() {
                full_args.push(part.to_string());
            }
        }
        for arg in args {
            full_args.push(arg.to_string());
        }
        full_args
    }

    /// 执行带工作区参数的 git 命令（所有 git 操作都应使用此方法）
    fn run_git(&self, args: &[&str]) -> std::io::Result<std::process::Output> {
        self.run_git_with_env(args, None)
    }

    /// 执行带工作区参数和环境变量的 git 命令(带超时)。
    ///
    /// 修复:之前用 `Arc<Mutex<Option<Child>>>` + 超时线程 `take()` child 的写法存在
    /// 致命竞态 —— 主线程会立刻 `take()` 走 child 调 `wait_with_output()`(无界阻塞),
    /// 超时线程 30s 后再 `take()` 拿到的是 None,**永远无法**真正 kill 子进程,
    /// 超时形同虚设。改为复用通用工具 [`utils::run_command_with_timeout`],
    /// 它通过 `try_wait` 轮询 + 到点 `kill()` 实现真正的硬超时。
    fn run_git_with_env(
        &self,
        args: &[&str],
        env: Option<&[(&str, &str)]>,
    ) -> std::io::Result<std::process::Output> {
        let full_args = self.build_git_command(args);

        let mut cmd = Command::new("git");
        cmd.args(&full_args);

        if let Some(env_vars) = env {
            for (key, value) in env_vars {
                cmd.env(key, value);
            }
        }

        crate::common::utils::run_command_with_timeout(cmd, GIT_TIMEOUT_SECS)
    }

    pub fn init(&self) -> std::io::Result<()> {
        if self.git_dir.exists() {
            return Ok(());
        }

        if let Some(parent) = self.git_dir.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // git init --bare 需要目标路径作为参数，不能用 --git-dir 前缀
        let output = Command::new("git")
            .args(["init", "--bare"])
            .arg(&self.git_dir)
            .output()?;

        if !output.status.success() {
            return Err(std::io::Error::other(format!(
                "git init failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        self.configure_repo()?;

        // 修复(P1.1):此前 `init` 会立刻 `git add -A` + `commit`,把整工作区
        // 拷进 ~/.movix/snapshots/<hash>/。对几百 MB 的仓库,启动一次就 OOM 风险。
        // 现在 `init` 只建 bare repo + 配置;首次 `before_modification` 才 lazy
        // 触发 `ensure_baseline` 做"initial snapshot"。
        Ok(())
    }

    /// 首次写入前的 baseline。幂等:已存在 commit 时直接返回。
    /// 修复(P1.1)的配套:`init` 不再吃整库,baseline 推迟到真正要 snapshot 的时刻。
    pub fn ensure_baseline(&self) -> std::io::Result<()> {
        if self.has_any_commit() {
            return Ok(());
        }
        let add_output = self.run_git(&["add", "-A"])?;
        if !add_output.status.success() {
            let stderr = String::from_utf8_lossy(&add_output.stderr);
            if !stderr.contains("nothing to commit") {
                tracing::warn!(target: "snapshot", "snapshot baseline add warning: {}", stderr);
            }
        }
        self.commit("initial snapshot")?;
        Ok(())
    }

    /// 仓库里是否已经有过任意提交(用 `rev-parse HEAD` 判断)。
    fn has_any_commit(&self) -> bool {
        match self.run_git(&["rev-parse", "--verify", "HEAD"]) {
            Ok(out) => out.status.success(),
            Err(_) => false,
        }
    }

    fn configure_repo(&self) -> std::io::Result<()> {
        let configs = [
            ("user.name", "movix-snapshot"),
            ("user.email", "snapshot@movix.dev"),
            ("commit.gpgsign", "false"),
        ];

        for (key, value) in configs {
            if let Err(e) = self.run_git(&["config", key, value]) {
                tracing::warn!("snapshot: git config {}={} failed: {}", key, value, e);
            }
        }

        Ok(())
    }

    pub fn snapshot(&self, label: &str) -> std::io::Result<SnapshotId> {
        // 修复(R6/snapshot-C4,关键):`git add -A` 会把工作区全部内容(含 .env、SSH key、
        // credentials.json 等密钥)提交进快照仓库 ~/.movix/snapshots/<hash>/.git,明文持久化。
        // 任何能读 ~/.movix 的进程/备份都拿到全部密钥。
        //
        // 防护:在快照仓库写入 .git/info/suspend(或 .gitignore)排除常见密钥文件,
        // 再 add。注意这是**快照仓库自身**的 .gitignore(位于 git_dir),不影响用户工作区。
        self.ensure_secret_exclusions()?;

        let add_output = self.run_git(&["add", "-A"])?;

        if !add_output.status.success() {
            let stderr = String::from_utf8_lossy(&add_output.stderr);
            if !stderr.contains("nothing to commit") && !stderr.contains("no changes added") {
                eprintln!("snapshot add warning: {}", stderr);
            }
        }

        self.commit(label)
    }

    /// 修复(R6/snapshot-C4):在快照仓库的 git_dir/info/exclude 写入常见密钥文件模式,
    /// 使 `git add -A` 跳过它们。这是快照仓库本地的排除规则,不修改用户工作区的任何文件。
    fn ensure_secret_exclusions(&self) -> std::io::Result<()> {
        let exclude_path = self.git_dir.join("info").join("exclude");
        if let Some(parent) = exclude_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        const SECRET_PATTERNS: &str = "\n\
# movix 自动添加:排除密钥/凭证文件,防止 git add -A 把它们提交进快照仓库\n\
.env\n\
.env.*\n\
*.pem\n\
*.key\n\
id_rsa*\n\
id_ed25519*\n\
*.ppk\n\
credentials\n\
credentials.json\n\
*.pfx\n\
*.p12\n\
.git-credentials\n\
.netrc\n\
.npmrc\n\
.pypirc\n\
";
        let existing = std::fs::read_to_string(&exclude_path).unwrap_or_default();
        if !existing.contains("# movix 自动添加") {
            std::fs::write(&exclude_path, format!("{}{}", existing, SECRET_PATTERNS))?;
        }
        Ok(())
    }

    fn commit(&self, label: &str) -> std::io::Result<SnapshotId> {
        let _output = self.run_git_with_env(
            &["commit", "-m", label, "--allow-empty"],
            Some(&[
                ("GIT_AUTHOR_NAME", "movix-snapshot"),
                ("GIT_AUTHOR_EMAIL", "snapshot@movix.dev"),
                ("GIT_COMMITTER_NAME", "movix-snapshot"),
                ("GIT_COMMITTER_EMAIL", "snapshot@movix.dev"),
            ]),
        )?;

        let sha_output = self.run_git(&["rev-parse", "HEAD"])?;

        if sha_output.status.success() {
            let sha = String::from_utf8_lossy(&sha_output.stdout)
                .trim()
                .to_string();
            Ok(SnapshotId(sha))
        } else {
            // 修复(Medium):原实现返回 SnapshotId("unknown"),这个无效 id 会被
            // push 进 snapshot_stack,之后 restore("unknown") 时 git checkout 报错,
            // 把失败推迟到最需要回滚的时刻。改为直接返回错误,让 before_modification
            // 提前失败,调用方可选择不依赖快照继续。
            Err(std::io::Error::other(format!(
                "snapshot commit succeeded but rev-parse HEAD failed: {}",
                String::from_utf8_lossy(&sha_output.stderr)
            )))
        }
    }

    pub fn restore(&self, snapshot_id: &SnapshotId) -> std::io::Result<()> {
        // 修复(Critical #C4):原实现仅 `git checkout <sha> -- .`,只能恢复**已跟踪**
        // 文件到该 commit 的版本,但**不会删除**快照之后新建的文件(它们不在该 commit
        // 的 tree 里)。导致回滚后本应消失的新文件仍残留,用户看到"半回滚"的工作区。
        //
        // 正确的回滚需两步:
        //   1) `git checkout -- .` 丢弃已跟踪文件的工作区改动;
        //   2) `git checkout <sha> -- .` 把已跟踪文件恢复到快照版本;
        //   3) `git clean -fd` 删除快照后新增的未跟踪文件与目录。
        // 注意:不使用 `git reset --hard <sha>`——那会移动 HEAD,污染快照历史栈,
        // 后续 list() / restore() 会定位不到旧快照。这里只改工作树,不动 HEAD/历史。
        //
        // 修复(R6/snapshot-restore,关键):原 `git checkout -- .`(无 pathspec)会丢弃
        // 用户在**无关文件**上的未提交修改(不是 agent 触碰的)。改为限定到 paths:
        // 只回滚 agent 实际修改过的文件。paths 为空时回退到原全量行为(向后兼容)。
        self.restore_paths(snapshot_id, &[])
    }

    /// 限定路径的回滚:只恢复 `paths` 列出的文件。paths 为空时回滚整个工作区(旧行为)。
    pub fn restore_paths(
        &self,
        snapshot_id: &SnapshotId,
        paths: &[std::path::PathBuf],
    ) -> std::io::Result<()> {
        let id = snapshot_id.as_str();
        // 把 paths 转成 git 可接受的相对路径字符串(相对 work_tree)。
        let path_args: Vec<String> = paths
            .iter()
            .filter_map(|p| {
                // 用相对 work_tree 的路径传给 git,避免绝对路径在不同 cwd 下歧义。
                p.strip_prefix(&self.work_tree)
                    .ok()
                    .and_then(|r| r.to_str())
                    .map(|s| s.to_string())
            })
            .collect();
        let scoped = !path_args.is_empty();

        // 1) 丢弃已跟踪文件的工作区改动(限定到 paths 或全量)。
        let discard_args: Vec<&str> = if scoped {
            let mut v = vec!["checkout", "--"];
            v.extend(path_args.iter().map(|s| s.as_str()));
            v
        } else {
            vec!["checkout", "--", "."]
        };
        let discard = self.run_git(&discard_args)?;
        if !discard.status.success() {
            // 无已跟踪改动时 checkout 可能报 warning,不致命,继续。
        }

        // 2) 从快照 commit 恢复已跟踪文件(限定到 paths 或全量)。
        let restore_args: Vec<&str> = if scoped {
            let mut v = vec!["checkout", id, "--"];
            v.extend(path_args.iter().map(|s| s.as_str()));
            v
        } else {
            vec!["checkout", id, "--", "."]
        };
        let restore = self.run_git(&restore_args)?;
        if !restore.status.success() {
            return Err(std::io::Error::other(format!(
                "git restore failed: {}",
                String::from_utf8_lossy(&restore.stderr)
            )));
        }

        // 3) 处理快照后新增的未跟踪文件。
        //    修复(H3,关键):原实现用 `git clean -fd` 会删除**整个工作区所有未跟踪
        //    文件与目录**,而非"快照后新增的"。`git clean` 没有"自某快照起"的语义——
        //    用户工作区里与本次工具调用无关的草稿、`.env.local`、新文件会被**永久
        //    删除**,`-f` 无确认,这是一条静默的数据破坏路径。
        //
        //    安全策略:默认**不**自动删除未跟踪文件。先用 `git clean -nd`(dry-run)
        //    列出"若回滚会消失"的文件,告知用户让其自行决定;仅在用户**显式**通过
        //    环境变量 `MOVIX_SNAPSHOT_CLEAN_UNTRACKED=1` 授权时才真正 `-fd` 删除。
        let allow_clean = std::env::var("MOVIX_SNAPSHOT_CLEAN_UNTRACKED")
            .map(|v| v == "1")
            .unwrap_or(false);
        let clean_args: &[&str] = if allow_clean {
            &["clean", "-fd"]
        } else {
            // -n = dry run,仅列出会删除的文件,不实际删除。
            &["clean", "-nd"]
        };
        let clean = self.run_git(clean_args)?;
        if !clean.status.success() {
            // clean 失败通常因文件被占用,记录但不视为致命——已跟踪文件已恢复。
            eprintln!(
                "snapshot restore: git clean warning: {}",
                String::from_utf8_lossy(&clean.stderr)
            );
        } else if !allow_clean {
            // dry-run 输出了候选删除项:告知用户,但不自动删。
            let candidates = String::from_utf8_lossy(&clean.stdout);
            if !candidates.trim().is_empty() {
                eprintln!(
                    "snapshot restore: 以下未跟踪文件在快照后出现,默认保留(不自动删除):\n{}\n\
                     \x20   若需随回滚一并删除,设置环境变量 MOVIX_SNAPSHOT_CLEAN_UNTRACKED=1 后重试。",
                    candidates
                );
            }
        }

        Ok(())
    }

    pub fn list(&self) -> std::io::Result<Vec<Snapshot>> {
        let output = self.run_git(&["log", "--format=%H%x00%s%x00%ct", "--"])?;

        if !output.status.success() {
            return Ok(Vec::new());
        }

        let log = String::from_utf8_lossy(&output.stdout);
        let mut snapshots = Vec::new();

        for line in log.lines() {
            let parts: Vec<&str> = line.splitn(3, '\0').collect();
            if parts.len() == 3 {
                snapshots.push(Snapshot {
                    id: SnapshotId(parts[0].to_string()),
                    label: parts[1].to_string(),
                    timestamp: parts[2].parse().unwrap_or(0),
                });
            }
        }

        Ok(snapshots)
    }

    pub fn is_initialized(&self) -> bool {
        self.git_dir.exists()
    }

    /// 计算工作区大小（跨平台，使用文件系统遍历）
    pub fn workspace_size_ok(&self) -> bool {
        let size_bytes = walk_dir_size(&self.work_tree);
        size_bytes < DEFAULT_MAX_WORKSPACE_BYTES
    }

    /// 修复(G-M4 → H3):运行 git gc 回收对象,防止长会话磁盘膨胀。
    ///
    /// 原实现只 `git gc --prune=now`,但所有快照提交构成一条从 HEAD 可达的线性链,
    /// `gc` 只能清"不可达"对象,对这条不断增长的可达提交链**无能为力**——注释声称
    /// 防膨胀,实际无效。
    ///
    /// 正确做法:先 `reflog expire --expire=now --all` 清掉 reflog 里的可达性来源,
    /// 再 `gc --prune=now`,这样除了当前 HEAD 链之外的 reflog 引用会被回收。
    /// 注:快照栈活跃部分的提交仍可达(在 HEAD 链上),不会被误清。
    pub fn gc(&self) -> std::io::Result<()> {
        // 1. 清空 reflog(否则 reflog 里的旧引用会让 gc 认为对象仍可达)。
        let _ = self.run_git(&["reflog", "expire", "--expire=now", "--all"])?;
        // 2. --quiet 避免污染输出;--prune=now 立即清理不可达对象。
        let _ = self.run_git(&["gc", "--quiet", "--prune=now"])?;
        Ok(())
    }
}

/// 递归遍历目录计算总大小（字节），跨平台兼容
fn walk_dir_size(path: &Path) -> u64 {
    let mut total: u64 = 0;
    fn walk(path: &Path, total: &mut u64) {
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Ok(meta) = entry.metadata() {
                    if meta.is_dir() {
                        walk(&path, total);
                    } else if meta.is_file() {
                        *total += meta.len();
                    }
                }
            }
        }
    }
    walk(path, &mut total);
    total
}

fn simple_hash(data: &[u8]) -> u64 {
    // 修复(Low #L1):原用 `DefaultHasher`,Rust 标准库未承诺其算法跨版本稳定,
    // 工具链升级可能导致 workspace 路径 → 哈希值变化,旧快照目录集体失效。
    // 改用 FNV-1a(64 位),算法固定、无外部依赖,适合做路径派生的稳定哈希。
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut hash = FNV_OFFSET;
    for &b in data {
        hash ^= b as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_hash() {
        let h1 = simple_hash(b"/path/to/workspace");
        let h2 = simple_hash(b"/path/to/other");
        assert_ne!(h1, h2);
    }

    /// P1.1:`init` 不应再吃整工作区。仓库应保持干净(无 commit),
    /// baseline 由首次 `ensure_baseline` 显式触发。
    #[test]
    fn init_does_not_commit_workspace() {
        // 注:CI 环境若无 git 可执行,跳过(避免在无 git 容器里 false fail)。
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }

        // 修复:macOS sandbox / TCC 下 `~/.movix` 与 `/var/folders/...` 都可能
        // 不可写,把 work_tree 与 git_dir 都锚到项目内 `target/test-tmp/`。
        let anchor = std::env::current_dir()
            .unwrap()
            .join("target")
            .join("test-tmp");
        std::fs::create_dir_all(&anchor).unwrap();
        let tmp_dir = tempfile::Builder::new()
            .prefix("movix_snap_")
            .tempdir_in(&anchor)
            .unwrap();
        let tmp = tmp_dir.path().to_path_buf();
        // 放一个大文件,确保如果旧逻辑把它 add 进去会被测出来。
        std::fs::write(tmp.join("big.bin"), vec![0u8; 64 * 1024]).unwrap();

        let git_dir = tmp.join(".movix-snapshots").join(".git");
        let repo = SnapshotRepo::with_git_dir(&tmp, git_dir);
        repo.init().unwrap();

        // 没有 commit
        assert!(!repo.has_any_commit(), "init 不应触发 baseline commit");
        assert!(repo.list().unwrap().is_empty());

        // ensure_baseline 之后才有 commit
        repo.ensure_baseline().unwrap();
        assert!(repo.has_any_commit());
        assert_eq!(repo.list().unwrap().len(), 1);

        // 幂等:重复 ensure_baseline 不再产生新 commit
        repo.ensure_baseline().unwrap();
        assert_eq!(repo.list().unwrap().len(), 1);

        // git_dir 在 tmp_dir 内,自动随 tempdir drop 一起清掉
        drop(tmp_dir);
    }
}

// ============================================================================
// P2:工作区变更管理 —— 合并自原 auto_snapshot.rs。
// 统一一个结构,承担"自动快照 / 失败回滚 / 修改文件跟踪 / 手动快照"四件事。
// ============================================================================

/// 自动快照触发原因
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoSnapshotReason {
    /// 文件写入前
    BeforeWrite,
    /// 文件删除前
    BeforeDelete,
    /// 批量修改前
    BeforeBatchEdit,
    /// 危险命令执行前
    BeforeDangerousCommand,
}

impl AutoSnapshotReason {
    pub fn label(&self) -> &'static str {
        match self {
            Self::BeforeWrite => "auto: before write",
            Self::BeforeDelete => "auto: before delete",
            Self::BeforeBatchEdit => "auto: before batch edit",
            Self::BeforeDangerousCommand => "auto: before dangerous command",
        }
    }
}

/// 回滚策略
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RollbackStrategy {
    /// 不自动回滚
    None,
    /// 工具执行失败时自动回滚
    OnToolFailure,
    /// 任何错误时自动回滚
    OnAnyError,
}

/// 回滚决策
#[derive(Debug, Clone)]
pub enum RollbackDecision {
    /// 不需要回滚
    NoRollback,
    /// 需要回滚到指定快照
    Rollback(SnapshotId),
}

/// 工作区变更管理器。统一管理:
/// - 自动快照栈(支持嵌套)
/// - 失败时按策略回滚
/// - 修改文件跟踪(供 verify / LSP / 审计用)
/// - 手动快照与恢复
pub struct WorkspaceMutationManager {
    repo: SnapshotRepo,
    /// 是否启用自动快照
    enabled: bool,
    /// 回滚策略
    rollback_strategy: RollbackStrategy,
    /// 当前活跃的快照栈(支持嵌套)
    snapshot_stack: Mutex<Vec<SnapshotId>>,
    /// 自上次快照以来修改的文件列表
    modified_files: Mutex<Vec<PathBuf>>,
    /// 最大自动快照数
    max_auto_snapshots: usize,
    /// 修复(G-M4):累计快照创建次数,用于定期触发 git gc 回收旧对象。
    total_snapshots: Mutex<u64>,
}

impl WorkspaceMutationManager {
    pub fn new(workspace: &Path) -> Self {
        Self {
            repo: SnapshotRepo::new(workspace),
            enabled: true,
            rollback_strategy: RollbackStrategy::OnToolFailure,
            snapshot_stack: Mutex::new(Vec::new()),
            modified_files: Mutex::new(Vec::new()),
            max_auto_snapshots: 50,
            total_snapshots: Mutex::new(0),
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn set_rollback_strategy(&mut self, strategy: RollbackStrategy) {
        self.rollback_strategy = strategy;
    }

    pub fn init(&self) -> std::io::Result<()> {
        self.repo.init()
    }

    /// 在修改操作前自动创建快照
    pub fn before_modification(
        &self,
        reason: AutoSnapshotReason,
        files: &[PathBuf],
    ) -> std::io::Result<Option<SnapshotId>> {
        if !self.enabled {
            return Ok(None);
        }

        if !self.repo.is_initialized() {
            self.repo.init()?;
        }
        // 修复(P1.1):init 不再吃整库,首次需要 snapshot 时才补一份 baseline。
        // 后续 `snapshot()` 会基于 baseline 做增量提交,不会重复拷贝整库。
        self.repo.ensure_baseline()?;

        {
            let mut stack = self
                .snapshot_stack
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            // 修复(Critical #C5):栈满后原实现直接 return Ok(None),静默丢弃新快照,
            // 长会话后半段所有"失败可回滚"承诺失效。改为 FIFO 弹出最旧快照腾出空间,
            // 并记录告警,保证快照功能在长会话持续可用。
            while stack.len() >= self.max_auto_snapshots {
                let evicted = stack.remove(0);
                tracing::warn!(
                    target: "snapshot",
                    "自动快照栈已满({}),FIFO 弹出最旧快照 {} 以腾出空间",
                    self.max_auto_snapshots,
                    evicted.as_str()
                );
            }
        }

        let label = reason.label();
        let snapshot_id = self.repo.snapshot(label)?;

        self.snapshot_stack
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(snapshot_id.clone());

        // 修复(G-M4):定期 git gc 回收旧快照对象。FIFO 弹出只清栈,git 对象库
        // 仍累积。每 50 次快照触发一次 gc,防止长会话磁盘膨胀。
        {
            let mut count = self
                .total_snapshots
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *count += 1;
            if (*count).is_multiple_of(50) {
                tracing::info!(target: "snapshot", "触发 git gc(累计 {} 次快照)", *count);
                if let Err(e) = self.repo.gc() {
                    tracing::warn!(target: "snapshot", "git gc 失败(已忽略): {}", e);
                }
            }
        }

        let mut modified = self
            .modified_files
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for f in files {
            if !modified.contains(f) {
                modified.push(f.clone());
            }
        }

        Ok(Some(snapshot_id))
    }

    /// 标记操作成功,弹出快照栈顶
    pub fn on_success(&self) -> Option<SnapshotId> {
        self.snapshot_stack
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop()
    }

    /// 标记操作失败,根据策略决定是否回滚
    pub fn on_failure(&self, _error: &str) -> RollbackDecision {
        match self.rollback_strategy {
            RollbackStrategy::None => RollbackDecision::NoRollback,
            RollbackStrategy::OnToolFailure | RollbackStrategy::OnAnyError => {
                let mut stack = self
                    .snapshot_stack
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if let Some(snapshot_id) = stack.pop() {
                    RollbackDecision::Rollback(snapshot_id)
                } else {
                    RollbackDecision::NoRollback
                }
            }
        }
    }

    pub fn rollback(&self, snapshot_id: &SnapshotId) -> std::io::Result<()> {
        // 修复(R6/snapshot-restore):只回滚 agent 实际修改过的文件,避免 `git checkout -- .`
        // 丢弃用户在无关文件上的未提交修改。
        let modified = self.get_modified_files();
        // 修复(审查,数据丢失):`restore_paths` 在 paths 为空时会退化为**全量**
        // `git checkout -- .`,把工作区所有已跟踪未提交改动(含用户编辑器并发写入)
        // 一并回滚。自动回滚由"首个失败命令"(如被沙箱拦截、cargo build 失败)触发,
        // 此时 modified_files 常为空 → 静默丢弃用户全部改动。改为:自动回滚在找不到
        // 具体修改文件时**拒绝执行**(fail-close),而不是 fallback 到全量。
        if modified.is_empty() {
            tracing::warn!(
                target: "auto_snapshot",
                "自动回滚跳过:没有可定位的已修改文件(避免全量 git checkout 丢弃用户改动)"
            );
            return Ok(());
        }
        self.repo.restore_paths(snapshot_id, &modified)
    }

    /// 判断工具调用是否需要自动快照。
    /// 临时保留基于工具名的白名单,P3 可改为消费 [`crate::tools::EffectKind::mutates_workspace`]
    /// + `requires_approval_hint` —— 用工具的 effect_kind 统一决定。
    pub fn should_snapshot_for_tool(&self, tool_name: &str) -> Option<AutoSnapshotReason> {
        if !self.enabled {
            return None;
        }

        match tool_name {
            "write_file" | "create_file" | "patch_file" => Some(AutoSnapshotReason::BeforeWrite),
            "delete_file" | "delete_files" => Some(AutoSnapshotReason::BeforeDelete),
            "search_and_replace" | "edit_file" => Some(AutoSnapshotReason::BeforeWrite),
            "run_command" => Some(AutoSnapshotReason::BeforeDangerousCommand),
            _ => None,
        }
    }

    /// 新接口:基于 Tool 元数据(不依赖工具名)判断是否需要快照。
    /// P3 应当切到这里。
    pub fn should_snapshot_for_effect(
        &self,
        effect: crate::tools::EffectKind,
    ) -> Option<AutoSnapshotReason> {
        if !self.enabled {
            return None;
        }
        match effect {
            crate::tools::EffectKind::WorkspaceWrite => Some(AutoSnapshotReason::BeforeWrite),
            crate::tools::EffectKind::Command => Some(AutoSnapshotReason::BeforeDangerousCommand),
            crate::tools::EffectKind::Composite => Some(AutoSnapshotReason::BeforeBatchEdit),
            _ => None,
        }
    }

    pub fn stack_depth(&self) -> usize {
        self.snapshot_stack
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    pub fn get_modified_files(&self) -> Vec<PathBuf> {
        self.modified_files
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn clear_modified_files(&self) {
        self.modified_files
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// 记录已修改文件(供 verify / LSP / 审计用)。
    /// 不一定伴随快照(快照由 before_modification 触发,这里只跟踪文件列表)。
    pub fn record_modified(&self, files: &[PathBuf]) {
        let mut modified = self
            .modified_files
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for f in files {
            if !modified.contains(f) {
                modified.push(f.clone());
            }
        }
    }

    pub fn list_snapshots(&self) -> std::io::Result<Vec<Snapshot>> {
        self.repo.list()
    }

    pub fn manual_snapshot(&self, label: &str) -> std::io::Result<SnapshotId> {
        if !self.repo.is_initialized() {
            self.repo.init()?;
        }
        self.repo.snapshot(label)
    }

    pub fn manual_restore(&self, snapshot_id: &SnapshotId) -> std::io::Result<()> {
        self.repo.restore(snapshot_id)
    }
}

#[cfg(test)]
mod mutation_tests {
    use super::*;

    #[test]
    fn reason_label_covers_all_variants() {
        // 防止后续加新变体但忘了对应 label。
        for r in [
            AutoSnapshotReason::BeforeWrite,
            AutoSnapshotReason::BeforeDelete,
            AutoSnapshotReason::BeforeBatchEdit,
            AutoSnapshotReason::BeforeDangerousCommand,
        ] {
            assert!(!r.label().is_empty());
        }
    }

    #[test]
    fn should_snapshot_for_effect_respects_enabled_flag() {
        let mgr = WorkspaceMutationManager::new(Path::new("/tmp/_movix_test_disabled"));
        // 默认 enabled = true
        assert!(matches!(
            mgr.should_snapshot_for_effect(crate::tools::EffectKind::WorkspaceWrite),
            Some(AutoSnapshotReason::BeforeWrite)
        ));
        assert!(matches!(
            mgr.should_snapshot_for_effect(crate::tools::EffectKind::Command),
            Some(AutoSnapshotReason::BeforeDangerousCommand)
        ));
        // ReadOnly 不需要快照
        assert!(
            mgr.should_snapshot_for_effect(crate::tools::EffectKind::ReadOnly)
                .is_none()
        );
        // Network 不在快照范围(网络副作用是另一回事,不在工作区)
        assert!(
            mgr.should_snapshot_for_effect(crate::tools::EffectKind::Network)
                .is_none()
        );
    }
}
