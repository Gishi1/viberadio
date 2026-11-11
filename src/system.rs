//! Platform integration for the sleep timer and the file manager.

use std::process::Command;

/// Program and arguments that power the machine off.
pub fn shutdown_command() -> (&'static str, &'static [&'static str]) {
    #[cfg(target_os = "linux")]
    {
        ("systemctl", &["poweroff"])
    }
    #[cfg(target_os = "windows")]
    {
        ("shutdown", &["/s", "/t", "0"])
    }
    #[cfg(target_os = "macos")]
    {
        ("osascript", &["-e", "tell app \"System Events\" to shut down"])
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        ("shutdown", &["-h", "now"])
    }
}

/// Ask the OS to power off. The command is spawned, not waited on, so a
/// polkit or confirmation prompt cannot block the UI.
pub fn shutdown() -> std::io::Result<()> {
    let (program, args) = shutdown_command();
    Command::new(program).args(args).spawn().map(|_| ())
}

/// Program that opens a folder in the desktop's file manager.
pub fn open_command() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        "xdg-open"
    }
    #[cfg(target_os = "windows")]
    {
        "explorer"
    }
    #[cfg(target_os = "macos")]
    {
        "open"
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        "xdg-open"
    }
}

/// Open a folder or file with the desktop's default handler.
pub fn open_path(path: &std::path::Path) -> std::io::Result<()> {
    Command::new(open_command()).arg(path).spawn().map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chooses_a_shutdown_command_for_this_platform() {
        let (program, args) = shutdown_command();
        assert!(!program.is_empty());
        assert!(!args.is_empty());
        #[cfg(target_os = "linux")]
        assert_eq!(program, "systemctl");
        #[cfg(target_os = "windows")]
        assert_eq!(program, "shutdown");
        #[cfg(target_os = "macos")]
        assert_eq!(program, "osascript");
    }

    #[test]
    fn chooses_a_folder_opener_for_this_platform() {
        let program = open_command();
        assert!(!program.is_empty());
        #[cfg(target_os = "linux")]
        assert_eq!(program, "xdg-open");
        #[cfg(target_os = "windows")]
        assert_eq!(program, "explorer");
        #[cfg(target_os = "macos")]
        assert_eq!(program, "open");
    }
}
