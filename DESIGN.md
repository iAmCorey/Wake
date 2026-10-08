---
name: Wake
description: 本地 Coding Agent 会话资料库的 macOS 桌面设计系统
---

# Wake Design System

> 代码是唯一真相：`crates/wake/src/theme.rs` 定义主题，`workbench.rs` 定义界面结构与对话正文渲染（原 detail.rs 已并入）。修改视觉后同步本文。

## 设计命题

Wake 的目标不是展示技术感，而是让用户在几秒内重新找到并继续一段对话。界面采用 macOS 原生资料库心智，以稳定三栏承载范围、会话和正文；全文搜索作为贯穿整个工作台的命令级能力。

视觉遵循现代 macOS Liquid Glass 的层级原则：

- 先用稳定的窗口拖拽区、来源列表、工具栏、菜单和键盘路径建立结构。
- 用自适应材质色差表达层级，避免给每个区域加边框。
- 常驻界面不使用投影；阴影只留给搜索面板、菜单、确认框和通知。
- 系统蓝只表达主操作、选择和焦点；Agent 身份只由品牌图标表达（`AgentId::brand_icon(dark)`，侧栏 18px、列表/搜索/详情 15px；单色素材按深浅模式取白版或 `-light` 深墨版）。
- 详情阅读面是内容主角，应用框架主动退后。

浅色是银白和暖灰，深色是石墨和暖黑。禁止滑向黑底霓虹、终端面板或仪表盘卡片墙。

## 信息架构

窗口使用稳定的三栏选择模型，不做 iOS 式逐页推进：

1. **资料库侧栏**：全部会话、收藏、智能体和项目。
2. **会话流**：当前范围的会话，按更新时间扫读和筛选。
3. **阅读区**：当前会话的身份、操作、完整元信息和正文。

选择状态显式且稳定。收藏、置顶、导出、显示原文件和删除都围绕当前会话发生；设置使用独立场景，不作为侧栏目的地。

## 窗口与布局

| 项目 | 规格 |
|---|---|
| 默认窗口 | 1180 × 760，居中(14" 屏约占 78% × 77%) |
| 最小窗口 | 940 × 620 |
| 窗口顶部 | macOS 主窗口使用 44px 透明标题栏，交通灯与拖拽区收在侧栏顶部；Windows 用原生标题栏，Linux 视 compositor 装饰协商而定 |
| 资料库侧栏 | 224px 固定宽度 |
| 会话流 | 336px 固定宽度 |
| 阅读区 | 剩余宽度，`min_w(0)`；正文最大宽度 720px |

窗口不绘制全宽标题栏。侧栏承接交通灯、窗口拖拽区和唯一的全文搜索入口；会话流与阅读区直接延伸到窗口顶部。侧栏、会话流、详情头和正文分别使用 `sidebar`、`list`、`background`、`popover` 材质表达层级，不加投影。

## 颜色

所有颜色必须来自 `theme.rs` 的语义 token(含 MODEL_BADGE_BG/STAR_YELLOW 两个专用常量);其他 UI 文件禁止颜色字面量。Agent 品牌资产只能来自 `AgentId::brand_icon(dark)`(内嵌 PNG 路径,定义在 `wake-core/src/models.rs`),加新 agent 时一处改完。

### 主要材质

| token | 浅色 | 深色 | 用途 |
|---|---:|---:|---|
| `title_bar` | `#EDEDEA` | `#1B1B1A` | 侧栏顶部窗口拖拽区 |
| `sidebar` | `#EDEDEA` | `#1B1B1A` | 资料库侧栏 |
| `list` | `#F7F7F5` | `#20201F` | 会话流 |
| `background` | `#F1F1EF` | `#242422` | 阅读区外层 |
| `popover` | `#FDFDFC` | `#2C2C2A` | 阅读面、对话框、菜单 |
| `muted` | `#E8E8E5` | `#323230` | 图标底、角标、静默面 |
| `secondary` | `#E8E8E5` | `#30302E` | 次级按钮、快捷键标签 |

### 文字与交互

| token | 浅色 | 深色 | 用途 |
|---|---:|---:|---|
| `foreground` | `#1D1D1F` | `#F0EFED` | 正文与标题 |
| `muted_foreground` | `#686761` | `#A9A8A2` | 元信息与说明 |
| `primary` | `#0A84FF` | `#4C8DFF` | 主操作、焦点、激活状态 |
| `list_hover` | `#EDEDEA` | `#2A2A28` | 会话 hover |
| `list_active` | `#E3EBF6` | `#303B4C` | 当前会话 |
| `sidebar_accent` | `#DEDEDA` | `#343432` | 当前资料库范围 |
| `danger` | `#E5484D` | `#FF6B60` | 删除与错误 |
| `success` | `#2F9E63` | `#56C789` | 刷新完成 |

主色 tint 必须有语义。不得为了“更活泼”给图标、面板或 Agent 名称随意上色。

## 字体与字阶(六档硬规范)

字体使用 `.AppleSystemUIFont`,等宽内容使用 Menlo。主题基准 14px。

**所有 UI 字号必须引用 `crates/wake/src/ui.rs` 的 FONT_* 常量**——禁止裸 `px` 数字,禁止 `text_sm` 等 rem 工具类(rem 被 Root 钉在 14px,`text_sm` 实渲 12.25px 这类幽灵值是层级失控的根源)。

| 档位 | 常量 | 值 | 字重 | 用途 |
|---|---|---|---|---|
| Title | `FONT_TITLE` | 22px | Semibold | 中栏上下文大标题 |
| Heading | `FONT_HEADING` | 16px | Semibold | 详情页会话标题、空态主标题 |
| Msg user | `FONT_MSG_USER` | 13.5px | Regular | 对话区用户气泡正文 |
| Msg body | `FONT_MSG_BODY` | 13px | Regular | 对话区助手正文 |
| Msg thinking | `FONT_MSG_THINKING` | 11.5px | Italic | 对话区 thinking 摘要 |
| Body | `FONT_BODY` | 14px | Regular(列表/搜索结果标题 Medium) | 导航行、**侧栏组头**、列表标题、按钮、输入、对话框正文 |
| Caption | `FONT_CAPTION` | 12px | Regular | 列表副行、元信息、占位、空态提示、路径 chip、侧栏子级行 |
| Label | `FONT_LABEL` | 11px | Regular | 计数、快捷键徽标、状态栏、会话行与详情头元信息 |

**颜色三级制**:`foreground`(主文字)/`muted_foreground`(全部辅助文字)/`primary`(强调与激活)。不引入第四种文字灰;不在 muted 色上叠 opacity。

**间距刻度(4px 网格)**:`SPACE_XS/SM/MD/LG/XL/XXL` = 4/8/12/16/20/24,定义在 `ui.rs`。新代码引用常量或显式 `px()`;**对齐敏感处禁用 rem 间距类**——`p_2p5` 实为 8.75px、`p_3` 实为 10.5px(rem=14 折算),均不在网格上,是对齐失真的来源。存量 rem 类已于 2026-08-24 全量迁移完毕,代码中不再允许出现 rem 间距类(`p_0` 例外,零无幽灵值)。**图标与紧随其后文字的间距只有一个值 `ICON_TEXT_GAP` = 6px**(侧栏行与组头、列表元信息行、详情 / 阅读面头部与元信息行、⌘K 结果、Insights 榜单、Open In 菜单、状态行、设置页与清理页的行;用户 2026-09-22 定统一,原先 4 / 6 / 7 / 8 / 12 各处不一);它故意不在 4px 网格上:品牌 PNG 没有留白、8px 显得空,lucide 线条图标自带约 1.5px 留白、6px 几何间距视觉约 7.5px。卡片级的 24px 头像(清理历史的记录卡)不在此列。

**侧栏中轴(x = 26.75)**:traffic lights 定位 (20,11),红灯实测直径 13.5px,中心即 **26.75**。侧栏所有行首元素的**视觉中心**压在这条竖线上,而非左缘对齐:

| 元素 | 常量 | 值 | 推导 |
|---|---|---:|---|
| 容器内边距 | `SIDEBAR_EDGE` | 10 | 行 hover/选中胶囊的左右留白 |
| 行首槽位 | `LEAD_BOX` | 18 | 最大前导元素(品牌图)尺寸,槽内**居中** |
| 行左内边距 | `LEAD_INSET` | 7.75 | 26.75 − 9(槽位半宽) − 10 |
| 组头(Agents / Projects) | `LEAD_INSET + (LEAD_BOX − 15) / 2` | 9.25 | 15px 折叠 chevron 按自己的框居中压轴,不占 18px 槽位(2026-09-22 起;原先靠首字母字形宽度反推 `GROUP_HEAD_INSET` = 12.125,已删) |
| 标题左内边距 | `TITLE_INSET` | 9 | 由 "Wake" 的 W 反推(Heading semibold,W 宽 14.25、左承距 0.5) |
| 分组项缩进 | `SUB_INDENT` | 12 | 分组项行首中心落在 38.75,表达从属 |

三条硬约束:

1. **中心对齐与左缘对齐是同一个自由度,只能满足一个。** 选了中心,18px 品牌图的左缘就落在 17.75,比红灯左缘还靠左 2.25——这是预期结果,不是错位。同理分组项一旦缩进就不再压轴。
2. **`TITLE_INSET` 不是间距,是从字形宽度反推的值**,标题的字号、字重一改立即失效,必须重新实测字形再算(组头原先同理:11px semibold 时需 13.25、Body 常规时 12.125——2026-09-22 组头改为 chevron 领头后不再依赖字形)。
3. **2x 屏光栅化步长是 0.5px,别往小数点后继续调。** 当年组头内边距取 12.125 落位 −0.125,改成 12.25 反而跳到 +0.375——两者落进不同物理像素。全部元素落在 ±0.375 以内即为达标。

**行高两级**(侧栏纵向层级的来源):主导航 `ROW_HEIGHT` 32px + Body 14,分组展开项 `ROW_HEIGHT_SUB` 26px + Caption 12。圆角均 8px。

UI 文案以英文为源、经 i18n 层出译文(0.5.2 起,内置简体中文,首次启动跟随系统语言);会话正文保持原语言。元信息分隔符固定为前后带空格的 ` · `。

## 组件规范

### 窗口顶部

macOS 不设置横跨三栏的自定义 header。主窗口透明标题栏高 44px，系统交通灯在其中垂直居中；其所在拖拽区与侧栏使用完全相同的材质和颜色。会话流与阅读区不为标题栏预留另一条色带。Windows 保留系统原生标题栏（贴靠布局与深色模式经 DWM 处理）；Linux 由 compositor 装饰协商决定，报 Client 时侧栏顶部挂自绘 TitleBar。

### 资料库侧栏

- 侧栏顶端按红绿灯、`Wake` 标题和搜索框的可见边界做光学对齐：标题容器上留 4px、下留 16px。窗口控制区和品牌行各高 44px，合计 88px。Memory 页把这个标题换成 "Memory"（顶上就说明现在管的是什么，2026-09-21 用户定，不另加模式标题行）。
- 顶部是唯一的全文搜索入口,文案 "Search sessions",右侧显示 `⌘K`;Search/All Sessions/Starred 固定不随滚动。Memory 页不画这个搜索框(它搜的是会话;⌘K 照常可用),导航紧接标题、直接从 All Memory 开始(补同高空白让行位置不跳的做法试过,用户定不要)。
- 搜索行必须有防溢出结构:标签文字 `flex_1 + min_w_0 + truncate`,图标与 `⌘K` 徽标显式 `flex_shrink_0`。裸文字子元素的最小宽度被内容锁死,侧栏一窄就会把右侧元素挤出边界裁掉。
- 侧栏搜索入口与 Filter 来源搜索共用 `ui::search_field_frame`：32px 高、8px 主题圆角、secondary 填充、10px 水平留白、13px 搜索图标和 Caption 字阶。局部搜索在同一外壳内放无外观的 Input，不叠加默认细边框输入框。
- **行分两级**(侧栏纵向层级的来源,不得拉平):主导航 All Sessions/Starred 32px 行高 + Body 14;分组展开项(agent/项目)26px 行高 + Caption 12 + 整行右移 `SUB_INDENT` 12px 表达从属。
- **每行必须有行首元素**,由 `RowLead` 枚举强制(`Icon` 或 `Brand` 两态,无 `None`):主导航用 Lucide 单线图标,agent 行用品牌 PNG,项目行用 `folder.svg`。槽位定宽 `LEAD_BOX` 管右侧文字起点统一,槽内居中管中轴对齐。
- 线条图标比实心品牌图视觉轻,同档里给小一号:分组项 Lucide 14 / 品牌图 18,主导航 Lucide 15。
- 行内容 = 行首元素 + 标题 + 计数;计数一律 Label 档 muted。
- 组头 "Agents"/"Projects" 用 Body 档常规字重 + muted 色(与主导航同字号同字重,仅靠颜色区分——加粗会让组头压过它统辖的行)。行首是 13px 的折叠 chevron(收起指右、展开指下),坐在与导航行同一个 18px 行首槽位里、中心压 26.75 轴(用户 2026-09-22 定:箭头放到开头,对准上面菜单的图标与红绿灯的红灯;原先 chevron 跟在文字后);chevron 取 15px(与导航行图标同档,描边粗细一致),**不占 18px 槽位、按自己的框居中压轴**,再接统一的 `ICON_TEXT_GAP`——槽位居中再接同一间距,墨迹到文字会比导航行空出一截(chevron 墨迹只有约 5px 宽,用户同日"间隔太大");按框压轴后墨迹到文字视觉约 10.9px 与导航行相当,文字起点比导航行标签左 1.5px。
- 底部工具条常驻,总高 44px（含顶部 1px hairline）,两端分置的**纯图标条**(2026-09-21 三轮定稿:六个一样的 ghost 图标挤一排分不出层级;带边框底色的分段盒子放在侧栏里像一个走错地方的表单控件——参照 Zed 状态栏 / Xcode 导航条,不带盒子):按钮统一 28×28、6px 圆角、14px 图标、2px 间距;**左端**——Sessions(messages-square,双气泡;单气泡的 message-square 留给消息级用途)/ Memory(brain)/ Insights(chart-column)三颗页切换,再接 brush-cleaning "Clean Up Sessions"(它是叠在会话视图上的模式,也靠左),当前页(或打开中的清理)那颗用侧栏行的选中色(`sidebar_accent` 底 + `sidebar_accent_foreground` 图标)做圆角底,与导航行选中态同一套语言;**右端只有齿轮 "Settings"**。左内边距 12.75 = 26.75(侧栏中轴)− 14(按钮半宽):最左那颗图标的中心与红绿灯红灯中心、导航行行首在同一条轴上。未选中的一律透明底、muted 图标,hover 才出色。**Refresh 不在工具条**:它住在每页页头右端(All Sessions 页头在 Sort 左侧,Memory / Insights 页头单独一个;tooltip 就叫 "Refresh"),会话页与 Insights 走整库重扫(Insights 的数据是扫描收尾派生的),Memory 页只同步记忆(读 Settings → Memory locations 的当下配置,不重扫会话;用户 2026-09-22 定各刷各的);进行中转圈并禁用,⌘R 恒为刷会话。Settings 同时进入 Wake 菜单并绑定 `⌘,`(其他平台 `Ctrl+,`),保持单例窗口。
- Settings 默认 820×600，采用 180px 窄侧栏 + 内容页结构，固定为 General / Session locations / Memory locations / Remote hosts / Connect / Data / Updates / About 八项(侧栏项原叫 Locations,有了 Memory locations 后改成与页标题一致的 Session locations,用户 2026-09-21 定)(Connect 为 0.5.0 新增,插在 Remote hosts 与 Data 之间——两页都是"把 Wake 接到外部"的集成面,Data 之后是纯本地信息;Memory locations 是记忆可见层二期加的,紧挨 Locations——两页都是"Wake 读哪里")；About 与功能设置分离并钉在侧栏底部，Wake 菜单的 About Wake 直达同一页。About 沿用 Kooky/Birth 的信息顺序：产品图标、名称、版本、tagline、GitHub、短分隔线、版权/许可证与作者署名。Updates 是独立功能页,仅在用户点击页面按钮或 macOS Wake 菜单的 Check for Updates 时读取 GitHub 最新正式 Release 元数据,明确呈现检查中/最新版/有新版/失败四种状态;有新版时打开 Release 页供用户下载,不后台检查、不自行覆盖应用包。内部常规文字按钮统一沿用主界面的 24px 高、6px 圆角和主题交互色,普通页面动作使用 muted 填充 + hairline；发现新版后的 View Update 是需要用户继续完成的主操作,使用 32px 高 primary 填充和轻阴影。Appearance 分段选择器也使用同一材质。General 只放真实可用的全局偏好,当前为持久化的 System / Light / Dark 外观选择与 Language(跟随系统或固定某个语言包,0.5.2 起);不提供默认 “Open In” 终端。Data 只展示 Wake 本地存储位置、会话数与磁盘占用并提供文件管理器入口,不重复放刷新或清库动作；常规 Refresh 的唯一入口仍是主侧栏底部。Locations 页按 AgentId 声明序以 agent 分组,品牌名只在组头出现一次;本机有数据的组优先,未检测到的 agent 默认收进可展开区。每条路径以路径为主信息、会话数/不可用状态为 muted 副信息,最右为逐路径开关;停用时只降低文字层级，开关与菜单保持完整对比度。行本身不承担编辑,`…` 菜单集中 Edit / Show in Finder / 自定义 Remove。顶部操作为低强调的 Add location,Restore defaults 收进页级 `…` 菜单且无偏离时禁用。添加/编辑仍复用 agent 下拉 + 可手输路径 + 目录选择表单;关闭 location 后保留配置、停止扫描/监听并从会话与搜索结果排除,重新开启即增量扫回;纯路径管理不做内容校验。**Memory locations 页与 Locations 同版式**(2026-09-21 记忆可见层二期):标题 "Memory locations" + 一句说明 + 右侧 Add location + 页级 `…` 里的 Restore defaults;按 AgentId 声明序分组,一行一个记忆来源——各家默认的(Claude 的 auto-memory 树与 `~/.claude/CLAUDE.md`、Codex 的 memories 目录、逐线程库、`~/.codex/AGENTS.md` 与 `rules/`、Gemini 的 `~/.gemini/GEMINI.md`、ZCode 的记忆树)、项目根模式(`<project>/CLAUDE.md`、`<project>/.cursor/rules` 一类,原样显示占位符)与用户加的自定义目录/文件;路径为主信息、文件数(或 "Not found")为 muted 副信息,最右逐来源开关;`…` 菜单:默认来源只有 Show in Finder(项目模式连这个也没有),自定义来源多 Edit / Remove。添加/编辑与 Session locations 是同一个表单(`open_path_form`),差别是字段叫 "Folder or file"、选择器也收文件。改动不换 roster,只补一轮增量扫描,记忆流随之更新。Remote hosts 页沿用 Locations 的版式:标题 + 说明,右侧低强调的 Sync now / Add host;host 列表为单张 popover 底圆角卡,一行一台(名字为主信息、同步状态为 muted 副信息,失败用 danger),`…` 菜单集中 Sync now / Remove,最右为开关;添加走与 location 同材质的单字段表单弹窗,SSH 前提说明放在字段下方。列表与详情页的远程会话以 `@host` 徽章标识:列表行为 primary 淡底填充胶囊(与 muted 项目胶囊区分),详情页与 model/source 同排用 primary 描边。Connect 页沿用 Data 页的版式,**只放状态与动作,不放文档**(用户 2026-09-08 三轮定稿:平级双卡带代码块、嵌套同卡、缩进从属卡都试过,根源是设置窗里塞了 README 内容,怎么排都像教程):标题 + 一句说明(不点名 MCP、不写 read-only 声明);「MCP server」卡 84px 一行显示 wake-mcp 名称与 mono 字体的 `~/…` 路径,右侧 Copy path,不放"Ready"状态行,只在找不到二进制时给一行 danger 文案;「MCP clients」卡三行(Claude Code / Codex / Cursor),17px 品牌图标领行、客户端名为主信息、用法提示为 muted 副信息、右侧是 ghost 的 Show/Hide 切换(chevron 图标 + 文案,muted 前景,用户 2026-09-08 追加)加 Copy command / Copy config;代码块**默认收起**,展开后出现在该行下方、左缘对齐到文字轴(行内边距 16 + 图标 17 + 间距 12),secondary 填充 + 主题圆角的 mono 逐行渲染,Settings 重开即复位。0.6.x 起页面是**四个区块**(用户 2026-09-11 定稿):MCP server / MCP clients / Command line / Skill——MCP server / Command line / Skill 各一张 84px 信息卡、MCP clients 仍是三行客户端列表(wake-mcp、wake-cli 是二进制名与 mono 路径 + Copy path;wake-cli 那张在 Copy path **左侧**多一个 Copy command,复制「把它放进 PATH」的那行命令——右侧留给两张卡同名的 Copy path,纵向才对得齐;deb/tar 已装进 `…/bin` 或 Windows 上没有可粘命令时这个按钮不出现;Skill 是 `npx skills add …` + Copy command),区块标题为「Agents」时看不出这块特指 MCP,故改名 MCP clients 与上方 MCP server 成对。**说明性 caption 一律不放**——那正是"设置窗里塞 README"的复发形态;Setup guide 链接改挂在区块标题右端,规则是**每个面的第一个区块挂自己的文档**:MCP server → docs/mcp.md,Command line → docs/cli.md(MCP clients 与 Skill 分属这两个面的第二块,不重复挂)。四块之后整页**超过一屏、需要滚动**——2026-09-08 那条"一屏放下"的约束随第三、四个区块作废,滚动容器本来就在(`flex_1().min_h_0().overflow_y_scroll()`),卡片记得 `flex_shrink_0` 否则会被压扁。复制反馈是按钮原地变 "Copied"(check 图标)1.6s 后复原,**不弹通知**——gpui-component 的 toast 在窗口失活或被悬停时暂停自动关闭,在 Settings 从属窗里经常就挂着不走(用户 2026-09-08 反馈),且主界面的 Copy Session ID / Copy code 本来就不弹;页面只展示不代写别家配置。
- 工具条内的**状态行"常态沉默"**:仅刷新中或监听不可用时出现在按钮行上方;文案须可 truncate,窄侧栏放不下长句(故为 "Live updates off" 而非带操作建议的整句)。刷新中行首是与页头 Refresh 按钮 loading 态同一个 Spinner(12px、muted 色),文字下方 8px 处一条 3px 进度条(`Progress`,primary 色、轨道同色 20%;会话扫描按 done/total 定值,总数未知 / 记忆同步 / 远程同步走不定值滑动;2026-09-22 用户要的),出错时行首回到静态 refresh-cw 图标。
- 手动 Refresh 始终后台运行；进度复用侧栏状态行,完成后发通知("Sessions refreshed" / "Memory refreshed"),不用模态框阻断浏览、搜索或阅读。
- 不把项目包装成卡片,不堆叠分支、时间或重复图标。项目行不加彩色标识——同一图标重复十几次不传递信息。

### 会话流

- 顶部由 22px 上下文标题、会话总数角标和 icon-only 排序按钮组成；整个标题区固定为 88px，与左栏顶部身份区等高。标题和会话总数保持 2px 紧凑间距，并作为一个信息组在 88px 内整体垂直居中，禁止拆成两条 44px 行。排序按钮与信息组顶部对齐，沿用 16px 图标、透明 ghost 常态和 6px 圆角；当前排序字段和方向放在 tooltip 与菜单选中态中。
- 会话流固定 336px；顶部使用 22px 上下文标题、Label 11 会话数量和 icon-only 排序按钮。
- 会话行使用 Body 14 标题与 Label 11 元信息，共两行；行内 `SPACE_SM` 上下内边距 + `SPACE_XS` 两行间距。
- 标题严格保持单行；超长标题按 `unicode-width` 的中英文显示宽度提前截断并补 `…`，状态图标占用的尾部宽度必须预留，Hover 展示完整标题。
- 第二行固定为品牌图标 15px、项目名 badge、弹性空隙和右对齐的当前排序时间（按创建时间排序时显示创建时间，其他排序显示更新时间）；一分钟内显示 Just now，一小时内显示分钟数，当天显示时间，昨天显示 Yesterday，本年显示月日，更早补年份。Hover 统一显示精确到秒的本地时间。
- 按创建或更新时间倒序时，置顶会话单列 `Pinned`，其余按 `Today` / `Yesterday` / `Earlier this week` / 月份分组；跨年月份补年份。分组标题采用“标签 + 右侧低对比度 hairline”，不使用贯穿整栏的底边，既标明分界又避免表格感。按消息数或任何升序排列时保持平铺，避免分组语义与实际顺序冲突。
- 会话流每页读取 100 条，距当前已加载内容末尾 20 行时后台预取下一页；同值排序以 session key 稳定打破平局，跨页追加去重并重算分组。筛选或排序变化会让旧分页请求失效，失败的页停住等待用户刷新，禁止触底重试风暴。
- 全文搜索命中不受当前已加载页限制：若目标不在首批数据中，后台继续按页读取到目标，再通过平铺下标与分组 `IndexPath` 的映射完成选中和滚动。
- 分支、token 和归档状态不在列表重复展示，移入详情元信息。
- 收藏以 11px macOS 系统黄实心星、置顶以 11px 系统蓝实心图标出现在标题尾部。
- 当前行使用低饱和蓝材质，不额外描边。
- 会话流不重复提供全文搜索入口；列表内输入只筛选当前范围。

### 详情头部

层级从上到下为：

1. Agent、项目、分支等来源上下文与右侧操作工具条，共享一个 44px 高的 Flex 行；28px 操作条上下各保留 8px，不使用绝对定位。项目 badge 可点击并在文件管理器中打开项目目录；空值、`HEAD`、`detached` 和 `detached HEAD` 不作为有效分支显示。
2. 会话标题独占第二行；单行标题时该行最小 44px，与第一行合计 88px 并保持三栏基线对齐。标题保持 22px，过长时详情头自然向下扩展并完整换行，不截断内容；Hover 同时展示完整标题 tooltip。
3. 模型、来源（Via）badge 与消息数/token 共处标题下方第一行；统计文本弹性占据剩余宽度，窄窗口下单行截断。
4. 项目路径独占第二行。
5. 第三行以 12px 日期图标开头，创建与更新信息复用会话列表的智能时间，两者以留白和低对比度中点明确分组；Hover 对应时间时统一展示精确到秒的完整本地时间。

五层信息一项不减；标题下方的三行信息不压进来源与标题区域。元信息区与标题、底部分隔线均保留 8px，层间保持 8px，让来源、标题、运行统计、位置和时间上下文能被分别扫读。

“在终端继续”是唯一主按钮。收藏、置顶、拷贝会话路径保留为独立图标按钮；导出、Finder 和删除进入“更多”菜单。拷贝路径在“更多”左侧，成功后原地显示勾号与「已拷贝」提示 1.6s，不弹通知；数据库会话复制去掉会话 ID 后缀的共享数据库路径，远程会话复制本地镜像路径，tooltip 明确这两种情况。按钮圆角固定 6px，危险操作在菜单中用分组隔开，图标与文案统一使用 danger 红色，并继续走确认框。

详情正文使用 `popover` 阅读底色，与 `background` 详情头形成明确分区；阅读面横向铺满且不套大号圆角卡。内容限制在 720px 阅读宽度并居中。助手正文保持平铺，不在每条回复前重复 Agent 署名。只有 thinking、没有回复正文或工具调用的中间事件不进入阅读视图；带正文的 thinking 摘要保留，工具调用继续默认折叠。

详情加载失败不得退化为空白阅读面：没有匹配 adapter 时明确说明 agent 与会话路径不匹配，转录解析失败时保留底层错误链；两者共用居中的错误态，并提供“在文件管理器中显示”动作定位原始会话文件。异步解析结果只允许写回仍然选中的同一会话，避免快速切换时旧任务覆盖新详情。

对话框标题一律 Heading 16 semibold:组件内建 `.title()` 不设字号(实渲窗口默认 14px),必须显式补 `text_size(FONT_HEADING)`。破坏性确认的主按钮点名动作并用 danger 形态（常规删除使用平台化的 Move to Trash／Move to Recycle Bin；清理入口使用 Delete，确认框按用户指定使用 Confirm；正文明确恢复方式及空间回收条件）；表单弹窗内控件同档取齐(输入框与下拉/浏览钮同高,次级动作行才允许 small)。输入框聚焦态只把边框染成 ring 色,不画框外的聚焦环(theme.rs 关掉了 `focus_ring`):组件的环是元素框外 2px 的绝对定位子元素,弹窗内容层与滚动列表都会裁掉它,染色边框不占空间也不会残缺。

### 对话阅读面

正文横向铺在 `popover` 材质阅读面中，与 `background` 详情头用 hairline 分开，不再套圆角大卡。助手正文 13px、用户正文 13.5px，行高分别为 1.9 / 1.85，可选择、可滚动。Markdown 继续由组件原生渲染表格、引用块和分隔线；h1–h4 相对正文依次使用 1.45 / 1.28 / 1.14 / 1.05 倍字号，不拆分 `TextView`，以保留跨块连续选择。阅读区使用窗口级文本选择：Shift 可跨消息扩选，拖到视口边缘时自动滚动，复制按文档顺序合并所选文字。

代码块使用 `muted` 阅读底、`border` 描边、8px 圆角和 4px 网格内边距；右上角显示语言名和复制操作。tree-sitter 高亮必须显式选择 light/dark theme，且 `TextView` id 带当前模式，让主题切换走同步重建，禁止短暂残留上一模式的代码配色。

对话角色不使用常驻标题或 emoji：

- 用户使用靠右的低饱和引用块。
- 助手正文平铺，不在每条回复前重复 Agent 名称。
- 消息图片以 104px、10px 圆角的缩略图随正文排列；点击后使用全窗口暗色遮罩按原始宽高比预览，绝不放大超过源尺寸。预览胶囊集中显示格式、尺寸、文件大小以及复制/保存动作；无法直接预览的格式保留原始字节，并提供 Save image（系统「另存为」，与导出共用上次目录）入口。
- 仅有 thinking、没有正文或工具的中间事件不进入阅读视图；正式回复所带的 thinking 显示为折叠面板，收起是一行摘要，展开后显示完整原文。Thinking 与工具调用分别保存展开状态，禁止互相连带。
- Codex review 的 `ExitedReviewMode.review_output` 转为可读 Markdown，展示结论、说明、finding、代码位置与置信度；注入式 `<user_action>` 只作缺少结构化事件时的文本兜底，不显示 XML 外壳，也不与结构化结果重复。
- 工具调用合并为低强调折叠卡：单条收起时显示工具名与参数摘要，多条显示数量与名称序列，失败数常驻。摘要格数从当前阅读区像素宽反算，再用 `unicode-width` 截断。展开后显示可用的完整 Input，以及成功和失败 Output；失败结果使用 danger 色。Input/Output 面板最多展示前 600 个字符，但始终提供复制完整原文的操作，不引入嵌套滚动区。

emoji 不再承担界面或正文结构图标职责。

### 空态

详情空态是 360px 宽的阅读材质面：58px 图标圆面、Heading 16 主句和 Caption 12 说明，内边距 `SPACE_XXL`。

空态标题**陈述状态**，不喊口号("No session selected"，而非 "Find that conversation" 这类无指代对象的祈使句)；说明只留一句、给一个可执行动作并直接点名快捷键("Pick one from the list, or press ⌘K to search.")。空态不重复放搜索按钮。会话列表无结果同构("No matching sessions" + 清空筛选或更换条件)，尺寸更紧凑。

### 全文搜索

- 面板宽 680px，距窗口顶部 72px。
- 大尺寸无外框搜索输入置顶。
- 未输入与无结果状态高 250px；结果列表高 460px。
- 结果行使用品牌图标、标题、项目与时间、单行片段。
- 搜索始终覆盖全部会话；页脚左侧显示“搜索范围：全部会话”，右侧显示 `↑↓`、`↩`、`esc` 键盘路径。
- 指针回调中不得同步派发新的键盘事件。需要关闭旧浮层或转移输入焦点时，应通过对应组件 API 或延迟到下一事件周期处理，避免在 AppKit `mouseUp` 路径里重入 GPUI 事件分发；打开搜索面板前必须让 Root 先保存原焦点，关闭后 `⌘K` 才能继续生效。

### Insights

侧栏底部工具条的 Insights 钮(chart-column)进入;它是与全部导航行互斥的**整页目的地**——打开时替换会话流与阅读区,点任意导航行(或工具条的 Sessions 钮)退出并落回 All Sessions。页头右端有 Refresh。设置仍是独立场景,Insights 不是。

- 页面用 `background` 材质整片承载;顶部 88px 标题区与中栏同节奏(Insights 22px semibold + Label 11 副行,副行只说 "Since {首会话月份}"),兼窗口拖拽区。内容限 720px 阅读宽居中,区块之间只用 32px 留白与 Body 14 semibold 组头分隔——**不做卡片墙、零投影**,延续"避免每个区域加边框"的层级原则。
- 统计口径与主 UI 一致(archived 不计):"Prompts" 一律指主线用户消息。数据在打开与每次 Refresh 后后台重算,不阻塞浏览。
- 概览行:28px semibold 大数字 + Caption 标签,序为 Sessions / Tokens / Prompts / Agents / Projects / Active days(用户钉序,2026-08-27);Tokens 仅在有 agent 报过用量时出现。数字千分位。
- Last 7 days(概览下方):同序的三个度量(Sessions / Prompts / Active days;Tokens 是会话终身累计量、没有时间维度,不进周对比),值 Heading 16 semibold + Caption 标签 + Label 11 的相对变化行("+12% vs last week" / "Same as last week" / "New this week" / "No activity");上涨用 `primary`,持平与下降 `muted_foreground`,不引入第四种文字色。会话按创建日归属,prompts 按消息日,对比窗为滚动 7 天对前 7 天(不按自然周,免得周一早上全是零)。
- Over time(热力图下方):近 53 周每周 prompts 的堆叠柱,列宽与步距与热力图完全一致(9px 柱 + 3px 缝,左侧留出热力图的 26px 星期标签列),两图的周列上下对齐,月份刻度同规则。按总量取前五个序列各自着色自下而上堆叠:agent 用品牌色(theme.rs `agent_series_color`,彩色品牌取内嵌 PNG 主色、单色字形的六家给固定备选色,同一家在任何机器上不变),模型按排名取 `SERIES_PALETTE` 六色分类色板;其余合并为 "Other"(`muted_foreground` 35%)。曾试单色 `primary` 五档阶梯,用户否决(分不清)。零周留 2px `muted` 基线;柱按窗口内峰值周归一。只按 agent 分层、不按模型(messages 表没有逐条 model,会话级 model 是末态,切周会改写历史);caption 点名本周领先者("Prompts per week by agent · Claude Code leads this week")。图例在刻度下方:9px 色块 + Label 11 名称,可换行。tooltip 给 "Week of Aug 3 · 120 prompts" 与前三层明细。
- 活跃热力图:53 周 × 7 天(周一起始,最右列为本周),9px 方格 + 3px 缝,总宽 662px。热力格、分布柱和图例统一使用 `RADIUS_CELL` 2px 圆角；强度 = `muted` 空格 + `primary` 25/50/75/100% 四档(按窗口内峰值分位);未来日期留白。月份与 Mon/Wed/Fri 标签用 Label 11 muted;每格 tooltip 给 "N prompts · Aug 3, 2026"。底注左侧为 streak 与最忙一日(Label 11,` · ` 分隔),右侧 Less–More 固定满阶梯图例。
- 分布图:hour(24 柱)/ weekday(7 柱)/ month(12 柱)三个维度共用一张竖柱图,组头右侧 ‹ › ghost 按钮循环切换(纯视图状态,数据三份常驻不重查);峰值柱全饱和 `primary`、其余 55%,零值保留 2px `muted` 基线;柱数越少缝越大(4/8/6px)。hour 只标 6 小时锚点(靠左),weekday/month 每柱标签与柱居中;组头副行点出峰值("Most active around 2 PM" / "on Sundays" / "in August")。
- Agents / Projects / Models 三个榜单同构:24px 条形行 = 行首(品牌图 15px 原色 / folder 图标 / 无)+ 名称列定宽 truncate + 6px 圆头轨道条(`muted` 轨、`primary` 填充,按组内峰值归一)+ 右对齐 Label 计数。三个组头都挂 ‹ › 切换度量,循环序与概览行一致:Sessions / Tokens / Prompts;**当前档位名(首字母大写的裸名词)显示在两键中间**——64px 定宽居中,Caption muted,按钮位置不随文本跳动;榜单组头因此为单行(标题与按钮组居中对齐),分布图组头保留 caption 双行、按钮中间无标签(其标题本身就是档位名)。每个榜单各自记忆档位,行按当前度量降序重排后取 top-N(Agents 全量、Projects/Models 各 6;截断在排序之后,换度量不漏项);Tokens 档只列报过用量的组、值用 K/M 缩写,组内无人报 token 时该档不进循环。
- 空态沿用详情空态形制("No activity yet" + "Refresh sessions to see your activity here.");加载用居中 Spinner,已有数据时静默换新不闪烁。

### Memory

侧栏底部工具条的 Memory 钮(brain 图标)进入;与 Insights 同为**整页目的地**——打开时替换会话流与阅读区,工具条的 Sessions 钮落回 All Sessions。页头右端有 Refresh——**只同步记忆**(读 Settings → Memory locations 的当下配置,不重扫会话、不碰远程、不弹进度框;用户 2026-09-22 定"memory 的只刷 memory、session 的只刷 session"),按钮进行中转圈禁用。只读:agent 自己写下的记忆文档(Claude Code 的 auto-memory、Codex 的 memories 与逐会话摘要、ZCode 的项目记忆),列、搜(MCP / CLI)、读,不编辑、不同步、不删除,要改一个 Reveal 就到。

- **侧栏在这一页换成记忆的导航**(2026-09-21 用户定,"侧栏还是会话的内容"很怪):All Sessions / Starred 与 Agents / Projects 两块让位给 All Memory 一行(file-text 图标)+ User memory 一行(user 图标,会话侧栏 Starred 的同位:只列用户记忆,计数为零不挂徽章,再点一次回到全部;用户 2026-09-22 定"应该在 All Memory 下加一个 User memory 的菜单")+ Agents / Projects 两组(曾在顶上加过一行 brain + "Memory" 的模式标题,底部工具条能点亮当前页之后用户定没必要,已去掉),行与组头是会话侧栏的同一套组件、同一个单选模型(agent 与项目互斥,再点一次回到全部),计数按**记忆文件数**而不是会话数;Projects 按最近更新排,没归属的 "Unknown project" 垫底,**项目行只算项目记忆**(用户记忆对每个项目都成立,曾把它加进每个项目行,用户 2026-09-22 定拿掉、归到 User memory 一行;Agents 行仍含用户记忆);折叠状态与会话侧栏共用。底部工具条与搜索框原样保留,进页时筛选从全部开始。
- **版式与会话页同形**(2026-09-21 第七轮:整页宽页头压着两栏"看着很奇怪"):左列 = 会话流同宽(336px、`colors.list` 底)+ 列内 `library_header`(88px:标题 = 当前侧栏筛选——All Memory / User memory / agent 名 / 项目名,Label 副行 "N memory files",右端 Refresh)+ 记忆流;右侧 = 阅读面。
- **记忆流与会话流同一套分组**(2026-09-21 第九至十四轮:先做成按项目的可折叠树——chevron、收起、子行缩进、竖线——用户逐项否掉,改成平铺的项目组头;行上挂了项目徽章之后组头也多余了,最终定"和会话列表一样"):User memory 单独一组置顶(会话列表 Pinned 的同位——它对每个项目都成立,混进时间线"有点奇怪",用户同日定;侧栏选了 User memory 行时整页都是它,不画这个组头、照时间分),其余按更新时间倒序,Today / Yesterday / Earlier this week / 月份的分割线(`session_group_label` 同一函数),Label 11 medium muted + 右侧低对比度 hairline,32px 高;文档行是标准行盒,不缩进,项目由行内徽章说明。行盒模型同会话行:高亮面 `SPACE_SM` 外距,内缩 = ListItem 自带的 12/4 加行内容的 4/8(没有 ListItem,两层加成一层写 16/12),文档行两行(Body 14 medium 标题一行,按显示宽度截断、Hover 全名;Label 11 第二行 = 品牌图 15px + 项目徽章(会话行同款 muted 胶囊,文案:项目名 / User memory / Unknown project(层级词沿用 Claude Code 文档的 User memory / Project memory,用户 2026-09-21 定)——曾因组头已说明而不挂,用户 2026-09-21 定要与会话行一致)+ 线程级的 "session memory"、远程的 `@host`(指令文件不另挂徽章——文件名已说明身份,"instructions" 徽章试过、用户定拿掉)+ 弹性空隙 + 右对齐更新时间,Hover 精确时间),选中 `list_active`、悬停 `list_hover`。列表 gpui::list 虚拟化,刷新保滚动位置、换筛选回顶部。Unknown project 只剩目录已不在磁盘上的那几份:Claude 项目目录没有会话时按目录名在本机文件系统上反推路径(`decode_project_dir`,只对默认根)。
- 阅读面头部是详情头部同款:44px 上下文行(品牌图 15 + agent 名 + 可点击的归属徽章(在文件管理器中打开项目),右端 Reveal / Copy path 两个 16px ghost 图标钮)→ 22px semibold 标题(可换行,最小 44px)→ Label 11 muted 两行元信息(file-text 图标 + `~` 折叠的文件路径;calendar 图标 + "Updated …",Hover 精确时间),`background` 底、hairline 收底、兼窗口拖拽区;正文 `popover` 底、720px 阅读宽居中、24px 阅读轴、上留 16 下留 24(与消息流首末条同数),与消息正文同一套 Markdown 渲染(相对链接按记忆文件所在目录解析)。
- 列表非空时恒有选中项(重载落到第一份),正文后台读磁盘、读到前居中 Spinner,文件被 agent 改过重读。空库时左列只有页头,右侧 `empty_state_card`("No memory files yet" + 一句说明它们从哪来)。

### 本机会话清理

- Filter 使用 16px 内边距，标题、日期区、来源区与操作栏共用左缘。标题右侧提供「仅显示可清理」多选选项，默认关闭并随其他条件一起记忆、清除。Clear（悬停提示 Clear filters）与 Done 集中到底栏，均为 32px 高、6px 圆角，复用 Settings 的普通／主要按钮样式；常态实色填充，前者 secondary、后者 primary，Done 最小宽度 80px。

- 清理入口 Delete 与最终 Confirm 按钮统一使用现有 danger 样式；取消选择、筛选与排序仍使用各自的普通控件。检查阶段显示已检查数 / 总数并可取消，执行阶段显示已处理数 / 总数并允许完成当前会话后停止。取消检查不产生确认框或清理记录。
- 确认框标题用 Delete this session?／Delete N sessions?，顶部仅接一行 Caption／muted 的 Local files · About X 体积摘要，随后直接显示会话列表。两条短说明固定在列表下方、按钮上方，与列表共用左缘：先说明可从废纸篓恢复及释放空间的条件，再说明原工具可能无法继续会话。说明区顶部的分隔线固定，清单内仅在会话之间分隔；说明与按钮间距 16px，不随文件列表滚动。移除重复的长段落和清理页中额外的 Codex 说明；文件名使用 12px muted 等宽文字，过长时省略，悬停可见完整路径。只有一项时使用单数文案。结果区把未开始、已移动、需要处理分开，未开始不标红；没有移动文件时不提示空间待回收。

- 入口在侧栏底部工具条左端，排在 Sessions / Memory / Insights 三颗页切换之后（见「资料库侧栏」）；清理使用 14px `brush-cleaning` 图标，打开时用侧栏选中色做圆角底表达激活态。
- 清理页保留侧栏，主区与 Insights 共用 `background` 页面底色及 24px 水平留白。标题通过 `library_header` 与 All Sessions、Insights 共用：88px 高、22px 标题、11px 副行、2px 行间距并支持窗口拖拽，Filter、Sort、刷新对齐标题首行右侧；不放装饰图标、独立体积大字或嵌套卡片。
- 标题下直接进入列表，不设置内容分类标签或独立工具栏。Filter 内容宽 468px，使用单层弹出面板：两项日期各用一行完整时长选项，沿用 Appearance 的中性分段控件，选中项用 popover 底色与轻阴影；Exclude 以独立标题与下方等宽选项呈现收藏/置顶/无法清理排除，标题右侧不放筛选条件。日期与排除条件固定可见。Agent、项目与 Exclude 统一使用原 Agents 的选项块样式：28px 行高、12px 文字，选中时浅蓝底色与右侧勾号，未选中时使用浅中性底色和细边框，不使用方形复选框。Agent 的 15px 品牌图标位于名称左侧。来源区域以分隔线分组，Sources 标题右侧放 284px 宽搜索框，来源选项单独滚动。输入来源搜索词不改变会话范围，点选后立即生效且面板保持打开。创建时间与更新时间各自提供完整时长快捷项，组合时取交集；副标题同时显示生效条件。旧单日期偏好迁移到对应项，另一项不限。候选无预设大小或对话轮数门槛，旧内容分类偏好加载时忽略。
- 收藏与置顶默认不排除，选中的排除条件同步显示在页面副标题；时间门槛完全由时间筛选决定，不另设隐藏的 7 天保护。可清理和无法清理的会话统一显示，参与同一套创建／更新日期、来源、收藏和置顶筛选以及排序。无法清理项禁用勾选，标题与品牌图保持正常显示，来源信息后直接呈现简短原因，完整说明通过悬停查看，保留预览；不再提供额外展开入口或原因弹窗。大小未知显示「—」，按大小升降序均排在最后，统计只累加可清理体积。Filter 可显式开启「仅显示可清理」。所有计数均为当前筛选范围；全选只选择可清理会话。
- 列表头保持 44px 单行：全选（可清理数量）、筛选结果总数、无法清理数量；可清理体积放在未勾选时的底部摘要，不重复状态词。两种会话共用 `cleanup/rows.rs` 的标题／来源两行布局，无法清理原因使用 Label 字阶接在项目徽章后；完整项目名与原因支持悬停查看。项目徽章上限 128px，品牌图保持 15px。每行占 64px，内部高亮面为 60px，上下各留 2px，避免相邻选中行连成大色块。日期列 112px，仅显示当前排序日期（按文件大小排序时，只有创建时间启用则显示创建日期，否则显示更新日期），悬停显示创建和更新的完整时间。体积列 88px，数字右对齐；移除常驻操作列，将空间留给会话标题；不加体积条。行高亮留 16px 外边距，内容内缩 8px，沿用 8px 主题圆角；勾选使用 `list_active`，悬停使用 `list_hover`。
- 清理记录直接打开主区历史列表，不用时间戳菜单；列表显示会话标题、品牌、时间、会话数、结果与体积。单次详情与列表形成明确返回层级，标题左侧返回按钮和 Esc 均可返回。标题复用 88px 页面头，正文遵循 Insights 的居中 720px 内容宽度；记录列表复用 Settings 的圆角分组面板，88px 行内以品牌、标题和时间为主，体积与轻量状态标记靠右。详情顶部展示结果摘要，下面的会话面板用 40px 整行控件展开文件位置；已还原条目不再显示废纸篓或恢复操作，部分完成、中断与待核对单独表达。纯浏览保留候选列表筛选、勾选和滚动，执行或恢复后回候选页才重新核对文件状态。
- 清理页、Filter、结果页和确认框的文字操作统一复用 `ui::action_button`：32px 高、左右 12px、6px 圆角，与 Settings 主操作共享尺寸；内部文字、图标和图文间距统一使用 Settings 常规按钮的 Small 档，避免仅改外框却遗留默认字号。页面主操作同排等高；历史页返回按钮为 32px，文件位置使用 40px 整行展开控件，顶部 Filter／Sort／Refresh 沿用 All Sessions 的图标按钮尺寸。
- 底部与侧栏工具条统一为 44px（包含顶部 hairline），摘要使用 11px Label 字阶。未选择时显示可清理体积及清理记录，无可清理项时显示对应引导；选择后原位显示数量、体积和单词按钮 `Clear`／`Delete`；按钮使用纯文字，未选择时显示纯文字 `History`；悬停分别说明取消全选和移到系统废纸篓。点击会话标题在主区域进入现有正文阅读器，保留侧栏，不使用弹窗或第二列；阅读器来源行增加返回按钮，也可按 Esc 返回，勾选、筛选和列表位置保持。进入清理页不自动加载正文。源文件明细归入清理确认框，每个会话显示品牌、标题、项目、数量与体积，文件数量按钮按需展开文件名、父目录和大小；长路径截断并支持悬停。结果页共用页面标题与 24px 留白；空状态复用现有 `empty_state_card`。
- `Delete` 先检查所选会话，再打开最终确认清单，只有确认框里的 Confirm 才执行。检查逐项汇总失败原因；部分失败时明确显示将跳过的项及真正将清理的数量与体积，全部失败时展示原因与刷新入口，不提供删除按钮，不用底部裸错误取代确认流程。
- Sort 与 All Sessions 使用相同菜单结构、180px 最小宽度和右对齐锚点：上半部分为更新时间、创建时间、文件大小，分隔线下方为降序、升序。字段和方向各自勾选，tooltip 沿用“排序：字段 · 方向”。切换字段保留当前方向；切换字段或方向都保留已选会话，改变筛选条件才清空勾选。

## 图标、形状与层次

- UI chrome 只使用内嵌 Lucide 单线 SVG，不使用 Unicode 或 emoji 图标；Agent 身份用内嵌品牌 PNG，经 `img()` 渲染并**保持原色**(不得用 `text_color` 着色,选中态也不变色)。
- 品牌 PNG 登记在 `assets.rs` 的 `brands!` 宏,文件名 = `AgentId::as_str()`,路径含 `.png`。入库前须裁掉透明边并保持正方形；带白色/彩色底的 app-icon 风格图必须先抠底,否则在侧栏材质上会露出白方块。
- 品牌图标侧栏分组项 18px、内容区(列表/搜索/详情)15px；Lucide 行内图标 13–15px；主操作与工具栏图标 14–16px；空态图标 22–26px。
- 面板圆角 12px，列表与侧栏选择 8px，按钮固定 6px，快捷键标签 5px，badge 胶囊 4px，数据格 2px。代码里前两档走 `theme.radius_lg` / `theme.radius`，其余引用 `ui.rs` 的 `RADIUS_BUTTON` / `RADIUS_KBD` / `RADIUS_BADGE` / `RADIUS_CELL`——不要再写裸数字。
- 常驻界面零投影、零渐变、无装饰性描边。菜单、命令面板、确认框和通知由组件库提供浮层阴影。
- 相邻的自定义材质只用一套 token 和圆角语言，避免每个按钮各自模拟玻璃。

## 可访问性与桌面交互

- 不依赖颜色单独传达 Agent 或状态；品牌点/品牌图标旁必须出现 Agent 名称。
- 所有主要操作必须同时有指针和键盘路径；全文搜索为 `⌘K`，全量刷新为 `⌘R`。
- 控件使用 tooltip；菜单项使用“动词 + 对象”的完整英文标签(如 "Refresh Sessions")。
- 双模式使用同一语义结构，只有 token 值变化。
- 最小窗口宽度必须保证标题、主操作和更多菜单不互相挤压。

### 界面缩放(2026-10-08,issue #51)

- 档位 100 / 110 / 125 / 150%,默认 100,只放大不缩小(用户 2026-10-08 定,去掉了 90 与 175);⌘+ / ⌘− / ⌘0(其他平台 Ctrl)、显示菜单的 Zoom In / Zoom Out / Actual Size、Settings → Appearance 的 Zoom 是同一个设置,两扇窗一起变。Appearance 是独立的设置页(Theme + Zoom 两行;主题行叫 Theme,页才叫 Appearance),不放 General(用户同日定)。快捷键在任何焦点下都有效(弹窗里、下拉刚关、设置窗在前);到头时什么也不做。
- **整体等比放大**:字号、间距、圆角、图标、行高、栏宽都乘同一个倍数——100% 档的设计稿就是唯一的设计,本文所有尺寸都指 100% 档。只有三类不跟着放大:1px 发丝线、窗口尺寸(最小尺寸 940×620 只在开窗时定)、traffic lights 本身的大小。
- traffic lights 跟着挪位:红灯中心压在放大后的侧栏中轴(26.75 × 倍数)上、在放大后的顶部净空里垂直居中——放大后的界面与 100% 档同构,中轴关系不变。
- 窗口不够宽时两条定宽栏(侧栏、会话流)按比例收窄,给阅读区留 380,最窄退回 100% 档的宽度;设置窗的侧栏同理给内容区留 540。弹窗宽度与 ⌘K 面板高度夹在窗口之内。窄栏里放不下的内容一律截断(头部徽章、侧栏 agent 名),不允许互相压盖。
- 换档时对话流与 Memory 列表按比例重量行高,阅读位置不跳。
- 设置窗不随档位改变自己的大小(开着时换档,150% 下 820×600 只够放 100% 的三分之二内容),所以设置卡片放不下时**右侧控件换到标题下一行**、标题列至少 180;设置各页(General / Appearance / Data / Connect / Updates 走同一个页骨架)与设置侧栏都可滚动。Insights 的概览大数字放不下就换行;热力图与趋势图共用的周格在栏宽放不下 53 周时等比收窄格子(标签列不缩),一年始终完整、两图周列仍对齐。

## 实现守则

- 尺寸一律经 `ui::Zpx`(常量)/ `ui::zpx(…)`(表达式)取值,不要再写裸 `px(…)`——裸 px 不会跟着缩放;只有 1px 发丝线、零值、已按倍数算好的实际宽度与测试例外。`ui.rs` 的 `bare_pixel_sizes_do_not_creep_back` 按文件卡裸 px 的个数,只许减不许增。

- 先改标准结构和控件，再添加自定义材质面。
- 颜色只改 `theme.rs`；图标必须登记到 `assets.rs`，路径包含后缀(`.svg` / `.png`,漏后缀 = 静默空白)。
- 所有交互元素先设置 `.id()` 再绑定点击或滚动行为。
- 每个窗口根节点在内容之后必须挂 `ui::overlay_layers(window, cx)`(封装 `Root::render_dialog_layer`、`Root::render_notification_layer` 与"点面板外关闭"的 sentinel,顺序即契约);普通弹窗经 `ui::open_closable_dialog` 打开,确认类用 `ui::open_alert`(两者都收设计稿宽度:随缩放放大、夹进窗口,内边距也随缩放;不要裸调 `window.open_dialog` / `open_alert_dialog`)。
- 对原始 Agent 数据目录继续只读；任何视觉改造不得破坏刷新、搜索跳转、恢复或删除语义。
- 术语统一:用户可见文案一律说 **Refresh** 与 **Session**,不出现 scan / rescan / rebuild / index(这些只保留在数据层内部命名中)。

## 验收

```bash
cargo build -p wake
scripts/build_and_run.sh --verify
```

视觉验收至少覆盖：空态、选中会话、详情阅读、更多菜单、`⌘K` 搜索，以及系统浅色和深色模式。


### 清理操作按钮规范（2026-09-16）

- 清理页文字操作统一采用纯文字按钮，包括 Clear、Delete、History、Filter 的 Clear／Done、失败后的 Refresh，以及历史恢复操作。
- 确认框为 Cancel／Confirm，保留 danger 层级；底部说明也使用纯文字，与列表左缘对齐。
- 顶部筛选／排序／刷新、返回和文件展开的功能图标，以及 Agent 品牌图标保留。
- Settings 更新动作沿用 refresh-cw／download 图标。

### 清理来源多选（2026-09-16）

- Filter 的 Agents、Projects 都支持多选，组内取并集、组间以及日期条件取交集；未选任何值表示该组不限。
- 多选统一采用原 Agents 的轻量选项块：28px 行高、12px 文字，选中时浅蓝底色与右侧勾号，未选中时使用浅中性底色和细边框；不使用方形复选框。Agents、Projects 与 Exclude 共用这一组件。Agent 图标保留在名称左侧，点击已选项可取消；标题显示整组选中数量，来源分组标题右侧的搜索只影响候选项显示。
- 不另设 All 选项，取消该组所有勾选即恢复不限；Clear 清除全部筛选。筛选在关闭面板、重新进入清理页和重启后保留，旧单选设置自动迁移。
- 页面摘要单项显示名称，多项显示分组数量；已选但数据暂时不可用的来源仍可取消。

### 清理记录的手动恢复（2026-09-16）

- 不提供应用内自动 Restore。macOS 的废纸篓权限会阻止目录读取，临时测试文件通过不能代表用户已有目录可访问。
- 顶部结果卡片在说明下方放置纯文字 Trash／Check，保持 32px 高、12px 水平留白和 6px 圆角。说明明确“在废纸篓选择放回原处，再点击检查”；两按钮与说明左对齐，不用页面底栏。
- Trash 打开系统废纸篓；Check 只验证原位置的会话及关联文件，完整后清除本次删除标记、重新扫描。Check 不读废纸篓、不移动文件。文件尚未放回时给普通操作提示，不显示权限栈或声称已恢复。
- 旧自动恢复的权限错误不把已经成功的删除显示成部分完成，改用手动恢复说明；其他实际文件或索引错误仍展示。全部重新收录后隐藏操作。
