//! The application service: the only implementation of [`MemoryService`] that
//! actually orchestrates use cases over the driven ports.
//!
//! It is generic over its ports rather than boxed, so composition happens at
//! compile time and the driving adapters pay no dynamic dispatch on the hot
//! `recall` path.

use async_trait::async_trait;

use crate::error::MemoryError;
use crate::model::{Memory, MemoryId, ScoredMemory, UserId};
use crate::ports::{
    Clock, EmbeddingProvider, IdGenerator, MemoryRepository, MemoryService, RecallQuery,
    RememberCommand, VectorQuery,
};

#[derive(Debug, Clone)]
pub struct LocalMemoryService<R, E, C, I> {
    repository: R,
    embedder: E,
    clock: C,
    ids: I,
}

impl<R, E, C, I> LocalMemoryService<R, E, C, I> {
    pub fn new(repository: R, embedder: E, clock: C, ids: I) -> Self {
        Self {
            repository,
            embedder,
            clock,
            ids,
        }
    }

    /// Borrow the ports, mainly so tests can assert on the fakes behind them.
    pub fn repository(&self) -> &R {
        &self.repository
    }

    pub fn embedder(&self) -> &E {
        &self.embedder
    }

    pub fn clock(&self) -> &C {
        &self.clock
    }

    pub fn ids(&self) -> &I {
        &self.ids
    }
}

impl<R, E, C, I> LocalMemoryService<R, E, C, I>
where
    E: EmbeddingProvider,
{
    /// Embed non-empty text and check the result against the configured
    /// dimension.
    ///
    /// The dimension check is deliberately redundant with the provider's own
    /// validation: a mismatch that slips through surfaces as an opaque
    /// DynamoDB rejection on write, or as silently meaningless rankings on
    /// search, and both are far harder to diagnose than this error.
    async fn embed_checked(&self, text: &str) -> Result<crate::model::Embedding, MemoryError> {
        if text.trim().is_empty() {
            return Err(MemoryError::EmptyText);
        }
        let embedding = self.embedder.embed(text).await?;
        let expected = self.embedder.dimensions();
        if embedding.dimensions() != expected {
            return Err(MemoryError::DimensionMismatch {
                expected,
                actual: embedding.dimensions(),
            });
        }
        Ok(embedding)
    }
}

#[async_trait]
impl<R, E, C, I> MemoryService for LocalMemoryService<R, E, C, I>
where
    R: MemoryRepository,
    E: EmbeddingProvider,
    C: Clock,
    I: IdGenerator,
{
    async fn remember(&self, command: RememberCommand) -> Result<Memory, MemoryError> {
        // Always re-embed from the text being stored. DynamoDB never recomputes
        // embeddings, so any path that writes text without a matching vector
        // would leave the index answering from stale meaning. Making this the
        // only way to store a memory removes that possibility.
        let embedding = self.embed_checked(&command.text).await?;

        let created_at = self.clock.now();
        let memory = Memory {
            user_id: command.user_id,
            memory_id: self.ids.next_id(),
            kind: command.kind,
            text: command.text,
            created_at,
            expires_at: command.ttl.map(|ttl| created_at + ttl),
            source: command.source,
            github_login: command.github_login,
            github_repo: command.github_repo,
            rating: command.rating,
            active: command.active,
        };

        self.repository.save(&memory, &embedding).await?;
        Ok(memory)
    }

    async fn recall(&self, query: RecallQuery) -> Result<Vec<ScoredMemory>, MemoryError> {
        let embedding = self.embed_checked(&query.text).await?;

        let mut hits = self
            .repository
            .search(&VectorQuery {
                user_id: query.user_id,
                embedding,
                top_k: query.top_k,
                kind: query.kind,
            })
            .await?;

        // Adapters are asked to return results best-first, but sorting here
        // makes the ordering a property of the domain rather than a promise
        // each adapter has to keep. At top_k <= 100 it costs nothing.
        hits.sort_by_key(|hit| hit.distance);

        // `SearchVectors` has no relevance threshold, so the cutoff is applied
        // client-side. Distance is "lower is better", hence `<=`.
        if let Some(max_distance) = query.max_distance {
            hits.retain(|hit| hit.distance <= max_distance);
        }

        hits.truncate(query.top_k.get() as usize);
        Ok(hits)
    }

    async fn forget(&self, user_id: &UserId, memory_id: &MemoryId) -> Result<(), MemoryError> {
        self.repository.delete(user_id, memory_id).await
    }

    /// Straight through: listing needs no embedding, so there is nothing for
    /// this layer to add beyond the port it already forwards.
    async fn list(
        &self,
        user_id: &UserId,
        query: &crate::ports::ListQuery,
    ) -> Result<crate::ports::MemoryPage, MemoryError> {
        self.repository.list(user_id, query).await
    }

    async fn update(&self, command: crate::ports::UpdateCommand) -> Result<Memory, MemoryError> {
        let mut new_embedding = None;
        if let Some(ref text) = command.text {
            new_embedding = Some(self.embed_checked(text).await?);
        }
        self.repository
            .update(command, new_embedding.as_ref())
            .await
    }

    async fn get(
        &self,
        user_id: &UserId,
        memory_id: &MemoryId,
    ) -> Result<Option<Memory>, MemoryError> {
        self.repository.get(user_id, memory_id).await
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::model::{MemoryKind, TopK};
    use crate::ports::{ListQuery, MemoryFilter, PageSize};
    use crate::testing::{FixedClock, InMemoryRepository, SeqIdGenerator, StubEmbedder};

    const DIMENSIONS: usize = 64;

    fn service() -> LocalMemoryService<InMemoryRepository, StubEmbedder, FixedClock, SeqIdGenerator>
    {
        LocalMemoryService::new(
            InMemoryRepository::default(),
            StubEmbedder::new(DIMENSIONS),
            FixedClock::new(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
            SeqIdGenerator::default(),
        )
    }

    fn user(name: &str) -> UserId {
        UserId::new(name).expect("valid user id")
    }

    fn remember(text: &str, kind: MemoryKind, owner: &str) -> RememberCommand {
        RememberCommand {
            user_id: user(owner),
            kind,
            text: text.to_string(),
            ttl: None,
            source: None,
            github_login: None,
            github_repo: None,
            rating: None,
            active: true,
        }
    }

    fn recall(text: &str, owner: &str) -> RecallQuery {
        RecallQuery {
            user_id: user(owner),
            text: text.to_string(),
            top_k: TopK::DEFAULT,
            kind: None,
            max_distance: None,
        }
    }

    #[tokio::test]
    async fn remember_derives_expiry_from_the_clock_and_ttl() {
        let service = service();
        let now = service.clock.now();

        let stored = service
            .remember(RememberCommand {
                ttl: Some(Duration::from_secs(3600)),
                ..remember("I drink pour-over coffee", MemoryKind::Preference, "alice")
            })
            .await
            .expect("remember succeeds");

        assert_eq!(stored.created_at, now);
        assert_eq!(stored.expires_at, Some(now + Duration::from_secs(3600)));
    }

    #[tokio::test]
    async fn remember_without_ttl_never_expires() {
        let service = service();
        let stored = service
            .remember(remember("I live in Lisbon", MemoryKind::Fact, "alice"))
            .await
            .expect("remember succeeds");
        assert_eq!(stored.expires_at, None);
    }

    #[tokio::test]
    async fn remember_rejects_blank_text_before_calling_the_embedder() {
        let service = service();
        let error = service
            .remember(remember("   ", MemoryKind::Fact, "alice"))
            .await
            .expect_err("blank text is rejected");

        assert!(matches!(error, MemoryError::EmptyText));
        assert_eq!(
            service.embedder.calls(),
            0,
            "an embedding call costs money, so validation must come first"
        );
    }

    #[tokio::test]
    async fn recall_returns_the_closest_memory_first() {
        let service = service();
        for (text, kind) in [
            (
                "I drink pour-over coffee every morning",
                MemoryKind::Preference,
            ),
            ("My dog is called Bilu", MemoryKind::Fact),
            (
                "I deployed the billing service on Friday",
                MemoryKind::Episode,
            ),
        ] {
            service
                .remember(remember(text, kind, "alice"))
                .await
                .expect("remember succeeds");
        }

        let hits = service
            .recall(recall("coffee", "alice"))
            .await
            .expect("recall succeeds");

        assert!(!hits.is_empty());
        assert!(
            hits[0].memory.text.contains("coffee"),
            "expected the coffee memory first, got {:?}",
            hits[0].memory.text
        );
        assert!(
            hits.windows(2)
                .all(|pair| pair[0].distance <= pair[1].distance),
            "results must be ordered best-first"
        );
    }

    #[tokio::test]
    async fn recall_never_crosses_the_user_boundary() {
        let service = service();
        service
            .remember(remember(
                "The launch codes are 1234",
                MemoryKind::Fact,
                "alice",
            ))
            .await
            .expect("remember succeeds");

        let hits = service
            .recall(recall("launch codes", "mallory"))
            .await
            .expect("recall succeeds");

        assert!(
            hits.is_empty(),
            "another user's memories must never surface"
        );
    }

    #[tokio::test]
    async fn recall_can_narrow_to_a_single_kind() {
        let service = service();
        service
            .remember(remember(
                "I prefer dark roast",
                MemoryKind::Preference,
                "alice",
            ))
            .await
            .expect("remember succeeds");
        service
            .remember(remember(
                "I bought dark roast beans",
                MemoryKind::Episode,
                "alice",
            ))
            .await
            .expect("remember succeeds");

        let hits = service
            .recall(RecallQuery {
                kind: Some(MemoryKind::Preference),
                ..recall("dark roast", "alice")
            })
            .await
            .expect("recall succeeds");

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].memory.kind, MemoryKind::Preference);
    }

    #[tokio::test]
    async fn max_distance_is_an_upper_bound_not_a_threshold() {
        // The whole point of the Distance newtype: filtering keeps the *near*
        // matches. Getting this backwards would return exactly the junk the
        // caller asked to exclude.
        let service = service();
        service
            .remember(remember(
                "espresso machine descaling",
                MemoryKind::Fact,
                "alice",
            ))
            .await
            .expect("remember succeeds");
        service
            .remember(remember(
                "kubernetes ingress controller",
                MemoryKind::Fact,
                "alice",
            ))
            .await
            .expect("remember succeeds");

        let unfiltered = service
            .recall(recall("espresso machine descaling", "alice"))
            .await
            .expect("recall succeeds");
        assert_eq!(unfiltered.len(), 2);

        let cutoff = unfiltered[0].distance;
        let filtered = service
            .recall(RecallQuery {
                max_distance: Some(cutoff),
                ..recall("espresso machine descaling", "alice")
            })
            .await
            .expect("recall succeeds");

        assert_eq!(filtered.len(), 1);
        assert!(filtered[0].memory.text.contains("espresso"));
    }

    #[tokio::test]
    async fn recall_honours_top_k() {
        let service = service();
        for index in 0..10 {
            service
                .remember(remember(
                    &format!("memory number {index}"),
                    MemoryKind::Fact,
                    "alice",
                ))
                .await
                .expect("remember succeeds");
        }

        let hits = service
            .recall(RecallQuery {
                top_k: TopK::new(3).expect("valid top_k"),
                ..recall("memory number", "alice")
            })
            .await
            .expect("recall succeeds");

        assert_eq!(hits.len(), 3);
    }

    #[tokio::test]
    async fn forget_removes_the_memory_from_later_recalls() {
        let service = service();
        let stored = service
            .remember(remember("temporary note", MemoryKind::Fact, "alice"))
            .await
            .expect("remember succeeds");

        service
            .forget(&stored.user_id, &stored.memory_id)
            .await
            .expect("forget succeeds");

        assert!(
            service
                .get(&stored.user_id, &stored.memory_id)
                .await
                .expect("get succeeds")
                .is_none()
        );
        assert!(
            service
                .recall(recall("temporary note", "alice"))
                .await
                .expect("recall succeeds")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn get_is_scoped_to_the_owning_user() {
        let service = service();
        let stored = service
            .remember(remember("private", MemoryKind::Fact, "alice"))
            .await
            .expect("remember succeeds");

        let leaked = service
            .get(&user("mallory"), &stored.memory_id)
            .await
            .expect("get succeeds");

        assert!(leaked.is_none());
    }

    #[tokio::test]
    async fn a_dimension_mismatch_is_reported_instead_of_reaching_the_repository() {
        let service = LocalMemoryService::new(
            InMemoryRepository::default(),
            // Claims 1024 but produces 64: exactly the misconfiguration that
            // otherwise fails deep inside DynamoDB.
            StubEmbedder::new(DIMENSIONS).claiming_dimensions(1024),
            FixedClock::new(SystemTime::UNIX_EPOCH),
            SeqIdGenerator::default(),
        );

        let error = service
            .remember(remember("anything", MemoryKind::Fact, "alice"))
            .await
            .expect_err("mismatch is rejected");

        assert!(matches!(
            error,
            MemoryError::DimensionMismatch {
                expected: 1024,
                actual: 64
            }
        ));
        assert_eq!(service.repository.len(), 0);
    }

    /// Walk every page the way an infinite scroll does, and report what it saw.
    async fn drain(
        service: &LocalMemoryService<InMemoryRepository, StubEmbedder, FixedClock, SeqIdGenerator>,
        owner: &str,
        limit: u32,
        filter: MemoryFilter,
    ) -> Vec<String> {
        let mut seen = Vec::new();
        let mut cursor = None;
        // The stopping rule is `next == None`, never "the page was short".
        loop {
            let page = service
                .list(
                    &user(owner),
                    &ListQuery {
                        limit: PageSize::new(limit).expect("a valid limit"),
                        cursor,
                        filter: filter.clone(),
                    },
                )
                .await
                .expect("listed");

            seen.extend(page.memories.iter().map(|memory| memory.text.clone()));

            match page.next {
                Some(next) => cursor = Some(next),
                None => break,
            }
            assert!(seen.len() < 100, "the cursor is not advancing");
        }
        seen
    }

    #[tokio::test]
    async fn paging_visits_every_memory_exactly_once() {
        let service = service();
        for index in 0..7 {
            service
                .remember(remember(
                    &format!("memória {index}"),
                    MemoryKind::Fact,
                    "rafa",
                ))
                .await
                .expect("remembered");
        }

        let seen = drain(&service, "rafa", 2, MemoryFilter::default()).await;

        assert_eq!(seen.len(), 7, "no page was skipped or repeated");
        let unique: std::collections::HashSet<_> = seen.iter().collect();
        assert_eq!(unique.len(), 7);
    }

    #[tokio::test]
    async fn a_filtered_page_can_be_empty_while_more_rows_remain() {
        // The invariant the whole cursor contract exists for. The filter runs
        // after the page is taken, so a caller that stops on a short page stops
        // on the first one that filtered everything out — and shows an empty
        // library over a full namespace.
        let service = service();
        for index in 0..6 {
            service
                .remember(remember(
                    &format!("comum {index}"),
                    MemoryKind::Fact,
                    "rafa",
                ))
                .await
                .expect("remembered");
        }
        service
            .remember(remember("o raro", MemoryKind::Preference, "rafa"))
            .await
            .expect("remembered");

        let wanted = MemoryFilter {
            kind: Some(MemoryKind::Preference),
            ..Default::default()
        };

        let first = service
            .list(
                &user("rafa"),
                &ListQuery {
                    limit: PageSize::new(2).expect("a valid limit"),
                    cursor: None,
                    filter: wanted.clone(),
                },
            )
            .await
            .expect("listed");
        assert!(
            first.memories.is_empty() && first.next.is_some(),
            "an empty page with a cursor is not the end"
        );

        assert_eq!(
            drain(&service, "rafa", 2, wanted).await,
            vec!["o raro".to_string()],
            "walking to the end still finds it"
        );
    }

    #[tokio::test]
    async fn filters_narrow_by_metadata_and_never_by_meaning() {
        let service = service();

        let mut from_repo = remember("usamos DynamoDB", MemoryKind::Fact, "rafa");
        from_repo.github_repo = Some("dmux/EchoBrain".to_string());
        from_repo.rating = Some(5);
        service.remember(from_repo).await.expect("remembered");

        let mut elsewhere = remember("usamos DynamoDB", MemoryKind::Fact, "rafa");
        elsewhere.github_repo = Some("outro/repo".to_string());
        service.remember(elsewhere).await.expect("remembered");

        let by_repo = drain(
            &service,
            "rafa",
            10,
            MemoryFilter {
                github_repo: Some("dmux/EchoBrain".to_string()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(by_repo.len(), 1, "same text, different repository");

        let rated = drain(
            &service,
            "rafa",
            10,
            MemoryFilter {
                min_rating: Some(3),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(rated.len(), 1, "an unrated memory is not a badly rated one");
    }

    #[tokio::test]
    async fn a_page_size_outside_the_bounds_is_rejected_before_any_read() {
        assert!(PageSize::new(0).is_err());
        assert!(PageSize::new(PageSize::MAX + 1).is_err());
        assert!(PageSize::new(PageSize::MAX).is_ok());
    }
}
