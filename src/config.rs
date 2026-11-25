//! 配置加载与运行时路径。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

pub const DEFAULT_CONFIG_RELATIVE: &str = "config/settings.yaml";

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct BookDefaults {
    pub source_lang: String,
    pub target_lang: String,
    pub title: String,
    pub author: String,
}

impl Default for BookDefaults {
    fn default() -> Self {
        Self {
            source_lang: "fr".into(),
            target_lang: "zh".into(),
            title: String::new(),
            author: String::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PathsConfig {
    pub workdir: String,
}

impl Default for PathsConfig {
    fn default() -> Self {
        Self {
            workdir: "work".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    #[serde(rename = "type")]
    pub provider_type: String,
    pub base_url: String,
    pub api_key_env: String,
    pub api_version: String,
    pub timeout_seconds: f64,
    pub json_mode: bool,
    pub extra_headers: HashMap<String, String>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            provider_type: "mock".into(),
            base_url: String::new(),
            api_key_env: String::new(),
            api_version: "2023-06-01".into(),
            timeout_seconds: 180.0,
            json_mode: true,
            extra_headers: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RouteConfig {
    pub provider: String,
    pub model: String,
    #[serde(default = "default_temperature")]
    pub temperature: f64,
    #[serde(default = "default_max_tokens")]
    pub max_output_tokens: u32,
}

fn default_temperature() -> f64 {
    0.2
}

fn default_max_tokens() -> u32 {
    8192
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteName {
    Bible,
    Pass1,
    Pass2,
    Judge,
}

impl RouteName {
    pub const ALL: [RouteName; 4] = [
        RouteName::Bible,
        RouteName::Pass1,
        RouteName::Pass2,
        RouteName::Judge,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bible => "bible",
            Self::Pass1 => "pass1",
            Self::Pass2 => "pass2",
            Self::Judge => "judge",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelsConfig {
    pub bible: RouteConfig,
    pub pass1: RouteConfig,
    pub pass2: RouteConfig,
    pub judge: RouteConfig,
}

impl ModelsConfig {
    pub fn get(&self, name: RouteName) -> &RouteConfig {
        match name {
            RouteName::Bible => &self.bible,
            RouteName::Pass1 => &self.pass1,
            RouteName::Pass2 => &self.pass2,
            RouteName::Judge => &self.judge,
        }
    }

    pub fn get_mut(&mut self, name: RouteName) -> &mut RouteConfig {
        match name {
            RouteName::Bible => &mut self.bible,
            RouteName::Pass1 => &mut self.pass1,
            RouteName::Pass2 => &mut self.pass2,
            RouteName::Judge => &mut self.judge,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ConcurrencyConfig {
    pub chapter_workers: usize,
    pub core_characters: usize,
    pub bible_max_input_chars: usize,
}

impl Default for ConcurrencyConfig {
    fn default() -> Self {
        Self {
            chapter_workers: 8,
            core_characters: 10,
            bible_max_input_chars: 1_200_000,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LengthRatioConfig {
    pub min: f64,
    pub max: f64,
    pub dialogue_min: f64,
    pub dialogue_max: f64,
}

impl Default for LengthRatioConfig {
    fn default() -> Self {
        Self {
            min: 1.1,
            max: 2.4,
            dialogue_min: 0.7,
            dialogue_max: 3.5,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct QaConfig {
    pub max_retry: usize,
    pub judge_pass_score: i32,
    pub length_ratio: LengthRatioConfig,
}

impl Default for QaConfig {
    fn default() -> Self {
        Self {
            max_retry: 2,
            judge_pass_score: 4,
            length_ratio: LengthRatioConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct PriceConfig {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    pub cached_input_per_mtok: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub book: BookDefaults,
    #[serde(default)]
    pub paths: PathsConfig,
    #[serde(default)]
    pub concurrency: ConcurrencyConfig,
    #[serde(default)]
    pub qa: QaConfig,
    pub models: ModelsConfig,
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub pricing: HashMap<String, PriceConfig>,
    #[serde(skip, default)]
    pub base_dir: PathBuf,
    #[serde(skip, default)]
    pub prompts_dir: PathBuf,
}

impl Settings {
    pub fn load(config_path: Option<&Path>) -> Result<Self> {
        let path = match config_path {
            Some(path) => {
                if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    std::env::current_dir()
                        .context("读取当前目录失败")?
                        .join(path)
                }
            }
            None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(DEFAULT_CONFIG_RELATIVE),
        };
        if !path.exists() {
            anyhow::bail!("配置文件不存在: {}", path.display());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("读取配置文件失败: {}", path.display()))?;
        let mut settings: Settings = serde_yaml::from_str(&text)
            .with_context(|| format!("解析 YAML 失败: {}", path.display()))?;

        let config_dir = path.parent().unwrap_or(Path::new("."));
        settings.base_dir = config_dir.parent().unwrap_or(Path::new(".")).to_path_buf();
        settings.prompts_dir = config_dir.join("prompts");
        load_dotenv(&settings.base_dir);
        Ok(settings)
    }

    pub fn apply_mock(&mut self) {
        for name in RouteName::ALL {
            let route = self.models.get_mut(name);
            route.provider = "mock".into();
            route.model = "mock-echo".into();
        }
    }

    pub fn provider_config(&self, provider_name: &str) -> Result<&ProviderConfig> {
        self.providers.get(provider_name).ok_or_else(|| {
            anyhow::anyhow!(
                "模型路由引用了未定义的 provider: {provider_name:?}，请检查 settings.yaml"
            )
        })
    }

    pub fn api_key(&self, provider_name: &str) -> Result<String> {
        let cfg = self.provider_config(provider_name)?;
        if cfg.api_key_env.is_empty() {
            return Ok(String::new());
        }
        Ok(std::env::var(&cfg.api_key_env).unwrap_or_default())
    }

    pub fn price_for(&self, provider: &str, model: &str) -> Option<&PriceConfig> {
        self.pricing.get(&format!("{provider}/{model}"))
    }

    pub fn workdir_path(&self) -> PathBuf {
        let raw = expand_home(Path::new(&self.paths.workdir));
        if raw.is_absolute() {
            raw
        } else {
            self.base_dir.join(raw)
        }
    }

    pub fn db_path(&self) -> PathBuf {
        self.workdir_path().join("state.db")
    }

    pub fn templates_dir(&self) -> PathBuf {
        self.workdir_path().join("templates")
    }

    pub fn output_dir(&self) -> PathBuf {
        self.workdir_path().join("output")
    }

    pub fn bible_path(&self) -> PathBuf {
        self.workdir_path().join("bible.json")
    }
}

fn expand_home(path: &Path) -> PathBuf {
    if let Ok(stripped) = path.strip_prefix("~") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(stripped);
        }
    }
    path.to_path_buf()
}

fn load_dotenv(base_dir: &Path) {
    let mut candidates = vec![base_dir.join(".env")];
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join(".env"));
    }
    for candidate in candidates {
        if candidate.is_file() {
            let _ = dotenvy::from_path(&candidate);
        }
    }
}
