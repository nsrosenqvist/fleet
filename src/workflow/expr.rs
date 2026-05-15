//! Tiny expression engine for workflow `when:` predicates.
//!
//! Grammar (v1):
//! ```text
//! expr     := or
//! or       := and ('||' and)*
//! and      := atom ('&&' atom)*
//! atom     := '(' or ')' | comparison
//! compare  := path ('==' | '!=') string
//! path     := IDENT '.' IDENT
//! string   := '"…"' | "'…'"      (no escape sequences in v1)
//! IDENT    := [A-Za-z_][A-Za-z0-9_]*
//! ```
//!
//! Outputs are addressed as `<node>.<output_name>`. The left-hand side
//! must resolve against the supplied [`OutputMap`]; an unresolved path
//! is a runtime error rather than a silent false, so a typo in a
//! workflow YAML surfaces loudly instead of skipping every branch.
//!
//! Scope deliberately small: numbers, `<` / `>`, function calls, and
//! escape sequences in string literals are out of scope. They land when
//! a workflow asks for them.
//!
//! The same evaluator backs the `assert` node kind once that lands —
//! `assert` runs the same predicate and fails the workflow if false.

use anyhow::{Result, anyhow, bail};
use std::collections::HashMap;

/// Resolved outputs from upstream workflow nodes. Keyed by
/// `(node_id, output_name)` so two nodes can declare the same local
/// output name without collision.
pub type OutputMap = HashMap<(String, String), String>;

/// Evaluate a `when:` predicate against the resolved upstream outputs.
/// Returns `Ok(bool)` for parse-clean expressions; structural parse
/// errors and unresolved `<node>.<name>` references return `Err` so a
/// broken predicate is loud rather than silently false-y.
pub fn evaluate(expr: &str, outputs: &OutputMap) -> Result<bool> {
    let tokens = tokenize(expr)?;
    let mut p = Parser { tokens, pos: 0 };
    let v = p.parse_or(outputs)?;
    if p.pos < p.tokens.len() {
        bail!(
            "trailing tokens after expression at index {} (saw {:?})",
            p.pos,
            p.tokens[p.pos]
        );
    }
    Ok(v)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    LParen,
    RParen,
    AndAnd,
    OrOr,
    EqEq,
    BangEq,
    Dot,
    Ident(String),
    Str(String),
}

fn tokenize(s: &str) -> Result<Vec<Tok>> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            b'(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            b')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            b'.' => {
                out.push(Tok::Dot);
                i += 1;
            }
            b'&' => {
                if bytes.get(i + 1) == Some(&b'&') {
                    out.push(Tok::AndAnd);
                    i += 2;
                } else {
                    bail!("expected `&&` at position {i} (single `&` not allowed)");
                }
            }
            b'|' => {
                if bytes.get(i + 1) == Some(&b'|') {
                    out.push(Tok::OrOr);
                    i += 2;
                } else {
                    bail!("expected `||` at position {i} (single `|` not allowed)");
                }
            }
            b'=' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    out.push(Tok::EqEq);
                    i += 2;
                } else {
                    bail!("expected `==` at position {i} (single `=` not allowed)");
                }
            }
            b'!' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    out.push(Tok::BangEq);
                    i += 2;
                } else {
                    bail!("expected `!=` at position {i} (bare `!` not supported in v1)");
                }
            }
            b'"' | b'\'' => {
                let quote = c;
                let start = i + 1;
                let mut j = start;
                while j < bytes.len() && bytes[j] != quote {
                    j += 1;
                }
                if j >= bytes.len() {
                    bail!("unterminated string literal starting at position {i}");
                }
                let inner = std::str::from_utf8(&bytes[start..j])
                    .map_err(|e| anyhow!("invalid utf-8 in string literal at {i}: {e}"))?
                    .to_string();
                out.push(Tok::Str(inner));
                i = j + 1;
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let start = i;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_')
                {
                    i += 1;
                }
                // Slice is valid UTF-8: we only advanced over ASCII bytes.
                let ident = std::str::from_utf8(&bytes[start..i]).unwrap().to_string();
                out.push(Tok::Ident(ident));
            }
            other => bail!("unexpected character `{}` at position {i}", other as char),
        }
    }
    Ok(out)
}

struct Parser {
    tokens: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.pos)
    }

    fn bump(&mut self) -> Option<Tok> {
        let t = self.tokens.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn parse_or(&mut self, ctx: &OutputMap) -> Result<bool> {
        let mut left = self.parse_and(ctx)?;
        while matches!(self.peek(), Some(Tok::OrOr)) {
            self.bump();
            let right = self.parse_and(ctx)?;
            left = left || right;
        }
        Ok(left)
    }

    fn parse_and(&mut self, ctx: &OutputMap) -> Result<bool> {
        let mut left = self.parse_atom(ctx)?;
        while matches!(self.peek(), Some(Tok::AndAnd)) {
            self.bump();
            let right = self.parse_atom(ctx)?;
            left = left && right;
        }
        Ok(left)
    }

    fn parse_atom(&mut self, ctx: &OutputMap) -> Result<bool> {
        if matches!(self.peek(), Some(Tok::LParen)) {
            self.bump();
            let inner = self.parse_or(ctx)?;
            match self.bump() {
                Some(Tok::RParen) => Ok(inner),
                other => bail!("expected `)`, got {other:?}"),
            }
        } else {
            self.parse_comparison(ctx)
        }
    }

    fn parse_comparison(&mut self, ctx: &OutputMap) -> Result<bool> {
        let node = match self.bump() {
            Some(Tok::Ident(s)) => s,
            other => bail!("expected identifier (node id), got {other:?}"),
        };
        match self.bump() {
            Some(Tok::Dot) => {}
            other => bail!("expected `.` after node id `{node}`, got {other:?}"),
        }
        let name = match self.bump() {
            Some(Tok::Ident(s)) => s,
            other => bail!("expected identifier after `{node}.`, got {other:?}"),
        };
        let op = match self.bump() {
            Some(t @ (Tok::EqEq | Tok::BangEq)) => t,
            other => bail!(
                "expected `==` or `!=` after `{node}.{name}`, got {other:?}"
            ),
        };
        let rhs = match self.bump() {
            Some(Tok::Str(s)) => s,
            other => bail!("expected string literal on RHS of comparison, got {other:?}"),
        };
        let lhs = ctx.get(&(node.clone(), name.clone())).ok_or_else(|| {
            anyhow!(
                "expression references unknown output `{node}.{name}` \
                 (upstream node did not declare or produce this output)"
            )
        })?;
        let eq = lhs == &rhs;
        Ok(if op == Tok::EqEq { eq } else { !eq })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(pairs: &[(&str, &str, &str)]) -> OutputMap {
        pairs
            .iter()
            .map(|(n, k, v)| ((n.to_string(), k.to_string()), v.to_string()))
            .collect()
    }

    #[test]
    fn simple_equality_true() {
        let m = ctx(&[("review", "decision", "approve")]);
        assert!(evaluate(r#"review.decision == "approve""#, &m).unwrap());
    }

    #[test]
    fn simple_equality_false() {
        let m = ctx(&[("review", "decision", "approve")]);
        assert!(!evaluate(r#"review.decision == "changes_requested""#, &m).unwrap());
    }

    #[test]
    fn inequality_inverts_result() {
        let m = ctx(&[("review", "decision", "approve")]);
        assert!(evaluate(r#"review.decision != "changes_requested""#, &m).unwrap());
        assert!(!evaluate(r#"review.decision != "approve""#, &m).unwrap());
    }

    #[test]
    fn single_quoted_string_literal_accepted() {
        let m = ctx(&[("review", "decision", "approve")]);
        assert!(evaluate("review.decision == 'approve'", &m).unwrap());
    }

    #[test]
    fn and_short_circuits_correctly() {
        let m = ctx(&[
            ("review", "decision", "approve"),
            ("plan", "kind", "feature"),
        ]);
        assert!(evaluate(
            r#"review.decision == "approve" && plan.kind == "feature""#,
            &m
        )
        .unwrap());
        assert!(!evaluate(
            r#"review.decision == "approve" && plan.kind == "bug""#,
            &m
        )
        .unwrap());
    }

    #[test]
    fn or_short_circuits_correctly() {
        let m = ctx(&[("review", "decision", "approve")]);
        assert!(evaluate(
            r#"review.decision == "approve" || review.decision == "changes_requested""#,
            &m
        )
        .unwrap());
        assert!(!evaluate(
            r#"review.decision == "rejected" || review.decision == "deferred""#,
            &m
        )
        .unwrap());
    }

    #[test]
    fn parentheses_change_precedence() {
        // a && (b || c): with a=true, b=false, c=true → true
        let m = ctx(&[
            ("a", "v", "1"),
            ("b", "v", "0"),
            ("c", "v", "1"),
        ]);
        assert!(evaluate(
            r#"a.v == "1" && (b.v == "1" || c.v == "1")"#,
            &m
        )
        .unwrap());
        // Without parens: a && b || c → (a && b) || c → false || true → true
        // With different shape — a=true, b=false, c=false → false
        let m = ctx(&[
            ("a", "v", "1"),
            ("b", "v", "0"),
            ("c", "v", "0"),
        ]);
        assert!(!evaluate(
            r#"a.v == "1" && (b.v == "1" || c.v == "1")"#,
            &m
        )
        .unwrap());
    }

    #[test]
    fn and_binds_tighter_than_or() {
        // a || b && c: with a=false, b=true, c=false → false || (true && false) → false
        let m = ctx(&[
            ("a", "v", "0"),
            ("b", "v", "1"),
            ("c", "v", "0"),
        ]);
        assert!(!evaluate(
            r#"a.v == "1" || b.v == "1" && c.v == "1""#,
            &m
        )
        .unwrap());
    }

    #[test]
    fn unknown_output_errors_loudly() {
        let m = ctx(&[]);
        let err = evaluate(r#"review.decision == "approve""#, &m).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unknown output `review.decision`"),
            "got: {msg}"
        );
    }

    #[test]
    fn unterminated_string_literal_is_an_error() {
        let m = ctx(&[]);
        let err = evaluate(r#"x.y == "approve"#, &m).unwrap_err();
        assert!(format!("{err:#}").contains("unterminated string"));
    }

    #[test]
    fn missing_rhs_literal_is_an_error() {
        let m = ctx(&[("x", "y", "1")]);
        let err = evaluate("x.y ==", &m).unwrap_err();
        assert!(
            format!("{err:#}").contains("expected string literal on RHS"),
            "got: {err:#}"
        );
    }

    #[test]
    fn missing_dot_between_node_and_name_is_an_error() {
        let m = ctx(&[]);
        let err = evaluate(r#"x == "y""#, &m).unwrap_err();
        assert!(
            format!("{err:#}").contains("expected `.` after node id"),
            "got: {err:#}"
        );
    }

    #[test]
    fn single_ampersand_is_an_error() {
        let m = ctx(&[("a", "v", "1"), ("b", "v", "1")]);
        let err = evaluate(r#"a.v == "1" & b.v == "1""#, &m).unwrap_err();
        assert!(format!("{err:#}").contains("expected `&&`"));
    }

    #[test]
    fn unbalanced_parens_are_an_error() {
        let m = ctx(&[("a", "v", "1")]);
        let err = evaluate(r#"(a.v == "1""#, &m).unwrap_err();
        assert!(format!("{err:#}").contains("expected `)`"));
    }

    #[test]
    fn whitespace_around_operators_is_optional() {
        let m = ctx(&[("a", "v", "x")]);
        assert!(evaluate(r#"a.v=="x""#, &m).unwrap());
        assert!(evaluate(r#"a.v == "x""#, &m).unwrap());
        assert!(evaluate(r#"  a.v   ==   "x"  "#, &m).unwrap());
    }

    #[test]
    fn trailing_garbage_after_expression_is_an_error() {
        let m = ctx(&[("a", "v", "x")]);
        let err = evaluate(r#"a.v == "x" foo"#, &m).unwrap_err();
        assert!(format!("{err:#}").contains("trailing tokens"));
    }

    #[test]
    fn empty_string_value_compares_equal() {
        // A node that explicitly emitted an empty string should still
        // compare equal to the empty literal — the strict-error path is
        // for *missing* keys, not present-but-empty ones.
        let m = ctx(&[("a", "v", "")]);
        assert!(evaluate(r#"a.v == """#, &m).unwrap());
        assert!(!evaluate(r#"a.v == "y""#, &m).unwrap());
    }
}
