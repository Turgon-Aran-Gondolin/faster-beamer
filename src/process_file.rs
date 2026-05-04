//
// process_file.rs
// Copyright (C) 2019 seitz_local <seitz_local@lmeXX>
// Distributed under terms of the GPLv3 license.
//
use crate::beamer::get_frames;
use crate::fs_utils::{cache_path, publish_file};
use crate::latexcompile::BibliographyTool;
use crate::parsing;

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use log::Level::Trace;

use crate::latexcompile::{LatexCompiler, LatexInput, LatexRunOptions};
use clap::ArgMatches;
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use regex::Regex;
use std::collections::HashSet;
use std::env::current_dir;
use std::fs;
use std::fs::write;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use std::vec::Vec;

#[derive(PartialEq)]
pub enum FasterBeamerError {
    InputFileNotExistent,
    IoError,
    CompileError,
    PdfUniteError,
}

pub type Result<T> = ::std::result::Result<T, FasterBeamerError>;

#[derive(Clone)]
struct SyncTexLineSegment {
    temp_start_line: usize,
    line_count: usize,
    source_start_line: usize,
}

struct FrameSyncTexMap {
    source_file: PathBuf,
    temp_file_name: String,
    segments: Vec<SyncTexLineSegment>,
}

impl FrameSyncTexMap {
    fn map_temp_line(&self, temp_line: usize) -> usize {
        for segment in self.segments.iter().rev() {
            if temp_line >= segment.temp_start_line
                && temp_line < segment.temp_start_line + segment.line_count
            {
                return segment.source_start_line + (temp_line - segment.temp_start_line);
            }
        }

        temp_line
    }
}

struct GeneratedDocument {
    tex_content: String,
    sync_map: FrameSyncTexMap,
    dependencies: Vec<PathBuf>,
    support_files: Vec<GeneratedSupportFile>,
}

struct GeneratedSupportFile {
    extension: &'static str,
    content: String,
}

struct FrameCompileFailure {
    frame_idx: usize,
    source_start_line: usize,
    source_line_count: usize,
    temp_file: PathBuf,
    temp_file_name: String,
    sync_segments: Vec<SyncTexLineSegment>,
    frame_preview: String,
    error: String,
}

struct SourceSection {
    line_number: usize,
    number: usize,
    title: String,
    is_appendix: bool,
}

struct TocFramePatch {
    runtime_setup: String,
    support_files: Vec<GeneratedSupportFile>,
    additional_dependencies: Vec<PathBuf>,
}

enum TocFrameSupport {
    None,
    Supported(TocFramePatch),
    UnsupportedDynamic,
}

lazy_static! {
    static ref FRAME_REGEX: Regex =
        Regex::new(r"(?ms)^[ \t]*\\begin\{frame\}.*?^[ \t]*\\end\{frame\}").unwrap();
}
lazy_static! {
    static ref SECTION_LINE_REGEX: Regex = Regex::new(
        r"(?x)
        ^[ \t]*
        \\section
        (?:\s*\[[^\]]*\])?
        \s*\{(?P<title>[^}]*)\}
    "
    )
    .unwrap();
}
lazy_static! {
    static ref APPENDIX_LINE_REGEX: Regex = Regex::new(r"^[ \t]*\\appendix\b").unwrap();
}
lazy_static! {
    static ref TOC_REGEX: Regex = Regex::new(r"\\tableofcontents(?:\s*\[[^\]]*\])?").unwrap();
}
lazy_static! {
    static ref DYNAMIC_TOC_OPTION_REGEX: Regex =
        Regex::new(r"\\tableofcontents\s*\[[^\]]*(?:currentsection|currentsubsection)[^\]]*\]").unwrap();
}
lazy_static! {
    static ref DOCUMENT_REGEX: Regex =
        Regex::new(r"(?ms)^[ \t]*\\begin\{document\}.*^[ \t]*\\end\{document\}").unwrap();
}

lazy_static! {
    static ref RELATED_FILE_REGEX: Regex = Regex::new(
        r"(?sx)
        \\(?P<command>includegraphics|input|include)
        (?:\s*\[[^\]]*\])?
        \s*\{
            (?P<path>[^}]*)
        \}
    "
    )
    .unwrap();
}

lazy_static! {
    static ref GRAPHICSPATH_REGEX: Regex = Regex::new(
        r"(?sx)
        \\graphicspath
        \s*\{
            (?P<paths>(?:\s*\{[^}]*\}\s*)+)
        \}
    "
    )
    .unwrap();
}

lazy_static! {
    static ref GRAPHICSPATH_ENTRY_REGEX: Regex = Regex::new(r"\{(?P<path>[^}]*)\}").unwrap();
}

lazy_static! {
    static ref TEX_LOG_LINE_REGEX: Regex = Regex::new(r"\bl\.(?P<line>\d+)\b").unwrap();
}

lazy_static! {
    static ref PREVIOUS_FRAMES: Mutex<Vec<String>> = Mutex::new(Vec::new());
}

const FRAME_TEMP_PREFIX: &str = "faster-beamer-temp-";
const PREAMBLE_TEMP_PREFIX: &str = "faster-beamer-preamble-";
const UNITED_TEMP_PREFIX: &str = "faster-beamer-united-";
const GRAPHICS_EXTENSIONS: [&str; 6] = ["pdf", "png", "jpg", "jpeg", "eps", "svg"];
const DEPENDENCY_MANIFEST_EXTENSION: &str = "deps";

fn frame_counter_setup(frame_idx: usize, correct_frame_numbers: bool) -> String {
    if correct_frame_numbers {
        format!("\\setcounter{{framenumber}}{{{}}}\n", frame_idx)
    } else {
        String::new()
    }
}

fn frame_preview(frame_text: &str) -> String {
    const PREVIEW_LIMIT: usize = 160;

    let line = frame_text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");

    if line.chars().count() <= PREVIEW_LIMIT {
        line.to_string()
    } else {
        let truncated: String = line.chars().take(PREVIEW_LIMIT).collect();
        format!("{}...", truncated)
    }
}

fn log_frame_compile_failures(
    failures: &[FrameCompileFailure],
    source_file: &Path,
    frame_count: usize,
) {
    error!(
        "Compilation aborted: {} frame build(s) failed.",
        failures.len()
    );

    for failure in failures {
        let source_end_line = failure
            .source_start_line
            .saturating_add(failure.source_line_count.saturating_sub(1));

        error!(
            "Frame {}/{} ({}:{}-{} -> {}): {}",
            failure.frame_idx + 1,
            frame_count,
            source_file.display(),
            failure.source_start_line,
            source_end_line,
            failure.temp_file.display(),
            failure.error
        );

        if !failure.frame_preview.is_empty() {
            error!("Frame preview: {}", failure.frame_preview);
        }
    }
}

fn map_temp_line_from_segments(segments: &[SyncTexLineSegment], temp_line: usize) -> usize {
    for segment in segments.iter().rev() {
        if temp_line >= segment.temp_start_line
            && temp_line < segment.temp_start_line + segment.line_count
        {
            return segment.source_start_line + (temp_line - segment.temp_start_line);
        }
    }

    temp_line
}

fn remap_frame_log_to_source(failure: &FrameCompileFailure, source_file_name: &str, log_content: &str) -> String {
    let mut remapped = log_content.replace(&failure.temp_file_name, source_file_name);
    remapped = remapped.replace(failure.temp_file.to_string_lossy().as_ref(), source_file_name);

    TEX_LOG_LINE_REGEX
        .replace_all(&remapped, |captures: &regex::Captures<'_>| {
            let line = captures
                .name("line")
                .and_then(|value| value.as_str().parse::<usize>().ok())
                .unwrap_or(0);
            let mapped = map_temp_line_from_segments(&failure.sync_segments, line);
            format!("l.{}", mapped)
        })
        .into_owned()
}

fn remap_log_lines_to_source(
    log_content: &str,
    source_file_name: &str,
    temp_file_name: &str,
    segments: &[SyncTexLineSegment],
) -> String {
    let remapped = log_content.replace(temp_file_name, source_file_name);

    TEX_LOG_LINE_REGEX
        .replace_all(&remapped, |captures: &regex::Captures<'_>| {
            let line = captures
                .name("line")
                .and_then(|value| value.as_str().parse::<usize>().ok())
                .unwrap_or(0);
            let mapped = map_temp_line_from_segments(segments, line);
            format!("l.{}", mapped)
        })
        .into_owned()
}

fn write_master_log(source_file: &Path, content: &str) -> Result<()> {
    let source_log = source_file.with_extension("log");
    fs::write(&source_log, content).map_err(|err| {
        error!("Failed to write master log {}: {}", source_log.display(), err);
        FasterBeamerError::IoError
    })
}

fn write_master_log_from_compile_failure(
    source_file: &Path,
    context: &str,
    compiler_log_path: &Path,
    fallback_message: &str,
) -> Result<()> {
    let source_file_name = tex_input_name(source_file);
    let mut master_log = String::new();

    master_log.push_str("This is faster-beamer, redirected compiler failure log.\n");
    master_log.push_str(&format!("(./{})\n", source_file_name));
    master_log.push_str(&format!("! faster-beamer: {} failed\n", context));

    match fs::read_to_string(compiler_log_path) {
        Ok(log_content) => {
            master_log.push_str(&log_content);
            if !log_content.ends_with('\n') {
                master_log.push('\n');
            }
        }
        Err(_) => {
            master_log.push_str(fallback_message);
            if !fallback_message.ends_with('\n') {
                master_log.push('\n');
            }
        }
    }

    write_master_log(source_file, &master_log)
}

fn write_master_log_for_united_failure(
    source_file: &Path,
    cache_subdir: &Path,
    sync_map: &FrameSyncTexMap,
    fallback_message: &str,
) -> Result<()> {
    let source_file_name = tex_input_name(source_file);
    let united_log_path =
        cache_subdir.join(Path::new(&sync_map.temp_file_name).with_extension("log"));
    let mut master_log = String::new();

    master_log.push_str("This is faster-beamer, redirected compiler failure log.\n");
    master_log.push_str(&format!("(./{})\n", source_file_name));
    master_log.push_str("! faster-beamer: united document compilation failed\n");

    match fs::read_to_string(&united_log_path) {
        Ok(log_content) => {
            let remapped = remap_log_lines_to_source(
                &log_content,
                source_file_name,
                &sync_map.temp_file_name,
                &sync_map.segments,
            );
            master_log.push_str(&remapped);
            if !remapped.ends_with('\n') {
                master_log.push('\n');
            }
        }
        Err(_) => {
            master_log.push_str(fallback_message);
            if !fallback_message.ends_with('\n') {
                master_log.push('\n');
            }
        }
    }

    write_master_log(source_file, &master_log)
}

fn write_master_log_for_frame_failures(
    failures: &[FrameCompileFailure],
    cache_subdir: &Path,
    source_file: &Path,
    frame_count: usize,
) -> Result<()> {
    let source_file_name = tex_input_name(source_file);
    let mut master_log = String::new();

    master_log.push_str("This is faster-beamer, aggregated frame failure log.\n");
    master_log.push_str(&format!("(./{})\n", source_file_name));

    for failure in failures {
        let log_path = cache_subdir.join(Path::new(&failure.temp_file_name).with_extension("log"));

        master_log.push_str(&format!(
            "! faster-beamer: frame {}/{} failed to compile\n",
            failure.frame_idx + 1,
            frame_count.max(1)
        ));
        master_log.push_str(&format!("l.{} {}\n", failure.source_start_line, failure.frame_preview));

        match fs::read_to_string(&log_path) {
            Ok(log_content) => {
                let remapped = remap_frame_log_to_source(failure, source_file_name, &log_content);
                master_log.push_str(&remapped);
                if !remapped.ends_with('\n') {
                    master_log.push('\n');
                }
            }
            Err(err) => {
                master_log.push_str(&format!(
                    "! faster-beamer: could not read frame log {} ({})\n",
                    log_path.display(),
                    err
                ));
                master_log.push_str(&format!("l.{}\n", failure.source_start_line));
                master_log.push_str(&failure.error);
                master_log.push('\n');
            }
        }
    }

    write_master_log(source_file, &master_log)
}

fn compile_progress_bar(total_jobs: usize) -> ProgressBar {
    let progress_bar = ProgressBar::new(total_jobs as u64);
    let style = ProgressStyle::with_template("Compile {pos}/{len} jobs [{bar:40.cyan/blue}]")
        .expect("compile progress bar template should be valid")
        .progress_chars("##-");
    progress_bar.set_style(style);
    progress_bar
}

fn show_error_slide(cachedir: &Path, output_file: &str) {
    let error_frame = String::from_utf8_lossy(include_bytes!("error.tex")).to_owned();
    let error_file = cachedir.join("error.tex");
    let error_pdf = cachedir.join("error.pdf");

    if !error_pdf.exists() && write(&error_file, &error_frame[..]).is_ok() {
        let compiler = LatexCompiler::new_in(cachedir.to_owned())
            .add_arg("-shell-escape")
            .add_arg("-interaction=nonstopmode");

        let _result = compiler.run(
            &error_file,
            &LatexInput::new(),
            LatexRunOptions::new(),
        );
    }
    if error_pdf.exists() {
        if let Err(err) = publish_file(&error_pdf, Path::new(output_file)) {
            error!("Failed to publish error slide: {}", err);
        }
    }
}

fn log_command_error(command: &str, context: &str, err: &std::io::Error) {
    if err.kind() == ErrorKind::NotFound {
        error!("Failed to {}: {} was not found on PATH.", context, command);
    } else {
        error!("Failed to {}: {}", context, err);
    }
}

fn publish_output_file(compiled_pdf: &Path, output_file: &str) -> Result<()> {
    info!("Published PDF: {}", Path::new(output_file).display());
    publish_file(compiled_pdf, Path::new(output_file)).map_err(|err| {
        error!("{}", err);
        FasterBeamerError::IoError
    })
}

fn clear_published_synctex(output_file: &str) {
    let synctex_file = Path::new(output_file).with_extension("synctex.gz");
    if synctex_file.is_file() {
        if let Err(err) = std::fs::remove_file(&synctex_file) {
            warn!(
                "Failed to remove stale SyncTeX file {}: {}",
                synctex_file.display(),
                err
            );
        }
    }
}

fn publish_synctex_file(compiled_pdf: &Path, output_file: &str) -> Result<()> {
    let compiled_synctex = compiled_pdf.with_extension("synctex.gz");
    if !compiled_synctex.is_file() {
        warn!(
            "Expected SyncTeX output {} but it was not generated.",
            compiled_synctex.display()
        );
        return Ok(());
    }

    let output_synctex = Path::new(output_file).with_extension("synctex.gz");
    info!("Published SyncTeX: {}", output_synctex.display());
    publish_file(&compiled_synctex, &output_synctex).map_err(|err| {
        error!("{}", err);
        FasterBeamerError::IoError
    })
}

fn publish_output_artifacts(
    compiled_pdf: &Path,
    output_file: &str,
    sync_map: Option<&FrameSyncTexMap>,
) -> Result<()> {
    publish_output_file(compiled_pdf, output_file)?;

    match sync_map {
        Some(sync_map) => {
            rewrite_synctex_to_original(compiled_pdf, sync_map)?;
            publish_synctex_file(compiled_pdf, output_file)
        }
        None => {
            clear_published_synctex(output_file);
            Ok(())
        }
    }
}

fn rewrite_synctex_to_original(compiled_pdf: &Path, sync_map: &FrameSyncTexMap) -> Result<()> {
    let synctex_file = compiled_pdf.with_extension("synctex.gz");
    if !synctex_file.is_file() {
        return Ok(());
    }

    let compressed = std::fs::read(&synctex_file).map_err(|err| {
        error!("Failed to read SyncTeX file {}: {}", synctex_file.display(), err);
        FasterBeamerError::IoError
    })?;

    let mut decoder = GzDecoder::new(&compressed[..]);
    let mut content = String::new();
    decoder.read_to_string(&mut content).map_err(|err| {
        error!("Failed to decode SyncTeX file {}: {}", synctex_file.display(), err);
        FasterBeamerError::IoError
    })?;

    let rewritten = remap_synctex_contents(&content, sync_map);
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(rewritten.as_bytes()).map_err(|err| {
        error!("Failed to encode SyncTeX file {}: {}", synctex_file.display(), err);
        FasterBeamerError::IoError
    })?;
    let compressed = encoder.finish().map_err(|err| {
        error!("Failed to finish SyncTeX encoding for {}: {}", synctex_file.display(), err);
        FasterBeamerError::IoError
    })?;

    std::fs::write(&synctex_file, compressed).map_err(|err| {
        error!("Failed to write SyncTeX file {}: {}", synctex_file.display(), err);
        FasterBeamerError::IoError
    })
}

fn remap_synctex_contents(content: &str, sync_map: &FrameSyncTexMap) -> String {
    let mut temp_tag = None;
    let mut rewritten_lines = Vec::new();

    for line in content.lines() {
        if let Some((tag, path)) = parse_synctex_input_line(line) {
            if synctex_input_matches(path, &sync_map.temp_file_name) {
                temp_tag = Some(tag);
                rewritten_lines.push(format!("Input:{}:{}", tag, synctex_path(&sync_map.source_file)));
                continue;
            }
        }

        if let Some(tag) = temp_tag {
            if let Some(rewritten) = remap_synctex_link_line(line, tag, sync_map) {
                rewritten_lines.push(rewritten);
                continue;
            }
        }

        rewritten_lines.push(line.to_owned());
    }

    let mut rewritten = rewritten_lines.join("\n");
    if content.ends_with('\n') {
        rewritten.push('\n');
    }
    rewritten
}

fn parse_synctex_input_line(line: &str) -> Option<(u32, &str)> {
    let rest = line.strip_prefix("Input:")?;
    let mut parts = rest.splitn(2, ':');
    let tag = parts.next()?.parse::<u32>().ok()?;
    let path = parts.next()?;
    Some((tag, path))
}

fn synctex_input_matches(path: &str, temp_file_name: &str) -> bool {
    path == temp_file_name
        || Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name == temp_file_name)
            .unwrap_or(false)
}

fn remap_synctex_link_line(line: &str, temp_tag: u32, sync_map: &FrameSyncTexMap) -> Option<String> {
    let first_char = line.chars().next()?;
    if !matches!(first_char, '[' | '(' | 'x' | 'k' | 'g' | '$' | 'v' | 'h') {
        return None;
    }

    let prefix_len = first_char.len_utf8();
    let rest = &line[prefix_len..];
    let colon_idx = rest.find(':')?;
    let link = &rest[..colon_idx];
    let mut parts = link.split(',');
    let tag = parts.next()?.parse::<u32>().ok()?;
    if tag != temp_tag {
        return None;
    }

    let line_no = parts.next()?.parse::<usize>().ok()?;
    let remapped_line = sync_map.map_temp_line(line_no);
    let mut rewritten_link = format!("{},{}", tag, remapped_line);
    if let Some(column) = parts.next() {
        rewritten_link.push(',');
        rewritten_link.push_str(column);
    }

    Some(format!(
        "{}{}{}",
        &line[..prefix_len],
        rewritten_link,
        &rest[colon_idx..]
    ))
}

fn logical_line_count(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.lines().count()
    }
}

fn line_number_at(text: &str, byte_idx: usize) -> usize {
    text[..byte_idx].bytes().filter(|byte| *byte == b'\n').count() + 1
}

fn strip_tex_comments(tex: &str) -> String {
    let mut stripped = String::with_capacity(tex.len());

    for chunk in tex.split_inclusive('\n') {
        let (line, newline) = match chunk.strip_suffix('\n') {
            Some(line) => (line, "\n"),
            None => (chunk, ""),
        };

        let mut escaped = false;
        let mut comment_start = line.len();
        for (idx, ch) in line.char_indices() {
            if ch == '%' && !escaped {
                comment_start = idx;
                break;
            }

            if ch == '\\' {
                escaped = !escaped;
            } else {
                escaped = false;
            }
        }

        stripped.push_str(&line[..comment_start]);
        stripped.push_str(newline);
    }

    stripped
}

fn frame_contains_table_of_contents(frame: &str) -> bool {
    TOC_REGEX.is_match(&strip_tex_comments(frame))
}

fn table_of_contents_uses_dynamic_section_context(frame: &str) -> bool {
    DYNAMIC_TOC_OPTION_REGEX.is_match(&strip_tex_comments(frame))
}

fn document_sections(source_content: &str) -> Vec<SourceSection> {
    let stripped = strip_tex_comments(source_content);
    let mut sections = Vec::new();
    let mut is_appendix = false;

    for (line_idx, line) in stripped.lines().enumerate() {
        if APPENDIX_LINE_REGEX.is_match(line) {
            is_appendix = true;
        }

        if let Some(captures) = SECTION_LINE_REGEX.captures(line) {
            let title = captures
                .name("title")
                .map(|capture| capture.as_str().trim().to_string())
                .unwrap_or_default();
            sections.push(SourceSection {
                line_number: line_idx + 1,
                number: sections.len() + 1,
                title,
                is_appendix,
            });
        }
    }

    sections
}

fn current_section_number(sections: &[SourceSection], frame_start_line: usize) -> usize {
    sections
        .iter()
        .rev()
        .find(|section| section.line_number < frame_start_line)
        .map(|section| section.number)
        .unwrap_or(0)
}

fn synthetic_toc_content(sections: &[SourceSection]) -> String {
    let mut content = String::new();

    for section in sections {
        content.push_str(&format!(
            "\\beamer@sectionintoc {{{}}}{{{}}}{{{}}}{{{}}}{{{}}}\n",
            section.number,
            section.title,
            section.number,
            if section.is_appendix { 1 } else { 0 },
            section.number,
        ));
    }

    content
}

fn toc_frame_patch(
    frame: &str,
    source_frame_start_line: usize,
    document_begin_line: usize,
    input_dir: &Path,
    input_path: &Path,
    sections: &[SourceSection],
) -> TocFrameSupport {
    if !frame_contains_table_of_contents(frame) {
        return TocFrameSupport::None;
    }

    if source_frame_start_line < document_begin_line
        && table_of_contents_uses_dynamic_section_context(frame)
    {
        return TocFrameSupport::UnsupportedDynamic;
    }

    let source_toc_path = input_dir.join(Path::new(tex_input_name(input_path)).with_extension("toc"));
    let mut additional_dependencies = Vec::new();
    let toc_content = match fs::read_to_string(&source_toc_path) {
        Ok(content) => {
            additional_dependencies.push(source_toc_path);
            content
        }
        Err(_) => synthetic_toc_content(sections),
    };

    let current_section = if table_of_contents_uses_dynamic_section_context(frame) {
        current_section_number(sections, source_frame_start_line)
    } else {
        0
    };

    TocFrameSupport::Supported(TocFramePatch {
        runtime_setup: format!(
            "\\setcounter{{section}}{{{}}}\n\\setcounter{{subsection}}{{0}}\n",
            current_section
        ),
        support_files: vec![GeneratedSupportFile {
            extension: "toc",
            content: toc_content,
        }],
        additional_dependencies,
    })
}

fn is_static_path_reference(raw_path: &str) -> bool {
    !raw_path.is_empty()
        && !raw_path
            .chars()
            .any(|ch| matches!(ch, '#' | '\\' | '{' | '}' | '\n' | '\r'))
}

fn dedupe_paths(paths: &mut Vec<PathBuf>) {
    let mut seen = HashSet::new();
    paths.retain(|path| seen.insert(path.clone()));
}

fn collect_graphics_paths(
    tex: &str,
    base_dir: &Path,
    inherited_paths: &[PathBuf],
) -> Vec<PathBuf> {
    let mut search_paths = inherited_paths.to_vec();
    if !search_paths.iter().any(|path| path == base_dir) {
        search_paths.push(base_dir.to_path_buf());
    }

    for captures in GRAPHICSPATH_REGEX.captures_iter(tex) {
        let raw_paths = captures
            .name("paths")
            .map(|value| value.as_str())
            .unwrap_or_default();

        for path_capture in GRAPHICSPATH_ENTRY_REGEX.captures_iter(raw_paths) {
            let raw_path = path_capture
                .name("path")
                .map(|value| value.as_str().trim())
                .unwrap_or_default();
            if !is_static_path_reference(raw_path) {
                continue;
            }

            let candidate = Path::new(raw_path);
            let resolved = if candidate.is_absolute() {
                candidate.to_path_buf()
            } else {
                base_dir.join(candidate)
            };
            search_paths.push(resolved);
        }
    }

    dedupe_paths(&mut search_paths);
    search_paths
}

fn resolve_tex_dependency(path: &Path) -> PathBuf {
    if path.exists() {
        return path.to_path_buf();
    }

    if path.extension().is_none() {
        let with_tex = path.with_extension("tex");
        if with_tex.exists() {
            return with_tex;
        }
        return with_tex;
    }

    path.to_path_buf()
}

fn resolve_graphics_dependency(path: &Path) -> PathBuf {
    if path.exists() {
        return path.to_path_buf();
    }

    if path.extension().is_some() {
        return path.to_path_buf();
    }

    for extension in GRAPHICS_EXTENSIONS {
        let candidate = path.with_extension(extension);
        if candidate.exists() {
            return candidate;
        }
    }

    path.to_path_buf()
}

fn resolve_graphics_from_paths(raw_path: &str, graphics_paths: &[PathBuf]) -> Option<PathBuf> {
    let candidate = Path::new(raw_path.trim());
    if candidate.is_absolute() {
        return Some(resolve_graphics_dependency(candidate));
    }

    for search_path in graphics_paths {
        let resolved = resolve_graphics_dependency(&search_path.join(candidate));
        if resolved.exists() {
            return Some(resolved);
        }
    }

    graphics_paths
        .first()
        .map(|search_path| resolve_graphics_dependency(&search_path.join(candidate)))
}

fn resolve_related_file(
    command: &str,
    raw_path: &str,
    base_dir: &Path,
    graphics_paths: &[PathBuf],
) -> Option<PathBuf> {
    if !is_static_path_reference(raw_path.trim()) {
        return None;
    }

    let candidate = Path::new(raw_path.trim());
    let path = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        base_dir.join(candidate)
    };

    match command {
        "includegraphics" => resolve_graphics_from_paths(raw_path, graphics_paths),
        "input" | "include" => Some(resolve_tex_dependency(&path)),
        _ => Some(path),
    }
}

fn collect_related_files_from_tex(
    tex: &str,
    base_dir: &Path,
    inherited_graphics_paths: &[PathBuf],
    dependencies: &mut Vec<PathBuf>,
    seen_paths: &mut HashSet<PathBuf>,
    visited_inputs: &mut HashSet<PathBuf>,
) {
    let stripped = strip_tex_comments(tex);
    let graphics_paths = collect_graphics_paths(&stripped, base_dir, inherited_graphics_paths);

    for captures in RELATED_FILE_REGEX.captures_iter(&stripped) {
        let command = captures
            .name("command")
            .map(|value| value.as_str())
            .unwrap_or_default();
        let raw_path = captures
            .name("path")
            .map(|value| value.as_str().trim())
            .unwrap_or_default();

        if raw_path.is_empty() {
            continue;
        }

        let Some(resolved) = resolve_related_file(command, raw_path, base_dir, &graphics_paths) else {
            continue;
        };
        if seen_paths.insert(resolved.clone()) {
            dependencies.push(resolved.clone());
        }

        if matches!(command, "input" | "include") && visited_inputs.insert(resolved.clone()) {
            if let Ok(content) = std::fs::read_to_string(&resolved) {
                let next_base_dir = resolved.parent().unwrap_or(base_dir);
                collect_related_files_from_tex(
                    &content,
                    next_base_dir,
                    &graphics_paths,
                    dependencies,
                    seen_paths,
                    visited_inputs,
                );
            }
        }
    }
}

fn collect_related_files(tex: &str, base_dir: &Path) -> Vec<PathBuf> {
    let mut dependencies = Vec::new();
    let mut seen_paths = HashSet::new();
    let mut visited_inputs = HashSet::new();
    collect_related_files_from_tex(
        tex,
        base_dir,
        &[base_dir.to_path_buf()],
        &mut dependencies,
        &mut seen_paths,
        &mut visited_inputs,
    );
    dependencies.sort();
    dependencies
}

fn dependency_manifest_path(cache_subdir: &Path, temp_file_name: &str) -> PathBuf {
    cache_subdir.join(Path::new(temp_file_name).with_extension(DEPENDENCY_MANIFEST_EXTENSION))
}

fn parse_dependency_manifest(content: &str) -> Vec<PathBuf> {
    let mut dependencies: Vec<PathBuf> = content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect();
    dependencies.sort();
    dependencies.dedup();
    dependencies
}

fn read_dependency_manifest(cache_subdir: &Path, temp_file_name: &str) -> Option<Vec<PathBuf>> {
    let manifest_path = dependency_manifest_path(cache_subdir, temp_file_name);
    fs::read_to_string(manifest_path)
        .ok()
        .map(|content| parse_dependency_manifest(&content))
}

fn should_track_runtime_dependency(path: &Path, cache_subdir: &Path) -> bool {
    if !path.is_file() {
        return false;
    }

    if path.starts_with(cache_subdir) {
        return false;
    }

    let file_name = match path.file_name().and_then(|name| name.to_str()) {
        Some(name) => name,
        None => return false,
    };

    !(file_name.starts_with(FRAME_TEMP_PREFIX)
        || file_name.starts_with(PREAMBLE_TEMP_PREFIX)
        || file_name.starts_with(UNITED_TEMP_PREFIX))
}

fn parse_fls_dependencies(content: &str, cache_subdir: &Path) -> Vec<PathBuf> {
    let mut dependencies = Vec::new();
    let mut seen_paths = HashSet::new();

    for line in content.lines() {
        let Some(raw_path) = line.strip_prefix("INPUT ") else {
            continue;
        };

        let path = PathBuf::from(raw_path.trim());
        if should_track_runtime_dependency(&path, cache_subdir) && seen_paths.insert(path.clone()) {
            dependencies.push(path);
        }
    }

    dependencies.sort();
    dependencies
}

fn write_dependency_manifest(
    cache_subdir: &Path,
    temp_file_name: &str,
    dependencies: &[PathBuf],
) -> Result<()> {
    let manifest_path = dependency_manifest_path(cache_subdir, temp_file_name);
    let content = dependencies
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<String>>()
        .join("\n");

    fs::write(&manifest_path, content).map_err(|err| {
        error!(
            "Failed to write dependency manifest {}: {}",
            manifest_path.display(),
            err
        );
        FasterBeamerError::IoError
    })
}

fn update_dependency_manifest(cache_subdir: &Path, temp_file_name: &str) -> Result<()> {
    let fls_path = cache_subdir.join(Path::new(temp_file_name).with_extension("fls"));
    let content = fs::read_to_string(&fls_path).map_err(|err| {
        error!("Failed to read recorder file {}: {}", fls_path.display(), err);
        FasterBeamerError::IoError
    })?;
    let dependencies = parse_fls_dependencies(&content, cache_subdir);
    write_dependency_manifest(cache_subdir, temp_file_name, &dependencies)
}

fn dependencies_for_document(cache_subdir: &Path, document: &GeneratedDocument) -> Vec<PathBuf> {
    read_dependency_manifest(cache_subdir, &document.sync_map.temp_file_name)
        .unwrap_or_else(|| document.dependencies.clone())
}

fn compiled_output_is_fresh(compiled_pdf: &Path, dependencies: &[PathBuf]) -> bool {
    let compiled_modified = match std::fs::metadata(compiled_pdf).and_then(|metadata| metadata.modified()) {
        Ok(modified) => modified,
        Err(_) => return false,
    };

    dependencies.iter().all(|dependency| {
        std::fs::metadata(dependency)
            .and_then(|metadata| metadata.modified())
            .map(|modified| modified <= compiled_modified)
            .unwrap_or(false)
    })
}

fn first_changed_frame_index(
    frames: &[String],
    previous_frames: &[String],
    generated_documents: &[GeneratedDocument],
    cache_subdir: &Path,
    force_recompile: bool,
) -> usize {
    for (frame_idx, frame) in frames.iter().enumerate() {
        if force_recompile {
            return frame_idx;
        }

        if previous_frames.get(frame_idx) != Some(frame) {
            return frame_idx;
        }

        let document = &generated_documents[frame_idx];
        let compiled_pdf = compiled_pdf_path(cache_subdir, &document.sync_map.temp_file_name);
        let dependencies = dependencies_for_document(cache_subdir, document);
        if !compiled_output_is_fresh(&compiled_pdf, &dependencies) {
            return frame_idx;
        }
    }

    frames.len()
}

fn build_mode_label(args: &ArgMatches) -> &'static str {
    if args.is_present("pdfunite") {
        "pdfunite"
    } else if args.is_present("unite") {
        "unite"
    } else {
        "preview"
    }
}

fn bibliography_label(bibliography: Option<BibliographyTool>) -> &'static str {
    match bibliography {
        Some(BibliographyTool::Bibtex) => "bibtex",
        Some(BibliographyTool::Biber) => "biber",
        None => "off",
    }
}

fn first_changed_frame_label(first_changed_frame: usize, frame_count: usize) -> String {
    if frame_count == 0 || first_changed_frame >= frame_count {
        String::from("none")
    } else {
        format!("{}/{}", first_changed_frame + 1, frame_count)
    }
}

fn synctex_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn append_united_source_segment(
    united_tex: &mut String,
    segments: &mut Vec<SyncTexLineSegment>,
    current_temp_line: &mut usize,
    source_content: &str,
    source_start_idx: usize,
    source_segment: &str,
) {
    united_tex.push_str(source_segment);

    let line_count = logical_line_count(source_segment);
    if line_count == 0 {
        return;
    }

    segments.push(SyncTexLineSegment {
        temp_start_line: *current_temp_line,
        line_count,
        source_start_line: line_number_at(source_content, source_start_idx),
    });
    *current_temp_line += line_count;
}

fn split_trailing_frame_boundary(segment: &str) -> (&str, &str) {
    let lines: Vec<&str> = segment.split_inclusive('\n').collect();
    let mut suffix_line_count = 0usize;

    for line in lines.iter().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('%') {
            suffix_line_count += 1;
        } else {
            break;
        }
    }

    if suffix_line_count == 0 {
        return (segment, "");
    }

    let split_idx = lines[..lines.len() - suffix_line_count]
        .iter()
        .map(|line| line.len())
        .sum();
    (&segment[..split_idx], &segment[split_idx..])
}

fn append_united_frame_placeholder(
    united_tex: &mut String,
    current_temp_line: &mut usize,
    source_frame_line_count: usize,
    replacement: &str,
) {
    united_tex.push_str(replacement);

    let line_count = logical_line_count(replacement);
    let _ = source_frame_line_count;
    *current_temp_line += line_count;
}

fn united_frame_replacement(frame_boundary_segment: &str, frame_pdf: &str) -> String {
    frame_boundary_segment.to_owned()
        + "{\\setbeamercolor{background canvas}{bg=}\n"
        + "\\setbeamertemplate{footline}{}\n"
        + "\\setbeamertemplate{headline}{}\n"
        + "\\setbeamertemplate{navigation symbols}{}\n"
        + "\\includepdf[\n  pages=-,\n  pagecommand={\\thispagestyle{empty}\\smash{\\hbox to 0pt{\\phantom{.}\\hss}}}\n]{"
        + frame_pdf
        + "}\n}"
}

fn build_united_document(
    source_content: &str,
    frames: &[String],
    frame_source_lines: &[(usize, usize)],
    generated_documents: &[GeneratedDocument],
    cache_subdir: &Path,
    original_source_path: &Path,
) -> Result<(String, FrameSyncTexMap)> {
    let mut united_tex = String::from("\\RequirePackage{pdfpages}\n");
    let mut segments = Vec::new();
    let mut current_temp_line = logical_line_count(&united_tex) + 1;
    let mut source_cursor = 0usize;
    let mut frame_path_segments = Vec::new();

    for ((frame, (source_frame_start_line, source_frame_line_count)), document) in frames
        .iter()
        .zip(frame_source_lines.iter())
        .zip(generated_documents.iter())
    {
        let frame_start_offset = source_content[source_cursor..].find(frame).ok_or_else(|| {
            error!("Failed to locate frame text while building united SyncTeX mapping.");
            FasterBeamerError::CompileError
        })?;
        let frame_start_idx = source_cursor + frame_start_offset;
        let source_segment = &source_content[source_cursor..frame_start_idx];
        let (source_segment, frame_boundary_segment) =
            split_trailing_frame_boundary(source_segment);

        append_united_source_segment(
            &mut united_tex,
            &mut segments,
            &mut current_temp_line,
            source_content,
            source_cursor,
            source_segment,
        );

        let frame_pdf = compiled_pdf_path(cache_subdir, &document.sync_map.temp_file_name)
            .to_string_lossy()
            .replace('\\', "/");
        let replacement = united_frame_replacement(frame_boundary_segment, &frame_pdf);
        frame_path_segments.push((frame_pdf, *source_frame_start_line));
        append_united_frame_placeholder(
            &mut united_tex,
            &mut current_temp_line,
            *source_frame_line_count,
            &replacement,
        );

        source_cursor = frame_start_idx + frame.len();
    }

    let source_suffix = &source_content[source_cursor..];
    append_united_source_segment(
        &mut united_tex,
        &mut segments,
        &mut current_temp_line,
        source_content,
        source_cursor,
        source_suffix,
    );

    let mut search_cursor = 0usize;
    for (frame_pdf, source_frame_start_line) in frame_path_segments {
        let path_offset = united_tex[search_cursor..].find(&frame_pdf).ok_or_else(|| {
            error!(
                "Failed to locate included PDF path while building united SyncTeX mapping."
            );
            FasterBeamerError::CompileError
        })?;
        let path_idx = search_cursor + path_offset;
        segments.push(SyncTexLineSegment {
            temp_start_line: line_number_at(&united_tex, path_idx),
            line_count: 1,
            source_start_line: source_frame_start_line,
        });
        search_cursor = path_idx + frame_pdf.len();
    }

    Ok((
        united_tex,
        FrameSyncTexMap {
            source_file: original_source_path.to_path_buf(),
            temp_file_name: format!("{}preview.tex", UNITED_TEMP_PREFIX),
            segments,
        },
    ))
}

fn tex_input_name(path: &Path) -> &str {
    path.file_name()
        .and_then(|name| name.to_str())
        .expect("Expected a TeX input file name")
}

fn default_output_file(input_path: &Path) -> String {
    input_path.with_extension("pdf").to_string_lossy().into_owned()
}

fn frame_temp_file_name(hash: &md5::Digest) -> String {
    format!("{}{:x}.tex", FRAME_TEMP_PREFIX, hash)
}

fn preamble_job_name(preamble_hash: &md5::Digest, is_draft: bool) -> String {
    format!("{}{:x}_{}", PREAMBLE_TEMP_PREFIX, preamble_hash, is_draft)
}

fn compiled_pdf_path(cache_subdir: &Path, temp_file_name: &str) -> PathBuf {
    cache_subdir.join(Path::new(temp_file_name).with_extension("pdf"))
}

fn current_cache_paths(input_file: &str) -> (PathBuf, PathBuf, PathBuf) {
    let cwd = current_dir().unwrap();
    let input_path = Path::new(input_file);
    let input_dir = input_path
        .parent()
        .unwrap_or(&cwd)
        .canonicalize()
        .unwrap_or_else(|_| cwd.to_owned());
    let cachedir = dirs::cache_dir().expect("This OS is not supported").join("faster-beamer");
    let cache_subdir = cache_path(&cachedir, &input_dir);

    (input_dir, cachedir, cache_subdir)
}

fn is_hex_digest(value: &str) -> bool {
    value.len() == 32 && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn is_legacy_frame_temp_file(path: &Path, file_name: &str) -> bool {
    match file_name.strip_suffix(".tex") {
        Some(stem) if is_hex_digest(stem) => {}
        _ => return false,
    }

    std::fs::read_to_string(path)
        .map(|content| content.contains("\\addtocounter{framenumber}") && content.contains("\\end{document}"))
        .unwrap_or(false)
}

fn is_legacy_preamble_temp_file(file_name: &str) -> bool {
    for extension in [".fmt", ".log"] {
        if let Some(stem) = file_name.strip_suffix(extension) {
            let mut parts = stem.rsplitn(2, '_');
            let draft_flag = parts.next();
            let digest = parts.next();
            if matches!(draft_flag, Some("true") | Some("false")) && digest.map(is_hex_digest).unwrap_or(false) {
                return true;
            }
        }
    }

    false
}

fn clean_prefixed_files(input_dir: &Path) -> Result<usize> {
    let mut removed = 0;
    let entries = std::fs::read_dir(input_dir).map_err(|err| {
        error!("Failed to read input directory {}: {}", input_dir.display(), err);
        FasterBeamerError::IoError
    })?;

    for entry in entries {
        let entry = entry.map_err(|err| {
            error!("Failed to inspect input directory {}: {}", input_dir.display(), err);
            FasterBeamerError::IoError
        })?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let file_name = match path.file_name().and_then(|name| name.to_str()) {
            Some(name) => name,
            None => continue,
        };

        if file_name.starts_with(FRAME_TEMP_PREFIX)
            || file_name.starts_with(PREAMBLE_TEMP_PREFIX)
            || file_name.starts_with(UNITED_TEMP_PREFIX)
            || is_legacy_frame_temp_file(&path, file_name)
            || is_legacy_preamble_temp_file(file_name)
        {
            std::fs::remove_file(&path).map_err(|err| {
                error!("Failed to remove temporary file {}: {}", path.display(), err);
                FasterBeamerError::IoError
            })?;
            removed += 1;
        }
    }

    Ok(removed)
}

fn prune_empty_cache_dirs(cache_dir: &Path, cache_subdir: &Path) {
    let mut current = cache_subdir.parent();
    while let Some(dir) = current {
        if dir == cache_dir {
            break;
        }

        match std::fs::remove_dir(dir) {
            Ok(_) => current = dir.parent(),
            Err(err) if err.kind() == ErrorKind::DirectoryNotEmpty => break,
            Err(err) => {
                warn!("Failed to prune empty cache directory {}: {}", dir.display(), err);
                break;
            }
        }
    }
}

pub fn clean_generated_artifacts(input_file: &str, args: &ArgMatches) -> Result<()> {
    let input_path = Path::new(input_file);
    if !input_path.is_file() {
        error!("Could not open {}", input_file);
        return Err(FasterBeamerError::InputFileNotExistent);
    }

    let (input_dir, cachedir, cache_subdir) = current_cache_paths(input_file);
    let output_file = output_file_arg(args, input_path);
    let removed_input_files = clean_prefixed_files(&input_dir)?;

    if cache_subdir.is_dir() {
        std::fs::remove_dir_all(&cache_subdir).map_err(|err| {
            error!("Failed to remove cache directory {}: {}", cache_subdir.display(), err);
            FasterBeamerError::IoError
        })?;
        prune_empty_cache_dirs(&cachedir, &cache_subdir);
    }

    clear_published_synctex(&output_file);
    info!(
        "Removed faster-beamer artifacts for {} ({} stale source temp files).",
        input_file,
        removed_input_files
    );

    Ok(())
}

fn output_file_arg(args: &ArgMatches, input_path: &Path) -> String {
    args.value_of("output")
        .or_else(|| args.value_of("OUTPUT"))
        .map(|output| output.to_owned())
        .unwrap_or_else(|| default_output_file(input_path))
}

fn compiler_options(args: &ArgMatches) -> Vec<String> {
    args.values_of("compiler-option")
        .map(|values| values.map(|value| value.to_owned()).collect())
        .unwrap_or_default()
}

fn parallel_job_count(args: &ArgMatches) -> Option<usize> {
    args.value_of("jobs")
        .and_then(|count| count.parse::<usize>().ok())
}

fn apply_compiler_options(mut compiler: LatexCompiler, compiler_options: &[String]) -> LatexCompiler {
    for option in compiler_options {
        compiler = compiler.add_arg(option);
    }

    compiler
}

fn bibliography_tool(args: &ArgMatches) -> Option<BibliographyTool> {
    match args.value_of("bibliography") {
        Some("bibtex") => Some(BibliographyTool::Bibtex),
        Some("biber") => Some(BibliographyTool::Biber),
        _ => None,
    }
}

fn latex_pass_count(args: &ArgMatches) -> usize {
    match (args.is_present("multi-pass"), args.value_of("multi-pass")) {
        (true, Some(pass_count)) => pass_count.parse::<usize>().unwrap_or(2).max(1),
        (true, None) => 2,
        (false, _) => 1,
    }
}

fn latex_run_options(latex_pass_count: usize, bibliography: Option<BibliographyTool>) -> LatexRunOptions {
    LatexRunOptions::new()
        .with_latex_pass_count(latex_pass_count)
        .with_bibliography_tool(bibliography)
}

pub fn process_file(input_file: &str, args: &ArgMatches) -> Result<()> {
    let input_path = Path::new(&input_file);
    let (input_dir, cachedir, cache_subdir) = current_cache_paths(input_file);
    let original_source_path = input_path
        .canonicalize()
        .unwrap_or_else(|_| input_dir.join(tex_input_name(input_path)));
    let output_file = output_file_arg(args, input_path);
    let correct_frame_numbers = args.is_present("frame-numbers");
    let latex_pass_count = latex_pass_count(args);
    let bibliography = bibliography_tool(args);
    let run_options = latex_run_options(latex_pass_count, bibliography);
    let force_recompile = args.is_present("force-recompile");
    let compiler_options = compiler_options(args);
    let parallel_job_count = parallel_job_count(args);
    let use_parallel = args.is_present("parallel") || parallel_job_count.is_some();
    let build_mode = build_mode_label(args);

    if !input_path.is_file() {
        error!("Could not open {}", input_file);
        return Err(FasterBeamerError::InputFileNotExistent);
    }

    let parsed_file = parsing::ParsedFile::new(input_file.to_string());
    trace!("{}", parsed_file.syntax_tree.root_node().to_sexp());

    let frame_nodes = if args.is_present("tree-sitter") {
        get_frames(&parsed_file)
    } else {
        Vec::new()
    };

    let mut frames = Vec::with_capacity(frame_nodes.len());
    let mut frame_source_lines = Vec::with_capacity(frame_nodes.len());
    if !frame_nodes.is_empty() {
        for f in frame_nodes.iter() {
            let node_string = parsed_file.get_node_string(&f);
            frames.push(node_string.to_string());
            frame_source_lines.push((
                line_number_at(&parsed_file.file_content, f.start_byte()),
                logical_line_count(node_string),
            ));
        }
    } else {
        for cap in FRAME_REGEX.captures_iter(&parsed_file.file_content) {
            let frame_match = cap.get(0).unwrap();
            let frame_string = frame_match.as_str().to_string();
            trace!("Frame {}:\n{}", frames.len() + 1, &frame_string);
            frames.push(frame_string);
            frame_source_lines.push((
                line_number_at(&parsed_file.file_content, frame_match.start()),
                logical_line_count(frame_match.as_str()),
            ));
        }
    }
    info!(
        "Build: {} -> {} [{}]",
        original_source_path.display(),
        Path::new(&output_file).display(),
        build_mode
    );
    info!(
        "Frames: total={}, parser={}",
        frames.len(),
        if !frame_nodes.is_empty() { "tree-sitter" } else { "regex" }
    );

    if log_enabled!(Trace) && args.is_present("tree-sitter") {
        let root_node = parsed_file.syntax_tree.root_node();
        let mut stack = vec![root_node];

        while !stack.is_empty() {
            let current_node = stack.pop().unwrap();
            if current_node.kind() == "ERROR" {
                error!(
                    "\n{}:\n\t {}",
                    current_node.kind(),
                    parsed_file.get_node_string(&current_node),
                );
            }

            for i in (0..current_node.named_child_count()).rev() {
                stack.push(current_node.named_child(i).unwrap());
            }
        }
    }

    //let document_env = tree_traversal::get_children(
    //parsed_file.syntax_tree.root_node(),
    //&|n| n.kind() == "document_env",
    //true,
    //TraversalOrder::BreadthFirst,
    //);
    //let preamble =[> if document_env.len() == 1 as usize {<]
    //parsed_file.file_content[0..document_env[0].start_byte()].to_owned()
    //} else {
    //warn!(
    //"Could not find document environment with tree_sitter ({})",
    //input_file
    /*);*/
    let find = parsed_file.file_content.find("\\begin{document}");
    let preamble = match find {
        Some(x) => Some(parsed_file.file_content[..x].to_owned()),
        None => None,
    }
    .unwrap_or_else(|| r"\documentclass[aspectratio=43,c,xcolor=dvipsnames]{beamer}".to_string());

    std::fs::create_dir_all(&cachedir).map_err(|ref err| {
        error!("Failed to create cache dir \"{}\": {}", cachedir.display(), err);
        FasterBeamerError::IoError
    })?;

    std::fs::create_dir_all(&cache_subdir).map_err(|ref err| {
        error!(
            "Failed to create cache subdir \"{}\": {}",
            cache_subdir.display(),
            err
        );
        FasterBeamerError::IoError
    })?;

    let preamble_hash = md5::compute(&preamble);
    let preamble_line_count = logical_line_count(&preamble);
    let document_begin_line = find
        .map(|idx| line_number_at(&parsed_file.file_content, idx))
        .unwrap_or(preamble_line_count + 1);
    let document_end_line = parsed_file
        .file_content
        .rfind("\\end{document}")
        .map(|idx| line_number_at(&parsed_file.file_content, idx))
        .unwrap_or(document_begin_line);
    let preamble_filename = preamble_job_name(&preamble_hash, args.is_present("draft"));
    let preamble_format_path = input_dir.join(format!("{}.fmt", preamble_filename));
    if preamble_format_path.is_file()
    {
        info!("Preamble: cached {}", preamble_format_path.display());
    } else {
        info!("Preamble: compiling {}", preamble_format_path.display());
        let mut command = Command::new("pdflatex");
        command
            .arg("-shell-escape")
            .arg("-ini")
            .arg(format!("-jobname={}", preamble_filename))
            .arg("&pdflatex")
            .arg("mylatexformat.ltx");
        for option in &compiler_options {
            command.arg(option);
        }
        let output = command.arg(tex_input_name(input_path)).current_dir(&input_dir).output();
        match output {
            Err(e) => {
                let preamble_log_path = input_dir.join(format!("{}.log", preamble_filename));
                let fallback = format!(
                    "Failed to compile preamble for {}: {}",
                    original_source_path.display(),
                    e
                );
                if let Err(_err) = write_master_log_from_compile_failure(
                    &original_source_path,
                    "preamble compilation",
                    &preamble_log_path,
                    &fallback,
                ) {
                    warn!(
                        "Failed to write source log for preamble failure: {}",
                        original_source_path.display()
                    );
                }
                log_command_error("pdflatex", "compile the preamble", &e);
                show_error_slide(&cachedir, &output_file);

                *PREVIOUS_FRAMES.lock().unwrap() = Vec::new();
                return Err(FasterBeamerError::CompileError);
            }
            Ok(output) if !output.status.success() => {
                let preamble_log_path = input_dir.join(format!("{}.log", preamble_filename));
                let stderr = str::from_utf8(&output.stderr).unwrap_or("").trim();
                let stdout = str::from_utf8(&output.stdout).unwrap_or("").trim();
                let fallback = if !stderr.is_empty() {
                    stderr.to_string()
                } else if !stdout.is_empty() {
                    stdout.to_string()
                } else {
                    String::from("preamble compilation failed")
                };
                if let Err(_err) = write_master_log_from_compile_failure(
                    &original_source_path,
                    "preamble compilation",
                    &preamble_log_path,
                    &fallback,
                ) {
                    warn!(
                        "Failed to write source log for preamble failure: {}",
                        original_source_path.display()
                    );
                }
                error!(
                    "Failed to compile preamble! {}",
                    str::from_utf8(&output.stderr).unwrap()
                );
                show_error_slide(&cachedir, &output_file);

                *PREVIOUS_FRAMES.lock().unwrap() = Vec::new();
                return Err(FasterBeamerError::CompileError);
            }
            _ => {}
        };
    }

    let source_sections = document_sections(&parsed_file.file_content);
    let mut generated_documents = Vec::new();
    let mut unsupported_dynamic_toc_frames = 0usize;
    let mut command = Command::new("pdfunite");
    for (frame_idx, (f, (source_frame_start_line, frame_line_count))) in frames
        .iter()
        .zip(frame_source_lines.iter())
        .enumerate()
    {
        let format_line = format!("%&{}\n", preamble_filename);
        let counter_setup = frame_counter_setup(frame_idx, correct_frame_numbers);
        let toc_frame_patch = toc_frame_patch(
            f,
            *source_frame_start_line,
            document_begin_line,
            &input_dir,
            input_path,
            &source_sections,
        );
        let (toc_runtime_setup, support_files, additional_dependencies) = match toc_frame_patch {
            TocFrameSupport::None => (String::new(), Vec::new(), Vec::new()),
            TocFrameSupport::Supported(patch) => (
                patch.runtime_setup,
                patch.support_files,
                patch.additional_dependencies,
            ),
            TocFrameSupport::UnsupportedDynamic => {
                unsupported_dynamic_toc_frames += 1;
                (String::new(), Vec::new(), Vec::new())
            }
        };
        let compile_prefix = format_line.clone()
            + &preamble
            + "\n\\begin{document}\n"
            + &counter_setup
            + &toc_runtime_setup;
        let compile_string = compile_prefix.clone() + &f + "\n\\end{document}\n";

        let mut hash_input = compile_string.clone();
        for support_file in &support_files {
            hash_input.push_str(support_file.extension);
            hash_input.push('\n');
            hash_input.push_str(&support_file.content);
            hash_input.push('\n');
        }

        let hash = md5::compute(&hash_input);
        let temp_file_name = frame_temp_file_name(&hash);
        let output = compiled_pdf_path(&cache_subdir, &temp_file_name);
        let temp_frame_start_line = logical_line_count(&compile_prefix) + 1;
        let temp_document_begin_line = logical_line_count(&(format_line.clone() + &preamble + "\n")) + 1;
        let temp_document_end_line = logical_line_count(&(compile_prefix.clone() + &f + "\n")) + 1;
        let mut segments = Vec::new();

        if preamble_line_count > 0 {
            segments.push(SyncTexLineSegment {
                temp_start_line: logical_line_count(&format_line) + 1,
                line_count: preamble_line_count,
                source_start_line: 1,
            });
        }
        segments.push(SyncTexLineSegment {
            temp_start_line: temp_document_begin_line,
            line_count: 1,
            source_start_line: document_begin_line,
        });
        segments.push(SyncTexLineSegment {
            temp_start_line: temp_frame_start_line,
            line_count: *frame_line_count,
            source_start_line: *source_frame_start_line,
        });
        segments.push(SyncTexLineSegment {
            temp_start_line: temp_document_end_line,
            line_count: 1,
            source_start_line: document_end_line,
        });
        let mut dependencies = collect_related_files(&compile_string, &input_dir);
        dependencies.extend(additional_dependencies);
        dependencies.sort();
        dependencies.dedup();

        generated_documents.push(GeneratedDocument {
            tex_content: compile_string,
            sync_map: FrameSyncTexMap {
                source_file: original_source_path.clone(),
                temp_file_name,
                segments,
            },
            dependencies,
            support_files,
        });

        command.arg(&output);
    }

    if unsupported_dynamic_toc_frames > 0 {
        warn!(
            "Detected {} dynamic Beamer TOC frame(s) that faster-beamer cannot render correctly as cached per-frame PDFs (for example \\AtBeginSection with \\tableofcontents[currentsection]). The build will continue, but the proper workflow is a full document compile such as: pdflatex -interaction=nonstopmode -halt-on-error {} ; pdflatex -interaction=nonstopmode -halt-on-error {} (and run bibtex/biber between passes if needed).",
            unsupported_dynamic_toc_frames,
            tex_input_name(input_path),
            tex_input_name(input_path),
        );
    }

    trace!("Comparing frames");
    let mut first_changed_frame = {
        let previous_frames = PREVIOUS_FRAMES.lock().unwrap();
        first_changed_frame_index(
            &frames,
            &previous_frames,
            &generated_documents,
            &cache_subdir,
            force_recompile,
        )
    };
    debug!(
        "Found first difference in frame {} from {}",
        &first_changed_frame,
        frames.len()
    );

    let mut seen_compile_jobs = HashSet::new();
    let compile_targets: Vec<(usize, &GeneratedDocument, bool)> = generated_documents
        .iter()
        .enumerate()
        .filter_map(|(frame_idx, document)| {
            if !seen_compile_jobs.insert(document.sync_map.temp_file_name.clone()) {
                return None;
            }

            let compiled_pdf = compiled_pdf_path(&cache_subdir, &document.sync_map.temp_file_name);
            let dependencies = dependencies_for_document(&cache_subdir, document);
            let needs_compile = force_recompile
                || !compiled_output_is_fresh(&compiled_pdf, &dependencies);
            Some((frame_idx, document, needs_compile))
        })
        .collect();
    let compile_job_count = compile_targets.len();
    let frames_to_compile = compile_targets
        .iter()
        .filter(|(_, _, needs_compile)| *needs_compile)
        .count();
    let cached_frames = compile_job_count.saturating_sub(frames_to_compile);
    let parallel_label = match parallel_job_count {
        Some(job_count) => format!("{} jobs", job_count),
        None if use_parallel => String::from("auto"),
        None => String::from("off"),
    };
    info!(
        "Compile: jobs={}, queued={}, cached={}, first-changed={}",
        compile_job_count,
        frames_to_compile,
        cached_frames,
        first_changed_frame_label(first_changed_frame, frames.len())
    );
    info!(
        "LaTeX: passes={}, bibliography={}, parallel={}",
        latex_pass_count,
        bibliography_label(bibliography),
        parallel_label
    );

    let progress_bar = compile_progress_bar(compile_targets.len());
    let latex_input = LatexInput::new();
    let compile_failures: Mutex<Vec<FrameCompileFailure>> = Mutex::new(Vec::new());

        let compile_document = |frame_idx: usize, document: &GeneratedDocument, needs_compile: bool| {
            let pdf = compiled_pdf_path(&cache_subdir, &document.sync_map.temp_file_name);
            let (source_start_line, source_line_count) = frame_source_lines[frame_idx];
            let temp_file = input_dir.join(&document.sync_map.temp_file_name);
            let frame_preview = frame_preview(&frames[frame_idx]);

            if !needs_compile {
                trace!("{} is already compiled!", pdf.to_str().unwrap_or("???"));
            } else {
                let mut support_paths = Vec::new();

                if write(&temp_file, &document.tex_content).is_ok() {
                    for support_file in &document.support_files {
                        let support_path = cache_subdir.join(
                            Path::new(&document.sync_map.temp_file_name)
                                .with_extension(support_file.extension),
                        );

                        match write(&support_path, &support_file.content) {
                            Ok(_) => support_paths.push(support_path),
                            Err(err) => warn!(
                                "Failed to write temporary support file {}: {}",
                                support_path.display(),
                                err
                            ),
                        }
                    }

                    let compiler = apply_compiler_options(
                        LatexCompiler::new_in(cache_subdir.clone())
                            .add_arg("-shell-escape")
                            .add_arg("-interaction=nonstopmode")
                            .with_current_dir(input_dir.clone()),
                        &compiler_options,
                    );
                    let document_run_options = if document.support_files.is_empty() {
                        run_options
                    } else {
                        run_options
                            .with_latex_pass_count(1)
                            .with_bibliography_tool(None)
                    };

                    let result = compiler.run(
                        Path::new(tex_input_name(&temp_file)),
                        &latex_input,
                        document_run_options,
                    );
                    if result.is_ok() {
                        if update_dependency_manifest(&cache_subdir, &document.sync_map.temp_file_name).is_err() {
                            warn!(
                                "Failed to update dependency manifest for {}",
                                document.sync_map.temp_file_name
                            );
                        }
                        if let Err(err) = std::fs::remove_file(&temp_file) {
                            warn!("Failed to remove temporary frame source {}: {}", temp_file.display(), err);
                        }
                        for support_path in &support_paths {
                            if let Err(err) = std::fs::remove_file(support_path) {
                                warn!(
                                    "Failed to remove temporary support file {}: {}",
                                    support_path.display(),
                                    err
                                );
                            }
                        }
                        trace!("Compiled file {}", &temp_file.to_str().unwrap());
                    } else {
                        let compile_error = result.err().unwrap();
                        for support_path in &support_paths {
                            if let Err(err) = std::fs::remove_file(support_path) {
                                warn!(
                                    "Failed to remove temporary support file {}: {}",
                                    support_path.display(),
                                    err
                                );
                            }
                        }
                        compile_failures
                            .lock()
                            .expect("compile_failures lock should not be poisoned")
                            .push(FrameCompileFailure {
                                frame_idx,
                                source_start_line,
                                source_line_count,
                                temp_file: temp_file.clone(),
                                temp_file_name: document.sync_map.temp_file_name.clone(),
                                sync_segments: document.sync_map.segments.clone(),
                                frame_preview,
                                error: format!("{}", compile_error),
                            });
                    };
                } else {
                    compile_failures
                        .lock()
                        .expect("compile_failures lock should not be poisoned")
                        .push(FrameCompileFailure {
                            frame_idx,
                            source_start_line,
                            source_line_count,
                            temp_file: temp_file.clone(),
                            temp_file_name: document.sync_map.temp_file_name.clone(),
                            sync_segments: document.sync_map.segments.clone(),
                            frame_preview,
                            error: String::from("Failed to write generated frame source to disk."),
                        });
                }
            };
            progress_bar.inc(1);
        };

    if use_parallel {
        if let Some(job_count) = parallel_job_count {
            rayon::ThreadPoolBuilder::new()
                .num_threads(job_count)
                .build()
                .expect("Failed to build the compile thread pool")
                .install(|| {
                    compile_targets
                        .par_iter()
                        .for_each(|(frame_idx, document, needs_compile)| {
                            compile_document(*frame_idx, document, *needs_compile)
                        });
                });
        } else {
            compile_targets
                .par_iter()
                .for_each(|(frame_idx, document, needs_compile)| {
                    compile_document(*frame_idx, document, *needs_compile)
                });
        }
    } else {
        compile_targets
            .iter()
            .for_each(|(frame_idx, document, needs_compile)| {
                compile_document(*frame_idx, document, *needs_compile)
            });
    }
    progress_bar.finish_and_clear();

    let failed_compiles = compile_failures
        .into_inner()
        .expect("compile_failures lock should not be poisoned");
    if !failed_compiles.is_empty() {
        if let Err(err) = write_master_log_for_frame_failures(
            &failed_compiles,
            &cache_subdir,
            &original_source_path,
            frames.len(),
        ) {
            let _ = err;
            warn!(
                "Failed to create aggregated source log for {}.",
                original_source_path.display()
            );
        }
        log_frame_compile_failures(&failed_compiles, &original_source_path, frames.len());
        show_error_slide(&cachedir, &output_file);
        *PREVIOUS_FRAMES.lock().unwrap() = Vec::new();
        return Err(FasterBeamerError::CompileError);
    }

    if args.is_present("pdfunite") {
        info!("Publish: pdfunite -> {}", Path::new(&output_file).display());
        let output = command.arg(&output_file).output();

        match output {
            Err(e) => {
                if e.kind() == ErrorKind::NotFound {
                    error!(
                        "Failed to run pdfunite: pdfunite was not found on PATH. Install it or use --unite instead."
                    );
                } else {
                    error!("Failed to run pdf unite!\n{}", e);
                }
                show_error_slide(&cachedir, &output_file);

                *PREVIOUS_FRAMES.lock().unwrap() = frames;
                return Err(FasterBeamerError::PdfUniteError);
            }
            Ok(output) if !output.status.success() => {
                error!(
                    "Failed to run pdfunite! {}",
                    str::from_utf8(&output.stderr).unwrap()
                );
                show_error_slide(&cachedir, &output_file);

                *PREVIOUS_FRAMES.lock().unwrap() = frames;
                return Err(FasterBeamerError::PdfUniteError);
            }
            _ => {
                clear_published_synctex(&output_file);
            }
        };
    } else if args.is_present("unite") {
        info!("Publish: united document -> {}", Path::new(&output_file).display());

        let (united_tex, mut united_sync_map) = build_united_document(
            &parsed_file.file_content,
            &frames,
            &frame_source_lines,
            &generated_documents,
            &cache_subdir,
            &original_source_path,
        )?;

        let united_suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or(0);
        let united_job_name = format!("{}{}", UNITED_TEMP_PREFIX, united_suffix);
        united_sync_map.temp_file_name = format!("{}.tex", united_job_name);

        let united_tex_file = input_dir.join(&united_sync_map.temp_file_name);
        let united_pdf = cache_subdir.join(format!("{}.pdf", united_job_name));
        let write_result = write(&united_tex_file, united_tex);
        if write_result.is_ok() {
            let compiler = apply_compiler_options(
                LatexCompiler::new_in(cache_subdir.clone())
                    .add_arg("-shell-escape")
                    .add_arg("-interaction=nonstopmode")
                    .with_current_dir(input_dir.clone()),
                &compiler_options,
            );

            let compile_result = compiler.run(
                Path::new(tex_input_name(&united_tex_file)),
                &LatexInput::new(),
                run_options,
            );

            let united_error_message = compile_result.err().map(|err| format!("{}", err));

            if let Some(error_message) = united_error_message.as_ref() {
                if let Err(_err) = write_master_log_for_united_failure(
                    &original_source_path,
                    &cache_subdir,
                    &united_sync_map,
                    error_message,
                ) {
                    warn!(
                        "Failed to write source log for united compilation failure: {}",
                        original_source_path.display()
                    );
                }
                error!("Failed to run pdf unite!\n{}", error_message);
            }

            if united_pdf.is_file() {
                publish_output_artifacts(&united_pdf, &output_file, Some(&united_sync_map))?;
                if let Err(err) = std::fs::remove_file(&united_tex_file) {
                    warn!(
                        "Failed to remove temporary united source {}: {}",
                        united_tex_file.display(),
                        err
                    );
                }
            } else {
                error!("Compilation failed!");
                show_error_slide(&cachedir, &output_file);

                *PREVIOUS_FRAMES.lock().unwrap() = frames;
                return Err(FasterBeamerError::CompileError);
            }
        } else {
            error!("Failed to write united.tex: {:?}", write_result.err());
            return Err(FasterBeamerError::PdfUniteError);
        }
    } else {
        if first_changed_frame == generated_documents.len() {
            first_changed_frame = 0;
        }
        if first_changed_frame < generated_documents.len() {
            info!(
                "Publish: preview frame {}/{} -> {}",
                first_changed_frame + 1,
                generated_documents.len(),
                Path::new(&output_file).display()
            );
            let document = &generated_documents[first_changed_frame];
            let compiled_pdf = compiled_pdf_path(&cache_subdir, &document.sync_map.temp_file_name);

            if Path::new(&compiled_pdf).is_file() {
                publish_output_artifacts(&compiled_pdf, &output_file, Some(&document.sync_map))?;
            } else {
                error!("Compilation failed!");
                show_error_slide(&cachedir, &output_file);

                *PREVIOUS_FRAMES.lock().unwrap() = frames;
                return Err(FasterBeamerError::CompileError);
            }
        }
    }

    *PREVIOUS_FRAMES.lock().unwrap() = frames;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::collect_related_files;
    use super::document_sections;
    use super::first_changed_frame_index;
    use super::frame_counter_setup;
    use super::toc_frame_patch;
    use super::united_frame_replacement;
    use super::FrameSyncTexMap;
    use super::GeneratedDocument;
    use super::TocFrameSupport;
    use std::fs;
    use std::thread;
    use std::time::Duration;
    use tempfile::tempdir;

    #[test]
    fn frame_number_setup_only_sets_framenumber() {
        assert_eq!(frame_counter_setup(3, true), "\\setcounter{framenumber}{3}\n");
    }

    #[test]
    fn frame_number_setup_is_empty_when_disabled() {
        assert_eq!(frame_counter_setup(3, false), "");
    }

    #[test]
    fn united_frame_replacement_suppresses_wrapper_templates() {
        let replacement = united_frame_replacement("", "frame.pdf");

        assert!(replacement.contains("\\setbeamertemplate{footline}{}"));
        assert!(replacement.contains("\\setbeamertemplate{headline}{}"));
        assert!(replacement.contains("\\setbeamertemplate{navigation symbols}{}"));
        assert!(replacement.contains("pagecommand={\\thispagestyle{empty}"));
    }

    #[test]
    fn collect_related_files_finds_nested_inputs_and_graphics() {
        let temp_dir = tempdir().unwrap();
        let nested_dir = temp_dir.path().join("figs");
        fs::create_dir_all(&nested_dir).unwrap();

        let chunk_path = nested_dir.join("chunk.tex");
        let graphic_path = nested_dir.join("plot.pdf");
        fs::write(&chunk_path, "\\includegraphics{plot}").unwrap();
        fs::write(&graphic_path, b"pdf").unwrap();

        let dependencies = collect_related_files("\\input{figs/chunk}", temp_dir.path());

        assert!(dependencies.contains(&chunk_path));
        assert!(dependencies.contains(&graphic_path));
    }

    #[test]
    fn collect_related_files_ignores_dynamic_macro_paths_in_inputs() {
        let temp_dir = tempdir().unwrap();
        let shared_path = temp_dir.path().join("shared.tex");
        fs::write(
            &shared_path,
            "\\newcommand{\\frameplot}[1]{\\includegraphics{#1}}\n\\newcommand{\\otherplot}[2]{\\includegraphics{#2}}",
        )
        .unwrap();

        let dependencies = collect_related_files("\\input{shared}", temp_dir.path());

        assert_eq!(dependencies, vec![shared_path]);
    }

    #[test]
    fn collect_related_files_uses_graphicspath_from_parent_input() {
        let temp_dir = tempdir().unwrap();
        let figs_dir = temp_dir.path().join("figs");
        fs::create_dir_all(&figs_dir).unwrap();

        let shared_path = temp_dir.path().join("shared.tex");
        let graphic_path = figs_dir.join("plot.pdf");
        fs::write(&shared_path, "\\includegraphics{plot}").unwrap();
        fs::write(&graphic_path, b"pdf").unwrap();

        let dependencies = collect_related_files(
            "\\graphicspath{{./figs/}}\\input{shared}",
            temp_dir.path(),
        );

        assert!(dependencies.contains(&shared_path));
        assert!(dependencies.contains(&graphic_path));
    }

    #[test]
    fn toc_frame_patch_generates_synthetic_toc_for_document_frame() {
        let temp_dir = tempdir().unwrap();
        let input_path = temp_dir.path().join("slides.tex");
        fs::write(&input_path, "\\documentclass{beamer}\n\\begin{document}\n").unwrap();

        let source = "\\documentclass{beamer}\n\\begin{document}\n\\section{Intro}\n\\begin{frame}{Agenda}\n\\tableofcontents\n\\end{frame}\n\\section{Next}\n\\end{document}\n";
        let sections = document_sections(source);
        let frame = "\\begin{frame}{Agenda}\n\\tableofcontents\n\\end{frame}";

        match toc_frame_patch(frame, 4, 2, temp_dir.path(), &input_path, &sections) {
            TocFrameSupport::Supported(patch) => {
                assert!(patch.runtime_setup.contains("\\setcounter{section}{0}"));
                assert_eq!(patch.support_files.len(), 1);
                assert!(patch.support_files[0].content.contains("\\beamer@sectionintoc {1}{Intro}"));
                assert!(patch.support_files[0].content.contains("\\beamer@sectionintoc {2}{Next}"));
            }
            _ => panic!("expected supported TOC frame patch"),
        }
    }

    #[test]
    fn toc_frame_patch_marks_dynamic_preamble_toc_as_unsupported() {
        let temp_dir = tempdir().unwrap();
        let input_path = temp_dir.path().join("slides.tex");
        fs::write(&input_path, "\\documentclass{beamer}\n").unwrap();

        let source = "\\documentclass{beamer}\n\\AtBeginSection[]{\\begin{frame}\\tableofcontents[currentsection]\\end{frame}}\n\\begin{document}\n\\section{Intro}\n\\end{document}\n";
        let sections = document_sections(source);
        let frame = "\\begin{frame}\\tableofcontents[currentsection]\\end{frame}";

        assert!(matches!(
            toc_frame_patch(frame, 2, 3, temp_dir.path(), &input_path, &sections),
            TocFrameSupport::UnsupportedDynamic
        ));
    }

    #[test]
    fn parse_fls_dependencies_ignores_generated_temp_files() {
        let temp_dir = tempdir().unwrap();
        let cache_dir = temp_dir.path().join("cache");
        let source_dir = temp_dir.path().join("src");
        fs::create_dir_all(&cache_dir).unwrap();
        fs::create_dir_all(&source_dir).unwrap();

        let source_file = source_dir.join("slides.tex");
        let graphic_file = source_dir.join("plot.pdf");
        let temp_file = source_dir.join("faster-beamer-temp-demo.tex");
        let cached_file = cache_dir.join("foo.sty");
        fs::write(&source_file, "slides").unwrap();
        fs::write(&graphic_file, b"pdf").unwrap();
        fs::write(&temp_file, "temp").unwrap();
        fs::write(&cached_file, "cached").unwrap();

        let fls = format!(
            "INPUT {}\nINPUT {}\nINPUT {}\nINPUT {}\n",
            source_file.display(),
            graphic_file.display(),
            temp_file.display(),
            cached_file.display()
        );

        let dependencies = super::parse_fls_dependencies(&fls, &cache_dir);

        assert_eq!(dependencies, vec![graphic_file, source_file]);
    }

    #[test]
    fn first_changed_frame_detects_stale_related_file() {
        let temp_dir = tempdir().unwrap();
        let cache_dir = temp_dir.path().join("cache");
        fs::create_dir_all(&cache_dir).unwrap();

        let dependency = temp_dir.path().join("plot.pdf");
        let compiled_pdf = cache_dir.join("faster-beamer-temp-test.pdf");
        fs::write(&dependency, b"old figure").unwrap();
        thread::sleep(Duration::from_millis(1100));
        fs::write(&compiled_pdf, b"compiled").unwrap();
        thread::sleep(Duration::from_millis(1100));
        fs::write(&dependency, b"new figure").unwrap();

        let generated_documents = vec![GeneratedDocument {
            tex_content: String::new(),
            sync_map: FrameSyncTexMap {
                source_file: temp_dir.path().join("slides.tex"),
                temp_file_name: String::from("faster-beamer-temp-test.tex"),
                segments: Vec::new(),
            },
            dependencies: vec![dependency],
            support_files: Vec::new(),
        }];

        let first_changed = first_changed_frame_index(
            &[String::from("same frame")],
            &[String::from("same frame")],
            &generated_documents,
            &cache_dir,
            false,
        );

        assert_eq!(first_changed, 0);
    }
}
