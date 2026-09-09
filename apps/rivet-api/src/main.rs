//! A read-only HTTP/JSON API over the models `rivet export-json` produced, so
//! an agent can be given tools against them and a retrieval index can be fed
//! from them.
//!
//! It serves artefacts rather than parsing `.rvt` itself: a full decode costs
//! seconds and gigabytes, which is not a request handler's business, and
//! serving the artefact keeps every answer traceable to the export it came
//! from. Point `--data` at a directory of `<model>.jsonl` files.

mod scenes;
mod store;
mod upload;

use std::collections::BTreeMap;
use std::error::Error;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use clap::Parser;
use scenes::{SceneError, Scenes};
use store::{Model, Query};
use tiny_http::{Header, Request, Response, Server};
use upload::{DEFAULT_MAX_UPLOAD_BYTES, Job, Uploads};

/// The viewer page, embedded so the binary needs nothing beside it.
const VIEWER_HTML: &str = include_str!("../../../web/viewer.html");

/// Page size a request gets when it asks for none.
const DEFAULT_LIMIT: usize = 50;
/// Page size no request may exceed, so one call cannot pull a whole model into
/// an agent's context by accident. `/documents` is the deliberate bulk route.
const MAX_LIMIT: usize = 1000;

#[derive(Parser, Debug)]
#[command(
    name = "rivet-api",
    about = "Read-only HTTP/JSON API over exported RVT models"
)]
struct Cli {
    /// Directory of `<model>.jsonl` files written by `rivet export-json`.
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
    /// Directory of `<scene>.rvs` files written by `rivet export-scene`. With
    /// one, `/viewer` draws them; without one, the scene routes are absent.
    #[arg(long)]
    scenes: Option<PathBuf>,
    /// The `rivet` binary that converts an uploaded model. Defaults to the one
    /// beside this executable; without either, uploading is refused and the
    /// scenes already on disk are still served.
    #[arg(long)]
    rivet: Option<PathBuf>,
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
        Header::from_bytes(&b"X-Rivet"[..], &b"header"[..]).expect("static header")
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
fn serve_upload(
    slot: Option<&ConversionSlot>,
    params: &BTreeMap<String, String>,
    mut request: Request,
) -> std::io::Result<()> {
    let Some(slot) = slot else {
        return request.respond(error(
            503,
            "this server cannot convert uploads: no rivet binary was found beside it, \
             and none was given with --rivet",
        ));
    };
    if request.method() != &tiny_http::Method::Post {
        return request.respond(error(405, "upload is a POST"));
    }
    let name = params.get("name").map_or("model", String::as_str);

    let received = slot.uploads.receive(name, request.as_reader());
    let (source, format, stem) = match received {
        Ok(received) => received,
        Err(message) => return request.respond(error(400, &message)),
    };
    let id = job_id();
    let job = Arc::new(Mutex::new(Job::new()));
    slot.remember(id.clone(), &job);
    let response = json_response(
        202,
        &serde_json::json!({ "job": id, "scene": stem, "format": format.extension() }),
    );

    let uploads = slot.uploads.clone();
    let running = Arc::clone(&slot.running);
    std::thread::spawn(move || {
        // One at a time, so two tabs cannot decode two models at once.
        let _guard = running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match uploads.convert(&source, &stem) {
            Ok(child) => upload::follow(child, &stem, &job),
            Err(failure) => {
                if let Ok(mut held) = job.lock() {
                    held.state = upload::JobState::Failed;
                    held.error = Some(format!("the converter could not be started: {failure}"));
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

#[allow(clippy::too_many_lines)] // One match over the routes this server serves.
fn handle(
    store: &Store,
    scenes: Option<&Scenes>,
    uploads: Option<&ConversionSlot>,
    request: Request,
) -> std::io::Result<()> {
    let (segments, params) = parse_target(request.url());
    let route = segments.iter().map(String::as_str).collect::<Vec<_>>();

    // The viewer and its scenes are answered before the JSON routes: a scene
    // is bytes served by range, not a document, and the page is HTML.
    match route.as_slice() {
        ["viewer"] => {
            let content_type =
                Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..])
                    .expect("static header");
            return request.respond(
                Response::from_data(VIEWER_HTML)
                    .with_header(content_type)
                    // The viewer is embedded in this binary. A local server
                    // restart must publish its new UI immediately rather than
                    // leave a browser running a cached copy of the old one.
                    .with_header(header("Cache-Control", "no-store, max-age=0")),
            );
        }
        ["scenes"] => {
            let names = scenes.map(Scenes::available).unwrap_or_default();
            return request.respond(json_response(200, &serde_json::json!({ "scenes": names })));
        }
        ["scenes", name] => return serve_scene(scenes, name, request),
        ["upload"] => return serve_upload(uploads, &params, request),
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
        [] | ["health"] => json_response(
            200,
            &serde_json::json!({
                "status": "ok",
                "service": "rivet-api",
                "models": store.available(),
                "routes": [
                    "/models",
                    "/models/{model}/summary",
                    "/models/{model}/elements?class=&category=&storey=&level=&name=&q=&model_elements=&with_geometry=&offset=&limit=",
                    "/models/{model}/elements/{id}",
                    "/models/{model}/rooms",
                    "/models/{model}/levels",
                    "/models/{model}/documents",
                    "/viewer",
                    "/scenes",
                    "/scenes/{scene}",
                    "/upload?name=",
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
        let rivet = cli
            .rivet
            .clone()
            .or_else(upload::rivet_beside_this_executable)?;
        Some(Arc::new(ConversionSlot {
            uploads: Uploads {
                scenes: cli.scenes.clone()?,
                rivet,
                max_bytes: cli.max_upload,
            },
            running: Arc::new(Mutex::new(())),
            jobs: Mutex::new(BTreeMap::new()),
        }))
    });
    let store = Arc::new(Store::new(cli.data.clone()));
    let available = store.available();
    if cli.preload {
        for name in &available {
            let _ = store.get(name);
        }
    }

    let server = Server::http(&cli.addr).map_err(|error| format!("listen failed: {error}"))?;
    println!(
        "rivet-api listening on http://{} over {} ({} model(s))",
        cli.addr,
        cli.data.display(),
        available.len()
    );
    if let Some(scenes) = scenes.as_ref() {
        let names = scenes.available();
        println!(
            "viewer on http://{}/viewer ({} scene(s): {})",
            cli.addr,
            names.len(),
            if names.is_empty() {
                "none yet - upload one, or run `rivet export-scene`".to_owned()
            } else {
                names.join(", ")
            }
        );
        match uploads.as_ref() {
            Some(slot) => println!("uploads convert with {}", slot.uploads.rivet.display()),
            None => println!(
                "uploads are refused: no `rivet` binary beside this one, and no --rivet given"
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
    let server = Arc::new(server);
    let mut workers = Vec::new();
    for _ in 0..cli.threads.max(1) {
        let server = Arc::clone(&server);
        let store = Arc::clone(&store);
        let scenes = scenes.clone();
        let uploads = uploads.clone();
        workers.push(std::thread::spawn(move || {
            for request in server.incoming_requests() {
                if let Err(error) = handle(&store, scenes.as_deref(), uploads.as_deref(), request) {
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
