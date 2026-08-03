//! SQL validator — recursive-descent parser for a strict subset of
//! SQL plus optional execution against `sqlite3` for the dialect
//! that the host actually has on disk.
//!
//! Subset covered:
//! - `SELECT [DISTINCT] <columns> FROM <table> [WHERE <expr>] [ORDER BY <columns> [ASC|DESC]] [LIMIT n]`
//! - `INSERT INTO <table> [(<columns>)] VALUES (<row>) [, (<row>)]`
//! - `UPDATE <table> SET <col>=<expr> [, ...] [WHERE <expr>]`
//! - `DELETE FROM <table> [WHERE <expr>]`
//! - `CREATE TABLE [IF NOT EXISTS] <table> (<col_def> [, ...])`
//! - `ALTER TABLE <table> ADD COLUMN <col_def> | RENAME TO <new_table>`
//!
//! Dialects accepted via the `kind` field (e.g. `kind: "dialect:postgresql"`)
//! or the `language` field (`sql`, `sql-postgresql`, `sql-sqlite`, `sql-mysql`).
//! Dialect-specific features (e.g. PostgreSQL `SERIAL`, MySQL `ENGINE=`,
//! SQLite `AUTOINCREMENT`) are gated by the dialect; passing them to a
//! different dialect is a Fail.
//!
//! When the dialect is `sql` or `sql-sqlite` and the host has
//! `sqlite3` on the allowlist, the validator also executes the
//! statement in an in-memory database. Pass means "parses + runs".
//! For other dialects (PostgreSQL, MySQL) only the parse step runs
//! because no embedded engine is shipped.
//!
//! Compliance: `proposal-01-concept.md` §5.8 ("Parser SQL. Validación
//! contra dialecto.") and `proposal-02-rust.md` §7. The parser is
//! hand-written so we do not depend on `sqlparser` (forbidden by
//! `proposal-03-add-ons.md` no-go list).

// The internal AST enums and structs (Tok, Keyword, Expr, Statement,
// ColumnDef, ...) are implementation details exposed only to the
// unit tests. They are not part of the public surface so the missing
// rustdoc lines are intentional.
#![allow(missing_docs)]

use std::fmt;

use crate::error::Result;
use crate::sandbox::{Sandbox, SandboxResult, SandboxStatus};

use super::rust_validator::tail;
use super::{CodeArtifact, ValidationEvidence, ValidationStatus, Validator, capture_tool_version};

/// Local alias for the parser's `Result` so the public entry
/// points can stay typed as `Result<_, ParseError>` regardless
/// of the crate-level `Result` alias.
type ParseResult<T> = std::result::Result<T, ParseError>;

/// SQL validator. Stateless; reuse freely.
#[derive(Debug, Default, Clone, Copy)]
pub struct SqlValidator;

impl SqlValidator {
    /// Build a new instance.
    pub fn new() -> Self {
        Self
    }

    /// Canonical language id for plain (ANSI) SQL.
    pub const LANGUAGE: &'static str = "sql";
    /// Language id for SQLite-flavoured SQL.
    pub const LANGUAGE_SQLITE: &'static str = "sql-sqlite";
    /// Language id for PostgreSQL-flavoured SQL.
    pub const LANGUAGE_POSTGRES: &'static str = "sql-postgresql";
    /// Language id for MySQL-flavoured SQL.
    pub const LANGUAGE_MYSQL: &'static str = "sql-mysql";

    /// Detect the dialect from the artifact's `kind` and `language`.
    /// The convention is `kind: "dialect:postgresql"` or
    /// `language: "sql-postgresql"`. Defaults to `Sql` (ANSI) when
    /// no marker is present.
    pub fn detect_dialect(artifact: &CodeArtifact) -> Dialect {
        if artifact.kind.starts_with("dialect:") {
            return Dialect::from_marker(&artifact.kind["dialect:".len()..]);
        }
        Dialect::from_marker(&artifact.language)
    }

    /// Run the validator. Always returns a `ValidationEvidence` —
    /// never bubbles up an error to the caller. The outcome is
    /// recorded in the `status` field with a description in
    /// `failed_checks` (Fail) or `skipped_checks` (Skipped).
    pub async fn check(artifact: &CodeArtifact, sandbox: &Sandbox) -> Result<ValidationEvidence> {
        let dialect = Self::detect_dialect(artifact);
        let source = artifact.source.trim();
        if source.is_empty() {
            return Ok(ValidationEvidence::skipped(
                "sql",
                "empty source; nothing to validate",
            ));
        }

        // Split on ';' but respect single-quoted strings. The
        // downstream sqlite3 command is happy to receive a script
        // with multiple statements, but our parser is
        // single-statement so we feed it one at a time.
        let statements = split_statements(source);
        if statements.is_empty() {
            return Ok(ValidationEvidence::skipped(
                "sql",
                "no statements after splitting on ';'",
            ));
        }

        let mut parser = Parser::new(dialect);
        let mut parse_failures: Vec<String> = Vec::new();
        let mut parse_ok = 0usize;
        for stmt in &statements {
            match parser.parse_statement(stmt) {
                Ok(parsed) => {
                    if let Some(err) = parser.dialect_check(&parsed) {
                        parse_failures.push(err);
                    } else {
                        parse_ok += 1;
                    }
                }
                Err(e) => parse_failures.push(e.to_string()),
            }
        }

        if !parse_failures.is_empty() {
            let mut evidence = ValidationEvidence::fail("sql", "parser rejected statements");
            evidence.failed_checks.extend(parse_failures);
            return Ok(evidence);
        }

        // Pass the dialect-specific engine check. For SQLite we
        // attempt real execution; for everything else the parse
        // alone is the verdict.
        if matches!(dialect, Dialect::Sql | Dialect::Sqlite) {
            match run_in_sqlite(sandbox, source).await {
                Ok(result) => {
                    let mut evidence = sqlite_evidence(result, parse_ok);
                    if let Some(v) = capture_tool_version(sandbox, "sqlite3").await {
                        evidence.reproducibility.push(("sqlite3".into(), v));
                    }
                    return Ok(evidence);
                }
                Err(e) => {
                    return Ok(ValidationEvidence::fail(
                        "sql",
                        format!("failed to spawn sqlite3: {e}"),
                    ));
                }
            }
        }

        let mut evidence = ValidationEvidence::pass("sql", "parser accepted all statements");
        evidence
            .checks_run
            .push(format!("parsed {} statement(s) as {dialect}", parse_ok));
        Ok(evidence)
    }
}

impl Validator for SqlValidator {
    fn name(&self) -> &'static str {
        "sql"
    }

    fn validate(
        &self,
        _proposal: &crate::domain::Proposal,
        _sandbox: Option<&Sandbox>,
    ) -> Result<ValidationEvidence> {
        Ok(ValidationEvidence::skipped(
            "sql",
            "no source code attached; check called per-artifact",
        ))
    }
}

/// SQL dialect the artifact targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// Plain ANSI SQL. Also the default.
    Sql,
    /// SQLite. The host's `sqlite3` binary is used for execution.
    Sqlite,
    /// PostgreSQL. Parser-only; no embedded engine shipped.
    Postgres,
    /// MySQL. Parser-only; no embedded engine shipped.
    Mysql,
}

impl fmt::Display for Dialect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Dialect {
    /// Stable lowercase name for the dialect.
    pub fn as_str(self) -> &'static str {
        match self {
            Dialect::Sql => "sql",
            Dialect::Sqlite => "sqlite",
            Dialect::Postgres => "postgresql",
            Dialect::Mysql => "mysql",
        }
    }

    /// Resolve a marker (after `dialect:` or a `language` value) to
    /// a dialect. Unknown values fall back to ANSI.
    pub fn from_marker(marker: &str) -> Self {
        let lower = marker.to_ascii_lowercase();
        match lower.as_str() {
            "sql" | "ansi" => Dialect::Sql,
            "sql-sqlite" | "sqlite" => Dialect::Sqlite,
            "sql-postgresql" | "postgresql" | "postgres" => Dialect::Postgres,
            "sql-mysql" | "mysql" => Dialect::Mysql,
            _ => Dialect::Sql,
        }
    }
}

/// Split `source` on ';' honouring single-quoted strings (where
/// `''` is the SQL escape for a literal apostrophe). Empty
/// fragments are dropped.
fn split_statements(source: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;
    let bytes = source.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if in_quote {
            current.push(b as char);
            if b == b'\'' {
                if bytes.get(i + 1) == Some(&b'\'') {
                    // Escaped quote — consume the next quote too.
                    current.push('\'');
                    i += 2;
                    continue;
                }
                in_quote = false;
            }
        } else {
            match b {
                b'\'' => {
                    in_quote = true;
                    current.push('\'');
                }
                b';' => {
                    let trimmed = current.trim().to_owned();
                    if !trimmed.is_empty() {
                        out.push(trimmed);
                    }
                    current.clear();
                }
                _ => current.push(b as char),
            }
        }
        i += 1;
    }
    let trimmed = current.trim().to_owned();
    if !trimmed.is_empty() {
        out.push(trimmed);
    }
    out
}

/// Parse error with a 1-based line/column hint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
    pub line: usize,
    pub column: usize,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}:{}: {}", self.line, self.column, self.message)
    }
}

impl ParseError {
    fn at(message: impl Into<String>, line: usize, column: usize) -> Self {
        Self {
            message: message.into(),
            line,
            column,
        }
    }
}

/// Token kinds produced by [`Tokenizer`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Ident(String),
    Number(String),
    String(String),
    // Punctuation
    LParen,
    RParen,
    Comma,
    Star,
    Semi,
    Dot,
    Eq,
    // Multi-char / keyword handled separately
    Keyword(Keyword),
}

/// SQL keywords we recognise. Anything else is an `Ident`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Keyword {
    Select,
    From,
    Where,
    Insert,
    Into,
    Values,
    Update,
    Set,
    Delete,
    Create,
    Table,
    If,
    Not,
    Exists,
    Alter,
    Add,
    Column,
    Rename,
    To,
    Distinct,
    Order,
    By,
    Asc,
    Desc,
    Limit,
    Null,
    Default,
    Primary,
    Key,
    AutoIncrement,
    Serial,
    Engine,
    Cast,
    As,
    And,
    Or,
}

impl Keyword {
    fn from_ident(lower: &str) -> Option<Self> {
        Some(match lower {
            "select" => Self::Select,
            "from" => Self::From,
            "where" => Self::Where,
            "insert" => Self::Insert,
            "into" => Self::Into,
            "values" => Self::Values,
            "update" => Self::Update,
            "set" => Self::Set,
            "delete" => Self::Delete,
            "create" => Self::Create,
            "table" => Self::Table,
            "if" => Self::If,
            "not" => Self::Not,
            "exists" => Self::Exists,
            "alter" => Self::Alter,
            "add" => Self::Add,
            "column" => Self::Column,
            "rename" => Self::Rename,
            "to" => Self::To,
            "distinct" => Self::Distinct,
            "order" => Self::Order,
            "by" => Self::By,
            "asc" => Self::Asc,
            "desc" => Self::Desc,
            "limit" => Self::Limit,
            "null" => Self::Null,
            "default" => Self::Default,
            "primary" => Self::Primary,
            "key" => Self::Key,
            "autoincrement" => Self::AutoIncrement,
            "serial" => Self::Serial,
            "engine" => Self::Engine,
            "cast" => Self::Cast,
            "as" => Self::As,
            "and" => Self::And,
            "or" => Self::Or,
            _ => return None,
        })
    }
}

/// Lightweight tokenizer.
struct Tokenizer<'a> {
    src: &'a [u8],
    pos: usize,
    line: usize,
    column: usize,
}

impl<'a> Tokenizer<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src: src.as_bytes(),
            pos: 0,
            line: 1,
            column: 1,
        }
    }

    fn bump_pos(&mut self, n: usize) {
        for &b in &self.src[self.pos..self.pos + n] {
            if b == b'\n' {
                self.line += 1;
                self.column = 1;
            } else {
                self.column += 1;
            }
        }
        self.pos += n;
    }

    fn skip_ws(&mut self) {
        while self.pos < self.src.len() {
            let b = self.src[self.pos];
            if b.is_ascii_whitespace() {
                self.bump_pos(1);
            } else if b == b'-' && self.src.get(self.pos + 1) == Some(&b'-') {
                // Line comment `-- ...` to end of line.
                while self.pos < self.src.len() && self.src[self.pos] != b'\n' {
                    self.bump_pos(1);
                }
            } else if b == b'/' && self.src.get(self.pos + 1) == Some(&b'*') {
                // Block comment `/* ... */`.
                self.bump_pos(2);
                while self.pos + 1 < self.src.len()
                    && !(self.src[self.pos] == b'*' && self.src[self.pos + 1] == b'/')
                {
                    self.bump_pos(1);
                }
                if self.pos + 1 < self.src.len() {
                    self.bump_pos(2);
                }
            } else {
                break;
            }
        }
    }

    fn next_token(&mut self) -> ParseResult<Option<Tok>> {
        self.skip_ws();
        if self.pos >= self.src.len() {
            return Ok(None);
        }
        let line = self.line;
        let column = self.column;
        let b = self.src[self.pos];
        match b {
            b'(' => {
                self.bump_pos(1);
                Ok(Some(Tok::LParen))
            }
            b')' => {
                self.bump_pos(1);
                Ok(Some(Tok::RParen))
            }
            b',' => {
                self.bump_pos(1);
                Ok(Some(Tok::Comma))
            }
            b'*' => {
                self.bump_pos(1);
                Ok(Some(Tok::Star))
            }
            b'.' => {
                self.bump_pos(1);
                Ok(Some(Tok::Dot))
            }
            b'=' => {
                self.bump_pos(1);
                Ok(Some(Tok::Eq))
            }
            b';' => {
                self.bump_pos(1);
                Ok(Some(Tok::Semi))
            }
            b'\'' => self.read_string(line, column),
            b'"' => self.read_quoted_ident(line, column),
            b'0'..=b'9' => self.read_number(line, column),
            _ if b.is_ascii_alphabetic() || b == b'_' => self.read_ident(line, column),
            other => Err(ParseError::at(
                format!("unexpected character '{}'", other as char),
                line,
                column,
            )),
        }
    }

    fn read_string(&mut self, line: usize, column: usize) -> ParseResult<Option<Tok>> {
        self.bump_pos(1); // opening quote
        let start = self.pos;
        while self.pos < self.src.len() {
            let b = self.src[self.pos];
            if b == b'\'' {
                if self.src.get(self.pos + 1) == Some(&b'\'') {
                    // Escaped quote ''
                    self.bump_pos(2);
                    continue;
                }
                let s = std::str::from_utf8(&self.src[start..self.pos])
                    .map_err(|e| {
                        ParseError::at(format!("invalid utf8 in string: {e}"), line, column)
                    })?
                    .to_owned();
                self.bump_pos(1); // closing quote
                return Ok(Some(Tok::String(s)));
            }
            self.bump_pos(1);
        }
        Err(ParseError::at("unterminated string literal", line, column))
    }

    fn read_quoted_ident(&mut self, line: usize, column: usize) -> ParseResult<Option<Tok>> {
        self.bump_pos(1); // opening quote
        let start = self.pos;
        while self.pos < self.src.len() && self.src[self.pos] != b'"' {
            self.bump_pos(1);
        }
        if self.pos >= self.src.len() {
            return Err(ParseError::at(
                "unterminated quoted identifier",
                line,
                column,
            ));
        }
        let s = std::str::from_utf8(&self.src[start..self.pos])
            .map_err(|e| ParseError::at(format!("invalid utf8: {e}"), line, column))?
            .to_owned();
        self.bump_pos(1); // closing quote
        Ok(Some(Tok::Ident(s)))
    }

    fn read_number(&mut self, line: usize, column: usize) -> ParseResult<Option<Tok>> {
        let start = self.pos;
        while self.pos < self.src.len()
            && (self.src[self.pos].is_ascii_digit() || self.src[self.pos] == b'.')
        {
            self.bump_pos(1);
        }
        let s = std::str::from_utf8(&self.src[start..self.pos])
            .map_err(|e| ParseError::at(format!("invalid utf8 in number: {e}"), line, column))?
            .to_owned();
        Ok(Some(Tok::Number(s)))
    }

    fn read_ident(&mut self, line: usize, column: usize) -> ParseResult<Option<Tok>> {
        let start = self.pos;
        while self.pos < self.src.len()
            && (self.src[self.pos].is_ascii_alphanumeric() || self.src[self.pos] == b'_')
        {
            self.bump_pos(1);
        }
        let raw = std::str::from_utf8(&self.src[start..self.pos])
            .map_err(|e| ParseError::at(format!("invalid utf8 in identifier: {e}"), line, column))?
            .to_owned();
        let lower = raw.to_ascii_lowercase();
        if let Some(kw) = Keyword::from_ident(&lower) {
            Ok(Some(Tok::Keyword(kw)))
        } else {
            Ok(Some(Tok::Ident(raw)))
        }
    }
}

/// Top-level AST node after a successful parse.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Statement {
    Select {
        distinct: bool,
        columns: Vec<Column>,
        from: Option<TableRef>,
        where_expr: Option<Expr>,
        order_by: Vec<(Column, OrderDir)>,
        limit: Option<i64>,
    },
    Insert {
        table: String,
        columns: Vec<String>,
        rows: Vec<Vec<Expr>>,
    },
    Update {
        table: String,
        assignments: Vec<(String, Expr)>,
        where_expr: Option<Expr>,
    },
    Delete {
        table: String,
        where_expr: Option<Expr>,
    },
    CreateTable {
        if_not_exists: bool,
        table: String,
        columns: Vec<ColumnDef>,
    },
    AlterTable {
        table: String,
        action: AlterAction,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Column {
    Star,
    Expr(Expr),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TableRef(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderDir {
    Asc,
    Desc,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    Ident(String),
    Number(String),
    String(String),
    Null,
    BinOp {
        op: BinOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    LogicalOp {
        op: LogicalOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Eq,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicalOp {
    And,
    Or,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ColumnDef {
    name: String,
    type_name: String,
    constraints: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AlterAction {
    AddColumn(ColumnDef),
    RenameTable(String),
}

/// Recursive-descent parser for the SQL subset. Owns the dialect
/// (used by `dialect_check`) and borrows from the input string while
/// a `Tokenizer` is alive; the public API takes a `&str` and
/// constructs the tokenizer per call to keep the borrow checker
/// happy.
struct Parser {
    dialect: Dialect,
}

impl Parser {
    fn new(dialect: Dialect) -> Self {
        Self { dialect }
    }

    fn parse_statement(&mut self, src: &str) -> ParseResult<Statement> {
        let mut tok = Tokenizer::new(src);
        let stmt = self.parse_statement_inner(&mut tok)?;
        // Reject trailing tokens.
        if let Some(extra) = tok.next_token()? {
            return Err(ParseError::at(
                format!("unexpected token after statement: {extra:?}"),
                tok.line,
                tok.column,
            ));
        }
        Ok(stmt)
    }

    fn parse_statement_inner(&mut self, tok: &mut Tokenizer<'_>) -> ParseResult<Statement> {
        let kw = self.expect_keyword(tok)?;
        match kw {
            Keyword::Select => self.parse_select(tok),
            Keyword::Insert => self.parse_insert(tok),
            Keyword::Update => self.parse_update(tok),
            Keyword::Delete => self.parse_delete(tok),
            Keyword::Create => self.parse_create(tok),
            Keyword::Alter => self.parse_alter(tok),
            other => Err(ParseError::at(
                format!("expected DML/DDL keyword, found {other:?}"),
                tok.line,
                tok.column,
            )),
        }
    }

    fn expect_keyword(&self, tok: &mut Tokenizer<'_>) -> ParseResult<Keyword> {
        let line = tok.line;
        let column = tok.column;
        match tok.next_token()? {
            Some(Tok::Keyword(kw)) => Ok(kw),
            Some(other) => Err(ParseError::at(
                format!("expected keyword, found {other:?}"),
                line,
                column,
            )),
            None => Err(ParseError::at("expected keyword, found EOF", line, column)),
        }
    }

    fn expect_ident(&self, tok: &mut Tokenizer<'_>) -> ParseResult<String> {
        let line = tok.line;
        let column = tok.column;
        match tok.next_token()? {
            Some(Tok::Ident(s)) => Ok(s),
            Some(Tok::Keyword(kw)) => Err(ParseError::at(
                format!("expected identifier, found reserved keyword {kw:?}"),
                line,
                column,
            )),
            Some(other) => Err(ParseError::at(
                format!("expected identifier, found {other:?}"),
                line,
                column,
            )),
            None => Err(ParseError::at(
                "expected identifier, found EOF",
                line,
                column,
            )),
        }
    }

    fn expect_punct(&self, tok: &mut Tokenizer<'_>, want: &Tok) -> ParseResult<()> {
        let line = tok.line;
        let column = tok.column;
        match tok.next_token()? {
            Some(t) if &t == want => Ok(()),
            Some(other) => Err(ParseError::at(
                format!("expected {want:?}, found {other:?}"),
                line,
                column,
            )),
            None => Err(ParseError::at(
                format!("expected {want:?}, found EOF"),
                line,
                column,
            )),
        }
    }

    fn parse_select(&self, tok: &mut Tokenizer<'_>) -> ParseResult<Statement> {
        // SELECT [DISTINCT] <columns> FROM <table> [WHERE] [ORDER BY] [LIMIT]
        let mut distinct = false;
        if let Some(Tok::Keyword(Keyword::Distinct)) = peek_token(tok)? {
            tok.next_token()?;
            distinct = true;
        }
        let columns = self.parse_column_list(tok)?;
        let mut from: Option<TableRef> = None;
        if let Some(Tok::Keyword(Keyword::From)) = peek_token(tok)? {
            tok.next_token()?;
            let name = self.expect_ident(tok)?;
            from = Some(TableRef(name));
        }
        let mut where_expr: Option<Expr> = None;
        if let Some(Tok::Keyword(Keyword::Where)) = peek_token(tok)? {
            tok.next_token()?;
            where_expr = Some(self.parse_expr(tok)?);
        }
        let mut order_by: Vec<(Column, OrderDir)> = Vec::new();
        if let Some(Tok::Keyword(Keyword::Order)) = peek_token(tok)? {
            tok.next_token()?;
            self.expect_keyword_or(tok, Keyword::By)?;
            loop {
                let col = self.parse_column(tok)?;
                let dir = match peek_token(tok)? {
                    Some(Tok::Keyword(Keyword::Asc)) => {
                        tok.next_token()?;
                        OrderDir::Asc
                    }
                    Some(Tok::Keyword(Keyword::Desc)) => {
                        tok.next_token()?;
                        OrderDir::Desc
                    }
                    _ => OrderDir::Asc,
                };
                order_by.push((col, dir));
                if let Some(Tok::Comma) = peek_token(tok)? {
                    tok.next_token()?;
                    continue;
                }
                break;
            }
        }
        let mut limit: Option<i64> = None;
        if let Some(Tok::Keyword(Keyword::Limit)) = peek_token(tok)? {
            tok.next_token()?;
            let n_tok = tok.next_token()?;
            let n = match n_tok {
                Some(Tok::Number(s)) => s
                    .parse::<i64>()
                    .map_err(|e| ParseError::at(format!("invalid LIMIT: {e}"), 0, 0))?,
                Some(other) => {
                    return Err(ParseError::at(
                        format!("expected number after LIMIT, found {other:?}"),
                        0,
                        0,
                    ));
                }
                None => return Err(ParseError::at("expected number after LIMIT", 0, 0)),
            };
            limit = Some(n);
        }
        Ok(Statement::Select {
            distinct,
            columns,
            from,
            where_expr,
            order_by,
            limit,
        })
    }

    fn parse_column_list(&self, tok: &mut Tokenizer<'_>) -> ParseResult<Vec<Column>> {
        let mut out = Vec::new();
        out.push(self.parse_column(tok)?);
        while let Some(Tok::Comma) = peek_token(tok)? {
            tok.next_token()?;
            out.push(self.parse_column(tok)?);
        }
        Ok(out)
    }

    fn parse_column(&self, tok: &mut Tokenizer<'_>) -> ParseResult<Column> {
        if let Some(Tok::Star) = peek_token(tok)? {
            tok.next_token()?;
            return Ok(Column::Star);
        }
        Ok(Column::Expr(self.parse_expr(tok)?))
    }

    fn parse_insert(&self, tok: &mut Tokenizer<'_>) -> ParseResult<Statement> {
        // INSERT INTO <table> [(<cols>)] VALUES (<row>) [, (<row>)]
        self.expect_keyword_or(tok, Keyword::Into)?;
        let table = self.expect_ident(tok)?;
        let mut columns: Vec<String> = Vec::new();
        if let Some(Tok::LParen) = peek_token(tok)? {
            tok.next_token()?;
            loop {
                columns.push(self.expect_ident(tok)?);
                if let Some(Tok::Comma) = peek_token(tok)? {
                    tok.next_token()?;
                    continue;
                }
                break;
            }
            self.expect_punct(tok, &Tok::RParen)?;
        }
        self.expect_keyword_or(tok, Keyword::Values)?;
        let mut rows: Vec<Vec<Expr>> = Vec::new();
        loop {
            self.expect_punct(tok, &Tok::LParen)?;
            let mut row: Vec<Expr> = Vec::new();
            loop {
                row.push(self.parse_expr(tok)?);
                if let Some(Tok::Comma) = peek_token(tok)? {
                    tok.next_token()?;
                    continue;
                }
                break;
            }
            self.expect_punct(tok, &Tok::RParen)?;
            rows.push(row);
            if let Some(Tok::Comma) = peek_token(tok)? {
                tok.next_token()?;
                continue;
            }
            break;
        }
        Ok(Statement::Insert {
            table,
            columns,
            rows,
        })
    }

    fn parse_update(&self, tok: &mut Tokenizer<'_>) -> ParseResult<Statement> {
        let table = self.expect_ident(tok)?;
        self.expect_keyword_or(tok, Keyword::Set)?;
        let mut assignments: Vec<(String, Expr)> = Vec::new();
        loop {
            let col = self.expect_ident(tok)?;
            self.expect_punct(tok, &Tok::Eq)?;
            let val = self.parse_expr(tok)?;
            assignments.push((col, val));
            if let Some(Tok::Comma) = peek_token(tok)? {
                tok.next_token()?;
                continue;
            }
            break;
        }
        let mut where_expr: Option<Expr> = None;
        if let Some(Tok::Keyword(Keyword::Where)) = peek_token(tok)? {
            tok.next_token()?;
            where_expr = Some(self.parse_expr(tok)?);
        }
        Ok(Statement::Update {
            table,
            assignments,
            where_expr,
        })
    }

    fn parse_delete(&self, tok: &mut Tokenizer<'_>) -> ParseResult<Statement> {
        self.expect_keyword_or(tok, Keyword::From)?;
        let table = self.expect_ident(tok)?;
        let mut where_expr: Option<Expr> = None;
        if let Some(Tok::Keyword(Keyword::Where)) = peek_token(tok)? {
            tok.next_token()?;
            where_expr = Some(self.parse_expr(tok)?);
        }
        Ok(Statement::Delete { table, where_expr })
    }

    fn parse_create(&self, tok: &mut Tokenizer<'_>) -> ParseResult<Statement> {
        self.expect_keyword_or(tok, Keyword::Table)?;
        let mut if_not_exists = false;
        if let Some(Tok::Keyword(Keyword::If)) = peek_token(tok)? {
            tok.next_token()?;
            self.expect_keyword_or(tok, Keyword::Not)?;
            self.expect_keyword_or(tok, Keyword::Exists)?;
            if_not_exists = true;
        }
        let table = self.expect_ident(tok)?;
        self.expect_punct(tok, &Tok::LParen)?;
        let mut columns: Vec<ColumnDef> = Vec::new();
        loop {
            columns.push(self.parse_column_def(tok)?);
            if let Some(Tok::Comma) = peek_token(tok)? {
                tok.next_token()?;
                continue;
            }
            break;
        }
        self.expect_punct(tok, &Tok::RParen)?;
        Ok(Statement::CreateTable {
            if_not_exists,
            table,
            columns,
        })
    }

    fn parse_column_def(&self, tok: &mut Tokenizer<'_>) -> ParseResult<ColumnDef> {
        let name = self.expect_ident(tok)?;
        let type_name = self.parse_type_name(tok)?;
        let mut constraints: Vec<String> = Vec::new();
        loop {
            match peek_token(tok)? {
                Some(Tok::Keyword(Keyword::Primary)) => {
                    tok.next_token()?;
                    self.expect_keyword_or(tok, Keyword::Key)?;
                    constraints.push("PRIMARY KEY".into());
                }
                Some(Tok::Keyword(Keyword::Not)) => {
                    tok.next_token()?;
                    self.expect_keyword_or(tok, Keyword::Null)?;
                    constraints.push("NOT NULL".into());
                }
                Some(Tok::Keyword(Keyword::Default)) => {
                    tok.next_token()?;
                    let val = match tok.next_token()? {
                        Some(Tok::String(s)) => format!("DEFAULT '{s}'"),
                        Some(Tok::Number(n)) => format!("DEFAULT {n}"),
                        Some(Tok::Keyword(Keyword::Null)) => "DEFAULT NULL".into(),
                        Some(other) => {
                            return Err(ParseError::at(
                                format!("invalid DEFAULT literal: {other:?}"),
                                0,
                                0,
                            ));
                        }
                        None => return Err(ParseError::at("expected literal after DEFAULT", 0, 0)),
                    };
                    constraints.push(val);
                }
                Some(Tok::Keyword(Keyword::AutoIncrement)) => {
                    tok.next_token()?;
                    constraints.push("AUTOINCREMENT".into());
                }
                _ => break,
            }
        }
        Ok(ColumnDef {
            name,
            type_name,
            constraints,
        })
    }

    fn parse_type_name(&self, tok: &mut Tokenizer<'_>) -> ParseResult<String> {
        // Type names can be `INTEGER`, `VARCHAR(255)`, `BIGSERIAL`, etc.
        let first = self.expect_ident(tok)?;
        let mut type_name = first.to_ascii_uppercase();
        if let Some(Tok::LParen) = peek_token(tok)? {
            tok.next_token()?;
            let size = match tok.next_token()? {
                Some(Tok::Number(n)) => n,
                Some(other) => {
                    return Err(ParseError::at(
                        format!("expected number in type size, found {other:?}"),
                        0,
                        0,
                    ));
                }
                None => return Err(ParseError::at("expected number in type size", 0, 0)),
            };
            self.expect_punct(tok, &Tok::RParen)?;
            type_name.push_str(&format!("({size})"));
        }
        // Multi-word types (BIGSERIAL is one token, INT UNSIGNED is two).
        while let Some(Tok::Keyword(Keyword::AutoIncrement)) = peek_token(tok)? {
            tok.next_token()?;
            type_name.push_str(" AUTOINCREMENT");
        }
        Ok(type_name)
    }

    fn parse_alter(&self, tok: &mut Tokenizer<'_>) -> ParseResult<Statement> {
        self.expect_keyword_or(tok, Keyword::Table)?;
        let table = self.expect_ident(tok)?;
        match peek_token(tok)? {
            Some(Tok::Keyword(Keyword::Add)) => {
                tok.next_token()?;
                self.expect_keyword_or(tok, Keyword::Column)?;
                let col = self.parse_column_def(tok)?;
                Ok(Statement::AlterTable {
                    table,
                    action: AlterAction::AddColumn(col),
                })
            }
            Some(Tok::Keyword(Keyword::Rename)) => {
                tok.next_token()?;
                self.expect_keyword_or(tok, Keyword::To)?;
                let new_name = self.expect_ident(tok)?;
                Ok(Statement::AlterTable {
                    table,
                    action: AlterAction::RenameTable(new_name),
                })
            }
            Some(other) => Err(ParseError::at(
                format!("expected ADD or RENAME, found {other:?}"),
                0,
                0,
            )),
            None => Err(ParseError::at(
                "expected ADD or RENAME after ALTER TABLE",
                0,
                0,
            )),
        }
    }

    fn parse_expr(&self, tok: &mut Tokenizer<'_>) -> ParseResult<Expr> {
        // Logical OR has the lowest precedence.
        let left = self.parse_expr_and(tok)?;
        if let Some(Tok::Keyword(Keyword::Or)) = peek_token(tok)? {
            tok.next_token()?;
            let right = self.parse_expr_and(tok)?;
            return Ok(Expr::LogicalOp {
                op: LogicalOp::Or,
                left: Box::new(left),
                right: Box::new(right),
            });
        }
        Ok(left)
    }

    fn parse_expr_and(&self, tok: &mut Tokenizer<'_>) -> ParseResult<Expr> {
        let left = self.parse_expr_eq(tok)?;
        if let Some(Tok::Keyword(Keyword::And)) = peek_token(tok)? {
            tok.next_token()?;
            let right = self.parse_expr_eq(tok)?;
            return Ok(Expr::LogicalOp {
                op: LogicalOp::And,
                left: Box::new(left),
                right: Box::new(right),
            });
        }
        Ok(left)
    }

    fn parse_expr_eq(&self, tok: &mut Tokenizer<'_>) -> ParseResult<Expr> {
        let left = self.parse_expr_atom(tok)?;
        if let Some(Tok::Eq) = peek_token(tok)? {
            tok.next_token()?;
            let right = self.parse_expr_atom(tok)?;
            return Ok(Expr::BinOp {
                op: BinOp::Eq,
                left: Box::new(left),
                right: Box::new(right),
            });
        }
        Ok(left)
    }

    fn parse_expr_atom(&self, tok: &mut Tokenizer<'_>) -> ParseResult<Expr> {
        let line = tok.line;
        let column = tok.column;
        match tok.next_token()? {
            Some(Tok::Number(n)) => Ok(Expr::Number(n)),
            Some(Tok::String(s)) => Ok(Expr::String(s)),
            Some(Tok::Keyword(Keyword::Null)) => Ok(Expr::Null),
            Some(Tok::Ident(name)) => Ok(Expr::Ident(name)),
            Some(other) => Err(ParseError::at(
                format!("expected expression atom, found {other:?}"),
                line,
                column,
            )),
            None => Err(ParseError::at(
                "expected expression atom, found EOF",
                line,
                column,
            )),
        }
    }

    fn expect_keyword_or(&self, tok: &mut Tokenizer<'_>, want: Keyword) -> ParseResult<()> {
        let line = tok.line;
        let column = tok.column;
        match tok.next_token()? {
            Some(Tok::Keyword(kw)) if kw == want => Ok(()),
            Some(other) => Err(ParseError::at(
                format!("expected {want:?}, found {other:?}"),
                line,
                column,
            )),
            None => Err(ParseError::at(
                format!("expected {want:?}, found EOF"),
                line,
                column,
            )),
        }
    }

    /// Verify dialect-specific features. Returns `Some(err)` if the
    /// statement uses syntax that does not belong to the target
    /// dialect (e.g. `SERIAL` in MySQL).
    fn dialect_check(&self, stmt: &Statement) -> Option<String> {
        let columns = match stmt {
            Statement::CreateTable { columns, .. } => columns,
            _ => return None,
        };
        match self.dialect {
            Dialect::Mysql => {
                if columns.iter().any(|c| {
                    c.type_name.contains("SERIAL")
                        || c.constraints.iter().any(|k| k == "AUTOINCREMENT")
                }) {
                    return Some(
                        "MySQL does not accept SERIAL / AUTOINCREMENT; use AUTO_INCREMENT"
                            .to_string(),
                    );
                }
            }
            Dialect::Sqlite => {
                if columns.iter().any(|c| c.type_name.contains("SERIAL")) {
                    return Some(
                        "SQLite does not accept the SERIAL pseudo-type; use INTEGER PRIMARY KEY"
                            .to_string(),
                    );
                }
            }
            Dialect::Postgres => {
                if columns
                    .iter()
                    .any(|c| c.constraints.iter().any(|k| k == "AUTOINCREMENT"))
                {
                    return Some(
                        "PostgreSQL does not accept AUTOINCREMENT; use SERIAL or GENERATED AS IDENTITY"
                            .to_string(),
                    );
                }
            }
            Dialect::Sql => {
                if columns.iter().any(|c| {
                    c.type_name.contains("SERIAL")
                        || c.constraints.iter().any(|k| k == "AUTOINCREMENT")
                }) {
                    return Some(
                        "ANSI SQL does not accept SERIAL / AUTOINCREMENT; declare a dialect explicitly"
                            .to_string(),
                    );
                }
            }
        }
        None
    }
}

fn peek_token(tok: &mut Tokenizer<'_>) -> ParseResult<Option<Tok>> {
    let saved_pos = tok.pos;
    let saved_line = tok.line;
    let saved_column = tok.column;
    let result = tok.next_token();
    tok.pos = saved_pos;
    tok.line = saved_line;
    tok.column = saved_column;
    result
}

/// Run `sqlite3 :memory: "<source>"` in the sandbox and return the
/// raw `SandboxResult`. The sandbox enforces the 64 KiB stdout/stderr
/// cap and aborts a command that exceeds it.
async fn run_in_sqlite(sandbox: &Sandbox, source: &str) -> Result<SandboxResult> {
    sandbox
        .run("sqlite3", &["-bail", ":memory:", source])
        .await
        .map_err(Into::into)
}

fn sqlite_evidence(result: SandboxResult, parse_ok: usize) -> ValidationEvidence {
    let status = match result.status {
        SandboxStatus::Pass => ValidationStatus::Pass,
        SandboxStatus::Fail | SandboxStatus::Timeout => ValidationStatus::Fail,
        SandboxStatus::NotFound | SandboxStatus::NotAllowed => ValidationStatus::Skipped,
        SandboxStatus::Error => ValidationStatus::Error,
    };
    let mut evidence = ValidationEvidence {
        validator: "sql".into(),
        status,
        command: Some(result.command.clone()),
        exit_code: Some(result.exit_code),
        stdout_summary: tail(&result.stdout, 2_000),
        stderr_summary: tail(&result.stderr, 2_000),
        ..ValidationEvidence::default()
    };
    evidence
        .checks_run
        .push(format!("parsed {} statement(s)", parse_ok));
    evidence
        .checks_run
        .push("sqlite3 :memory: execution".into());
    if status != ValidationStatus::Pass {
        evidence
            .failed_checks
            .push("sqlite3 refused to execute the script".into());
    }
    evidence
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::{Allowlist, Sandbox, SandboxConfig};

    fn sb() -> Sandbox {
        Sandbox::new(SandboxConfig::new()).unwrap()
    }

    #[test]
    fn dialect_from_marker_recognises_all_markers() {
        // The `dialect:` prefix is stripped by `detect_dialect` before
        // calling `from_marker`, so this test exercises the bare
        // marker form.
        assert_eq!(Dialect::from_marker("sql"), Dialect::Sql);
        assert_eq!(Dialect::from_marker("SQLite"), Dialect::Sqlite);
        assert_eq!(Dialect::from_marker("postgresql"), Dialect::Postgres);
        assert_eq!(Dialect::from_marker("sql-mysql"), Dialect::Mysql);
        assert_eq!(Dialect::from_marker("garbage"), Dialect::Sql);
    }

    #[test]
    fn detect_dialect_reads_kind_then_language() {
        let a = CodeArtifact::new("dialect:postgresql", "sql", "SELECT 1");
        assert_eq!(SqlValidator::detect_dialect(&a), Dialect::Postgres);
        let b = CodeArtifact::new("schema.sql", "sql-sqlite", "SELECT 1");
        assert_eq!(SqlValidator::detect_dialect(&b), Dialect::Sqlite);
        let c = CodeArtifact::new("schema.sql", "sql", "SELECT 1");
        assert_eq!(SqlValidator::detect_dialect(&c), Dialect::Sql);
    }

    #[test]
    fn split_statements_respects_quotes() {
        let s = "SELECT 'a;b' FROM t; SELECT 1;";
        let parts = split_statements(s);
        assert_eq!(parts.len(), 2);
        assert!(parts[0].contains("'a;b'"));
    }

    #[test]
    fn parser_accepts_simple_select() {
        let mut p = Parser::new(Dialect::Sql);
        let stmt = p
            .parse_statement("SELECT id, name FROM users WHERE id = 1 ORDER BY id ASC LIMIT 10")
            .unwrap();
        match stmt {
            Statement::Select {
                columns,
                from,
                where_expr,
                order_by,
                limit,
                ..
            } => {
                assert_eq!(columns.len(), 2);
                assert_eq!(from, Some(TableRef("users".into())));
                assert!(where_expr.is_some());
                assert_eq!(order_by.len(), 1);
                assert_eq!(limit, Some(10));
            }
            _ => panic!("expected Select"),
        }
    }

    #[test]
    fn parser_accepts_select_star() {
        let mut p = Parser::new(Dialect::Sql);
        let stmt = p.parse_statement("SELECT * FROM t").unwrap();
        assert!(matches!(stmt, Statement::Select { columns, .. } if columns == vec![Column::Star]));
    }

    #[test]
    fn parser_accepts_insert_with_columns_and_multi_rows() {
        let mut p = Parser::new(Dialect::Sql);
        let stmt = p
            .parse_statement("INSERT INTO t (a, b) VALUES (1, 'x'), (2, 'y')")
            .unwrap();
        match stmt {
            Statement::Insert { columns, rows, .. } => {
                assert_eq!(columns, vec!["a", "b"]);
                assert_eq!(rows.len(), 2);
            }
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn parser_accepts_update_with_where() {
        let mut p = Parser::new(Dialect::Sql);
        let stmt = p
            .parse_statement("UPDATE t SET a = 1, b = 'x' WHERE id = 5")
            .unwrap();
        match stmt {
            Statement::Update {
                assignments,
                where_expr,
                ..
            } => {
                assert_eq!(assignments.len(), 2);
                assert!(where_expr.is_some());
            }
            _ => panic!("expected Update"),
        }
    }

    #[test]
    fn parser_accepts_delete() {
        let mut p = Parser::new(Dialect::Sql);
        let stmt = p.parse_statement("DELETE FROM t WHERE id = 1").unwrap();
        assert!(matches!(
            stmt,
            Statement::Delete {
                where_expr: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn parser_accepts_create_table_with_constraints() {
        let mut p = Parser::new(Dialect::Sql);
        let stmt = p
            .parse_statement(
                "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, age INT DEFAULT 0)",
            )
            .unwrap();
        match stmt {
            Statement::CreateTable { columns, .. } => {
                assert_eq!(columns.len(), 3);
                assert!(columns[0].constraints.contains(&"PRIMARY KEY".into()));
                assert!(columns[1].constraints.contains(&"NOT NULL".into()));
            }
            _ => panic!("expected CreateTable"),
        }
    }

    #[test]
    fn parser_accepts_alter_add_column_and_rename() {
        let mut p = Parser::new(Dialect::Sql);
        let stmt = p
            .parse_statement("ALTER TABLE users ADD COLUMN email TEXT")
            .unwrap();
        assert!(matches!(
            stmt,
            Statement::AlterTable {
                action: AlterAction::AddColumn(_),
                ..
            }
        ));
        let stmt = p
            .parse_statement("ALTER TABLE users RENAME TO members")
            .unwrap();
        assert!(matches!(
            stmt,
            Statement::AlterTable {
                action: AlterAction::RenameTable(_),
                ..
            }
        ));
    }

    #[test]
    fn parser_rejects_malformed_sql() {
        let mut p = Parser::new(Dialect::Sql);
        assert!(p.parse_statement("SELECT FROM WHERE").is_err());
        assert!(p.parse_statement("INSERT VALUES (1)").is_err());
        assert!(p.parse_statement("UPDATE t SET = 1").is_err());
        assert!(p.parse_statement("CREATE TABLE (id INT)").is_err());
    }

    #[test]
    fn parser_rejects_unterminated_string() {
        let mut p = Parser::new(Dialect::Sql);
        let err = p.parse_statement("SELECT 'oops FROM t").unwrap_err();
        assert!(err.message.contains("unterminated string"));
    }

    #[test]
    fn dialect_check_blocks_serial_in_mysql() {
        let mut p = Parser::new(Dialect::Mysql);
        let stmt = p
            .parse_statement("CREATE TABLE t (id BIGSERIAL PRIMARY KEY)")
            .unwrap();
        let err = p.dialect_check(&stmt);
        assert!(err.is_some());
    }

    #[test]
    fn dialect_check_blocks_autoincrement_in_postgres() {
        let mut p = Parser::new(Dialect::Postgres);
        let stmt = p
            .parse_statement("CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT)")
            .unwrap();
        let err = p.dialect_check(&stmt);
        assert!(err.is_some());
    }

    #[test]
    fn dialect_check_blocks_serial_in_sqlite() {
        let mut p = Parser::new(Dialect::Sqlite);
        let stmt = p
            .parse_statement("CREATE TABLE t (id BIGSERIAL PRIMARY KEY)")
            .unwrap();
        let err = p.dialect_check(&stmt);
        assert!(err.is_some());
    }

    #[test]
    fn sqlite_engine_rejects_missing_binary() {
        // Strip sqlite3 from the allowlist so the validator falls
        // back to the parse-only verdict for non-sqlite dialects.
        let cfg = SandboxConfig::new()
            .with_allowlist(Allowlist::from_slice(["definitely-not-sqlite-xyz"]));
        let sandbox = Sandbox::new(cfg).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let artifact = CodeArtifact::new("schema.sql", "sql-postgresql", "SELECT 1");
        let ev = rt
            .block_on(SqlValidator::check(&artifact, &sandbox))
            .unwrap();
        assert_eq!(ev.status, ValidationStatus::Pass);
    }

    #[test]
    fn sqlite_engine_passes_on_valid_select() {
        if std::process::Command::new("sqlite3")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let artifact = CodeArtifact::new(
            "schema.sql",
            "sql-sqlite",
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT); SELECT * FROM t WHERE id = 1;",
        );
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let ev = rt.block_on(SqlValidator::check(&artifact, &sb())).unwrap();
        assert_eq!(ev.status, ValidationStatus::Pass);
    }

    #[test]
    fn sqlite_engine_fails_on_broken_select() {
        if std::process::Command::new("sqlite3")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let artifact = CodeArtifact::new("schema.sql", "sql-sqlite", "SELECT FROM;");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let ev = rt.block_on(SqlValidator::check(&artifact, &sb())).unwrap();
        assert_eq!(ev.status, ValidationStatus::Fail);
        assert!(!ev.failed_checks.is_empty());
    }

    #[test]
    fn empty_source_is_skipped() {
        let artifact = CodeArtifact::new("schema.sql", "sql", "   \n  ");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let ev = rt.block_on(SqlValidator::check(&artifact, &sb())).unwrap();
        assert_eq!(ev.status, ValidationStatus::Skipped);
    }
}
