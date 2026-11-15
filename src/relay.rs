//! Private mode's relay (Linux). Soloist can't be told it's in a private session (its core
//! ignores the account setting), and Discord's Spotify status and other Connect observers go by
//! the `is_private_session` flag the playing device announces. So while private mode is on,
//! Soloist runs through this local proxy, which sets that flag in Soloist's Connect state and
//! changes nothing else:
//!
//! - audio and image CDNs skip the relay entirely (NO_PROXY);
//! - every other host is tunnelled byte for byte, still end-to-end encrypted;
//! - only Spotify's spclient hosts are decrypted, with a local CA that only Soloist trusts
//!   (via SPOTIFY_SSL_CERT_DIR) and that can't vouch for anything outside spotify.com;
//! - of those requests, only `PUT /connect-state/v1/devices/<id>` is edited, and only while
//!   the private session lasts. Anything unexpected is passed on unchanged.

use std::{
    collections::HashMap,
    convert::Infallible,
    fs,
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::{Request, Response, StatusCode, body::Incoming, header, service::service_fn};
use hyper_tls::HttpsConnector;
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::{TokioExecutor, TokioIo},
};
use openssl::{
    asn1::Asn1Time,
    bn::{BigNum, MsbOption},
    ec::{EcGroup, EcKey},
    hash::MessageDigest,
    nid::Nid,
    pkey::{PKey, Private},
    x509::{
        X509, X509Builder, X509Extension, X509NameBuilder,
        extension::{AuthorityKeyIdentifier, BasicConstraints, ExtendedKeyUsage, KeyUsage, SubjectAlternativeName, SubjectKeyIdentifier},
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_native_tls::TlsAcceptor;

type Res<T> = Result<T, String>;
type Body = BoxBody<Bytes, hyper::Error>;

fn e<E: std::fmt::Display>(err: E) -> String {
    err.to_string()
}

pub struct Relay {
    port: u16,
    certs: PathBuf,
}

struct State {
    ca_key: PKey<Private>,
    ca_cert: X509,
    leaf_key: PKey<Private>,
    acceptors: Mutex<HashMap<String, TlsAcceptor>>,
    upstream: Client<HttpsConnector<HttpConnector>, Full<Bytes>>,
    /// End of the private session (unix seconds); the flag is only set before it.
    until: Arc<AtomicU64>,
}

impl Relay {
    /// Starts listening on a free localhost port. `until` is shared with the app, so turning
    /// private mode off (or the session running out) stops the edits without a restart.
    pub async fn start(data_dir: &Path, until: Arc<AtomicU64>) -> Res<Relay> {
        let dir = data_dir.join("relay");
        fs::create_dir_all(&dir).map_err(e)?;
        let (ca_key, ca_cert) = load_or_create_ca(&dir)?;
        let certs = trust_dir(&dir, &ca_cert)?;
        let listener = TcpListener::bind("127.0.0.1:0").await.map_err(e)?;
        let port = listener.local_addr().map_err(e)?.port();
        let state = Arc::new(State {
            ca_key,
            ca_cert,
            leaf_key: ec_key()?,
            acceptors: Mutex::new(HashMap::new()),
            upstream: Client::builder(TokioExecutor::new()).build(HttpsConnector::new()),
            until,
        });
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(err) = handle(sock, state).await {
                        log::debug!("relay: {err}");
                    }
                });
            }
        });
        log::info!("private-mode relay on 127.0.0.1:{port}");
        Ok(Relay { port, certs })
    }

    /// The environment Soloist runs with while the relay is up.
    pub fn env(&self) -> Vec<(String, String)> {
        let proxy = format!("http://127.0.0.1:{}", self.port);
        // Audio, covers and other CDN downloads (the bulk of Soloist's traffic) go direct.
        let direct = "scdn.co,spotifycdn.com,localhost,127.0.0.1";
        [
            ("HTTPS_PROXY", proxy.as_str()),
            ("https_proxy", &proxy),
            ("HTTP_PROXY", &proxy),
            ("http_proxy", &proxy),
            ("NO_PROXY", direct),
            ("no_proxy", direct),
            ("SPOTIFY_SSL_CERT_DIR", &self.certs.to_string_lossy()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }
}

// ---------------------------------------------------------------- proxy

/// One client connection: an HTTP CONNECT, then a tunnel (or, for spclient, a decrypted relay).
async fn handle(mut sock: TcpStream, state: Arc<State>) -> Res<()> {
    let mut head = Vec::with_capacity(512);
    let mut buf = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = sock.read(&mut buf).await.map_err(e)?;
        if n == 0 || head.len() + n > 8192 {
            return Err("bad proxy request".into());
        }
        head.extend_from_slice(&buf[..n]);
    }
    let line = head.split(|&b| b == b'\r').next().unwrap_or_default();
    let line = std::str::from_utf8(line).map_err(e)?;
    let Some((host, port)) = connect_target(line) else {
        let _ = sock.write_all(b"HTTP/1.1 405 Method Not Allowed\r\ncontent-length: 0\r\n\r\n").await;
        return Err(format!("refused: {line}"));
    };
    sock.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").await.map_err(e)?;
    if port == 443 && is_spclient(&host) {
        relay_https(sock, host, state).await
    } else {
        let mut up = TcpStream::connect((host.as_str(), port)).await.map_err(e)?;
        tokio::io::copy_bidirectional(&mut sock, &mut up).await.map_err(e)?;
        Ok(())
    }
}

/// `CONNECT host:port HTTP/1.1` to a Spotify host on a Spotify port, else None.
fn connect_target(line: &str) -> Option<(String, u16)> {
    let mut parts = line.split(' ');
    if parts.next()? != "CONNECT" {
        return None;
    }
    let (host, port) = parts.next()?.rsplit_once(':')?;
    let port: u16 = port.parse().ok()?;
    let host = host.to_ascii_lowercase();
    let ok_host = !host.is_empty()
        && host.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        && ["spotify.com", "spotify.net", "scdn.co", "spotifycdn.com"]
            .iter()
            .any(|d| host == *d || host.ends_with(&format!(".{d}")));
    (ok_host && matches!(port, 443 | 4070 | 80)).then_some((host, port))
}

fn is_spclient(host: &str) -> bool {
    host.contains("spclient") && host.ends_with(".spotify.com")
}

async fn relay_https(sock: TcpStream, host: String, state: Arc<State>) -> Res<()> {
    let acceptor = state.acceptor(&host)?;
    let tls = acceptor.accept(sock).await.map_err(e)?;
    let svc = service_fn(move |req| {
        let (state, host) = (state.clone(), host.clone());
        async move { Ok::<_, Infallible>(forward(&state, &host, req).await) }
    });
    hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(tls), svc)
        .await
        .map_err(e)
}

const HOP: [&str; 8] = ["connection", "proxy-connection", "keep-alive", "te", "trailer", "transfer-encoding", "upgrade", "proxy-authorization"];

async fn forward(state: &State, host: &str, req: Request<Incoming>) -> Response<Body> {
    let (mut parts, body) = req.into_parts();
    let body = match body.collect().await {
        Ok(b) => b.to_bytes(),
        Err(err) => return error(StatusCode::BAD_REQUEST, err),
    };
    let path = parts.uri.path_and_query().map_or("/", |p| p.as_str()).to_string();
    let body = if parts.method == hyper::Method::PUT && is_device_state(&path) && state.private_now() {
        let gzip = parts.headers.get(header::CONTENT_ENCODING).is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"gzip"));
        match patch_state(&body, gzip) {
            Some(new) => {
                log::debug!("relay: marked Soloist's Connect state private ({} B)", new.len());
                parts.headers.insert(header::CONTENT_LENGTH, new.len().into());
                Bytes::from(new)
            }
            None => {
                log::warn!("relay: unfamiliar Connect state, passed on unchanged");
                body
            }
        }
    } else {
        body
    };
    parts.uri = match format!("https://{host}{path}").parse() {
        Ok(u) => u,
        Err(err) => return error(StatusCode::BAD_REQUEST, err),
    };
    for h in HOP {
        parts.headers.remove(h);
    }
    match state.upstream.request(Request::from_parts(parts, Full::new(body))).await {
        Ok(resp) => {
            let (mut parts, body) = resp.into_parts();
            for h in HOP {
                parts.headers.remove(h);
            }
            Response::from_parts(parts, body.boxed())
        }
        Err(err) => error(StatusCode::BAD_GATEWAY, err),
    }
}

fn error(status: StatusCode, err: impl std::fmt::Display) -> Response<Body> {
    log::debug!("relay: {status}: {err}");
    let mut r = Response::new(Full::new(Bytes::new()).map_err(|never| match never {}).boxed());
    *r.status_mut() = status;
    r
}

/// `/connect-state/v1/devices/<id>` (the device's full state), not `.../inactive` and friends.
fn is_device_state(path: &str) -> bool {
    path.strip_prefix("/connect-state/v1/devices/").is_some_and(|rest| !rest.is_empty() && !rest.contains('/') && !rest.contains('?'))
}

impl State {
    fn private_now(&self) -> bool {
        self.until.load(Ordering::Relaxed) > crate::app::unix_now()
    }

    fn acceptor(&self, host: &str) -> Res<TlsAcceptor> {
        let mut map = self.acceptors.lock().map_err(e)?;
        if let Some(a) = map.get(host) {
            return Ok(a.clone());
        }
        let cert = leaf_cert(host, &self.leaf_key, &self.ca_key, &self.ca_cert)?;
        let identity = native_tls::Identity::from_pkcs8(
            &cert.to_pem().map_err(e)?,
            &self.leaf_key.private_key_to_pem_pkcs8().map_err(e)?,
        )
        .map_err(e)?;
        let a = TlsAcceptor::from(native_tls::TlsAcceptor::new(identity).map_err(e)?);
        map.insert(host.to_string(), a.clone());
        Ok(a)
    }
}

// ---------------------------------------------------------------- the edit

/// Soloist's `PutStateRequest` with `device.device_info.is_private_session` (2 → 1 → 11) set,
/// every other byte as it was. None if it isn't shaped like one.
fn patch_state(body: &[u8], gzip: bool) -> Option<Vec<u8>> {
    if !gzip {
        return mark_private(body);
    }
    let mut raw = Vec::with_capacity(body.len() * 8);
    flate2::read::GzDecoder::new(body).read_to_end(&mut raw).ok()?;
    let patched = mark_private(&raw)?;
    let mut gz = flate2::write::GzEncoder::new(Vec::with_capacity(body.len() + 16), flate2::Compression::fast());
    gz.write_all(&patched).ok()?;
    gz.finish().ok()
}

fn mark_private(req: &[u8]) -> Option<Vec<u8>> {
    let mut found = false;
    let out = rewrite_field(req, 2, |device| {
        rewrite_field(device, 1, |info| {
            found = true;
            let mut kept = Vec::with_capacity(info.len() + 2);
            for (f, raw) in fields(info)? {
                if f != 11 {
                    kept.extend_from_slice(raw);
                }
            }
            kept.extend_from_slice(&[11 << 3, 1]); // field 11, varint 1
            Some(kept)
        })
    })?;
    found.then_some(out)
}

/// Copy of `msg` with the length-delimited field `num` replaced by `f(its payload)`.
fn rewrite_field(msg: &[u8], num: u64, mut f: impl FnMut(&[u8]) -> Option<Vec<u8>>) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(msg.len() + 4);
    for (field, raw) in fields(msg)? {
        if field == num && raw.first().is_some_and(|k| k & 7 == 2) {
            let (_, i) = varint(raw, 0)?;
            let (_, j) = varint(raw, i)?;
            let payload = f(&raw[j..])?;
            put_varint(&mut out, num << 3 | 2);
            put_varint(&mut out, payload.len() as u64);
            out.extend_from_slice(&payload);
        } else {
            out.extend_from_slice(raw);
        }
    }
    Some(out)
}

/// The fields of a protobuf message as (number, raw bytes including key); None if malformed.
fn fields(msg: &[u8]) -> Option<Vec<(u64, &[u8])>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < msg.len() {
        let start = i;
        let (key, k) = varint(msg, i)?;
        i = match key & 7 {
            0 => varint(msg, k)?.1,
            1 => k + 8,
            2 => {
                let (len, l) = varint(msg, k)?;
                l.checked_add(usize::try_from(len).ok()?)?
            }
            5 => k + 4,
            _ => return None,
        };
        if i > msg.len() {
            return None;
        }
        out.push((key >> 3, &msg[start..i]));
    }
    Some(out)
}

fn varint(b: &[u8], mut i: usize) -> Option<(u64, usize)> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let c = *b.get(i)?;
        i += 1;
        v |= u64::from(c & 0x7f) << shift;
        if c < 0x80 {
            return Some((v, i));
        }
    }
    None
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

// ---------------------------------------------------------------- certificates

fn ec_key() -> Res<PKey<Private>> {
    let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).map_err(e)?;
    PKey::from_ec_key(EcKey::generate(&group).map_err(e)?).map_err(e)
}

fn serial() -> Res<openssl::asn1::Asn1Integer> {
    let mut bn = BigNum::new().map_err(e)?;
    bn.rand(127, MsbOption::MAYBE_ZERO, false).map_err(e)?;
    bn.to_asn1_integer().map_err(e)
}

/// The relay's CA, made once and kept in `relay/` (key readable by you only). Remade when
/// it's missing, unreadable or near its end.
fn load_or_create_ca(dir: &Path) -> Res<(PKey<Private>, X509)> {
    let (key_path, cert_path) = (dir.join("ca.key"), dir.join("ca.pem"));
    if let (Ok(k), Ok(c)) = (fs::read(&key_path), fs::read(&cert_path))
        && let (Ok(key), Ok(cert)) = (PKey::private_key_from_pem(&k), X509::from_pem(&c))
        && Asn1Time::days_from_now(30).is_ok_and(|soon| cert.not_after() > soon)
    {
        return Ok((key, cert));
    }
    let key = ec_key()?;
    let mut name = X509NameBuilder::new().map_err(e)?;
    name.append_entry_by_nid(Nid::ORGANIZATIONNAME, "trill").map_err(e)?;
    name.append_entry_by_nid(Nid::COMMONNAME, "trill private-mode relay (Soloist only)").map_err(e)?;
    let name = name.build();
    let mut b = X509Builder::new().map_err(e)?;
    b.set_version(2).map_err(e)?;
    let (sn, from, to) = (serial()?, Asn1Time::days_from_now(0).map_err(e)?, Asn1Time::days_from_now(3650).map_err(e)?);
    b.set_serial_number(&sn).map_err(e)?;
    b.set_subject_name(&name).map_err(e)?;
    b.set_issuer_name(&name).map_err(e)?;
    b.set_pubkey(&key).map_err(e)?;
    b.set_not_before(&from).map_err(e)?;
    b.set_not_after(&to).map_err(e)?;
    b.append_extension(BasicConstraints::new().critical().ca().pathlen(0).build().map_err(e)?).map_err(e)?;
    b.append_extension(KeyUsage::new().critical().key_cert_sign().crl_sign().build().map_err(e)?).map_err(e)?;
    let ski = SubjectKeyIdentifier::new().build(&b.x509v3_context(None, None)).map_err(e)?;
    b.append_extension(ski).map_err(e)?;
    // Even with its key, this CA can't vouch for anything outside spotify.com.
    #[allow(deprecated)] // openssl has no typed builder for name constraints
    let nc = X509Extension::new_nid(None, None, Nid::NAME_CONSTRAINTS, "critical,permitted;DNS:spotify.com").map_err(e)?;
    b.append_extension(nc).map_err(e)?;
    b.sign(&key, MessageDigest::sha256()).map_err(e)?;
    let cert = b.build();
    let mut f = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&key_path).map_err(e)?;
    f.write_all(&key.private_key_to_pem_pkcs8().map_err(e)?).map_err(e)?;
    fs::write(&cert_path, cert.to_pem().map_err(e)?).map_err(e)?;
    Ok((key, cert))
}

fn leaf_cert(host: &str, key: &PKey<Private>, ca_key: &PKey<Private>, ca: &X509) -> Res<X509> {
    let mut name = X509NameBuilder::new().map_err(e)?;
    name.append_entry_by_nid(Nid::COMMONNAME, host).map_err(e)?;
    let name = name.build();
    let mut b = X509Builder::new().map_err(e)?;
    b.set_version(2).map_err(e)?;
    let sn = serial()?;
    b.set_serial_number(&sn).map_err(e)?;
    b.set_subject_name(&name).map_err(e)?;
    b.set_issuer_name(ca.subject_name()).map_err(e)?;
    b.set_pubkey(key).map_err(e)?;
    // a day back, in case the clock is a little off
    let from = Asn1Time::from_unix(crate::app::unix_now().saturating_sub(86_400) as i64).map_err(e)?;
    let to = Asn1Time::days_from_now(365).map_err(e)?;
    b.set_not_before(&from).map_err(e)?;
    b.set_not_after(&to).map_err(e)?;
    b.append_extension(BasicConstraints::new().build().map_err(e)?).map_err(e)?;
    b.append_extension(KeyUsage::new().critical().digital_signature().build().map_err(e)?).map_err(e)?;
    b.append_extension(ExtendedKeyUsage::new().server_auth().build().map_err(e)?).map_err(e)?;
    let san = SubjectAlternativeName::new().dns(host).build(&b.x509v3_context(Some(ca), None)).map_err(e)?;
    b.append_extension(san).map_err(e)?;
    let aki = AuthorityKeyIdentifier::new().keyid(false).build(&b.x509v3_context(Some(ca), None)).map_err(e)?;
    b.append_extension(aki).map_err(e)?;
    b.sign(ca_key, MessageDigest::sha256()).map_err(e)?;
    Ok(b.build())
}

/// A certificate folder for Soloist: the system's (so every other host still checks out) plus
/// the relay's CA, in OpenSSL's hashed-name layout.
/// shortcut: needs the hashed /etc/ssl/certs that Arch, Debian and Ubuntu ship; a distro with
/// only a bundle file would need a split step (or SPOTIFY_SSL_CERT_FILE) here.
fn trust_dir(dir: &Path, ca: &X509) -> Res<PathBuf> {
    let certs = dir.join("certs");
    let _ = fs::remove_dir_all(&certs);
    fs::create_dir_all(&certs).map_err(e)?;
    let mut linked = 0;
    for entry in fs::read_dir("/etc/ssl/certs").map_err(|err| format!("no system certificates: {err}"))?.flatten() {
        let name = entry.file_name();
        let s = name.to_string_lossy();
        let hashed = s.len() >= 10 && s[..8].bytes().all(|b| b.is_ascii_hexdigit()) && s.as_bytes()[8] == b'.' && s[9..].bytes().all(|b| b.is_ascii_digit());
        if hashed && std::os::unix::fs::symlink(entry.path(), certs.join(&name)).is_ok() {
            linked += 1;
        }
    }
    if linked == 0 {
        return Err("no hashed system certificates in /etc/ssl/certs".into());
    }
    let hash = ca.subject_name_hash();
    let n = (0..).find(|n| !certs.join(format!("{hash:08x}.{n}")).exists()).unwrap_or(0);
    fs::write(certs.join(format!("{hash:08x}.{n}")), ca.to_pem().map_err(e)?).map_err(e)?;
    Ok(certs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use librespot_protocol::{
        connect::{Capabilities, Device, DeviceInfo, PutStateRequest},
        player::PlayerState,
    };
    use protobuf::{Message, MessageField};

    fn sample(private: bool) -> Vec<u8> {
        PutStateRequest {
            message_id: 7,
            device: MessageField::some(Device {
                device_info: MessageField::some(DeviceInfo {
                    name: "trill".into(),
                    volume: 40_000,
                    is_private_session: private,
                    capabilities: MessageField::some(Capabilities { volume_steps: 64, ..Default::default() }),
                    ..Default::default()
                }),
                player_state: MessageField::some(PlayerState { position_as_of_timestamp: 1234, ..Default::default() }),
                ..Default::default()
            }),
            ..Default::default()
        }
        .write_to_bytes()
        .unwrap()
    }

    #[test]
    fn marks_the_device_private_and_keeps_the_rest() {
        for gzip in [false, true] {
            let raw = sample(false);
            let body = if gzip {
                let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                gz.write_all(&raw).unwrap();
                gz.finish().unwrap()
            } else {
                raw
            };
            let out = patch_state(&body, gzip).expect("patched");
            let out = if gzip {
                let mut v = Vec::new();
                flate2::read::GzDecoder::new(&out[..]).read_to_end(&mut v).unwrap();
                v
            } else {
                out
            };
            let req = PutStateRequest::parse_from_bytes(&out).unwrap();
            let mut want = PutStateRequest::parse_from_bytes(&sample(true)).unwrap();
            want.special_fields = req.special_fields.clone();
            assert_eq!(req, want);
        }
        // already private: still exactly one flag
        let twice = mark_private(&mark_private(&sample(false)).unwrap()).unwrap();
        assert_eq!(twice, mark_private(&sample(false)).unwrap());
    }

    #[test]
    fn leaves_unfamiliar_bodies_alone() {
        assert!(mark_private(b"").is_none()); // no device
        assert!(mark_private(&[0x12, 0x05, 0x01]).is_none()); // truncated
        assert!(patch_state(b"not gzip", true).is_none());
        let no_info = PutStateRequest { message_id: 1, ..Default::default() }.write_to_bytes().unwrap();
        assert!(mark_private(&no_info).is_none());
    }

    #[test]
    fn only_spotify_connects_get_through() {
        assert_eq!(connect_target("CONNECT gue1-spclient.spotify.com:443 HTTP/1.1"), Some(("gue1-spclient.spotify.com".into(), 443)));
        assert_eq!(connect_target("CONNECT ap-gue1.spotify.com:4070 HTTP/1.1"), Some(("ap-gue1.spotify.com".into(), 4070)));
        assert!(connect_target("CONNECT evil.com:443 HTTP/1.1").is_none());
        assert!(connect_target("CONNECT notspotify.com:443 HTTP/1.1").is_none());
        assert!(connect_target("CONNECT spclient.wg.spotify.com:22 HTTP/1.1").is_none());
        assert!(connect_target("GET http://spotify.com/ HTTP/1.1").is_none());
        assert!(is_device_state("/connect-state/v1/devices/d8fafbf7-659c"));
        assert!(!is_device_state("/connect-state/v1/devices/d8fafbf7-659c/inactive"));
        assert!(is_spclient("gue1-spclient.spotify.com") && !is_spclient("dealer.spotify.com"));
    }

    #[test]
    fn ca_is_limited_to_spotify() {
        let dir = std::env::temp_dir().join(format!("trill-relay-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let (key, ca) = load_or_create_ca(&dir).unwrap();
        let text = String::from_utf8(ca.to_text().unwrap()).unwrap();
        assert!(text.contains("Permitted") && text.contains("DNS:spotify.com"), "{text}");
        let mode = std::os::unix::fs::PermissionsExt::mode(&fs::metadata(dir.join("ca.key")).unwrap().permissions());
        assert_eq!(mode & 0o777, 0o600);
        // reloads instead of remaking
        assert_eq!(load_or_create_ca(&dir).unwrap().1.to_der().unwrap(), ca.to_der().unwrap());
        // a leaf for an spclient host verifies against it
        let leaf = leaf_cert("gue1-spclient.spotify.com", &ec_key().unwrap(), &key, &ca).unwrap();
        assert!(leaf.verify(&ca.public_key().unwrap()).unwrap());
        let _ = fs::remove_dir_all(&dir);
    }
}
