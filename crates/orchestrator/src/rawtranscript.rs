//! Lossless, bounded transcript reads (STUDIO-1155; no Go counterpart).

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::{ffi::OsStrExt, fs::OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct RawLine {
    pub offset: u64,
    /// Includes the original line ending, if any.
    pub text: String,
}

#[derive(Debug, Default, Serialize)]
pub struct RawPage {
    pub run_id: i64,
    pub size_bytes: u64,
    pub lines: Vec<RawLine>,
    pub prev_cursor: Option<u64>,
    pub next_cursor: Option<u64>,
    pub at_start: bool,
    pub at_end: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub missing: bool,
}

#[derive(Clone, Copy)]
pub enum Direction {
    Forward,
    Backward,
}

const MAX_LINES: usize = 500;
const MAX_BYTES: u64 = 4 << 20;

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("transcript path refused")]
    Refused,
    #[error("transcript read failed")]
    Io(#[from] io::Error),
}

/// Only the persisted row may supply `path`. A pruned file is None; an escape is refused,
/// including symlinks. Open each canonical component without following replacement symlinks.
pub fn open_file(root: &Path, path: &Path, stderr: bool) -> Result<Option<File>, OpenError> {
    if path.as_os_str().is_empty() {
        return Ok(None);
    }
    if root.as_os_str().is_empty() {
        return Err(OpenError::Refused);
    }
    let root = canonical_or_missing(root)?;
    let transcript = canonical_or_missing(path)?;
    if !transcript.starts_with(&root) || transcript == root {
        return Err(OpenError::Refused);
    }
    let path = if stderr {
        transcript.with_extension("stderr.log")
    } else {
        transcript
    };
    let path = canonical_or_missing(&path)?;
    if !path.starts_with(&root) || path == root {
        return Err(OpenError::Refused);
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open("/")?;
    let parts: Vec<_> = path
        .components()
        .filter(|p| *p != Component::RootDir)
        .collect();
    for (index, part) in parts.iter().enumerate() {
        let Component::Normal(name) = part else {
            return Err(OpenError::Refused);
        };
        let name = std::ffi::CString::new(name.as_bytes()).map_err(|_| OpenError::Refused)?;
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | if index + 1 < parts.len() {
                libc::O_DIRECTORY
            } else {
                0
            };
        // SAFETY: the parent fd is owned and name is NUL terminated. Take sole ownership of
        // each newly opened fd immediately; dropping the previous parent closes it.
        let fd = unsafe { libc::openat(file.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::NotFound {
                Ok(None)
            } else {
                Err(OpenError::Refused)
            };
        }
        // SAFETY: openat returned a newly owned descriptor.
        file = unsafe { File::from_raw_fd(fd) };
    }
    if !file.metadata()?.is_file() {
        return Err(OpenError::Refused);
    }
    Ok(Some(file))
}

/// Resolve existing ancestors even for a pruned leaf; never treat a dangling symlink as missing.
fn canonical_or_missing(path: &Path) -> Result<PathBuf, OpenError> {
    match path.canonicalize() {
        Ok(path) => Ok(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if std::fs::symlink_metadata(path).is_ok() {
                return Err(OpenError::Refused);
            }
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .ok_or(OpenError::Refused)?;
            let name = path.file_name().ok_or(OpenError::Refused)?;
            Ok(canonical_or_missing(parent)?.join(name))
        }
        Err(_) => Err(OpenError::Refused),
    }
}

pub fn read_page(
    mut file: File,
    run_id: i64,
    cursor: Option<u64>,
    direction: Direction,
    limit: usize,
) -> io::Result<RawPage> {
    // Pin this read to the size observed on entry, even while the worker appends.
    let size = file.metadata()?.len();
    let limit = limit.clamp(1, MAX_LINES);
    let direction = if cursor.is_none() {
        Direction::Backward
    } else {
        direction
    };
    let cursor = cursor.unwrap_or(size);
    if cursor > size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cursor exceeds file size",
        ));
    }
    if cursor != 0 && cursor != size {
        file.seek(SeekFrom::Start(cursor - 1))?;
        let mut byte = [0];
        file.read_exact(&mut byte)?;
        if byte[0] != b'\n' {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cursor must be a line boundary",
            ));
        }
    }
    let (start, end) = match direction {
        Direction::Forward => (cursor, size),
        Direction::Backward => {
            let mut start = cursor;
            for _ in 0..limit {
                if start == 0 {
                    break;
                }
                let earlier = previous_line_start(&mut file, start)?;
                if start != cursor && cursor - earlier > MAX_BYTES {
                    break;
                }
                start = earlier;
                if cursor - start >= MAX_BYTES {
                    break;
                }
            }
            (start, cursor)
        }
    };
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::new(file.take(end - start));
    let mut lines = Vec::new();
    let mut offset = start;
    while lines.len() < limit {
        let mut bytes = Vec::new();
        if reader.read_until(b'\n', &mut bytes)? == 0 {
            break;
        }
        let len = bytes.len() as u64;
        if !lines.is_empty() && offset - start + len > MAX_BYTES {
            break;
        }
        let text =
            String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        lines.push(RawLine { offset, text });
        offset += len;
        if offset - start >= MAX_BYTES {
            break;
        }
    }
    Ok(RawPage {
        run_id,
        size_bytes: size,
        lines,
        prev_cursor: (start > 0).then_some(start),
        next_cursor: (offset < size).then_some(offset),
        at_start: start == 0,
        at_end: offset == size,
        missing: false,
    })
}

fn previous_line_start(file: &mut File, end: u64) -> io::Result<u64> {
    // Skip end-1, which may be the current line's terminating newline. Search bounded blocks,
    // not a byte-at-a-time seek and not the full 100 MB prefix of a tail request.
    let mut pos = end.saturating_sub(1);
    let mut buffer = [0; 64 << 10];
    while pos > 0 {
        let start = pos.saturating_sub(buffer.len() as u64);
        let count = (pos - start) as usize;
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buffer[..count])?;
        if let Some(index) = buffer[..count].iter().rposition(|b| *b == b'\n') {
            return Ok(start + index as u64 + 1);
        }
        pos = start;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::TempDir;

    fn fixture(dir: &TempDir, bytes: &[u8]) -> String {
        let path = dir.child("run.jsonl");
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn pages_reassemble_exact_bytes_in_both_directions() {
        let dir = TempDir::new();
        for bytes in [b"one\n\nthree\r\nfour".as_slice(), b"one\nlast\n", b""] {
            let path = fixture(&dir, bytes);
            let mut forward = Vec::new();
            let mut cursor = Some(0);
            loop {
                let page = read_page(File::open(&path).unwrap(), 1, cursor, Direction::Forward, 2)
                    .unwrap();
                for line in page.lines {
                    assert_eq!(line.offset as usize, forward.len());
                    forward.extend_from_slice(line.text.as_bytes());
                }
                if page.at_end {
                    break;
                }
                assert_ne!(page.next_cursor, cursor);
                cursor = page.next_cursor;
            }
            assert_eq!(forward, bytes);
            let mut backward = Vec::new();
            cursor = None;
            loop {
                let page = read_page(
                    File::open(&path).unwrap(),
                    1,
                    cursor,
                    Direction::Backward,
                    2,
                )
                .unwrap();
                let chunk: String = page.lines.iter().map(|line| line.text.as_str()).collect();
                backward.splice(0..0, chunk.bytes());
                if page.at_start {
                    break;
                }
                assert_ne!(page.prev_cursor, cursor);
                cursor = page.prev_cursor;
            }
            assert_eq!(backward, bytes);
        }
    }

    #[test]
    fn oversize_line_is_alone_and_whole_in_both_directions() {
        let dir = TempDir::new();
        let huge = "x".repeat(6 << 20) + "\n";
        let path = fixture(&dir, format!("first\n{huge}last").as_bytes());
        for (cursor, direction) in [
            (6, Direction::Forward),
            (6 + huge.len() as u64, Direction::Backward),
        ] {
            let page =
                read_page(File::open(&path).unwrap(), 1, Some(cursor), direction, 500).unwrap();
            assert_eq!(page.lines.len(), 1);
            assert_eq!(page.lines[0].offset, 6);
            assert_eq!(page.lines[0].text, huge);
        }
    }

    #[test]
    fn default_is_tail_and_limit_is_capped() {
        let dir = TempDir::new();
        let path = fixture(&dir, "line\n".repeat(501).as_bytes());
        let page = read_page(File::open(&path).unwrap(), 2, None, Direction::Forward, 999).unwrap();
        assert_eq!(page.lines.len(), 500);
        assert_eq!(page.lines[0].offset, 5);
        assert!(page.at_end);
        assert!(!page.at_start);
        assert_eq!(page.prev_cursor, Some(5));
    }

    #[test]
    fn byte_budget_stops_before_a_second_large_line() {
        let dir = TempDir::new();
        let line = "é".repeat((1 << 20) + 1) + "\r\n";
        let path = fixture(&dir, (line.clone() + &line).as_bytes());
        for (cursor, direction) in [(Some(0), Direction::Forward), (None, Direction::Backward)] {
            let page = read_page(File::open(&path).unwrap(), 1, cursor, direction, 500).unwrap();
            assert_eq!(page.lines.len(), 1);
            assert_eq!(page.lines[0].text, line);
            assert_eq!(page.size_bytes, 2 * line.len() as u64);
        }
    }

    #[test]
    fn open_refuses_escapes_symlinks_and_non_regular_files() {
        let dir = TempDir::new();
        let root = PathBuf::from(dir.child("logs"));
        std::fs::create_dir(&root).unwrap();
        let outside = PathBuf::from(fixture(&dir, b"private"));
        std::os::unix::fs::symlink(&outside, root.join("escape.jsonl")).unwrap();
        let path = root.join("safe.jsonl");
        std::fs::write(&path, "safe\n").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("safe.stderr.log")).unwrap();
        for (path, stderr) in [
            (&outside, false),
            (&root.join("escape.jsonl"), false),
            (&path, true),
            (&root, false),
        ] {
            assert!(matches!(
                open_file(&root, path, stderr),
                Err(OpenError::Refused)
            ));
        }
        assert!(
            open_file(&root, &root.join("pruned/run.jsonl"), false)
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            open_file(&root, Path::new(&dir.child("missing.jsonl")), false),
            Err(OpenError::Refused)
        ));
    }

    #[test]
    fn bad_cursors_and_invalid_utf8_are_errors_not_rewritten_lines() {
        let dir = TempDir::new();
        let path = fixture(&dir, b"abc\n");
        for cursor in [1, 5] {
            assert_eq!(
                read_page(
                    File::open(&path).unwrap(),
                    1,
                    Some(cursor),
                    Direction::Forward,
                    1
                )
                .unwrap_err()
                .kind(),
                io::ErrorKind::InvalidInput
            );
        }
        let path = fixture(&dir, &[0xff, b'\n']);
        assert_eq!(
            read_page(File::open(path).unwrap(), 1, None, Direction::Backward, 1)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }
}
