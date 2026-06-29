use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TlsBackend {
    Rustls,
    NativeTls,
}

impl Default for TlsBackend {
    fn default() -> Self {
        Self::Rustls
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ToolCompatibilityMode {
    ClaudeCode,
    Raw,
}

impl Default for ToolCompatibilityMode {
    fn default() -> Self {
        Self::ClaudeCode
    }
}

/// 429 重试策略模式。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RetryMode {
    Turbo,
    Fast,
    Balanced,
    Steady,
    Polite,
    Custom,
}

impl Default for RetryMode {
    fn default() -> Self {
        Self::Fast
    }
}

impl std::fmt::Display for RetryMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Turbo => "turbo",
            Self::Fast => "fast",
            Self::Balanced => "balanced",
            Self::Steady => "steady",
            Self::Polite => "polite",
            Self::Custom => "custom",
        };
        f.write_str(value)
    }
}

impl std::str::FromStr for RetryMode {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "turbo" => Ok(Self::Turbo),
            "fast" => Ok(Self::Fast),
            "balanced" => Ok(Self::Balanced),
            "steady" => Ok(Self::Steady),
            "polite" => Ok(Self::Polite),
            "custom" => Ok(Self::Custom),
            _ => anyhow::bail!("无效的重试模式: {}", value),
        }
    }
}

/// 普通 429 的可配置重试策略。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RetryPolicy {
    pub rate_limit_cooldown_ms: u64,
    pub max_request_retries: usize,
    pub base_backoff_ms: u64,
    pub max_backoff_ms: u64,
    #[serde(rename = "credentialSwitchOn429")]
    pub credential_switch_on_429: bool,
    pub respect_retry_after: bool,
}

impl RetryPolicy {
    pub fn preset(mode: RetryMode) -> Self {
        match mode {
            RetryMode::Turbo => Self {
                rate_limit_cooldown_ms: 1_000,
                max_request_retries: 12,
                base_backoff_ms: 100,
                max_backoff_ms: 1_000,
                credential_switch_on_429: true,
                respect_retry_after: false,
            },
            RetryMode::Fast => Self {
                rate_limit_cooldown_ms: 3_000,
                max_request_retries: 9,
                base_backoff_ms: 200,
                max_backoff_ms: 2_000,
                credential_switch_on_429: true,
                respect_retry_after: false,
            },
            RetryMode::Balanced => Self {
                rate_limit_cooldown_ms: 10_000,
                max_request_retries: 9,
                base_backoff_ms: 500,
                max_backoff_ms: 5_000,
                credential_switch_on_429: true,
                respect_retry_after: false,
            },
            RetryMode::Steady => Self {
                rate_limit_cooldown_ms: 30_000,
                max_request_retries: 6,
                base_backoff_ms: 1_000,
                max_backoff_ms: 10_000,
                credential_switch_on_429: true,
                respect_retry_after: true,
            },
            RetryMode::Polite => Self {
                rate_limit_cooldown_ms: 60_000,
                max_request_retries: 4,
                base_backoff_ms: 2_000,
                max_backoff_ms: 30_000,
                credential_switch_on_429: false,
                respect_retry_after: true,
            },
            RetryMode::Custom => Self::preset(RetryMode::Fast),
        }
    }

    pub fn effective(mode: RetryMode, custom: Option<&RetryPolicy>) -> anyhow::Result<Self> {
        let policy = if mode == RetryMode::Custom {
            custom
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("custom 模式需要 retryPolicy"))?
        } else {
            Self::preset(mode)
        };

        policy.validate()?;
        Ok(policy)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if !(500..=120_000).contains(&self.rate_limit_cooldown_ms) {
            anyhow::bail!("rateLimitCooldownMs 必须在 500..=120000 之间");
        }
        if !(1..=30).contains(&self.max_request_retries) {
            anyhow::bail!("maxRequestRetries 必须在 1..=30 之间");
        }
        if !(50..=30_000).contains(&self.base_backoff_ms) {
            anyhow::bail!("baseBackoffMs 必须在 50..=30000 之间");
        }
        if self.max_backoff_ms < self.base_backoff_ms || self.max_backoff_ms > 120_000 {
            anyhow::bail!("maxBackoffMs 必须在 baseBackoffMs..=120000 之间");
        }
        Ok(())
    }
}

fn default_true() -> bool {
    true
}

fn default_tool_result_max_chars() -> usize {
    8_000
}

fn default_tool_result_head_lines() -> usize {
    80
}

fn default_tool_result_tail_lines() -> usize {
    40
}

fn default_tool_use_input_max_chars() -> usize {
    6_000
}

fn default_tool_definition_max_bytes() -> usize {
    20 * 1024
}

/// 请求压缩配置。
///
/// 第一版只启用低风险瘦身项：空白压缩、tool_result 文本截断、
/// 历史 tool_use.input 大字符串截断、工具定义 schema/description 压缩。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CompressionConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,

    #[serde(default = "default_true")]
    pub whitespace_compression: bool,

    #[serde(default = "default_tool_result_max_chars")]
    pub tool_result_max_chars: usize,

    #[serde(default = "default_tool_result_head_lines")]
    pub tool_result_head_lines: usize,

    #[serde(default = "default_tool_result_tail_lines")]
    pub tool_result_tail_lines: usize,

    #[serde(default = "default_tool_use_input_max_chars")]
    pub tool_use_input_max_chars: usize,

    #[serde(default = "default_tool_definition_max_bytes")]
    pub tool_definition_max_bytes: usize,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            whitespace_compression: default_true(),
            tool_result_max_chars: default_tool_result_max_chars(),
            tool_result_head_lines: default_tool_result_head_lines(),
            tool_result_tail_lines: default_tool_result_tail_lines(),
            tool_use_input_max_chars: default_tool_use_input_max_chars(),
            tool_definition_max_bytes: default_tool_definition_max_bytes(),
        }
    }
}

/// KNA 应用配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default = "default_host")]
    pub host: String,

    #[serde(default = "default_port")]
    pub port: u16,

    #[serde(default = "default_region")]
    pub region: String,

    /// Auth Region（用于 Token 刷新），未配置时回退到 region
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_region: Option<String>,

    /// API Region（用于 API 请求），未配置时回退到 region
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_region: Option<String>,

    #[serde(default = "default_kiro_version")]
    pub kiro_version: String,

    #[serde(default)]
    pub machine_id: Option<String>,

    #[serde(default)]
    pub api_key: Option<String>,

    #[serde(default = "default_system_version")]
    pub system_version: String,

    #[serde(default = "default_node_version")]
    pub node_version: String,

    #[serde(default = "default_tls_backend")]
    pub tls_backend: TlsBackend,

    /// 外部 count_tokens API 地址（可选）
    #[serde(default)]
    pub count_tokens_api_url: Option<String>,

    /// count_tokens API 密钥（可选）
    #[serde(default)]
    pub count_tokens_api_key: Option<String>,

    /// count_tokens API 认证类型（可选，"x-api-key" 或 "bearer"，默认 "x-api-key"）
    #[serde(default = "default_count_tokens_auth_type")]
    pub count_tokens_auth_type: String,

    /// HTTP 代理地址（可选）
    /// 支持格式: http://host:port, https://host:port, socks5://host:port
    #[serde(default)]
    pub proxy_url: Option<String>,

    /// 代理认证用户名（可选）
    #[serde(default)]
    pub proxy_username: Option<String>,

    /// 代理认证密码（可选）
    #[serde(default)]
    pub proxy_password: Option<String>,

    /// Admin API 密钥（可选，启用 Admin API 功能）
    #[serde(default)]
    pub admin_api_key: Option<String>,

    /// 上一次成功更新前正在运行的版本号，用于在前端展示「回退到 vX.Y.Z」按钮。
    /// 实际回退动作通过 `<exe>.backup` 文件完成，无需访问网络。
    #[serde(default)]
    pub update_previous_version: Option<String>,

    /// GitHub Personal Access Token（可选）。设置后 GitHub Releases 接口会带上
    /// `Authorization: Bearer <token>`，把限流从匿名 60/h 提到认证 5000/h。
    /// 仅需 `public_repo` 读取权限即可。
    #[serde(default)]
    pub github_token: Option<String>,

    /// 上一次成功完成在线更新的时间（RFC3339）。前端用于显示「上次更新于 …」。
    #[serde(default)]
    pub update_last_applied_at: Option<String>,

    /// 是否启用无人值守自动更新。开启后服务会在每天的 `update_auto_apply_time`
    /// 时刻检查 GitHub Releases，发现新版本即自动下载二进制并替换重启。
    #[serde(default)]
    pub update_auto_apply: bool,

    /// 自动更新的每日触发时间（本地时区，`HH:MM` 24 小时制）。
    /// 默认 03:00 凌晨执行，对在线服务影响最小。
    #[serde(default = "default_update_auto_apply_time")]
    pub update_auto_apply_time: String,

    /// 负载均衡模式（"priority" 或 "balanced"）
    #[serde(default = "default_load_balancing_mode")]
    pub load_balancing_mode: String,

    /// 凭据默认并发上限；单个凭据未配置且无法从订阅等级推断时使用。
    #[serde(default = "default_concurrency_limit")]
    pub default_concurrency_limit: u32,

    /// 普通 429 重试策略模式。
    #[serde(default = "default_retry_mode")]
    pub retry_mode: RetryMode,

    /// 自定义普通 429 重试策略（retryMode=custom 时使用）。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_policy: Option<RetryPolicy>,

    /// 账号级 429 风控触发时是否对当前凭据进入冷却并故障转移（默认 true）。
    ///
    /// 关闭后：429 + suspicious activity 仍按普通瞬态错误重试，不切换凭据。
    /// 开启后：识别到 suspicious activity 字符串时，把当前凭据冷却 `account_throttle_cooldown_secs` 秒，
    /// 立即切换到下一个可用凭据。
    #[serde(default = "default_account_throttle_failover")]
    pub account_throttle_failover: bool,

    /// 账号级风控冷却时长（秒，默认 1800 = 30 分钟）。
    #[serde(default = "default_account_throttle_cooldown_secs")]
    pub account_throttle_cooldown_secs: u64,

    /// 是否开启非流式响应的 thinking 块提取（默认 true）
    ///
    /// 启用后，非流式响应中的 `<thinking>...</thinking>` 标签会被解析为
    /// 独立的 `{"type": "thinking", ...}` 内容块,与流式响应行为一致。
    #[serde(default = "default_extract_thinking")]
    pub extract_thinking: bool,

    /// 默认端点名称（凭据未显式指定 endpoint 时使用，默认 "ide"）
    #[serde(default = "default_endpoint")]
    pub default_endpoint: String,

    /// 工具兼容模式。默认将 Claude Code 工具名/参数适配为 Kiro 内置工具；
    /// "raw" 保留旧行为，直接透传客户端工具 schema，用于排障。
    #[serde(default = "default_tool_compatibility_mode")]
    pub tool_compatibility_mode: ToolCompatibilityMode,

    /// 请求压缩配置。第一版只包含低风险压缩，不做 history 截断或 thinking 丢弃。
    #[serde(default)]
    pub compression: CompressionConfig,

    /// 是否启用请求链路追踪（写 traces.db）。默认 true。
    ///
    /// 关闭后：不再写入 trace 记录、不走 TraceSink，但 `GET /api/admin/traces`
    /// 仍可查询历史已存记录。适合隐私敏感或磁盘紧张的场景。
    #[serde(default = "default_trace_enabled")]
    pub trace_enabled: bool,

    /// 请求链路追踪记录保留天数（默认 7）。后台任务每天清理超期记录。
    #[serde(default = "default_trace_retention_days")]
    pub trace_retention_days: u32,

    /// 请求用量日志（usage_log.*.jsonl + 聚合桶）保留天数（默认 31）。
    #[serde(default = "default_usage_log_retention_days")]
    pub usage_log_retention_days: u32,

    /// 端点特定的配置
    ///
    /// 键为端点名（如 "ide" / "cli"），值为该端点自由定义的参数对象。
    /// 未在此表出现的端点沿用实现内置默认值。
    #[serde(default)]
    pub endpoints: HashMap<String, serde_json::Value>,

    /// 配置文件路径（运行时元数据，不写入 JSON）
    #[serde(skip)]
    config_path: Option<PathBuf>,
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}

fn default_port() -> u16 {
    8080
}

fn default_region() -> String {
    "us-east-1".to_string()
}

fn default_kiro_version() -> String {
    "2.6.0".to_string()
}

fn default_system_version() -> String {
    "macos".to_string()
}

fn default_node_version() -> String {
    "22.22.0".to_string()
}

fn default_count_tokens_auth_type() -> String {
    "x-api-key".to_string()
}

fn default_tls_backend() -> TlsBackend {
    TlsBackend::Rustls
}

fn default_load_balancing_mode() -> String {
    "priority".to_string()
}

fn default_concurrency_limit() -> u32 {
    3
}

fn default_retry_mode() -> RetryMode {
    RetryMode::default()
}

fn default_account_throttle_failover() -> bool {
    true
}

fn default_account_throttle_cooldown_secs() -> u64 {
    30 * 60
}

fn default_update_auto_apply_time() -> String {
    "03:00".to_string()
}

fn default_extract_thinking() -> bool {
    true
}

fn default_endpoint() -> String {
    crate::kiro::endpoint::ide::IDE_ENDPOINT_NAME.to_string()
}

fn default_tool_compatibility_mode() -> ToolCompatibilityMode {
    ToolCompatibilityMode::ClaudeCode
}

fn default_trace_enabled() -> bool {
    true
}

fn default_trace_retention_days() -> u32 {
    7
}

fn default_usage_log_retention_days() -> u32 {
    31
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            region: default_region(),
            auth_region: None,
            api_region: None,
            kiro_version: default_kiro_version(),
            machine_id: None,
            api_key: None,
            system_version: default_system_version(),
            node_version: default_node_version(),
            tls_backend: default_tls_backend(),
            count_tokens_api_url: None,
            count_tokens_api_key: None,
            count_tokens_auth_type: default_count_tokens_auth_type(),
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            admin_api_key: None,
            update_previous_version: None,
            github_token: None,
            update_last_applied_at: None,
            update_auto_apply: false,
            update_auto_apply_time: default_update_auto_apply_time(),
            load_balancing_mode: default_load_balancing_mode(),
            default_concurrency_limit: default_concurrency_limit(),
            retry_mode: default_retry_mode(),
            retry_policy: None,
            account_throttle_failover: default_account_throttle_failover(),
            account_throttle_cooldown_secs: default_account_throttle_cooldown_secs(),
            extract_thinking: default_extract_thinking(),
            default_endpoint: default_endpoint(),
            tool_compatibility_mode: default_tool_compatibility_mode(),
            compression: CompressionConfig::default(),
            trace_enabled: default_trace_enabled(),
            trace_retention_days: default_trace_retention_days(),
            usage_log_retention_days: default_usage_log_retention_days(),
            endpoints: HashMap::new(),
            config_path: None,
        }
    }
}

impl Config {
    /// 获取默认配置文件路径
    pub fn default_config_path() -> &'static str {
        "config.json"
    }

    /// 获取有效的 Auth Region（用于 Token 刷新）
    /// 优先使用 auth_region，未配置时回退到 region
    pub fn effective_auth_region(&self) -> &str {
        self.auth_region.as_deref().unwrap_or(&self.region)
    }

    /// 获取有效的 API Region（用于 API 请求）
    /// 优先使用 api_region，未配置时回退到 region
    pub fn effective_api_region(&self) -> &str {
        self.api_region.as_deref().unwrap_or(&self.region)
    }

    /// 从文件加载配置
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            // 配置文件不存在，返回默认配置
            let mut config = Self::default();
            config.config_path = Some(path.to_path_buf());
            return Ok(config);
        }

        let content = fs::read_to_string(path)?;
        let mut config: Config = serde_json::from_str(&content)?;
        config.config_path = Some(path.to_path_buf());

        // 用户手工把字符串字段清空（如 `"updateAutoApplyTime": ""`）时，serde 默认值不会
        // 介入；这里把"看起来像空"的关键字段回退到默认值，避免后续业务用到
        // 空字符串导致难以诊断的错误。
        if config.update_auto_apply_time.trim().is_empty() {
            config.update_auto_apply_time = default_update_auto_apply_time();
        }

        Ok(config)
    }

    /// 获取配置文件路径（如果有）
    pub fn config_path(&self) -> Option<&Path> {
        self.config_path.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn set_config_path_for_test<P: Into<PathBuf>>(&mut self, path: P) {
        self.config_path = Some(path.into());
    }

    /// 将当前配置写回原始配置文件
    pub fn save(&self) -> anyhow::Result<()> {
        let path = self
            .config_path
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("配置文件路径未知，无法保存配置"))?;

        let content = serde_json::to_string_pretty(self).context("序列化配置失败")?;
        fs::write(path, content)
            .with_context(|| format!("写入配置文件失败: {}", path.display()))?;
        Ok(())
    }
}
