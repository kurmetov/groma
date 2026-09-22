//! A read-only HTTP/JSON API over the models `groma export-json` produced, so
//! an agent can be given tools against them and a retrieval index can be fed
//! from them.
//!
//! It serves artefacts rather than parsing `.rvt` itself: a full decode costs
//! seconds and gigabytes, which is not a request handler's business, and
//! serving the artefact keeps every answer traceable to the export it came
//! from. Point `--data` at a directory of `<model>.jsonl` files.

/// The same allocator the converter uses. The server holds a parsed model
/// of millions of small values, and freeing one was measured at three and a
/// half seconds of a twelve-second conversion under the system allocator.
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod scenes;
mod store;
mod upload;

use std::collections::BTreeMap;
use std::error::Error;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use clap::Parser;
use scenes::{SceneError, Scenes};
use store::{Model, Query};
use tiny_http::{Header, Method, Request, Response, Server};
use upload::{DEFAULT_MAX_UPLOAD_BYTES, Job, Uploads};

/// What a request for the page is told when this server was started without
/// one. Said the same way everywhere, because the cause is a missing flag
/// rather than a missing file and the answer is the same in both places.
const NO_VIEWER: &str = "this server was started without --viewer";

/// The viewer page, when one is configured.
///
/// This server is the API first; the page that draws its scenes is a separate
/// project with its own build. So the two files are read from a directory at
/// startup rather than compiled in, and a server started without one serves
/// the JSON routes alone. Read once and held, not read per request: the page
/// is served with `no-store` so that a restart publishes a new UI, and a
/// restart is exactly when this is re-read.
struct Viewer {
    html: Vec<u8>,
    bundle: Vec<u8>,
}

impl Viewer {
    /// Both files or neither. A page served without its bundle renders an
    /// empty frame with nothing in the console to say why, which is a worse
    /// failure than refusing to start.
    fn load(directory: &Path) -> Result<Self, String> {
        Ok(Self {
            html: read_viewer_file(&directory.join("viewer.html"))?,
            bundle: read_viewer_file(&directory.join("viewer-ui.js"))?,
        })
    }
}

fn read_viewer_file(path: &Path) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))
}

/// Page size a request gets when it asks for none.
const DEFAULT_LIMIT: usize = 50;
/// Page size no request may exceed, so one call cannot pull a whole model into
/// an agent's context by accident. `/documents` is the deliberate bulk route.
const MAX_LIMIT: usize = 1000;

#[derive(Parser, Debug)]
#[command(
    name = "groma-api",
    about = "Read-only HTTP/JSON API over exported RVT models"
)]
struct Cli {
    /// Directory of `<model>.jsonl` files written by `groma export-json`.
    #[arg(long)]
    data: PathBuf,
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:8787")]
    addr: String,
    /// Worker threads. Each serves one request at a time.
    #[arg(long, default_value_t = 4)]
    threads: usize,
    /// Load every model at startup instead of on first use.
    #[arg(long)]
    preload: bool,
    /// Directory of `<scene>.rvs` files written by `groma export-scene`. With
    /// one, `/viewer` draws them; without one, the scene routes are absent.
    #[arg(long)]
    scenes: Option<PathBuf>,
    /// Directory holding the viewer's `viewer.html` and `viewer-ui.js`. With
    /// one, `/` and `/viewer` serve the page; without one, this is the JSON
    /// API alone and those routes are absent.
    #[arg(long)]
    viewer: Option<PathBuf>,
    /// The `groma` binary that converts an uploaded model. Defaults to the one
    /// beside this executable; without either, uploading is refused and the
    /// scenes already on disk are still served.
    #[arg(long)]
    groma: Option<PathBuf>,
    /// Largest upload accepted, in bytes.
    #[arg(long, default_value_t = DEFAULT_MAX_UPLOAD_BYTES)]
    max_upload: u64,
}

/// Models available on disk, loaded on first use and then kept.
struct Store {
    directory: PathBuf,
    loaded: RwLock<BTreeMap<String, Arc<Model>>>,
    loading: Mutex<()>,
}

impl Store {
    fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            loaded: RwLock::new(BTreeMap::new()),
            loading: Mutex::new(()),
        }
    }

    /// Model names on disk, whether or not they are loaded yet.
    fn available(&self) -> Vec<String> {
        let mut names = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.directory) else {
            return names;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "jsonl") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    names.push(stem.to_owned());
                }
            }
        }
        names.sort();
        names
    }

    fn get(&self, name: &str) -> Option<Arc<Model>> {
        if let Some(model) = self.loaded.read().ok()?.get(name) {
            return Some(Arc::clone(model));
        }
        // A model name must resolve to a file directly inside the data
        // directory. Anything with a separator or a parent segment in it is
        // refused rather than sanitised, so a request cannot reach outside.
        if name.is_empty()
            || name.contains(['/', '\\'])
            || name.contains("..")
            || name.starts_with('.')
        {
            return None;
        }
        let path = self.directory.join(format!("{name}.jsonl"));
        if !path.is_file() {
            return None;
        }
        // One loader at a time: two requests for a cold 800 000-element model
        // would otherwise both pay for it.
        let _guard = self.loading.lock().ok()?;
        if let Some(model) = self.loaded.read().ok()?.get(name) {
            return Some(Arc::clone(model));
        }
        let started = std::time::Instant::now();
        let (model, skipped) = Model::load(name, &path).ok()?;
        eprintln!(
            "loaded {name}: {} elements in {:.1}s{}",
            model.len(),
            started.elapsed().as_secs_f64(),
            if skipped > 0 {
                format!(", {skipped} unparsable lines skipped")
            } else {
                String::new()
            }
        );
        let model = Arc::new(model);
        self.loaded
            .write()
            .ok()?
            .insert(name.to_owned(), Arc::clone(&model));
        Some(model)
    }
}

fn json_response(status: u16, body: &serde_json::Value) -> Response<Cursor<Vec<u8>>> {
    let bytes = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    let header = Header::from_bytes(
        &b"Content-Type"[..],
        &b"application/json; charset=utf-8"[..],
    )
    .expect("static header");
    Response::from_data(bytes)
        .with_status_code(status)
        .with_header(header)
}

fn error(status: u16, message: &str) -> Response<Cursor<Vec<u8>>> {
    json_response(status, &serde_json::json!({ "error": message }))
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).unwrap_or_else(|()| {
        Header::from_bytes(&b"X-groma"[..], &b"header"[..]).expect("static header")
    })
}

fn content_type(value: &str) -> Header {
    header("Content-Type", value)
}

/// Split a target into its path segments and its decoded query pairs.
fn parse_target(target: &str) -> (Vec<String>, BTreeMap<String, String>) {
    let (path, raw_query) = target.split_once('?').unwrap_or((target, ""));
    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(percent_decode)
        .collect();
    let mut query = BTreeMap::new();
    for pair in raw_query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        query.insert(percent_decode(key), percent_decode(value));
    }
    (segments, query)
}

/// Decode `%XX` escapes and `+`. A malformed escape is left as written rather
/// than dropped, so a name containing a bare `%` still round-trips.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let decoded = std::str::from_utf8(&bytes[index + 1..index + 3])
                    .ok()
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok());
                if let Some(byte) = decoded {
                    out.push(byte);
                    index += 3;
                } else {
                    out.push(bytes[index]);
                    index += 1;
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_from(params: &BTreeMap<String, String>) -> Query {
    let text = |key: &str| {
        params
            .get(key)
            .map(String::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let flag = |key: &str| {
        params
            .get(key)
            .is_some_and(|value| matches!(value.as_str(), "" | "1" | "true" | "yes"))
    };
    let number = |key: &str, fallback: usize| {
        params
            .get(key)
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(fallback)
    };
    Query {
        class: text("class"),
        category: text("category"),
        level: text("level"),
        // Resolved by the route, which has the model to fold the storey with.
        level_ids: None,
        name: text("name"),
        text: text("q"),
        model_elements_only: flag("model_elements"),
        with_geometry: flag("with_geometry"),
        offset: number("offset", 0),
        limit: number("limit", DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
    }
}

/// The `/elements` answer: the filtered page, and what the filters removed.
fn elements_response(
    name: &str,
    model: &Model,
    params: &BTreeMap<String, String>,
) -> Response<Cursor<Vec<u8>>> {
    let mut query = query_from(params);
    // A storey is many `Level` records, so the filter is the whole set. An id
    // that names no storey is refused: answering it with an empty page would
    // read as an empty storey.
    if let Some(storey) = params.get("storey").filter(|value| !value.is_empty()) {
        match storey
            .parse::<u32>()
            .ok()
            .and_then(|id| model.storey_level_ids(id))
        {
            Some(level_ids) => query.level_ids = Some(level_ids),
            None => return error(400, "storey must be an `id` from /levels"),
        }
    }
    let page = model.query(&query);
    let mut body = serde_json::json!({
        "model": name,
        "total": page.total,
        "offset": query.offset,
        "limit": query.limit,
        "elements": page.elements,
    });
    if page.excluded_as_not_model_elements > 0 {
        body["excluded_as_not_model_elements"] =
            serde_json::json!(page.excluded_as_not_model_elements);
        body["note"] = serde_json::json!(
            "model_elements dropped that many matches: rooms, levels, type \
             definitions and annotation are not model elements. Drop the flag \
             to count them."
        );
    }
    json_response(200, &body)
}

/// The conversions this server has been asked for, and the one slot they run
/// in.
///
/// A decode holds gigabytes - 3.4 GB on a 230 MB architectural model - so two
/// at once is how a machine with this open in two tabs runs out of memory. The
/// second upload waits rather than competing.
struct ConversionSlot {
    uploads: Uploads,
    running: Arc<Mutex<()>>,
    jobs: Mutex<BTreeMap<String, Arc<Mutex<Job>>>>,
}

impl ConversionSlot {
    fn job(&self, id: &str) -> Option<Arc<Mutex<Job>>> {
        self.jobs.lock().ok()?.get(id).cloned()
    }

    /// Every conversion still remembered, newest first. Job identifiers lead
    /// with the clock, so the map's own order is chronological.
    fn listing(&self) -> Vec<serde_json::Value> {
        let Ok(jobs) = self.jobs.lock() else {
            return Vec::new();
        };
        jobs.iter()
            .rev()
            .map(|(id, job)| {
                let mut entry = job
                    .lock()
                    .map_or_else(|held| held.into_inner().to_json(), |held| held.to_json());
                if let Some(object) = entry.as_object_mut() {
                    object.insert("job".to_owned(), serde_json::Value::String(id.clone()));
                }
                entry
            })
            .collect()
    }

    /// Remember a job, and forget the oldest once there are many: this is a
    /// viewer, not a queue, and a session's worth is all anyone asks about.
    fn remember(&self, id: String, job: &Arc<Mutex<Job>>) {
        if let Ok(mut jobs) = self.jobs.lock() {
            while jobs.len() >= 32 {
                let Some(oldest) = jobs.keys().next().cloned() else {
                    break;
                };
                jobs.remove(&oldest);
            }
            jobs.insert(id, Arc::clone(job));
        }
    }
}

/// Receive a model and start converting it. The reply names a job the page
/// then asks about, rather than holding the connection open for the minute a
/// large model takes.
/// What one upload request produced.
enum Upload {
    /// Held with the others of a federation that is still being uploaded.
    /// Nothing is converted until a request says the set is complete.
    Staged { set: String, files: usize },
    /// Everything a conversion needs, whether it came from one file or a set.
    Ready(Box<Received>),
}

/// The sources of one conversion, received and checked.
struct Received {
    /// Every source to read, in a deterministic order.
    sources: Vec<PathBuf>,
    formats: Vec<upload::Format>,
    /// What the output is named.
    stem: String,
    /// The set to forget once the conversion has finished, if any.
    set: Option<String>,
    /// The sources' total size, for the job listing.
    bytes: u64,
}

/// Receive one upload, staging it with a federation's other files or handing
/// back everything a conversion needs.
///
/// `set` names a federation: the file is held rather than converted, and the
/// scene takes the set's name rather than the file's. `complete` says this is
/// the last file, at which point every file held for the set is converted
/// together. Without `set` this is a single-file conversion, exactly as
/// before.
fn receive_upload(
    uploads: &upload::Uploads,
    params: &BTreeMap<String, String>,
    name: &str,
    body: &mut dyn std::io::Read,
) -> Result<Upload, String> {
    let set = params.get("set").filter(|set| !set.is_empty());
    let complete = params.contains_key("complete");
    let (source, format, stem) = uploads.receive(set.map(String::as_str), name, body)?;

    let Some(set) = set else {
        let bytes = std::fs::metadata(&source).map_or(0, |file| file.len());
        return Ok(Upload::Ready(Box::new(Received {
            sources: vec![source],
            formats: vec![format],
            stem,
            set: None,
            bytes,
        })));
    };
    let staged = uploads.staged(set);
    if !complete {
        return Ok(Upload::Staged {
            set: set.clone(),
            files: staged.len(),
        });
    }
    // The formats are read again from the staged files rather than remembered
    // across requests: the set is on disk, and a server that trusted a
    // client's earlier claim would be trusting the claim.
    let mut formats = Vec::with_capacity(staged.len());
    let mut bytes = 0_u64;
    for path in &staged {
        let mut head = [0_u8; bim_convert::SNIFF_BYTES];
        let read = std::fs::File::open(path)
            .and_then(|mut file| std::io::Read::read(&mut file, &mut head))
            .map_err(|error| error.to_string())?;
        let Some(format) = upload::Format::sniff(&head[..read]) else {
            return Err(format!(
                "{} is no longer a file this server reads",
                path.display()
            ));
        };
        formats.push(format);
        bytes = bytes.saturating_add(std::fs::metadata(path).map_or(0, |file| file.len()));
    }
    Ok(Upload::Ready(Box::new(Received {
        sources: staged,
        formats,
        stem: upload::scene_name(set),
        set: Some(set.clone()),
        bytes,
    })))
}

/// The format to show in the job listing: the one every source shares, or
/// that they do not share one.
fn format_label(formats: &[upload::Format]) -> String {
    let mut kinds: Vec<&str> = formats.iter().map(|format| format.extension()).collect();
    kinds.sort_unstable();
    kinds.dedup();
    match kinds.as_slice() {
        [only] => (*only).to_owned(),
        _ => "mixed".to_owned(),
    }
}

/// Attach the user's single confirmation to every process it starts. The
/// values are presentation metadata only; paths and process arguments never
/// use them.
fn describe_batch(job: &mut Job, id: &str, params: &BTreeMap<String, String>, target: &str) {
    let clean = |key: &str, fallback: &str| {
        params
            .get(key)
            .map(|value| value.chars().take(160).collect::<String>())
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| fallback.to_owned())
    };
    job.batch = Some(clean("batch", id));
    job.batch_label = Some(clean("batch-label", "Conversion"));
    job.batch_index = params
        .get("batch-index")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1)
        .max(1);
    job.batch_total = params
        .get("batch-total")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1)
        .clamp(1, 1_000);
    job.target = Some(target.to_owned());
}

/// Receive a model - or a federation of them - and start converting it to a
/// scene.
#[allow(clippy::too_many_lines)] // One request read through to the job it starts.
fn serve_upload(
    slot: Option<&ConversionSlot>,
    params: &BTreeMap<String, String>,
    mut request: Request,
) -> std::io::Result<()> {
    let Some(slot) = slot else {
        return request.respond(error(
            503,
            "this server cannot convert uploads: no groma binary was found beside it, \
             and none was given with --groma",
        ));
    };
    if request.method() != &tiny_http::Method::Post {
        return request.respond(error(405, "upload is a POST"));
    }
    let name = params.get("name").map_or("model", String::as_str);

    let received = match receive_upload(&slot.uploads, params, name, request.as_reader()) {
        Ok(Upload::Staged { set, files }) => {
            // Held, not converted. The client sends the last file with
            // `complete` and the whole set is read as one model then.
            return request.respond(json_response(
                200,
                &serde_json::json!({ "set": set, "files": files }),
            ));
        }
        Ok(Upload::Ready(received)) => *received,
        Err(message) => return request.respond(error(400, &message)),
    };

    // The size ceiling is a plan drawn from this machine's capacity; this is
    // the check against the moment. A conversion that cannot fit in the memory
    // free right now is refused with a reason, because the alternative is the
    // host swapping until someone reboots it. A federation is charged as the
    // sum, because its models are held together.
    let sized: Vec<(u64, upload::Format)> = received
        .sources
        .iter()
        .zip(&received.formats)
        .map(|(path, format)| {
            (
                std::fs::metadata(path).map_or(0, |file| file.len()),
                *format,
            )
        })
        .collect();
    if let Err(message) = upload::room_to_convert_all(&sized) {
        for source in &received.sources {
            let _ = std::fs::remove_file(source);
        }
        if let Some(set) = &received.set {
            slot.uploads.discard_set(set);
        }
        return request.respond(error(507, &message));
    }
    let id = job_id();
    let job = Arc::new(Mutex::new(Job::new()));
    let Received {
        sources,
        formats,
        stem,
        set,
        bytes,
    } = received;
    let label = format_label(&formats);
    if let Ok(mut held) = job.lock() {
        describe_batch(&mut held, &id, params, "scene");
        held.source = Some(set.clone().unwrap_or_else(|| name.to_owned()));
        held.format = Some(label.clone());
        held.bytes = bytes;
        held.scene = Some(stem.clone());
    }
    slot.remember(id.clone(), &job);
    let response = json_response(
        202,
        &serde_json::json!({
            "job": id,
            "scene": stem,
            "format": label,
            "documents": sources.len(),
        }),
    );

    let uploads = slot.uploads.clone();
    let running = Arc::clone(&slot.running);
    std::thread::spawn(move || {
        // One at a time, so two tabs cannot decode two models at once.
        let _guard = running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if job_cancelled(&job) {
            if let Some(set) = &set {
                uploads.discard_set(set);
            }
            return;
        }
        match uploads.convert(&sources, &stem, &formats) {
            Ok(child) => upload::follow(
                child,
                &stem,
                upload::Product::Scene,
                Some(&uploads.scenes.join(format!("{stem}.rvs"))),
                &job,
            ),
            Err(failure) => {
                if let Ok(mut held) = job.lock() {
                    if !held.is_cancelled() {
                        held.state = upload::JobState::Failed;
                        held.error = Some(format!("the converter could not be started: {failure}"));
                    }
                }
            }
        }
        if let Some(set) = set {
            // The staged files have been read; the scene is what is kept.
            uploads.discard_set(&set);
        }
    });
    request.respond(response)
}

/// Receive a Revit model and start exporting it as IFC, with the settings the
/// query asks for. The reply names a job, as an upload's does; the file is at
/// `/exports/{name}.ifc` once the job is done.
fn serve_export_ifc(
    slot: Option<&ConversionSlot>,
    params: &BTreeMap<String, String>,
    mut request: Request,
) -> std::io::Result<()> {
    let Some(slot) = slot else {
        return request.respond(error(
            503,
            "this server cannot export: no groma binary was found beside it, \
             and none was given with --groma",
        ));
    };
    if request.method() != &tiny_http::Method::Post {
        return request.respond(error(405, "an export is a POST"));
    }
    let settings = match upload::IfcRequest::from_params(params) {
        Ok(settings) => settings,
        Err(message) => return request.respond(error(400, &message)),
    };
    let name = params.get("name").map_or("model", String::as_str);

    // An IFC source is no longer refused here: `export-ifc` reads a model and
    // no longer cares which format stated it, and reading a federation of IFCs
    // out as one file is the whole point of accepting several.
    let received = match receive_upload(&slot.uploads, params, name, request.as_reader()) {
        Ok(Upload::Staged { set, files }) => {
            return request.respond(json_response(
                200,
                &serde_json::json!({ "set": set, "files": files }),
            ));
        }
        Ok(Upload::Ready(received)) => *received,
        Err(message) => return request.respond(error(400, &message)),
    };

    let id = job_id();
    let job = Arc::new(Mutex::new(Job::new()));
    let Received {
        sources,
        formats,
        stem,
        set,
        bytes,
    } = received;
    if let Ok(mut held) = job.lock() {
        describe_batch(&mut held, &id, params, "ifc");
        held.source = Some(set.clone().unwrap_or_else(|| name.to_owned()));
        held.format = Some(format_label(&formats));
        held.bytes = bytes;
    }
    slot.remember(id.clone(), &job);
    let response = json_response(
        202,
        &serde_json::json!({ "job": id, "ifc": format!("{stem}.ifc") }),
    );

    let uploads = slot.uploads.clone();
    let running = Arc::clone(&slot.running);
    std::thread::spawn(move || {
        // One at a time, for the same reason an upload is: a decode holds
        // gigabytes and two at once is what takes a machine down.
        let _guard = running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if job_cancelled(&job) {
            if let Some(set) = &set {
                uploads.discard_set(set);
            }
            return;
        }
        match uploads.export_ifc(&sources, &stem, &settings) {
            Ok(child) => upload::follow(
                child,
                &stem,
                upload::Product::Ifc,
                Some(&uploads.exports().join(format!("{stem}.ifc"))),
                &job,
            ),
            Err(failure) => {
                if let Ok(mut held) = job.lock() {
                    if !held.is_cancelled() {
                        held.state = upload::JobState::Failed;
                        held.error = Some(format!("the exporter could not be started: {failure}"));
                    }
                }
            }
        }
        if let Some(set) = set {
            uploads.discard_set(&set);
        }
    });
    request.respond(response)
}

/// Serve an exported IFC. It is a file to save rather than a page to read, so
/// it is offered as a download under the name it was exported as.
fn serve_export(
    slot: Option<&ConversionSlot>,
    name: &str,
    request: Request,
) -> std::io::Result<()> {
    // The extension in the request picks which export is meant, and only the
    // two this server writes are answered.
    let (stem, extension, mime) = match () {
        () if name.to_ascii_lowercase().ends_with(".jsonl") => (
            name.trim_end_matches(".jsonl"),
            "jsonl",
            "application/x-ndjson",
        ),
        () => (name.trim_end_matches(".ifc"), "ifc", "application/x-step"),
    };
    let Some(path) = slot.and_then(|slot| slot.uploads.exported_as(stem, extension)) else {
        return request.respond(error(404, "no export by that name"));
    };
    match std::fs::File::open(&path) {
        Ok(file) => request.respond(
            Response::from_file(file)
                .with_header(content_type(mime))
                .with_header(header(
                    "Content-Disposition",
                    &format!("attachment; filename=\"{stem}.{extension}\""),
                )),
        ),
        Err(_) => request.respond(error(500, "the export could not be read")),
    }
}

/// Convert an uploaded Revit model into JSON lines.
///
/// It mirrors the IFC export: the model arrives as the request body, the
/// answer names a job, and the file is fetched from `/exports` when that job
/// says it is done.
fn serve_export_json(
    slot: Option<&ConversionSlot>,
    params: &BTreeMap<String, String>,
    mut request: Request,
) -> std::io::Result<()> {
    let Some(slot) = slot else {
        return request.respond(error(
            503,
            "this server cannot export: no groma binary was found beside it, \
             and none was given with --groma",
        ));
    };
    if request.method() != &Method::Post {
        return request.respond(error(405, "an export is a POST"));
    }
    let full = params.get("full").is_some_and(|value| value != "false");
    let name = params.get("name").map_or("model", String::as_str);

    let received = slot.uploads.receive(None, name, request.as_reader());
    let (source, format, stem) = match received {
        Ok(received) => received,
        Err(message) => return request.respond(error(400, &message)),
    };
    if format != upload::Format::Rvt {
        let _ = std::fs::remove_file(&source);
        return request.respond(error(400, "a JSON export is made from a Revit model"));
    }

    let id = job_id();
    let job = Arc::new(Mutex::new(Job::new()));
    let source_bytes = std::fs::metadata(&source).map_or(0, |file| file.len());
    if let Ok(mut held) = job.lock() {
        describe_batch(&mut held, &id, params, "json");
        held.source = Some(name.to_owned());
        held.format = Some(format.extension().to_owned());
        held.bytes = source_bytes;
    }
    slot.remember(id.clone(), &job);
    let response = json_response(
        202,
        &serde_json::json!({ "job": id, "json": format!("{stem}.jsonl") }),
    );

    let uploads = slot.uploads.clone();
    let running = Arc::clone(&slot.running);
    std::thread::spawn(move || {
        let _guard = running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if job_cancelled(&job) {
            return;
        }
        match uploads.export_json(&source, &stem, full) {
            Ok(child) => upload::follow(
                child,
                &stem,
                upload::Product::Json,
                Some(&uploads.exports().join(format!("{stem}.jsonl"))),
                &job,
            ),
            Err(failure) => {
                if let Ok(mut held) = job.lock() {
                    if !held.is_cancelled() {
                        held.state = upload::JobState::Failed;
                        held.error = Some(format!("the exporter could not be started: {failure}"));
                    }
                }
            }
        }
    });
    request.respond(response)
}

/// An identifier for one conversion. It only has to be unlike the others this
/// process hands out, which a counter and the clock together are.
fn job_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let count = NEXT.fetch_add(1, Ordering::Relaxed);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis());
    format!("{now:x}-{count}")
}

fn job_cancelled(job: &Arc<Mutex<Job>>) -> bool {
    job.lock().map_or_else(
        |held| held.into_inner().is_cancelled(),
        |held| held.is_cancelled(),
    )
}

fn serve_cancel_job(
    slot: Option<&ConversionSlot>,
    id: &str,
    request: Request,
) -> std::io::Result<()> {
    let Some(job) = slot.and_then(|slot| slot.job(id)) else {
        return request.respond(error(404, "unknown job"));
    };
    let (accepted, process) = job
        .lock()
        .map_or_else(|mut held| held.get_mut().cancel(), |mut held| held.cancel());
    if !accepted {
        return request.respond(error(409, "this job is no longer running"));
    }
    if let Some(process) = process {
        let _ = process
            .lock()
            .map_or_else(|mut held| held.get_mut().kill(), |mut held| held.kill());
    }
    request.respond(json_response(
        202,
        &serde_json::json!({ "job": id, "state": "cancelled" }),
    ))
}

/// Answer one scene request, by range where one was asked for.
///
/// A viewer reads this format by range - 24 bytes of trailer, then the
/// manifest, then the chunks it means to draw - so the range path is the
/// normal one here, not an optimisation.
fn serve_scene(scenes: Option<&Scenes>, name: &str, request: Request) -> std::io::Result<()> {
    let Some(scenes) = scenes else {
        return request.respond(error(404, "this server was started without --scenes"));
    };
    let wanted = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Range"))
        .and_then(|header| scenes::parse_range(header.value.as_str()));
    match scenes.read(name, wanted) {
        Ok((bytes, range, total)) => {
            let mut response = Response::from_data(bytes)
                .with_header(content_type("application/octet-stream"))
                .with_header(header("Accept-Ranges", "bytes"));
            if wanted.is_some() {
                response = response.with_status_code(206).with_header(header(
                    "Content-Range",
                    &format!("bytes {}-{}/{total}", range.start, range.end),
                ));
            }
            request.respond(response)
        }
        Err(SceneError::NotFound) => request.respond(error(404, "unknown scene")),
        // A 416 must state the length, so a reader that got it wrong can
        // correct itself rather than only learning that it failed.
        Err(SceneError::NotSatisfiable(total)) => request.respond(
            error(416, "range not satisfiable")
                .with_header(header("Content-Range", &format!("bytes */{total}"))),
        ),
        Err(SceneError::Io(failure)) => {
            eprintln!("scene {name}: {failure}");
            request.respond(error(500, "the scene could not be read"))
        }
    }
}

/// Delete one scene, and the file it was converted from where the caller asks
/// for it with `?source=rvt` or `?source=ifc`.
///
/// The parameter names a format, never a path, so this cannot be pointed at a
/// file outside the uploads directory. Which of the two a scene actually came
/// from is the reader's to say: both can exist under one name, and only the
/// manifest records which one was converted.
fn serve_delete(
    scenes: Option<&Scenes>,
    name: &str,
    params: &BTreeMap<String, String>,
    request: Request,
) -> std::io::Result<()> {
    let Some(scenes) = scenes else {
        return request.respond(error(404, "this server was started without --scenes"));
    };
    let source = params.get("source").map(String::as_str);
    match scenes.remove(name, source) {
        Ok(removed) => request.respond(json_response(
            200,
            &serde_json::json!({
                "deleted": name,
                "scene": removed.scene,
                "preview": removed.preview,
                "source": removed.source,
                "bytes": removed.bytes,
            }),
        )),
        Err(SceneError::NotFound) => request.respond(error(404, "unknown scene")),
        Err(failure) => {
            eprintln!("delete {name}: {failure:?}");
            request.respond(error(500, "the scene could not be deleted"))
        }
    }
}

/// The largest preview a viewer may store: these are small JPEGs of a framed
/// model, and a cap keeps the route from becoming a way to fill the disk.
const MAX_PREVIEW_BYTES: usize = 2 * 1024 * 1024;

/// A scene's cached preview image: `GET` to read one, `PUT` to store the one a
/// viewer just rendered.
fn serve_preview(scenes: Option<&Scenes>, name: &str, mut request: Request) -> std::io::Result<()> {
    let Some(scenes) = scenes else {
        return request.respond(error(404, "this server was started without --scenes"));
    };
    if request.method() == &Method::Put {
        let mut image = Vec::new();
        // Read one byte past the cap, so a body that is exactly at it is still
        // told apart from one that runs over. The reader borrows the request,
        // so it is scoped to end before the request is answered.
        let read = {
            let reader: &mut dyn Read = request.as_reader();
            Read::take(reader, MAX_PREVIEW_BYTES as u64 + 1).read_to_end(&mut image)
        };
        if read.is_err() {
            return request.respond(error(400, "the preview could not be read"));
        }
        if image.len() > MAX_PREVIEW_BYTES {
            return request.respond(error(413, "preview too large"));
        }
        return match scenes.store_preview(name, &image) {
            Ok(()) => request.respond(json_response(200, &serde_json::json!({ "stored": name }))),
            Err(SceneError::NotFound) => request.respond(error(404, "unknown scene")),
            Err(failure) => {
                eprintln!("preview {name}: {failure:?}");
                request.respond(error(500, "the preview could not be stored"))
            }
        };
    }
    match scenes.preview(name) {
        Some(image) => request.respond(
            Response::from_data(image)
                .with_header(content_type("image/png"))
                // A preview changes only when a viewer redraws it, and the
                // page asks for it by name; revalidating keeps a replaced one
                // from sticking.
                .with_header(header("Cache-Control", "no-cache")),
        ),
        None => request.respond(error(404, "no preview yet")),
    }
}

#[allow(clippy::too_many_lines)] // One match over the routes this server serves.
fn handle(
    store: &Store,
    scenes: Option<&Scenes>,
    uploads: Option<&ConversionSlot>,
    viewer: Option<&Viewer>,
    request: Request,
) -> std::io::Result<()> {
    let (segments, params) = parse_target(request.url());
    let route = segments.iter().map(String::as_str).collect::<Vec<_>>();

    // The viewer and its scenes are answered before the JSON routes: a scene
    // is bytes served by range, not a document, and the page is HTML.
    match route.as_slice() {
        [] | ["files" | "convert" | "viewer"] => {
            if let Some(viewer) = viewer {
                let content_type =
                    Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..])
                        .expect("static header");
                return request.respond(
                    Response::from_data(viewer.html.clone())
                        .with_header(content_type)
                        // A local server restart must publish its new UI
                        // immediately rather than leave a browser running a
                        // cached copy of the old one.
                        .with_header(header("Cache-Control", "no-store, max-age=0")),
                );
            }
            // A named workspace route with no page behind it is a plain
            // absence. The bare root is not: it falls through to the API's own
            // index below, so a server answering its address says what it is
            // rather than 404-ing at whoever just opened it in a browser.
            if !route.is_empty() {
                return request.respond(error(404, NO_VIEWER));
            }
        }
        ["viewer-ui.js"] => {
            let Some(viewer) = viewer else {
                return request.respond(error(404, NO_VIEWER));
            };
            let content_type =
                Header::from_bytes(&b"Content-Type"[..], &b"text/javascript; charset=utf-8"[..])
                    .expect("static header");
            return request.respond(
                Response::from_data(viewer.bundle.clone())
                    .with_header(content_type)
                    .with_header(header("Cache-Control", "no-store, max-age=0")),
            );
        }
        ["scenes"] => {
            let listed = scenes.map(Scenes::listing).unwrap_or_default();
            return request.respond(json_response(
                200,
                &serde_json::json!({
                    // `scenes` stays a plain list of names, which is what a
                    // caller wanting only the names already reads; `entries`
                    // carries what a library page shows beside each one.
                    "scenes": listed.iter().map(|entry| entry.name.clone()).collect::<Vec<_>>(),
                    "entries": listed.iter().map(|entry| serde_json::json!({
                        "name": entry.name,
                        "bytes": entry.bytes,
                        "modified": entry.modified,
                        "hasPreview": entry.has_preview,
                        // What deleting this model could also remove.
                        "sources": entry.sources.iter().map(|(extension, bytes)| {
                            serde_json::json!({ "extension": extension, "bytes": bytes })
                        }).collect::<Vec<_>>(),
                    })).collect::<Vec<_>>(),
                }),
            ));
        }
        ["scenes", name] if request.method() == &Method::Delete => {
            return serve_delete(scenes, name, &params, request);
        }
        ["scenes", name] => return serve_scene(scenes, name, request),
        ["previews", name] => return serve_preview(scenes, name, request),
        ["upload"] => return serve_upload(uploads, &params, request),
        ["export-ifc"] => return serve_export_ifc(uploads, &params, request),
        ["export-json"] => return serve_export_json(uploads, &params, request),
        ["exports", name] => return serve_export(uploads, name, request),
        ["jobs"] => {
            let listed = uploads.map(ConversionSlot::listing).unwrap_or_default();
            return request.respond(json_response(200, &serde_json::json!({ "jobs": listed })));
        }
        ["jobs", id] if request.method() == &Method::Delete => {
            return serve_cancel_job(uploads, id, request);
        }
        ["jobs", id] => {
            let answer = uploads.and_then(|slot| slot.job(id)).map(|job| {
                job.lock()
                    .map_or_else(|held| held.into_inner().to_json(), |held| held.to_json())
            });
            return request.respond(match answer {
                Some(body) => json_response(200, &body),
                None => error(404, "unknown job"),
            });
        }
        _ => {}
    }

    // `/documents` streams NDJSON and is the one route with no page cap, so it
    // is answered before the JSON routes.
    if let ["models", name, "documents"] = route.as_slice() {
        let Some(model) = store.get(name) else {
            return request.respond(error(404, "unknown model"));
        };
        let mut body = Vec::new();
        for document in model.documents() {
            if let Ok(mut line) = serde_json::to_vec(&document) {
                line.push(b'\n');
                body.extend_from_slice(&line);
            }
        }
        let header = Header::from_bytes(
            &b"Content-Type"[..],
            &b"application/x-ndjson; charset=utf-8"[..],
        )
        .expect("static header");
        return request.respond(Response::from_data(body).with_header(header));
    }

    let response = match route.as_slice() {
        [] | ["api" | "health"] => json_response(
            200,
            &serde_json::json!({
                "status": "ok",
                "service": "groma-api",
                "models": store.available(),
                "routes": [
                    "/models",
                    "/models/{model}/summary",
                    "/models/{model}/elements?class=&category=&storey=&level=&name=&q=&model_elements=&with_geometry=&offset=&limit=",
                    "/models/{model}/elements/{id}",
                    "/models/{model}/rooms",
                    "/models/{model}/levels",
                    "/models/{model}/documents",
                    "/ (the workspace: /files, /convert and /viewer - needs --viewer)",
                    "/api (this list)",
                    "/scenes",
                    "/scenes/{scene}",
                    "/previews/{scene}",
                    "DELETE /scenes/{scene}?source=rvt|ifc",
                    "POST /upload?name= (one file), or ?name=&set=&complete= to \
                     federate several: repeat with the same set, mark the last one \
                     complete, and every file held is read as one model",
                    "POST /export-ifc?name=&set=&complete=&length-unit=&no-types=\
                     &no-openings=&no-revit-property-sets=&no-revit-type-property-sets=\
                     &no-ifc-common-property-sets=&base-quantities=&class-mapping=",
                    "POST /export-json?name=&full=",
                    "/exports/{name}.ifc|.jsonl",
                    "/jobs",
                    "/jobs/{job}",
                ],
                "scenes": scenes.map(Scenes::available).unwrap_or_default(),
            }),
        ),
        ["models"] => json_response(200, &serde_json::json!({ "models": store.available() })),
        ["models", name] | ["models", name, "summary"] => match store.get(name) {
            Some(model) => json_response(200, &model.summary()),
            None => error(404, "unknown model"),
        },
        ["models", name, "elements"] => match store.get(name) {
            Some(model) => elements_response(name, &model, &params),
            None => error(404, "unknown model"),
        },
        ["models", name, "elements", id] => match (store.get(name), id.parse::<u32>()) {
            (Some(model), Ok(id)) => match model.get(id) {
                Some(element) => json_response(200, &serde_json::json!(element)),
                None => error(404, "unknown element"),
            },
            (Some(_), Err(_)) => error(400, "element id must be a number"),
            (None, _) => error(404, "unknown model"),
        },
        ["models", name, "rooms"] => match store.get(name) {
            Some(model) => {
                let rooms = model.rooms();
                json_response(
                    200,
                    &serde_json::json!({ "model": name, "total": rooms.len(), "rooms": rooms }),
                )
            }
            None => error(404, "unknown model"),
        },
        ["models", name, "levels"] => match store.get(name) {
            Some(model) => {
                let levels = model.storeys();
                json_response(
                    200,
                    &serde_json::json!({ "model": name, "total": levels.len(), "levels": levels }),
                )
            }
            None => error(404, "unknown model"),
        },
        _ => error(404, "no such route"),
    };
    request.respond(response)
}

fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    if !cli.data.is_dir() {
        return Err(format!("--data is not a directory: {}", cli.data.display()).into());
    }
    let scenes = match cli.scenes.clone() {
        Some(directory) if !directory.is_dir() => {
            return Err(format!("--scenes is not a directory: {}", directory.display()).into());
        }
        Some(directory) => Some(Arc::new(Scenes::new(directory))),
        None => None,
    };
    let uploads = scenes.as_ref().and_then(|_| {
        let groma = cli
            .groma
            .clone()
            .or_else(upload::groma_beside_this_executable)?;
        Some(Arc::new(ConversionSlot {
            uploads: Uploads {
                scenes: cli.scenes.clone()?,
                groma,
                max_bytes: cli.max_upload,
            },
            running: Arc::new(Mutex::new(())),
            jobs: Mutex::new(BTreeMap::new()),
        }))
    });
    let viewer = match cli.viewer.as_ref() {
        // Refused here rather than at the first request: a mistyped path is a
        // start-up error, and finding it out from a browser is too late.
        Some(directory) => Some(Arc::new(
            Viewer::load(directory).map_err(|error| format!("--viewer {error}"))?,
        )),
        None => None,
    };
    let store = Arc::new(Store::new(cli.data.clone()));
    let available = store.available();
    if cli.preload {
        for name in &available {
            let _ = store.get(name);
        }
    }

    let server = Server::http(&cli.addr).map_err(|error| format!("listen failed: {error}"))?;
    announce(
        &cli,
        scenes.as_deref(),
        uploads.as_deref(),
        viewer.as_deref(),
        available.len(),
    );
    let server = Arc::new(server);
    let mut workers = Vec::new();
    for _ in 0..cli.threads.max(1) {
        let server = Arc::clone(&server);
        let store = Arc::clone(&store);
        let scenes = scenes.clone();
        let uploads = uploads.clone();
        let viewer = viewer.clone();
        workers.push(std::thread::spawn(move || {
            for request in server.incoming_requests() {
                if let Err(error) = handle(
                    &store,
                    scenes.as_deref(),
                    uploads.as_deref(),
                    viewer.as_deref(),
                    request,
                ) {
                    eprintln!("request failed: {error}");
                }
            }
        }));
    }
    for worker in workers {
        let _ = worker.join();
    }
    Ok(())
}

/// Everything this server says about itself before it starts answering.
///
/// Lifted out of `main` because the banner grows with every surface the server
/// gains - scenes, uploads, now a viewer - while `main`'s own job is the
/// wiring. What is said here is read by someone deciding whether the thing
/// came up the way they meant it to, so each line names a surface and where it
/// is, or says plainly that it is absent.
fn announce(
    cli: &Cli,
    scenes: Option<&Scenes>,
    uploads: Option<&ConversionSlot>,
    viewer: Option<&Viewer>,
    models: usize,
) {
    println!(
        "groma-api listening on http://{} over {} ({} model(s))",
        cli.addr,
        cli.data.display(),
        models
    );
    if let Some(scenes) = scenes {
        let names = scenes.available();
        println!(
            "{} scene(s) on http://{}/scenes: {}",
            names.len(),
            cli.addr,
            if names.is_empty() {
                "none yet - upload one, or run `groma export-scene`".to_owned()
            } else {
                names.join(", ")
            }
        );
        match viewer {
            Some(_) => println!(
                "viewer on http://{}/viewer, drawn from {}",
                cli.addr,
                cli.viewer
                    .as_ref()
                    .expect("a viewer was loaded from a path")
                    .display()
            ),
            None => println!(
                "no viewer: scenes are served over the API alone (pass --viewer to draw them)"
            ),
        }
        match uploads {
            Some(slot) => {
                println!("uploads convert with {}", slot.uploads.groma.display());
                // Said out loud, because "why was my file refused" should not
                // need a reading of the source. An IFC is held in memory to be
                // parsed and so stops sooner than an RVT does.
                #[allow(clippy::cast_precision_loss)] // A figure printed to one decimal.
                let gigabytes = |bytes: u64| bytes as f64 / (1024.0 * 1024.0 * 1024.0);
                println!(
                    "largest upload: {:.1} GiB, or {:.1} GiB for an IFC (raise with --max-upload)",
                    gigabytes(slot.uploads.max_bytes),
                    gigabytes(slot.uploads.max_bytes.min(upload::max_ifc_bytes()))
                );
            }
            None => println!(
                "uploads are refused: no `groma` binary beside this one, and no --groma given"
            ),
        }
        // The upload route writes files and runs a converter, so it is worth
        // saying plainly when it is reachable from anywhere but this machine.
        if !cli.addr.starts_with("127.")
            && !cli.addr.starts_with("localhost")
            && !cli.addr.starts_with("[::1]")
        {
            println!(
                "warning: {} is not loopback, so anyone who can reach it can upload and convert",
                cli.addr
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_target_into_segments_and_decoded_parameters() {
        let (segments, params) =
            parse_target("/models/AR_S1/elements?level=01%20%D0%AD%D1%82%D0%B0%D0%B6&limit=5");
        assert_eq!(segments, ["models", "AR_S1", "elements"]);
        assert_eq!(params["level"], "01 Этаж");
        assert_eq!(params["limit"], "5");
    }

    #[test]
    fn keeps_jobs_from_one_confirmation_in_one_batch() {
        let mut job = Job::new();
        let params = BTreeMap::from([
            ("batch".to_owned(), "batch-42".to_owned()),
            ("batch-label".to_owned(), "4 files".to_owned()),
            ("batch-index".to_owned(), "3".to_owned()),
            ("batch-total".to_owned(), "4".to_owned()),
        ]);
        describe_batch(&mut job, "fallback", &params, "ifc");
        let json = job.to_json();
        assert_eq!(json["batch"], "batch-42");
        assert_eq!(json["batchLabel"], "4 files");
        assert_eq!(json["batchIndex"], 3);
        assert_eq!(json["batchTotal"], 4);
        assert_eq!(json["target"], "ifc");
    }

    #[test]
    fn caps_the_page_size_and_defaults_it() {
        let (_, params) = parse_target("/x?limit=99999");
        assert_eq!(query_from(&params).limit, MAX_LIMIT);
        let (_, params) = parse_target("/x");
        assert_eq!(query_from(&params).limit, DEFAULT_LIMIT);
        // A flag with no value reads as set, which is what `?model_elements`
        // in a hand-written URL means.
        let (_, params) = parse_target("/x?model_elements&with_geometry=false");
        let query = query_from(&params);
        assert!(query.model_elements_only);
        assert!(!query.with_geometry);
    }

    #[test]
    fn refuses_a_model_name_that_tries_to_leave_the_data_directory() {
        let store = Store::new(std::env::temp_dir());
        for name in ["../etc/passwd", "a/b", "..", ".hidden", ""] {
            assert!(store.get(name).is_none(), "{name} was not refused");
        }
    }

    #[test]
    fn decodes_a_malformed_escape_without_dropping_it() {
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("a%zzb"), "a%zzb");
        assert_eq!(percent_decode("a+b"), "a b");
    }
}
