//! Development-only pinned metadata transport, not a production SSRF resolver.
use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};

pub const URL: &str = "https://client.example.invalid/oauth/client-metadata.json";
#[derive(Debug, PartialEq)]
pub enum FetchError {
    Policy,
    Transport,
    Response,
    TooLarge,
}
pub struct PinnedTarget {
    address: SocketAddr,
}
impl PinnedTarget {
    pub fn approve(
        url: &str,
        answers: &[IpAddr],
        approved_ips: &[IpAddr],
    ) -> Result<Self, FetchError> {
        if url != URL || answers.len() != 1 {
            return Err(FetchError::Policy);
        }
        let ip = answers[0];
        let IpAddr::V4(v4) = ip else {
            return Err(FetchError::Policy);
        };
        let [a, b, c, _] = v4.octets();
        // Conservative special-use rejection, plus explicit operator IP allowlist.
        // This is NOT a maintained IANA classification or a DNS implementation.
        let blocked = matches!(a, 0 | 10 | 127 | 224..=255)
            || (a == 100 && (64..=127).contains(&b))
            || (a == 169 && b == 254)
            || (a == 172 && (16..=31).contains(&b))
            || (a == 192 && (b == 168 || (b == 0 && matches!(c, 0 | 2)) || (b == 88 && c == 99)))
            || (a == 198 && (matches!(b, 18 | 19) || (b == 51 && c == 100)))
            || (a == 203 && b == 0 && c == 113);
        if blocked || !approved_ips.contains(&ip) {
            return Err(FetchError::Policy);
        }
        Ok(Self {
            address: SocketAddr::new(ip, 443),
        })
    }
    // Deliberate local-only test seam, unavailable through any product endpoint.
    pub fn laboratory(address: SocketAddr) -> Result<Self, FetchError> {
        if address.ip() != IpAddr::from([127, 0, 0, 1]) || address.port() == 0 {
            return Err(FetchError::Policy);
        }
        Ok(Self { address })
    }
}
pub async fn fetch(target: PinnedTarget, root_pem: &[u8]) -> Result<Vec<u8>, FetchError> {
    // reqwest defers some root parsing until connect; reject malformed config
    // before creating any connection, using the already pinned crypto library.
    openssl::x509::X509::from_pem(root_pem).map_err(|_| FetchError::Policy)?;
    let root = reqwest::Certificate::from_pem(root_pem).map_err(|_| FetchError::Policy)?;
    let client = reqwest::Client::builder()
        .https_only(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .timeout(Duration::from_millis(500))
        .connect_timeout(Duration::from_millis(300))
        .pool_max_idle_per_host(0)
        .resolve("client.example.invalid", target.address)
        .tls_certs_only([root])
        .build()
        .map_err(|_| FetchError::Policy)?;
    let mut response = client
        .get(URL)
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|_| FetchError::Transport)?;
    if response.status() != 200
        || response
            .headers()
            .get("content-type")
            .and_then(|h| h.to_str().ok())
            != Some("application/json")
        || response.headers().contains_key("content-encoding")
    {
        return Err(FetchError::Response);
    }
    if response
        .content_length()
        .is_some_and(|length| length > 8192)
    {
        return Err(FetchError::TooLarge);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| FetchError::Transport)? {
        if bytes.len() + chunk.len() > 8192 {
            return Err(FetchError::TooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

pub mod laboratory {
    use super::*;
    use openssl::{
        asn1::Asn1Time,
        bn::{BigNum, MsbOption},
        hash::MessageDigest,
        pkey::PKey,
        rsa::Rsa,
        ssl::{SslAcceptor, SslMethod},
        x509::{
            X509, X509NameBuilder,
            extension::{BasicConstraints, ExtendedKeyUsage, KeyUsage, SubjectAlternativeName},
        },
    };
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        thread,
    };
    pub struct Server {
        pub address: SocketAddr,
        pub root: Vec<u8>,
        pub requests: Arc<AtomicUsize>,
        stop: Arc<AtomicBool>,
        task: Option<thread::JoinHandle<()>>,
    }
    impl Server {
        pub fn start(response: Vec<u8>, host: &str, delay: Duration) -> Self {
            let ca_key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
            let key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
            let mut name = X509NameBuilder::new().unwrap();
            name.append_entry_by_text("CN", "Artificial metadata CA")
                .unwrap();
            let name = name.build();
            let mut ca = X509::builder().unwrap();
            ca.set_version(2).unwrap();
            let mut serial = BigNum::new().unwrap();
            serial.rand(128, MsbOption::MAYBE_ZERO, false).unwrap();
            ca.set_serial_number(&serial.to_asn1_integer().unwrap())
                .unwrap();
            ca.set_subject_name(&name).unwrap();
            ca.set_issuer_name(&name).unwrap();
            ca.set_pubkey(&ca_key).unwrap();
            ca.set_not_before(&Asn1Time::days_from_now(0).unwrap())
                .unwrap();
            ca.set_not_after(&Asn1Time::days_from_now(2).unwrap())
                .unwrap();
            ca.append_extension(BasicConstraints::new().critical().ca().build().unwrap())
                .unwrap();
            ca.append_extension(KeyUsage::new().critical().key_cert_sign().build().unwrap())
                .unwrap();
            ca.sign(&ca_key, MessageDigest::sha256()).unwrap();
            let ca = ca.build();
            let mut leaf = X509::builder().unwrap();
            leaf.set_version(2).unwrap();
            serial.rand(128, MsbOption::MAYBE_ZERO, false).unwrap();
            leaf.set_serial_number(&serial.to_asn1_integer().unwrap())
                .unwrap();
            leaf.set_subject_name(&name).unwrap();
            leaf.set_issuer_name(ca.subject_name()).unwrap();
            leaf.set_pubkey(&key).unwrap();
            leaf.set_not_before(&Asn1Time::days_from_now(0).unwrap())
                .unwrap();
            leaf.set_not_after(&Asn1Time::days_from_now(1).unwrap())
                .unwrap();
            leaf.append_extension(BasicConstraints::new().critical().build().unwrap())
                .unwrap();
            leaf.append_extension(
                KeyUsage::new()
                    .critical()
                    .digital_signature()
                    .key_encipherment()
                    .build()
                    .unwrap(),
            )
            .unwrap();
            leaf.append_extension(ExtendedKeyUsage::new().server_auth().build().unwrap())
                .unwrap();
            let san = SubjectAlternativeName::new()
                .dns(host)
                .build(&leaf.x509v3_context(Some(&ca), None))
                .unwrap();
            leaf.append_extension(san).unwrap();
            leaf.sign(&ca_key, MessageDigest::sha256()).unwrap();
            let mut tls = SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
            tls.set_private_key(&key).unwrap();
            tls.set_certificate(&leaf.build()).unwrap();
            tls.check_private_key().unwrap();
            let tls = tls.build();
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            listener.set_nonblocking(true).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let requests = Arc::new(AtomicUsize::new(0));
            let worker_stop = stop.clone();
            let worker_requests = requests.clone();
            let task = thread::spawn(move || {
                while !worker_stop.load(Ordering::SeqCst) {
                    let stream = match listener.accept() {
                        Ok((stream, _)) => stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(1));
                            continue;
                        }
                        Err(_) => break,
                    };
                    stream
                        .set_read_timeout(Some(Duration::from_secs(1)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(1)))
                        .unwrap();
                    let Ok(mut stream) = tls.accept(stream) else {
                        continue;
                    };
                    let mut header = Vec::new();
                    let mut buffer = [0; 1024];
                    while header.len() < 8192 && !header.windows(4).any(|w| w == b"\r\n\r\n") {
                        match stream.read(&mut buffer) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => header.extend_from_slice(&buffer[..n]),
                        }
                    }
                    let lower = String::from_utf8_lossy(&header).to_ascii_lowercase();
                    if !header.starts_with(b"GET /oauth/client-metadata.json HTTP/1.1\r\n")
                        || !lower.contains("\r\nhost: client.example.invalid\r\n")
                        || lower.contains("\r\nauthorization:")
                        || lower.contains("\r\ncookie:")
                    {
                        continue;
                    }
                    worker_requests.fetch_add(1, Ordering::SeqCst);
                    thread::sleep(delay);
                    let _ = stream.write_all(&response);
                    let _ = stream.flush();
                    let _ = stream.shutdown();
                }
            });
            Self {
                address,
                root: ca.to_pem().unwrap(),
                requests,
                stop,
                task: Some(task),
            }
        }
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(task) = self.task.take() {
                task.join().unwrap();
            }
        }
    }
    pub fn json_response(body: &[u8]) -> Vec<u8> {
        let mut response=format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).into_bytes();
        response.extend_from_slice(body);
        response
    }
}
