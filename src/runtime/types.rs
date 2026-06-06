use crate::ast::{Endpoint, FileAST, Sym};
use crate::planner::{EndpointPlan, ExecutionPlan};
use dashmap::DashMap;
use lasso::Rodeo;
use prost_reflect::DescriptorPool;
use reqwest::Client;
use serde_json::Value as JsonValue;
use sqlx::AnyPool;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Once;
use std::sync::atomic::AtomicU64;
use std::time::Duration;
use tokio::sync::Semaphore;

#[derive(Clone, Debug)]
pub struct CommandOptions {
    pub enabled: bool,
    pub timeout: Duration,
    pub max_concurrency: usize,
    pub max_output_bytes: usize,
}

impl Default for CommandOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            timeout: Duration::from_secs(30),
            max_concurrency: 4,
            max_output_bytes: 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RateLimitOptions {
    pub trusted_proxies: Vec<ipnet::IpNet>,
    pub max_tracked_clients: usize,
    pub cleanup_interval: Duration,
}

impl Default for RateLimitOptions {
    fn default() -> Self {
        Self {
            trusted_proxies: Vec::new(),
            max_tracked_clients: 100_000,
            cleanup_interval: Duration::from_secs(60),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct RuntimeOptions {
    pub command: CommandOptions,
    pub rate_limit: RateLimitOptions,
    pub health_path: Option<String>,
    pub readiness_path: Option<String>,
    pub metrics_path: Option<String>,
}

#[derive(Default)]
pub(super) struct RuntimeMetrics {
    pub(super) requests: AtomicU64,
    pub(super) failures: AtomicU64,
    pub(super) rate_limited: AtomicU64,
    pub(super) commands_started: AtomicU64,
    pub(super) commands_rejected: AtomicU64,
}

#[derive(Clone)]
pub struct Runtime {
    pub(super) ast: Arc<FileAST>,
    pub(super) interner: Arc<Rodeo>,
    pub(super) plan: Arc<ExecutionPlan>,
    pub(super) client: Client,
    pub(super) db_urls: Arc<HashMap<String, String>>,
    pub(super) db_pools: Arc<DashMap<String, AnyPool>>,
    pub(super) proto_paths: Arc<HashMap<String, String>>,
    pub(super) proto_pools: Arc<DashMap<String, DescriptorPool>>,
    pub(super) options: Arc<RuntimeOptions>,
    pub(super) command_slots: Arc<Semaphore>,
    pub(super) metrics: Arc<RuntimeMetrics>,
}

#[derive(Debug)]
pub enum RuntimeError {
    InvalidMethod { method: String, path: String },
    InvalidBindAddress(String),
    Bind(std::io::Error),
    Serve(std::io::Error),
    BadRequest(String),
    Upstream(String),
    Timeout(String),
    Database(String),
    Grpc(String),
    Config(String),
    Execution(String),
    RouteConflict(String),
}

pub(super) type Vars = HashMap<Sym, JsonValue>;
pub(super) type Value = JsonValue;
pub(super) type RuntimeResult<T> = Result<T, RuntimeError>;
pub(super) static SQLX_DRIVERS: Once = Once::new();

pub(super) struct EndpointRuntime {
    pub(super) endpoint: Endpoint,
    pub(super) plan: EndpointPlan,
    pub(super) interner: Arc<Rodeo>,
    pub(super) client: Client,
    pub(super) static_vars: Vars,
    pub(super) db_urls: Arc<HashMap<String, String>>,
    pub(super) db_pools: Arc<DashMap<String, AnyPool>>,
    pub(super) proto_paths: Arc<HashMap<String, String>>,
    pub(super) proto_pools: Arc<DashMap<String, DescriptorPool>>,
    pub(super) options: Arc<RuntimeOptions>,
    pub(super) command_slots: Arc<Semaphore>,
    pub(super) metrics: Arc<RuntimeMetrics>,
}

pub(super) struct StepRuntimeDeps<'a> {
    pub(super) client: &'a Client,
    pub(super) db_urls: &'a HashMap<String, String>,
    pub(super) db_pools: &'a DashMap<String, AnyPool>,
    pub(super) proto_paths: &'a HashMap<String, String>,
    pub(super) proto_pools: &'a DashMap<String, DescriptorPool>,
    pub(super) options: &'a RuntimeOptions,
    pub(super) command_slots: &'a Semaphore,
    pub(super) metrics: &'a RuntimeMetrics,
}

pub(super) struct GrpcRequest {
    pub(super) method: String,
}

#[derive(Clone)]
pub(super) struct DynamicGrpcCodec {
    pub(super) output: prost_reflect::MessageDescriptor,
}

#[derive(Clone)]
pub(super) struct DynamicGrpcEncoder;

#[derive(Clone)]
pub(super) struct DynamicGrpcDecoder {
    pub(super) output: prost_reflect::MessageDescriptor,
    pub(super) consumed: bool,
}
