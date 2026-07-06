// Cypher subset parser (M3.c): a small hand-rolled recursive-descent
// parser, not an external crate. Checked before committing to this
// approach: no mature, actively-maintained, AST-only Cypher-parsing crate
// exists on crates.io analogous to `sqlparser` for SQL (see MEMORY.md's
// M3 planning notes) — `open-cypher` is abandoned, `drasi-query-cypher` is
// built for a different (continuous/incremental) execution model,
// `uni-cypher` has no independent track record. Hand-rolling a
// deliberately narrow grammar is the "practical subset" call, matching how
// this project already scoped SQL's own WHERE clause to AND-only.
//
// Grammar (v1, locked): `MATCH (a)-[:TYPE]->(b) WHERE <predicate> RETURN
// <items>` — single fixed-length directed hop, edge type always in
// brackets (optionally empty: `-[]->` matches any type), `WHERE` is
// AND-only comparisons, `RETURN` is a comma-separated list of bare
// identifiers. Node variables are opaque `i64` IDs only — `a.x`/`b.x`
// property access is rejected with a clear error, enforcing the M3
// decision that there is no backing "nodes" table to join against.
//
// The Cypher-variable-to-edge-column mapping happens entirely in this
// file: by the time a `CypherQuery` exists, `predicate` is an ordinary
// `sql::logical::Expr` referencing `__edges__`'s real column names
// (`from_id`, `to_id`, `edge_type`), so `graph::executor` never needs to
// know Cypher variable names existed.

use crate::{
    error::{DbError, Result},
    sql::logical::{CmpOp, Expr, Literal},
};

use super::logical::{CypherQuery, ReturnItem};

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Ident(String),
    Number(String),
    Str(String),
    LParen,
    RParen,
    LBracket,
    RBracket,
    Colon,
    Dash,
    Arrow,
    Comma,
    Dot,
    Op(String),
}

fn tokenize(input: &str) -> Result<Vec<Token>> {
    let chars: Vec<char> = input.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        match c {
            '(' => {
                tokens.push(Token::LParen);
                i += 1;
            }
            ')' => {
                tokens.push(Token::RParen);
                i += 1;
            }
            '[' => {
                tokens.push(Token::LBracket);
                i += 1;
            }
            ']' => {
                tokens.push(Token::RBracket);
                i += 1;
            }
            ':' => {
                tokens.push(Token::Colon);
                i += 1;
            }
            ',' => {
                tokens.push(Token::Comma);
                i += 1;
            }
            '.' => {
                tokens.push(Token::Dot);
                i += 1;
            }
            '-' => {
                if chars.get(i + 1) == Some(&'>') {
                    tokens.push(Token::Arrow);
                    i += 2;
                } else if chars.get(i + 1).is_some_and(|c| c.is_ascii_digit()) {
                    let start = i;
                    i += 1;
                    while chars.get(i).is_some_and(|c| c.is_ascii_digit()) {
                        i += 1;
                    }
                    tokens.push(Token::Number(chars[start..i].iter().collect()));
                } else {
                    tokens.push(Token::Dash);
                    i += 1;
                }
            }
            '=' => {
                tokens.push(Token::Op("=".to_string()));
                i += 1;
            }
            '!' if chars.get(i + 1) == Some(&'=') => {
                tokens.push(Token::Op("!=".to_string()));
                i += 2;
            }
            '<' => {
                if chars.get(i + 1) == Some(&'=') {
                    tokens.push(Token::Op("<=".to_string()));
                    i += 2;
                } else {
                    tokens.push(Token::Op("<".to_string()));
                    i += 1;
                }
            }
            '>' => {
                if chars.get(i + 1) == Some(&'=') {
                    tokens.push(Token::Op(">=".to_string()));
                    i += 2;
                } else {
                    tokens.push(Token::Op(">".to_string()));
                    i += 1;
                }
            }
            '\'' | '"' => {
                let quote = c;
                i += 1;
                let start = i;
                while chars.get(i).is_some_and(|&c| c != quote) {
                    i += 1;
                }
                if i >= chars.len() {
                    return Err(DbError::SqlParse("unterminated string literal".into()));
                }
                tokens.push(Token::Str(chars[start..i].iter().collect()));
                i += 1;
            }
            c if c.is_ascii_digit() => {
                let start = i;
                while chars.get(i).is_some_and(|c| c.is_ascii_digit()) {
                    i += 1;
                }
                tokens.push(Token::Number(chars[start..i].iter().collect()));
            }
            c if c.is_alphabetic() || c == '_' => {
                let start = i;
                while chars
                    .get(i)
                    .is_some_and(|c| c.is_alphanumeric() || *c == '_')
                {
                    i += 1;
                }
                tokens.push(Token::Ident(chars[start..i].iter().collect()));
            }
            other => {
                return Err(DbError::SqlParse(format!(
                    "unexpected character '{other}' at position {i}"
                )))
            }
        }
    }
    Ok(tokens)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn advance(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).cloned();
        self.pos += 1;
        t
    }

    fn next_token(&mut self) -> Result<Token> {
        self.advance()
            .ok_or_else(|| DbError::SqlParse("unexpected end of Cypher query".into()))
    }

    fn expect(&mut self, want: Token) -> Result<()> {
        let got = self.next_token()?;
        if got == want {
            Ok(())
        } else {
            Err(DbError::SqlParse(format!(
                "expected {want:?}, found {got:?}"
            )))
        }
    }

    fn expect_ident(&mut self) -> Result<String> {
        match self.next_token()? {
            Token::Ident(s) => Ok(s),
            other => Err(DbError::SqlParse(format!(
                "expected identifier, found {other:?}"
            ))),
        }
    }

    fn peek_keyword(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Token::Ident(s)) if s.eq_ignore_ascii_case(kw))
    }

    fn expect_keyword(&mut self, kw: &str) -> Result<()> {
        match self.next_token()? {
            Token::Ident(s) if s.eq_ignore_ascii_case(kw) => Ok(()),
            other => Err(DbError::SqlParse(format!(
                "expected keyword {kw}, found {other:?}"
            ))),
        }
    }

    /// Reject `ident.anything` — node variables are opaque IDs only (M3's
    /// confirmed scope decision), so property access always errors clearly
    /// rather than silently mis-parsing.
    fn reject_dot(&self) -> Result<()> {
        if self.peek() == Some(&Token::Dot) {
            return Err(DbError::SqlUnsupported(
                "property access on node variables is not supported — nodes are opaque IDs only"
                    .into(),
            ));
        }
        Ok(())
    }

    fn resolve_ident(&self, name: &str, from_var: &str, to_var: &str) -> Result<Expr> {
        if name == from_var {
            Ok(Expr::Column("from_id".to_string()))
        } else if name == to_var {
            Ok(Expr::Column("to_id".to_string()))
        } else if name.eq_ignore_ascii_case("type") {
            Ok(Expr::Column("edge_type".to_string()))
        } else {
            Err(DbError::SqlUnsupported(format!(
                "unknown identifier in Cypher query: {name}"
            )))
        }
    }

    fn parse_where_expr(&mut self, from_var: &str, to_var: &str) -> Result<Expr> {
        let mut expr = self.parse_comparison(from_var, to_var)?;
        while self.peek_keyword("AND") {
            self.advance();
            let rhs = self.parse_comparison(from_var, to_var)?;
            expr = Expr::And(Box::new(expr), Box::new(rhs));
        }
        Ok(expr)
    }

    fn parse_comparison(&mut self, from_var: &str, to_var: &str) -> Result<Expr> {
        let lhs = self.parse_operand(from_var, to_var)?;
        let op = self.parse_cmp_op()?;
        let rhs = self.parse_operand(from_var, to_var)?;
        Ok(Expr::BinOp {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        })
    }

    fn parse_operand(&mut self, from_var: &str, to_var: &str) -> Result<Expr> {
        let expr = match self.next_token()? {
            Token::Ident(name) => self.resolve_ident(&name, from_var, to_var)?,
            Token::Number(s) => {
                let n = s
                    .parse::<i64>()
                    .map_err(|_| DbError::SqlParse(format!("invalid integer literal: {s}")))?;
                Expr::Literal(Literal::Int(n))
            }
            Token::Str(s) => Expr::Literal(Literal::Text(s)),
            other => {
                return Err(DbError::SqlParse(format!(
                    "expected an identifier or literal, found {other:?}"
                )))
            }
        };
        self.reject_dot()?;
        Ok(expr)
    }

    fn parse_cmp_op(&mut self) -> Result<CmpOp> {
        match self.next_token()? {
            Token::Op(s) => match s.as_str() {
                "=" => Ok(CmpOp::Eq),
                "!=" => Ok(CmpOp::Ne),
                "<" => Ok(CmpOp::Lt),
                ">" => Ok(CmpOp::Gt),
                "<=" => Ok(CmpOp::Le),
                ">=" => Ok(CmpOp::Ge),
                other => Err(DbError::SqlParse(format!("unsupported operator: {other}"))),
            },
            other => Err(DbError::SqlParse(format!(
                "expected a comparison operator, found {other:?}"
            ))),
        }
    }

    fn parse_return_list(&mut self, from_var: &str, to_var: &str) -> Result<Vec<ReturnItem>> {
        let mut items = vec![self.parse_return_item(from_var, to_var)?];
        while self.peek() == Some(&Token::Comma) {
            self.advance();
            items.push(self.parse_return_item(from_var, to_var)?);
        }
        Ok(items)
    }

    fn parse_return_item(&mut self, from_var: &str, to_var: &str) -> Result<ReturnItem> {
        let name = self.expect_ident()?;
        if self.peek() == Some(&Token::Dot) {
            return Err(DbError::SqlUnsupported(
                "property access in RETURN is not supported — nodes are opaque IDs only".into(),
            ));
        }
        if name == from_var {
            Ok(ReturnItem::FromVar)
        } else if name == to_var {
            Ok(ReturnItem::ToVar)
        } else if name.eq_ignore_ascii_case("type") {
            Ok(ReturnItem::EdgeColumn("edge_type".to_string()))
        } else if name.eq_ignore_ascii_case("props") {
            Ok(ReturnItem::EdgeColumn("props".to_string()))
        } else {
            Err(DbError::SqlUnsupported(format!(
                "unknown RETURN item: {name}"
            )))
        }
    }
}

/// `MATCH (a)-[:TYPE]->(b) WHERE <predicate> RETURN <items>`. `-[]->`
/// (empty brackets) matches any edge type.
pub fn parse_cypher(input: &str) -> Result<CypherQuery> {
    let tokens = tokenize(input)?;
    let mut p = Parser { tokens, pos: 0 };

    p.expect_keyword("MATCH")?;
    p.expect(Token::LParen)?;
    let from_var = p.expect_ident()?;
    p.expect(Token::RParen)?;
    p.expect(Token::Dash)?;
    p.expect(Token::LBracket)?;
    let edge_type = if p.peek() == Some(&Token::Colon) {
        p.advance();
        Some(p.expect_ident()?)
    } else {
        None
    };
    p.expect(Token::RBracket)?;
    p.expect(Token::Arrow)?;
    p.expect(Token::LParen)?;
    let to_var = p.expect_ident()?;
    p.expect(Token::RParen)?;

    let predicate = if p.peek_keyword("WHERE") {
        p.advance();
        Some(p.parse_where_expr(&from_var, &to_var)?)
    } else {
        None
    };

    p.expect_keyword("RETURN")?;
    let returns = p.parse_return_list(&from_var, &to_var)?;

    if p.pos != p.tokens.len() {
        return Err(DbError::SqlParse(
            "unexpected trailing tokens after RETURN clause".into(),
        ));
    }

    Ok(CypherQuery {
        from_var,
        to_var,
        edge_type,
        predicate,
        returns,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_hop_with_type_and_where_and_return() {
        let q = parse_cypher("MATCH (a)-[:KNOWS]->(b) WHERE a = 1 RETURN b").unwrap();
        assert_eq!(q.from_var, "a");
        assert_eq!(q.to_var, "b");
        assert_eq!(q.edge_type, Some("KNOWS".to_string()));
        assert_eq!(
            q.predicate,
            Some(Expr::BinOp {
                op: CmpOp::Eq,
                lhs: Box::new(Expr::Column("from_id".to_string())),
                rhs: Box::new(Expr::Literal(Literal::Int(1))),
            })
        );
        assert_eq!(q.returns, vec![ReturnItem::ToVar]);
    }

    #[test]
    fn parses_without_where_clause() {
        let q = parse_cypher("MATCH (a)-[:KNOWS]->(b) RETURN a, b").unwrap();
        assert_eq!(q.predicate, None);
        assert_eq!(q.returns, vec![ReturnItem::FromVar, ReturnItem::ToVar]);
    }

    #[test]
    fn empty_brackets_match_any_edge_type() {
        let q = parse_cypher("MATCH (a)-[]->(b) RETURN b").unwrap();
        assert_eq!(q.edge_type, None);
    }

    #[test]
    fn parses_anded_where_predicate() {
        let q = parse_cypher("MATCH (a)-[:KNOWS]->(b) WHERE a = 1 AND type = 'KNOWS' RETURN b")
            .unwrap();
        assert!(matches!(q.predicate, Some(Expr::And(_, _))));
    }

    #[test]
    fn parses_return_edge_columns() {
        let q = parse_cypher("MATCH (a)-[:KNOWS]->(b) RETURN b, type, props").unwrap();
        assert_eq!(
            q.returns,
            vec![
                ReturnItem::ToVar,
                ReturnItem::EdgeColumn("edge_type".to_string()),
                ReturnItem::EdgeColumn("props".to_string()),
            ]
        );
    }

    #[test]
    fn rejects_property_access_in_where() {
        let err = parse_cypher("MATCH (a)-[:KNOWS]->(b) WHERE a.name = 'alice' RETURN b");
        assert!(matches!(err, Err(DbError::SqlUnsupported(_))));
    }

    #[test]
    fn rejects_property_access_in_return() {
        let err = parse_cypher("MATCH (a)-[:KNOWS]->(b) RETURN b.name");
        assert!(matches!(err, Err(DbError::SqlUnsupported(_))));
    }

    #[test]
    fn rejects_missing_return_clause() {
        let err = parse_cypher("MATCH (a)-[:KNOWS]->(b)");
        assert!(matches!(err, Err(DbError::SqlParse(_))));
    }

    #[test]
    fn keywords_are_case_insensitive() {
        let q = parse_cypher("match (a)-[:knows]->(b) where a = 1 return b").unwrap();
        assert_eq!(q.edge_type, Some("knows".to_string()));
    }
}
