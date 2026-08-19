# Product

<!-- impeccable:product-schema 1 -->

## Platform

macos(原生桌面,Rust + GPUI;非 web)

## Stack

Rust + gpui 0.2 + gpui-component 0.5(用户既定,workspace: crates/wake-core 数据层 + crates/wake UI)

## Users

Corey 本人(独立开发者,主力工具 Claude Code 与 Codex,中文为主)。已开源(2026-08-18,v0.1.0,github.com/iAmCorey/Wake,MIT):面向同时使用多个 coding agent 的开发者。

## Product Purpose

把散落在本机各 coding agent 私有目录里的会话统一起来:浏览、全文搜索(中文+代码子串)、一键在终端恢复、收藏/导出/删除。成功 = 想找任何一段历史对话时,几秒内定位到并能继续它。

## Positioning

唯一以"本地文件为唯一事实源"的多 agent 会话管理器:全程只读原始数据、零网络请求、索引可随时重建;竞品要么单一 agent 要么云端。

## Operating Context

日常开发中随手唤起(常驻后台索引);与终端、编辑器并排使用;深浅色环境都会出现(跟随系统)。数据规模:本机 ~289 会话/约 800MB JSONL,实时增量。

## Capabilities and Constraints

已实现:八家 adapter、FTS5 trigram 搜索(<3 码点 LIKE 降级)、**搜索跳转定位**(2026-08-18:⌘K 命中直达详情页对应消息并高亮,seq 契约保证)、详情页逐消息渲染(气泡/工具折叠簇/thinking/tree-sitter 高亮;2026-08-17 由整篇 markdown 方案升级)、恢复/收藏/置顶/导出/删除(废纸篓+墓碑)、文件监听增量、测试套件(wake-core 合成 fixture + CI + pre-commit)。
约束:对 agent 数据目录只读;绝不写 Codex 的 SQLite;不读凭证;GPUI 无 SF Symbols(图标用 lucide SVG 自备)。
已支持八家 agent:Claude Code、Codex、Copilot CLI、Cursor(CLI transcripts)、OpenCode、Kiro、Gemini CLI、Grok Build(Cursor IDE chats 正文加密不做,Windsurf/Trae 加密、Amp/Factory/Warp 云端无本地数据)。

## Brand Commitments

名称 Wake(2026-08-14 由 Vibex 更名;取「船迹」——agent 驶过的痕迹,兼「唤醒」恢复会话之意)。界面语言英文(2026-08-14 由中文切换,用户反馈中文 UI 词汇观感生硬)。视觉基准(用户 2026-08-14 确认):现代 macOS 原生规范,工艺对标 Things / Bear(优雅轻盈的原生感);外观跟随系统(浅+深双模式)。agent 品牌色作为功能性识别色保留(Claude 橙 #D97757、Codex 绿 #12A06B 等,见 models.rs)。

## Evidence on Hand

开发验证用真实本机数据(~289 会话)。**对外截图/演示一律用合成数据**:`scripts/demo-home.py` 生成假家目录(合成会话/假项目/八家全亮),2026-08-19 定——真实项目名私密,不对外展示。

## Product Principles

- 本地优先,只读别家数据,一切可重建
- 找回一段对话的速度是唯一北极星
- 原生质感优先于个性表达(Operate 工具,克制)
- 中文内容(会话正文)的排版与混排质量是一等公民;UI 语言为英文
- 开源可读:代码与设计决策都要经得起外人看

## Accessibility & Inclusion

跟随系统深浅色;文字对比按 HIG;不依赖纯色区分状态(色点旁始终有文字)。
