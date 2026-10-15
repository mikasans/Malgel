//! A second launch hands its files to the Malgel already running for the
//! same user and session, the way VS Code wakes its existing window.
//!
//! The channel is scoped to the user, the config folder and the display, so
//! two sessions (another user, another `HOME`, another X display or Wayland
//! socket, or parallel test runs) never hand files to each other. On Unix it
//! is a socket in `XDG_RUNTIME_DIR` (or a per-user temporary folder); on
//! Windows a loopback port whose handshake carries the same scope.

use std::hash::{Hash as _, Hasher as _};
use std::io::Write as _;
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

const MAGIC: &str = "malgel-cli-v2";
const ACK: &str = "malgel-cli-ok";

/// What tells one session from another.
pub fn scope_key() -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for var in [
        "USER",
        "USERNAME",
        "HOME",
        "USERPROFILE",
        "XDG_CONFIG_HOME",
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "SESSIONNAME",
    ] {
        var.hash(&mut hasher);
        std::env::var_os(var).hash(&mut hasher);
    }
    crate::settings::config_dir().hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn payload(scope: &str, paths: &[PathBuf]) -> String {
    let mut payload = format!("{MAGIC}\n{scope}\n");
    for path in paths {
        payload.push_str(&path.to_string_lossy());
        payload.push('\n');
    }
    payload
}

/// The paths in a forwarded launch, if it is one for this scope.
fn parse(payload: &str, scope: &str) -> Option<Vec<PathBuf>> {
    let mut lines = payload.lines();
    if lines.next() != Some(MAGIC) || lines.next() != Some(scope) {
        return None;
    }
    Some(
        lines
            .filter(|line| !line.is_empty())
            .map(PathBuf::from)
            .map(|path| path.canonicalize().unwrap_or(path))
            .collect(),
    )
}

fn exchange(mut stream: impl std::io::Read + std::io::Write, message: &str) -> bool {
    if stream.write_all(message.as_bytes()).is_err() {
        return false;
    }
    let mut reply = String::new();
    // The listener answers after reading to the end of the message, which a
    // half-closed stream marks; `shutdown` is done by the caller.
    stream.read_to_string(&mut reply).is_ok() && reply.contains(ACK)
}

fn serve(
    mut stream: impl std::io::Read + std::io::Write,
    scope: &str,
    open_tx: &futures::channel::mpsc::UnboundedSender<Vec<PathBuf>>,
) {
    let mut message = String::new();
    if stream.read_to_string(&mut message).is_err() {
        return;
    }
    if let Some(paths) = parse(&message, scope) {
        let _ = open_tx.unbounded_send(paths);
        let _ = stream.write_all(ACK.as_bytes());
    }
}

#[cfg(unix)]
pub fn socket_path(scope: &str) -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_dir())
        .unwrap_or_else(|| {
            // SAFETY: getuid has no preconditions.
            let uid = unsafe { libc_getuid() };
            std::env::temp_dir().join(format!("malgel-{uid}"))
        });
    dir.join(format!("malgel-{scope}.sock"))
}

#[cfg(unix)]
unsafe extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

/// Hand `paths` to the running instance of this session; `true` when it took
/// them.
#[cfg(unix)]
pub fn forward(paths: &[PathBuf]) -> bool {
    let scope = scope_key();
    forward_to(&socket_path(&scope), &scope, paths)
}

#[cfg(unix)]
fn forward_to(socket: &Path, scope: &str, paths: &[PathBuf]) -> bool {
    let Ok(stream) = std::os::unix::net::UnixStream::connect(socket) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let mut writer = &stream;
    if writer.write_all(payload(scope, paths).as_bytes()).is_err() {
        return false;
    }
    let _ = stream.shutdown(std::net::Shutdown::Write);
    exchange(&stream, "")
}

/// Accept forwarded launches for this session until the app exits.
#[cfg(unix)]
pub fn listen(open_tx: futures::channel::mpsc::UnboundedSender<Vec<PathBuf>>) {
    let scope = scope_key();
    listen_on(&socket_path(&scope), &scope, open_tx);
}

#[cfg(unix)]
fn listen_on(
    socket: &Path,
    scope: &str,
    open_tx: futures::channel::mpsc::UnboundedSender<Vec<PathBuf>>,
) {
    use std::os::unix::fs::PermissionsExt as _;
    if let Some(dir) = socket.parent() {
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    // `forward` already failed, so a socket file left here is stale.
    let _ = std::fs::remove_file(socket);
    let Ok(listener) = std::os::unix::net::UnixListener::bind(socket) else {
        return;
    };
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            continue;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        serve(&stream, scope, &open_tx);
    }
}

#[cfg(not(unix))]
const PORTS: [u16; 10] = [
    47615, 47616, 47617, 47618, 47619, 47620, 47621, 47622, 47623, 47624,
];

#[cfg(not(unix))]
pub fn forward(paths: &[PathBuf]) -> bool {
    use std::net::TcpStream;
    let scope = scope_key();
    for port in PORTS {
        let Ok(stream) = TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            Duration::from_millis(250),
        ) else {
            continue;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let mut writer = &stream;
        if writer.write_all(payload(&scope, paths).as_bytes()).is_err() {
            continue;
        }
        let _ = stream.shutdown(std::net::Shutdown::Write);
        // An instance of another session reads the message and says nothing.
        if exchange(&stream, "") {
            return true;
        }
    }
    false
}

#[cfg(not(unix))]
pub fn listen(open_tx: futures::channel::mpsc::UnboundedSender<Vec<PathBuf>>) {
    let scope = scope_key();
    for port in PORTS {
        let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", port)) else {
            continue;
        };
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                continue;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            serve(&stream, &scope, &open_tx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn another_scope_is_not_handed_files() {
        let message = payload("aaaa", &[PathBuf::from("/x.md")]);
        assert_eq!(parse(&message, "aaaa"), Some(vec![PathBuf::from("/x.md")]));
        assert_eq!(parse(&message, "bbbb"), None);
        assert_eq!(parse("malgel-cli-v1\n/x.md\n", "aaaa"), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_launch_reaches_only_the_instance_of_its_session() {
        let dir = std::env::temp_dir().join(format!("malgel-si-test-{}", std::process::id()));
        let socket = dir.join("s.sock");
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let listening = socket.clone();
        std::thread::spawn(move || listen_on(&listening, "mine", tx));
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // Another session's launch is refused, so it starts its own window.
        assert!(!forward_to(&socket, "theirs", &[PathBuf::from("/a.md")]));
        assert!(forward_to(&socket, "mine", &[PathBuf::from("/b.md")]));
        assert_eq!(rx.try_recv().unwrap(), (vec![PathBuf::from("/b.md")]));
        // Nothing listening: the launch starts its own window.
        assert!(!forward_to(&dir.join("none.sock"), "mine", &[]));
        let _ = std::fs::remove_dir_all(dir);
    }
}
