//! End-to-end smoke test against a deployed stack.
//!
//! Written as an example rather than a shell script so it reuses the real SigV4
//! signing path — there is no separate `awscurl`-style implementation that
//! could disagree with what the MCP server actually sends.
//!
//! Run it with `make smoke` after `make deploy`.

use std::time::{Duration, Instant};

use agent_memory_client::RemoteMemoryService;
use agent_memory_core::{MemoryKind, MemoryService, RecallQuery, RememberCommand, TopK, UserId};

/// Search results are eventually consistent, so a write is not immediately
/// visible to a search.
const CONSISTENCY_TIMEOUT: Duration = Duration::from_secs(90);

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let service = RemoteMemoryService::from_env(Some("smoke-test".to_string()), None).await?;

    // The value below is deliberately never sent: the client omits the user id
    // entirely and the server derives it from the SigV4 principal.
    let placeholder = UserId::new("ignored-by-the-server")?;

    let text = format!(
        "I prefer pour-over coffee, no sugar (smoke test {})",
        std::process::id()
    );

    println!("storing a memory...");
    let stored = service
        .remember(RememberCommand {
            user_id: placeholder.clone(),
            kind: MemoryKind::Preference,
            text: text.clone(),
            // Expire on its own so a smoke run leaves nothing behind even if
            // the delete at the end never happens.
            ttl: Some(Duration::from_secs(3600)),
            source: None,
            github_login: None,
            github_repo: None,
            rating: None,
            active: true,
        })
        .await?;

    println!("  memory_id = {}", stored.memory_id);
    println!("  user_id   = {}", stored.user_id);

    // The point of the whole identity design: the namespace came back derived
    // from the signing principal, not from anything the client chose.
    anyhow::ensure!(
        stored.user_id != placeholder,
        "the server echoed back the client-supplied user id, which means the \
         namespace is NOT being derived from the IAM principal"
    );
    anyhow::ensure!(
        stored.user_id.as_str().contains(':'),
        "expected a namespace derived from a caller ARN, got {}",
        stored.user_id
    );
    println!("  -> namespace derived server-side from the IAM principal");

    println!("recalling by meaning (waiting for eventual consistency)...");
    let started = Instant::now();
    loop {
        let hits = service
            .recall(RecallQuery {
                user_id: placeholder.clone(),
                text: "what coffee do I like?".to_string(),
                top_k: TopK::new(5)?,
                kind: None,
                max_distance: None,
            })
            .await?;

        if let Some(hit) = hits
            .iter()
            .find(|hit| hit.memory.memory_id == stored.memory_id)
        {
            println!("  found after {:?}", started.elapsed());
            println!("  distance = {} (lower is closer)", hit.distance);
            println!("  text     = {}", hit.memory.text);

            anyhow::ensure!(
                hits.windows(2)
                    .all(|pair| pair[0].distance <= pair[1].distance),
                "results must come back closest-first"
            );
            break;
        }

        anyhow::ensure!(
            started.elapsed() < CONSISTENCY_TIMEOUT,
            "the memory never became searchable within {CONSISTENCY_TIMEOUT:?}"
        );
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    println!("cleaning up...");
    service.forget(&stored.user_id, &stored.memory_id).await?;

    println!("\nsmoke test passed");
    Ok(())
}
