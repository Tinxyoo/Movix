//! DeepSeek 定价 —— 加载 / 计算 / 格式化 / 官方抓取
//!
//! 定价从 `~/.movix/pricing.toml` 加载,缺失时回退内置默认值。
//! `/pricing` 命令从 https://api-docs.deepseek.com/zh-cn/quick_start/pricing/ 抓取并落盘。
//! deepseek-chat / deepseek-reasoner 将于 2026/07/24 弃用,按 v4-flash 计价做兼容。

use crate::common::deepseek::TokenStats;
use anyhow::{Context, Result};
use regex::Regex;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 单个模型的单价(人民币 ¥,每百万 tokens)
#[derive(Debug, Clone, Copy)]
struct CurrencyPricing {
    hit: f64,
    miss: f64,
    output: f64,
}

/// 从官方页面解析的 v4 系列全套定价
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OfficialPricing {
    pub v4_flash_hit: f64,
    pub v4_flash_miss: f64,
    pub v4_flash_output: f64,
    pub v4_pro_hit: f64,
    pub v4_pro_miss: f64,
    pub v4_pro_output: f64,
}

/// 内置默认值(2026-06 官方价)
const DEFAULT_PRICING: OfficialPricing = OfficialPricing {
    v4_flash_hit: 0.02,
    v4_flash_miss: 1.0,
    v4_flash_output: 2.0,
    v4_pro_hit: 0.025,
    v4_pro_miss: 3.0,
    v4_pro_output: 6.0,
};

impl OfficialPricing {
    pub fn to_display_table(&self) -> String {
        format!(
            "| 模型 | 命中 ¥/M | 未命中 ¥/M | 输出 ¥/M |\n\
             |------|----------|------------|----------|\n\
             | deepseek-v4-flash | {:.4} | {:.4} | {:.4} |\n\
             | deepseek-v4-pro  | {:.4} | {:.3}  | {:.3}  |",
            self.v4_flash_hit,
            self.v4_flash_miss,
            self.v4_flash_output,
            self.v4_pro_hit,
            self.v4_pro_miss,
            self.v4_pro_output,
        )
    }

    pub fn to_toml(&self) -> String {
        // 修复(浮点序列化):此前用裸 `{}` 格式化价格,整数价(如 3.0)会被写成 `3`,
        // TOML 解析为整数,而 load_pricing_from_config 用 as_float() 读取会返回 None,
        // 导致用户落盘的 pricing.toml 反而加载失败、静默回退默认值。
        // 改用显式浮点格式,保证输出始终带小数点。
        format!(
            r#"# Movix 定价配置文件
# 由 `/pricing` 命令生成于 {ts}。来源: {url}
# 手动修改后重启 Movix 生效。

[models.deepseek-v4-flash]
input_cache_hit_per_million_cny = {fh:?}
input_cache_miss_per_million_cny = {fm:?}
output_per_million_cny = {fo:?}

[models.deepseek-v4-pro]
input_cache_hit_per_million_cny = {ph:?}
input_cache_miss_per_million_cny = {pm:?}
output_per_million_cny = {po:?}

[models.deepseek-chat]
input_cache_hit_per_million_cny = {fh:?}
input_cache_miss_per_million_cny = {fm:?}
output_per_million_cny = {fo:?}
"#,
            ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
            url = "https://api-docs.deepseek.com/zh-cn/quick_start/pricing/",
            fh = self.v4_flash_hit,
            fm = self.v4_flash_miss,
            fo = self.v4_flash_output,
            ph = self.v4_pro_hit,
            pm = self.v4_pro_miss,
            po = self.v4_pro_output,
        )
    }
}
// ── 配置加载 ──

/// 从 `~/.movix/pricing.toml` 加载模型定价
fn load_pricing_from_config(model: &str) -> Option<CurrencyPricing> {
    let path = crate::common::utils::home_dir().join(".movix/pricing.toml");
    let doc: toml::Table = std::fs::read_to_string(&path).ok()?.parse().ok()?;
    let s = doc.get("models")?.get(model)?;
    let hit = s.get("input_cache_hit_per_million_cny")?.as_float()?;
    let miss = s.get("input_cache_miss_per_million_cny")?.as_float()?;
    let output = s.get("output_per_million_cny")?.as_float()?;
    // 修复(M-pricing):拒绝负价。负价会让 calculate_cost 返回负数污染成本统计。
    // 静默回退到内置默认(None → 调用方用 fallback),并记录告警便于排查配置错误。
    if hit < 0.0 || miss < 0.0 || output < 0.0 {
        tracing::warn!(
            target: "pricing",
            "pricing.toml 中模型 {} 含负价 (hit={}, miss={}, output={}),回退到内置默认",
            model, hit, miss, output
        );
        return None;
    }
    Some(CurrencyPricing { hit, miss, output })
}

/// 模型→单价(优先配置文件,回退内置值)
fn pricing_for_model(model: &str) -> Option<CurrencyPricing> {
    load_pricing_from_config(model).or_else(|| {
        let lower = model.to_lowercase();
        if !lower.contains("deepseek") {
            return None;
        }
        // 修复(R5/H12):原实现对任何含 v4-pro 且不含 flash 的模型(含 deepseek-v4-pro-max)
        // 都回退到 pro 价格。但 ProMax 是更高价档(cost_efficiency=0.25 vs pro 的 0.5),
        // 静默按 pro 计价会少报成本 → 预算守卫滞后。现对 pro-max 显式告警并提示用户在
        // pricing.toml 配置准确单价;在配置缺失前仍用 pro 价格作为保守下限(不返回 None,
        // 避免成本显示为 0 更误导)。
        if lower.contains("pro-max") || lower.contains("promax") {
            tracing::warn!(
                target: "pricing",
                "模型 {} 疑似 ProMax 高价档,但 pricing.toml 未配置其单价,暂按 pro 价格估算(可能少报)。\
                 请在 ~/.movix/pricing.toml [models.{}] 段配置准确单价",
                model, model
            );
        }
        // v4-flash 或 chat(即将弃用) → flash 价格; v4-pro(含 pro-max) → pro 价格
        let p = &DEFAULT_PRICING;
        let (h, m, o) = if lower.contains("v4-pro") && !lower.contains("flash") {
            (p.v4_pro_hit, p.v4_pro_miss, p.v4_pro_output)
        } else {
            (p.v4_flash_hit, p.v4_flash_miss, p.v4_flash_output)
        };
        Some(CurrencyPricing {
            hit: h,
            miss: m,
            output: o,
        })
    })
}

/// 生成默认定价配置文件(如不存在)
pub fn ensure_default_pricing_config(config_dir: &Path) -> bool {
    let path = config_dir.join("pricing.toml");
    if path.exists() {
        return false;
    }
    std::fs::write(&path, DEFAULT_PRICING.to_toml()).is_ok()
}

// ── 官方抓取 ──

const OFFICIAL_PRICING_URL: &str = "https://api-docs.deepseek.com/zh-cn/quick_start/pricing/";

/// 从 HTML 中解析定价表
fn parse_pricing_table(html: &str) -> Result<OfficialPricing> {
    let hit = Regex::new(
        r"百万tokens[（(]缓存命中[)）][\s\S]{0,400}?([\d.]+)\s*元[\s\S]{0,400}?([\d.]+)\s*元",
    )?;
    let miss = Regex::new(
        r"百万tokens[（(]缓存未命中[)）][\s\S]{0,400}?([\d.]+)\s*元[\s\S]{0,400}?([\d.]+)\s*元",
    )?;
    let out = Regex::new(r"百万tokens输出[\s\S]{0,400}?([\d.]+)\s*元[\s\S]{0,400}?([\d.]+)\s*元")?;

    let caps = |re: Regex| -> Result<[f64; 2]> {
        let caps = re
            .captures(html)
            .context("未找到定价行——官方页结构可能已变更")?;
        Ok([caps[1].parse::<f64>()?, caps[2].parse::<f64>()?])
    };

    let h = caps(hit)?;
    let m = caps(miss)?;
    let o = caps(out)?;
    Ok(OfficialPricing {
        v4_flash_hit: h[0],
        v4_pro_hit: h[1],
        v4_flash_miss: m[0],
        v4_pro_miss: m[1],
        v4_flash_output: o[0],
        v4_pro_output: o[1],
    })
}

/// 一站式:抓取+写入,供 `/pricing` 命令调用
pub fn refresh_from_official() -> Result<(OfficialPricing, PathBuf)> {
    let p = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(8))
        .user_agent(concat!(
            "Movix/",
            env!("CARGO_PKG_VERSION"),
            " (+pricing refresh)"
        ))
        .build()?
        .get(OFFICIAL_PRICING_URL)
        .send()?
        .text()?;
    let pricing = parse_pricing_table(&p)?;

    let dir = crate::common::utils::home_dir().join(".movix");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("pricing.toml");
    std::fs::write(&path, pricing.to_toml())?;
    Ok((pricing, path))
}

// ── 费用计算 ──

pub fn calculate_cost(stats: &TokenStats, model: &str) -> f64 {
    let Some(p) = pricing_for_model(model) else {
        return 0.0;
    };
    // 修复(M-pricing):pricing.toml 接受负价(as_float 不拒负数),负价可让 calculate_cost
    // 返回负数,污染成本统计。这里把负价当 0(免费),保证成本非负。
    let hit = p.hit.max(0.0);
    let miss = p.miss.max(0.0);
    let output = p.output.max(0.0);
    let (ch, cm) = (
        stats.cache_hit_tokens as f64 / 1_000_000.0,
        stats.cache_miss_tokens as f64 / 1_000_000.0,
    );
    let nc = (stats.prompt_tokens as f64 / 1_000_000.0 - ch - cm).max(0.0);
    let out = stats.completion_tokens as f64 / 1_000_000.0;
    let cost = ch * hit + (cm + nc) * miss + out * output;
    // 兜底:任何浮点异常导致负值,归零。
    if cost.is_finite() && cost >= 0.0 {
        cost
    } else {
        0.0
    }
}

pub fn format_cost(cny: f64) -> String {
    if cny < 0.01 {
        format!("¥{:.6}", cny)
    } else if cny < 1.0 {
        format!("¥{:.4}", cny)
    } else {
        format!("¥{:.2}", cny)
    }
}

pub fn cost_badge(cny: f64) -> (&'static str, &'static str) {
    if cny < 0.35 {
        ("low", "green")
    } else if cny < 1.40 {
        ("medium", "yellow")
    } else {
        ("high", "red")
    }
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_flash_cost() {
        let s = TokenStats {
            prompt_tokens: 100_000,
            completion_tokens: 10_000,
            total_tokens: 110_000,
            cache_hit_tokens: 80_000,
            cache_miss_tokens: 20_000,
            reasoning_tokens: 0,
        };
        assert!((calculate_cost(&s, "deepseek-v4-flash") - 0.0416).abs() < 1e-6);
        assert!((calculate_cost(&s, "deepseek-v4-pro") - 0.122).abs() < 1e-6);
        assert_eq!(calculate_cost(&s, "gpt-4"), 0.0);
    }

    #[test]
    fn test_badge_and_format() {
        assert_eq!(cost_badge(0.07).0, "low");
        assert_eq!(cost_badge(0.70).0, "medium");
        assert_eq!(cost_badge(3.50).0, "high");
        assert!(format_cost(0.0001).starts_with('¥'));
    }

    #[test]
    fn test_default_config_generation() {
        let dir = tempfile::tempdir().unwrap();
        assert!(ensure_default_pricing_config(dir.path()));
        assert!(!ensure_default_pricing_config(dir.path())); // 已存在
        let c = std::fs::read_to_string(dir.path().join("pricing.toml")).unwrap();
        assert!(c.contains("[models.deepseek-v4-pro]"));
        assert!(c.contains("[models.deepseek-chat]"));
    }

    #[test]
    fn test_parse_pricing_table() {
        // 注:正则按 DeepSeek 官网真实结构编写 —— "百万tokens"后直接跟括号,
        // 不含"输入"二字(输出行除外,输出行本就是"百万tokens输出")。
        // 此前测试 HTML 误加了"输入",导致 hit/miss 行匹配失败。
        let html = "<table><tr><td>百万tokens（缓存命中）</td><td>0.02元</td><td>0.025元</td></tr>\
                    <tr><td>百万tokens（缓存未命中）</td><td>1元</td><td>3元</td></tr>\
                    <tr><td>百万tokens输出</td><td>2元</td><td>6元</td></tr></table>";
        let p = parse_pricing_table(html).unwrap();
        assert_eq!(p.v4_flash_hit, 0.02);
        assert_eq!(p.v4_pro_output, 6.0);
        assert!(parse_pricing_table("<html>no</html>").is_err());
    }

    #[test]
    fn test_toml_round_trip() {
        let toml = DEFAULT_PRICING.to_toml();
        let v: toml::Table = toml.parse().unwrap();
        assert_eq!(
            v["models"]["deepseek-v4-pro"]["input_cache_miss_per_million_cny"]
                .as_float()
                .unwrap(),
            3.0
        );
    }

    #[test]
    fn test_display_table() {
        let t = DEFAULT_PRICING.to_display_table();
        assert!(t.starts_with("| 模型"));
        assert!(t.contains("deepseek-v4-flash"));
    }

    #[test]
    fn test_default_consistency() {
        let d = &DEFAULT_PRICING;
        let f = pricing_for_model("deepseek-v4-flash").unwrap();
        assert!((f.hit - d.v4_flash_hit).abs() < 1e-9);
        let p = pricing_for_model("deepseek-v4-pro").unwrap();
        assert!((p.miss - d.v4_pro_miss).abs() < 1e-9);
    }
}
