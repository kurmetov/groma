//! The corpus regression gate.
//!
//! Every decode rule in this project is justified by a measurement over the
//! reference corpus, and the project's working notes requires those measurements to be
//! taken before and after any change to the record walk - a change can buy one
//! class by selling another, and nothing else catches that. Until now that
//! comparison was a human diffing two TSV files by eye, which means a
//! regression is caught only if someone remembers to look.
//!
//! This test does the comparison instead. `tests/baseline/corpus_metrics.tsv`
//! holds the accepted numbers; this test re-measures them and fails on any
//! movement in the wrong direction.
//!
//! The corpus is confidential and is not in the repository, so the gate is
//! opt-in: it runs only when `RIVET_CORPUS` points at a directory of `.rvt`
//! files, and skips otherwise. That keeps `cargo test` fast and keeps CI - which
//! has no corpus - meaningful.
//!
//! ```bash
//! scripts/corpus_check.sh                      # measure and compare
//! scripts/corpus_check.sh --write              # accept current numbers
//! ```
//!
//! The baseline records only counts, never bytes or file names from the corpus:
//! files are keyed by the `SMALL`/`MEDIUM`/`BIG` prefix of their name, so no
//! client model is identified by anything committed here.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Classes always measured, whether or not they are among a file's most
/// numerous. the project's working notes calls these out: `GElement` is the number
/// geometry depends on, and the two family classes carry the element/type link.
const ALWAYS_MEASURED: [&str; 3] = ["GElement", "FamilySymbol", "FamilyInstance"];

/// How many of a file's most numerous classes are measured.
const TOP_CLASSES: usize = 20;

/// Which way a metric is allowed to move.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Better {
    /// More is progress; a fall is a regression. Records explained, faces resolved.
    Higher,
    /// Less is progress; a rise is a regression. Faces excluded, edges unresolved.
    Lower,
}

impl Better {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "higher" => Some(Self::Higher),
            "lower" => Some(Self::Lower),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Higher => "higher",
            Self::Lower => "lower",
        }
    }

    /// Whether moving from `baseline` to `measured` is a regression.
    fn is_regression(self, baseline: u64, measured: u64) -> bool {
        match self {
            Self::Higher => measured < baseline,
            Self::Lower => measured > baseline,
        }
    }
}

/// One measured number, keyed by corpus file label and metric name.
struct Metric {
    label: String,
    key: String,
    value: u64,
    better: Better,
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root resolves")
}

fn baseline_path() -> PathBuf {
    workspace_root().join("tests/baseline/corpus_metrics.tsv")
}

/// The `rivet` binary to measure with.
///
/// Defaults to the one cargo built for this test, but the corpus files run to
/// hundreds of megabytes and an unoptimized walk over them is slow enough to
/// discourage running the gate at all, so `scripts/corpus_check.sh` points this
/// at the release build.
fn rivet_binary() -> PathBuf {
    std::env::var_os("RIVET_BIN")
        .map_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_rivet")), PathBuf::from)
}

/// The corpus files, labelled by the prefix of their name up to the first `_`.
fn corpus_files(directory: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("rvt"))
        {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let label = name.split('_').next().unwrap_or(name).to_owned();
        files.push((label, path));
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

fn run_rivet(binary: &Path, args: &[&str]) -> String {
    let output = Command::new(binary)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("running {} failed: {e}", binary.display()));
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The `index`-th run of digits in `text`, parsed.
fn integer_at(text: &str, index: usize) -> Option<u64> {
    text.split(|c: char| !c.is_ascii_digit())
        .filter(|piece| !piece.is_empty())
        .nth(index)
        .and_then(|piece| piece.parse().ok())
}

/// The `index`-th number on the line introduced by `label:`.
///
/// Matching the label as a prefix of the trimmed line, rather than searching
/// anywhere, keeps a phrase that also appears inside a parenthetical from being
/// read as its own field.
fn field_at(text: &str, label: &str, index: usize) -> Option<u64> {
    text.lines().find_map(|line| {
        let rest = line.trim_start().strip_prefix(label)?.strip_prefix(':')?;
        integer_at(rest, index)
    })
}

fn field(text: &str, label: &str) -> Option<u64> {
    field_at(text, label, 0)
}

/// A file's most numerous element classes, most numerous first.
fn top_classes(binary: &Path, file: &Path) -> Vec<String> {
    let text = run_rivet(binary, &["inspect", &file.to_string_lossy()]);
    let mut classes = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        if line.starts_with("Element classes") {
            inside = true;
            continue;
        }
        if inside {
            if line.trim().is_empty() {
                break;
            }
            let mut columns = line.split_whitespace();
            let (Some(id), Some(name)) = (columns.next(), columns.next()) else {
                continue;
            };
            if id.chars().all(|c| c.is_ascii_digit()) {
                classes.push(name.to_owned());
            }
        }
    }
    classes.truncate(TOP_CLASSES);
    classes
}

/// Measure one corpus file across `classes`, plus its whole-file B-Rep tallies.
fn measure(binary: &Path, label: &str, file: &Path, classes: &[String]) -> Vec<Metric> {
    let path = file.to_string_lossy().into_owned();
    let mut metrics = Vec::new();
    let mut push = |key: String, value: u64, better: Better| {
        metrics.push(Metric {
            label: label.to_owned(),
            key,
            value,
            better,
        });
    };

    for class in classes {
        let text = run_rivet(
            binary,
            &["serial-probe", &path, "--record", "--class", class],
        );
        let Some(walked) = field(&text, "Records walked") else {
            continue;
        };
        // A class with no records says nothing and would only add noise that
        // can never move.
        if walked == 0 {
            continue;
        }
        push(format!("class.{class}.walked"), walked, Better::Higher);
        if let Some(exact) = field(&text, "explained exactly") {
            push(format!("class.{class}.exact"), exact, Better::Higher);
        }

        // The geometry-bearing subset, which is the number bodies depend on.
        if class == "GElement" {
            let faces = "records carrying boundary faces";
            if let Some(records) = field_at(&text, faces, 0) {
                push("gelement.face_records".to_owned(), records, Better::Higher);
            }
            if let Some(exact) = field_at(&text, faces, 1) {
                push(
                    "gelement.face_records_exact".to_owned(),
                    exact,
                    Better::Higher,
                );
            }
            if let Some(count) = field(&text, "faces in them") {
                push("gelement.faces".to_owned(), count, Better::Higher);
            }
        }
    }

    let text = run_rivet(binary, &["brep", &path]);
    for (key, label, better) in [
        (
            "brep.records_with_body",
            "Records producing a body",
            Better::Higher,
        ),
        (
            "brep.bodies_complete",
            "every face resolved",
            Better::Higher,
        ),
        ("brep.faces_resolved", "Faces resolved", Better::Higher),
        ("brep.faces_excluded", "Faces excluded", Better::Lower),
        (
            "brep.edges_unresolved",
            "Edges that did not resolve",
            Better::Lower,
        ),
        (
            "brep.complete_bounding_a_volume",
            "complete records bounding a volume",
            Better::Higher,
        ),
        // The backlog the openness breakdown leaves: a complete record no body
        // of which closes, by either reading. Lower is better, and it is
        // checked in that direction so a change that closes more records by
        // dropping faces out of them cannot pass.
        (
            "brep.complete_with_no_closed_body",
            "of those, none of their bodies closes at all",
            Better::Lower,
        ),
    ] {
        if let Some(value) = field(&text, label) {
            push(key.to_owned(), value, better);
        }
    }

    metrics
}

fn write_baseline(metrics: &[Metric], path: &Path) {
    let mut text = String::new();
    text.push_str(
        "# Rivet corpus regression baseline.\n\
         #\n\
         # Accepted measurements over the reference corpus. Counts only: no bytes\n\
         # and no file names from the corpus appear here, so this file carries no\n\
         # client model content. Files are keyed by the prefix of their name.\n\
         #\n\
         # Regenerate with `scripts/corpus_check.sh --write` and commit the result\n\
         # together with the change that moved it, so the diff shows what the\n\
         # change bought and what, if anything, it sold.\n\
         #\n\
         # label\tmetric\tvalue\tbetter\n",
    );
    let mut sorted: Vec<&Metric> = metrics.iter().collect();
    sorted.sort_by(|a, b| a.label.cmp(&b.label).then(a.key.cmp(&b.key)));
    for metric in sorted {
        let _ = writeln!(
            text,
            "{}\t{}\t{}\t{}",
            metric.label,
            metric.key,
            metric.value,
            metric.better.as_str()
        );
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("baseline directory is creatable");
    }
    std::fs::write(path, text).expect("baseline is writable");
}

/// `(label, metric) -> (value, direction)`.
type Baseline = BTreeMap<(String, String), (u64, Better)>;

fn read_baseline(path: &Path) -> Baseline {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| {
        panic!(
            "the baseline at {} could not be read ({e}); generate it with \
             `scripts/corpus_check.sh --write`",
            path.display()
        )
    });
    let mut baseline = BTreeMap::new();
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let mut columns = line.split('\t');
        let (Some(label), Some(key), Some(value), Some(better)) = (
            columns.next(),
            columns.next(),
            columns.next(),
            columns.next(),
        ) else {
            panic!("baseline line is not four tab-separated columns: {line}");
        };
        let value = value
            .parse()
            .unwrap_or_else(|_| panic!("baseline value is not a number: {line}"));
        let better = Better::parse(better)
            .unwrap_or_else(|| panic!("baseline direction is not higher/lower: {line}"));
        baseline.insert((label.to_owned(), key.to_owned()), (value, better));
    }
    baseline
}

/// The classes a label's baseline rows mention, so a check measures exactly
/// what was accepted rather than re-deriving the class list.
fn baselined_classes(baseline: &Baseline, label: &str) -> Vec<String> {
    let mut classes: Vec<String> = baseline
        .keys()
        .filter(|(row_label, _)| row_label == label)
        .filter_map(|(_, key)| {
            key.strip_prefix("class.")
                .and_then(|rest| rest.strip_suffix(".walked"))
                .map(str::to_owned)
        })
        .collect();
    classes.sort();
    classes.dedup();
    classes
}

#[test]
fn corpus_measurements_have_not_regressed() {
    let Some(corpus) = std::env::var_os("RIVET_CORPUS").map(|value| {
        // Cargo runs a test with the package directory as its working
        // directory, not the workspace root, so a relative corpus path means
        // what the person typing it meant only if it is resolved from the root.
        let path = PathBuf::from(value);
        if path.is_relative() {
            workspace_root().join(path)
        } else {
            path
        }
    }) else {
        eprintln!(
            "skipping the corpus regression gate: RIVET_CORPUS is not set.\n\
             Run `scripts/corpus_check.sh` on a machine that has the corpus."
        );
        return;
    };

    let files = corpus_files(&corpus);
    assert!(
        !files.is_empty(),
        "RIVET_CORPUS points at {} but it holds no .rvt files",
        corpus.display()
    );

    let binary = rivet_binary();
    let path = baseline_path();
    let writing = std::env::var_os("RIVET_BASELINE_WRITE").is_some();

    if writing {
        let mut metrics = Vec::new();
        for (label, file) in &files {
            let mut classes = top_classes(&binary, file);
            for forced in ALWAYS_MEASURED {
                if !classes.iter().any(|c| c == forced) {
                    classes.push(forced.to_owned());
                }
            }
            eprintln!("measuring {label} across {} classes", classes.len());
            metrics.extend(measure(&binary, label, file, &classes));
        }
        write_baseline(&metrics, &path);
        eprintln!("wrote {} rows to {}", metrics.len(), path.display());
        return;
    }

    let baseline = read_baseline(&path);
    let mut regressions = String::new();
    let mut improvements = String::new();
    let mut missing = String::new();
    let mut compared = 0_usize;

    for (label, file) in &files {
        let classes = baselined_classes(&baseline, label);
        if classes.is_empty() {
            // A corpus file the baseline never covered. Not a failure: the gate
            // judges what was accepted, and a new file is accepted by writing it.
            eprintln!("no baseline rows for {label}; skipping it");
            continue;
        }
        eprintln!("checking {label} across {} classes", classes.len());
        let measured = measure(&binary, label, file, &classes);
        let seen: BTreeMap<&str, u64> =
            measured.iter().map(|m| (m.key.as_str(), m.value)).collect();

        for ((row_label, key), (expected, better)) in &baseline {
            if row_label != label {
                continue;
            }
            let Some(&actual) = seen.get(key.as_str()) else {
                let _ = writeln!(
                    missing,
                    "  {label} {key}: baseline has {expected}, not measured"
                );
                continue;
            };
            compared += 1;
            if better.is_regression(*expected, actual) {
                let _ = writeln!(
                    regressions,
                    "  {label} {key}: {expected} -> {actual} ({} is better)",
                    better.as_str()
                );
            } else if actual != *expected {
                let _ = writeln!(improvements, "  {label} {key}: {expected} -> {actual}");
            }
        }
    }

    assert!(
        compared > 0,
        "no baseline row matched any corpus file; the corpus at {} does not \
         correspond to the committed baseline",
        corpus.display()
    );

    if !improvements.is_empty() {
        eprintln!("measurements improved:\n{improvements}");
    }
    if !missing.is_empty() {
        eprintln!("baseline rows that produced no measurement:\n{missing}");
    }

    assert!(
        regressions.is_empty() && missing.is_empty(),
        "the corpus measurements moved the wrong way against \
         tests/baseline/corpus_metrics.tsv.\n\n\
         regressions:\n{regressions}\n\
         rows that produced no measurement:\n{missing}\n\
         If the change is a deliberate trade, record why in the commit message \
         and re-accept with `scripts/corpus_check.sh --write`.",
    );

    eprintln!("{compared} measurements checked, none regressed");
}

/// The baseline itself, checked without a corpus.
///
/// This is the half of the gate CI can run: it cannot re-measure anything, but
/// it can catch a baseline that was hand-edited into nonsense, truncated by a
/// bad merge, or committed with a class whose "explained exactly" exceeds the
/// records walked - which would silently make that row unfailable.
#[test]
fn baseline_is_internally_consistent() {
    let baseline = read_baseline(&baseline_path());
    assert!(!baseline.is_empty(), "the baseline holds no rows");

    let mut labels: Vec<&str> = baseline
        .keys()
        .map(|(label, _)| label.as_str())
        .collect::<Vec<_>>();
    labels.sort_unstable();
    labels.dedup();
    assert!(!labels.is_empty(), "the baseline names no corpus file");

    for label in labels {
        let classes = baselined_classes(&baseline, label);
        assert!(
            !classes.is_empty(),
            "{label} has no class rows, so nothing about its record walk is gated"
        );

        for class in &classes {
            let walked = baseline.get(&(label.to_owned(), format!("class.{class}.walked")));
            let exact = baseline.get(&(label.to_owned(), format!("class.{class}.exact")));
            if let (Some((walked, _)), Some((exact, _))) = (walked, exact) {
                assert!(
                    exact <= walked,
                    "{label} {class}: {exact} explained exactly out of {walked} walked"
                );
            }
        }

        // The numbers geometry depends on. If these ever drop out of the
        // baseline, the gate would still pass while gating nothing that matters.
        for key in [
            "gelement.face_records",
            "gelement.face_records_exact",
            "brep.bodies_complete",
            "brep.faces_resolved",
            "brep.complete_bounding_a_volume",
            "brep.complete_with_no_closed_body",
        ] {
            assert!(
                baseline.contains_key(&(label.to_owned(), key.to_owned())),
                "{label} is missing {key}, which is a number geometry depends on"
            );
        }

        let face_records = baseline.get(&(label.to_owned(), "gelement.face_records".to_owned()));
        let face_exact =
            baseline.get(&(label.to_owned(), "gelement.face_records_exact".to_owned()));
        if let (Some((records, _)), Some((exact, _))) = (face_records, face_exact) {
            assert!(
                exact <= records,
                "{label}: {exact} face-bearing records explained exactly out of {records}"
            );
        }
    }

    // Directions must be the ones that make a regression fail. An excluded-face
    // tally recorded as "higher is better" would invert the test it exists for,
    // and so would the backlog of records no body of which closes.
    for ((label, key), (_, better)) in &baseline {
        let expected = if key.ends_with("faces_excluded")
            || key.ends_with("edges_unresolved")
            || key.ends_with("with_no_closed_body")
        {
            Better::Lower
        } else {
            Better::Higher
        };
        assert!(
            *better == expected,
            "{label} {key} is recorded as \"{}\" is better",
            better.as_str()
        );
    }
}
