use anyhow::{Context as _, Result, ensure};
use gpui::{App, Global};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

pub const COMMAND_ENV: &str = "TIMBER_IDE_OPEN_COMMAND";
pub const SOCKET_ENV: &str = "TIMBER_IDE_OPEN_SOCKET";
const REQUEST_LIMIT: usize = 16 * 1024;

#[derive(Default)]
struct WindowEndpoints(HashMap<u64, (PathBuf, PathBuf)>);
impl Global for WindowEndpoints {}

#[derive(Serialize, Deserialize)]
struct OpenRequest {
    version: u32,
    path: PathBuf,
    #[serde(default)]
    terminal: Vec<String>,
}

pub struct OpenHandoff {
    pub path: PathBuf,
    pub terminal: Vec<String>,
}

fn decode_request(bytes: &[u8]) -> Result<OpenHandoff> {
    ensure!(bytes.len() <= REQUEST_LIMIT, "Timber handoff is too large");
    let request: OpenRequest = serde_json::from_slice(bytes).context("Invalid Timber handoff")?;
    ensure!(
        request.version == 1 && request.path.is_absolute(),
        "Unsupported Timber handoff or relative worktree path"
    );
    ensure!(
        request.terminal.len() <= 32
            && request
                .terminal
                .iter()
                .all(|argument| !argument.is_empty() && !argument.contains('\0')),
        "Invalid Timber terminal command"
    );
    Ok(OpenHandoff {
        path: request.path,
        terminal: request.terminal,
    })
}

/// `project_id` is the `Project` entity id `TerminalBuilder::new` already receives.
/// Integrated terminals are spawned from `Project` and do not have the GPUI window id.
pub fn insert_terminal_env(
    project_id: u64,
    remote: bool,
    env: &mut HashMap<String, String>,
    cx: &App,
) {
    env.insert(COMMAND_ENV.into(), String::new());
    env.insert(SOCKET_ENV.into(), String::new());
    if !remote
        && let Some((command, socket)) = cx
            .try_global::<WindowEndpoints>()
            .and_then(|endpoints| endpoints.0.get(&project_id))
    {
        env.insert(COMMAND_ENV.into(), command.to_string_lossy().into_owned());
        env.insert(SOCKET_ENV.into(), socket.to_string_lossy().into_owned());
    }
}

pub fn bind_project(project_id: u64, command: PathBuf, socket: PathBuf, cx: &mut App) {
    cx.default_global::<WindowEndpoints>()
        .0
        .insert(project_id, (command, socket));
}

pub fn unbind_project(project_id: u64, cx: &mut App) {
    if cx.has_global::<WindowEndpoints>() {
        cx.global_mut::<WindowEndpoints>().0.remove(&project_id);
    }
}

#[cfg(unix)]
pub struct WindowBridge {
    socket: smol::net::unix::UnixDatagram,
    directory: tempfile::TempDir,
}

#[cfg(unix)]
impl WindowBridge {
    fn new() -> Result<Self> {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = tempfile::Builder::new()
            .prefix("timber-ide-window-")
            .tempdir()?;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
        let socket = smol::net::unix::UnixDatagram::bind(directory.path().join("open.sock"))?;
        Ok(Self { socket, directory })
    }

    pub fn open() -> Result<Self> {
        Self::new()
    }

    pub fn endpoint(&self) -> PathBuf {
        self.directory.path().join("open.sock")
    }

    pub async fn receive(&self) -> Result<OpenHandoff> {
        let mut bytes = vec![0; REQUEST_LIMIT + 1];
        let (count, _) = self.socket.recv_from(&mut bytes).await?;
        decode_request(&bytes[..count])
    }
}

#[cfg(unix)]
fn send_path(socket: &Path, path: PathBuf, terminal: Vec<String>) -> Result<()> {
    let bytes = serde_json::to_vec(&OpenRequest {
        version: 1,
        path,
        terminal,
    })?;
    decode_request(&bytes)?;
    let sender = std::os::unix::net::UnixDatagram::unbound()?;
    sender.set_write_timeout(Some(std::time::Duration::from_secs(2)))?;
    let count = sender
        .send_to(&bytes, socket)
        .context("Timber IDE window is unavailable; open a fresh integrated terminal")?;
    ensure!(count == bytes.len(), "Incomplete Timber handoff");
    Ok(())
}

pub fn forward_if_requested() -> Result<bool> {
    let mut arguments = std::env::args_os().skip(1);
    if arguments.next().as_deref() != Some(std::ffi::OsStr::new("--timber-open")) {
        return Ok(false);
    }
    let path = arguments
        .next()
        .map(PathBuf::from)
        .context("Missing worktree path")?;
    let mut terminal = Vec::new();
    for argument in arguments {
        let argument = argument
            .to_str()
            .context("Timber terminal command is not Unicode")?
            .to_owned();
        ensure!(
            !argument.is_empty() && !argument.contains('\0'),
            "Invalid Timber terminal command"
        );
        terminal.push(argument);
    }
    ensure!(
        terminal.len() <= 32,
        "Timber terminal command has too many arguments"
    );
    #[cfg(unix)]
    {
        let socket = std::env::var_os(SOCKET_ENV)
            .filter(|value| !value.is_empty())
            .context("No Timber IDE window handoff is configured")?;
        send_path(Path::new(&socket), path, terminal)?;
        Ok(true)
    }
    #[cfg(not(unix))]
    {
        let _path = path;
        let _terminal = terminal;
        anyhow::bail!("Timber terminal handoff is currently supported on Unix")
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn exact_path_reaches_the_window_socket() {
        let bridge = WindowBridge::new().expect("private window bridge");
        let path = PathBuf::from("/worktrees/space and\nnewline");
        let terminal = vec!["codex".to_owned(), "resume".to_owned()];
        send_path(&bridge.endpoint(), path.clone(), terminal.clone()).expect("send exact path");
        let handoff = smol::block_on(bridge.receive()).expect("receive");
        assert_eq!(handoff.path, path);
        assert_eq!(handoff.terminal, terminal);
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(bridge.directory.path())
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    #[test]
    fn malformed_and_relative_requests_are_rejected() {
        for bytes in [
            b"{}".as_slice(),
            br#"{"version":2,"path":"/worktree"}"#,
            br#"{"version":1,"path":"relative"}"#,
        ] {
            assert!(decode_request(bytes).is_err());
        }
    }
    #[gpui::test]
    fn endpoint_environment_is_window_scoped_and_never_sent_to_remote(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            let bridge = WindowBridge::open().expect("private window bridge");
            let command = PathBuf::from("timber-ide");
            let socket = bridge.endpoint();
            bind_project(7, command.clone(), socket.clone(), cx);
            bind_project(9, command, socket.clone(), cx);
            let mut env = HashMap::new();
            insert_terminal_env(7, false, &mut env, cx);
            assert_eq!(env.get(SOCKET_ENV).map(String::as_str), socket.to_str());
            insert_terminal_env(9, false, &mut env, cx);
            assert_eq!(
                env.get(SOCKET_ENV).map(String::as_str),
                socket.to_str(),
                "every project in the window shares that window's socket"
            );
            insert_terminal_env(8, false, &mut env, cx);
            assert_eq!(env.get(SOCKET_ENV).map(String::as_str), Some(""));
            insert_terminal_env(7, true, &mut env, cx);
            assert_eq!(env.get(COMMAND_ENV).map(String::as_str), Some(""));
            unbind_project(7, cx);
            unbind_project(9, cx);
        });
    }
}
