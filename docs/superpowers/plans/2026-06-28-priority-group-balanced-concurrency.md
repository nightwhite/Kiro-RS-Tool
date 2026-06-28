# 优先级分层均衡并发 实现计划

> **面向 AI 代理的工作者：** 必需子技能：使用 superpowers:subagent-driven-development（推荐）或 superpowers:executing-plans 逐任务实现此计划。步骤使用复选框（`- [ ]`）语法来跟踪进度。

**目标：** 在当前单实例进程内并发限制基础上，新增“优先级分层 + 层内均衡”调度，让同一层账号均匀承载，上一层容量耗尽或冷却后才下沉到下一层。

**架构：** 保留现有 `priority` 和 `balanced` 模式，新增 `priority_group_balanced` 模式。凭据新增 `priorityGroup` 字段，调度器先过滤不可用账号，再选择最小优先组，并在组内按 `inFlight / concurrentLimit` 最低的账号占位。

**技术栈：** Rust 后端（Axum、parking_lot Mutex、serde）、React/TypeScript 管理面板、Cargo tests、Vite build。

---

## 文件结构

- 修改：`src/kiro/model/credentials.rs`
  - 增加 `priority_group: u32` 字段，JSON 为 `priorityGroup`，默认 0，默认值不序列化。
- 修改：`src/kiro/token_manager.rs`
  - 快照增加 `priority_group`。
  - 新增 `priority_group_balanced` 调度分支。
  - 添加组内均衡、组满下沉、冷却跳过的单元测试。
- 修改：`src/model/config.rs`
  - 允许 `loadBalancingMode = "priority_group_balanced"` 持久化。
- 修改：`src/admin/types.rs`
  - `CredentialStatusItem`、`AddCredentialRequest`、`UpdateCredentialRequest` 增加 `priorityGroup`。
- 修改：`src/admin/service.rs`
  - 管理接口读写 `priorityGroup`。
  - `set_load_balancing_mode` 接受新模式。
- 修改：`admin-ui/src/types/api.ts`
  - TypeScript 类型增加 `priorityGroup`。
  - `getLoadBalancingMode` / `setLoadBalancingMode` 类型扩展新模式。
- 修改：`admin-ui/src/components/credential-card.tsx`
  - 卡片显示优先组。
- 修改：`admin-ui/src/components/edit-credential-dialog.tsx`
  - 编辑凭据时可设置优先组。
- 修改：`admin-ui/src/hooks/use-credentials.ts` 或相关配置组件（按现有入口定位）
  - 管理面板负载均衡模式选项增加“优先级分层均衡”。
- 修改：`README.md`、`config.example.json`
  - 记录新模式和字段。

---

### 任务 1：为凭据模型增加优先组字段

**文件：**
- 修改：`src/kiro/model/credentials.rs`
- 测试：`src/kiro/model/credentials.rs`

- [ ] **步骤 1：编写失败的序列化测试**

在 `src/kiro/model/credentials.rs` 的测试模块中增加：

```rust
#[test]
fn test_priority_group_default_and_explicit() {
    let default_creds: KiroCredentials = serde_json::from_str(r#"{"refreshToken":"t"}"#).unwrap();
    assert_eq!(default_creds.priority_group, 0);

    let explicit: KiroCredentials = serde_json::from_str(
        r#"{"refreshToken":"t","priorityGroup":2}"#,
    )
    .unwrap();
    assert_eq!(explicit.priority_group, 2);

    let json = serde_json::to_string(&default_creds).unwrap();
    assert!(!json.contains("priorityGroup"));
}
```

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo test test_priority_group_default_and_explicit`

预期：FAIL，报错包含 `no field priority_group`。

- [ ] **步骤 3：实现字段**

在 `KiroCredentials` 中增加：

```rust
/// 优先组；数字越小越先使用。同组内可均衡分摊。
#[serde(default)]
#[serde(skip_serializing_if = "is_zero")]
pub priority_group: u32,
```

在 `Debug` 输出中增加：

```rust
.field("priority_group", &self.priority_group)
```

所有显式 `KiroCredentials { ... }` 字面量补：

```rust
priority_group: 0,
```

- [ ] **步骤 4：运行测试验证通过**

运行：`cargo test test_priority_group_default_and_explicit`

预期：PASS。

---

### 任务 2：后端 API 透出和写入优先组

**文件：**
- 修改：`src/admin/types.rs`
- 修改：`src/admin/service.rs`
- 修改：`src/kiro/token_manager.rs`

- [ ] **步骤 1：编写失败的管理层测试**

在 `src/kiro/token_manager.rs` 测试模块中增加：

```rust
#[test]
fn test_snapshot_includes_priority_group() {
    let config = Config::default();
    let mut cred = KiroCredentials::default();
    cred.priority_group = 2;

    let manager = MultiTokenManager::new(config, vec![cred], None, None, false).unwrap();
    let snapshot = manager.snapshot();

    assert_eq!(snapshot.entries[0].priority_group, 2);
}
```

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo test test_snapshot_includes_priority_group`

预期：FAIL，报错包含 `no field priority_group`。

- [ ] **步骤 3：扩展快照和 Admin 类型**

在 `CredentialEntrySnapshot` 增加：

```rust
pub priority_group: u32,
```

在 `snapshot()` 映射中增加：

```rust
priority_group: e.credentials.priority_group,
```

在 `CredentialStatusItem` 增加：

```rust
pub priority_group: u32,
```

在 `AdminService::get_all_credentials()` 映射中增加：

```rust
priority_group: entry.priority_group,
```

- [ ] **步骤 4：支持新增和编辑写入**

在 `AddCredentialRequest` 增加：

```rust
#[serde(default)]
pub priority_group: u32,
```

创建 `KiroCredentials` 时设置：

```rust
priority_group: req.priority_group,
```

在 `UpdateCredentialRequest` 增加：

```rust
pub priority_group: Option<u32>,
```

扩展 `MultiTokenManager::update_credential(...)` 参数：

```rust
priority_group: Option<u32>,
```

在更新逻辑中：

```rust
if let Some(value) = priority_group {
    entry.credentials.priority_group = value;
}
```

更新所有调用点，未修改优先组的调用传 `None`。

- [ ] **步骤 5：运行测试验证通过**

运行：`cargo test test_snapshot_includes_priority_group`

预期：PASS。

---

### 任务 3：新增优先级分层均衡调度模式

**文件：**
- 修改：`src/kiro/token_manager.rs`
- 修改：`src/model/config.rs`
- 修改：`src/admin/service.rs`

- [ ] **步骤 1：编写组内均衡失败测试**

在 `src/kiro/token_manager.rs` 测试模块中增加：

```rust
#[tokio::test]
async fn test_priority_group_balanced_spreads_within_highest_group() {
    let mut config = Config::default();
    config.load_balancing_mode = "priority_group_balanced".to_string();

    let mut a = KiroCredentials::default();
    a.access_token = Some("a".to_string());
    a.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
    a.priority_group = 0;
    a.concurrent_limit = Some(3);

    let mut b = KiroCredentials::default();
    b.access_token = Some("b".to_string());
    b.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
    b.priority_group = 0;
    b.concurrent_limit = Some(3);

    let manager = Arc::new(MultiTokenManager::new(config, vec![a, b], None, None, false).unwrap());

    let first = manager.acquire_call(None).await.unwrap();
    let second = manager.acquire_call(None).await.unwrap();

    assert_ne!(first.id, second.id);
}
```

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo test test_priority_group_balanced_spreads_within_highest_group`

预期：FAIL，两个请求命中同一账号或模式被当成旧 priority。

- [ ] **步骤 3：实现模式常量和验证**

在 `src/model/config.rs` 注释中说明 `priority_group_balanced` 是合法值。

在 `AdminService::set_load_balancing_mode()` 校验中允许：

```rust
if req.mode != "priority" && req.mode != "balanced" && req.mode != "priority_group_balanced" {
```

在 `MultiTokenManager::set_load_balancing_mode()` 校验中同样允许。

- [ ] **步骤 4：实现调度分支**

在 `try_acquire_concurrency_slot()` 的 `match mode.as_str()` 增加：

```rust
"priority_group_balanced" => entries
    .iter()
    .enumerate()
    .filter(|(_, e)| {
        !e.disabled
            && !e.throttled_until.map(|t| t > now).unwrap_or(false)
            && !e.rate_limited_until.map(|t| t > now).unwrap_or(false)
            && (!is_opus || e.credentials.supports_opus())
            && e.in_flight < self.credential_concurrent_limit(&e.credentials)
    })
    .min_by(|(_, a), (_, b)| {
        let a_limit = self.credential_concurrent_limit(&a.credentials);
        let b_limit = self.credential_concurrent_limit(&b.credentials);
        (
            a.credentials.priority_group,
            a.in_flight * b_limit,
            a.success_count,
            a.credentials.priority,
        )
            .cmp(&(
                b.credentials.priority_group,
                b.in_flight * a_limit,
                b.success_count,
                b.credentials.priority,
            ))
    })
    .map(|(idx, _)| idx),
```

这里用交叉乘法比较 `inFlight / limit`，避免浮点数。

- [ ] **步骤 5：运行组内均衡测试**

运行：`cargo test test_priority_group_balanced_spreads_within_highest_group`

预期：PASS。

---

### 任务 4：覆盖组满下沉和冷却跳过

**文件：**
- 修改：`src/kiro/token_manager.rs`

- [ ] **步骤 1：编写组满下沉失败测试**

增加测试：

```rust
#[tokio::test]
async fn test_priority_group_balanced_falls_through_when_group_full() {
    let mut config = Config::default();
    config.load_balancing_mode = "priority_group_balanced".to_string();

    let mut pro = KiroCredentials::default();
    pro.access_token = Some("pro".to_string());
    pro.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
    pro.priority_group = 0;
    pro.concurrent_limit = Some(1);

    let mut pro_plus = KiroCredentials::default();
    pro_plus.access_token = Some("pro-plus".to_string());
    pro_plus.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
    pro_plus.priority_group = 1;
    pro_plus.concurrent_limit = Some(1);

    let manager = Arc::new(MultiTokenManager::new(config, vec![pro, pro_plus], None, None, false).unwrap());

    let first = manager.acquire_call(None).await.unwrap();
    let second = manager.acquire_call(None).await.unwrap();

    assert_eq!(first.id, 1);
    assert_eq!(second.id, 2);
}
```

- [ ] **步骤 2：编写冷却跳过测试**

增加测试：

```rust
#[tokio::test]
async fn test_priority_group_balanced_skips_rate_limited_account() {
    let mut config = Config::default();
    config.load_balancing_mode = "priority_group_balanced".to_string();

    let mut a = KiroCredentials::default();
    a.access_token = Some("a".to_string());
    a.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
    a.priority_group = 0;

    let mut b = KiroCredentials::default();
    b.access_token = Some("b".to_string());
    b.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
    b.priority_group = 0;

    let manager = Arc::new(MultiTokenManager::new(config, vec![a, b], None, None, false).unwrap());
    assert!(manager.report_rate_limited(1, StdDuration::from_secs(60)));

    let ctx = manager.acquire_call(None).await.unwrap();
    assert_eq!(ctx.id, 2);
}
```

- [ ] **步骤 3：运行测试验证失败或通过**

运行：

```bash
cargo test test_priority_group_balanced_falls_through_when_group_full
cargo test test_priority_group_balanced_skips_rate_limited_account
```

预期：如果任务 3 已正确实现，两个测试 PASS；否则修调度过滤。

- [ ] **步骤 4：运行完整 token manager 测试**

运行：`cargo test kiro::token_manager::tests::`

预期：全部 PASS。

---

### 任务 5：前端类型和编辑面板支持优先组

**文件：**
- 修改：`admin-ui/src/types/api.ts`
- 修改：`admin-ui/src/components/credential-card.tsx`
- 修改：`admin-ui/src/components/edit-credential-dialog.tsx`

- [ ] **步骤 1：扩展 TypeScript 类型**

在 `CredentialStatusItem` 增加：

```ts
priorityGroup: number
```

在 `AddCredentialRequest` 和 `UpdateCredentialRequest` 增加：

```ts
priorityGroup?: number
```

- [ ] **步骤 2：卡片展示优先组**

在 `credential-card.tsx` 信息区加入：

```tsx
<div className="flex items-center justify-between gap-2">
  <dt className="text-muted-foreground">优先组</dt>
  <dd className="tabular-nums font-medium">{credential.priorityGroup}</dd>
</div>
```

- [ ] **步骤 3：编辑弹窗支持优先组**

在 `edit-credential-dialog.tsx` 增加状态：

```ts
const [priorityGroup, setPriorityGroup] = useState(String(credential.priorityGroup ?? 0))
```

打开弹窗时重置：

```ts
setPriorityGroup(String(credential.priorityGroup ?? 0))
```

提交前校验：

```ts
const parsedPriorityGroup = Number(priorityGroup)
if (!Number.isInteger(parsedPriorityGroup) || parsedPriorityGroup < 0) {
  toast.error('优先组必须是 0 或正整数')
  return
}
```

提交请求中加入：

```ts
priorityGroup: parsedPriorityGroup,
```

表单加入输入：

```tsx
<div className="space-y-2">
  <label htmlFor="priorityGroup" className="text-sm font-medium">优先组</label>
  <Input
    id="priorityGroup"
    type="number"
    min="0"
    value={priorityGroup}
    onChange={(e) => setPriorityGroup(e.target.value)}
    disabled={isPending}
  />
  <p className="text-xs text-muted-foreground">
    数字越小越先使用；同一优先组会按实时并发分摊
  </p>
</div>
```

- [ ] **步骤 4：运行前端构建**

运行：`bun run --cwd admin-ui build`

预期：PASS。

---

### 任务 6：负载均衡模式配置支持新选项

**文件：**
- 修改：`admin-ui/src/api/credentials.ts`
- 修改：`admin-ui/src/hooks/use-credentials.ts`
- 修改：实际渲染负载均衡模式的组件（用 `rg "loadBalancingMode|setLoadBalancingMode|负载均衡" admin-ui/src` 定位）

- [ ] **步骤 1：扩展 API 类型**

将类型从：

```ts
'priority' | 'balanced'
```

改为：

```ts
type LoadBalancingMode = 'priority' | 'balanced' | 'priority_group_balanced'
```

`getLoadBalancingMode()` 和 `setLoadBalancingMode()` 使用该类型。

- [ ] **步骤 2：新增 UI 选项**

在负载均衡模式选择器中增加：

```tsx
<option value="priority_group_balanced">优先级分层均衡</option>
```

如果该组件使用按钮或卡片，则新增同等选项，展示文案：

```text
优先级分层均衡：先使用较小优先组，同组账号按实时并发分摊
```

- [ ] **步骤 3：运行前端构建**

运行：`bun run --cwd admin-ui build`

预期：PASS。

---

### 任务 7：文档和示例配置

**文件：**
- 修改：`README.md`
- 修改：`config.example.json`

- [ ] **步骤 1：更新示例配置**

在 `config.example.json` 中增加或调整：

```json
"loadBalancingMode": "priority_group_balanced",
"defaultConcurrencyLimit": 3
```

- [ ] **步骤 2：更新 README 配置说明**

在负载均衡说明处写明：

```markdown
- `priority`：优先使用当前最高优先级凭据，满并发或不可用时切换。
- `balanced`：在所有可用凭据中按使用量均衡。
- `priority_group_balanced`：先按 `priorityGroup` 分层，组内按 `inFlight / concurrentLimit` 均衡；当前组满并发、禁用或冷却后才下沉到下一组。
```

在凭据字段表增加：

```markdown
| `priorityGroup` | number | 优先组，数字越小越先使用；同组内可均衡分摊，默认 0 |
| `concurrentLimit` | number | 单凭据并发上限；未配置时按订阅或全局默认计算 |
```

- [ ] **步骤 3：文档自检**

运行：`rg -n "priority_group_balanced|priorityGroup|concurrentLimit|defaultConcurrencyLimit" README.md config.example.json`

预期：能查到所有新增字段说明。

---

### 任务 8：最终验证

**文件：**
- 不修改文件

- [ ] **步骤 1：Rust 格式化**

运行：`cargo fmt`

预期：无输出。

- [ ] **步骤 2：后端测试**

运行：`cargo test kiro::token_manager::tests::`

预期：全部 PASS。

- [ ] **步骤 3：后端编译检查**

运行：`cargo check`

预期：PASS。

- [ ] **步骤 4：前端构建**

运行：`bun run --cwd admin-ui build`

预期：`tsc -b && vite build` 成功。

- [ ] **步骤 5：行为复核**

用测试结果复核以下行为：

```text
同优先组：按 inFlight / limit 分摊
高优先组满：下沉下一组
账号 429 冷却：跳过该账号
全部满：返回 429，不排队
permit 释放：请求结束后 inFlight 归零
```

---

## 风险和边界

- 本计划仍然是单实例进程内并发限制；多 Docker 副本不会共享 `inFlight`。
- 不新增排队、等待、超时策略；全部满时继续直接 429。
- 不改变旧模式含义，降低回归风险。
- `priorityGroup` 是人工配置，不自动根据 Pro/Pro+/Power 改组；这样避免误识别订阅导致调度变化。
