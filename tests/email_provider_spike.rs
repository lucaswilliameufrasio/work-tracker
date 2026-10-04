//! SaaS protocol fakes on real loopback HTTP. No external email is ever sent.
#[path = "support/email_contract.rs"]
mod email_contract;

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::IntoResponse,
    routing::post,
};
use email_contract::{Acceptance, Delivery, Email, FailureKind, LocalEmailClient, Provider};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;

const ACCOUNT: &str = "0123456789abcdef0123456789abcdef";
const TOKEN: &str = "fake-token-only-for-loopback";
const RECIPIENT: &str = "recipient@example.invalid";

#[derive(Clone)]
struct Script {
    status: StatusCode,
    body: String,
    headers: HeaderMap,
    delay: Duration,
    calls: Arc<Mutex<Vec<Captured>>>,
}
struct Captured {
    path: String,
    headers: HeaderMap,
    body: Vec<u8>,
}
struct Fake {
    base: String,
    script: Script,
    task: JoinHandle<()>,
}
impl Drop for Fake {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Fake {
    async fn start(status: u16, body: String, headers: HeaderMap, delay: Duration) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let script = Script {
            status: StatusCode::from_u16(status).unwrap(),
            body,
            headers,
            delay,
            calls: Arc::new(Mutex::new(vec![])),
        };
        let app = Router::new()
            .fallback(post(receive))
            .with_state(script.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { base, script, task }
    }
    fn client(&self, provider: Provider) -> LocalEmailClient {
        LocalEmailClient::new(provider, &self.base, ACCOUNT, TOKEN, Duration::from_secs(2)).unwrap()
    }
}
async fn receive(
    State(script): State<Script>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    script.calls.lock().unwrap().push(Captured {
        path: uri.path().into(),
        headers,
        body: body.to_vec(),
    });
    tokio::time::sleep(script.delay).await;
    (script.status, script.headers, script.body)
}

fn email() -> Email {
    Email {
        from: "Puxiri <sender@example.invalid>".into(),
        to: RECIPIENT.into(),
        reply_to: "replies@example.invalid".into(),
        subject: "Atribuição PX-1".into(),
        text: "PX-1 foi atribuído. Abra https://installation.example.invalid/issues/PX-1".into(),
        html: "<p>PX-1 foi atribuído.</p>".into(),
    }
}
fn cloudflare(group: &str, message_id: bool) -> String {
    let mut result =
        json!({"delivered":[],"queued":[],"permanent_bounces":[],"suppressed_recipients":[]});
    result[group] = json!([RECIPIENT]);
    if message_id {
        result["message_id"] = json!("<fixture@example.invalid>");
    }
    json!({"success":true,"errors":[],"messages":[],"result":result}).to_string()
}

#[tokio::test]
async fn provider_paths_auth_content_length_and_payload_are_sent_over_real_http() {
    for (provider, body, path, expected) in [
        (
            Provider::Cloudflare,
            cloudflare("queued", true),
            format!("/client/v4/accounts/{ACCOUNT}/email/sending/send"),
            Delivery::Queued,
        ),
        (
            Provider::Resend,
            json!({"id":"fixture-resend-id"}).to_string(),
            "/emails".into(),
            Delivery::Accepted,
        ),
    ] {
        let fake = Fake::start(200, body, HeaderMap::new(), Duration::ZERO).await;
        let receipt = fake
            .client(provider)
            .send(&email(), "stable-message-key")
            .await
            .unwrap();
        assert_eq!(receipt.delivery, expected);
        assert!(receipt.message_id.is_some());
        let calls = fake.script.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].path, path);
        assert_eq!(calls[0].headers["authorization"], format!("Bearer {TOKEN}"));
        assert_eq!(calls[0].headers["content-type"], "application/json");
        assert_eq!(
            calls[0].headers["content-length"]
                .to_str()
                .unwrap()
                .parse::<usize>()
                .unwrap(),
            calls[0].body.len()
        );
        let payload: Value = serde_json::from_slice(&calls[0].body).unwrap();
        assert_eq!(payload, serde_json::to_value(email()).unwrap());
        assert!(payload.get("attachments").is_none());
        assert!(payload.get("replyTo").is_none());
        assert_eq!(
            calls[0]
                .headers
                .get("idempotency-key")
                .map(|value| value.to_str().unwrap()),
            matches!(provider, Provider::Resend).then_some("stable-message-key")
        );
    }
}

#[tokio::test]
async fn cloudflare_distinguishes_per_recipient_outcomes_and_does_not_invent_missing_message_id() {
    for (group, delivery) in [
        ("delivered", Delivery::Delivered),
        ("queued", Delivery::Queued),
        ("permanent_bounces", Delivery::PermanentBounce),
        ("suppressed_recipients", Delivery::Suppressed),
    ] {
        let fake = Fake::start(
            200,
            cloudflare(group, false),
            HeaderMap::new(),
            Duration::ZERO,
        )
        .await;
        let receipt = fake
            .client(Provider::Cloudflare)
            .send(&email(), "not-used")
            .await
            .unwrap();
        assert_eq!(receipt.message_id, None);
        assert_eq!(receipt.delivery, delivery);
        assert_eq!(fake.script.calls.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn ambiguous_or_malformed_success_never_becomes_delivered_or_safe_to_fallback() {
    let mut unrelated: Value = serde_json::from_str(&cloudflare("queued", true)).unwrap();
    unrelated["result"]["queued"] = json!(["other@example.invalid"]);
    let mut duplicated: Value = serde_json::from_str(&cloudflare("queued", true)).unwrap();
    duplicated["result"]["delivered"] = json!([RECIPIENT]);
    for (provider, body) in [
        (Provider::Cloudflare, unrelated.to_string()),
        (Provider::Cloudflare, duplicated.to_string()),
        (
            Provider::Cloudflare,
            json!({"success":false,"errors":[{"code":10001}],"result":null}).to_string(),
        ),
        (
            Provider::Cloudflare,
            json!({"success":true,"errors":[],"result":{}}).to_string(),
        ),
        (Provider::Cloudflare, "not json".into()),
        (Provider::Resend, "{}".into()),
        (Provider::Resend, json!({"id":""}).to_string()),
        (Provider::Resend, json!({"id":12}).to_string()),
        (Provider::Resend, "<html>proxy error</html>".into()),
        (Provider::Resend, "x".repeat(65537)),
    ] {
        let fake = Fake::start(200, body, HeaderMap::new(), Duration::ZERO).await;
        let failure = fake
            .client(provider)
            .send(&email(), "key")
            .await
            .unwrap_err();
        assert_eq!(failure.kind, FailureKind::Protocol);
        assert_eq!(failure.acceptance, Acceptance::Unknown);
        assert_eq!(fake.script.calls.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn http_errors_classify_acceptance_and_retry_after_without_leaking_bodies_or_retrying() {
    for provider in [Provider::Cloudflare, Provider::Resend, Provider::SendGrid] {
        for (status, kind, acceptance) in [
            (400, FailureKind::InvalidRequest, Acceptance::Rejected),
            (401, FailureKind::Authentication, Acceptance::Rejected),
            (403, FailureKind::Authentication, Acceptance::Rejected),
            (404, FailureKind::Rejected, Acceptance::Rejected),
            (422, FailureKind::InvalidRequest, Acceptance::Rejected),
            (429, FailureKind::Throttled, Acceptance::Rejected),
            (500, FailureKind::Temporary, Acceptance::Unknown),
            (503, FailureKind::Temporary, Acceptance::Unknown),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert("retry-after", "120".parse().unwrap());
            let fake = Fake::start(
                status,
                format!("secret-provider-body {TOKEN} {RECIPIENT}"),
                headers,
                Duration::ZERO,
            )
            .await;
            let failure = fake
                .client(provider)
                .send(&email(), "key")
                .await
                .unwrap_err();
            assert_eq!(failure.kind, kind);
            assert_eq!(failure.acceptance, acceptance);
            assert_eq!(failure.status, Some(status));
            assert_eq!(failure.retry_after_seconds, Some(120));
            let debug = format!("{failure:?}");
            for secret in [TOKEN, RECIPIENT, "secret-provider-body", fake.base.as_str()] {
                assert!(!debug.contains(secret));
            }
            assert_eq!(fake.script.calls.lock().unwrap().len(), 1);
        }
    }
}

#[tokio::test]
async fn resend_conflicts_and_stable_keys_are_not_treated_as_global_exactly_once() {
    for (name, kind, acceptance) in [
        (
            "invalid_idempotent_request",
            FailureKind::Conflict,
            Acceptance::Rejected,
        ),
        (
            "concurrent_idempotent_requests",
            FailureKind::Temporary,
            Acceptance::Unknown,
        ),
        (
            "resource_locked",
            FailureKind::Temporary,
            Acceptance::Unknown,
        ),
        ("unknown", FailureKind::Protocol, Acceptance::Unknown),
    ] {
        let fake = Fake::start(
            409,
            json!({"name":name}).to_string(),
            HeaderMap::new(),
            Duration::ZERO,
        )
        .await;
        let failure = fake
            .client(Provider::Resend)
            .send(&email(), "same-key")
            .await
            .unwrap_err();
        assert_eq!(failure.kind, kind);
        assert_eq!(failure.acceptance, acceptance);
        assert_eq!(fake.script.calls.lock().unwrap().len(), 1);
    }
    let fake = Fake::start(
        200,
        json!({"id":"same-id"}).to_string(),
        HeaderMap::new(),
        Duration::ZERO,
    )
    .await;
    let client = fake.client(Provider::Resend);
    client.send(&email(), "same-key").await.unwrap();
    client.send(&email(), "same-key").await.unwrap();
    let calls = fake.script.calls.lock().unwrap();
    assert_eq!(calls.len(), 2); // explicit caller retries, not a provider dedup proof.
    assert_eq!(
        calls[0].headers["idempotency-key"],
        calls[1].headers["idempotency-key"]
    );
    assert_eq!(calls[0].body, calls[1].body);
}

#[tokio::test]
async fn timeout_after_request_acceptance_is_unknown_and_redirect_cannot_forward_token() {
    let target = Fake::start(200, "{}".into(), HeaderMap::new(), Duration::ZERO).await;
    let mut headers = HeaderMap::new();
    headers.insert("location", format!("{}/leak", target.base).parse().unwrap());
    let redirect = Fake::start(307, "".into(), headers, Duration::ZERO).await;
    let failure = redirect
        .client(Provider::Resend)
        .send(&email(), "key")
        .await
        .unwrap_err();
    assert_eq!(failure.kind, FailureKind::Protocol);
    assert_eq!(failure.acceptance, Acceptance::Unknown);
    assert_eq!(redirect.script.calls.lock().unwrap().len(), 1);
    assert!(target.script.calls.lock().unwrap().is_empty());
    let delayed = Fake::start(
        200,
        json!({"id":"accepted-but-late"}).to_string(),
        HeaderMap::new(),
        Duration::from_millis(500),
    )
    .await;
    let client = LocalEmailClient::new(
        Provider::Resend,
        &delayed.base,
        ACCOUNT,
        TOKEN,
        Duration::from_millis(100),
    )
    .unwrap();
    let failure = client.send(&email(), "key").await.unwrap_err();
    assert_eq!(failure.kind, FailureKind::Transport);
    assert_eq!(failure.acceptance, Acceptance::Unknown);
    assert_eq!(delayed.script.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn invalid_local_configuration_headers_and_keys_never_reach_transport() {
    for (base, account, token) in [
        ("https://api.resend.com/", ACCOUNT, TOKEN),
        ("http://example.com/", ACCOUNT, TOKEN),
        ("http://127.0.0.1/?secret=query", ACCOUNT, TOKEN),
        ("http://127.0.0.1/", "bad/path", TOKEN),
        ("http://127.0.0.1/", ACCOUNT, "bad\ntoken"),
    ] {
        assert!(
            LocalEmailClient::new(
                Provider::Cloudflare,
                base,
                account,
                token,
                Duration::from_secs(1)
            )
            .is_err()
        );
    }
    let fake = Fake::start(200, "{}".into(), HeaderMap::new(), Duration::ZERO).await;
    let client = fake.client(Provider::Resend);
    for key in ["", "bad\nkey", &"x".repeat(257)] {
        assert_eq!(
            client.send(&email(), key).await.unwrap_err().acceptance,
            Acceptance::Rejected
        );
    }
    let mut invalid = email();
    invalid
        .subject
        .push_str("\r\nBcc: injected@example.invalid");
    assert_eq!(
        client.send(&invalid, "key").await.unwrap_err().kind,
        FailureKind::InvalidRequest
    );
    assert!(fake.script.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn interrupted_response_body_is_unknown_even_after_http_200() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut connection, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        loop {
            let mut chunk = [0u8; 4096];
            let n = connection.read(&mut chunk).await.unwrap();
            assert!(n > 0);
            request.extend_from_slice(&chunk[..n]);
            assert!(request.len() <= 16384);
            if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                let headers = String::from_utf8(request[..end].to_vec()).unwrap();
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse().unwrap())
                    })
                    .unwrap();
                if request.len() >= end + 4 + length {
                    break;
                }
            }
        }
        // Declare more bytes than we send, then close the real TCP connection.
        connection.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 128\r\nConnection: close\r\n\r\n{\"id\":").await.unwrap();
        connection.shutdown().await.unwrap();
    });
    let client = LocalEmailClient::new(
        Provider::Resend,
        &base,
        ACCOUNT,
        TOKEN,
        Duration::from_secs(2),
    )
    .unwrap();
    let failure = client.send(&email(), "interrupted-body").await.unwrap_err();
    assert_eq!(failure.kind, FailureKind::Transport);
    assert_eq!(failure.acceptance, Acceptance::Unknown);
    assert_eq!(failure.status, Some(200));
    assert!(!format!("{failure:?}").contains(&base));
    server.await.unwrap();
}

#[tokio::test]
async fn sendgrid_202_uses_structured_mailboxes_and_optional_header_id_not_json_delivery() {
    for id in [Some("fixture-sg-id"), None] {
        let mut headers = HeaderMap::new();
        if let Some(id) = id {
            headers.insert("x-message-id", id.parse().unwrap());
        }
        let fake = Fake::start(202, String::new(), headers, Duration::ZERO).await;
        let receipt = fake
            .client(Provider::SendGrid)
            .send(&email(), "not-an-idempotency-key")
            .await
            .unwrap();
        assert_eq!(receipt.delivery, Delivery::Accepted);
        assert_eq!(receipt.message_id.as_deref(), id);
        let calls = fake.script.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].path, "/v3/mail/send");
        assert_eq!(calls[0].headers["authorization"], format!("Bearer {TOKEN}"));
        assert!(calls[0].headers.get("idempotency-key").is_none());
        let payload: Value = serde_json::from_slice(&calls[0].body).unwrap();
        assert_eq!(
            payload["from"],
            json!({"email":"sender@example.invalid","name":"Puxiri"})
        );
        assert_eq!(
            payload["personalizations"][0]["to"],
            json!([{"email":RECIPIENT}])
        );
        assert_eq!(
            payload["reply_to"],
            json!({"email":"replies@example.invalid"})
        );
        assert_eq!(payload["content"][0]["type"], "text/plain");
        assert_eq!(payload["content"][1]["type"], "text/html");
        assert_eq!(
            payload["tracking_settings"]["open_tracking"]["enable"],
            false
        );
        assert_eq!(
            payload["tracking_settings"]["click_tracking"]["enable"],
            false
        );
        assert!(payload.get("attachments").is_none());
    }
    for (status, body) in [(200, "{}"), (202, "{\"id\":\"not-the-contract\"}")] {
        let fake = Fake::start(status, body.into(), HeaderMap::new(), Duration::ZERO).await;
        assert_eq!(
            fake.client(Provider::SendGrid)
                .send(&email(), "unused")
                .await
                .unwrap_err()
                .acceptance,
            Acceptance::Unknown
        );
    }
}

fn ses_client(base: &str) -> aws_sdk_sesv2::Client {
    use aws_sdk_sesv2::config::{
        BehaviorVersion, Credentials, Region, retry::RetryConfig, timeout::TimeoutConfig,
    };
    let url = reqwest::Url::parse(base).unwrap();
    assert_eq!(url.host_str(), Some("127.0.0.1"));
    assert_eq!(url.scheme(), "http");
    let config = aws_sdk_sesv2::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .credentials_provider(Credentials::new(
            "AKIDFAKEONLY",
            "fake-secret-never-real",
            Some("fake-session-token".into()),
            None,
            "fixture",
        ))
        .endpoint_url(base)
        .retry_config(RetryConfig::disabled())
        .timeout_config(
            TimeoutConfig::builder()
                .operation_timeout(Duration::from_secs(2))
                .build(),
        )
        .build();
    aws_sdk_sesv2::Client::from_conf(config)
}

async fn ses_send(
    client: &aws_sdk_sesv2::Client,
) -> Result<
    aws_sdk_sesv2::operation::send_email::SendEmailOutput,
    Box<aws_sdk_sesv2::error::SdkError<aws_sdk_sesv2::operation::send_email::SendEmailError>>,
> {
    use aws_sdk_sesv2::types::{Body, Content, Destination, EmailContent, Message};
    let message = email();
    client
        .send_email()
        .from_email_address(message.from)
        .reply_to_addresses(message.reply_to)
        .destination(Destination::builder().to_addresses(message.to).build())
        .content(
            EmailContent::builder()
                .simple(
                    Message::builder()
                        .subject(
                            Content::builder()
                                .data(message.subject)
                                .charset("UTF-8")
                                .build()
                                .unwrap(),
                        )
                        .body(
                            Body::builder()
                                .text(
                                    Content::builder()
                                        .data(message.text)
                                        .charset("UTF-8")
                                        .build()
                                        .unwrap(),
                                )
                                .html(
                                    Content::builder()
                                        .data(message.html)
                                        .charset("UTF-8")
                                        .build()
                                        .unwrap(),
                                )
                                .build(),
                        )
                        .build(),
                )
                .build(),
        )
        .send()
        .await
        .map_err(Box::new)
}

#[tokio::test]
async fn ses_official_sdk_signs_region_scoped_simple_send_without_loading_real_credentials() {
    let fake = Fake::start(
        200,
        json!({"MessageId":"fixture-ses-id"}).to_string(),
        HeaderMap::new(),
        Duration::ZERO,
    )
    .await;
    let output = ses_send(&ses_client(&fake.base)).await.unwrap();
    assert_eq!(output.message_id(), Some("fixture-ses-id")); // accepted, not delivered.
    let calls = fake.script.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].path, "/v2/email/outbound-emails");
    let auth = calls[0].headers["authorization"].to_str().unwrap();
    assert!(auth.starts_with("AWS4-HMAC-SHA256 "));
    assert!(auth.contains("/us-east-1/ses/aws4_request"));
    assert!(auth.contains("Credential=AKIDFAKEONLY/"));
    assert!(auth.contains("Signature="));
    assert_eq!(
        calls[0].headers["x-amz-security-token"],
        "fake-session-token"
    );
    assert!(calls[0].headers.get("x-amz-date").is_some());
    let payload: Value = serde_json::from_slice(&calls[0].body).unwrap();
    assert_eq!(payload["Destination"]["ToAddresses"], json!([RECIPIENT]));
    assert_eq!(payload["Content"]["Simple"]["Subject"]["Charset"], "UTF-8");
    assert_eq!(
        payload["Content"]["Simple"]["Body"]["Text"]["Data"],
        email().text
    );
    assert_eq!(
        payload["ReplyToAddresses"],
        json!(["replies@example.invalid"])
    );
}

#[tokio::test]
async fn ses_errors_are_modeled_without_hidden_throttle_or_service_retries() {
    for (status, code) in [
        (400, "MessageRejected"),
        (400, "MailFromDomainNotVerifiedException"),
        (400, "AccountSuspendedException"),
        (429, "TooManyRequestsException"),
        (500, "InternalFailure"),
    ] {
        let mut headers = HeaderMap::new();
        headers.insert("x-amzn-errortype", code.parse().unwrap());
        let fake = Fake::start(
            status,
            json!({"message":"fake failure"}).to_string(),
            headers,
            Duration::ZERO,
        )
        .await;
        let error = ses_send(&ses_client(&fake.base)).await.unwrap_err();
        use aws_sdk_sesv2::error::ProvideErrorMetadata;
        assert_eq!(error.raw_response().unwrap().status().as_u16(), status);
        assert_eq!(error.as_service_error().unwrap().code(), Some(code));
        assert_eq!(fake.script.calls.lock().unwrap().len(), 1);
        // SDK errors are intentionally not logged: they may embed bodies/metadata.
    }
}
