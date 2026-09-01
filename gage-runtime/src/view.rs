//! Prompt-ready session views: `compress` renders the whole session
//! as a compressed session --- a text rendering that fits a token
//! budget. It is a prompt-construction helper for scanners that
//! deliver session content in the agent's initial prompt instead of
//! having the agent page it through tool results.
//!
//! `Session::compress(budget, rules)` takes a token budget (or
//! `"unlimited"`) and a map from selector string to attributes.
//! Attributes give per-message `min` and optional `max` chars, a
//! `weight` (default 1) that governs slack-budget share when the
//! budget is not exhausted by mins, and an optional `priority` list
//! naming the content parts to keep when a message is cut (`"start"`,
//! `"end"`, `"line1"`). A message whose type/subtype is not covered
//! by any rule is excluded from the output entirely. When more than
//! one rule matches, the most specific wins: a `type.subtype`
//! selector beats a bare-token selector, and the `[error]` qualifier
//! adds specificity within a matching pattern. See
//! `.local.design/roadmap-scan.md` for the algorithm.

use std::collections::BTreeMap;

use datafusion::arrow::array::{Array, Int64Array, StringArray};
use datafusion::common::ScalarValue;
use rune::runtime::{Protocol, Ref, Value};
use rune::{Any, ContextError, Module};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value as Json;

use crate::error::Error;
use crate::scan::Session;
use crate::state::current_scan_ctx;

/// Chars per token used to convert a token budget to a working char
/// count. From the token ratio experiments in the roadmap-scan design
/// doc: the dense end of realistic session content. May be exposed to
/// configuration later.
const CHARS_PER_TOKEN: f64 = 2.2;

pub(crate) fn register(m: &mut Module) -> Result<(), ContextError> {
    m.function_meta(compress)?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: SessionCompress| async move {
        do_session_compress(q).await
    })?;

    Ok(())
}

pub(crate) fn register_types(m: &mut Module) -> Result<(), ContextError> {
    m.ty::<SessionCompress>()?;
    Ok(())
}

#[derive(Any)]
#[rune(item = ::gage)]
pub struct SessionCompress {
    #[rune(skip)]
    session_id: String,
    #[rune(skip)]
    lines: Option<(u64, u64)>,
    #[rune(skip)]
    budget: Value,
    #[rune(skip)]
    rules: Value,
}

/// Render the session as a compressed session for an agent prompt:
/// every message under its `[L<line> …]` header with its text cut per
/// the matching rule, sized to fit the token budget. Truncation
/// markers carry the omitted char counts so the agent can judge what
/// to retrieve in full.
#[rune::function(instance)]
fn compress(session: Ref<Session>, budget: Value, rules: Value) -> SessionCompress {
    SessionCompress {
        session_id: session.id.clone(),
        lines: session.range.map(|r| (r.start, r.end)),
        budget,
        rules,
    }
}

async fn do_session_compress(q: SessionCompress) -> super::Result<String> {
    let budget = parse_budget(&q.budget)?;
    let rules = parse_rules(&q.rules)?;
    let rows = fetch_rows(&q.session_id, q.lines).await?;
    build_compressed(&q.session_id, &rows, budget, &rules)
}

enum Budget {
    /// Working char count converted from the token budget.
    Chars(usize),
    Unlimited,
}

fn parse_budget(v: &Value) -> super::Result<Budget> {
    let json = serde_json::to_value(v)
        .map_err(|e| Error::Args(format!("`budget` could not be read: {e}")))?;
    match json {
        Json::String(s) if s == "unlimited" => Ok(Budget::Unlimited),
        Json::Number(n) => {
            let tokens = n
                .as_f64()
                .filter(|t| t.is_finite() && *t >= 0.0)
                .ok_or_else(|| Error::Args(format!("invalid `budget` {n}")))?;
            Ok(Budget::Chars((tokens * CHARS_PER_TOKEN) as usize))
        }
        other => Err(Error::Args(format!(
            "`budget` must be a token count or \"unlimited\", got {other}"
        ))),
    }
}

/// One parsed rule: selector plus attributes.
struct Rule {
    selector: Selector,
    min: usize,
    max: Option<usize>,
    weight: u64,
    parts: Vec<Part>,
}

struct Selector {
    pattern: Pattern,
    /// Bracketed qualifier, resolved by peeking at the message `raw`.
    error_only: bool,
}

impl Selector {
    /// Higher wins when more than one rule matches a message.
    /// `type.subtype` (2) beats bare token (1), and the `[error]`
    /// qualifier adds one within the same pattern kind.
    fn specificity(&self) -> u32 {
        let base = match self.pattern {
            Pattern::TypeSubtype(_, _) => 2,
            Pattern::Token(_) => 1,
        };
        base * 2 + u32::from(self.error_only)
    }
}

enum Pattern {
    /// `type.subtype`
    TypeSubtype(String, String),
    /// Bare token — matches the message type or subtype.
    Token(String),
}

#[derive(Clone, Copy, PartialEq)]
enum Part {
    Start,
    End,
    Line1,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RuleAttrs {
    min: Option<u64>,
    max: Option<u64>,
    weight: Option<u64>,
    priority: Option<Vec<String>>,
}

fn parse_rules(v: &Value) -> super::Result<Vec<Rule>> {
    let entries: BTreeMap<String, RuleAttrs> = parse_json(v, "rules")?;
    entries
        .into_iter()
        .map(|(sel, attrs)| {
            let min = attrs.min.unwrap_or(0) as usize;
            let max = attrs.max.map(|m| m as usize);
            if let Some(m) = max
                && min > m
            {
                return Err(Error::Args(format!(
                    "rule `{sel}`: min ({min}) exceeds max ({m})"
                )));
            }
            Ok(Rule {
                selector: parse_selector(&sel)?,
                min,
                max,
                weight: attrs.weight.unwrap_or(1),
                parts: parse_parts(&sel, attrs.priority)?,
            })
        })
        .collect()
}

fn parse_selector(sel: &str) -> super::Result<Selector> {
    let (base, error_only) = match sel.strip_suffix("[error]") {
        Some(base) => (base, true),
        None => (sel, false),
    };
    if base.is_empty() || base.contains(['[', ']', '*']) {
        return Err(Error::Args(format!("invalid rule selector `{sel}`")));
    }
    let pattern = match base.split_once('.') {
        Some((ty, sub)) if !ty.is_empty() && !sub.is_empty() => {
            Pattern::TypeSubtype(ty.to_string(), sub.to_string())
        }
        Some(_) => return Err(Error::Args(format!("invalid rule selector `{sel}`"))),
        None => Pattern::Token(base.to_string()),
    };
    Ok(Selector {
        pattern,
        error_only,
    })
}

fn parse_parts(sel: &str, priority: Option<Vec<String>>) -> super::Result<Vec<Part>> {
    let Some(names) = priority else {
        return Ok(vec![Part::Start]);
    };
    if names.is_empty() {
        return Err(Error::Args(format!(
            "rule `{sel}`: `priority` must name at least one part"
        )));
    }
    names
        .iter()
        .map(|n| match n.as_str() {
            "start" => Ok(Part::Start),
            "end" => Ok(Part::End),
            "line1" => Ok(Part::Line1),
            other => Err(Error::Args(format!(
                "rule `{sel}`: unknown priority part `{other}`: \
                 expected `start`, `end`, or `line1`"
            ))),
        })
        .collect()
}

fn parse_json<T: DeserializeOwned>(v: &Value, what: &str) -> super::Result<T> {
    let json = serde_json::to_value(v)
        .map_err(|e| Error::Args(format!("`{what}` could not be read: {e}")))?;
    serde_json::from_value(json).map_err(|e| Error::Args(format!("invalid `{what}`: {e}")))
}

/// One message paired with its matched rule and current allocation.
struct Msg<'a> {
    row: &'a MessageRow,
    text: Vec<char>,
    /// Index into the rule list.
    rule: usize,
    /// Kept chars for the final render.
    alloc: usize,
    /// Set once base cost is finalized so `cost` calls don't repeat
    /// the header-only rendering.
    demoted: bool,
}

impl Msg<'_> {
    fn cost(&self, rule: &Rule, alloc: usize) -> usize {
        message_len(self.row, &self.text, alloc, &rule.parts)
    }

    fn max_alloc(&self, rule: &Rule) -> usize {
        rule.max.unwrap_or(usize::MAX).min(self.text.len())
    }

    fn min_alloc(&self, rule: &Rule) -> usize {
        rule.min.min(self.text.len())
    }
}

fn build_compressed(
    session_id: &str,
    rows: &[MessageRow],
    budget: Budget,
    rules: &[Rule],
) -> super::Result<String> {
    // Drop messages no rule matches; carry the matched rule index
    let mut msgs: Vec<Msg> = rows
        .iter()
        .filter_map(|row| {
            match_rule(rules, row).map(|rule| Msg {
                row,
                text: row.text.chars().collect(),
                rule,
                alloc: 0,
                demoted: false,
            })
        })
        .collect();

    match budget {
        Budget::Unlimited => {
            for m in &mut msgs {
                m.alloc = m.max_alloc(rule_of(rules, m.rule));
            }
        }
        Budget::Chars(budget) => fit_to_budget(session_id, &mut msgs, rules, budget)?,
    }

    let out: Vec<String> = msgs
        .iter()
        .map(|m| render_message(m.row, &m.text, m.alloc, &rule_of(rules, m.rule).parts))
        .collect();
    Ok(out.join("\n\n"))
}

/// Highest-specificity matching rule for a row, or `None` if no rule
/// matches.
fn match_rule(rules: &[Rule], row: &MessageRow) -> Option<usize> {
    rules
        .iter()
        .enumerate()
        .filter(|(_, r)| selector_matches(&r.selector, row))
        .max_by_key(|(_, r)| r.selector.specificity())
        .map(|(i, _)| i)
}

/// Set each message's allocation honoring the budget: every message
/// at its rule's `min`, messages demoted (lowest rule weight first)
/// if that alone exceeds the budget, then remaining budget
/// distributed among non-demoted messages in proportion to their
/// rule's `weight`, bounded by each rule's `max` and the message's
/// content length.
fn fit_to_budget(
    session_id: &str,
    msgs: &mut [Msg],
    rules: &[Rule],
    budget: usize,
) -> super::Result<()> {
    let sep_total = msgs.len().saturating_sub(1) * 2; // "\n\n" joins

    // Demote messages (lowest rule weight first; tie: later document
    // order) until the minimum representation fits.
    loop {
        for m in msgs.iter_mut() {
            m.alloc = if m.demoted {
                0
            } else {
                m.min_alloc(rule_of(rules, m.rule))
            };
        }
        let total = sep_total + msg_body_cost(msgs, rules);
        if total <= budget {
            break;
        }
        let victim = msgs
            .iter()
            .enumerate()
            .filter(|(_, m)| !m.demoted)
            .min_by_key(|(i, m)| (rule_of(rules, m.rule).weight, std::cmp::Reverse(*i)))
            .map(|(i, _)| i);
        match victim {
            Some(i) => msg_at_mut(msgs, i).demoted = true,
            None => {
                return Err(Error::Args(format!(
                    "session {session_id} exceeds the budget at {total} chars \
                     with every rule demoted to marker-only (budget {budget})"
                )));
            }
        }
    }

    // Weight-proportional expansion. In each round, distribute
    // remaining budget across active messages by weight, bounded by
    // each rule's max_alloc. Iterate until no active message can grow
    // or budget is exhausted.
    loop {
        let active: Vec<usize> = msgs
            .iter()
            .enumerate()
            .filter(|(_, m)| !m.demoted && m.alloc < m.max_alloc(rule_of(rules, m.rule)))
            .map(|(i, _)| i)
            .collect();
        if active.is_empty() {
            break;
        }
        let used = sep_total + msg_body_cost(msgs, rules);
        let remaining = budget.saturating_sub(used);
        if remaining == 0 {
            break;
        }
        let total_weight: u64 = active
            .iter()
            .map(|&i| rule_of(rules, msg_at(msgs, i).rule).weight)
            .sum();
        if total_weight == 0 {
            break;
        }

        let mut any_progress = false;
        for &i in &active {
            let (rule, m_alloc, ceiling) = {
                let m = msg_at(msgs, i);
                let rule = rule_of(rules, m.rule);
                (rule, m.alloc, m.max_alloc(rule))
            };
            let cur_used = sep_total + msg_body_cost(msgs, rules);
            let budget_left = budget.saturating_sub(cur_used);
            if budget_left == 0 {
                return Ok(());
            }
            let share = ((remaining as u64 * rule.weight) / total_weight).max(1) as usize;
            let target = m_alloc.saturating_add(share).min(ceiling);
            if target <= m_alloc {
                continue;
            }
            let m = msg_at(msgs, i);
            // Cost can drop as alloc grows because the truncation
            // marker ("… [+N chars]") shrinks and vanishes when alloc
            // reaches text length. A non-positive delta means the
            // expansion is free (or shortens the output), so it is
            // always safe to take.
            let cost_at = |a| m.cost(rule, a);
            let delta = cost_at(target).saturating_sub(cost_at(m_alloc));
            if delta <= budget_left {
                msg_at_mut(msgs, i).alloc = target;
                any_progress = true;
                continue;
            }
            // Partial expansion: back off until it fits, then stop.
            let mut t = target;
            while t > m_alloc && cost_at(t).saturating_sub(cost_at(m_alloc)) > budget_left {
                t -= 1;
            }
            msg_at_mut(msgs, i).alloc = t;
            return Ok(());
        }
        if !any_progress {
            break;
        }
    }

    Ok(())
}

fn rule_of(rules: &[Rule], idx: usize) -> &Rule {
    rules.get(idx).expect("msg rule index is in range")
}

fn msg_at<'a, 'b>(msgs: &'a [Msg<'b>], idx: usize) -> &'a Msg<'b> {
    msgs.get(idx).expect("active msg index is in range")
}

fn msg_at_mut<'a, 'b>(msgs: &'a mut [Msg<'b>], idx: usize) -> &'a mut Msg<'b> {
    msgs.get_mut(idx).expect("active msg index is in range")
}

fn msg_body_cost(msgs: &[Msg], rules: &[Rule]) -> usize {
    msgs.iter()
        .map(|m| m.cost(rule_of(rules, m.rule), m.alloc))
        .sum()
}

fn selector_matches(sel: &Selector, row: &MessageRow) -> bool {
    let matched = match &sel.pattern {
        Pattern::TypeSubtype(ty, sub) => {
            row.type_ == *ty && row.subtype.as_deref() == Some(sub.as_str())
        }
        Pattern::Token(tok) => row.type_ == *tok || row.subtype.as_deref() == Some(tok.as_str()),
    };
    matched && (!sel.error_only || is_error_result(row))
}

/// Qualifier peek into `raw`: any tool_result block with
/// `is_error: true`.
fn is_error_result(row: &MessageRow) -> bool {
    let Ok(raw) = serde_json::from_str::<Json>(&row.raw) else {
        return false;
    };
    let Some(Json::Array(blocks)) = raw.get("message").and_then(|m| m.get("content")) else {
        return false;
    };
    blocks.iter().any(|b| {
        b.get("type").and_then(Json::as_str) == Some("tool_result")
            && b.get("is_error").and_then(Json::as_bool) == Some(true)
    })
}

fn render_message(row: &MessageRow, text: &[char], alloc: usize, parts: &[Part]) -> String {
    let mut s = header(row);
    let body = render_body(text, alloc, parts);
    if !body.is_empty() {
        s.push('\n');
        s.push_str(&body);
    }
    s
}

/// Rendered char length of a message at the given allocation. Kept in
/// step with `render_message` (measured, not estimated).
fn message_len(row: &MessageRow, text: &[char], alloc: usize, parts: &[Part]) -> usize {
    render_message(row, text, alloc, parts).chars().count()
}

fn header(row: &MessageRow) -> String {
    match &row.subtype {
        Some(sub) if *sub != row.type_ => format!("[L{} {} {}]", row.line, row.type_, sub),
        _ => format!("[L{} {}]", row.line, row.type_),
    }
}

/// Cut the text to `alloc` kept chars, spending them on the listed
/// parts in order, and mark every cut with its omitted char count.
fn render_body(text: &[char], alloc: usize, parts: &[Part]) -> String {
    if alloc == 0 || text.is_empty() {
        return String::new();
    }
    let spans = keep_spans(text, alloc, parts);
    let mut out = String::new();
    let mut pos = 0;
    for (a, b) in spans {
        if a > pos {
            out.push_str(&format!("… [{} chars omitted] …", a - pos));
        }
        out.extend(text.iter().skip(a).take(b - a));
        pos = b;
    }
    if pos < text.len() {
        out.push_str(&format!("… [+{} chars]", text.len() - pos));
    }
    out
}

/// The kept spans of `text`, in position order. `alloc` chars are
/// divided across the parts in listed order; a part with intrinsic
/// size (`line1`) takes only what it needs and the remainder flows to
/// the other listed parts.
fn keep_spans(text: &[char], alloc: usize, parts: &[Part]) -> Vec<(usize, usize)> {
    let n = text.len();
    let k = parts.len();
    // Even division, remainder to the earlier parts
    let mut shares: Vec<usize> = (0..k)
        .map(|i| alloc / k + usize::from(i < alloc % k))
        .collect();
    // Sized parts release what they don't need to the other parts,
    // in listed order
    let line1_len = text.iter().position(|c| *c == '\n').unwrap_or(n);
    let mut leftover = 0;
    for (share, part) in shares.iter_mut().zip(parts) {
        if *part == Part::Line1 && *share > line1_len {
            leftover += *share - line1_len;
            *share = line1_len;
        }
    }
    for (share, part) in shares.iter_mut().zip(parts) {
        if leftover == 0 {
            break;
        }
        if *part != Part::Line1 {
            *share += leftover;
            leftover = 0;
        }
    }

    let mut spans: Vec<(usize, usize)> = Vec::with_capacity(k);
    for (part, share) in parts.iter().zip(&shares) {
        if *share == 0 {
            continue;
        }
        let span = match part {
            Part::Start => (0, (*share).min(n)),
            Part::End => (n - (*share).min(n), n),
            Part::Line1 => (0, (*share).min(line1_len)),
        };
        spans.push(span);
    }
    spans.sort_unstable();
    // Merge overlaps so each kept char renders once
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
    for (a, b) in spans {
        match merged.last_mut() {
            Some((_, prev_b)) if a <= *prev_b => *prev_b = (*prev_b).max(b),
            _ => merged.push((a, b)),
        }
    }
    merged
}

pub(crate) struct MessageRow {
    pub line: u64,
    pub type_: String,
    pub subtype: Option<String>,
    pub text: String,
    pub raw: String,
}

async fn fetch_rows(session_id: &str, lines: Option<(u64, u64)>) -> super::Result<Vec<MessageRow>> {
    let ctx = current_scan_ctx();
    let df_ctx = &ctx.run.scan_ctx;

    let mut params: Vec<ScalarValue> = vec![ScalarValue::Utf8(Some(session_id.to_string()))];
    let mut clauses = vec!["session_id = $1".to_string()];
    if let Some((start, end)) = lines {
        params.push(ScalarValue::Int64(Some(start as i64)));
        clauses.push(format!("line >= ${}", params.len()));
        params.push(ScalarValue::Int64(Some(end as i64)));
        clauses.push(format!("line <= ${}", params.len()));
    }
    let sql = format!(
        "SELECT * FROM message WHERE {} ORDER BY line",
        clauses.join(" AND ")
    );

    let df = df_ctx
        .sql(&sql)
        .await
        .map_err(|e| Error::Db(e.to_string()))?;
    let df = df
        .with_param_values(params)
        .map_err(|e| Error::Db(e.to_string()))?;
    let batches = df.collect().await.map_err(|e| Error::Db(e.to_string()))?;

    let mut rows = Vec::new();
    for batch in &batches {
        if batch.num_rows() == 0 {
            continue;
        }
        let schema = batch.schema();
        let col = |name: &str| batch.column(schema.index_of(name).unwrap());
        let line_arr = col("line").as_any().downcast_ref::<Int64Array>().unwrap();
        let type_arr = col("type").as_any().downcast_ref::<StringArray>().unwrap();
        let subtype_arr = col("subtype")
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let text_arr = col("text").as_any().downcast_ref::<StringArray>().unwrap();
        let raw_arr = col("raw").as_any().downcast_ref::<StringArray>().unwrap();
        for i in 0..batch.num_rows() {
            rows.push(MessageRow {
                line: line_arr.value(i) as u64,
                type_: type_arr.value(i).to_string(),
                subtype: opt_str(subtype_arr, i),
                text: text_arr.value(i).to_string(),
                raw: raw_arr.value(i).to_string(),
            });
        }
    }
    Ok(rows)
}

fn opt_str(arr: &StringArray, i: usize) -> Option<String> {
    if arr.is_null(i) {
        None
    } else {
        Some(arr.value(i).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(line: u64, type_: &str, subtype: Option<&str>, text: &str) -> MessageRow {
        MessageRow {
            line,
            type_: type_.to_string(),
            subtype: subtype.map(str::to_string),
            text: text.to_string(),
            raw: "{}".to_string(),
        }
    }

    fn rule(selector: &str, min: usize, max: Option<usize>, weight: u64, parts: Vec<Part>) -> Rule {
        Rule {
            selector: parse_selector(selector).unwrap(),
            min,
            max,
            weight,
            parts,
        }
    }

    fn compress(rows: &[MessageRow], budget: Budget, rules: &[Rule]) -> String {
        build_compressed("test-session", rows, budget, rules).unwrap()
    }

    /// Allocations from `fit_to_budget` for a chars budget. Rows a
    /// rule does not match are excluded, so the returned vector's
    /// length matches the number of matched rows.
    fn fit(rows: &[MessageRow], rules: &[Rule], budget: usize) -> Vec<usize> {
        let mut msgs: Vec<Msg> = rows
            .iter()
            .filter_map(|row| {
                match_rule(rules, row).map(|rule_idx| Msg {
                    row,
                    text: row.text.chars().collect(),
                    rule: rule_idx,
                    alloc: 0,
                    demoted: false,
                })
            })
            .collect();
        fit_to_budget("s", &mut msgs, rules, budget).unwrap();
        msgs.iter().map(|m| m.alloc).collect()
    }

    #[test]
    fn cuts_text_at_max_keeping_start() {
        let long = "x".repeat(600);
        let rows = [row(1, "user", Some("text"), &long)];
        let rules = [rule("user.text", 0, Some(500), 1, vec![Part::Start])];
        let out = compress(&rows, Budget::Unlimited, &rules);
        assert!(out.starts_with("[L1 user text]\n"));
        assert!(out.contains(&"x".repeat(500)));
        assert!(out.ends_with("… [+100 chars]"));
        assert!(!out.contains(&"x".repeat(501)));
    }

    #[test]
    fn priority_start_end_keeps_both_ends() {
        let text = format!("HEAD{}TAIL", "m".repeat(1000));
        let rows = [row(1, "assistant", Some("thinking"), &text)];
        let rules = [rule(
            "assistant.thinking",
            0,
            Some(500),
            1,
            vec![Part::Start, Part::End],
        )];
        let out = compress(&rows, Budget::Unlimited, &rules);
        assert!(out.contains("HEAD"));
        assert!(out.contains("… [508 chars omitted] …"));
        assert!(out.ends_with("TAIL"));
    }

    #[test]
    fn priority_line1_keeps_first_line_only() {
        let rows = [row(
            1,
            "user",
            Some("tool_result"),
            "line one\nline two\nrest",
        )];
        let rules = [rule("tool_result", 0, Some(200), 1, vec![Part::Line1])];
        let out = compress(&rows, Budget::Unlimited, &rules);
        assert!(out.contains("line one… [+14 chars]"));
        assert!(!out.contains("line two"));
    }

    #[test]
    fn most_specific_rule_wins() {
        let rows = [row(1, "user", Some("text"), &"a".repeat(100))];
        // Both rules match: bare token `text` matches by subtype and
        // `user.text` matches by type+subtype; the specific one wins
        let rules = [
            rule("user.text", 0, Some(10), 1, vec![Part::Start]),
            rule("text", 0, Some(90), 1, vec![Part::Start]),
        ];
        let out = compress(&rows, Budget::Unlimited, &rules);
        assert!(out.contains("… [+90 chars]"), "{out}");
    }

    #[test]
    fn unmatched_message_is_excluded() {
        let rows = [
            row(1, "system", Some("local_command"), "sys content"),
            row(2, "user", Some("text"), "kept content"),
        ];
        let rules = [rule("user.text", 0, None, 1, vec![Part::Start])];
        let out = compress(&rows, Budget::Unlimited, &rules);
        assert_eq!(out, "[L2 user text]\nkept content");
    }

    #[test]
    fn bare_token_selector_matches_subtype() {
        let rows = [
            row(1, "assistant", Some("tool_use"), &"u".repeat(100)),
            row(2, "user", Some("tool_result"), &"r".repeat(100)),
        ];
        let rules = [
            rule("tool_use", 0, Some(10), 1, vec![Part::Start]),
            rule("tool_result", 0, Some(20), 1, vec![Part::Start]),
        ];
        let out = compress(&rows, Budget::Unlimited, &rules);
        assert!(out.contains("… [+90 chars]"));
        assert!(out.contains("… [+80 chars]"));
    }

    #[test]
    fn error_qualifier_peeks_raw() {
        let mk = |line, is_error: bool| {
            let raw = serde_json::json!({
                "message": { "content": [{
                    "type": "tool_result",
                    "is_error": is_error,
                    "content": "boom",
                }] },
            });
            MessageRow {
                line,
                type_: "user".to_string(),
                subtype: Some("tool_result".to_string()),
                text: "e".repeat(100),
                raw: raw.to_string(),
            }
        };
        let rows = [mk(1, true), mk(2, false)];
        let rules = [
            rule("tool_result[error]", 0, Some(50), 1, vec![Part::Start]),
            rule("tool_result", 0, Some(10), 1, vec![Part::Start]),
        ];
        let out = compress(&rows, Budget::Unlimited, &rules);
        assert!(out.contains("… [+50 chars]"), "{out}"); // error row at 50
        assert!(out.contains("… [+90 chars]"), "{out}"); // plain row at 10
    }

    #[test]
    fn budget_partial_expansion_stays_within_budget() {
        let rows = [row(1, "user", Some("text"), &"a".repeat(1000))];
        let rules = [rule("user.text", 0, None, 1, vec![Part::Start])];
        let budget_chars = 200;
        let allocs = fit(&rows, &rules, budget_chars);
        let out = render_message(
            &rows[0],
            &rows[0].text.chars().collect::<Vec<_>>(),
            allocs.first().copied().unwrap(),
            &rules[0].parts,
        );
        assert!(out.chars().count() <= budget_chars);
        assert!(out.contains("… [+"));
    }

    #[test]
    fn min_reserves_content_before_expansion() {
        let rows = [
            row(1, "user", Some("text"), &"a".repeat(500)),
            row(2, "assistant", Some("text"), &"b".repeat(500)),
        ];
        let rules = [
            rule("user.text", 0, None, 10, vec![Part::Start]),
            rule("assistant.text", 50, Some(60), 1, vec![Part::Start]),
        ];
        // The high-weight rule takes most of the slack, but the
        // low-weight rule's `min` of 50 must still be honored
        let allocs = fit(&rows, &rules, 200);
        assert!(
            allocs.get(1).copied().unwrap() >= 50,
            "assistant min not reserved: {allocs:?}"
        );
    }

    #[test]
    fn slack_splits_by_weight() {
        // Two messages of equal content length, differing only in
        // rule weight. Under a budget that can not fit both fully,
        // the higher-weighted message gets the larger share.
        let rows = [
            row(1, "user", Some("text"), &"a".repeat(1000)),
            row(2, "assistant", Some("text"), &"b".repeat(1000)),
        ];
        let rules = [
            rule("user.text", 0, None, 3, vec![Part::Start]),
            rule("assistant.text", 0, None, 1, vec![Part::Start]),
        ];
        let allocs = fit(&rows, &rules, 400);
        let (u, a) = (allocs[0], allocs[1]);
        assert!(
            u > a,
            "weight 3 should outpace weight 1: user={u}, assistant={a}"
        );
        assert!(u as f64 / (u + a).max(1) as f64 > 0.6, "user share too low");
    }

    #[test]
    fn demotion_drops_lowest_weight_first() {
        let rows = [
            row(1, "user", Some("text"), &"a".repeat(100)),
            row(2, "assistant", Some("text"), &"b".repeat(100)),
        ];
        let rules = [
            rule("user.text", 40, Some(40), 1, vec![Part::Start]),
            rule("assistant.text", 40, Some(40), 5, vec![Part::Start]),
        ];
        // Both mins together push past the budget; the lower-weight
        // (user.text, weight 1) demotes first
        let allocs = fit(&rows, &rules, 100);
        assert_eq!(
            allocs[0], 0,
            "lowest-weight rule should demote first: {allocs:?}"
        );
        assert_eq!(allocs[1], 40, "highest-weight min must survive: {allocs:?}");
    }

    #[test]
    fn expansion_across_text_end_does_not_underflow() {
        // Growing alloc past text.len() removes the "… [+N chars]"
        // marker, so cost drops as alloc rises through that edge.
        // The delta computation must not underflow.
        let rows = [row(
            1,
            "assistant",
            Some("text"),
            &"a".repeat(505), // just over min
        )];
        let rules = [rule("assistant.text", 500, None, 1, vec![Part::Start])];
        // Budget lets alloc grow from min=500 to text.len()=505;
        // the marker "… [+5 chars]" disappears in that step
        let out = compress(&rows, Budget::Chars(1000), &rules);
        assert!(out.contains(&"a".repeat(505)));
        assert!(!out.contains("chars]"));
    }

    #[test]
    fn over_budget_with_all_demoted_errors() {
        let rows: Vec<MessageRow> = (1..=10)
            .map(|i| row(i, "user", Some("text"), "content"))
            .collect();
        let rules = [rule("user.text", 0, None, 1, vec![Part::Start])];
        let err = build_compressed("big-session", &rows, Budget::Chars(10), &rules).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("big-session"), "{msg}");
        assert!(msg.contains("marker-only"), "{msg}");
    }

    #[test]
    fn budget_token_to_char_conversion() {
        let b = parse_budget(&rune::to_value(100_i64).unwrap()).unwrap();
        match b {
            Budget::Chars(c) => assert_eq!(c, 220),
            Budget::Unlimited => panic!("expected chars"),
        }
        let b = parse_budget(&rune::to_value(100.5_f64).unwrap()).unwrap();
        match b {
            Budget::Chars(c) => assert_eq!(c, 221),
            Budget::Unlimited => panic!("expected chars"),
        }
        let unlimited = rune::to_value("unlimited").unwrap();
        assert!(matches!(
            parse_budget(&unlimited).unwrap(),
            Budget::Unlimited
        ));
        assert!(parse_budget(&rune::to_value(-1_i64).unwrap()).is_err());
    }

    #[test]
    fn rules_reject_unknown_attributes() {
        let rules = serde_json::json!({ "user.text": { "mim": 10 } });
        let v = crate::value::json_to_value(&rules);
        assert!(parse_rules(&v).is_err());
    }

    #[test]
    fn rules_reject_bad_priority_part() {
        let rules = serde_json::json!({ "user.text": { "priority": ["mid"] } });
        let v = crate::value::json_to_value(&rules);
        assert!(parse_rules(&v).is_err());
    }

    #[test]
    fn rules_reject_wildcard_selector() {
        let rules = serde_json::json!({ "*": { "min": 10 } });
        let v = crate::value::json_to_value(&rules);
        assert!(parse_rules(&v).is_err());
    }

    #[test]
    fn rules_reject_min_over_max() {
        let rules = serde_json::json!({ "user.text": { "min": 100, "max": 50 } });
        let v = crate::value::json_to_value(&rules);
        assert!(parse_rules(&v).is_err());
    }

    #[test]
    fn render_body_char_boundary_safe() {
        let text: Vec<char> = "日本語のテキスト".chars().collect();
        let out = render_body(&text, 3, &[Part::Start]);
        assert_eq!(out, "日本語… [+5 chars]");
    }

    #[test]
    fn compress_builder_leaves_caller_values_readable() {
        use crate::datetime::DateTime;
        use crate::scan::Range;
        use std::path::PathBuf;

        let session = Session {
            id: "11111111-1111-1111-1111-111111111111".to_string(),
            modified: DateTime::from_millis(0),
            src: PathBuf::from("/tmp/session.jsonl"),
            range: Some(Range { start: 3, end: 9 }),
        };
        let sv = rune::to_value(session).unwrap();
        let budget = rune::to_value(1000_i64).unwrap();
        let rules_json = serde_json::json!({ "user.text": { "max": 500 } });
        let rules = crate::value::json_to_value(&rules_json);

        let q = SessionCompress {
            session_id: sv.borrow_ref::<Session>().unwrap().id.clone(),
            lines: Some((3, 9)),
            budget: budget.clone(),
            rules: rules.clone(),
        };
        assert!(matches!(
            parse_budget(&q.budget).unwrap(),
            Budget::Chars(2200)
        ));
        assert_eq!(parse_rules(&q.rules).unwrap().len(), 1);

        // The caller's values are reads, not takes: all stay readable
        let s = sv.borrow_ref::<Session>().unwrap();
        assert_eq!(s.id, "11111111-1111-1111-1111-111111111111");
        assert!(matches!(parse_budget(&budget).unwrap(), Budget::Chars(_)));
        assert_eq!(parse_rules(&rules).unwrap().len(), 1);
    }
}
