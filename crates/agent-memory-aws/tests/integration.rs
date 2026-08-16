//! Integration tests against real AWS.
//!
//! These are `#[ignore]`d and additionally gated on `DDB_VECTOR_IT=1`, because
//! they create a DynamoDB table with a vector index and call Bedrock. Run them
//! with `make test-it`.
//!
//! There is no local substitute. `SearchVectors` resolves to a dedicated
//! endpoint (`search-dynamodb.<region>.amazonaws.com`) rather than the standard
//! DynamoDB one, so DynamoDB Local cannot serve these paths, and overriding
//! `endpoint_url` breaks search routing instead of helping.

use std::time::{Duration, SystemTime};

use agent_memory_aws::admin::{self, VectorIndexSpec};
use agent_memory_aws::{BedrockEmbedder, DynamoDbMemoryRepository, SystemClock, UuidGenerator};
use agent_memory_core::{
    Embedding, EmbeddingProvider, LocalMemoryService, MemoryKind, MemoryService, RecallQuery,
    RememberCommand, ScoredMemory, TopK, UserId,
};

const DIMENSIONS: usize = 1024;
const INDEX_NAME: &str = "MemoryIndex";
/// Index creation plus backfill is not fast.
const READY_TIMEOUT: Duration = Duration::from_secs(600);
/// Search results are eventually consistent, so writes need time to appear.
const CONSISTENCY_TIMEOUT: Duration = Duration::from_secs(90);

fn enabled() -> bool {
    std::env::var("DDB_VECTOR_IT").as_deref() == Ok("1")
}

fn unique_table_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    format!("agent-memory-it-{nanos}")
}

struct Fixture {
    dynamodb: aws_sdk_dynamodb::Client,
    table_name: String,
}

impl Fixture {
    /// Create a table with its vector index and wait until it will answer a
    /// search.
    async fn create() -> Self {
        let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let dynamodb = aws_sdk_dynamodb::Client::new(&config);
        let table_name = unique_table_name();
        let spec = VectorIndexSpec::new(INDEX_NAME, DIMENSIONS as i64);

        admin::create_table_with_vector_index(&dynamodb, &table_name, &spec)
            .await
            .expect("create table with vector index");
        admin::wait_until_searchable(&dynamodb, &table_name, INDEX_NAME, READY_TIMEOUT)
            .await
            .expect("vector index becomes searchable");
        admin::enable_ttl(&dynamodb, &table_name)
            .await
            .expect("enable ttl");

        Self {
            dynamodb,
            table_name,
        }
    }

    fn repository(&self) -> DynamoDbMemoryRepository {
        DynamoDbMemoryRepository::new(self.dynamodb.clone(), &self.table_name, INDEX_NAME)
    }

    async fn cleanup(self) {
        if let Err(error) = admin::delete_table(&self.dynamodb, &self.table_name).await {
            eprintln!("WARNING: leaked table {}: {error}", self.table_name);
        }
    }
}

async fn embedder() -> BedrockEmbedder {
    let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    BedrockEmbedder::titan_v2(aws_sdk_bedrockruntime::Client::new(&config), DIMENSIONS)
}

fn user(name: &str) -> UserId {
    UserId::new(name).expect("valid user id")
}

/// Retry a search until it satisfies `predicate`.
///
/// Vector search is eventually consistent: asserting immediately after a write
/// is a guaranteed flake, not an occasional one.
async fn recall_until<S, F>(
    service: &S,
    query: impl Fn() -> RecallQuery,
    predicate: F,
) -> Vec<ScoredMemory>
where
    S: MemoryService,
    F: Fn(&[ScoredMemory]) -> bool,
{
    let deadline = SystemTime::now() + CONSISTENCY_TIMEOUT;
    let mut last = Vec::new();
    while SystemTime::now() < deadline {
        last = service.recall(query()).await.expect("recall succeeds");
        if predicate(&last) {
            return last;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    panic!("condition not met within {CONSISTENCY_TIMEOUT:?}; last results: {last:?}");
}

#[tokio::test]
#[ignore = "creates real AWS resources and calls Bedrock"]
async fn the_full_memory_lifecycle_works_against_real_aws() {
    if !enabled() {
        eprintln!("skipping: set DDB_VECTOR_IT=1 to run");
        return;
    }

    let fixture = Fixture::create().await;
    let service = LocalMemoryService::new(
        fixture.repository(),
        embedder().await,
        SystemClock,
        UuidGenerator,
    );

    let store = |owner: &'static str, text: &'static str, kind: MemoryKind| {
        let service = &service;
        async move {
            service
                .remember(RememberCommand {
                    user_id: user(owner),
                    kind,
                    text: text.to_string(),
                    ttl: None,
                    source: None,
                    github_login: None,
                    github_repo: None,
                    rating: None,
                    active: true,
                })
                .await
                .expect("remember succeeds")
        }
    };

    let coffee = store(
        "alice",
        "I only drink pour-over coffee, never espresso",
        MemoryKind::Preference,
    )
    .await;
    store(
        "alice",
        "My deployment pipeline runs on Friday afternoons",
        MemoryKind::Episode,
    )
    .await;
    store(
        "bob",
        "I take my coffee with three sugars",
        MemoryKind::Preference,
    )
    .await;

    let query = || RecallQuery {
        user_id: user("alice"),
        text: "how do I like my coffee?".to_string(),
        top_k: TopK::new(5).expect("valid"),
        kind: None,
        max_distance: None,
    };

    // Semantic relevance: the query shares almost no words with the stored
    // text, so a keyword search would miss it entirely.
    let hits = recall_until(&service, query, |hits| !hits.is_empty()).await;
    assert_eq!(
        hits[0].memory.memory_id, coffee.memory_id,
        "the coffee preference should rank first, got {:?}",
        hits[0].memory.text
    );

    // Tenant isolation: bob's coffee memory is at least as relevant to this
    // query, so its absence is evidence the partition key is doing its job.
    assert!(
        hits.iter().all(|hit| hit.memory.user_id == user("alice")),
        "another user's memories must never appear"
    );

    // Inline filter, applied at the storage layer.
    let filtered = service
        .recall(RecallQuery {
            kind: Some(MemoryKind::Episode),
            ..query()
        })
        .await
        .expect("filtered recall succeeds");
    assert!(
        filtered
            .iter()
            .all(|hit| hit.memory.kind == MemoryKind::Episode),
        "the inline filter must exclude every other kind"
    );

    // Ordering is ascending because the index is COSINE.
    assert!(
        hits.windows(2)
            .all(|pair| pair[0].distance <= pair[1].distance),
        "results must come back best-first"
    );

    // Forget removes the item from the index too.
    service
        .forget(&coffee.user_id, &coffee.memory_id)
        .await
        .expect("forget succeeds");
    let remaining = recall_until(&service, query, |hits| {
        hits.iter()
            .all(|hit| hit.memory.memory_id != coffee.memory_id)
    })
    .await;
    assert!(
        remaining
            .iter()
            .all(|hit| hit.memory.memory_id != coffee.memory_id)
    );

    fixture.cleanup().await;
}

/// Answers the one open question the plan flagged: `UpdateTable` documents
/// `AttributeDefinitions` in terms of global secondary indexes, and this is the
/// path Terraform must use because no provider supports vector indexes. If this
/// test fails, the Terraform escape hatch has to create the whole table instead
/// of adding the index to an existing one.
#[tokio::test]
#[ignore = "creates real AWS resources"]
async fn adding_a_vector_index_can_register_its_search_schema_attribute() {
    if !enabled() {
        eprintln!("skipping: set DDB_VECTOR_IT=1 to run");
        return;
    }

    // The waiter methods live behind a trait.
    use aws_sdk_dynamodb::client::Waiters as _;
    use aws_sdk_dynamodb::types::{
        AttributeDefinition, BillingMode, KeySchemaElement, KeyType, ScalarAttributeType,
    };

    let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let dynamodb = aws_sdk_dynamodb::Client::new(&config);
    let table_name = unique_table_name();

    let attribute = |name: &str| {
        AttributeDefinition::builder()
            .attribute_name(name)
            .attribute_type(ScalarAttributeType::S)
            .build()
            .expect("builds")
    };
    let key = |name: &str, key_type: KeyType| {
        KeySchemaElement::builder()
            .attribute_name(name)
            .key_type(key_type)
            .build()
            .expect("builds")
    };

    // Deliberately created WITHOUT `kind`, exactly as the Terraform
    // `aws_dynamodb_table` resource must be: the provider rejects an attribute
    // that no key or index uses.
    dynamodb
        .create_table()
        .table_name(&table_name)
        .billing_mode(BillingMode::PayPerRequest)
        .attribute_definitions(attribute("user_id"))
        .attribute_definitions(attribute("memory_id"))
        .key_schema(key("user_id", KeyType::Hash))
        .key_schema(key("memory_id", KeyType::Range))
        .send()
        .await
        .expect("create table");

    dynamodb
        .wait_until_table_exists()
        .table_name(&table_name)
        .wait(READY_TIMEOUT)
        .await
        .expect("table becomes active");

    let spec = VectorIndexSpec::new(INDEX_NAME, DIMENSIONS as i64);
    let result = admin::add_vector_index(&dynamodb, &table_name, &spec).await;

    // Clean up before asserting, so a failure does not leak a table.
    let outcome = match result {
        Ok(()) => {
            let ready =
                admin::wait_until_searchable(&dynamodb, &table_name, INDEX_NAME, READY_TIMEOUT)
                    .await;
            admin::delete_vector_index(&dynamodb, &table_name, INDEX_NAME)
                .await
                .ok();
            ready.map_err(|error| error.to_string())
        }
        Err(error) => Err(error.to_string()),
    };
    let _ = admin::delete_table(&dynamodb, &table_name).await;

    outcome.expect(
        "UpdateTable must accept AttributeDefinitions alongside VectorIndexUpdates; \
         if it does not, the Terraform escape hatch has to create the whole table",
    );
}

#[tokio::test]
#[ignore = "calls Bedrock"]
async fn titan_returns_a_vector_of_the_configured_dimension() {
    if !enabled() {
        eprintln!("skipping: set DDB_VECTOR_IT=1 to run");
        return;
    }

    let embedder = embedder().await;
    let embedding: Embedding = embedder
        .embed("I prefer pour-over coffee")
        .await
        .expect("embed succeeds");

    assert_eq!(embedding.dimensions(), DIMENSIONS);
    assert!(
        embedding.as_slice().iter().all(|value| value.is_finite()),
        "DynamoDB cannot store NaN or infinity in an N attribute"
    );

    // `normalize: true` was requested, so the vector should be unit length.
    let norm = embedding
        .as_slice()
        .iter()
        .map(|value| value * value)
        .sum::<f32>()
        .sqrt();
    assert!(
        (norm - 1.0).abs() < 0.01,
        "expected a unit-length vector, got norm {norm}"
    );
}
