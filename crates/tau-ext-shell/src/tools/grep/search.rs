//! Ignore-aware traversal, bounded line search, and borrowed-byte rendering.

#[cfg(test)]
mod tests;

use std::cell::Cell;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;
use std::sync::mpsc;

use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, MmapChoice, SearcherBuilder, Sink, SinkContext, SinkMatch};
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;

use super::{GrepOptions, GrepPattern, GrepStreamResult, render_grep_heading, render_grep_line};
use crate::display::ToolFailure;
use crate::tools::find::render_path;

const SEARCH_HEAP_LIMIT: usize = 16 * 1024 * 1024;
const REGEX_SIZE_LIMIT: usize = 10 * 1024 * 1024;
const DFA_CACHE_LIMIT: usize = 16 * 1024 * 1024;
const READ_CHUNK: usize = 64 * 1024;

/// Latches an active cancellation signal without treating sender disconnect as
/// a request.
struct Cancellation {
    /// Existing tool-lifecycle channel.
    receiver: Option<mpsc::Receiver<()>>,
    /// Whether a signal has already been observed.
    cancelled: Cell<bool>,
}

impl Cancellation {
    fn check(&self) -> bool {
        if !self.cancelled.get()
            && self
                .receiver
                .as_ref()
                .is_some_and(|rx| rx.try_recv().is_ok())
        {
            self.cancelled.set(true);
        }
        self.cancelled.get()
    }
}

/// Caps individual reads and checks cancellation even when no line matches.
struct CancellableReader<'a> {
    /// Open regular file.
    file: File,
    /// Invocation cancellation latch.
    cancellation: &'a Cancellation,
}

impl Read for CancellableReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cancellation.check() {
            return Err(io::Error::other("grep cancelled"));
        }
        let size = buf.len().min(READ_CHUNK);
        self.file.read(&mut buf[..size])
    }
}

/// Renders borrowed searcher lines into the existing bounded-per-line
/// accumulator.
struct GrepSink<'a> {
    /// Escaped display path for this file.
    path: String,
    /// Shared result for the entire invocation.
    result: &'a mut GrepStreamResult,
    /// Invocation-wide matching-line cap.
    limit: usize,
    /// Invocation cancellation latch.
    cancellation: &'a Cancellation,
    /// Most recently emitted file heading.
    heading: &'a mut Option<String>,
}

impl GrepSink<'_> {
    fn record(&mut self, lineno: u64, separator: char, bytes: &[u8]) {
        if self.heading.as_deref() != Some(self.path.as_str()) {
            let (line, truncated) = render_grep_heading(&self.path);
            self.result.lines_truncated |= truncated;
            self.result.result_lines.push(line);
            *self.heading = Some(self.path.clone());
        }
        let bytes = if let Some(line) = bytes.strip_suffix(b"\r\n") {
            line
        } else {
            bytes.strip_suffix(b"\n").unwrap_or(bytes)
        };
        // Lossy conversion only touches the visible prefix. Leave enough bytes
        // for replacement characters at the clipping boundary.
        let prefix = &bytes[..bytes.len().min(512)];
        let text = String::from_utf8_lossy(prefix);
        let (line, truncated) = render_grep_line(lineno, separator, &text);
        self.result.lines_truncated |= truncated || prefix.len() < bytes.len();
        self.result.result_lines.push(line);
    }

    fn check(&self) -> io::Result<()> {
        if self.cancellation.check() {
            Err(io::Error::other("grep cancelled"))
        } else {
            Ok(())
        }
    }
}

impl Sink for GrepSink<'_> {
    type Error = io::Error;

    fn matched(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        mat: &SinkMatch<'_>,
    ) -> io::Result<bool> {
        self.check()?;
        if self.result.match_count >= self.limit {
            self.result.match_limit_reached = true;
            return Ok(false);
        }
        self.result.match_count += 1;
        self.record(mat.line_number().unwrap_or(0), ':', mat.bytes());
        Ok(true)
    }

    fn context(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        context: &SinkContext<'_>,
    ) -> io::Result<bool> {
        self.check()?;
        self.record(context.line_number().unwrap_or(0), '-', context.bytes());
        Ok(true)
    }

    fn begin(&mut self, _searcher: &grep_searcher::Searcher) -> io::Result<bool> {
        self.check()?;
        Ok(true)
    }

    fn binary_data(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        _offset: u64,
    ) -> io::Result<bool> {
        self.check()?;
        Ok(true)
    }
}

/// Searches one root, returning `None` only for a genuine cancellation.
pub(super) fn search(
    options: &GrepOptions,
    cancel_rx: Option<mpsc::Receiver<()>>,
) -> Result<Option<GrepStreamResult>, ToolFailure> {
    let cancellation = Cancellation {
        receiver: cancel_rx,
        cancelled: Cell::new(false),
    };
    if cancellation.check() {
        return Ok(None);
    }
    let mut builder = RegexMatcherBuilder::new();
    builder
        .fixed_strings(matches!(options.pattern, GrepPattern::Literal(_)))
        .case_insensitive(options.ignore_case)
        .multi_line(true)
        .line_terminator(Some(b'\n'))
        .ban_byte(Some(b'\0'))
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(DFA_CACHE_LIMIT);
    let matcher = builder
        .build(options.pattern.text())
        .map_err(|e| ToolFailure::new(format!("regex parse or resource error: {e}")))?;
    if cancellation.check() {
        return Ok(None);
    }

    let root = options.search_path();
    let metadata = fs::metadata(root).map_err(|e| file_error(root, &e))?;
    if !metadata.is_file() && !metadata.is_dir() {
        return Err(ToolFailure::new(
            "grep path must be a regular file or directory",
        ));
    }
    let explicit_file = metadata.is_file();
    let mut walk = WalkBuilder::new(root);
    walk.hidden(false).add_custom_ignore_filename(".rgignore");
    if let Some(glob) = &options.glob {
        // The old rg subprocess used its startup cwd even when admission
        // rewrote the search root against a remembered workdir.
        let mut overrides = OverrideBuilder::new(
            std::env::current_dir().map_err(|e| ToolFailure::new(e.to_string()))?,
        );
        overrides
            .add(glob)
            .map_err(|e| ToolFailure::new(format!("invalid glob: {e}")))?;
        walk.overrides(
            overrides
                .build()
                .map_err(|e| ToolFailure::new(format!("invalid glob: {e}")))?,
        );
    }
    let mut result = GrepStreamResult {
        result_lines: Vec::new(),
        match_count: 0,
        lines_truncated: false,
        match_limit_reached: false,
    };
    let mut heading = None;
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .before_context(options.context.unwrap_or(0))
        .after_context(options.context.unwrap_or(0))
        .heap_limit(Some(SEARCH_HEAP_LIMIT))
        .memory_map(MmapChoice::never())
        .bom_sniffing(true)
        .build();
    // The walker reserves "-" for stdin. An explicitly named regular file
    // (including one called "-") must instead be opened by its actual path.
    let mut entries: Box<dyn Iterator<Item = Result<Option<_>, _>>> = if explicit_file {
        Box::new(std::iter::once(Ok(Some(root.to_path_buf()))))
    } else {
        Box::new(walk.build().map(|entry| {
            entry.map(|entry| {
                entry
                    .file_type()
                    .is_some_and(|ty| ty.is_file())
                    .then(|| entry.path().to_path_buf())
            })
        }))
    };
    loop {
        let entry = next_entry(&mut *entries, &cancellation);
        let Ok(Some(entry)) = entry else {
            if entry.is_err() {
                return Ok(None);
            }
            break;
        };
        let path = entry.map_err(|e: ignore::Error| {
            e.io_error().map_or_else(
                || ToolFailure::new(e.to_string()),
                |io| file_error(root, io),
            )
        })?;
        let Some(path) = path else {
            continue;
        };
        let file = File::open(&path).map_err(|e| file_error(&path, &e))?;
        // Explicit files use convert mode, while discovered files quit at NUL.
        searcher.set_binary_detection(if explicit_file {
            BinaryDetection::convert(b'\0')
        } else {
            BinaryDetection::quit(b'\0')
        });
        let sink = GrepSink {
            path: render_path(&path),
            result: &mut result,
            limit: options.limit,
            cancellation: &cancellation,
            heading: &mut heading,
        };
        let reader = CancellableReader {
            file,
            cancellation: &cancellation,
        };
        if let Err(error) = searcher.search_reader(&matcher, reader, sink) {
            if cancellation.check() {
                return Ok(None);
            }
            return Err(file_error(&path, &error));
        }
        if cancellation.check() {
            return Ok(None);
        }
        if result.match_limit_reached {
            break;
        }
    }
    Ok(Some(result))
}

/// Check both sides of traversal, including a walk that exhausts after
/// cancellation.
fn next_entry<I: Iterator + ?Sized>(
    entries: &mut I,
    cancellation: &Cancellation,
) -> Result<Option<I::Item>, ()> {
    if cancellation.check() {
        return Err(());
    }
    let entry = entries.next();
    if cancellation.check() {
        return Err(());
    }
    Ok(entry)
}

fn file_error(path: &Path, error: &io::Error) -> ToolFailure {
    let category = match error.kind() {
        io::ErrorKind::NotFound => "no such file or directory",
        io::ErrorKind::PermissionDenied => "permission denied",
        _ => {
            return ToolFailure::new(format!(
                "grep I/O or resource error at {}: {error}",
                path.display()
            ));
        }
    };
    ToolFailure::new(category)
}
