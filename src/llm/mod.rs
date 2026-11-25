//! 多模型 LLM 客户端：OpenAI 兼容 / Anthropic / Gemini / Mock。

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use regex::Regex;
use reqwest::Client;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tracing::{debug, warn};

use crate::config::{ProviderConfig, RouteConfig, RouteName, Settings};
use crate::models::Validate;
use crate::state::StateStore;

const RETRYABLE_STATUS: &[u16] = &[408, 409, 425, 429, 500, 502, 503, 504, 529];
static FENCE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)```(?:json)?\s*(.*?)```").unwrap());
static TAG_INNER_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/?tag_\d+/?$").unwrap());

#[derive(Debug, Clone)]
pub struct LlmResponse {
    pub text: String,
    pub provider: String,
    pub model: String,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cached_tokens: i64,
}

#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub route: RouteConfig,
    pub api_key: String,
    pub system: String,
    pub user: String,
    pub cache_system: bool,
    pub meta: Value,
}

#[async_trait]
pub trait Completer: Send + Sync {
    async fn complete(&self, request: &CompletionRequest) -> Result<LlmResponse>;
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

fn build_client(cfg: &ProviderConfig) -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs_f64(cfg.timeout_seconds.max(1.0)))
        .build()
        .context("创建 HTTP 客户端失败")
}

fn base_headers(cfg: &ProviderConfig) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (key, value) in &cfg.extra_headers {
        insert_header(&mut headers, key, value);
    }
    headers
}

fn insert_header(headers: &mut HeaderMap, name: &str, value: &str) {
    if let (Ok(name), Ok(value)) = (
        HeaderName::from_bytes(name.as_bytes()),
        HeaderValue::from_str(value),
    ) {
        headers.insert(name, value);
    }
}

async fn post_json(
    provider: &str,
    client: &Client,
    url: &str,
    headers: HeaderMap,
    query: &[(String, String)],
    body: &Value,
) -> Result<Value> {
    let mut last_error = String::new();
    for attempt in 0..4u32 {
        let mut builder = client.post(url).headers(headers.clone()).json(body);
        if !query.is_empty() {
            builder = builder.query(query);
        }

        match builder.send().await {
            Ok(response) => {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                if status.is_success() {
                    return serde_json::from_str(&text).with_context(|| {
                        format!("{provider} 返回非 JSON: {}", truncate(&text, 500))
                    });
                }
                if RETRYABLE_STATUS.contains(&status.as_u16()) {
                    warn!(
                        "{provider} 返回 {status}（第 {} 次），稍后重试: {}",
                        attempt + 1,
                        truncate(&text, 300)
                    );
                    sleep_backoff(attempt).await;
                    continue;
                }
                bail!("{provider} API 错误 {status}: {}", truncate(&text, 800));
            }
            Err(error) => {
                last_error = error.to_string();
                warn!("{provider} 网络错误（第 {} 次）: {error}", attempt + 1);
                sleep_backoff(attempt).await;
            }
        }
    }
    bail!("{provider} 请求最终失败: {last_error}")
}

async fn sleep_backoff(attempt: u32) {
    let seconds = (2u64.pow(attempt) * 2).min(30);
    tokio::time::sleep(Duration::from_secs(seconds)).await;
}

fn truncate(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

fn int_at(value: &Value, pointer: &str) -> i64 {
    value.pointer(pointer).and_then(Value::as_i64).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Providers
// ---------------------------------------------------------------------------

pub struct OpenAiCompleter {
    name: String,
    cfg: ProviderConfig,
    client: Client,
}

impl OpenAiCompleter {
    fn new(name: &str, cfg: ProviderConfig) -> Result<Self> {
        Ok(Self {
            name: name.to_string(),
            client: build_client(&cfg)?,
            cfg,
        })
    }
}

#[async_trait]
impl Completer for OpenAiCompleter {
    async fn complete(&self, request: &CompletionRequest) -> Result<LlmResponse> {
        if request.api_key.is_empty() {
            bail!(
                "缺少 {} 的 API Key（环境变量 {}）",
                self.name,
                self.cfg.api_key_env
            );
        }
        let base = if self.cfg.base_url.is_empty() {
            "https://api.openai.com/v1"
        } else {
            &self.cfg.base_url
        };
        let url = format!("{}/chat/completions", base.trim_end_matches('/'));
        let mut headers = base_headers(&self.cfg);
        insert_header(
            &mut headers,
            "authorization",
            &format!("Bearer {}", request.api_key),
        );
        let mut body = json!({
            "model": request.route.model,
            "messages": [
                {"role": "system", "content": request.system},
                {"role": "user", "content": request.user},
            ],
            "temperature": request.route.temperature,
            "max_tokens": request.route.max_output_tokens,
        });
        if self.cfg.json_mode {
            body["response_format"] = json!({"type": "json_object"});
        }

        let value = post_json(&self.name, &self.client, &url, headers, &[], &body).await?;
        let text = value
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if text.is_empty() {
            bail!(
                "{} 返回空内容: {}",
                self.name,
                truncate(&value.to_string(), 500)
            );
        }
        Ok(LlmResponse {
            text,
            provider: self.name.clone(),
            model: request.route.model.clone(),
            prompt_tokens: int_at(&value, "/usage/prompt_tokens"),
            completion_tokens: int_at(&value, "/usage/completion_tokens"),
            cached_tokens: int_at(&value, "/usage/prompt_tokens_details/cached_tokens"),
        })
    }
}

pub struct AnthropicCompleter {
    name: String,
    cfg: ProviderConfig,
    client: Client,
}

impl AnthropicCompleter {
    fn new(name: &str, cfg: ProviderConfig) -> Result<Self> {
        Ok(Self {
            name: name.to_string(),
            client: build_client(&cfg)?,
            cfg,
        })
    }
}

#[async_trait]
impl Completer for AnthropicCompleter {
    async fn complete(&self, request: &CompletionRequest) -> Result<LlmResponse> {
        if request.api_key.is_empty() {
            bail!(
                "缺少 {} 的 API Key（环境变量 {}）",
                self.name,
                self.cfg.api_key_env
            );
        }
        let base = if self.cfg.base_url.is_empty() {
            "https://api.anthropic.com"
        } else {
            &self.cfg.base_url
        };
        let url = format!("{}/v1/messages", base.trim_end_matches('/'));
        let mut headers = base_headers(&self.cfg);
        insert_header(&mut headers, "x-api-key", &request.api_key);
        insert_header(&mut headers, "anthropic-version", &self.cfg.api_version);
        insert_header(&mut headers, "content-type", "application/json");

        let mut system_block = json!({"type": "text", "text": request.system});
        if request.cache_system {
            system_block["cache_control"] = json!({"type": "ephemeral"});
        }
        let body = json!({
            "model": request.route.model,
            "system": [system_block],
            "messages": [{"role": "user", "content": request.user}],
            "temperature": request.route.temperature,
            "max_tokens": request.route.max_output_tokens,
        });

        let value = post_json(&self.name, &self.client, &url, headers, &[], &body).await?;
        let text = value
            .get("content")
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|block| block.get("text").and_then(Value::as_str))
                    .collect::<String>()
            })
            .unwrap_or_default();
        if text.is_empty() {
            bail!(
                "{} 返回空内容: {}",
                self.name,
                truncate(&value.to_string(), 500)
            );
        }
        Ok(LlmResponse {
            text,
            provider: self.name.clone(),
            model: request.route.model.clone(),
            prompt_tokens: int_at(&value, "/usage/input_tokens"),
            completion_tokens: int_at(&value, "/usage/output_tokens"),
            cached_tokens: int_at(&value, "/usage/cache_read_input_tokens"),
        })
    }
}

pub struct GeminiCompleter {
    name: String,
    cfg: ProviderConfig,
    client: Client,
}

impl GeminiCompleter {
    fn new(name: &str, cfg: ProviderConfig) -> Result<Self> {
        Ok(Self {
            name: name.to_string(),
            client: build_client(&cfg)?,
            cfg,
        })
    }
}

#[async_trait]
impl Completer for GeminiCompleter {
    async fn complete(&self, request: &CompletionRequest) -> Result<LlmResponse> {
        if request.api_key.is_empty() {
            bail!(
                "缺少 {} 的 API Key（环境变量 {}）",
                self.name,
                self.cfg.api_key_env
            );
        }
        let base = if self.cfg.base_url.is_empty() {
            "https://generativelanguage.googleapis.com/v1beta"
        } else {
            &self.cfg.base_url
        };
        let url = format!(
            "{}/models/{}:generateContent",
            base.trim_end_matches('/'),
            request.route.model
        );
        let body = json!({
            "systemInstruction": {"parts": [{"text": request.system}]},
            "contents": [{"role": "user", "parts": [{"text": request.user}]}],
            "generationConfig": {
                "temperature": request.route.temperature,
                "maxOutputTokens": request.route.max_output_tokens,
                "responseMimeType": "application/json",
            },
        });
        let query = vec![("key".to_string(), request.api_key.clone())];

        let value = post_json(
            &self.name,
            &self.client,
            &url,
            base_headers(&self.cfg),
            &query,
            &body,
        )
        .await?;
        let text = value
            .pointer("/candidates/0/content/parts")
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .collect::<String>()
            })
            .unwrap_or_default();
        if text.is_empty() {
            bail!(
                "{} 返回空内容: {}",
                self.name,
                truncate(&value.to_string(), 500)
            );
        }
        Ok(LlmResponse {
            text,
            provider: self.name.clone(),
            model: request.route.model.clone(),
            prompt_tokens: int_at(&value, "/usageMetadata/promptTokenCount"),
            completion_tokens: int_at(&value, "/usageMetadata/candidatesTokenCount"),
            cached_tokens: int_at(&value, "/usageMetadata/cachedContentTokenCount"),
        })
    }
}

pub struct MockCompleter;

#[async_trait]
impl Completer for MockCompleter {
    async fn complete(&self, request: &CompletionRequest) -> Result<LlmResponse> {
        let stage = request
            .meta
            .get("stage")
            .and_then(Value::as_str)
            .unwrap_or("");
        let source = request
            .meta
            .get("source_text")
            .and_then(Value::as_str)
            .unwrap_or("");
        let draft = request
            .meta
            .get("draft")
            .and_then(Value::as_str)
            .unwrap_or(source);
        let text = match stage {
            "bible" => json!({
                "global_style_guide": {
                    "narrative_tone": "（模拟模式）冷静克制的零度叙事，暗藏不动声色的反讽",
                    "free_indirect_discourse_rule": "自由间接引语与叙述者声音保持同一层次，不额外加引号",
                    "syntax_strategy": "拆解欧化长从句，以汉语短句与流水句重排节奏",
                    "honorifics_rules": "Monsieur→先生；Madame→夫人；Mademoiselle→小姐"
                },
                "core_characters": [],
                "glossary": []
            })
            .to_string(),
            "pass1" => json!({
                "syntax_and_nuance_notes": "（mock）代词指代与长句结构已简化",
                "faithful_draft": source
            })
            .to_string(),
            "pass2" => json!({
                "revision_thought": "（mock）按模板替换为占位中文",
                "literary_translation": mock_translate(draft)
            })
            .to_string(),
            "judge" => json!({
                "score": 5,
                "pass": true,
                "error_routing": "NONE",
                "issues": []
            })
            .to_string(),
            other => json!({"error": format!("mock 未实现的 stage: {other}")}).to_string(),
        };

        Ok(LlmResponse {
            prompt_tokens: (request.user.len() / 4) as i64,
            completion_tokens: (text.len() / 4) as i64,
            text,
            provider: "mock".into(),
            model: request.route.model.clone(),
            cached_tokens: 0,
        })
    }
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

pub struct LlmService {
    settings: Arc<Settings>,
    store: Arc<StateStore>,
    completers: HashMap<String, Arc<dyn Completer>>,
}

impl LlmService {
    pub fn new(settings: Arc<Settings>, store: Arc<StateStore>) -> Result<Self> {
        let mut completers: HashMap<String, Arc<dyn Completer>> = HashMap::new();
        for (name, cfg) in &settings.providers {
            completers.insert(name.clone(), build_completer(name, cfg)?);
        }
        completers
            .entry("mock".into())
            .or_insert_with(|| Arc::new(MockCompleter));
        Ok(Self {
            settings,
            store,
            completers,
        })
    }

    /// 供测试注入脚本化 Completer。
    pub fn with_override(
        settings: Arc<Settings>,
        store: Arc<StateStore>,
        provider: &str,
        completer: Arc<dyn Completer>,
    ) -> Result<Self> {
        let mut service = Self::new(settings, store)?;
        service.completers.insert(provider.to_string(), completer);
        Ok(service)
    }

    fn resolve(&self, route_name: RouteName) -> Result<(&RouteConfig, Arc<dyn Completer>, String)> {
        let route = self.settings.models.get(route_name);
        let completer = self
            .completers
            .get(&route.provider)
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "路由 {} 指向未注册的 provider {:?}，请检查 settings.yaml",
                    route_name.as_str(),
                    route.provider
                )
            })?;
        let config = self.settings.provider_config(&route.provider)?;
        let api_key = if config.provider_type == "mock" {
            String::new()
        } else {
            let key = self.settings.api_key(&route.provider)?;
            if key.is_empty() {
                bail!(
                    "缺少 {} 的 API Key：请设置环境变量 {} 或改用 --mock",
                    route.provider,
                    config.api_key_env
                );
            }
            key
        };
        Ok((route, completer, api_key))
    }

    pub fn validate_routes(&self) -> Result<()> {
        for route in RouteName::ALL {
            self.resolve(route)?;
        }
        Ok(())
    }

    pub async fn structured<T>(
        &self,
        route_name: RouteName,
        system: &str,
        user: &str,
        stage: &str,
        mut meta: Value,
        segment_id: Option<&str>,
    ) -> Result<T>
    where
        T: DeserializeOwned + Validate,
    {
        let (route, completer, api_key) = self.resolve(route_name)?;
        let route = route.clone();
        let mut prompt = user.to_string();
        if !meta.is_object() {
            meta = json!({});
        }
        meta["stage"] = json!(stage);
        if let Some(segment_id) = segment_id {
            meta["segment_id"] = json!(segment_id);
        }

        for attempt in 0..2 {
            let request = CompletionRequest {
                route: route.clone(),
                api_key: api_key.clone(),
                system: system.to_string(),
                user: prompt.clone(),
                cache_system: true,
                meta: meta.clone(),
            };
            let response = completer
                .complete(&request)
                .await
                .with_context(|| format!("{stage} 阶段调用模型失败"))?;
            self.record_cost(&route, &response, stage, segment_id);

            match parse_and_validate::<T>(&response.text) {
                Ok(value) => return Ok(value),
                Err(error) => {
                    if attempt == 0 {
                        warn!("[{stage}] JSON 校验失败，触发修复重试: {error:#}");
                        prompt = format!(
                            "{user}\n\n<previous_output>\n{}\n</previous_output>\n\n\
                             你上一次的输出无法通过 JSON Schema 校验：{error}\n\
                             请修正后只输出合法 JSON，不要输出任何解释或 Markdown 代码块。",
                            response.text
                        );
                        continue;
                    }
                    bail!("{stage} 结构化输出连续两次校验失败: {error:#}");
                }
            }
        }
        unreachable!("structured retry loop")
    }

    fn record_cost(
        &self,
        route: &RouteConfig,
        response: &LlmResponse,
        stage: &str,
        segment_id: Option<&str>,
    ) -> f64 {
        let mut cost = 0.0;
        if let Some(price) = self.settings.price_for(&route.provider, &route.model) {
            let cached = response.cached_tokens.min(response.prompt_tokens);
            let uncached = (response.prompt_tokens - cached).max(0);
            let cached_rate = price.cached_input_per_mtok.unwrap_or(price.input_per_mtok);
            cost = uncached as f64 / 1_000_000.0 * price.input_per_mtok
                + cached as f64 / 1_000_000.0 * cached_rate
                + response.completion_tokens as f64 / 1_000_000.0 * price.output_per_mtok;
        }
        if let Err(error) = self.store.add_cost(
            stage,
            &response.provider,
            &response.model,
            response.prompt_tokens,
            response.completion_tokens,
            response.cached_tokens,
            cost,
            segment_id,
        ) {
            warn!("写入成本台账失败（stage={stage}）: {error:#}");
        }
        debug!(
            "llm[{}] {stage} tokens={}+{} cached={} cost=${cost:.4}",
            route.model, response.prompt_tokens, response.completion_tokens, response.cached_tokens
        );
        cost
    }
}

fn build_completer(name: &str, cfg: &ProviderConfig) -> Result<Arc<dyn Completer>> {
    match cfg.provider_type.as_str() {
        "openai" => Ok(Arc::new(OpenAiCompleter::new(name, cfg.clone())?)),
        "anthropic" => Ok(Arc::new(AnthropicCompleter::new(name, cfg.clone())?)),
        "gemini" => Ok(Arc::new(GeminiCompleter::new(name, cfg.clone())?)),
        "mock" => Ok(Arc::new(MockCompleter)),
        other => bail!("未知 provider 类型: {other:?}（provider={name}）"),
    }
}

fn parse_and_validate<T: DeserializeOwned + Validate>(text: &str) -> Result<T> {
    let value = extract_json(text)?;
    let parsed: T = serde_json::from_value(value).context("JSON 结构与 Schema 不匹配")?;
    parsed
        .validate()
        .map_err(|error| anyhow::anyhow!("Schema 校验失败: {error}"))?;
    Ok(parsed)
}

pub fn extract_json(text: &str) -> Result<Value> {
    let trimmed = text.trim();
    let candidate = FENCE_RE
        .captures(trimmed)
        .and_then(|captures| captures.get(1))
        .map(|matched| matched.as_str().trim())
        .unwrap_or(trimmed);
    if let Ok(value) = serde_json::from_str(candidate) {
        return Ok(value);
    }

    let start = candidate
        .char_indices()
        .find(|(_, ch)| *ch == '{' || *ch == '[')
        .map(|(index, _)| index)
        .ok_or_else(|| anyhow::anyhow!("模型输出中找不到 JSON 对象"))?;

    let mut depth = 0i64;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, ch) in candidate[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth -= 1;
                if depth == 0 {
                    let end = start + offset + ch.len_utf8();
                    return serde_json::from_str(&candidate[start..end])
                        .context("模型输出中的 JSON 无法解析");
                }
            }
            _ => {}
        }
    }
    bail!("模型输出中的 JSON 括号不闭合")
}

pub fn mock_translate(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut index = 0usize;
    while index < text.len() {
        let ch = text[index..].chars().next().expect("char boundary");
        if ch == '[' {
            if let Some(rel_end) = text[index..].find(']') {
                let end = index + rel_end;
                let inner = &text[index + 1..end];
                if TAG_INNER_RE.is_match(inner) {
                    out.push_str(&text[index..=end]);
                    index = end + 1;
                    continue;
                }
            }
        }
        if ch.is_alphanumeric() || ch == '\'' || ch == '’' || ch == '-' {
            let mut cursor = index;
            while cursor < text.len() {
                let current = text[cursor..].chars().next().expect("char boundary");
                if current.is_alphanumeric() || current == '\'' || current == '’' || current == '-'
                {
                    cursor += current.len_utf8();
                } else {
                    break;
                }
            }
            out.push_str("文字");
            index = cursor;
        } else {
            out.push(ch);
            index += ch.len_utf8();
        }
    }
    out
}
