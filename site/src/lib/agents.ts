// The agents Wake reads, in the app's own order (AgentId::ALL). Names, brand marks and
// series colours mirror crates/wake-core/src/models.rs and crates/wake/src/theme.rs;
// `where` is the short form of the README's data-source column.
export interface Agent {
  id: string;
  name: string;
  /** Brand mark under /brands, the dark-background variant. */
  icon: string;
  color: string;
  where: string;
}

export const AGENTS: Agent[] = [
  { id: 'claude-code', name: 'Claude Code', icon: 'claude-code', color: '#D97757', where: '~/.claude/projects' },
  { id: 'codex', name: 'Codex', icon: 'codex', color: '#7A8DFF', where: '~/.codex/sessions' },
  { id: 'qoder', name: 'Qoder CLI', icon: 'qoder', color: '#2BB454', where: '~/.qoder/projects' },
  { id: 'copilot', name: 'Copilot CLI', icon: 'copilot', color: '#6E9C3F', where: '~/.copilot/session-store.db' },
  { id: 'cursor', name: 'Cursor', icon: 'cursor', color: '#2DB6C8', where: '~/.cursor/projects + state.vscdb' },
  { id: 'opencode', name: 'OpenCode', icon: 'opencode', color: '#C98A2E', where: '~/.local/share/opencode' },
  { id: 'kiro', name: 'Kiro', icon: 'kiro', color: '#9148FF', where: '~/.kiro/sessions/cli' },
  { id: 'gemini', name: 'Gemini CLI', icon: 'gemini', color: '#3B8BD9', where: '~/.gemini/tmp' },
  { id: 'pi', name: 'Pi', icon: 'pi', color: '#3AAE8C', where: '~/.pi/agent/sessions' },
  { id: 'omp', name: 'Oh My Pi', icon: 'omp', color: '#B05CE6', where: '~/.omp/agent/sessions' },
  { id: 'grok', name: 'Grok Build', icon: 'grok', color: '#8A7F73', where: '~/.grok/sessions' },
  { id: 'kimi', name: 'Kimi Code', icon: 'kimi', color: '#E4739E', where: '~/.kimi-code/sessions' },
  { id: 'antigravity', name: 'Antigravity', icon: 'antigravity', color: '#648AB5', where: '~/.gemini/antigravity/brain' },
  { id: 'dsh', name: 'DeepSeek Harness', icon: 'deepseek', color: '#4D6BFE', where: '~/.dsh/sessions' },
  { id: 'hermes', name: 'Hermes Agent', icon: 'hermes', color: '#E0B040', where: '~/.hermes/state.db' },
  { id: 'openclaw', name: 'OpenClaw', icon: 'openclaw', color: '#E04A4A', where: '~/.openclaw/agents' },
  { id: 'codebuddy', name: 'CodeBuddy', icon: 'codebuddy', color: '#6C4DFF', where: '~/.codebuddy/projects' },
  { id: 'workbuddy', name: 'WorkBuddy', icon: 'workbuddy', color: '#0EC8A9', where: '~/.workbuddy/projects' },
  { id: 'zcode', name: 'ZCode', icon: 'zcode', color: '#8B95A5', where: '~/.zcode/cli/db' },
  { id: 'craft-agents', name: 'Craft Agents', icon: 'craft-agents', color: '#9570BE', where: '~/.craft-agent/workspaces' },
  { id: 'devin', name: 'Devin', icon: 'devin', color: '#3BB3F1', where: '~/.local/share/devin' },
  { id: 'kilo', name: 'Kilo Code', icon: 'kilo', color: '#C8C035', where: '~/.local/share/kilo + globalStorage' },
];

export const AGENT_BY_ID = Object.fromEntries(AGENTS.map((a) => [a.id, a])) as Record<string, Agent>;

export const brandSrc = (a: Agent) => `/brands/${a.icon}.webp`;
