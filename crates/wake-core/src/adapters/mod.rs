pub mod claude;
pub mod codex;
pub mod copilot;
pub mod cursor;
pub mod gemini;
pub mod grok;
pub mod kiro;
pub mod opencode;

pub(crate) mod parse_utils;
mod sqlite_ro;

use crate::models::*;
use anyhow::Result;
use std::path::Path;

/// agent 数据源适配器。列表扫描与详情解析共用同一核心解析器,
/// 保证 FTS 的 seq 与详情页消息序号一致(搜索跳转依赖)。
pub trait AgentAdapter: Send + Sync {
    fn agent(&self) -> AgentId;
    /// 数据根目录是否存在
    fn detect(&self) -> bool;
    /// 枚举全部会话文件(只 stat 不读内容)
    fn list_session_files(&self) -> Result<Vec<SessionFileRef>>;
    /// watcher 事件路径 → 本 adapter 的会话文件引用;None = 非会话文件
    /// (边车、子代理转录等)。默认:非空 .jsonl,stem 即 native_id。
    /// 各家的路径布局知识收敛在此,watcher 不再硬编码任何 agent 特例。
    fn file_ref(&self, path: &Path) -> Option<SessionFileRef> {
        parse_utils::default_file_ref(self.agent(), path)
    }
    /// 快路径:不解析文件直接给出 meta(Codex 走 state DB)。None = 无快路径
    fn quick_meta(&self, _refs: &[SessionFileRef]) -> Option<std::collections::HashMap<String, SessionMeta>> {
        None
    }
    /// quick 与 parsed 的合并策略:默认 parsed 为准、quick 补缺。
    /// Codex 覆写(state DB 的 title 是用户手动命名,优先级更高)。
    fn merge_quick_meta(&self, mut parsed: SessionMeta, quick: &SessionMeta) -> SessionMeta {
        if parsed.source.is_none() {
            parsed.source = quick.source.clone();
        }
        if parsed.model.is_none() {
            parsed.model = quick.model.clone();
        }
        if parsed.tokens_used.is_none() {
            parsed.tokens_used = quick.tokens_used;
        }
        parsed
    }
    /// 全解析:meta + FTS 单元
    fn parse_session(&self, r: &SessionFileRef) -> Result<ParsedSession>;
    /// 详情解析
    fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript>;
    /// 加载 sidechain 消息(仅 Claude/Cursor subagents)
    fn load_sidechain(&self, _r: &SessionFileRef, _sidechain_id: &str) -> Result<Vec<TranscriptMessage>> {
        Ok(Vec::new())
    }
    /// 会话在磁盘上的全部归属路径(删除时一并 trash)。默认仅主文件;
    /// 有边车/目录布局的 adapter 覆写。
    fn session_paths(&self, meta: &SessionMeta) -> Vec<String> {
        vec![meta.file_path.clone()]
    }
    /// 文件监听根目录
    fn watch_paths(&self) -> Vec<std::path::PathBuf>;
}

pub fn create_adapters() -> Vec<Box<dyn AgentAdapter>> {
    let all: Vec<Box<dyn AgentAdapter>> = vec![
        Box::new(claude::ClaudeAdapter::new()),
        Box::new(codex::CodexAdapter::new()),
        Box::new(copilot::CopilotAdapter::new()),
        Box::new(cursor::CursorAdapter::new()),
        Box::new(opencode::OpencodeAdapter::new()),
        Box::new(kiro::KiroAdapter::new()),
        Box::new(gemini::GeminiAdapter::new()),
        Box::new(grok::GrokAdapter::new()),
    ];
    all.into_iter().filter(|a| a.detect()).collect()
}

/// 从解析后的消息派生 FTS 单元(text + tool 名称/输入摘要)
pub(crate) fn units_from_messages(messages: &[TranscriptMessage]) -> Vec<IndexUnit> {
    messages
        .iter()
        .filter(|m| m.kind == MessageKind::Text)
        .filter_map(|m| {
            let mut parts = vec![m.text.clone()];
            for tc in &m.tool_calls {
                parts.push(format!("{} {}", tc.name, tc.input_preview));
            }
            let text = parse_utils::clip(&parts.join("\n"), MAX_MSG_TEXT).0;
            if text.trim().is_empty() {
                None
            } else {
                Some(IndexUnit {
                    seq: m.seq,
                    sidechain_id: None,
                    role: m.role,
                    timestamp: m.timestamp,
                    text,
                })
            }
        })
        .collect()
}
