//! Clipboard helpers: URLs as text and GIF files as `image/gif` bytes.
//!
//! Wayland uses `wl-copy` (serving the GIF from stdin); X11 falls back to
//! `xclip` (reading the GIF from a file argument). Pasting into Concord
//! with Ctrl+V then inserts a real animated `.gif` attachment.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

/// Clipboard front-end supported on this platform stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardTool {
    /// Wayland clipboard helper; serves the GIF from stdin.
    WlCopy,
    /// X11 clipboard helper; reads the GIF from the file argument.
    Xclip,
}

impl ClipboardTool {
    pub fn program(self) -> &'static str {
        match self {
            ClipboardTool::WlCopy => "wl-copy",
            ClipboardTool::Xclip => "xclip",
        }
    }

    /// Build the command that places GIF bytes on the clipboard.
    ///
    /// wl-copy reads the GIF from stdin (`wl-copy --type image/gif < file`);
    /// xclip reads the trailing file argument
    /// (`xclip -selection clipboard -t image/gif -i <file>`). The wl-copy
    /// stdin redirect is attached by the caller so tests can inspect the
    /// argv without opening files.
    pub fn gif_command(self, file: &Path) -> Command {
        let mut cmd = Command::new(self.program());
        match self {
            ClipboardTool::WlCopy => {
                cmd.arg("--type").arg("image/gif");
            }
            ClipboardTool::Xclip => {
                cmd.args(["-selection", "clipboard", "-t", "image/gif", "-i"])
                    .arg(file);
            }
        }
        cmd
    }

    /// First clipboard tool found on `$PATH`, preferring wl-copy.
    pub fn detect() -> Option<Self> {
        if in_path("wl-copy") {
            Some(ClipboardTool::WlCopy)
        } else if in_path("xclip") {
            Some(ClipboardTool::Xclip)
        } else {
            None
        }
    }
}

/// Copy text (e.g. a GIF URL) to the clipboard.
pub fn copy_text(text: &str) -> anyhow::Result<()> {
    let tool = ClipboardTool::detect()
        .ok_or_else(|| anyhow::anyhow!("no clipboard tool found (install wl-copy or xclip)"))?;
    let ok = match tool {
        ClipboardTool::WlCopy => Command::new(tool.program())
            .arg(text)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false),
        ClipboardTool::Xclip => {
            let child = Command::new(tool.program())
                .args(["-selection", "clipboard"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            match child {
                Ok(mut child) => {
                    if let Some(mut stdin) = child.stdin.take() {
                        let _ = stdin.write_all(text.as_bytes());
                    }
                    child.wait().map(|s| s.success()).unwrap_or(false)
                }
                Err(_) => false,
            }
        }
    };
    if ok {
        Ok(())
    } else {
        anyhow::bail!("clipboard copy via {} failed", tool.program())
    }
}

/// Best-effort text copy for the post-TUI path; errors are ignored.
pub fn copy_text_quiet(text: &str) {
    let _ = copy_text(text);
}

/// Download the GIF at `url` into a fresh temp file under
/// `<cache>/gifdeck/` and return its path. The caller is responsible for
/// removing the file (see `place_gif_on_clipboard`).
pub async fn download_gif(http: &reqwest::Client, url: &str) -> anyhow::Result<PathBuf> {
    let bytes = http
        .get(url)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("download failed: {e}"))?
        .error_for_status()
        .map_err(|e| anyhow::anyhow!("download failed: {e}"))?
        .bytes()
        .await
        .map_err(|e| anyhow::anyhow!("download failed: {e}"))?;
    let path = temp_gif_path()?;
    std::fs::write(&path, &bytes)
        .map_err(|e| anyhow::anyhow!("failed to write {}: {e}", path.display()))?;
    Ok(path)
}

/// Place a GIF file's bytes on the clipboard as `image/gif`, then remove
/// the temp file (the clipboard helper keeps serving from its open file
/// descriptor after unlink).
pub fn place_gif_on_clipboard(path: &Path) -> anyhow::Result<()> {
    let result = (|| -> anyhow::Result<()> {
        let tool = ClipboardTool::detect().ok_or_else(|| {
            anyhow::anyhow!("no clipboard tool found (install wl-copy or xclip)")
        })?;
        let mut cmd = tool.gif_command(path);
        if matches!(tool, ClipboardTool::WlCopy) {
            cmd.stdin(std::fs::File::open(path)?);
        } else {
            cmd.stdin(Stdio::null());
        }
        let status = cmd
            .status()
            .map_err(|e| anyhow::anyhow!("failed to run {}: {e}", tool.program()))?;
        if !status.success() {
            anyhow::bail!("{} exited with {status}", tool.program());
        }
        Ok(())
    })();
    let _ = std::fs::remove_file(path);
    result
}

/// Download the GIF at `url` and put its bytes on the clipboard as
/// `image/gif`, so a paste inserts a real animated GIF attachment.
pub async fn copy_gif_file(http: &reqwest::Client, url: &str) -> anyhow::Result<()> {
    let path = download_gif(http, url).await?;
    place_gif_on_clipboard(&path)
}

/// Fresh temp file path under `<cache>/gifdeck/`.
fn temp_gif_path() -> anyhow::Result<PathBuf> {
    let dir = dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from(std::env::temp_dir()))
        .join("gifdeck");
    std::fs::create_dir_all(&dir)
        .map_err(|e| anyhow::anyhow!("failed to create {}: {e}", dir.display()))?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Ok(dir.join(format!(
        "gifdeck-{}-{nanos}.gif",
        std::process::id()
    )))
}

/// Whether `name` is an executable file on `$PATH`.
fn in_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| is_executable(dir.join(name)))
        })
        .unwrap_or(false)
}

fn is_executable(path: PathBuf) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(&path) {
        Ok(m) => m.is_file() && m.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn args_of(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn wl_copy_gif_command_reads_gif_type_from_stdin() {
        let cmd = ClipboardTool::WlCopy.gif_command(Path::new("/tmp/x.gif"));
        assert_eq!(cmd.get_program(), "wl-copy");
        assert_eq!(args_of(&cmd), vec!["--type", "image/gif"]);
        // The file is attached via stdin by the caller, never as an argv.
        assert!(!args_of(&cmd).iter().any(|a| a.contains("x.gif")));
    }

    #[test]
    fn xclip_gif_command_passes_the_file() {
        let cmd = ClipboardTool::Xclip.gif_command(Path::new("/tmp/y.gif"));
        assert_eq!(cmd.get_program(), "xclip");
        assert_eq!(
            args_of(&cmd),
            vec![
                "-selection",
                "clipboard",
                "-t",
                "image/gif",
                "-i",
                "/tmp/y.gif"
            ]
        );
    }

    #[test]
    fn detect_finds_no_such_binary() {
        // A tool that certainly does not exist must not be "detected".
        assert!(!in_path("gifdeck-definitely-not-a-real-binary"));
    }

    #[tokio::test]
    async fn download_gif_writes_bytes_to_cache_dir() {
        let server = MockServer::start().await;
        let gif: &[u8] = b"GIF89a-fake-bytes";
        Mock::given(method("GET"))
            .and(path("/x.gif"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(gif.to_vec()))
            .mount(&server)
            .await;

        let path = download_gif(&reqwest::Client::new(), &format!("{}/x.gif", server.uri()))
            .await
            .unwrap();
        assert!(path.to_string_lossy().contains("gifdeck"), "{path:?}");
        assert_eq!(std::fs::read(&path).unwrap(), gif);
        std::fs::remove_file(&path).unwrap();
    }

    #[tokio::test]
    async fn download_gif_http_error_is_reported() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/missing.gif"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let err = download_gif(
            &reqwest::Client::new(),
            &format!("{}/missing.gif", server.uri()),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("404"), "got: {err}");
    }

    #[test]
    fn place_gif_on_clipboard_cleans_up_on_failure() {
        // The path is unique and never created: the tool either can't be
        // found or fails to open/run — either way this must not panic and
        // must not leave the (nonexistent) file behind.
        let path = temp_gif_path().unwrap();
        assert!(place_gif_on_clipboard(&path).is_err());
        assert!(!path.exists());
    }
}