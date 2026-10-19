use core::fmt::{self, Write};
use std::io;
use std::sync::Once;

use axum::routing::get;
use axum::Router;
use emerald_build_info::BuildMetadata;
use malachitebft_app::metrics::export;
use malachitebft_metrics::prometheus::encoding::{EncodeLabelValue, LabelValueEncoder};
use malachitebft_metrics::prometheus::metrics::family::Family;
use malachitebft_metrics::prometheus::metrics::gauge::Gauge;
use malachitebft_metrics::{Registry, SharedRegistry};
use tokio::net::{TcpListener, ToSocketAddrs};
use tracing::{error, info};

#[tracing::instrument(name = "metrics", skip_all)]
pub async fn serve(listen_addr: impl ToSocketAddrs) {
    if let Err(e) = inner(listen_addr).await {
        error!("Metrics server failed: {e}");
    }
}

async fn inner(listen_addr: impl ToSocketAddrs) -> io::Result<()> {
    let app = metrics_router();
    let listener = TcpListener::bind(listen_addr).await?;
    let local_addr = listener.local_addr()?;

    info!(address = %local_addr, "Serving metrics");
    axum::serve(listener, app).await?;

    Ok(())
}

fn metrics_router() -> Router {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        SharedRegistry::global().with_prefix("target", |registry| {
            register_build_info(registry, BuildMetadata::current());
        });
    });
    Router::new().route("/metrics", get(get_metrics))
}

fn register_build_info(registry: &mut Registry, metadata: BuildMetadata) {
    let family = Family::<Vec<(&'static str, BuildLabelValue)>, Gauge>::default();
    let labels = metadata
        .labels()
        .map(|(key, value)| (key, BuildLabelValue(value)))
        .to_vec();
    family.get_or_create(&labels).set(1);
    registry.register(
        "info",
        "Build information for the running Emerald binary",
        family,
    );
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct BuildLabelValue(&'static str);

impl EncodeLabelValue for BuildLabelValue {
    fn encode(&self, encoder: &mut LabelValueEncoder<'_>) -> fmt::Result {
        // prometheus-client 0.23 writes string labels verbatim. Escape only at
        // the text boundary, preserving the original compile-time metadata.
        for ch in self.0.chars() {
            match ch {
                '\\' => encoder.write_str("\\\\")?,
                '"' => encoder.write_str("\\\"")?,
                '\n' => encoder.write_str("\\n")?,
                _ => encoder.write_char(ch)?,
            }
        }
        Ok(())
    }
}

async fn get_metrics() -> String {
    let mut buf = String::new();
    export(&mut buf);
    buf
}

#[cfg(test)]
mod tests {
    use emerald_build_info::build_info;

    use super::*;

    #[tokio::test]
    async fn startup_exports_one_build_gauge_and_preserves_existing_metrics() {
        use malachitebft_metrics::prometheus::metrics::gauge::Gauge;
        use malachitebft_metrics::SharedRegistry;

        let existing = Gauge::<i64>::default();
        existing.set(7);
        SharedRegistry::global().with_prefix("build_info_test", |registry| {
            registry.register("existing", "Existing metric", existing);
        });

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, metrics_router()).await.unwrap();
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let url = format!("http://{address}/metrics");
        let first = client
            .get(&url)
            .send()
            .await
            .expect("metrics server starts without consensus")
            .text()
            .await
            .unwrap();
        // A second metrics initialization must not register another build-info series.
        drop(metrics_router());
        let second = client.get(&url).send().await.unwrap().text().await.unwrap();
        server.abort();

        for text in [first, second] {
            assert!(text.contains("# TYPE target_info gauge\n"), "{text}");
            let samples: Vec<_> = text
                .lines()
                .filter(|line| line.starts_with("target_info{"))
                .collect();
            assert_eq!(samples.len(), 1, "{text}");
            assert!(samples[0].ends_with("} 1"));
            assert_eq!(samples[0].matches("=\"").count(), 5);
            let expected_branch = match build_info::BRANCH {
                "HEAD" | ""
                    if !build_info::COMMIT_HASH.is_empty()
                        && build_info::COMMIT_HASH != "unknown" =>
                {
                    "detached"
                }
                "" => "unknown",
                branch => branch,
            };
            for (key, value) in [
                ("service_version", build_info::PKG_VERSION),
                ("service_build_time", build_info::BUILD_TIME_3339),
                ("service_rust_version", build_info::RUST_VERSION),
                ("vcs_ref_head", build_info::COMMIT_HASH),
                ("vcs_ref_head_branch", expected_branch),
            ] {
                assert!(samples[0].contains(&format!("{key}=\"{value}\"")), "{text}");
            }
            assert!(text.contains("build_info_test_existing 7\n"));
        }
    }

    #[test]
    fn build_labels_escape_quotes_backslashes_and_newlines() {
        use malachitebft_metrics::prometheus::encoding::text::encode;

        let metadata = BuildMetadata {
            service_version: "0.0.1",
            service_build_time: "2026-10-19T10:00:00+00:00",
            service_rust_version: "rustc 1.91.1",
            vcs_ref_head: "ef0c236f62bb8a44c3063c08297636e6fea05d83",
            vcs_ref_head_branch: "topic/\"quoted\"\\path\nline",
        };
        let mut registry = Registry::with_prefix("target");
        register_build_info(&mut registry, metadata);
        let mut text = String::new();
        encode(&mut text, &registry).unwrap();
        assert!(
            text.contains(r#"vcs_ref_head_branch="topic/\"quoted\"\\path\nline""#),
            "{text}"
        );
        assert_eq!(
            text.lines().filter(|line| !line.starts_with('#')).count(),
            1
        );
    }
}
