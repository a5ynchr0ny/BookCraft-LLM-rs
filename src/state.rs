//! SQLite 状态机：段落生命周期、QA 记录、Token 成本台账。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::models::{BookMeta, ErrorRouting, SegmentState, TranslationBible};
use crate::tags::TagMap;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS chapters (
    id            TEXT PRIMARY KEY,
    order_index   INTEGER NOT NULL,
    doc_name      TEXT NOT NULL,
    title         TEXT NOT NULL DEFAULT '',
    template_path TEXT NOT NULL,
    status        TEXT NOT NULL DEFAULT 'PENDING'
);

CREATE TABLE IF NOT EXISTS segments (
    id            TEXT PRIMARY KEY,
    chapter_id    TEXT NOT NULL REFERENCES chapters(id) ON DELETE CASCADE,
    order_index   INTEGER NOT NULL,
    source_text   TEXT NOT NULL,
    source_html   TEXT NOT NULL,
    tag_map       TEXT NOT NULL DEFAULT '{}',
    source_words  INTEGER NOT NULL DEFAULT 0,
    is_dialogue   INTEGER NOT NULL DEFAULT 0,
    state         TEXT NOT NULL DEFAULT 'PENDING',
    pass1_notes   TEXT,
    pass1_draft   TEXT,
    final_text    TEXT,
    best_text     TEXT,
    qa_score      REAL,
    last_routing  TEXT,
    last_error    TEXT,
    retry_count   INTEGER NOT NULL DEFAULT 0,
    updated_at    TEXT NOT NULL,
    UNIQUE(chapter_id, order_index)
);

CREATE INDEX IF NOT EXISTS idx_segments_chapter ON segments(chapter_id, order_index);
CREATE INDEX IF NOT EXISTS idx_segments_state ON segments(state);

CREATE TABLE IF NOT EXISTS qa_reports (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    segment_id  TEXT NOT NULL,
    attempt     INTEGER NOT NULL,
    passed      INTEGER NOT NULL,
    score       REAL,
    routing     TEXT NOT NULL DEFAULT 'NONE',
    issues      TEXT NOT NULL DEFAULT '[]',
    created_at  TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_qa_segment ON qa_reports(segment_id, attempt);

CREATE TABLE IF NOT EXISTS cost_ledger (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    segment_id        TEXT,
    stage             TEXT NOT NULL,
    provider          TEXT NOT NULL,
    model             TEXT NOT NULL,
    prompt_tokens     INTEGER NOT NULL DEFAULT 0,
    cached_tokens     INTEGER NOT NULL DEFAULT 0,
    completion_tokens INTEGER NOT NULL DEFAULT 0,
    cost_usd          REAL NOT NULL DEFAULT 0,
    created_at        TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_cost_stage ON cost_ledger(stage);
"#;

#[derive(Debug, Clone)]
pub struct ChapterRecord {
    pub id: String,
    pub order_index: i64,
    pub doc_name: String,
    pub title: String,
    pub template_path: String,
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct SegmentRecord {
    pub id: String,
    pub chapter_id: String,
    pub order_index: i64,
    pub source_text: String,
    pub source_html: String,
    pub tag_map: TagMap,
    pub source_words: i64,
    pub is_dialogue: bool,
    pub state: SegmentState,
    pub pass1_notes: Option<String>,
    pub pass1_draft: Option<String>,
    pub final_text: Option<String>,
    pub best_text: Option<String>,
    pub qa_score: Option<f64>,
    pub last_routing: Option<String>,
    pub last_error: Option<String>,
    pub retry_count: i64,
    pub updated_at: String,
}

impl SegmentRecord {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let tag_map_json: String = row.get("tag_map")?;
        let tag_map: TagMap = serde_json::from_str(&tag_map_json).unwrap_or_default();
        let state_str: String = row.get("state")?;
        let state = state_str
            .parse::<SegmentState>()
            .unwrap_or(SegmentState::Pending);
        Ok(Self {
            id: row.get("id")?,
            chapter_id: row.get("chapter_id")?,
            order_index: row.get("order_index")?,
            source_text: row.get("source_text")?,
            source_html: row.get("source_html")?,
            tag_map,
            source_words: row.get("source_words")?,
            is_dialogue: row.get::<_, i64>("is_dialogue")? != 0,
            state,
            pass1_notes: row.get("pass1_notes")?,
            pass1_draft: row.get("pass1_draft")?,
            final_text: row.get("final_text")?,
            best_text: row.get("best_text")?,
            qa_score: row.get("qa_score")?,
            last_routing: row.get("last_routing")?,
            last_error: row.get("last_error")?,
            retry_count: row.get("retry_count")?,
            updated_at: row.get("updated_at")?,
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct StageCost {
    pub stage: String,
    pub calls: i64,
    pub cost_usd: f64,
}

#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub chapters: i64,
    pub segments: i64,
    pub states: BTreeMap<String, i64>,
    pub cost_usd: f64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cached_tokens: i64,
    pub by_stage: Vec<StageCost>,
}

#[derive(Debug, Clone, Default)]
pub struct ChapterProgress {
    pub id: String,
    pub title: String,
    pub order_index: i64,
    pub total: i64,
    pub passed: i64,
    pub failed: i64,
    pub human: i64,
    pub pending: i64,
}

pub struct StateStore {
    #[allow(dead_code)]
    path: PathBuf,
    conn: Mutex<Connection>,
}

impl StateStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&path)
            .with_context(|| format!("打开状态库失败: {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let store = Self {
            path,
            conn: Mutex::new(conn),
        };
        store.initialize()?;
        Ok(store)
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn initialize(&self) -> Result<()> {
        self.conn().execute_batch(SCHEMA)?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Meta
    // ------------------------------------------------------------------
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO meta(key, value) VALUES(?, ?) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let value = self
            .conn()
            .query_row("SELECT value FROM meta WHERE key=?", params![key], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(value)
    }

    pub fn save_book_meta(&self, meta: &BookMeta) -> Result<()> {
        self.set_meta("book_meta", &serde_json::to_string(meta)?)
    }

    pub fn load_book_meta(&self) -> Result<Option<BookMeta>> {
        match self.get_meta("book_meta")? {
            Some(raw) => Ok(Some(serde_json::from_str(&raw)?)),
            None => Ok(None),
        }
    }

    pub fn save_bible(&self, bible: &TranslationBible) -> Result<()> {
        self.set_meta("bible", &serde_json::to_string(bible)?)
    }

    pub fn load_bible(&self) -> Result<Option<TranslationBible>> {
        match self.get_meta("bible")? {
            Some(raw) => Ok(Some(serde_json::from_str(&raw)?)),
            None => Ok(None),
        }
    }

    // ------------------------------------------------------------------
    // Content
    // ------------------------------------------------------------------
    pub fn has_content(&self) -> Result<bool> {
        let count: i64 = self
            .conn()
            .query_row("SELECT COUNT(*) FROM segments", [], |row| row.get(0))?;
        Ok(count > 0)
    }

    pub fn reset_all(&self, keep_bible: bool) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM qa_reports", [])?;
        tx.execute("DELETE FROM cost_ledger", [])?;
        tx.execute("DELETE FROM segments", [])?;
        tx.execute("DELETE FROM chapters", [])?;
        if keep_bible {
            tx.execute("DELETE FROM meta WHERE key != 'bible'", [])?;
        } else {
            tx.execute("DELETE FROM meta", [])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn replace_content(
        &self,
        chapters: &[ChapterRecord],
        segments: &[SegmentRecord],
    ) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM qa_reports", [])?;
        tx.execute("DELETE FROM segments", [])?;
        tx.execute("DELETE FROM chapters", [])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO chapters(id, order_index, doc_name, title, template_path, status) \
                 VALUES(?, ?, ?, ?, ?, ?)",
            )?;
            for chapter in chapters {
                stmt.execute(params![
                    chapter.id,
                    chapter.order_index,
                    chapter.doc_name,
                    chapter.title,
                    chapter.template_path,
                    chapter.status
                ])?;
            }
        }
        {
            let mut stmt = tx.prepare(
                "INSERT INTO segments(
                    id, chapter_id, order_index, source_text, source_html, tag_map,
                    source_words, is_dialogue, state, updated_at
                ) VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%SZ','now'))",
            )?;
            for segment in segments {
                stmt.execute(params![
                    segment.id,
                    segment.chapter_id,
                    segment.order_index,
                    segment.source_text,
                    segment.source_html,
                    serde_json::to_string(&segment.tag_map)?,
                    segment.source_words,
                    if segment.is_dialogue { 1 } else { 0 },
                    SegmentState::Pending.as_str(),
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn list_chapters(&self) -> Result<Vec<ChapterRecord>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, order_index, doc_name, title, template_path, status \
             FROM chapters ORDER BY order_index",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ChapterRecord {
                id: row.get(0)?,
                order_index: row.get(1)?,
                doc_name: row.get(2)?,
                title: row.get(3)?,
                template_path: row.get(4)?,
                status: row.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn get_chapter(&self, chapter_id: &str) -> Result<Option<ChapterRecord>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT id, order_index, doc_name, title, template_path, status \
                 FROM chapters WHERE id=?",
                params![chapter_id],
                |row| {
                    Ok(ChapterRecord {
                        id: row.get(0)?,
                        order_index: row.get(1)?,
                        doc_name: row.get(2)?,
                        title: row.get(3)?,
                        template_path: row.get(4)?,
                        status: row.get(5)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn list_segments(&self, chapter_id: Option<&str>) -> Result<Vec<SegmentRecord>> {
        let conn = self.conn();
        let mut segments = Vec::new();
        if let Some(chapter_id) = chapter_id {
            let mut stmt =
                conn.prepare("SELECT * FROM segments WHERE chapter_id=? ORDER BY order_index")?;
            let rows = stmt.query_map(params![chapter_id], SegmentRecord::from_row)?;
            for row in rows {
                segments.push(row?);
            }
        } else {
            let mut stmt =
                conn.prepare("SELECT * FROM segments ORDER BY chapter_id, order_index")?;
            let rows = stmt.query_map([], SegmentRecord::from_row)?;
            for row in rows {
                segments.push(row?);
            }
        }
        Ok(segments)
    }

    pub fn get_segment(&self, segment_id: &str) -> Result<Option<SegmentRecord>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT * FROM segments WHERE id=?",
                params![segment_id],
                SegmentRecord::from_row,
            )
            .optional()?)
    }

    // ------------------------------------------------------------------
    // State transitions
    // ------------------------------------------------------------------
    fn current_state(&self, segment_id: &str) -> Result<SegmentState> {
        let raw: String = self
            .conn()
            .query_row(
                "SELECT state FROM segments WHERE id=?",
                params![segment_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| anyhow::anyhow!("段落不存在: {segment_id}"))?;
        Ok(raw.parse().unwrap_or(SegmentState::Pending))
    }

    fn ensure_transition(&self, segment_id: &str, target: SegmentState) -> Result<()> {
        let current = self.current_state(segment_id)?;
        if current == target || allowed_transition(current, target) {
            return Ok(());
        }
        bail!(
            "非法状态迁移 {}: {} → {}",
            segment_id,
            current.as_str(),
            target.as_str()
        )
    }

    pub fn mark_drafted(
        &self,
        segment_id: &str,
        notes: &str,
        draft: &str,
    ) -> Result<SegmentRecord> {
        self.ensure_transition(segment_id, SegmentState::Drafted)?;
        self.conn().execute(
            "UPDATE segments SET state='DRAFTED', pass1_notes=?, pass1_draft=?, \
             updated_at=strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE id=?",
            params![notes, draft, segment_id],
        )?;
        self.get_segment(segment_id)?
            .ok_or_else(|| anyhow::anyhow!("段落不存在: {segment_id}"))
    }

    pub fn mark_polished(&self, segment_id: &str, translation: &str) -> Result<SegmentRecord> {
        self.ensure_transition(segment_id, SegmentState::Polished)?;
        self.conn().execute(
            "UPDATE segments SET state='POLISHED', final_text=?, \
             updated_at=strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE id=?",
            params![translation, segment_id],
        )?;
        self.get_segment(segment_id)?
            .ok_or_else(|| anyhow::anyhow!("段落不存在: {segment_id}"))
    }

    pub fn mark_qa_passed(
        &self,
        segment_id: &str,
        translation: &str,
        score: Option<f64>,
        retry_count: i64,
    ) -> Result<SegmentRecord> {
        self.ensure_transition(segment_id, SegmentState::QaPassed)?;
        self.conn().execute(
            "UPDATE segments SET state='QA_PASSED', final_text=?, best_text=?, qa_score=?, \
             last_routing='NONE', last_error=NULL, retry_count=?, \
             updated_at=strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE id=?",
            params![translation, translation, score, retry_count, segment_id],
        )?;
        self.get_segment(segment_id)?
            .ok_or_else(|| anyhow::anyhow!("段落不存在: {segment_id}"))
    }

    pub fn mark_qa_failed(
        &self,
        segment_id: &str,
        final_text: &str,
        score: Option<f64>,
        routing: ErrorRouting,
        error: &str,
        retry_count: i64,
    ) -> Result<SegmentRecord> {
        self.ensure_transition(segment_id, SegmentState::QaFailed)?;
        self.conn().execute(
            "UPDATE segments SET state='QA_FAILED', final_text=?, best_text=?, qa_score=?, \
             last_routing=?, last_error=?, retry_count=?, \
             updated_at=strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE id=?",
            params![
                final_text,
                final_text,
                score,
                routing.as_str(),
                error,
                retry_count,
                segment_id
            ],
        )?;
        self.get_segment(segment_id)?
            .ok_or_else(|| anyhow::anyhow!("段落不存在: {segment_id}"))
    }

    pub fn mark_human(&self, segment_id: &str, text: Option<&str>, error: &str) -> Result<()> {
        self.ensure_transition(segment_id, SegmentState::HumanReview)?;
        self.conn().execute(
            "UPDATE segments SET state='HUMAN_REVIEW', final_text=?, last_error=?, \
             updated_at=strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE id=?",
            params![text, error, segment_id],
        )?;
        Ok(())
    }

    pub fn record_qa(
        &self,
        segment_id: &str,
        attempt: i64,
        passed: bool,
        score: Option<f64>,
        routing: ErrorRouting,
        issues_json: &str,
    ) -> Result<()> {
        self.conn().execute(
            "INSERT INTO qa_reports(segment_id, attempt, passed, score, routing, issues, created_at) \
             VALUES(?, ?, ?, ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%SZ','now'))",
            params![
                segment_id,
                attempt,
                if passed { 1 } else { 0 },
                score,
                routing.as_str(),
                issues_json
            ],
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_cost(
        &self,
        stage: &str,
        provider: &str,
        model: &str,
        prompt_tokens: i64,
        completion_tokens: i64,
        cached_tokens: i64,
        cost_usd: f64,
        segment_id: Option<&str>,
    ) -> Result<()> {
        self.conn().execute(
            "INSERT INTO cost_ledger(
                segment_id, stage, provider, model, prompt_tokens,
                cached_tokens, completion_tokens, cost_usd, created_at
             ) VALUES(?, ?, ?, ?, ?, ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%SZ','now'))",
            params![
                segment_id,
                stage,
                provider,
                model,
                prompt_tokens,
                cached_tokens,
                completion_tokens,
                cost_usd
            ],
        )?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Reports
    // ------------------------------------------------------------------
    pub fn stats(&self) -> Result<Stats> {
        let conn = self.conn();
        let mut stats = Stats {
            chapters: conn.query_row("SELECT COUNT(*) FROM chapters", [], |row| row.get(0))?,
            ..Default::default()
        };

        let mut stmt = conn.prepare("SELECT state, COUNT(*) FROM segments GROUP BY state")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (state, count) = row?;
            stats.segments += count;
            stats.states.insert(state, count);
        }

        let (cost, prompt, completion, cached) = conn.query_row(
            "SELECT COALESCE(SUM(cost_usd),0), COALESCE(SUM(prompt_tokens),0), \
                    COALESCE(SUM(completion_tokens),0), COALESCE(SUM(cached_tokens),0) \
             FROM cost_ledger",
            [],
            |row| {
                Ok((
                    row.get::<_, f64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )?;
        stats.cost_usd = (cost * 10_000.0).round() / 10_000.0;
        stats.prompt_tokens = prompt;
        stats.completion_tokens = completion;
        stats.cached_tokens = cached;

        let mut stmt = conn.prepare(
            "SELECT stage, COUNT(*), COALESCE(SUM(cost_usd),0) \
             FROM cost_ledger GROUP BY stage ORDER BY stage",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(StageCost {
                stage: row.get(0)?,
                calls: row.get(1)?,
                cost_usd: row.get(2)?,
            })
        })?;
        stats.by_stage = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(stats)
    }

    pub fn chapter_progress(&self) -> Result<Vec<ChapterProgress>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT c.id, c.title, c.order_index, COUNT(s.id), \
                    SUM(CASE WHEN s.state='QA_PASSED' THEN 1 ELSE 0 END), \
                    SUM(CASE WHEN s.state='QA_FAILED' THEN 1 ELSE 0 END), \
                    SUM(CASE WHEN s.state='HUMAN_REVIEW' THEN 1 ELSE 0 END), \
                    SUM(CASE WHEN s.state IN ('PENDING','DRAFTED','POLISHED') THEN 1 ELSE 0 END) \
             FROM chapters c LEFT JOIN segments s ON s.chapter_id=c.id \
             GROUP BY c.id ORDER BY c.order_index",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ChapterProgress {
                id: row.get(0)?,
                title: row.get(1)?,
                order_index: row.get(2)?,
                total: row.get(3)?,
                passed: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                failed: row.get::<_, Option<i64>>(5)?.unwrap_or(0),
                human: row.get::<_, Option<i64>>(6)?.unwrap_or(0),
                pending: row.get::<_, Option<i64>>(7)?.unwrap_or(0),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn failed_segments(&self) -> Result<Vec<SegmentRecord>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT * FROM segments WHERE state IN ('QA_FAILED','HUMAN_REVIEW') \
             ORDER BY chapter_id, order_index",
        )?;
        let rows = stmt.query_map([], SegmentRecord::from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

fn allowed_transition(current: SegmentState, target: SegmentState) -> bool {
    use SegmentState::*;
    match current {
        Pending => matches!(target, Drafted | HumanReview),
        Drafted => matches!(target, Polished | Pending | HumanReview),
        Polished => matches!(target, QaPassed | QaFailed | HumanReview),
        QaFailed => matches!(target, Pending | Drafted | Polished | HumanReview),
        QaPassed => matches!(target, Polished | HumanReview),
        HumanReview => true,
    }
}
