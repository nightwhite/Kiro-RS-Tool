//! 中转层 prompt cache（无外部依赖）
//!
//! Kiro 上游不下发 cache_creation / cache_read token 字段（实测 meteringEvent
//! 只给 credit 计费量），所以这里在中转层自行模拟"提示词缓存"，复现 Anthropic
//! 滑动窗口缓存的「最长公共前缀命中」语义：
//!
//! - 把 prompt 的稳定前缀按 message 边界切成一条递增前缀段链：
//!   `[tools+system] → [+msg0] → [+msg1] → ... → [+msg(n-2)]`，每段 hash 是
//!   「从头累积到该边界」的指纹，token 是该前缀的累计估算。
//! - 只有显式 `cache_control` 会创建缓存断点；一旦出现过 cache_control，后续
//!   message end 会继承当前 TTL 继续创建递增前缀断点。
//! - lookup 取最深命中段 = 最长已缓存前缀 = `cache_read_input_tokens`；其后到
//!   末段 = `cache_creation_input_tokens`；完全 miss → cache_read = 0。
//!
//! 跨轮命中的关键：历史消息逐字节不变，故 Turn N+1 的历史前缀段 hash 必然等于
//! Turn N 写入的同一段。隔离维度为上游 credential_id，避免不同 Kiro 账号互相命中。
//!
//! 内存 + JSON 落盘：每分钟一次写到 `cache_dir/cache_metering.json`，启动时读
//! 回过期记录会被丢掉。**不依赖 Redis 或任何外部 KV**。

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// 默认条目上限（防止内存无限增长）
const DEFAULT_CAPACITY: usize = 4096;
/// 最长 TTL（1h，与 Anthropic ttl="1h" 对齐）
const MAX_TTL_SECS: i64 = 3600;
/// 默认 TTL（5min，ephemeral 默认值）
const DEFAULT_TTL_SECS: i64 = 5 * 60;
const PREFIX_LOOKBACK_LIMIT: usize = 10;

/// 单个缓存条目
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    /// 该前缀段累计的估算 token 数
    pub tokens: u32,
    /// 过期时间戳（unix 秒）
    pub expires_at: i64,
    /// 上次命中时间（用于 LRU 淘汰）
    pub last_hit_at: i64,
}

/// 一次查询的结果（每段一份）
#[derive(Debug, Clone, Copy)]
pub struct SegmentResult {
    /// 该段是否命中
    pub hit: bool,
    /// 该段累计 tokens（保留供调试 / 调用方扩展，dead_code 抑制）
    #[allow(dead_code)]
    pub tokens: u32,
}

/// `compute_cache_usage` 的结果：缓存计费量 + 比例分摊所需的 estimate 口径基准。
///
/// `cache_creation` / `cache_read` 是按 `estimate_tokens` 口径算出的「被缓存覆盖
/// 前缀」的拆分；但最终上报要换算到**真实 total 口径**（contextUsage 真值或
/// `count_tokens` 估算），两个估算器尺度不同，所以这里额外带出两个 estimate 口径
/// 的基准量，供调用方做**无量纲比例分摊**：
///   - `cache_covered_est` = 被缓存覆盖前缀的 estimate token（= creation + read）
///   - `prompt_total_est`  = 整个 prompt（含最深断点之后未缓存尾部）的 estimate token
///
/// 调用方据此算 `prefix_ratio = cache_covered_est / prompt_total_est`，再乘到真实
/// total 上得到缓存覆盖部分，剩余即未缓存的 `input_tokens`，三者互斥相加 == total。
#[derive(Debug, Clone, Copy, Default)]
pub struct CacheUsage {
    /// 缓存读取 token（estimate 口径，最深命中段累计）。
    /// creation 部分 = `cache_covered_est − cache_read`，无需单独存储。
    pub cache_read: i32,
    /// 被缓存覆盖前缀的 estimate token 总量（read + creation）。
    pub cache_covered_est: i32,
    /// 整个 prompt 的 estimate token 总量（比例分摊的分母）。
    pub prompt_total_est: i32,
    /// 5m TTL 的缓存创建 token（estimate 口径）。
    pub cache_creation_5m: i32,
    /// 1h TTL 的缓存创建 token（estimate 口径）。
    pub cache_creation_1h: i32,
}

impl CacheUsage {
    /// 按真实 total 口径做互斥分摊，返回 `(input_tokens, cache_creation, cache_read)`。
    ///
    /// `total_real` 是最终上报口径的全量 prompt token（contextUsage 真值优先，
    /// 否则 `count_tokens` 估算）。三者满足 `input + creation + read == total_real`。
    ///
    /// 无缓存覆盖（`cache_covered_est == 0`）或基准缺失时，直接返回
    /// `(total_real, 0, 0)`——全部计入 input，不凭空造缓存计数。
    pub fn split_against_total(&self, total_real: i32) -> (i32, i32, i32) {
        let total = total_real.max(0);
        if self.cache_covered_est <= 0 || self.prompt_total_est <= 0 {
            return (total, 0, 0);
        }
        // 比例无量纲，跨估算器成立；clamp 到 [0, total] 防止 estimate 偏差越界。
        let ratio = (self.cache_covered_est as f64 / self.prompt_total_est as f64).clamp(0.0, 1.0);
        let cache_total = ((total as f64) * ratio).round() as i32;
        let cache_total = cache_total.min(total);
        // 在缓存覆盖部分内部，按 estimate 口径的 read/creation 占比二次拆分。
        let read = if self.cache_covered_est > 0 {
            ((cache_total as f64) * (self.cache_read as f64 / self.cache_covered_est as f64))
                .round() as i32
        } else {
            0
        };
        let read = read.clamp(0, cache_total);
        let creation = cache_total - read;
        let input = total - cache_total;
        (input, creation, read)
    }

    /// 按真实 total 口径拆分，并额外返回 Anthropic `cache_creation` TTL 明细。
    ///
    /// 返回 `(input_tokens, cache_creation, cache_read, cache_creation_5m, cache_creation_1h)`。
    pub fn split_with_ttl_breakdown(&self, total_real: i32) -> (i32, i32, i32, i32, i32) {
        let (input, creation, read) = self.split_against_total(total_real);
        if creation <= 0 {
            return (input, creation, read, 0, 0);
        }
        let creation_est = self.cache_covered_est.saturating_sub(self.cache_read);
        if creation_est <= 0 {
            return (input, creation, read, 0, 0);
        }
        let ttl_creation_est = self
            .cache_creation_5m
            .saturating_add(self.cache_creation_1h)
            .max(creation_est);
        let one_hour = ((creation as f64)
            * (self.cache_creation_1h as f64 / ttl_creation_est as f64))
            .round() as i32;
        let one_hour = one_hour.clamp(0, creation);
        let five_min = creation - one_hour;
        (input, creation, read, five_min, one_hour)
    }
}

/// 一次 cache metering 的完整计划。
///
/// 查询阶段只读取当前缓存并计算 usage，不能立即写入新段；否则上游 429/5xx/断流
/// 失败也会污染本地模拟缓存。成功路径调用 [`Self::commit_success`] 后才写入新段。
#[derive(Clone, Default)]
pub struct CacheUsagePlan {
    pub cache_read: i32,
    pub cache_covered_est: i32,
    pub prompt_total_est: i32,
    pub cache_creation_5m: i32,
    pub cache_creation_1h: i32,
    commit: Option<CacheCommit>,
}

impl CacheUsagePlan {
    pub fn usage(&self) -> CacheUsage {
        CacheUsage {
            cache_read: self.cache_read,
            cache_covered_est: self.cache_covered_est,
            prompt_total_est: self.prompt_total_est,
            cache_creation_5m: self.cache_creation_5m,
            cache_creation_1h: self.cache_creation_1h,
        }
    }

    pub fn split_against_total(&self, total_real: i32) -> (i32, i32, i32) {
        self.usage().split_against_total(total_real)
    }

    pub fn split_with_ttl_breakdown(&self, total_real: i32) -> (i32, i32, i32, i32, i32) {
        self.usage().split_with_ttl_breakdown(total_real)
    }

    pub fn commit_success(&self) {
        if let Some(commit) = &self.commit {
            commit.commit();
        }
    }
}

#[derive(Clone)]
struct CacheCommit {
    cache: SharedCacheMeter,
    segment_hashes: Vec<u64>,
    segment_tokens: Vec<u32>,
    segment_ttls: Vec<i64>,
}

impl CacheCommit {
    fn commit(&self) {
        self.cache.record_with_ttls(
            &self.segment_hashes,
            &self.segment_tokens,
            &self.segment_ttls,
        );
    }
}

/// 进程内提示词缓存
pub struct CacheMeter {
    inner: Mutex<Inner>,
    persist_path: Option<PathBuf>,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<u64, CacheEntry>,
    /// 自上次落盘后是否有变化
    dirty: bool,
}

impl CacheMeter {
    /// 创建一个空 cache。`persist_path` 为 `Some` 时会自动从该文件加载历史。
    pub fn new(persist_path: Option<PathBuf>) -> Self {
        let mut inner = Inner::default();
        if let Some(path) = persist_path.as_ref() {
            if let Ok(bytes) = std::fs::read(path) {
                if let Ok(entries) = serde_json::from_slice::<HashMap<u64, CacheEntry>>(&bytes) {
                    let now = now_secs();
                    for (k, v) in entries {
                        if v.expires_at > now {
                            inner.entries.insert(k, v);
                        }
                    }
                    tracing::info!(
                        "CacheMeter 重建：从 {} 加载 {} 条有效记录",
                        path.display(),
                        inner.entries.len()
                    );
                }
            }
        }
        Self {
            inner: Mutex::new(inner),
            persist_path,
        }
    }

    /// 查询一组前缀段哈希，返回每段命中情况；命中段会刷新 last_hit_at。
    ///
    /// `segment_hashes` 顺序必须与请求中 cache_control 断点顺序一致；
    /// `segment_tokens` 是每段累计 tokens（即 segment_hashes[i] 对应的整段累加值）。
    pub fn lookup(&self, segment_hashes: &[u64], segment_tokens: &[u32]) -> Vec<SegmentResult> {
        debug_assert_eq!(segment_hashes.len(), segment_tokens.len());
        let now = now_secs();
        let mut inner = self.inner.lock();
        let mut out = Vec::with_capacity(segment_hashes.len());
        for (h, t) in segment_hashes.iter().zip(segment_tokens.iter()) {
            let hit = match inner.entries.get_mut(h) {
                Some(entry) if entry.expires_at > now => {
                    entry.last_hit_at = now;
                    true
                }
                _ => false,
            };
            out.push(SegmentResult { hit, tokens: *t });
        }
        out
    }

    /// 把一组前缀段写入缓存（用于成功请求后登记）。`ttl_secs` clip 到 [60, MAX_TTL_SECS]。
    /// 已存在的前缀不刷新 `expires_at`，与 Anthropic prompt cache 从首次写入起算 TTL 的语义对齐。
    #[cfg(test)]
    fn record(&self, segment_hashes: &[u64], segment_tokens: &[u32], ttl_secs: i64) {
        let segment_ttls = vec![ttl_secs; segment_hashes.len()];
        self.record_with_ttls(segment_hashes, segment_tokens, &segment_ttls);
    }

    fn record_with_ttls(
        &self,
        segment_hashes: &[u64],
        segment_tokens: &[u32],
        segment_ttls: &[i64],
    ) {
        debug_assert_eq!(segment_hashes.len(), segment_tokens.len());
        debug_assert_eq!(segment_hashes.len(), segment_ttls.len());
        let now = now_secs();
        let mut inner = self.inner.lock();
        for ((h, t), ttl_secs) in segment_hashes
            .iter()
            .zip(segment_tokens.iter())
            .zip(segment_ttls.iter())
        {
            let ttl = (*ttl_secs).clamp(60, MAX_TTL_SECS);
            let expires_at = now + ttl;
            match inner.entries.get_mut(h) {
                Some(entry) if entry.expires_at > now => {
                    entry.tokens = entry.tokens.max(*t);
                    entry.last_hit_at = now;
                }
                _ => {
                    inner.entries.insert(
                        *h,
                        CacheEntry {
                            tokens: *t,
                            expires_at,
                            last_hit_at: now,
                        },
                    );
                }
            }
        }
        inner.dirty = true;
        // 容量超限：按 last_hit_at 淘汰最旧的若干条
        if inner.entries.len() > DEFAULT_CAPACITY {
            let drop_n = inner.entries.len() - DEFAULT_CAPACITY;
            let mut victims: Vec<(u64, i64)> = inner
                .entries
                .iter()
                .map(|(k, v)| (*k, v.last_hit_at))
                .collect();
            victims.sort_by_key(|x| x.1);
            for (k, _) in victims.into_iter().take(drop_n) {
                inner.entries.remove(&k);
            }
        }
    }

    /// 把当前快照写到 persist_path（仅在 dirty 时实际落盘）
    pub fn flush_to_disk(&self) {
        let path = match self.persist_path.clone() {
            Some(p) => p,
            None => return,
        };
        let snapshot = {
            let mut inner = self.inner.lock();
            if !inner.dirty {
                return;
            }
            inner.dirty = false;
            inner.entries.clone()
        };
        let json = match serde_json::to_vec(&snapshot) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("CacheMeter 序列化失败: {}", e);
                return;
            }
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&path, json) {
            tracing::warn!("CacheMeter 落盘失败 {}: {}", path.display(), e);
        }
    }

    /// 启动后台周期任务：定期 flush + 清理过期条目
    pub fn spawn_background(self: Arc<Self>) {
        let weak = Arc::downgrade(&self);
        tokio::spawn(async move {
            let interval = std::time::Duration::from_secs(60);
            loop {
                tokio::time::sleep(interval).await;
                let Some(cache) = weak.upgrade() else { return };
                cache.evict_expired();
                cache.flush_to_disk();
            }
        });
    }

    /// 删除已过期条目（lookup 不命中过期时只是返回 miss，不会顺手清理；
    /// 这里在后台周期里清一次，避免内存膨胀）。
    pub fn evict_expired(&self) {
        let now = now_secs();
        let mut inner = self.inner.lock();
        let before = inner.entries.len();
        inner.entries.retain(|_, v| v.expires_at > now);
        if inner.entries.len() != before {
            inner.dirty = true;
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().entries.len()
    }
}

fn now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 解析 cache_control 的 ttl 字符串（"5m" / "1h"）→ 秒
pub fn parse_ttl(ttl: Option<&str>) -> i64 {
    match ttl {
        Some(s) if s.eq_ignore_ascii_case("1h") => 3600,
        Some(s) if s.eq_ignore_ascii_case("5m") => 300,
        _ => DEFAULT_TTL_SECS,
    }
}

/// `Arc<CacheMeter>` 别名
pub type SharedCacheMeter = Arc<CacheMeter>;

// ============================================================================
// 与请求体协议层的接线
// ============================================================================

use super::stream::estimate_tokens;
use super::types::{MessagesRequest, SystemMessage, Tool};

/// 协议层提取出来的一个"段"（segment）：从请求开头累计到本断点的所有内容。
///
/// `tokens` 是该前缀**累计**的估算 token 数；`hash` 由前缀文本的累加 SHA-256
/// 折叠得到（取低 64 位作 key，与 CacheMeter 的 u64 key 兼容）。
#[derive(Debug, Clone, Copy)]
struct Segment {
    hash: u64,
    cumulative_tokens: u32,
    /// 该段单独的 ttl（秒）
    ttl_secs: i64,
}

/// 调用 CacheMeter 计算本次请求的缓存覆盖情况，并把所有断点（含命中段）记录回
/// cache、刷新 TTL。返回 [`CacheUsage`]，由调用方在拿到真实 total 后做互斥分摊。
///
/// **完全按 Anthropic 协议**：取最深命中的段索引 i*，那么（estimate 口径）
/// - `cache_read = segments[i*].cumulative_tokens`
/// - `cache_creation = segments.last().cumulative_tokens - segments[i*].cumulative_tokens`
///
/// 全部 miss 时 cache_read = 0，cache_creation = 最深段累计 tokens。
///
/// 注意 `cache_creation` 只累计到**最深断点**为止；最深断点之后的 prompt 尾部
/// （未被任何 cache_control 覆盖）属于真 input，不计入缓存——这正是 `prompt_total_est`
/// 与 `cache_covered_est` 的差值。
///
/// 没有任何 cache_control 断点时，不产生可缓存覆盖。
///
/// `credential_id` 是上游 Kiro 账号 id，用于缓存隔离：同一账号可复用缓存，
/// 不同账号互不命中。
pub fn compute_cache_usage(
    cache: &SharedCacheMeter,
    req: &MessagesRequest,
    credential_id: u64,
) -> CacheUsagePlan {
    let (segments, prompt_total_est, cache_enabled) = extract_segments(req, credential_id);
    let min_cacheable_tokens = minimum_cacheable_tokens_for_model(&req.model);
    let segments: Vec<Segment> = segments
        .into_iter()
        .filter(|segment| segment.cumulative_tokens >= min_cacheable_tokens)
        .collect();
    if segments.is_empty() {
        // 无断点：仍带出 prompt_total_est 以便调用方将来扩展，但 covered=0 → 全入 input。
        return CacheUsagePlan {
            prompt_total_est: prompt_total_est as i32,
            ..Default::default()
        };
    }

    let hashes: Vec<u64> = segments.iter().map(|s| s.hash).collect();
    let cum_tokens: Vec<u32> = segments.iter().map(|s| s.cumulative_tokens).collect();
    let lookup_start = hashes.len().saturating_sub(PREFIX_LOOKBACK_LIMIT);
    let results = cache.lookup(&hashes[lookup_start..], &cum_tokens[lookup_start..]);

    // 诊断（DEBUG 级）：打印每段 hash / 累计 token / 命中情况，排查跨轮 miss。
    if tracing::enabled!(tracing::Level::DEBUG) {
        let dump: Vec<String> = segments
            .iter()
            .zip(
                (0..segments.len())
                    .map(|i| i.checked_sub(lookup_start).and_then(|idx| results.get(idx))),
            )
            .enumerate()
            .map(|(i, (s, result))| {
                format!(
                    "[{i}] hash={} cum={} hit={}",
                    s.hash,
                    s.cumulative_tokens,
                    result.is_some_and(|r| r.hit)
                )
            })
            .collect();
        tracing::debug!(
            "CacheMeter: {} 段, msgs={} | {}",
            segments.len(),
            req.messages.len(),
            dump.join(", ")
        );
    }

    let deepest_hit = results
        .iter()
        .rposition(|r| r.hit)
        .map(|idx| idx + lookup_start);
    // 被缓存覆盖的前缀 = 最深断点累计（最深断点之后的尾部是未缓存的真 input）。
    // 命中时 read = 命中段累计、creation = covered − read；全 miss 时 read = 0。
    let covered = *cum_tokens.last().unwrap();
    let cache_read = match deepest_hit {
        Some(i) => cum_tokens[i],
        None => 0u32,
    };

    let last_ttl_secs = segments
        .last()
        .map(|s| s.ttl_secs)
        .unwrap_or(DEFAULT_TTL_SECS);
    CacheUsagePlan {
        cache_read: cache_read as i32,
        cache_covered_est: covered as i32,
        prompt_total_est: prompt_total_est as i32,
        cache_creation_5m: if last_ttl_secs >= MAX_TTL_SECS {
            0
        } else {
            covered.saturating_sub(cache_read) as i32
        },
        cache_creation_1h: if last_ttl_secs >= MAX_TTL_SECS {
            covered.saturating_sub(cache_read) as i32
        } else {
            0
        },
        commit: cache_enabled.then(|| CacheCommit {
            cache: Arc::clone(cache),
            segment_hashes: hashes,
            segment_tokens: cum_tokens,
            segment_ttls: segments.iter().map(|s| s.ttl_secs).collect(),
        }),
    }
}

fn minimum_cacheable_tokens_for_model(model: &str) -> u32 {
    let model_lower = model.to_lowercase();
    if model_lower.contains("opus") {
        4096
    } else if model_lower.contains("haiku") {
        2048
    } else {
        1024
    }
}

/// 从请求体里按顺序提取断点段：tools → system → messages
///
/// 这个顺序与 Anthropic 拼接 prompt 的顺序对齐：tools 在最前，system 次之，
/// 然后才是 messages。每遇到一个 cache_control 断点就产生一个 Segment。
/// 累计 token 数随处理顺序累加，永远是当前位置的"前缀总量"。
///
/// 返回 `(segments, prompt_total_est)`，其中 `prompt_total_est` 是喂完整个 prompt
/// （含最深断点之后的尾部）后的 estimate token 累计，用作比例分摊的分母。
///
/// `credential_id` 用于账号隔离：哈希以 credential id 起头，种子不计入 token。
fn extract_segments(req: &MessagesRequest, credential_id: u64) -> (Vec<Segment>, u32, bool) {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let mut cache_tokens: u32 = 0;
    let mut prompt_total_tokens: u32 = 0;
    let mut segments: Vec<Segment> = Vec::new();

    let cache_enabled = credential_id != 0;
    hasher.update(format!("credential:{credential_id}").as_bytes());
    let request_prelude = canonical_json(&serde_json::json!({
        "model": req.model,
        "tool_choice": req.tool_choice,
    }));
    hasher.update(request_prelude.as_bytes());

    // feed 解耦哈希与 token 估算：`hash_text` 进哈希链（决定命中），`token_text`
    // 进 token 累计（决定数值口径）。两者分离是为了让 token 计数贴近**原文**，
    // 不被签名前缀（"block:"/"tool:"）、分隔符（"|"）、role 名等噪声污染；而哈希
    // 仍用结构化签名以保持命中判定稳定。token_text 传空串即「只哈希、不计 token」。
    let feed = |hasher: &mut Sha256,
                hash_text: &str,
                token_text: &str,
                cache_tokens: &mut u32,
                prompt_total_tokens: &mut u32,
                participates_in_cache_hash: bool| {
        if participates_in_cache_hash {
            hasher.update(hash_text.as_bytes());
        }
        if !token_text.is_empty() {
            let tokens = estimate_tokens(token_text).max(0) as u32;
            *prompt_total_tokens = prompt_total_tokens.saturating_add(tokens);
            if participates_in_cache_hash {
                *cache_tokens = cache_tokens.saturating_add(tokens);
            }
        }
    };

    let commit =
        |hasher: &Sha256, cache_tokens: u32, segments: &mut Vec<Segment>, ttl_secs: i64| {
            if !cache_enabled || cache_tokens == 0 {
                return;
            }
            let digest = hasher.clone().finalize();
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&digest[..8]);
            let hash = u64::from_be_bytes(buf);
            if let Some(last) = segments.last_mut()
                && last.hash == hash
                && last.cumulative_tokens == cache_tokens
            {
                last.ttl_secs = last.ttl_secs.max(ttl_secs);
                return;
            }
            segments.push(Segment {
                hash,
                cumulative_tokens: cache_tokens,
                ttl_secs,
            });
        };

    // 参考实现语义：显式 cache_control 创建第一个断点；之后的 message end
    // 继承 active TTL 创建递增前缀断点。没有 active TTL 时不自动创建断点。
    let mut active_ttl: Option<i64> = None;

    // 1. tools（全部喂入，作为前缀基础的一部分；工具定义跨轮稳定）。
    if let Some(tools) = req.tools.as_ref() {
        for t in tools {
            feed(
                &mut hasher,
                &tool_signature(t),
                &tool_token_text(t),
                &mut cache_tokens,
                &mut prompt_total_tokens,
                cache_enabled,
            );
            if let Some(cc) = t.cache_control.as_ref() {
                let ttl = parse_ttl(cc.ttl.as_deref());
                active_ttl = Some(ttl);
                commit(&hasher, cache_tokens, &mut segments, ttl);
            }
        }
    }

    // 2. system。
    if let Some(systems) = req.system.as_ref() {
        for sys in systems {
            feed(
                &mut hasher,
                &system_signature(sys),
                &sys.text,
                &mut cache_tokens,
                &mut prompt_total_tokens,
                cache_enabled,
            );
            if let Some(cc) = sys.cache_control.as_ref() {
                let ttl = parse_ttl(cc.ttl.as_deref());
                active_ttl = Some(ttl);
                commit(&hasher, cache_tokens, &mut segments, ttl);
            }
        }
    }

    // 3. messages。
    for msg in &req.messages {
        // role 进哈希（区分 user/assistant 边界），但不计入 token。
        feed(
            &mut hasher,
            &msg.role,
            "",
            &mut cache_tokens,
            &mut prompt_total_tokens,
            cache_enabled,
        );
        match &msg.content {
            serde_json::Value::String(s) => {
                feed(
                    &mut hasher,
                    s,
                    s,
                    &mut cache_tokens,
                    &mut prompt_total_tokens,
                    cache_enabled,
                );
            }
            serde_json::Value::Array(arr) => {
                // 逐 block 处理：文本块哈希用结构化签名、token 算原文；图片块哈希纳入
                // 图片数据指纹（区分不同图）、token 用 Anthropic 口径估算（(w×h)/750）。
                // 不反序列化整个 block、不 clone Value：省开销，且避免「某 block
                // 反序列化失败被跳过」造成的前缀漂移。
                for v in arr {
                    if v.get("type").and_then(|t| t.as_str()) == Some("image") {
                        // 图片：哈希喂 media_type + 数据（保证不同图 hash 不同、同图稳定），
                        // token 按真实尺寸估算后直接累加（base64 不进文本 estimate）。
                        let (media_type, data) = image_source_parts(v);
                        if cache_enabled {
                            hasher.update(image_signature_value(v).as_bytes());
                        }
                        let img_tokens =
                            crate::image_resize::estimate_image_tokens(media_type, data);
                        prompt_total_tokens = prompt_total_tokens.saturating_add(img_tokens);
                        if cache_enabled {
                            cache_tokens = cache_tokens.saturating_add(img_tokens);
                        }
                    } else {
                        feed(
                            &mut hasher,
                            &block_signature_value(v),
                            &block_token_text(v),
                            &mut cache_tokens,
                            &mut prompt_total_tokens,
                            cache_enabled,
                        );
                    }
                    if block_has_cache_control(v) {
                        let ttl = block_cache_ttl(v);
                        active_ttl = Some(ttl);
                        commit(&hasher, cache_tokens, &mut segments, ttl);
                    }
                }
            }
            _ => {}
        }
        if let Some(ttl) = active_ttl {
            commit(&hasher, cache_tokens, &mut segments, ttl);
        }
    }

    (segments, prompt_total_tokens, cache_enabled)
}

fn block_cache_ttl(v: &serde_json::Value) -> i64 {
    parse_ttl(
        v.get("cache_control")
            .and_then(|cc| cc.get("ttl"))
            .and_then(|ttl| ttl.as_str()),
    )
}

fn tool_signature(t: &Tool) -> String {
    // 把 name + description + input_schema 序列化为稳定文本
    let schema = serde_json::to_string(&t.input_schema).unwrap_or_default();
    format!("tool:{}|{}|{}", t.name, t.description, schema)
}

/// 工具的 token 估算原文：name + description + schema 拼接，不含签名前缀/分隔符。
/// 与 [`tool_signature`] 分离，让 token 计数贴近真实内容、不被结构标记污染。
fn tool_token_text(t: &Tool) -> String {
    let schema = serde_json::to_string(&t.input_schema).unwrap_or_default();
    format!("{} {} {}", t.name, t.description, schema)
}

fn system_signature(s: &SystemMessage) -> String {
    let text = if s.text.starts_with("x-anthropic-billing-header:") {
        "__anthropic_billing_header__"
    } else {
        &s.text
    };
    format!("sys:{text}")
}

/// 直接从 content block 的 JSON 值算语义签名。
///
/// 随机/漂移字段（如 `tool_use.id`、`tool_result.tool_use_id`）不参与签名；
/// 语义字段必须参与，否则不同工具输入/结果会错误命中同一段模拟缓存。
fn block_signature_value(v: &serde_json::Value) -> String {
    let s = |key: &str| v.get(key).and_then(|x| x.as_str()).unwrap_or("");
    match s("type") {
        "text" => format!("block:text|{}", s("text")),
        "thinking" => format!("block:thinking|{}", s("thinking")),
        "redacted_thinking" => format!("block:redacted_thinking|{}", s("data")),
        "tool_use" => format!(
            "block:tool_use|{}|{}",
            s("name"),
            v.get("input").map(canonical_json).unwrap_or_default()
        ),
        "tool_result" => format!(
            "block:tool_result|{}|{}",
            v.get("is_error").and_then(|x| x.as_bool()).unwrap_or(false),
            v.get("content").map(canonical_json).unwrap_or_default()
        ),
        "image" => image_signature_value(v),
        other => format!(
            "block:{}|{}",
            other,
            canonical_json_without_cache_control(v)
        ),
    }
}

fn canonical_json_without_cache_control(v: &serde_json::Value) -> String {
    let mut cloned = v.clone();
    strip_cache_control_value(&mut cloned);
    canonical_json(&cloned)
}

fn strip_cache_control_value(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Array(values) => {
            for value in values {
                strip_cache_control_value(value);
            }
        }
        serde_json::Value::Object(map) => {
            map.remove("cache_control");
            for value in map.values_mut() {
                strip_cache_control_value(value);
            }
        }
        _ => {}
    }
}

fn image_signature_value(v: &serde_json::Value) -> String {
    let (media_type, data) = image_source_parts(v);
    if data.is_empty() {
        format!("block:image|{}|{}", media_type, canonical_json(v))
    } else {
        format!("block:image|{}|{}", media_type, data)
    }
}

fn canonical_json(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Object(map) => {
            let mut pairs: Vec<(&String, &serde_json::Value)> = map.iter().collect();
            pairs.sort_by(|a, b| a.0.cmp(b.0));
            let fields = pairs
                .into_iter()
                .map(|(k, v)| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(k).unwrap_or_default(),
                        canonical_json(v)
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{fields}}}")
        }
        serde_json::Value::Array(values) => {
            let items = values
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",");
            format!("[{items}]")
        }
        _ => serde_json::to_string(v).unwrap_or_default(),
    }
}

/// content block 的 token 估算原文：覆盖 text/thinking/tool/image 语义内容，
/// 不含签名结构标记。
fn block_token_text(v: &serde_json::Value) -> String {
    let s = |key: &str| v.get(key).and_then(|x| x.as_str()).unwrap_or("");
    match s("type") {
        "text" => s("text").to_string(),
        "thinking" => s("thinking").to_string(),
        "redacted_thinking" => s("data").to_string(),
        "tool_use" => {
            let input = v.get("input").map(canonical_json).unwrap_or_default();
            if s("name").is_empty() {
                input
            } else {
                format!("{} {}", s("name"), input)
            }
        }
        "tool_result" => v
            .get("content")
            .map(block_content_token_text)
            .unwrap_or_default(),
        "image" => String::new(),
        _ => canonical_json(v),
    }
}

fn block_content_token_text(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => arr
            .iter()
            .map(|item| {
                if item.get("type").and_then(|v| v.as_str()) == Some("image") {
                    String::new()
                } else {
                    block_token_text(item)
                }
            })
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
        _ => canonical_json(v),
    }
}

fn block_has_cache_control(v: &serde_json::Value) -> bool {
    v.get("cache_control").is_some()
}

/// 从 image content block 的 JSON 值取 `(media_type, base64_data)`。
///
/// 兼容 base64 source（`source.type == "base64"`）；缺字段时返回空串，由调用方
/// 的图片 token 估算走保底逻辑。url 类图片无 data，返回空 data（估算保底）。
fn image_source_parts(v: &serde_json::Value) -> (&str, &str) {
    let src = v.get("source");
    let media_type = src
        .and_then(|s| s.get("media_type"))
        .and_then(|x| x.as_str())
        .unwrap_or("");
    let data = src
        .and_then(|s| s.get("data"))
        .and_then(|x| x.as_str())
        .unwrap_or("");
    (media_type, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_shared_cache() -> SharedCacheMeter {
        Arc::new(CacheMeter::new(None))
    }

    #[test]
    fn lookup_miss_then_record_then_hit() {
        let cache = CacheMeter::new(None);
        let hashes = [1u64, 2u64];
        let tokens = [10u32, 25u32];
        let r1 = cache.lookup(&hashes, &tokens);
        assert!(r1.iter().all(|s| !s.hit));

        cache.record(&hashes, &tokens, 300);
        let r2 = cache.lookup(&hashes, &tokens);
        assert!(r2.iter().all(|s| s.hit));
    }

    #[test]
    fn record_does_not_extend_existing_entry_ttl() {
        let cache = CacheMeter::new(None);
        let hash = [42u64];
        let tokens = [100u32];
        cache.record(&hash, &tokens, 300);
        let first_expires_at = cache.inner.lock().entries.get(&42).unwrap().expires_at;

        cache.record(&hash, &tokens, 3600);
        let second_expires_at = cache.inner.lock().entries.get(&42).unwrap().expires_at;

        assert_eq!(
            second_expires_at, first_expires_at,
            "rewriting an existing prefix must not extend prompt cache ttl"
        );
    }

    #[test]
    #[should_panic]
    fn record_with_ttls_rejects_mismatched_ttl_lengths_in_debug() {
        let cache = CacheMeter::new(None);

        cache.record_with_ttls(&[1, 2], &[10, 20], &[300]);
    }

    #[test]
    fn ttl_expiry_makes_entry_miss() {
        let cache = CacheMeter::new(None);
        cache.record(&[42], &[100], 60);
        // 手动让条目过期
        {
            let mut inner = cache.inner.lock();
            if let Some(e) = inner.entries.get_mut(&42) {
                e.expires_at = now_secs() - 1;
            }
        }
        let r = cache.lookup(&[42], &[100]);
        assert!(!r[0].hit);
    }

    #[test]
    fn evict_expired_removes_dead_entries() {
        let cache = CacheMeter::new(None);
        cache.record(&[1, 2], &[5, 5], 60);
        {
            let mut inner = cache.inner.lock();
            for (_, v) in inner.entries.iter_mut() {
                v.expires_at = now_secs() - 1;
            }
        }
        cache.evict_expired();
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn parse_ttl_handles_known_values() {
        assert_eq!(parse_ttl(Some("1h")), 3600);
        assert_eq!(parse_ttl(Some("5m")), 300);
        assert_eq!(parse_ttl(None), 300);
        assert_eq!(parse_ttl(Some("garbage")), 300);
    }

    #[test]
    fn flush_and_reload_round_trip() {
        let tmp = std::env::temp_dir().join(format!("kiro-pc-{}.json", now_secs()));
        let cache = CacheMeter::new(Some(tmp.clone()));
        cache.record(&[7], &[42], 600);
        cache.flush_to_disk();

        let cache2 = CacheMeter::new(Some(tmp.clone()));
        let r = cache2.lookup(&[7], &[42]);
        assert!(r[0].hit);

        let _ = std::fs::remove_file(&tmp);
    }

    fn build_request_with_system_breakpoint() -> super::super::types::MessagesRequest {
        use super::super::types::{CacheControl, Message, MessagesRequest, SystemMessage};
        MessagesRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            max_tokens: 32,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::Value::String("Hello".to_string()),
            }],
            stream: false,
            system: Some(vec![SystemMessage {
                text: "You are a helpful assistant. ".repeat(300),
                cache_control: Some(CacheControl {
                    cache_type: "ephemeral".to_string(),
                    ttl: None,
                }),
            }]),
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        }
    }

    #[test]
    fn compute_cache_usage_first_miss_then_hit() {
        let cache = new_shared_cache();
        let req = build_request_with_system_breakpoint();

        // 第一次：所有段都 miss → 覆盖前缀全部算 creation（read == 0）。
        let u1 = compute_cache_usage(&cache, &req, 1);
        assert!(u1.cache_covered_est > 0, "first call should cover prefix");
        assert_eq!(u1.cache_read, 0, "first call has nothing cached to read");
        // 用真实 total 分摊：全部进 creation，input = total − covered。
        let total = u1.prompt_total_est; // 取 estimate total 作为「真实 total」便于断言
        let (in1, cc1, cr1) = u1.split_against_total(total);
        assert!(cc1 > 0, "first call creation>0, cc={}", cc1);
        assert_eq!(cr1, 0);
        assert_eq!(in1 + cc1 + cr1, total, "互斥口径必须自洽");
        u1.commit_success();

        // 第二次：相同请求 → 命中，覆盖前缀全部算 read（creation == 0）。
        let u2 = compute_cache_usage(&cache, &req, 1);
        assert!(u2.cache_read > 0, "second call should hit");
        let (in2, cc2, cr2) = u2.split_against_total(total);
        assert_eq!(cc2, 0, "second call creation should be 0, got {}", cc2);
        assert!(cr2 > 0, "second call read>0, cr={}", cr2);
        assert_eq!(in2 + cc2 + cr2, total, "互斥口径必须自洽");
        // 两次拆分的「缓存覆盖部分」一致：第一次的 creation == 第二次的 read。
        assert_eq!(cc1, cr2);
    }

    #[test]
    fn split_against_total_is_mutually_exclusive() {
        // input + creation + read 必须恒等于 total，且缓存覆盖比例正确分摊。
        let u = CacheUsage {
            cache_read: 30,
            cache_covered_est: 80, // creation 部分 = 50
            prompt_total_est: 100,
            cache_creation_5m: 50,
            cache_creation_1h: 0,
        };
        // covered 占 prompt 的 80% → 真实 total=1000 时缓存覆盖 800。
        let (input, creation, read) = u.split_against_total(1000);
        assert_eq!(input + creation + read, 1000);
        assert_eq!(input, 200, "尾部 20% 是未缓存 input");
        // 覆盖部分 800 内按 read:creation = 30:50 拆分 → read=300, creation=500。
        assert_eq!(read, 300);
        assert_eq!(creation, 500);
    }

    #[test]
    fn split_against_total_no_cache_all_input() {
        let u = CacheUsage {
            cache_read: 0,
            cache_covered_est: 0,
            prompt_total_est: 100,
            cache_creation_5m: 0,
            cache_creation_1h: 0,
        };
        assert_eq!(u.split_against_total(500), (500, 0, 0));
    }

    #[test]
    fn compute_cache_usage_single_message_no_prefix() {
        // 单条 user 消息、无 system/tools：没有可缓存的历史前缀（最后一条不切段）
        // → covered=0，total 全进 input。
        use super::super::types::{Message, MessagesRequest};
        let cache = new_shared_cache();
        let req = MessagesRequest {
            model: "x".to_string(),
            max_tokens: 8,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::Value::String("Hello".to_string()),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };
        let u = compute_cache_usage(&cache, &req, 1);
        assert_eq!(u.cache_covered_est, 0);
        assert_eq!(u.split_against_total(123), (123, 0, 0));
    }

    #[test]
    fn different_model_does_not_reuse_same_prompt_cache() {
        let cache = new_shared_cache();
        let mut req = req_with_messages(vec![
            msg_with_cc("user", &"stable prompt ".repeat(420), true),
            msg_with_cc("assistant", "ok", false),
            msg_with_cc("user", "again", false),
        ]);
        req.model = "claude-sonnet-4-5".to_string();

        let first = compute_cache_usage(&cache, &req, 1);
        assert_eq!(first.cache_read, 0);
        assert!(first.cache_covered_est > 0);
        first.commit_success();

        req.model = "claude-opus-4-5".to_string();
        let second = compute_cache_usage(&cache, &req, 1);
        assert_eq!(
            second.cache_read, 0,
            "prompt cache identity must include model"
        );
    }

    #[test]
    fn different_tool_choice_does_not_reuse_same_prompt_cache() {
        let cache = new_shared_cache();
        let mut req = req_with_messages(vec![
            msg_with_cc("user", &"stable prompt ".repeat(420), true),
            msg_with_cc("assistant", "ok", false),
            msg_with_cc("user", "again", false),
        ]);
        req.tool_choice = Some(serde_json::json!({"type": "auto"}));

        let first = compute_cache_usage(&cache, &req, 1);
        assert_eq!(first.cache_read, 0);
        assert!(first.cache_covered_est > 0);
        first.commit_success();

        req.tool_choice = Some(serde_json::json!({"type": "tool", "name": "search"}));
        let second = compute_cache_usage(&cache, &req, 1);
        assert_eq!(
            second.cache_read, 0,
            "prompt cache identity must include tool_choice"
        );
    }

    /// 构造一个普通工具，input_schema 的顶层 key 按给定顺序插入。
    /// 用于验证：无论插入顺序如何，tool_signature 都稳定（BTreeMap 保证）。
    fn build_tool_with_schema_order(insert_required_first: bool) -> super::super::types::Tool {
        use super::super::types::Tool;
        let mut schema = std::collections::BTreeMap::new();
        // 故意用不同的插入顺序，模拟上游 JSON 解析的不确定迭代序。
        if insert_required_first {
            schema.insert("required".to_string(), serde_json::json!([]));
            schema.insert("properties".to_string(), serde_json::json!({}));
            schema.insert("type".to_string(), serde_json::json!("object"));
            schema.insert(
                "description".to_string(),
                serde_json::json!("schema prompt ".repeat(5000)),
            );
        } else {
            schema.insert("type".to_string(), serde_json::json!("object"));
            schema.insert(
                "description".to_string(),
                serde_json::json!("schema prompt ".repeat(5000)),
            );
            schema.insert("properties".to_string(), serde_json::json!({}));
            schema.insert("required".to_string(), serde_json::json!([]));
        }
        Tool {
            tool_type: None,
            name: "my_tool".to_string(),
            description: "desc".to_string(),
            input_schema: schema,
            max_uses: None,
            cache_control: None,
        }
    }

    #[test]
    fn tool_signature_stable_across_insert_order() {
        let a = build_tool_with_schema_order(true);
        let b = build_tool_with_schema_order(false);
        // 逻辑等价、插入顺序不同的 schema 必须产生相同签名，
        // 否则 tools 段 hash 抖动会让后续 system/messages 断点连锁 miss。
        assert_eq!(tool_signature(&a), tool_signature(&b));
    }

    #[test]
    fn compute_cache_usage_tools_hit_regardless_of_schema_order() {
        use super::super::types::{CacheControl, Message, MessagesRequest};

        let make_req = |insert_required_first: bool| {
            let mut tool = build_tool_with_schema_order(insert_required_first);
            tool.cache_control = Some(CacheControl {
                cache_type: "ephemeral".to_string(),
                ttl: None,
            });
            MessagesRequest {
                model: "claude-sonnet-4-5-20250929".to_string(),
                max_tokens: 32,
                messages: vec![Message {
                    role: "user".to_string(),
                    content: serde_json::Value::String("Hello".to_string()),
                }],
                stream: false,
                system: None,
                tools: Some(vec![tool]),
                tool_choice: None,
                thinking: None,
                output_config: None,
                metadata: None,
            }
        };

        let cache = new_shared_cache();
        // 第一次：用一种插入顺序，应写缓存（miss → read==0）。
        let u1 = compute_cache_usage(&cache, &make_req(false), 1);
        assert!(u1.cache_covered_est > 0, "first call should cover prefix");
        assert_eq!(u1.cache_read, 0);
        u1.commit_success();

        // 第二次：换一种插入顺序但逻辑等价，应命中缓存（read 等于第一次覆盖前缀）。
        let u2 = compute_cache_usage(&cache, &make_req(true), 1);
        assert_eq!(
            u2.cache_read, u1.cache_covered_est,
            "schema 顺序不应影响命中：second read 应等于 first covered"
        );
    }

    /// 构造一条带 cache_control 的 user/assistant 文本消息。
    fn msg_with_cc(role: &str, text: &str, with_cc: bool) -> super::super::types::Message {
        use super::super::types::Message;
        let block = if with_cc {
            serde_json::json!({
                "type": "text",
                "text": text,
                "cache_control": {"type": "ephemeral"}
            })
        } else {
            serde_json::json!({"type": "text", "text": text})
        };
        Message {
            role: role.to_string(),
            content: serde_json::Value::Array(vec![block]),
        }
    }

    fn req_with_messages(
        messages: Vec<super::super::types::Message>,
    ) -> super::super::types::MessagesRequest {
        use super::super::types::MessagesRequest;
        MessagesRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            max_tokens: 32,
            messages,
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        }
    }

    /// 模拟 Claude Code 真实工具调用序列：tool_use(assistant) / tool_result(user)
    /// 块每轮回传时带每次新生成的 id。验证前缀链对「含 id 漂移的工具块」仍能命中。
    #[test]
    fn tool_call_history_still_hits_despite_id_drift() {
        let body = "analyze the repository structure carefully ".repeat(180);
        // assistant 轮：一个 tool_use 块，input 是工具参数，id 每轮可能不同。
        let assistant_tool = |id: &str| {
            use super::super::types::Message;
            Message {
                role: "assistant".to_string(),
                content: serde_json::json!([
                    {"type": "text", "text": body},
                    {"type": "tool_use", "id": id, "name": "bash", "input": {"cmd": "ls"}}
                ]),
            }
        };
        // user 轮：tool_result 块，tool_use_id 对应上面的 id。
        let user_result = |id: &str| {
            use super::super::types::Message;
            Message {
                role: "user".to_string(),
                content: serde_json::json!([
                    {"type": "tool_result", "tool_use_id": id, "content": body}
                ]),
            }
        };
        let user_anchor = |t: &str| msg_with_cc("user", t, true);
        let user_text = |t: &str| msg_with_cc("user", t, false);

        let cache = new_shared_cache();
        // Turn 1: user → assistant(tool_use #a) → user(tool_result #a) → assistant(text) → user(新问题)
        let turn1 = req_with_messages(vec![
            user_anchor(&body),
            assistant_tool("toolu_aaa"),
            user_result("toolu_aaa"),
            msg_with_cc("assistant", &body, false),
            user_text("next question one"),
        ]);
        let u1 = compute_cache_usage(&cache, &turn1, 1);
        assert!(u1.cache_covered_est > 0);
        assert_eq!(u1.cache_read, 0, "turn1 无历史可命中");
        u1.commit_success();

        // Turn 2: 追加 assistant(text) + user(新问题)。前 5 条历史逐字节不变。
        let turn2 = req_with_messages(vec![
            user_anchor(&body),
            assistant_tool("toolu_aaa"),
            user_result("toolu_aaa"),
            msg_with_cc("assistant", &body, false),
            user_text("next question one"),
            msg_with_cc("assistant", &body, false),
            user_text("next question two"),
        ]);
        let u2 = compute_cache_usage(&cache, &turn2, 1);
        assert!(
            u2.cache_read > 0,
            "turn2 应命中 turn1 的历史前缀（即便工具块带 id）"
        );
        assert_eq!(
            u2.cache_read, u1.cache_covered_est,
            "命中的最深前缀应等于上一轮 covered"
        );
    }

    #[test]
    fn tool_result_content_change_must_not_cache_hit() {
        let body = "stable prompt prefix ".repeat(260);
        let make = |result: &str| {
            req_with_messages(vec![
                msg_with_cc("user", &body, true),
                {
                    use super::super::types::Message;
                    Message {
                        role: "assistant".to_string(),
                        content: serde_json::json!([
                            {"type": "tool_use", "id": "toolu_a", "name": "lookup", "input": {"id": 7}}
                        ]),
                    }
                },
                {
                    use super::super::types::Message;
                    Message {
                        role: "user".to_string(),
                        content: serde_json::json!([
                            {"type": "tool_result", "tool_use_id": "toolu_a", "content": result}
                        ]),
                    }
                },
                msg_with_cc("user", "next", false),
            ])
        };

        let cache = new_shared_cache();
        let first = compute_cache_usage(&cache, &make("order status: paid"), 1);
        assert_eq!(first.cache_read, 0);
        first.commit_success();

        let changed = compute_cache_usage(&cache, &make("order status: refunded"), 1);
        assert!(
            changed.cache_read < first.cache_covered_est,
            "semantic tool_result changes must not hit the prior deepest cache segment that included the old tool_result"
        );
    }

    #[test]
    fn tool_use_id_drift_with_same_semantic_content_still_hits() {
        let body = "stable prompt prefix ".repeat(260);
        let make = |id: &str| {
            req_with_messages(vec![
                msg_with_cc("user", &body, true),
                {
                    use super::super::types::Message;
                    Message {
                        role: "assistant".to_string(),
                        content: serde_json::json!([
                            {"type": "tool_use", "id": id, "name": "lookup", "input": {"id": 7}}
                        ]),
                    }
                },
                {
                    use super::super::types::Message;
                    Message {
                        role: "user".to_string(),
                        content: serde_json::json!([
                            {"type": "tool_result", "tool_use_id": id, "content": "order status: paid"}
                        ]),
                    }
                },
                msg_with_cc("user", "next", false),
            ])
        };

        let cache = new_shared_cache();
        let first = compute_cache_usage(&cache, &make("toolu_old"), 1);
        assert_eq!(first.cache_read, 0);
        first.commit_success();

        let drifted = compute_cache_usage(&cache, &make("toolu_new"), 1);
        assert!(
            drifted.cache_read > 0,
            "tool_use.id and tool_result.tool_use_id drift should not affect semantic cache hits"
        );
    }

    #[test]
    fn multi_turn_prefix_chain_produces_read_hit() {
        // 前缀链模型：turn4 在 turn3 基础上追加 a/u 一对，历史前缀逐字节不变，
        // 所以 turn4 应命中 turn3 写入的最深历史前缀段（cache_read > 0）。
        let cache = new_shared_cache();
        let body = "the quick brown fox jumps over the lazy dog ".repeat(460);

        // 第 3 轮：u,a,u,a,u（5 条）。切段：除最后一条外，每条 message 一个前缀段
        // → idx 0,1,2,3 共 4 个段（无 system/tools）。
        let turn3 = req_with_messages(vec![
            msg_with_cc("user", &body, true),
            msg_with_cc("assistant", &body, false),
            msg_with_cc("user", &body, false),
            msg_with_cc("assistant", &body, false),
            msg_with_cc("user", &body, false),
        ]);
        let u3 = compute_cache_usage(&cache, &turn3, 1);
        assert!(u3.cache_covered_est > 0, "turn3 should create cache");
        assert_eq!(u3.cache_read, 0, "turn3 has no prior cache to read");
        u3.commit_success();

        // 第 4 轮：追加 a3,u4（7 条）。历史 idx 0..=5 切段，最后一条 idx6 不切。
        // turn3 的最深段在 idx3（其前缀=u,a,u,a），turn4 的 idx3 段前缀逐字节相同
        // → 命中。turn4 还新增 idx4,5 两个更深的历史前缀段。
        let turn4 = req_with_messages(vec![
            msg_with_cc("user", &body, true),
            msg_with_cc("assistant", &body, false),
            msg_with_cc("user", &body, false),
            msg_with_cc("assistant", &body, false),
            msg_with_cc("user", &body, false),
            msg_with_cc("assistant", &body, false),
            msg_with_cc("user", &body, false),
        ]);
        let u4 = compute_cache_usage(&cache, &turn4, 1);
        assert!(u4.cache_read > 0, "turn4 should hit a prior-turn prefix");
        // turn4 命中的最深前缀 = turn3 的最深段（idx3 前缀，即 turn3 的 covered）。
        assert_eq!(
            u4.cache_read, u3.cache_covered_est,
            "read 应等于上一轮写入的最深历史前缀"
        );
        // turn4 覆盖前缀更深（新增历史段）→ creation 部分 > 0。
        assert!(
            u4.cache_covered_est > u4.cache_read,
            "turn4 仍会为新增的历史前缀创建缓存"
        );
    }

    #[test]
    fn explicit_cache_control_on_current_message_can_read_and_create() {
        // 真实 Anthropic cache 可在同一请求里读到旧历史前缀，同时写入本轮
        // 当前消息上的新 cache_control 断点。旧实现无条件跳过最后一条 message，
        // 导致这类请求只能 read、不能 creation。
        let cache = new_shared_cache();
        let body = "stable history prefix ".repeat(280);

        let turn1 = req_with_messages(vec![
            msg_with_cc("user", &body, true),
            msg_with_cc("assistant", &body, false),
            msg_with_cc("user", "current cacheable query one", true),
        ]);
        let u1 = compute_cache_usage(&cache, &turn1, 1);
        assert_eq!(u1.cache_read, 0);
        assert!(u1.cache_covered_est > 0);
        u1.commit_success();

        let turn2 = req_with_messages(vec![
            msg_with_cc("user", &body, true),
            msg_with_cc("assistant", &body, false),
            msg_with_cc("user", "current cacheable query two", true),
        ]);
        let u2 = compute_cache_usage(&cache, &turn2, 1);
        assert!(u2.cache_read > 0, "应命中 turn1 的稳定历史前缀");
        assert!(
            u2.cache_covered_est > u2.cache_read,
            "当前消息显式 cache_control 应产生新 creation 覆盖"
        );

        let (_input, creation, read) = u2.split_against_total(u2.prompt_total_est);
        assert!(read > 0, "同一次请求应有 cache_read");
        assert!(creation > 0, "同一次请求应有 cache_creation");
    }

    #[test]
    fn no_cache_control_does_not_create_cacheable_breakpoints() {
        // 参考实现只在显式 cache_control 出现后创建缓存断点；
        // 没有 cache_control 时不应凭历史 message 边界模拟 cache usage。
        let cache = new_shared_cache();
        let body = "lorem ipsum dolor sit amet ".repeat(220);
        let turn1 = req_with_messages(vec![
            msg_with_cc("user", &body, false),
            msg_with_cc("assistant", &body, false),
            msg_with_cc("user", &body, false),
        ]);
        let u1 = compute_cache_usage(&cache, &turn1, 1);
        assert_eq!(u1.cache_covered_est, 0, "无 cache_control 不应创建缓存段");
        assert_eq!(u1.cache_read, 0);
        u1.commit_success();

        let turn2 = req_with_messages(vec![
            msg_with_cc("user", &body, false),
            msg_with_cc("assistant", &body, false),
            msg_with_cc("user", &body, false),
            msg_with_cc("assistant", &body, false),
            msg_with_cc("user", &body, false),
        ]);
        let u2 = compute_cache_usage(&cache, &turn2, 1);
        assert_eq!(u2.cache_read, 0, "无 cache_control 不应跨轮命中");
        assert_eq!(
            u2.cache_covered_est, 0,
            "无 cache_control 不应上报 creation"
        );
    }

    /// 参考实现只 canonicalize Anthropic billing header，避免该头漂移破坏命中。
    #[test]
    fn anthropic_billing_system_header_drift_does_not_break_cache_hit() {
        use super::super::types::{CacheControl, Message, MessagesRequest, SystemMessage};
        let stable_sys = "You are a coding assistant. ".repeat(200);
        let body = "implement the feature step by step ".repeat(180);

        let make_req = |billing_header: &str, msgs: Vec<Message>| MessagesRequest {
            model: "claude-opus-4-8".to_string(),
            max_tokens: 64,
            messages: msgs,
            stream: false,
            system: Some(vec![
                SystemMessage {
                    text: billing_header.to_string(),
                    cache_control: None,
                },
                SystemMessage {
                    text: stable_sys.clone(),
                    cache_control: Some(CacheControl {
                        cache_type: "ephemeral".to_string(),
                        ttl: None,
                    }),
                },
            ]),
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let cache = new_shared_cache();
        let u1 = compute_cache_usage(
            &cache,
            &make_req(
                "x-anthropic-billing-header: cc_version=1; cch=aaaa;",
                vec![
                    msg_with_cc("user", &body, false),
                    msg_with_cc("assistant", &body, false),
                    msg_with_cc("user", &body, false),
                ],
            ),
            1,
        );
        assert!(u1.cache_covered_est > 0);
        assert_eq!(u1.cache_read, 0, "turn1 无历史可命中");
        u1.commit_success();

        let u2 = compute_cache_usage(
            &cache,
            &make_req(
                "x-anthropic-billing-header: cc_version=2; cch=bbbbb; extra=1;",
                vec![
                    msg_with_cc("user", &body, false),
                    msg_with_cc("assistant", &body, false),
                    msg_with_cc("user", &body, false),
                    msg_with_cc("assistant", &body, false),
                    msg_with_cc("user", &body, false),
                ],
            ),
            1,
        );
        assert!(
            u2.cache_read > 0,
            "billing header drift should not break stable prefix hit"
        );
    }

    #[test]
    fn non_billing_dynamic_system_header_change_misses() {
        use super::super::types::{CacheControl, Message, MessagesRequest, SystemMessage};
        let stable = "stable tool and policy prompt ".repeat(240);
        let make_req = |dynamic: &str| MessagesRequest {
            model: "claude-opus-4-8".to_string(),
            max_tokens: 64,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::json!([{"type":"text","text":"hello"}]),
            }],
            stream: false,
            system: Some(vec![
                SystemMessage {
                    text: dynamic.to_string(),
                    cache_control: None,
                },
                SystemMessage {
                    text: stable.clone(),
                    cache_control: Some(CacheControl {
                        cache_type: "ephemeral".to_string(),
                        ttl: None,
                    }),
                },
            ]),
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let cache = new_shared_cache();
        let first = compute_cache_usage(&cache, &make_req("now=1001"), 1);
        assert_eq!(first.cache_read, 0);
        first.commit_success();
        let second = compute_cache_usage(&cache, &make_req("now=2002"), 1);
        assert_eq!(second.cache_read, 0, "普通 system 动态文本变化应导致 miss");
    }

    /// 参考实现按上游 credential 维度隔离，而不是按客户端 Key 维度隔离。
    #[test]
    fn same_credential_reuses_cache_across_client_keys() {
        let cache = new_shared_cache();
        let body = "shared system prompt and history ".repeat(180);
        let msgs = || {
            vec![
                msg_with_cc("user", &body, true),
                msg_with_cc("assistant", &body, false),
                msg_with_cc("user", &body, false),
            ]
        };
        // 这里 compute_cache_usage 的第三个参数应代表 credential_id。
        let a = compute_cache_usage(&cache, &req_with_messages(msgs()), 1);
        assert!(a.cache_covered_est > 0);
        assert_eq!(a.cache_read, 0);
        a.commit_success();
        let b = compute_cache_usage(&cache, &req_with_messages(msgs()), 1);
        assert!(b.cache_read > 0, "同一 credential 应复用缓存");
    }

    /// metadata session 不应参与缓存隔离；参考实现只按上游 credential 隔离。
    #[test]
    fn metadata_session_does_not_scope_cache() {
        use super::super::types::{Message, MessagesRequest, Metadata};
        let body = "conversation prefix that stays stable ".repeat(180);
        let make = |session: &str| MessagesRequest {
            model: "claude-opus-4-8".to_string(),
            max_tokens: 64,
            messages: vec![
                Message {
                    role: "user".into(),
                    content: serde_json::json!([{
                        "type":"text",
                        "text":body,
                        "cache_control":{"type":"ephemeral"}
                    }]),
                },
                Message {
                    role: "assistant".into(),
                    content: serde_json::json!([{"type":"text","text":body}]),
                },
                Message {
                    role: "user".into(),
                    content: serde_json::json!([{"type":"text","text":body}]),
                },
            ],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: Some(Metadata {
                user_id: Some(format!("user_abc_account__session_{session}")),
            }),
        };
        let cache = new_shared_cache();
        let s1a = compute_cache_usage(&cache, &make("aaa"), 7);
        assert_eq!(s1a.cache_read, 0);
        s1a.commit_success();
        let s2 = compute_cache_usage(&cache, &make("bbb"), 7);
        assert!(s2.cache_read > 0, "同一 credential 不应被不同 session 隔离");
    }

    #[test]
    fn failed_upstream_request_does_not_commit_cache_segments() {
        let cache = new_shared_cache();
        let req = req_with_messages(vec![
            msg_with_cc("user", &"cacheable prompt ".repeat(260), true),
            msg_with_cc("assistant", "ok", false),
            msg_with_cc("user", "next", false),
        ]);

        let failed_plan = compute_cache_usage(&cache, &req, 1);
        assert_eq!(failed_plan.cache_read, 0);
        assert!(failed_plan.cache_covered_est > 0);
        // Intentionally no commit_success(): this simulates 429/5xx/broken upstream.

        let retry_plan = compute_cache_usage(&cache, &req, 1);
        assert_eq!(
            retry_plan.cache_read, 0,
            "a failed upstream request must not poison simulated cache state"
        );

        retry_plan.commit_success();
        let later = compute_cache_usage(&cache, &req, 1);
        assert!(
            later.cache_read > 0,
            "successful commit should still enable later hits"
        );
    }

    /// token 口径纯净性：cum_tokens 只算原文，不含 role / 签名前缀 / 分隔符噪声。
    #[test]
    fn token_count_excludes_signature_noise() {
        use super::super::types::{Message, MessagesRequest};
        // 两条消息：第一条是历史（切段），内容为已知纯文本；最后一条占位（不切段）。
        let history_text = "the quick brown fox jumps over the lazy dog ".repeat(460);
        let req = MessagesRequest {
            model: "m".to_string(),
            max_tokens: 8,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::json!([{
                    "type": "text",
                        "text": history_text,
                    "cache_control": {"type": "ephemeral"}
                }]),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };
        let cache = new_shared_cache();
        let u = compute_cache_usage(&cache, &req, 1);
        // 历史段（第一条）的 covered 应严格等于纯文本 estimate——
        // 不含 "user" role、"block:" 前缀、"|" 分隔符的任何 token。
        let pure = estimate_tokens(&history_text) as i32;
        assert_eq!(
            u.cache_covered_est, pure,
            "covered 应只算原文 token，实测 {} vs 纯文本 {}",
            u.cache_covered_est, pure
        );
    }

    /// 含图片的历史段：covered 应计入图片的 Anthropic 口径 token，且跨轮稳定命中。
    #[test]
    fn image_block_contributes_tokens_and_hits() {
        use super::super::types::{Message, MessagesRequest};
        // 用 image_resize 的同款 PNG 生成器造一张 1200×1200（>1024 token）的真图。
        let png = make_test_png(1200, 1200);
        let img_tokens = crate::image_resize::estimate_image_tokens("image/png", &png) as i32;
        assert!(
            img_tokens > 100,
            "前提：测试图应有可观 token，实测 {img_tokens}"
        );

        let make = |trailing: &str| MessagesRequest {
            model: "m".to_string(),
            max_tokens: 8,
            messages: vec![
                Message {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type":"image","source":{"type":"base64","media_type":"image/png","data": png}},
                        {"type":"text","text":"describe","cache_control":{"type":"ephemeral"}}
                    ]),
                },
                Message {
                    role: "assistant".to_string(),
                    content: serde_json::json!("a pixel"),
                },
                Message {
                    role: "user".to_string(),
                    content: serde_json::json!(trailing),
                },
            ],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let cache = new_shared_cache();
        // Turn 1：含图的 user 是历史第一段，其 covered 必须包含图片 token。
        let u1 = compute_cache_usage(&cache, &make("q1"), 1);
        let text_only = estimate_tokens("describe") as i32;
        // 最深历史段至少覆盖到 [含图user] 段，covered 应 ≥ 图片 token（远大于纯文本）。
        assert!(
            u1.cache_covered_est >= img_tokens + text_only - 5,
            "covered({}) 应含图片 token({})",
            u1.cache_covered_est,
            img_tokens
        );
        assert_eq!(u1.cache_read, 0);
        u1.commit_success();

        // Turn 2：追加一轮，含图历史逐字节不变 → 命中（read 含图片 token）。
        let u2 = compute_cache_usage(&cache, &make("q2"), 1);
        assert!(
            u2.cache_read >= img_tokens,
            "含图历史应跨轮命中且 read({}) 含图片 token({})",
            u2.cache_read,
            img_tokens
        );
    }

    #[test]
    fn below_model_minimum_tokens_are_not_cacheable() {
        let cache = new_shared_cache();
        let req = req_with_messages(vec![msg_with_cc("user", "tiny", true)]);

        let usage = compute_cache_usage(&cache, &req, 1);

        assert_eq!(
            usage.cache_covered_est, 0,
            "Anthropic prompt cache should ignore breakpoints below the model minimum token threshold"
        );
        assert_eq!(usage.cache_read, 0);
    }

    #[test]
    fn haiku_models_use_anthropic_2048_prompt_cache_minimum() {
        assert_eq!(
            minimum_cacheable_tokens_for_model("claude-3-haiku-20240307"),
            2048
        );
        assert_eq!(
            minimum_cacheable_tokens_for_model("claude-3-5-haiku-20241022"),
            2048
        );
    }

    #[test]
    fn lookup_only_considers_last_ten_cacheable_breakpoints() {
        let cache = new_shared_cache();
        let mut messages = Vec::new();
        for idx in 0..12 {
            messages.push(msg_with_cc(
                "user",
                &format!("stable prefix block {idx} {}", "cache token ".repeat(220)),
                true,
            ));
        }
        let full = req_with_messages(messages);
        let full_usage = compute_cache_usage(&cache, &full, 1);
        assert!(full_usage.cache_covered_est > 0);

        let first_hash = extract_segments(&full, 1).0[0].hash;
        let first_tokens = extract_segments(&full, 1).0[0].cumulative_tokens;
        cache.record(&[first_hash], &[first_tokens], 300);

        let retry = compute_cache_usage(&cache, &full, 1);

        assert_eq!(
            retry.cache_read, 0,
            "cache lookup should ignore matches older than the last 10 cacheable breakpoints"
        );
    }

    #[test]
    fn mixed_ttl_breakpoints_expire_independently() {
        let cache = new_shared_cache();
        let req = req_with_messages(vec![
            msg_with_ttl("user", &"one hour prefix ".repeat(260), "1h"),
            msg_with_ttl("assistant", &"five minute suffix ".repeat(260), "5m"),
        ]);

        let first = compute_cache_usage(&cache, &req, 1);
        assert_eq!(first.cache_read, 0);
        assert_eq!(
            first.cache_creation_1h, 0,
            "reference attributes creation to the last cacheable breakpoint TTL"
        );
        assert!(
            first.cache_creation_5m > 0,
            "last breakpoint should be 5m cache creation"
        );
        first.commit_success();

        let segments = extract_segments(&req, 1).0;
        assert_eq!(
            segments.len(),
            2,
            "test setup should create two explicit breakpoints"
        );
        {
            let mut inner = cache.inner.lock();
            inner
                .entries
                .get_mut(&segments[1].hash)
                .expect("5m segment should have been recorded")
                .expires_at = now_secs() - 1;
        }

        let second = compute_cache_usage(&cache, &req, 1);

        assert_eq!(
            second.cache_read, segments[0].cumulative_tokens as i32,
            "after the 5m suffix expires, the older 1h prefix should still be readable"
        );
        assert_eq!(
            second.cache_creation_5m,
            segments[1]
                .cumulative_tokens
                .saturating_sub(segments[0].cumulative_tokens) as i32,
            "only the expired 5m suffix should be recreated"
        );
        assert_eq!(
            second.cache_creation_1h, 0,
            "the still-valid 1h prefix should not be recreated"
        );
    }

    fn msg_with_ttl(role: &str, text: &str, ttl: &str) -> super::super::types::Message {
        use super::super::types::Message;
        Message {
            role: role.to_string(),
            content: serde_json::json!([{
                "type": "text",
                "text": text,
                "cache_control": {"type": "ephemeral", "ttl": ttl}
            }]),
        }
    }

    /// 测试用 PNG 生成器（与 image_resize 测试同款，渐变填充更接近真实压缩比）。
    fn make_test_png(w: u32, h: u32) -> String {
        use base64::{Engine, engine::general_purpose::STANDARD as B64};
        use image::{ImageFormat, Rgb, RgbImage};
        use std::io::Cursor;
        let mut img = RgbImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                img.put_pixel(x, y, Rgb([(x % 256) as u8, (y % 256) as u8, 128]));
            }
        }
        let mut buf = Vec::new();
        img.write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)
            .unwrap();
        B64.encode(&buf)
    }
}
