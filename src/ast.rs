use lasso::Spur;
use std::collections::HashMap;

// Теперь наш тип символа — это Spur (структурный u32 из lasso)
pub type Sym = Spur;

#[derive(Debug, Clone)]
pub struct GatewayConfig {
    pub name: String,
    pub port: u16,
    pub port_raw: Option<i64>,
    pub host: Option<String>,
    pub env_file: Option<String>,
    pub constants: Vec<GatewayConstant>,
    pub static_dbs: Vec<StaticDb>,
    pub static_protos: Vec<StaticProto>,
}

#[derive(Debug, Clone)]
pub struct GatewayConstant {
    pub name: Sym,
    pub value: Expression,
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
    Mod,
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
    pub method: Option<String>,
    pub body: Option<Expression>,
    pub timeout_ms: Option<u64>,
    pub retries: Option<u32>,
    pub delay_ms: Option<u64>,
    pub fallback: Option<Expression>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct TailConfig {
    pub method: Option<String>,
    pub body: Option<Expression>,
    pub timeout_ms: Option<u64>,
    pub retries: Option<u32>,
    pub delay_ms: Option<u64>,
    pub fallback: Option<Expression>,
}

#[derive(Debug, Clone)]
pub(crate) enum TailConfigEntry {
    Method(String),
    Body(Expression),
    Timeout(u64),
    Retry(u32),
    Delay(u64),
    Fallback(Expression),
}

#[derive(Debug, Clone)]
pub(crate) enum GatewayItem {
    Port(i64),
    Host(String),
    EnvFile(String),
    Constants(Vec<GatewayConstant>),
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

macro_rules! pipe_ops {
    ($dispatch:ident) => {
        $dispatch! {
            closure {
                filter => Filter(condition),
                map => Map(value),
                sort => Sort(key),
                group_by => GroupBy(key),
                sum => Sum(value),
                avg => Avg(value),
                min => Min(value),
                max => Max(value),
                unique => Unique(key),
                flat_map => FlatMap(value),
            }
            expr {
                limit => Limit,
                offset => Offset,
                take => Take,
            }
            none {
                count => Count,
                first => First,
                last => Last,
            }
            reduce {
                reduce => Reduce(value),
            }
        }
    };
}

macro_rules! declare_pipe_ops {
    (
        closure { $($closure_name:ident => $closure_variant:ident($closure_field:ident),)* }
        expr { $($expr_name:ident => $expr_variant:ident,)* }
        none { $($none_name:ident => $none_variant:ident,)* }
        reduce { $($reduce_name:ident => $reduce_variant:ident($reduce_field:ident),)* }
    ) => {
        #[derive(Debug, Clone)]
        pub enum PipeOp {
            $(
                $closure_variant {
                    param: Sym,
                    $closure_field: Expression,
                },
            )*
            $($expr_variant(Expression),)*
            $($none_variant,)*
            $(
                $reduce_variant {
                    initial: Expression,
                    acc: Sym,
                    param: Sym,
                    $reduce_field: Expression,
                },
            )*
        }

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum PipeClosureOp {
            $($closure_variant,)*
        }

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum PipeExprOp {
            $($expr_variant,)*
        }

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum PipeNoneOp {
            $($none_variant,)*
        }

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum PipeReduceOp {
            $($reduce_variant,)*
        }

        impl PipeClosureOp {
            pub fn from_keyword(name: &str) -> Option<Self> {
                match name {
                    $(stringify!($closure_name) => Some(Self::$closure_variant),)*
                    _ => None,
                }
            }

            pub fn build(self, param: Sym, value: Expression) -> PipeOp {
                match self {
                    $(Self::$closure_variant => PipeOp::$closure_variant { param, $closure_field: value },)*
                }
            }
        }

        impl PipeExprOp {
            pub fn from_keyword(name: &str) -> Option<Self> {
                match name {
                    $(stringify!($expr_name) => Some(Self::$expr_variant),)*
                    _ => None,
                }
            }

            pub fn build(self, value: Expression) -> PipeOp {
                match self {
                    $(Self::$expr_variant => PipeOp::$expr_variant(value),)*
                }
            }
        }

        impl PipeNoneOp {
            pub fn from_keyword(name: &str) -> Option<Self> {
                match name {
                    $(stringify!($none_name) => Some(Self::$none_variant),)*
                    _ => None,
                }
            }

            pub fn build(self) -> PipeOp {
                match self {
                    $(Self::$none_variant => PipeOp::$none_variant,)*
                }
            }
        }

        impl PipeReduceOp {
            pub fn from_keyword(name: &str) -> Option<Self> {
                match name {
                    $(stringify!($reduce_name) => Some(Self::$reduce_variant),)*
                    _ => None,
                }
            }

            pub fn build(self, initial: Expression, acc: Sym, param: Sym, value: Expression) -> PipeOp {
                match self {
                    $(Self::$reduce_variant => PipeOp::$reduce_variant { initial, acc, param, $reduce_field: value },)*
                }
            }
        }

        impl PipeOp {
            pub fn registered_ops() -> &'static [(&'static str, &'static str)] {
                &[
                    $((stringify!($closure_name), "closure"),)*
                    $((stringify!($expr_name), "expr"),)*
                    $((stringify!($none_name), "none"),)*
                    $((stringify!($reduce_name), "expr_reduce_closure"),)*
                ]
            }
        }
    };
}

pipe_ops!(declare_pipe_ops);

impl PipeOp {
    pub fn shape_name(&self) -> &'static str {
        match self {
            Self::Filter { .. }
            | Self::Map { .. }
            | Self::Sort { .. }
            | Self::GroupBy { .. }
            | Self::Sum { .. }
            | Self::Avg { .. }
            | Self::Min { .. }
            | Self::Max { .. }
            | Self::Unique { .. }
            | Self::FlatMap { .. } => "closure",
            Self::Limit(_) | Self::Offset(_) | Self::Take(_) => "expr",
            Self::Count | Self::First | Self::Last => "none",
            Self::Reduce { .. } => "expr_reduce_closure",
        }
    }
}

#[derive(Debug, Clone)]
pub enum Step {
    Let {
        var_name: Sym,
        value: Expression,
    },
    Command {
        var_name: Sym,
        command: String,
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

impl Step {
    pub fn var_name(&self) -> Sym {
        match self {
            Self::Let { var_name, .. }
            | Self::Command { var_name, .. }
            | Self::FetchHttp { var_name, .. }
            | Self::CallGrpc { var_name, .. }
            | Self::QueryDb { var_name, .. }
            | Self::Pipe { var_name, .. } => *var_name,
        }
    }
}

#[derive(Debug, Clone)]
pub enum EndpointOption {
    Secure(Vec<SecureRule>),
    RateLimit {
        limit: u32,
        unit: Sym,
        window_ms: u64,
    },
}

#[derive(Debug, Clone)]
pub struct SecureRule {
    pub scheme: Sym,
    pub secret: Option<Expression>,
    pub username: Option<Expression>,
    pub password: Option<Expression>,
    pub checks: Vec<Expression>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SecureRuleConfig {
    pub secret: Option<Expression>,
    pub username: Option<Expression>,
    pub password: Option<Expression>,
    pub checks: Vec<Expression>,
}

#[derive(Debug, Clone)]
pub(crate) enum SecureRuleConfigEntry {
    Secret(Expression),
    Username(Expression),
    Password(Expression),
    Checks(Vec<Expression>),
}

#[derive(Debug, Clone)]
pub(crate) enum EndpointBodyItem {
    Step(Step),
    Sync(Vec<Step>),
}

#[derive(Debug, Clone)]
pub struct EndpointResponse {
    pub status: u16,
    pub status_raw: i64,
    pub body: Option<HashMap<String, Expression>>,
    pub headers: HashMap<String, Expression>,
    pub cookies: HashMap<String, Expression>,
}

impl Default for EndpointResponse {
    fn default() -> Self {
        Self {
            status: 200,
            status_raw: 200,
            body: None,
            headers: HashMap::new(),
            cookies: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct EndpointResponseParts {
    pub body: Option<HashMap<String, Expression>>,
    pub headers: HashMap<String, Expression>,
    pub cookies: HashMap<String, Expression>,
}

#[derive(Debug, Clone)]
pub(crate) enum EndpointResponsePart {
    Body(HashMap<String, Expression>),
    Headers(HashMap<String, Expression>),
    Cookies(HashMap<String, Expression>),
}

#[derive(Debug, Clone)]
pub struct Endpoint {
    pub method: String,
    pub path: String,
    pub options: Vec<EndpointOption>,
    pub steps: Vec<Step>,
    pub sync_boundaries: Vec<usize>,
    pub response: EndpointResponse,
}

#[derive(Debug, Clone)]
pub struct FileAST {
    pub gateway: GatewayConfig,
    pub endpoints: Vec<Endpoint>,
}
