//! The `statusline` subcommand: a Claude Code status line rendered by the binary
//! itself -- no `sh`/`jq`/`date`/`grep` needed, and it works on every platform
//! the binary targets. Reads the harness JSON context on stdin and prints:
//!
//! ```text
//! plouf <n>n/<e>e [<age> ago] | <model> | <dir> | ctx <k>k
//! ```
//!
//! `[<age> ago]` is how long since `plouf-rs` was last queried (from the
//! transcript); a stale value -- or `[unused]` -- is a visible cue that the
//! agent has stopped reaching for the graph.

use std::io::{Read, Seek, SeekFrom};

use serde_json::Value;

/// Read the harness context on stdin and print one status line.
pub fn run() -> Result<(), std::io::Error> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let ctx: Value = serde_json::from_str(&input).unwrap_or(Value::Null);
    println!("{}", render(&ctx));
    Ok(())
}

/// Build the status line from an already-parsed harness context (the pure part
/// of [`run`], split out so it is testable without stdin/stdout).
fn render(ctx: &Value) -> String {
    let proj = ctx["workspace"]["project_dir"].as_str().or_else(|| ctx["cwd"].as_str()).unwrap_or(".");
    let dir = ctx["workspace"]["current_dir"].as_str().or_else(|| ctx["cwd"].as_str()).unwrap_or(".");
    let model = ctx["model"]["display_name"].as_str().or_else(|| ctx["model"]["id"].as_str()).unwrap_or("?");
    let transcript = ctx["transcript_path"].as_str().unwrap_or("");

    let (recency, ctx_k) = transcript_stats(transcript);
    let dir_base = dir.rsplit('/').next().unwrap_or(dir);
    let ctx_seg = ctx_k.map_or_else(String::new, |k| format!(" | ctx {k}k"));

    format!("{} [{recency}] | {model} | {dir_base}{ctx_seg}", graph_size(proj))
}

/// Node/edge counts from the `stats.json` sidecar `plouf-rs index` writes; a
/// terse "(no index)" when it is absent.
fn graph_size(proj: &str) -> String {
    let path = format!("{proj}/build/plouf-rs-out/.graph/stats.json");
    std::fs::read_to_string(path).ok().map_or_else(
        || "plouf (no index)".to_string(),
        |s| {
            let v: Value = serde_json::from_str(&s).unwrap_or(Value::Null);
            format!("plouf {}n/{}e", v["nodes"].as_u64().unwrap_or(0), v["edges"].as_u64().unwrap_or(0))
        },
    )
}

/// `(recency, ctx-thousands)` from the transcript tail: the age of the newest
/// `plouf-rs` query, and the most recent message's token usage.
fn transcript_stats(path: &str) -> (String, Option<u64>) {
    let Some(tail) = read_tail(path, 4_000_000) else {
        return ("unused".to_string(), None);
    };
    let mut last_query: Option<i64> = None;
    let mut tokens: Option<u64> = None;
    for line in tail.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if let Some(t) = usage_tokens(&v) {
            tokens = Some(t / 1000);
        }
        if is_plouf_query(&v) {
            if let Some(ts) = v["timestamp"].as_str().and_then(parse_iso_epoch) {
                last_query = Some(ts);
            }
        }
    }
    let recency = last_query.map_or_else(|| "unused".to_string(), |ts| format_age(now_epoch().saturating_sub(ts)));
    (recency, tokens)
}

/// Read at most the last `max` bytes of a file (lossily), so a multi-MB
/// transcript stays cheap. `None` on an empty path or a read error.
fn read_tail(path: &str, max: u64) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    if len > max {
        f.seek(SeekFrom::Start(len - max)).ok()?;
    }
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Total input tokens (fresh + cached) of a message's `usage`, if present.
fn usage_tokens(v: &Value) -> Option<u64> {
    let u = &v["message"]["usage"];
    let t = u["input_tokens"].as_u64().unwrap_or(0)
        + u["cache_read_input_tokens"].as_u64().unwrap_or(0)
        + u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
    (t > 0).then_some(t)
}

/// Is this entry an assistant `Bash` tool call that runs `plouf-rs`?
fn is_plouf_query(v: &Value) -> bool {
    v["message"]["content"].as_array().is_some_and(|content| {
        content.iter().any(|c| {
            c["type"] == "tool_use"
                && c["name"] == "Bash"
                && c["input"]["command"].as_str().is_some_and(|cmd| cmd.contains("plouf-rs"))
        })
    })
}

/// Parse a UTC ISO-8601 timestamp (`2026-08-24T10:19:02.221Z`) to epoch seconds,
/// ignoring the fractional part and the trailing `Z`.
fn parse_iso_epoch(s: &str) -> Option<i64> {
    let field = |a: usize, z: usize| -> Option<i64> { s.get(a..z)?.parse().ok() };
    let (y, mo, d) = (field(0, 4)?, field(5, 7)?, field(8, 10)?);
    let (h, mi, se) = (field(11, 13)?, field(14, 16)?, field(17, 19)?);
    Some(days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + se)
}

/// Days from 1970-01-01 to `y-m-d` (Howard Hinnant's algorithm) -- no chrono dep.
const fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs().cast_signed())
}

/// A compact age: `45s ago` / `3m ago` / `2h ago` / `5d ago`.
fn format_age(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{
        days_from_civil, format_age, graph_size, is_plouf_query, parse_iso_epoch, read_tail, render,
        transcript_stats, usage_tokens,
    };
    use serde_json::json;

    #[test]
    fn epoch_and_civil_days_are_correct() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(parse_iso_epoch("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso_epoch("2000-01-01T00:00:00Z"), Some(946_684_800));
        assert_eq!(parse_iso_epoch("2026-08-24T10:19:02.221Z"), Some(1_787_566_742));
        assert_eq!(parse_iso_epoch("nonsense"), None);
    }

    #[test]
    fn age_formatting_buckets() {
        assert_eq!(format_age(45), "45s ago");
        assert_eq!(format_age(120), "2m ago");
        assert_eq!(format_age(7200), "2h ago");
        assert_eq!(format_age(172_800), "2d ago");
    }

    #[test]
    fn detects_a_plouf_bash_call_and_usage() {
        let plouf = json!({"message": {"content": [
            {"type": "text", "text": "hi"},
            {"type": "tool_use", "name": "Bash", "input": {"command": "plouf-rs callers X"}}
        ]}});
        let other = json!({"message": {"content": [
            {"type": "tool_use", "name": "Bash", "input": {"command": "grep -r X app/"}}
        ]}});
        assert!(is_plouf_query(&plouf));
        assert!(!is_plouf_query(&other));
        assert!(!is_plouf_query(&json!({"type": "user", "message": {"content": "plouf-rs is great"}})));

        let usage = json!({"message": {"usage": {"input_tokens": 10, "cache_read_input_tokens": 5, "cache_creation_input_tokens": 2}}});
        assert_eq!(usage_tokens(&usage), Some(17));
        assert_eq!(usage_tokens(&json!({"message": {}})), None);
    }

    #[test]
    fn graph_size_reads_stats_or_reports_no_index() {
        let proj = std::env::temp_dir().join(format!("plouf_sl_gs_{}", std::process::id()));
        let graph = proj.join("build/plouf-rs-out/.graph");
        std::fs::create_dir_all(&graph).unwrap();
        std::fs::write(graph.join("stats.json"), r#"{"nodes":10,"edges":20}"#).unwrap();
        assert_eq!(graph_size(proj.to_str().unwrap()), "plouf 10n/20e");
        // A project with no index sidecar reports a terse marker.
        let empty = std::env::temp_dir().join(format!("plouf_sl_none_{}", std::process::id()));
        assert_eq!(graph_size(empty.to_str().unwrap()), "plouf (no index)");
        std::fs::remove_dir_all(&proj).ok();
    }

    #[test]
    fn read_tail_returns_last_bytes_and_handles_missing() {
        let f = std::env::temp_dir().join(format!("plouf_sl_tail_{}", std::process::id()));
        std::fs::write(&f, "abcdefghij").unwrap();
        // A max larger than the file yields the whole content; a smaller one, the tail.
        assert_eq!(read_tail(f.to_str().unwrap(), 100).as_deref(), Some("abcdefghij"));
        assert_eq!(read_tail(f.to_str().unwrap(), 4).as_deref(), Some("ghij"));
        // An empty path and a missing file both yield None.
        assert_eq!(read_tail("", 10), None);
        assert_eq!(read_tail("/no/such/plouf/transcript", 10), None);
        std::fs::remove_file(&f).ok();
    }

    #[test]
    fn transcript_stats_reports_recency_and_context_tokens() {
        let f = std::env::temp_dir().join(format!("plouf_sl_ts_{}", std::process::id()));
        let plouf_call = json!({
            "timestamp": "2020-01-02T03:04:05.000Z",
            "message": {"content": [
                {"type": "tool_use", "name": "Bash", "input": {"command": "plouf-rs find X"}}
            ]}
        });
        let usage = json!({"message": {"usage": {"input_tokens": 40000, "cache_read_input_tokens": 5000}}});
        std::fs::write(&f, format!("{plouf_call}\n{usage}\n")).unwrap();
        let (recency, ctx_k) = transcript_stats(f.to_str().unwrap());
        // A 2020 query is years in the past -> an "Nd ago" recency, never "unused".
        assert!(recency.ends_with("ago") && recency != "unused", "recency: {recency}");
        assert_eq!(ctx_k, Some(45)); // (40000 + 5000) / 1000
        // An empty transcript path -> unused, no tokens.
        assert_eq!(transcript_stats(""), ("unused".to_string(), None));
        std::fs::remove_file(&f).ok();
    }

    #[test]
    fn render_builds_the_status_line_with_fallbacks() {
        // No index, no transcript: falls back to "(no index)" / "[unused]", and
        // the cwd is shown by its basename with no ctx segment.
        let ctx = json!({
            "workspace": {"project_dir": "/no/such/plouf/proj", "current_dir": "/srv/apps/myrepo"},
            "model": {"display_name": "Opus 4.8"},
            "transcript_path": ""
        });
        assert_eq!(render(&ctx), "plouf (no index) [unused] | Opus 4.8 | myrepo");

        // Model id is the fallback when there is no display name; `.` when the
        // workspace is absent entirely.
        let bare = json!({"model": {"id": "claude-x"}});
        assert_eq!(render(&bare), "plouf (no index) [unused] | claude-x | .");
    }
}
