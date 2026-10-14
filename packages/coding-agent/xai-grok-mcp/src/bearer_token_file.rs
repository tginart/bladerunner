//! Per-request credentials, including atomic replacement and in-place rotation.
//! Blocking filesystem jobs retain their shared guard after timeout or caller cancellation.

use http::HeaderValue;
use std::io::{self, Read};
use std::sync::Arc;
use std::time::{Duration, Instant};
use xai_grok_config::BearerTokenPath;

const MAX_TOKEN_FILE_BYTES: u64 = 16 * 1024;
const TOKEN_READ_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct BearerTokenFile {
    path: Arc<BearerTokenPath>,
    read_timeout: Duration,
    gate: Arc<tokio::sync::Semaphore>,
    started_at: Arc<parking_lot::Mutex<Option<Instant>>>,
}

/// Clear the job timestamp before releasing the permit, even on panic.
struct ReadGuard {
    started_at: Arc<parking_lot::Mutex<Option<Instant>>>,
    _permit: tokio::sync::OwnedSemaphorePermit,
}
impl Drop for ReadGuard {
    fn drop(&mut self) {
        *self.started_at.lock() = None;
    }
}

impl BearerTokenFile {
    pub fn new(path: BearerTokenPath) -> Self {
        Self {
            path: Arc::new(path),
            read_timeout: TOKEN_READ_TIMEOUT,
            gate: Arc::new(tokio::sync::Semaphore::new(1)),
            started_at: Arc::default(),
        }
    }

    pub(crate) async fn read(&self) -> io::Result<String> {
        self.read_with(|| {}).await
    }

    async fn read_with(&self, before_read: impl FnOnce() + Send + 'static) -> io::Result<String> {
        if self
            .started_at
            .lock()
            .is_some_and(|at| at.elapsed() >= self.read_timeout)
        {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "an earlier read of bearer_token_file {} has not returned",
                    self.path
                ),
            ));
        }
        // The deadline covers acquiring the gate and completing the read. There is no
        // await between owning the permit and handing it to the blocking job.
        tokio::time::timeout(self.read_timeout, async {
            let permit = Arc::clone(&self.gate)
                .acquire_owned()
                .await
                .map_err(io::Error::other)?;
            *self.started_at.lock() = Some(Instant::now());
            let guard = ReadGuard {
                started_at: Arc::clone(&self.started_at),
                _permit: permit,
            };
            let path = Arc::clone(&self.path);
            tokio::task::spawn_blocking(move || {
                let _guard = guard;
                before_read();
                read_token_file_sync(&path)
            })
            .await
            .map_err(|_| io::Error::other("token file reader failed"))?
            .map_err(|e| io::Error::new(e.kind(), format!("bearer_token_file {}: {e}", self.path)))
        })
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("reading bearer_token_file {} timed out", self.path),
            )
        })?
    }
}

fn read_token_file_sync(path: &BearerTokenPath) -> io::Result<String> {
    let mut contents = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_TOKEN_FILE_BYTES + 1)
        .read_to_end(&mut contents)?;
    if contents.len() as u64 > MAX_TOKEN_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("larger than {MAX_TOKEN_FILE_BYTES} bytes"),
        ));
    }
    let contents = String::from_utf8(contents)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "not UTF-8"))?;
    let token = contents.trim();
    if token.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "is empty"));
    }
    if HeaderValue::from_str(&format!("Bearer {token}")).is_err() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "holds characters not allowed in an HTTP header",
        ));
    }
    Ok(token.to_owned())
}

#[cfg(test)]
#[path = "bearer_token_file_tests.rs"]
mod tests;
