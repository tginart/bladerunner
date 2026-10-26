//! Purpose: Own a private Cua daemon for the built-in computer-use MCP server.
//! Input: The installed driver path and the MCP environment.
//! Output: A private socket and a daemon bound to the parent's lifetime pipe.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::EnvVariable;
use tempfile::TempDir;
use tokio::process::{Child, Command};
use xai_grok_tools::util::{ProcessGroup, ProcessScope};

const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// Purpose: Keep the daemon's lifetime pipe and private socket directory alive.
/// Input: A direct child process and its socket directory.
/// Output: Automatic daemon shutdown when the MCP transport closes.
pub(crate) struct CuaDaemon {
    child: Option<Child>,
    process_group: Option<Arc<ProcessGroup>>,
    directory: Option<TempDir>,
    socket_path: PathBuf,
}

impl CuaDaemon {
    /// Purpose: Start the daemon as a direct child and wait for its private socket.
    /// Input: The executable, environment, startup limit, and session process scope.
    /// Output: A live daemon guard or a startup error.
    pub(crate) async fn start(
        executable: &OsStr,
        environment: &[EnvVariable],
        timeout: Duration,
        scope: Option<&ProcessScope>,
    ) -> io::Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix("br-cua-")
            .tempdir_in("/tmp")?;
        let socket_path = directory.path().join("driver.sock");
        let log_directory = xai_grok_config::grok_home().join("logs/mcp");
        std::fs::create_dir_all(&log_directory)?;
        let log_path = log_directory.join("computer-use-daemon.stderr.log");
        let mut log_options = std::fs::OpenOptions::new();
        log_options.create(true).append(true);
        {
            use std::os::unix::fs::OpenOptionsExt;
            log_options.mode(0o600);
        }
        let log = log_options.open(&log_path)?;
        let mut command = Command::new(executable);
        command
            .args(["serve", "--embedded", "--socket"])
            .arg(&socket_path)
            .arg("--pid-file")
            .arg(directory.path().join("driver.pid"))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            // Cua writes a shutdown message after lifetime EOF. A pipe whose
            // reader died with the parent can panic its GUI worker on EPIPE.
            .stderr(Stdio::from(log))
            .kill_on_drop(true);
        for variable in environment {
            command.env(&variable.name, &variable.value);
        }
        command.env("CUA_DRIVER_EMBEDDED", "1");
        command.env("CUA_DRIVER_PARENT_LIVENESS_STDIN", "1");
        xai_grok_tools::util::detach_command(&mut command);
        #[allow(clippy::disallowed_methods)] // enrolled in the session scope below
        let child = command.spawn()?;
        let mut group = ProcessGroup::new()?;
        group.attach(&child)?;
        let group = Arc::new(group);
        let mut daemon = Self {
            child: Some(child),
            process_group: Some(group.clone()),
            directory: Some(directory),
            socket_path,
        };
        if let Some(scope) = scope
            && !scope.register(&group)
        {
            return Err(io::Error::other(
                "session closed before the Cua daemon started",
            ));
        }
        tokio::time::timeout(timeout, async {
            loop {
                if let Some(status) = daemon.child.as_mut().expect("daemon child").try_wait()? {
                    return Err(io::Error::other(format!("Cua daemon exited: {status}")));
                }
                if tokio::net::UnixStream::connect(&daemon.socket_path)
                    .await
                    .is_ok()
                {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Cua daemon startup timed out"))??;
        Ok(daemon)
    }

    pub(crate) fn proxy_args(&self) -> Vec<String> {
        vec![
            "mcp".to_string(),
            "--embedded".to_string(),
            "--socket".to_string(),
            self.socket_path.to_string_lossy().into_owned(),
        ]
    }
}

impl Drop for CuaDaemon {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        // Only Bladerunner holds this write end. A hard parent kill closes
        // it too, without relying on this destructor or a signal handler.
        drop(child.stdin.take());
        let group = self.process_group.take();
        let directory = self.directory.take();
        let cleanup = async move {
            let result = tokio::time::timeout(SHUTDOWN_GRACE, child.wait()).await;
            if !matches!(result, Ok(Ok(_))) {
                if let Ok(Err(error)) = result {
                    tracing::warn!(%error, "Could not wait for the Cua daemon");
                }
                if let Some(group) = &group
                    && let Err(error) = group.kill()
                {
                    tracing::warn!(%error, "Could not stop the Cua daemon process group");
                }
                if let Err(error) = child.kill().await {
                    tracing::warn!(%error, "Could not reap the Cua daemon");
                }
            }
            drop(directory);
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(cleanup);
        } else if let Err(error) = std::thread::Builder::new()
            .name("cua-daemon-cleanup".to_string())
            .spawn(move || {
                match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime.block_on(cleanup),
                    Err(error) => {
                        tracing::warn!(%error, "Could not create the Cua cleanup runtime")
                    }
                }
            })
        {
            tracing::warn!(%error, "Could not start the Cua cleanup thread");
        }
    }
}

pub(crate) fn is_embedded_cua(name: &str, executable: &str, args: &[String]) -> bool {
    name == "computer-use"
        && Path::new(executable)
            .file_name()
            .is_some_and(|name| name == "cua-driver")
        && args == ["mcp", "--embedded"]
}
