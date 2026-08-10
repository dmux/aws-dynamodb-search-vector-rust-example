//! The memory API, deployed as an AWS Lambda behind an API Gateway HTTP API.
//!
//! This binary is the **composition root**: the only place that knows about
//! every layer at once. It wires the AWS driven adapters into the domain
//! service and hands the result to the HTTP router.

mod error;
mod principal;
mod router;

use std::sync::Arc;

use agent_memory_aws::{BedrockEmbedder, DynamoDbMemoryRepository, SystemClock, UuidGenerator};
use agent_memory_core::LocalMemoryService;
use aws_config::BehaviorVersion;
use lambda_http::{Error, service_fn};

/// Read a required setting, failing at startup rather than on the first request.
fn env(name: &str) -> Result<String, Error> {
    std::env::var(name).map_err(|_| Error::from(format!("missing environment variable {name}")))
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    // CloudWatch already prefixes every line with a timestamp, and nothing here
    // renders ANSI colours.
    tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::INFO)
        .init();

    let table_name = env("TABLE_NAME")?;
    let index_name = env("VECTOR_INDEX_NAME")?;
    let model_id = env("EMBEDDING_MODEL_ID")?;
    let dimensions: usize = env("EMBEDDING_DIMENSIONS")?
        .parse()
        .map_err(|_| Error::from("EMBEDDING_DIMENSIONS must be a positive integer"))?;

    // Built once, outside the handler, so the connection pools and credential
    // cache survive across invocations instead of being rebuilt every time.
    //
    // Note there is no VPC configuration anywhere in this stack: SearchVectors
    // resolves to a dedicated endpoint (search-dynamodb.<region>.amazonaws.com)
    // distinct from the regular DynamoDB one. Inside a VPC that host would need
    // its own egress path, and the failure would show up only on search.
    let config = aws_config::load_defaults(BehaviorVersion::latest()).await;
    let service = Arc::new(LocalMemoryService::new(
        DynamoDbMemoryRepository::new(
            aws_sdk_dynamodb::Client::new(&config),
            table_name,
            index_name,
        ),
        BedrockEmbedder::new(
            aws_sdk_bedrockruntime::Client::new(&config),
            model_id,
            dimensions,
        ),
        SystemClock,
        UuidGenerator,
    ));

    lambda_http::run(service_fn(move |request| {
        let service = Arc::clone(&service);
        async move { Ok::<_, Error>(router::route(service.as_ref(), request).await) }
    }))
    .await
}
