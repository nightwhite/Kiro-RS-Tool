//! Anthropic API 中间件

use std::sync::Arc;

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};
use parking_lot::RwLock;

use crate::admin::client_keys::SharedClientKeyManager;
use crate::admin::trace_db::SharedTraceStore;
use crate::admin::usage_stats::{SharedAggregator, SharedRecorder};
use crate::common::auth;
use crate::kiro::provider::KiroProvider;
use crate::model::config::{CompressionConfig, ToolCompatibilityMode};

use super::cache_metering::SharedCacheMeter;
use super::types::ErrorResponse;

/// 命中的鉴权上下文（注入到请求扩展，供 handler 记录用量）
#[derive(Clone, Copy, Debug)]
pub struct KeyContext {
    /// 命中的客户端 Key id；0 表示用 master apiKey 调用
    pub key_id: u64,
}

/// 应用共享状态
#[derive(Clone)]
pub struct AppState {
    /// API 密钥（运行时可修改，与 Admin 持久化共享）
    pub api_key: Arc<RwLock<String>>,
    /// Kiro Provider（可选，用于实际 API 调用）
    /// 内部使用 MultiTokenManager，已支持线程安全的多凭据管理
    pub kiro_provider: Option<Arc<KiroProvider>>,
    /// 是否开启非流式响应的 thinking 块提取
    pub extract_thinking: bool,
    /// 工具兼容模式
    pub tool_compatibility_mode: ToolCompatibilityMode,
    /// 请求压缩配置
    pub compression_config: Arc<RwLock<CompressionConfig>>,
    /// 客户端 Key 管理器（可选，未启用 Admin 时为 None）
    pub client_keys: Option<SharedClientKeyManager>,
    /// 用量日志记录器
    pub usage_recorder: Option<SharedRecorder>,
    /// 用量聚合器
    pub usage_aggregator: Option<SharedAggregator>,
    /// 中转层 cache meter（基于 cache_control 断点的缓存计量）
    pub cache_meter: Option<SharedCacheMeter>,
    /// 请求链路追踪存储（SQLite，可选）
    pub trace_store: Option<SharedTraceStore>,
}

impl AppState {
    /// 创建新的应用状态
    ///
    /// 默认入口通过 `with_provider` 注入 `KiroProvider`；这个简化构造函数留给
    /// 下游 lib 用户使用（e.g. 测试、嵌入到其他服务时）。
    #[allow(dead_code)]
    pub fn new(
        api_key: impl Into<String>,
        extract_thinking: bool,
        tool_compatibility_mode: ToolCompatibilityMode,
    ) -> Self {
        Self {
            api_key: Arc::new(RwLock::new(api_key.into())),
            kiro_provider: None,
            extract_thinking,
            tool_compatibility_mode,
            compression_config: Arc::new(RwLock::new(CompressionConfig::default())),
            client_keys: None,
            usage_recorder: None,
            usage_aggregator: None,
            cache_meter: None,
            trace_store: None,
        }
    }

    /// 使用现有 Arc 共享 api_key（用于与 Admin 模块共享同一份内存）
    pub fn with_shared_api_key(
        api_key: Arc<RwLock<String>>,
        extract_thinking: bool,
        tool_compatibility_mode: ToolCompatibilityMode,
    ) -> Self {
        Self {
            api_key,
            kiro_provider: None,
            extract_thinking,
            tool_compatibility_mode,
            compression_config: Arc::new(RwLock::new(CompressionConfig::default())),
            client_keys: None,
            usage_recorder: None,
            usage_aggregator: None,
            cache_meter: None,
            trace_store: None,
        }
    }

    /// 设置 KiroProvider
    pub fn with_kiro_provider(mut self, provider: Arc<KiroProvider>) -> Self {
        self.kiro_provider = Some(provider);
        self
    }

    /// 注入用量记录组件
    pub fn with_usage(
        mut self,
        client_keys: Option<SharedClientKeyManager>,
        recorder: Option<SharedRecorder>,
        aggregator: Option<SharedAggregator>,
    ) -> Self {
        self.client_keys = client_keys;
        self.usage_recorder = recorder;
        self.usage_aggregator = aggregator;
        self
    }

    /// 注入 CacheMeter
    pub fn with_cache_meter(mut self, cache: Option<SharedCacheMeter>) -> Self {
        self.cache_meter = cache;
        self
    }

    /// 注入请求压缩配置
    pub fn with_compression_config(mut self, config: Arc<RwLock<CompressionConfig>>) -> Self {
        self.compression_config = config;
        self
    }

    /// 注入链路追踪存储
    pub fn with_trace_store(mut self, store: Option<SharedTraceStore>) -> Self {
        self.trace_store = store;
        self
    }
}

/// API Key 认证中间件
///
/// 鉴权顺序：master apiKey → 客户端 Key（`csk_*`）。命中后向请求扩展注入
/// [`KeyContext`]，供 handler 记录用量时使用。
pub async fn auth_middleware(
    State(state): State<AppState>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let presented = match auth::extract_api_key(&request) {
        Some(k) => k,
        None => {
            let error = ErrorResponse::authentication_error();
            return (StatusCode::UNAUTHORIZED, Json(error)).into_response();
        }
    };

    // 1) master apiKey
    let master = state.api_key.read().clone();
    if auth::constant_time_eq(&presented, &master) {
        request.extensions_mut().insert(KeyContext { key_id: 0 });
        return next.run(request).await;
    }

    // 2) 客户端 Key
    if let Some(mgr) = &state.client_keys {
        if let Some(id) = mgr.verify_and_touch(&presented) {
            request.extensions_mut().insert(KeyContext { key_id: id });
            return next.run(request).await;
        }
    }

    let error = ErrorResponse::authentication_error();
    (StatusCode::UNAUTHORIZED, Json(error)).into_response()
}

/// CORS 中间件层
///
/// **安全说明**：当前配置允许所有来源（Any），这是为了支持公开 API 服务。
/// 如果需要更严格的安全控制，请根据实际需求配置具体的允许来源、方法和头信息。
///
/// # 配置说明
/// - `allow_origin(Any)`: 允许任何来源的请求
/// - `allow_methods(Any)`: 允许任何 HTTP 方法
/// - `allow_headers(Any)`: 允许任何请求头
pub fn cors_layer() -> tower_http::cors::CorsLayer {
    use axum::http::{HeaderValue, Method};
    use tower_http::cors::CorsLayer;

    CorsLayer::new()
        .allow_origin([
            HeaderValue::from_static("http://localhost:8990"),
            HeaderValue::from_static("http://127.0.0.1:8990"),
            HeaderValue::from_static("http://localhost:8080"),
            HeaderValue::from_static("http://127.0.0.1:8080"),
        ])
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([
            axum::http::header::AUTHORIZATION,
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderName::from_static("x-api-key"),
        ])
}
