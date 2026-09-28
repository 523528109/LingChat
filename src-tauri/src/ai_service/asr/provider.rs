//! 云 ASR provider 抽象 + qwen-asr 实现（阿里云 DashScope）。
//!
//! 设计目标：v1 只做"调用云 API"的最薄一层；端点检测、会话编排、配置持久化
//! 由同目录其它子模块负责（vad / session / settings，后续 Task）。
//!
//! 复用策略：
//! - HTTP 客户端由调用方传入（`&reqwest::Client`），调用方负责 TLS / 超时（30s）。
//! - 错误统一返回 [`AsrError`]，不外泄 `reqwest::Error` / `serde_json::Error`。
//! - 不引入新依赖（reqwest / serde / serde_json / async-trait / tracing / thiserror
//!   / base64 都已在 Cargo.toml）。
//!
//! ============================================================================
//! 扩展指南（新增 provider / 接入 OpenAI 兼容服务）
//! ============================================================================
//!
//! 新增一个**专用协议** provider（3 步，参照 [`QwenAsrProvider`]）：
//! 1. 实现 [`AsrProvider`] trait（recognize 必选；流式见下）
//! 2. 写 `config_fields()`——设置页据此动态渲染输入框，前端零改动
//! 3. 注册到 `list_provider_info()` 与 `get_provider()`
//!
//! 接入**OpenAI 兼容服务**（whisper.cpp / faster-whisper-server / Groq /
//! 阿里云 DashScope compatible-mode 的 qwen-audio-asr 等）：
//! - 协议是 `POST {endpoint}/v1/audio/transcriptions`（multipart：file/model/
//!   可选 language/prompt）+ `GET /v1/models` 动态模型列表
//! - 现成的可复用件：
//!   - [`provider_stream_llama::recognize_stream`]——通用 SSE 结果流式客户端
//!     （对 `<asr_text>` 标记自动检测，纯 OpenAI 文本也能解析；llama-asr 已在用）
//!   - [`parse_llama_text`] / [`parse_llama_models`]——响应与模型列表解析
//! - 泛化方案备忘（未实施，2026-08 评审预留）：
//!   - 新增固定 id `openai-compatible` 的通用 struct（id 保持 `&'static str`，
//!     trait 无需改；参照 LLM 侧 `lmstudio` 固定 id + 可配 endpoint 的先例）
//!   - llama-asr 保留（默认端点/默认模型/热词有友好默认值，老配置不断），
//!     与通用 struct 共享上述复用件
//!   - 前端 `isLlamaStream()`（useAsrInput.ts）需改为覆盖 SSE 类 provider
//!     的集合判定——llama-asr 与 openai-compatible 都是"整段上传 + SSE
//!     partial"，必须同链路，新增第三个 SSE 类 provider 时同步扩展该判定

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STD;
use serde::Serialize;
use serde_json::{Value as JsonValue, json};
use tracing::{debug, instrument, warn};

use super::error::AsrError;
use super::region::DashScopeRegion;

// ============================================================================
// 公共结果类型
// ============================================================================

/// provider 识别返回结果。
#[derive(Debug, Clone, Serialize)]
pub struct AsrResult {
    /// 识别出的文本。
    pub text: String,
    /// provider 报告的语言代码（可选）。
    pub language: Option<String>,
    /// provider 报告的置信度 0~1（可选）。
    pub confidence: Option<f32>,
    /// provider id（与 `list_provider_info` 一致）。
    pub provider_id: String,
}

// ============================================================================
// Provider 配置元数据
// ============================================================================

/// provider 配置字段类型，供前端 SettingsAsr.vue 渲染输入框。
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConfigFieldKind {
    /// 普通文本。
    Text,
    /// 密码框（API key 等敏感字段）。
    Password,
    /// 整数。
    Number,
    /// 布尔开关。
    Boolean,
    /// 下拉单选（配合 [`AsrConfigField::options`]，如地域选择）。
    Select,
    /// 密码框，但**值按地域分开存**（`provider_configs[id].api_keys[地域id]`）。
    ///
    /// UI 只显示当前地域的那一个框（切地域时内容随之切换），存储却是每个
    /// 地域一份——因为各地域的 Key 互相独立、不能混用（见 [`super::region`]）。
    /// 前端需要的当前地域取自 `ProviderInfo.regions` + 配置里的 `region`，
    /// 因此本字段不需要自己的 `options`。
    PasswordMap,
}

/// [`ConfigFieldKind::Select`] 的一个选项。
#[derive(Debug, Clone, Serialize)]
pub struct AsrConfigFieldOption {
    /// 写入配置的值。
    pub value: &'static str,
    /// UI 显示名。
    pub label: &'static str,
}

/// provider 在 UI 上展示需要填写的字段。
///
/// **注意**：字段 key 会被前端写成 `provider_configs[id]` 的**顶层键**
/// （见 SettingsAsr.vue 的 `providerCfgRecord[field.key]`），而 `ProviderConfig`
/// 是没有 `deny_unknown_fields` 的固定 struct —— 新增字段必须同步加进 struct，
/// 否则用户设置后会被 serde 静默丢弃、刷新即失。
#[derive(Debug, Clone, Serialize)]
pub struct AsrConfigField {
    /// 字段 key（写入 `provider_configs[id].<key>`）。
    pub key: &'static str,
    /// 字段显示名（前端可自行 i18n）。
    pub label: &'static str,
    /// 字段类型。
    pub kind: ConfigFieldKind,
    /// 是否必填。
    pub required: bool,
    /// 默认值（字符串形式）。
    pub default_value: Option<&'static str>,
    /// 占位提示文字。
    pub placeholder: Option<&'static str>,
    /// 提示说明（显示在输入框下方）。
    pub hint: Option<&'static str>,
    /// [`ConfigFieldKind::Select`] 的选项列表；其它类型为空。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<AsrConfigFieldOption>,
}

/// provider 静态元数据（id / 显示名 / 配置字段）。
#[derive(Debug, Clone, Serialize)]
pub struct ProviderInfo {
    /// 唯一 id，写入配置 `active_provider` 用。
    pub id: &'static str,
    /// UI 显示名（如 "OpenAI Whisper"）。
    pub display_name: &'static str,
    /// 简短描述。
    pub description: &'static str,
    /// 是否支持流式协议（前端据此决定流式开关是否可用）。
    pub supports_streaming: bool,
    /// UI 需要展示的配置字段。
    pub config_fields: Vec<AsrConfigField>,
    /// 该 provider 可选的地域列表（含各地域的端点默认值）。
    ///
    /// 空数组 = 该 provider 无地域概念（如本地 llama-asr），前端不渲染地域下拉。
    /// 非空时，前端切地域会按这里的 `http_endpoint` / `ws_endpoint` 自动填端点
    /// —— 端点默认值的单一真相在后端（与 `ModelInfo` 的预设机制一致）。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub regions: Vec<super::region::AsrRegionInfo>,
}

// ============================================================================
// 热词与调用级参数
// ============================================================================

/// 一条热词。
///
/// `weight` 只对 DashScope 的**即时热词**（`parameters.vocabulary`）有意义；
/// 预编译热词（`vocabulary_id`）与本地 llama-asr 的 `prompt` 偏置都忽略它。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hotword {
    pub text: String,
    pub weight: u8,
}

impl Hotword {
    /// 默认权重。DashScope 文档推荐从 4 起测。
    pub const DEFAULT_WEIGHT: u8 = 4;
    /// 权重上界：50 是文档里的「超级热词」特殊值（召回率大幅提升，最多 50 个）。
    pub const MAX_WEIGHT: u8 = 50;

    /// 用默认权重构造。
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            weight: Self::DEFAULT_WEIGHT,
        }
    }

    /// 指定权重构造（clamp 到 `1..=MAX_WEIGHT`）。
    pub fn with_weight(text: impl Into<String>, weight: u8) -> Self {
        Self {
            text: text.into(),
            weight: weight.clamp(1, Self::MAX_WEIGHT),
        }
    }

    /// 纯词表 → 热词列表（统一用默认权重的降级入口）。
    pub fn from_text_list<I, S>(texts: I) -> Vec<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        texts
            .into_iter()
            .map(|t| Self::new(t))
            .filter(|h| !h.text.trim().is_empty())
            .collect()
    }
}

/// 一次识别调用的可选参数。
///
/// 用结构体而非继续加函数参数：热词之后还会有采样率、角色 id（日志归因）等，
/// 参数列表会持续膨胀，而调用点数量有限，一次改到位成本更低。
#[derive(Debug, Clone, Default)]
pub struct AsrOptions {
    /// 可选 BCP-47 语言码，如 `"zh"` / `"en"` / `"ja"`。
    pub language_hint: Option<String>,
    /// **按调用传入**的热词（未来 = 当前角色的热词，随角色切换而变）。
    ///
    /// 非空时**覆盖** provider 配置级热词（`ProviderCredentials::hotwords`）；
    /// 空时回退到配置级。合并规则集中在
    /// [`ProviderCredentials::effective_hotwords`]，各 provider 不再自行判断
    /// —— 后续接入角色级热词时无需改动任何 provider。
    pub hotwords: Vec<Hotword>,
}

// ============================================================================
// Provider 凭证（最小子集，不依赖 settings.rs）
// ============================================================================

/// provider 运行时凭证：api_key + 端点 + model + 地域 + 热词。
#[derive(Debug, Clone, Default)]
pub struct ProviderCredentials {
    pub api_key: String,
    /// 同步（非实时）识别端点；空/非 http(s) = 按 [`Self::region_enum`] 派生。
    pub endpoint: String,
    /// 实时识别 WebSocket 端点；空/非 ws(s) = 按 [`Self::region_enum`] 派生。
    ///
    /// 与 [`Self::endpoint`] **分开**存储：两者协议不同，且用户可能只覆盖其一
    /// （业务空间专属域名 / 自建代理的 HTTP 与 WS 未必同源）。历史上共用一个
    /// 字段，选中流式模型时会被 `ModelInfo` 预设改写成 `wss://`，导致同步路径
    /// 拿到一个 WebSocket 地址——拆开是对这个既有问题的根治。
    pub ws_endpoint: String,
    /// 识别的模型名；空串 = provider 默认模型。
    pub model: String,
    /// 地域 id（见 [`super::region::DashScopeRegion`]）；空/未知 = 默认地域。
    pub region: String,
    /// 预编译热词列表 ID。仅 `fun-asr-realtime` / `paraformer-realtime` 系支持
    /// （`qwen-audio-3.x` 系走即时热词，见 [`Self::hotwords`]）。
    pub vocabulary_id: String,
    /// provider 配置级热词（兜底）。被 [`AsrOptions::hotwords`] 覆盖。
    pub hotwords: Vec<Hotword>,
}

impl ProviderCredentials {
    /// 从 endpoint 字符串中剔除末尾 `/`，便于直接拼 `/audio/transcriptions`。
    pub fn normalized_endpoint(&self) -> String {
        self.endpoint.trim_end_matches('/').to_string()
    }

    /// api_key 是否非空（剪掉首尾空白后判断）。
    pub fn has_api_key(&self) -> bool {
        !self.api_key.trim().is_empty()
    }

    /// 解析后的地域（空/未知 → 默认地域）。
    pub fn region_enum(&self) -> super::region::DashScopeRegion {
        super::region::DashScopeRegion::parse(&self.region)
    }

    /// 实际使用的同步端点：配置为空或非 http(s) 时按地域派生默认。
    ///
    /// 「非 http(s) 一律回退」同时是历史数据的防御：老配置里 `endpoint` 可能被
    /// 模型预设写成 `wss://...`（见 [`Self::ws_endpoint`] 的说明），直接拿去发
    /// HTTP 请求会让 reqwest 报 builder error。
    pub fn effective_http_endpoint(&self) -> String {
        let e = self.normalized_endpoint();
        if e.starts_with("http://") || e.starts_with("https://") {
            e
        } else {
            self.region_enum().http_endpoint()
        }
    }

    /// 实际使用的实时端点：配置为空或非 ws(s) 时按地域派生默认。
    ///
    /// 注意裁剪的是 [`Self::ws_endpoint`] 而不是 [`Self::endpoint`] —— 两者是
    /// 独立的配置项，取错字段会让用户手填的 WS 地址被静默忽略。
    pub fn effective_ws_endpoint(&self) -> String {
        let e = self.ws_endpoint.trim_end_matches('/');
        if e.starts_with("wss://") || e.starts_with("ws://") {
            e.to_string()
        } else {
            self.region_enum().ws_endpoint()
        }
    }

    /// 本次调用生效的热词：调用级非空则覆盖配置级，否则回退配置级。
    pub fn effective_hotwords(&self, opts: &AsrOptions) -> Vec<Hotword> {
        if opts.hotwords.is_empty() {
            self.hotwords.clone()
        } else {
            opts.hotwords.clone()
        }
    }
}

// ============================================================================
// AsrProvider trait
// ============================================================================

/// 所有云 ASR provider 必须实现的接口。
#[async_trait]
pub trait AsrProvider: Send + Sync {
    /// provider id（与 `ProviderInfo.id` 一致）。
    fn id(&self) -> &'static str;

    /// UI 显示名。
    fn display_name(&self) -> &'static str;

    /// provider 在 SettingsAsr.vue 中渲染所需的配置字段。
    fn config_fields(&self) -> Vec<AsrConfigField>;

    /// 调用云 API 识别一段 WAV 字节。
    ///
    /// - `wav_bytes`：前端 OfflineAudioContext 重采样后的 16kHz mono WAV。
    /// - `opts`：本次调用的可选参数（语言提示 + 热词）。热词走这里而非
    ///   `ProviderCredentials`，是因为它的来源是**当前角色**，随角色切换而变。
    ///
    /// 错误统一返回 [`AsrError`]。
    async fn recognize(&self, wav_bytes: Vec<u8>, opts: &AsrOptions)
    -> Result<AsrResult, AsrError>;

    /// 是否支持流式协议（WebSocket 实时识别）。默认不支持。
    fn supports_streaming(&self) -> bool {
        false
    }

    /// 结果流式识别（SSE 类协议：音频整段上传、结果增量返回）。默认不支持。
    ///
    /// 与 WS 会话流式（`asr_start_streaming`/`stop_streaming`）独立：llama-asr
    /// 走这里（provider_stream_llama.rs），qwen 走 WebSocket 会话路径。
    /// 默认实现返回 [`AsrError::StreamingNotSupported`]。
    ///
    /// `on_partial`：增量文本回调（整段累积视图，每次整体替换）——由调用方
    /// （session / 命令层）注入，provider 不直接依赖 Tauri 事件发射（展示
    /// 与识别解耦，provider 可脱离 Tauri 环境测试）。
    async fn stream_recognize(
        &self,
        _wav_bytes: Vec<u8>,
        _opts: &AsrOptions,
        _on_partial: Option<Arc<dyn for<'a> Fn(&'a str) + Send + Sync + 'static>>,
    ) -> Result<AsrResult, AsrError> {
        Err(AsrError::StreamingNotSupported(self.id().into()))
    }
}

// ============================================================================
// Qwen ASR (DashScope)
// ============================================================================

/// Qwen ASR（阿里云 DashScope）。
///
/// 非流式走 `multimodal-generation` 端点（JSON body + base64 音频）；
/// 流式（paraformer-realtime-v2）走 WebSocket 实时端点（provider_stream.rs）。
pub struct QwenAsrProvider {
    http: reqwest::Client,
    cred: ProviderCredentials,
}

impl QwenAsrProvider {
    pub const ID: &'static str = "qwen-asr";
    pub const DISPLAY: &'static str = "Qwen ASR（阿里云百炼）";

    pub fn new(http: reqwest::Client, cred: ProviderCredentials) -> Result<Self, AsrError> {
        if !cred.has_api_key() {
            // 带上地域名：Key 是分地域存的，只说「需要 api_key」无法指出该去哪个
            // 地域的输入框填（切到没配过的地域时最容易撞上）
            return Err(AsrError::MissingCredentials(format!(
                "Qwen ASR 需要 DashScope api_key（当前地域：{}）",
                cred.region_enum().label()
            )));
        }
        Ok(Self { http, cred })
    }

    /// 同步路径生效的模型。
    ///
    /// 配置为空、或配了一个流式模型（同步端点不认识它们，DashScope 会返回
    /// HTTP 400 "url error"）→ 回退到该地域的同步默认模型。
    fn effective_batch_model(&self) -> &str {
        let m = self.cred.model.trim();
        if m.is_empty() || qwen_is_streaming_model(m) {
            qwen_default_model(false, self.cred.region_enum())
        } else {
            m
        }
    }
}

/// 热词列表 → DashScope 即时热词对象 `{"词": 权重}`。
///
/// 同词重复时保留**先出现**的权重（调用方已按优先级排序，见
/// [`AsrOptions::hotwords`] 的覆盖语义）。
fn hotwords_to_vocabulary_json(hotwords: &[Hotword]) -> JsonValue {
    let mut map = serde_json::Map::new();
    for h in hotwords {
        let text = h.text.trim();
        if text.is_empty() {
            continue;
        }
        map.entry(text.to_string())
            .or_insert_with(|| json!(h.weight));
    }
    JsonValue::Object(map)
}

#[async_trait]
impl AsrProvider for QwenAsrProvider {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn display_name(&self) -> &'static str {
        Self::DISPLAY
    }

    fn config_fields(&self) -> Vec<AsrConfigField> {
        qwen_asr_config_fields()
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    #[instrument(skip(self, wav_bytes, opts), fields(provider = Self::ID))]
    async fn recognize(
        &self,
        wav_bytes: Vec<u8>,
        opts: &AsrOptions,
    ) -> Result<AsrResult, AsrError> {
        let endpoint = self.cred.effective_http_endpoint();
        let model = self.effective_batch_model();
        // 同步端点未定义 language_hints 参数（文档的 parameters 只有
        // format / sample_rate / vocabulary），保持不发送。
        let b64 = BASE64_STD.encode(&wav_bytes);
        let data_url = format!("data:audio/wav;base64,{b64}");
        // 音频内容格式按模型分流：qwen-audio-3.x 系要求
        // `{"type":"input_audio","input_audio":{"data":...}}`；历史 Fun-ASR-Realtime
        // 走旧格式 `{"audio": ...}`。一刀切替换会让老配置从「能用」变成「未知错误」。
        let legacy = qwen_uses_legacy_audio_content(model);
        let audio_content = if legacy {
            json!({ "audio": data_url })
        } else {
            json!({ "type": "input_audio", "input_audio": { "data": data_url } })
        };
        let mut body = json!({
            "model": model,
            "input": {
                "messages": [{
                    "role": "user",
                    "content": [audio_content]
                }]
            },
            "parameters": {
                "format": "wav",
                "sample_rate": 16000
            }
        });
        if legacy {
            // 旧协议实测带这个字段；新协议按官方示例不带
            body["resources"] = json!([]);
        }

        // 即时热词：文档明确仅 qwen-audio-3.x 系支持，对 fun-asr-realtime /
        // paraformer 系发这个参数很可能直接 400 —— 必须门控而非「有热词就发」。
        let hotwords = self.cred.effective_hotwords(opts);
        if !hotwords.is_empty() {
            if qwen_supports_inline_vocabulary(model) {
                body["parameters"]["vocabulary"] = hotwords_to_vocabulary_json(&hotwords);
            } else {
                warn!(
                    "[ASR] 模型 {model} 不支持即时热词，已忽略 {} 条热词（如需热词请改用 qwen-audio-3.x 系）",
                    hotwords.len()
                );
            }
        }

        let resp = self
            .http
            .post(&endpoint)
            .bearer_auth(&self.cred.api_key)
            .header("X-DashScope-SSE", "disable")
            .json(&body)
            .send()
            .await
            .map_err(map_reqwest_error)?;

        let status = resp.status();
        if status == reqwest::StatusCode::REQUEST_TIMEOUT
            || status == reqwest::StatusCode::GATEWAY_TIMEOUT
        {
            return Err(AsrError::ProviderTimeout(Self::ID.into()));
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(AsrError::ProviderApiError {
                provider: Self::ID.into(),
                message: format!("HTTP {status}: {body}"),
            });
        }

        let body_text = resp.text().await.map_err(map_reqwest_error)?;
        let text = parse_qwen_text(&body_text).ok_or_else(|| AsrError::ProviderApiError {
            provider: Self::ID.into(),
            message: format!("无法从响应中提取文本: {body_text}"),
        })?;

        Ok(AsrResult {
            text,
            language: opts.language_hint.clone(),
            confidence: None,
            provider_id: Self::ID.into(),
        })
    }
}

/// 解析 DashScope multimodal-generation 响应文本。
///
/// Fun-ASR-Realtime 非流式实际响应结构（实测）：
/// `{"output": {"output": {"text": "识别文本", "sentence": {...}}, "usage": {...}}}`
/// 宽松解析：优先 `output.output.text` / `output.output.sentence.text`，
/// 兜底 OpenAI 风格 `output.choices[0].message.content` 及 `text` 字段。
fn parse_qwen_text(body: &str) -> Option<String> {
    let value: JsonValue = serde_json::from_str(body).ok()?;
    // Fun-ASR-Realtime：output.output.text（sentence 内也有一份）
    if let Some(s) = value
        .get("output")
        .and_then(|v| v.get("output"))
        .and_then(|v| v.get("text"))
        .and_then(|v| v.as_str())
    {
        return Some(s.to_string());
    }
    if let Some(s) = value
        .get("output")
        .and_then(|v| v.get("output"))
        .and_then(|v| v.get("sentence"))
        .and_then(|v| v.get("text"))
        .and_then(|v| v.as_str())
    {
        return Some(s.to_string());
    }
    // OpenAI 风格：output.choices[0].message.content（content 可能是数组）
    if let Some(content) = value
        .get("output")
        .and_then(|v| v.get("choices"))
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|v| v.get("message"))
        .and_then(|v| v.get("content"))
    {
        if let Some(s) = content.as_str() {
            return Some(s.to_string());
        }
        if let Some(arr) = content.as_array() {
            let joined: String = arr
                .iter()
                .filter_map(|part| part.get("text").and_then(|t| t.as_str()))
                .collect();
            if !joined.is_empty() {
                return Some(joined);
            }
        }
    }
    if let Some(s) = value.get("text").and_then(|v| v.as_str()) {
        return Some(s.to_string());
    }
    if let Some(s) = value
        .get("output")
        .and_then(|v| v.get("text"))
        .and_then(|v| v.as_str())
    {
        return Some(s.to_string());
    }
    if let Some(s) = value
        .get("result")
        .and_then(|v| v.get("text"))
        .and_then(|v| v.as_str())
    {
        return Some(s.to_string());
    }
    None
}

// ============================================================================
// Llama ASR (llama.cpp llama-server，本地 Qwen3-ASR)
// ============================================================================

/// 本地 llama-server（llama.cpp）Qwen3-ASR。
///
/// 协议（D:\asr-deploy\API接入文档.md 实测）：
/// - 端点 `POST {endpoint}/v1/audio/transcriptions`（OpenAI 兼容 multipart）
/// - 音频必须是 16kHz 单声道 WAV（前端 OfflineAudioContext 已产出同格式）
/// - `model` 必须是 `/v1/models` 返回的全名（简写会 400 model not found）
/// - 响应 `text` 格式 `language <lang><asr_text><文本>`，切 `<asr_text>` 取文本
/// - 热词：multipart `prompt` 字段做上下文偏置（偏置非强制；热词接口保留，
///   设置页暂不做输入 UI，来源 `ProviderConfig.extra["hotwords"]` 逗号分隔）
/// - 流式：llama-server 走 SSE（HTTP，OpenAI 兼容语义——每条 data 是当前
///   累积的完整转录）——结果流式经 `stream_recognize` 接入（provider_stream_llama.rs），
///   partial 经 `asr://stream_partial` 事件实时 emit；音频仍整段上传
///   （Qwen3-ASR 非因果 encoder，无流式音频输入）
pub struct LlamaAsrProvider {
    http: reqwest::Client,
    cred: ProviderCredentials,
}

impl LlamaAsrProvider {
    pub const ID: &'static str = "llama-asr";
    pub const DISPLAY: &'static str = "本地 ASR（llama-server）";
    pub const DEFAULT_ENDPOINT: &'static str = "http://127.0.0.1:8080";
    pub const DEFAULT_MODEL: &'static str = "models/Qwen3-ASR-1.7B-Q8_0.gguf";

    pub fn new(http: reqwest::Client, cred: ProviderCredentials) -> Self {
        Self { http, cred }
    }

    /// 模型选择：配置非空用配置，否则默认 1.7B 全名。
    fn effective_model(&self) -> String {
        if self.cred.model.trim().is_empty() {
            Self::DEFAULT_MODEL.to_string()
        } else {
            self.cred.model.trim().to_string()
        }
    }

    /// 端点选择：配置非空且为 http(s) URL 时用配置，否则默认 `127.0.0.1:8080`。
    ///
    /// 与 qwen 同款校验——空 endpoint 会拼出相对 URL，reqwest 报 builder error
    ///（设置页未填 endpoint 时配置为空串，必须回退默认）。
    fn effective_endpoint(&self) -> String {
        let e = self.cred.normalized_endpoint();
        if e.is_empty() || !(e.starts_with("http://") || e.starts_with("https://")) {
            Self::DEFAULT_ENDPOINT.to_string()
        } else {
            e
        }
    }
}

#[async_trait]
impl AsrProvider for LlamaAsrProvider {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn display_name(&self) -> &'static str {
        Self::DISPLAY
    }

    fn config_fields(&self) -> Vec<AsrConfigField> {
        llama_asr_config_fields()
    }

    fn supports_streaming(&self) -> bool {
        // 结果流式（SSE）已接入（stream_recognize / provider_stream_llama.rs）；
        // 与 llama_models() 的模型级 supports_streaming=true 保持一致
        true
    }

    #[instrument(skip(self, wav_bytes, opts), fields(provider = Self::ID))]
    async fn recognize(
        &self,
        wav_bytes: Vec<u8>,
        opts: &AsrOptions,
    ) -> Result<AsrResult, AsrError> {
        // llama-server 转写不支持语言提示（模型自动判语言），忽略。
        let endpoint = format!("{}/v1/audio/transcriptions", self.effective_endpoint());

        let mut form = reqwest::multipart::Form::new()
            .text("model", self.effective_model())
            .text("response_format", "json")
            .part(
                "file",
                reqwest::multipart::Part::bytes(wav_bytes)
                    .file_name("audio.wav")
                    .mime_str("audio/wav")
                    .map_err(|e| AsrError::ProviderApiError {
                        provider: Self::ID.into(),
                        message: format!("构造 multipart 失败: {e}"),
                    })?,
            );
        // 热词接口：作为 prompt 上下文偏置传入
        //（偏置非强制——提升特定词命中概率，不保证一定识别为热词）
        let hotwords = self.cred.effective_hotwords(opts);
        if let Some(prompt) = llama_prompt_from_hotwords(&hotwords) {
            debug!(
                "[ASR] llama-asr prompt 偏置（{} 字符）: {prompt}",
                prompt.chars().count()
            );
            form = form.text("prompt", prompt);
        }

        let mut req = self.http.post(&endpoint).multipart(form);
        // api_key 可选：本地服务无鉴权时不发；带 --api-key 部署时用 Bearer
        if self.cred.has_api_key() {
            req = req.bearer_auth(&self.cred.api_key);
        }
        let resp = req.send().await.map_err(map_reqwest_error)?;

        let status = resp.status();
        if status == reqwest::StatusCode::REQUEST_TIMEOUT
            || status == reqwest::StatusCode::GATEWAY_TIMEOUT
        {
            return Err(AsrError::ProviderTimeout(Self::ID.into()));
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(AsrError::ProviderApiError {
                provider: Self::ID.into(),
                message: format!("HTTP {status}: {body}"),
            });
        }

        let body_text = resp.text().await.map_err(map_reqwest_error)?;
        let (text, language) =
            parse_llama_text(&body_text).ok_or_else(|| AsrError::ProviderApiError {
                provider: Self::ID.into(),
                message: format!("无法从响应中提取文本: {body_text}"),
            })?;

        Ok(AsrResult {
            text,
            language,
            confidence: None,
            provider_id: Self::ID.into(),
        })
    }
    /// 结果流式识别：整段 WAV 上传 + SSE 增量 partial（`on_partial` 回调，
    /// 事件发射由调用方负责）→ final。复用整句的端点/模型/热词选择逻辑。
    #[instrument(skip(self, wav_bytes, opts, on_partial), fields(provider = Self::ID))]
    async fn stream_recognize(
        &self,
        wav_bytes: Vec<u8>,
        opts: &AsrOptions,
        on_partial: Option<Arc<dyn for<'a> Fn(&'a str) + Send + Sync + 'static>>,
    ) -> Result<AsrResult, AsrError> {
        let prompt = llama_prompt_from_hotwords(&self.cred.effective_hotwords(opts));
        super::provider_stream_llama::recognize_stream(
            &self.http,
            &self.cred,
            &self.effective_endpoint(),
            &self.effective_model(),
            prompt,
            wav_bytes,
            on_partial,
        )
        .await
    }
}

/// llama-asr 偏置文本的总字符上限（含引导语）。
///
/// 约 224 token 的 prompt 有效窗口在中文下大致对应这个量级；保守取 200。
const LLAMA_PROMPT_MAX_CHARS: usize = 200;

/// 热词 → llama-server 的 `prompt` 偏置文本。
///
/// 形式：`热词：A、B、C。`——**不是**裸的逗号列表。Qwen3-ASR 的 prompt 偏置是
/// 「前文上下文」，裸列表容易被当成待续的转录文本；带引导语更接近上游 HF
/// 文档的推荐用法（`prompt="Vocabulary: ..."`），偏置目标更明确。
///
/// 两个上限（原实现直接 `join(", ")`，都没有）：
/// - **去重**：同一角色可能重复配置，重复词只会挤占窗口
/// - **总长 ≤ [`LLAMA_PROMPT_MAX_CHARS`]**：OpenAI 兼容 `prompt` 的有效窗口约
///   224 token，超长会被服务端截断，甚至让模型把尾巴的语气当成待续文本而
///   干扰解码。按 **字符**（非字节）截断，避免切断 UTF-8
///
/// ⚠️ **待验证**：`prompt` 是否真被 llama-server 的 `/v1/audio/transcriptions`
/// 消费尚未证实（上游 llama.cpp 无文档确认；已知 vLLM / omlx 实现了该字段）。
/// 若服务端忽略它，热词会**静默失效**——排查时先开 `llama-server --verbose`
/// 看 prompt 是否出现在构造的消息里。
fn llama_prompt_from_hotwords(hotwords: &[Hotword]) -> Option<String> {
    let mut seen = std::collections::HashSet::new();
    let mut words: Vec<&str> = Vec::new();
    for h in hotwords {
        let t = h.text.trim();
        if t.is_empty() {
            continue;
        }
        if seen.insert(t) {
            words.push(t);
        }
    }
    if words.is_empty() {
        return None;
    }
    let prefix = "热词：";
    let suffix = "。";
    let budget =
        LLAMA_PROMPT_MAX_CHARS.saturating_sub(prefix.chars().count() + suffix.chars().count());
    let mut body = String::new();
    for w in words {
        let sep = if body.is_empty() { "" } else { "、" };
        let extra = sep.chars().count() + w.chars().count();
        if body.chars().count() + extra > budget {
            break;
        }
        body.push_str(sep);
        body.push_str(w);
    }
    if body.is_empty() {
        return None;
    }
    Some(format!("{prefix}{body}{suffix}"))
}

/// 解析 llama-server 转写响应文本。
///
/// 实测格式：`{"text": "language <lang><asr_text><转写文本>"}`（无语音时
/// `<lang>` 为 `None`、文本为空）。切 `<asr_text>`：后半是文本，前半是语言。
/// 供整句识别与 SSE 结果流式（provider_stream_llama.rs）共用。
pub(crate) fn parse_llama_text(body: &str) -> Option<(String, Option<String>)> {
    let value: JsonValue = serde_json::from_str(body).ok()?;
    let text = value.get("text").and_then(|t| t.as_str())?;
    match text.split_once("<asr_text>") {
        Some((lang_part, content)) => {
            // `language Chinese` → Chinese；`language None` → None
            let lang = lang_part
                .strip_prefix("language")
                .map(str::trim)
                .filter(|s| !s.is_empty() && *s != "None")
                .map(str::to_string);
            Some((content.to_string(), lang))
        },
        None => Some((text.to_string(), None)),
    }
}

/// 解析 llama-server `/v1/models` 响应，提取模型全名列表。
///
/// 兼容两种结构：`data[].id`（OpenAI 兼容）与 `models[].name`。
fn parse_llama_models(body: &str) -> Vec<String> {
    let value: JsonValue = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mut names = Vec::new();
    if let Some(data) = value.get("data").and_then(|v| v.as_array()) {
        for item in data {
            if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                names.push(id.to_string());
            }
        }
    }
    if names.is_empty() {
        if let Some(models) = value.get("models").and_then(|v| v.as_array()) {
            for item in models {
                if let Some(name) = item.get("name").and_then(|v| v.as_str()) {
                    names.push(name.to_string());
                }
            }
        }
    }
    names
}

// ============================================================================
// 工具函数
// ============================================================================

/// 把 `reqwest::Error` 映射成 [`AsrError`]。
///
/// reqwest 的网络/超时/协议错误统一归类为 provider 错误；上层无需关心细节。
fn map_reqwest_error(e: reqwest::Error) -> AsrError {
    if e.is_timeout() {
        AsrError::ProviderTimeout("network".into())
    } else if e.is_connect() || e.is_request() {
        AsrError::ProviderApiError {
            provider: "network".into(),
            message: format!("请求失败: {e}"),
        }
    } else {
        warn!("reqwest 错误: {e}");
        AsrError::ProviderApiError {
            provider: "network".into(),
            message: format!("{e}"),
        }
    }
}

/// 构造一个 30s 超时的默认 reqwest Client（TLS 走统一的 webpki-roots 配置，
/// Android 上 rustls-platform-verifier 未初始化会 panic，见 utils/tls.rs）。
///
/// 仅供测试 / 内部默认；生产环境调用方应通过 `factory::build_http_client`
/// 注入正确的 TLS 配置。
#[allow(dead_code)]
pub fn default_http_client() -> reqwest::Client {
    let tls = crate::utils::tls::build_tls_config().expect("构建默认 TLS 配置失败");
    reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .timeout(Duration::from_secs(30))
        .build()
        .expect("构建默认 HTTP 客户端失败")
}

// ============================================================================
// Provider 注册表
// ============================================================================

/// 列出所有 provider 的静态元数据。
pub fn list_provider_info() -> Vec<ProviderInfo> {
    vec![
        ProviderInfo {
            id: QwenAsrProvider::ID,
            display_name: QwenAsrProvider::DISPLAY,
            description: "阿里云百炼 ASR（实时 / 非实时）",
            supports_streaming: true,
            config_fields: qwen_asr_config_fields(),
            regions: DashScopeRegion::ALL
                .iter()
                .copied()
                .map(Into::into)
                .collect(),
        },
        ProviderInfo {
            id: LlamaAsrProvider::ID,
            display_name: LlamaAsrProvider::DISPLAY,
            description: "本地 llama-server Qwen3-ASR（整句识别）",
            supports_streaming: false,
            config_fields: llama_asr_config_fields(),
            // 本地服务无地域概念：空数组 → 前端不渲染地域下拉
            regions: Vec::new(),
        },
    ]
}

/// 模型对应的端点类型（选中该模型时应把哪个端点字段切到该协议）。
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EndpointKind {
    /// 同步（非实时）端点 → `provider_configs[id].endpoint`。
    Http,
    /// 实时 WebSocket 端点 → `provider_configs[id].ws_endpoint`。
    Ws,
}

/// 模型元数据（`asr_list_models` 返回给前端渲染下拉）。
#[derive(Debug, Clone, Serialize)]
pub struct ModelInfo {
    /// 模型 id（写入 `provider_configs[id].model`）。qwen 是协议名；
    /// llama-asr 是 `/v1/models` 返回的动态全名（非静态，用 String）。
    pub id: String,
    /// UI 显示名。
    pub display_name: String,
    /// 是否支持流式协议（前端流式开关可用性的权威判定）。
    pub supports_streaming: bool,
    /// 是否默认模型（`provider_configs[id].model` 为空时生效）。
    pub is_default: bool,
    /// 端点预设：选中该模型时把对应端点字段填成**当前地域**的默认值。
    /// `None` = 不干预端点（llama-asr 的端点与模型无关）。
    ///
    /// 只给类型不给完整 URL：端点是地域相关的，由前端从
    /// [`ProviderInfo::regions`] 取当前地域的默认值，避免两处真相。
    pub endpoint_kind: Option<EndpointKind>,
}

/// 单个 DashScope 模型的协议能力描述。
///
/// 模型清单、流式判定、body 格式、热词门控、端点预设**全部从这张表派生**
/// —— 单一真相，避免「加了模型忘了改 `matches!`」导致静默走错链路。
struct QwenModelSpec {
    id: &'static str,
    display: &'static str,
    /// 是否出现在设置页的模型列表里。
    ///
    /// `false` = 隐藏的历史模型：不出现在清单里，但 [`qwen_model_spec`] 仍能查到它，
    /// 因此老配置（`model: "fun-asr-realtime"`）的协议行为完全不变。
    listed: bool,
    /// 走 WebSocket 实时端点（`false` = 同步 HTTP 端点）。
    ws_realtime: bool,
    /// 支持即时热词 `parameters.vocabulary`（文档明确**仅 qwen-audio-3.x 系**）。
    supports_inline_vocabulary: bool,
    /// 支持预编译热词 `parameters.vocabulary_id`。
    supports_vocabulary_id: bool,
    /// 支持 `language_hints`。
    supports_language_hints: bool,
    /// 同步 body 是否沿用旧格式 `content:[{"audio": "data:..."}]`。
    ///
    /// 新的 qwen-audio-3.x 系要求 `content:[{"type":"input_audio","input_audio":{"data":...}}]`；
    /// 历史 Fun-ASR-Realtime 走的是旧格式。一刀切替换会让老配置从「能用」变成
    /// 「未知错误」，故按模型分流。
    use_legacy_audio_content: bool,
    /// 该模型可用的地域。
    regions: &'static [DashScopeRegion],
    /// 所属地域是否以它作为**同步**默认模型。
    is_default_batch: bool,
    /// 前端 `find(supports_streaming)` 挑流式模型时的**首选**。
    is_default_stream: bool,
}

const BOTH_REGIONS: &[DashScopeRegion] =
    &[DashScopeRegion::CnBeijing, DashScopeRegion::ApSoutheast1];
const CN_ONLY: &[DashScopeRegion] = &[DashScopeRegion::CnBeijing];

/// DashScope 语音识别模型清单（按协议实接情况维护）。
///
/// 异步任务类（`*-filetrans`、`paraformer-v2`）协议未接入，不列出。
const QWEN_MODELS: &[QwenModelSpec] = &[
    QwenModelSpec {
        id: "qwen-audio-3.0-asr-flash",
        display: "Qwen-Audio-3.0-ASR-Flash（非实时）",
        listed: true,
        ws_realtime: false,
        supports_inline_vocabulary: true,
        supports_vocabulary_id: true,
        supports_language_hints: true,
        use_legacy_audio_content: false,
        regions: BOTH_REGIONS,
        is_default_batch: true,
        is_default_stream: false,
    },
    QwenModelSpec {
        id: "qwen-audio-3.1-asr-flash",
        display: "Qwen-Audio-3.1-ASR-Flash（非实时）",
        listed: true,
        ws_realtime: false,
        supports_inline_vocabulary: true,
        supports_vocabulary_id: true,
        supports_language_hints: true,
        use_legacy_audio_content: false,
        regions: BOTH_REGIONS,
        // 保留 3.0 作默认：新增模型不改既有用户的行为。想改用 3.1 作默认为改此行
        is_default_batch: false,
        is_default_stream: false,
    },
    // 历史默认非流式模型。DashScope 已把它归入实时（WebSocket）族，但本项目的
    // 同步路径一直在用它且实测可用，故按旧格式保留规格，避免老配置失效。
    // **不在设置页列出**（listed: false）：它与 fun-asr-realtime-2026-02-28 同名
    // 不同协议，两条"Fun-ASR-Realtime"摆在列表里会让人误选；但老配置里存着它的
    // model 名，规格必须仍能查到，否则会静默换 body 格式（它走旧 content 格式）。
    QwenModelSpec {
        id: "fun-asr-realtime",
        display: "Fun-ASR-Realtime（非实时·历史协议）",
        listed: false,
        ws_realtime: false,
        supports_inline_vocabulary: false,
        supports_vocabulary_id: true,
        supports_language_hints: false,
        use_legacy_audio_content: true,
        regions: BOTH_REGIONS,
        is_default_batch: false,
        is_default_stream: false,
    },
    // 流式首选仍是它：现有 WS 客户端是对着这个模型实证写出来的，
    // fun-asr-realtime-2026-02-28 是否复用同一协议尚未实测（见计划 V2）。
    QwenModelSpec {
        id: "paraformer-realtime-v2",
        display: "Paraformer-Realtime-V2",
        listed: true,
        ws_realtime: true,
        supports_inline_vocabulary: false,
        supports_vocabulary_id: true,
        supports_language_hints: true,
        use_legacy_audio_content: false,
        regions: BOTH_REGIONS,
        is_default_batch: false,
        is_default_stream: true,
    },
    // 文档的热词支持表只在北京地域列出它（新加坡只列 fun-asr-realtime 与
    // fun-asr-realtime-2025-11-07），故保守标为北京独有。
    //
    // **不再在设置页列出**（listed: false）：它同样是"Fun-ASR-Realtime"，与上面
    // 那条靠 display 里的一段后缀区分，实测中确实被误认成同一个模型。规格同样
    // 保留——老配置里可能存着它，查不到就会按非流式处理、发到同步端点直接 400
    // （它是 ws_realtime 模型）。
    QwenModelSpec {
        id: "fun-asr-realtime-2026-02-28",
        display: "Fun-ASR-Realtime（2026-02-28·历史）",
        listed: false,
        ws_realtime: true,
        supports_inline_vocabulary: false,
        supports_vocabulary_id: true,
        supports_language_hints: true,
        use_legacy_audio_content: false,
        regions: CN_ONLY,
        is_default_batch: false,
        is_default_stream: false,
    },
    // 文档里实时识别的首推模型。与 fun-asr-realtime 共用同一个 WebSocket 接入
    // 协议（同一份接入文档），因此不需要新的客户端代码——但它和
    // fun-asr-realtime-2026-02-28 一样，**是否与现有客户端完全兼容尚未实测**
    // （现有客户端是对着 paraformer-realtime-v2 实证写的）。
    QwenModelSpec {
        id: "qwen-audio-3.1-asr-flash-streaming",
        display: "Qwen-Audio-3.1-ASR-Flash-Streaming（实时）",
        listed: true,
        ws_realtime: true,
        supports_inline_vocabulary: true,
        supports_vocabulary_id: true,
        supports_language_hints: true,
        use_legacy_audio_content: false,
        regions: BOTH_REGIONS,
        is_default_batch: false,
        is_default_stream: false,
    },
];

/// 按 id 查模型规格。未知模型返回 `None`。
fn qwen_model_spec(model: &str) -> Option<&'static QwenModelSpec> {
    QWEN_MODELS.iter().find(|s| s.id == model)
}

/// qwen（DashScope）语音识别模型清单，按地域过滤，**不含隐藏的历史模型**。
pub fn qwen_models(region: DashScopeRegion) -> Vec<ModelInfo> {
    QWEN_MODELS
        .iter()
        .filter(|s| s.listed && s.regions.contains(&region))
        .map(|s| ModelInfo {
            id: s.id.to_string(),
            display_name: s.display.to_string(),
            supports_streaming: s.ws_realtime,
            is_default: s.is_default_batch,
            endpoint_kind: Some(if s.ws_realtime {
                EndpointKind::Ws
            } else {
                EndpointKind::Http
            }),
        })
        .collect()
}

/// 该模型是否走 WebSocket 实时端点。
///
/// 流式模型只能走 WebSocket；非流式端点（multimodal-generation）不认识它们，
/// DashScope 会返回 HTTP 400 "url error"（模型名与端点不匹配）。
pub fn qwen_is_streaming_model(model: &str) -> bool {
    qwen_model_spec(model).is_some_and(|s| s.ws_realtime)
}

/// 该模型是否支持即时热词 `parameters.vocabulary`。
///
/// 文档明确仅 qwen-audio-3.x 系支持；对 fun-asr-realtime / paraformer 系发这个
/// 参数很可能直接 400，所以必须门控而不是「有热词就发」。
pub fn qwen_supports_inline_vocabulary(model: &str) -> bool {
    qwen_model_spec(model).is_some_and(|s| s.supports_inline_vocabulary)
}

/// 该模型是否支持预编译热词 `parameters.vocabulary_id`。
pub fn qwen_supports_vocabulary_id(model: &str) -> bool {
    qwen_model_spec(model).is_some_and(|s| s.supports_vocabulary_id)
}

/// 该模型是否支持 `language_hints`。
pub fn qwen_supports_language_hints(model: &str) -> bool {
    qwen_model_spec(model).is_some_and(|s| s.supports_language_hints)
}

/// 该模型的同步 body 是否用旧格式（`{"audio": ...}` 而非 `input_audio`）。
fn qwen_uses_legacy_audio_content(model: &str) -> bool {
    qwen_model_spec(model).is_some_and(|s| s.use_legacy_audio_content)
}

/// 给定地域下该走哪种协议的默认模型。
///
/// `ws = true` 取流式首选，`false` 取同步默认。地域内无对应模型时回退到
/// 清单里第一个同协议模型；清单为空（不可能）时回退第一个模型。
///
/// 取代了两处硬编码回退：`QwenAsrProvider::MODEL` 常量与
/// `asr_start_streaming` 里的 `"paraformer-realtime-v2"` 字面量。
pub fn qwen_default_model(ws: bool, region: DashScopeRegion) -> &'static str {
    // 只在「已列出」的模型里挑默认：隐藏的历史模型不该被选为当前模型
    let avail = |s: &QwenModelSpec| s.listed && s.regions.contains(&region) && s.ws_realtime == ws;
    QWEN_MODELS
        .iter()
        .find(|s| {
            avail(s)
                && if ws {
                    s.is_default_stream
                } else {
                    s.is_default_batch
                }
        })
        .or_else(|| QWEN_MODELS.iter().find(|s| avail(s)))
        .or_else(|| QWEN_MODELS.first())
        .map(|s| s.id)
        .unwrap_or("qwen-audio-3.0-asr-flash")
}

/// 按模型能力构造 WS run-task 的可选参数。
///
/// **热词门控集中在这里**：模型不支持某种热词机制时**不发**该字段，而不是
/// 「有热词就发」—— 对不支持的模型发 `parameters.vocabulary` 很可能直接 400。
/// 静默丢弃会让用户困惑，所以同时 warn 一条说明原因。
pub fn build_stream_params(
    model: &str,
    cred: &ProviderCredentials,
    opts: &AsrOptions,
) -> super::provider_stream::StreamParams {
    let hotwords = cred.effective_hotwords(opts);
    let vocabulary = if hotwords.is_empty() {
        None
    } else if qwen_supports_inline_vocabulary(model) {
        Some(hotwords_to_vocabulary_json(&hotwords))
    } else {
        warn!(
            "[ASR] 流式模型 {model} 不支持即时热词，已忽略 {} 条热词\
             （该模型族只支持预编译热词表 vocabulary_id）",
            hotwords.len()
        );
        None
    };

    if !cred.vocabulary_id.trim().is_empty() && !qwen_supports_vocabulary_id(model) {
        warn!(
            "[ASR] 模型 {model} 不支持预编译热词表，已忽略 vocabulary_id={}",
            cred.vocabulary_id.trim()
        );
    }
    let vocabulary_id = if qwen_supports_vocabulary_id(model) {
        Some(cred.vocabulary_id.trim().to_string()).filter(|s| !s.is_empty())
    } else {
        None
    };

    super::provider_stream::StreamParams {
        // 文档：qwen-audio-3.x 最多 4 个、fun-asr-realtime 系只取第一个。
        // 这里只做能力门控，个数由服务端按各自规则处理
        language_hint: if qwen_supports_language_hints(model) {
            opts.language_hint.clone()
        } else {
            None
        },
        vocabulary,
        vocabulary_id,
    }
}

/// 按 provider id 返回模型清单。
///
/// qwen 返回静态清单；llama-asr 动态请求服务端 `/v1/models`（endpoint 取
/// 当前设置，默认 `http://127.0.0.1:8080`；服务未启动/模型列表为空时返回
/// `ProviderApiError`，前端展示错误并回退为模型文本输入）。未接入模型选择
/// 的 provider 返回空数组（前端据此隐藏模型下拉）。
///
/// `region_override` 是调用方（设置页表单）当前选中的地域，优先于持久化配置
/// ——理由见 [`qwen_region_override`]。只有 qwen 用得上。
pub async fn list_models(
    provider_id: &str,
    region_override: Option<&str>,
    app: &tauri::AppHandle,
    http: &reqwest::Client,
) -> Result<Vec<ModelInfo>, AsrError> {
    match provider_id {
        // unwrap_or_else 而非 unwrap_or：被覆盖时不必白读一次磁盘配置
        QwenAsrProvider::ID => Ok(qwen_models(
            qwen_region_override(region_override).unwrap_or_else(|| qwen_region(app)),
        )),
        // llama-asr 的清单来自服务端 /v1/models，与地域无关，忽略该参数
        LlamaAsrProvider::ID => llama_models(app, http).await,
        _ => Ok(Vec::new()),
    }
}

/// 调用方显式指定的地域。`None` = 未指定，由 [`qwen_region`] 读持久化配置。
///
/// **为什么需要它**：`qwen_region` 读的是**已落盘**的 settings.json，而设置页
/// 改地域后要等 500ms debounce 才写盘，却在那之前就重拉模型（切地域要立刻刷新
/// 列表）。不传覆盖值的话后端会按旧地域返回，列表要等下次打开设置页才更新
/// ——表现为"切地域后少/多一个模型"，且 `ensureModelValidForRegion` 会拿着
/// 旧清单做回退判断。传了覆盖值，清单就只取决于用户当前选的地域，与磁盘无关。
///
/// 空白串按"未指定"处理：调用方拿到空配置时不该把地域意外重置成默认值。
fn qwen_region_override(region_override: Option<&str>) -> Option<DashScopeRegion> {
    match region_override {
        Some(r) if !r.trim().is_empty() => Some(DashScopeRegion::parse(r)),
        _ => None,
    }
}

/// 读 qwen provider 当前配置的地域（模型清单按地域过滤）。
///
/// 读设置失败时回退默认地域而非报错：模型下拉拉不出来是可用性问题，
/// 不该让整个设置页变成一个错误弹窗。
fn qwen_region(app: &tauri::AppHandle) -> DashScopeRegion {
    match super::settings::load(app) {
        Ok(s) => DashScopeRegion::parse(
            &s.provider_configs
                .get(QwenAsrProvider::ID)
                .map(|c| c.region.clone())
                .unwrap_or_default(),
        ),
        Err(e) => {
            warn!("[ASR] 读取地域配置失败，回退默认地域: {e}");
            DashScopeRegion::DEFAULT
        },
    }
}

/// 请求 llama-server `/v1/models`，映射为 ModelInfo 列表（llama-asr 全部非流式）。
///
/// 显示名取模型文件名的最后一段（`models/Qwen3-ASR-1.7B-Q8_0.gguf` →
/// `Qwen3-ASR-1.7B-Q8_0.gguf`），id 保留全名（服务端按全名匹配模型）。
async fn llama_models(
    app: &tauri::AppHandle,
    http: &reqwest::Client,
) -> Result<Vec<ModelInfo>, AsrError> {
    let settings = super::settings::load(app)?;
    let cred = settings
        .provider_configs
        .get(LlamaAsrProvider::ID)
        .cloned()
        .unwrap_or_default();
    let endpoint = if cred.endpoint.trim().is_empty() {
        LlamaAsrProvider::DEFAULT_ENDPOINT.to_string()
    } else {
        cred.endpoint.trim_end_matches('/').to_string()
    };
    let url = format!("{endpoint}/v1/models");
    debug!("[ASR] 拉取模型列表: {url}");
    let resp = http.get(&url).send().await.map_err(map_reqwest_error)?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(AsrError::ProviderApiError {
            provider: LlamaAsrProvider::ID.into(),
            message: format!("HTTP {status}: {body}"),
        });
    }
    let body_text = resp.text().await.map_err(map_reqwest_error)?;
    let names = parse_llama_models(&body_text);
    if names.is_empty() {
        return Err(AsrError::ProviderApiError {
            provider: LlamaAsrProvider::ID.into(),
            message: "模型列表为空（/v1/models 未返回任何模型）".into(),
        });
    }
    Ok(names
        .into_iter()
        .enumerate()
        .map(|(i, id)| ModelInfo {
            display_name: id.rsplit('/').next().unwrap_or(&id).to_string(),
            // 结果流式（SSE）：音频整段上传、结果增量返回。与 qwen WS 真流式
            // 语义不同，但前端流式开关可用（录音结束出 partial 而非边录边出）
            supports_streaming: true,
            is_default: i == 0,
            // 端点与模型无关（本地服务地址固定）
            endpoint_kind: None,
            id,
        })
        .collect())
}

/// 按 id 创建 provider 实例。
///
/// 找不到 id 时返回 [`AsrError::ProviderNotFound`]。
pub async fn get_provider(
    id: &str,
    cred: &ProviderCredentials,
    http: &reqwest::Client,
) -> Result<Arc<dyn AsrProvider>, AsrError> {
    debug!("创建 ASR provider: {id}");
    let provider: Arc<dyn AsrProvider> = match id {
        QwenAsrProvider::ID => Arc::new(QwenAsrProvider::new(http.clone(), cred.clone())?),
        LlamaAsrProvider::ID => Arc::new(LlamaAsrProvider::new(http.clone(), cred.clone())),
        other => {
            return Err(AsrError::ProviderNotFound(other.into()));
        },
    };
    Ok(provider)
}

// ============================================================================
// 静态字段（让 trait 方法与 list_provider_info 共用一份数据）
// ============================================================================

/// 地域下拉的选项（由 [`super::region::DashScopeRegion::ALL`] 投影而来）。
fn region_field_options() -> Vec<AsrConfigFieldOption> {
    DashScopeRegion::ALL
        .iter()
        .map(|r| AsrConfigFieldOption {
            value: r.id(),
            label: r.label(),
        })
        .collect()
}

fn qwen_asr_config_fields() -> Vec<AsrConfigField> {
    vec![
        AsrConfigField {
            key: "region",
            label: "地域",
            kind: ConfigFieldKind::Select,
            required: true,
            default_value: Some(DashScopeRegion::DEFAULT.id()),
            placeholder: None,
            hint: Some(
                "华北2（北京）与新加坡的域名、API Key、模型列表互相独立，不能混用。\
                 下方 API Key 按地域分开保存，切换后显示的是该地域自己的那一份",
            ),
            options: region_field_options(),
        },
        AsrConfigField {
            // 不是 `api_key` 而是按地域分开存的 `api_keys` 映射：北京与新加坡的
            // Key 互相独立，切换地域不能把另一个地域的 Key 冲掉。UI 仍只显示
            // 当前地域的一个框（见 ConfigFieldKind::PasswordMap）。
            key: "api_keys",
            label: "DashScope API Key",
            kind: ConfigFieldKind::PasswordMap,
            required: true,
            default_value: None,
            placeholder: Some("sk-..."),
            // hint 由前端以纯文本渲染（`{{ field.hint }}`），不能写 Markdown 记号
            hint: Some(
                "阿里云百炼（Model Studio）平台 Key，须与所选地域一致。\
                 每个地域单独保存：切换上方地域后，这里显示的是该地域自己的 Key，\
                 不会覆盖另一个地域已填的",
            ),
            options: Vec::new(),
        },
        AsrConfigField {
            key: "endpoint",
            label: "非实时端点",
            kind: ConfigFieldKind::Text,
            required: false,
            // 默认值随地域变化，是派生值而非静态串 —— 由 ProviderInfo.regions
            // 下发、前端在切地域时填入（见 SettingsAsr.vue 的地域 watch）。
            default_value: None,
            placeholder: Some("留空使用所选地域的默认地址"),
            hint: Some(
                "multimodal-generation 端点。填业务空间专属域名可覆盖，形如 \
                 https://{WorkspaceId}.cn-beijing.maas.aliyuncs.com/api/v1/services/aigc/multimodal-generation/generation",
            ),
            options: Vec::new(),
        },
        AsrConfigField {
            key: "ws_endpoint",
            label: "实时（流式）端点",
            kind: ConfigFieldKind::Text,
            required: false,
            default_value: None,
            placeholder: Some("留空使用所选地域的默认地址"),
            hint: Some(
                "WebSocket 端点，仅流式模型使用。形如 \
                 wss://{WorkspaceId}.ap-southeast-1.maas.aliyuncs.com/api-ws/v1/inference",
            ),
            options: Vec::new(),
        },
        AsrConfigField {
            key: "vocabulary_id",
            label: "热词表 ID（预编译热词）",
            kind: ConfigFieldKind::Text,
            required: false,
            default_value: None,
            placeholder: Some("vocab-xxxxxxxx"),
            hint: Some(
                "仅 fun-asr-realtime / paraformer-realtime 系生效（qwen-audio-3.x 系用不上的话请填下方热词）。\
                 需在百炼控制台预先创建，且建表时的 target_model 必须与识别模型一致，否则热词静默不生效",
            ),
            options: Vec::new(),
        },
        AsrConfigField {
            key: "hotwords",
            label: "热词（可选）",
            kind: ConfigFieldKind::Text,
            required: false,
            default_value: None,
            placeholder: Some("量子计算, Anthropic:5, 厄洛替尼"),
            hint: Some(
                "逗号/分号/空白分隔；可选权重「词:权重」（1-5，或 50 为超级热词），缺省 4。\
                 仅 qwen-audio-3.x 系支持即时热词；fun-asr-realtime / paraformer 系请改用上方的热词表 ID",
            ),
            options: Vec::new(),
        },
    ]
}

fn llama_asr_config_fields() -> Vec<AsrConfigField> {
    vec![
        AsrConfigField {
            key: "endpoint",
            label: "服务地址",
            kind: ConfigFieldKind::Text,
            required: false,
            default_value: Some(LlamaAsrProvider::DEFAULT_ENDPOINT),
            placeholder: Some("http://127.0.0.1:8080"),
            hint: Some("llama-server 地址（Qwen3-ASR 本地部署）；局域网部署改 http://<IP>:8080"),
            options: Vec::new(),
        },
        AsrConfigField {
            key: "model",
            label: "模型",
            kind: ConfigFieldKind::Text,
            required: false,
            default_value: Some(LlamaAsrProvider::DEFAULT_MODEL),
            placeholder: Some("models/Qwen3-ASR-1.7B-Q8_0.gguf"),
            hint: Some("从上方模型列表选择，或用 /v1/models 查询服务端全名"),
            options: Vec::new(),
        },
        AsrConfigField {
            key: "api_key",
            label: "API Key（可选）",
            kind: ConfigFieldKind::Password,
            required: false,
            default_value: None,
            placeholder: Some("本地服务无需填写"),
            hint: Some("llama-server 带 --api-key 部署时填写，否则留空"),
            options: Vec::new(),
        },
        AsrConfigField {
            key: "hotwords",
            label: "热词（可选）",
            kind: ConfigFieldKind::Text,
            required: false,
            default_value: None,
            placeholder: Some("量子计算, Anthropic, 厄洛替尼"),
            hint: Some(
                "逗号/分号/空白分隔，作为 prompt 偏置提升专名命中率。\
                 总长超 200 字符会截断。注意：本地服务是否消费 prompt 尚未验证，\
                 若无效可开 llama-server --verbose 核对",
            ),
            options: Vec::new(),
        },
    ]
}

// ============================================================================
// 单元测试（zero deps：仅覆盖可纯函数测的部分）
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_qwen_text_prefers_top_level_text() {
        let body = r#"{"text": "你好"}"#;
        assert_eq!(parse_qwen_text(body).as_deref(), Some("你好"));
    }

    #[test]
    fn parse_qwen_text_falls_back_to_output_text() {
        let body = r#"{"output": {"text": "hi"}}"#;
        assert_eq!(parse_qwen_text(body).as_deref(), Some("hi"));
    }

    #[test]
    fn parse_qwen_text_falls_back_to_result_text() {
        let body = r#"{"result": {"text": "hola"}}"#;
        assert_eq!(parse_qwen_text(body).as_deref(), Some("hola"));
    }

    #[test]
    fn parse_qwen_text_returns_none_for_garbage() {
        assert!(parse_qwen_text("not json").is_none());
        assert!(parse_qwen_text(r#"{"foo": 1}"#).is_none());
    }

    #[test]
    fn provider_credentials_normalizes_trailing_slash() {
        let c = ProviderCredentials {
            api_key: "k".into(),
            endpoint: "http://x.example.com/".into(),
            ..Default::default()
        };
        assert_eq!(c.normalized_endpoint(), "http://x.example.com");
        assert!(c.has_api_key());
    }

    #[test]
    fn provider_credentials_whitespace_api_key_is_empty() {
        let c = ProviderCredentials {
            api_key: "   ".into(),
            ..Default::default()
        };
        assert!(!c.has_api_key());
    }

    #[test]
    fn list_provider_info_has_two_providers() {
        let info = list_provider_info();
        assert_eq!(info.len(), 2);
        assert!(info.iter().any(|p| p.id == "qwen-asr"));
        assert!(info.iter().any(|p| p.id == "llama-asr"));
    }

    // ── 地域与模型表 ────────────────────────────────────────────────

    #[test]
    fn only_qwen_declares_regions() {
        let info = list_provider_info();
        let qwen = info.iter().find(|p| p.id == "qwen-asr").unwrap();
        let llama = info.iter().find(|p| p.id == "llama-asr").unwrap();
        assert_eq!(qwen.regions.len(), DashScopeRegion::ALL.len());
        // 本地服务无地域概念 → 空数组，前端据此不渲染下拉
        assert!(llama.regions.is_empty());
    }

    #[test]
    fn region_config_field_matches_region_table() {
        let fields = qwen_asr_config_fields();
        let region = fields.iter().find(|f| f.key == "region").unwrap();
        assert_eq!(region.kind, ConfigFieldKind::Select);
        let ids: Vec<_> = region.options.iter().map(|o| o.value).collect();
        let expected: Vec<_> = DashScopeRegion::ALL.iter().map(|r| r.id()).collect();
        assert_eq!(ids, expected);
    }

    #[test]
    fn qwen_region_override_only_accepts_a_meaningful_value() {
        // 显式传地域 → 用它（模型清单不再依赖已落盘的配置）
        assert_eq!(
            qwen_region_override(Some("ap-southeast-1")),
            Some(DashScopeRegion::ApSoutheast1)
        );
        // 未知地域按解析规则回退默认，与别处同一套语义
        assert_eq!(
            qwen_region_override(Some("火星")),
            Some(DashScopeRegion::DEFAULT)
        );
        // 未传 / 空白 = "未指定"，交给持久化配置，不能被误当成默认地域
        assert_eq!(qwen_region_override(None), None);
        assert_eq!(qwen_region_override(Some("")), None);
        assert_eq!(qwen_region_override(Some("   ")), None);
    }

    #[test]
    fn qwen_api_key_field_is_region_scoped() {
        let fields = qwen_asr_config_fields();
        let key = fields.iter().find(|f| f.key == "api_keys").unwrap();
        assert_eq!(key.kind, ConfigFieldKind::PasswordMap);
        // 不能再留一个扁平的单键入口：两个入口会并存两份真相，
        // 用户填了其中一个、另一个静默生效，且分地域存储形同虚设
        assert!(!fields.iter().any(|f| f.key == "api_key"));

        // llama-asr 无地域概念（regions 为空），Key 保持扁平的单键字段
        let llama = llama_asr_config_fields();
        assert!(llama.iter().any(|f| f.key == "api_key"));
        assert!(!llama.iter().any(|f| f.key == "api_keys"));
    }

    #[test]
    fn every_config_field_key_exists_on_provider_config() {
        // 回归防线：设置页把 config_field.key 写成 ProviderConfig 的顶层键，
        // 而 ProviderConfig 没有 deny_unknown_fields —— 漏加字段会被 serde
        // 静默丢弃（用户填了不生效、刷新即失）。这里用序列化往返兜底。
        for fields in [qwen_asr_config_fields(), llama_asr_config_fields()] {
            for f in fields {
                let cfg = crate::ai_service::asr::settings::ProviderConfig::default();
                let value = serde_json::to_value(&cfg).expect("ProviderConfig 可序列化");
                let obj = value.as_object().expect("是对象");
                assert!(
                    obj.contains_key(f.key),
                    "config_field '{}' 在 ProviderConfig 上不存在——会被静默丢弃",
                    f.key
                );
            }
        }
    }

    #[test]
    fn model_spec_table_is_self_consistent() {
        for s in QWEN_MODELS {
            assert!(!s.regions.is_empty(), "{} 没有可用地域", s.id);
            if s.ws_realtime {
                // 流式模型只能走 WebSocket 端点
                assert!(
                    !s.use_legacy_audio_content,
                    "{} 是流式模型，不该有同步 body 格式选项",
                    s.id
                );
            }
            // 即时热词是 qwen-audio-3.x 系专属能力
            if s.supports_inline_vocabulary {
                assert!(
                    s.id.starts_with("qwen-audio-3"),
                    "{} 声明了即时热词，但文档仅 qwen-audio-3.x 系支持",
                    s.id
                );
            }
        }
    }

    #[test]
    fn each_region_has_a_default_batch_and_stream_model() {
        for r in DashScopeRegion::ALL.iter().copied() {
            let models = qwen_models(r);
            assert!(!models.is_empty(), "地域 {} 没有模型", r.id());
            assert_eq!(
                models.iter().filter(|m| m.is_default).count(),
                1,
                "地域 {} 的同步默认模型不唯一",
                r.id()
            );
            let default = qwen_default_model(false, r);
            assert!(models.iter().any(|m| m.id == default));
            assert!(!qwen_is_streaming_model(default));
            // 流式默认模型也必须在清单里
            let stream = qwen_default_model(true, r);
            assert!(models.iter().any(|m| m.id == stream));
            assert!(qwen_is_streaming_model(stream));
        }
    }

    #[test]
    fn streaming_models_are_flagged_by_the_table() {
        // 取代原来写死的 matches!(model, "paraformer-realtime-v2")
        assert!(qwen_is_streaming_model("paraformer-realtime-v2"));
        assert!(qwen_is_streaming_model("fun-asr-realtime-2026-02-28"));
        assert!(!qwen_is_streaming_model("qwen-audio-3.0-asr-flash"));
        assert!(!qwen_is_streaming_model("fun-asr-realtime"));
        assert!(!qwen_is_streaming_model("不存在的模型"));
    }

    #[test]
    fn hidden_models_are_unlisted_but_still_resolvable() {
        // 两条 Fun-ASR-Realtime 都已从设置页移除：同名不同协议，摆在一起会被
        // 误选。但**规格必须保留**——老配置里可能存着这些名字，查不到就会按
        // 非流式处理（流式的那个会被发到同步端点，DashScope 直接 400），
        // 或者静默换掉同步 body 格式（fun-asr-realtime 走旧 content 格式）。
        for id in ["fun-asr-realtime", "fun-asr-realtime-2026-02-28"] {
            for r in DashScopeRegion::ALL.iter().copied() {
                assert!(
                    !qwen_models(r).iter().any(|m| m.id == id),
                    "{id} 不该出现在地域 {} 的清单里",
                    r.id()
                );
            }
            assert!(qwen_model_spec(id).is_some(), "{id} 的规格被删了");
        }
        // 规格保留的实际后果：协议判定仍照表走
        assert!(qwen_is_streaming_model("fun-asr-realtime-2026-02-28"));
        assert!(!qwen_is_streaming_model("fun-asr-realtime"));
        assert!(qwen_uses_legacy_audio_content("fun-asr-realtime"));
    }

    #[test]
    fn listed_models_are_available_in_every_region() {
        // 目前两地清单应完全一致——唯一的北京独有模型（fun-asr-realtime-2026-02-28）
        // 已下线。将来若新增地域专属模型，这条会失败，而那正是需要停下来确认
        // 「设置页切地域后列表会变、模型可能失效」的时刻。
        let cn: Vec<String> = qwen_models(DashScopeRegion::CnBeijing)
            .into_iter()
            .map(|m| m.id)
            .collect();
        let sg: Vec<String> = qwen_models(DashScopeRegion::ApSoutheast1)
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(cn, sg, "两地模型清单不再一致");
        assert!(cn.contains(&"qwen-audio-3.0-asr-flash".to_string()));
    }

    #[test]
    fn hidden_legacy_model_is_unlisted_but_still_resolvable() {
        // 历史 fun-asr-realtime 在哪个地域都不出现。
        // 断言用「成员/属性」而非数量——数量会随每次加模型而失效，
        // 那样这个测试就沦为改一次模型改一次断言，失去防回归的意义。
        for r in DashScopeRegion::ALL.iter().copied() {
            let ids: Vec<_> = qwen_models(r).into_iter().map(|m| m.id).collect();
            assert!(
                !ids.contains(&"fun-asr-realtime".to_string()),
                "地域 {} 不该列出历史模型: {ids:?}",
                r.id()
            );
            // 列出的每一项都必须是显式标记 listed 的（隐藏项不得漏出去）
            assert!(
                ids.iter()
                    .all(|id| qwen_model_spec(id).is_some_and(|s| s.listed)),
                "地域 {} 列出了未标记 listed 的模型: {ids:?}",
                r.id()
            );
        }

        // 但规格必须仍能查到——否则老配置（model: "fun-asr-realtime"）会静默换
        // body 格式，从「能用」变成「未知错误」。这是本次最容易漏的回归点。
        assert!(!qwen_is_streaming_model("fun-asr-realtime"));
        assert!(qwen_supports_vocabulary_id("fun-asr-realtime"));
        assert!(!qwen_supports_inline_vocabulary("fun-asr-realtime"));
        assert!(qwen_uses_legacy_audio_content("fun-asr-realtime"));
        // 列出的模型则用新格式
        assert!(!qwen_uses_legacy_audio_content("qwen-audio-3.0-asr-flash"));

        // 隐藏模型不该被选为地域默认
        for r in DashScopeRegion::ALL.iter().copied() {
            assert_ne!(qwen_default_model(false, r), "fun-asr-realtime");
            assert_ne!(qwen_default_model(true, r), "fun-asr-realtime");
        }
    }

    #[test]
    fn qwen_audio_31_models_are_available_in_both_regions() {
        for r in DashScopeRegion::ALL.iter().copied() {
            let ids: Vec<_> = qwen_models(r).into_iter().map(|m| m.id).collect();
            assert!(
                ids.contains(&"qwen-audio-3.1-asr-flash".to_string()),
                "地域 {} 缺 3.1 同步版: {ids:?}",
                r.id()
            );
            assert!(
                ids.contains(&"qwen-audio-3.1-asr-flash-streaming".to_string()),
                "地域 {} 缺 3.1 流式版: {ids:?}",
                r.id()
            );
        }
        // 同步版走非流式端点，流式版走 WebSocket
        assert!(!qwen_is_streaming_model("qwen-audio-3.1-asr-flash"));
        assert!(qwen_is_streaming_model(
            "qwen-audio-3.1-asr-flash-streaming"
        ));
        // 两者都属于 qwen-audio-3.x 系，都支持即时热词
        assert!(qwen_supports_inline_vocabulary("qwen-audio-3.1-asr-flash"));
        assert!(qwen_supports_inline_vocabulary(
            "qwen-audio-3.1-asr-flash-streaming"
        ));
        // 都不该被选为地域默认（默认仍是 3.0 同步 + paraformer 流式）
        for r in DashScopeRegion::ALL.iter().copied() {
            assert_eq!(qwen_default_model(false, r), "qwen-audio-3.0-asr-flash");
            assert_eq!(qwen_default_model(true, r), "paraformer-realtime-v2");
        }
    }

    #[test]
    fn inline_vocabulary_is_gated_to_qwen_audio_3x() {
        assert!(qwen_supports_inline_vocabulary("qwen-audio-3.0-asr-flash"));
        // 文档明确 fun-asr-realtime / paraformer 系不支持即时热词
        assert!(!qwen_supports_inline_vocabulary(
            "fun-asr-realtime-2026-02-28"
        ));
        assert!(!qwen_supports_inline_vocabulary("paraformer-realtime-v2"));
        assert!(qwen_supports_vocabulary_id("paraformer-realtime-v2"));
    }

    // ── 端点派生 ────────────────────────────────────────────────────

    #[test]
    fn effective_endpoints_fall_back_by_region() {
        let sg = ProviderCredentials {
            region: "ap-southeast-1".into(),
            ..Default::default()
        };
        assert_eq!(
            sg.effective_http_endpoint(),
            DashScopeRegion::ApSoutheast1.http_endpoint()
        );
        assert_eq!(
            sg.effective_ws_endpoint(),
            DashScopeRegion::ApSoutheast1.ws_endpoint()
        );

        let unknown = ProviderCredentials {
            region: "火星".into(),
            ..Default::default()
        };
        assert_eq!(
            unknown.effective_http_endpoint(),
            DashScopeRegion::DEFAULT.http_endpoint()
        );
    }

    #[test]
    fn effective_endpoints_keep_explicit_config() {
        let c = ProviderCredentials {
            endpoint: "https://llm-x.cn-beijing.maas.aliyuncs.com/api/v1/x".into(),
            ws_endpoint: "wss://llm-x.cn-beijing.maas.aliyuncs.com/api-ws/v1/inference".into(),
            region: "ap-southeast-1".into(),
            ..Default::default()
        };
        assert_eq!(
            c.effective_http_endpoint(),
            "https://llm-x.cn-beijing.maas.aliyuncs.com/api/v1/x"
        );
        assert_eq!(
            c.effective_ws_endpoint(),
            "wss://llm-x.cn-beijing.maas.aliyuncs.com/api-ws/v1/inference"
        );
    }

    #[test]
    fn http_endpoint_rejects_a_ws_address() {
        // 老数据的坑：唯一的 endpoint 被模型预设写成了 wss://
        let c = ProviderCredentials {
            endpoint: "wss://dashscope.aliyuncs.com/api-ws/v1/inference".into(),
            region: "ap-southeast-1".into(),
            ..Default::default()
        };
        // 拿去发 HTTP 会 builder error，必须回退该地域的同步端点
        assert_eq!(
            c.effective_http_endpoint(),
            DashScopeRegion::ApSoutheast1.http_endpoint()
        );
    }

    // ── 热词 ────────────────────────────────────────────────────────

    #[test]
    fn call_level_hotwords_override_config_level() {
        let cred = ProviderCredentials {
            hotwords: vec![Hotword::new("配置级")],
            ..Default::default()
        };
        // 空 → 回退配置级
        assert_eq!(
            cred.effective_hotwords(&AsrOptions::default())[0].text,
            "配置级"
        );
        // 非空 → 覆盖
        let opts = AsrOptions {
            language_hint: None,
            hotwords: vec![Hotword::new("调用级")],
        };
        assert_eq!(cred.effective_hotwords(&opts)[0].text, "调用级");
    }

    #[test]
    fn hotword_weights_are_clamped() {
        assert_eq!(Hotword::with_weight("词", 0).weight, 1);
        assert_eq!(Hotword::with_weight("词", 255).weight, Hotword::MAX_WEIGHT);
        assert_eq!(Hotword::new("词").weight, Hotword::DEFAULT_WEIGHT);
    }

    #[test]
    fn hotword_from_text_list_drops_blanks() {
        let list = Hotword::from_text_list(["张三", "  ", "", "李四"]);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].text, "张三");
        assert_eq!(list[0].weight, Hotword::DEFAULT_WEIGHT);
    }

    #[test]
    fn vocabulary_json_uses_weight_and_dedups() {
        let v = hotwords_to_vocabulary_json(&[
            Hotword::with_weight("张三", 5),
            Hotword::with_weight("李四", 50),
            Hotword::new("   "),             // 空白词条丢弃
            Hotword::with_weight("张三", 1), // 重复 → 保留先出现的
        ]);
        assert_eq!(v["张三"], 5);
        assert_eq!(v["李四"], 50);
        assert_eq!(v.as_object().unwrap().len(), 2);
    }

    #[test]
    fn stream_params_gate_hotwords_by_model() {
        // paraformer 系：不发即时热词，发 vocabulary_id
        let cred = ProviderCredentials {
            hotwords: vec![Hotword::new("张三")],
            vocabulary_id: "vocab-abc".into(),
            ..Default::default()
        };
        let opts = AsrOptions {
            language_hint: Some("zh".into()),
            hotwords: Vec::new(),
        };
        let p = build_stream_params("paraformer-realtime-v2", &cred, &opts);
        assert!(p.vocabulary.is_none(), "paraformer 不支持即时热词，不该发");
        assert_eq!(p.vocabulary_id.as_deref(), Some("vocab-abc"));
        assert_eq!(p.language_hint.as_deref(), Some("zh"));

        // 不支持的模型上配了 vocabulary_id → 忽略（避免 400）
        let p2 = build_stream_params("不存在的模型", &cred, &opts);
        assert!(p2.vocabulary_id.is_none());
        assert!(p2.language_hint.is_none());
    }

    #[test]
    fn stream_params_omit_empty_hotwords_and_vocabulary_id() {
        let cred = ProviderCredentials::default();
        let p = build_stream_params("paraformer-realtime-v2", &cred, &AsrOptions::default());
        assert!(p.vocabulary.is_none());
        assert!(p.vocabulary_id.is_none());
        assert!(p.language_hint.is_none());
    }

    // ── llama 偏置文本 ──────────────────────────────────────────────

    #[test]
    fn llama_prompt_is_none_without_hotwords() {
        assert!(llama_prompt_from_hotwords(&[]).is_none());
        // 全是空白词条 → 等同没有
        assert!(llama_prompt_from_hotwords(&[Hotword::new("  ")]).is_none());
    }

    #[test]
    fn llama_prompt_is_not_a_bare_comma_list() {
        let p = llama_prompt_from_hotwords(&[Hotword::new("张三"), Hotword::new("李四")]).unwrap();
        // 带引导语的自然语言形式，而非裸 "A, B"
        assert!(p.starts_with("热词："));
        assert!(p.contains("张三、李四"));
        assert!(p.ends_with("。"));
    }

    #[test]
    fn llama_prompt_dedups_and_caps_length() {
        let dup = llama_prompt_from_hotwords(&[
            Hotword::new("张三"),
            Hotword::new("张三"),
            Hotword::new("李四"),
        ])
        .unwrap();
        assert_eq!(dup.matches("张三").count(), 1);

        // 超长输入被截断到上限内（按字符，不切断 UTF-8）
        let many: Vec<Hotword> = (0..200).map(|i| Hotword::new(format!("词{i}"))).collect();
        let long = llama_prompt_from_hotwords(&many).unwrap();
        assert!(long.chars().count() <= LLAMA_PROMPT_MAX_CHARS);
        assert!(long.starts_with("热词："));
    }

    #[test]
    fn llama_prompt_ignores_weights() {
        // llama 协议无权重概念：权重不同不应改变输出
        let a = llama_prompt_from_hotwords(&[Hotword::with_weight("张三", 1)]).unwrap();
        let b = llama_prompt_from_hotwords(&[Hotword::with_weight("张三", 50)]).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn llama_effective_endpoint_falls_back_when_empty() {
        // 空 endpoint → 默认 127.0.0.1:8080（否则拼出相对 URL，reqwest builder error）
        let p = LlamaAsrProvider::new(
            default_http_client(),
            ProviderCredentials {
                endpoint: String::new(),
                ..Default::default()
            },
        );
        assert_eq!(p.effective_endpoint(), LlamaAsrProvider::DEFAULT_ENDPOINT);
    }

    #[test]
    fn llama_effective_endpoint_rejects_non_http_scheme() {
        // 缺 http:// 前缀（手填 127.0.0.1:8080）→ 回退默认
        let p = LlamaAsrProvider::new(
            default_http_client(),
            ProviderCredentials {
                endpoint: "127.0.0.1:8080".into(),
                ..Default::default()
            },
        );
        assert_eq!(p.effective_endpoint(), LlamaAsrProvider::DEFAULT_ENDPOINT);
    }

    #[test]
    fn llama_effective_endpoint_keeps_valid_url_and_trims_slash() {
        let p = LlamaAsrProvider::new(
            default_http_client(),
            ProviderCredentials {
                endpoint: "http://192.168.1.5:9000/".into(),
                ..Default::default()
            },
        );
        assert_eq!(p.effective_endpoint(), "http://192.168.1.5:9000");
    }

    #[test]
    fn parse_llama_text_splits_asr_text_marker() {
        let body = r#"{"text":"language Chinese<asr_text>甚至出现交易几乎停滞的情况。"}"#;
        let (text, lang) = parse_llama_text(body).expect("应解析成功");
        assert_eq!(text, "甚至出现交易几乎停滞的情况。");
        assert_eq!(lang.as_deref(), Some("Chinese"));
    }

    #[test]
    fn parse_llama_text_none_language_is_empty() {
        // 无语音：`language None<asr_text>` 空文本
        let body = r#"{"text":"language None<asr_text>"}"#;
        let (text, lang) = parse_llama_text(body).expect("应解析成功");
        assert_eq!(text, "");
        assert_eq!(lang, None);
    }

    #[test]
    fn parse_llama_text_plain_text_fallback() {
        // 无 <asr_text> 标记（协议异常兜底）：整体当文本
        let body = r#"{"text":"你好"}"#;
        let (text, lang) = parse_llama_text(body).expect("应解析成功");
        assert_eq!(text, "你好");
        assert_eq!(lang, None);
    }

    #[test]
    fn parse_llama_text_garbage_returns_none() {
        assert!(parse_llama_text("not json").is_none());
        assert!(parse_llama_text(r#"{"foo": 1}"#).is_none());
    }

    #[test]
    fn parse_llama_models_extracts_data_ids() {
        let body = r#"{"data":[{"id":"models/Qwen3-ASR-1.7B-Q8_0.gguf"},{"id":"models/Qwen3-ASR-0.6B-Q8_0.gguf"}]}"#;
        assert_eq!(
            parse_llama_models(body),
            vec![
                "models/Qwen3-ASR-1.7B-Q8_0.gguf",
                "models/Qwen3-ASR-0.6B-Q8_0.gguf"
            ]
        );
    }

    #[test]
    fn parse_llama_models_falls_back_to_models_names() {
        // llama.cpp 某些版本返回 models[].name 而非 data[].id
        let body = r#"{"models":[{"name":"models/Qwen3-ASR-1.7B-Q8_0.gguf"}]}"#;
        assert_eq!(
            parse_llama_models(body),
            vec!["models/Qwen3-ASR-1.7B-Q8_0.gguf"]
        );
    }

    #[test]
    fn parse_llama_models_empty_or_garbage() {
        assert!(parse_llama_models("not json").is_empty());
        assert!(parse_llama_models(r#"{"data":[]}"#).is_empty());
    }
}
