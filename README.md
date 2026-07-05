# cc-statusline

A two-row status line for [Claude Code](https://docs.claude.com/en/docs/claude-code).
It replaces the default single line with a per-session token and cost breakdown,
read straight from the transcript on disk.

```
src (main) | Opus 4.8 | ████████░░ 80%
$28.85 run · ~$811 total | haiku+opus | in 880k · out 4.9M · in cache 15.0M · out cache 1.1B · total 1.1B | 9a5e80ae
```

Row 1 is the usual directory, git branch, model, and context-window bar. Row 2 is
the new part: cost, the models actually used (main thread plus subagents), the
token breakdown, and the session id.

## Why

The payload Claude Code hands the status-line command carries the current
context-window usage and `total_cost_usd`, but not the cumulative token counts.
`ccusage` has them but runs as a separate command. This puts the same numbers in
the footer, cheap enough to run on every render.

## Install

Needs a Rust toolchain (`rustup`) and `git` on PATH.

```sh
git clone git@github.com:jantimon/cc-statusline.git
cd cc-statusline
cargo build --release
```

Point Claude Code at the binary in `~/.claude/settings.json`:

```json
{
  "statusLine": {
    "type": "command",
    "command": "/absolute/path/to/cc-statusline/target/release/cc-statusline",
    "padding": 0
  }
}
```

Reload the status line (start a new session, or run `/config`) to pick it up.

## How it works

Claude Code stores each session as append-only JSONL: the main thread at
`~/.claude/projects/<project>/<session>.jsonl`, and every Task subagent as its own
file under `<session>/subagents/`. cc-statusline reads all of them.

- **Incremental.** A per-file byte offset and the running per-model token totals
  live in `~/.claude/statusline-cache/<session>.json`. Each render parses only the
  bytes appended since the previous one. Cold start on a 100 MB transcript is a
  one-time cost; warm renders are single-digit milliseconds.
- **Deduped.** Streaming writes the same assistant record several times with a
  growing `output_tokens`. Records are deduped by `(message.id, requestId)`,
  keeping the max output. This matches how `ccusage` counts.
- **Subagents count.** They run their own models (often haiku) with their own
  usage, so skipping `subagents/` both undercounts tokens and hides models. The
  model list is the union across every file.

## The two cost figures

`total_cost_usd` from the payload is authoritative, but it only covers the current
CLI process. A session resumed over several days shows a small run cost against a
much larger transcript. So row 2 also computes a lifetime estimate from every
transcript token times per-model pricing:

- `$28.85` on its own when the run cost and the estimate agree within 10%.
- `$28.85 run · ~$811 total` when they diverge (a resumed session).

The estimate carries a `~` because pricing lives in a hardcoded table
(`price_for`), which drifts when Anthropic changes prices. The authoritative
number is never wrong; the estimate exists to make the token breakdown add up.

## Maintenance

Two tables in `src/main.rs` are the only things that go stale:

- `price_for`: per-family prices per 1M tokens.
- `latest_version`: newest version per family, used to render the current model as
  a bare `opus` instead of `opus-4.8`.

Rebuild after editing. The status line runs the compiled binary, not the source.
