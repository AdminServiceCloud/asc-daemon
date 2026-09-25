//! S3-compatible backup storage (DMN-115): AWS S3, MinIO, Backblaze B2,
//! Wasabi, Yandex Object Storage and anything else that speaks the S3 REST
//! API with Signature Version 4.
//!
//! Deliberately small instead of an SDK: the four operations a backup
//! storage needs (put, get, list, delete) plus multipart upload for large
//! archives, signed by hand (SigV4 is HMAC-SHA256 over a canonical request)
//! and sent with the blocking `ureq` client — the backup code already runs
//! on a blocking thread, and pulling a full async AWS SDK into an open
//! source daemon for four calls is the opposite of "minimal dependencies".
//!
//! Addressing: virtual-hosted style (`bucket.host/key`) for AWS endpoints,
//! path style (`host/bucket/key`) everywhere else — the same choice
//! minio-go's `BucketLookupAuto` makes, which is what the platform's own
//! connection check (services/storage) uses, so a storage that passes
//! "Проверить подключение" there is addressed the same way here.
//!
//! Payloads: uploads are sent as `UNSIGNED-PAYLOAD` (the body is streamed
//! from disk, hashing it first would read a multi-gigabyte archive twice);
//! every other request signs the real body hash.

use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use super::storage::{BackupObject, BackupStorage};

/// Archives up to this size go up in one `PUT`; bigger ones use multipart.
const SINGLE_PUT_LIMIT: u64 = 64 * 1024 * 1024;
/// Smallest multipart part (S3 minimum is 5 MiB for every part but the
/// last). Grown for huge archives to stay under the 10 000-part cap.
const MIN_PART_SIZE: u64 = 16 * 1024 * 1024;
const MAX_PARTS: u64 = 9_000;
/// SHA-256 of an empty body — what GET/DELETE/list requests sign.
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";

/// One configured S3 bucket (+ optional key prefix).
pub struct S3 {
    scheme: &'static str,
    /// `host[:port]` of the service endpoint, without the bucket.
    host: String,
    virtual_hosted: bool,
    region: String,
    bucket: String,
    access_key: String,
    secret_key: String,
    /// Normalized: no leading/trailing slash, may be empty.
    prefix: String,
    agent: ureq::Agent,
}

impl S3 {
    /// Build a client. `endpoint` may be empty (AWS for `region`), a bare
    /// host (`s3.example.com`, https implied) or a URL (`http://minio:9000`).
    pub fn new(
        endpoint: Option<&str>,
        region: &str,
        bucket: &str,
        access_key: &str,
        secret_key: &str,
        prefix: Option<&str>,
    ) -> Result<Self> {
        if bucket.trim().is_empty() {
            bail!("S3 storage has no bucket");
        }
        let region = match region.trim() {
            "" => "us-east-1".to_string(),
            r => r.to_string(),
        };
        let (scheme, host) = parse_endpoint(endpoint.unwrap_or(""), &region)?;
        let virtual_hosted = host.ends_with("amazonaws.com") && !bucket.contains('.');
        let config = ureq::Agent::config_builder()
            // Error bodies carry S3's <Code>/<Message> — read them instead of
            // getting an opaque "status 403".
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_recv_response(Some(Duration::from_secs(300)))
            .build();
        Ok(Self {
            scheme,
            host,
            virtual_hosted,
            region,
            bucket: bucket.trim().to_string(),
            access_key: access_key.to_string(),
            secret_key: secret_key.to_string(),
            prefix: prefix
                .unwrap_or("")
                .trim_matches('/')
                .split('/')
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("/"),
            agent: config.into(),
        })
    }

    /// Object key for a backup file name.
    fn key(&self, name: &str) -> String {
        if self.prefix.is_empty() {
            name.to_string()
        } else {
            format!("{}/{name}", self.prefix)
        }
    }

    fn request_host(&self) -> String {
        if self.virtual_hosted {
            format!("{}.{}", self.bucket, self.host)
        } else {
            self.host.clone()
        }
    }

    /// Canonical (already URI-encoded) path for a key, `""` for the bucket.
    fn path(&self, key: &str) -> String {
        let mut path = String::from("/");
        if !self.virtual_hosted {
            path.push_str(&uri_encode(&self.bucket, false));
            if !key.is_empty() {
                path.push('/');
            }
        }
        path.push_str(&uri_encode(key, true));
        path
    }

    /// Sign and send one request. `query` pairs are unencoded; `body` is
    /// what goes on the wire, `payload_hash` what is signed for it.
    fn send(
        &self,
        method: &str,
        key: &str,
        query: &[(&str, &str)],
        payload_hash: &str,
        body: Body,
    ) -> Result<ureq::http::Response<ureq::Body>> {
        let host = self.request_host();
        let path = self.path(key);
        let mut pairs: Vec<(String, String)> = query
            .iter()
            .map(|(k, v)| (uri_encode(k, false), uri_encode(v, false)))
            .collect();
        pairs.sort();
        let canonical_query = pairs
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");

        let (amz_date, date) = amz_timestamps(unix_now());
        let authorization = authorization(
            &Credentials {
                access_key: &self.access_key,
                secret_key: &self.secret_key,
                region: &self.region,
            },
            &Canonical {
                method,
                host: &host,
                path: &path,
                query: &canonical_query,
                payload_hash,
            },
            &amz_date,
            &date,
        );

        let mut url = format!("{}://{host}{path}", self.scheme);
        if !canonical_query.is_empty() {
            url.push('?');
            url.push_str(&canonical_query);
        }
        let builder = ureq::http::Request::builder()
            .method(method)
            .uri(&url)
            .header("x-amz-content-sha256", payload_hash)
            .header("x-amz-date", &amz_date)
            .header("authorization", authorization);
        let result = match body {
            Body::Empty => self.agent.run(builder.body(())?),
            Body::Bytes(bytes) => self.agent.run(builder.body(bytes)?),
            Body::File(file) => self.agent.run(builder.body(file)?),
        };
        result.with_context(|| format!("S3 {method} {} failed", self.describe(key)))
    }

    /// `s3://bucket/key` for error messages — never the credentials.
    fn describe(&self, key: &str) -> String {
        format!("s3://{}/{key}", self.bucket)
    }

    /// Turn a non-2xx response into an error carrying S3's own code.
    fn check(
        &self,
        what: &str,
        key: &str,
        mut response: ureq::http::Response<ureq::Body>,
    ) -> Result<ureq::http::Response<ureq::Body>> {
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let body = response
            .body_mut()
            .with_config()
            .limit(64 * 1024)
            .read_to_string()
            .unwrap_or_default();
        let code = xml_tag(&body, "Code").unwrap_or_default();
        let message = xml_tag(&body, "Message").unwrap_or_default();
        let detail = match (code.is_empty(), message.is_empty()) {
            (true, true) => status.to_string(),
            (false, true) => format!("{status} {code}"),
            (_, false) => format!("{status} {code}: {message}"),
        };
        bail!("S3 {what} {} rejected: {detail}", self.describe(key))
    }

    fn put_single(&self, local_archive: &Path, key: &str) -> Result<()> {
        let file = fs::File::open(local_archive)
            .with_context(|| format!("cannot open {}", local_archive.display()))?;
        let response = self.send("PUT", key, &[], UNSIGNED_PAYLOAD, Body::File(file))?;
        self.check("upload", key, response)?;
        Ok(())
    }

    fn put_multipart(&self, local_archive: &Path, key: &str, size: u64) -> Result<()> {
        let response = self.send("POST", key, &[("uploads", "")], EMPTY_SHA256, Body::Empty)?;
        let mut response = self.check("multipart start", key, response)?;
        let body = response
            .body_mut()
            .read_to_string()
            .context("cannot read multipart upload id")?;
        let upload_id = xml_tag(&body, "UploadId").context("S3 returned no UploadId")?;

        let result = self.upload_parts(local_archive, key, size, &upload_id);
        if result.is_err() {
            // Best-effort cleanup: an abandoned multipart upload keeps
            // billing for its parts until a lifecycle rule removes it.
            let _ = self
                .send(
                    "DELETE",
                    key,
                    &[("uploadId", &upload_id)],
                    EMPTY_SHA256,
                    Body::Empty,
                )
                .map(|r| self.check("multipart abort", key, r));
        }
        result
    }

    fn upload_parts(
        &self,
        local_archive: &Path,
        key: &str,
        size: u64,
        upload_id: &str,
    ) -> Result<()> {
        let part_size = MIN_PART_SIZE.max(size.div_ceil(MAX_PARTS));
        let mut file = fs::File::open(local_archive)
            .with_context(|| format!("cannot open {}", local_archive.display()))?;
        let mut etags = Vec::new();
        let mut number = 1u32;
        loop {
            // Parts are buffered: a Content-Length is mandatory for UploadPart,
            // and a bounded buffer (16 MiB by default) is what gives us one.
            let mut buf = Vec::with_capacity(part_size as usize);
            (&mut file)
                .take(part_size)
                .read_to_end(&mut buf)
                .with_context(|| format!("cannot read {}", local_archive.display()))?;
            if buf.is_empty() {
                break;
            }
            let part = number.to_string();
            let response = self.send(
                "PUT",
                key,
                &[("partNumber", &part), ("uploadId", upload_id)],
                UNSIGNED_PAYLOAD,
                Body::Bytes(buf),
            )?;
            let response = self.check("part upload", key, response)?;
            let etag = response
                .headers()
                .get("etag")
                .and_then(|v| v.to_str().ok())
                .context("S3 part upload returned no ETag")?
                .to_string();
            etags.push(etag);
            number += 1;
        }
        let mut xml = String::from("<CompleteMultipartUpload>");
        for (i, etag) in etags.iter().enumerate() {
            let _ = write!(
                xml,
                "<Part><PartNumber>{}</PartNumber><ETag>{}</ETag></Part>",
                i + 1,
                xml_escape(etag)
            );
        }
        xml.push_str("</CompleteMultipartUpload>");
        let hash = hex(&Sha256::digest(xml.as_bytes()));
        let response = self.send(
            "POST",
            key,
            &[("uploadId", upload_id)],
            &hash,
            Body::Bytes(xml.into_bytes()),
        )?;
        let mut response = self.check("multipart complete", key, response)?;
        // CompleteMultipartUpload can answer 200 with an <Error> body when
        // the assembly fails after the headers went out.
        let body = response.body_mut().read_to_string().unwrap_or_default();
        if body.contains("<Error>") {
            let code = xml_tag(&body, "Code").unwrap_or_default();
            let message = xml_tag(&body, "Message").unwrap_or_default();
            bail!(
                "S3 multipart complete {} failed: {code}: {message}",
                self.describe(key)
            );
        }
        Ok(())
    }
}

enum Body {
    Empty,
    Bytes(Vec<u8>),
    File(fs::File),
}

impl BackupStorage for S3 {
    fn push(&self, local_archive: &Path, remote_name: &str) -> Result<()> {
        let key = self.key(remote_name);
        let size = fs::metadata(local_archive)
            .with_context(|| format!("cannot stat {}", local_archive.display()))?
            .len();
        if size <= SINGLE_PUT_LIMIT {
            self.put_single(local_archive, &key)
        } else {
            self.put_multipart(local_archive, &key, size)
        }
    }

    fn pull(&self, remote_name: &str, local_dest: &Path) -> Result<()> {
        let key = self.key(remote_name);
        let response = self.send("GET", &key, &[], EMPTY_SHA256, Body::Empty)?;
        let response = self.check("download", &key, response)?;
        let mut reader = response
            .into_body()
            .into_with_config()
            .limit(u64::MAX)
            .reader();
        let mut out = fs::File::create(local_dest)
            .with_context(|| format!("cannot create {}", local_dest.display()))?;
        io::copy(&mut reader, &mut out)
            .with_context(|| format!("cannot download {}", self.describe(&key)))?;
        Ok(())
    }

    fn list(&self, app_id: &str) -> Result<Vec<BackupObject>> {
        let list_prefix = self.key(&format!("{app_id}-"));
        let strip = if self.prefix.is_empty() {
            0
        } else {
            self.prefix.len() + 1
        };
        let mut objects = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut query = vec![("list-type", "2"), ("prefix", list_prefix.as_str())];
            if let Some(t) = token.as_deref() {
                query.push(("continuation-token", t));
            }
            let response = self.send("GET", "", &query, EMPTY_SHA256, Body::Empty)?;
            let mut response = self.check("list", &list_prefix, response)?;
            let body = response
                .body_mut()
                .with_config()
                .limit(32 * 1024 * 1024)
                .read_to_string()
                .context("cannot read S3 listing")?;
            for contents in xml_blocks(&body, "Contents") {
                let Some(key) = xml_tag(contents, "Key") else {
                    continue;
                };
                let name = key.get(strip..).unwrap_or(&key).to_string();
                // Only this app's archives: the `<app>-` key prefix also
                // matches app `<app>-2`, a nested "folder" or a stray upload.
                if name.contains('/') || !super::storage::belongs_to(&name, app_id) {
                    continue;
                }
                let size = xml_tag(contents, "Size")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                objects.push(BackupObject { name, size });
            }
            let truncated = xml_tag(&body, "IsTruncated").is_some_and(|v| v == "true");
            token = xml_tag(&body, "NextContinuationToken");
            if !truncated || token.is_none() {
                break;
            }
        }
        objects.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(objects)
    }

    fn remove(&self, remote_name: &str) -> Result<()> {
        let key = self.key(remote_name);
        let response = self.send("DELETE", &key, &[], EMPTY_SHA256, Body::Empty)?;
        self.check("delete", &key, response)?;
        Ok(())
    }
}

/// `(scheme, host[:port])` from the configured endpoint.
fn parse_endpoint(raw: &str, region: &str) -> Result<(&'static str, String)> {
    let raw = raw.trim().trim_end_matches('/');
    if raw.is_empty() {
        return Ok(("https", format!("s3.{region}.amazonaws.com")));
    }
    let (scheme, rest) = match raw.split_once("://") {
        Some(("https", rest)) => ("https", rest),
        Some(("http", rest)) => ("http", rest),
        Some((other, _)) => bail!("S3 endpoint scheme '{other}' is not http or https"),
        None => ("https", raw),
    };
    // A path on the endpoint is not part of S3 addressing — refuse rather
    // than silently sign a different URL than the one that is sent.
    if rest.is_empty() || rest.contains('/') {
        bail!("S3 endpoint '{raw}' must be a host, optionally with a scheme and port");
    }
    Ok((scheme, rest.to_ascii_lowercase()))
}

/// RFC 3986 encoding the way SigV4 wants it: unreserved characters stay,
/// everything else is `%XX`; `/` stays only inside an object key path.
fn uri_encode(value: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            b'/' if keep_slash => out.push('/'),
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// `("YYYYMMDDTHHMMSSZ", "YYYYMMDD")` in UTC.
fn amz_timestamps(epoch_secs: i64) -> (String, String) {
    let epoch: libc::time_t = epoch_secs as libc::time_t;
    // SAFETY: gmtime_r writes only into the caller's buffer.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::gmtime_r(&epoch, &mut tm) };
    let date = format!(
        "{:04}{:02}{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday
    );
    let stamp = format!("{date}T{:02}{:02}{:02}Z", tm.tm_hour, tm.tm_min, tm.tm_sec);
    (stamp, date)
}

struct Credentials<'a> {
    access_key: &'a str,
    secret_key: &'a str,
    region: &'a str,
}

/// The already-encoded pieces of one request that SigV4 signs.
struct Canonical<'a> {
    method: &'a str,
    host: &'a str,
    path: &'a str,
    query: &'a str,
    payload_hash: &'a str,
}

/// The `Authorization` header value (SigV4, fixed signed-header set
/// `host;x-amz-content-sha256;x-amz-date`).
fn authorization(creds: &Credentials, req: &Canonical, amz_date: &str, date: &str) -> String {
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";
    let canonical_request = format!(
        "{}\n{}\n{}\nhost:{}\nx-amz-content-sha256:{}\nx-amz-date:{amz_date}\n\n{signed_headers}\n{}",
        req.method, req.path, req.query, req.host, req.payload_hash, req.payload_hash
    );
    let scope = format!("{date}/{}/s3/aws4_request", creds.region);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex(&Sha256::digest(canonical_request.as_bytes()))
    );
    let key = signing_key(creds.secret_key, date, creds.region);
    let signature = hex(&hmac_sha256(&key, string_to_sign.as_bytes()));
    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        creds.access_key
    )
}

fn signing_key(secret: &str, date: &str, region: &str) -> [u8; 32] {
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, b"s3");
    hmac_sha256(&k_service, b"aws4_request")
}

/// HMAC-SHA256 (RFC 2104) over the `sha2` crate already in the tree.
fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut block_key = [0u8; BLOCK];
    if key.len() > BLOCK {
        let digest: [u8; 32] = Sha256::digest(key).into();
        block_key[..32].copy_from_slice(&digest);
    } else {
        block_key[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= block_key[i];
        opad[i] ^= block_key[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(data);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner);
    outer.finalize().into()
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Text of the first `<tag>…</tag>` in `xml`, entity-decoded.
fn xml_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml_unescape(&xml[start..end]))
}

/// Inner text of every `<tag>…</tag>` block.
fn xml_blocks<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        let Some(end) = after.find(&close) else {
            break;
        };
        out.push(&after[..end]);
        rest = &after[end + close.len()..];
    }
    out
}

fn xml_unescape(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc4231_case_2() {
        // RFC 4231 test case 2: key "Jefe".
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn list_objects_signature_matches_aws_documentation() {
        // "GET Bucket (List Objects)" example from the AWS "Signature
        // Version 4 — header-based auth" documentation: exactly our signed
        // header set, so the whole signature must match byte for byte.
        let auth = authorization(
            &Credentials {
                access_key: "AKIAIOSFODNN7EXAMPLE",
                secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
                region: "us-east-1",
            },
            &Canonical {
                method: "GET",
                host: "examplebucket.s3.amazonaws.com",
                path: "/",
                query: "max-keys=2&prefix=J",
                payload_hash: EMPTY_SHA256,
            },
            "20130524T000000Z",
            "20130524",
        );
        assert!(
            auth.ends_with(
                "Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
            ),
            "{auth}"
        );
        assert!(auth.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date,"
        ));
    }

    #[test]
    fn timestamps_and_uri_encoding() {
        let (stamp, date) = amz_timestamps(1_369_353_600);
        assert_eq!(stamp, "20130524T000000Z");
        assert_eq!(date, "20130524");
        assert_eq!(uri_encode("a b/c+d", true), "a%20b/c%2Bd");
        assert_eq!(uri_encode("a/b", false), "a%2Fb");
    }

    #[test]
    fn endpoints_and_addressing() {
        let (scheme, host) = parse_endpoint("", "eu-west-1").unwrap();
        assert_eq!(
            (scheme, host.as_str()),
            ("https", "s3.eu-west-1.amazonaws.com")
        );
        let (scheme, host) = parse_endpoint("http://MinIO:9000/", "us-east-1").unwrap();
        assert_eq!((scheme, host.as_str()), ("http", "minio:9000"));
        let (scheme, host) = parse_endpoint("storage.yandexcloud.net", "ru-central1").unwrap();
        assert_eq!(
            (scheme, host.as_str()),
            ("https", "storage.yandexcloud.net")
        );
        assert!(parse_endpoint("ftp://x", "r").is_err());
        assert!(parse_endpoint("https://host/path", "r").is_err());

        let aws = S3::new(
            None,
            "eu-west-1",
            "bkt",
            "a",
            "s",
            Some("/team/asc-backups/node-1/"),
        )
        .unwrap();
        assert!(aws.virtual_hosted);
        assert_eq!(aws.request_host(), "bkt.s3.eu-west-1.amazonaws.com");
        assert_eq!(
            aws.key("app-1.tar.gz"),
            "team/asc-backups/node-1/app-1.tar.gz"
        );
        assert_eq!(aws.path("team/x y.tar.gz"), "/team/x%20y.tar.gz");

        let minio = S3::new(Some("http://minio:9000"), "", "bkt", "a", "s", None).unwrap();
        assert!(!minio.virtual_hosted);
        assert_eq!(minio.region, "us-east-1");
        assert_eq!(minio.path("app-1.tar.gz"), "/bkt/app-1.tar.gz");
        assert_eq!(minio.path(""), "/bkt");
    }

    /// Live round trip against a real S3-compatible server — skipped unless
    /// `ASC_TEST_S3_ENDPOINT` (plus `_BUCKET`, `_ACCESS_KEY`, `_SECRET_KEY`)
    /// point at one, e.g. a throwaway MinIO container. Covers a single PUT,
    /// a multipart upload (> SINGLE_PUT_LIMIT), listing with sizes, download
    /// and delete, and that a wrong secret is rejected with S3's own code.
    #[test]
    fn live_round_trip_when_configured() {
        let Ok(endpoint) = std::env::var("ASC_TEST_S3_ENDPOINT") else {
            eprintln!("ASC_TEST_S3_ENDPOINT not set — skipping live S3 test");
            return;
        };
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set"));
        let bucket = var("ASC_TEST_S3_BUCKET");
        let access = var("ASC_TEST_S3_ACCESS_KEY");
        let secret = var("ASC_TEST_S3_SECRET_KEY");
        let prefix = format!("asc-test/{}", std::process::id());
        let s3 = S3::new(
            Some(&endpoint),
            "us-east-1",
            &bucket,
            &access,
            &secret,
            Some(&prefix),
        )
        .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let small = dir.path().join("small.tar.gz");
        fs::write(&small, b"small archive").unwrap();
        let big = dir.path().join("big.tar.gz");
        {
            use std::io::Write;
            let mut f = fs::File::create(&big).unwrap();
            let chunk: Vec<u8> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
            for _ in 0..(SINGLE_PUT_LIMIT / (1024 * 1024) + 3) {
                f.write_all(&chunk).unwrap();
            }
        }
        let big_len = fs::metadata(&big).unwrap().len();

        s3.push(&small, "demo-100.tar.gz").unwrap();
        s3.push(&big, "demo-200.tar.gz").unwrap();
        s3.push(&small, "demo-2-300.tar.gz").unwrap();

        let listed = s3.list("demo").unwrap();
        let names: Vec<&str> = listed.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(
            names,
            ["demo-100.tar.gz", "demo-200.tar.gz"],
            "demo-2 must not leak in"
        );
        assert_eq!(listed[0].size, 13);
        assert_eq!(listed[1].size, big_len);

        let back = dir.path().join("back.tar.gz");
        s3.pull("demo-200.tar.gz", &back).unwrap();
        assert_eq!(fs::read(&back).unwrap(), fs::read(&big).unwrap());

        for name in ["demo-100.tar.gz", "demo-200.tar.gz", "demo-2-300.tar.gz"] {
            s3.remove(name).unwrap();
        }
        assert!(s3.list("demo").unwrap().is_empty());

        let wrong = S3::new(Some(&endpoint), "us-east-1", &bucket, &access, "nope", None).unwrap();
        let err = format!("{:#}", wrong.list("demo").unwrap_err());
        assert!(
            err.contains("SignatureDoesNotMatch") || err.contains("403"),
            "{err}"
        );
    }

    #[test]
    fn xml_helpers() {
        let body = "<ListBucketResult><IsTruncated>true</IsTruncated>\
            <Contents><Key>p/app-1.tar.gz</Key><Size>10</Size></Contents>\
            <Contents><Key>p/a&amp;b-2.tar.gz</Key><Size>20</Size></Contents>\
            <NextContinuationToken>tok</NextContinuationToken></ListBucketResult>";
        let blocks = xml_blocks(body, "Contents");
        assert_eq!(blocks.len(), 2);
        assert_eq!(xml_tag(blocks[1], "Key").unwrap(), "p/a&b-2.tar.gz");
        assert_eq!(xml_tag(body, "NextContinuationToken").unwrap(), "tok");
        assert_eq!(xml_escape("\"e&t\""), "&quot;e&amp;t&quot;");
    }
}
