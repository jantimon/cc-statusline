//! cc-statusline: a two-row Claude Code status line.
//!
//! Row 1: `dir (branch) | model | <bar> pct%` (parity with the old bash script).
//! Row 2: `cost | models | in / out / cache write / cache read / total | session`.
//!
//! Row 2 needs numbers the status-line payload does not carry, so it parses the
//! transcript directly: the main `<project>/<session>.jsonl` plus every
//! `<project>/<session>/subagents/*.jsonl` (subagents run their own models and
//! carry their own usage). Transcripts are append-only, so parsing is incremental:
//! a per-file byte offset and per-model token buckets are cached, and each render
//! only reads the bytes appended since last time.
//!
//! Two cost figures. `cost.total_cost_usd` from the payload is authoritative but
//! covers the current CLI run only. The lifetime figure is computed from every
//! transcript token times per-model pricing. They diverge once a session is
//! resumed across runs, so row 2 shows the estimate only when the gap exceeds 10%.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

// ---------- statusline stdin payload (only the fields we use) ----------

#[derive(Deserialize, Default)]
struct Payload {
    #[serde(default)]
    model: ModelField,
    #[serde(default)]
    workspace: Workspace,
    #[serde(default)]
    cost: Cost,
    #[serde(default)]
    context_window: Option<ContextWindow>,
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    transcript_path: String,
}

#[derive(Deserialize, Default)]
struct ModelField {
    #[serde(default)]
    display_name: String,
}

#[derive(Deserialize, Default)]
struct Workspace {
    #[serde(default)]
    project_dir: String,
    #[serde(default)]
    current_dir: String,
}

#[derive(Deserialize, Default)]
struct Cost {
    #[serde(default)]
    total_cost_usd: f64,
}

#[derive(Deserialize)]
struct ContextWindow {
    #[serde(default)]
    current_usage: Option<CurrentUsage>,
    #[serde(default)]
    context_window_size: u64,
}

#[derive(Deserialize, Default)]
struct CurrentUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

// ---------- transcript line (one JSONL record) ----------

#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type", default)]
    ty: String,
    #[serde(rename = "requestId", default)]
    request_id: String,
    #[serde(default)]
    message: Option<Msg>,
}

#[derive(Deserialize)]
struct Msg {
    #[serde(default)]
    id: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    /// Cache-creation tokens split by TTL. 1h writes cost 2x input, 5m 1.25x.
    #[serde(default)]
    cache_creation: Option<CacheCreation>,
}

#[derive(Deserialize, Default)]
struct CacheCreation {
    #[serde(default)]
    ephemeral_1h_input_tokens: u64,
    #[serde(default)]
    ephemeral_5m_input_tokens: u64,
}

// ---------- persisted cache ----------

/// Per-model token buckets. Cache-creation is split by TTL so it can be priced.
#[derive(Serialize, Deserialize, Default, Clone)]
struct ModelTokens {
    input: u64,
    output: u64,
    cw_5m: u64,
    cw_1h: u64,
    cr: u64,
}

#[derive(Serialize, Deserialize, Default)]
struct Cache {
    /// short model name -> token buckets (subagent models included).
    #[serde(default)]
    models: HashMap<String, ModelTokens>,
    #[serde(default)]
    offsets: HashMap<String, u64>,
    /// `hash(message.id, requestId)` -> output_tokens already counted. Streaming
    /// writes the same record several times; input/cache stay constant (count
    /// once), output grows to a final value (keep the max via positive deltas).
    #[serde(default)]
    seen: HashMap<u64, u64>,
}

// ---------- pricing (USD per 1M tokens) ----------

struct Price {
    input: f64,
    output: f64,
    cw_5m: f64,
    cw_1h: f64,
    cr: f64,
}

/// Per-family pricing. Cache-write 5m is 1.25x input, 1h is 2x input, read is 0.1x.
/// Bump when Anthropic changes prices. Unknown families fall through to opus-tier.
fn price_for(family: &str) -> Price {
    match family {
        "haiku" => Price {
            input: 1.0,
            output: 5.0,
            cw_5m: 1.25,
            cw_1h: 2.0,
            cr: 0.10,
        },
        "sonnet" => Price {
            input: 3.0,
            output: 15.0,
            cw_5m: 3.75,
            cw_1h: 6.0,
            cr: 0.30,
        },
        "fable" => Price {
            input: 10.0,
            output: 50.0,
            cw_5m: 12.5,
            cw_1h: 20.0,
            cr: 1.0,
        },
        _ => Price {
            input: 5.0,
            output: 25.0,
            cw_5m: 6.25,
            cw_1h: 10.0,
            cr: 0.50,
        },
    }
}

fn model_cost(name: &str, t: &ModelTokens) -> f64 {
    let family = name.split('-').next().unwrap_or(name);
    let p = price_for(family);
    (t.input as f64 * p.input
        + t.output as f64 * p.output
        + t.cw_5m as f64 * p.cw_5m
        + t.cw_1h as f64 * p.cw_1h
        + t.cr as f64 * p.cr)
        / 1_000_000.0
}

// ANSI colors (match the old script).
const CYAN: &str = "\x1b[36m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const LAVENDER: &str = "\x1b[38;2;178;185;244m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const LABEL: &str = "\x1b[38;2;150;158;178m"; // readable muted gray-blue for labels
const RESET: &str = "\x1b[0m";

fn main() {
    let mut input = String::new();
    let _ = io::stdin().read_to_string(&mut input);
    let p: Payload = serde_json::from_str(&input).unwrap_or_default();

    let agg = aggregate(&p);

    let row1 = build_row1(&p);
    let row2 = build_row2(&p, &agg);

    let mut out = io::stdout();
    let _ = write!(out, "{row1}\n{row2}");
}

// ---------- aggregation ----------

struct Agg {
    input: u64,
    output: u64,
    cache_write: u64,
    cache_read: u64,
    models: Vec<String>,
    /// Lifetime cost, computed from every transcript token times per-model pricing.
    cost: f64,
}

fn aggregate(p: &Payload) -> Agg {
    let empty = Agg {
        input: 0,
        output: 0,
        cache_write: 0,
        cache_read: 0,
        models: Vec::new(),
        cost: 0.0,
    };
    if p.transcript_path.is_empty() {
        return empty;
    }
    let transcript = PathBuf::from(&p.transcript_path);
    if !transcript.exists() {
        return empty;
    }

    // Canonical session key = the transcript file stem (matches the on-disk
    // subagents dir name); fall back to payload session_id.
    let stem = transcript
        .file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| p.session_id.clone());

    // File set: main transcript + <project>/<stem>/subagents/*.jsonl
    let mut files: Vec<PathBuf> = vec![transcript.clone()];
    if let Some(parent) = transcript.parent() {
        let subdir = parent.join(&stem).join("subagents");
        if let Ok(entries) = fs::read_dir(&subdir) {
            for e in entries.flatten() {
                let path = e.path();
                if path.extension().and_then(|x| x.to_str()) == Some("jsonl") {
                    files.push(path);
                }
            }
        }
    }

    let cache_path = cache_path_for(&stem);
    let mut cache: Cache = fs::read(&cache_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();

    // Shrink guard: if a tracked file is now smaller than its recorded offset it
    // was rewritten (compaction, rotation), so recompute from scratch.
    let shrank = cache
        .offsets
        .iter()
        .any(|(path, &off)| fs::metadata(path).map(|m| off > m.len()).unwrap_or(false));
    if shrank {
        cache = Cache::default();
    }

    for path in &files {
        let key = path.to_string_lossy().to_string();
        let off = cache.offsets.get(&key).copied().unwrap_or(0);
        let Ok(mut f) = File::open(path) else {
            continue;
        };
        let Ok(meta) = f.metadata() else { continue };
        let len = meta.len();
        if off >= len {
            continue; // nothing new
        }
        if f.seek(SeekFrom::Start(off)).is_err() {
            continue;
        }
        let mut buf = Vec::new();
        if f.read_to_end(&mut buf).is_err() {
            continue;
        }
        // Only consume up to the last newline; a trailing partial line is left
        // for the next render (guards against reading a half-written record).
        let Some(last_nl) = buf.iter().rposition(|&b| b == b'\n') else {
            continue;
        };
        for line in buf[..=last_nl].split(|&b| b == b'\n') {
            if line.is_empty() {
                continue;
            }
            process_line(line, &mut cache);
        }
        cache.offsets.insert(key, off + last_nl as u64 + 1);
    }

    write_cache_atomic(&cache_path, &cache);

    // Derive display globals + lifetime cost from the per-model buckets.
    let mut agg = empty;
    for (name, t) in &cache.models {
        agg.input += t.input;
        agg.output += t.output;
        agg.cache_write += t.cw_5m + t.cw_1h;
        agg.cache_read += t.cr;
        agg.cost += model_cost(name, t);
        agg.models.push(name.clone());
    }
    agg
}

/// Add a record's full token counts to a model bucket (cache-creation split by TTL).
fn add_full(m: &mut ModelTokens, u: &Usage) {
    m.input += u.input_tokens;
    m.output += u.output_tokens;
    m.cr += u.cache_read_input_tokens;
    match &u.cache_creation {
        Some(cc) if cc.ephemeral_1h_input_tokens + cc.ephemeral_5m_input_tokens > 0 => {
            m.cw_1h += cc.ephemeral_1h_input_tokens;
            m.cw_5m += cc.ephemeral_5m_input_tokens;
        }
        // No TTL split present, so assume 5m (the cheaper rate).
        _ => m.cw_5m += u.cache_creation_input_tokens,
    }
}

fn process_line(line: &[u8], cache: &mut Cache) {
    let Ok(rec) = serde_json::from_slice::<Line>(line) else {
        return;
    };
    if rec.ty != "assistant" {
        return;
    }
    let request_id = rec.request_id;
    let Some(msg) = rec.message else { return };
    let Some(usage) = msg.usage else { return };
    let model = msg.model.unwrap_or_default();
    if model == "<synthetic>" || model.is_empty() {
        return;
    }
    let short = short_model(&model);
    let out = usage.output_tokens;

    // Dedup by (message.id, requestId); see Cache::seen. Lines without an id
    // can't be deduped, so count them in full. A duplicate shares its original's
    // model, so an output delta always lands in the same bucket.
    if msg.id.is_empty() {
        add_full(cache.models.entry(short).or_default(), &usage);
        return;
    }
    let key = dedup_hash(&msg.id, &request_id);
    match cache.seen.get(&key).copied() {
        None => {
            add_full(cache.models.entry(short).or_default(), &usage);
            cache.seen.insert(key, out);
        }
        Some(prev_out) => {
            if out > prev_out {
                cache.models.entry(short).or_default().output += out - prev_out;
                cache.seen.insert(key, out);
            }
        }
    }
}

// ---------- cache location + atomic write ----------

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn cache_path_for(stem: &str) -> PathBuf {
    let dir = home().join(".claude").join("statusline-cache");
    let _ = fs::create_dir_all(&dir);
    dir.join(format!("{stem}.json"))
}

fn write_cache_atomic(path: &Path, cache: &Cache) {
    let Ok(bytes) = serde_json::to_vec(cache) else {
        return;
    };
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    if fs::write(&tmp, &bytes).is_ok() {
        let _ = fs::rename(&tmp, path);
    }
}

// ---------- formatting helpers ----------

/// Deterministic FNV-1a hash of `id|requestId` for dedup (std's RandomState is
/// per-process randomized, which would break the persisted seen-set).
fn dedup_hash(id: &str, req: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in id.bytes().chain(std::iter::once(b'|')).chain(req.bytes()) {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// Newest version per family. A model on it renders as the bare family name
/// (`opus`, not `opus-4.8`). Bump when new models ship.
fn latest_version(family: &str) -> Option<&'static str> {
    match family {
        "opus" => Some("4.8"),
        "sonnet" => Some("5"),
        "haiku" => Some("4.5"),
        "fable" => Some("5"),
        _ => None,
    }
}

/// `claude-opus-4-8` -> `opus` (latest), `claude-haiku-4-5-20251001` -> `haiku`
/// (latest), an older `claude-opus-4-7` -> `opus-4.7`.
fn short_model(m: &str) -> String {
    let base = m.strip_prefix("claude-").unwrap_or(m);
    let mut parts: Vec<&str> = base.split('-').collect();
    // Drop a trailing date stamp like `20251001`.
    if let Some(last) = parts.last() {
        if last.len() == 8 && last.chars().all(|c| c.is_ascii_digit()) {
            parts.pop();
        }
    }
    if parts.is_empty() {
        return base.to_string();
    }
    let family = parts[0];
    let version = parts[1..].join(".");
    if version.is_empty() || latest_version(family) == Some(version.as_str()) {
        family.to_string()
    } else {
        format!("{family}-{version}")
    }
}

/// Distinct ANSI color per model family so multiple models are easy to tell
/// apart in row 2.
fn model_color(short: &str) -> &'static str {
    let family = short.split('-').next().unwrap_or(short);
    match family {
        "opus" => "\x1b[38;2;178;185;244m", // lavender
        "sonnet" => "\x1b[36m",             // cyan
        "haiku" => "\x1b[35m",              // magenta
        "fable" => "\x1b[32m",              // green
        _ => YELLOW,
    }
}

/// Compact humanized token count: `736k`, `4.9M`, `1.1B`.
fn humanize(n: u64) -> String {
    let f = n as f64;
    if n >= 1_000_000_000 {
        format!("{:.1}B", f / 1e9)
    } else if n >= 1_000_000 {
        format!("{:.1}M", f / 1e6)
    } else if n >= 1_000 {
        format!("{:.0}k", f / 1e3)
    } else {
        n.to_string()
    }
}

/// `$28.85` under $100, `$811` above (no cents where they'd be noise).
fn money(v: f64) -> String {
    if v >= 100.0 {
        format!("${:.0}", v)
    } else {
        format!("${:.2}", v)
    }
}

fn git_branch(dir: &str) -> String {
    if dir.is_empty() {
        return String::new();
    }
    let out = Command::new("git")
        .args(["-C", dir, "--no-optional-locks", "branch", "--show-current"])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let b = String::from_utf8_lossy(&o.stdout).trim().to_string();
            b // empty string in detached HEAD is fine
        }
        _ => String::new(),
    }
}

fn build_row1(p: &Payload) -> String {
    let project = &p.workspace.project_dir;
    let current = &p.workspace.current_dir;

    // Relative path from project root (empty when at root).
    let rel = if current == project {
        String::new()
    } else if !project.is_empty() && current.starts_with(&format!("{project}/")) {
        current[project.len() + 1..].to_string()
    } else {
        current.clone()
    };

    let branch = git_branch(current);

    // Context window bar.
    let (bar, pct) = context_bar(p);

    let mut parts: Vec<String> = Vec::new();
    if !rel.is_empty() {
        parts.push(format!("{CYAN}{rel}{RESET}"));
    }
    if !branch.is_empty() {
        parts.push(format!("{GREEN}({branch}){RESET}"));
    }
    let mut left = parts.join(" ");
    if !left.is_empty() {
        left.push_str(" | ");
    }

    let model = &p.model.display_name;
    format!("{left}{YELLOW}{model}{RESET} | {LAVENDER}{bar}{RESET} {pct}%")
}

fn context_bar(p: &Payload) -> (String, u64) {
    let pct = match &p.context_window {
        Some(cw) => match &cw.current_usage {
            Some(u) if cw.context_window_size > 0 => {
                let used =
                    u.input_tokens + u.cache_creation_input_tokens + u.cache_read_input_tokens;
                used * 100 / cw.context_window_size
            }
            _ => 0,
        },
        None => 0,
    };
    let width = 10u64;
    let filled = (pct * width / 100).min(width) as usize;
    let empty = width as usize - filled;
    let bar = "█".repeat(filled) + &"░".repeat(empty);
    (bar, pct)
}

/// Authoritative run cost, plus the computed lifetime estimate when the two
/// differ by more than 10% (which happens once a session is resumed across runs).
fn cost_field(run: f64, computed: f64) -> String {
    if run <= 0.0 {
        // No authoritative value, so show the estimate alone (or $0.00).
        return if computed > 0.0 {
            format!("{GREEN}~{}{RESET}", money(computed))
        } else {
            format!("{GREEN}$0.00{RESET}")
        };
    }
    let within_10pct = computed <= 0.0 || (computed - run).abs() <= 0.10 * run;
    if within_10pct {
        format!("{GREEN}{}{RESET}", money(run))
    } else {
        format!(
            "{GREEN}{}{RESET} {DIM}run{RESET} {DIM}·{RESET} {GREEN}~{}{RESET} {DIM}total{RESET}",
            money(run),
            money(computed)
        )
    }
}

fn build_row2(p: &Payload, agg: &Agg) -> String {
    let cost = cost_field(p.cost.total_cost_usd, agg.cost);

    let mut models = agg.models.clone();
    models.sort();
    let models_str = if models.is_empty() {
        format!("{DIM}-{RESET}")
    } else {
        models
            .iter()
            .map(|m| format!("{}{m}{RESET}", model_color(m)))
            .collect::<Vec<_>>()
            .join(&format!("{DIM}+{RESET}"))
    };

    let total = agg.input + agg.output + agg.cache_write + agg.cache_read;

    let session_id = if !p.session_id.is_empty() {
        p.session_id.clone()
    } else {
        PathBuf::from(&p.transcript_path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string()
    };
    let session8: String = session_id.chars().take(8).collect();

    let tok = format!(
        "{LABEL}in{RESET} {BOLD}{}{RESET} {DIM}·{RESET} {LABEL}out{RESET} {BOLD}{}{RESET} {DIM}·{RESET} \
         {LABEL}in cache{RESET} {BOLD}{}{RESET} {DIM}·{RESET} {LABEL}out cache{RESET} {BOLD}{}{RESET} {DIM}·{RESET} \
         {LABEL}total{RESET} {BOLD}{}{RESET}",
        humanize(agg.input),
        humanize(agg.output),
        humanize(agg.cache_write),
        humanize(agg.cache_read),
        humanize(total),
    );

    format!(
        "{cost} {DIM}|{RESET} {models_str} {DIM}|{RESET} {tok} \
         {DIM}|{RESET} {CYAN}{session8}{RESET}"
    )
}
