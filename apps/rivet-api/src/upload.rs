//! Converting a model a user hands the server into a scene it can draw.
//!
//! The conversion is the `rivet` binary, run as a child process. That is a
//! deliberate choice rather than a shortcut: a decode holds gigabytes and can
//! fail on a malformed file, and a request handler is the wrong place for
//! either. As a child it is bounded, killable, and its stages arrive on a pipe
//! - which is exactly what the progress display needs.

pub use bim_convert::Format;

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// Largest upload accepted, unless `--max-upload` says otherwise. The corpus
/// runs to 450 MB a file, so the default has to clear that. An upload is
/// streamed to disk a megabyte at a time and costs no memory to hold, so this
/// guards the disk; what can be *converted* is [`max_ifc_bytes`].
pub const DEFAULT_MAX_UPLOAD_BYTES: u64 = 2 * 1024 * 1024 * 1024;

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
            budget / (Format::Ifc.memory_ratio().unwrap_or(1) * CONVERSION_SHARE)
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
    // Only a whole-file reader's cost scales with the source, and only such a
    // format declares a ratio. An RVT is bounded a member at a time instead.
    let Some(ratio) = format.memory_ratio() else {
        return Ok(());
    };
    let Some(needed) = bytes.checked_mul(ratio) else {
        return Err("this file is too large to convert".to_owned());
    };
    let Some(free) = meminfo_field("MemAvailable:") else {
        return Ok(());
    };
    if needed > free {
        let megabytes = |value: u64| value / (1024 * 1024);
        return Err(format!(
            "converting this {} MB {} needs about {} MB of memory and only {} MB is free; \
             close something or try again",
            megabytes(bytes),
            format.label(),
            megabytes(needed),
            megabytes(free)
        ));
    }
    Ok(())
}

/// The name a scene made from a federation takes, sanitised like any other.
#[must_use]
pub fn scene_name(set: &str) -> String {
    sanitise(set)
}

/// Whether there is memory free now to convert a whole federation.
///
/// The sources are read one after another but their models are held together,
/// so what has to fit is the sum. Only a whole-file reader's cost scales with
/// its source, so only those contribute.
///
/// # Errors
///
/// The message explaining which conversion will not fit.
pub fn room_to_convert_all(sources: &[(u64, Format)]) -> Result<(), String> {
    let mut planned = 0_u64;
    for (bytes, format) in sources {
        if format.memory_ratio().is_some() {
            planned = planned.saturating_add(*bytes);
        }
    }
    // Charged against the format that actually scales; a set of RVTs plans
    // nothing here and is bounded a member at a time, exactly as one is.
    room_to_convert(planned, Format::Ifc)
}

/// What a model may be called once it is on disk. A name is derived from what
/// the client sent rather than trusted: everything outside this set becomes an
/// underscore, so no upload can name a path.
fn sanitise(name: &str) -> String {
    let stem = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let stem = Format::strip_extension(stem);
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

fn conversion_limit(format: Format, configured: u64) -> u64 {
    if format.memory_ratio().is_some() {
        configured.min(max_ifc_bytes())
    } else {
        configured
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
    /// `set` stages the file beside the others of a federation instead of
    /// converting it on its own; see [`Uploads::staged`].
    ///
    /// # Errors
    ///
    /// Fails where the body cannot be read or stored, where it is too large,
    /// or where its leading bytes name no format this server can read.
    pub fn receive(
        &self,
        set: Option<&str>,
        name: &str,
        body: &mut dyn Read,
    ) -> Result<(PathBuf, Format, String), String> {
        let directory = match set {
            Some(set) => self.set_directory(set),
            None => self.scenes.join("uploads"),
        };
        std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;

        // The format is read from the first bytes, so the extension a client
        // claims never decides how the file is treated.
        let mut head = [0_u8; bim_convert::SNIFF_BYTES];
        let mut filled = 0;
        while filled < head.len() {
            match body.read(&mut head[filled..]) {
                Ok(0) => break,
                Ok(read) => filled += read,
                Err(error) => return Err(error.to_string()),
            }
        }
        let format = match Format::sniff(&head[..filled]) {
            Some(format) if format.is_readable() => format,
            Some(format) => {
                return Err(format!(
                    "this is a{} {}, which this server cannot convert yet",
                    if format.label().starts_with(['A', 'E', 'I', 'O', 'U']) {
                        "n"
                    } else {
                        ""
                    },
                    format.label()
                ));
            }
            None => return Err("this is not a model file this server reads".to_owned()),
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

    /// Where the files of one federation are held until it is converted.
    ///
    /// A directory per set, so that two clients federating at once cannot see
    /// each other's files, and so that the set can be emptied by removing one
    /// directory. The name is sanitised like any other, because it comes from
    /// a request.
    fn set_directory(&self, set: &str) -> PathBuf {
        self.scenes.join("uploads").join("sets").join(sanitise(set))
    }

    /// The files held for one federation, in a deterministic order.
    ///
    /// Sorted by name rather than left in directory order: the order decides
    /// which document is which in the output, and a conversion repeated on the
    /// same files has to produce the same identifiers.
    #[must_use]
    pub fn staged(&self, set: &str) -> Vec<PathBuf> {
        let mut held: Vec<PathBuf> = std::fs::read_dir(self.set_directory(set))
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_file())
            .collect();
        held.sort();
        held
    }

    /// Forget the files of one federation, once it has been converted.
    pub fn discard_set(&self, set: &str) {
        let _ = std::fs::remove_dir_all(self.set_directory(set));
    }

    /// Start the conversion. The child's stdout carries one JSON object per
    /// stage, which [`stream_progress`] forwards to the caller as it arrives.
    ///
    /// # Errors
    ///
    /// Fails where the converter cannot be started.
    pub fn convert(
        &self,
        sources: &[PathBuf],
        scene_name: &str,
        formats: &[Format],
    ) -> std::io::Result<Child> {
        let scene = self.scenes.join(format!("{scene_name}.rvs"));
        let mut command = Command::new(&self.rivet);
        command.arg("export-scene");
        // Several sources are read as one federated model. The converter
        // qualifies every identifier by the file it came from, so the scene
        // that comes back can be filtered by document.
        for source in sources {
            command.arg(source);
        }
        command.arg("--output").arg(&scene).arg("--progress");
        // The converter guards its own reading with the same flag, and its
        // default is the constant this server used to stop at. Without saying
        // so, an IFC this server has just accepted would be refused by the
        // process it hands it to - so the ceiling the upload was measured
        // against is passed on. An RVT is left alone: there the flag bounds one
        // decoded member rather than the source, and is not ours to raise.
        if formats.iter().any(|format| format.memory_ratio().is_some()) {
            command
                .arg("--max-member-bytes")
                .arg(max_ifc_bytes().to_string());
        }
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
    }

    /// Where an exported IFC is written, and where [`Uploads::exported`]
    /// serves it from.
    #[must_use]
    pub fn exports(&self) -> PathBuf {
        self.scenes.join("exports")
    }

    /// One export produced by this server, by name and extension. The extension is one this server
    /// writes, never something a request supplies, so no name can reach a file
    /// the exporter did not make.
    #[must_use]
    pub fn exported_as(&self, name: &str, extension: &str) -> Option<PathBuf> {
        if !["ifc", "jsonl"].contains(&extension) {
            return None;
        }
        let path = self
            .exports()
            .join(format!("{}.{extension}", sanitise(name)));
        path.is_file().then_some(path)
    }

    /// Write the recovered records as JSON lines, the other thing a Revit
    /// model converts to today.
    ///
    /// # Errors
    ///
    /// Fails where the exporter cannot be started.
    pub fn export_json(&self, source: &Path, name: &str, full: bool) -> std::io::Result<Child> {
        let directory = self.exports();
        std::fs::create_dir_all(&directory)?;
        let output = directory.join(format!("{name}.jsonl"));
        let mut command = Command::new(&self.rivet);
        command
            .arg("export-json")
            .arg(source)
            .arg("--output")
            .arg(&output)
            // Stdout is free once the export has a file of its own, so this
            // conversion reports its stages like the other two rather than
            // leaving the page to show one unexplained total.
            .arg("--progress");
        if full {
            command.arg("--full");
        }
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
    }

    /// Start an IFC export of an uploaded model, with the setup asked for.
    ///
    /// # Errors
    ///
    /// Fails where the exports directory cannot be made or the exporter cannot
    /// be started.
    pub fn export_ifc(
        &self,
        sources: &[PathBuf],
        name: &str,
        request: &IfcRequest,
    ) -> std::io::Result<Child> {
        let directory = self.exports();
        std::fs::create_dir_all(&directory)?;
        let output = directory.join(format!("{name}.ifc"));
        let mut command = Command::new(&self.rivet);
        command.arg("export-ifc");
        // Several sources are read as one federated model, exactly as a scene
        // conversion reads them.
        for source in sources {
            command.arg(source);
        }
        command.arg("--output").arg(&output);
        // The stages go to the caller the way a scene conversion's do, so the
        // page watching an export shows where the minute went rather than the
        // word "converting".
        command.arg("--progress");
        request.apply(&mut command);
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
    /// The IFC file, once it exists. A job produces one or the other: an
    /// upload becomes a scene to draw, an export becomes a file to download.
    pub ifc: Option<String>,
    /// The JSON-lines export, once it exists.
    pub json: Option<String>,
    pub error: Option<String>,
    /// The converter's own summary lines, for the panel to show.
    pub notes: Vec<String>,
    /// The name the model was uploaded under, its format and its size, so a
    /// conversion can still be described after it has finished and the page
    /// that started it has gone.
    pub source: Option<String>,
    pub format: Option<String>,
    pub bytes: u64,
    /// What the conversion wrote, once it has. Beside `bytes` this is the
    /// answer to the question a reader of a several-hundred-megabyte export
    /// asks first, and it is measured from the file rather than reported by
    /// the converter so that it describes what is actually on the disk.
    pub produced_bytes: Option<u64>,
    /// Seconds since the Unix epoch, for ordering a history a reader scrolls.
    pub started: u64,
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
            ifc: None,
            json: None,
            error: None,
            source: None,
            format: None,
            bytes: 0,
            produced_bytes: None,
            started: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_secs()),
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
            "ifc": self.ifc,
            "json": self.json,
            "error": self.error,
            "notes": self.notes,
            "source": self.source,
            "format": self.format,
            "bytes": self.bytes,
            "producedBytes": self.produced_bytes,
            "started": self.started,
        })
    }
}

impl Default for Job {
    fn default() -> Self {
        Self::new()
    }
}

/// The IFC export setup a request asks for.
///
/// The names are the exporter's own flags, so what a caller may ask for over
/// HTTP and what `rivet export-ifc` accepts stay one list rather than two that
/// drift. A parameter this does not know is refused: a setting silently
/// dropped is a file that is not what was asked for.
// One field per exporter flag, on purpose: this is the list of what a caller
// may ask for, and keeping it flat is what makes it readable beside
// `rivet export-ifc --help`.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IfcRequest {
    pub length_unit: Option<String>,
    pub no_revit_property_sets: bool,
    pub no_revit_type_property_sets: bool,
    pub no_ifc_common_property_sets: bool,
    pub base_quantities: bool,
    pub no_types: bool,
    /// A class mapping table already on this server, named by path.
    pub class_mapping: Option<PathBuf>,
}

impl IfcRequest {
    /// Read the setup out of a request's query parameters.
    ///
    /// # Errors
    ///
    /// Returns the message to answer with where a parameter is not one of
    /// these, or its value is not one this exporter has.
    pub fn from_params(
        params: &std::collections::BTreeMap<String, String>,
    ) -> Result<Self, String> {
        let mut request = Self::default();
        let flag = |value: &str| match value {
            "" | "1" | "true" | "yes" => Ok(true),
            "0" | "false" | "no" => Ok(false),
            other => Err(format!("{other} is not true or false")),
        };
        for (key, value) in params {
            match key.as_str() {
                // The model this export is for and, where several files are
                // read as one, which set they belong to and whether this is
                // the last of them. All three are the upload's business, not
                // the exporter's, and are handled before this is reached -
                // they are named here so that a federated export is not
                // refused for asking for a setting that is not one.
                "name" | "set" | "complete" => {}
                "length-unit" => match value.as_str() {
                    "metre" | "millimetre" => request.length_unit = Some(value.clone()),
                    other => {
                        return Err(format!("length-unit is metre or millimetre, not {other}"));
                    }
                },
                "no-revit-property-sets" => request.no_revit_property_sets = flag(value)?,
                "no-revit-type-property-sets" => {
                    request.no_revit_type_property_sets = flag(value)?;
                }
                "no-ifc-common-property-sets" => {
                    request.no_ifc_common_property_sets = flag(value)?;
                }
                "base-quantities" => request.base_quantities = flag(value)?,
                "no-types" => request.no_types = flag(value)?,
                "class-mapping" => request.class_mapping = Some(PathBuf::from(value)),
                other => return Err(format!("{other} is not an export setting")),
            }
        }
        Ok(request)
    }

    fn apply(&self, command: &mut Command) {
        if let Some(unit) = &self.length_unit {
            command.arg("--length-unit").arg(unit);
        }
        for (asked, flag) in [
            (self.no_revit_property_sets, "--no-revit-property-sets"),
            (
                self.no_revit_type_property_sets,
                "--no-revit-type-property-sets",
            ),
            (
                self.no_ifc_common_property_sets,
                "--no-ifc-common-property-sets",
            ),
            (self.base_quantities, "--base-quantities"),
            (self.no_types, "--no-types"),
        ] {
            if asked {
                command.arg(flag);
            }
        }
        if let Some(path) = &self.class_mapping {
            command.arg("--class-mapping").arg(path);
        }
    }
}

/// What a conversion is producing, so a finished job names the right thing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Product {
    Scene,
    Ifc,
    Json,
}

/// Read the converter's stages into `job` until it exits.
pub fn follow(
    mut child: Child,
    name: &str,
    product: Product,
    produced: Option<&Path>,
    job: &std::sync::Mutex<Job>,
) {
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
                            // A federation reports each stage once per source
                            // file. Summed into one row, because a progress
                            // display showing "Decode" three times says less
                            // than one showing what reading the sources cost.
                            if let Some(entry) =
                                held.finished.iter_mut().find(|(held, _)| *held == stage)
                            {
                                entry.1 += seconds;
                            } else {
                                held.finished.push((stage.clone(), seconds));
                            }
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
            held.produced_bytes = produced
                .and_then(|path| std::fs::metadata(path).ok())
                .map(|entry| entry.len());
            match product {
                Product::Scene => held.scene = Some(name.to_owned()),
                Product::Ifc => held.ifc = Some(name.to_owned()),
                Product::Json => held.json = Some(name.to_owned()),
            }
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
    use std::collections::BTreeMap;

    use super::{
        Format, IfcRequest, MAX_AUTOMATIC_IFC_BYTES, MIN_MAX_IFC_BYTES, Uploads, conversion_limit,
        max_ifc_bytes, memory_budget, room_to_convert, room_to_convert_all, sanitise, scene_name,
    };

    fn params(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    /// A set's files decide which document is which in the output, so the
    /// order has to be the same every time the same files are converted.
    #[test]
    fn a_set_is_read_back_in_a_deterministic_order_and_can_be_forgotten() {
        let directory = std::env::temp_dir().join(format!("rivet-set-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let uploads = Uploads {
            scenes: directory.clone(),
            rivet: std::path::PathBuf::from("rivet"),
            max_bytes: 1 << 20,
        };

        assert!(uploads.staged("tower").is_empty(), "nothing staged yet");
        let held = directory.join("uploads").join("sets").join("tower");
        std::fs::create_dir_all(&held).expect("a staging directory");
        // Written out of order on purpose.
        for name in ["st.ifc", "ar.ifc", "mep.ifc"] {
            std::fs::write(held.join(name), b"ISO-10303-21;").expect("a staged file");
        }
        let staged = uploads.staged("tower");
        let names: Vec<String> = staged
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["ar.ifc", "mep.ifc", "st.ifc"]);

        uploads.discard_set("tower");
        assert!(uploads.staged("tower").is_empty(), "the set was forgotten");
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// A set name reaches the filesystem, so it is sanitised like any other
    /// name a request supplies.
    #[test]
    fn a_set_name_cannot_leave_the_staging_directory() {
        assert_eq!(scene_name("../../etc/passwd"), "passwd");
        assert_eq!(scene_name("tower/ar"), "ar");
        assert_eq!(scene_name(""), "model");
    }

    /// A federation's models are held together, so what has to fit is the
    /// sum - and only the formats whose cost scales with their source count.
    #[test]
    fn a_federation_is_charged_as_the_sum_of_its_whole_file_readers() {
        let ceiling = 8 * MIN_MAX_IFC_BYTES;
        // Well inside anything, whether counted once or three times.
        let small = [(1 << 20, Format::Ifc); 3];
        assert!(room_to_convert_all(&small).is_ok());
        // An RVT is bounded a member at a time, so a set of them plans
        // nothing here however large they are.
        let huge_rvt = [(ceiling, Format::Rvt); 4];
        assert!(room_to_convert_all(&huge_rvt).is_ok());
        // And the sum is what is charged, not the largest: three sources that
        // each fit can still fail together, if the host is small enough to say
        // so. Only assert the arithmetic, since free memory is not ours.
        let summed = [(1 << 30, Format::Ifc), (1 << 30, Format::Ifc)];
        let one = [(2 << 30, Format::Ifc)];
        assert_eq!(
            room_to_convert_all(&summed).is_ok(),
            room_to_convert_all(&one).is_ok()
        );
    }

    #[test]
    fn reads_an_export_setup_out_of_the_query() {
        let request = IfcRequest::from_params(&params(&[
            ("name", "tower"),
            ("length-unit", "millimetre"),
            ("no-types", "true"),
            // A flag with no value is the flag being set, which is how a bare
            // `?no-ifc-common-property-sets` arrives.
            ("no-ifc-common-property-sets", ""),
        ]))
        .expect("a readable setup");
        assert_eq!(request.length_unit.as_deref(), Some("millimetre"));
        assert!(request.no_types);
        assert!(request.no_ifc_common_property_sets);
        assert!(!request.no_revit_property_sets);
    }

    /// A federated export names its set on every request of it. Those three
    /// are the upload's parameters rather than the exporter's, and refusing
    /// them is what stopped several files being exported as one model.
    #[test]
    fn reads_a_setup_that_also_names_a_federated_set() {
        let request = IfcRequest::from_params(&params(&[
            ("name", "tower-2.rvt"),
            ("set", "tower"),
            ("complete", "true"),
            ("length-unit", "millimetre"),
        ]))
        .expect("a readable setup");
        assert_eq!(request.length_unit.as_deref(), Some("millimetre"));
    }

    /// A setting the exporter does not have, or a value it cannot honour, is
    /// refused. The alternative is a file that is quietly not what was asked
    /// for, which nobody can tell by looking at it.
    #[test]
    fn refuses_a_setting_this_exporter_does_not_have() {
        let error = IfcRequest::from_params(&params(&[("length-unit", "cubits")]))
            .expect_err("an unknown unit");
        assert!(error.contains("cubits"), "{error}");
        let error = IfcRequest::from_params(&params(&[("tessellate", "1")]))
            .expect_err("an unknown setting");
        assert!(error.contains("tessellate"), "{error}");
        let error = IfcRequest::from_params(&params(&[("no-types", "perhaps")]))
            .expect_err("an unreadable flag");
        assert!(error.contains("perhaps"), "{error}");
    }

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
