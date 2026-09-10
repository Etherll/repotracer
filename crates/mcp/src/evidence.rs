//! Bounded reads for the source excerpts embedded in an MCP handoff.
use super::{EvidenceBundle, EvidenceSpan, ValidatedCitation, MAX_HANDOFF_BYTES};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek};
use std::path::Path;

const MAX_SOURCE_SCAN_BYTES: u64 = 8 * 1024 * 1024;
const MAX_SOURCE_CAPTURE_BYTES: usize = 4 * MAX_HANDOFF_BYTES;

/// Charge actual file reads, including buffering, to one budget for the handoff.
fn with_reader<T>(
    file: &mut File,
    remaining: &mut u64,
    operation: impl FnOnce(&mut dyn BufRead) -> io::Result<T>,
) -> io::Result<T> {
    file.rewind()?;
    let limit = *remaining;
    let mut reader = BufReader::new(file.take(limit));
    let result = operation(&mut reader);
    *remaining -= limit - reader.get_ref().limit();
    result
}

/// Check only as far as the last requested line; do not read an uncited tail.
fn count_requested_lines(file: &mut File, end: u32, remaining: &mut u64) -> io::Result<u32> {
    with_reader(file, remaining, |reader| {
        let mut count = 0;
        while count < end && !reader.fill_buf()?.is_empty() {
            count += 1;
            if count < end {
                reader.skip_until(b'\n')?;
            }
        }
        Ok(count)
    })
}

/// Skip uncited lines without allocating them and cap every captured line.
fn read_source_span(
    file: &mut File,
    start: u32,
    end: u32,
    remaining_scan: &mut u64,
    capture_limit: usize,
) -> io::Result<Option<(String, bool)>> {
    if capture_limit == 0 || *remaining_scan == 0 {
        return Ok(None);
    }
    let (mut text, mut truncated, reached_end, line_terminated) =
        with_reader(file, remaining_scan, |reader| {
            for _ in 1..start {
                if reader.skip_until(b'\n')? == 0 {
                    return Ok((String::new(), false, false, false));
                }
            }
            let mut text = String::new();
            let mut bytes = Vec::new();
            let mut truncated = false;
            let mut reached_end = false;
            let mut line_terminated = false;
            for line in start..=end {
                let prefix = format!("{line}: ");
                let available = capture_limit.saturating_sub(text.len() + prefix.len() + 1);
                if available == 0 {
                    truncated = true;
                    break;
                }
                bytes.clear();
                let read = (&mut *reader)
                    .take(available as u64)
                    .read_until(b'\n', &mut bytes)?;
                if read == 0 {
                    break;
                }
                let has_newline = bytes.ends_with(b"\n");
                if !has_newline && read == available {
                    truncated = true;
                }
                let raw = if has_newline {
                    let raw = &bytes[..bytes.len() - 1];
                    raw.strip_suffix(b"\r").unwrap_or(raw)
                } else {
                    bytes.as_slice()
                };
                let source = match std::str::from_utf8(raw) {
                    Ok(source) => source,
                    Err(error) if error.error_len().is_none() && error.valid_up_to() > 0 => {
                        truncated = true;
                        std::str::from_utf8(&raw[..error.valid_up_to()])
                            .expect("validated UTF-8 prefix")
                    }
                    Err(error) => return Err(io::Error::new(io::ErrorKind::InvalidData, error)),
                };
                text.push_str(&prefix);
                text.push_str(source);
                text.push('\n');
                reached_end = line == end;
                line_terminated = has_newline;
                if !has_newline || truncated {
                    break;
                }
            }
            Ok((text, truncated, reached_end, line_terminated))
        })?;
    if *remaining_scan == 0 && (!reached_end || !line_terminated) {
        truncated = true;
    }
    if text.is_empty() || (!reached_end && !truncated) {
        return Ok(None);
    }
    while text.ends_with('\n') {
        text.pop();
    }
    Ok(Some((text, truncated)))
}

pub(super) fn evidence_excerpts(root: &Path, citations: &[ValidatedCitation]) -> EvidenceBundle {
    #[derive(Debug)]
    struct PathRanges {
        path: String,
        ranges: Vec<(u32, u32, usize)>,
    }

    let mut groups: Vec<PathRanges> = Vec::new();
    let mut bundle = EvidenceBundle::default();
    for citation in citations {
        if citation.start_line == 0 || citation.end_line < citation.start_line {
            bundle.omitted_citations += 1;
            bundle.omitted_spans += 1;
            continue;
        }
        let Some(group) = groups.iter_mut().find(|group| group.path == citation.path) else {
            groups.push(PathRanges {
                path: citation.path.clone(),
                ranges: vec![(citation.start_line, citation.end_line, 1)],
            });
            continue;
        };
        group
            .ranges
            .push((citation.start_line, citation.end_line, 1));
    }

    let mut remaining_scan = MAX_SOURCE_SCAN_BYTES;
    let mut remaining_capture = MAX_SOURCE_CAPTURE_BYTES;
    for group in groups {
        let Ok(path) = repotracer_repo_tools::resolve_in_root(root, &group.path) else {
            bundle.omitted_citations += group.ranges.iter().map(|range| range.2).sum::<usize>();
            bundle.omitted_spans += group.ranges.len();
            continue;
        };
        if !path.is_file() || remaining_scan == 0 || remaining_capture == 0 {
            bundle.omitted_citations += group.ranges.iter().map(|range| range.2).sum::<usize>();
            bundle.omitted_spans += group.ranges.len();
            continue;
        }
        let Ok(mut file) = File::open(path) else {
            bundle.omitted_citations += group.ranges.iter().map(|range| range.2).sum::<usize>();
            bundle.omitted_spans += group.ranges.len();
            continue;
        };
        let last_requested = group.ranges.iter().map(|range| range.1).max().unwrap_or(0);
        let line_count =
            count_requested_lines(&mut file, last_requested, &mut remaining_scan).unwrap_or(0);
        let mut ranges = group.ranges;
        ranges.sort_by_key(|range| (range.0, range.1));
        let mut merged: Vec<(u32, u32, usize)> = Vec::new();
        for (start, end, count) in ranges {
            if start > line_count || end > line_count {
                bundle.omitted_citations += count;
                bundle.omitted_spans += 1;
                continue;
            }
            if let Some(previous) = merged.last_mut() {
                if start <= previous.1.saturating_add(1) {
                    previous.1 = previous.1.max(end);
                    previous.2 += count;
                    continue;
                }
            }
            merged.push((start, end, count));
        }
        for (start_line, end_line, citation_count) in merged {
            let captured = read_source_span(
                &mut file,
                start_line,
                end_line,
                &mut remaining_scan,
                remaining_capture.min(MAX_HANDOFF_BYTES),
            );
            if let Ok(Some((text, truncated))) = captured {
                remaining_capture -= text.len();
                bundle.spans.push(EvidenceSpan {
                    priority: citations
                        .iter()
                        .position(|citation| {
                            citation.path == group.path
                                && citation.start_line <= end_line
                                && citation.end_line >= start_line
                        })
                        .expect("span was constructed from a citation"),
                    path: group.path.clone(),
                    start_line,
                    end_line,
                    text,
                    citation_count,
                    truncated,
                });
            } else {
                bundle.omitted_citations += citation_count;
                bundle.omitted_spans += 1;
            }
        }
    }
    bundle
}

#[cfg(test)]
mod tests {
    use super::*;

    fn citation(path: &str, start: u32, end: u32) -> ValidatedCitation {
        ValidatedCitation {
            path: path.into(),
            start_line: start,
            end_line: end,
            reason: None,
        }
    }

    #[test]
    fn uncited_long_lines_cannot_exceed_the_scan_budget() {
        let mut file = tempfile::tempfile().unwrap();
        file.set_len(MAX_SOURCE_SCAN_BYTES * 2).unwrap();
        let mut budget = MAX_SOURCE_SCAN_BYTES;
        assert_eq!(count_requested_lines(&mut file, 2, &mut budget).unwrap(), 1);
        assert_eq!(budget, 0);
        assert_eq!(file.stream_position().unwrap(), MAX_SOURCE_SCAN_BYTES);
    }

    #[test]
    fn source_capture_is_bounded_across_files() {
        let root = tempfile::tempdir().unwrap();
        let citations = (0..6)
            .map(|index| {
                let path = format!("{index}.rs");
                std::fs::write(root.path().join(&path), "x".repeat(MAX_HANDOFF_BYTES * 2)).unwrap();
                citation(&path, 1, 1)
            })
            .collect::<Vec<_>>();
        let bundle = evidence_excerpts(root.path(), &citations);
        assert!(
            bundle
                .spans
                .iter()
                .map(|span| span.text.len())
                .sum::<usize>()
                <= MAX_SOURCE_CAPTURE_BYTES
        );
        assert!(bundle.spans.iter().all(|span| span.truncated));
        assert!(bundle.omitted_citations > 0);
    }

    #[test]
    fn scan_budget_cannot_label_a_partial_last_line_as_complete() {
        use std::io::Write;
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"0123456789long line\n").unwrap();
        let mut budget = 5;
        let (text, truncated) = read_source_span(&mut file, 1, 1, &mut budget, 100)
            .unwrap()
            .unwrap();
        assert_eq!(text, "1: 01234");
        assert!(truncated);
    }

    #[test]
    fn invalid_overlapping_ranges_do_not_hide_valid_crlf_source() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("lib.rs"), "one\r\ntwo\r\nthree").unwrap();
        let bundle = evidence_excerpts(
            root.path(),
            &[citation("lib.rs", 2, 3), citation("lib.rs", 1, 99)],
        );
        assert_eq!(bundle.spans.len(), 1);
        assert_eq!(bundle.spans[0].text, "2: two\n3: three");
        assert!(!bundle.spans[0].truncated);
        assert_eq!(bundle.omitted_citations, 1);
    }
}
