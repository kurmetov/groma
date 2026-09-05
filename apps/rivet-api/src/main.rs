//! A read-only HTTP/JSON API over the models `rivet export-json` produced, so
//! an agent can be given tools against them and a retrieval index can be fed
//! from them.
//!
//! It serves artefacts rather than parsing `.rvt` itself: a full decode costs
//! seconds and gigabytes, which is not a request handler's business, and
//! serving the artefact keeps every answer traceable to the export it came
//! from. Point `--data` at a directory of `<model>.jsonl` files.

mod store;

use std::collections::BTreeMap;
use std::error::Error;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use clap::Parser;
use store::{Model, Query};
use tiny_http::{Header, Request, Response, Server};

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
        name: text("name"),
        text: text("q"),
        model_elements_only: flag("model_elements"),
        with_geometry: flag("with_geometry"),
        offset: number("offset", 0),
        limit: number("limit", DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
    }
}

fn handle(store: &Store, request: Request) -> std::io::Result<()> {
    let (segments, params) = parse_target(request.url());
    let route = segments.iter().map(String::as_str).collect::<Vec<_>>();

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
                    "/models/{model}/elements?class=&category=&level=&name=&q=&model_elements=&with_geometry=&offset=&limit=",
                    "/models/{model}/elements/{id}",
                    "/models/{model}/rooms",
                    "/models/{model}/levels",
                    "/models/{model}/documents",
                ],
            }),
        ),
        ["models"] => json_response(200, &serde_json::json!({ "models": store.available() })),
        ["models", name] | ["models", name, "summary"] => match store.get(name) {
            Some(model) => json_response(200, &model.summary()),
            None => error(404, "unknown model"),
        },
        ["models", name, "elements"] => match store.get(name) {
            Some(model) => {
                let query = query_from(&params);
                let (page, total) = model.query(&query);
                json_response(
                    200,
                    &serde_json::json!({
                        "model": name,
                        "total": total,
                        "offset": query.offset,
                        "limit": query.limit,
                        "elements": page,
                    }),
                )
            }
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
                let levels = model.levels();
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
    let server = Arc::new(server);
    let mut workers = Vec::new();
    for _ in 0..cli.threads.max(1) {
        let server = Arc::clone(&server);
        let store = Arc::clone(&store);
        workers.push(std::thread::spawn(move || {
            for request in server.incoming_requests() {
                if let Err(error) = handle(&store, request) {
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
