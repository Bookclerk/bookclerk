//! Reaches the deliverability checkers over HTTPS.
//!
//! [`HttpCheckers`] asks Postmark's spam check, Cloudflare's DNS-over-HTTPS
//! resolver and mail-tester.com, each through the host's socket proxy, so
//! only the hosts in the manifest's `tcp` grants the operator approved can
//! be reached. Answers are read up to
//! [`crate::deliverability::MAX_ANSWER_BYTES`] and handed, unparsed, to
//! [`crate::deliverability`]. Errors never carry the message, a setting or
//! the checker's own text.

use std::time::Duration;

use async_trait::async_trait;
use bookclerk_plugin_sdk::http::{header, Client};

use crate::deliverability::{DOH_URL, MAIL_TESTER_URL, MAX_ANSWER_BYTES, POSTMARK_URL};

/// How long one checker request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// The checkers' answers, as text; tests swap in canned ones.
#[async_trait(?Send)]
pub trait Checkers {
    /// Posts a composed message to Postmark's spam check.
    ///
    /// # Arguments
    ///
    /// * `raw` - The whole message, CRLF line ends.
    ///
    /// # Errors
    ///
    /// A short reason when the service can't be reached or answers badly.
    async fn spamcheck(&self, raw: &str) -> Result<String, String>;

    /// Looks up a name's TXT records over DNS-over-HTTPS.
    ///
    /// # Arguments
    ///
    /// * `name` - The DNS name.
    ///
    /// # Errors
    ///
    /// A short reason when the resolver can't be reached.
    async fn txt(&self, name: &str) -> Result<String, String>;

    /// Reads one mail-tester result as JSON.
    ///
    /// # Arguments
    ///
    /// * `id` - The test address's local part (`<account>-<token>`).
    ///
    /// # Errors
    ///
    /// A short reason when mail-tester can't be reached.
    async fn mail_tester(&self, id: &str) -> Result<String, String>;

    /// Waits between mail-tester polls.
    ///
    /// # Arguments
    ///
    /// * `wait` - How long.
    async fn pause(&self, wait: Duration);
}

/// The checkers over HTTPS through the host.
#[derive(Debug, Default, Clone, Copy)]
pub struct HttpCheckers;

/// Why a host could not be reached, in words the operator can act on.
///
/// # Arguments
///
/// * `host` - The checker's host.
fn unreachable(host: &str) -> String {
    format!("{host} could not be reached; approve it in the plugin's network settings, or try again later")
}

/// Sends a request and reads its body up to [`MAX_ANSWER_BYTES`].
///
/// # Arguments
///
/// * `request` - The request, built.
/// * `host` - The checker's host, for errors.
///
/// # Errors
///
/// When the host can't be reached, answers with an error status, or sends
/// too much.
async fn read_capped(
    request: bookclerk_plugin_sdk::http::RequestBuilder,
    host: &str,
) -> Result<String, String> {
    let mut response = request.send().await.map_err(|_| unreachable(host))?;
    if !response.status().is_success() {
        return Err(format!(
            "{host} answered HTTP {}",
            response.status().as_u16()
        ));
    }
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| unreachable(host))? {
        if body.len() + chunk.len() > MAX_ANSWER_BYTES {
            return Err(format!("{host} sent too long an answer"));
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body).map_err(|_| format!("{host} sent an answer that is not text"))
}

/// A client with the request timeout.
///
/// # Errors
///
/// When the host's socket proxy is unusable.
fn client() -> Result<Client, String> {
    Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|_| "the plugin could not open network connections".to_owned())
}

#[async_trait(?Send)]
impl Checkers for HttpCheckers {
    async fn spamcheck(&self, raw: &str) -> Result<String, String> {
        let body = serde_json::json!({ "email": raw, "options": "long" }).to_string();
        let request = client()?
            .post(POSTMARK_URL)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json")
            .body(body);
        read_capped(request, "spamcheck.postmarkapp.com").await
    }

    async fn txt(&self, name: &str) -> Result<String, String> {
        let request = client()?
            .get(DOH_URL)
            .query(&[("name", name), ("type", "TXT")])
            .header(header::ACCEPT, "application/dns-json");
        read_capped(request, "cloudflare-dns.com").await
    }

    async fn mail_tester(&self, id: &str) -> Result<String, String> {
        let request = client()?
            .get(format!("{MAIL_TESTER_URL}/{id}"))
            .query(&[("format", "json")])
            .header(header::ACCEPT, "application/json");
        read_capped(request, "www.mail-tester.com").await
    }

    async fn pause(&self, wait: Duration) {
        tokio::time::sleep(wait).await;
    }
}
