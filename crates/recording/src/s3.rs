//! Putting one finished file into an object store, whole.
//!
//! # Why this is eighty lines of signing and not a dependency
//!
//! This crate makes exactly one kind of request: `PUT` an object it has just
//! finished writing. It does not list, copy, delete, tag, presign, assume a
//! role or walk a paginator. An SDK for that would bring a runtime's worth of
//! machinery, a credential-provider chain nobody here wants -- the credentials
//! are in the config file -- and a dependency tree larger than the rest of this
//! workspace, to send one request whose whole specification is a canonical
//! string and four HMACs. The workspace already has `hmac` and `sha2` because
//! it signs tokens and webhooks.
//!
//! The risk of writing a signature by hand is getting it wrong quietly, so the
//! signing has a known-answer test taken from AWS's own published example and
//! checked against an independent implementation -- see the tests at the bottom.
//!
//! # One `PUT` and no multipart
//!
//! S3 takes a single object up to five gigabytes in one request, and a
//! recording is never anywhere near that: Opus at the bitrate a browser sends
//! is a few megabytes an hour, so a full day of one participant is tens of
//! megabytes, and the recorder rolls a file over long before it could become a
//! problem. Multipart exists to make a very large object resumable and
//! parallel; here it would be machinery guarding against a size this cannot
//! reach.
//!
//! The body is streamed off the disk rather than read into memory, and the
//! payload is hashed by streaming it once first, so uploading a large recording
//! costs one buffer and not one copy of the file.

use std::io::Read;
use std::path::{Path, PathBuf};

use futures_util::TryStreamExt;
use hmac::{Hmac, Mac};
use mediacore_session::Recording;
use sha2::{Digest, Sha256};
use url::Url;

/// Where the object store is and how to speak to it.
#[derive(Debug, Clone)]
pub struct S3Settings {
    /// The endpoint, scheme and all: `https://s3.eu-central-1.amazonaws.com`,
    /// or whatever a MinIO or Ceph deployment answers on.
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    /// `<endpoint>/<bucket>/<key>` rather than `<bucket>.<endpoint>/<key>`.
    ///
    /// True by default, because the deployments this server is built for run
    /// their own object store, and MinIO and Ceph want path style. Amazon's own
    /// S3 wants the other one for buckets made since 2020.
    pub path_style: bool,
    /// Put in front of every key, so that one bucket can hold this server's
    /// recordings beside something else's.
    pub prefix: String,
}

/// What can go wrong on the way to the object store.
#[derive(Debug)]
pub enum UploadError {
    /// The file named on the recording is not on this node's disk. Either the
    /// row belongs to another node's disk, or the volume did not come back with
    /// the process.
    Missing(PathBuf),
    Io(std::io::Error),
    /// The request never got an answer: DNS, connection, timeout.
    Unreachable(String),
    /// The object store answered, and said no.
    Refused {
        status: u16,
        body: String,
    },
}

impl std::fmt::Display for UploadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(path) => write!(
                formatter,
                "the recording is not on this node's disk: {}",
                path.display()
            ),
            Self::Io(error) => write!(formatter, "the recording could not be read: {error}"),
            Self::Unreachable(what) => write!(formatter, "the object store did not answer: {what}"),
            Self::Refused { status, body } => {
                write!(formatter, "the object store refused it ({status}): {body}")
            }
        }
    }
}

impl std::error::Error for UploadError {}

/// One object store, ready to be handed files.
pub struct S3 {
    client: reqwest::Client,
    endpoint: Url,
    settings: S3Settings,
}

impl S3 {
    /// Builds the client, or says why the settings will never work.
    ///
    /// Called at startup, and its failures are all typos: an endpoint that is
    /// not a url, a bucket that is not a name. A server that started anyway
    /// would record for a day and be unable to upload any of it.
    pub fn new(settings: S3Settings, timeout: std::time::Duration) -> Result<Self, String> {
        let endpoint = Url::parse(settings.endpoint.trim_end_matches('/'))
            .map_err(|error| format!("recording.s3.endpoint is not a url: {error}"))?;

        if endpoint.host_str().is_none() {
            return Err("recording.s3.endpoint names no host".to_owned());
        }
        if settings.bucket.trim().is_empty() {
            return Err("recording.s3.bucket is empty".to_owned());
        }

        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|error| format!("the object store client could not be built: {error}"))?;

        Ok(Self {
            client,
            endpoint,
            settings,
        })
    }

    /// The bucket, for a log line and for the announcement.
    pub fn bucket(&self) -> &str {
        &self.settings.bucket
    }

    /// Where one recording goes in the bucket.
    ///
    /// Meeting first, because it is the high-cardinality part -- which is what
    /// keeps a bucket's prefixes spread out -- and because it is the prefix
    /// somebody deletes when a meeting turns out not to have been worth
    /// keeping. The rest is the participant and which of their files this is,
    /// which is exactly the key the database row carries, so an object can be
    /// found from a row and a row from an object without a lookup either way.
    pub fn key_for(&self, recording: &Recording) -> String {
        let prefix = self.settings.prefix.trim_matches('/');
        let name = format!(
            "{}/{}-{}.ogg",
            recording.session, recording.participant, recording.segment
        );

        if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        }
    }

    /// Puts a file in the object store under `key`.
    pub async fn put(&self, key: &str, path: &Path) -> Result<u64, UploadError> {
        let metadata = match tokio::fs::metadata(path).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(UploadError::Missing(path.to_path_buf()));
            }
            Err(error) => return Err(UploadError::Io(error)),
        };
        let length = metadata.len();

        // Hashing walks the file once before sending it. The alternative is
        // `UNSIGNED-PAYLOAD`, which S3 accepts and which would let a proxy
        // between here and the bucket change the bytes without the signature
        // noticing. One extra pass over a local file is cheap; a recording that
        // arrives altered and signed for is not.
        let hashing = path.to_path_buf();
        let payload_hash = tokio::task::spawn_blocking(move || digest_file(&hashing))
            .await
            .map_err(|error| UploadError::Unreachable(error.to_string()))?
            .map_err(UploadError::Io)?;

        let url = self.object_url(key);
        let host = authority(&url);
        let (amz_date, date) = utc(now_seconds());
        let headers = vec![
            ("host".to_owned(), host.clone()),
            ("x-amz-content-sha256".to_owned(), payload_hash.clone()),
            ("x-amz-date".to_owned(), amz_date.clone()),
        ];
        let authorization = authorization(
            "PUT",
            &encode_path(url.path()),
            "",
            &headers,
            &payload_hash,
            &amz_date,
            &date,
            &self.settings.region,
            "s3",
            &self.settings.access_key_id,
            &self.settings.secret_access_key,
        );

        let file = tokio::fs::File::open(path).await.map_err(UploadError::Io)?;
        let body =
            reqwest::Body::wrap_stream(tokio_util_free_stream(file).map_err(std::io::Error::other));

        let response = self
            .client
            .put(url)
            .header("x-amz-content-sha256", &payload_hash)
            .header("x-amz-date", &amz_date)
            .header("authorization", authorization)
            .header("content-type", "audio/ogg")
            .header(reqwest::header::CONTENT_LENGTH, length)
            .body(body)
            .send()
            .await
            .map_err(|error| UploadError::Unreachable(error.to_string()))?;

        let status = response.status();
        if status.is_success() {
            return Ok(length);
        }

        // The body of an S3 refusal is a small XML document naming the code,
        // and it is the only thing that tells "your clock is wrong" from "that
        // bucket does not exist". It goes into `last_error` on the work row,
        // truncated, because a queue row is not a place to keep a page of XML.
        let mut body = response.text().await.unwrap_or_default();
        body.truncate(500);

        Err(UploadError::Refused {
            status: status.as_u16(),
            body,
        })
    }

    fn object_url(&self, key: &str) -> Url {
        let mut url = self.endpoint.clone();

        if self.settings.path_style {
            url.set_path(&format!("/{}/{key}", self.settings.bucket));
            return url;
        }

        let host = url.host_str().unwrap_or_default().to_owned();
        let _ = url.set_host(Some(&format!("{}.{host}", self.settings.bucket)));
        url.set_path(&format!("/{key}"));
        url
    }
}

/// The file as a stream of chunks, without pulling in a compatibility crate for
/// it: one 64 KiB buffer, refilled until the file runs out.
fn tokio_util_free_stream(
    file: tokio::fs::File,
) -> impl futures_util::Stream<Item = std::io::Result<bytes::Bytes>> {
    futures_util::stream::try_unfold(file, |mut file| async move {
        use tokio::io::AsyncReadExt as _;

        let mut buffer = vec![0u8; 64 * 1024];
        let read = file.read(&mut buffer).await?;

        if read == 0 {
            return Ok(None);
        }

        buffer.truncate(read);
        Ok(Some((bytes::Bytes::from(buffer), file)))
    })
}

/// The `Host` header the request will carry, which has to be the one that is
/// signed: with the port when it is not the scheme's default, without it when
/// it is, exactly as every HTTP client writes it.
fn authority(url: &Url) -> String {
    match url.port() {
        Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
        None => url.host_str().unwrap_or_default().to_owned(),
    }
}

fn digest_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];

    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(hex(&hasher.finalize()))
}

/// The Authorization header for one request, AWS Signature Version 4.
///
/// Written against the specification rather than against S3 in particular --
/// the service and region are arguments -- so that the test at the bottom can
/// drive it with AWS's own published example, which is for IAM.
#[allow(clippy::too_many_arguments)]
fn authorization(
    method: &str,
    path: &str,
    query: &str,
    headers: &[(String, String)],
    payload_hash: &str,
    amz_date: &str,
    date: &str,
    region: &str,
    service: &str,
    key_id: &str,
    secret: &str,
) -> String {
    let mut headers = headers.to_vec();
    headers.sort_by(|left, right| left.0.cmp(&right.0));

    let signed: Vec<&str> = headers.iter().map(|(name, _)| name.as_str()).collect();
    let signed = signed.join(";");
    let canonical_headers: String = headers
        .iter()
        .map(|(name, value)| format!("{name}:{}\n", value.trim()))
        .collect();

    let canonical =
        format!("{method}\n{path}\n{query}\n{canonical_headers}\n{signed}\n{payload_hash}");
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex(&Sha256::digest(canonical.as_bytes()))
    );

    let key = hmac(
        &hmac(
            &hmac(
                &hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes()),
                region.as_bytes(),
            ),
            service.as_bytes(),
        ),
        b"aws4_request",
    );
    let signature = hex(&hmac(&key, to_sign.as_bytes()));

    format!(
        "AWS4-HMAC-SHA256 Credential={key_id}/{scope}, SignedHeaders={signed}, \
         Signature={signature}"
    )
}

fn hmac(key: &[u8], message: &[u8]) -> Vec<u8> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("hmac takes a key of any size");
    mac.update(message);
    mac.finalize().into_bytes().to_vec()
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);

    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }

    out
}

/// The path as the canonical request wants it: every segment percent-encoded,
/// with the slashes between them left alone.
///
/// `Url` has already encoded the path once for the wire, and its rules are not
/// AWS's -- it leaves characters alone that the canonical request wants encoded.
/// Encoding what it produced would double every escape, so this decodes nothing
/// and re-encodes only what AWS calls unreserved.
fn encode_path(path: &str) -> String {
    path.split('/')
        .map(|segment| {
            percent_decode(segment)
                .iter()
                .map(|byte| {
                    if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                        (*byte as char).to_string()
                    } else {
                        format!("%{byte:02X}")
                    }
                })
                .collect::<String>()
        })
        .collect::<Vec<String>>()
        .join("/")
}

fn percent_decode(segment: &str) -> Vec<u8> {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;

    while at < bytes.len() {
        if bytes[at] == b'%'
            && at + 2 < bytes.len()
            && let (Some(high), Some(low)) = (
                (bytes[at + 1] as char).to_digit(16),
                (bytes[at + 2] as char).to_digit(16),
            )
        {
            out.push((high * 16 + low) as u8);
            at += 3;
            continue;
        }

        out.push(bytes[at]);
        at += 1;
    }

    out
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// `20260907T101500Z` and `20260907`, which are the two forms a signature
/// wants.
///
/// Done here rather than with a date library, because a date library is a
/// dependency and this is the civil-from-days arithmetic every one of them
/// contains. There is no time zone and no locale: the only calendar a signature
/// knows about is UTC.
fn utc(epoch_seconds: i64) -> (String, String) {
    let days = epoch_seconds.div_euclid(86_400);
    let seconds = epoch_seconds.rem_euclid(86_400);

    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;

    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);

    let date = format!("{year:04}{month:02}{day:02}");
    let hour = seconds / 3_600;
    let minute = (seconds / 60) % 60;
    let second = seconds % 60;

    (format!("{date}T{hour:02}{minute:02}{second:02}Z"), date)
}

#[cfg(test)]
#[path = "s3_tests.rs"]
mod tests;
