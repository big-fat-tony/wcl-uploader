//! Chunked combat-log reading and payload compression.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde::Serialize;

/// Default part limits, matching the official client.
pub const MAX_LINES: usize = 5_000;
pub const MAX_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileInfo {
    pub file_path: String,
    pub file_name: String,
    pub size: u64,
    pub last_modified_ms: i64,
}

impl FileInfo {
    pub fn path(&self) -> &Path {
        Path::new(&self.file_path)
    }
}

pub fn file_info(path: &Path) -> io::Result<FileInfo> {
    let meta = std::fs::metadata(path)?;
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    Ok(FileInfo {
        file_path: path.to_string_lossy().into_owned(),
        file_name: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        size: meta.len(),
        last_modified_ms: modified,
    })
}

/// A slice of the log file: complete lines only, plus where reading stopped.
#[derive(Debug, Clone)]
pub struct FilePart {
    pub lines: Vec<String>,
    pub starting_position: u64,
    pub current_position: u64,
    pub end_of_file: bool,
}

/// Read up to `max_lines` complete lines (or `max_bytes`) starting at byte
/// offset `position`.
///
/// A trailing line without a newline is only returned when `include_partial_tail`
/// is set (a finished log file); while tailing a live log it is left for the
/// next read so the parser never sees a half-written line.
pub fn read_file_part(
    path: &Path,
    position: u64,
    max_lines: usize,
    max_bytes: u64,
    include_partial_tail: bool,
) -> io::Result<FilePart> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let start = position.min(len);
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::with_capacity(256 * 1024, file);

    let mut lines = Vec::with_capacity(max_lines.min(5_000));
    let mut pos = start;
    let mut buf = Vec::new();
    let mut bytes_read = 0u64;

    while lines.len() < max_lines && bytes_read < max_bytes {
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf)?;
        if n == 0 {
            break;
        }
        let complete = buf.ends_with(b"\n");
        if !complete && !(include_partial_tail && pos + n as u64 >= len) {
            // Half-written tail line; leave it for the next read.
            break;
        }
        bytes_read += n as u64;
        pos += n as u64;

        let mut slice: &[u8] = &buf;
        if pos - n as u64 == 0 && slice.starts_with(&[0xEF, 0xBB, 0xBF]) {
            slice = &slice[3..];
        }
        let mut line = String::from_utf8_lossy(slice).into_owned();
        while line.ends_with('\n') || line.ends_with('\r') {
            line.pop();
        }
        lines.push(line);
    }

    Ok(FilePart {
        lines,
        starting_position: start,
        current_position: pos,
        end_of_file: pos >= len,
    })
}

/// Newest file in `dir` whose name matches `pattern`, ignoring files last
/// modified more than `max_age_ms` ago (when `max_age_ms > 0`).
pub fn latest_file_in_dir(dir: &Path, pattern: &Regex, max_age_ms: i64) -> Option<FileInfo> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| pattern.is_match(&e.file_name().to_string_lossy()))
        .filter_map(|e| file_info(&e.path()).ok())
        .filter(|f| max_age_ms <= 0 || now - f.last_modified_ms <= max_age_ms)
        .max_by_key(|f| f.last_modified_ms)
}

/// Any line containing `COMBAT_LOG_VERSION` is a header line the parser needs
/// before it can interpret the events that follow.
pub fn is_header_line(line: &str) -> bool {
    line.contains("COMBAT_LOG_VERSION")
}

/// Collect header lines found in `[0, position)` so a tail read can be primed.
pub fn header_lines_up_to(path: &Path, position: u64) -> io::Result<Vec<String>> {
    let mut headers = Vec::new();
    let mut pos = 0u64;
    while pos < position {
        let part = read_file_part(path, pos, MAX_LINES, MAX_BYTES.min(position - pos).max(1), false)?;
        headers.extend(part.lines.iter().filter(|l| is_header_line(l)).cloned());
        if part.end_of_file || part.current_position == pos {
            break;
        }
        pos = part.current_position;
    }
    Ok(headers)
}

/// Does the line start with a WoW combat log timestamp (`M/D HH:MM:SS.mmm`,
/// optionally with a year and zone offset)?
pub fn has_timestamp(line: &str) -> bool {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\d{1,2}/\d{1,2}(/\d{4})?\s+\d{1,2}:\d{2}:\d{2}\.\d{3}").unwrap())
        .is_match(line)
}

/// Compress `text` as the single entry `log.txt` of a DEFLATE(9) zip, the
/// payload format `add-report-segment` / `set-report-master-table` expect.
pub fn zip_text(text: &str) -> io::Result<Vec<u8>> {
    let mut cursor = io::Cursor::new(Vec::with_capacity(text.len() / 4));
    {
        let mut zip = zip::ZipWriter::new(&mut cursor);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .compression_level(Some(9));
        zip.start_file("log.txt", options)?;
        zip.write_all(text.as_bytes())?;
        zip.finish()?;
    }
    Ok(cursor.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_file(contents: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("logs-uploader-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("log-{}.txt", rand_suffix()));
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn rand_suffix() -> u128 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    }

    #[test]
    fn reads_complete_lines_and_leaves_partial_tail() {
        let path = temp_file(b"a\r\nb\nc");
        let part = read_file_part(&path, 0, 100, 1 << 20, false).unwrap();
        assert_eq!(part.lines, vec!["a", "b"]);
        assert_eq!(part.current_position, 5);
        assert!(!part.end_of_file);

        let part = read_file_part(&path, 0, 100, 1 << 20, true).unwrap();
        assert_eq!(part.lines, vec!["a", "b", "c"]);
        assert!(part.end_of_file);
    }

    #[test]
    fn respects_line_limit_and_resumes() {
        let path = temp_file(b"1\n2\n3\n");
        let first = read_file_part(&path, 0, 2, 1 << 20, true).unwrap();
        assert_eq!(first.lines, vec!["1", "2"]);
        assert!(!first.end_of_file);
        let second = read_file_part(&path, first.current_position, 2, 1 << 20, true).unwrap();
        assert_eq!(second.lines, vec!["3"]);
        assert!(second.end_of_file);
    }

    #[test]
    fn strips_bom() {
        let path = temp_file(b"\xEF\xBB\xBFCOMBAT_LOG_VERSION,x\n");
        let part = read_file_part(&path, 0, 10, 1 << 20, true).unwrap();
        assert_eq!(part.lines, vec!["COMBAT_LOG_VERSION,x"]);
    }

    #[test]
    fn timestamp_detection() {
        assert!(has_timestamp("9/14 20:01:05.123  COMBAT_LOG_VERSION,22"));
        assert!(has_timestamp("9/14/2026 20:01:05.123+2  ENCOUNTER_START,1"));
        assert!(!has_timestamp("garbage"));
    }

    #[test]
    fn zip_roundtrip() {
        let bytes = zip_text("hello\n").unwrap();
        let mut archive = zip::ZipArchive::new(io::Cursor::new(bytes)).unwrap();
        let mut entry = archive.by_name("log.txt").unwrap();
        let mut s = String::new();
        io::Read::read_to_string(&mut entry, &mut s).unwrap();
        assert_eq!(s, "hello\n");
    }
}
