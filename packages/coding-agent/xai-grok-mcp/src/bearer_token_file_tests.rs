use std::path::{Path, PathBuf};

use super::*;

fn token_file_at(path: &Path) -> BearerTokenFile {
    BearerTokenFile::new(BearerTokenPath::try_from(path.to_path_buf()).expect("absolute path"))
}

fn token_file(contents: &[u8]) -> (tempfile::TempDir, BearerTokenFile) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("token");
    std::fs::write(&path, contents).expect("write token");
    let file = token_file_at(&path);
    (dir, file)
}

async fn invalid_data_message(contents: &[u8]) -> String {
    let (_dir, file) = token_file(contents);
    let err = file.read().await.expect_err("invalid token file");
    assert_eq!(io::ErrorKind::InvalidData, err.kind(), "{err}");
    err.to_string()
}

#[tokio::test]
async fn blank_file_is_invalid_data() {
    let message = invalid_data_message(b" \n").await;
    assert!(message.ends_with("is empty"), "{message}");
}

#[tokio::test]
async fn oversized_file_is_invalid_data_without_echoing_contents() {
    let contents = vec![b'a'; usize::try_from(MAX_TOKEN_FILE_BYTES).expect("fits") + 1];
    let message = invalid_data_message(&contents).await;
    assert!(message.contains("larger than"), "{message}");
    assert!(!message.contains("aaaa"), "{message}");
}

#[tokio::test]
async fn non_utf8_file_is_invalid_data() {
    let message = invalid_data_message(b"tok\xff").await;
    assert!(message.ends_with("not UTF-8"), "{message}");
}

#[tokio::test]
async fn inner_control_byte_is_invalid_data_without_echoing_contents() {
    let message = invalid_data_message(b"secret\nsecond-line").await;
    assert!(
        message.contains("not allowed in an HTTP header"),
        "{message}"
    );
    assert!(!message.contains("secret"), "{message}");
}

#[tokio::test]
async fn missing_file_names_the_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("absent");
    let err = token_file_at(&path)
        .read()
        .await
        .expect_err("missing token file");
    assert_eq!(io::ErrorKind::NotFound, err.kind());
    assert!(
        err.to_string().contains(&path.display().to_string()),
        "{err}"
    );
}

#[tokio::test]
async fn exact_cap_and_surrounding_whitespace_are_valid() {
    let (_dir, file) = token_file(&vec![b't'; MAX_TOKEN_FILE_BYTES as usize]);
    assert_eq!(
        file.read().await.unwrap().len(),
        MAX_TOKEN_FILE_BYTES as usize
    );
    let (_dir, file) = token_file(b" \trotated-token\n");
    assert_eq!(file.read().await.unwrap(), "rotated-token");
}

#[tokio::test(flavor = "multi_thread")]
async fn healthy_concurrent_clones_recover_all_permits() {
    let (_dir, file) = token_file(b"healthy");
    let mut tasks = Vec::new();
    for _ in 0..32 {
        let f = file.clone();
        tasks.push(tokio::spawn(async move { f.read().await }));
    }
    for task in tasks {
        assert_eq!(task.await.unwrap().unwrap(), "healthy");
    }
    assert_eq!(file.gate.available_permits(), 1);
    assert!(file.started_at.lock().is_none());
}

#[cfg(unix)]
struct Fifo {
    dir: tempfile::TempDir,
    path: PathBuf,
    released: std::cell::Cell<bool>,
}
#[cfg(unix)]
impl Fifo {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        Self {
            dir,
            path,
            released: std::cell::Cell::new(false),
        }
    }
    fn release(&self) {
        if self.released.replace(true) {
            return;
        }
        use std::os::unix::fs::OpenOptionsExt;
        // Pair with the waiting reader using a real writer, then supply data and EOF.
        // A nonblocking open retries briefly rather than hanging test teardown.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&self.path)
            {
                Ok(mut writer) => {
                    use std::io::Write;
                    let _ = writer.write_all(b"released");
                    break;
                }
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(_) => break,
            }
        }
    }
}
#[cfg(unix)]
impl Drop for Fifo {
    fn drop(&mut self) {
        self.release();
        let _ = &self.dir;
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn stalled_job_survives_aborted_waiter_and_clones_fail_fast_until_recovery() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let fifo = Fifo::new();
    let file = BearerTokenFile {
        read_timeout: Duration::from_millis(100),
        ..token_file_at(&fifo.path)
    };
    let jobs = Arc::new(AtomicUsize::new(0));
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let f = file.clone();
    let count = Arc::clone(&jobs);
    let first = tokio::spawn(async move {
        f.read_with(move || {
            count.fetch_add(1, Ordering::SeqCst);
            let _ = started_tx.send(());
        })
        .await
    });
    started_rx.await.unwrap();
    first.abort();
    let _ = first.await;
    let mut tasks = Vec::new();
    for _ in 0..20 {
        let f = file.clone();
        let count = Arc::clone(&jobs);
        tasks.push(tokio::spawn(async move {
            f.read_with(move || {
                count.fetch_add(1, Ordering::SeqCst);
            })
            .await
        }));
    }
    for task in tasks {
        assert_eq!(
            task.await.unwrap().unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }
    assert_eq!(
        jobs.load(Ordering::SeqCst),
        1,
        "no cloned request may spawn a second filesystem job"
    );
    tokio::time::timeout(Duration::from_millis(30), file.read())
        .await
        .expect("stalled job fails fast")
        .unwrap_err();
    assert_eq!(
        file.gate.available_permits(),
        0,
        "aborted waiter cannot release the job's permit"
    );
    fifo.release();
    tokio::time::timeout(Duration::from_secs(2), async {
        while file.gate.available_permits() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    std::fs::remove_file(&fifo.path).unwrap();
    std::fs::write(&fifo.path, b"recovered").unwrap();
    assert_eq!(file.read().await.unwrap(), "recovered");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn timed_out_job_stays_guarded_until_filesystem_read_returns() {
    let fifo = Fifo::new();
    let file = BearerTokenFile {
        read_timeout: Duration::from_millis(100),
        ..token_file_at(&fifo.path)
    };
    assert_eq!(
        file.read().await.unwrap_err().kind(),
        io::ErrorKind::TimedOut
    );
    let error = tokio::time::timeout(Duration::from_millis(30), file.read())
        .await
        .expect("fail fast")
        .unwrap_err();
    assert!(error.to_string().contains("has not returned"));
    fifo.release();
    tokio::time::timeout(Duration::from_secs(2), async {
        while file.gate.available_permits() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}
