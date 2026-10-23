//! Pure helpers for building SQL safely and shaping results. Identifiers are
//! only ever quoted from names the catalog returned, never from raw input.

use serde_json::Value;

/// Cells longer than this (bytes) are cut and end with [`ELLIPSIS`].
pub const CELL_BYTES: usize = 10 * 1024;
/// What we ask Postgres for per cell (characters), so a huge value never
/// crosses the network whole: one more than fits, to know it was cut.
pub const CELL_CHARS: usize = CELL_BYTES + 1;
pub const ELLIPSIS: &str = "…";

/// `"name"`, with embedded quotes doubled.
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// `"schema"."name"`.
pub fn qualified(schema: &str, name: &str) -> String {
    format!("{}.{}", quote_ident(schema), quote_ident(name))
}

/// A string literal (standard_conforming_strings, the default since 9.1).
pub fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// A cell as JSON, capped at [`CELL_BYTES`]; also returns its size so callers
/// can budget the response.
pub fn render_cell(value: Option<&str>) -> (Value, usize) {
    let Some(v) = value else {
        return (Value::Null, 4);
    };
    if v.len() <= CELL_BYTES {
        return (Value::String(v.to_owned()), v.len());
    }
    let mut end = CELL_BYTES;
    while !v.is_char_boundary(end) {
        end -= 1;
    }
    let cut = format!("{}{ELLIPSIS}", &v[..end]);
    let len = cut.len();
    (Value::String(cut), len)
}

/// `schema.name` → (Some(schema), name); a bare name → (None, name). Table
/// names may contain dots, so callers also try the whole input as a name.
pub fn split_table(input: &str) -> (Option<&str>, &str) {
    match input.split_once('.') {
        Some((schema, name)) if !schema.is_empty() && !name.is_empty() => (Some(schema), name),
        _ => (None, input),
    }
}

/// Split a script into statements on top-level semicolons, skipping quoted
/// strings, quoted identifiers, dollar-quoted bodies and comments. Empty
/// statements are dropped. Only used to label results; Postgres parses the
/// real thing.
pub fn split_statements(sql: &str) -> Vec<&str> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let (mut start, mut i) = (0, 0);
    while i < b.len() {
        match b[i] {
            b'\'' | b'"' => {
                let q = b[i];
                i += 1;
                while i < b.len() {
                    if b[i] == q {
                        // A doubled quote is an escaped one.
                        if b.get(i + 1) == Some(&q) {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => i += block_comment_len(&sql[i..]),
            b'$' => match dollar_tag(&sql[i..]) {
                Some(tag) => {
                    let body = i + tag.len();
                    i = match sql[body..].find(tag) {
                        Some(end) => body + end + tag.len(),
                        None => b.len(),
                    };
                }
                None => i += 1,
            },
            b';' => {
                out.push(&sql[start..i]);
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    out.push(&sql[start..]);
    out.into_iter()
        .filter(|s| !skip_comments(s).is_empty())
        .collect()
}

/// The length of the block comment `s` starts with (they nest in Postgres);
/// all of `s` when it never closes.
fn block_comment_len(s: &str) -> usize {
    let b = s.as_bytes();
    let (mut depth, mut i) = (0, 0);
    while i < b.len() {
        if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
            depth += 1;
            i += 2;
        } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return i;
            }
        } else {
            i += 1;
        }
    }
    b.len()
}

/// `$tag$` (or `$$`) at the start of `s`, if it opens a dollar quote.
fn dollar_tag(s: &str) -> Option<&str> {
    let rest = &s[1..];
    let end = rest.find('$')?;
    let tag = &rest[..end];
    let ok = tag
        .bytes()
        .enumerate()
        .all(|(i, c)| c == b'_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit()));
    ok.then(|| &s[..end + 2])
}

/// The text after leading whitespace and comments.
fn skip_comments(stmt: &str) -> &str {
    let mut s = stmt;
    loop {
        s = s.trim_start();
        if let Some(rest) = s.strip_prefix("--") {
            s = rest.split_once('\n').map_or("", |(_, r)| r);
        } else if s.starts_with("/*") {
            s = &s[block_comment_len(s)..];
        } else {
            return s;
        }
    }
}

/// The statement's first keyword, uppercased ("SELECT", "INSERT", …), after
/// whitespace and comments; empty when it doesn't start with one.
pub fn leading_keyword(stmt: &str) -> String {
    skip_comments(stmt)
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_and_literals_are_quoted() {
        assert_eq!(quote_ident("users"), r#""users""#);
        assert_eq!(quote_ident(r#"we"ird"#), r#""we""ird""#);
        assert_eq!(
            quote_ident(r#"x"; DROP TABLE t; --"#),
            r#""x""; DROP TABLE t; --""#
        );
        assert_eq!(qualified("my schema", "T"), r#""my schema"."T""#);
        assert_eq!(quote_literal("it's"), "'it''s'");
    }

    #[test]
    fn cells_are_capped_on_a_char_boundary() {
        assert_eq!(render_cell(None).0, Value::Null);
        assert_eq!(render_cell(Some("hi")), (Value::String("hi".into()), 2));
        let exact = "a".repeat(CELL_BYTES);
        assert_eq!(render_cell(Some(&exact)).0, Value::String(exact.clone()));
        // A multi-byte char straddling the limit is dropped whole.
        let long = format!("{}é{}", "a".repeat(CELL_BYTES - 1), "b".repeat(50));
        let (v, len) = render_cell(Some(&long));
        let s = v.as_str().unwrap();
        assert!(s.ends_with(ELLIPSIS));
        assert_eq!(s.len(), CELL_BYTES - 1 + ELLIPSIS.len());
        assert_eq!(len, s.len());
    }

    #[test]
    fn table_names_split_on_the_first_dot() {
        assert_eq!(split_table("public.users"), (Some("public"), "users"));
        assert_eq!(split_table("users"), (None, "users"));
        assert_eq!(split_table("a.b.c"), (Some("a"), "b.c"));
        assert_eq!(split_table(".x"), (None, ".x"));
    }

    #[test]
    fn statements_split_outside_quotes_and_comments() {
        let sql = "SELECT 'a;b', \"c;d\" FROM t; -- x;y\n\
                   /* one /* nested; */ still; */ INSERT INTO t VALUES ($$;$$, $f$ ; $f$);\
                   ;  ; \n UPDATE t SET a = 'it''s;'";
        let parts = split_statements(sql);
        assert_eq!(parts.len(), 3, "{parts:?}");
        assert_eq!(leading_keyword(parts[0]), "SELECT");
        assert_eq!(leading_keyword(parts[1]), "INSERT");
        assert_eq!(leading_keyword(parts[2]), "UPDATE");
        assert!(split_statements(" ; -- nothing\n").is_empty());
        assert_eq!(split_statements("select 1").len(), 1);
        assert_eq!(split_statements("(select 1); (select 2)").len(), 2);
        // $1 is a parameter, not a dollar quote.
        assert_eq!(split_statements("SELECT $1; SELECT 2").len(), 2);
    }

    #[test]
    fn leading_keywords_skip_comments() {
        assert_eq!(leading_keyword("  -- hi\n /* x */ select 1"), "SELECT");
        assert_eq!(
            leading_keyword("with x as (select 1) select * from x"),
            "WITH"
        );
        assert_eq!(leading_keyword("-- only a comment"), "");
        assert_eq!(leading_keyword("(select 1)"), "");
    }
}
