// Synthetic sessions for the search demo on the home page. Made up from scratch, like
// scripts/demo-home.py in the app repo: no real project or conversation appears here.
export type Role = 'user' | 'assistant' | 'tool';

export interface DemoMessage {
  role: Role;
  text: string;
}

export interface DemoSession {
  id: string;
  agent: string;
  project: string;
  branch?: string;
  title: string;
  /** Minutes since the last update; drives ordering and the "3h" label. */
  age: number;
  model?: string;
  messages: DemoMessage[];
}

const H = 60;
const D = 24 * H;

export const SESSIONS: DemoSession[] = [
  {
    id: 's01', agent: 'claude-code', project: 'acme-web', branch: 'main', age: 3 * H, model: 'claude-fable-5',
    title: 'Add a rate limiter to the public API',
    messages: [
      { role: 'user', text: 'One client is hammering the public API. Add a rate limiter: per API key, 100 requests a minute.' },
      { role: 'assistant', text: "I'll use a token bucket keyed by API key and keep it in Redis, so the limit holds across all instances. Refill 100 per minute, burst of 20." },
      { role: 'tool', text: 'Edit src/middleware/rateLimit.ts' },
      { role: 'assistant', text: 'Requests over the limit now get a 429 with a Retry-After header. Added tests for the burst and the refill.' },
    ],
  },
  {
    id: 's02', agent: 'codex', project: 'acme-web', branch: 'fix/healthz', age: 1 * D + 2 * H, model: 'gpt-5.5',
    title: 'Health checks get 429 after the rate limiter change',
    messages: [
      { role: 'user', text: '/healthz returns 429 to the load balancer since the rate limiter went in. Instances keep getting pulled out.' },
      { role: 'assistant', text: "Health probes send no API key, so they all share the anonymous bucket and drain it within seconds. I'll mount /healthz and /readyz before the rate limiter middleware." },
      { role: 'tool', text: 'Edit src/server.ts' },
      { role: 'assistant', text: 'Probes no longer count against any limit. Added a test that fires 500 probes in a row.' },
    ],
  },
  {
    id: 's03', agent: 'cursor', project: 'pocket-pay', age: 2 * D, model: 'composer-2',
    title: '生成付款二维码并支持保存到相册',
    messages: [
      { role: 'user', text: '付款页需要生成二维码,用户长按可以保存到相册。' },
      { role: 'assistant', text: '用 qrcode 在客户端生成 SVG 二维码;长按时转成 PNG 再写入相册。iOS 要在 Info.plist 里补上相册写入权限的说明,否则保存会直接失败。' },
      { role: 'tool', text: 'Edit app/screens/PayScreen.tsx' },
    ],
  },
  {
    id: 's04', agent: 'gemini', project: 'pocket-pay', age: 5 * D, model: 'gemini-3-pro',
    title: '暗色模式下二维码扫不出来',
    messages: [
      { role: 'user', text: '暗色模式下二维码变成深底浅码,有些扫码器识别不了。' },
      { role: 'assistant', text: '反色二维码的兼容性很差。不管什么主题都固定白底黑码,四周留出 16px 的白边(quiet zone),扫码成功率就回来了。' },
    ],
  },
  {
    id: 's05', agent: 'claude-code', project: 'blog-engine', branch: 'main', age: 6 * H, model: 'claude-fable-5',
    title: 'Comments load twice on page open',
    messages: [
      { role: 'user', text: 'The comments widget fetches twice when the page opens, only in development.' },
      { role: 'assistant', text: "StrictMode mounts effects twice in development, which is what surfaces it. The real bug is useEffect(() => { fetchComments() }, []) with no cleanup: the first response can land after the second and overwrite it. I'll pass an AbortController and abort in the cleanup." },
      { role: 'tool', text: 'Edit src/components/Comments.tsx' },
    ],
  },
  {
    id: 's06', agent: 'opencode', project: 'rusty-search', age: 2 * D + 4 * H, model: 'kimi-k3',
    title: 'Benchmark FTS5 trigram against the LIKE fallback',
    messages: [
      { role: 'user', text: 'How much slower is the LIKE fallback for two-character queries?' },
      { role: 'tool', text: 'Bash cargo bench --bench search' },
      { role: 'assistant', text: 'On 5.4M rows: trigram MATCH takes 0.8 ms, LIKE takes 41 ms. Fine for queries shorter than three characters, which is the only time it runs.' },
    ],
  },
  {
    id: 's07', agent: 'copilot', project: 'dotfiles', age: 3 * D,
    title: 'zsh takes 1.2 seconds to start',
    messages: [
      { role: 'user', text: 'Opening a new terminal tab takes over a second. Find out why.' },
      { role: 'tool', text: 'Bash zsh -i -c exit --profile' },
      { role: 'assistant', text: 'nvm init accounts for 900 ms of it. I lazy-load nvm on the first call to node, npm or npx; startup is now 180 ms.' },
    ],
  },
  {
    id: 's08', agent: 'kiro', project: 'oss-metrics', age: 4 * D, model: 'claude-sonnet-4.5',
    title: 'Move CI from CircleCI to GitHub Actions',
    messages: [
      { role: 'user', text: 'Port .circleci/config.yml to GitHub Actions and keep the dependency cache.' },
      { role: 'assistant', text: 'The three CircleCI jobs become one matrix workflow. The cache is keyed on the hash of Cargo.lock, so it survives unrelated commits.' },
      { role: 'tool', text: 'Write .github/workflows/ci.yml' },
    ],
  },
  {
    id: 's09', agent: 'grok', project: 'acme-web', age: 7 * D, model: 'grok-code-2',
    title: 'Flaky WebSocket reconnect test',
    messages: [
      { role: 'user', text: 'The reconnect test fails about one run in ten on CI.' },
      { role: 'assistant', text: 'It sleeps a fixed 500 ms, but on slow runners the reconnect backoff reaches 640 ms. The test now waits for the open event instead of sleeping.' },
    ],
  },
  {
    id: 's10', agent: 'kimi', project: 'pocket-pay', age: 8 * D, model: 'kimi-k3',
    title: '退款接口被重试时会退两次款',
    messages: [
      { role: 'user', text: '客户端超时重试的时候,退款接口会把同一笔钱退两次。' },
      { role: 'assistant', text: '加一个 Idempotency-Key 请求头。服务端用 Redis 的 SET NX 记下这个 key,保留 24 小时;重复的请求直接返回第一次的结果,不再调用支付通道。' },
      { role: 'tool', text: 'Edit server/refunds.go' },
    ],
  },
  {
    id: 's11', agent: 'dsh', project: 'rusty-search', age: 8 * H, model: 'deepseek-v4',
    title: 'Borrow checker error in indexer.rs',
    messages: [
      { role: 'user', text: 'cannot borrow `self.buf` as mutable because it is also borrowed as immutable — in indexer.rs line 88.' },
      { role: 'assistant', text: '`tokens` still holds a slice of `self.buf` when `self.flush()` runs. Collect the tokens into a Vec first, or move the flush after the loop.' },
    ],
  },
  {
    id: 's12', agent: 'codex', project: 'blog-engine', branch: 'queue', age: 14 * D, model: 'gpt-5.5',
    title: 'Move the job queue to Redis streams',
    messages: [
      { role: 'user', text: 'Bull keeps losing jobs when a worker restarts. Should we move to Redis streams?' },
      { role: 'assistant', text: 'Yes. Streams with consumer groups give at-least-once delivery: a job a worker never acknowledged stays pending, and XAUTOCLAIM hands it to another worker after 60 seconds.' },
      { role: 'tool', text: 'Edit worker/queue.ts' },
    ],
  },
  {
    id: 's13', agent: 'qoder', project: 'oss-metrics', age: 3 * D + 5 * H,
    title: 'Migration for the events table',
    messages: [
      { role: 'user', text: 'Add an index on created_at and backfill the source column.' },
      { role: 'assistant', text: 'Migration 0042 builds the index concurrently and backfills in batches of 10,000, so the table is never locked for writes.' },
    ],
  },
  {
    id: 's14', agent: 'devin', project: 'acme-web', age: 1 * D + 6 * H, model: 'devin-2',
    title: 'Settings page ignores dark mode',
    messages: [
      { role: 'user', text: 'The settings page stays white in dark mode.' },
      { role: 'assistant', text: 'It hard-codes #fff. Replaced it with the --surface and --text tokens and checked contrast at 4.5:1 in both themes.' },
    ],
  },
  {
    id: 's15', agent: 'kilo', project: 'pocket-pay', age: 6 * D,
    title: 'Upgrade React Native to 0.79',
    messages: [
      { role: 'user', text: 'Upgrade to React Native 0.79 and fix whatever breaks.' },
      { role: 'assistant', text: 'Two native modules needed new versions; the camera module had to move to the new architecture. iOS and Android builds pass.' },
    ],
  },
  {
    id: 's16', agent: 'zcode', project: 'rusty-search', age: 4 * D + 3 * H, model: 'glm-5.3',
    title: '给搜索结果加上高亮',
    messages: [
      { role: 'user', text: '搜索结果里把命中的词高亮出来。' },
      { role: 'assistant', text: '在 Rust 里按原文取片段:以最早出现的词为锚,前 40 后 80 个字符,窗口里的每个词都包一层高亮。大小写逐字符折叠,中文和代码一样能命中。' },
    ],
  },
  {
    id: 's17', agent: 'claude-code', project: 'oss-metrics', age: 2 * D + 1 * H, model: 'claude-fable-5',
    title: 'Weekly active users query is slow',
    messages: [
      { role: 'user', text: 'The weekly active users query takes 9 seconds.' },
      { role: 'assistant', text: "It scans every event since launch. A partial index on (user_id, ts) covering the last 35 days brings it to 120 ms; I'll add it in a migration." },
    ],
  },
  {
    id: 's18', agent: 'cursor', project: 'acme-web', age: 10 * H, model: 'composer-2',
    title: 'Load plugins asynchronously',
    messages: [
      { role: 'user', text: 'Plugin loading blocks startup. Make it async.' },
      { role: 'assistant', text: 'Plugins now load in parallel with Promise.allSettled; one broken plugin logs an error instead of stopping the others.' },
    ],
  },
  {
    id: 's19', agent: 'pi', project: 'dotfiles', age: 16 * D,
    title: 'Neovim: set up rust-analyzer',
    messages: [
      { role: 'user', text: 'rust-analyzer keeps re-indexing on every save in Neovim.' },
      { role: 'assistant', text: 'checkOnSave runs clippy on the whole workspace. Pointing it at the current crate and excluding target/ from the watcher stops the loop.' },
    ],
  },
  {
    id: 's20', agent: 'hermes', project: 'blog-engine', age: 5 * D + 2 * H,
    title: 'Release notes for 2.4',
    messages: [
      { role: 'user', text: 'Draft the release notes for 2.4 from the merged pull requests.' },
      { role: 'assistant', text: 'Grouped 31 pull requests into New, Fix and Update; the Redis streams queue leads, since it changes how workers are deployed.' },
    ],
  },
  {
    id: 's21', agent: 'claude-code', project: 'rusty-search', age: 4 * D + 6 * H, model: 'claude-fable-5',
    title: '重建索引时进度条停在 0%',
    messages: [
      { role: 'user', text: '重建索引的时候进度条一直停在 0%,结束了才一下跳到 100%。' },
      { role: 'assistant', text: '进度事件是在扫描线程里攒着、结束时才发的。改成每处理 64 个文件发一次,并且不管正常结束还是出错,收尾时都一定发一次终态。' },
      { role: 'tool', text: 'Edit src/scanner.rs' },
    ],
  },
  {
    id: 's22', agent: 'antigravity', project: 'pocket-pay', age: 9 * D, model: 'gemini-3-pro',
    title: 'Webhook signature check fails in staging',
    messages: [
      { role: 'user', text: 'Payment webhooks fail signature verification in staging only.' },
      { role: 'assistant', text: 'Staging sits behind a proxy that re-encodes the JSON body. Verify the signature against the raw bytes, before any body parser runs.' },
    ],
  },
];

/** Searches the auto-demo cycles through; each lands on hits from more than one agent. */
export const DEMO_QUERIES = ['rate limiter', '二维码', 'useEffect(', 'redis'];
