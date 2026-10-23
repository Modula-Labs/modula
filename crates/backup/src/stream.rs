//! The two streaming pipelines:
//!
//! - backup: plaintext chunks → age encryption → fixed-size parts PUT to
//!   presigned multipart-upload URLs;
//! - restore: presigned GET → age decryption → plaintext chunks.
//!
//! At most one part of ciphertext is held in memory; plaintext is never held
//! beyond the chunk being encrypted, and nothing is written to disk.

use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use age::x25519::{Identity, Recipient};
use anyhow::{Context, bail};
use bytes::Bytes;
use futures::stream::BoxStream;
use futures::{AsyncReadExt, Stream, StreamExt, TryStreamExt};

/// S3's minimum size for every part but the last.
pub const MIN_PART_SIZE: usize = 5 * 1024 * 1024;
const PART_ATTEMPTS: u32 = 3;
const READ_CHUNK: usize = 64 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Uploaded {
    /// Ciphertext bytes uploaded.
    pub size_bytes: u64,
    /// One ETag per part, in part order (for CompleteMultipartUpload).
    pub etags: Vec<String>,
}

/// The encryptor's output: ciphertext waits here until a part is full.
#[derive(Clone, Default)]
struct Spool(Arc<Mutex<Vec<u8>>>);

impl Write for Spool {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("spool lock").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Spool {
    /// The next full part, once `size` bytes are waiting.
    fn take_part(&self, size: usize) -> Option<Bytes> {
        let mut buf = self.0.lock().expect("spool lock");
        if buf.len() < size {
            return None;
        }
        let rest = buf.split_off(size);
        Some(Bytes::from(std::mem::replace(&mut *buf, rest)))
    }

    fn take_rest(&self) -> Bytes {
        Bytes::from(std::mem::take(&mut *self.0.lock().expect("spool lock")))
    }
}

/// Encrypt `plaintext` to `recipient` and upload it as the parts of one
/// multipart upload: `part_urls[i]` is the presigned UploadPart URL of part
/// i+1, and every part except the last is exactly `part_size` bytes. Fails
/// (without completing anything) if the plaintext stream yields an error.
pub async fn encrypt_and_upload<S, E>(
    http: &reqwest::Client,
    mut plaintext: S,
    recipient: &Recipient,
    part_urls: &[String],
    part_size: usize,
) -> anyhow::Result<Uploaded>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: Into<anyhow::Error>,
{
    anyhow::ensure!(part_size > 0, "invalid part size");
    let spool = Spool::default();
    let mut writer = age::Encryptor::with_recipients(std::iter::once(recipient as _))?
        .wrap_output(spool.clone())?;
    let mut parts = Parts {
        http,
        urls: part_urls,
        uploaded: Uploaded {
            size_bytes: 0,
            etags: Vec::new(),
        },
    };
    while let Some(chunk) = plaintext.next().await {
        writer.write_all(&chunk.map_err(Into::into)?)?;
        while let Some(part) = spool.take_part(part_size) {
            parts.put(part).await?;
        }
    }
    // Writes the final (authenticated) chunk; without it the file is truncated.
    writer.finish()?;
    while let Some(part) = spool.take_part(part_size) {
        parts.put(part).await?;
    }
    let rest = spool.take_rest();
    if !rest.is_empty() || parts.uploaded.etags.is_empty() {
        parts.put(rest).await?;
    }
    Ok(parts.uploaded)
}

struct Parts<'a> {
    http: &'a reqwest::Client,
    urls: &'a [String],
    uploaded: Uploaded,
}

impl Parts<'_> {
    async fn put(&mut self, body: Bytes) -> anyhow::Result<()> {
        let n = self.uploaded.etags.len();
        let Some(url) = self.urls.get(n) else {
            bail!(
                "the backup is larger than its {} upload parts allow",
                self.urls.len()
            );
        };
        let mut attempt = 1;
        let etag = loop {
            match put_part(self.http, url, body.clone()).await {
                Ok(etag) => break etag,
                Err(e) if attempt >= PART_ATTEMPTS => {
                    return Err(e.context(format!("cannot upload part {}", n + 1)));
                }
                Err(_) => {
                    tokio::time::sleep(Duration::from_secs(u64::from(attempt) * 2)).await;
                    attempt += 1;
                }
            }
        };
        self.uploaded.size_bytes += body.len() as u64;
        self.uploaded.etags.push(etag);
        Ok(())
    }
}

async fn put_part(http: &reqwest::Client, url: &str, body: Bytes) -> anyhow::Result<String> {
    // Errors never carry the URL: it is a (short-lived) credential.
    let res = http
        .put(url)
        .body(body)
        .send()
        .await
        .map_err(reqwest::Error::without_url)?;
    let status = res.status();
    if !status.is_success() {
        bail!(
            "the storage answered {status}: {}",
            storage_error(res).await
        );
    }
    res.headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .context("the storage answered without an ETag")
}

/// A short, printable summary of an error response body (S3 sends XML).
pub async fn storage_error(res: reqwest::Response) -> String {
    let text = res.text().await.unwrap_or_default();
    let code = text
        .split_once("<Code>")
        .and_then(|(_, rest)| rest.split_once("</Code>"))
        .map(|(code, _)| code);
    match code {
        Some(code) => code.chars().take(100).collect(),
        None => text.chars().filter(|c| !c.is_control()).take(200).collect(),
    }
}

/// Download an encrypted backup from a presigned URL and decrypt it as it
/// arrives. Each yielded chunk has been authenticated; a truncated or
/// tampered file ends the stream with an error.
pub async fn download_and_decrypt(
    http: &reqwest::Client,
    url: &str,
    identity: &Identity,
) -> anyhow::Result<BoxStream<'static, anyhow::Result<Bytes>>> {
    let res = http
        .get(url)
        .send()
        .await
        .map_err(reqwest::Error::without_url)?;
    let status = res.status();
    if !status.is_success() {
        bail!(
            "cannot download the backup: the storage answered {status}: {}",
            storage_error(res).await
        );
    }
    let body = res
        .bytes_stream()
        .map_err(|e| io::Error::other(e.without_url()))
        .boxed()
        .into_async_read();
    let reader = age::Decryptor::new_async_buffered(body)
        .await?
        .decrypt_async(std::iter::once(identity as _))?;
    Ok(
        futures::stream::try_unfold(reader, |mut reader| async move {
            let mut buf = vec![0u8; READ_CHUNK];
            let n = reader.read(&mut buf).await?;
            if n == 0 {
                return Ok(None);
            }
            buf.truncate(n);
            Ok(Some((Bytes::from(buf), reader)))
        })
        .boxed(),
    )
}

/// Decrypt a whole backup from a reader (the restore CLI).
pub fn decrypt_reader<R: Read>(input: R, identity: &Identity) -> anyhow::Result<impl Read> {
    let decryptor = age::Decryptor::new_buffered(io::BufReader::new(input))?;
    Ok(decryptor.decrypt(std::iter::once(identity as _))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spool_hands_out_exact_parts() {
        let mut spool = Spool::default();
        spool.write_all(&[1; 25]).unwrap();
        assert_eq!(spool.take_part(10).unwrap().len(), 10);
        assert_eq!(spool.take_part(10).unwrap().len(), 10);
        assert!(spool.take_part(10).is_none());
        spool.write_all(&[2; 3]).unwrap();
        let rest = spool.take_rest();
        assert_eq!(&rest[..], &[1, 1, 1, 1, 1, 2, 2, 2]);
        assert!(spool.take_rest().is_empty());
    }

    #[test]
    fn spooled_ciphertext_decrypts() {
        let (recipient, identity) = crate::keys::generate();
        let recipient = crate::keys::parse_recipient(&recipient).unwrap();
        let identity = crate::keys::parse_identity(&identity).unwrap();
        let spool = Spool::default();
        let mut w = age::Encryptor::with_recipients(std::iter::once(&recipient as _))
            .unwrap()
            .wrap_output(spool.clone())
            .unwrap();
        let plaintext = vec![42u8; 200_000];
        w.write_all(&plaintext).unwrap();
        w.finish().unwrap();
        let mut parts = Vec::new();
        while let Some(p) = spool.take_part(65_536) {
            parts.extend_from_slice(&p);
        }
        parts.extend_from_slice(&spool.take_rest());
        assert_ne!(parts, plaintext);
        let mut out = Vec::new();
        decrypt_reader(&parts[..], &identity)
            .unwrap()
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out, plaintext);
    }
}
