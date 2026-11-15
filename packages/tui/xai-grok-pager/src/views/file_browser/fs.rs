/*
Purpose: Read one directory or a bounded text preview outside the UI thread.
Input: A local path and the current visibility option.
Output: Directory entries or source lines with syntax colors.
*/

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};

use super::super::file_search::line_viewer::{SourceLine, build_source_lines};

const MAX_PREVIEW_BYTES: u64 = 256 * 1024;
const MAX_PREVIEW_LINES: usize = 4000;
const MAX_LINE_BYTES: usize = 4096;

#[derive(Clone, Debug)]
pub(super) struct Entry {
    pub path: PathBuf,
    pub is_directory: bool,
    pub is_symlink: bool,
}

pub(super) enum Request {
    Directory {
        path: PathBuf,
        show_all: bool,
        epoch: u64,
    },
    Preview {
        path: PathBuf,
        generation: u64,
    },
}

pub(super) enum Response {
    Directory {
        path: PathBuf,
        epoch: u64,
        result: Result<Vec<Entry>, String>,
    },
    Preview {
        path: PathBuf,
        generation: u64,
        result: Result<TextPreview, String>,
    },
}

pub(super) struct TextPreview {
    pub lines: Vec<SourceLine>,
    pub is_truncated: bool,
}

/*
Purpose: Process file requests on one worker thread.
Input: The request receiver and the result sender.
Output: One response for each request, until the view closes its channels.
*/
pub(super) fn run(requests: Receiver<Request>, responses: Sender<Response>) {
    for request in requests {
        let response = match request {
            Request::Directory {
                path,
                show_all,
                epoch,
            } => {
                let result = read_directory(&path, show_all).map_err(|error| error.to_string());
                Response::Directory {
                    path,
                    epoch,
                    result,
                }
            }
            Request::Preview { path, generation } => {
                let result = read_preview(&path).map_err(|error| error.to_string());
                Response::Preview {
                    path,
                    generation,
                    result,
                }
            }
        };
        if responses.send(response).is_err() {
            return;
        }
    }
}

/*
Purpose: List direct children, with directories first and names in lexical order.
Input: A directory path and the option to include hidden and ignored files.
Output: The direct children. Symlink directories remain leaves to prevent cycles.
*/
fn read_directory(path: &Path, show_all: bool) -> io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    let walker = ignore::WalkBuilder::new(path)
        .max_depth(Some(1))
        .follow_links(false)
        .hidden(!show_all)
        .ignore(!show_all)
        .git_ignore(!show_all)
        .git_global(!show_all)
        .git_exclude(!show_all)
        .build();
    for entry in walker {
        let entry = entry.map_err(io::Error::other)?;
        if entry.depth() == 0 {
            continue;
        }
        let file_type = entry
            .file_type()
            .ok_or_else(|| io::Error::other("Unknown file type"))?;
        entries.push(Entry {
            path: entry.into_path(),
            is_directory: file_type.is_dir(),
            is_symlink: file_type.is_symlink(),
        });
    }
    entries.sort_by(|left, right| {
        right
            .is_directory
            .cmp(&left.is_directory)
            .then_with(|| left.path.file_name().cmp(&right.path.file_name()))
    });
    Ok(entries)
}

/*
Purpose: Read a safe text prefix and apply syntax colors to it.
Input: The selected file path.
Output: Source lines, or an error for a binary file or a special file.
Note: The byte, line, and line-length limits bound the syntax parser work.
*/
fn read_preview(path: &Path) -> io::Result<TextPreview> {
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(io::Error::other(
            "Preview is available for regular text files only",
        ));
    }
    let mut options = File::options();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // A file can become a FIFO after the metadata check.
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other(
            "Preview is available for regular text files only",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_PREVIEW_BYTES + 1).read_to_end(&mut bytes)?;
    let mut is_truncated = bytes.len() as u64 > MAX_PREVIEW_BYTES;
    bytes.truncate(MAX_PREVIEW_BYTES as usize);
    if bytes.contains(&0) {
        return Err(io::Error::other("Binary file — use Open in Default App"));
    }
    let content = match std::str::from_utf8(&bytes) {
        Ok(content) => content,
        Err(error) if is_truncated && error.error_len().is_none() => {
            std::str::from_utf8(&bytes[..error.valid_up_to()]).expect("valid UTF-8 prefix")
        }
        Err(_) => return Err(io::Error::other("Non-UTF-8 file — use Open in Default App")),
    };
    let mut bounded = String::new();
    for (index, line) in content.lines().enumerate() {
        if index >= MAX_PREVIEW_LINES {
            is_truncated = true;
            break;
        }
        let mut end = line.len().min(MAX_LINE_BYTES);
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        // Render controls as visible spaces, never as terminal escape sequences.
        for character in line[..end].chars() {
            bounded.push(if character.is_control() && character != '\t' {
                ' '
            } else {
                character
            });
        }
        if end < line.len() {
            bounded.push_str(" …");
            is_truncated = true;
        }
        bounded.push('\n');
    }
    Ok(TextPreview {
        lines: build_source_lines(path, &bounded),
        is_truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_is_shallow_sorted_and_filters_ignored_files() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(root.path().join(".gitignore"), "ignored.txt\n").unwrap();
        for file in ["z.txt", "a.txt", "ignored.txt", ".hidden"] {
            std::fs::write(root.path().join(file), "text").unwrap();
        }
        let entries = read_directory(root.path(), false).unwrap();
        let names: Vec<_> = entries
            .iter()
            .map(|entry| entry.path.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(names, ["src", "a.txt", "z.txt"]);
        assert_eq!(read_directory(root.path(), true).unwrap().len(), 7);
    }

    #[test]
    fn preview_handles_empty_binary_large_and_invalid_utf8_files() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("test.txt");
        std::fs::write(&path, "").unwrap();
        assert!(!read_preview(&path).unwrap().is_truncated);
        std::fs::write(&path, [0, 1, 2]).unwrap();
        assert!(
            read_preview(&path)
                .err()
                .unwrap()
                .to_string()
                .contains("Binary")
        );
        std::fs::write(&path, [0xff]).unwrap();
        assert!(read_preview(&path).is_err());
        std::fs::write(&path, "a".repeat(MAX_PREVIEW_BYTES as usize + 1)).unwrap();
        assert!(read_preview(&path).unwrap().is_truncated);
        assert!(read_preview(root.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn directory_symlinks_are_leaves() {
        let root = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(root.path(), root.path().join("loop")).unwrap();
        let entries = read_directory(root.path(), false).unwrap();
        assert!(entries[0].is_symlink);
        assert!(!entries[0].is_directory);
    }
}
