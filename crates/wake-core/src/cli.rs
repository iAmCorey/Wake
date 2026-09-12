//! wake-cli 的解析层:argv → 一次 MCP 工具调用。bin 只做 I/O 与退出码,有
//! 判断的东西全在这里、可单测。文案恒英文(wake-core 不参与 i18n,别包 t())。
//!
//! **值一律原样当字符串塞进 JSON**:数字不解析、范围不裁剪、agent 名不认、
//! since 文法不懂——`mcp::tools` 的 int_arg 认字符串、agents_arg 认逗号串,
//! 校验与裁剪只此一家。同一个坏参数在 CLI 与 MCP 两条路上才会给出同一句话,
//! 两条路的输出也才可能逐字节相同(tests/cli.rs 卡着)。
//!
//! COMMANDS 表与 `tools::definitions()` 必须双射,单测卡死:工具新增参数、
//! 改名、改类型、加第五个工具,这个文件都会红。

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::mcp::tools;
use crate::models::AgentId;
use crate::services::terminal::sh_quote;

// ---------------------------------------------------------------- 命令表

/// 取值检查函数:出错时返回给用户看的整句
pub type Validator = fn(&str) -> Result<(), String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arity {
    /// 取一个值,重复出现后来者覆盖
    Value,
    /// 可重复;最后以逗号串一次性写入(agents_arg 自己切分/trim/去重/校验)
    Repeated,
    /// 出现即 JSON true;**不出现就不写这个键**,让工具的默认值生效
    Switch,
}

#[derive(Debug, Clone, Copy)]
pub struct FlagSpec {
    /// 含前导 `--`
    pub long: &'static str,
    /// MCP 工具的参数名,必须与 inputSchema 的 property 同名(单测卡)
    pub key: &'static str,
    pub arity: Arity,
    /// 帮助里的值占位符;Switch 用 ""
    pub placeholder: &'static str,
    /// 帮助里的说明。**不写默认值/取值区间的数字**——那些由 definitions() 现渲染
    pub help: &'static str,
    /// 取值检查。**CLI 仅有的值语义只能长在这里**:别在 parse 里按 key 名写
    /// `if`,那样这张表就不再是 CLI 表面的完整描述,加第二条时没有东西会红
    pub validate: Option<Validator>,
}

impl FlagSpec {
    const fn new(
        long: &'static str,
        key: &'static str,
        arity: Arity,
        placeholder: &'static str,
        help: &'static str,
    ) -> Self {
        Self {
            long,
            key,
            arity,
            placeholder,
            help,
            validate: None,
        }
    }

    const fn validated(mut self, v: Validator) -> Self {
        self.validate = Some(v);
        self
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CommandSpec {
    pub name: &'static str,
    /// 恒取 tools 的常量、不写字面量——常量改名即编译错
    pub tool: &'static str,
    /// 位置参数的 (JSON 键, 占位符);None = 不收位置参数
    pub positional: Option<(&'static str, &'static str)>,
    pub flags: &'static [FlagSpec],
    pub summary: &'static str,
}

/// 不走 `mcp::tools` 的子命令:不收位置参数、不收旗标,因此也不参与
/// COMMANDS 与 `tools::definitions()` 的双射。加一个只在这里加一行
const PLAIN: &[(&str, &str)] = &[
    (
        "setup",
        "print this binary's path and how to point an agent at it",
    ),
    (
        "index",
        "build the index once, when Wake has never run here",
    ),
];

const F_PROJECT: FlagSpec = FlagSpec::new(
    "--project",
    "project",
    Arity::Value,
    "PATH",
    "one project: an absolute path (use \"$PWD\") or a project name",
)
.validated(check_project);
const F_AGENT: FlagSpec = FlagSpec::new(
    "--agent",
    "agents",
    Arity::Repeated,
    "ID",
    "only this agent; repeat the flag or comma-separate",
);
const F_SINCE: FlagSpec = FlagSpec::new(
    "--since",
    "since",
    Arity::Value,
    "WHEN",
    "only what was updated at or after WHEN",
);
const F_LIMIT: FlagSpec = FlagSpec::new(
    "--limit",
    "limit",
    Arity::Value,
    "N",
    "maximum rows to return",
);
const F_STARRED: FlagSpec = FlagSpec::new(
    "--starred",
    "starred",
    Arity::Switch,
    "",
    "only sessions starred in Wake",
);
const F_FROM: FlagSpec = FlagSpec::new(
    "--from",
    "from_seq",
    Arity::Value,
    "SEQ",
    "start at this message seq (a wake://…#N reference sets it too)",
);
const F_MESSAGES: FlagSpec = FlagSpec::new(
    "--messages",
    "max_messages",
    Arity::Value,
    "N",
    "messages per page",
);
const F_CHARS: FlagSpec = FlagSpec::new(
    "--chars",
    "max_chars",
    Arity::Value,
    "N",
    "character budget for the page",
);
const F_MESSAGE_CHARS: FlagSpec = FlagSpec::new(
    "--message-chars",
    "max_message_chars",
    Arity::Value,
    "N",
    "truncate each message to this many characters",
);
const F_TOOLS: FlagSpec = FlagSpec::new(
    "--tools",
    "include_tools",
    Arity::Switch,
    "",
    "include tool inputs and outputs",
);
const F_THINKING: FlagSpec = FlagSpec::new(
    "--thinking",
    "include_thinking",
    Arity::Switch,
    "",
    "include the assistant's thinking where the agent recorded it",
);

const SEARCH_FLAGS: &[FlagSpec] = &[F_PROJECT, F_AGENT, F_SINCE, F_LIMIT];
const SESSIONS_FLAGS: &[FlagSpec] = &[F_PROJECT, F_AGENT, F_SINCE, F_LIMIT, F_STARRED];
const SHOW_FLAGS: &[FlagSpec] = &[
    F_FROM,
    F_MESSAGES,
    F_CHARS,
    F_MESSAGE_CHARS,
    F_TOOLS,
    F_THINKING,
];
const PROJECTS_FLAGS: &[FlagSpec] = &[F_SINCE, F_LIMIT];

/// 子命令与 MCP 工具一一对应。加一家工具就在这里加一行,否则单测红
pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "search",
        tool: tools::SEARCH,
        positional: Some(("query", "QUERY")),
        flags: SEARCH_FLAGS,
        summary: "search transcripts across every indexed session",
    },
    CommandSpec {
        name: "sessions",
        tool: tools::LIST_SESSIONS,
        positional: None,
        flags: SESSIONS_FLAGS,
        summary: "list the most recently updated sessions",
    },
    CommandSpec {
        name: "show",
        tool: tools::GET_SESSION,
        positional: Some(("key", "KEY")),
        flags: SHOW_FLAGS,
        summary: "print one transcript as compact Markdown",
    },
    CommandSpec {
        name: "projects",
        tool: tools::LIST_PROJECTS,
        positional: None,
        flags: PROJECTS_FLAGS,
        summary: "list projects that have session history",
    },
];

// ---------------------------------------------------------------- 类型

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Help,
    Version,
    Setup,
    /// 从零建一次索引。**只在索引不存在时**——已存在的库归 GUI 管
    Index,
    /// 一次工具调用。`args` **恒为 JSON object**——mcp/mod.rs 那道
    /// `arguments` 形状检查在这条路上不可达,别再补一遍
    Tool {
        tool: &'static str,
        args: Value,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Invocation {
    /// `--db`;None = `db::default_db_path()`(由 bin 惰性求值)
    pub db: Option<PathBuf>,
    pub action: Action,
}

/// 已经是给人看的整句(小写开头、不带 `wake-cli: ` 前缀)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError {
    pub message: String,
    /// 命令级的错(没给命令 / 命令不认识 / 选项不认识 / 位置参数缺或多)附短
    /// USAGE;"这个选项缺值"一类本身已完整,不附——wake_mcp.rs 的这处不对称
    /// 是有意的,别顺手抹平
    pub with_usage: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub stream: Stream,
    pub text: String,
    pub code: u8,
}

impl CliError {
    pub fn report(&self) -> Report {
        let mut text = format!("wake-cli: {}", self.message);
        if self.with_usage {
            text.push_str("\n\n");
            text.push_str(USAGE);
        }
        Report {
            stream: Stream::Stderr,
            text,
            code: 2,
        }
    }
}

/// 工具结果 → 写哪条流、什么退出码。三个变体全列、不写 `_`:ToolError 加变体
/// 时这里编译不过,正是要的
pub fn report(result: Result<String, tools::ToolError>) -> Report {
    match result {
        Ok(text) => Report {
            stream: Stream::Stdout,
            text,
            code: 0,
        },
        // 值不对就是命令行敲错了(--since/--limit/--agent 的值全交给 tools.rs
        // 校验),与 "unknown option" 同一类,退 2。`wake-mcp call` 把它压成 1
        // 是因为那条路过 JSON-RPC 信封、分类由 payload 携带,退出码不是通道
        Err(tools::ToolError::InvalidParams(m)) => Report {
            stream: Stream::Stderr,
            text: format!("wake-cli: {m}"),
            code: 2,
        },
        // Failed = 这份会话读不出来;Internal = 库在但查询炸了(索引损坏 /
        // schema 漂移)。对用户是同一件事:跑起来了,然后失败
        Err(tools::ToolError::Failed(m) | tools::ToolError::Internal(m)) => Report {
            stream: Stream::Stderr,
            text: format!("wake-cli: {m}"),
            code: 1,
        },
    }
}

/// 文本写出去:末尾没换行就补一个(search/sessions/projects 收在 index_note
/// 上、无换行,get_session 自带),除此之外一个字节不加不减。**必须 flush**:
/// BrokenPipe 常常只在 flush 时才浮出来,而 Stdout 的析构会把错误吞掉
pub fn emit(text: &str, out: &mut impl Write) -> std::io::Result<()> {
    out.write_all(text.as_bytes())?;
    if !text.ends_with('\n') {
        out.write_all(b"\n")?;
    }
    out.flush()
}

// ---------------------------------------------------------------- 解析

fn err(message: String, with_usage: bool) -> CliError {
    CliError {
        message,
        with_usage,
    }
}

/// 缺值的错**不附 USAGE**(与 wake_mcp.rs 的 `--db needs a path` 同款)
fn next_value(it: &mut std::slice::Iter<'_, String>, msg: &str) -> Result<String, CliError> {
    it.next()
        .cloned()
        .ok_or_else(|| err(msg.to_string(), false))
}

/// 这个 long 归哪些命令(给 "只对 sessions 有效" / "要放在命令之后" 用)
fn flag_owners(long: &str) -> Vec<&'static str> {
    COMMANDS
        .iter()
        .filter(|c| c.flags.iter().any(|f| f.long == long))
        .map(|c| c.name)
        .collect()
}

/// 旗标放错地方或根本不认识——三句话一个出口,免得两处各写一遍 "unknown
/// option" 然后各自漂
fn misplaced(name: &str, in_command: bool) -> CliError {
    let owners = flag_owners(name);
    let msg = match (owners.is_empty(), in_command) {
        (true, _) => format!("unknown option {name}"),
        (false, false) => format!("{name} must come after the command"),
        (false, true) => format!("{name} is only valid for `{}`", owners.join("` and `")),
    };
    err(msg, true)
}

/// CLI 仅有的取值检查(挂在 F_PROJECT 上,不是按 key 名的 if):`--project .`
/// 在 tools 那边不算路径(见 context.rs 的 looks_like_path),会静默降级成项目
/// 名匹配、报 "No indexed project matches `.`"。相对路径这边也解不对(符号链
/// 接下 getcwd 与 $PWD 不是一回事),干脆直说,别猜
fn check_project(v: &str) -> Result<(), String> {
    let head = v.trim().split(std::path::is_separator).next().unwrap_or("");
    if head == "." || head == ".." {
        return Err("--project needs an absolute path or a project name; \
                    pass --project \"$PWD\" for the current directory"
            .into());
    }
    Ok(())
}

/// 解析途中已经认出的东西。三态一个枚举——`Option<&CommandSpec>` 加一个
/// `setup: bool` 能拼出第四种不该存在的状态,于是每个分支都要自证不可达
#[derive(Clone, Copy)]
enum Seen {
    Nothing,
    /// PLAIN 表里的子命令,按名字记
    Plain(&'static str),
    Command(&'static CommandSpec),
}

/// argv(**不含** argv[0])→ 一次调用。纯函数:不读环境、不碰文件系统、不
/// exit。wake_mcp.rs 在 parse_args 里直接 process::exit(0) 处理 --help /
/// --version,那样这两条路没法单测,这里改成返回 Action
pub fn parse(argv: &[String]) -> Result<Invocation, CliError> {
    let mut db: Option<PathBuf> = None;
    let mut seen = Seen::Nothing;
    let mut map = Map::new();
    let mut positional: Option<String> = None;
    let mut only_positional = false; // `--` 之后
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        let s = a.as_str();
        if !only_positional {
            // 1) 全局:位置无关,与 wake_mcp.rs 同形。--db 后面那个词恒被吃掉
            //    (`--db --help` 把 --help 当路径),行为与 wake-mcp 一致
            if s == "--db" {
                db = Some(PathBuf::from(next_value(&mut it, "--db needs a path")?));
                continue;
            }
            if let Some(v) = s.strip_prefix("--db=") {
                db = Some(PathBuf::from(v));
                continue;
            }
            if s == "--help" || s == "-h" {
                return Ok(Invocation {
                    db,
                    action: Action::Help,
                });
            }
            if s == "--version" || s == "-V" {
                return Ok(Invocation {
                    db,
                    action: Action::Version,
                });
            }
            if s == "--" {
                only_positional = true;
                continue;
            }
            if s.starts_with('-') && s != "-" {
                let (name, inline) = match s.split_once('=') {
                    Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
                    _ => (s, None),
                };
                // 2) 子命令 flag。命令还没出现 ⇒ 位置写反了,直说
                let cmd = match seen {
                    Seen::Command(c) => c,
                    Seen::Plain(p) => {
                        return Err(err(format!("{p} takes no options (got {name})"), true))
                    }
                    Seen::Nothing => return Err(misplaced(name, false)),
                };
                let Some(f) = cmd.flags.iter().find(|f| f.long == name) else {
                    return Err(misplaced(name, true));
                };
                let v = match (f.arity, inline) {
                    (Arity::Switch, Some(_)) => {
                        return Err(err(format!("{name} takes no value"), true))
                    }
                    (Arity::Switch, None) => {
                        map.insert(f.key.to_string(), Value::Bool(true));
                        continue;
                    }
                    (_, Some(v)) => v,
                    (_, None) => next_value(&mut it, &format!("{name} needs a value"))?,
                };
                if let Some(check) = f.validate {
                    check(&v).map_err(|m| err(m, false))?;
                }
                if f.arity == Arity::Repeated {
                    // 就地拼成逗号串:agents_arg 自己切分/trim/去重/校验,
                    // `--agent a --agent b` 与 `--agent a,b` 于是同义
                    if let Some(Value::String(prev)) = map.get_mut(f.key) {
                        prev.push(',');
                        prev.push_str(&v);
                        continue;
                    }
                }
                // 原样当字符串传,校验与裁剪交给 mcp::tools
                map.insert(f.key.to_string(), Value::String(v));
                continue;
            }
        }
        // 3) 位置参数:第一个是命令名
        match seen {
            Seen::Nothing if PLAIN.iter().any(|(n, _)| *n == s) => {
                seen = Seen::Plain(PLAIN.iter().find(|(n, _)| *n == s).expect("just matched").0)
            }
            Seen::Nothing => {
                seen = Seen::Command(
                    COMMANDS
                        .iter()
                        .find(|c| c.name == s)
                        .ok_or_else(|| err(format!("unknown command {s}"), true))?,
                )
            }
            Seen::Plain(p) => return Err(err(format!("{p} takes no arguments (got `{s}`)"), true)),
            Seen::Command(cmd) => {
                let Some((_, ph)) = cmd.positional else {
                    return Err(err(
                        format!("{} takes no arguments (got `{s}`)", cmd.name),
                        true,
                    ));
                };
                if positional.is_some() {
                    return Err(err(
                        format!(
                            "{} takes one {ph} — quote it: wake-cli {} \"…\"",
                            cmd.name, cmd.name
                        ),
                        true,
                    ));
                }
                positional = Some(a.clone());
            }
        }
    }
    let cmd = match seen {
        Seen::Plain("setup") => {
            return Ok(Invocation {
                db,
                action: Action::Setup,
            })
        }
        Seen::Plain("index") => {
            return Ok(Invocation {
                db,
                action: Action::Index,
            })
        }
        Seen::Plain(other) => unreachable!("PLAIN 表加了 {other} 但没在这里给 Action"),
        Seen::Nothing => return Err(err("needs a command".into(), true)),
        Seen::Command(c) => c,
    };
    if let Some((key, ph)) = cmd.positional {
        match positional {
            Some(v) => {
                map.insert(key.to_string(), Value::String(v));
            }
            None => return Err(err(format!("{} needs a {ph}", cmd.name), true)),
        }
    }
    Ok(Invocation {
        db,
        action: Action::Tool {
            tool: cmd.tool,
            args: Value::Object(map),
        },
    })
}

// ---------------------------------------------------------------- 帮助

/// 短用法:命令级错误后附在 stderr。全文另有 `help()`——把 50 行参考手册贴在
/// "`--limit` needs a value" 后面是骚扰,所以拆成两份(单测卡这份的行数)
const USAGE: &str = "wake-cli — query your Wake session index from the terminal

USAGE:
  wake-cli search QUERY [OPTIONS]
  wake-cli sessions [OPTIONS]
  wake-cli show KEY [OPTIONS]
  wake-cli projects [OPTIONS]
  wake-cli setup
  wake-cli --help | --version

Run `wake-cli --help` for every option.";

const HELP_HEAD: &str = "wake-cli — query your Wake session index from the terminal

Wake indexes every coding-agent session on this machine (Claude Code, Codex,
Cursor, Gemini CLI and more). This reads that index; it never writes to it.

USAGE:
  wake-cli <command> [ARGS] [OPTIONS]

COMMANDS:";

const HELP_GLOBALS: &str = "
GLOBAL OPTIONS:
  --db PATH           index database (default: Wake's own, e.g. ~/Library/Application Support/wake/wake.db)
  --help, -h          print this help
  --version, -V       print the version
  --                  treat everything after it as arguments, not options
";

const HELP_WHEN: &str = "
WHEN:
  Relative: 30m, 24h, 7d, 2w. Absolute: 2026-09-01, \"2026-09-01 09:30\",
  2026-09-01T09:30:00Z. Date-times without a zone are read in local time.
  Values outside a flag's range are clamped, not rejected.
";

const HELP_TAIL: &str = "
KEYS AND REFERENCES:
  A session key is <agent>:<id>, or <agent>:<host>:<id> for a session mirrored
  from a remote host. `sessions` and `search` print the key of every session
  they list; `search` also prints a reference under each snippet:

      ref: wake://session/claude-code:1b2c…#42

  Pass a reference to `show` and the page starts at that message.

EXAMPLES:
  wake-cli sessions --project \"$PWD\" --limit 5
  wake-cli search \"useEffect(\" --project \"$PWD\" --since 7d
  wake-cli search \"rate limiter\" --agent codex --agent claude-code
  wake-cli show 'wake://session/claude-code:1b2c…#42' --tools
  wake-cli search -- --dry-run          # a query that starts with a dash

EXIT CODES:
  0  it ran — including \"no matches\" and \"no such project\"
  1  a session could not be read (unknown key, or a transcript that would not parse)
  2  the command line was wrong, or the index is missing / unreadable / too old

Everything is read-only. Full reference: docs/cli.md";

/// 命令块与选项块的说明列起始列
const CMD_COL: usize = 36;
const OPT_COL: usize = 22;

/// 左列补空格到 width,左列过长时至少留两格
fn row(left: String, right: &str, width: usize) -> String {
    let pad = width.saturating_sub(left.chars().count()).max(2);
    format!("{left}{}{right}\n", " ".repeat(pad))
}

/// 某个工具参数的默认值与取值区间,现从 `tools::definitions()` 的 schema 取。
/// 帮助文本里最容易烂掉的就是这些数字,渲染出来它们至少不会与 schema 各说各
/// 话(schema 与 int_arg 调用点仍是两份,那是 tools.rs 自己的事)
fn limits_of(defs: &[Value], tool: &str, key: &str) -> String {
    let Some(p) = defs
        .iter()
        .find(|d| d["name"] == tool)
        .and_then(|d| d["inputSchema"]["properties"].get(key))
    else {
        return String::new();
    };
    let mut bits = Vec::new();
    if let Some(v) = p.get("default").filter(|v| v.is_number()) {
        bits.push(format!("default {v}"));
    }
    // 上下界成对才写:from_seq 只有 minimum 0,"at least 0" 是废话
    if let (Some(lo), Some(hi)) = (p.get("minimum"), p.get("maximum")) {
        bits.push(format!("{lo}–{hi}"));
    }
    if bits.is_empty() {
        String::new()
    } else {
        format!(" ({})", bits.join(", "))
    }
}

/// 逗号分隔、缩进两格、贪心折到 76 列
fn wrapped(items: &[&str], width: usize) -> String {
    let mut out = String::new();
    let mut line = String::from("  ");
    for (i, it) in items.iter().enumerate() {
        let piece = if i + 1 == items.len() {
            it.to_string()
        } else {
            format!("{it},")
        };
        let len = line.chars().count();
        if len > 2 {
            if len + 1 + piece.chars().count() > width {
                out.push_str(&line);
                out.push('\n');
                line = String::from("  ");
            } else {
                line.push(' ');
            }
        }
        line.push_str(&piece);
    }
    out.push_str(&line);
    out
}

/// `--help` 的全文。命令块、各命令的选项块、默认值上限与 agent 名全部由
/// COMMANDS 表与 `tools::definitions()` / `AgentId::ALL` 渲染,静态散文只剩
/// 不含事实的那几段
pub fn help() -> String {
    let defs = tools::definitions();
    let mut out = String::from(HELP_HEAD);
    out.push('\n');
    for c in COMMANDS {
        let arg = c
            .positional
            .map(|(_, ph)| format!(" {ph}"))
            .unwrap_or_default();
        out.push_str(&row(
            format!("  wake-cli {}{arg}", c.name),
            c.summary,
            CMD_COL,
        ));
    }
    for (name, summary) in PLAIN {
        out.push_str(&row(format!("  wake-cli {name}"), summary, CMD_COL));
    }
    out.push_str(HELP_GLOBALS);
    for c in COMMANDS {
        out.push_str(&format!("\n{} OPTIONS:\n", c.name));
        for f in c.flags {
            let left = if f.placeholder.is_empty() {
                format!("  {}", f.long)
            } else {
                format!("  {} {}", f.long, f.placeholder)
            };
            let help = format!("{}{}", f.help, limits_of(&defs, c.tool, f.key));
            out.push_str(&row(left, &help, OPT_COL));
        }
    }
    out.push_str(HELP_WHEN);
    let ids: Vec<&str> = AgentId::ALL.iter().map(|a| a.as_str()).collect();
    out.push_str(&format!(
        "\nAGENT IDS:\n{}\n  Display names (\"Claude Code\") and a few aliases work too.\n",
        wrapped(&ids, 76)
    ));
    out.push_str(HELP_TAIL);
    out
}

// ---------------------------------------------------------------- setup

/// 贴进 CLAUDE.md / AGENTS.md 的那段——给不想装 skill、只想在一个项目里用的
/// 人。与 skills/wake/SKILL.md 说的是同一件事,那份更全(找二进制、翻页、
/// 退出码);这里只留最短的一版
const AGENT_MEMO: &str =
    "Earlier sessions with every coding agent on this machine are searchable with
wake-cli. Reach for it when the user asks about an earlier conversation, a past
decision, why something was done a certain way, where previous work stopped, or
whether an error has been seen before — git history does not hold that.

    wake-cli sessions --project \"$PWD\" --limit 10
    wake-cli search \"<terms>\" --project \"$PWD\"
    wake-cli show <key>

`sessions` and `search` print a key for every session; `search` also prints a
`wake://session/<key>#<seq>` reference that `show` accepts. Always pass
--project \"$PWD\" — without it every project on the machine is in scope.
Everything is read-only.";

/// 一行装好 skill。`owner/repo` 形式由 vercel-labs 的 skills CLI 认,仓库里
/// 有 `skills/` 目录即可被发现。Settings → Connect 的 Skill 卡展示的就是它,
/// 与 `wake-cli setup` 同一个来源
pub const SKILL_INSTALL: &str = "npx skills add iAmCorey/Wake";

/// 把 CLI 放进 PATH 的那一条**可粘命令**,给 GUI 的 Copy 按钮用。与
/// `wake-cli setup` 打印的是同一个来源。返回 None 的两种情况都不该给按钮:
/// 已经装在 `…/bin` 里(deb / tar)、以及 Windows——那边 path_hint 给的是
/// "把这个目录加进 PATH" 的说明,不是能粘进终端跑的东西
pub fn path_command(cli_bin: &Path) -> Option<String> {
    if cfg!(target_os = "windows") {
        return None;
    }
    path_hint(cli_bin)
}

/// `wake-cli setup` 要说的事实(由 bin 查好传进来,函数本身仍是纯的)
pub struct SetupFacts<'a> {
    pub cli_bin: &'a Path,
    /// 同目录的 wake-mcp(存在才给)
    pub mcp_bin: Option<&'a Path>,
    pub db: &'a Path,
    /// 库打不开的原因(`{e:#}`);能打开是 None
    pub db_error: Option<String>,
}

/// 已经在 `…/bin` 里的(deb / tar 装法)不必再叫人 symlink,那是噪音;
/// current_exe 失败时只剩个裸文件名,parent 为空,更不该打印自指的 ln
fn path_hint(bin: &Path) -> Option<String> {
    let dir = bin.parent().filter(|d| !d.as_os_str().is_empty())?;
    if dir.file_name().is_some_and(|n| n == "bin") {
        return None;
    }
    if cfg!(target_os = "windows") {
        return Some(format!("Add this folder to PATH:\n\n{}", dir.display()));
    }
    let quoted = sh_quote(&bin.to_string_lossy());
    Some(if cfg!(target_os = "macos") {
        // 出厂 macOS 的 /usr/local/bin 是 root:wheel,而且可能压根不存在——
        // 不带 sudo 的 ln 在干净机器上必然 Permission denied
        format!("sudo mkdir -p /usr/local/bin && sudo ln -sf {quoted} /usr/local/bin/wake-cli")
    } else {
        format!("mkdir -p ~/.local/bin && ln -sf {quoted} ~/.local/bin/wake-cli")
    })
}

/// `## {标题} — {怎么用这段}` 与 `wake-mcp setup` 同形,两个 bin 的输出读起来
/// 才是一家人
pub fn setup_text(f: &SetupFacts<'_>) -> String {
    let mut out = format!(
        "wake-cli binary: {}\nIndex database: {}\n",
        f.cli_bin.display(),
        f.db.display()
    );
    if let Some(hint) = path_hint(f.cli_bin) {
        out.push_str(&format!(
            "\n## Put it on your PATH — Run in a terminal\n\n{hint}\n"
        ));
    }
    out.push_str(&format!(
        "\n## Teach an agent to use it — Paste into the project's CLAUDE.md or AGENTS.md\n\n{AGENT_MEMO}\n"
    ));
    out.push_str(&format!(
        "\n## Install it once for every project — Run in a terminal\n\n{SKILL_INSTALL}\n\n\
         That adds the same guidance as a skill, so you do not have to paste it per\n\
         project. Manual install: copy `skills/wake/` from the repository into\n\
         `~/.claude/skills/wake/`.\n"
    ));
    // 片段按 agent 认,不按下标——那个顺序是 Connect 页的事;hint 也用它自己
    // 带的,两个 bin 的 setup 输出才真的同形
    if let Some(snippet) = f.mcp_bin.and_then(|mcp| {
        crate::mcp::setup_snippets(mcp)
            .into_iter()
            .find(|s| s.agent == AgentId::ClaudeCode)
    }) {
        out.push_str(&format!(
            "\n## Tool-calling clients: prefer MCP — {}\n\n{}\n\n\
             `wake-mcp setup` prints the same for Codex and Cursor.\n",
            snippet.hint, snippet.text
        ));
    }
    out.push_str("\nFull reference: docs/cli.md\n");
    if let Some(e) = &f.db_error {
        out.push_str(&format!("\nNote: {e}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 故意不给 CLI 子命令的工具(现在没有)。加工具时要么进 COMMANDS,要么
    /// 在这里写明白为什么不进
    const NOT_IN_CLI: &[&str] = &[];

    fn p(a: &[&str]) -> Result<Invocation, CliError> {
        parse(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    fn args_of(a: &[&str]) -> Value {
        match p(a).expect("parses").action {
            Action::Tool { args, .. } => args,
            other => panic!("expected a tool call, got {other:?}"),
        }
    }

    fn message(a: &[&str]) -> String {
        p(a).expect_err("should not parse").message
    }

    /// 这个文件的看家测试:CLI 的旗标表与 MCP 工具的 inputSchema 必须双射。
    /// 工具加参数、改名、改类型、加第五家,这里就红
    #[test]
    fn every_tool_parameter_has_exactly_one_flag() {
        let defs = tools::definitions();
        for d in &defs {
            let name = d["name"].as_str().expect("tool has a name");
            if NOT_IN_CLI.contains(&name) {
                continue;
            }
            let cmd = COMMANDS.iter().find(|c| c.tool == name).unwrap_or_else(|| {
                panic!("{name} has no wake-cli subcommand; add one to COMMANDS or list it in NOT_IN_CLI")
            });
            let props = d["inputSchema"]["properties"]
                .as_object()
                .expect("inputSchema has properties");
            let mut mine: Vec<&str> = cmd.flags.iter().map(|f| f.key).collect();
            if let Some((key, _)) = cmd.positional {
                mine.push(key);
            }
            mine.sort_unstable();
            let mut theirs: Vec<&str> = props.keys().map(String::as_str).collect();
            theirs.sort_unstable();
            assert_eq!(mine, theirs, "{name}: CLI 旗标与工具参数对不上");

            // required 只能是位置参数——旗标是可选的
            for r in d["inputSchema"]["required"].as_array().unwrap_or(&vec![]) {
                let r = r.as_str().expect("required entries are strings");
                assert_eq!(
                    Some(r),
                    cmd.positional.map(|(k, _)| k),
                    "{name}: 必填参数 {r} 在 CLI 里不是位置参数"
                );
            }
            for f in cmd.flags {
                let ty = props[f.key]["type"].as_str().unwrap_or_default();
                let ok = match f.arity {
                    Arity::Switch => ty == "boolean",
                    Arity::Repeated => ty == "array",
                    Arity::Value => ty == "string" || ty == "integer" || ty == "number",
                };
                assert!(
                    ok,
                    "{name}.{}: arity {:?} 配不上 schema 的 {ty}",
                    f.key, f.arity
                );
            }
        }
        for c in COMMANDS {
            assert!(
                defs.iter().any(|d| d["name"] == c.tool),
                "COMMANDS 里的 {} 在 tools::definitions() 中不存在",
                c.tool
            );
        }
    }

    #[test]
    fn flag_specs_are_consistent_across_commands() {
        for c in COMMANDS {
            for f in c.flags {
                for other in COMMANDS.iter().flat_map(|c| c.flags) {
                    if other.long == f.long {
                        assert_eq!(other.key, f.key, "{} 在两处映到不同的键", f.long);
                        assert_eq!(other.arity, f.arity, "{} 在两处 arity 不同", f.long);
                        assert_eq!(other.help, f.help, "{} 在两处说明不同", f.long);
                    }
                }
            }
        }
    }

    #[test]
    fn no_duplicate_flag_or_command_names() {
        let mut names: Vec<&str> = COMMANDS.iter().map(|c| c.name).collect();
        names.extend(PLAIN.iter().map(|(n, _)| *n));
        names.sort_unstable();
        let n = names.len();
        names.dedup();
        assert_eq!(n, names.len(), "命令重名");
        for c in COMMANDS {
            let mut longs: Vec<&str> = c.flags.iter().map(|f| f.long).collect();
            longs.sort_unstable();
            let n = longs.len();
            longs.dedup();
            assert_eq!(n, longs.len(), "{} 的旗标重名", c.name);
        }
    }

    #[test]
    fn argv_maps_to_the_mcp_arguments_object() {
        assert_eq!(
            args_of(&[
                "search",
                "qr login",
                "--project",
                "/w",
                "--agent",
                "codex",
                "--limit",
                "3"
            ]),
            json!({"query":"qr login","project":"/w","agents":"codex","limit":"3"})
        );
        assert_eq!(
            args_of(&["show", "k", "--tools", "--from", "7"]),
            json!({"key":"k","include_tools":true,"from_seq":"7"})
        );
        // 路由 + args 内容 + "args 恒为 object" 一次比完。后者意味着
        // mcp/mod.rs 那道 `arguments` 形状检查在这条路上不可达,别再补一遍
        assert_eq!(
            p(&["projects"]).unwrap().action,
            Action::Tool {
                tool: tools::LIST_PROJECTS,
                args: json!({})
            }
        );
        assert_eq!(
            p(&["sessions"]).unwrap().action,
            Action::Tool {
                tool: tools::LIST_SESSIONS,
                args: json!({})
            }
        );
    }

    /// 整个设计压在这条上:数字不解析,交给 int_arg 去认字符串并裁剪
    #[test]
    fn numeric_values_stay_strings() {
        for c in COMMANDS {
            for f in c.flags.iter().filter(|f| f.arity == Arity::Value) {
                let mut argv = vec![c.name];
                if c.positional.is_some() {
                    argv.push("x");
                }
                argv.push(f.long);
                argv.push("7");
                let v = args_of(&argv);
                assert!(
                    v[f.key].is_string(),
                    "{} {} 应当原样传字符串",
                    c.name,
                    f.long
                );
            }
        }
    }

    #[test]
    fn switches_are_json_booleans() {
        assert_eq!(args_of(&["sessions", "--starred"])["starred"], json!(true));
        assert_eq!(
            args_of(&["show", "k", "--thinking"])["include_thinking"],
            json!(true)
        );
    }

    #[test]
    fn repeated_and_comma_separated_agents_become_one_string() {
        let a = args_of(&["search", "x", "--agent", "codex", "--agent", "claude"]);
        let b = args_of(&["search", "x", "--agent", "codex,claude"]);
        assert_eq!(a["agents"], json!("codex,claude"));
        assert_eq!(a, b);
    }

    #[test]
    fn equals_form_and_global_flags_anywhere() {
        assert_eq!(
            args_of(&["sessions", "--limit=5"]),
            args_of(&["sessions", "--limit", "5"])
        );
        for a in [vec!["--db=/x", "sessions"], vec!["sessions", "--db", "/x"]] {
            assert_eq!(p(&a).unwrap().db, Some(PathBuf::from("/x")));
        }
    }

    #[test]
    fn double_dash_ends_option_parsing() {
        assert_eq!(
            args_of(&["search", "--", "--dry-run"])["query"],
            json!("--dry-run")
        );
    }

    #[test]
    fn flags_are_rejected_by_name_when_they_belong_elsewhere() {
        let m = message(&["search", "x", "--starred"]);
        assert!(m.contains("only valid for `sessions`"), "{m}");
        assert!(message(&["--limit", "5", "sessions"]).contains("must come after the command"));
        assert!(message(&["projects", "--starred"]).contains("only valid for"));
    }

    /// 缺值的错不附 USAGE、形状错附——wake_mcp.rs 的这处不对称是有意的
    #[test]
    fn missing_values_and_unknown_options_are_parse_errors() {
        assert!(!p(&["--db"]).unwrap_err().with_usage);
        assert!(!p(&["sessions", "--limit"]).unwrap_err().with_usage);
        assert!(p(&["sessions", "--nope"]).unwrap_err().with_usage);
        assert!(message(&["sessions", "--nope"]).contains("unknown option"));
        assert!(message(&["sessions", "--starred=false"]).contains("takes no value"));
    }

    #[test]
    fn positional_count_errors() {
        assert!(message(&["search"]).contains("needs a QUERY"));
        assert!(message(&["search", "a", "b"]).contains("quote it"));
        assert!(message(&["show"]).contains("needs a KEY"));
        assert!(message(&["sessions", "x"]).contains("takes no arguments"));
        assert!(message(&["setup", "x"]).contains("takes no arguments"));
        assert!(message(&["index", "x"]).contains("takes no arguments"));
        // PLAIN 命令不收旗标,包括全局之外的任何一个
        assert!(message(&["index", "--limit", "5"]).contains("takes no options"));
        assert!(message(&[]).contains("needs a command"));
        assert!(message(&["nope"]).contains("unknown command"));
    }

    #[test]
    fn relative_project_paths_are_refused_with_advice() {
        for v in [".", "./x", "..", "../x"] {
            assert!(
                message(&["sessions", "--project", v]).contains("$PWD"),
                "{v} 应当被拒并给出 $PWD 的写法"
            );
        }
        for v in ["/abs", "~/x", "myname", ".hidden"] {
            assert_eq!(
                args_of(&["sessions", "--project", v])["project"],
                json!(v),
                "{v} 应当原样传下去"
            );
        }
    }

    #[test]
    fn help_and_version_short_circuit() {
        assert_eq!(p(&["--help", "--nonsense"]).unwrap().action, Action::Help);
        assert_eq!(p(&["sessions", "-h"]).unwrap().action, Action::Help);
        assert_eq!(p(&["-V"]).unwrap().action, Action::Version);
        assert_eq!(p(&["setup"]).unwrap().action, Action::Setup);
        assert_eq!(p(&["index"]).unwrap().action, Action::Index);
        assert!(USAGE.lines().count() < 15, "USAGE 不该长回参考手册");
        assert!(!USAGE.ends_with('\n'));
    }

    #[test]
    fn help_lists_every_command_flag_and_agent() {
        let h = help();
        for c in COMMANDS {
            assert!(h.contains(c.name), "help 缺命令 {}", c.name);
            assert!(h.contains(c.summary));
            for f in c.flags {
                assert!(h.contains(f.long), "help 缺旗标 {}", f.long);
            }
        }
        for (name, summary) in PLAIN {
            assert!(h.contains(name), "help 缺命令 {name}");
            assert!(h.contains(summary), "help 缺 {name} 的说明");
        }
        for a in AgentId::ALL {
            assert!(h.contains(a.as_str()), "help 缺 agent {}", a.as_str());
        }
        assert!(h.contains("wake://session/"));
        assert!(h.contains("EXIT CODES:"));
        // 默认值与取值区间现渲染,不是手写进散文里的
        assert!(
            h.contains("default 10, 1–30"),
            "search --limit 的区间没渲染出来"
        );
        assert!(
            h.contains("default 60, 1–200"),
            "show --messages 的区间没渲染出来"
        );
        // 下界曾经不渲染,于是 `--chars 5` 被悄悄裁到 200 而 help 里一个字都没有
        assert!(
            h.contains("default 20000, 200–100000"),
            "show --chars 的下界没渲染出来"
        );
        // AGENT IDS 必须落在 WHEN 与 KEYS 之间,不能被甩到文末
        let (when, ids, keys) = (
            h.find("\nWHEN:").expect("WHEN"),
            h.find("\nAGENT IDS:").expect("AGENT IDS"),
            h.find("\nKEYS AND REFERENCES:").expect("KEYS"),
        );
        assert!(when < ids && ids < keys, "help 的段落顺序错了");
        assert!(!h.ends_with('\n'), "emit 负责补最后那个换行");
    }

    #[test]
    fn report_splits_argument_errors_from_read_failures() {
        let r = report(Ok("body".into()));
        assert_eq!(
            (r.stream, r.code, r.text.as_str()),
            (Stream::Stdout, 0, "body")
        );
        let r = report(Err(tools::ToolError::InvalidParams("x".into())));
        assert_eq!(
            (r.stream, r.code, r.text.as_str()),
            (Stream::Stderr, 2, "wake-cli: x")
        );
        let r = report(Err(tools::ToolError::Failed("x".into())));
        assert_eq!((r.stream, r.code), (Stream::Stderr, 1));
        let r = report(Err(tools::ToolError::Internal("x".into())));
        assert_eq!((r.stream, r.code), (Stream::Stderr, 1));
        assert_eq!(
            CliError {
                message: "m".into(),
                with_usage: false
            }
            .report()
            .code,
            2
        );
    }

    #[test]
    fn emit_adds_at_most_one_trailing_newline() {
        for (input, want) in [("a", "a\n"), ("a\n", "a\n"), ("a\n\n", "a\n\n"), ("", "\n")] {
            let mut buf: Vec<u8> = Vec::new();
            emit(input, &mut buf).unwrap();
            assert_eq!(String::from_utf8(buf).unwrap(), want, "input {input:?}");
        }
    }

    /// 下游关掉管道要浮成 Err 让 bin 去分类,不能 panic。真管道不好测(fixture
    /// 转录塞得进 64K 管道缓冲,子进程往往先写完),这里用假 writer 卡行为
    #[test]
    fn a_closed_pipe_surfaces_as_an_error_not_a_panic() {
        struct FailWrite;
        impl Write for FailWrite {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "gone"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        struct FailFlush;
        impl Write for FailFlush {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "gone"))
            }
        }
        assert_eq!(
            emit("x", &mut FailWrite).unwrap_err().kind(),
            std::io::ErrorKind::BrokenPipe
        );
        assert_eq!(
            emit("x\n", &mut FailFlush).unwrap_err().kind(),
            std::io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn setup_text_names_the_binary_and_the_gap() {
        let cli_bin = PathBuf::from("/Apps/Wake.app/Contents/MacOS/wake-cli");
        let mcp_bin = PathBuf::from("/Apps/Wake.app/Contents/MacOS/wake-mcp");
        let db = PathBuf::from("/tmp/wake.db");
        let t = setup_text(&SetupFacts {
            cli_bin: &cli_bin,
            mcp_bin: Some(&mcp_bin),
            db: &db,
            db_error: None,
        });
        assert!(t.contains(&cli_bin.display().to_string()));
        assert!(t.contains("/tmp/wake.db"));
        assert!(t.contains("docs/cli.md"));
        assert!(t.contains("--project \"$PWD\""));
        // setup 必须给出真正能跑的安装方式(阶段 1 那句"还没有 skill 包"的棘轮)
        assert!(t.contains(SKILL_INSTALL), "{t}");
        assert!(t.contains("~/.claude/skills/wake/"), "{t}");
        assert!(t.contains("claude mcp add"));

        let t = setup_text(&SetupFacts {
            cli_bin: &cli_bin,
            mcp_bin: None,
            db: &db,
            db_error: Some("no Wake index at /tmp/wake.db".into()),
        });
        assert!(
            !t.contains("claude mcp add"),
            "没有 wake-mcp 就不该给 MCP 片段"
        );
        assert!(t
            .trim_end()
            .ends_with("Note: no Wake index at /tmp/wake.db"));
    }

    #[test]
    fn path_command_is_pasteable_or_absent() {
        let bundled = PathBuf::from("/Apps/Wake.app/Contents/MacOS/wake-cli");
        if cfg!(target_os = "windows") {
            // Windows 的 path_hint 是散文,绝不能进剪贴板
            assert_eq!(path_command(&bundled), None);
        } else {
            let c = path_command(&bundled).expect("bundle 里要给命令");
            assert!(c.contains("wake-cli"), "{c}");
            assert_eq!(c.lines().count(), 1, "要能一行粘进终端: {c}");
        }
        // 已经在 …/bin 里就不必说了
        assert_eq!(path_command(Path::new("/usr/bin/wake-cli")), None);
        // current_exe 失败时的裸名兜底,没有目录可言
        assert_eq!(path_command(Path::new("wake-cli")), None);
    }

    #[test]
    fn path_hint_quotes_spaces_and_skips_bin_dirs() {
        assert_eq!(path_hint(Path::new("/usr/bin/wake-cli")), None);
        assert_eq!(path_hint(Path::new("/home/me/.local/bin/wake-cli")), None);
        // current_exe 失败时的兜底裸名:没有目录可说,别给自指的 ln
        assert_eq!(path_hint(Path::new("wake-cli")), None);
        let hint = path_hint(Path::new("/Apps/My Wake.app/Contents/MacOS/wake-cli"))
            .expect("bundle 里要给提示");
        if cfg!(target_os = "windows") {
            // Windows 没有 ln -s,给的是"把这个目录加进 PATH"
            assert!(hint.contains("MacOS"), "{hint}");
        } else {
            assert!(
                hint.contains("'/Apps/My Wake.app/Contents/MacOS/wake-cli'"),
                "{hint}"
            );
        }
    }
}
