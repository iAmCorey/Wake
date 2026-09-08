#!/bin/zsh
# Wake 本地测试入口——改完代码跑一跑(对齐 kooky 的 `swift test` 习惯)。
#
#   scripts/test.sh           # 数据层 + UI 单元测试 + UI 编译门槛,~秒级
#   scripts/test.sh --smoke   # 追加真实数据冒烟:全量扫描统计(人眼对量级,
#                             # 基准见 CLAUDE.md;只读你本机的 agent 数据)
set -euo pipefail
cd "$(dirname "$0")/.."

echo "── cargo test -p wake-core(adapter 契约 / seq 一致性 / FTS / 扫描终态 / wake-mcp stdio)"
cargo test -p wake-core --quiet

echo "── cargo test -p wake(UI 单元逻辑；联网测试默认忽略)"
cargo test -p wake --quiet

echo "── cargo check -p wake(UI 编译门槛)"
cargo check -p wake --quiet

if [[ "${1:-}" == "--smoke" ]]; then
  echo "── scan 冒烟(真实数据,量级应符合 CLAUDE.md 基准)"
  cargo run -p wake-core --bin scan -- --quiet
fi

echo "✓ all green"
