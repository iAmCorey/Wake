//! 远程会话的 adapter 装饰器。
//!
//! `RemoteAdapter` 包一个 `with_custom_root(缓存内数据目录)` 的常规实例,
//! 在**每个产出 `SessionMeta` 的出口**(parse_session / parse_transcript /
//! quick_meta / parent_links)统一做两件事:key 在 agent 段之后插入 host 段
//! (`agent:native` → `agent:host:native`,经 models::session_key 单点)、
//! `meta.host` 赋值。inner 对远程零感知,merge_quick_meta 只在两个已改写的
//! meta 之间搬运字段(codex 的 `parsed.key = quick.key` 语义因此透明成立);
//! 各家的解析/枚举逻辑零改动(构造点仅机械补了 `host: String::new()`)。
//!
//! **必须逐方法显式转发**:trait 的默认实现一旦被装饰器"继承",inner 的
//! 覆写(codex 的 quick_meta、dsh 的 file_ref、grok 的 parent_links)就被
//! 静默绕过——新增 trait 方法时这里必须跟着补转发。

use super::AgentAdapter;
use crate::models::*;
use anyhow::Result;
use std::path::Path;

pub struct RemoteAdapter {
    inner: Box<dyn AgentAdapter>,
    host: String,
}

impl RemoteAdapter {
    pub fn new(inner: Box<dyn AgentAdapter>, host: impl Into<String>) -> Self {
        Self {
            inner,
            host: host.into(),
        }
    }

    /// `agent:native` → `agent:host:native`(经 models::session_key 单点)。
    /// agent 恒为首段——scanner 的易主检测(`key.split(':').next()`)依赖。
    fn rewrite_key(&self, local_key: &str) -> String {
        // inner 生产的 key 恒为 `agent:` 前缀;万一不是,整串当 native 仍唯一
        let native = local_key
            .split_once(':')
            .map(|(_, rest)| rest)
            .unwrap_or(local_key);
        session_key(self.inner.agent(), &self.host, native)
    }

    /// 装饰器契约的唯一实现:每个产出 SessionMeta 的出口做且只做这两件事。
    /// &mut 签名让四个出口(含 quick_meta 的 values_mut 循环)共用同一份,
    /// 出口再加改写字段时不存在漏改其一的第二副本
    fn rewrite_meta(&self, meta: &mut SessionMeta) {
        meta.key = self.rewrite_key(&meta.key);
        meta.host = self.host.clone();
    }
}

/// 对一个 host 构造整组远程实例:十六家模板 × `with_custom_root`(缓存内
/// 挂载点,见 remote::REMOTE_LAYOUTS)× 装饰器。追加进 roster 的 active 尾部
/// (不进 Session locations 面板)。`templates` 由 roster 唯一构造点传入
/// (不变量 8:本模块不得自行二次 create_adapters());产物的数据根 100%
/// 派生自 `host_cache`,不携带模板的 env/文件系统快照。
pub fn create_remote_adapters(
    templates: &[Box<dyn AgentAdapter>],
    host: &str,
    host_cache: &Path,
) -> Vec<Box<dyn AgentAdapter>> {
    let mut out: Vec<Box<dyn AgentAdapter>> = Vec::new();
    for layout in crate::remote::REMOTE_LAYOUTS {
        let Some(template) = templates.iter().find(|a| a.agent() == layout.agent) else {
            continue;
        };
        let inner = template.with_custom_root(host_cache.join(layout.mount));
        out.push(Box::new(RemoteAdapter::new(inner, host)));
    }
    out
}

impl AgentAdapter for RemoteAdapter {
    fn agent(&self) -> AgentId {
        self.inner.agent()
    }

    fn host(&self) -> &str {
        &self.host
    }

    fn detect(&self) -> bool {
        self.inner.detect()
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        self.inner.list_session_files()
    }

    fn file_ref(&self, path: &Path) -> Option<SessionFileRef> {
        self.inner.file_ref(path)
    }

    fn quick_meta(
        &self,
        refs: &[SessionFileRef],
    ) -> Option<std::collections::HashMap<String, SessionMeta>> {
        // 原地改值:map 的键(file_path)不变,重建整表是纯浪费——这张表
        // 是"整家会话"量级,每轮扫描都过一次
        let mut map = self.inner.quick_meta(refs)?;
        for meta in map.values_mut() {
            self.rewrite_meta(meta);
        }
        Some(map)
    }

    fn merge_quick_meta(&self, parsed: SessionMeta, quick: &SessionMeta) -> SessionMeta {
        // 两个输入都已在各自出口改写;inner 只搬运字段(codex 会整体取
        // quick.key),结果无需再套一层
        self.inner.merge_quick_meta(parsed, quick)
    }

    fn parse_session(&self, r: &SessionFileRef) -> Result<ParsedSession> {
        let mut parsed = self.inner.parse_session(r)?;
        self.rewrite_meta(&mut parsed.meta);
        Ok(parsed)
    }

    fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript> {
        let mut parsed = self.inner.parse_transcript(r)?;
        self.rewrite_meta(&mut parsed.meta);
        Ok(parsed)
    }

    fn load_sidechain(
        &self,
        r: &SessionFileRef,
        sidechain_id: &str,
    ) -> Result<Vec<TranscriptMessage>> {
        self.inner.load_sidechain(r, sidechain_id)
    }

    fn session_paths(&self, meta: &SessionMeta) -> Vec<String> {
        // 远程会话 UI 禁删;这里如实返回缓存内路径,消费方(若有)拿到的
        // 是本地副本而非远端文件
        self.inner.session_paths(meta)
    }

    fn begin_scan(&self) {
        self.inner.begin_scan();
    }

    fn manages_parent_links(&self) -> bool {
        self.inner.manages_parent_links()
    }

    fn parent_links(&self) -> Vec<(String, String)> {
        self.inner
            .parent_links()
            .into_iter()
            .map(|(child, parent)| (self.rewrite_key(&child), self.rewrite_key(&parent)))
            .collect()
    }

    fn is_parent_link_event(&self, path: &Path) -> bool {
        self.inner.is_parent_link_event(path)
    }

    fn data_roots(&self) -> Vec<std::path::PathBuf> {
        self.inner.data_roots()
    }

    fn watch_paths(&self) -> Vec<std::path::PathBuf> {
        self.inner.watch_paths()
    }

    fn with_custom_root(&self, dir: std::path::PathBuf) -> Box<dyn AgentAdapter> {
        // 远程实例没有自定义 location 的产品入口;保持装饰不变量以防未来误用
        Box::new(Self::new(
            self.inner.with_custom_root(dir),
            self.host.clone(),
        ))
    }

    fn supports_individual_root_removal(&self) -> bool {
        self.inner.supports_individual_root_removal()
    }

    fn excluding_data_roots(&self, roots: &[std::path::PathBuf]) -> Option<Box<dyn AgentAdapter>> {
        let inner = self.inner.excluding_data_roots(roots)?;
        Some(Box::new(Self::new(inner, self.host.clone())))
    }
}
