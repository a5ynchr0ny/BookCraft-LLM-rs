//! BookCraft-LLM Rust CLI。

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use sha2::{Digest, Sha256};
use tracing::Level;

use bookcraft::bible_builder::BibleBuilder;
use bookcraft::config::Settings;
use bookcraft::epub_builder::EpubBuilder;
use bookcraft::epub_parser::EpubParser;
use bookcraft::llm::{LlmService, error_is_billing};
use bookcraft::prompts::PromptLibrary;
use bookcraft::state::{StateStore, Stats};
use bookcraft::translator::{TranslationEngine, TranslationOptions};

#[derive(Parser)]
#[command(
    name = "bookcraft",
    version,
    about = "面向长篇文学作品的「直译 → 文学重塑 → 自动质检」多 Agent 精译管线"
)]
struct Cli {
    #[arg(long, global = true, help = "配置文件路径")]
    config: Option<PathBuf>,
    #[arg(long, global = true, help = "覆盖 settings.yaml 中的 workdir")]
    workdir: Option<String>,
    #[arg(long, global = true, help = "离线 mock 全链路，无需 API Key")]
    mock: bool,
    #[arg(short, long, global = true, help = "输出 DEBUG 日志")]
    verbose: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 解析 EPUB，切分自然段并建立状态库
    Prepare {
        epub: PathBuf,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        author: Option<String>,
        #[arg(long = "source-lang")]
        source_lang: Option<String>,
        #[arg(long = "target-lang")]
        target_lang: Option<String>,
        #[arg(long, help = "覆盖已有解析结果与翻译进度")]
        force: bool,
    },
    /// Step 1：全书扫描生成《翻译圣经》
    Bible {
        #[arg(long, help = "忽略已有圣经，重新生成")]
        force: bool,
    },
    /// Step 2/3：双步翻译 + QA 门禁
    Translate {
        #[arg(long, help = "仅处理指定章节，如 1-3,7")]
        chapters: Option<String>,
        #[arg(long, help = "本次最多处理多少段（压测用）")]
        limit: Option<usize>,
        #[arg(long = "retry-human", help = "连同 HUMAN_REVIEW 段落一起重试")]
        retry_human: bool,
    },
    /// Step 4：导出纯中文 / 双语 EPUB 与 Markdown
    Export,
    /// 查看进度、失败段落与成本
    Status,
    /// 一键执行 prepare → bible → translate → export
    Run {
        #[arg(help = "输入 EPUB（省略则复用状态库）")]
        epub: Option<PathBuf>,
        #[arg(long)]
        chapters: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long = "retry-human")]
        retry_human: bool,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        author: Option<String>,
        #[arg(long = "source-lang")]
        source_lang: Option<String>,
        #[arg(long = "target-lang")]
        target_lang: Option<String>,
        #[arg(long, help = "强制重新解析 EPUB")]
        force: bool,
    },
}

struct PrepareOptions {
    epub: PathBuf,
    title: Option<String>,
    author: Option<String>,
    source_lang: Option<String>,
    target_lang: Option<String>,
    force: bool,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run_cli().await {
        if error_is_billing(&error) {
            eprintln!("⛔ 计费/额度保护：{error:#}");
            std::process::exit(3);
        }
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}

async fn run_cli() -> Result<()> {
    let cli = Cli::parse();
    let _ = tracing_subscriber::fmt()
        .with_max_level(if cli.verbose {
            Level::DEBUG
        } else {
            Level::INFO
        })
        .with_target(false)
        .try_init();

    let mut settings = Settings::load(cli.config.as_deref()).context("加载配置失败")?;
    if let Some(workdir) = cli.workdir {
        settings.paths.workdir = workdir;
    }
    if cli.mock {
        settings.apply_mock();
    }
    let settings = Arc::new(settings);
    let store = Arc::new(StateStore::open(settings.db_path())?);

    match cli.command {
        Command::Prepare {
            epub,
            title,
            author,
            source_lang,
            target_lang,
            force,
        } => do_prepare(
            &settings,
            &store,
            PrepareOptions {
                epub,
                title,
                author,
                source_lang,
                target_lang,
                force,
            },
        ),
        Command::Bible { force } => {
            let prompts = Arc::new(PromptLibrary::new(&settings.prompts_dir)?);
            let llm = Arc::new(LlmService::new(settings.clone(), store.clone())?);
            do_bible(&settings, &store, &prompts, &llm, force).await
        }
        Command::Translate {
            chapters,
            limit,
            retry_human,
        } => {
            let prompts = Arc::new(PromptLibrary::new(&settings.prompts_dir)?);
            let llm = Arc::new(LlmService::new(settings.clone(), store.clone())?);
            do_translate(
                &settings,
                &store,
                &prompts,
                &llm,
                parse_chapters(chapters.as_deref()),
                limit,
                retry_human,
            )
            .await
        }
        Command::Export => do_export(&settings, &store),
        Command::Status => do_status(&store),
        Command::Run {
            epub,
            chapters,
            limit,
            retry_human,
            title,
            author,
            source_lang,
            target_lang,
            force,
        } => {
            do_run(
                &settings,
                &store,
                epub,
                PrepareOptions {
                    epub: PathBuf::new(),
                    title,
                    author,
                    source_lang,
                    target_lang,
                    force,
                },
                parse_chapters(chapters.as_deref()),
                limit,
                retry_human,
            )
            .await
        }
    }
}

fn do_prepare(settings: &Settings, store: &StateStore, options: PrepareOptions) -> Result<()> {
    if store.has_content()? && !options.force {
        bail!("状态库已有解析内容。若确认重新解析（会清空翻译进度），请加 --force");
    }
    let epub_path = options.epub;
    let title = options
        .title
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| settings.book.title.clone());
    let author = options
        .author
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| settings.book.author.clone());
    let source_lang = options
        .source_lang
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| settings.book.source_lang.clone());
    let target_lang = options
        .target_lang
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| settings.book.target_lang.clone());

    let parser = EpubParser::new(
        &epub_path,
        settings.templates_dir(),
        &title,
        &author,
        &source_lang,
        &target_lang,
    );
    let result = parser.parse()?;

    let old = store.load_book_meta()?;
    let same_book = old
        .as_ref()
        .map(|meta| meta.source_sha256 == result.meta.source_sha256)
        .unwrap_or(false);
    store.reset_all(same_book)?;
    store.save_book_meta(&result.meta)?;
    store.replace_content(&result.chapters, &result.segments)?;

    let dialogue_count = result
        .segments
        .iter()
        .filter(|segment| segment.is_dialogue)
        .count();
    println!("✔ 解析完成：{}", result.meta.title);
    println!(
        "  作者       : {}",
        if result.meta.author.is_empty() {
            "（未知）"
        } else {
            &result.meta.author
        }
    );
    println!(
        "  语言       : {} → {}",
        result.meta.source_lang, result.meta.target_lang
    );
    println!(
        "  章节 / 段落: {} / {}（对白段 {}）",
        result.chapters.len(),
        result.segments.len(),
        dialogue_count
    );
    println!("  模板目录   : {}", settings.templates_dir().display());
    println!("  状态库     : {}", settings.db_path().display());
    println!("  下一步     : bookcraft bible");
    Ok(())
}

async fn do_bible(
    settings: &Arc<Settings>,
    store: &Arc<StateStore>,
    prompts: &Arc<PromptLibrary>,
    llm: &Arc<LlmService>,
    force: bool,
) -> Result<()> {
    if store.load_book_meta()?.is_none() {
        bail!("尚未解析书籍，请先运行：bookcraft prepare <book.epub>");
    }
    llm.preflight_billing().await?;
    let builder = BibleBuilder::new(
        settings.clone(),
        store.clone(),
        llm.clone(),
        prompts.clone(),
    );
    let bible = builder.build(force).await?;
    println!("✔ 《翻译圣经》就绪");
    println!("  核心角色: {}", bible.core_characters.len());
    for character in bible.core_characters.iter().take(10) {
        println!(
            "    - {} → {}",
            character.source_name, character.target_name
        );
    }
    println!("  术语条目: {}", bible.glossary.len());
    for entry in bible.glossary.iter().take(10) {
        println!(
            "    - {} → {} [{}]",
            entry.source_term, entry.target_term, entry.category
        );
    }
    println!("  文件    : {}", settings.bible_path().display());
    println!("  下一步  : bookcraft translate");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn do_translate(
    settings: &Arc<Settings>,
    store: &Arc<StateStore>,
    prompts: &Arc<PromptLibrary>,
    llm: &Arc<LlmService>,
    chapters: Option<Vec<String>>,
    limit: Option<usize>,
    retry_human: bool,
) -> Result<()> {
    llm.preflight_billing().await?;
    let bible = store
        .load_bible()?
        .ok_or_else(|| anyhow::anyhow!("尚未生成《翻译圣经》，请先运行：bookcraft bible"))?;
    let engine = TranslationEngine::new(
        settings.clone(),
        store.clone(),
        llm.clone(),
        prompts.clone(),
        bible,
    )?;
    let stats = engine
        .run(TranslationOptions {
            chapter_ids: chapters,
            limit,
            retry_human,
        })
        .await?;
    print_stats(&stats);
    println!("  下一步: bookcraft export");
    Ok(())
}

fn do_export(settings: &Arc<Settings>, store: &Arc<StateStore>) -> Result<()> {
    let builder = EpubBuilder::new(settings.clone(), store.clone());
    let paths = builder.export_all()?;
    let labels = [
        ("zh_epub", "纯中文 EPUB"),
        ("bilingual_epub", "双语对照 EPUB"),
        ("zh_md", "纯中文 Markdown"),
        ("bilingual_md", "双语 Markdown"),
        ("qa_report", "QA 报告"),
    ];
    println!("✔ 导出完成");
    for (key, label) in labels {
        if let Some(path) = paths.get(key) {
            println!("  {label:<16}: {}", path.display());
        }
    }
    Ok(())
}

fn do_status(store: &Arc<StateStore>) -> Result<()> {
    let meta = match store.load_book_meta()? {
        Some(meta) => meta,
        None => {
            println!("状态库为空。先运行：bookcraft prepare <book.epub>");
            return Ok(());
        }
    };
    let stats = store.stats()?;
    println!("书名    : {}", meta.title);
    println!(
        "作者    : {}",
        if meta.author.is_empty() {
            "（未知）"
        } else {
            &meta.author
        }
    );
    println!("语言    : {} → {}", meta.source_lang, meta.target_lang);
    println!("EPUB    : {}", meta.source_path);
    println!("章节/段 : {} / {}", stats.chapters, stats.segments);
    let states = stats
        .states
        .iter()
        .map(|(state, count)| format!("{state}={count}"))
        .collect::<Vec<_>>()
        .join(" / ");
    println!(
        "状态    : {}",
        if states.is_empty() {
            "（空）"
        } else {
            &states
        }
    );
    println!(
        "成本    : ${:.4}（输入 {} / 输出 {} tokens）",
        stats.cost_usd, stats.prompt_tokens, stats.completion_tokens
    );
    match store.load_bible()? {
        Some(bible) => println!(
            "圣经    : {} 角色 / {} 术语",
            bible.core_characters.len(),
            bible.glossary.len()
        ),
        None => println!("圣经    : 未生成"),
    }

    println!("\n章节进度：");
    for row in store.chapter_progress()? {
        println!(
            "  {}  {:<36} {:>4}/{} 通过  {:>3} 失败  {:>3} 待复核",
            row.id,
            truncate_display(&row.title, 34),
            row.passed,
            row.total,
            row.failed,
            row.human
        );
    }

    let failed = store.failed_segments()?;
    if !failed.is_empty() {
        println!("\n需人工复核（{} 段，展示前 15 段）：", failed.len());
        for segment in failed.iter().take(15) {
            let reason = segment
                .last_error
                .clone()
                .unwrap_or_default()
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(100)
                .collect::<String>();
            println!("  {} [{}] {}", segment.id, segment.state.as_str(), reason);
        }
        println!("  报告: bookcraft export 会同步生成 qa_report.md");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn do_run(
    settings: &Arc<Settings>,
    store: &Arc<StateStore>,
    epub: Option<PathBuf>,
    prepare: PrepareOptions,
    chapters: Option<Vec<String>>,
    limit: Option<usize>,
    retry_human: bool,
) -> Result<()> {
    if let Some(epub) = epub {
        let existing = store.load_book_meta()?;
        let same_book = match &existing {
            Some(meta) => meta.source_sha256 == sha256_hex(&epub)?,
            None => false,
        };
        if prepare.force || !same_book {
            do_prepare(
                settings,
                store,
                PrepareOptions {
                    epub,
                    title: prepare.title,
                    author: prepare.author,
                    source_lang: prepare.source_lang,
                    target_lang: prepare.target_lang,
                    force: true,
                },
            )?;
        } else if let Some(meta) = existing {
            println!(
                "↻ 复用已有解析：{}（{} 段）",
                meta.title, meta.segment_count
            );
        }
    }

    if store.load_book_meta()?.is_none() {
        bail!("状态库为空：请提供 EPUB，或先运行 bookcraft prepare");
    }

    let prompts = Arc::new(PromptLibrary::new(&settings.prompts_dir)?);
    let llm = Arc::new(LlmService::new(settings.clone(), store.clone())?);
    if store.load_bible()?.is_none() {
        do_bible(settings, store, &prompts, &llm, false).await?;
    } else {
        println!("↻ 复用已有《翻译圣经》");
    }
    do_translate(
        settings,
        store,
        &prompts,
        &llm,
        chapters,
        limit,
        retry_human,
    )
    .await?;
    do_export(settings, store)
}

fn parse_chapters(spec: Option<&str>) -> Option<Vec<String>> {
    let spec = spec?;
    let mut items = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((start, end)) = part.split_once('-') {
            if let (Ok(start), Ok(end)) = (start.trim().parse::<i32>(), end.trim().parse::<i32>()) {
                if end >= start && end - start < 1000 {
                    for value in start..=end {
                        items.push(value.to_string());
                    }
                    continue;
                }
            }
        }
        items.push(part.to_string());
    }
    if items.is_empty() { None } else { Some(items) }
}

fn sha256_hex(path: &PathBuf) -> Result<String> {
    let bytes = std::fs::read(path)?;
    let digest = Sha256::digest(&bytes);
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn print_stats(stats: &Stats) {
    println!("✔ 本轮翻译结束");
    for (state, count) in &stats.states {
        println!("  {state:<12}: {count}");
    }
    println!(
        "  Token       : 输入 {} / 输出 {} / 缓存命中 {}",
        stats.prompt_tokens, stats.completion_tokens, stats.cached_tokens
    );
    println!("  累计成本    : ${:.4}", stats.cost_usd);
}

fn truncate_display(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}
