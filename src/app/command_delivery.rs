//! Delivering a composed command into a pane without typing an over-long line.
//!
//! herdr runs a command in a pane by typing it at the pane's shell. A terminal
//! in canonical mode assembles one input line in a fixed-size buffer and hands
//! the reader nothing until a terminator arrives, so a line longer than that
//! buffer cannot be delivered at all: the kernel keeps a prefix, silently
//! discards the rest, and still reports the whole `write()` as successful
//! (refs #2862).
//!
//! Canonical mode is the default rather than an edge case. `bash` and `zsh`
//! clear it only while their line editor is reading and restore it for the
//! whole duration of every foreground command, and `dash` -- `/bin/sh` on
//! Ubuntu -- never leaves it at all. So herdr can neither wait the condition
//! out nor detect its way around it; the only reliable move is to keep the
//! typed line short.
//!
//! A command that would be too long is written to a file instead, and herdr
//! types a line that sources it. That typed line is a function of the path,
//! not of the payload, so it is the same length for a one kilobyte prompt and
//! a one megabyte one.

use std::io;
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;

/// The longest command herdr types directly into a pane.
///
/// The canonical buffer is 1024 bytes on Darwin and 4096 on Linux, both
/// measured exactly against a bare PTY pair. This sits under the smaller with
/// room to spare, and well above any ordinary command, so the file path is
/// reached only by payloads that would otherwise be truncated.
///
/// It bounds what herdr itself types; it cannot bound what is already sitting
/// unread in the terminal's buffer, which no threshold can.
#[cfg(unix)]
const MAX_TYPED_COMMAND: usize = 512;

/// A command staged for delivery to a pane.
pub(crate) struct StagedCommand {
    /// The text herdr should type.
    pub(crate) text: String,
    /// The payload file backing `text`, when the command was too long to type.
    payload: Option<PathBuf>,
}

impl StagedCommand {
    fn typed(command: &str) -> Self {
        Self {
            text: command.to_string(),
            payload: None,
        }
    }

    /// Remove the payload when the command could not be handed to the pane, so
    /// a failed send does not leave it staged on disk. A delivered command
    /// removes its own file as the first thing it does.
    pub(crate) fn discard(self) {
        if let Some(path) = self.payload {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Stage `command` for a pane whose shell is `shell_name`, labelling any
/// payload file with `label` so the line the user sees names what it runs.
#[cfg(unix)]
pub(crate) fn stage(command: &str, shell_name: &str, label: &str) -> io::Result<StagedCommand> {
    if command.len() <= MAX_TYPED_COMMAND {
        return Ok(StagedCommand::typed(command));
    }
    stage_in(&payload_dir(), command, shell_name, label)
}

/// ConPTY has no canonical line discipline, so Windows types every command.
#[cfg(windows)]
pub(crate) fn stage(command: &str, _shell_name: &str, _label: &str) -> io::Result<StagedCommand> {
    Ok(StagedCommand::typed(command))
}

#[cfg(unix)]
fn payload_dir() -> PathBuf {
    crate::config::state_dir().join("staged-commands")
}

#[cfg(unix)]
fn stage_in(dir: &Path, command: &str, shell_name: &str, label: &str) -> io::Result<StagedCommand> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;

    let path = dir.join(payload_name(label));
    let quoted = shell_path(&path, shell_name)?;
    let remove = shell_command(&["rm", "-f", "--", &quoted], shell_name)?;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    // The removal runs first: unlinking only drops the directory entry, and the
    // shell keeps reading through its open descriptor, so the command cleans
    // itself up immediately instead of outliving a long-running agent.
    writeln!(file, "{remove}")?;
    writeln!(file, "{command}")?;
    file.sync_all()?;

    let keyword = if shell_name.contains("fish") {
        // fish dropped `.` as a source alias.
        "source"
    } else {
        "."
    };
    let text = shell_command(&[keyword, &quoted], shell_name)?;
    Ok(StagedCommand {
        text,
        payload: Some(path),
    })
}

#[cfg(unix)]
fn shell_path(path: &Path, shell_name: &str) -> io::Result<String> {
    let _ = shell_name;
    path.to_str().map(str::to_string).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "staged command path is not valid UTF-8",
        )
    })
}

#[cfg(unix)]
fn shell_command(argv: &[&str], shell_name: &str) -> io::Result<String> {
    let argv: Vec<String> = argv.iter().map(|part| (*part).to_string()).collect();
    crate::platform::interactive_shell_command(&argv, shell_name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "could not quote a staged command for this shell",
        )
    })
}

/// A unique, readable file name. The label is what the user sees in the pane,
/// so it is kept intact apart from characters that do not belong in a path.
#[cfg(unix)]
fn payload_name(label: &str) -> String {
    let label: String = label
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    format!("{label}-{}-{nanos:x}", std::process::id())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let dir = std::env::temp_dir().join(format!(
            "herdr-staged-{label}-{}-{nanos:x}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// refs #2862: an ordinary command is typed exactly as it always was, so
    /// the common path gains no file, no syscall and no new failure mode.
    #[test]
    fn an_ordinary_command_is_typed_unchanged() {
        let dir = scratch("ordinary");
        let staged = stage("claude --model sonnet", "zsh", "agent-claude").expect("stage");
        assert_eq!(staged.text, "claude --model sonnet");
        assert!(staged.payload.is_none());
        assert!(!dir.exists(), "no payload directory should be created");
    }

    /// refs #2862: a command too long to type becomes a short line that sources
    /// a file, and the file carries the real command.
    #[test]
    fn an_over_long_command_is_staged_to_a_file() {
        let dir = scratch("over-long");
        let command = format!("claude --append-system-prompt '{}'", "A".repeat(4000));
        let staged = stage_in(&dir, &command, "zsh", "agent-claude").expect("stage");

        let path = staged.payload.clone().expect("payload path");
        assert!(
            staged.text.len() < MAX_TYPED_COMMAND,
            "the typed line must be short, got {}: {}",
            staged.text.len(),
            staged.text
        );
        assert!(staged.text.starts_with(". "), "typed: {}", staged.text);
        assert!(
            staged.text.contains("agent-claude"),
            "typed: {}",
            staged.text
        );

        let body = std::fs::read_to_string(&path).expect("payload readable");
        assert!(
            body.lines()
                .next()
                .expect("first line")
                .starts_with("rm -f --"),
            "the payload must remove itself first: {body:?}"
        );
        assert!(
            body.contains(&command),
            "the payload must carry the command"
        );

        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "payload must not be world readable");
        let dir_mode = std::fs::metadata(&dir)
            .expect("dir metadata")
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o700);

        staged.discard();
        assert!(!path.exists(), "discard must remove an undelivered payload");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// refs #2862: the typed line is a function of the path, not the payload,
    /// which is the property that takes the line cap out of play.
    #[test]
    fn the_typed_line_does_not_grow_with_the_command() {
        let dir = scratch("length");
        let small = stage_in(&dir, &format!("claude '{}'", "A".repeat(1_000)), "zsh", "a")
            .expect("stage small");
        let large = stage_in(
            &dir,
            &format!("claude '{}'", "A".repeat(900_000)),
            "zsh",
            "a",
        )
        .expect("stage large");
        assert_eq!(
            small.text.len(),
            large.text.len(),
            "a 1 KB and a 900 KB command must type the same number of bytes"
        );
        small.discard();
        large.discard();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// fish dropped `.` as a source alias, so it needs the spelled-out form.
    #[test]
    fn fish_sources_with_its_own_keyword() {
        let dir = scratch("fish");
        let command = format!("claude '{}'", "A".repeat(4000));
        let staged = stage_in(&dir, &command, "fish", "agent-claude").expect("stage");
        assert!(
            staged.text.starts_with("source "),
            "fish must not be handed `.`: {}",
            staged.text
        );
        staged.discard();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
