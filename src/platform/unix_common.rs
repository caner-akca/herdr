use std::fs;
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use interprocess::local_socket::{prelude::*, GenericFilePath, Listener, ListenerOptions};
use interprocess::os::unix::local_socket::ListenerOptionsExt as _;

/// Socket file mode for Herdr's private listeners (owner read/write only).
const SOCKET_PERMISSION_MODE: u32 = 0o600;

/// Binds a listener for private local traffic with owner-only permissions.
///
/// The mode is applied to the socket descriptor before `bind()`, so the socket's
/// pathname is never `chmod()`ed and filesystems that reject permission changes
/// on socket inodes are never asked to. Platforms whose kernel cannot set a
/// socket's mode before bind keep the previous bind-then-restrict behavior.
pub(crate) fn bind_private_local_listener(path: &Path) -> io::Result<Listener> {
    bind_private_socket_with(path, bind_local_listener, restrict_socket_permissions)
}

fn bind_local_listener(path: &Path, mode: Option<u32>) -> io::Result<Listener> {
    let name = path.to_fs_name::<GenericFilePath>()?;
    let options = ListenerOptions::new().name(name).reclaim_name(false);
    match mode {
        Some(mode) => options.mode(mode as libc::mode_t).create_sync(),
        None => options.create_sync(),
    }
}

#[cfg(test)]
pub(crate) fn bind_local_listener_for_test(path: &Path) -> io::Result<Listener> {
    bind_local_listener(path, None)
}

fn bind_private_socket_with<T>(
    path: &Path,
    mut bind: impl FnMut(&Path, Option<u32>) -> io::Result<T>,
    restrict: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<T> {
    match bind(path, Some(SOCKET_PERMISSION_MODE)) {
        Ok(bound) => Ok(bound),
        // The platform cannot set a socket's mode before bind. Restrict the
        // bound socket instead, exactly as Herdr did before creation-time modes.
        Err(err) if err.kind() == io::ErrorKind::Unsupported => {
            let bound = bind(path, None)?;
            let bound_socket = socket_identity(path);
            if let Err(err) = restrict(path) {
                // The socket is published but unrestricted. Listeners are
                // created with name reclamation disabled, so unbind it and take
                // the pathname down instead of leaving it for the next start.
                drop(bound);
                remove_socket_if_unchanged(path, bound_socket);
                return Err(err);
            }
            Ok(bound)
        }
        Err(err) => Err(err),
    }
}

fn restrict_socket_permissions(path: &Path) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(SOCKET_PERMISSION_MODE))
}

/// Identifies the socket at `path`, or `None` when it is absent or not a socket.
fn socket_identity(path: &Path) -> Option<(u64, u64)> {
    let metadata = fs::symlink_metadata(path).ok()?;
    metadata
        .file_type()
        .is_socket()
        .then(|| (metadata.dev(), metadata.ino()))
}

/// Unlinks `path` only while it still holds the socket Herdr bound there, so a
/// pathname another process substituted is left alone.
fn remove_socket_if_unchanged(path: &Path, bound: Option<(u64, u64)>) {
    if bound.is_some() && socket_identity(path) == bound {
        let _ = fs::remove_file(path);
    }
}

fn set_sigpipe_disposition(handler: libc::sighandler_t) {
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = handler;
    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
        // Rust starts with SIGPIPE ignored. If this best-effort transition
        // fails, stdout retains the existing Rust behavior.
        libc::sigaction(libc::SIGPIPE, &action, std::ptr::null_mut());
    }
}

pub(crate) fn begin_cli_output() {
    set_sigpipe_disposition(libc::SIG_DFL);
}

pub(crate) fn end_cli_output() {
    set_sigpipe_disposition(libc::SIG_IGN);
}

pub(crate) fn remote_ssh_config_paths() -> super::RemoteSshConfigPaths {
    super::RemoteSshConfigPaths {
        user_config: std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(".ssh").join("config")),
        system_config: Some(PathBuf::from("/etc/ssh/ssh_config")),
        multiplexing: true,
    }
}

pub(crate) fn create_remote_ssh_config_dir(control_socket_name: &str) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;

    let mut bases = vec![std::env::temp_dir()];
    let short_tmp = PathBuf::from("/tmp");
    if bases.first() != Some(&short_tmp) {
        bases.push(short_tmp);
    }

    let mut last_error = None;
    let mut path_fits = false;
    for base in bases {
        for attempt in 0..100 {
            let dir = base.join(format!("herdr-ssh-{}-{attempt}", std::process::id()));
            if !fits_unix_socket_path(&dir.join(control_socket_name)) {
                continue;
            }
            path_fits = true;
            match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => return Ok(dir),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => {
                    last_error = Some(err);
                    break;
                }
            }
        }
    }

    if let Some(err) = last_error {
        return Err(err);
    }
    let message = if path_fits {
        "failed to create private herdr ssh config directory"
    } else {
        "SSH control socket path exceeds the Unix socket length limit"
    };
    Err(std::io::Error::new(
        if path_fits {
            std::io::ErrorKind::AlreadyExists
        } else {
            std::io::ErrorKind::InvalidInput
        },
        message,
    ))
}

pub(crate) fn create_remote_ssh_config_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

pub(crate) fn create_remote_private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    std::fs::DirBuilder::new().mode(0o700).create(path)
}

pub(crate) fn remote_private_temp_base() -> PathBuf {
    std::env::temp_dir()
}

pub(crate) fn remote_bridge_endpoint_path(readable_name: &str, short_name: &str) -> PathBuf {
    let tmp = std::env::temp_dir();
    let readable = tmp.join(readable_name);
    if fits_unix_socket_path(&readable) {
        return readable;
    }
    let short = tmp.join(short_name);
    if fits_unix_socket_path(&short) {
        return short;
    }
    PathBuf::from("/tmp").join(short_name)
}

pub(crate) fn remote_reattach_program(program: &str) -> String {
    shell_quote(if program.is_empty() { "herdr" } else { program })
}

pub(crate) fn remote_reattach_argument(value: &str) -> String {
    shell_quote(value)
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value.chars().all(|ch| {
            ch.is_ascii_alphanumeric()
                || matches!(
                    ch,
                    '@' | '%' | '_' | '+' | '=' | ':' | ',' | '.' | '/' | '-'
                )
        })
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn fits_unix_socket_path(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;

    path.as_os_str().as_bytes().len() <= 103
}

/// The machine's node name, as shown by tmux's `#h`.
pub(crate) fn hostname() -> Option<String> {
    let mut buffer = [0_u8; 256];
    let result =
        unsafe { libc::gethostname(buffer.as_mut_ptr().cast::<libc::c_char>(), buffer.len()) };
    if result != 0 {
        return None;
    }
    let end = buffer
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(buffer.len());
    let name = String::from_utf8_lossy(&buffer[..end]).into_owned();
    (!name.is_empty()).then_some(name)
}

pub(crate) fn local_datetime() -> Option<time::PrimitiveDateTime> {
    let mut timestamp: libc::time_t = 0;
    if unsafe { libc::time(&mut timestamp) } == -1 {
        return None;
    }
    let mut local: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&timestamp, &mut local) }.is_null() {
        return None;
    }
    datetime_from_tm(&local)
}

pub(crate) fn status_commands_supported() -> bool {
    true
}

pub(crate) fn configure_status_command(process: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;

    process.process_group(0);
}

pub(crate) struct StatusCommandGuard {
    process_group_id: Option<i32>,
}

impl StatusCommandGuard {
    pub(crate) fn new(child: &tokio::process::Child) -> std::io::Result<Self> {
        let process_id = child
            .id()
            .ok_or_else(|| std::io::Error::other("status command has no process id"))?;
        let process_group_id = i32::try_from(process_id)
            .map_err(|_| std::io::Error::other("status command process id exceeds i32"))?;
        Ok(Self {
            process_group_id: Some(process_group_id),
        })
    }
}

impl StatusCommandGuard {
    pub(crate) fn terminate(&mut self) {
        if let Some(process_group_id) = self.process_group_id.take() {
            // The command was spawned as this process group's leader. Killing the
            // group also cleans up background descendants on completion/cancellation.
            unsafe {
                libc::kill(-process_group_id, libc::SIGKILL);
            }
        }
    }
}

impl Drop for StatusCommandGuard {
    fn drop(&mut self) {
        self.terminate();
    }
}

fn datetime_from_tm(value: &libc::tm) -> Option<time::PrimitiveDateTime> {
    let month = time::Month::try_from(u8::try_from(value.tm_mon + 1).ok()?).ok()?;
    let date = time::Date::from_calendar_date(
        value.tm_year + 1900,
        month,
        u8::try_from(value.tm_mday).ok()?,
    )
    .ok()?;
    let time = time::Time::from_hms(
        u8::try_from(value.tm_hour).ok()?,
        u8::try_from(value.tm_min).ok()?,
        u8::try_from(value.tm_sec).ok()?,
    )
    .ok()?;
    Some(time::PrimitiveDateTime::new(date, time))
}

pub(crate) fn set_default_plugin_pane_pwd(env: &mut Vec<(String, String)>, cwd: &std::path::Path) {
    if !env.iter().any(|(key, _)| key == "PWD") {
        env.push(("PWD".to_string(), cwd.display().to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_test_dir(label: &str) -> PathBuf {
        let path = Path::new("/tmp").join(format!(
            "herdr-platform-socket-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn plugin_pane_pwd_defaults_to_cwd_without_overriding_explicit_env() {
        let cwd = Path::new("/plugin-cwd");
        let mut derived = vec![("OTHER".to_string(), "value".to_string())];
        set_default_plugin_pane_pwd(&mut derived, cwd);
        assert!(derived.contains(&("PWD".to_string(), "/plugin-cwd".to_string())));

        let mut explicit = vec![("PWD".to_string(), "/caller-pwd".to_string())];
        set_default_plugin_pane_pwd(&mut explicit, cwd);
        assert_eq!(explicit, [("PWD".to_string(), "/caller-pwd".to_string())]);
    }

    #[test]
    fn remote_ssh_config_dir_rejects_overlong_control_socket_name() {
        let err = create_remote_ssh_config_dir(&"x".repeat(200)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn private_local_listener_restricts_socket_to_owner() {
        let parent = temp_test_dir("owner-only");
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
        let socket = parent.join("herdr.sock");

        let listener = bind_private_local_listener(&socket).unwrap();

        assert_eq!(
            fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
            0o755,
            "binding a private socket must not alter its parent directory"
        );

        drop(listener);
        fs::remove_dir_all(&parent).unwrap();
    }

    #[test]
    fn private_socket_restricts_after_bind_when_creation_mode_is_unsupported() {
        let parent = temp_test_dir("post-bind");
        let socket = parent.join("herdr.sock");
        let mut attempts = Vec::new();

        let listener = bind_private_socket_with(
            &socket,
            |path, requested_mode| {
                attempts.push(requested_mode);
                if requested_mode.is_some() {
                    return Err(io::Error::from(io::ErrorKind::Unsupported));
                }
                std::os::unix::net::UnixListener::bind(path)
            },
            restrict_socket_permissions,
        )
        .unwrap();

        assert_eq!(attempts, [Some(0o600), None]);
        assert_eq!(
            fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
            0o600
        );

        drop(listener);
        fs::remove_dir_all(&parent).unwrap();
    }

    #[test]
    fn private_socket_keeps_other_creation_errors_fatal() {
        let parent = temp_test_dir("fatal");
        let socket = parent.join("herdr.sock");
        let mut attempts = Vec::new();

        let err = bind_private_socket_with(
            &socket,
            |_path, requested_mode| {
                attempts.push(requested_mode);
                Err::<std::os::unix::net::UnixListener, _>(io::Error::from(
                    io::ErrorKind::PermissionDenied,
                ))
            },
            restrict_socket_permissions,
        )
        .unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(attempts, [Some(0o600)]);
        assert!(!socket.exists());
        fs::remove_dir_all(&parent).unwrap();
    }

    #[test]
    fn private_socket_is_removed_when_post_bind_restriction_fails() {
        let parent = temp_test_dir("restrict-failure");
        let socket = parent.join("herdr.sock");

        let err = bind_private_socket_with(
            &socket,
            |path, requested_mode| {
                if requested_mode.is_some() {
                    return Err(io::Error::from(io::ErrorKind::Unsupported));
                }
                std::os::unix::net::UnixListener::bind(path)
            },
            |_path| Err(io::Error::from(io::ErrorKind::PermissionDenied)),
        )
        .unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            !socket.exists(),
            "an unrestricted socket must not stay published"
        );
        fs::remove_dir_all(&parent).unwrap();
    }

    #[test]
    fn restriction_failure_leaves_a_replaced_pathname_alone() {
        let parent = temp_test_dir("restrict-replacement");
        let socket = parent.join("herdr.sock");
        let replacement = parent.join("replacement.sock");

        let err = bind_private_socket_with(
            &socket,
            |path, requested_mode| {
                if requested_mode.is_some() {
                    return Err(io::Error::from(io::ErrorKind::Unsupported));
                }
                std::os::unix::net::UnixListener::bind(path)
            },
            |path| {
                // Stand in for another process swapping the pathname between
                // bind and the failing restriction. Dropping the listener does
                // not unlink its pathname.
                std::os::unix::net::UnixListener::bind(&replacement)?;
                fs::remove_file(path)?;
                fs::rename(&replacement, path)?;
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            },
        )
        .unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            socket.exists(),
            "a pathname Herdr no longer owns must not be unlinked"
        );
        fs::remove_dir_all(&parent).unwrap();
    }
}
