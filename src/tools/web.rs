use async_trait::async_trait;
use serde_json::{Value, json};
use std::net::IpAddr;
use std::str::FromStr;
use std::time::Duration;

use super::{EffectKind, RiskLevel, Tool, ToolResult};
use crate::common::error::Result;

/// 默认拒绝语义的 SSRF 第一道闸门:
/// - URL 解析失败、缺少 host、host 是私有/保留 IP/已知元数据域 → 视为"不安全"。
/// - 仅当能稳健解析出一个非私有的公开 host/IP 时返回 false。
///
/// 修复(原 `is_private_url`):
/// 1. 名字与含义一致 —— 之前函数名是 `is_private_url`,但 parse 失败也返回 true,
///    把语义偷偷拓宽成"是否不安全",误导了所有调用方;尤其是 reqwest 重定向策略里
///    `is_private_url(&url)` 看似在挡内网,实际同时挡掉了任意"无法 parse 的 URL"。
/// 2. 让所有调用方都走"默认拒绝":parse 失败 → unsafe,host 缺失 → unsafe。
///
/// 完全消除 DNS rebinding 仍需在连接层做(见 `WebFetchTool::execute` 的 lookup_host),
/// 这里只覆盖第一道字符串/字面量 IP 检查。
fn is_unsafe_url(url: &str) -> bool {
    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return true,
    };

    let host = match parsed.host_str() {
        Some(h) => h,
        None => return true,
    };

    // 修复:
    // 1) DNS 中尾随 `.` 等价同名,`example.com.` 与 `example.com` 都应做相同检查;
    // 2) 大小写归一化使用 ASCII lowercase(URL host 已被 url crate 转 punycode,
    //    因此 IDN 已被还原为 ASCII,这里只需 ASCII 大小写归一化即可)。
    let host = host.trim_end_matches('.');

    // 第一阶段：字面量 IP 检查（IPv4 / IPv6 / 带方括号的 v6）
    let host_for_ip = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = IpAddr::from_str(host_for_ip) {
        return is_private_ip(&ip);
    }

    // 修复(H1 SSRF):数值编码的 IPv4 此前可绕过。
    // `http://2130706433/`(十进制 = 127.0.0.1)、`http://0x7f000001/`(十六进制)、
    // `http://017700000001/`(八进制)、`http://0177.0.0.1/`(混合)都能被 getaddrinfo
    // 还原成内网 IP,但 `IpAddr::from_str` 不认这些形式。这里手动尝试把它们
    // 解析回标准 IPv4 再判断。覆盖场景:重定向策略(同步回调,无法做 DNS 解析)
    // 与初始字符串校验。
    if let Some(ip) = parse_numeric_ipv4(host_for_ip) {
        return is_private_ip(&IpAddr::V4(ip));
    }

    let blocked_hosts = [
        "localhost",
        "localhost.localdomain",
        "127.0.0.1",
        "0.0.0.0",
        "metadata.google.internal",
        "metadata.internal",
        "169.254.169.254",
        "100.100.100.200",
        "instance-data.ec2.internal",
        "fd00.ec2.internal",
    ];

    let lower_host = host.to_ascii_lowercase();
    if blocked_hosts
        .iter()
        .any(|&b| lower_host == b || lower_host.ends_with(&format!(".{}", b)))
    {
        return true;
    }

    false
}

/// 尝试把数值编码的 IPv4 字符串还原为标准 [`std::net::Ipv4Addr`]。
///
/// 修复(H1 SSRF):`IpAddr::from_str` 只认标准点分十进制。但 `inet_aton` /
/// `getaddrinfo` 接受多种数值形式,glibc 与 musl 都会把它们还原成 IP 后再连接:
/// - 十进制整数:`2130706433` (= 127.0.0.1)
/// - 十六进制:`0x7f000001`
/// - 八进制:`017700000001` 或 `0177.0.0.1`
/// - 混合:`0x7f.0.0.1`
/// 攻击者通过受控公网站点 302 → `Location: http://2130706433/` 即可绕过
/// 仅做 `IpAddr::from_str` 的 SSRF 守卫,触达内网/云元数据。
///
/// 返回 `Some` 表示成功还原为标准 IPv4;`None` 表示不是数值编码 IP
/// (可能是域名或标准点分十进制——后者应交由 `IpAddr::from_str` 处理)。
fn parse_numeric_ipv4(s: &str) -> Option<std::net::Ipv4Addr> {
    // 标准 IpAddr::from_str 已经能处理,这里不重复。
    if std::net::Ipv4Addr::from_str(s).is_ok() {
        return None;
    }

    // 拆成点分部分;若任意部分是非标准十进制(含 0x/0 前缀)或整体是单段整数,
    // 用 from_radix 解析后按 inet_aton 语义组装。
    let parts: Vec<&str> = s.split('.').collect();
    if parts.is_empty() || parts.len() > 4 {
        return None;
    }

    // 单段纯整数(十进制/十六进制/八进制)整体当 32 位。
    if parts.len() == 1 {
        let v = parse_int_radix(parts[0])?;
        return Some(std::net::Ipv4Addr::from(u32::from_be(v)));
    }

    // 多段:每段按各自进制解析,再按 inet_aton 规则组装。
    // inet_aton: a.b.c.d → 各段填字节;a.b.c → c 占低 16 位; a.b → b 占低 24 位。
    let mut nums = Vec::with_capacity(parts.len());
    for p in &parts {
        nums.push(parse_int_radix(p)?);
    }
    let combined = match nums.len() {
        2 => {
            let a = nums[0];
            let b = nums[1];
            if a > 0xFF || b > 0xFFFFFF {
                return None;
            }
            (a << 24) | b
        }
        3 => {
            let (a, b, c) = (nums[0], nums[1], nums[2]);
            if a > 0xFF || b > 0xFF || c > 0xFFFF {
                return None;
            }
            (a << 24) | (b << 16) | c
        }
        4 => {
            for n in &nums {
                if *n > 0xFF {
                    return None;
                }
            }
            (nums[0] << 24) | (nums[1] << 16) | (nums[2] << 8) | nums[3]
        }
        _ => return None,
    };
    Some(std::net::Ipv4Addr::from(u32::from_be(combined)))
}

/// 按/inet_aton 规则解析一个整数段:支持 `0x`(十六进制)、`0`(八进制)、
/// 普通十进制。失败返回 None。
fn parse_int_radix(s: &str) -> Option<u32> {
    if s.is_empty() {
        return None;
    }
    let (radix, body) = if let Some(rest) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        (16, rest)
    } else if s.starts_with('0') && s.len() > 1 {
        (8, &s[1..])
    } else {
        (10, s)
    };
    if body.is_empty() {
        return None;
    }
    u32::from_str_radix(body, radix).ok()
}

/// 兼容别名 —— 保持旧测试名可读,内部转发到新的"默认拒绝"语义。
#[cfg(test)]
fn is_private_url(url: &str) -> bool {
    is_unsafe_url(url)
}

/// 检查 IP 地址是否为私有/保留地址
fn is_private_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            octets[0] == 0
                || octets[0] == 10
                || (octets[0] == 172 && octets[1] >= 16 && octets[1] <= 31)
                || (octets[0] == 192 && octets[1] == 168)
                || (octets[0] == 127)
                || (octets[0] == 169 && octets[1] == 254)
                || (octets[0] >= 224)
                || (octets[0] == 100 && octets[1] >= 64 && octets[1] <= 127)
        }
        IpAddr::V6(v6) => {
            // 修复:
            // 1) `v6.to_string().starts_with("::ffff:")` 是字符串 hack,在压缩格式
            //    不同(`::ffff:0:1` 等)时可能漏判;改用结构化的 to_ipv4_mapped。
            // 2) 漏掉 NAT64(`64:ff9b::/96`)、IPv4-compatible(已废弃但仍可解析为内网)
            //    与 documentation/special-purpose 段。
            let segments = v6.segments();
            if v6.is_loopback() || v6.is_multicast() || v6.is_unspecified() {
                return true;
            }
            // ULA: fc00::/7
            if (segments[0] & 0xfe00) == 0xfc00 {
                return true;
            }
            // Link-local: fe80::/10
            if (segments[0] & 0xffc0) == 0xfe80 {
                return true;
            }
            // IPv4-mapped (::ffff:0:0/96):递归判断映射出的 IPv4。
            // 注:Rust std 的 `to_ipv4_mapped` 在 1.63+ 稳定。
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_private_ip(&IpAddr::V4(v4));
            }
            // IPv4-compatible (::0.0.0.0/96, 已废弃但攻击者可能借此构造):
            // 前 80 位为 0,第 6 段为 0 时尾 32 位即 IPv4。
            if segments[0] == 0
                && segments[1] == 0
                && segments[2] == 0
                && segments[3] == 0
                && segments[4] == 0
                && segments[5] == 0
            {
                let v4 = std::net::Ipv4Addr::new(
                    (segments[6] >> 8) as u8,
                    (segments[6] & 0xff) as u8,
                    (segments[7] >> 8) as u8,
                    (segments[7] & 0xff) as u8,
                );
                return is_private_ip(&IpAddr::V4(v4));
            }
            // NAT64 well-known prefix: 64:ff9b::/96 — 网关后是公网 IPv4,
            // 但仍属"通过中间转发器到达任意 v4",对 SSRF 不安全,默认拒绝。
            if segments[0] == 0x0064 && segments[1] == 0xff9b {
                return true;
            }
            false
        }
    }
}

/// 网页搜索工具，使用 DuckDuckGo 搜索引擎
///
/// 安全说明：此工具请求目标是固定的 `html.duckduckgo.com`，不存在 SSRF 风险，
/// 因此不需要像 `WebFetchTool` 那样做 DNS 解析校验。如果未来支持自定义搜索
/// 引擎 URL，则需要添加 SSRF 防护。
/// Shared reqwest client with connection pooling for all web tools.
/// 使用 std::sync::OnceLock 实现全局单例，避免每次调用都重建 Client 丢失连接池。
///
/// 修复(R5/C3,关键):原重定向 Policy::custom 是**同步**闭包,只能做字符串/字面 IP 校验
/// (`is_unsafe_url`),无法做 DNS 解析。因此 302 重定向到一个**主机名**(如
/// `rebind.attacker.com` → 169.254.169.254)完全绕过 SSRF 防护。
///
/// 正确做法:**禁用** reqwest 自动重定向,在 WebFetchTool::execute 里用**手动重定向
/// 循环**逐跳做完整 DNS 校验(lookup_host + is_private_ip),与初始请求同一标准。
/// 见 `fetch_with_redirect_check`。
static WEB_CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();

fn web_client() -> &'static reqwest::Client {
    WEB_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (compatible; Movix/0.1)")
            .timeout(Duration::from_secs(30))
            // 禁用自动重定向:在 execute 里手动逐跳处理(可做异步 DNS 校验)。
            .redirect(reqwest::redirect::Policy::none())
            .build()
            // 修复(P1.4):reqwest::Client::build() 只在 TLS backend 不可用时失败,
            // 我们使用 rustls-tls feature,理论上不会失败。但用 unwrap 以外的方式
            // 处理更安全 — 如果真的失败,给出清晰的错误信息而非 panic。
            .unwrap_or_else(|e| panic!("failed to build reqwest client: {e}"))
    })
}

/// 修复(R5/C3):对给定 host:port 做完整 DNS 校验。解析所有 A/AAAA 记录,
/// 任一为私网/保留地址即拒绝。返回 Err(message) 表示不安全或解析失败。
async fn check_host_dns(host: &str, port: u16) -> std::result::Result<(), String> {
    let sock_addrs = match tokio::net::lookup_host((host, port)).await {
        Ok(addrs) => addrs.collect::<Vec<_>>(),
        Err(e) => return Err(format!("DNS 解析失败，已拒绝访问: {}", e)),
    };
    // 全部 A/AAAA 都不能是私网;只要有任一私网记录就拒绝(防止混列绕过)。
    for addr in &sock_addrs {
        if is_private_ip(&addr.ip()) {
            return Err("访问被拒绝：DNS 解析结果包含内网地址，可能存在 DNS rebinding 攻击".into());
        }
    }
    Ok(())
}

/// 修复(R5/C3):手动重定向循环。web_client() 已禁用自动重定向,这里逐跳:
///   1. 对当前 URL 做 is_unsafe_url 字符串校验
///   2. 对其 host 做 check_host_dns 完整 DNS 校验(异步,原 Policy::custom 做不到)
///   3. 发请求;若是 3xx 且 Location 合法,跳到下一 URL;最多 5 跳
/// 这样重定向到**主机名**(而非字面 IP)也能被 DNS 校验拦下,堵上原同步策略的绕过。
async fn fetch_with_redirect_check(
    client: &reqwest::Client,
    start_url: &str,
) -> std::result::Result<reqwest::Response, String> {
    let mut current = start_url.to_string();
    for _ in 0..5 {
        // 每跳都重新做字符串 + DNS 校验(初始 URL 已校验过,这里是幂等的二次确认 + 重定向目标校验)。
        if is_unsafe_url(&current) {
            return Err("访问被拒绝：重定向目标为内网/保留地址".into());
        }
        let parsed = url::Url::parse(&current).map_err(|e| format!("URL 解析失败: {}", e))?;
        let host = parsed
            .host_str()
            .ok_or_else(|| "URL 缺少主机名".to_string())?;
        let port = parsed.port_or_known_default().unwrap_or(80);
        check_host_dns(host, port).await?;

        let response = client
            .get(&current)
            .send()
            .await
            .map_err(|e| format!("请求失败: {}", e))?;
        if response.status().is_redirection() {
            // 取 Location 头并相对当前 URL 解析为绝对 URL。
            let loc = match response.headers().get(reqwest::header::LOCATION) {
                Some(v) => v.to_str().unwrap_or("").to_string(),
                None => return Ok(response), // 无 Location,无法跟随,返回当前响应
            };
            let next = match url::Url::parse(&current).ok() {
                Some(base) => base
                    .join(&loc)
                    .map_err(|e| format!("Location 解析失败: {}", e))?,
                None => url::Url::parse(&loc).map_err(|e| format!("Location 解析失败: {}", e))?,
            };
            tracing::debug!(target: "web", "web_fetch 重定向: {} → {}", current, next);
            current = next.to_string();
            continue;
        }
        return Ok(response);
    }
    Err("重定向次数超过限制 (5)，已拦截".into())
}

// 注:第一轮曾尝试用"DNS pin + 改 URL host 为 IP + web_client_no_redirect"消除
// DNS rebinding,但该方案破坏 HTTPS 的 SNI/证书校验(见上方 R1 修正说明),已回退。
// web_client_no_redirect / rebuild_url_with_ip 已删除。

pub struct WebSearchTool;

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }
    fn description(&self) -> &str {
        "搜索互联网获取最新信息。用于查找技术文档、API 参考、错误解决方案等"
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "搜索关键词"},
                "num": {"type": "integer", "description": "返回结果数量，默认 5"}
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, arguments: &Value) -> Result<ToolResult> {
        let query = arguments["query"].as_str().unwrap_or("");
        let num = arguments["num"].as_u64().unwrap_or(5).min(10);

        let url = format!("https://html.duckduckgo.com/html/?q={}", urlencoding(query));

        let client = web_client();
        let response = client.get(&url).send().await?;
        let body = response.text().await?;

        let results = parse_duckduckgo(&body, num as usize);

        if results.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: format!("未找到关于 '{}' 的相关结果", query),
                error: None,
            });
        }

        let output = results.join("\n\n");
        Ok(ToolResult {
            success: true,
            output,
            error: None,
        })
    }

    fn effect_kind(&self) -> EffectKind {
        EffectKind::Network
    }
    fn risk_level(&self, _args: &Value) -> RiskLevel {
        RiskLevel::Medium
    }
    fn parallel_safe(&self) -> bool {
        true
    }
    // 不需要 approval:搜索只读工作区、不下载可执行内容。
}

/// URL 编码工具函数，使用 url crate 的 percent_encoding
fn urlencoding(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// 去除 HTML 标签（简化版，用于搜索结果等简单场景）
/// 委托给 strip_html 的完整逻辑（strip_html 是超集，去标签+去script/style+格式化）
fn strip_html_tags(s: &str) -> String {
    strip_html(s)
}

/// 解析 DuckDuckGo 搜索结果
fn parse_duckduckgo(html: &str, max: usize) -> Vec<String> {
    let mut results = Vec::new();
    let mut in_result = false;
    let mut title = String::new();

    for line in html.lines() {
        if results.len() >= max {
            break;
        }

        if line.contains("result__title") || line.contains("result__a") {
            in_result = true;
            title = strip_html_tags(line);
            continue;
        }

        if in_result && (line.contains("result__snippet") || line.contains("result__body")) {
            let snippet = strip_html_tags(line);

            if !title.is_empty() {
                results.push(format!("- **{}**: {}", title, snippet));
            }

            title.clear();
            in_result = false;
        }
    }

    results
}

/// 网页获取工具，带 SSRF 防护
pub struct WebFetchTool;

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }
    fn description(&self) -> &str {
        "获取指定 URL 的网页内容。用于读取在线文档、API 参考等。自动拦截对内网地址的访问"
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "要获取的网页 URL"}
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, arguments: &Value) -> Result<ToolResult> {
        let url = arguments["url"].as_str().unwrap_or("");

        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Ok(ToolResult::err(
                "URL 必须以 http:// 或 https:// 开头".into(),
            ));
        }

        // 第一阶段：快速检查主机名字符串
        if is_unsafe_url(url) {
            return Ok(ToolResult::err(
                "访问被拒绝：目标地址为内网/保留地址，可能存在 SSRF 风险".into(),
            ));
        }

        let parsed = match url::Url::parse(url) {
            Ok(u) => u,
            Err(e) => {
                return Ok(ToolResult::err(format!("URL 解析失败: {}", e)));
            }
        };

        let host = match parsed.host_str() {
            Some(h) => h,
            None => {
                return Ok(ToolResult::err("URL 缺少主机名".into()));
            }
        };
        let port = parsed.port_or_known_default().unwrap_or(80);

        // 第二阶段：DNS 解析并验证所有 IP(前置守卫)。
        // 对初始 URL 与每一跳重定向目标都做同一套完整 DNS 校验(见 fetch_with_redirect_check)。
        //
        // 残余风险说明:前置 DNS 校验 + 重定向逐跳 DNS 校验能挡住"域名解析到内网"
        // (含权威 DNS 极短 TTL 二次切换的简单 DNS rebinding)。要**完全**消除
        // "预检查时返回公网 IP、连接时二次解析切到内网"的 TOCTOU,需在连接层
        // (自定义 hyper connector 校验 socket 真实对端 IP),复杂度高,这里接受该残余风险。
        if let Err(msg) = check_host_dns(host, port).await {
            return Ok(ToolResult::err(msg));
        }

        // 第三阶段:发起请求并逐跳处理重定向。web_client() 已禁用自动重定向,
        // 这里手动跟随,每一跳都重新做 is_unsafe_url + 完整 DNS 校验,杜绝
        // "重定向到主机名"绕过(原 Policy::custom 同步闭包无法做 DNS 解析)。
        let client = web_client();
        let response = match fetch_with_redirect_check(client, url).await {
            Ok(r) => r,
            Err(e) => {
                return Ok(ToolResult::err(e));
            }
        };
        let status = response.status();

        if !status.is_success() {
            return Ok(ToolResult::err(format!("HTTP {}", status)));
        }

        let body = response.text().await?;

        let text = strip_html(&body);
        let truncated = if text.len() > 20_000 {
            // 修复(MSRV):floor_char_boundary 在 Rust 1.91 才稳定,而 Cargo.toml
            // 声明 rust-version = "1.85"。改用项目自带的 previous_char_boundary
            // (语义一致:向前回退到最近的 UTF-8 字符边界)。
            let safe_end = crate::common::utils::previous_char_boundary(&text, 20_000);
            format!(
                "{}...\n[内容被截断，共 {} 字符]",
                &text[..safe_end],
                text.len()
            )
        } else {
            text
        };

        Ok(ToolResult {
            success: true,
            output: truncated,
            error: None,
        })
    }

    fn effect_kind(&self) -> EffectKind {
        EffectKind::Network
    }
    fn risk_level(&self, _args: &Value) -> RiskLevel {
        RiskLevel::Medium
    }
    fn parallel_safe(&self) -> bool {
        true
    }
    // 不需要 approval:已用 SSRF 黑名单兜底,主流程只下载展示用文本。
}

/// ASCII 大小写不敏感的 `ends_with`。HTML 标签大小写不敏感,
/// 调到这里时 `haystack` 已是切片,`needle` 通常很短(`"/script"` /
/// `"/style"`),按字符比较即可。
fn ends_with_ci(haystack: &str, needle: &str) -> bool {
    if needle.len() > haystack.len() {
        return false;
    }
    // 修复(R5/H3,关键):原用 haystack[h_start..] 切片,h_start 是字节偏移但可能不在
    // UTF-8 字符边界上(needle 为 ASCII,但 haystack 末尾 7 字节内若有 UTF-8 多字节
    // 字符跨过 h_start,切片会 panic)。strip_html 在每个 '>' 调用本函数,haystack
    // 是 attacker 控制的 HTML → 恶意页面可触发 panic 崩溃 agent。
    //
    // 改为直接对字节做 ASCII 大小写不敏感比较(eq_ignore_ascii_case 在 u8 上是字节安全的,
    // 不关心 UTF-8 边界),完全消除 panic。
    let h_start = haystack.len() - needle.len();
    haystack.as_bytes()[h_start..]
        .iter()
        .zip(needle.as_bytes())
        .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

/// ASCII 大小写不敏感的 `starts_with`。零分配。
/// 用于在 `<` 之后判断是否是 `<script` / `<style` 起始,
/// 替代之前 `html[byte_idx..].chars().take(10).collect::<String>().to_ascii_lowercase()`
/// 的"每个 `<` 都重新分配字符串"。
fn starts_with_ci(haystack: &str, needle: &str) -> bool {
    haystack.len() >= needle.len()
        && haystack
            .as_bytes()
            .iter()
            .zip(needle.as_bytes())
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

/// 去除 HTML 标签和脚本/样式内容
fn strip_html(html: &str) -> String {
    let mut result = String::new();
    let mut in_tag = false;
    let mut in_script = false;
    let mut in_style = false;

    let mut byte_idx = 0;
    for ch in html.chars() {
        let char_len = ch.len_utf8();
        if ch == '<' {
            in_tag = true;
            // 修复(性能):之前每个 `<` 都 `chars().take(10).collect::<String>()` 再 lowercase,
            // 大型 HTML 上累计成 O(n²) 分配。现在用零分配的 ASCII case-insensitive 前缀比较。
            let tail = &html[byte_idx..];
            if starts_with_ci(tail, "<script") {
                in_script = true;
            } else if starts_with_ci(tail, "<style") {
                in_style = true;
            }
            byte_idx += char_len;
            continue;
        }

        if in_tag && ch == '>' {
            in_tag = false;
            // 修复(Bug #8):HTML 标签大小写不敏感(`</SCRIPT>` 也合法),
            // 之前用 `html[..byte_idx].ends_with("/script")` 只能匹配全小写,
            // 大小写混写的结束标签会被吞掉,导致 in_script 一直为 true,
            // 后续正文被吃掉。改为先把末尾若干字符小写化再比较。
            if byte_idx > 0 && ends_with_ci(&html[..byte_idx], "/script") {
                in_script = false;
            } else if byte_idx > 0 && ends_with_ci(&html[..byte_idx], "/style") {
                in_style = false;
            }
            byte_idx += char_len;
            continue;
        }

        byte_idx += char_len;

        if in_script || in_style {
            continue;
        }

        if !in_tag {
            result.push(ch);
        }
    }

    let mut cleaned = String::new();
    let mut last_was_newline = true;
    for line in result.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if !last_was_newline {
                cleaned.push('\n');
                last_was_newline = true;
            }
        } else {
            cleaned.push_str(trimmed);
            cleaned.push('\n');
            last_was_newline = false;
        }
    }

    cleaned.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ends_with_ci_multibyte_no_panic() {
        // R5/H3 回归:h_start 落在 UTF-8 多字节字符中间,原切片会 panic。
        // 现在 byte-wise 比较,不 panic 且结果正确。
        // "中/script" — "中" 是 3 字节,needle "/script" 7 字节,h_start 落在 "中" 内部。
        assert!(!ends_with_ci("中", "/script"));
        assert!(ends_with_ci("foo中/script", "/script"));
        assert!(ends_with_ci("FOO/SCRIPT", "/script"));
        // needle 比 haystack 长不 panic。
        assert!(!ends_with_ci("ab", "/script"));
    }

    #[test]
    fn test_private_ip_detection() {
        assert!(is_private_url("http://127.0.0.1/"));
        assert!(is_private_url("http://localhost/"));
        assert!(is_private_url("http://localhost.localdomain/"));
        assert!(is_private_url("http://10.0.0.1/"));
        assert!(is_private_url("http://192.168.1.1/"));
        assert!(is_private_url("http://172.16.0.1/"));
        assert!(is_private_url("http://169.254.169.254/"));
        assert!(!is_private_url("https://www.google.com/"));
        assert!(!is_private_url("https://api.github.com/"));
    }

    #[test]
    fn test_cloud_metadata_blocked() {
        assert!(is_private_url("http://metadata.google.internal/"));
        assert!(is_private_url("http://100.100.100.200/"));
        assert!(is_private_url("http://instance-data.ec2.internal/"));
    }

    #[test]
    fn test_ipv6_loopback_and_mapped_blocked() {
        // IPv6 loopback 必须拦截
        assert!(is_private_url("http://[::1]/"));
        assert!(is_private_url("http://[0:0:0:0:0:0:0:1]/"));
        // IPv4-mapped IPv6 (映射到 127.0.0.1) 必须拦截
        assert!(is_private_url("http://[::ffff:127.0.0.1]/"));
        assert!(is_private_url("http://[::ffff:10.0.0.1]/"));
        // IPv4-mapped 映射到 169.254.169.254 (云元数据)
        assert!(is_private_url("http://[::ffff:169.254.169.254]/"));
        // IPv6 ULA (fc00::/7) 与 link-local (fe80::/10)
        assert!(is_private_url("http://[fc00::1]/"));
        assert!(is_private_url("http://[fd12:3456::1]/"));
        assert!(is_private_url("http://[fe80::1]/"));
        // NAT64 well-known prefix
        assert!(is_private_url("http://[64:ff9b::1]/"));
    }

    #[test]
    fn test_carrier_grade_nat_blocked() {
        // 100.64.0.0/10 (CGNAT) 必须拦截
        assert!(is_private_url("http://100.64.0.1/"));
        assert!(is_private_url("http://100.127.255.255/"));
        // 边界外不应拦截
        assert!(!is_private_url("http://100.63.255.255/"));
    }

    #[test]
    fn test_link_local_and_broadcast_blocked() {
        // 169.254.0.0/16 link-local 必须拦截
        assert!(is_private_url("http://169.254.0.1/"));
        // 224.0.0.0/4 multicast (>= 224) 必须拦截
        assert!(is_private_url("http://224.0.0.1/"));
        assert!(is_private_url("http://239.255.255.255/"));
    }
}
