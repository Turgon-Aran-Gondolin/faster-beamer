use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

#[derive(Debug, PartialEq, Eq)]
pub enum RedirectResult {
    Redirected,
    DifferentOptions,
    NoGuard,
    IoError,
}

pub struct GuardRegistration {
    metadata_path: PathBuf,
}

impl Drop for GuardRegistration {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.metadata_path);
    }
}

struct GuardMetadata {
    pid: u32,
    input_path: String,
    fingerprint: String,
    addr: String,
}

fn guard_dir() -> Option<PathBuf> {
    dirs::cache_dir().map(|dir| dir.join("faster-beamer").join("guards"))
}

fn guard_metadata_path(input_path: &Path) -> Option<PathBuf> {
    let digest = md5::compute(input_path.to_string_lossy().as_bytes());
    guard_dir().map(|dir| dir.join(format!("{:x}.guard", digest)))
}

pub fn invocation_fingerprint(
    build_mode: &str,
    output_file: &str,
    correct_frame_numbers: bool,
    use_tree_sitter: bool,
    multi_pass: Option<&str>,
    bibliography: Option<&str>,
    engine: &str,
    precompile_preamble: bool,
    no_precompile_preamble: bool,
    force_recompile: bool,
    parallel: bool,
    jobs: Option<&str>,
    compiler_options: Vec<&str>,
) -> String {
    let mut parts = vec![
        format!("mode={}", build_mode),
        format!("output={}", output_file),
        format!("frame_numbers={}", correct_frame_numbers),
        format!("tree_sitter={}", use_tree_sitter),
        format!("multi_pass={}", multi_pass.unwrap_or("")),
        format!("bibliography={}", bibliography.unwrap_or("")),
        format!("engine={}", engine),
        format!("precompile_preamble={}", precompile_preamble),
        format!("no_precompile_preamble={}", no_precompile_preamble),
        format!("force_recompile={}", force_recompile),
        format!("parallel={}", parallel),
        format!("jobs={}", jobs.unwrap_or("")),
    ];

    parts.extend(
        compiler_options
            .into_iter()
            .map(|option| format!("compiler_option={}", option)),
    );
    parts.join("\n")
}

fn parse_metadata(content: &str) -> Option<GuardMetadata> {
    let mut pid = None;
    let mut input_path = None;
    let mut fingerprint = None;
    let mut addr = None;

    for line in content.lines() {
        if let Some(value) = line.strip_prefix("pid=") {
            pid = value.parse::<u32>().ok();
        } else if let Some(value) = line.strip_prefix("input=") {
            input_path = Some(value.to_owned());
        } else if let Some(value) = line.strip_prefix("fingerprint=") {
            fingerprint = Some(value.replace("\\n", "\n"));
        } else if let Some(value) = line.strip_prefix("addr=") {
            addr = Some(value.to_owned());
        }
    }

    Some(GuardMetadata {
        pid: pid?,
        input_path: input_path?,
        fingerprint: fingerprint?,
        addr: addr?,
    })
}

fn write_metadata(path: &Path, metadata: &GuardMetadata) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    fs::write(
        path,
        format!(
            "pid={}\ninput={}\nfingerprint={}\naddr={}\n",
            metadata.pid,
            metadata.input_path,
            metadata.fingerprint.replace('\n', "\\n"),
            metadata.addr
        ),
    )
}

fn request_rebuild(addr: &str) -> std::io::Result<bool> {
    let mut stream = TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(b"rebuild\n")?;

    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    Ok(response.trim() == "ok")
}

pub fn redirect_to_guard(input_path: &Path, fingerprint: &str) -> RedirectResult {
    let metadata_path = match guard_metadata_path(input_path) {
        Some(path) => path,
        None => return RedirectResult::NoGuard,
    };
    let content = match fs::read_to_string(&metadata_path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return RedirectResult::NoGuard,
        Err(_) => return RedirectResult::IoError,
    };
    let metadata = match parse_metadata(&content) {
        Some(metadata) => metadata,
        None => {
            let _ = fs::remove_file(&metadata_path);
            return RedirectResult::NoGuard;
        }
    };

    if metadata.input_path != input_path.to_string_lossy() || metadata.fingerprint != fingerprint {
        return RedirectResult::DifferentOptions;
    }

    match request_rebuild(&metadata.addr) {
        Ok(true) => RedirectResult::Redirected,
        Ok(false) => RedirectResult::IoError,
        Err(_) => {
            let _ = fs::remove_file(&metadata_path);
            RedirectResult::NoGuard
        }
    }
}

pub fn start_guard(
    input_path: &Path,
    fingerprint: &str,
    rebuild_tx: Sender<()>,
) -> std::io::Result<GuardRegistration> {
    let metadata_path = guard_metadata_path(input_path)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "missing cache dir"))?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?.to_string();

    write_metadata(
        &metadata_path,
        &GuardMetadata {
            pid: std::process::id(),
            input_path: input_path.to_string_lossy().into_owned(),
            fingerprint: fingerprint.to_owned(),
            addr,
        },
    )?;

    thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = match stream {
                Ok(stream) => stream,
                Err(_) => continue,
            };
            let mut request = String::new();
            let mut reader = BufReader::new(&stream);
            if reader.read_line(&mut request).is_err() {
                continue;
            }
            if request.trim() == "rebuild" {
                let _ = rebuild_tx.send(());
                let _ = stream.write_all(b"ok\n");
            }
        }
    });

    Ok(GuardRegistration { metadata_path })
}

#[cfg(test)]
mod tests {
    use super::invocation_fingerprint;

    #[test]
    fn invocation_fingerprint_changes_with_build_options() {
        let base = invocation_fingerprint(
            "preview",
            "slides.pdf",
            false,
            false,
            None,
            None,
            "pdflatex",
            false,
            false,
            false,
            false,
            None,
            Vec::new(),
        );
        let changed = invocation_fingerprint(
            "pdfunite",
            "slides.pdf",
            false,
            false,
            None,
            None,
            "pdflatex",
            false,
            false,
            false,
            false,
            None,
            Vec::new(),
        );

        assert_ne!(base, changed);
    }
}
