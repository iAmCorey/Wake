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

**间距刻度(4px 网格)**:`SPACE_XS/SM/MD/LG/XL/XXL` = 4/8/12/16/20/24,定义在 `ui.rs`。新代码引用常量或显式 `px()`;**对齐敏感处禁用 rem 间距类**——`p_2p5` 实为 8.75px、`p_3` 实为 10.5px(rem=14 折算),均不在网格上,是对齐失真的来源。存量 rem 类已于 2026-08-24 全量迁移完毕,代码中不再允许出现 rem 间距类(`p_0` 例外,零无幽灵值)。

**侧栏中轴(x = 26.75)**:traffic lights 定位 (20,11),红灯实测直径 13.5px,中心即 **26.75**。侧栏所有行首元素的**视觉中心**压在这条竖线上,而非左缘对齐:

| 元素 | 常量 | 值 | 推导 |
|---|---|---:|---|
| 容器内边距 | `SIDEBAR_EDGE` | 10 | 行 hover/选中胶囊的左右留白 |
| 行首槽位 | `LEAD_BOX` | 18 | 最大前导元素(品牌图)尺寸,槽内**居中** |
| 行左内边距 | `LEAD_INSET` | 7.75 | 26.75 − 9(槽位半宽) − 10 |
| 组头左内边距 | `GROUP_HEAD_INSET` | 12.125 | 由首字母字形中心反推(Body 常规下 A 宽 8.25、P 宽 7.25,两者中心恰好重合) |
| 标题左内边距 | `TITLE_INSET` | 9 | 由 "Wake" 的 W 反推(Heading semibold,W 宽 14.25、左承距 0.5) |
| 分组项缩进 | `SUB_INDENT` | 12 | 分组项行首中心落在 38.75,表达从属 |

三条硬约束:

1. **中心对齐与左缘对齐是同一个自由度,只能满足一个。** 选了中心,18px 品牌图的左缘就落在 17.75,比红灯左缘还靠左 2.25——这是预期结果,不是错位。同理分组项一旦缩进就不再压轴。
2. **`GROUP_HEAD_INSET` / `TITLE_INSET` 不是间距,是从字形宽度反推的值**,组头或标题的字号、字重一改立即失效,必须重新实测字形再算(11px semibold 时需 13.25/13.0,Body 常规时变成 12.125)。
3. **2x 屏光栅化步长是 0.5px,别往小数点后继续调。** 实测 `GROUP_HEAD_INSET` 取 12.125 落位 −0.125,改成 12.25 反而跳到 +0.375——两者落进不同物理像素。全部元素落在 ±0.375 以内即为达标。

**行高两级**(侧栏纵向层级的来源):主导航 `ROW_HEIGHT` 32px + Body 14,分组展开项 `ROW_HEIGHT_SUB` 26px + Caption 12。圆角均 8px。

UI 语言英文;会话正文保持原语言。元信息分隔符固定为前后带空格的 ` · `。

## 组件规范

### 窗口顶部

macOS 不设置横跨三栏的自定义 header。主窗口透明标题栏高 44px，系统交通灯在其中垂直居中；其所在拖拽区与侧栏使用完全相同的材质和颜色。会话流与阅读区不为标题栏预留另一条色带。Windows 保留系统原生标题栏（贴靠布局与深色模式经 DWM 处理）；Linux 由 compositor 装饰协商决定，报 Client 时侧栏顶部挂自绘 TitleBar。

### 资料库侧栏

- 侧栏顶端按红绿灯、`Wake` 标题和搜索框的可见边界做光学对齐：标题容器上留 4px、下留 16px。窗口控制区和品牌行各高 44px，合计 88px。
- 顶部是唯一的全文搜索入口,文案 "Search sessions",右侧显示 `⌘K`;Search/All Sessions/Starred 固定不随滚动。
- 搜索行必须有防溢出结构:标签文字 `flex_1 + min_w_0 + truncate`,图标与 `⌘K` 徽标显式 `flex_shrink_0`。裸文字子元素的最小宽度被内容锁死,侧栏一窄就会把右侧元素挤出边界裁掉。
- **行分两级**(侧栏纵向层级的来源,不得拉平):主导航 All Sessions/Starred 32px 行高 + Body 14;分组展开项(agent/项目)26px 行高 + Caption 12 + 整行右移 `SUB_INDENT` 12px 表达从属。
- **每行必须有行首元素**,由 `RowLead` 枚举强制(`Icon` 或 `Brand` 两态,无 `None`):主导航用 Lucide 单线图标,agent 行用品牌 PNG,项目行用 `folder.svg`。槽位定宽 `LEAD_BOX` 管右侧文字起点统一,槽内居中管中轴对齐。
- 线条图标比实心品牌图视觉轻,同档里给小一号:分组项 Lucide 14 / 品牌图 18,主导航 Lucide 15。
- 行内容 = 行首元素 + 标题 + 计数;计数一律 Label 档 muted。
- 组头 "Agents"/"Projects" 用 Body 档常规字重 + muted 色(与主导航同字号同字重,仅靠颜色和"无行首图标"区分——加粗会让组头压过它统辖的行),带 13px chevron 可折叠。
- 底部工具条常驻,总高 44px（含顶部 1px hairline）,按钮靠右排列(次要操作区:透明底、hover 才出色,不与导航行选中态抢注意力)——依次为 chart-column "Insights"、齿轮 "Settings"、refresh。Insights 页打开时其图标以 primary 点亮,是工具条里唯一有激活态的按钮。Settings 同时进入 Wake 菜单并绑定 `⌘,`(其他平台 `Ctrl+,`),保持单例窗口。
- Settings 默认 820×600，采用 180px 窄侧栏 + 内容页结构，固定为 General / Locations / Remote hosts / Data / Updates / About 六项；About 与功能设置分离并钉在侧栏底部，Wake 菜单的 About Wake 直达同一页。About 沿用 Kooky/Birth 的信息顺序：产品图标、名称、版本、tagline、GitHub、短分隔线、版权/许可证与作者署名。Updates 是独立功能页,仅在用户点击页面按钮或 macOS Wake 菜单的 Check for Updates 时读取 GitHub 最新正式 Release 元数据,明确呈现检查中/最新版/有新版/失败四种状态;有新版时打开 Release 页供用户下载,不后台检查、不自行覆盖应用包。内部常规文字按钮统一沿用主界面的 24px 高、6px 圆角和主题交互色,普通页面动作使用 muted 填充 + hairline；发现新版后的 View Update 是需要用户继续完成的主操作,使用 32px 高 primary 填充和轻阴影。Appearance 分段选择器也使用同一材质。General 只放真实可用的全局偏好,当前为持久化的 System / Light / Dark 外观选择;不提供默认 “Open In” 终端。Data 只展示 Wake 本地存储位置、会话数与磁盘占用并提供文件管理器入口,不重复放刷新或清库动作；常规 Refresh 的唯一入口仍是主侧栏底部。Locations 页按 AgentId 声明序以 agent 分组,品牌名只在组头出现一次;本机有数据的组优先,未检测到的 agent 默认收进可展开区。每条路径以路径为主信息、会话数/不可用状态为 muted 副信息,最右为逐路径开关;停用时只降低文字层级，开关与菜单保持完整对比度。行本身不承担编辑,`…` 菜单集中 Edit / Show in Finder / 自定义 Remove。顶部操作为低强调的 Add location,Restore defaults 收进页级 `…` 菜单且无偏离时禁用。添加/编辑仍复用 agent 下拉 + 可手输路径 + 目录选择表单;关闭 location 后保留配置、停止扫描/监听并从会话与搜索结果排除,重新开启即增量扫回;纯路径管理不做内容校验。Remote hosts 页沿用 Locations 的版式:标题 + 说明,右侧低强调的 Sync now / Add host;host 列表为单张 popover 底圆角卡,一行一台(名字为主信息、同步状态为 muted 副信息,失败用 danger),`…` 菜单集中 Sync now / Remove,最右为开关;添加走与 location 同材质的单字段表单弹窗,SSH 前提说明放在字段下方。列表与详情页的远程会话以 `@host` 徽章标识:列表行为 primary 淡底填充胶囊(与 muted 项目胶囊区分),详情页与 model/source 同排用 primary 描边。
- 工具条内的**状态行"常态沉默"**:仅刷新中或监听不可用时出现在按钮行上方;文案须可 truncate,窄侧栏放不下长句(故为 "Live updates off" 而非带操作建议的整句)。
- 手动 Refresh 始终后台运行；进度复用侧栏状态行，完成后发通知，不用模态框阻断浏览、搜索或阅读。
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

“在终端继续”是唯一主按钮。收藏、置顶保留为独立图标按钮；导出、Finder 和删除进入“更多”菜单。按钮圆角固定 6px，危险操作在菜单中用分组隔开，图标与文案统一使用 danger 红色，并继续走确认框。

详情正文使用 `popover` 阅读底色，与 `background` 详情头形成明确分区；阅读面横向铺满且不套大号圆角卡。内容限制在 720px 阅读宽度并居中。助手正文保持平铺，不在每条回复前重复 Agent 署名。只有 thinking、没有回复正文或工具调用的中间事件不进入阅读视图；带正文的 thinking 摘要保留，工具调用继续默认折叠。

详情加载失败不得退化为空白阅读面：没有匹配 adapter 时明确说明 agent 与会话路径不匹配，转录解析失败时保留底层错误链；两者共用居中的错误态，并提供“在文件管理器中显示”动作定位原始会话文件。异步解析结果只允许写回仍然选中的同一会话，避免快速切换时旧任务覆盖新详情。

对话框标题一律 Heading 16 semibold:组件内建 `.title()` 不设字号(实渲窗口默认 14px),必须显式补 `text_size(FONT_HEADING)`。破坏性确认的主按钮点名动作并用 danger 形态("Move to Trash",Windows 上经 trash_copy! 平台文案为 "Move to Recycle Bin",不留裸 "OK");表单弹窗内控件同档取齐(输入框与下拉/浏览钮同高,次级动作行才允许 small)。输入框聚焦态只把边框染成 ring 色,不画框外的聚焦环(theme.rs 关掉了 `focus_ring`):组件的环是元素框外 2px 的绝对定位子元素,弹窗内容层与滚动列表都会裁掉它,染色边框不占空间也不会残缺。

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

侧栏底部工具条的 chart-column 按钮进入;它是与全部导航行互斥的**整页目的地**——打开时替换会话流与阅读区,点任意导航行(或再点入口)退出并落回 All Sessions。设置仍是独立场景,Insights 不是。

- 页面用 `background` 材质整片承载;顶部 88px 标题区与中栏同节奏(Insights 22px semibold + Label 11 副行,副行只说 "Since {首会话月份}"),兼窗口拖拽区。内容限 720px 阅读宽居中,区块之间只用 32px 留白与 Body 14 semibold 组头分隔——**不做卡片墙、零投影**,延续"避免每个区域加边框"的层级原则。
- 统计口径与主 UI 一致(archived 不计):"Prompts" 一律指主线用户消息。数据在打开与每次 Refresh 后后台重算,不阻塞浏览。
- 概览行:28px semibold 大数字 + Caption 标签,序为 Sessions / Tokens / Prompts / Agents / Projects / Active days(用户钉序,2026-08-27);Tokens 仅在有 agent 报过用量时出现。数字千分位。
- 活跃热力图:53 周 × 7 天(周一起始,最右列为本周),9px 方格 + 3px 缝,总宽 662px。热力格、分布柱和图例统一使用 `RADIUS_CELL` 2px 圆角；强度 = `muted` 空格 + `primary` 25/50/75/100% 四档(按窗口内峰值分位);未来日期留白。月份与 Mon/Wed/Fri 标签用 Label 11 muted;每格 tooltip 给 "N prompts · Aug 3, 2026"。底注左侧为 streak 与最忙一日(Label 11,` · ` 分隔),右侧 Less–More 固定满阶梯图例。
- 分布图:hour(24 柱)/ weekday(7 柱)/ month(12 柱)三个维度共用一张竖柱图,组头右侧 ‹ › ghost 按钮循环切换(纯视图状态,数据三份常驻不重查);峰值柱全饱和 `primary`、其余 55%,零值保留 2px `muted` 基线;柱数越少缝越大(4/8/6px)。hour 只标 6 小时锚点(靠左),weekday/month 每柱标签与柱居中;组头副行点出峰值("Most active around 2 PM" / "on Sundays" / "in August")。
- Agents / Projects / Models 三个榜单同构:24px 条形行 = 行首(品牌图 15px 原色 / folder 图标 / 无)+ 名称列定宽 truncate + 6px 圆头轨道条(`muted` 轨、`primary` 填充,按组内峰值归一)+ 右对齐 Label 计数。三个组头都挂 ‹ › 切换度量,循环序与概览行一致:Sessions / Tokens / Prompts;**当前档位名(首字母大写的裸名词)显示在两键中间**——64px 定宽居中,Caption muted,按钮位置不随文本跳动;榜单组头因此为单行(标题与按钮组居中对齐),分布图组头保留 caption 双行、按钮中间无标签(其标题本身就是档位名)。每个榜单各自记忆档位,行按当前度量降序重排后取 top-N(Agents 全量、Projects/Models 各 6;截断在排序之后,换度量不漏项);Tokens 档只列报过用量的组、值用 K/M 缩写,组内无人报 token 时该档不进循环。
- 空态沿用详情空态形制("No activity yet" + "Refresh sessions to see your activity here.");加载用居中 Spinner,已有数据时静默换新不闪烁。

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

## 实现守则

- 先改标准结构和控件，再添加自定义材质面。
- 颜色只改 `theme.rs`；图标必须登记到 `assets.rs`，路径包含后缀(`.svg` / `.png`,漏后缀 = 静默空白)。
- 所有交互元素先设置 `.id()` 再绑定点击或滚动行为。
- 每个窗口根节点在内容之后必须挂 `ui::overlay_layers(window, cx)`(封装 `Root::render_dialog_layer`、`Root::render_notification_layer` 与"点面板外关闭"的 sentinel,顺序即契约);普通弹窗经 `ui::open_closable_dialog` 打开,确认类用 `open_alert_dialog`。
- 对原始 Agent 数据目录继续只读；任何视觉改造不得破坏刷新、搜索跳转、恢复或删除语义。
- 术语统一:用户可见文案一律说 **Refresh** 与 **Session**,不出现 scan / rescan / rebuild / index(这些只保留在数据层内部命名中)。

## 验收

```bash
cargo build -p wake
scripts/build_and_run.sh --verify
```

视觉验收至少覆盖：空态、选中会话、详情阅读、更多菜单、`⌘K` 搜索，以及系统浅色和深色模式。
