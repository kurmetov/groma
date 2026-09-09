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
/// runs to 450 MB a file, so the default has to clear that. An upload is
/// streamed to disk a megabyte at a time and costs no memory to hold, so this
/// guards the disk; what can be *converted* is [`max_ifc_bytes`].
pub const DEFAULT_MAX_UPLOAD_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// How much memory the STEP reader needs, as a multiple of the source text.
///
/// Measured on SMALL's own 643 MB IFC export: 2.20 GB peak, a 3.4x ratio,
/// steady from 65 MB up to a synthetic 2.4 GiB. Four is that with headroom.
const IFC_MEMORY_RATIO: u64 = 4;

/// The share of the memory budget one conversion may plan to occupy.
///
/// A conversion is not the only thing on the machine. Sizing the ceiling to
/// the *whole* budget is what turned a workstation with an editor and a
/// browser open into a swapping brick: the arithmetic said the file fit, and
/// it did, with nothing left for anything else. Half is what a single job may
/// assume, and [`room_to_convert`] still checks the moment it starts.
const CONVERSION_SHARE: u64 = 2;

/// Smallest IFC ceiling, whatever the host says. Below this the reader is
/// being denied files it has always managed.
pub const MIN_MAX_IFC_BYTES: u64 = 256 * 1024 * 1024;

/// Largest IFC ceiling this will choose on its own.
///
/// Deliberately modest. A 2 GiB IFC already asks about 7 GB of memory to
/// parse, which is a lot to spend without being told to; an operator who
/// wants more says so with `--max-upload` on a host with the memory for it.
/// Scaling this to a big machine's whole capacity - 14.8 GiB on a 59 GB box -
/// is exactly the mistake that froze one.
pub const MAX_AUTOMATIC_IFC_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// The memory this process may actually use, in bytes.
///
/// Inside a container `/proc/meminfo` reports the *host's* memory, not the
/// cgroup's limit, so a 4 GB container reads 59 GB and plans to use all of it
/// until the kernel kills it. The cgroup limit is checked first for that
/// reason, v2 then v1, and `MemTotal` is the fallback for a bare host.
fn memory_budget() -> Option<u64> {
    let cgroup = [
        "/sys/fs/cgroup/memory.max",
        "/sys/fs/cgroup/memory/memory.limit_in_bytes",
    ]
    .into_iter()
    .filter_map(|path| std::fs::read_to_string(path).ok())
    .find_map(|text| text.trim().parse::<u64>().ok())
    // An unlimited cgroup reports "max" (v2) or a number near u64::MAX
    // (v1), neither of which is a budget.
    .filter(|limit| *limit < u64::MAX / 2);
    cgroup
        .or_else(|| meminfo_field("MemTotal:"))
        .filter(|budget| *budget > 0)
}

/// One `/proc/meminfo` field, in bytes.
fn meminfo_field(field: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = status.lines().find(|line| line.starts_with(field))?;
    line.split_whitespace()
        .nth(1)
        .and_then(|value| value.parse::<u64>().ok())
        .and_then(|kilobytes| kilobytes.checked_mul(1024))
}

/// Largest IFC this host will be asked to convert.
///
/// The reader holds the whole entity table in memory, so the ceiling belongs
/// to the machine rather than to a constant: 256 MB refused files a
/// workstation converts in twelve seconds, and the same number is still too
/// generous on a small container. This scales between the two and stops at
/// [`MAX_AUTOMATIC_IFC_BYTES`], because a default should be safe on a busy
/// machine rather than merely arithmetically possible on an idle one.
#[must_use]
pub fn max_ifc_bytes() -> u64 {
    memory_budget()
        .map_or(MIN_MAX_IFC_BYTES, |budget| {
            budget / (IFC_MEMORY_RATIO * CONVERSION_SHARE)
        })
        .clamp(MIN_MAX_IFC_BYTES, MAX_AUTOMATIC_IFC_BYTES)
}

/// Whether there is memory free *now* to convert a source of `bytes`, or the
/// message explaining why not.
///
/// The ceiling above is a plan made from capacity; this is the check against
/// the moment. Refusing here costs the caller a clear error, where going ahead
/// costs everyone the machine - and a host that swaps is not one anybody can
/// see a progress bar on.
pub fn room_to_convert(bytes: u64, format: Format) -> Result<(), String> {
    if format != Format::Ifc {
        return Ok(());
    }
    let Some(needed) = bytes.checked_mul(IFC_MEMORY_RATIO) else {
        return Err("this file is too large to convert".to_owned());
    };
    let Some(free) = meminfo_field("MemAvailable:") else {
        return Ok(());
    };
    if needed > free {
        let megabytes = |value: u64| value / (1024 * 1024);
        return Err(format!(
            "converting this {} MB IFC needs about {} MB of memory and only {} MB is free; \
             close something or try again",
            megabytes(bytes),
            megabytes(needed),
            megabytes(free)
        ));
    }
    Ok(())
}

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

fn conversion_limit(format: Format, configured: u64) -> u64 {
    match format {
        Format::Ifc => configured.min(max_ifc_bytes()),
        Format::Rvt => configured,
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
        let max_bytes = conversion_limit(format, self.max_bytes);
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
    pub fn convert(
        &self,
        source: &Path,
        scene_name: &str,
        format: Format,
    ) -> std::io::Result<Child> {
        let scene = self.scenes.join(format!("{scene_name}.rvs"));
        let mut command = Command::new(&self.rivet);
        command
            .arg("export-scene")
            .arg(source)
            .arg("--output")
            .arg(&scene)
            .arg("--progress");
        // The converter guards its own reading with the same flag, and its
        // default is the constant this server used to stop at. Without saying
        // so, an IFC this server has just accepted would be refused by the
        // process it hands it to - so the ceiling the upload was measured
        // against is passed on. An RVT is left alone: there the flag bounds one
        // decoded member rather than the source, and is not ours to raise.
        if format == Format::Ifc {
            command
                .arg("--max-member-bytes")
                .arg(max_ifc_bytes().to_string());
        }
        command
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
    let finished = status.as_ref().ok().copied();
    let succeeded = finished.is_some_and(|status| status.success());
    let reason = failure_reason(finished, &stderr_text);
    update(&|held: &mut Job| {
        held.running = None;
        if succeeded {
            held.state = JobState::Done;
            held.scene = Some(scene_name.to_owned());
        } else {
            held.state = JobState::Failed;
            held.error = Some(reason.clone());
        }
    });
}

/// Why a conversion did not finish, in terms the page can show.
///
/// A converter killed for its memory says nothing on stderr - the kernel does
/// not give it the chance - so the bare "the conversion failed" that used to
/// appear was the one case where the reason mattered most and was least
/// visible. A `SIGKILL` with no output is that case: under a container memory
/// limit it is the cgroup's OOM killer, and on a bare host the kernel's.
fn failure_reason(status: Option<std::process::ExitStatus>, stderr_text: &str) -> String {
    let said = stderr_text.trim();
    if !said.is_empty() {
        return said.to_owned();
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if status.and_then(|status| status.signal()) == Some(9) {
            let budget = memory_budget().map_or_else(
                || "this machine".to_owned(),
                |bytes| format!("the {} MB available to it", bytes / (1024 * 1024)),
            );
            return format!(
                "the converter ran out of memory and was killed. This model needs more than \
                 {budget}. Geometry-heavy models cost far more than their file size suggests - \
                 a 709 MB structural IFC measured here reached 39 GB, against 2.2 GB for a \
                 643 MB one - so raise the limit for the container or convert it with the \
                 `rivet export-scene` command directly."
            );
        }
    }
    let _ = status;
    "the conversion failed".to_owned()
}

#[cfg(test)]
mod tests {
    use super::{
        Format, MAX_AUTOMATIC_IFC_BYTES, MIN_MAX_IFC_BYTES, conversion_limit, max_ifc_bytes,
        memory_budget, room_to_convert, sanitise,
    };

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

    #[test]
    fn bounds_the_in_memory_ifc_reader_without_lowering_the_rvt_upload_cap() {
        let configured = 2 * 1024 * 1024 * 1024;
        assert_eq!(
            conversion_limit(Format::Ifc, configured),
            configured.min(max_ifc_bytes())
        );
        assert_eq!(conversion_limit(Format::Rvt, configured), configured);
        // A stated limit below the ceiling still wins: this narrows, never
        // widens, what was configured.
        assert_eq!(conversion_limit(Format::Ifc, 1024), 1024);
    }

    #[test]
    fn the_ifc_ceiling_stays_between_its_floor_and_a_modest_default() {
        let ceiling = max_ifc_bytes();
        assert!(ceiling >= MIN_MAX_IFC_BYTES, "{ceiling} is below the floor");
        // The point of the clamp: a big machine must not talk this into a
        // multi-gigabyte default just because the arithmetic allows it.
        assert!(
            ceiling <= MAX_AUTOMATIC_IFC_BYTES,
            "{ceiling} is above what may be chosen without being asked"
        );
        // Reading twice gives the same answer - the ceiling is capacity, not
        // whatever is free this second.
        assert_eq!(max_ifc_bytes(), ceiling);
        if std::path::Path::new("/proc/meminfo").exists() {
            assert!(memory_budget().is_some_and(|budget| budget > 0));
        }
    }

    #[test]
    fn refuses_a_conversion_the_free_memory_will_not_hold() {
        // An RVT is not held in memory this way and is never refused here.
        assert!(room_to_convert(u64::MAX, Format::Rvt).is_ok());
        // A small IFC always fits.
        assert!(room_to_convert(1024, Format::Ifc).is_ok());
        // One the size of the machine does not, and says so rather than
        // taking the host down to find out.
        let refusal = room_to_convert(u64::MAX / 2, Format::Ifc);
        assert!(refusal.is_err(), "an impossible conversion was allowed");
    }
}
