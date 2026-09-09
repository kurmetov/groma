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
        let mut names = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.directory) else {
            return names;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|extension| extension == "rvs") {
                if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
                    names.push(stem.to_owned());
                }
            }
        }
        names.sort();
        names
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
    use super::{Range, is_safe_name, parse_range, resolve};

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
