use crate::ast::{Endpoint, FileAST, Sym};
use crate::planner::{EndpointPlan, ExecutionPlan};
use lasso::Rodeo;
use prost_reflect::DescriptorPool;
use reqwest::Client;
use serde_json::Value as JsonValue;
use sqlx::AnyPool;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Once;
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct Runtime {
    pub(super) ast: Arc<FileAST>,
    pub(super) interner: Arc<Rodeo>,
    pub(super) plan: Arc<ExecutionPlan>,
    pub(super) client: Client,
    pub(super) db_urls: Arc<HashMap<String, String>>,
    pub(super) db_pools: Arc<Mutex<HashMap<String, AnyPool>>>,
    pub(super) proto_paths: Arc<HashMap<String, String>>,
    pub(super) proto_pools: Arc<Mutex<HashMap<String, DescriptorPool>>>,
}

#[derive(Debug)]
pub enum RuntimeError {
    InvalidMethod { method: String, path: String },
    InvalidBindAddress(String),
    Bind(std::io::Error),
    Serve(std::io::Error),
    Execution(String),
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
    pub(super) db_pools: Arc<Mutex<HashMap<String, AnyPool>>>,
    pub(super) proto_paths: Arc<HashMap<String, String>>,
    pub(super) proto_pools: Arc<Mutex<HashMap<String, DescriptorPool>>>,
}

pub(super) struct StepRuntimeDeps<'a> {
    pub(super) client: &'a Client,
    pub(super) db_urls: &'a HashMap<String, String>,
    pub(super) db_pools: &'a Mutex<HashMap<String, AnyPool>>,
    pub(super) proto_paths: &'a HashMap<String, String>,
    pub(super) proto_pools: &'a Mutex<HashMap<String, DescriptorPool>>,
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
