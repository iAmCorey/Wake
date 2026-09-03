# Product

<!-- impeccable:product-schema 1 -->

## Platform

macos(主平台,原生桌面,Rust + GPUI;非 web);Linux(experimental,2026-08-24 起,arm64/x86_64 预编包)与 Windows(experimental,2026-08-25 起,x86_64 zip)为次级平台,数据层三端同测,桌面集成 experimental

## Stack

Rust + gpui 0.2 + gpui-component 0.5(用户既定,workspace: crates/wake-core 数据层 + crates/wake UI)

## Users

Corey 本人(独立开发者,主力工具 Claude Code 与 Codex,中文为主)。已开源(2026-08-18 v0.1.0 首发,当前 v0.3.6 2026-09-02,github.com/iAmCorey/Wake,MIT):面向同时使用多个 coding agent 的开发者。

## Product Purpose

把散落在本机各 coding agent 私有目录里的会话统一起来:浏览、全文搜索(中文+代码子串)、一键在终端恢复、收藏/导出/删除。成功 = 想找任何一段历史对话时,几秒内定位到并能继续它。

## Positioning

唯一以"本地文件为唯一事实源"的多 agent 会话管理器:全程只读原始数据、无后台网络请求、索引可随时重建;仅在用户于 Updates 页或 macOS Wake 菜单主动检查更新时读取 Wake 的公开 GitHub Release 元数据,不发送任何会话数据。竞品要么单一 agent 要么云端。

## Operating Context

日常开发中随手唤起(常驻后台索引);与终端、编辑器并排使用;深浅色环境都会出现(跟随系统)。数据规模:本机 ~310 会话/约 800MB JSONL,实时增量。

## Capabilities and Constraints

已实现:十六家 adapter、FTS5 trigram 搜索(<3 码点 LIKE 降级)、**搜索跳转定位**(2026-08-18:⌘K 命中直达详情页对应消息并高亮,seq 契约保证)、详情页逐消息渲染(气泡/工具折叠簇/thinking/tree-sitter 高亮;2026-08-17 由整篇 markdown 方案升级)、恢复/收藏/置顶/导出/删除(废纸篓+墓碑)、文件监听增量、Insights 统计页(0.3.0:活跃热力图与 streak、时段/星期/月份分布、Agents·Projects·Models 三榜单按 sessions/tokens/prompts 切换;2026-09-03 加 Last 7 days 周对比行与 Over time 近 53 周按 agent/模型堆叠的每周 prompts 趋势图;口径=主线用户消息,规格见 DESIGN.md)、Updates 页与 macOS Wake 菜单手动检查 GitHub 最新正式版并打开 Release 页更新(不后台联网、不自动替换应用包)、测试套件(adapter 契约、DB 往返、scanner 回归 + CI 三平台 + pre-commit,合成 fixture)、三端桌面(macOS 主平台;Linux/Windows experimental,终端恢复/废纸篓·回收站/剪贴板按平台原生实现,发版 CI 自动出六产物)、窗口记忆(0.3.6:记住所在屏幕/屏内位置/最大化/全屏,重启与 Dock 重开都回到原处,Settings 开在主窗所在屏)、完整 macOS 菜单栏(0.3.6:标准 Edit/Window 菜单、⌘W、Window → Main Window,主窗关闭后 About/Settings/Updates 仍可用)、导出 Markdown 与保存图片走系统「另存为」并记住上次目录(0.3.6,issue #25)、远程 host(0.4.0,issue #21:Settings → Remote hosts 配 SSH 目标,rsync 白名单镜像会话数据到本地缓存后与本地会话同列同搜,`@host` 徽章标识;阶段 1 只读——不动远端文件、远程会话禁删,resume 以 Copy SSH command 形态提供;分期计划见 CLAUDE.md「远程 host」节)。
约束:对 agent 数据目录只读;绝不写 Codex 的 SQLite;不读凭证;GPUI 无 SF Symbols(图标用 lucide SVG 自备)。
已支持十六家 agent:Claude Code、Codex、Qoder CLI(`~/.qoder/projects` JSONL,active-leaf 分支恢复、tool result 回挂、`QODER_CONFIG_DIR`)、Copilot CLI、Cursor(CLI transcripts)、OpenCode(含 OpenCode 2 next,stable 的 `opencode.db` 与 next 的 `opencode-next.db` 同时扫描;逐会话兼容 `message+part`、真实 `session+session_message` 及早期 `session_v2` schema;preview 会话标 opencode2 徽章)、Kiro、Gemini CLI(2026-08-17 P1 五家落地)+ Pi、Oh My Pi、Grok Build、Kimi Code、Antigravity CLI(2026-08-19 对齐 kooky 内置 roster;Antigravity 正文加密仅元数据卡片)+ DeepSeek Harness(dsh,2026-08-20,zstd 事件日志透明解压;格式由源码推断,当天用户跑出真实会话完成首验)。做不了的:Cursor IDE chats 正文加密,Windsurf/Trae 加密,Amp/Factory(Droid)/Warp 云端无本地数据,Reasonix 本机零会话格式未实测。

## Brand Commitments

名称 Wake(2026-08-14 由 Vibex 更名;取「船迹」——agent 驶过的痕迹,兼「唤醒」恢复会话之意)。界面语言英文(2026-08-14 由中文切换,用户反馈中文 UI 词汇观感生硬)。视觉基准(用户 2026-08-14 确认):现代 macOS 原生规范,工艺对标 Things / Bear(优雅轻盈的原生感);支持跟随系统或固定浅/深外观。agent 品牌色作为功能性识别色保留(Claude 橙 #D97757、Codex 绿 #12A06B 等,见 models.rs)。Session locations 自 0.2.9 起归入独立 Settings 窗口,主界面只保留齿轮入口;Settings 固定为 General / Locations / Remote hosts / Data / Updates / About(Remote hosts 为 0.4.0 新增),不提供默认 “Open In” 终端选择。

## Evidence on Hand

开发验证用真实本机数据(~310 会话)。**对外截图/演示一律用合成数据**:`scripts/demo-home.py` 生成假家目录(22 个合成会话/5 个假项目/七家全亮),2026-08-19 定——真实项目名私密,不对外展示。

## Product Principles

- 本地优先,只读别家数据,一切可重建
- 找回一段对话的速度是唯一北极星
- 原生质感优先于个性表达(Operate 工具,克制)
- 中文内容(会话正文)的排版与混排质量是一等公民;UI 语言为英文
- 开源可读:代码与设计决策都要经得起外人看

## Accessibility & Inclusion

跟随系统深浅色;文字对比按 HIG;不依赖纯色区分状态(色点旁始终有文字)。
