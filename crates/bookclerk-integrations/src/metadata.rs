//! The integration registry as the host's set of metadata providers.
//!
//! Every registered integration that exports the `metadata` entrypoint is a
//! provider. Calls go to each one in registration order; one that fails is
//! reported in the answer and does not stop the rest. Each call is bounded by
//! [`PROVIDER_CALL_TIMEOUT`], so a provider that hangs is reported as failed
//! and the next one is still asked.

use std::future::Future;
use std::time::Duration;

use async_trait::async_trait;
use bookclerk_enrich::{
    ChaptersAnswer, MetadataAnswer, MetadataProviders, ProvidedChapters, ProvidedRecord,
    ProviderFailure,
};
use bookclerk_plugin_abi::{MetadataChaptersParams, MetadataLookupParams, MetadataSearchParams};
use tracing::warn;

use crate::error::{IntegrationError, Result};
use crate::registry::IntegrationRegistry;
use crate::traits::Integration;

/// Longest one metadata provider call may take before the host gives up on
/// it and asks the next provider. Covers the plugin's own outbound requests
/// (Open Library can take several seconds) with room to spare.
pub const PROVIDER_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Runs one provider call, failing it once [`PROVIDER_CALL_TIMEOUT`] passes.
///
/// # Arguments
///
/// * `call` - The provider call.
///
/// # Errors
///
/// The call's own error, or a timeout error when it did not finish in time.
async fn bounded<T>(call: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(PROVIDER_CALL_TIMEOUT, call)
        .await
        .unwrap_or_else(|_| {
            Err(IntegrationError::message(format!(
                "no answer within {} s",
                PROVIDER_CALL_TIMEOUT.as_secs()
            )))
        })
}

/// Records a failed provider call in `failures` and logs it.
fn failed(
    failures: &mut Vec<ProviderFailure>,
    integration: &dyn Integration,
    err: &dyn std::fmt::Display,
    method: &str,
) {
    warn!(
        plugin = integration.plugin_key(),
        method,
        error = %err,
        "metadata provider call failed"
    );
    failures.push(ProviderFailure {
        provider: integration.plugin_key().to_string(),
        message: err.to_string(),
    });
}

#[async_trait]
impl MetadataProviders for IntegrationRegistry {
    async fn lookup(&self, params: &MetadataLookupParams) -> MetadataAnswer {
        let mut answer = MetadataAnswer::default();
        for integration in self.metadata_providers() {
            match bounded(integration.metadata_lookup(params.clone())).await {
                Ok(found) => {
                    answer
                        .records
                        .extend(found.into_iter().map(|record| ProvidedRecord {
                            provider: integration.plugin_key().to_string(),
                            record,
                        }))
                }
                Err(err) => failed(&mut answer.failures, integration.as_ref(), &err, "lookup"),
            }
        }
        answer
    }

    async fn search(&self, params: &MetadataSearchParams) -> MetadataAnswer {
        let mut answer = MetadataAnswer::default();
        for integration in self.metadata_providers() {
            match bounded(integration.metadata_search(params.clone())).await {
                Ok(found) => {
                    answer
                        .records
                        .extend(found.into_iter().map(|record| ProvidedRecord {
                            provider: integration.plugin_key().to_string(),
                            record,
                        }))
                }
                Err(err) => failed(&mut answer.failures, integration.as_ref(), &err, "search"),
            }
        }
        answer
    }

    async fn chapters(&self, params: &MetadataChaptersParams) -> ChaptersAnswer {
        let mut answer = ChaptersAnswer::default();
        for integration in self.metadata_providers() {
            match bounded(integration.metadata_chapters(params.clone())).await {
                Ok(layout) if layout.found => {
                    answer.found = Some(ProvidedChapters {
                        provider: integration.plugin_key().to_string(),
                        chapters: layout,
                    });
                    break;
                }
                Ok(_) => {}
                Err(err) => failed(&mut answer.failures, integration.as_ref(), &err, "chapters"),
            }
        }
        answer
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use bookclerk_enrich::MetadataProviders;
    use bookclerk_plugin_abi::{
        DomainEvent, EventResult, MetadataChapter, MetadataChapters, MetadataChaptersParams,
        MetadataLookupParams, MetadataRecord, MetadataSearchParams,
    };

    use crate::error::{IntegrationError, Result};
    use crate::registry::IntegrationRegistry;
    use crate::traits::{Integration, IntegrationContext};
    use crate::types::IntegrationHealth;

    /// How a fake provider answers every call.
    #[derive(Clone, Copy)]
    enum Mode {
        /// Not a metadata provider at all.
        NotProvider,
        /// Answers with one record titled after the key, and a layout.
        Answers,
        /// Answers with nothing and no layout.
        Empty,
        /// Fails every call.
        Fails,
        /// Never answers.
        Hangs,
    }

    struct Fake {
        key: &'static str,
        mode: Mode,
    }

    impl Fake {
        fn records(&self) -> Result<Vec<MetadataRecord>> {
            match self.mode {
                Mode::Answers => Ok(vec![MetadataRecord {
                    title: self.key.into(),
                    ..MetadataRecord::default()
                }]),
                Mode::Empty | Mode::NotProvider => Ok(Vec::new()),
                Mode::Fails => Err(IntegrationError::message("boom")),
                Mode::Hangs => unreachable!("hanging fakes never get here"),
            }
        }

        /// Waits forever when this fake hangs.
        async fn maybe_hang(&self) {
            if matches!(self.mode, Mode::Hangs) {
                std::future::pending::<()>().await;
            }
        }
    }

    #[async_trait]
    impl Integration for Fake {
        fn id(&self) -> &str {
            self.key
        }

        async fn start(&self, _ctx: IntegrationContext) -> Result<()> {
            Ok(())
        }

        async fn deliver_domain_event(&self, _event: DomainEvent) -> Result<EventResult> {
            Ok(EventResult::Ack)
        }

        async fn health(&self) -> Result<IntegrationHealth> {
            Ok(IntegrationHealth {
                id: self.key.into(),
                enabled: true,
                ok: true,
                detail: None,
            })
        }

        fn supports_metadata(&self) -> bool {
            !matches!(self.mode, Mode::NotProvider)
        }

        async fn metadata_lookup(
            &self,
            _params: MetadataLookupParams,
        ) -> Result<Vec<MetadataRecord>> {
            self.maybe_hang().await;
            self.records()
        }

        async fn metadata_search(
            &self,
            _params: MetadataSearchParams,
        ) -> Result<Vec<MetadataRecord>> {
            self.maybe_hang().await;
            self.records()
        }

        async fn metadata_chapters(
            &self,
            _params: MetadataChaptersParams,
        ) -> Result<MetadataChapters> {
            self.maybe_hang().await;
            match self.mode {
                Mode::Answers => Ok(MetadataChapters {
                    found: true,
                    chapters: vec![MetadataChapter {
                        title: self.key.into(),
                        ..MetadataChapter::default()
                    }],
                    ..MetadataChapters::default()
                }),
                Mode::Empty | Mode::NotProvider => Ok(MetadataChapters::default()),
                Mode::Fails => Err(IntegrationError::message("boom")),
                Mode::Hangs => unreachable!("hanging fakes never get here"),
            }
        }
    }

    fn registry(fakes: Vec<Fake>) -> IntegrationRegistry {
        let mut registry = IntegrationRegistry::new();
        for fake in fakes {
            registry.register(Arc::new(fake));
        }
        registry
    }

    #[tokio::test]
    async fn every_provider_answers_in_order_and_failures_are_reported() {
        let registry = registry(vec![
            Fake {
                key: "plain",
                mode: Mode::NotProvider,
            },
            Fake {
                key: "broken",
                mode: Mode::Fails,
            },
            Fake {
                key: "first",
                mode: Mode::Answers,
            },
            Fake {
                key: "empty",
                mode: Mode::Empty,
            },
            Fake {
                key: "second",
                mode: Mode::Answers,
            },
        ]);
        assert_eq!(registry.metadata_providers().len(), 4);
        for answer in [
            registry.lookup(&MetadataLookupParams::default()).await,
            registry.search(&MetadataSearchParams::default()).await,
        ] {
            let providers: Vec<_> = answer.records.iter().map(|r| r.provider.as_str()).collect();
            assert_eq!(providers, ["first", "second"]);
            assert_eq!(answer.records[0].record.title, "first");
            assert_eq!(answer.failures.len(), 1);
            assert_eq!(answer.failures[0].provider, "broken");
            assert!(answer.failures[0].message.contains("boom"));
            assert!(answer.incomplete());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_hanging_provider_times_out_and_the_next_is_still_asked() {
        let registry = registry(vec![
            Fake {
                key: "stuck",
                mode: Mode::Hangs,
            },
            Fake {
                key: "first",
                mode: Mode::Answers,
            },
        ]);
        for answer in [
            registry.lookup(&MetadataLookupParams::default()).await,
            registry.search(&MetadataSearchParams::default()).await,
        ] {
            assert_eq!(answer.records.len(), 1);
            assert_eq!(answer.records[0].provider, "first");
            assert_eq!(answer.failures.len(), 1);
            assert_eq!(answer.failures[0].provider, "stuck");
            assert!(answer.failures[0].message.contains("no answer"));
        }
        let chapters = registry.chapters(&MetadataChaptersParams::default()).await;
        assert_eq!(chapters.found.map(|c| c.provider).as_deref(), Some("first"));
        assert_eq!(chapters.failures[0].provider, "stuck");
    }

    #[tokio::test]
    async fn chapters_stop_at_the_first_layout() {
        let providers = registry(vec![
            Fake {
                key: "broken",
                mode: Mode::Fails,
            },
            Fake {
                key: "empty",
                mode: Mode::Empty,
            },
            Fake {
                key: "first",
                mode: Mode::Answers,
            },
            Fake {
                key: "second",
                mode: Mode::Answers,
            },
        ]);
        let answer = providers.chapters(&MetadataChaptersParams::default()).await;
        let found = answer.found.expect("a layout");
        assert_eq!(found.provider, "first");
        assert_eq!(found.chapters.chapters[0].title, "first");
        assert_eq!(answer.failures.len(), 1);

        let none = registry(vec![Fake {
            key: "empty",
            mode: Mode::Empty,
        }])
        .chapters(&MetadataChaptersParams::default())
        .await;
        assert!(none.found.is_none());
        assert!(none.failures.is_empty());
    }

    #[tokio::test]
    async fn default_methods_refuse() {
        struct Plain;
        #[async_trait]
        impl Integration for Plain {
            fn id(&self) -> &str {
                "plain"
            }
            async fn start(&self, _ctx: IntegrationContext) -> Result<()> {
                Ok(())
            }
            async fn deliver_domain_event(&self, _event: DomainEvent) -> Result<EventResult> {
                Ok(EventResult::Ack)
            }
            async fn health(&self) -> Result<IntegrationHealth> {
                Err(IntegrationError::message("unused"))
            }
        }
        let plain = Plain;
        assert!(!plain.supports_metadata());
        for err in [
            plain
                .metadata_lookup(MetadataLookupParams::default())
                .await
                .unwrap_err(),
            plain
                .metadata_search(MetadataSearchParams::default())
                .await
                .unwrap_err(),
            plain
                .metadata_chapters(MetadataChaptersParams::default())
                .await
                .unwrap_err(),
        ] {
            assert!(err.to_string().contains("does not provide metadata"));
        }
        assert!(!plain.supports_covers());
        let err = plain
            .fetch_covers(vec!["https://a.example/1.jpg".into()], 1)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("does not fetch covers"), "{err}");
    }
}
