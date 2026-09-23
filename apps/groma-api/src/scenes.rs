//! Serving `.rvs` scenes to a viewer, and keeping the versions of each.
//!
//! The format is built to be read by range: a viewer takes the 24-byte
//! trailer, then the manifest it points at, then only the chunks it means to
//! draw. Serving whole files would throw that away, so this answers `Range`
//! and seeks to the bytes asked for rather than reading a 20 MB scene into
//! memory to send 24 bytes of it.
//!
//! Converting a model that has been converted before does not overwrite what
//! is there. Each conversion writes the next version, the name resolves to
//! the newest, and the ones before it stay readable by number - which is what
//! lets a model be re-converted after a decode improvement without losing the
//! scene anyone was already looking at. See [`versions_in`] for the layout.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// The formats an upload can have arrived in. A request naming a source to
/// delete picks one of these; it never names a path, so no request can reach a
/// file outside the uploads directory.
pub const SOURCE_EXTENSIONS: [&str; 2] = ["rvt", "ifc"];

/// The directory the versions of every scene are kept under, one directory
/// per scene name inside it.
///
/// It sits beside the scenes rather than among them, so the listing - which
/// looks for `.rvs` files directly in the directory - never sees it.
const VERSIONS: &str = "versions";

/// One version of a scene, as the filesystem states it.
///
/// Nothing here is read out of the scene itself. What a version *is* - the
/// model it came from, which save of that document, its counts - is in the
/// manifest, and a reader takes that by range for itself rather than having
/// this inflate every version of every scene to list them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Version {
    /// One-based, and the order they were converted in.
    pub number: u32,
    pub bytes: u64,
    /// Seconds since the Unix epoch, where the filesystem states one.
    pub modified: Option<u64>,
}

/// Every version of one scene, oldest first.
///
/// The layout is `versions/{name}/{number}.rvs`, plus one older location:
/// `{name}.rvs` directly in the scene directory, which is where every scene
/// written before there were versions sits. That file is read as version 1
/// rather than left outside the sequence, so a model converted once before
/// this existed and once after has a history of two rather than a history
/// that starts at the second. A numbered file wins over it, which can only
/// happen if one was put there by hand.
#[must_use]
pub fn versions_in(directory: &Path, name: &str) -> Vec<Version> {
    version_paths(directory, name)
        .into_iter()
        .map(|(number, path)| {
            let metadata = std::fs::metadata(&path).ok();
            Version {
                number,
                bytes: metadata.as_ref().map_or(0, std::fs::Metadata::len),
                modified: metadata
                    .and_then(|metadata| metadata.modified().ok())
                    .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|since| since.as_secs()),
            }
        })
        .collect()
}

/// The file beside a scene's versions that remembers the highest number ever
/// handed out for it, so that deleting a version does not free its number.
///
/// Its stem is not a number, so [`version_paths`] passes over it and it is
/// never mistaken for a version.
const HIGH_WATER: &str = "last";

/// Where the next conversion of this scene writes.
///
/// One past the highest number ever handed out, which is not the same as one
/// past the highest version present: a number is never reused, so a client
/// that pinned version 3 is never later handed a different model under that
/// name because 3 was deleted in between. The mark is kept in a file beside
/// the versions; if it is lost, the numbering falls back to one past what is
/// there, which is the best that can be said from the directory alone.
///
/// The conversion writes straight here rather than writing over the current
/// scene and archiving it afterwards: a conversion that fails, or that is
/// cancelled halfway, then leaves a partial file nobody resolves to instead
/// of a broken current version.
///
/// # Errors
///
/// Fails where the name could not resolve inside the directory, or where the
/// directory for it cannot be made.
pub fn next_version_path(directory: &Path, name: &str) -> std::io::Result<PathBuf> {
    if !is_safe_name(name) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "a scene name must resolve inside the scene directory",
        ));
    }
    let at = directory.join(VERSIONS).join(name);
    std::fs::create_dir_all(&at)?;
    let present = version_paths(directory, name)
        .keys()
        .next_back()
        .copied()
        .unwrap_or(0);
    let handed = std::fs::read_to_string(at.join(HIGH_WATER))
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
        .unwrap_or(0);
    let number = present.max(handed).saturating_add(1);
    // A mark that will not write leaves the numbering to the directory, which
    // is what it was before there was a mark. That is worth a conversion that
    // still happens rather than one refused over a bookkeeping file.
    let _ = std::fs::write(at.join(HIGH_WATER), number.to_string());
    Ok(at.join(format!("{number}.rvs")))
}

/// Where a scene's cached preview image lives, given the directory alone.
///
/// A preview is a picture of the version that was current when it was drawn,
/// so it is the newest version's picture and no older one's. [`forget_preview_in`]
/// is what keeps that true.
#[must_use]
pub fn preview_path_in(directory: &Path, name: &str) -> Option<PathBuf> {
    // PNG, because a preview is drawn on a transparent ground so it can sit
    // on either theme's card; JPEG would flatten that to black.
    is_safe_name(name).then(|| directory.join("previews").join(format!("{name}.png")))
}

/// Drop a scene's cached preview, which a new version has just made a picture
/// of something older. Says whether there was one.
pub fn forget_preview_in(directory: &Path, name: &str) -> bool {
    preview_path_in(directory, name).is_some_and(|at| std::fs::remove_file(at).is_ok())
}

/// The file each version number resolves to, lowest first.
fn version_paths(directory: &Path, name: &str) -> BTreeMap<u32, PathBuf> {
    let mut found = BTreeMap::new();
    if !is_safe_name(name) {
        return found;
    }
    let flat = directory.join(format!("{name}.rvs"));
    if flat.is_file() {
        found.insert(1, flat);
    }
    let Ok(entries) = std::fs::read_dir(directory.join(VERSIONS).join(name)) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "rvs") {
            continue;
        }
        // The number is the file's whole stem. A stem that is not one names
        // no version and is left alone rather than guessed at.
        let number = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| stem.parse::<u32>().ok());
        if let Some(number) = number.filter(|number| *number > 0) {
            found.insert(number, path);
        }
    }
    found
}

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
        let mut names = std::collections::BTreeSet::new();
        for entry in std::fs::read_dir(&self.directory)
            .into_iter()
            .flatten()
            .flatten()
        {
            let path = entry.path();
            if path.extension().is_none_or(|extension| extension != "rvs") {
                continue;
            }
            if let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) {
                names.insert(name.to_owned());
            }
        }
        // A scene converted since there were versions has no file of its own
        // in the directory, only a directory of versions under `versions/`.
        for entry in std::fs::read_dir(self.directory.join(VERSIONS))
            .into_iter()
            .flatten()
            .flatten()
        {
            if let Some(name) = entry.file_name().to_str()
                && entry.path().is_dir()
            {
                names.insert(name.to_owned());
            }
        }

        let mut listed = Vec::new();
        for name in names {
            let versions = self.versions(&name);
            // A directory left behind with nothing in it is not a scene.
            let Some(current) = versions.last().copied() else {
                continue;
            };
            listed.push(Listed {
                // The newest version is what the name resolves to, so it is
                // its size and its time that describe the scene.
                bytes: current.bytes,
                // Seconds since the epoch, which is what a page formats in the
                // reader's own locale. A clock that predates it is reported as
                // unknown rather than as a negative time.
                modified: current.modified,
                version: current.number,
                versions: versions.len(),
                has_preview: self.preview_path(&name).is_some_and(|at| at.is_file()),
                sources: self.sources(&name),
                name,
            });
        }
        listed.sort_by(|left, right| left.name.cmp(&right.name));
        listed
    }

    /// Every version of one scene, oldest first.
    #[must_use]
    pub fn versions(&self, name: &str) -> Vec<Version> {
        versions_in(&self.directory, name)
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
        let held = version_paths(&self.directory, name);
        if held.is_empty() {
            return Err(SceneError::NotFound);
        }
        let mut removed = Removed::default();
        // Every version, not only the one the name resolves to: this is the
        // request to forget the model, and leaving its history behind would
        // have a deleted scene come back in the next listing.
        for path in held.values() {
            let bytes = std::fs::metadata(path).map_or(0, |at| at.len());
            std::fs::remove_file(path).map_err(SceneError::Io)?;
            removed.bytes += bytes;
            removed.versions += 1;
        }
        // The directory the versions were in, with the high-water mark that
        // was kept beside them: the model is being forgotten, so the
        // numbering it had goes with it. It is not an error for the directory
        // to be missing - a scene written before there were versions never
        // had one.
        let _ = std::fs::remove_dir_all(self.directory.join(VERSIONS).join(name));
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

    /// Delete one version of a scene, leaving the rest of its history alone.
    ///
    /// Deleting the newest is allowed and makes the one before it current
    /// again, which is the way back from a conversion that came out worse
    /// than the one it followed. Deleting the last version left is not: that
    /// is deleting the scene, and [`Scenes::remove`] is the request that says
    /// so and clears the preview and the upload with it.
    ///
    /// # Errors
    ///
    /// [`SceneError::NotFound`] where the scene has no such version,
    /// [`SceneError::OnlyVersion`] where it is the only one, and
    /// [`SceneError::Io`] where the file cannot be removed.
    pub fn remove_version(&self, name: &str, version: u32) -> Result<Removed, SceneError> {
        let held = version_paths(&self.directory, name);
        let path = held.get(&version).ok_or(SceneError::NotFound)?;
        if held.len() == 1 {
            return Err(SceneError::OnlyVersion);
        }
        let bytes = std::fs::metadata(path).map_or(0, |at| at.len());
        std::fs::remove_file(path).map_err(SceneError::Io)?;
        let mut removed = Removed {
            bytes,
            versions: 1,
            ..Removed::default()
        };
        // The preview is a picture of whatever was newest when it was drawn.
        // Removing the newest version makes it a picture of a version that is
        // gone, so it goes too; removing an older one leaves it right.
        if held.keys().next_back() == Some(&version) {
            removed.preview = forget_preview_in(&self.directory, name);
        }
        Ok(removed)
    }

    /// Where a scene's cached preview image lives. Previews sit in their own
    /// directory so the listing, which looks for `.rvs`, never sees them.
    #[must_use]
    pub fn preview_path(&self, name: &str) -> Option<PathBuf> {
        preview_path_in(&self.directory, name)
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

    /// The file one name resolves to - its newest version - or `None` if the
    /// name names no scene this directory holds.
    #[must_use]
    pub fn path(&self, name: &str) -> Option<PathBuf> {
        self.resolve(name, None).map(|(_, path)| path)
    }

    /// One named version, or the newest where none is named, with the number
    /// it turned out to be.
    ///
    /// The number is returned rather than assumed by the caller, so that a
    /// reply can state which version it is answering with: a viewer reads
    /// this format over several range requests, and a conversion landing
    /// between two of them would otherwise have it stitch one version's
    /// trailer to another's chunks without either side noticing.
    #[must_use]
    pub fn resolve(&self, name: &str, version: Option<u32>) -> Option<(u32, PathBuf)> {
        let mut found = version_paths(&self.directory, name);
        match version {
            Some(number) => found.remove(&number).map(|path| (number, path)),
            None => found.pop_last(),
        }
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
        version: Option<u32>,
        wanted: Option<(Option<u64>, Option<u64>)>,
    ) -> Result<Served, SceneError> {
        let (version, path) = self.resolve(name, version).ok_or(SceneError::NotFound)?;
        let mut file = File::open(&path).map_err(SceneError::Io)?;
        let total = file.metadata().map_err(SceneError::Io)?.len();
        let range = resolve_range(wanted, total).ok_or(SceneError::NotSatisfiable(total))?;
        file.seek(SeekFrom::Start(range.start))
            .map_err(SceneError::Io)?;
        let length = usize::try_from(range.length()).unwrap_or(usize::MAX);
        let mut bytes = vec![0_u8; length];
        file.read_exact(&mut bytes).map_err(SceneError::Io)?;
        Ok(Served {
            bytes,
            range,
            total,
            version,
        })
    }
}

/// One scene as a listing shows it. The size and the time are the newest
/// version's, which is what the name resolves to.
pub struct Listed {
    pub name: String,
    pub bytes: u64,
    /// Seconds since the Unix epoch, where the filesystem states one.
    pub modified: Option<u64>,
    /// The version the name resolves to today.
    pub version: u32,
    /// How many versions are kept, this one included.
    pub versions: usize,
    pub has_preview: bool,
    /// `(extension, bytes)` for every upload sharing this scene's name.
    pub sources: Vec<(String, u64)>,
}

/// One answered read: the bytes, the range they cover, the length of the file
/// they came out of, and which version that was.
pub struct Served {
    pub bytes: Vec<u8>,
    pub range: Range,
    pub total: u64,
    pub version: u32,
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
    /// How many versions were deleted.
    pub versions: usize,
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
    /// The version named is the only one the scene has, so deleting it would
    /// delete the scene rather than a version of it.
    OnlyVersion,
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
fn resolve_range(wanted: Option<(Option<u64>, Option<u64>)>, total: u64) -> Option<Range> {
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
    use super::{
        Range, SceneError, Scenes, Version, is_safe_name, next_version_path, parse_range,
        resolve_range,
    };
    use std::path::PathBuf;

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
            resolve_range(Some((None, Some(24))), 1000),
            Some(Range {
                start: 976,
                end: 999
            })
        );
        // A suffix longer than the file is the whole file, not an error.
        assert_eq!(
            resolve_range(Some((None, Some(4000))), 1000),
            Some(Range { start: 0, end: 999 })
        );
    }

    #[test]
    fn clamps_an_end_past_the_last_byte_and_refuses_a_start_past_it() {
        assert_eq!(
            resolve_range(Some((Some(990), Some(99_999))), 1000),
            Some(Range {
                start: 990,
                end: 999
            })
        );
        assert_eq!(resolve_range(Some((Some(1000), None)), 1000), None);
        assert_eq!(resolve_range(None, 0), None);
    }

    /// A scene already on disk when versions arrived is version 1, and the
    /// next conversion of it is version 2 - so its history starts where it
    /// actually started rather than at the first conversion after the change.
    #[test]
    fn a_scene_from_before_versions_is_the_first_of_them() {
        let root = temporary("from-before");
        std::fs::write(root.join("model.rvs"), b"first-bytes").unwrap();
        let scenes = Scenes::new(root.clone());
        assert_eq!(
            scenes.versions("model"),
            vec![Version {
                number: 1,
                bytes: 11,
                modified: scenes.versions("model")[0].modified,
            }]
        );

        let next = next_version_path(&root, "model").unwrap();
        assert_eq!(next, root.join("versions/model/2.rvs"));
        std::fs::write(&next, b"second").unwrap();

        // The name resolves to the newest, and the one before it is still
        // there to be asked for.
        assert_eq!(scenes.path("model"), Some(next.clone()));
        assert_eq!(
            scenes.resolve("model", Some(1)),
            Some((1, root.join("model.rvs")))
        );
        assert_eq!(scenes.resolve("model", Some(3)), None);
        let numbers: Vec<u32> = scenes
            .versions("model")
            .iter()
            .map(|version| version.number)
            .collect();
        assert_eq!(numbers, vec![1, 2]);

        // And the listing describes the newest, while saying how many there
        // are behind it.
        let listed = scenes.listing();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "model");
        assert_eq!(listed[0].version, 2);
        assert_eq!(listed[0].versions, 2);
        assert_eq!(listed[0].bytes, 6);
        std::fs::remove_dir_all(&root).ok();
    }

    /// A version number is never reused, so a client that pinned one keeps
    /// reading the file it pinned even after the version above it is gone.
    #[test]
    fn a_deleted_version_does_not_free_its_number() {
        let root = temporary("numbering");
        let scenes = Scenes::new(root.clone());
        for bytes in [b"one".as_slice(), b"two", b"three"] {
            let at = next_version_path(&root, "model").unwrap();
            std::fs::write(at, bytes).unwrap();
        }
        assert_eq!(
            scenes.path("model"),
            Some(root.join("versions/model/3.rvs"))
        );

        let removed = scenes.remove_version("model", 3).unwrap();
        assert_eq!((removed.versions, removed.bytes), (1, 5));
        // Two is current again, and the next conversion is four rather than
        // three a second time.
        assert_eq!(
            scenes.path("model"),
            Some(root.join("versions/model/2.rvs"))
        );
        assert_eq!(
            next_version_path(&root, "model").unwrap(),
            root.join("versions/model/4.rvs")
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// Deleting the last version left is deleting the scene, and is refused
    /// as such rather than quietly leaving a name with nothing behind it.
    #[test]
    fn the_only_version_is_the_scene_and_is_not_deleted_as_a_version() {
        let root = temporary("only-version");
        let scenes = Scenes::new(root.clone());
        let at = next_version_path(&root, "model").unwrap();
        std::fs::write(at, b"only").unwrap();

        assert!(matches!(
            scenes.remove_version("model", 1),
            Err(SceneError::OnlyVersion)
        ));
        assert!(matches!(
            scenes.remove_version("model", 9),
            Err(SceneError::NotFound)
        ));

        // The request that does mean it takes the version and the directory
        // that held it, and the scene stops being listed.
        let removed = scenes.remove("model", None).unwrap();
        assert!(removed.scene);
        assert_eq!((removed.versions, removed.bytes), (1, 4));
        assert!(!root.join("versions/model").exists());
        assert!(scenes.listing().is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    /// A read answers out of the version it was asked for, and says which one
    /// that was: the viewer takes the trailer, then the manifest, then chunks,
    /// and all three have to come out of one file.
    #[test]
    fn a_read_answers_from_one_version_and_names_it() {
        let root = temporary("pinned-read");
        let scenes = Scenes::new(root.clone());
        for bytes in [b"aaaaaaaa".as_slice(), b"bbbbbbbb"] {
            let at = next_version_path(&root, "model").unwrap();
            std::fs::write(at, bytes).unwrap();
        }
        let newest = scenes.read("model", None, None).unwrap();
        assert_eq!(
            (newest.version, newest.bytes.as_slice()),
            (2, b"bbbbbbbb".as_slice())
        );

        let pinned = scenes
            .read("model", Some(1), Some((None, Some(3))))
            .unwrap();
        assert_eq!(pinned.version, 1);
        assert_eq!(pinned.bytes, b"aaa");
        assert_eq!(pinned.total, 8);

        assert!(matches!(
            scenes.read("model", Some(7), None),
            Err(SceneError::NotFound)
        ));
        std::fs::remove_dir_all(&root).ok();
    }

    /// One directory per test, since they run in one process at once.
    fn temporary(test: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("groma-scenes-{}-{test}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn refuses_a_name_that_tries_to_leave_the_scene_directory() {
        for name in ["../secret", "a/b", "..", ".hidden", ""] {
            assert!(!is_safe_name(name), "{name} was not refused");
        }
        assert!(is_safe_name("small"));
    }
}
