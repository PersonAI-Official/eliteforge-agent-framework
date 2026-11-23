//! Strict, context-based text patches. No shell, Git, or filesystem dependency.
#[derive(Debug)]
pub enum FilePatch {
    Add {
        path: String,
        content: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        destination: Option<String>,
        hunks: Vec<TextHunk>,
    },
}
#[derive(Debug)]
pub struct TextHunk {
    pub(crate) anchor: Option<String>,
    pub(crate) before: Vec<String>,
    pub(crate) after: Vec<String>,
    eof: bool,
}

pub fn parse_text_patch(patch: &str) -> Result<Vec<FilePatch>, String> {
    if patch.len() > 4 * 1024 * 1024 {
        return Err("patch exceeds 4 MiB".into());
    }
    let lines: Vec<_> = patch.lines().collect();
    if lines.first() != Some(&"*** Begin Patch") || lines.last() != Some(&"*** End Patch") {
        return Err("patch requires *** Begin Patch and *** End Patch markers".into());
    }
    let mut index = 1;
    let mut files = Vec::new();
    while index < lines.len() - 1 {
        let header = lines[index];
        index += 1;
        if let Some(path) = header.strip_prefix("*** Add File: ") {
            let mut content = String::new();
            while index < lines.len() - 1 && !lines[index].starts_with("*** ") {
                let line = lines[index]
                    .strip_prefix('+')
                    .ok_or("added lines must start with +")?;
                content.push_str(line);
                content.push('\n');
                index += 1;
            }
            files.push(FilePatch::Add {
                path: path.into(),
                content,
            });
        } else if let Some(path) = header.strip_prefix("*** Delete File: ") {
            files.push(FilePatch::Delete { path: path.into() });
        } else if let Some(path) = header.strip_prefix("*** Update File: ") {
            let destination = lines
                .get(index)
                .and_then(|line| line.strip_prefix("*** Move to: "))
                .map(str::to_owned);
            if destination.is_some() {
                index += 1;
            }
            let mut hunks = Vec::new();
            while index < lines.len() - 1 && !lines[index].starts_with("*** ") {
                let marker = lines[index];
                let anchor = if marker == "@@" {
                    None
                } else {
                    Some(
                        marker
                            .strip_prefix("@@ ")
                            .ok_or("update hunks must begin with @@ or @@ anchor")?
                            .to_string(),
                    )
                };
                index += 1;
                let mut hunk = TextHunk {
                    anchor,
                    before: Vec::new(),
                    after: Vec::new(),
                    eof: false,
                };
                let mut changed = false;
                while index < lines.len() - 1
                    && !lines[index].starts_with("@@")
                    && !lines[index].starts_with("*** ")
                {
                    let line = lines[index];
                    index += 1;
                    match line.as_bytes().first() {
                        Some(b' ') => {
                            hunk.before.push(line[1..].into());
                            hunk.after.push(line[1..].into());
                        }
                        Some(b'-') => {
                            hunk.before.push(line[1..].into());
                            changed = true;
                        }
                        Some(b'+') => {
                            hunk.after.push(line[1..].into());
                            changed = true;
                        }
                        _ => return Err("hunk lines require a space, + or - prefix".into()),
                    }
                }
                if lines.get(index) == Some(&"*** End of File") {
                    hunk.eof = true;
                    index += 1;
                }
                if !changed {
                    return Err("hunk contains no changes".into());
                }
                hunks.push(hunk);
            }
            if hunks.is_empty() && destination.is_none() {
                return Err("update requires a hunk or a move destination".into());
            }
            files.push(FilePatch::Update {
                path: path.into(),
                destination,
                hunks,
            });
        } else if header.starts_with("*** Move File:") {
            return Err("invalid move header; use *** Update File: old/path followed by *** Move to: new/path".into());
        } else {
            return Err(format!("invalid patch header: {header}"));
        }
        if files.len() > 100 {
            return Err("patch exceeds 100 files".into());
        }
    }
    if files.is_empty() {
        return Err("patch contains no file operations".into());
    }
    Ok(files)
}

pub fn apply_text_hunks(source: &str, hunks: &[TextHunk]) -> Result<String, String> {
    if hunks.is_empty() {
        return Ok(source.into());
    }
    let crlf = source.contains("\r\n");
    if crlf && source.replace("\r\n", "").contains('\n') {
        return Err("mixed line endings; normalize the file before patching".into());
    }
    let ending = if crlf { "\r\n" } else { "\n" };
    let trailing_newline = source.ends_with('\n');
    let mut lines: Vec<String> = source.lines().map(str::to_owned).collect();
    let mut cursor = 0;
    for (number, hunk) in hunks.iter().enumerate() {
        if let Some(anchor) = &hunk.anchor {
            let matches: Vec<_> = (cursor..lines.len())
                .filter(|&i| &lines[i] == anchor)
                .collect();
            if matches.len() != 1 {
                return Err(format!(
                    "hunk {}: anchor has {} matches; provide unique context",
                    number + 1,
                    matches.len()
                ));
            }
            cursor = matches[0] + 1;
        }
        let candidates = if hunk.before.is_empty() {
            if lines.is_empty() || hunk.eof {
                vec![lines.len()]
            } else {
                return Err(format!(
                    "hunk {}: insertion requires context or *** End of File",
                    number + 1
                ));
            }
        } else {
            (cursor..=lines.len())
                .filter(|&i| {
                    let end = i + hunk.before.len();
                    end <= lines.len()
                        && lines[i..end] == hunk.before
                        && (!hunk.eof || end == lines.len())
                })
                .collect::<Vec<_>>()
        };
        if candidates.len() != 1 {
            return Err(format!(
                "hunk {}: context has {} matches; re-read the file and provide unique exact context",
                number + 1,
                candidates.len()
            ));
        }
        let start = candidates[0];
        lines.splice(start..start + hunk.before.len(), hunk.after.clone());
        cursor = start + hunk.after.len();
    }
    let mut result = lines.join(ending);
    if !lines.is_empty() && (trailing_newline || source.is_empty()) {
        result.push_str(ending);
    }
    Ok(result)
}
