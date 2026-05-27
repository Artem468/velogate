use super::RuntimeResult;
use super::types::{
    DynamicGrpcCodec, DynamicGrpcDecoder, DynamicGrpcEncoder, GrpcRequest, RuntimeError,
};
use prost::Message;
use prost_reflect::{DynamicMessage, MessageDescriptor};
use std::fmt;
use tonic::Status;
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};

impl GrpcRequest {
    pub(super) fn parse(raw: &str) -> RuntimeResult<Self> {
        let rest = if let Some(rest) = raw.strip_prefix("http://") {
            rest
        } else if let Some(rest) = raw.strip_prefix("https://") {
            rest
        } else {
            raw
        };

        let Some((target, method)) = rest.split_once('/') else {
            return Err(RuntimeError::Execution(
                "grpc method must look like `host:port/package.Service/Method`".to_string(),
            ));
        };

        if target.is_empty() || method.is_empty() {
            return Err(RuntimeError::Execution(
                "grpc target and method must not be empty".to_string(),
            ));
        }

        Ok(Self {
            method: method.to_string(),
        })
    }
}

impl DynamicGrpcCodec {
    pub(super) fn new(output: MessageDescriptor) -> Self {
        Self { output }
    }
}

impl Codec for DynamicGrpcCodec {
    type Encode = DynamicMessage;
    type Decode = DynamicMessage;
    type Encoder = DynamicGrpcEncoder;
    type Decoder = DynamicGrpcDecoder;

    fn encoder(&mut self) -> Self::Encoder {
        DynamicGrpcEncoder
    }

    fn decoder(&mut self) -> Self::Decoder {
        DynamicGrpcDecoder {
            output: self.output.clone(),
            consumed: false,
        }
    }
}

impl Encoder for DynamicGrpcEncoder {
    type Item = DynamicMessage;
    type Error = Status;

    fn encode(&mut self, item: Self::Item, dst: &mut EncodeBuf<'_>) -> Result<(), Self::Error> {
        item.encode(dst).map_err(|err| {
            Status::internal(format!("failed to encode dynamic grpc request: {err}"))
        })
    }
}

impl Decoder for DynamicGrpcDecoder {
    type Item = DynamicMessage;
    type Error = Status;

    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<Self::Item>, Self::Error> {
        if self.consumed {
            return Ok(None);
        }
        self.consumed = true;
        DynamicMessage::decode(self.output.clone(), src)
            .map(Some)
            .map_err(|err| {
                Status::internal(format!("failed to decode dynamic grpc response: {err}"))
            })
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMethod { method, path } => {
                write!(f, "unsupported HTTP method `{method}` for `{path}`")
            }
            Self::InvalidBindAddress(addr) => write!(f, "invalid bind address `{addr}`"),
            Self::Bind(err) => write!(f, "failed to bind listener: {err}"),
            Self::Serve(err) => write!(f, "server failed: {err}"),
            Self::Execution(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for RuntimeError {}
