//! Serving `.rvs` scenes to a viewer.
//!
//! The format is built to be read by range: a viewer takes the 24-byte
//! trailer, then the manifest it points at, then only the chunks it means to
//! draw. Serving whole files would throw that away, so this answers `Range`
//! and seeks to the bytes asked for rather than reading a 20 MB scene into
//! memory to send 24 bytes of it.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

/// The formats an upload can have arrived in. A request naming a source to
/// delete picks one of these; it never names a path, so no request can reach a
/// file outside the uploads directory.
pub const SOURCE_EXTENSIONS: [&str; 2] = ["rvt", "ifc"];

/// Scenes available on disk. Nothing is cached: a range read is a seek and a
/// short read, which the page cache already makes cheap.
pub struct Scenes {
    directory: PathBuf,
}

/// One byte range to answer, resolved against the file's own length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range {
    pub start: u64,
    /// Inclusive, as HTTP counts it.
    pub end: u64,
}

impl Range {
    #[must_use]
    pub fn length(self) -> u64 {
        self.end - self.start + 1
    }
}

impl Scenes {
    #[must_use]
    pub fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    /// Scene names on disk, without the extension.
    #[must_use]
    pub fn available(&self) -> Vec<String> {
        self.listing().into_iter().map(|entry| entry.name).collect()
    }

    /// Every scene with what can be known about it without decompressing it:
    /// its size and when it was written. What the scene *is* - the model it
    /// came from, its counts, whether it was an RVT or an IFC - lives in the
    /// manifest, which a reader takes by range for itself rather than having
    /// this inflate every scene on every listing.
    #[must_use]
    pub fn listing(&self) -> Vec<Listed> {
        let mut listed = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.directory) else {
            return listed;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|extension| extension != "rvs") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let metadata = entry.metadata().ok();
            listed.push(Listed {
                name: name.to_owned(),
                bytes: metadata.as_ref().map_or(0, std::fs::Metadata::len),
                // Seconds since the epoch, which is what a page formats in the
                // reader's own locale. A clock that predates it is reported as
                // unknown rather than as a negative time.
                modified: metadata
                    .and_then(|metadata| metadata.modified().ok())
                    .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|since| since.as_secs()),
                has_preview: self.preview_path(name).is_some_and(|at| at.is_file()),
                sources: self.sources(name),
            });
        }
        listed.sort_by(|left, right| left.name.cmp(&right.name));
        listed
    }

    /// The uploaded files a scene's name could have been converted from, with
    /// their sizes. Two can exist at once - the same model uploaded as an RVT
    /// and as an IFC share a stem - so which one this scene actually came from
    /// is settled by the manifest, which the reader already holds, not here.
    #[must_use]
    pub fn sources(&self, name: &str) -> Vec<(String, u64)> {
        if !is_safe_name(name) {
            return Vec::new();
        }
        let uploads = self.directory.join("uploads");
        SOURCE_EXTENSIONS
            .iter()
            .filter_map(|extension| {
                let at = uploads.join(format!("{name}.{extension}"));
                let bytes = std::fs::metadata(&at)
                    .ok()
                    .filter(std::fs::Metadata::is_file)?
                    .len();
                Some(((*extension).to_owned(), bytes))
            })
            .collect()
    }

    /// Delete a scene, its cached preview, and optionally the file it was
    /// converted from.
    ///
    /// `source` is an extension from [`SOURCE_EXTENSIONS`], never a path:
    /// anything else leaves the uploads directory untouched. What was removed
    /// is reported back, so a caller can say what it freed rather than assume.
    ///
    /// # Errors
    ///
    /// [`SceneError::NotFound`] where the name names no scene, and
    /// [`SceneError::Io`] where the scene itself cannot be removed. A preview
    /// or source that will not delete is reported as not removed rather than
    /// failing the request: the scene is already gone by then.
    pub fn remove(&self, name: &str, source: Option<&str>) -> Result<Removed, SceneError> {
        let scene = self.path(name).ok_or(SceneError::NotFound)?;
        let mut removed = Removed::default();
        removed.bytes += std::fs::metadata(&scene).map_or(0, |at| at.len());
        std::fs::remove_file(&scene).map_err(SceneError::Io)?;
        removed.scene = true;

        if let Some(preview) = self.preview_path(name) {
            if let Ok(at) = std::fs::metadata(&preview) {
                if std::fs::remove_file(&preview).is_ok() {
                    removed.bytes += at.len();
                    removed.preview = true;
                }
            }
        }
        // An extension this does not know names nothing, so an unexpected
        // value deletes the scene and stops rather than guessing at a file.
        if let Some(extension) = source.filter(|wanted| SOURCE_EXTENSIONS.contains(wanted)) {
            if is_safe_name(name) {
                let at = self
                    .directory
                    .join("uploads")
                    .join(format!("{name}.{extension}"));
                if let Ok(metadata) = std::fs::metadata(&at) {
                    if metadata.is_file() && std::fs::remove_file(&at).is_ok() {
                        removed.bytes += metadata.len();
                        removed.source = Some(extension.to_owned());
                    }
                }
            }
        }
        Ok(removed)
    }

    /// Where a scene's cached preview image lives. Previews sit in their own
    /// directory so the listing, which looks for `.rvs`, never sees them.
    #[must_use]
    pub fn preview_path(&self, name: &str) -> Option<PathBuf> {
        // PNG, because a preview is drawn on a transparent ground so it can sit
        // on either theme's card; JPEG would flatten that to black.
        is_safe_name(name).then(|| self.directory.join("previews").join(format!("{name}.png")))
    }

    /// One cached preview's bytes, or `None` where none has been made.
    #[must_use]
    pub fn preview(&self, name: &str) -> Option<Vec<u8>> {
        std::fs::read(self.preview_path(name)?).ok()
    }

    /// Cache a preview a viewer rendered. The scene must exist: a preview is
    /// a picture of something this serves, not a way to write arbitrary files
    /// into the directory.
    ///
    /// # Errors
    ///
    /// [`SceneError::NotFound`] where the name names no scene, and
    /// [`SceneError::Io`] where the image cannot be written.
    pub fn store_preview(&self, name: &str, image: &[u8]) -> Result<(), SceneError> {
        if self.path(name).is_none() {
            return Err(SceneError::NotFound);
        }
        let at = self.preview_path(name).ok_or(SceneError::NotFound)?;
        if let Some(parent) = at.parent() {
            std::fs::create_dir_all(parent).map_err(SceneError::Io)?;
        }
        std::fs::write(at, image).map_err(SceneError::Io)
    }

    /// The file one name resolves to, or `None` if the name does not name a
    /// scene directly inside the directory.
    #[must_use]
    pub fn path(&self, name: &str) -> Option<PathBuf> {
        if !is_safe_name(name) {
            return None;
        }
        let path = self.directory.join(format!("{name}.rvs"));
        path.is_file().then_some(path)
    }

    /// Read one range of a named scene, or the whole of it when no range is
    /// asked for. Returns the bytes, the range they cover, and the file's
    /// total length.
    ///
    /// # Errors
    ///
    /// [`SceneError::NotFound`] where the name names no scene,
    /// [`SceneError::NotSatisfiable`] where the range lies past the end, and
    /// [`SceneError::Io`] where the file cannot be read.
    pub fn read(
        &self,
        name: &str,
        wanted: Option<(Option<u64>, Option<u64>)>,
    ) -> Result<(Vec<u8>, Range, u64), SceneError> {
        let path = self.path(name).ok_or(SceneError::NotFound)?;
        let mut file = File::open(&path).map_err(SceneError::Io)?;
        let total = file.metadata().map_err(SceneError::Io)?.len();
        let range = resolve(wanted, total).ok_or(SceneError::NotSatisfiable(total))?;
        file.seek(SeekFrom::Start(range.start))
            .map_err(SceneError::Io)?;
        let length = usize::try_from(range.length()).unwrap_or(usize::MAX);
        let mut bytes = vec![0_u8; length];
        file.read_exact(&mut bytes).map_err(SceneError::Io)?;
        Ok((bytes, range, total))
    }
}

/// One scene as a listing shows it.
pub struct Listed {
    pub name: String,
    pub bytes: u64,
    /// Seconds since the Unix epoch, where the filesystem states one.
    pub modified: Option<u64>,
    pub has_preview: bool,
    /// `(extension, bytes)` for every upload sharing this scene's name.
    pub sources: Vec<(String, u64)>,
}

/// What one delete actually removed. Reported rather than assumed, because a
/// scene converted by the CLI has no upload behind it and a preview exists
/// only once someone has opened the model.
#[derive(Debug, Default)]
pub struct Removed {
    pub scene: bool,
    pub preview: bool,
    /// The extension of the source removed, where one was asked for and found.
    pub source: Option<String>,
    /// Bytes freed across all of them.
    pub bytes: u64,
}

/// Why a scene could not be served.
#[derive(Debug)]
pub enum SceneError {
    /// The name names no scene in the directory.
    NotFound,
    /// The range lies past the end. Carries the length, which is what a 416
    /// must state so a reader can correct itself.
    NotSatisfiable(u64),
    Io(std::io::Error),
}

/// A name must resolve to a file directly inside the scene directory.
/// Anything with a separator or a parent segment is refused rather than
/// sanitised, so a request cannot reach outside it.
fn is_safe_name(name: &str) -> bool {
    !(name.is_empty() || name.contains(['/', '\\']) || name.contains("..") || name.starts_with('.'))
}

/// Resolve a parsed range against the length of the file it applies to.
/// `None` means the range cannot be satisfied and the answer is a 416.
fn resolve(wanted: Option<(Option<u64>, Option<u64>)>, total: u64) -> Option<Range> {
    if total == 0 {
        return None;
    }
    let last = total - 1;
    match wanted {
        None => Some(Range {
            start: 0,
            end: last,
        }),
        // `bytes=-N`: the final N bytes, which is how a reader takes the
        // trailer without knowing the length first.
        Some((None, Some(suffix))) => {
            let suffix = suffix.min(total);
            (suffix > 0).then(|| Range {
                start: total - suffix,
                end: last,
            })
        }
        Some((Some(start), end)) => {
            if start > last {
                return None;
            }
            Some(Range {
                start,
                end: end.unwrap_or(last).min(last),
            })
        }
        Some((None, None)) => None,
    }
}

/// Read a `Range` header. Only the single-range `bytes=` forms are accepted;
/// a multi-range request is answered whole, which is allowed and simpler than
/// a multipart body no viewer here asks for.
#[must_use]
pub fn parse_range(header: &str) -> Option<(Option<u64>, Option<u64>)> {
    let value = header.trim().strip_prefix("bytes=")?;
    if value.contains(',') {
        return None;
    }
    let (start, end) = value.split_once('-')?;
    let start = start.trim();
    let end = end.trim();
    let parsed_start = if start.is_empty() {
        None
    } else {
        Some(start.parse().ok()?)
    };
    let parsed_end = if end.is_empty() {
        None
    } else {
        Some(end.parse().ok()?)
    };
    if parsed_start.is_none() && parsed_end.is_none() {
        return None;
    }
    Some((parsed_start, parsed_end))
}

#[cfg(test)]
mod tests {
    use super::{Range, Scenes, is_safe_name, parse_range, resolve};

    /// A delete must reach the scene, its preview and the one upload it names
    /// - and nothing else, however the request spells the name or the format.
    #[test]
    fn deletes_a_scene_with_its_preview_and_named_source_only() {
        let root = std::env::temp_dir().join(format!("openrvt-scenes-{}", std::process::id()));
        let uploads = root.join("uploads");
        std::fs::create_dir_all(&uploads).unwrap();
        std::fs::create_dir_all(root.join("previews")).unwrap();
        std::fs::write(root.join("model.rvs"), b"scene-bytes").unwrap();
        std::fs::write(root.join("previews/model.png"), b"png!").unwrap();
        std::fs::write(uploads.join("model.rvt"), b"revit-source").unwrap();
        std::fs::write(uploads.join("model.ifc"), b"ifc-source").unwrap();
        std::fs::write(root.join("keep.rvs"), b"other").unwrap();
        let scenes = Scenes::new(root.clone());

        // Both uploads are offered, since a name alone cannot say which one
        // this scene came from.
        assert_eq!(
            scenes.sources("model"),
            vec![("rvt".to_owned(), 12), ("ifc".to_owned(), 10)]
        );

        let removed = scenes.remove("model", Some("ifc")).unwrap();
        assert!(removed.scene && removed.preview);
        assert_eq!(removed.source.as_deref(), Some("ifc"));
        assert_eq!(removed.bytes, 11 + 4 + 10);
        assert!(!root.join("model.rvs").exists());
        assert!(!root.join("previews/model.png").exists());
        assert!(!uploads.join("model.ifc").exists());
        // The format that was not named is the user's other upload.
        assert!(uploads.join("model.rvt").exists());
        assert!(root.join("keep.rvs").exists());

        // A name that is gone, a name that tries to leave the directory, and
        // a format that is not one this serves.
        assert!(scenes.remove("model", None).is_err());
        assert!(scenes.remove("../keep", None).is_err());
        std::fs::write(root.join("other.rvs"), b"x").unwrap();
        let removed = scenes.remove("other", Some("../../model")).unwrap();
        assert_eq!(removed.source, None, "an unknown format named a file");
        assert!(uploads.join("model.rvt").exists());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn reads_the_range_forms_a_viewer_sends() {
        assert_eq!(parse_range("bytes=0-99"), Some((Some(0), Some(99))));
        assert_eq!(parse_range("bytes=100-"), Some((Some(100), None)));
        assert_eq!(parse_range("bytes=-24"), Some((None, Some(24))));
        assert_eq!(parse_range(" bytes=5-9 "), Some((Some(5), Some(9))));
    }

    #[test]
    fn refuses_the_forms_it_does_not_answer() {
        for header in [
            "",
            "items=0-1",
            "bytes=",
            "bytes=-",
            "bytes=0-1,5-6",
            "bytes=a-b",
        ] {
            assert!(parse_range(header).is_none(), "{header} was not refused");
        }
    }

    #[test]
    fn resolves_a_suffix_range_against_the_length() {
        // The trailer request: the last 24 bytes of a 1000-byte file.
        assert_eq!(
            resolve(Some((None, Some(24))), 1000),
            Some(Range {
                start: 976,
                end: 999
            })
        );
        // A suffix longer than the file is the whole file, not an error.
        assert_eq!(
            resolve(Some((None, Some(4000))), 1000),
            Some(Range { start: 0, end: 999 })
        );
    }

    #[test]
    fn clamps_an_end_past_the_last_byte_and_refuses_a_start_past_it() {
        assert_eq!(
            resolve(Some((Some(990), Some(99_999))), 1000),
            Some(Range {
                start: 990,
                end: 999
            })
        );
        assert_eq!(resolve(Some((Some(1000), None)), 1000), None);
        assert_eq!(resolve(None, 0), None);
    }

    #[test]
    fn refuses_a_name_that_tries_to_leave_the_scene_directory() {
        for name in ["../secret", "a/b", "..", ".hidden", ""] {
            assert!(!is_safe_name(name), "{name} was not refused");
        }
        assert!(is_safe_name("small"));
    }
}
