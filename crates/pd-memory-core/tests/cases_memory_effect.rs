//! 记忆效果用例集（C1–C8）：验证记忆系统“真的记住了”，而不是只测内部函数。
//!
//! 这些用例与 `crates/pd-memory-core/tests/` 下已有的单元/流水线测试互补：
//! 每个用例都从**真实调用链**出发（[`MemoryEngine`] + `FileMemoryStore` +
//! `FileControlPlane`，临时目录隔离，注入固定时钟和确定性模型），断言的是
//! 用户/宿主可观察的记忆效果：
//!
//! - 记忆文件是否按预期的类型 / 作用域 / 名称 / 正文落盘；
//! - `MEMORY.md`（manifest）是否同步、链接是否可读；
//! - 后续会话的 `retrieve_context` 是否命中事实；
//! - 项目之间是否隔离、全局用户记忆是否跨项目共享；
//! - 重复 capture / flush 是否幂等、模型故障是否按上游降级；
//! - 模型故障时召回是否安全降级（不 panic、不注入错误内容）；
//! - Dream 合并与回滚是否可逆（回滚后记录文件字节级恢复）；
//! - 中文 / emoji / 超长内容是否安全往返与截断。
//!
//! 用例编号与 `docs/memory-test-cases.zh.md` 一一对应。
//!
//! C8（MCP 工具面语义）**不在本文件**：`pd-memory-mcp` 依赖
//! `pd-memory-core`，core 的集成测试若再依赖 mcp 会形成 cargo 依赖环（同一
//! crate 不允许被自身 dev-dependency 反向依赖）。该语义由
//! `crates/pd-memory-mcp/tests/engine_roundtrip.rs` 直接覆盖（`memory_search`
//! / `memory_overview` / `memory_add` 都在那里断言），因此这里按任务约定跳过
//! 并在中文用例文档中说明分工。
//!
//! 运行：
//!
//! ```text
//! cargo test -p pd-memory-core --test cases_memory_effect -- --nocapture
//! ```
//!
//! 约束：完全离线、确定性、无 sleep。时间戳要么显式传给 capture，要么由
//! [`FixedClock`] 提供；模型响应用本文件内的 [`EffectModel`] 确定性脚本。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};

use pd_memory_core::config::MemoryConfig;
use pd_memory_core::control_plane::FileControlPlane;
use pd_memory_core::engine::{FlushOptions, MemoryEngine, MemoryEngineOptions};
use pd_memory_core::error::{MemoryError, Result as MemoryResult};
use pd_memory_core::files::FileMemoryStore;
use pd_memory_core::frontmatter::parse_markdown;
use pd_memory_core::manifest::MANIFEST_FILE;
use pd_memory_core::ports::{
    Clock, CompletionRequest, CompletionResponse, MemoryStore, ModelProvider, NoopTelemetry,
    QueueStore, Redactor, StateStore,
};
use pd_memory_core::recall::RetrieveOptions;
use pd_memory_core::redact::DefaultRedactor;
use pd_memory_core::types::{
    CaptureTurnResult, DreamPipelineStatus, DreamTrigger, HeartbeatStats, MemoryCandidate,
    MemoryListOptions, MemoryManifestEntry, MemoryMessage, MemoryRecordType, MemoryRoute,
    MemoryScope, RetrievalResult,
};

// ---------------------------------------------------------------------------
// 固定时钟：保证 capture 的 created_at、retrieve trace、dream 快照元数据稳定。
// ---------------------------------------------------------------------------

/// 不会自己走动、只能由测试显式推进的 [`Clock`]。
struct FixedClock {
    now: Mutex<DateTime<Utc>>,
}

impl FixedClock {
    fn new(iso: &str) -> Self {
        let parsed = DateTime::parse_from_rfc3339(iso).expect("fixed clock timestamp is RFC 3339");
        Self {
            now: Mutex::new(parsed.with_timezone(&Utc)),
        }
    }

    /// 模拟会话之间时间流逝（只影响 injected clock，不影响文件系统 wall clock）。
    fn advance_minutes(&self, minutes: i64) {
        let mut now = self.now.lock().expect("fixed clock mutex");
        *now = now.clone() + Duration::minutes(minutes);
    }
}

impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        self.now.lock().expect("fixed clock mutex").clone()
    }
}

// ---------------------------------------------------------------------------
// 确定性模型：按 schema 返回脚本化的“像人一样”的记忆决策。
// ---------------------------------------------------------------------------

/// 脚本化故障模式，用于“召回降级 / 抽取降级”用例。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum FailMode {
    #[default]
    Off,
    /// 返回不可解析的非 JSON 文本（单次解析失败）。
    InvalidJson,
    /// 直接返回传输层错误（provider 不可用）。
    Transport,
}

/// [`EffectModel`] 的行为开关；测试按需修改。
#[derive(Clone)]
struct Plan {
    /// gate 正常时返回的召回路由。
    gate_route: MemoryRoute,
    gate_fail: FailMode,
    select_fail: FailMode,
    /// 当 classify 的 focus 文本包含该标记时注入模型故障（C4 抽取降级）。
    classify_fail_marker: Option<String>,
    /// 覆盖 create_note 的返回（C7 中文/emoji/超长内容）。
    note_override: Option<Value>,
    /// Dream merge 的脚本化返回（C6）。
    dream_merge: Option<Value>,
    /// Dream 全局用户画像重写的脚本化返回（C2）。
    dream_user_profile: Option<Value>,
}

impl Default for Plan {
    fn default() -> Self {
        Self {
            gate_route: MemoryRoute::Project,
            gate_fail: FailMode::Off,
            select_fail: FailMode::Off,
            classify_fail_marker: None,
            note_override: None,
            dream_merge: None,
            dream_user_profile: None,
        }
    }
}

/// 一个只依赖 prompt 内容的确定性 [`ModelProvider`]：
///
/// - `memory_classify`：按 focus 文本的关键词判断类别（user / feedback / project）；
/// - `memory_create_*`：返回“像真实抽取结果”的中文笔记；
/// - `memory_gate` / `memory_select_files`：按 plan 返回路由、选择全部候选；
/// - `dream_*`：按 plan 返回合并/画像脚本，cluster 自动把全部 header 归为一簇。
///
/// 它不访问网络、不读环境变量、不 sleep，同一输入永远得到同一输出。
struct EffectModel {
    plan: Mutex<Plan>,
}

impl EffectModel {
    fn new() -> Self {
        Self {
            plan: Mutex::new(Plan::default()),
        }
    }

    fn set_gate_route(&self, route: MemoryRoute) {
        self.plan.lock().expect("model plan").gate_route = route;
    }

    fn set_gate_failure(&self, mode: FailMode) {
        self.plan.lock().expect("model plan").gate_fail = mode;
    }

    fn set_select_failure(&self, mode: FailMode) {
        self.plan.lock().expect("model plan").select_fail = mode;
    }

    fn set_classify_failure_marker(&self, marker: Option<&str>) {
        self.plan.lock().expect("model plan").classify_fail_marker = marker.map(str::to_string);
    }

    fn set_note_override(&self, payload: Value) {
        self.plan.lock().expect("model plan").note_override = Some(payload);
    }

    fn set_dream_merge(&self, payload: Value) {
        self.plan.lock().expect("model plan").dream_merge = Some(payload);
    }

    fn set_dream_user_profile(&self, payload: Value) {
        self.plan.lock().expect("model plan").dream_user_profile = Some(payload);
    }
}

impl ModelProvider for EffectModel {
    fn complete(&self, request: CompletionRequest) -> MemoryResult<CompletionResponse> {
        let plan = self.plan.lock().expect("model plan");
        let text = match request.schema_name.as_str() {
            "memory_gate" => match plan.gate_fail {
                FailMode::Off => json!({ "route": plan.gate_route.as_str() }).to_string(),
                FailMode::InvalidJson => "gate: this is definitely not json".to_string(),
                FailMode::Transport => {
                    return Err(MemoryError::Model(
                        "scripted gate transport failure".to_string(),
                    ))
                }
            },
            "memory_select_files" => match plan.select_fail {
                FailMode::Off => select_response(&request.user),
                FailMode::InvalidJson => "select: this is definitely not json".to_string(),
                FailMode::Transport => {
                    return Err(MemoryError::Model(
                        "scripted selection transport failure".to_string(),
                    ))
                }
            },
            "memory_classify" => {
                let focus = focus_user_content(&request.user);
                if let Some(marker) = plan.classify_fail_marker.as_deref() {
                    if focus.contains(marker) {
                        return Err(MemoryError::Model(format!(
                            "scripted classify failure for marker {marker}"
                        )));
                    }
                }
                classify_response(&focus)
            }
            "memory_create_project" | "memory_create_user" | "memory_create_feedback" => {
                match plan.note_override.as_ref() {
                    Some(payload) => payload.to_string(),
                    None => default_note(&request.schema_name),
                }
            }
            "dream_cluster" => dream_cluster_response(&request.user),
            "dream_merge" => plan
                .dream_merge
                .as_ref()
                .map(Value::to_string)
                .ok_or_else(|| MemoryError::Model("no scripted dream merge".to_string()))?,
            "dream_project_meta" => {
                json!({ "should_update": false, "reason": "keep the current metadata" }).to_string()
            }
            "dream_user_profile" => plan
                .dream_user_profile
                .as_ref()
                .map(Value::to_string)
                .ok_or_else(|| {
                    MemoryError::Model("no scripted user profile rewrite".to_string())
                })?,
            other => {
                return Err(MemoryError::Model(format!(
                    "research test model has no script for schema {other}"
                )))
            }
        };
        Ok(CompletionResponse { text, usage: None })
    }
}

/// 从 classify prompt（`PromptWindow` JSON）里取出 focus user 文本。
fn focus_user_content(user_prompt: &str) -> String {
    serde_json::from_str::<Value>(user_prompt)
        .ok()
        .and_then(|value| {
            value
                .pointer("/focus_user_turn/content")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default()
}

/// 关键词规则：模拟真模型对“身份 / 协作约定 / 项目事实”的分类。
fn classify_response(focus: &str) -> String {
    let kind = if focus.contains("身份") || focus.contains("我是") {
        "user"
    } else if focus.contains("以后")
        || focus.contains("所有文档")
        || focus.contains("约定")
        || focus.contains("规则")
    {
        "feedback"
    } else {
        "project"
    };
    json!({
        "should_store": true,
        "labels": [{
            "type": kind,
            "reason": "deterministic test classification",
            "evidence": focus.chars().take(80).collect::<String>(),
        }]
    })
    .to_string()
}

/// 默认 create_note 返回：中文、可读、且带用例要断言的事实片段。
fn default_note(schema: &str) -> String {
    match schema {
        "memory_create_feedback" => json!({
            "name": "文档语言与包管理约定",
            "description": "所有文档用中文写，项目统一使用 pnpm",
            "markdown": "## Rule\n- 以后所有文档用中文写。\n- 项目统一使用 pnpm。\n",
        }),
        "memory_create_user" => json!({
            "name": "用户身份：后端工程师",
            "description": "用户是一名后端工程师",
            "markdown": "## 身份背景\n- 用户是后端工程师，负责支付系统。\n- 长期在金融科技领域工作。\n",
        }),
        _ => json!({
            "name": "Boreal 项目状态",
            "description": "本地知识库整理工具，当前处于设计阶段",
            "markdown": "## Summary\nBoreal 是一个本地知识库整理工具。\n\n## Current Stage\n设计阶段。\n",
        }),
    }
    .to_string()
}

/// select 的默认策略：从 selection prompt 的 manifest 里选择全部候选（≤ limit）。
fn select_response(user_prompt: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(user_prompt) else {
        return "{}".to_string();
    };
    let limit = value.get("limit").and_then(Value::as_u64).unwrap_or(5) as usize;
    let selected: Vec<&str> = value
        .get("manifest")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("id").and_then(Value::as_str))
                .take(limit)
                .collect()
        })
        .unwrap_or_default();
    json!({ "selected_ids": selected }).to_string()
}

/// cluster 的默认策略：把传进来的全部 header 合并为一簇（用于 C6）。
fn dream_cluster_response(user_prompt: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(user_prompt) else {
        return json!({ "summary": "unparseable", "clusters": [] }).to_string();
    };
    let member_relative_paths: Vec<&str> = value
        .get("headers")
        .and_then(Value::as_array)
        .map(|headers| {
            headers
                .iter()
                .filter_map(|header| header.get("relative_path").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    if member_relative_paths.len() < 2 {
        return json!({ "summary": "keep separate", "clusters": [] }).to_string();
    }
    json!({
        "summary": "merge duplicated delivery rules",
        "clusters": [{
            "member_relative_paths": member_relative_paths,
            "reason": "两条交付规则内容重复，可以合并为一条",
        }]
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// 引擎 / 断言辅助函数
// ---------------------------------------------------------------------------

const T0: &str = "2026-10-18T09:00:00.000Z";
const T1: &str = "2026-10-18T09:05:00.000Z";
const T2: &str = "2026-10-18T09:10:00.000Z";
const CLOCK_BASE: &str = "2026-10-19T08:00:00.000Z";

/// 每个用例独立 data root（`<temp>/data`）+ 显式 workspace id。
fn test_config(root: &Path, workspace_id: &str) -> MemoryConfig {
    let mut config = MemoryConfig::for_data_root(root.join("data"));
    config.workspace_id = Some(workspace_id.to_string());
    config
}

/// 用真实 `FileMemoryStore` + `FileControlPlane` + 固定时钟构造引擎。
fn open_engine(
    config: MemoryConfig,
    model: &Arc<EffectModel>,
    clock: &Arc<FixedClock>,
) -> MemoryEngine {
    let layout = config.store_layout();
    let control_dir = layout.roots.workspace_dir.clone();
    let store: Box<dyn MemoryStore> = Box::new(FileMemoryStore::new(layout.roots));
    let queue: Box<dyn QueueStore> =
        Box::new(FileControlPlane::new(control_dir.clone()).expect("queue control plane"));
    let state: Box<dyn StateStore> =
        Box::new(FileControlPlane::new(control_dir).expect("state control plane"));
    let model: Arc<dyn ModelProvider> = model.clone();
    let clock: Arc<dyn Clock> = clock.clone();
    let redactor: Arc<dyn Redactor> = Arc::new(DefaultRedactor::new(config.redaction.clone()));
    MemoryEngine::new(MemoryEngineOptions {
        config,
        store,
        queue,
        state,
        model,
        clock,
        telemetry: Arc::new(NoopTelemetry),
        redactor,
    })
    .expect("engine opens over the tempdir")
}

fn capture_turn(
    engine: &MemoryEngine,
    session: &str,
    timestamp: &str,
    user: &str,
    assistant: &str,
) -> CaptureTurnResult {
    engine
        .capture_turn(
            &[
                MemoryMessage::user(user),
                MemoryMessage::assistant(assistant),
            ],
            session,
            Some(timestamp.to_string()),
            "cases_memory_effect",
        )
        .expect("capture turn")
}

fn flush(engine: &MemoryEngine) -> HeartbeatStats {
    engine
        .flush(FlushOptions {
            reason: "cases_memory_effect".to_string(),
            ..Default::default()
        })
        .expect("flush indexes the pending turns")
}

fn retrieve(engine: &MemoryEngine, query: &str) -> RetrievalResult {
    engine
        .retrieve_context(
            query,
            RetrieveOptions {
                recent_messages: Vec::new(),
                workspace_hint: None,
                explicit: false,
            },
        )
        .expect("retrieve_context")
}

fn list_entries(engine: &MemoryEngine, kinds: &[MemoryRecordType]) -> Vec<MemoryManifestEntry> {
    engine
        .list(&MemoryListOptions {
            kinds: kinds.to_vec(),
            include_deprecated: true,
            ..Default::default()
        })
        .expect("list entries")
}

fn read_file(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// 递归收集目录下的 `*.md`，排除派生的 `MEMORY.md` 清单文件。
fn record_files(root: &Path) -> Vec<PathBuf> {
    fn walk(directory: &Path, out: &mut Vec<PathBuf>) {
        let Ok(read_dir) = std::fs::read_dir(directory) else {
            return;
        };
        for item in read_dir.flatten() {
            let path = item.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().and_then(|extension| extension.to_str()) == Some("md") {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    walk(root, &mut files);
    files.retain(|path| {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        name != MANIFEST_FILE && name != "project.meta.md"
    });
    files.sort();
    files
}

/// 一个引擎可见的全部记忆记录文件：workspace memory 目录 + global 目录。
fn engine_record_files(engine: &MemoryEngine) -> Vec<PathBuf> {
    let roots = engine.store_roots().expect("store roots");
    let mut files = record_files(&roots.memory_dir);
    files.extend(record_files(&roots.global_dir));
    files.sort();
    files
}

fn manifest_text(engine: &MemoryEngine) -> String {
    let roots = engine.store_roots().expect("store roots");
    read_file(&roots.memory_dir.join(MANIFEST_FILE))
}

/// C6 用：直接向真实文件存储写一条笔记，返回落盘后的 manifest entry。
fn seed_candidate(
    kind: MemoryRecordType,
    name: &str,
    description: &str,
    body: &str,
    captured_at: &str,
    session_key: &str,
) -> MemoryCandidate {
    MemoryCandidate {
        kind,
        scope: if kind == MemoryRecordType::User {
            MemoryScope::Global
        } else {
            MemoryScope::Project
        },
        name: name.to_string(),
        description: description.to_string(),
        body: body.to_string(),
        captured_at: Some(captured_at.to_string()),
        source_session_key: Some(session_key.to_string()),
        ..Default::default()
    }
}

// ===========================================================================
// C1 跨会话记忆：前两个会话写入的约定 / 项目事实，第三个会话能召回。
// ===========================================================================
//
// 场景：用户先用两个不同会话分别交代“文档语言 + 包管理器”约定和项目事实，
//       后续新会话提问时，系统应把两类记忆都注入上下文。
// 输入：session-a：以后所有文档用中文写，项目统一使用 pnpm。
//       session-b：再记录一下：Boreal 项目目标是整理本地知识库。
//       新会话提问：我们文档应该用什么语言写？项目用哪个包管理器？
// 预期记忆效果：Feedback/ 落一条 feedback 笔记、Project/ 落一条 project 笔记，
//       MEMORY.md 同时包含两个分区；新会话 retrieve_context 返回非 none 路由，
//       system_context（上游四段结构）同时包含“中文”“pnpm”和项目事实。
// 断言点：文件类型/作用域/来源会话、manifest 分区、召回路由、上下文文本。
#[test]
fn c1_cross_session_memory_is_recalled_in_a_later_session() {
    let temp = tempfile::tempdir().expect("tempdir");
    let provider = Arc::new(EffectModel::new());
    let clock = Arc::new(FixedClock::new(CLOCK_BASE));
    let engine = open_engine(
        test_config(temp.path(), "cross-session-ws"),
        &provider,
        &clock,
    );

    // 会话 A：语言 + 包管理器约定（feedback）。
    let captured_a = capture_turn(
        &engine,
        "session-a",
        T0,
        "以后所有文档用中文写，项目统一使用 pnpm。",
        "好的，我记住了这两条约定。",
    );
    assert!(captured_a.captured, "session-a 的约定应进入 L0 队列");
    flush(&engine);

    // 会话 B：不同 session，再记一条项目事实（project）。
    clock.advance_minutes(2);
    let captured_b = capture_turn(
        &engine,
        "session-b",
        T1,
        "再记录一下：Boreal 项目目标是整理本地知识库。",
        "收到，已记录到项目记忆。",
    );
    assert!(captured_b.captured, "不同 session 的新内容应被捕获");
    flush(&engine);

    // 记忆效果 1：两条不同类别的记忆文件按预期落盘。
    let feedback_files = list_entries(&engine, &[MemoryRecordType::Feedback]);
    assert_eq!(feedback_files.len(), 1, "应只有 1 条 feedback 记忆");
    let feedback = &feedback_files[0];
    assert_eq!(feedback.frontmatter.scope, MemoryScope::Project);
    let feedback_raw = read_file(Path::new(&feedback.absolute_path));
    assert!(feedback_raw.contains("type: feedback"), "{feedback_raw}");
    assert!(feedback_raw.contains("scope: project"), "{feedback_raw}");
    assert!(feedback_raw.contains("中文"), "{feedback_raw}");
    assert!(feedback_raw.contains("pnpm"), "{feedback_raw}");

    let project_files = list_entries(&engine, &[MemoryRecordType::Project]);
    assert_eq!(project_files.len(), 1, "应只有 1 条 project 记忆");
    let project_raw = read_file(Path::new(&project_files[0].absolute_path));
    assert!(project_raw.contains("type: project"), "{project_raw}");
    assert!(project_raw.contains("本地知识库整理工具"), "{project_raw}");

    // 派生的 MEMORY.md 同时出现两个分区（可读的记忆目录）。
    let manifest = manifest_text(&engine);
    assert!(manifest.contains("## Feedback Memory"), "{manifest}");
    assert!(manifest.contains("## Project Memory"), "{manifest}");
    assert_eq!(
        engine.overview().expect("overview").pending_dialogue_turns,
        0,
        "两次 flush 后队列应清空"
    );

    // 记忆效果 2：新会话（session-c）的召回命中两个会话写入的事实。
    let result = retrieve(&engine, "我们文档应该用什么语言写？项目用哪个包管理器？");
    assert_ne!(result.route, MemoryRoute::None, "该问题必须触发召回");
    let context = result.context.clone().expect("应渲染原始召回上下文");
    assert!(context.contains("中文"), "{context}");
    assert!(context.contains("pnpm"), "{context}");
    let system_context = result
        .system_context
        .clone()
        .expect("应生成四段结构的系统上下文");
    assert!(
        system_context.starts_with("## ClawXMemory Recall\n\n"),
        "{system_context}"
    );
    assert!(
        system_context.contains("follow the current-turn user instructions."),
        "{system_context}"
    );
    assert!(system_context.contains("中文"), "{system_context}");
    assert!(system_context.contains("pnpm"), "{system_context}");
    assert!(!result.selected_ids.is_empty(), "应选中记忆文件");
}

// ===========================================================================
// C2 用户身份跨项目（global）：user 笔记写全局目录，两个 workspace 都能召回。
// ===========================================================================
//
// 场景：用户在 workspace A 交代自己的稳定身份；Dream 把它合并进全局用户画像
//       （global/UserIdentity/user-profile.md）；随后 workspace B 的新引擎也应
//       能召回同一身份，且用户笔记不能写进任何项目的 memory 目录。
// 输入：workspace A，session：请记住：我是后端工程师，长期负责支付系统。
// 预期记忆效果：笔记先落在 <data_root>/global/UserIdentityNotes/（type=user,
//       scope=global）；Dream 后生成 global/UserIdentity/user-profile.md 并吸收
//       原笔记；两个 workspace 的 retrieve_context（route=user）都包含“后端工程师”。
// 断言点：全局目录落盘/隔离、Dream 画像、两 workspace 召回内容、项目目录无污染。
#[test]
fn c2_global_user_identity_is_shared_across_workspaces() {
    let temp = tempfile::tempdir().expect("tempdir");
    let provider = Arc::new(EffectModel::new());
    provider.set_gate_route(MemoryRoute::User);
    provider.set_dream_user_profile(json!({
        "identity_background_markdown": "- 用户是一名后端工程师，长期负责支付系统。"
    }));
    let clock = Arc::new(FixedClock::new(CLOCK_BASE));
    let config_a = test_config(temp.path(), "global-ws-a");
    let engine_a = open_engine(config_a, &provider, &clock);

    // workspace A 捕获一条 user 身份笔记。
    capture_turn(
        &engine_a,
        "session-identity",
        T0,
        "请记住：我是后端工程师，长期负责支付系统。",
        "好的，我记下了你的长期身份。",
    );
    flush(&engine_a);

    // 记忆效果 1：user 笔记写进 global 目录，而不是项目 memory 目录。
    let roots_a = engine_a.store_roots().expect("store roots");
    let global_notes = record_files(&roots_a.global_dir);
    assert_eq!(global_notes.len(), 1, "应只有一条全局 user 笔记");
    let note_path = global_notes[0].clone();
    assert!(
        note_path.starts_with(roots_a.global_dir.join("UserIdentityNotes")),
        "user 笔记必须落在 UserIdentityNotes：{}",
        note_path.display()
    );
    let note_raw = read_file(&note_path);
    let parsed_note = parse_markdown(&note_raw)
        .expect("笔记应可解析")
        .expect("笔记应有合法 frontmatter");
    assert_eq!(parsed_note.frontmatter.kind, MemoryRecordType::User);
    assert_eq!(parsed_note.frontmatter.scope, MemoryScope::Global);
    assert!(note_raw.contains("后端工程师"), "{note_raw}");
    assert!(
        record_files(&roots_a.memory_dir).is_empty(),
        "全局 user 笔记不得写入当前项目的 memory 目录"
    );

    // Dream：把 user 笔记合并进全局用户画像（真实 staging + commit）。
    let dream = engine_a
        .dream(DreamTrigger::Manual)
        .expect("dream consolidates the global user profile");
    assert_eq!(dream.status, DreamPipelineStatus::Success);
    assert!(dream.profile_updated, "Dream 应重写全局用户画像");
    assert_eq!(dream.deleted_files, 1, "原 user 笔记应被画像吸收");
    assert!(!note_path.exists(), "被吸收的 user 笔记应删除");

    let profile_path = roots_a.global_dir.join("UserIdentity/user-profile.md");
    let profile_raw = read_file(&profile_path);
    assert!(profile_raw.contains("scope: global"), "{profile_raw}");
    assert!(profile_raw.contains("身份背景"), "{profile_raw}");
    assert!(profile_raw.contains("后端工程师"), "{profile_raw}");
    assert_eq!(
        engine_a.overview().expect("overview").user_profiles,
        1,
        "全局用户画像计数应为 1"
    );

    // 记忆效果 2：workspace A / B 的 user 路由召回都能使用同一份全局画像。
    let engine_b = open_engine(test_config(temp.path(), "global-ws-b"), &provider, &clock);
    let roots_b = engine_b.store_roots().expect("store roots");
    assert_ne!(
        roots_a.memory_dir, roots_b.memory_dir,
        "两个 workspace 目录必须不同"
    );
    assert!(
        record_files(&roots_b.memory_dir).is_empty(),
        "workspace B 不应凭空出现项目记忆文件"
    );

    for (label, engine) in [("workspace-a", &engine_a), ("workspace-b", &engine_b)] {
        let summary = engine.get_user_summary().expect("user summary");
        assert!(
            summary
                .identity_background
                .iter()
                .any(|fact| fact.contains("后端工程师")),
            "{label} 的 user summary 应包含身份事实：{:?}",
            summary.identity_background
        );
        let result = retrieve(engine, "你还记得我是谁吗？");
        assert_eq!(result.route, MemoryRoute::User, "{label} 应走 user 召回");
        let context = result.context.clone().expect("user 路由应渲染身份背景");
        assert!(context.contains("后端工程师"), "{label}: {context}");
        assert!(
            result
                .system_context
                .clone()
                .expect("应生成系统上下文")
                .contains("后端工程师"),
            "{label} 的系统上下文应包含身份事实"
        );
    }
}

// ===========================================================================
// C3 项目隔离：workspace A 的记忆不进入 workspace B 的 recall / manifest。
// ===========================================================================
//
// 场景：同一个 data root 下有两个项目（workspace id 不同）。A 写入了项目事实，
//       B 的引擎在召回时不得看到 A 的记忆，B 的 MEMORY.md 也不得包含 A 的内容。
// 输入：workspace A：项目叫 Boreal，目标是整理本地知识库，目前处于设计阶段。
//       workspace B 提问同一话题。
// 预期记忆效果：A 的召回命中事实；B 的召回 context / system_context 为空、
//       selected_ids 为空、list 为空、MEMORY.md 不含 A 的事实。
// 断言点：A 命中、B 全空、两个 workspace 目录互相独立。
#[test]
fn c3_project_memory_is_isolated_between_workspaces() {
    let temp = tempfile::tempdir().expect("tempdir");
    let provider = Arc::new(EffectModel::new());
    let clock = Arc::new(FixedClock::new(CLOCK_BASE));

    let engine_a = open_engine(
        test_config(temp.path(), "isolation-ws-a"),
        &provider,
        &clock,
    );
    capture_turn(
        &engine_a,
        "session-iso-a",
        T0,
        "项目叫 Boreal，目标是整理本地知识库，目前处于设计阶段。",
        "好的，已记录项目背景。",
    );
    flush(&engine_a);

    let roots_a = engine_a.store_roots().expect("store roots");
    let roots_b = open_engine(
        test_config(temp.path(), "isolation-ws-b"),
        &provider,
        &clock,
    )
    .store_roots()
    .expect("store roots");
    assert_ne!(roots_a.memory_dir, roots_b.memory_dir);

    // A：同一话题的召回命中自己的项目记忆。
    let result_a = retrieve(&engine_a, "Boreal 现在处于什么阶段？");
    assert_ne!(result_a.route, MemoryRoute::None);
    let context_a = result_a.context.clone().expect("workspace A 应命中记忆");
    assert!(
        context_a.contains("Boreal 是一个本地知识库整理工具"),
        "{context_a}"
    );

    // B：同 data root 但不同 workspace，召回 / manifest / list 均看不到 A 的记忆。
    let engine_b = open_engine(
        test_config(temp.path(), "isolation-ws-b"),
        &provider,
        &clock,
    );
    let result_b = retrieve(&engine_b, "Boreal 现在处于什么阶段？");
    assert!(
        result_b.context.is_none(),
        "workspace B 不应召回 A 的记忆：{:?}",
        result_b.context
    );
    assert!(result_b.system_context.is_none());
    assert!(result_b.selected_ids.is_empty());
    assert!(
        list_entries(&engine_b, &[]).is_empty(),
        "workspace B 的 manifest 应为空"
    );
    let manifest_b = manifest_text(&engine_b);
    assert!(
        !manifest_b.contains("Boreal") && !manifest_b.contains("本地知识库"),
        "workspace B 的 MEMORY.md 不应包含 A 的记忆：{manifest_b}"
    );
    assert!(
        record_files(&roots_b.memory_dir).is_empty(),
        "workspace B 目录不应出现 A 的记忆文件"
    );
}

// ===========================================================================
// C4 幂等去重：重复捕获只留一条 L0；模型故障降级后不重放。
// ===========================================================================
//
// 同一轮捕获两次，再捕获一条含故障标记的新约定。flush 保留第一条记忆，
// 将第二条分类故障降级为空结果，两条 L0 均 indexed；后续 flush 不再提取，
// 文件数和字节不变。
#[test]
fn c4_capture_and_flush_are_idempotent() {
    let temp = tempfile::tempdir().expect("tempdir");
    let provider = Arc::new(EffectModel::new());
    let clock = Arc::new(FixedClock::new(CLOCK_BASE));
    let engine = open_engine(test_config(temp.path(), "dedupe-ws"), &provider, &clock);

    // 1) 同一 (timestamp, payload) capture 两次：第二次应被去重。
    let first = capture_turn(
        &engine,
        "session-dedupe",
        T0,
        "第一条约定：交付时先给 3 个标题。",
        "好的，我记住了。",
    );
    assert!(first.captured, "首次 capture 应入队");
    let second = capture_turn(
        &engine,
        "session-dedupe",
        T0,
        "第一条约定：交付时先给 3 个标题。",
        "好的，我记住了。",
    );
    assert!(
        second.captured,
        "非空输入按上游 facade 返回 true，队列仍去重"
    );
    let overview = engine.overview().expect("overview");
    assert_eq!(
        overview.pending_dialogue_turns, 1,
        "重复 capture 后队列中只能有 1 条 L0"
    );

    // 2) 同一 session 再记一条新约定（带故障标记）：首次 flush 会写第一条，并对第二条的模型故障降级。
    let third = capture_turn(
        &engine,
        "session-dedupe",
        T1,
        "第二条约定：交付时先给正文，再给标题。FAULT_MARKER",
        "收到，记录第二条约定。",
    );
    assert!(third.captured);
    assert_eq!(
        engine.overview().expect("overview").pending_dialogue_turns,
        2
    );

    provider.set_classify_failure_marker(Some("FAULT_MARKER"));
    let stats = engine
        .flush(FlushOptions::default())
        .expect("上游将分类故障降级为空结果");
    assert_eq!(stats.processed_sessions, 2);
    assert_eq!(stats.written_files, 1);
    assert!(stats.errors.is_empty());
    assert_eq!(engine.overview().unwrap().pending_dialogue_turns, 0);
    let after_flush = engine_record_files(&engine);
    assert_eq!(after_flush.len(), 1);
    provider.set_classify_failure_marker(None);
    assert_eq!(
        flush(&engine).processed_turns,
        0,
        "已索引的模型降级记录不会重放"
    );

    // 4) 再 flush 一次：没有待处理轮次，文件数与字节完全不变。
    let before_noop: Vec<(PathBuf, Vec<u8>)> = after_flush
        .iter()
        .map(|path| (path.clone(), std::fs::read(path).expect("read before noop")))
        .collect();
    let noop_stats = flush(&engine);
    assert_eq!(noop_stats.processed_turns, 0, "空 flush 不应处理任何轮次");
    assert_eq!(noop_stats.written_files, 0, "空 flush 不应写文件");
    let after_noop: Vec<(PathBuf, Vec<u8>)> = engine_record_files(&engine)
        .iter()
        .map(|path| (path.clone(), std::fs::read(path).expect("read after noop")))
        .collect();
    assert_eq!(before_noop, after_noop, "重复 flush 不得改变文件集合与内容");
}

// ===========================================================================
// C5 召回降级：gate / selection 模型故障时，retrieve 安全降级、不注入错误内容。
// ===========================================================================
//
// 场景：模型服务可能返回无效 JSON 或直接报错。召回必须：
//       - gate 故障 -> route=none、空上下文，绝不 panic；
//       - selection 故障 -> 使用最近候选兜底，仍然渲染真实记忆；
//       - 任何情况下都不能把模型错误文本当成记忆内容注入。
// 输入：先写入一条项目记忆；然后分别让 gate 返回无效 JSON、gate 传输报错、
//       selection 返回无效 JSON、selection 传输报错，再检索。
// 预期记忆效果：前两种返回 route none 且无上下文；后两种仍返回 route project +
//       包含真实项目事实的上下文（无错误文本）。
// 断言点：route、context/system_context、selected_ids、错误文本缺席。
#[test]
fn c5_recall_degrades_gracefully_when_the_model_fails() {
    let temp = tempfile::tempdir().expect("tempdir");
    let provider = Arc::new(EffectModel::new());
    let clock = Arc::new(FixedClock::new(CLOCK_BASE));
    let engine = open_engine(test_config(temp.path(), "degrade-ws"), &provider, &clock);

    capture_turn(
        &engine,
        "session-c5",
        T0,
        "项目叫 Boreal，目标是整理本地知识库，目前处于设计阶段。",
        "好的，已记录。",
    );
    flush(&engine);
    assert_eq!(list_entries(&engine, &[MemoryRecordType::Project]).len(), 1);

    // 1) gate 返回无效 JSON：单次解析失败 -> route none。
    provider.set_gate_failure(FailMode::InvalidJson);
    let invalid_gate = retrieve(&engine, "Boreal 现在处于什么阶段？");
    assert_eq!(invalid_gate.route, MemoryRoute::None);
    assert!(invalid_gate.context.is_none());
    assert!(invalid_gate.system_context.is_none());
    assert!(invalid_gate.selected_ids.is_empty());

    // 2) gate 传输报错：同样降级为 none，而不是 panic。
    provider.set_gate_failure(FailMode::Transport);
    let failing_gate = retrieve(&engine, "Boreal 有哪些风险？");
    assert_eq!(failing_gate.route, MemoryRoute::None);
    assert!(failing_gate.context.is_none());
    assert!(failing_gate.system_context.is_none());

    // 3) gate 正常、selection 返回无效 JSON：最近候选兜底仍渲染真实记忆。
    provider.set_gate_failure(FailMode::Off);
    provider.set_select_failure(FailMode::InvalidJson);
    let fallback = retrieve(&engine, "Boreal 设计阶段有哪些约束？");
    assert_eq!(fallback.route, MemoryRoute::Project);
    assert!(
        !fallback.selected_ids.is_empty(),
        "selection 失败时应回退到候选文件"
    );
    let fallback_context = fallback.context.clone().expect("兜底应渲染记忆内容");
    assert!(
        fallback_context.contains("Boreal 是一个本地知识库整理工具"),
        "{fallback_context}"
    );
    assert_failure_text_absent(&fallback_context);

    // 4) selection 传输报错：同样最近候选兜底。
    provider.set_select_failure(FailMode::Transport);
    let transport_fallback = retrieve(&engine, "Boreal 当前的发布时间是什么时候？");
    assert_eq!(transport_fallback.route, MemoryRoute::Project);
    let transport_context = transport_fallback
        .context
        .clone()
        .expect("兜底应渲染记忆内容");
    assert!(
        transport_context.contains("本地知识库整理工具"),
        "{transport_context}"
    );
    assert_failure_text_absent(&transport_context);
    assert!(transport_fallback
        .system_context
        .clone()
        .expect("应生成系统上下文")
        .starts_with("## ClawXMemory Recall\n\n"));
}

/// 召回上下文里不得出现任何模型错误/内部提示文本。
fn assert_failure_text_absent(context: &str) {
    for marker in [
        "not a parseable JSON object",
        "definitely not json",
        "scripted",
        "model request failed",
        "model_output",
        "no scripted response",
    ] {
        assert!(
            !context.contains(marker),
            "错误文本 {marker:?} 不得注入召回上下文：{context}"
        );
    }
}

// ===========================================================================
// C6 Dream 合并 + 回滚：两个可合并 feedback 文件 -> 合并 1 个；回滚字节级恢复。
// ===========================================================================
//
// 场景：项目里有两条内容重复的交付规则 feedback 文件。Dream 应调用模型把它们
//       合并成 1 条新文件、删除旧文件、刷新 MEMORY.md；rollback 应恢复旧文件
//       的**原始字节**并回到 Dream 前的状态。
// 输入：直接向真实 FileMemoryStore 写入两条 feedback 笔记（旧文件字节先快照）；
//       确定性 cluster（自动合成一簇）+ scripted merge 返回合并结果。
// 预期记忆效果：dream 后 Feedback 只剩 1 条、旧文件删除、MEMORY.md 可读地更新；
//       lastDreamStatus=success；rollback 后两条旧文件字节级恢复、合并文件消失、
//       lastDreamStatus 回到 Dream 之前（无记录）。
// 断言点：文件集合/名称/删除、manifest、状态、回滚字节相等。
#[test]
fn c6_dream_merges_feedback_files_and_rollback_restores_bytes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config = test_config(temp.path(), "dream-ws");

    // 预置两条可合并的 feedback 记忆（真实文件存储）。
    let mut seed = {
        let layout = config.store_layout();
        let mut store = FileMemoryStore::new(layout.roots);
        store.ensure_layout().expect("seed layout");
        store
    };
    let rule_a = seed_candidate(
        MemoryRecordType::Feedback,
        "交付顺序约定",
        "交付时先给 3 个标题",
        "## Rule\n每次交付先给 3 个标题。\n",
        "2026-10-17T09:00:00.000Z",
        "session-dream",
    );
    let rule_b = seed_candidate(
        MemoryRecordType::Feedback,
        "标题模板约定",
        "交付标题使用固定模板",
        "## Rule\n交付标题使用固定模板。\n",
        "2026-10-17T09:10:00.000Z",
        "session-dream",
    );
    let entry_a = seed.upsert_candidate(&rule_a).expect("seed rule a");
    let entry_b = seed.upsert_candidate(&rule_b).expect("seed rule b");
    let path_a = PathBuf::from(&entry_a.absolute_path);
    let path_b = PathBuf::from(&entry_b.absolute_path);
    let bytes_a = std::fs::read(&path_a).expect("read original a");
    let bytes_b = std::fs::read(&path_b).expect("read original b");
    drop(seed);

    // cluster 自动合并全部 header；merge 返回脚本化结果；project meta 保持不动。
    let provider = Arc::new(EffectModel::new());
    provider.set_dream_merge(json!({
        "summary": "merged duplicated delivery rules",
        "name": "交付规则（合并）",
        "description": "合并后的交付顺序与标题模板规则",
        "markdown": "## Rule\n交付时先给标题，再给正文。"
    }));
    let clock = Arc::new(FixedClock::new(CLOCK_BASE));
    let engine = open_engine(config, &provider, &clock);
    assert_eq!(
        list_entries(&engine, &[MemoryRecordType::Feedback]).len(),
        2,
        "Dream 前应有两条 feedback 记忆"
    );

    // 记忆效果 1：Dream 合并成功。
    let dream = engine.dream(DreamTrigger::Manual).expect("dream run");
    assert_eq!(dream.status, DreamPipelineStatus::Success);
    assert_eq!(dream.rewritten_projects, 1, "应合并 1 个 cluster");
    assert_eq!(dream.deleted_files, 2, "两条旧文件都应删除");

    let after_dream = list_entries(&engine, &[MemoryRecordType::Feedback]);
    assert_eq!(after_dream.len(), 1, "合并后 feedback 只剩 1 条");
    assert_eq!(after_dream[0].frontmatter.name, "交付规则（合并）");
    assert!(!path_a.exists(), "旧文件 A 应被删除");
    assert!(!path_b.exists(), "旧文件 B 应被删除");
    let merged_path = PathBuf::from(&after_dream[0].absolute_path);
    assert!(merged_path.is_file(), "合并后的文件应存在");
    let merged_raw = read_file(&merged_path);
    assert!(merged_raw.contains("先给标题"), "{merged_raw}");

    let manifest_after_dream = manifest_text(&engine);
    assert!(
        manifest_after_dream.contains("交付规则（合并）"),
        "{manifest_after_dream}"
    );
    assert!(
        !manifest_after_dream.contains("交付顺序约定"),
        "MEMORY.md 不应再包含被合并的旧文件：{manifest_after_dream}"
    );
    let overview_after_dream = engine.overview().expect("overview");
    assert_eq!(
        overview_after_dream.last_dream_status,
        Some(DreamPipelineStatus::Success)
    );
    assert!(overview_after_dream.last_dream_at.is_some());
    assert!(
        engine.last_dream_snapshot().expect("snapshot").is_some(),
        "Dream 后应存在可回滚快照"
    );

    // 记忆效果 2：回滚恢复 Dream 前状态（旧文件字节级一致）。
    let rollback = engine.rollback_last_dream().expect("rollback");
    assert_eq!(
        rollback.last_dream_status, None,
        "回滚后 lastDreamStatus 应恢复到 Dream 之前（无记录）"
    );
    assert!(!rollback.rolled_back_at.is_empty());
    assert_eq!(
        std::fs::read(&path_a).expect("read restored a"),
        bytes_a,
        "回滚后文件 A 应字节级恢复"
    );
    assert_eq!(
        std::fs::read(&path_b).expect("read restored b"),
        bytes_b,
        "回滚后文件 B 应字节级恢复"
    );
    assert!(!merged_path.exists(), "回滚后合并文件应消失");
    assert_eq!(
        list_entries(&engine, &[MemoryRecordType::Feedback]).len(),
        2,
        "回滚后应恢复两条 feedback 记忆"
    );
    assert_eq!(
        engine.overview().expect("overview").last_dream_status,
        None,
        "回滚后 overview 的 lastDreamStatus 应为空"
    );
    let manifest_after_rollback = manifest_text(&engine);
    assert!(
        manifest_after_rollback.contains("交付顺序约定"),
        "{manifest_after_rollback}"
    );
    assert!(
        !manifest_after_rollback.contains("交付规则（合并）"),
        "{manifest_after_rollback}"
    );
}

// ===========================================================================
// C7 中文与特殊字符：中文 slug / frontmatter / MEMORY.md 往返，emoji 与超长安全截断。
// ===========================================================================
//
// 场景：中文项目标题 + emoji + 超长标题/描述/正文，在文件命名、frontmatter、
//       manifest、按行读取召回各环节都必须安全（UTF-8 边界安全、不 panic）。
// 输入：第一条笔记：名称“星尘项目发布清单 🚀”、中文描述与正文；
//       第二条笔记：72 个“长” + 10 个 🚀 的超长名称（80 UTF-16 单元上限）、
//       240 字符描述、12 行正文。
// 预期记忆效果：文件名 slug 保留中文、hash 后缀 10 位；frontmatter 可解析且
//       往返出中文/emoji；MEMORY.md markdown 链接可读；超长名称按 UTF-16 单元
//       安全截断（不劈开 emoji）、描述截到 180、按行读取召回正文安全截断。
// 断言点：相对路径、frontmatter 往返、manifest 行、截断长度与预期值。
#[test]
fn c7_chinese_and_special_characters_round_trip_safely() {
    let temp = tempfile::tempdir().expect("tempdir");
    let provider = Arc::new(EffectModel::new());
    let clock = Arc::new(FixedClock::new(CLOCK_BASE));
    let engine = open_engine(test_config(temp.path(), "cjk-ws"), &provider, &clock);

    // 第一条：正常长度的中文 + emoji 笔记。
    let normal_name = "星尘项目发布清单 🚀";
    provider.set_note_override(json!({
        "name": normal_name,
        "description": "中文描述：星尘项目发布流程与注意事项",
        "markdown": "## Summary\n中文正文：星尘项目 🚀 发布分三步走。\n",
    }));
    capture_turn(
        &engine,
        "session-cjk",
        T0,
        "项目叫星尘，发布清单分三步走。",
        "好的，已记录发布清单。",
    );
    flush(&engine);

    // 第二条：超长中文名称 + 超长描述 + 多行正文（同一 session 的新一轮）。
    let long_name_seed = format!("{}{}", "长".repeat(72), "🚀".repeat(10));
    let long_description_seed = "描述".repeat(120);
    let mut long_markdown = String::from("## Summary\n");
    for index in 1..=12 {
        long_markdown.push_str(&format!("第{index}行：超长正文内容……\n"));
    }
    provider.set_note_override(json!({
        "name": long_name_seed,
        "description": long_description_seed,
        "markdown": long_markdown,
    }));
    capture_turn(
        &engine,
        "session-cjk",
        T2,
        "再补充一下发布清单的细节。",
        "好的，已补充。",
    );
    flush(&engine);

    let project_entries = list_entries(&engine, &[MemoryRecordType::Project]);
    assert_eq!(project_entries.len(), 2, "应写入两条 project 记忆");

    // 记忆效果 1：中文标题/描述/正文往返，文件名 slug 保留中文 + 10 位 hash 后缀。
    let normal = project_entries
        .iter()
        .find(|entry| entry.frontmatter.name == normal_name)
        .expect("应存在正常的 emoji 中文笔记");
    assert!(
        normal
            .relative_path
            .starts_with("Project/星尘项目发布清单-"),
        "中文 slug 应保留在文件名中：{}",
        normal.relative_path
    );
    let normal_file = normal.relative_path.rsplit('/').next().expect("file name");
    let hash_suffix = normal_file
        .strip_prefix("星尘项目发布清单-")
        .and_then(|rest| rest.strip_suffix(".md"))
        .expect("文件名应形如 <slug>-<hash10>.md");
    assert_eq!(hash_suffix.len(), 10, "hash 后缀应为 10 位：{hash_suffix}");
    assert!(
        hash_suffix
            .chars()
            .all(|character| character.is_ascii_hexdigit()),
        "hash 后缀应为十六进制：{hash_suffix}"
    );

    let normal_raw = read_file(Path::new(&normal.absolute_path));
    assert!(normal_raw.contains("type: project"), "{normal_raw}");
    assert!(normal_raw.contains("scope: project"), "{normal_raw}");
    assert!(normal_raw.contains("中文正文"), "{normal_raw}");
    assert!(
        normal_raw.contains("🚀"),
        "emoji 应保留在正文：{normal_raw}"
    );
    let parsed_normal = parse_markdown(&normal_raw)
        .expect("中文 frontmatter 应可解析")
        .expect("应存在合法 frontmatter");
    assert_eq!(parsed_normal.frontmatter.name, normal_name);
    assert_eq!(
        parsed_normal.frontmatter.description,
        "中文描述：星尘项目发布流程与注意事项"
    );
    assert!(
        parsed_normal.body.contains("中文正文"),
        "{:?}",
        parsed_normal.body
    );

    // MEMORY.md 的 markdown 链接可读（中文名 + 相对路径）。
    let manifest = manifest_text(&engine);
    assert!(
        manifest.contains(&format!("[{}]({})", normal_name, normal.relative_path)),
        "MEMORY.md 应包含可读链接：{manifest}"
    );
    assert!(manifest.contains("中文描述"), "{manifest}");

    // 记忆效果 2：超长内容安全截断（UTF-16 单元边界，绝不劈开 emoji）。
    let expected_long_name = format!("{}{}", "长".repeat(72), "🚀".repeat(4));
    let expected_long_description = "描述".repeat(90);
    let long = project_entries
        .iter()
        .find(|entry| entry.frontmatter.name == expected_long_name)
        .unwrap_or_else(|| {
            panic!(
                "超长名称应被安全截断为 72 个“长” + 4 个 🚀，实际条目：{:?}",
                project_entries
                    .iter()
                    .map(|entry| entry.frontmatter.name.clone())
                    .collect::<Vec<_>>()
            )
        });
    assert_eq!(
        long.frontmatter.name, expected_long_name,
        "超长名称不应劈开 emoji 或被截成非法字符串"
    );
    assert_eq!(long.frontmatter.name.chars().count(), 76);
    assert!(long.frontmatter.name.ends_with("🚀"));
    assert_eq!(
        long.frontmatter.description, expected_long_description,
        "描述应截断到 180 个 UTF-16 单元"
    );
    assert_eq!(long.frontmatter.description.chars().count(), 180);

    let long_raw = read_file(Path::new(&long.absolute_path));
    assert!(long_raw.contains("第12行"), "正文应完整落盘：{long_raw}");
    let long_id = long.relative_path.clone();

    // 按行读取召回时安全截断：3 行 + "..."
    let truncated = engine
        .get(&[long_id], 3)
        .expect("get long record with max_lines=3");
    assert_eq!(truncated.len(), 1);
    assert!(
        truncated[0].content.ends_with("\n..."),
        "按行截断应以 ... 结尾：{:?}",
        truncated[0].content
    );
    assert_eq!(
        truncated[0].content.lines().count(),
        4,
        "3 行内容 + 省略号行"
    );
}

// ===========================================================================
// C8（跳过）：MCP / 工具面语义。
// ===========================================================================
//
// `memory_search` / `memory_overview` / `memory_add` 的语义由
// `crates/pd-memory-mcp/tests/engine_roundtrip.rs` 覆盖。这里不能写：
// core 的集成测试若依赖 pd-memory-mcp（其本身依赖 pd-memory-core）会形成
// cargo 依赖环。原因与分工已写入 `docs/memory-test-cases.zh.md`。
