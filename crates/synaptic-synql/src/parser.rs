//! SYNQL recursive-descent parser: tokens -> [`Query`].

use std::collections::HashSet;

use crate::ast::*;
use crate::lexer::{lex, Spanned, Tok};

/// Parse a SYNQL query string.
pub fn parse(input: &str) -> Result<Query, String> {
    let toks = lex(input)?;
    let mut p = Parser { toks, pos: 0 };
    let q = p.query()?;
    if p.pos != p.toks.len() {
        return Err(format!("unexpected trailing input at {}", p.toks[p.pos].at));
    }
    validate(&q)?;
    Ok(q)
}

struct Parser {
    toks: Vec<Spanned>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|s| &s.tok)
    }

    fn bump(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).map(|s| s.tok.clone());
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn at(&self) -> String {
        match self.toks.get(self.pos) {
            Some(s) => format!("{:?} at {}", s.tok, s.at),
            None => "end of query".to_string(),
        }
    }

    fn expect(&mut self, want: &Tok, label: &str) -> Result<(), String> {
        if self.peek() == Some(want) {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!("expected {label} but found {}", self.at()))
        }
    }

    fn ident(&mut self) -> Result<String, String> {
        match self.bump() {
            Some(Tok::Ident(s)) => Ok(s),
            _ => {
                self.pos = self.pos.saturating_sub(1);
                Err(format!("expected an identifier but found {}", self.at()))
            }
        }
    }

    fn query(&mut self) -> Result<Query, String> {
        self.expect(&Tok::Match, "MATCH")?;
        let pattern = self.pattern()?;
        let where_ = if self.peek() == Some(&Tok::Where) {
            self.pos += 1;
            Some(self.expr()?)
        } else {
            None
        };
        self.expect(&Tok::Return, "RETURN")?;
        let ret = self.ret_list()?;
        let limit = if self.peek() == Some(&Tok::Limit) {
            self.pos += 1;
            match self.bump() {
                Some(Tok::Num(n)) if n >= 0.0 => Some(n as usize),
                _ => return Err(format!("expected a LIMIT count but found {}", self.at())),
            }
        } else {
            None
        };
        Ok(Query {
            pattern,
            where_,
            ret,
            limit,
        })
    }

    fn pattern(&mut self) -> Result<Pattern, String> {
        let mut nodes = vec![self.nodepat()?];
        let mut rels = Vec::new();
        while matches!(self.peek(), Some(Tok::DashBracket) | Some(Tok::ArrowLDash)) {
            rels.push(self.relpat()?);
            nodes.push(self.nodepat()?);
        }
        Ok(Pattern { nodes, rels })
    }

    fn nodepat(&mut self) -> Result<NodePat, String> {
        self.expect(&Tok::LParen, "'('")?;
        let mut var = None;
        let mut kind = None;
        if let Some(Tok::Ident(_)) = self.peek() {
            var = Some(self.ident()?);
        }
        if self.peek() == Some(&Tok::Colon) {
            self.pos += 1;
            kind = Some(self.ident()?);
        }
        self.expect(&Tok::RParen, "')'")?;
        Ok(NodePat { var, kind })
    }

    fn relpat(&mut self) -> Result<RelPat, String> {
        match self.bump() {
            Some(Tok::DashBracket) => {
                let rel = self.opt_relname()?;
                let (min, max) = self.opt_varlen()?;
                match self.bump() {
                    Some(Tok::ArrowR) => Ok(RelPat {
                        rel,
                        dir: Dir::LtoR,
                        min,
                        max,
                    }),
                    Some(Tok::DashBracketEnd) => Ok(RelPat {
                        rel,
                        dir: Dir::Either,
                        min,
                        max,
                    }),
                    _ => {
                        self.pos = self.pos.saturating_sub(1);
                        Err(format!("expected ']->' or ']-' but found {}", self.at()))
                    }
                }
            }
            Some(Tok::ArrowLDash) => {
                let rel = self.opt_relname()?;
                let (min, max) = self.opt_varlen()?;
                self.expect(&Tok::DashBracketEnd, "']-'")?;
                Ok(RelPat {
                    rel,
                    dir: Dir::RtoL,
                    min,
                    max,
                })
            }
            _ => {
                self.pos = self.pos.saturating_sub(1);
                Err(format!("expected a relationship but found {}", self.at()))
            }
        }
    }

    fn opt_relname(&mut self) -> Result<Option<String>, String> {
        if self.peek() == Some(&Tok::Colon) {
            self.pos += 1;
            Ok(Some(self.ident()?))
        } else {
            Ok(None)
        }
    }

    /// Optional variable-length bound after a relationship: `*`, `*n`, `*..m`,
    /// `*n..`, `*n..m`. Returns (1,1) when absent.
    fn opt_varlen(&mut self) -> Result<(u32, u32), String> {
        if self.peek() != Some(&Tok::Star) {
            return Ok((1, 1));
        }
        self.pos += 1; // consume '*'
        let explicit_min = if let Some(Tok::Num(n)) = self.peek() {
            let n = *n;
            self.pos += 1;
            Some(n.max(1.0) as u32)
        } else {
            None
        };
        if self.peek() == Some(&Tok::DotDot) {
            self.pos += 1;
            let max = if let Some(Tok::Num(n)) = self.peek() {
                let n = *n;
                self.pos += 1;
                (n as u32).min(VARLEN_CAP)
            } else {
                VARLEN_CAP
            };
            let min = explicit_min.unwrap_or(1);
            Ok((min, max.max(min)))
        } else {
            match explicit_min {
                Some(n) => Ok((n, n)),       // *n
                None => Ok((1, VARLEN_CAP)), // * alone
            }
        }
    }

    fn ret_list(&mut self) -> Result<Vec<RetItem>, String> {
        let mut items = vec![self.ret_item()?];
        while self.peek() == Some(&Tok::Comma) {
            self.pos += 1;
            items.push(self.ret_item()?);
        }
        Ok(items)
    }

    fn ret_item(&mut self) -> Result<RetItem, String> {
        // `count(...)`: only an aggregate when the ident `count` is followed by '('.
        if let Some(Tok::Ident(w)) = self.peek() {
            if w.eq_ignore_ascii_case("count")
                && self.toks.get(self.pos + 1).map(|s| &s.tok) == Some(&Tok::LParen)
            {
                self.pos += 1; // count
                self.expect(&Tok::LParen, "'('")?;
                let arg = match self.peek() {
                    Some(Tok::Star) => {
                        self.pos += 1;
                        None
                    }
                    Some(Tok::Ident(_)) => Some(self.ident()?),
                    _ => {
                        return Err(format!(
                            "expected a variable or '*' but found {}",
                            self.at()
                        ))
                    }
                };
                self.expect(&Tok::RParen, "')'")?;
                return Ok(RetItem::Count(arg));
            }
        }
        let var = self.ident()?;
        if self.peek() == Some(&Tok::Dot) {
            self.pos += 1;
            let field_name = self.ident()?;
            let field = Field::parse(&field_name).ok_or_else(|| {
                format!(
                    "unknown field '{field_name}'; valid fields: {}",
                    Field::valid_names()
                )
            })?;
            Ok(RetItem::Prop(Prop { var, field }))
        } else {
            Ok(RetItem::Var(var))
        }
    }

    // expr := and ('OR' and)*
    fn expr(&mut self) -> Result<Expr, String> {
        let mut lhs = self.and()?;
        while self.peek() == Some(&Tok::Or) {
            self.pos += 1;
            let rhs = self.and()?;
            lhs = Expr::Or(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn and(&mut self) -> Result<Expr, String> {
        let mut lhs = self.not()?;
        while self.peek() == Some(&Tok::And) {
            self.pos += 1;
            let rhs = self.not()?;
            lhs = Expr::And(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn not(&mut self) -> Result<Expr, String> {
        if self.peek() == Some(&Tok::Not) {
            self.pos += 1;
            Ok(Expr::Not(Box::new(self.not()?)))
        } else {
            self.atom()
        }
    }

    fn atom(&mut self) -> Result<Expr, String> {
        match self.peek() {
            Some(Tok::LParen) => {
                self.pos += 1;
                let e = self.expr()?;
                self.expect(&Tok::RParen, "')'")?;
                Ok(e)
            }
            Some(Tok::Has) => self.has(),
            _ => self.comparison(),
        }
    }

    fn has(&mut self) -> Result<Expr, String> {
        self.expect(&Tok::Has, "has")?;
        self.expect(&Tok::LParen, "'('")?;
        let var = self.ident()?;
        self.expect(&Tok::Comma, "','")?;
        let modifier = match self.bump() {
            Some(Tok::Str(s)) => s,
            _ => {
                return Err(format!(
                    "expected a modifier string but found {}",
                    self.at()
                ))
            }
        };
        self.expect(&Tok::RParen, "')'")?;
        Ok(Expr::Has(var, modifier))
    }

    fn comparison(&mut self) -> Result<Expr, String> {
        let var = self.ident()?;
        self.expect(&Tok::Dot, "'.'")?;
        let field_name = self.ident()?;
        let field = Field::parse(&field_name).ok_or_else(|| {
            format!(
                "unknown field '{field_name}'; valid fields: {}",
                Field::valid_names()
            )
        })?;
        let op = match self.bump() {
            Some(Tok::Eq) => Op::Eq,
            Some(Tok::Ne) => Op::Ne,
            Some(Tok::Lt) => Op::Lt,
            Some(Tok::Le) => Op::Le,
            Some(Tok::Gt) => Op::Gt,
            Some(Tok::Ge) => Op::Ge,
            Some(Tok::Regex) => Op::Regex,
            _ => {
                self.pos = self.pos.saturating_sub(1);
                return Err(format!(
                    "expected a comparison operator but found {}",
                    self.at()
                ));
            }
        };
        let value = match self.bump() {
            Some(Tok::Str(s)) => Value::Str(s),
            Some(Tok::Num(n)) => Value::Num(n),
            Some(Tok::Ident(s)) => Value::Str(s), // bare ident as a string literal
            _ => {
                self.pos = self.pos.saturating_sub(1);
                return Err(format!("expected a value but found {}", self.at()));
            }
        };
        Ok(Expr::Cmp(Prop { var, field }, op, value))
    }
}

/// Every variable used in WHERE / RETURN must be bound by the pattern.
fn validate(q: &Query) -> Result<(), String> {
    let bound: HashSet<&str> = q
        .pattern
        .nodes
        .iter()
        .filter_map(|n| n.var.as_deref())
        .collect();
    for item in &q.ret {
        if let Some(v) = item.var() {
            if !bound.contains(v) {
                return Err(format!("RETURN variable '{v}' is not bound by the pattern"));
            }
        }
    }
    if let Some(e) = &q.where_ {
        check_vars(e, &bound)?;
    }
    Ok(())
}

fn check_vars(e: &Expr, bound: &HashSet<&str>) -> Result<(), String> {
    match e {
        Expr::And(a, b) | Expr::Or(a, b) => {
            check_vars(a, bound)?;
            check_vars(b, bound)
        }
        Expr::Not(a) => check_vars(a, bound),
        Expr::Cmp(p, _, _) => {
            if bound.contains(p.var.as_str()) {
                Ok(())
            } else {
                Err(format!(
                    "WHERE variable '{}' is not bound by the pattern",
                    p.var
                ))
            }
        }
        Expr::Has(v, _) => {
            if bound.contains(v.as_str()) {
                Ok(())
            } else {
                Err(format!("WHERE variable '{v}' is not bound by the pattern"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_property_query() {
        let q = parse("MATCH (c:class) WHERE c.loc > 500 AND c.fan_out > 20 RETURN c LIMIT 10")
            .unwrap();
        assert_eq!(q.pattern.nodes.len(), 1);
        assert_eq!(q.pattern.nodes[0].var.as_deref(), Some("c"));
        assert_eq!(q.pattern.nodes[0].kind.as_deref(), Some("class"));
        assert_eq!(q.ret, vec![RetItem::Var("c".into())]);
        assert_eq!(q.limit, Some(10));
        assert!(matches!(q.where_, Some(Expr::And(_, _))));
    }

    #[test]
    fn parses_varlen_and_aggregates() {
        let q = parse("MATCH (a)-[:calls*1..3]->(b) RETURN a").unwrap();
        assert_eq!(q.pattern.rels[0].min, 1);
        assert_eq!(q.pattern.rels[0].max, 3);
        let q2 = parse("MATCH (a)-[:calls*]->(b) RETURN a").unwrap();
        assert_eq!(
            (q2.pattern.rels[0].min, q2.pattern.rels[0].max),
            (1, VARLEN_CAP)
        );
        let q3 = parse("MATCH (c:class) RETURN c.community, count(c)").unwrap();
        assert!(q3.is_aggregate());
        assert_eq!(q3.ret.len(), 2);
        // a bare `count` (no parens) is still a normal variable reference
        assert!(matches!(
            parse("MATCH (count) RETURN count").unwrap().ret[0],
            RetItem::Var(_)
        ));
    }

    #[test]
    fn parses_relationship_directions() {
        let q = parse("MATCH (a:class)-[:implements]->(b:interface) RETURN a, b").unwrap();
        assert_eq!(q.pattern.rels.len(), 1);
        assert_eq!(q.pattern.rels[0].rel.as_deref(), Some("implements"));
        assert_eq!(q.pattern.rels[0].dir, Dir::LtoR);
        assert_eq!(
            q.ret,
            vec![RetItem::Var("a".into()), RetItem::Var("b".into())]
        );

        let q2 = parse("MATCH (a)<-[:calls]-(b) RETURN a").unwrap();
        assert_eq!(q2.pattern.rels[0].dir, Dir::RtoL);
        let q3 = parse("MATCH (a)-[]-(b) RETURN a").unwrap();
        assert_eq!(q3.pattern.rels[0].dir, Dir::Either);
        assert_eq!(q3.pattern.rels[0].rel, None);
    }

    #[test]
    fn unbound_return_var_errors() {
        let e = parse("MATCH (a) RETURN b").unwrap_err();
        assert!(e.contains("not bound"), "{e}");
    }

    #[test]
    fn unknown_field_errors() {
        let e = parse("MATCH (c) WHERE c.bogus = 1 RETURN c").unwrap_err();
        assert!(e.contains("unknown field"), "{e}");
    }
}
