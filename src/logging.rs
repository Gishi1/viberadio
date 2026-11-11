//! File logging and a crash record in the data folder.
//!
//! The desktop app has no console, so the log and the panic hook are the only
//! way a user can hand over what happened. Both files are appended and bounded
//! at 1 MiB, and the log is teed to stderr so `cargo run` still shows it.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const MAX_LOG_BYTES: u64 = 1_048_576;

pub fn init() {
    let data_dir = crate::app::data_dir();
    let log_path = data_dir.as_ref().map(|dir| dir.join("viberadio.log"));
    // Dependencies such as zbus and tracing emit very chatty INFO records; keep
    // the default at warn and enable info only for this crate.
    let mut builder = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("warn,viberadio=info"),
    );
    if let Some(path) = log_path {
        match RotatingFile::open(path) {
            Ok(file) => {
                builder.target(env_logger::Target::Pipe(Box::new(Tee {
                    file,
                    stderr: io::stderr(),
                })));
            }
            Err(e) => eprintln!("could not open the log file: {e}"),
        }
    }
    builder.init();

    let crash_path = data_dir.map(|dir| dir.join("viberadio-crash.log"));
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(path) = &crash_path {
            let _ = write_crash(path, info);
        }
        previous(info);
    }));
}

/// Append-only file that rotates to `.log.1` once it reaches 1 MiB. Rotation is
/// checked per write, so a long-running session is bounded too.
struct RotatingFile {
    path: PathBuf,
    file: File,
    written: u64,
}

impl RotatingFile {
    fn open(path: PathBuf) -> io::Result<Self> {
        let written = std::fs::metadata(&path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            path,
            file,
            written,
        })
    }

    fn rotate(&mut self) -> io::Result<()> {
        let _ = std::fs::rename(&self.path, self.path.with_extension("log.1"));
        self.file = OpenOptions::new().create(true).append(true).open(&self.path)?;
        self.written = 0;
        Ok(())
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.written + buf.len() as u64 > MAX_LOG_BYTES {
            self.rotate()?;
        }
        let written = self.file.write(buf)?;
        self.written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// Keep stderr visible when the app is launched from a terminal.
struct Tee {
    file: RotatingFile,
    stderr: io::Stderr,
}

impl Write for Tee {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.file.write(buf)?;
        let _ = self.stderr.write_all(&buf[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        let _ = self.stderr.flush();
        self.file.flush()
    }
}

fn write_crash(path: &Path, info: &std::panic::PanicHookInfo<'_>) -> io::Result<()> {
    let mut file = RotatingFile::open(path.to_path_buf())?;
    let location = info
        .location()
        .map(|location| {
            format!(
                "{}:{}:{}",
                location.file(),
                location.line(),
                location.column()
            )
        })
        .unwrap_or_else(|| "unknown location".to_string());
    let thread = std::thread::current()
        .name()
        .unwrap_or("unnamed")
        .to_string();
    writeln!(
        file,
        "{}",
        crash_line(&thread, &location, &payload(info))
    )?;
    let backtrace_enabled = std::env::var_os("RUST_BACKTRACE")
        .map(|value| value != std::ffi::OsStr::new("0"))
        .unwrap_or(false);
    if backtrace_enabled {
        let backtrace = std::backtrace::Backtrace::force_capture();
        writeln!(file, "{backtrace}")?;
    }
    file.flush()?;
    Ok(())
}

fn crash_line(thread: &str, location: &str, payload: &str) -> String {
    format!(
        "[{}] panic in thread '{thread}' at {location}: {payload}",
        chrono::Utc::now().to_rfc3339()
    )
}

fn payload(info: &std::panic::PanicHookInfo<'_>) -> String {
    if let Some(message) = info.payload().downcast_ref::<&str>() {
        message.to_string()
    } else if let Some(message) = info.payload().downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "viberadio-{name}-{}-{}.log",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("log.1"));
        path
    }

    #[test]
    fn crash_line_names_the_thread_location_and_message() {
        let line = crash_line("worker", "src/player.rs:42:7", "boom");
        assert!(line.contains("panic in thread 'worker'"));
        assert!(line.contains("src/player.rs:42:7"));
        assert!(line.ends_with(": boom"));
    }

    #[test]
    fn a_crash_record_is_appended_not_truncated() {
        let path = temp_path("crash-append");
        let mut file = RotatingFile::open(path.clone()).unwrap();
        writeln!(file, "first").unwrap();
        writeln!(
            file,
            "{}",
            crash_line("main", "src/main.rs:1:1", "second")
        )
        .unwrap();
        drop(file);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("first\n"));
        assert!(text.contains("second"));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("log.1"));
    }

    #[test]
    fn the_log_rotates_at_the_size_limit() {
        let path = temp_path("rotate");
        let mut file = RotatingFile::open(path.clone()).unwrap();
        file.write_all(&vec![b'x'; MAX_LOG_BYTES as usize]).unwrap();
        file.write_all(b"tail").unwrap();
        drop(file);
        assert!(path.with_extension("log.1").exists());
        assert!(std::fs::metadata(&path).unwrap().len() < MAX_LOG_BYTES);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("log.1"));
    }
}
