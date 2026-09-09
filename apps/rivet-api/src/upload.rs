//! Converting a model a user hands the server into a scene it can draw.
//!
//! The conversion is the `rivet` binary, run as a child process. That is a
//! deliberate choice rather than a shortcut: a decode holds gigabytes and can
//! fail on a malformed file, and a request handler is the wrong place for
//! either. As a child it is bounded, killable, and its stages arrive on a pipe
//! - which is exactly what the progress display needs.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// Largest upload accepted, unless `--max-upload` says otherwise. The corpus
/// runs to 450 MB a file, so the default has to clear that.
pub const DEFAULT_MAX_UPLOAD_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Largest IFC accepted by the current in-memory STEP reader. Its compact
/// entity table still needs substantially more memory than the source text;
/// refusing a larger file is preferable to letting an upload kill the host.
pub const DEFAULT_MAX_IFC_BYTES: u64 = 256 * 1024 * 1024;

/// What a model may be called once it is on disk. A name is derived from what
/// the client sent rather than trusted: everything outside this set becomes an
/// underscore, so no upload can name a path.
fn sanitise(name: &str) -> String {
    let stem = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let stem = stem
        .strip_suffix(".rvt")
        .or_else(|| stem.strip_suffix(".RVT"))
        .or_else(|| stem.strip_suffix(".ifc"))
        .or_else(|| stem.strip_suffix(".IFC"))
        .unwrap_or(stem);
    let cleaned: String = stem
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '_'
            }
        })
        .take(96)
        .collect();
    let trimmed = cleaned.trim_matches('_');
    if trimmed.is_empty() {
        "model".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// The format an upload is in, decided by what the bytes say rather than by
/// what the name claims.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Rvt,
    Ifc,
}

impl Format {
    /// Read the format from the file's leading bytes.
    ///
    /// An RVT is a compound file, which always begins with the OLE signature.
    /// An IFC is STEP text, which begins with `ISO-10303-21`, possibly behind
    /// whitespace or a byte-order mark.
    #[must_use]
    pub fn sniff(head: &[u8]) -> Option<Self> {
        const OLE: [u8; 8] = [0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1];
        if head.starts_with(&OLE) {
            return Some(Self::Rvt);
        }
        let text = head.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(head);
        let text = String::from_utf8_lossy(&text[..text.len().min(64)]);
        text.trim_start()
            .starts_with("ISO-10303-21")
            .then_some(Self::Ifc)
    }

    #[must_use]
    pub fn extension(self) -> &'static str {
        match self {
            Self::Rvt => "rvt",
            Self::Ifc => "ifc",
        }
    }
}

/// Where uploads and the scenes made from them live.
#[derive(Clone, Debug)]
pub struct Uploads {
    /// The directory scenes are served from.
    pub scenes: PathBuf,
    /// The `rivet` binary that does the conversion.
    pub rivet: PathBuf,
    pub max_bytes: u64,
}

/// The `rivet` binary to run: the one beside this executable, which is where
/// a cargo build and an installed pair both put it.
#[must_use]
pub fn rivet_beside_this_executable() -> Option<PathBuf> {
    let directory = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let candidate = directory.join(if cfg!(windows) { "rivet.exe" } else { "rivet" });
    candidate.is_file().then_some(candidate)
}

impl Uploads {
    /// Save a request body to the uploads directory, refusing anything that is
    /// not a model and anything over the size cap.
    ///
    /// # Errors
    ///
    /// Fails where the body cannot be read or stored, where it is too large,
    /// or where its leading bytes are neither an RVT nor an IFC.
    pub fn receive(
        &self,
        name: &str,
        body: &mut dyn Read,
    ) -> Result<(PathBuf, Format, String), String> {
        let directory = self.scenes.join("uploads");
        std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;

        // The format is read from the first bytes, so the extension a client
        // claims never decides how the file is treated.
        let mut head = [0_u8; 64];
        let mut filled = 0;
        while filled < head.len() {
            match body.read(&mut head[filled..]) {
                Ok(0) => break,
                Ok(read) => filled += read,
                Err(error) => return Err(error.to_string()),
            }
        }
        let Some(format) = Format::sniff(&head[..filled]) else {
            return Err("this is neither a Revit file nor an IFC file".to_owned());
        };

        let stem = sanitise(name);
        let max_bytes = if format == Format::Ifc {
            self.max_bytes.min(DEFAULT_MAX_IFC_BYTES)
        } else {
            self.max_bytes
        };
        let path = directory.join(format!("{stem}.{}", format.extension()));
        let mut file = std::fs::File::create(&path).map_err(|error| error.to_string())?;
        file.write_all(&head[..filled])
            .map_err(|error| error.to_string())?;
        let mut written = filled as u64;
        let mut buffer = vec![0_u8; 1 << 20];
        loop {
            let read = body.read(&mut buffer).map_err(|error| error.to_string())?;
            if read == 0 {
                break;
            }
            written += read as u64;
            if written > max_bytes {
                drop(file);
                let _ = std::fs::remove_file(&path);
                return Err(format!(
                    "the {} upload is larger than the {} MB this server can safely convert",
                    format.extension().to_uppercase(),
                    max_bytes / (1024 * 1024)
                ));
            }
            file.write_all(&buffer[..read])
                .map_err(|error| error.to_string())?;
        }
        file.flush().map_err(|error| error.to_string())?;
        Ok((path, format, stem))
    }

    /// Start the conversion. The child's stdout carries one JSON object per
    /// stage, which [`stream_progress`] forwards to the caller as it arrives.
    ///
    /// # Errors
    ///
    /// Fails where the converter cannot be started.
    pub fn convert(&self, source: &Path, scene_name: &str) -> std::io::Result<Child> {
        let scene = self.scenes.join(format!("{scene_name}.rvs"));
        Command::new(&self.rivet)
            .arg("export-scene")
            .arg(source)
            .arg("--output")
            .arg(&scene)
            .arg("--progress")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
    }
}

/// What a conversion has done so far, as the page asks about it.
///
/// The conversion runs in its own thread and writes here; a request reads it.
/// Polling rather than holding the response open is deliberate: `tiny_http`
/// buffers a chunked body until the chunk fills, so a streamed reply would
/// arrive all at once at the end - and a 40-second held connection is fragile
/// in a way that asking again is not.
#[derive(Clone, Debug)]
pub struct Job {
    /// Stages that have finished, with the seconds each took.
    pub finished: Vec<(String, f64)>,
    /// The stage running now, if one is.
    pub running: Option<String>,
    pub seconds: f64,
    pub state: JobState,
    /// The scene, once it exists.
    pub scene: Option<String>,
    pub error: Option<String>,
    /// The converter's own summary lines, for the panel to show.
    pub notes: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobState {
    Running,
    Done,
    Failed,
}

impl Job {
    #[must_use]
    pub fn new() -> Self {
        Self {
            finished: Vec::new(),
            running: None,
            seconds: 0.0,
            state: JobState::Running,
            scene: None,
            error: None,
            notes: Vec::new(),
        }
    }

    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "state": match self.state {
                JobState::Running => "running",
                JobState::Done => "done",
                JobState::Failed => "failed",
            },
            "stages": self.finished.iter()
                .map(|(name, seconds)| serde_json::json!({ "name": name, "seconds": seconds }))
                .collect::<Vec<_>>(),
            "running": self.running,
            "seconds": self.seconds,
            "scene": self.scene,
            "error": self.error,
            "notes": self.notes,
        })
    }
}

impl Default for Job {
    fn default() -> Self {
        Self::new()
    }
}

/// Read the converter's stages into `job` until it exits.
pub fn follow(mut child: Child, scene_name: &str, job: &std::sync::Mutex<Job>) {
    let update = |change: &dyn Fn(&mut Job)| {
        if let Ok(mut held) = job.lock() {
            change(&mut held);
        }
    };
    if let Some(stdout) = child.stdout.take() {
        for read in BufReader::new(stdout).lines().map_while(Result::ok) {
            match serde_json::from_str::<serde_json::Value>(&read) {
                Ok(value) => {
                    let stage = value["stage"].as_str().unwrap_or_default().to_owned();
                    let total = value["totalSeconds"].as_f64().unwrap_or_default();
                    if value["event"] == "begin" {
                        update(&|held: &mut Job| {
                            held.running = Some(stage.clone());
                            held.seconds = total;
                        });
                    } else {
                        let seconds = value["seconds"].as_f64().unwrap_or_default();
                        update(&|held: &mut Job| {
                            held.finished.push((stage.clone(), seconds));
                            held.running = None;
                            held.seconds = total;
                        });
                    }
                }
                // The converter prints its summary as prose. Keep it: it is
                // what says how many elements and triangles were made.
                Err(_) if !read.trim().is_empty() => {
                    update(&|held: &mut Job| held.notes.push(read.clone()));
                }
                Err(_) => {}
            }
        }
    }
    let status = child.wait();
    let mut stderr_text = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        let _ = stderr.read_to_string(&mut stderr_text);
    }
    let succeeded = status.is_ok_and(|status| status.success());
    update(&|held: &mut Job| {
        held.running = None;
        if succeeded {
            held.state = JobState::Done;
            held.scene = Some(scene_name.to_owned());
        } else {
            held.state = JobState::Failed;
            held.error = Some(if stderr_text.trim().is_empty() {
                "the conversion failed".to_owned()
            } else {
                stderr_text.trim().to_owned()
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{Format, sanitise};

    #[test]
    fn derives_a_name_that_cannot_leave_the_directory() {
        assert_eq!(sanitise("../../etc/passwd"), "passwd");
        assert_eq!(sanitise("C:\\models\\Tower.rvt"), "Tower");
        assert_eq!(sanitise("AR_S1.rvt"), "AR_S1");
        assert_eq!(sanitise("модель.rvt"), "model");
        assert_eq!(sanitise(""), "model");
        assert_eq!(sanitise("..."), "model");
    }

    #[test]
    fn reads_the_format_from_the_bytes_and_not_the_name() {
        let ole = [0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1, 0, 0];
        assert_eq!(Format::sniff(&ole), Some(Format::Rvt));
        assert_eq!(Format::sniff(b"ISO-10303-21;\nHEADER;"), Some(Format::Ifc));
        assert_eq!(
            Format::sniff(b"\xef\xbb\xbfISO-10303-21;"),
            Some(Format::Ifc)
        );
        assert_eq!(Format::sniff(b"\n  ISO-10303-21;"), Some(Format::Ifc));
        assert_eq!(Format::sniff(b"PK\x03\x04 a zip"), None);
        assert_eq!(Format::sniff(b""), None);
    }
}
