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

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use russh::keys::PrivateKeyWithHashAlg;
use russh::keys::ssh_key::{self, HashAlg, PublicKey};
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
    /// The host key this target is pinned to, in OpenSSH one-line form. Empty
    /// means nothing is pinned yet, and the first connection records what the
    /// host presents.
    pub host_key: String,
}

impl std::fmt::Debug for SshTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never let key material reach a log line.
        f.debug_struct("SshTarget")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("private_key", &"<redacted>")
            .field("host_key", &fingerprint_of(&self.host_key))
            .finish()
    }
}

/// What a connection concluded about the host key it was presented.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostKeyOutcome {
    /// Nothing was pinned, so this key was trusted on first use. The caller
    /// records it; every later connection is then verified against it.
    Pinned { key: String, fingerprint: String },
    /// The key matched the pinned one, which is the ordinary case.
    Matched { fingerprint: String },
    /// The key did not match. The connection was refused before
    /// authentication, so the target never saw our private key.
    Changed {
        expected_fingerprint: String,
        key: String,
        fingerprint: String,
    },
}

/// The host key a connection saw, published by the handler for the caller.
///
/// `check_server_key` runs inside the handshake with no route back to the
/// store, so it records its verdict here and [`SshSession::connect`] returns it
/// for the caller to persist.
type HostKeySlot = Arc<Mutex<Option<HostKeyOutcome>>>;

/// A connection refused because the host key changed.
///
/// A distinct error type rather than a string, because the caller has to do
/// something with it beyond reporting it: record the key for review, so it can
/// be accepted without editing the database.
#[derive(Debug, Clone)]
pub struct HostKeyChanged {
    pub host: String,
    pub port: u16,
    pub expected_fingerprint: String,
    /// The key the host presented, in OpenSSH form. Untrusted — this is what is
    /// held for review, not what is pinned.
    pub key: String,
    pub fingerprint: String,
}

impl std::fmt::Display for HostKeyChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the host key for {}:{} has changed — refusing to connect. \
             pinned: {}; presented: {}. If this host was legitimately rebuilt, \
             review and accept the new key (`nudo targets host-key <id> --accept \
             <fingerprint>`, or the target's page in the dashboard). If it was not, \
             something is answering for that address that should not be.",
            self.host, self.port, self.expected_fingerprint, self.fingerprint
        )
    }
}

impl std::error::Error for HostKeyChanged {}

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

/// A `russh` client handler that verifies the host key.
///
/// Trust on first use: with nothing pinned, whatever the host presents is
/// recorded and becomes the expectation. With a key pinned, anything else is
/// refused — and refused *here*, during the handshake, so a host that is not the
/// one we pinned never gets an authentication attempt with the private key nudo
/// holds for it. That ordering is the entire point of the check.
struct Handler {
    /// The pinned key in OpenSSH form, or empty for first use.
    expected: String,
    /// Where the verdict is published for [`SshSession::connect`].
    outcome: HostKeySlot,
}

impl client::Handler for Handler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        let verdict = verdict_for(&self.expected, server_public_key)
            .map_err(|_| russh::Error::Inconsistent)?;

        let accepted = !matches!(verdict, HostKeyOutcome::Changed { .. });
        if let Ok(mut slot) = self.outcome.lock() {
            *slot = Some(verdict);
        }
        Ok(accepted)
    }
}

/// Decides what a presented host key means given what is pinned.
///
/// Separated from the handler so the rule can be tested directly: this is the
/// function that decides whether nudo talks to a machine, and it should not
/// need an SSH server to exercise.
fn verdict_for(expected: &str, presented: &PublicKey) -> Result<HostKeyOutcome, ssh_key::Error> {
    let fingerprint = presented.fingerprint(HashAlg::Sha256).to_string();
    // Re-encoded from the key material alone, so what gets stored is one
    // canonical form: no leading whitespace, no trailing comment, nothing that
    // a later comparison or a display could differ on.
    let presented_openssh = PublicKey::new(presented.key_data().clone(), "").to_openssh()?;

    Ok(match parse_host_key(expected) {
        // Compared on the key material alone. `PublicKey`'s own equality
        // includes the comment, so a pinned key carrying one — which is what
        // ssh-keygen and ssh-keyscan produce — would read as changed against
        // the identical key presented by the host, and refuse a connection to
        // a host that never changed anything.
        Some(pinned) if pinned.key_data() == presented.key_data() => {
            HostKeyOutcome::Matched { fingerprint }
        }
        Some(pinned) => HostKeyOutcome::Changed {
            expected_fingerprint: pinned.fingerprint(HashAlg::Sha256).to_string(),
            key: presented_openssh,
            fingerprint,
        },
        // A pinned value that will not parse is a mismatch, not an absence.
        // Falling through to first use here would let a corrupted column
        // silently re-pin whatever answered.
        None if host_key_is_pinned(expected) => HostKeyOutcome::Changed {
            expected_fingerprint: "<unparseable pinned key>".to_string(),
            key: presented_openssh,
            fingerprint,
        },
        // Nothing pinned yet: trust this one and let the caller record it.
        None => HostKeyOutcome::Pinned {
            key: presented_openssh,
            fingerprint,
        },
    })
}

/// An authenticated SSH session against one target.
pub struct SshSession {
    handle: client::Handle<Handler>,
    host_key: HostKeyOutcome,
}

impl std::fmt::Debug for SshSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The handle holds no printable state worth showing; what the session
        // concluded about the host key is the part worth seeing.
        f.debug_struct("SshSession")
            .field("host_key", &self.host_key)
            .finish()
    }
}

impl SshSession {
    /// Connects and authenticates with the target's private key, verifying the
    /// host key first.
    ///
    /// A changed host key fails here rather than anywhere later: read-only
    /// operations are not exempt, because a mismatch may mean this is not the
    /// host at all, and reading logs from the wrong machine is its own problem.
    pub async fn connect(target: &SshTarget) -> anyhow::Result<Self> {
        let config = Arc::new(client::Config {
            inactivity_timeout: Some(Duration::from_secs(3600)),
            ..Default::default()
        });

        let key = russh::keys::decode_secret_key(&target.private_key, target.passphrase.as_deref())
            .context(
                "decoding the target's SSH private key — it must be an OpenSSH or PEM private key",
            )?;

        let outcome: HostKeySlot = Arc::default();
        let handler = Handler {
            expected: target.host_key.clone(),
            outcome: outcome.clone(),
        };

        let addr = (target.host.as_str(), target.port);
        let connected =
            tokio::time::timeout(CONNECT_TIMEOUT, client::connect(config, addr, handler))
                .await
                .map_err(|_| {
                    anyhow!(
                        "timed out connecting to {}:{} after {}s",
                        target.host,
                        target.port,
                        CONNECT_TIMEOUT.as_secs()
                    )
                })?;

        // Read the verdict before the error, so a refused connection reports the
        // fingerprints rather than russh's generic rejection message.
        let verdict = outcome.lock().ok().and_then(|slot| slot.clone());
        if let Some(HostKeyOutcome::Changed {
            expected_fingerprint,
            key,
            fingerprint,
        }) = verdict.clone()
        {
            return Err(HostKeyChanged {
                host: target.host.clone(),
                port: target.port,
                expected_fingerprint,
                key,
                fingerprint,
            }
            .into());
        }

        let mut handle =
            connected.with_context(|| format!("connecting to {}:{}", target.host, target.port))?;

        // A connection that completed the handshake always ran the check, so a
        // missing verdict would mean the invariant this whole path rests on is
        // broken. Refuse rather than proceed unverified.
        let host_key = verdict.ok_or_else(|| {
            anyhow!(
                "connected to {}:{} without the host key being checked",
                target.host,
                target.port
            )
        })?;

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

        Ok(Self { handle, host_key })
    }

    /// What this connection concluded about the target's host key.
    ///
    /// Only [`HostKeyOutcome::Pinned`] and [`HostKeyOutcome::Matched`] can be
    /// observed here: a change fails the connect, so there is no session to ask.
    pub fn host_key(&self) -> &HostKeyOutcome {
        &self.host_key
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
                // Not `Eof`: SSH sends EOF *before* the exit status, so breaking
                // there would leave every command reporting failure. `Close` is
                // the end of the channel, and `wait()` returning None covers a
                // peer that closes without one.
                ChannelMsg::Close => break,
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
                ChannelMsg::Data { data }
                    if !drain_lines(&mut stdout_buf, &data, false, &sink).await =>
                {
                    // Receiver is gone — the browser navigated away or the
                    // CLI detached. Stop rather than filling memory.
                    break;
                }
                ChannelMsg::ExtendedData { data, .. }
                    if !drain_lines(&mut stderr_buf, &data, true, &sink).await =>
                {
                    break;
                }
                ChannelMsg::ExitStatus { exit_status } => exit_code = exit_status as i32,
                // See `exec`: EOF precedes the exit status, so only `Close` ends
                // the loop.
                ChannelMsg::Close => break,
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
                // An empty artifact reports complete rather than dividing by
                // zero.
                let percent = (written * 100).checked_div(total).unwrap_or(100);
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
            .exec(&format!("wc -c < {} | tr -d ' \\n'", quote(path)))
            .await?;
        let remote_size: usize = remote_size
            .trimmed()
            .parse()
            .with_context(|| format!("reading back the size of {path}"))?;
        if remote_size != total {
            bail!("upload of {path} is incomplete: wrote {total} bytes, target has {remote_size}");
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
                channel
                    .request_shell(true)
                    .await
                    .context("requesting a shell")?;
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

/// Parses a stored host key in OpenSSH one-line form.
///
/// `None` for an empty value — nothing pinned — and also for an unparseable one.
/// Treating corruption as "not pinned" would silently downgrade to first use, so
/// callers must distinguish the two; [`host_key_is_pinned`] is what they use to
/// tell a blank column from a broken one.
fn parse_host_key(raw: &str) -> Option<PublicKey> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    PublicKey::from_openssh(raw).ok()
}

/// Whether a stored value names a host key at all, parseable or not.
pub fn host_key_is_pinned(raw: &str) -> bool {
    !raw.trim().is_empty()
}

/// The SHA-256 fingerprint of a host key in OpenSSH form, for display.
///
/// Unparseable input yields a marker rather than an error: this is only ever
/// used to describe a key in a message, and failing to format one should not
/// fail the operation being described.
pub fn fingerprint_of(raw: &str) -> String {
    match parse_host_key(raw) {
        Some(key) => key.fingerprint(HashAlg::Sha256).to_string(),
        None if raw.trim().is_empty() => "<none>".to_string(),
        None => "<unparseable>".to_string(),
    }
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
        assert_eq!(
            quote("/opt/hft-bot/current/bin"),
            "/opt/hft-bot/current/bin"
        );
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
        assert_eq!(
            parent_dir("/opt/bot/releases/r1/bin"),
            "/opt/bot/releases/r1"
        );
        assert_eq!(
            parent_dir("/etc/systemd/system/x.service"),
            "/etc/systemd/system"
        );
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
        let error = result
            .require_success("restarting unit")
            .expect_err("must fail");
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

    // Note on the receive loops in `exec` and `exec_streaming`: SSH delivers
    // `Eof` *before* `ExitStatus`, so only `Close` (or `wait()` returning None)
    // may end the loop. Ending it on `Eof` makes every command report failure
    // however well it ran — a real bug here, caught by the end-to-end test,
    // which reported `systemd 252 (...)` as a *failed* systemd check. The
    // end-to-end suite covers this against a real sshd; a unit test would need a
    // full SSH server to be meaningful.

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

    // ---- host-key verification ----
    //
    // Two real ed25519 public keys, fixed rather than generated, so a test that
    // fails names the same fingerprints every time.

    const KEY_A: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAINbQLN3OR4KHUki7vfmdITOI3q+Nfu9w3X2agJ+IDHXR";
    const FINGERPRINT_A: &str = "SHA256:YDKOP3XHL0C4Ib51j2RJjuZhmu8rexWKe7IVDDOurMI";

    const KEY_B: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIwpYjPDsSOQR3dD30dkum4PseIZzCiIqleJEkpyKBfu";
    const FINGERPRINT_B: &str = "SHA256:YWnilawiH+UPKWl5LfNJH4a2YHBRRXkqVxVpE40NZNk";

    fn public_key(openssh: &str) -> PublicKey {
        PublicKey::from_openssh(openssh).expect("a valid test key")
    }

    #[test]
    fn the_test_keys_are_what_ssh_keygen_says_they_are() {
        // If this drifts, every assertion below is comparing nudo against
        // itself rather than against the tool operators actually use.
        assert_eq!(fingerprint_of(KEY_A), FINGERPRINT_A);
        assert_eq!(fingerprint_of(KEY_B), FINGERPRINT_B);
        assert_ne!(FINGERPRINT_A, FINGERPRINT_B);
    }

    #[test]
    fn nothing_pinned_means_the_first_key_seen_is_trusted_and_recorded() {
        let verdict = verdict_for("", &public_key(KEY_A)).expect("verdict");
        match verdict {
            HostKeyOutcome::Pinned { key, fingerprint } => {
                assert_eq!(fingerprint, FINGERPRINT_A);
                // The recorded key must be usable as the next expectation.
                assert_eq!(fingerprint_of(&key), FINGERPRINT_A);
            }
            other => panic!("expected first-use pinning, got {other:?}"),
        }
    }

    #[test]
    fn the_pinned_key_presented_again_matches() {
        let verdict = verdict_for(KEY_A, &public_key(KEY_A)).expect("verdict");
        assert_eq!(
            verdict,
            HostKeyOutcome::Matched {
                fingerprint: FINGERPRINT_A.to_string()
            }
        );
    }

    #[test]
    fn a_different_key_is_a_change_carrying_both_fingerprints() {
        // The whole point: this is what stops nudo authenticating to a host
        // that is not the one it pinned.
        let verdict = verdict_for(KEY_A, &public_key(KEY_B)).expect("verdict");
        match verdict {
            HostKeyOutcome::Changed {
                expected_fingerprint,
                key,
                fingerprint,
            } => {
                assert_eq!(expected_fingerprint, FINGERPRINT_A);
                assert_eq!(fingerprint, FINGERPRINT_B);
                // The presented key is carried so the change can be reviewed
                // rather than only reported.
                assert_eq!(fingerprint_of(&key), FINGERPRINT_B);
            }
            other => panic!("a changed key must not be accepted, got {other:?}"),
        }
    }

    #[test]
    fn a_pinned_key_that_will_not_parse_is_a_change_rather_than_an_absence() {
        // The dangerous failure mode: treating a corrupt column as "nothing
        // pinned" would silently re-pin whatever answered, turning a database
        // problem into a security downgrade with no signal.
        let verdict =
            verdict_for("ssh-ed25519 not-actually-base64", &public_key(KEY_A)).expect("verdict");
        assert!(
            matches!(verdict, HostKeyOutcome::Changed { .. }),
            "got {verdict:?}"
        );
    }

    #[test]
    fn whitespace_and_a_trailing_comment_do_not_make_a_key_look_changed() {
        // ssh-keyscan and ssh-keygen emit comments and trailing newlines; a
        // pinned key differing only in those must still match, or every host
        // would look compromised after a round-trip through a text field.
        let decorated = format!("  {KEY_A} operator@laptop\n");
        assert_eq!(
            verdict_for(&decorated, &public_key(KEY_A)).expect("verdict"),
            HostKeyOutcome::Matched {
                fingerprint: FINGERPRINT_A.to_string()
            }
        );
    }

    #[test]
    fn a_recorded_key_is_stored_without_the_comment_it_arrived_with() {
        // Stored canonically so what is pinned, what is compared and what is
        // shown are the same string. A comment riding along would make the
        // stored value differ from host to host for the same key material.
        let mut commented = public_key(KEY_A);
        commented.set_comment("root@rebuilt-host");

        let HostKeyOutcome::Pinned { key, .. } = verdict_for("", &commented).expect("verdict")
        else {
            panic!("expected first-use pinning");
        };
        assert_eq!(key, KEY_A);
        assert!(!key.contains("root@rebuilt-host"));
    }

    #[test]
    fn a_pinned_value_is_recognised_as_pinned_even_when_it_is_unparseable() {
        assert!(host_key_is_pinned(KEY_A));
        assert!(host_key_is_pinned("garbage"));
        // Only genuinely empty values are "not pinned".
        assert!(!host_key_is_pinned(""));
        assert!(!host_key_is_pinned("   \n "));
    }

    #[test]
    fn fingerprints_are_formatted_for_display_without_failing_on_junk() {
        assert_eq!(fingerprint_of(KEY_A), FINGERPRINT_A);
        assert_eq!(fingerprint_of(""), "<none>");
        assert_eq!(fingerprint_of("nonsense"), "<unparseable>");
    }

    #[test]
    fn a_changed_host_key_error_says_what_changed_and_what_to_do() {
        let message = HostKeyChanged {
            host: "10.0.0.5".to_string(),
            port: 22,
            expected_fingerprint: FINGERPRINT_A.to_string(),
            key: KEY_B.to_string(),
            fingerprint: FINGERPRINT_B.to_string(),
        }
        .to_string();

        // Both fingerprints, so the operator can compare them.
        assert!(message.contains(FINGERPRINT_A), "got: {message}");
        assert!(message.contains(FINGERPRINT_B), "got: {message}");
        assert!(message.contains("10.0.0.5:22"), "got: {message}");
        // And the way out, since refusing without one is just a wall.
        assert!(message.contains("host-key"), "got: {message}");
    }

    #[test]
    fn an_ssh_target_debug_shows_the_host_key_fingerprint_not_the_key() {
        // The fingerprint is the useful thing in a log line; the key itself is
        // 68 characters of noise.
        let target = SshTarget {
            host: "10.0.0.5".into(),
            port: 22,
            user: "root".into(),
            private_key: String::new(),
            passphrase: None,
            host_key: KEY_A.into(),
        };
        let debug = format!("{target:?}");
        assert!(debug.contains(FINGERPRINT_A), "got: {debug}");
    }

    #[test]
    fn ssh_targets_never_print_their_key_material() {
        let target = SshTarget {
            host: "10.0.0.5".into(),
            port: 22,
            user: "root".into(),
            private_key: "-----BEGIN OPENSSH PRIVATE KEY-----\nsecret\n".into(),
            passphrase: Some("hunter2".into()),
            host_key: String::new(),
        };
        let debug = format!("{target:?}");
        assert!(!debug.contains("secret"));
        assert!(!debug.contains("hunter2"));
        assert!(debug.contains("10.0.0.5"));
    }
}
