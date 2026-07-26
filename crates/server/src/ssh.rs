//! SSH executor: the only way the control plane touches a target.
//!
//! Targets run no agent. Every operation — probing, writing unit files,
//! uploading releases, reading journald, opening a PTY — is an SSH channel
//! opened from here. That keeps the target clean (nothing but the OS, systemd
//! and the deployed binary) and keeps the control plane out of the hot path.
//!
//! Connections are made per operation rather than pooled. A latency-critical
//! box should not have a long-lived connection from the control plane sitting
//! against it, and SSH handshake cost is irrelevant next to the work each
//! operation does.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use russh::keys::PrivateKeyWithHashAlg;
use russh::{ChannelMsg, client};
use tokio::sync::mpsc;

/// How long to wait for the TCP connect and SSH handshake.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Everything needed to reach one target. Assembled server-side from the
/// database; a client never supplies these fields.
#[derive(Clone)]
pub struct SshTarget {
    pub host: String,
    pub port: u16,
    pub user: String,
    /// OpenSSH-format private key material, already decrypted from the secret
    /// store.
    pub private_key: String,
    /// Passphrase for the key, if it is encrypted.
    pub passphrase: Option<String>,
}

impl std::fmt::Debug for SshTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never let key material reach a log line.
        f.debug_struct("SshTarget")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("private_key", &"<redacted>")
            .finish()
    }
}

/// Result of a one-shot command.
#[derive(Debug, Clone)]
pub struct CommandResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl CommandResult {
    /// Whether the command succeeded.
    pub fn ok(&self) -> bool {
        self.exit_code == 0
    }

    /// stdout with surrounding whitespace removed — almost every caller wants
    /// this rather than the raw bytes.
    pub fn trimmed(&self) -> &str {
        self.stdout.trim()
    }

    /// Turns a non-zero exit into an error carrying stderr, which is what makes
    /// deployment failures legible in the UI.
    pub fn require_success(&self, what: &str) -> anyhow::Result<&Self> {
        if self.ok() {
            return Ok(self);
        }
        let detail = if self.stderr.trim().is_empty() {
            self.stdout.trim()
        } else {
            self.stderr.trim()
        };
        bail!("{what} failed (exit {}): {detail}", self.exit_code)
    }
}

/// A `russh` client handler.
///
/// Host keys are accepted on first use. The control plane reaches targets the
/// operator has explicitly registered, and there is no interactive prompt to
/// approve a fingerprint; recording and pinning host keys is noted as deferred
/// in CHANGES.md.
struct Handler;

impl client::Handler for Handler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

/// An authenticated SSH session against one target.
pub struct SshSession {
    handle: client::Handle<Handler>,
}

impl SshSession {
    /// Connects and authenticates with the target's private key.
    pub async fn connect(target: &SshTarget) -> anyhow::Result<Self> {
        let config = Arc::new(client::Config {
            inactivity_timeout: Some(Duration::from_secs(3600)),
            ..Default::default()
        });

        let key = russh::keys::decode_secret_key(
            &target.private_key,
            target.passphrase.as_deref(),
        )
        .context(
            "decoding the target's SSH private key — it must be an OpenSSH or PEM private key",
        )?;

        let addr = (target.host.as_str(), target.port);
        let mut handle = tokio::time::timeout(CONNECT_TIMEOUT, client::connect(config, addr, Handler))
            .await
            .map_err(|_| {
                anyhow!(
                    "timed out connecting to {}:{} after {}s",
                    target.host,
                    target.port,
                    CONNECT_TIMEOUT.as_secs()
                )
            })?
            .with_context(|| format!("connecting to {}:{}", target.host, target.port))?;

        let best_hash = handle.best_supported_rsa_hash().await?.flatten();
        let auth = handle
            .authenticate_publickey(
                target.user.clone(),
                PrivateKeyWithHashAlg::new(Arc::new(key), best_hash),
            )
            .await
            .context("public-key authentication")?;

        if !auth.success() {
            bail!(
                "public-key authentication failed for {}@{}",
                target.user,
                target.host
            );
        }

        Ok(Self { handle })
    }

    /// Runs a command to completion, collecting stdout and stderr.
    ///
    /// Suitable for the short commands this control plane issues (`systemctl
    /// show`, `ln -sfn`, `mkdir`). Use [`SshSession::exec_streaming`] for
    /// anything whose output is unbounded, such as a build or `journalctl -f`.
    pub async fn exec(&self, command: &str) -> anyhow::Result<CommandResult> {
        let mut channel = self.handle.channel_open_session().await?;
        channel.exec(true, command).await?;

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        // Absent an explicit exit status (the peer closed without sending one)
        // treat it as failure rather than silently reporting success.
        let mut exit_code = -1;

        while let Some(msg) = channel.wait().await {
            match msg {
                ChannelMsg::Data { data } => stdout.extend_from_slice(&data),
                ChannelMsg::ExtendedData { data, ext } => {
                    // ext 1 is stderr; anything else is not something we asked
                    // for, so fold it into stderr rather than dropping it.
                    let _ = ext;
                    stderr.extend_from_slice(&data);
                }
                ChannelMsg::ExitStatus { exit_status } => exit_code = exit_status as i32,
                ChannelMsg::Eof | ChannelMsg::Close => break,
                _ => {}
            }
        }

        Ok(CommandResult {
            exit_code,
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        })
    }

    /// Runs a command with a wall-clock limit.
    pub async fn exec_timeout(
        &self,
        command: &str,
        timeout: Duration,
    ) -> anyhow::Result<CommandResult> {
        tokio::time::timeout(timeout, self.exec(command))
            .await
            .map_err(|_| anyhow!("command timed out after {}s", timeout.as_secs()))?
    }

    /// Runs a command, forwarding output lines to `sink` as they arrive.
    ///
    /// This is the shape live deploy output and `journalctl --follow` need:
    /// output must reach the dashboard while the command is still running, not
    /// after it finishes. Partial lines are buffered until a newline arrives so
    /// a consumer never sees a line split across two events.
    pub async fn exec_streaming(
        &self,
        command: &str,
        sink: mpsc::Sender<OutputLine>,
    ) -> anyhow::Result<i32> {
        let mut channel = self.handle.channel_open_session().await?;
        channel.exec(true, command).await?;

        let mut exit_code = -1;
        let mut stdout_buf = Vec::new();
        let mut stderr_buf = Vec::new();

        while let Some(msg) = channel.wait().await {
            match msg {
                ChannelMsg::Data { data } => {
                    if !drain_lines(&mut stdout_buf, &data, false, &sink).await {
                        // Receiver is gone — the browser navigated away or the
                        // CLI detached. Stop rather than filling memory.
                        break;
                    }
                }
                ChannelMsg::ExtendedData { data, .. } => {
                    if !drain_lines(&mut stderr_buf, &data, true, &sink).await {
                        break;
                    }
                }
                ChannelMsg::ExitStatus { exit_status } => exit_code = exit_status as i32,
                ChannelMsg::Eof | ChannelMsg::Close => break,
                _ => {}
            }
        }

        // Emit whatever was left without a trailing newline, so the last line
        // of output is never swallowed.
        flush_remainder(stdout_buf, false, &sink).await;
        flush_remainder(stderr_buf, true, &sink).await;

        Ok(exit_code)
    }

    /// Writes `contents` to `path` on the target, creating parent directories.
    ///
    /// Uses `base64 -d` on the far side rather than a heredoc so the payload
    /// cannot terminate the shell command early — a unit file or secret value
    /// containing the delimiter, a quote, or a newline is transferred exactly.
    pub async fn write_file(
        &self,
        path: &str,
        contents: &[u8],
        mode: Option<&str>,
    ) -> anyhow::Result<()> {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(contents);

        let parent = parent_dir(path);
        let mut command = format!(
            "mkdir -p {} && printf '%s' {} | base64 -d > {}",
            quote(&parent),
            quote(&encoded),
            quote(path)
        );
        if let Some(mode) = mode {
            command.push_str(&format!(" && chmod {} {}", quote(mode), quote(path)));
        }

        self.exec(&command)
            .await?
            .require_success(&format!("writing {path}"))?;
        Ok(())
    }

    /// Uploads bytes to `path` in chunks, reporting progress.
    ///
    /// Release binaries can be hundreds of megabytes, which is too large for a
    /// single command line, so the payload is appended chunk by chunk. Each
    /// chunk is base64 so no byte sequence can escape the shell.
    pub async fn upload_file(
        &self,
        path: &str,
        contents: &[u8],
        mode: Option<&str>,
        progress: Option<&mpsc::Sender<OutputLine>>,
    ) -> anyhow::Result<()> {
        use base64::Engine as _;

        // Sized to stay well inside a conservative ARG_MAX once base64 has
        // inflated it by 4/3.
        const CHUNK: usize = 512 * 1024;

        let parent = parent_dir(path);
        self.exec(&format!(
            "mkdir -p {} && : > {}",
            quote(&parent),
            quote(path)
        ))
        .await?
        .require_success(&format!("creating {path}"))?;

        let total = contents.len();
        let mut written = 0usize;

        for chunk in contents.chunks(CHUNK) {
            let encoded = base64::engine::general_purpose::STANDARD.encode(chunk);
            self.exec(&format!(
                "printf '%s' {} | base64 -d >> {}",
                quote(&encoded),
                quote(path)
            ))
            .await?
            .require_success("uploading chunk")?;

            written += chunk.len();
            if let Some(sink) = progress {
                let percent = if total == 0 { 100 } else { written * 100 / total };
                let _ = sink
                    .send(OutputLine::stdout(format!(
                        "uploaded {}/{} bytes ({percent}%)",
                        written, total
                    )))
                    .await;
            }
        }

        // Verify the transfer rather than trusting it: a truncated binary that
        // systemd then tries to exec is a much worse failure than a failed
        // deploy.
        let remote_size = self
            .exec(&format!(
                "wc -c < {} | tr -d ' \\n'",
                quote(path)
            ))
            .await?;
        let remote_size: usize = remote_size
            .trimmed()
            .parse()
            .with_context(|| format!("reading back the size of {path}"))?;
        if remote_size != total {
            bail!(
                "upload of {path} is incomplete: wrote {total} bytes, target has {remote_size}"
            );
        }

        if let Some(mode) = mode {
            self.exec(&format!("chmod {} {}", quote(mode), quote(path)))
                .await?
                .require_success(&format!("chmod {path}"))?;
        }

        Ok(())
    }

    /// Opens an interactive PTY channel for the terminal feature.
    ///
    /// Returns the channel so the caller can pump bytes both ways and issue
    /// window-change requests on resize.
    pub async fn open_pty(
        &self,
        cols: u32,
        rows: u32,
        initial_command: Option<&str>,
    ) -> anyhow::Result<russh::Channel<client::Msg>> {
        let channel = self.handle.channel_open_session().await?;

        channel
            .request_pty(
                true,
                "xterm-256color",
                cols.clamp(1, 1000),
                rows.clamp(1, 1000),
                0,
                0,
                &[],
            )
            .await
            .context("requesting a PTY")?;

        match initial_command {
            // A command is run instead of a login shell when the caller asked
            // for one; `exec` with a PTY is what gives it job control.
            Some(command) if !command.trim().is_empty() => {
                channel.exec(true, command).await.context("exec on PTY")?;
            }
            _ => {
                channel.request_shell(true).await.context("requesting a shell")?;
            }
        }

        Ok(channel)
    }

    /// Closes the session cleanly so the target does not accumulate half-open
    /// connections.
    pub async fn close(&self) -> anyhow::Result<()> {
        self.handle
            .disconnect(russh::Disconnect::ByApplication, "", "en")
            .await?;
        Ok(())
    }
}

/// One line of output from a streamed command.
#[derive(Debug, Clone)]
pub struct OutputLine {
    pub text: String,
    pub stderr: bool,
}

impl OutputLine {
    pub fn stdout(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            stderr: false,
        }
    }

    pub fn stderr(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            stderr: true,
        }
    }
}

/// Appends `data` to `buf` and forwards every complete line to `sink`.
/// Returns false once the receiver has hung up.
async fn drain_lines(
    buf: &mut Vec<u8>,
    data: &[u8],
    stderr: bool,
    sink: &mpsc::Sender<OutputLine>,
) -> bool {
    buf.extend_from_slice(data);

    while let Some(newline) = buf.iter().position(|b| *b == b'\n') {
        let line: Vec<u8> = buf.drain(..=newline).collect();
        let text = String::from_utf8_lossy(&line)
            .trim_end_matches(['\n', '\r'])
            .to_string();
        let line = if stderr {
            OutputLine::stderr(text)
        } else {
            OutputLine::stdout(text)
        };
        if sink.send(line).await.is_err() {
            return false;
        }
    }

    // Guard against a command that emits megabytes with no newline at all.
    const MAX_PARTIAL: usize = 1024 * 1024;
    if buf.len() > MAX_PARTIAL {
        let text = String::from_utf8_lossy(buf).into_owned();
        buf.clear();
        let line = if stderr {
            OutputLine::stderr(text)
        } else {
            OutputLine::stdout(text)
        };
        if sink.send(line).await.is_err() {
            return false;
        }
    }

    true
}

/// Emits a trailing partial line, if any.
async fn flush_remainder(buf: Vec<u8>, stderr: bool, sink: &mpsc::Sender<OutputLine>) {
    if buf.is_empty() {
        return;
    }
    let text = String::from_utf8_lossy(&buf)
        .trim_end_matches(['\n', '\r'])
        .to_string();
    if text.is_empty() {
        return;
    }
    let line = if stderr {
        OutputLine::stderr(text)
    } else {
        OutputLine::stdout(text)
    };
    let _ = sink.send(line).await;
}

/// The directory portion of an absolute path.
fn parent_dir(path: &str) -> String {
    match path.rfind('/') {
        Some(0) => "/".to_string(),
        Some(idx) => path[..idx].to_string(),
        None => ".".to_string(),
    }
}

/// Quotes a value for safe interpolation into a POSIX shell command.
///
/// Every value the control plane sends to a target goes through here. SSH has
/// no argv — the remote side gets one string that its shell parses — so a
/// service name, path or secret containing a quote or semicolon would otherwise
/// be command injection.
pub fn quote(value: &str) -> String {
    shell_escape::unix::escape(std::borrow::Cow::Borrowed(value)).into_owned()
}

/// Joins a command and its arguments into a shell-safe string.
pub fn join_command(command: &str, args: &[String]) -> String {
    let mut out = quote(command);
    for arg in args {
        out.push(' ');
        out.push_str(&quote(arg));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_neutralizes_shell_metacharacters() {
        // These are the sequences that would otherwise let a service name or a
        // secret value run a second command on the target.
        for hostile in [
            "; rm -rf /",
            "$(whoami)",
            "`id`",
            "a && b",
            "a | b",
            "new\nline",
            "quote'inside",
            "$HOME",
        ] {
            let quoted = quote(hostile);
            // Single-quoted, with any embedded quote broken out and re-escaped;
            // nothing inside can be interpreted by the shell.
            assert!(
                quoted.starts_with('\'') || !quoted.contains(|c: char| ";|&$`\n".contains(c)),
                "{hostile:?} quoted as {quoted:?} still looks interpretable"
            );
        }
    }

    #[test]
    fn quoting_leaves_ordinary_paths_readable() {
        // Over-quoting would make every generated command unreadable in logs.
        assert_eq!(quote("/opt/hft-bot/current/bin"), "/opt/hft-bot/current/bin");
        assert_eq!(quote("hft-bot.service"), "hft-bot.service");
    }

    #[test]
    fn a_single_quote_in_a_value_is_escaped_rather_than_closing_the_quote() {
        let quoted = quote("it's");
        // The naive result "'it's'" would end the quote early.
        assert_ne!(quoted, "'it's'");
        assert!(quoted.contains("\\'") || quoted.contains("'\\''"));
    }

    #[test]
    fn joining_a_command_quotes_every_argument_separately() {
        let joined = join_command(
            "systemctl",
            &["restart".to_string(), "my service.service".to_string()],
        );
        assert!(joined.starts_with("systemctl restart "));
        // The space in the unit name must not split into two arguments.
        assert!(joined.contains("'my service.service'"));
    }

    #[test]
    fn parent_directories_are_extracted_from_absolute_paths() {
        assert_eq!(parent_dir("/opt/bot/releases/r1/bin"), "/opt/bot/releases/r1");
        assert_eq!(parent_dir("/etc/systemd/system/x.service"), "/etc/systemd/system");
        assert_eq!(parent_dir("/file"), "/");
        assert_eq!(parent_dir("relative"), ".");
    }

    #[tokio::test]
    async fn streamed_output_is_split_into_whole_lines() {
        let (tx, mut rx) = mpsc::channel(16);
        let mut buf = Vec::new();

        // Arrives split mid-line, as it does over a real channel.
        assert!(drain_lines(&mut buf, b"first\nsec", false, &tx).await);
        assert!(drain_lines(&mut buf, b"ond\nthird\n", false, &tx).await);

        assert_eq!(rx.recv().await.expect("line").text, "first");
        assert_eq!(rx.recv().await.expect("line").text, "second");
        assert_eq!(rx.recv().await.expect("line").text, "third");
    }

    #[tokio::test]
    async fn carriage_returns_are_stripped_from_line_ends() {
        let (tx, mut rx) = mpsc::channel(4);
        let mut buf = Vec::new();
        drain_lines(&mut buf, b"windows\r\n", false, &tx).await;
        assert_eq!(rx.recv().await.expect("line").text, "windows");
    }

    #[tokio::test]
    async fn a_trailing_partial_line_is_flushed_at_the_end() {
        let (tx, mut rx) = mpsc::channel(4);
        let mut buf = Vec::new();
        drain_lines(&mut buf, b"no trailing newline", false, &tx).await;
        // Nothing emitted yet — the line may still be incomplete.
        assert!(rx.try_recv().is_err());

        flush_remainder(buf, false, &tx).await;
        assert_eq!(rx.recv().await.expect("line").text, "no trailing newline");
    }

    #[tokio::test]
    async fn stderr_lines_are_tagged_as_such() {
        let (tx, mut rx) = mpsc::channel(4);
        let mut buf = Vec::new();
        drain_lines(&mut buf, b"boom\n", true, &tx).await;
        let line = rx.recv().await.expect("line");
        assert!(line.stderr);
        assert_eq!(line.text, "boom");
    }

    #[tokio::test]
    async fn draining_reports_when_the_receiver_has_hung_up() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let mut buf = Vec::new();
        assert!(
            !drain_lines(&mut buf, b"line\n", false, &tx).await,
            "a dropped receiver must stop the stream rather than buffer forever"
        );
    }

    #[test]
    fn a_non_zero_exit_becomes_an_error_carrying_stderr() {
        let result = CommandResult {
            exit_code: 1,
            stdout: String::new(),
            stderr: "Unit not found.".to_string(),
        };
        let error = result.require_success("restarting unit").expect_err("must fail");
        let message = error.to_string();
        assert!(message.contains("restarting unit"));
        assert!(message.contains("Unit not found."));
        assert!(message.contains("exit 1"));
    }

    #[test]
    fn a_successful_command_passes_through() {
        let result = CommandResult {
            exit_code: 0,
            stdout: "active\n".to_string(),
            stderr: String::new(),
        };
        assert!(result.require_success("x").is_ok());
        assert_eq!(result.trimmed(), "active");
    }

    #[test]
    fn a_command_with_no_exit_status_is_treated_as_failed() {
        // A peer that closes without an exit-status must not read as success.
        let result = CommandResult {
            exit_code: -1,
            stdout: String::new(),
            stderr: String::new(),
        };
        assert!(!result.ok());
    }

    #[test]
    fn ssh_targets_never_print_their_key_material() {
        let target = SshTarget {
            host: "10.0.0.5".into(),
            port: 22,
            user: "root".into(),
            private_key: "-----BEGIN OPENSSH PRIVATE KEY-----\nsecret\n".into(),
            passphrase: Some("hunter2".into()),
        };
        let debug = format!("{target:?}");
        assert!(!debug.contains("secret"));
        assert!(!debug.contains("hunter2"));
        assert!(debug.contains("10.0.0.5"));
    }
}
