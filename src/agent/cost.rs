//! Best-effort cost extraction from agent CLI output.
//!
//! Fleet runs agents as opaque external binaries (Claude Code, aider,
//! whatever the user has wired into `agents.registry`). Each prints a
//! summary line containing its own dollar figure at the end of a run.
//! The shapes vary across versions and across agents; we don't control
//! them. So this module is *intentionally permissive*: a small list of
//! regex-like patterns per known agent, last-match-wins (the final
//! summary tends to come last in stdout), and `None` when nothing
//! matches.
//!
//! `None` is a meaningful signal — it surfaces in the UI as "no cost
//! data" rather than `$0.00`. A user seeing `None` for a workflow
//! whose agent definitely ran knows to file a parser update; a user
//! seeing `$0.00` would assume the agent did no work.
//!
//! Why hand-written matchers instead of pulling in the `regex` crate:
//! the patterns are short (single capture group around a decimal),
//! the surface area is tiny, and adding a dependency for ~50 lines of
//! string scanning isn't worth the compile-time hit. If the parser
//! menagerie grows past two agents we'll revisit.

/// Extract a USD cost figure from an agent's captured output. `stdout`
/// and `stderr` are checked separately because some agents (notably
/// older Claude Code builds) print the summary to stderr when
/// `stdout` is redirected.
///
/// Returns `None` when no known pattern matches — different from
/// `Some(0.0)`, which would mean "matched, value is zero." Callers
/// surface the distinction in the UI.
#[must_use]
pub fn parse_agent_cost_usd(agent_name: &str, stdout: &str, stderr: &str) -> Option<f64> {
    let patterns = patterns_for(agent_name);
    if patterns.is_empty() {
        return None;
    }
    // Search both streams, return the last match across both. Stderr
    // is searched second so a trailing summary on stderr overrides an
    // earlier stdout figure (relevant when an agent reports per-
    // message costs on stdout and a session total on stderr).
    last_match_across(&[stdout, stderr], &patterns)
}

/// Patterns this module knows about, keyed by the fleet agent name (as
/// declared in `.fleet/config.yaml`'s `agents.registry`).
///
/// Adding a new agent: add a `match` arm here. Adding a new pattern
/// for an existing agent: append to the slice — order matters only
/// when multiple patterns hit; the last one wins. Pattern format:
/// `(prefix, suffix)`. The cost figure must appear between the two.
fn patterns_for(agent_name: &str) -> Vec<CostPattern> {
    match agent_name {
        "claude-code" => vec![
            // Modern Claude Code (`claude`) prints this at session
            // end. The `$` is optional because some versions omit it.
            CostPattern::new("Total cost (USD):", ""),
            CostPattern::new("Total cost:", ""),
            // Older builds.
            CostPattern::new("Session cost:", ""),
            // JSON output mode (`--output-format json`) — the field
            // appears in a single line; the matcher will stop at the
            // first non-numeric char so trailing commas are fine.
            CostPattern::new("\"total_cost_usd\":", ""),
            CostPattern::new("\"cost_usd\":", ""),
        ],
        "aider" => vec![
            // aider prints "Tokens: X sent, Y received. Cost: $0.012
            // message, $0.034 session." — the session total is the
            // *second* dollar figure, anchored by the word "session"
            // that follows it. The suffix anchor distinguishes it
            // from the per-message cost.
            CostPattern::new("$", "session"),
        ],
        _ => Vec::new(),
    }
}

/// One pattern: the literal prefix that must precede the cost figure
/// and an optional suffix that must follow. Empty suffix = "no anchor
/// — accept any decimal after the prefix." Non-empty suffix = "only
/// accept the match if the suffix appears after the decimal (with at
/// most a short whitespace gap)."
///
/// The suffix is how we disambiguate when an agent prints multiple
/// dollar figures on one line. Aider's "Cost: $X message, $Y session."
/// is the canonical case: the prefix `"$"` matches both, but the
/// suffix `"session"` only matches the second.
#[derive(Debug, Clone)]
struct CostPattern {
    prefix: &'static str,
    suffix: &'static str,
}

impl CostPattern {
    const fn new(prefix: &'static str, suffix: &'static str) -> Self {
        Self { prefix, suffix }
    }
}

/// Maximum characters of whitespace tolerated between the parsed
/// decimal and the suffix anchor. Small because real anchors sit
/// adjacent ("0.034 session"); a generous window would let unrelated
/// text slip in.
const SUFFIX_GAP_BUDGET: usize = 4;

/// Walk every stream looking for any pattern; return the rightmost
/// successfully-parsed figure. "Rightmost" = the one whose match
/// position is largest, breaking ties by the order in `streams`.
///
/// A match is accepted iff the prefix is followed by a decimal *and*
/// (when the pattern carries a suffix) the suffix appears within
/// [`SUFFIX_GAP_BUDGET`] characters after the decimal. This is how
/// aider's per-message cost is rejected in favour of its session
/// total: same prefix `$`, only the suffix `session` matches the one
/// we want.
fn last_match_across(streams: &[&str], patterns: &[CostPattern]) -> Option<f64> {
    let mut best: Option<(usize, f64)> = None; // (absolute position, value)
    let mut offset = 0usize;
    for stream in streams {
        for pattern in patterns {
            // Multiple occurrences within a single stream: take the
            // last that passes the suffix check.
            let mut start = 0usize;
            while let Some(idx) = stream[start..].find(pattern.prefix) {
                let pos = start + idx + pattern.prefix.len();
                if let Some((value, consumed)) = read_decimal_with_len(&stream[pos..]) {
                    if suffix_anchors(&stream[pos + consumed..], pattern.suffix) {
                        let abs = offset + pos;
                        if best.is_none_or(|(pos_prev, _)| abs >= pos_prev) {
                            best = Some((abs, value));
                        }
                    }
                }
                start = start + idx + pattern.prefix.len();
            }
        }
        offset += stream.len();
    }
    best.map(|(_, v)| v)
}

/// True iff `suffix` is empty (no anchor required), or appears within
/// the small whitespace budget after the decimal we just consumed.
fn suffix_anchors(after_decimal: &str, suffix: &str) -> bool {
    if suffix.is_empty() {
        return true;
    }
    let bytes = after_decimal.as_bytes();
    let mut i = 0;
    while i < bytes.len() && i < SUFFIX_GAP_BUDGET && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    after_decimal[i..].starts_with(suffix)
}

/// Skip leading whitespace + an optional `$`, then read a decimal.
/// Returns the parsed value plus the number of bytes consumed from the
/// start of `s` (so the caller can resume scanning after the figure
/// to look for a suffix anchor). `None` when no decimal follows the
/// optional prefix. Tolerates either integer (`12`) or fractional
/// (`0.12` / `.12`) shapes.
fn read_decimal_with_len(s: &str) -> Option<(f64, usize)> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b'$' {
        i += 1;
    }
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    let start = i;
    let mut saw_digit = false;
    let mut saw_dot = false;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_digit() {
            saw_digit = true;
            i += 1;
        } else if b == b'.' && !saw_dot {
            saw_dot = true;
            i += 1;
        } else {
            break;
        }
    }
    if !saw_digit {
        return None;
    }
    let value = s[start..i].parse::<f64>().ok()?;
    Some((value, i))
}

/// Thin wrapper preserving the original `read_decimal` shape for test
/// callers that don't need the byte count.
#[cfg(test)]
fn read_decimal(s: &str) -> Option<f64> {
    read_decimal_with_len(s).map(|(v, _)| v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_none_for_unknown_agent() {
        assert_eq!(parse_agent_cost_usd("nonesuch", "Total cost: $0.42", ""), None);
    }

    #[test]
    fn returns_none_when_no_pattern_matches_in_output() {
        assert_eq!(
            parse_agent_cost_usd("claude-code", "everything went fine\n", ""),
            None
        );
    }

    #[test]
    fn claude_code_modern_total_cost_usd() {
        let stdout = "\
working...
done
Total cost (USD): $0.1234
Total duration (API): 2m
";
        assert_eq!(
            parse_agent_cost_usd("claude-code", stdout, ""),
            Some(0.1234)
        );
    }

    #[test]
    fn claude_code_no_dollar_sign_still_parses() {
        assert_eq!(
            parse_agent_cost_usd("claude-code", "Total cost (USD): 0.42\n", ""),
            Some(0.42)
        );
    }

    #[test]
    fn claude_code_falls_back_to_total_cost_when_usd_variant_absent() {
        let stdout = "Total cost: $1.50\n";
        assert_eq!(
            parse_agent_cost_usd("claude-code", stdout, ""),
            Some(1.50)
        );
    }

    #[test]
    fn claude_code_session_cost_legacy_format() {
        assert_eq!(
            parse_agent_cost_usd("claude-code", "Session cost: $0.05\n", ""),
            Some(0.05)
        );
    }

    #[test]
    fn claude_code_json_output_mode() {
        let stdout = r#"{"result":"ok","total_cost_usd":0.0833,"duration_ms":12345}"#;
        assert_eq!(
            parse_agent_cost_usd("claude-code", stdout, ""),
            Some(0.0833)
        );
    }

    #[test]
    fn claude_code_picks_last_match_when_multiple_present() {
        // An interactive session can print incremental cost updates as
        // it runs. The final summary line is what we want.
        let stdout = "\
Cost so far: ignored — wrong prefix
Total cost: $0.05
... continuing ...
Total cost: $0.12
";
        assert_eq!(
            parse_agent_cost_usd("claude-code", stdout, ""),
            Some(0.12)
        );
    }

    #[test]
    fn claude_code_stderr_overrides_earlier_stdout() {
        // Some builds print the summary to stderr.
        let stdout = "Total cost: $0.01\n";
        let stderr = "Total cost: $0.99\n";
        assert_eq!(
            parse_agent_cost_usd("claude-code", stdout, stderr),
            Some(0.99)
        );
    }

    #[test]
    fn aider_session_cost() {
        let stdout = "Tokens: 1024 sent, 256 received. Cost: $0.012 message, $0.034 session.\n";
        assert_eq!(parse_agent_cost_usd("aider", stdout, ""), Some(0.034));
    }

    #[test]
    fn aider_no_match_when_only_message_cost_printed() {
        // A message-cost-only line shouldn't be picked up — the
        // session pattern requires "session." to appear before "Cost:".
        let stdout = "Cost: $0.012 message\n";
        assert_eq!(parse_agent_cost_usd("aider", stdout, ""), None);
    }

    #[test]
    fn parse_returns_zero_when_agent_reports_zero() {
        // Distinguish "matched, value is zero" (Some(0.0)) from "no
        // match" (None). Important UX signal.
        assert_eq!(
            parse_agent_cost_usd("claude-code", "Total cost: $0.00\n", ""),
            Some(0.0)
        );
    }

    #[test]
    fn read_decimal_handles_dollar_prefix() {
        assert_eq!(read_decimal(" $0.42 rest"), Some(0.42));
        assert_eq!(read_decimal("$0.42"), Some(0.42));
        assert_eq!(read_decimal("0.42"), Some(0.42));
    }

    #[test]
    fn read_decimal_handles_integer() {
        assert_eq!(read_decimal(" 12 cents"), Some(12.0));
    }

    #[test]
    fn read_decimal_handles_leading_dot() {
        // `.5` is a valid f64 literal; tolerate it.
        assert_eq!(read_decimal(".5"), Some(0.5));
    }

    #[test]
    fn read_decimal_returns_none_for_non_numeric_input() {
        assert_eq!(read_decimal("hello"), None);
        assert_eq!(read_decimal(""), None);
        assert_eq!(read_decimal("$"), None);
    }

    #[test]
    fn read_decimal_stops_at_second_dot() {
        // IP addresses or version strings shouldn't be parsed as a
        // single decimal — stop at the second dot.
        assert_eq!(read_decimal("1.2.3"), Some(1.2));
    }

}
