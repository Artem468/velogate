use logos::Logos;
use std::ops::Range;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LexicalError {
    pub span: Range<usize>,
    pub message: String,
}

#[derive(Logos, Debug, Clone, PartialEq)]
#[logos(skip r"[ \t\r\n\f]+")]
#[logos(skip(r"//[^\r\n]*", allow_greedy = true))]
pub(crate) enum Token {
    #[token("gateway")]
    Gateway,
    #[token("endpoint")]
    Endpoint,
    #[token("databases")]
    Databases,
    #[token("protos")]
    Protos,
    #[token("port")]
    Port,
    #[token("host")]
    Host,
    #[token("env_file")]
    EnvFile,
    #[token("constants")]
    Constants,
    #[token("url")]
    Url,
    #[token("path")]
    Path,
    #[token("window")]
    Window,

    #[token("secure")]
    Secure,
    #[token("secret")]
    Secret,
    #[token("username")]
    Username,
    #[token("password")]
    Password,
    #[token("checks")]
    Checks,
    #[token("rate_limit")]
    RateLimit,
    #[token("let")]
    Let,
    #[token("fetch")]
    Fetch,
    #[token("db")]
    Db,
    #[token("query")]
    Query,
    #[token("env")]
    Env,
    #[token("grpc")]
    Grpc,
    #[token("call")]
    Call,
    #[token("command")]
    Command,
    #[token("as")]
    As,
    #[token("if")]
    If,
    #[token("else")]
    Else,
    #[token("respond")]
    Respond,

    #[token("timeout")]
    Timeout,
    #[token("retry")]
    Retry,
    #[token("times")]
    Times,
    #[token("delay")]
    Delay,
    #[token("fallback")]
    Fallback,
    #[token("method")]
    Method,
    #[token("body")]
    Body,
    #[token("headers")]
    Headers,
    #[token("cookies")]
    Cookies,

    #[token("filter")]
    Filter,
    #[token("map")]
    Map,
    #[token("take")]
    Take,
    #[token("sort")]
    Sort,
    #[token("limit")]
    Limit,
    #[token("offset")]
    Offset,
    #[token("group_by")]
    GroupBy,
    #[token("reduce")]
    Reduce,
    #[token("count")]
    Count,
    #[token("sum")]
    Sum,
    #[token("avg")]
    Avg,
    #[token("min")]
    Min,
    #[token("max")]
    Max,
    #[token("unique")]
    Unique,
    #[token("flat_map")]
    FlatMap,
    #[token("first")]
    First,
    #[token("last")]
    Last,
    #[token("sync")]
    Sync,

    #[regex(r"[a-zA-Z_][a-zA-Z0-9_]*", |lex| lex.slice().to_string())]
    Ident(String),

    #[regex(r#""([^"\\]|\\.)*""#, |lex| {
        let s = lex.slice();
        s[1..s.len()-1].to_string()
    })]
    String(String),

    #[regex(r"[0-9]+\.[0-9]+", |lex| lex.slice().parse::<f64>().unwrap())]
    Float(f64),

    #[regex(r"[0-9]+", |lex| lex.slice().parse::<i64>().unwrap())]
    Int(i64),

    #[token("true")]
    True,
    #[token("false")]
    False,
    #[token("null")]
    Null,

    #[token("{")]
    BraceOpen,
    #[token("}")]
    BraceClose,
    #[token("[")]
    BracketOpen,
    #[token("]")]
    BracketClose,
    #[token("(")]
    ParenOpen,
    #[token(")")]
    ParenClose,
    #[token(":")]
    Colon,
    #[token("::")]
    PathSep,
    #[token(";")]
    Semicolon,
    #[token(",")]
    Comma,
    #[token(".")]
    Dot,

    #[token("=")]
    Assign,
    #[token("+")]
    Plus,
    #[token("-")]
    Minus,
    #[token("*")]
    Star,
    #[token("/")]
    Slash,
    #[token("%")]
    Percent,
    #[token("|")]
    Pipe,
    #[token("=>")]
    Arrow,

    #[token("==")]
    Eq,
    #[token("!=")]
    Neq,
    #[token(">")]
    Gt,
    #[token("<")]
    Lt,
    #[token(">=")]
    Gte,
    #[token("<=")]
    Lte,
    #[token("&&")]
    And,
    #[token("||")]
    Or,
}

pub(crate) type SpannedToken = Result<(usize, Token, usize), LexicalError>;

pub(crate) fn lex(source: &str) -> impl Iterator<Item = SpannedToken> + '_ {
    Token::lexer(source)
        .spanned()
        .map(|(token, span)| match token {
            Ok(token) => Ok((span.start, token, span.end)),
            Err(()) => Err(LexicalError {
                span: span.clone(),
                message: format!("unexpected token {:?}", &source[span]),
            }),
        })
}
