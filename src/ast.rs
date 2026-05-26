use lasso::Spur;
use std::collections::HashMap;

// Теперь наш тип символа — это Spur (структурный u32 из lasso)
pub type Sym = Spur;

#[derive(Debug, Clone)]
pub struct GatewayConfig {
    pub name: String,
    pub port: u16,
    pub host: Option<String>,
    pub static_dbs: Vec<StaticDb>,
    pub static_protos: Vec<StaticProto>,
}

#[derive(Debug, Clone)]
pub struct StaticDb {
    pub name: Sym,   // Интернированное имя базы
    pub url: String, // Строка подключения
}

#[derive(Debug, Clone)]
pub struct StaticProto {
    pub name: Sym,
    pub path: String,
}

#[derive(Debug, Clone)]
pub enum Expression {
    Variable(Sym),
    Number(f64),
    String(String),
    Boolean(bool),
    Object(HashMap<String, Expression>),
    Array(Vec<Expression>),
    PropertyAccess(Box<Expression>, Sym),
    Call {
        callee: Box<Expression>,
        args: Vec<Expression>,
    },
    BinaryOp(Box<Expression>, BinaryOperator, Box<Expression>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOperator {
    Add,
    Sub,
    Mul,
    Div,
    Eq,
    Neq,
    Gt,
    Lt,
    Gte,
    Lte,
    And,
    Or,
}

#[derive(Debug, Clone)]
pub struct HttpConfig {
    pub url: Expression,
    pub timeout_ms: Option<u64>,
    pub retries: Option<u32>,
    pub delay_ms: Option<u64>,
    pub fallback: Option<Expression>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct TailConfig {
    pub timeout_ms: Option<u64>,
    pub retries: Option<u32>,
    pub delay_ms: Option<u64>,
    pub fallback: Option<Expression>,
}

#[derive(Debug, Clone)]
pub(crate) enum TailConfigEntry {
    Timeout(u64),
    Retry(u32),
    Delay(u64),
    Fallback(Expression),
}

#[derive(Debug, Clone)]
pub(crate) enum GatewayItem {
    Port(u16),
    Host(String),
    Databases(Vec<StaticDb>),
    Protos(Vec<StaticProto>),
}

#[derive(Debug, Clone)]
pub struct GrpcConfig {
    pub service_method: String,
    pub proto_path: Option<String>,
    pub service: Option<String>,
    pub method: Option<String>,
    pub payload: Expression,
    pub timeout_ms: Option<u64>,
    pub fallback: Option<Expression>,
}

#[derive(Debug, Clone)]
pub struct DbQueryConfig {
    pub db_source: Expression,
    pub sql: String,
    pub params: Vec<Expression>,
    pub timeout_ms: Option<u64>,
    pub fallback: Option<Expression>,
}

#[derive(Debug, Clone)]
pub enum PipeOp {
    Filter {
        param: Sym,
        condition: Expression,
    },
    Map {
        param: Sym,
        layout: HashMap<String, Expression>,
    },
    Take(usize),
}

#[derive(Debug, Clone)]
pub enum Step {
    Let {
        var_name: Sym,
        value: Expression,
    },
    FetchHttp {
        var_name: Sym,
        config: HttpConfig,
    },
    CallGrpc {
        var_name: Sym,
        config: GrpcConfig,
    },
    QueryDb {
        var_name: Sym,
        config: DbQueryConfig,
    },
    Pipe {
        var_name: Sym,
        source: Expression,
        operations: Vec<PipeOp>,
    },
}

#[derive(Debug, Clone)]
pub enum EndpointOption {
    Secure(Vec<Sym>),
    RateLimit {
        limit: u32,
        unit: Sym,
        window_ms: u64,
    },
}

#[derive(Debug, Clone)]
pub struct Endpoint {
    pub method: String,
    pub path: String,
    pub options: Vec<EndpointOption>,
    pub steps: Vec<Step>,
    pub response_status: u16,
    pub response_body: HashMap<String, Expression>,
}

#[derive(Debug, Clone)]
pub struct FileAST {
    pub gateway: GatewayConfig,
    pub endpoints: Vec<Endpoint>,
}
