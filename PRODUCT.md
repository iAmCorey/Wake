# Product

<!-- impeccable:product-schema 1 -->

## Platform

macos(主平台,原生桌面,Rust + GPUI;非 web);Linux(experimental,2026-08-24 起,arm64/x86_64 预编包)与 Windows(experimental,2026-08-25 起,x86_64 zip)为次级平台,数据层三端同测,桌面集成 experimental

## Stack

Rust + gpui 0.2 + gpui-component 0.5(用户既定,workspace: crates/wake-core 数据层 + crates/wake UI)

## Users

Corey 本人(独立开发者,主力工具 Claude Code 与 Codex,中文为主)。已开源(2026-08-18 v0.1.0 首发,当前 v0.6.7 2026-09-15,github.com/iAmCorey/Wake,MIT):面向同时使用多个 coding agent 的开发者。

## Product Purpose

把散落在本机各 coding agent 私有目录里的会话统一起来:浏览、全文搜索(中文+代码子串)、一键在终端恢复、收藏/导出/删除。成功 = 想找任何一段历史对话时,几秒内定位到并能继续它。

## Positioning

唯一以"本地文件为唯一事实源"的多 agent 会话管理器:全程只读原始数据、无后台网络请求、索引可随时重建;仅在用户于 Updates 页或 macOS Wake 菜单主动检查更新时读取 Wake 的公开 GitHub Release 元数据,不发送任何会话数据。竞品要么单一 agent 要么云端。

## Operating Context

日常开发中随手唤起(常驻后台索引);与终端、编辑器并排使用;深浅色环境都会出现(跟随系统)。数据规模:本机 ~310 会话/约 800MB JSONL,实时增量。

## Capabilities and Constraints

已实现:二十二家 adapter、FTS5 trigram 搜索(<3 码点 LIKE 降级)、**搜索跳转定位**(2026-08-18:⌘K 命中直达详情页对应消息并高亮,seq 契约保证)、详情页逐消息渲染(气泡/工具折叠簇/thinking/tree-sitter 高亮;2026-08-17 由整篇 markdown 方案升级)、恢复/收藏/置顶/导出/删除(废纸篓+墓碑)、文件监听增量、Insights 统计页(0.3.0:活跃热力图与 streak、时段/星期/月份分布、Agents·Projects·Models 三榜单按 sessions/tokens/prompts 切换;2026-09-03 加 Last 7 days 周对比行与 Over time 近 53 周按 agent 堆叠的每周 prompts 趋势图;口径=主线用户消息,规格见 DESIGN.md)、Updates 页与 macOS Wake 菜单手动检查 GitHub 最新正式版并打开 Release 页更新(不后台联网、不自动替换应用包)、测试套件(adapter 契约、DB 往返、scanner 回归 + CI 三平台 + pre-commit,合成 fixture)、三端桌面(macOS 主平台;Linux/Windows experimental,终端恢复/废纸篓·回收站/剪贴板按平台原生实现,发版 CI 自动出六产物)、窗口记忆(0.3.6:记住所在屏幕/屏内位置/最大化/全屏,重启与 Dock 重开都回到原处,Settings 开在主窗所在屏)、完整 macOS 菜单栏(0.3.6:标准 Edit/Window 菜单、⌘W、Window → Main Window,主窗关闭后 About/Settings/Updates 仍可用)、导出 Markdown 与保存图片走系统「另存为」并记住上次目录(0.3.6,issue #25)、远程 host(0.4.0,issue #21:Settings → Remote hosts 配 SSH 目标,rsync 白名单镜像会话数据到本地缓存后与本地会话同列同搜,`@host` 徽章标识;阶段 1 只读——不动远端文件、远程会话禁删,resume 以 Copy SSH command 形态提供;分期计划见 CLAUDE.md「远程 host」节)、**MCP 接入**(0.5.0:随应用打包的只读 `wake-mcp` stdio server,四个工具 wake_search / wake_list_sessions / wake_get_session / wake_list_projects,Claude Code / Codex / Cursor 等任何 MCP 客户端可搜历史、按项目列会话、分页读转录;Settings → Connect 展示路径与可复制配置片段,只展示不代写别家配置;零 LLM、零网络、不写库;完整参考 docs/mcp.md)、**命令行与 Skill**(0.6.0:并排打包的只读 `wake-cli`,四个子命令 search / sessions / show / projects 加 setup,给"能跑 shell 但不说 MCP"的 agent 和真人用——它不新写查询逻辑,argv 翻成 JSON 喂同一批工具,输出与 MCP 客户端看到的**逐字节相同**(仅 CLI 末尾补一个换行)并由测试卡住,所以文档只需描述一份格式;`skills/wake/SKILL.md` 是可一行安装的 skill(`npx skills add iAmCorey/Wake`),用触发条件而非名词解释写 description,让 agent 自己想得起来去翻会话历史;完整参考 docs/cli.md;0.6.3 加 `wake-cli index`——装了 Wake 却从没启动过时,agent 自己跑一条就能把索引建起来继续干活,而不是停下来问人。它**只建不存在的索引**,已有的库原样交给 GUI:扫描落在旁边的临时库、扫完才原子占位,所以被中途杀掉可重试、撞上 GUI 首扫就退让;2026-09-22 加 `wake-cli refresh`,issue #43:app 不开时增量刷新已有索引,给靠 MCP / CLI 用 Wake、只在浏览时开 app 的人用 launch agent / systemd timer 定时跑,Wake 开着窗或还有扫描在写就退让——GUI 与两个写命令共用一把索引写锁,锁跟 Workbench 与写线程走、关窗即放,GUI 启动会等正在跑的 refresh 收尾)、Connect 页四区块(0.6.1:MCP server / MCP clients / Command line / Skill,每个面的第一块挂自己的文档链接)、**记忆可见层**(2026-09-17 起,只读:把各家 agent 自己写下的记忆——Claude Code 的按项目 auto-memory、Codex 的 memories 与逐会话摘要、ZCode 的按项目记忆——按项目聚合;GUI 的 Memory 页是侧栏底部的整页目的地,侧栏在这一页换成按记忆计数的 All Memory / Agents / Projects 导航;MCP 加第五个工具 `wake_list_memories`,`wake_get_session` 认 `wake://memory/…` 引用现场读文件,`wake_search` 末尾附记忆命中;CLI 加 `memories`;记忆的项目归属按它挂的会话在读时解析,永远跟着索引走。二期(2026-09-21)把用户写给 agent 的指令文件也收进来:各家 home 里的 CLAUDE.md / AGENTS.md(与 `rules/*.rules`)/ GEMINI.md,以及已索引项目根下的 CLAUDE.md / AGENTS.md / GEMINI.md / `.cursor/rules` / `.cursorrules` / `.kiro/steering` / `.github/copilot-instructions.md`,与记忆同列同搜、不另作区分;Settings 加 Memory locations 页,与 Session locations 同形制:逐来源开关、添加自定义目录或文件、Restore defaults。不编辑、不同步、不删除——写别家数据破铁律,要改一个 Reveal 就到;也不做"把会话自动写成记忆")。后续边界(2026-09-07 定):摘要层(需 LLM、opt-in、明示会把会话正文发给用户配置的端点)只在 MCP 接入有真实使用之后再做;Timeline / Profile 一类视图建立在摘要之上;不做 ChatGPT / Claude.ai 导出导入主线、不做实体本体与知识图谱、不做 Chrome 扩展注入。
约束:对 agent 数据目录只读;绝不写 Codex 的 SQLite;不读凭证;GPUI 无 SF Symbols(图标用 lucide SVG 自备)。
已支持二十二家 agent:Claude Code、Codex(0.6.6 起排除可识别的内部线程:guardian auto-review、`/review`、compaction、memory consolidation 与用户会话同树,adapter 文件边界按首行 session_meta 的 source / thread_source 认出即不枚举;老格式或读不出的首行保守放行,`codex exec --thread-source` 打了标签的自动化线程照常收录,issue #30。`spawn_agent` 子代理是其中唯一的例外,issue #42:那是用户自己派出去的活,收录后挂在派它的会话下面、以任务名为标题,fork 进来的父线程历史折成一条不入库的占位,父会话的对话不会被索引两遍)、Qoder CLI(`~/.qoder/projects` JSONL,active-leaf 分支恢复、tool result 回挂、`QODER_CONFIG_DIR`)、Copilot CLI、Cursor(CLI 转录 + 0.6.7 起 IDE Chat/Composer 历史,PR #32:`globalStorage/state.vscdb` 明文 KV 库,composerData 给顺序、bubbleId 行给正文;转录带正文的会话归转录源,库只补转录缺失或只剩 turn_ended 空壳的会话;库里多数老会话没存工作区路径,落 Unknown project)、OpenCode(含 OpenCode 2 next,stable 的 `opencode.db` 与 next 的 `opencode-next.db` 同时扫描;逐会话兼容 `message+part`、真实 `session+session_message` 及早期 `session_v2` schema;preview 会话标 opencode2 徽章)、Kiro、Gemini CLI(2026-08-17 P1 五家落地)+ Pi、Oh My Pi、Grok Build、Kimi Code、Antigravity CLI(2026-08-19 对齐 kooky 内置 roster,正文加密仅元数据卡片)、Antigravity IDE(2026-09-05 起读 `~/.gemini/antigravity-ide/brain` 明文转录全解析,含用户贴图;2026-09-25 与 CLI 拆成两个 agent——共用一个笼统的 Antigravity 名会让人以为桌面端 agy 本体也支持了)+ DeepSeek Harness(dsh,2026-08-20,zstd 事件日志透明解压;格式由源码推断,当天用户跑出真实会话完成首验)+ Hermes Agent(2026-09-03,多档案 state.db,无 cwd 故 project 留空)、OpenClaw(2026-09-03,两代存储同扫;本机零会话,格式由源码推断未经真机验证)、CodeBuddy(2026-09-14,issue #27:`~/.codebuddy/projects` 的 Responses 形 JSONL,custom / ai / topic 三级标题,`CODEBUDDY_CONFIG_DIR`;本机零会话,格式由官方 CLI 文档与三家第三方实现交叉推断、未经真机验证)、WorkBuddy(同日,CodeBuddy 的孪生实例:同一解析核心,`~/.workbuddy/projects`、`WORKBUDDY_CONFIG_DIR`;桌面 app 无 CLI,不提供 resume;同构只有 cc-switch 一家佐证)、ZCode(2026-09-21:Z.ai 的 GLM-5.3 官方 harness,桌面 app;运行时是 OpenCode 衍生物——`~/.zcode/cli/db/db.sqlite` 的 session/message/part 与 OpenCode v1 同形、part 解码复用,但 session 表没有 model/tokens 列故另起一家;`v2/tasks-index.sqlite` 只借 deleted / migration_source 两个过滤位;按 `task_type` 白名单只列 interactive / fork / selection_side_chat(subagent_child 等运行时会话不列);`semantics.transcriptVisibility == hidden` 与用户消息 `origin != real_user` 归 Meta、compact_summary 折成一条、字段缺席放行;`ZCODE_STORAGE_DIR`;桌面 app 无 CLI、`zcode://` 没有按会话打开的深链,不提供 resume;本机 3.14.0 真会话首验通过,同日开源(Apache-2.0)后语义全部按源码核对)、Craft Agents(2026-09-24,issue #44:Craft Docs 的开源桌面 app,自己不带引擎——Anthropic 连接跑 Claude Agent SDK、其余连接(ChatGPT / Codex 订阅、Copilot、Google)跑 Pi SDK,另记一份给用户看的 `~/.craft-agent/workspaces/<工作区>/sessions/<id>/session.jsonl`,Wake 读这一份;子任务与分支挂回原会话,分支从父会话复制来的那段折成一条;Claude 连接在 `~/.claude/projects` 留下的引擎副本由通用的"认领"机制藏掉,原件还在才藏;工作区可建在任意位置,但登记表带 server token 不读,别处的工作区走 Session locations;桌面 app 无 CLI,不提供 resume;本机 0.13.5 真会话首验通过,ChatGPT 与 Claude 连接各一条)、Devin(2026-09-24,issue #46:`~/.local/share/devin/cli/sessions.db` 单库两表,message_nodes 森林按 `main_chain_id` 叶子沿 `parent_node_id` 取主链,hidden 会话与零正文不列,逐消息 `generation_model` 与 `metrics` 按主链累计;`XDG_DATA_HOME`;`devin --resume <id>` 按 cwd 分桶 resume;格式按本机真库验证)。做不了的:Windsurf/Trae 加密,Amp/Factory(Droid)/Warp 云端无本地数据,Reasonix 本机零会话格式未实测。

## Brand Commitments

名称 Wake(2026-08-14 由 Vibex 更名;取「船迹」——agent 驶过的痕迹,兼「唤醒」恢复会话之意)。界面语言以英文为源(2026-08-14 由中文切换,用户反馈中文 UI 词汇观感生硬),0.5.2 起经 i18n 层出译文、内置简体中文、首次启动跟随系统语言,Settings → General 可固定某一种。视觉基准(用户 2026-08-14 确认):现代 macOS 原生规范,工艺对标 Things / Bear(优雅轻盈的原生感);支持跟随系统或固定浅/深外观。agent 品牌色作为功能性识别色保留(Claude 橙 #D97757、Codex 绿 #12A06B 等,见 models.rs)。Session locations 自 0.2.9 起归入独立 Settings 窗口,主界面只保留齿轮入口;Settings 固定为 General / Locations / Remote hosts / Connect / Data / Updates / About(Remote hosts 为 0.4.0 新增,Connect 为 0.5.0 新增),不提供默认 “Open In” 终端选择。

## Evidence on Hand

开发验证用真实本机数据(~310 会话)。**对外截图/演示一律用合成数据**:`scripts/demo-home.py` 生成假家目录(22 个合成会话/5 个假项目/七家全亮),2026-08-19 定——真实项目名私密,不对外展示。

## Product Principles

- 本地优先,只读别家数据,一切可重建
- 找回一段对话的速度是唯一北极星
- 原生质感优先于个性表达(Operate 工具,克制)
- 中文内容(会话正文)的排版与混排质量是一等公民;UI 文案以英文为源,译文经 i18n 层产出
- 开源可读:代码与设计决策都要经得起外人看

## Accessibility & Inclusion

跟随系统深浅色;文字对比按 HIG;不依赖纯色区分状态(色点旁始终有文字)。
