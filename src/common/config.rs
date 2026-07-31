use std::io::{self, Write};
use std::path::PathBuf;

use crate::common::deepseek::MAX_OUTPUT_TOKENS;

// 修复(Low #L4):原 derive(Debug) 会把明文 api_key 打到任何 {:?} / panic backtrace。
// 改为手写 Debug,对 api_key 输出脱敏占位,其余字段正常。
#[derive(Clone)]
pub struct MovixConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub max_iterations: u32,
    pub max_tokens: u32,
    pub workspace: PathBuf,
    pub log_level: String,
    pub thinking_enabled: bool,
    pub reasoning_effort: String,
    pub language: String,
    /// 启动时是否自动恢复上一次会话（默认 true）。
    pub auto_restore: bool,
}

impl std::fmt::Debug for MovixConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let masked = if self.api_key.is_empty() {
            "<empty>".to_string()
        } else {
            format!("{} chars", self.api_key.len())
        };
        f.debug_struct("MovixConfig")
            .field("api_key", &masked)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("max_iterations", &self.max_iterations)
            .field("max_tokens", &self.max_tokens)
            .field("workspace", &self.workspace)
            .field("log_level", &self.log_level)
            .field("thinking_enabled", &self.thinking_enabled)
            .field("reasoning_effort", &self.reasoning_effort)
            .field("language", &self.language)
            .field("auto_restore", &self.auto_restore)
            .finish()
    }
}

const ENV_TEMPLATE: &str = r#"DEEPSEEK_BASE_URL=https://api.deepseek.com
DEEPSEEK_MODEL=deepseek-v4-flash
MOVIX_MAX_ITERATIONS=30
MOVIX_MAX_TOKENS=393216
MOVIX_THINKING=enabled
MOVIX_REASONING_EFFORT=auto
"#;

impl MovixConfig {
    pub fn from_env() -> crate::common::error::Result<Self> {
        load_dotenv_smart();

        let api_key = std::env::var("DEEPSEEK_API_KEY").map_err(|_| {
            crate::common::error::MovixError::EnvError(
                "DEEPSEEK_API_KEY 未设置。\n\
                 请在项目目录下创建 .env 文件（可复制 .env.example）:\n\
                 echo 'DEEPSEEK_API_KEY=sk-your-key' > .env\n\
                 已搜索位置: 当前目录 和 程序所在目录"
                    .to_string(),
            )
        })?;

        Ok(Self::build(api_key))
    }

    pub fn from_env_optional() -> Self {
        load_dotenv_smart();
        let api_key = std::env::var("DEEPSEEK_API_KEY").unwrap_or_default();
        Self::build(api_key)
    }

    /// 交互式引导用户输入 API key 并写入 .env 文件
    pub fn prompt_api_key() -> Self {
        load_dotenv_smart();
        let existing_key = std::env::var("DEEPSEEK_API_KEY").unwrap_or_default();

        if !existing_key.is_empty() {
            return Self::build(existing_key);
        }

        println!();
        println!("  ▣  Movix — DeepSeek 专属Agent");
        println!();
        println!("  欢迎使用 Movix！需要配置 DeepSeek API Key 才能开始。");
        println!();
        println!("  获取 API Key: https://platform.deepseek.com/");
        println!();

        print!("  请输入 DeepSeek API Key: ");
        io::stdout().flush().ok();
        let mut key = String::new();
        io::stdin().read_line(&mut key).ok();

        let key = key.trim().to_string();
        if key.is_empty() {
            println!("  ⚠ 未输入 API Key，将使用空密钥启动（功能受限）");
            return Self::build(String::new());
        }

        // 修复(M12/R15/S15):原实现把 key 原样写进 .env,若含换行/`=`/引号会注入多行 .env。
        // 改为**白名单**:允许字母数字与常见 key 符号。
        // 修复(S15):R15 白名单只允许 [A-Za-z0-9\-_.],但 README 宣称 OpenAI 兼容,
        // 部分平台/代理签发的 token 是 base64(含 + / =)。加入这些 base64 安全字符
        // (它们不会破坏 dotenvy 的单行解析,因为不含换行/引号/#)。
        let valid = key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '/'));
        if !valid {
            eprintln!(
                "  ⚠ API Key 含非法字符(仅允许字母数字与 - _ . + /),拒绝写入 .env。将使用内存中的 key 启动。"
            );
            return Self::build(key);
        }

        // 写入 .env 文件
        // 修复(R5/C8,关键):原实现写到 cwd/.env,但 load_dotenv_smart 的信任模型只自动
        // 加载 ~/.movix/.env(把 cwd/.env 当不可信,需 MOVIX_TRUST_REPO_ENV=1)。
        // 结果新用户每次启动都要重输 key(注释 229 行还错误地声称"写到 ~/.movix/.env")。
        // 改为写到 ~/.movix/.env,与信任模型的加载路径一致。
        let movix_dir = crate::common::utils::home_dir().join(".movix");
        if !movix_dir.exists() {
            if let Err(e) = std::fs::create_dir_all(&movix_dir) {
                eprintln!("  ⚠ 无法创建目录 {}: {}", movix_dir.display(), e);
                return Self::build(key);
            }
        }
        let env_path = movix_dir.join(".env");

        let existing_content = if env_path.exists() {
            std::fs::read_to_string(&env_path).unwrap_or_default()
        } else {
            ENV_TEMPLATE.to_string()
        };

        let new_content = if existing_content.contains("DEEPSEEK_API_KEY=") {
            let re = regex::Regex::new(r"(?m)^DEEPSEEK_API_KEY=.*$").unwrap();
            re.replace(&existing_content, format!("DEEPSEEK_API_KEY={}", key))
                .to_string()
        } else {
            format!("DEEPSEEK_API_KEY={}\n{}", key, existing_content)
        };

        // 修复(审查):原实现先 fs::write(默认 0644)再 chmod 0600,存在竞态窗口
        // (写盘与 chmod 之间其他用户可读);chmod 失败也仅告警。改为创建时直接 0600。
        if let Err(e) = crate::common::utils::write_private(&env_path, &new_content) {
            eprintln!("  ⚠ 无法写入 .env 文件: {}", e);
        } else {
            println!("  ✓ API Key 已保存到 {}", env_path.display());
        }

        println!();

        Self::build(key)
    }

    fn build(api_key: String) -> Self {
        // 修复(H7,关键):原实现 `parse_env_u32` 返回任意 u32,下游 `.min(MAX_OUTPUT_TOKENS)`
        // 只防"过大"不防"过小"。恶意 .env(见 C1)可设 `MOVIX_MAX_TOKENS=0`,经
        // `0.min(393216)==0` 后把 `max_tokens=0` 发给 API,导致 400 或静默空回复,
        // 让 agent 在用户无感下失效。这里对 max_tokens / max_iterations 做双向 clamp。
        let max_iterations = parse_env_u32_clamped("MOVIX_MAX_ITERATIONS", 30, 1, 10_000);
        // 下限 256:任何低于此值的 max_tokens 都无意义(连一个 tool_call 都装不下);
        // 上限由 MAX_OUTPUT_TOKENS 兜底。min(config, MAX) 仍在 deepseek.rs 保留作第二道。
        let max_tokens = parse_env_u32_clamped(
            "MOVIX_MAX_TOKENS",
            MAX_OUTPUT_TOKENS,
            256,
            MAX_OUTPUT_TOKENS,
        );

        // 修复(R5/H14):base_url 无 scheme 校验。http:// 会让 Bearer token + 全部 prompt
        // 明文传输(可被中间人/日志窃取);file:// 或内网地址是 SSRF。默认 https://api.deepseek.com,
        // 用户自定义时校验 scheme:非 https 仅在显式 opt-in(MOVIX_ALLOW_INSECURE_ENDPOINT=1)时放行,
        // 否则告警并回退到默认(防 ~/.movix/.env 被篡改后静默泄露)。
        let raw_base_url = env_or("DEEPSEEK_BASE_URL", "https://api.deepseek.com");
        let allow_insecure = env_or("MOVIX_ALLOW_INSECURE_ENDPOINT", "0") == "1";
        let base_url = if raw_base_url.starts_with("https://") {
            raw_base_url
        } else if allow_insecure {
            eprintln!(
                "  ⚠ DEEPSEEK_BASE_URL='{}' 非 https,已因 MOVIX_ALLOW_INSECURE_ENDPOINT=1 放行。\
                 Bearer token 与 prompt 将明文传输,仅限受信内网/本地代理使用。",
                raw_base_url
            );
            raw_base_url
        } else {
            eprintln!(
                "  ⚠ DEEPSEEK_BASE_URL='{}' 非 https,已拒绝并回退到默认 https://api.deepseek.com \
                 (防明文泄露 Bearer token)。如需自定义非 https 端点(如本地代理),设置 \
                 MOVIX_ALLOW_INSECURE_ENDPOINT=1。",
                raw_base_url
            );
            "https://api.deepseek.com".to_string()
        };

        Self {
            api_key,
            base_url,
            model: env_or("DEEPSEEK_MODEL", "deepseek-v4-flash"),
            max_iterations,
            max_tokens,
            workspace: env_or("MOVIX_WORKSPACE", ".").into(),
            log_level: env_or("MOVIX_LOG_LEVEL", "info"),
            thinking_enabled: env_or("MOVIX_THINKING", "enabled").to_lowercase() != "disabled",
            reasoning_effort: env_or("MOVIX_REASONING_EFFORT", "auto"),
            language: env_or("MOVIX_LANGUAGE", "zh"),
            // 默认启动时自动恢复上次会话；设 MOVIX_AUTO_RESTORE=0 关闭。
            auto_restore: env_or("MOVIX_AUTO_RESTORE", "1") != "0",
        }
    }
}

fn env_or(key: &str, default: &'static str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn parse_env_u32(key: &str, default: u32) -> u32 {
    match std::env::var(key) {
        Ok(val) => match val.parse() {
            Ok(v) => v,
            Err(_) => {
                tracing::warn!(target: "config", "环境变量 {}=\"{}\" 不是有效数字，将使用默认值 {}", key, val, default);
                default
            }
        },
        Err(_) => default,
    }
}

/// 解析环境变量为 u32 并 clamp 到 [min, max] 区间。
/// 修复(H7):parse_env_u32 接受任意 u32,无法防御 0 / 极小值。下游 `.min(MAX)` 只防
/// 过大不防过小。此函数提供双向 clamp,用于有语义下限的数值(max_tokens/max_iterations)。
fn parse_env_u32_clamped(key: &str, default: u32, min: u32, max: u32) -> u32 {
    let raw = parse_env_u32(key, default);
    let clamped = raw.clamp(min, max);
    if clamped != raw {
        tracing::warn!(
            target: "config",
            "环境变量 {}={} 超出合法范围 [{}, {}],已 clamp 为 {}",
            key, raw, min, max, clamped
        );
    }
    clamped
}

fn load_dotenv_smart() {
    // 信任模型修复(C1,关键):
    //
    // 原实现第一步是 `dotenvy::dotenv()`,它会从当前工作目录向上爬到 git 根,
    // **命中即 return,永不读 `~/.movix/.env`**。对 AI 编程 Agent 而言,工作目录是
    // **不可信的外部输入**——它很可能就是被分析/被攻击的仓库。攻击者只需在公开仓库里
    // 放一个 `.env`(`DEEPSEEK_BASE_URL=http://attacker.tld`),用户 `git clone && cd
    // && movix` 即可让全部 LLM 流量(含 `Authorization: Bearer <key>`)发往攻击者。
    //
    // 正确的信任顺序:
    //   1. **`~/.movix/.env`(用户全局,可信)** —— 永远最先加载,这是 prompt_api_key()
    //      写入的位置,属于用户私有目录。
    //   2. **仓库本地 `.env`(不可信)** —— 仅在用户**显式**通过环境变量
    //      `MOVIX_TRUST_REPO_ENV=1` 授权后才加载,否则只警告。默认拒绝,避免克隆即中招。
    //
    // 注:dotenvy 已加载的变量不会被子进程继承问题影响——env::var 读的是进程级 env,
    // 后加载的不会覆盖先加载的(dotenvy 默认 `Not present` 策略),所以全局源先生效。

    // 1. 全局可信源:~/.movix/.env
    let home_dotenv = crate::common::utils::home_dir().join(".movix").join(".env");
    if home_dotenv.exists() {
        let _ = dotenvy::from_path(&home_dotenv);
    }

    // 2. 仓库本地 .env:默认拒绝,显式授权才加载
    let trust_repo_env = env_or("MOVIX_TRUST_REPO_ENV", "0") == "1";
    let cwd_dotenv = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".env");
    if cwd_dotenv.exists() {
        if trust_repo_env {
            // 用户已明确授权:加载仓库 .env(不覆盖已设置的全局变量)
            let _ = dotenvy::from_path(&cwd_dotenv);
        } else {
            // 默认拒绝,仅警告。告知用户存在该文件 + 如何启用。
            eprintln!(
                "  ⚠ 检测到工作区 .env 但未加载(防止恶意仓库劫持 API 配置)。\n\
                 \x20   若你确信该 .env 可信,设置环境变量 MOVIX_TRUST_REPO_ENV=1 后重启。"
            );
        }
    }

    // 历史修复(保留):原实现曾向 exe 祖先目录爬两级找 .env,在多用户系统上 movix
    // 二进制位于 ~/.cargo/bin/,会逐级访问 /Users、/,任何放在那些目录下的恶意 .env
    // 都会被读入。该路径已彻底移除,仅保留 ~/.movix 与(显式授权的)cwd 两个源。
}
